use std::path::PathBuf;

use anyhow::Result;
use atra_client::{CheckpointSubscription, ProcessSubscription, ThreadSubscription};
use atra_protocol::{
    ActiveTurn, ApprovalId, CheckpointId, Model, PendingApproval, Thread, ThreadCheckpoint,
    ThreadEvent, ThreadId,
};
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed, options::WordBreakInvariantOptions};

use crate::{
    history,
    input::InputBuffer,
    layout::ViewLayout,
    state::{CheckpointPicker, Overlay, TurnState, ViewState},
    sync::{CheckpointSync, ControllerSync, ProcessSync, ThreadSync},
    transcript::{TranscriptState, transcript_from_events},
};

mod interaction;
mod update;

pub(crate) const COMMAND_HELP: &[(&str, &str)] = &[
    ("/thread", "Select a thread"),
    ("/new", "Start a new thread"),
    ("/model", "Select the model and reasoning effort"),
    ("/checkpoint", "Save the current thread"),
    ("/checkpoints", "Browse saved checkpoints"),
    ("/fork", "Fork at the selected message"),
    ("/rewind", "Rewind to the selected message"),
    ("/restore", "Restore the displayed checkpoint"),
    ("/continue", "Continue an incomplete turn"),
    ("/compact", "Compact the current thread"),
    ("/processes", "Inspect background commands"),
    ("/help", "Show this command list"),
    ("/exit", "Exit Atra"),
];

pub(crate) struct HistoryChange {
    pub(super) thread_id: ThreadId,
    pub(super) subscription: ThreadSubscription,
}

pub(crate) enum Target {
    New {
        model: Option<(String, String, String)>,
    },
    Thread {
        id: ThreadId,
        view: ThreadView,
    },
}

pub(crate) enum ThreadView {
    Live,
    Checkpoint { picker: CheckpointPicker },
}

impl Target {
    pub(crate) fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::New { .. } => None,
            Self::Thread { id, .. } => Some(*id),
        }
    }

    pub(crate) fn is_checkpoint(&self) -> bool {
        matches!(
            self,
            Self::Thread {
                view: ThreadView::Checkpoint { .. },
                ..
            }
        )
    }

    pub(crate) fn checkpoint_picker(&self) -> Option<&CheckpointPicker> {
        match self {
            Self::Thread {
                view: ThreadView::Checkpoint { picker, .. },
                ..
            } => Some(picker),
            Self::New { .. }
            | Self::Thread {
                view: ThreadView::Live,
                ..
            } => None,
        }
    }

    fn checkpoint_picker_mut(&mut self) -> Option<&mut CheckpointPicker> {
        match self {
            Self::Thread {
                view: ThreadView::Checkpoint { picker, .. },
                ..
            } => Some(picker),
            Self::New { .. }
            | Self::Thread {
                view: ThreadView::Live,
                ..
            } => None,
        }
    }

    pub(crate) fn new_thread_model(&self) -> Option<&(String, String, String)> {
        match self {
            Self::New { model } => model.as_ref(),
            Self::Thread { .. } => None,
        }
    }
}

pub(crate) enum TurnUpdate {
    Started {
        thread_id: ThreadId,
        subscription: ThreadSubscription,
    },
    StreamFailed(anyhow::Error),
    ApprovalResolved {
        approval_id: ApprovalId,
        result: Result<()>,
    },
    CancelCompleted {
        thread_id: ThreadId,
        result: Result<()>,
    },
    LoginCompleted(Result<()>),
    ThreadSelected {
        thread_id: ThreadId,
        result: Result<ThreadSubscription>,
    },
    ThreadRenamed {
        result: Result<()>,
    },
    ThreadDeleted {
        thread_id: ThreadId,
        result: Result<()>,
    },
    ModelChanged {
        result: Result<()>,
    },
    CheckpointsLoaded {
        thread_id: ThreadId,
        result: Result<Option<CheckpointSubscription>>,
    },
    CheckpointLoaded {
        checkpoint_id: CheckpointId,
        result: Result<CheckpointSubscription>,
    },
    HistoryChanged {
        source_thread_id: ThreadId,
        draft: Option<String>,
        result: Result<HistoryChange>,
    },
    ProcessesLoaded {
        thread_id: ThreadId,
        result: Result<Option<ProcessSubscription>>,
    },
    ProcessStopped {
        thread_id: ThreadId,
        result: Result<()>,
    },
}

pub(crate) struct App {
    pub(crate) endpoint: PathBuf,
    pub(crate) message_history_path: PathBuf,
    pub(crate) command_history_path: PathBuf,
    pub(crate) target: Target,
    pub(crate) transcript: TranscriptState,
    pub(crate) message_input: InputBuffer,
    pub(crate) command_input: InputBuffer,
    pub(crate) overlay: Overlay,
    pub(crate) word_segmenter: WordSegmenterBorrowed<'static>,
    pub(crate) error: Option<anyhow::Error>,
    pub(crate) login_required: bool,
    pub(crate) login_pending: bool,
    pub(crate) view: ViewState,
    pub(crate) layout: ViewLayout,
    pub(crate) turn: TurnState,
    pub(crate) process_selection_pending: bool,
    pub(crate) controller_subscription: ControllerSync,
    pub(crate) thread_subscription: Option<ThreadSync>,
    pub(crate) checkpoint_subscription: Option<CheckpointSync>,
    pub(crate) process_subscription: Option<ProcessSync>,
}

impl App {
    pub(crate) fn active_turn(&self) -> Option<&ActiveTurn> {
        self.thread_subscription
            .as_ref()
            .and_then(|subscription| subscription.state().active_turn())
    }

    pub(crate) fn pending_approval(&self) -> Option<&PendingApproval> {
        let approval = self
            .active_turn()
            .and_then(|turn| turn.pending_approval())?;
        if matches!(
            self.turn,
            TurnState::ResolvingApproval { approval_id, .. } if approval_id == approval.id()
        ) {
            return None;
        }
        Some(approval)
    }

    pub(crate) fn turn_is_running(&self) -> bool {
        self.turn.is_pending() || self.active_turn().is_some()
    }

    pub(crate) fn reset_turn_interaction(&mut self) {
        self.turn = TurnState::Idle;
    }

    pub(crate) fn checkpoints(&self) -> &[ThreadCheckpoint] {
        self.thread_subscription
            .as_ref()
            .map(|subscription| subscription.state().checkpoints())
            .unwrap_or_default()
    }

    pub(crate) fn checkpoint(&self) -> Option<&ThreadCheckpoint> {
        if !self.target.is_checkpoint() {
            return None;
        }
        self.checkpoint_subscription
            .as_ref()
            .map(|subscription| subscription.state().metadata())
    }

    pub(crate) fn displayed_events(&self) -> &[ThreadEvent] {
        if self.target.is_checkpoint() {
            return self
                .checkpoint_subscription
                .as_ref()
                .map(|subscription| subscription.state().events())
                .unwrap_or_default();
        }
        self.thread_subscription
            .as_ref()
            .map(|subscription| subscription.state().events())
            .unwrap_or_default()
    }

    pub(crate) fn selected_provider(&self) -> Option<&str> {
        self.controller_subscription
            .state()
            .threads()
            .iter()
            .find(|thread| Some(thread.id) == self.target.thread_id())
            .map(|thread| thread.provider.as_str())
            .or_else(|| {
                self.target
                    .new_thread_model()
                    .map(|(provider, _, _)| provider.as_str())
            })
    }

    pub(crate) fn threads(&self) -> &[Thread] {
        self.controller_subscription.state().threads()
    }

    pub(crate) fn models(&self) -> Vec<Model> {
        self.controller_subscription
            .state()
            .providers()
            .iter()
            .flat_map(|provider| provider.models().iter().cloned())
            .collect()
    }

    pub(crate) fn selected_rate_limits(&self) -> Option<&serde_json::Value> {
        let provider = self.selected_provider()?;
        self.controller_subscription
            .state()
            .providers()
            .iter()
            .find(|current| current.id() == provider)
            .and_then(|current| current.rate_limits())
    }

    pub(crate) fn processes(&self) -> &[atra_protocol::ProcessSummary] {
        self.thread_subscription
            .as_ref()
            .map(|subscription| subscription.state().processes())
            .unwrap_or_default()
    }

    pub(super) async fn load(
        endpoint: PathBuf,
        message_history_path: PathBuf,
        command_history_path: PathBuf,
    ) -> Result<Self> {
        let client = atra_client::Client::new(&endpoint);
        let controller_subscription = client.subscribe_controller().await?;
        let threads = controller_subscription.state().threads().to_vec();
        let thread_id = threads.first().map(|thread| thread.id);
        let thread_subscription = match thread_id {
            Some(thread_id) => Some(client.subscribe_thread(thread_id).await?),
            None => None,
        };
        let events = thread_subscription
            .as_ref()
            .map(|subscription| subscription.state().events().to_vec())
            .unwrap_or_default();
        let transcript = transcript_from_events(&events);
        let mut transcript = TranscriptState::new(transcript);
        if let Some(subscription) = &thread_subscription {
            transcript.rebuild(subscription.state());
        }
        let codex = controller_subscription
            .state()
            .providers()
            .iter()
            .find(|provider| provider.id() == "codex");
        let login_required = codex.is_none_or(|provider| {
            !matches!(
                provider.lifecycle(),
                atra_protocol::ProviderLifecycle::LoggedIn { .. }
            )
        });
        let models = controller_subscription
            .state()
            .providers()
            .iter()
            .flat_map(|provider| provider.models().iter().cloned())
            .collect::<Vec<_>>();
        let target = match thread_id {
            Some(id) => Target::Thread {
                id,
                view: ThreadView::Live,
            },
            None => Target::New {
                model: models.first().map(|model| {
                    (
                        model.provider.clone(),
                        model.id.clone(),
                        model.default_reasoning_effort.clone(),
                    )
                }),
            },
        };
        let message_history = history::load(&message_history_path)?;
        let command_history = history::load(&command_history_path)?;
        Ok(Self {
            endpoint,
            message_history_path,
            command_history_path,
            target,
            transcript,
            message_input: InputBuffer::new(message_history, true),
            command_input: InputBuffer::new(command_history, false),
            overlay: Overlay::None,
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            error: None,
            login_required,
            login_pending: false,
            view: ViewState::default(),
            layout: ViewLayout::default(),
            turn: TurnState::Idle,
            process_selection_pending: false,
            controller_subscription: controller_subscription.into(),
            thread_subscription: thread_subscription.map(Into::into),
            checkpoint_subscription: None,
            process_subscription: None,
        })
    }
}
#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
