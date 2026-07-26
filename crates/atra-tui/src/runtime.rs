use std::{io, time::Duration};

use anyhow::Result;
use atra_protocol::{ControllerRequest, ControllerResponse};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::{
    app::{App, TurnCompletion, TurnUpdate, load_transcript},
    controller::{request, request_stream},
};

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
        request: ControllerRequest,
    },
    ResolveApproval {
        endpoint: std::path::PathBuf,
        approval_id: u64,
        request: ControllerRequest,
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
        request: ControllerRequest,
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
                    request: turn_request,
                } => {
                    let result = request_stream(&endpoint, turn_request, thread_id, &updates)
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
                    request: approval_request,
                } => {
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
                    if let Err(error) =
                        request(&endpoint, ControllerRequest::ThreadCancel { thread_id }).await
                    {
                        let _ = updates.send(TurnUpdate::CancelRequestFailed { thread_id, error });
                    }
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
                    request: history_request,
                } => {
                    let result = async {
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
                        Ok((response, selected_thread_id, threads, transcript))
                    }
                    .await;
                    let _ = updates.send(TurnUpdate::HistoryChanged {
                        source_thread_id: thread_id,
                        draft,
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
