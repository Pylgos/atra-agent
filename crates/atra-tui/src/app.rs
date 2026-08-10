use std::path::{Path, PathBuf};

use anyhow::Result;
use atra_client::{CancelResult, ProviderLoginStatus, TurnResult};
use atra_protocol::{
    ApprovalId, BackgroundProcess, BackgroundProcessDetail, Model, ProcessId, Thread,
    ThreadCheckpoint, ThreadEvent, ThreadId,
};
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed, options::WordBreakInvariantOptions};

use crate::{
    history,
    input::InputBuffer,
    layout::ViewLayout,
    state::{CheckpointPicker, Overlay, TurnState, ViewState},
    transcript::{TranscriptEntry, TranscriptState, transcript_from_events},
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
    pub(super) message: String,
    pub(super) thread_id: ThreadId,
    pub(super) threads: Vec<Thread>,
    pub(super) transcript: Vec<TranscriptEntry>,
    pub(super) events: Vec<ThreadEvent>,
}

pub(crate) enum Activity {
    Info(String),
    Error(String),
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
    Checkpoint {
        checkpoint: ThreadCheckpoint,
        picker: CheckpointPicker,
    },
}

impl Target {
    pub(crate) fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::New { .. } => None,
            Self::Thread { id, .. } => Some(*id),
        }
    }

    pub(crate) fn checkpoint(&self) -> Option<&ThreadCheckpoint> {
        match self {
            Self::Thread {
                view: ThreadView::Checkpoint { checkpoint, .. },
                ..
            } => Some(checkpoint),
            Self::New { .. }
            | Self::Thread {
                view: ThreadView::Live,
                ..
            } => None,
        }
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
        message: String,
        thread_id: ThreadId,
        threads: Vec<Thread>,
    },
    Stream(atra_client::TurnUpdate),
    StreamFailed(anyhow::Error),
    Compacted {
        thread_id: ThreadId,
        result: Result<(Vec<TranscriptEntry>, Vec<ThreadEvent>)>,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        result: Result<TurnResult>,
    },
    CancelCompleted {
        thread_id: ThreadId,
        result: Result<CancelResult>,
    },
    LoginCompleted(Result<ProviderLoginStatus>),
    RateLimitsLoaded {
        provider: String,
        result: Result<serde_json::Value>,
    },
    ThreadSelected {
        thread_id: ThreadId,
        result: Result<(Vec<TranscriptEntry>, Vec<ThreadEvent>)>,
    },
    ThreadRenamed {
        thread_id: ThreadId,
        display_name: String,
        result: Result<()>,
    },
    ModelChanged {
        thread_id: ThreadId,
        provider: String,
        model: String,
        reasoning_effort: String,
        result: Result<()>,
    },
    CheckpointsLoaded {
        thread_id: ThreadId,
        result: Result<(Vec<ThreadCheckpoint>, Vec<ThreadEvent>)>,
    },
    CheckpointLoaded(Result<(ThreadCheckpoint, Vec<ThreadEvent>)>),
    HistoryChanged {
        source_thread_id: ThreadId,
        draft: Option<String>,
        result: Result<HistoryChange>,
    },
    ProcessesLoaded {
        thread_id: ThreadId,
        result: Result<(Vec<BackgroundProcess>, Option<BackgroundProcessDetail>)>,
    },
    ProcessStopped {
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
        result: Result<()>,
    },
}

pub(crate) struct App {
    pub(crate) endpoint: PathBuf,
    pub(crate) message_history_path: PathBuf,
    pub(crate) command_history_path: PathBuf,
    pub(crate) threads: Vec<Thread>,
    pub(crate) models: Vec<Model>,
    pub(crate) target: Target,
    pub(crate) transcript: TranscriptState,
    pub(crate) message_input: InputBuffer,
    pub(crate) command_input: InputBuffer,
    pub(crate) overlay: Overlay,
    pub(crate) word_segmenter: WordSegmenterBorrowed<'static>,
    pub(crate) activity: Option<Activity>,
    pub(crate) login_required: bool,
    pub(crate) view: ViewState,
    pub(crate) layout: ViewLayout,
    pub(crate) turn: TurnState,
    pub(crate) metrics_stale: bool,
    pub(crate) rate_limits: serde_json::Value,
    pub(crate) rate_limit_refresh_pending: bool,
    pub(crate) processes: Vec<BackgroundProcess>,
    pub(crate) process_refresh_pending: bool,
}

impl App {
    pub(crate) fn selected_provider(&self) -> Option<&str> {
        self.threads
            .iter()
            .find(|thread| Some(thread.id) == self.target.thread_id())
            .map(|thread| thread.provider.as_str())
            .or_else(|| {
                self.target
                    .new_thread_model()
                    .map(|(provider, _, _)| provider.as_str())
            })
    }

    pub(super) async fn load(
        endpoint: PathBuf,
        message_history_path: PathBuf,
        command_history_path: PathBuf,
    ) -> Result<Self> {
        let client = atra_client::Client::new(&endpoint);
        let threads = client.thread_list().await?;
        let thread_id = threads.first().map(|thread| thread.id);
        let (transcript, events) = match thread_id {
            Some(thread_id) => load_transcript(&endpoint, thread_id).await?,
            None => (Vec::new(), Vec::new()),
        };
        let login_required = match client.provider_login_status("codex".to_owned()).await? {
            ProviderLoginStatus::LoginRequired => true,
            ProviderLoginStatus::LoggedIn { .. } => false,
        };
        let models = client.model_list().await.unwrap_or_default();
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
            threads,
            models,
            target,
            transcript: TranscriptState::new(transcript, events),
            message_input: InputBuffer::new(message_history, true),
            command_input: InputBuffer::new(command_history, false),
            overlay: Overlay::None,
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            activity: Some(Activity::Info(if login_required {
                "Codex login required · Ctrl-L login".to_owned()
            } else {
                "/thread · /new · /model · Ctrl-P/Ctrl-/ command · Tab focus · Ctrl-C copies"
                    .to_owned()
            })),
            login_required,
            view: ViewState::default(),
            layout: ViewLayout::default(),
            turn: TurnState::Idle,
            metrics_stale: false,
            rate_limits: serde_json::Value::Array(Vec::new()),
            rate_limit_refresh_pending: false,
            processes: Vec::new(),
            process_refresh_pending: false,
        })
    }
}
pub(super) async fn load_transcript(
    endpoint: &Path,
    thread_id: ThreadId,
) -> Result<(Vec<TranscriptEntry>, Vec<ThreadEvent>)> {
    let events = atra_client::Client::new(endpoint)
        .thread_events(thread_id)
        .await?;
    let transcript = transcript_from_events(&events);
    Ok((transcript, events))
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
