use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use atra_platform::PlatformStore;
use atra_protocol::{
    ApprovalPolicy, ControllerOperation, ProcessHandle, ProcessId, ProcessStatus,
    Runner as RunnerInfo, RunnerLifecycle, RunnerState, ThreadId,
};
use atra_store::Store;
use tokio::sync::Mutex;

use crate::{Runner, RunnerConfig, Views, runner_client::CallbackEvent, skills};

pub(super) struct RunnerPool {
    runners: Mutex<HashMap<String, RunnerSlot>>,
    processes: Mutex<HashMap<ProcessKey, ProcessRecord>>,
    platform: Option<Arc<PlatformStore>>,
    views: std::sync::Weak<Views>,
    callback_events: tokio::sync::mpsc::UnboundedSender<CallbackEvent>,
}

struct RunnerSlot {
    runner: Arc<Runner>,
    watcher: tokio::task::JoinHandle<()>,
}

impl Drop for RunnerSlot {
    fn drop(&mut self) {
        self.watcher.abort();
    }
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

pub(super) struct TakenProcess {
    key: ProcessKey,
    record: ProcessRecord,
    runner: Option<Arc<Runner>>,
}

pub(super) struct ManagedProcess {
    pub(super) runner: String,
    pub(super) process_id: ProcessId,
    pub(super) command: String,
    pub(super) started_at_ms: i64,
    pub(super) status: ProcessStatus,
}

pub(super) struct ManagedProcessDetail {
    pub(super) process: ManagedProcess,
    pub(super) output_tail: String,
    pub(super) omitted_bytes: usize,
}

impl RunnerPool {
    pub(super) fn new(
        platform: Option<Arc<PlatformStore>>,
        views: std::sync::Weak<Views>,
        callback_events: tokio::sync::mpsc::UnboundedSender<CallbackEvent>,
    ) -> Self {
        Self {
            runners: Mutex::new(HashMap::new()),
            processes: Mutex::new(HashMap::new()),
            platform,
            views,
            callback_events,
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
    ) -> Result<()> {
        if name.is_empty() {
            bail!("runner name must not be empty");
        }
        if command.is_empty() {
            bail!("runner command must not be empty");
        }

        let views = self.views.upgrade().context("controller is stopping")?;
        let info = RunnerInfo {
            name: name.clone(),
            description: description.clone(),
            approval,
        };
        let mut runners = self.runners.lock().await;
        views.start_runner_launch(info.clone()).await?;
        if let Some(slot) = runners.get_mut(&name) {
            slot.watcher.abort();
        }

        let result = async {
            let existing = runners.get(&name).map(|slot| Arc::clone(&slot.runner));
            if let Some(runner) = existing
                && runner
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
                let info = {
                    let config = runner.config.lock().await;
                    RunnerInfo {
                        name: name.clone(),
                        description: config.description.clone(),
                        approval: config.approval,
                    }
                };
                let watcher = watch_runner(Arc::downgrade(&views), info, Arc::clone(&runner));
                runners.insert(name.clone(), RunnerSlot { runner, watcher });
                return Ok(());
            }

            let runner = Arc::new(
                Runner::start(
                    &name,
                    description,
                    approval,
                    command,
                    self.platform.clone(),
                    self.callback_events.clone(),
                )
                .await?,
            );
            runner.sync_skills(skill_store, generation).await?;
            let watcher = watch_runner(Arc::downgrade(&views), info.clone(), Arc::clone(&runner));
            runners.insert(name.clone(), RunnerSlot { runner, watcher });
            Ok(())
        }
        .await;

        let lifecycle = match &result {
            Ok(()) => RunnerLifecycle::Running,
            Err(error) => RunnerLifecycle::Failed {
                message: format!("{error:#}"),
            },
        };
        views
            .apply_controller(ControllerOperation::RunnerUpdated {
                runner: RunnerState::new(info, lifecycle),
            })
            .await?;
        result
    }

    pub(super) async fn list(&self) -> Result<Vec<RunnerInfo>> {
        let runners = self
            .runners
            .lock()
            .await
            .iter()
            .map(|(name, slot)| (name.clone(), Arc::clone(&slot.runner)))
            .collect::<Vec<_>>();
        let mut result = Vec::new();
        for (name, runner) in &runners {
            if runner
                .child
                .lock()
                .await
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_some()
            {
                continue;
            }
            let config = runner.config.lock().await;
            result.push(RunnerInfo {
                name: name.clone(),
                description: config.description.clone(),
                approval: config.approval,
            });
        }
        result.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(result)
    }

    pub(super) async fn get(&self, name: &str) -> Result<Arc<Runner>> {
        let runner = self
            .runners
            .lock()
            .await
            .get(name)
            .map(|slot| Arc::clone(&slot.runner))
            .with_context(|| format!("runner {name} is not running"))?;
        if runner
            .child
            .lock()
            .await
            .try_wait()
            .with_context(|| format!("failed to inspect runner {name}"))?
            .is_some()
        {
            bail!("runner {name} is not running");
        }
        Ok(runner)
    }

    pub(super) async fn sync_skills(
        &self,
        store: &Store,
        generation: &skills::SkillGeneration,
    ) -> Result<()> {
        let runners = self
            .runners
            .lock()
            .await
            .iter()
            .map(|(name, slot)| (name.clone(), Arc::clone(&slot.runner)))
            .collect::<Vec<_>>();
        for (name, runner) in runners {
            if runner
                .child
                .lock()
                .await
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_some()
            {
                continue;
            }
            runner
                .sync_skills(store, generation)
                .await
                .with_context(|| format!("failed to synchronize skills to runner {name}"))?;
        }
        Ok(())
    }

    pub(super) async fn process(&self, key: &ProcessKey) -> Option<ProcessRecord> {
        self.processes.lock().await.get(key).cloned()
    }

    pub(super) async fn insert_process(&self, key: ProcessKey, record: ProcessRecord) -> bool {
        use std::collections::hash_map::Entry;

        match self.processes.lock().await.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(record);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    pub(super) async fn remove_process(&self, key: &ProcessKey) {
        self.processes.lock().await.remove(key);
    }

    pub(super) async fn take_thread_processes(&self, thread_ids: &[ThreadId]) -> Vec<TakenProcess> {
        let runners = self.runners.lock().await;
        let mut processes = self.processes.lock().await;
        let keys = processes
            .keys()
            .filter(|key| thread_ids.contains(&key.thread_id))
            .cloned()
            .collect::<Vec<_>>();
        keys.into_iter()
            .map(|key| TakenProcess {
                runner: runners
                    .get(&key.runner)
                    .map(|slot| Arc::clone(&slot.runner)),
                record: processes
                    .remove(&key)
                    .expect("process key was collected under the same lock"),
                key,
            })
            .collect()
    }

    pub(super) async fn stop_taken_processes(processes: Vec<TakenProcess>) {
        for process in processes {
            let result = match process.runner {
                Some(runner) => runner.stop(process.record.handle).await.map(|_| ()),
                None => Err(anyhow::anyhow!(
                    "runner {} is no longer available",
                    process.key.runner
                )),
            };
            if let Err(error) = result {
                tracing::warn!(
                    process_id = %process.key.process_id,
                    error = %format!("{error:#}"),
                    "failed to stop process while deleting its thread"
                );
            }
        }
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

    pub(super) async fn list_processes(&self, thread_id: ThreadId) -> Vec<ManagedProcess> {
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
            processes.push(ManagedProcess {
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
    ) -> ManagedProcessDetail {
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
        ManagedProcessDetail {
            process: ManagedProcess {
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

    pub(super) async fn stop_process(&self, key: &ProcessKey) -> Result<()> {
        let record = self
            .process(key)
            .await
            .context("background process is no longer available")?;
        self.get(&key.runner).await?.stop(record.handle).await?;
        self.remove_process(key).await;
        Ok(())
    }
}

fn watch_runner(
    views: std::sync::Weak<Views>,
    info: RunnerInfo,
    runner: Arc<Runner>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let failure = match runner.child.lock().await.try_wait() {
                Ok(None) => continue,
                Ok(Some(_)) => "Runner exited".to_owned(),
                Err(error) => format!("failed to inspect Runner: {error:#}"),
            };
            let Some(views) = views.upgrade() else {
                return;
            };
            if let Err(error) = views
                .apply_controller(ControllerOperation::RunnerUpdated {
                    runner: RunnerState::new(
                        info.clone(),
                        RunnerLifecycle::Failed { message: failure },
                    ),
                })
                .await
            {
                tracing::error!(runner = info.name, error = %format!("{error:#}"), "failed to update stopped Runner state");
            }
            return;
        }
    })
}
