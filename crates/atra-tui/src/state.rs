use std::collections::HashSet;

use atra_protocol::{
    ApprovalId, BackgroundProcessDetail, CheckpointId, EventSequence, Model, ProcessId,
    ThreadCheckpoint,
};

use crate::{input::InputBuffer, layout::SelectionPoint};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusPane {
    Input,
    Checkpoints,
    Transcript,
}

#[derive(Default)]
pub(crate) enum TurnState {
    #[default]
    Idle,
    Starting,
    Running,
    Reloading,
    Cancelling,
    AwaitingApproval(Approval),
    ResolvingApproval(Approval),
}

impl TurnState {
    pub(crate) fn is_running(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub(crate) fn approval(&self) -> Option<&Approval> {
        match self {
            Self::AwaitingApproval(approval) => Some(approval),
            _ => None,
        }
    }

    pub(crate) fn approval_mut(&mut self) -> Option<&mut Approval> {
        match self {
            Self::AwaitingApproval(approval) => Some(approval),
            _ => None,
        }
    }
}

pub(crate) struct ViewState {
    pub(crate) selection_start: Option<SelectionPoint>,
    pub(crate) selection_end: Option<SelectionPoint>,
    pub(crate) focus: FocusPane,
    pub(crate) transcript_scroll: usize,
    pub(crate) expanded_tools: HashSet<usize>,
    pub(crate) selected_item: Option<usize>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            selection_start: None,
            selection_end: None,
            focus: FocusPane::Input,
            transcript_scroll: 0,
            expanded_tools: HashSet::new(),
            selected_item: None,
        }
    }
}

pub(crate) enum Overlay {
    None,
    Command,
    Help,
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
    pub(crate) id: ApprovalId,
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
    pub(crate) provider_index: usize,
    pub(crate) model_index: usize,
    pub(crate) effort_index: usize,
    pub(crate) query: String,
    pub(crate) stage: ModelPickerStage,
}

pub(crate) enum ModelPickerStage {
    Provider,
    Model,
    Effort,
}

impl ModelPicker {
    pub(crate) fn providers(&self) -> Vec<&str> {
        let mut providers = Vec::new();
        for model in &self.models {
            if !providers.contains(&model.provider.as_str()) {
                providers.push(model.provider.as_str());
            }
        }
        providers
    }

    pub(crate) fn visible_model_indices(&self) -> Vec<usize> {
        let providers = self.providers();
        let Some(provider) = providers.get(self.provider_index) else {
            return Vec::new();
        };
        let query = self.query.to_lowercase();
        self.models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                model.provider == *provider
                    && (query.is_empty()
                        || model.id.to_lowercase().contains(&query)
                        || model.display_name.to_lowercase().contains(&query)
                        || model
                            .description
                            .as_deref()
                            .is_some_and(|description| description.to_lowercase().contains(&query)))
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn selected_model(&self) -> Option<&Model> {
        self.models.get(self.model_index)
    }

    pub(crate) fn select_provider(&mut self, provider_index: usize) {
        self.provider_index = provider_index.min(self.providers().len().saturating_sub(1));
        self.query.clear();
        if let Some(model_index) = self.visible_model_indices().first().copied() {
            self.select_model(model_index);
        }
    }

    pub(crate) fn select_first_visible_model(&mut self) {
        if let Some(model_index) = self.visible_model_indices().first().copied() {
            self.select_model(model_index);
        }
    }

    pub(crate) fn select_model(&mut self, model_index: usize) {
        self.model_index = model_index;
        let model = &self.models[model_index];
        self.effort_index = model
            .supported_reasoning_efforts
            .iter()
            .position(|effort| effort == &model.default_reasoning_effort)
            .unwrap_or(0);
    }
}

pub(crate) struct ThreadPicker {
    pub(crate) selected: usize,
    pub(crate) state: ThreadPickerState,
}

pub(crate) enum ThreadPickerState {
    Browsing,
    ConfirmingDelete,
    Deleting,
}

pub(crate) struct ProcessPicker {
    pub(crate) selected: usize,
    pub(crate) detail: Option<BackgroundProcessDetail>,
    pub(crate) output_scroll: usize,
    pub(crate) state: ProcessPickerState,
}

pub(crate) enum ProcessPickerState {
    Browsing,
    ConfirmingStop {
        runner: String,
        process_id: ProcessId,
    },
}

pub(crate) struct CheckpointPicker {
    pub(crate) checkpoints: Vec<ThreadCheckpoint>,
    pub(crate) selected: usize,
}

pub(crate) enum HistoryAction {
    Rewind {
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
        draft: Option<String>,
    },
    Restore {
        checkpoint_id: CheckpointId,
    },
}
