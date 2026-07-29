use std::{io, time::Duration};

use anyhow::Result;
use atra_client::Client;
use atra_protocol::{ApprovalId, CheckpointId, EventSequence, ProcessId, ThreadId};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::{
    app::{App, HistoryChange, TurnUpdate, load_transcript},
    controller::forward_turn,
};

pub(crate) enum ApprovalDecision {
    Allow,
    Deny { reason: Option<String> },
}

pub(crate) enum HistoryOperation {
    CreateCheckpoint,
    Fork {
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
    },
    Rewind {
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
    },
    Restore {
        checkpoint_id: CheckpointId,
    },
}

pub(crate) enum Effect {
    Login {
        endpoint: std::path::PathBuf,
    },
    PollRateLimits {
        endpoint: std::path::PathBuf,
    },
    SelectThread {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    RenameThread {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        display_name: String,
    },
    ChangeModel {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        model: String,
        reasoning_effort: String,
    },
    SendTurn {
        endpoint: std::path::PathBuf,
        thread_id: Option<ThreadId>,
        new_thread_model: Option<(String, String)>,
        message: String,
    },
    ContinueTurn {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    ResolveApproval {
        endpoint: std::path::PathBuf,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    CancelTurn {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    LoadCheckpoints {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    LoadCheckpoint {
        endpoint: std::path::PathBuf,
        checkpoint: atra_protocol::ThreadCheckpoint,
    },
    HistoryRequest {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        draft: Option<String>,
        operation: HistoryOperation,
    },
    PollProcesses {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        selected: Option<(String, ProcessId)>,
    },
    StopProcess {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
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
    let mut rate_limit_poll = tokio::time::interval(Duration::from_secs(60));
    rate_limit_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
            _ = rate_limit_poll.tick(), if !app.login_required => {
                if !app.rate_limit_refresh_pending {
                    app.rate_limit_refresh_pending = true;
                    effects.send(Effect::PollRateLimits {
                        endpoint: app.endpoint.clone(),
                    }).ok();
                }
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
                    let result = Client::new(&endpoint).codex_login().await;
                    let _ = updates.send(TurnUpdate::LoginCompleted(result));
                }
                Self::PollRateLimits { endpoint } => {
                    let result = Client::new(&endpoint).codex_rate_limits().await;
                    let _ = updates.send(TurnUpdate::RateLimitsLoaded(result));
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
                    let result = Client::new(&endpoint)
                        .thread_rename(thread_id, display_name.clone())
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
                    let result = Client::new(&endpoint)
                        .thread_set_model(thread_id, model.clone(), reasoning_effort.clone())
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
                    if let Err(error) =
                        send_turn(&endpoint, thread_id, new_thread_model, message, &updates).await
                    {
                        let _ = updates.send(TurnUpdate::StreamFailed(error));
                    }
                }
                Self::ContinueTurn {
                    endpoint,
                    thread_id,
                } => {
                    let result = async {
                        let stream = Client::new(&endpoint).thread_continue(thread_id).await?;
                        forward_turn(stream, &updates).await
                    }
                    .await;
                    if let Err(error) = result {
                        let _ = updates.send(TurnUpdate::StreamFailed(error));
                    }
                }
                Self::ResolveApproval {
                    endpoint,
                    approval_id,
                    decision,
                } => {
                    let client = Client::new(&endpoint);
                    let result = match decision {
                        ApprovalDecision::Allow => client.approval_allow(approval_id).await,
                        ApprovalDecision::Deny { reason } => {
                            client.approval_deny(approval_id, reason).await
                        }
                    };
                    let _ = updates.send(TurnUpdate::ApprovalResolved {
                        approval_id,
                        result,
                    });
                }
                Self::CancelTurn {
                    endpoint,
                    thread_id,
                } => {
                    let result = Client::new(&endpoint).thread_cancel(thread_id).await;
                    let _ = updates.send(TurnUpdate::CancelCompleted { thread_id, result });
                }
                Self::LoadCheckpoints {
                    endpoint,
                    thread_id,
                } => {
                    let result = async {
                        let client = Client::new(&endpoint);
                        let checkpoints = client.checkpoint_list(thread_id).await?;
                        let events = match checkpoints.first() {
                            Some(checkpoint) => client.checkpoint_events(checkpoint.id).await?,
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
                    let result = Client::new(&endpoint)
                        .checkpoint_events(checkpoint.id)
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
                        let client = Client::new(&endpoint);
                        let (selected_thread_id, message) = match operation {
                            HistoryOperation::CreateCheckpoint => {
                                let checkpoint_id = client.checkpoint_create(thread_id).await?;
                                (thread_id, format!("Checkpoint {checkpoint_id} created"))
                            }
                            HistoryOperation::Fork {
                                checkpoint_id,
                                sequence,
                            } => (
                                client
                                    .thread_fork(thread_id, checkpoint_id, sequence, None)
                                    .await?,
                                "Thread forked".to_owned(),
                            ),
                            HistoryOperation::Rewind {
                                checkpoint_id,
                                sequence,
                            } => {
                                client
                                    .thread_rewind(thread_id, checkpoint_id, sequence)
                                    .await?;
                                (thread_id, "Thread rewound".to_owned())
                            }
                            HistoryOperation::Restore { checkpoint_id } => {
                                client.checkpoint_restore(thread_id, checkpoint_id).await?;
                                (thread_id, "Checkpoint restored".to_owned())
                            }
                        };
                        let threads = client.thread_list().await?;
                        let transcript = load_transcript(&endpoint, selected_thread_id).await?;
                        let (transcript, events) = transcript;
                        Ok(HistoryChange {
                            message,
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
                        let client = Client::new(&endpoint);
                        let processes = client.thread_process_list(thread_id).await?;
                        let detail = match selected.filter(|(runner, process_id)| {
                            processes.iter().any(|process| {
                                process.runner == *runner && process.process_id == *process_id
                            })
                        }) {
                            Some((runner, process_id)) => client
                                .thread_process_inspect(thread_id, runner, process_id)
                                .await
                                .ok(),
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
                    let result = Client::new(&endpoint)
                        .stop_process(thread_id, runner.clone(), process_id.clone())
                        .await
                        .map(|_| ());
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

async fn send_turn(
    endpoint: &std::path::Path,
    existing_thread_id: Option<ThreadId>,
    new_thread_model: Option<(String, String)>,
    message: String,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<()> {
    let client = Client::new(endpoint);
    let thread_id = match existing_thread_id {
        Some(thread_id) => thread_id,
        None => {
            let thread_id = client.thread_create(None).await?;
            if let Some((model, reasoning_effort)) = new_thread_model {
                client
                    .thread_set_model(thread_id, model, reasoning_effort)
                    .await?;
            }
            let threads = client.thread_list().await?;
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
    let stream = client.thread_send(thread_id, message).await?;
    forward_turn(stream, updates).await
}
