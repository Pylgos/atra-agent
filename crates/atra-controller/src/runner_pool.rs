use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, bail};
use atra_platform::PlatformStore;
use atra_protocol::{
    ApprovalPolicy, BackgroundProcess, BackgroundProcessDetail, CommandOutput, ProcessHandle,
    ProcessId, ProcessStatus, Runner as RunnerInfo, ThreadId,
};
use atra_store::Store;
use tokio::sync::Mutex;

use crate::{Runner, RunnerConfig, skills};

pub(super) struct RunnerPool {
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    processes: Mutex<HashMap<ProcessKey, ProcessRecord>>,
    platform: Option<Arc<PlatformStore>>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct ProcessKey {
    pub(super) thread_id: ThreadId,
    pub(super) runner: String,
    pub(super) process_id: ProcessId,
}

#[derive(Clone)]
pub(super) struct ProcessRecord {
    pub(super) handle: ProcessHandle,
    pub(super) command: String,
    pub(super) started_at_ms: i64,
}

impl RunnerPool {
    pub(super) fn new(platform: Option<Arc<PlatformStore>>) -> Self {
        Self {
            runners: Mutex::new(HashMap::new()),
            processes: Mutex::new(HashMap::new()),
            platform,
        }
    }

    pub(super) async fn launch(
        &self,
        name: String,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
        skill_store: &Store,
        generation: &skills::SkillGeneration,
    ) -> Result<bool> {
        if name.is_empty() {
            bail!("runner name must not be empty");
        }
        if command.is_empty() {
            bail!("runner command must not be empty");
        }

        let mut runners = self.runners.lock().await;
        if let Some(runner) = runners.get(&name) {
            if runner
                .child
                .lock()
                .await
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_none()
            {
                *runner.config.lock().await = RunnerConfig {
                    description,
                    approval,
                };
                return Ok(false);
            }
            runners.remove(&name);
        }

        let runner = Arc::new(
            Runner::start(&name, description, approval, command, self.platform.clone()).await?,
        );
        runner.sync_skills(skill_store, generation).await?;
        runners.insert(name, runner);
        Ok(true)
    }

    pub(super) async fn list(&self) -> Result<Vec<RunnerInfo>> {
        let mut runners = self.runners.lock().await;
        let mut stopped = Vec::new();
        let mut result = Vec::new();
        for (name, runner) in runners.iter() {
            if runner
                .child
                .lock()
                .await
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_some()
            {
                stopped.push(name.clone());
                continue;
            }
            result.push(RunnerInfo {
                name: name.clone(),
                description: runner.config.lock().await.description.clone(),
            });
        }
        for name in stopped {
            runners.remove(&name);
        }
        result.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(result)
    }

    pub(super) async fn get(&self, name: &str) -> Result<Arc<Runner>> {
        self.runners
            .lock()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("runner {name} is not running"))
    }

    pub(super) async fn sync_skills(
        &self,
        store: &Store,
        generation: &skills::SkillGeneration,
    ) -> Result<()> {
        let runners = self.runners.lock().await;
        for (name, runner) in runners.iter() {
            runner
                .sync_skills(store, generation)
                .await
                .with_context(|| format!("failed to synchronize skills to runner {name}"))?;
        }
        Ok(())
    }

    pub(super) async fn contains_process(&self, key: &ProcessKey) -> bool {
        let Some(record) = self.process(key).await else {
            return false;
        };
        let available = match self.get(&key.runner).await {
            Ok(runner) => runner.client.status(record.handle).await.is_ok(),
            Err(_) => false,
        };
        if !available {
            self.remove_process(key).await;
        }
        available
    }

    pub(super) async fn process(&self, key: &ProcessKey) -> Option<ProcessRecord> {
        self.processes.lock().await.get(key).cloned()
    }

    pub(super) async fn insert_process(&self, key: ProcessKey, record: ProcessRecord) {
        self.processes.lock().await.insert(key, record);
    }

    pub(super) async fn remove_process(&self, key: &ProcessKey) {
        self.processes.lock().await.remove(key);
    }

    pub(super) async fn generate_process_id(&self, thread_id: ThreadId, runner: &str) -> ProcessId {
        let processes = self.processes.lock().await;
        loop {
            let process_id = ProcessId(atra_id::generate().replace(' ', "-"));
            let key = ProcessKey {
                thread_id,
                runner: runner.to_owned(),
                process_id: process_id.clone(),
            };
            if crate::valid_process_id(process_id.as_ref()) && !processes.contains_key(&key) {
                return process_id;
            }
        }
    }

    pub(super) async fn list_processes(&self, thread_id: ThreadId) -> Vec<BackgroundProcess> {
        let records = self
            .processes
            .lock()
            .await
            .iter()
            .filter(|(key, _)| key.thread_id == thread_id)
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect::<Vec<_>>();
        let mut processes = Vec::with_capacity(records.len());
        let mut unavailable = Vec::new();
        for (key, record) in records {
            let status = self.process_status(&key, &record).await;
            if matches!(status, ProcessStatus::Unavailable { .. }) {
                unavailable.push(key);
                continue;
            }
            processes.push(BackgroundProcess {
                runner: key.runner,
                process_id: key.process_id,
                command: record.command,
                started_at_ms: record.started_at_ms,
                status,
            });
        }
        let mut records = self.processes.lock().await;
        for key in unavailable {
            records.remove(&key);
        }
        drop(records);
        processes.sort_by(|left, right| {
            left.started_at_ms
                .cmp(&right.started_at_ms)
                .then_with(|| left.process_id.cmp(&right.process_id))
        });
        processes
    }

    pub(super) async fn inspect_process(
        &self,
        key: ProcessKey,
        record: ProcessRecord,
    ) -> BackgroundProcessDetail {
        let response = match self.get(&key.runner).await {
            Ok(runner) => runner.client.inspect(record.handle.clone()).await,
            Err(error) => Err(error),
        };
        let (status, output_tail, omitted_bytes) = match response {
            Ok(crate::runner_client::ProcessInspection {
                status,
                output_tail,
                omitted_bytes,
            }) => (status, output_tail, omitted_bytes),
            Err(error) => (
                ProcessStatus::Unavailable {
                    message: format!("{error:#}"),
                },
                String::new(),
                0,
            ),
        };
        BackgroundProcessDetail {
            process: BackgroundProcess {
                runner: key.runner,
                process_id: key.process_id,
                command: record.command,
                started_at_ms: record.started_at_ms,
                status,
            },
            output_tail,
            omitted_bytes,
        }
    }

    async fn process_status(&self, key: &ProcessKey, record: &ProcessRecord) -> ProcessStatus {
        let response = match self.get(&key.runner).await {
            Ok(runner) => runner.client.status(record.handle.clone()).await,
            Err(error) => Err(error),
        };
        match response {
            Ok(process_status) => process_status,
            Err(error) => ProcessStatus::Unavailable {
                message: format!("{error:#}"),
            },
        }
    }

    pub(super) async fn stop_process(&self, key: &ProcessKey) -> Result<CommandOutput> {
        let record = self
            .process(key)
            .await
            .context("background process is no longer available")?;
        let output = self.get(&key.runner).await?.stop(record.handle).await?;
        self.remove_process(key).await;
        Ok(output)
    }
}
