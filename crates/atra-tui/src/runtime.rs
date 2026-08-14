use std::{io, time::Duration};

use anyhow::{Result, bail};
use atra_client::{Client, SubscriptionError};
use atra_protocol::{
    ApprovalId, CheckpointId, Command as StateCommand, CommandResult, EventSequence, HistoryTarget,
    ProcessId, ProcessLocator, ThreadId,
};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::app::{App, HistoryChange, TurnUpdate};

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
    SelectThread {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    RenameThread {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        display_name: String,
    },
    DeleteThread {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    ChangeModel {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        provider: String,
        model: String,
        reasoning_effort: String,
    },
    SendTurn {
        endpoint: std::path::PathBuf,
        thread_id: Option<ThreadId>,
        new_thread_model: Option<(String, String, String)>,
        message: String,
    },
    ContinueTurn {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
    },
    CompactTurn {
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
        checkpoint_id: Option<CheckpointId>,
    },
    LoadCheckpoint {
        endpoint: std::path::PathBuf,
        checkpoint_id: CheckpointId,
    },
    HistoryRequest {
        endpoint: std::path::PathBuf,
        thread_id: ThreadId,
        draft: Option<String>,
        operation: HistoryOperation,
    },
    SelectProcess {
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
    terminal.draw(|frame| app.render(frame))?;
    redraw.tick().await;
    let mut dirty = false;
    loop {
        tokio::select! {
            result = app.controller_subscription.receive() => {
                let change = match result {
                    Ok(change) => change,
                    Err(error) if is_terminal(&error, atra_protocol::SubscriptionTerminal::ControllerShutdown) => return Ok(()),
                    Err(error) => return Err(error),
                };
                app.apply_controller_change(change);
                dirty = true;
            }
            result = receive_thread(&mut app.thread_subscription) => {
                let change = match result {
                    Ok(change) => change,
                    Err(error) if is_terminal(&error, atra_protocol::SubscriptionTerminal::Deleted) => {
                        app.thread_subscription = None;
                        app.reset_to_new_thread();
                        app.activity = Some(crate::app::Activity::Info("Thread deleted".to_owned()));
                        dirty = true;
                        continue;
                    }
                    Err(error) if is_terminal(&error, atra_protocol::SubscriptionTerminal::ControllerShutdown) => return Ok(()),
                    Err(error) => return Err(error),
                };
                app.apply_thread_change(change);
                dirty = true;
            }
            result = receive_process(&mut app.process_subscription) => {
                match result {
                    Ok(_) => {}
                    Err(error) if is_terminal(&error, atra_protocol::SubscriptionTerminal::Deleted) => {
                        app.process_subscription = None;
                    }
                    Err(error) if is_terminal(&error, atra_protocol::SubscriptionTerminal::ControllerShutdown) => return Ok(()),
                    Err(error) => return Err(error),
                }
                dirty = true;
            }
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
                app.update(update)?;
                dirty = true;
            }
            Some(effect) = pending_effects.recv() => {
                effect.start(updates.clone());
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

fn is_terminal(error: &anyhow::Error, terminal: atra_protocol::SubscriptionTerminal) -> bool {
    error
        .downcast_ref::<SubscriptionError>()
        .is_some_and(|error| error.terminal() == &terminal)
}

async fn receive_thread(
    subscription: &mut Option<crate::sync::ThreadSync>,
) -> Result<atra_protocol::ThreadChange> {
    match subscription {
        Some(subscription) => subscription.receive().await,
        None => std::future::pending().await,
    }
}

async fn receive_process(
    subscription: &mut Option<crate::sync::ProcessSync>,
) -> Result<atra_protocol::ProcessChange> {
    match subscription {
        Some(subscription) => subscription.receive().await,
        None => std::future::pending().await,
    }
}

impl Effect {
    fn start(self, updates: mpsc::UnboundedSender<TurnUpdate>) {
        tokio::spawn(async move {
            match self {
                Self::Login { endpoint } => {
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ProviderLogin {
                            provider: "codex".to_owned(),
                            credential: None,
                        })
                        .await
                        .and_then(accepted);
                    let _ = updates.send(TurnUpdate::LoginCompleted(result));
                }
                Self::SelectThread {
                    endpoint,
                    thread_id,
                } => {
                    let result = Client::new(&endpoint).subscribe_thread(thread_id).await;
                    let _ = updates.send(TurnUpdate::ThreadSelected { thread_id, result });
                }
                Self::RenameThread {
                    endpoint,
                    thread_id,
                    display_name,
                } => {
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ThreadRename {
                            thread_id,
                            display_name: display_name.clone(),
                        })
                        .await
                        .and_then(accepted);
                    let _ = updates.send(TurnUpdate::ThreadRenamed { result });
                }
                Self::DeleteThread {
                    endpoint,
                    thread_id,
                } => {
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ThreadDelete { thread_id })
                        .await
                        .and_then(accepted);
                    let _ = updates.send(TurnUpdate::ThreadDeleted { thread_id, result });
                }
                Self::ChangeModel {
                    endpoint,
                    thread_id,
                    provider,
                    model,
                    reasoning_effort,
                } => {
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ThreadSetModel {
                            thread_id,
                            provider: provider.clone(),
                            model: model.clone(),
                            reasoning_effort: reasoning_effort.clone(),
                        })
                        .await
                        .and_then(accepted);
                    let _ = updates.send(TurnUpdate::ModelChanged { result });
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
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ThreadContinue { thread_id })
                        .await
                        .and_then(accepted);
                    if let Err(error) = result {
                        let _ = updates.send(TurnUpdate::StreamFailed(error));
                    }
                }
                Self::CompactTurn {
                    endpoint,
                    thread_id,
                } => {
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ThreadCompact { thread_id })
                        .await
                        .and_then(accepted);
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
                        ApprovalDecision::Allow => {
                            client
                                .command(StateCommand::ApprovalAllow { approval_id })
                                .await
                        }
                        ApprovalDecision::Deny { reason } => {
                            client
                                .command(StateCommand::ApprovalDeny {
                                    approval_id,
                                    reason,
                                })
                                .await
                        }
                    }
                    .and_then(accepted);
                    let _ = updates.send(TurnUpdate::ApprovalResolved {
                        approval_id,
                        result,
                    });
                }
                Self::CancelTurn {
                    endpoint,
                    thread_id,
                } => {
                    let result = Client::new(&endpoint)
                        .command(StateCommand::ThreadCancel { thread_id })
                        .await
                        .and_then(accepted);
                    let _ = updates.send(TurnUpdate::CancelCompleted { thread_id, result });
                }
                Self::LoadCheckpoints {
                    endpoint,
                    thread_id,
                    checkpoint_id,
                } => {
                    let result = async {
                        let checkpoint = match checkpoint_id {
                            Some(checkpoint_id) => Some(
                                Client::new(&endpoint)
                                    .subscribe_checkpoint(checkpoint_id)
                                    .await?,
                            ),
                            None => None,
                        };
                        Ok(checkpoint)
                    }
                    .await;
                    let _ = updates.send(TurnUpdate::CheckpointsLoaded { thread_id, result });
                }
                Self::LoadCheckpoint {
                    endpoint,
                    checkpoint_id,
                } => {
                    let result = Client::new(&endpoint)
                        .subscribe_checkpoint(checkpoint_id)
                        .await;
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
                                let mut subscription = client.subscribe_thread(thread_id).await?;
                                accepted(
                                    client
                                        .command(StateCommand::ThreadCheckpointCreate { thread_id })
                                        .await?,
                                )?;
                                let checkpoint_id = loop {
                                    if let atra_protocol::ThreadChange::Checkpoint(id) =
                                        subscription.receive().await?
                                    {
                                        break id;
                                    }
                                };
                                (thread_id, format!("Checkpoint {checkpoint_id} created"))
                            }
                            HistoryOperation::Fork {
                                checkpoint_id,
                                sequence,
                            } => (
                                match client
                                    .command(StateCommand::ThreadFork {
                                        thread_id,
                                        checkpoint_id,
                                        sequence,
                                        display_name: None,
                                    })
                                    .await?
                                {
                                    CommandResult::ThreadForked { thread_id } => thread_id,
                                    result => bail!("unexpected command result: {result:?}"),
                                },
                                "Thread forked".to_owned(),
                            ),
                            HistoryOperation::Rewind {
                                checkpoint_id,
                                sequence,
                            } => {
                                accepted(
                                    client
                                        .command(StateCommand::ThreadReplaceHistory {
                                            thread_id,
                                            target: HistoryTarget::Message {
                                                checkpoint_id,
                                                sequence,
                                            },
                                        })
                                        .await?,
                                )?;
                                (thread_id, "Thread rewound".to_owned())
                            }
                            HistoryOperation::Restore { checkpoint_id } => {
                                accepted(
                                    client
                                        .command(StateCommand::ThreadReplaceHistory {
                                            thread_id,
                                            target: HistoryTarget::Checkpoint { checkpoint_id },
                                        })
                                        .await?,
                                )?;
                                (thread_id, "Checkpoint restored".to_owned())
                            }
                        };
                        let subscription = client.subscribe_thread(selected_thread_id).await?;
                        Ok(HistoryChange {
                            message,
                            thread_id: selected_thread_id,
                            subscription,
                        })
                    }
                    .await;
                    let _ = updates.send(TurnUpdate::HistoryChanged {
                        source_thread_id: thread_id,
                        draft,
                        result,
                    });
                }
                Self::SelectProcess {
                    endpoint,
                    thread_id,
                    selected,
                } => {
                    let result = async {
                        let client = Client::new(&endpoint);
                        match selected {
                            Some((runner, process_id)) => client
                                .subscribe_process(ProcessLocator::new(
                                    thread_id, runner, process_id,
                                ))
                                .await
                                .map(Some),
                            None => Ok(None),
                        }
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
                        .command(StateCommand::StopProcess {
                            process: ProcessLocator::new(
                                thread_id,
                                runner.clone(),
                                process_id.clone(),
                            ),
                        })
                        .await
                        .and_then(accepted);
                    let _ = updates.send(TurnUpdate::ProcessStopped {
                        thread_id,
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
    new_thread_model: Option<(String, String, String)>,
    message: String,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<()> {
    let client = Client::new(endpoint);
    let thread_id = match existing_thread_id {
        Some(thread_id) => thread_id,
        None => {
            let thread_id = match client
                .command(StateCommand::ThreadCreate { display_name: None })
                .await?
            {
                CommandResult::ThreadCreated { thread_id } => thread_id,
                result => bail!("unexpected command result: {result:?}"),
            };
            if let Some((provider, model, reasoning_effort)) = new_thread_model {
                accepted(
                    client
                        .command(StateCommand::ThreadSetModel {
                            thread_id,
                            provider,
                            model,
                            reasoning_effort,
                        })
                        .await?,
                )?;
            }
            let subscription = client.subscribe_thread(thread_id).await?;
            updates
                .send(TurnUpdate::Started {
                    thread_id,
                    subscription,
                })
                .ok();
            thread_id
        }
    };
    accepted(
        client
            .command(StateCommand::ThreadSend { thread_id, message })
            .await?,
    )?;
    Ok(())
}

fn accepted(result: CommandResult) -> Result<()> {
    match result {
        CommandResult::Accepted => Ok(()),
        result => bail!("unexpected command result: {result:?}"),
    }
}
