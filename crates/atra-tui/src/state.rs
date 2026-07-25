use std::collections::HashSet;

use atra_protocol::Model;

use crate::{input::InputBuffer, layout::SelectionPoint};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptMode {
    Coding,
    Debug,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusPane {
    Input,
    Transcript,
    Requests,
    Detail,
}

#[derive(Default)]
pub(crate) enum TurnState {
    #[default]
    Idle,
    Running,
}

impl TurnState {
    pub(crate) fn is_running(&self) -> bool {
        matches!(self, Self::Running)
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
    pub(crate) deny_reason: Option<InputBuffer>,
}

pub(crate) struct ModelPicker {
    pub(crate) models: Vec<Model>,
    pub(crate) model_index: usize,
    pub(crate) effort_index: usize,
    pub(crate) selecting_effort: bool,
}

pub(crate) struct ThreadPicker {
    pub(crate) selected: usize,
}
