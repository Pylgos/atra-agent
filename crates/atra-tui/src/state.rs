use std::collections::HashSet;

use atra_protocol::{BackgroundProcessDetail, Model, ThreadCheckpoint};

use crate::{input::InputBuffer, layout::SelectionPoint};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptMode {
    Coding,
    Debug,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusPane {
    Input,
    Checkpoints,
    Transcript,
    Requests,
    Detail,
}

#[derive(Default)]
pub(crate) enum TurnState {
    #[default]
    Idle,
    Starting,
    Running,
    Cancelling,
}

impl TurnState {
    pub(crate) fn is_running(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

pub(crate) struct ViewState {
    pub(crate) selection_start: Option<SelectionPoint>,
    pub(crate) selection_end: Option<SelectionPoint>,
    pub(crate) transcript_mode: TranscriptMode,
    pub(crate) focus: FocusPane,
    pub(crate) transcript_scroll: usize,
    pub(crate) detail_scroll: usize,
    pub(crate) selected_request: Option<usize>,
    pub(crate) raw_request: bool,
    pub(crate) expanded_tools: HashSet<usize>,
    pub(crate) selected_item: Option<usize>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            selection_start: None,
            selection_end: None,
            transcript_mode: TranscriptMode::Coding,
            focus: FocusPane::Input,
            transcript_scroll: 0,
            detail_scroll: 0,
            selected_request: None,
            raw_request: false,
            expanded_tools: HashSet::new(),
            selected_item: None,
        }
    }
}

pub(crate) enum Overlay {
    None,
    Command,
    Help,
    Approval(Approval),
    Rename,
    ModelPicker(ModelPicker),
    ThreadPicker(ThreadPicker),
    Processes(ProcessPicker),
    HistoryConfirmation(HistoryAction),
}

impl Overlay {
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub(crate) fn model_picker(&self) -> Option<&ModelPicker> {
        match self {
            Self::ModelPicker(picker) => Some(picker),
            _ => None,
        }
    }
}

pub(crate) struct Approval {
    pub(crate) id: u64,
    pub(crate) runner: String,
    pub(crate) label: String,
    pub(crate) operation_index: Option<usize>,
    pub(crate) state: ApprovalState,
}

pub(crate) enum ApprovalState {
    Pending,
    EnteringDenyReason(InputBuffer),
}

pub(crate) struct ModelPicker {
    pub(crate) models: Vec<Model>,
    pub(crate) model_index: usize,
    pub(crate) effort_index: usize,
    pub(crate) stage: ModelPickerStage,
}

pub(crate) enum ModelPickerStage {
    Model,
    Effort,
}

pub(crate) struct ThreadPicker {
    pub(crate) selected: usize,
}

pub(crate) struct ProcessPicker {
    pub(crate) selected: usize,
    pub(crate) detail: Option<BackgroundProcessDetail>,
    pub(crate) output_scroll: usize,
    pub(crate) state: ProcessPickerState,
}

pub(crate) enum ProcessPickerState {
    Browsing,
    ConfirmingStop { runner: String, process_id: String },
}

pub(crate) struct CheckpointPicker {
    pub(crate) checkpoints: Vec<ThreadCheckpoint>,
    pub(crate) selected: usize,
}

pub(crate) enum HistoryAction {
    Rewind {
        checkpoint_id: Option<i64>,
        sequence: i64,
        draft: Option<String>,
    },
    Restore {
        checkpoint_id: i64,
    },
}
