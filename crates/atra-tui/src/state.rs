use std::collections::{HashMap, HashSet};

use atra_protocol::{
    CheckpointId, EventSequence, InteractionId, Model, PendingQuestionRequest, ProcessId,
    QuestionAnswer, ThreadId, TurnPhase,
};

use crate::{input::InputBuffer, layout::SelectionPoint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusPane {
    Input,
    Checkpoints,
    Transcript,
}

#[derive(Default)]
pub(crate) enum TurnState {
    #[default]
    Idle,
    Starting {
        phase: TurnPhase,
    },
    Cancelling,
    EnteringDenyReason {
        approval_id: InteractionId,
        reason: InputBuffer,
    },
    ResolvingApproval {
        approval_id: InteractionId,
    },
    AnsweringQuestions(QuestionForm),
}

impl TurnState {
    pub(crate) fn is_pending(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

pub(crate) struct QuestionForm {
    pub(crate) request: PendingQuestionRequest,
    pub(crate) drafts: Vec<QuestionDraft>,
    pub(crate) current: usize,
    pub(crate) mode: QuestionFormMode,
    pub(crate) scroll: usize,
}

pub(crate) struct QuestionDraft {
    pub(crate) selected: usize,
    pub(crate) note: InputBuffer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuestionFormMode {
    Normal,
    Note,
    Confirm,
    Submitting,
}

impl QuestionForm {
    pub(crate) fn new(request: PendingQuestionRequest) -> Self {
        let drafts = request
            .questions
            .iter()
            .map(|_| QuestionDraft {
                selected: 0,
                note: InputBuffer::new(Vec::new(), true),
            })
            .collect();
        Self {
            request,
            drafts,
            current: 0,
            mode: QuestionFormMode::Normal,
            scroll: 0,
        }
    }

    pub(crate) fn id(&self) -> InteractionId {
        self.request.id
    }

    pub(crate) fn answers(&self) -> Vec<QuestionAnswer> {
        self.request
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| QuestionAnswer {
                selected_option: question
                    .options
                    .get(self.drafts[index].selected)
                    .map(|option| option.label.clone()),
                note: self.drafts[index].note.value.clone(),
            })
            .collect()
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
    LoadingCheckpoints,
    NoCheckpoints,
    Operation(OperationOverlay),
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

pub(crate) enum OperationOverlay {
    RenamingThread,
    ChangingModel,
    CreatingCheckpoint,
    ForkingThread,
    RewindingThread,
    RestoringCheckpoint,
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
    pub(crate) collapsed: HashSet<ThreadId>,
}

pub(crate) fn visible_threads<'a>(
    threads: &'a [atra_protocol::Thread],
    collapsed: &HashSet<ThreadId>,
) -> Vec<(&'a atra_protocol::Thread, usize)> {
    let by_parent = threads.iter().fold(
        HashMap::<Option<ThreadId>, Vec<_>>::new(),
        |mut map, thread| {
            map.entry(thread.parent_thread_id).or_default().push(thread);
            map
        },
    );
    fn append<'a>(
        parent: Option<ThreadId>,
        depth: usize,
        map: &HashMap<Option<ThreadId>, Vec<&'a atra_protocol::Thread>>,
        collapsed: &HashSet<ThreadId>,
        output: &mut Vec<(&'a atra_protocol::Thread, usize)>,
    ) {
        if let Some(children) = map.get(&parent) {
            for thread in children {
                output.push((thread, depth));
                if !collapsed.contains(&thread.id) {
                    append(Some(thread.id), depth + 1, map, collapsed, output);
                }
            }
        }
    }
    let mut output = Vec::new();
    append(None, 0, &by_parent, collapsed, &mut output);
    output
}

pub(crate) enum ThreadPickerState {
    Browsing,
    Selecting,
    ConfirmingDelete,
    Deleting,
}

pub(crate) struct ProcessPicker {
    pub(crate) selected: usize,
    pub(crate) output_scroll: usize,
    pub(crate) state: ProcessPickerState,
}

pub(crate) enum ProcessPickerState {
    Browsing,
    Stopping {
        process_id: ProcessId,
    },
    ConfirmingStop {
        runner: String,
        process_id: ProcessId,
    },
}

pub(crate) struct CheckpointPicker {
    pub(crate) selected: CheckpointId,
    pub(crate) loading: bool,
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
