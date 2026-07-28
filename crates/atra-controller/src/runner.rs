use super::*;
use crate::tools::format_command_output;

pub(super) struct Runner {
    pub(super) config: Mutex<RunnerConfig>,
    pub(super) child: Mutex<Child>,
    pub(super) client: RunnerClient,
    pub(super) environment: Mutex<CommandEnvironment>,
    skill_digest: Mutex<Option<String>>,
}

pub(super) struct RunnerConfig {
    pub(super) description: String,
    pub(super) approval: ApprovalPolicy,
}

impl Runner {
    pub(super) async fn start(
        name: &str,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
        platform: Option<Arc<PlatformStore>>,
    ) -> Result<Self> {
        tracing::info!(runner = name, executable = command[0], "starting runner");
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start runner {name} using {}", command[0]))?;
        let stdin = child
            .stdin
            .take()
            .context("runner stdin was not available")?;
        let stdout = child
            .stdout
            .take()
            .context("runner stdout was not available")?;
        let stderr = child
            .stderr
            .take()
            .context("runner stderr was not available")?;

        let runner_name = name.to_owned();
        tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match stderr.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        tracing::info!(
                            runner = runner_name,
                            message = line.trim_end(),
                            "runner log"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            runner = runner_name,
                            %error,
                            "failed to read runner log"
                        );
                        break;
                    }
                }
            }
        });

        let client = RunnerClient::new(stdin, stdout, name);
        client
            .initialize()
            .await
            .with_context(|| format!("runner {name} failed to initialize"))?;
        let mut environment = CommandEnvironment::default();
        if let Some(platform) = platform {
            let tools = platform.tools()?;
            let path = deploy_tree(&client, TreeObjects::Platform(platform), tools).await?;
            environment.prepend_path.push(format!("{path}/bin"));
        }
        if child
            .try_wait()
            .with_context(|| format!("failed to inspect runner {name}"))?
            .is_some()
        {
            return Err(anyhow!("runner {name} exited during initialization"));
        }
        tracing::info!(runner = name, "runner ready");

        Ok(Self {
            config: Mutex::new(RunnerConfig {
                description,
                approval,
            }),
            child: Mutex::new(child),
            client,
            environment: Mutex::new(environment),
            skill_digest: Mutex::new(None),
        })
    }

    pub(super) async fn start_command(&self, command: String) -> Result<ProcessHandle> {
        self.client
            .start(command, self.environment.lock().await.clone())
            .await
    }

    pub(super) async fn wait_raw(
        &self,
        process_handle: ProcessHandle,
        timeout_ms: u64,
    ) -> Result<RunnerResponse> {
        self.client.wait(process_handle, timeout_ms).await
    }

    pub(super) async fn wait(
        &self,
        process_handle: ProcessHandle,
        timeout_ms: u64,
    ) -> Result<ControllerResponse> {
        map_runner_response(self.wait_raw(process_handle, timeout_ms).await?)
    }

    pub(super) async fn stop(&self, process_handle: ProcessHandle) -> Result<CommandOutput> {
        self.client.stop(process_handle).await
    }

    pub(super) async fn stop_response(
        &self,
        process_handle: ProcessHandle,
    ) -> Result<ControllerResponse> {
        map_runner_response(RunnerResponse::ProcessStopped {
            output: self.stop(process_handle).await?,
        })
    }

    pub(super) async fn approval(&self) -> ApprovalPolicy {
        self.config.lock().await.approval
    }

    pub(super) async fn sync_skills(
        &self,
        store: &AtraStore,
        generation: &skills::SkillGeneration,
    ) -> Result<()> {
        let digest = generation.manifest.digest();
        if self.skill_digest.lock().await.as_deref() == Some(&digest) {
            return Ok(());
        }
        let mut environment = self.environment.lock().await;
        if generation.manifest.entries.is_empty() {
            environment.set.remove("ATRA_SKILLS");
        } else {
            let path = deploy_tree(
                &self.client,
                TreeObjects::Store(store.clone()),
                generation.manifest.clone(),
            )
            .await?;
            environment
                .set
                .insert("ATRA_SKILLS".to_owned(), format!("{path}/skills"));
        }
        *self.skill_digest.lock().await = Some(digest);
        Ok(())
    }
}

#[derive(Clone)]
enum TreeObjects {
    Platform(Arc<PlatformStore>),
    Store(AtraStore),
}

async fn deploy_tree(
    client: &RunnerClient,
    objects: TreeObjects,
    manifest: TreeManifest,
) -> Result<String> {
    let expected_digest = manifest.digest();
    loop {
        match client.prepare_tree(manifest.clone()).await? {
            PrepareTreeResult::MissingObjects(digests) => {
                for digest in digests {
                    let objects = objects.clone();
                    let object_digest = digest.clone();
                    let (compressed, executable) = tokio::task::spawn_blocking(move || {
                        let mut encoder = zstd::Encoder::new(Vec::new(), 3)
                            .context("failed to compress object")?;
                        let executable = match objects {
                            TreeObjects::Platform(platform) => {
                                platform.copy_object_to(&object_digest, &mut encoder)?
                            }
                            TreeObjects::Store(store) => {
                                store.copy_object_to(&object_digest, &mut encoder)?
                            }
                        };
                        let compressed = encoder.finish().context("failed to finish object")?;
                        Ok::<_, anyhow::Error>((compressed, executable))
                    })
                    .await
                    .context("object compression task failed")??;
                    client
                        .upload_object(digest, executable, STANDARD.encode(compressed))
                        .await?;
                }
            }
            PrepareTreeResult::Ready { digest, path } => {
                if digest != expected_digest {
                    bail!("runner returned tree digest {digest}, expected {expected_digest}");
                }
                return Ok(path);
            }
        }
    }
}

pub(super) fn map_runner_response(response: RunnerResponse) -> Result<ControllerResponse> {
    match response {
        RunnerResponse::Ready => Ok(ControllerResponse::Running),
        RunnerResponse::MissingObjects { .. }
        | RunnerResponse::TreeReady { .. }
        | RunnerResponse::ObjectStored => {
            bail!("runner returned an initialization response after becoming ready")
        }
        RunnerResponse::ProcessStarted { process_handle } => {
            Ok(ControllerResponse::ProcessStarted { process_handle })
        }
        RunnerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(ControllerResponse::ProcessRunning {
            process_handle,
            output: format_command_output(&output),
        }),
        RunnerResponse::ProcessFinished { output, exit_code } => {
            tracing::info!(
                ?exit_code,
                output_bytes = output.content.len(),
                "process finished"
            );
            Ok(ControllerResponse::ProcessFinished {
                output: format_command_output(&output),
                exit_code,
            })
        }
        RunnerResponse::ProcessTimedOut { output } => Ok(ControllerResponse::ProcessTimedOut {
            output: format_command_output(&output),
        }),
        RunnerResponse::ProcessStopped { output } => Ok(ControllerResponse::ProcessStopped {
            output: format_command_output(&output),
        }),
        RunnerResponse::PatchCompleted { .. } => {
            bail!("runner returned an unexpected patch response")
        }
        RunnerResponse::ProcessInspected { .. } => {
            bail!("runner returned an unexpected process inspection response")
        }
        RunnerResponse::ProcessStatus { .. } => {
            bail!("runner returned an unexpected process status response")
        }
        RunnerResponse::Error { message } => bail!("{message}"),
    }
}

pub(super) fn format_exec_response(runner: &str, response: RunnerResponse) -> Result<String> {
    match response {
        RunnerResponse::ProcessStarted { process_handle } => {
            Ok(format!("Process started with ID {process_handle}"))
        }
        RunnerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(append_process_status(
            model_command_output(&output, runner),
            &format!("Process {process_handle} is still running"),
        )),
        RunnerResponse::ProcessFinished {
            output,
            exit_code: Some(0),
        } => {
            let output = model_command_output(&output, runner);
            Ok(if output.is_empty() {
                "Process completed with no output".to_owned()
            } else {
                output
            })
        }
        RunnerResponse::ProcessFinished { output, exit_code } => {
            let exit_code = exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            Ok(append_process_status(
                model_command_output(&output, runner),
                &format!("Process exited with code {exit_code}"),
            ))
        }
        RunnerResponse::ProcessTimedOut { output } => Ok(append_process_status(
            model_command_output(&output, runner),
            "Process timed out",
        )),
        RunnerResponse::Error { message } => bail!("{message}"),
        RunnerResponse::Ready
        | RunnerResponse::MissingObjects { .. }
        | RunnerResponse::TreeReady { .. }
        | RunnerResponse::ObjectStored
        | RunnerResponse::ProcessStopped { .. }
        | RunnerResponse::ProcessInspected { .. }
        | RunnerResponse::ProcessStatus { .. }
        | RunnerResponse::PatchCompleted { .. } => {
            bail!("runner returned an invalid tool response")
        }
    }
}
