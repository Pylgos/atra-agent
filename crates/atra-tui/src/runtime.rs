use std::{io, time::Duration};

use anyhow::Result;
use atra_protocol::{ControllerRequest, ControllerResponse};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::{
    app::{App, HistoryChange, TurnCompletion, TurnUpdate, load_transcript},
    controller::{request, request_stream},
};

pub(crate) enum ApprovalDecision {
    Allow,
    Deny { reason: Option<String> },
}

pub(crate) enum HistoryOperation {
    CreateCheckpoint,
    Fork {
        checkpoint_id: Option<i64>,
        sequence: i64,
    },
    Rewind {
        checkpoint_id: Option<i64>,
        sequence: i64,
    },
    Restore {
        checkpoint_id: i64,
    },
}

pub(crate) enum Effect {
    Login {
        endpoint: std::path::PathBuf,
    },
    SelectThread {
        endpoint: std::path::PathBuf,
        thread_id: i64,
    },
    RenameThread {
        endpoint: std::path::PathBuf,
        thread_id: i64,
        display_name: String,
    },
    ChangeModel {
        endpoint: std::path::PathBuf,
        thread_id: i64,
        model: String,
        reasoning_effort: String,
    },
    SendTurn {
        endpoint: std::path::PathBuf,
        thread_id: Option<i64>,
        new_thread_model: Option<(String, String)>,
        message: String,
    },
    ContinueTurn {
        endpoint: std::path::PathBuf,
        thread_id: i64,
    },
    ResolveApproval {
        endpoint: std::path::PathBuf,
        approval_id: u64,
        decision: ApprovalDecision,
    },
    CancelTurn {
        endpoint: std::path::PathBuf,
        thread_id: i64,
    },
    LoadCheckpoints {
        endpoint: std::path::PathBuf,
        thread_id: i64,
    },
    LoadCheckpoint {
        endpoint: std::path::PathBuf,
        checkpoint: atra_protocol::ThreadCheckpoint,
    },
    HistoryRequest {
        endpoint: std::path::PathBuf,
        thread_id: i64,
        draft: Option<String>,
        operation: HistoryOperation,
    },
    PollProcesses {
        endpoint: std::path::PathBuf,
        thread_id: i64,
        selected: Option<(String, String)>,
    },
    StopProcess {
        endpoint: std::path::PathBuf,
        thread_id: i64,
        runner: String,
        process_id: String,
    },
}

pub(super) async fn run(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let mut events = EventStream::new();
    let (effects, mut pending_effects) = mpsc::unbounded_channel();
    let (updates, mut pending_updates) = mpsc::unbounded_channel();
    let mut redraw = tokio::time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut process_poll = tokio::time::interval(Duration::from_secs(1));
    process_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    terminal.draw(|frame| app.render(frame))?;
    redraw.tick().await;
    let mut dirty = false;
    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event.transpose()? else {
                    return Ok(());
                };
                match event {
                    Event::Key(key) if app.handle_key(key, &effects)? => return Ok(()),
                    Event::Mouse(mouse) => app.handle_mouse(mouse)?,
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                dirty = true;
            }
            Some(update) = pending_updates.recv() => {
                app.update(update, &effects)?;
                dirty = true;
            }
            Some(effect) = pending_effects.recv() => {
                effect.start(updates.clone());
            }
            _ = process_poll.tick() => {
                app.poll_processes(&effects);
            }
            _ = redraw.tick() => {
                if dirty {
                    terminal.draw(|frame| app.render(frame))?;
                    dirty = false;
                }
            }
        }
    }
}

impl Effect {
    fn start(self, updates: mpsc::UnboundedSender<TurnUpdate>) {
        tokio::spawn(async move {
            match self {
                Self::Login { endpoint } => {
                    let result = request(&endpoint, ControllerRequest::CodexLogin).await;
                    let _ = updates.send(TurnUpdate::LoginCompleted(result));
                }
                Self::SelectThread {
                    endpoint,
                    thread_id,
                } => {
                    let result = load_transcript(&endpoint, thread_id).await;
                    let _ = updates.send(TurnUpdate::ThreadSelected { thread_id, result });
                }
                Self::RenameThread {
                    endpoint,
                    thread_id,
                    display_name,
                } => {
                    let result = request(
                        &endpoint,
                        ControllerRequest::ThreadRename {
                            thread_id,
                            display_name: display_name.clone(),
                        },
                    )
                    .await;
                    let _ = updates.send(TurnUpdate::ThreadRenamed {
                        thread_id,
                        display_name,
                        result,
                    });
                }
                Self::ChangeModel {
                    endpoint,
                    thread_id,
                    model,
                    reasoning_effort,
                } => {
                    let result = request(
                        &endpoint,
                        ControllerRequest::ThreadSetModel {
                            thread_id,
                            model: model.clone(),
                            reasoning_effort: reasoning_effort.clone(),
                        },
                    )
                    .await;
                    let _ = updates.send(TurnUpdate::ModelChanged {
                        thread_id,
                        model,
                        reasoning_effort,
                        result,
                    });
                }
                Self::SendTurn {
                    endpoint,
                    thread_id,
                    new_thread_model,
                    message,
                } => {
                    let result =
                        send_turn(&endpoint, thread_id, new_thread_model, message, &updates).await;
                    let _ = updates.send(TurnUpdate::Completed(result));
                }
                Self::ContinueTurn {
                    endpoint,
                    thread_id,
                } => {
                    let result = request_stream(
                        &endpoint,
                        ControllerRequest::ThreadContinue { thread_id },
                        thread_id,
                        &updates,
                    )
                    .await
                    .map(|response| TurnCompletion {
                        thread_id,
                        response,
                    });
                    let _ = updates.send(TurnUpdate::Completed(result));
                }
                Self::ResolveApproval {
                    endpoint,
                    approval_id,
                    decision,
                } => {
                    let approval_request = match decision {
                        ApprovalDecision::Allow => ControllerRequest::ApprovalAllow { approval_id },
                        ApprovalDecision::Deny { reason } => ControllerRequest::ApprovalDeny {
                            approval_id,
                            reason,
                        },
                    };
                    let result = request(&endpoint, approval_request).await;
                    let _ = updates.send(TurnUpdate::ApprovalResolved {
                        approval_id,
                        result,
                    });
                }
                Self::CancelTurn {
                    endpoint,
                    thread_id,
                } => {
                    let result =
                        request(&endpoint, ControllerRequest::ThreadCancel { thread_id }).await;
                    let _ = updates.send(TurnUpdate::CancelCompleted { thread_id, result });
                }
                Self::LoadCheckpoints {
                    endpoint,
                    thread_id,
                } => {
                    let result = async {
                        let checkpoints = match request(
                            &endpoint,
                            ControllerRequest::ThreadCheckpointList { thread_id },
                        )
                        .await?
                        {
                            ControllerResponse::ThreadCheckpointList { checkpoints } => checkpoints,
                            ControllerResponse::Error { message } => anyhow::bail!("{message}"),
                            response => anyhow::bail!(
                                "controller returned an unexpected response: {response:?}"
                            ),
                        };
                        let events = match checkpoints.first() {
                            Some(checkpoint) => checkpoint_events(&endpoint, checkpoint.id).await?,
                            None => Vec::new(),
                        };
                        Ok((checkpoints, events))
                    }
                    .await;
                    let _ = updates.send(TurnUpdate::CheckpointsLoaded { thread_id, result });
                }
                Self::LoadCheckpoint {
                    endpoint,
                    checkpoint,
                } => {
                    let result = checkpoint_events(&endpoint, checkpoint.id)
                        .await
                        .map(|events| (checkpoint, events));
                    let _ = updates.send(TurnUpdate::CheckpointLoaded(result));
                }
                Self::HistoryRequest {
                    endpoint,
                    thread_id,
                    draft,
                    operation,
                } => {
                    let result = async {
                        let history_request = match operation {
                            HistoryOperation::CreateCheckpoint => {
                                ControllerRequest::ThreadCheckpointCreate { thread_id }
                            }
                            HistoryOperation::Fork {
                                checkpoint_id,
                                sequence,
                            } => ControllerRequest::ThreadFork {
                                thread_id,
                                checkpoint_id,
                                sequence,
                                display_name: None,
                            },
                            HistoryOperation::Rewind {
                                checkpoint_id,
                                sequence,
                            } => ControllerRequest::ThreadRewind {
                                thread_id,
                                checkpoint_id,
                                sequence,
                            },
                            HistoryOperation::Restore { checkpoint_id } => {
                                ControllerRequest::ThreadCheckpointRestore {
                                    thread_id,
                                    checkpoint_id,
                                }
                            }
                        };
                        let response = request(&endpoint, history_request).await?;
                        if let ControllerResponse::Error { message } = response {
                            anyhow::bail!("{message}");
                        }
                        let selected_thread_id = match &response {
                            ControllerResponse::ThreadForked { thread_id } => *thread_id,
                            _ => thread_id,
                        };
                        let threads =
                            match request(&endpoint, ControllerRequest::ThreadList).await? {
                                ControllerResponse::ThreadList { threads } => threads,
                                ControllerResponse::Error { message } => anyhow::bail!("{message}"),
                                response => anyhow::bail!(
                                    "controller returned an unexpected response: {response:?}"
                                ),
                            };
                        let transcript = load_transcript(&endpoint, selected_thread_id).await?;
                        let (transcript, events) = transcript;
                        Ok(HistoryChange {
                            response,
                            thread_id: selected_thread_id,
                            threads,
                            transcript,
                            events,
                        })
                    }
                    .await;
                    let _ = updates.send(TurnUpdate::HistoryChanged {
                        source_thread_id: thread_id,
                        draft,
                        result,
                    });
                }
                Self::PollProcesses {
                    endpoint,
                    thread_id,
                    selected,
                } => {
                    let result = async {
                        let processes = match request(
                            &endpoint,
                            ControllerRequest::ThreadProcessList { thread_id },
                        )
                        .await?
                        {
                            ControllerResponse::ThreadProcessList { processes } => processes,
                            ControllerResponse::Error { message } => anyhow::bail!("{message}"),
                            response => anyhow::bail!(
                                "controller returned an unexpected process response: {response:?}"
                            ),
                        };
                        let detail = match selected.filter(|(runner, process_id)| {
                            processes.iter().any(|process| {
                                process.runner == *runner && process.process_id == *process_id
                            })
                        }) {
                            Some((runner, process_id)) => match request(
                                &endpoint,
                                ControllerRequest::ThreadProcessInspect {
                                    thread_id,
                                    runner,
                                    process_id,
                                },
                            )
                            .await?
                            {
                                ControllerResponse::ThreadProcessInspect { process } => {
                                    Some(process)
                                }
                                ControllerResponse::Error { .. } => None,
                                response => anyhow::bail!(
                                    "controller returned an unexpected process response: {response:?}"
                                ),
                            },
                            None => None,
                        };
                        Ok((processes, detail))
                    }
                    .await;
                    let _ = updates.send(TurnUpdate::ProcessesLoaded { thread_id, result });
                }
                Self::StopProcess {
                    endpoint,
                    thread_id,
                    runner,
                    process_id,
                } => {
                    let result = request(
                        &endpoint,
                        ControllerRequest::ThreadProcessStop {
                            thread_id,
                            runner: runner.clone(),
                            process_id: process_id.clone(),
                        },
                    )
                    .await;
                    let _ = updates.send(TurnUpdate::ProcessStopped {
                        thread_id,
                        runner,
                        process_id,
                        result,
                    });
                }
            }
        });
    }
}

async fn checkpoint_events(
    endpoint: &std::path::Path,
    checkpoint_id: i64,
) -> Result<Vec<atra_protocol::ThreadEvent>> {
    match request(
        endpoint,
        ControllerRequest::ThreadCheckpointEvents { checkpoint_id },
    )
    .await?
    {
        ControllerResponse::ThreadCheckpointEvents { events } => Ok(events),
        ControllerResponse::Error { message } => anyhow::bail!("{message}"),
        response => anyhow::bail!("controller returned an unexpected response: {response:?}"),
    }
}

async fn send_turn(
    endpoint: &std::path::Path,
    existing_thread_id: Option<i64>,
    new_thread_model: Option<(String, String)>,
    message: String,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<TurnCompletion> {
    let thread_id = match existing_thread_id {
        Some(thread_id) => thread_id,
        None => {
            let thread_id = match request(
                endpoint,
                ControllerRequest::ThreadCreate { display_name: None },
            )
            .await?
            {
                ControllerResponse::ThreadCreated { thread_id } => thread_id,
                ControllerResponse::Error { message } => anyhow::bail!("{message}"),
                response => {
                    anyhow::bail!("controller returned an unexpected response: {response:?}")
                }
            };
            if let Some((model, reasoning_effort)) = new_thread_model {
                match request(
                    endpoint,
                    ControllerRequest::ThreadSetModel {
                        thread_id,
                        model,
                        reasoning_effort,
                    },
                )
                .await?
                {
                    ControllerResponse::ThreadModelChanged => {}
                    ControllerResponse::Error { message } => anyhow::bail!("{message}"),
                    response => {
                        anyhow::bail!("controller returned an unexpected response: {response:?}")
                    }
                }
            }
            let threads = match request(endpoint, ControllerRequest::ThreadList).await? {
                ControllerResponse::ThreadList { threads } => threads,
                ControllerResponse::Error { message } => anyhow::bail!("{message}"),
                response => {
                    anyhow::bail!("controller returned an unexpected response: {response:?}")
                }
            };
            updates
                .send(TurnUpdate::Started {
                    message: message.clone(),
                    thread_id,
                    threads,
                })
                .ok();
            thread_id
        }
    };
    let response = request_stream(
        endpoint,
        ControllerRequest::ThreadSend { thread_id, message },
        thread_id,
        updates,
    )
    .await?;
    Ok(TurnCompletion {
        thread_id,
        response,
    })
}
