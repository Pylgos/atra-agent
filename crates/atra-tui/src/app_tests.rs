use std::collections::HashSet;

use super::*;
use crate::transcript::{
    Author, ToolArtifact, TranscriptEntry, TranscriptItem, TranscriptState, layout_transcript,
    prepare_transcript, sanitize, transcript_lines, transcript_text,
};
use crate::ui::{preserve_transcript_viewport, render_model_picker};
use crate::{
    layout::SelectionPoint,
    runtime::Effect,
    state::{
        CheckpointPicker, ModelPicker, ModelPickerStage, Overlay, ThreadPicker, ThreadPickerState,
    },
};
use atra_protocol::{
    CommandExecutionArtifact, RetryStatus, ThreadOperation, ThreadState, TurnPhase,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, layout::Rect, text::Line};

fn test_app(items: Vec<TranscriptEntry>) -> App {
    let threads = vec![
        Thread {
            id: atra_protocol::ThreadId(2),
            display_name: Some("Current work".to_owned()),
            provider: "codex".to_owned(),
            model: "gpt-5.6-sol".to_owned(),
            reasoning_effort: "medium".to_owned(),
        },
        Thread {
            id: atra_protocol::ThreadId(1),
            display_name: None,
            provider: "codex".to_owned(),
            model: "gpt-5.6-sol".to_owned(),
            reasoning_effort: "medium".to_owned(),
        },
    ];
    App {
        endpoint: PathBuf::new(),
        message_history_path: PathBuf::new(),
        command_history_path: PathBuf::new(),
        target: Target::Thread {
            id: atra_protocol::ThreadId(2),
            view: ThreadView::Live,
        },
        transcript: TranscriptState::new(items),
        message_input: {
            let mut input = InputBuffer::new(Vec::new(), true);
            input.set("next".to_owned());
            input
        },
        command_input: InputBuffer::new(Vec::new(), false),
        overlay: Overlay::None,
        word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
        error: None,
        login_required: false,
        login_pending: false,
        view: ViewState::default(),
        layout: ViewLayout::default(),
        turn: TurnState::Idle,
        process_selection_pending: false,
        controller_subscription: crate::sync::ControllerSync::Snapshot(
            atra_protocol::ControllerState::new(
                atra_protocol::ControllerLifecycle::Running,
                threads.clone(),
                Vec::new(),
                Vec::new(),
            ),
        ),
        thread_subscription: Some(crate::sync::ThreadSync::Snapshot(
            atra_protocol::ThreadState::materialize(
                threads[0].clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .unwrap(),
        )),
        checkpoint_subscription: None,
        process_subscription: None,
    }
}

fn set_threads(app: &mut App, threads: Vec<Thread>) {
    app.controller_subscription =
        crate::sync::ControllerSync::Snapshot(atra_protocol::ControllerState::new(
            atra_protocol::ControllerLifecycle::Running,
            threads,
            Vec::new(),
            Vec::new(),
        ));
}

fn set_active_turn(app: &mut App) {
    let Some(crate::sync::ThreadSync::Snapshot(state)) = &mut app.thread_subscription else {
        panic!("test app must use a snapshot thread");
    };
    atra_protocol::ThreadOperation::ActiveTurnStarted {
        phase: atra_protocol::TurnPhase::Running,
    }
    .apply(state)
    .unwrap();
}

fn set_pending_question(app: &mut App, request: atra_protocol::PendingQuestionRequest) {
    set_active_turn(app);
    let Some(crate::sync::ThreadSync::Snapshot(state)) = &mut app.thread_subscription else {
        panic!("test app must use a snapshot thread");
    };
    ThreadOperation::InteractionRequested {
        interaction: atra_protocol::PendingInteraction::Questions(request),
    }
    .apply(state)
    .unwrap();
}

#[test]
fn approval_is_derived_from_the_thread_snapshot() {
    let mut app = test_app(Vec::new());
    set_active_turn(&mut app);
    let Some(crate::sync::ThreadSync::Snapshot(state)) = &mut app.thread_subscription else {
        panic!("test app must use a snapshot thread");
    };
    atra_protocol::ThreadOperation::InteractionRequested {
        interaction: atra_protocol::PendingInteraction::Approval(
            atra_protocol::PendingApproval::new(
                atra_protocol::InteractionId(7),
                "command".to_owned(),
                serde_json::json!({"runner": "local"}),
                Some(2),
                Some("Run tests".to_owned()),
            ),
        ),
    }
    .apply(state)
    .unwrap();

    assert_eq!(
        app.pending_approval().unwrap().id(),
        atra_protocol::InteractionId(7)
    );
    assert!(app.turn_is_running());
    assert!(matches!(app.turn, TurnState::Idle));
}

fn model(provider: &str, id: &str, display_name: &str, description: &str) -> Model {
    Model {
        provider: provider.to_owned(),
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        description: Some(description.to_owned()),
        default_reasoning_effort: "medium".to_owned(),
        supported_reasoning_efforts: vec!["low".to_owned(), "medium".to_owned()],
        context_window: None,
        auto_compact_token_limit: None,
    }
}

#[test]
fn model_picker_selects_provider_and_filters_its_models() {
    let mut app = test_app(Vec::new());
    app.overlay = Overlay::ModelPicker(ModelPicker {
        models: vec![
            model("codex", "gpt-5.6-sol", "GPT 5.6", "Frontier model"),
            model("codex", "gpt-5-mini", "GPT Mini", "Fast model"),
            model("ollama", "qwen3:8b", "Qwen 3", "Local model"),
        ],
        provider_index: 0,
        model_index: 0,
        effort_index: 1,
        query: String::new(),
        stage: ModelPickerStage::Provider,
    });
    let (effects, _pending_effects) = tokio::sync::mpsc::unbounded_channel();

    let Overlay::ModelPicker(picker) = &app.overlay else {
        panic!("model picker was not opened");
    };
    assert!(matches!(picker.stage, ModelPickerStage::Provider));
    assert_eq!(picker.providers(), vec!["codex", "ollama"]);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &effects)
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &effects)
        .unwrap();
    let Overlay::ModelPicker(picker) = &app.overlay else {
        panic!("model picker was closed");
    };
    assert!(matches!(picker.stage, ModelPickerStage::Model));
    assert_eq!(picker.selected_model().unwrap().provider, "ollama");

    for character in "qwen".chars() {
        app.handle_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &effects,
        )
        .unwrap();
    }
    let Overlay::ModelPicker(picker) = &app.overlay else {
        panic!("model picker was closed");
    };
    assert_eq!(picker.query, "qwen");
    assert_eq!(picker.visible_model_indices(), vec![2]);
}

#[test]
fn model_picker_search_matches_id_name_and_description() {
    let mut picker = ModelPicker {
        models: vec![
            model("codex", "alpha", "First", "General model"),
            model("codex", "beta-mini", "Second", "Quick response"),
            model("ollama", "beta-mini", "Local", "Quick response"),
        ],
        provider_index: 0,
        model_index: 0,
        effort_index: 1,
        query: "MINI".to_owned(),
        stage: ModelPickerStage::Model,
    };
    assert_eq!(picker.visible_model_indices(), vec![1]);

    picker.query = "second".to_owned();
    assert_eq!(picker.visible_model_indices(), vec![1]);
    picker.query = "quick".to_owned();
    assert_eq!(picker.visible_model_indices(), vec![1]);
}

#[test]
fn model_picker_keeps_the_last_model_visible_in_a_long_list() {
    let models = (0..20)
        .map(|index| {
            model(
                "codex",
                &format!("model-{index:02}"),
                &format!("Model {index:02}"),
                "Description",
            )
        })
        .collect::<Vec<_>>();
    let picker = ModelPicker {
        model_index: models.len() - 1,
        models,
        provider_index: 0,
        effort_index: 1,
        query: String::new(),
        stage: ModelPickerStage::Model,
    };
    let backend = ratatui::backend::TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| render_model_picker(frame, &picker))
        .unwrap();
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("Model 19 · model-19"));
    assert!(!rendered.contains("Model 00 · model-00"));
}

#[test]
fn deleting_the_last_thread_keeps_the_picker_open() {
    let mut app = test_app(Vec::new());
    let only_thread = app.threads()[0].clone();
    set_threads(&mut app, vec![only_thread]);
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 0,
        state: ThreadPickerState::Deleting,
    });
    let thread_id = app.threads()[0].id;
    set_threads(&mut app, Vec::new());
    app.update(TurnUpdate::ThreadDeleted {
        thread_id,
        result: Ok(()),
    })
    .unwrap();

    assert!(matches!(
        app.overlay,
        Overlay::ThreadPicker(ThreadPicker {
            selected: 0,
            state: ThreadPickerState::Browsing,
        })
    ));
    assert!(matches!(app.target, Target::New { .. }));
}

#[test]
fn deleting_a_thread_keeps_the_picker_open_and_clamps_selection() {
    let mut app = test_app(Vec::new());
    let deleted_thread = app.threads()[1].id;
    let remaining_thread = app.threads()[0].clone();
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 1,
        state: ThreadPickerState::Deleting,
    });
    set_threads(&mut app, vec![remaining_thread]);

    app.update(TurnUpdate::ThreadDeleted {
        thread_id: deleted_thread,
        result: Ok(()),
    })
    .unwrap();

    assert!(matches!(
        app.overlay,
        Overlay::ThreadPicker(ThreadPicker {
            selected: 0,
            state: ThreadPickerState::Browsing,
        })
    ));
    assert_eq!(app.target.thread_id(), Some(atra_protocol::ThreadId(2)));
}

#[test]
fn empty_thread_picker_stays_open() {
    let mut app = test_app(Vec::new());
    set_threads(&mut app, Vec::new());
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 0,
        state: ThreadPickerState::Browsing,
    });
    let (effects, mut pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &effects)
        .unwrap();

    assert!(matches!(app.overlay, Overlay::ThreadPicker(_)));
    assert!(app.error.is_none());
    assert!(pending_effects.try_recv().is_err());
}

#[test]
fn thread_picker_handles_threads_disappearing_during_delete_confirmation() {
    let mut app = test_app(Vec::new());
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 1,
        state: ThreadPickerState::ConfirmingDelete,
    });
    set_threads(&mut app, Vec::new());
    let (effects, mut pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &effects,
    )
    .unwrap();

    assert!(pending_effects.try_recv().is_err());
    assert!(matches!(
        app.overlay,
        Overlay::ThreadPicker(ThreadPicker {
            selected: 0,
            state: ThreadPickerState::Browsing,
        })
    ));
}

#[test]
fn thread_picker_clamps_selection_when_threads_shrink() {
    let mut app = test_app(Vec::new());
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 1,
        state: ThreadPickerState::Browsing,
    });
    let remaining_thread = app.threads()[0].clone();
    let remaining_thread_id = remaining_thread.id;
    set_threads(&mut app, vec![remaining_thread]);
    let (effects, mut pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &effects)
        .unwrap();

    assert!(matches!(
        pending_effects.try_recv().unwrap(),
        Effect::SelectThread { thread_id, .. } if thread_id == remaining_thread_id
    ));
    assert!(matches!(
        app.overlay,
        Overlay::ThreadPicker(ThreadPicker {
            selected: 0,
            state: ThreadPickerState::Selecting,
        })
    ));
}

#[test]
fn stale_checkpoint_error_does_not_change_the_current_load() {
    let mut app = test_app(Vec::new());
    let selected = atra_protocol::CheckpointId(2);
    app.target = Target::Thread {
        id: atra_protocol::ThreadId(2),
        view: ThreadView::Checkpoint {
            picker: CheckpointPicker {
                selected,
                loading: true,
            },
        },
    };

    app.update(TurnUpdate::CheckpointLoaded {
        checkpoint_id: atra_protocol::CheckpointId(1),
        result: Err(anyhow::anyhow!("stale failure")),
    })
    .unwrap();

    assert!(app.error.is_none());
    assert!(app.target.checkpoint_picker().unwrap().loading);
}

#[test]
fn checkpoint_error_is_ignored_after_leaving_the_picker() {
    let mut app = test_app(Vec::new());

    app.update(TurnUpdate::CheckpointLoaded {
        checkpoint_id: atra_protocol::CheckpointId(1),
        result: Err(anyhow::anyhow!("stale failure")),
    })
    .unwrap();

    assert!(app.error.is_none());
    assert!(matches!(
        app.target,
        Target::Thread {
            view: ThreadView::Live,
            ..
        }
    ));
}

#[test]
fn current_checkpoint_error_stops_loading_and_is_displayed() {
    let mut app = test_app(Vec::new());
    let selected = atra_protocol::CheckpointId(2);
    app.target = Target::Thread {
        id: atra_protocol::ThreadId(2),
        view: ThreadView::Checkpoint {
            picker: CheckpointPicker {
                selected,
                loading: true,
            },
        },
    };

    app.update(TurnUpdate::CheckpointLoaded {
        checkpoint_id: selected,
        result: Err(anyhow::anyhow!("current failure")),
    })
    .unwrap();

    assert!(!app.target.checkpoint_picker().unwrap().loading);
    assert_eq!(app.error.unwrap().to_string(), "current failure");
}

#[test]
fn operation_errors_use_a_dismissible_modal() {
    let mut app = test_app(Vec::new());
    let history = std::env::temp_dir().join(format!(
        "atra-tui-command-history-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::File::create(&history).unwrap();
    app.command_history_path = history.clone();
    app.overlay = Overlay::Command;
    app.command_input.set("missing".to_owned());
    let (effects, _) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &effects)
        .unwrap();
    assert!(app.error.is_some());

    app.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &effects,
    )
    .unwrap();
    assert!(app.error.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &effects)
        .unwrap();
    assert!(app.error.is_none());
    std::fs::remove_file(history).unwrap();
}

#[test]
fn retry_state_is_rendered_in_the_status_line() {
    let mut app = test_app(Vec::new());
    let mut state =
        ThreadState::materialize(app.threads()[0].clone(), Vec::new(), Vec::new(), Vec::new())
            .unwrap();
    ThreadOperation::ActiveTurnStarted {
        phase: TurnPhase::Running,
    }
    .apply(&mut state)
    .unwrap();
    ThreadOperation::RetryScheduled {
        retry: RetryStatus::new("service overloaded".to_owned(), 2, 5),
    }
    .apply(&mut state)
    .unwrap();
    app.thread_subscription = Some(crate::sync::ThreadSync::Snapshot(state));
    let backend = ratatui::backend::TestBackend::new(96, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| app.render(frame)).unwrap();

    assert!(
        terminal
            .backend()
            .to_string()
            .contains("service overloaded: retrying 2/5 · esc cancel │")
    );
}

#[test]
fn thread_picker_ignores_input_while_deleting() {
    let mut app = test_app(Vec::new());
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 0,
        state: ThreadPickerState::ConfirmingDelete,
    });
    let deleted_thread = app.threads()[0].id;
    let (effects, mut pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &effects,
    )
    .unwrap();
    assert!(matches!(
        pending_effects.try_recv().unwrap(),
        Effect::DeleteThread {
            thread_id,
            ..
        } if thread_id == deleted_thread
    ));
    assert!(matches!(
        app.overlay,
        Overlay::ThreadPicker(ThreadPicker {
            state: ThreadPickerState::Deleting,
            ..
        })
    ));

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &effects)
        .unwrap();

    assert!(pending_effects.try_recv().is_err());
    assert!(matches!(
        app.overlay,
        Overlay::ThreadPicker(ThreadPicker {
            selected: 0,
            state: ThreadPickerState::Deleting,
        })
    ));
}

#[test]
fn sanitizes_terminal_control_sequences() {
    assert_eq!(
        sanitize("safe\x1b[31m red\x1b[0m\x1b]52;c;bad\x07\nnext"),
        "safe red\nnext"
    );
}

#[test]
fn transcript_scroll_follows_new_content_from_bottom() {
    assert_eq!(preserve_transcript_viewport(0, 20, 35), 0);
}

#[test]
fn transcript_scroll_preserves_viewport_when_content_grows() {
    assert_eq!(preserve_transcript_viewport(8, 20, 35), 23);
}

#[test]
fn transcript_scroll_clamps_viewport_when_content_shrinks() {
    assert_eq!(preserve_transcript_viewport(8, 20, 10), 0);
}

#[test]
fn transcript_render_is_stable() {
    let items = vec![
        TranscriptEntry::message(Author::User, "hello".to_owned()),
        TranscriptEntry::message(
            Author::Assistant,
            "a deliberately wrapped response".to_owned(),
        ),
    ];
    let backend = ratatui::backend::TestBackend::new(42, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = test_app(items);

    terminal.draw(|frame| app.render(frame)).unwrap();

    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn mouse_selection_can_be_deleted_in_message_and_command_inputs() {
    let backend = ratatui::backend::TestBackend::new(42, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = test_app(Vec::new());
    let (effects, _pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.message_input.set("abcdef".to_owned());
    terminal.draw(|frame| app.render(frame)).unwrap();
    let message_area = app.layout.input_text_area;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: message_area.x + 1,
        row: message_area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: message_area.x + 4,
        row: message_area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &effects)
        .unwrap();
    assert_eq!(app.message_input.value, "aef");

    app.overlay = Overlay::Command;
    app.command_input.set("checkpoint".to_owned());
    terminal.draw(|frame| app.render(frame)).unwrap();
    let command_area = app.layout.command_input_area;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: command_area.x + 5,
        row: command_area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: command_area.x + 10,
        row: command_area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &effects,
    )
    .unwrap();
    assert_eq!(app.command_input.value, "check");
}

#[test]
fn mouse_selection_edits_the_question_note_in_the_composer() {
    use crate::state::{QuestionForm, QuestionFormMode};
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let request = PendingQuestionRequest {
        id: InteractionId(9),
        questions: vec![Question {
            question: "Add details".to_owned(),
            options: vec![QuestionOption {
                label: "A".to_owned(),
                description: "First option".to_owned(),
            }],
            recommended_options: vec![],
        }],
    };
    let mut form = QuestionForm::new(request);
    form.mode = QuestionFormMode::Note;
    form.drafts[0].note.set("abcdef".to_owned());
    let mut app = test_app(Vec::new());
    app.turn = TurnState::AnsweringQuestions(form);
    let backend = ratatui::backend::TestBackend::new(42, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    let (effects, _pending_effects) = tokio::sync::mpsc::unbounded_channel();

    terminal.draw(|frame| app.render(frame)).unwrap();
    let note_area = app.layout.input_text_area;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: note_area.x + 1,
        row: note_area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: note_area.x + 4,
        row: note_area.y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &effects)
        .unwrap();

    let TurnState::AnsweringQuestions(form) = &app.turn else {
        panic!("question form should remain active");
    };
    assert_eq!(form.drafts[0].note.value, "aef");
    assert_eq!(app.message_input.value, "next");
}

#[test]
fn ctrl_c_prioritizes_input_copy_transcript_copy_cancel_and_clear() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    let mut items = vec![TranscriptEntry::message(
        Author::User,
        "transcript".to_owned(),
    )];
    prepare_transcript(&mut items, &HashSet::new(), 42);
    let mut app = test_app(items);
    let (effects, mut pending_effects) = tokio::sync::mpsc::unbounded_channel();
    app.message_input.set("message".to_owned());
    app.message_input.begin_selection(0);
    app.message_input.extend_selection(3);
    app.view.selection_start = Some(SelectionPoint { offset: 0 });
    app.view.selection_end = Some(SelectionPoint { offset: 3 });
    set_active_turn(&mut app);
    app.handle_key(ctrl_c, &effects).unwrap();
    assert_eq!(app.message_input.value, "message");
    assert!(app.turn_is_running());
    assert!(matches!(app.turn, TurnState::Idle));
    assert!(pending_effects.try_recv().is_err());

    app.message_input.set("message".to_owned());
    app.handle_key(ctrl_c, &effects).unwrap();
    assert!(app.turn_is_running());
    assert!(pending_effects.try_recv().is_err());

    app.clear_selection();
    app.handle_key(ctrl_c, &effects).unwrap();
    assert!(matches!(app.turn, TurnState::Cancelling));
    assert!(matches!(
        pending_effects.try_recv(),
        Ok(Effect::CancelTurn { .. })
    ));

    let mut app = test_app(Vec::new());
    let (effects, _pending_effects) = tokio::sync::mpsc::unbounded_channel();
    app.overlay = Overlay::Command;
    app.command_input.set("checkpoint".to_owned());
    app.command_input.begin_selection(5);
    app.command_input.extend_selection(10);
    set_active_turn(&mut app);
    app.handle_key(ctrl_c, &effects).unwrap();
    assert_eq!(app.command_input.value, "checkpoint");
    assert!(app.turn_is_running());

    let mut app = test_app(Vec::new());
    app.overlay = Overlay::Command;
    app.command_input.set("checkpoint".to_owned());
    app.handle_key(ctrl_c, &effects).unwrap();
    assert!(app.command_input.value.is_empty());
}

#[test]
fn layout_mapping_does_not_insert_soft_wraps() {
    let mut items = vec![TranscriptEntry::new(TranscriptItem::ToolResult {
        artifacts: vec![ToolArtifact::CommandExecution(
            CommandExecutionArtifact::Finished {
                output: "abcdefgh\nsecond".to_owned(),
                exit_code: Some(0),
                runner: "local".to_owned(),
                full_output_path: "/tmp/output".into(),
            },
        )],
        masked: false,
    })];

    prepare_transcript(&mut items, &HashSet::new(), 4);
    let layout = layout_transcript(&items, Rect::new(1, 1, 4, 8), 0);

    let text = transcript_text(&items);
    assert_eq!(text, "finished · exit 0\nabcdefgh\nsecond\n");
    assert_eq!(layout.rows[0].cells, vec![0, 1]);
    let output_start = text.find("abcdefgh").unwrap();
    assert_eq!(&text[output_start..output_start + 8], "abcdefgh");
}

#[test]
fn markdown_and_patch_command_render_before_completion() {
    let mut items = vec![
        TranscriptEntry::message(
            Author::Assistant,
            "# Result\n\n| File | State |\n|---|---|\n| a.rs | **changed** |\n\n```rust\nfn main() {}\n```"
                .to_owned(),
        ),
        TranscriptEntry::new(TranscriptItem::ToolCall {
            name: "command".to_owned(),
            arguments: Some(serde_json::Value::String(
                "*** Runner local\natri patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch\nPATCH"
                    .to_owned(),
            )),
        }),
    ];

    prepare_transcript(&mut items, &HashSet::new(), 80);
    let lines = transcript_lines(&items, None, None, 80, 0..usize::MAX);
    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("Result"));
    assert!(rendered.contains("changed"));
    assert!(rendered.contains("fn main() {}"));
    assert!(rendered.contains("*** Update File: src/main.rs"));
    assert!(rendered.contains("+new"));
}

#[test]
fn question_tool_call_has_a_structured_transcript_rendering() {
    let mut items = vec![TranscriptEntry::new(TranscriptItem::ToolCall {
        name: "question".to_owned(),
        arguments: Some(serde_json::json!({
            "questions": [{
                "question": "Which layout?",
                "options": [
                    {"label": "Stacked", "description": "Keep the transcript visible"},
                    {"label": "Overlay", "description": "Cover the transcript"}
                ],
                "recommended_options": ["Stacked"]
            }]
        })),
    })];

    prepare_transcript(&mut items, &HashSet::new(), 80);
    let rendered = transcript_text(&items);

    assert!(rendered.contains("question"));
    assert!(rendered.contains("1. Which layout?"));
    assert!(rendered.contains("○ Stacked  ★ recommended"));
    assert!(rendered.contains("Keep the transcript visible"));
    assert!(!rendered.contains("recommended_options:"));
}

#[test]
fn question_form_navigation_note_and_confirmation_keys() {
    use crate::state::{QuestionForm, QuestionFormMode};
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let request = PendingQuestionRequest {
        id: InteractionId(9),
        questions: vec![
            Question {
                question: "First?".to_owned(),
                options: vec![QuestionOption {
                    label: "A".to_owned(),
                    description: "First option".to_owned(),
                }],
                recommended_options: vec![],
            },
            Question {
                question: "Second?".to_owned(),
                options: vec![QuestionOption {
                    label: "B".to_owned(),
                    description: "Second option".to_owned(),
                }],
                recommended_options: vec![],
            },
        ],
    };
    let mut app = test_app(Vec::new());
    app.turn = TurnState::AnsweringQuestions(QuestionForm::new(request));
    let (effects, mut received) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &effects)
        .unwrap();
    let TurnState::AnsweringQuestions(form) = &app.turn else {
        panic!("question form should remain active");
    };
    assert_eq!(form.current, 1);

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &effects)
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &effects)
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &effects)
        .unwrap();
    let TurnState::AnsweringQuestions(form) = &app.turn else {
        panic!("question form should remain active");
    };
    assert_eq!(form.mode, QuestionFormMode::Note);
    assert_eq!(form.drafts[1].note.value, "\n");

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &effects)
        .unwrap();
    let TurnState::AnsweringQuestions(form) = &app.turn else {
        panic!("question form should remain active");
    };
    assert_eq!(form.mode, QuestionFormMode::Normal);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &effects)
        .unwrap();
    app.handle_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        &effects,
    )
    .unwrap();
    let TurnState::AnsweringQuestions(form) = &app.turn else {
        panic!("question form should remain active");
    };
    assert_eq!(form.mode, QuestionFormMode::Confirm);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &effects)
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &effects)
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &effects)
        .unwrap();

    assert!(matches!(
        app.turn,
        TurnState::AnsweringQuestions(ref form)
            if form.id() == InteractionId(9)
                && form.mode == QuestionFormMode::Submitting
    ));
    let Effect::ResolveQuestion {
        request_id,
        answers,
        ..
    } = received.try_recv().unwrap()
    else {
        panic!("question answer effect should be emitted");
    };
    assert_eq!(request_id, InteractionId(9));
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].selected_option.as_deref(), Some("A"));
    assert_eq!(answers[1].selected_option, None);
}

#[test]
fn question_form_keeps_the_transcript_visible_and_reuses_the_composer_for_notes() {
    use crate::state::{QuestionForm, QuestionFormMode};
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let request = PendingQuestionRequest {
        id: InteractionId(9),
        questions: vec![Question {
            question: "Choose a layout".to_owned(),
            options: vec![QuestionOption {
                label: "Stacked".to_owned(),
                description: "Keep context visible".to_owned(),
            }],
            recommended_options: vec!["Stacked".to_owned()],
        }],
    };
    let mut form = QuestionForm::new(request);
    form.mode = QuestionFormMode::Note;
    form.drafts[0].note.set("Useful context".to_owned());
    let mut app = test_app(vec![TranscriptEntry::message(
        Author::Assistant,
        "Transcript remains visible".to_owned(),
    )]);
    app.turn = TurnState::AnsweringQuestions(form);
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| app.render(frame)).unwrap();
    let rendered = terminal.backend().to_string();

    assert!(rendered.contains("Transcript remains visible"));
    assert!(rendered.contains("Choose a layout"));
    assert!(rendered.contains("Stacked"));
    assert!(rendered.contains("Note (optional) · 1/1"));
    assert!(rendered.contains("Useful context"));
    assert!(!rendered.contains("Message"));
}

#[test]
fn interaction_sync_restores_a_pending_question_from_the_thread_snapshot() {
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let mut app = test_app(Vec::new());
    set_pending_question(
        &mut app,
        PendingQuestionRequest {
            id: InteractionId(9),
            questions: vec![Question {
                question: "Choose".to_owned(),
                options: vec![QuestionOption {
                    label: "A".to_owned(),
                    description: "First option".to_owned(),
                }],
                recommended_options: vec![],
            }],
        },
    );
    app.turn = TurnState::Cancelling;

    app.update(TurnUpdate::CancelCompleted {
        thread_id: atra_protocol::ThreadId(2),
        result: Err(anyhow::anyhow!("cancel failed")),
    })
    .unwrap();

    assert!(matches!(
        app.turn,
        TurnState::AnsweringQuestions(ref form) if form.id() == InteractionId(9)
    ));
}

#[test]
fn question_form_sanitizes_model_text_when_rendering() {
    use crate::state::QuestionForm;
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let request = PendingQuestionRequest {
        id: InteractionId(9),
        questions: vec![Question {
            question: "Safe question\x1b]52;c;bad-question\x07".to_owned(),
            options: vec![QuestionOption {
                label: "Visible label\x1b]52;c;bad-label\x07".to_owned(),
                description: "\x1b[31mred description\x1b[0m".to_owned(),
            }],
            recommended_options: vec![],
        }],
    };
    for mode in [
        crate::state::QuestionFormMode::Normal,
        crate::state::QuestionFormMode::Confirm,
    ] {
        let mut form = QuestionForm::new(request.clone());
        form.mode = mode;
        let mut app = test_app(Vec::new());
        app.turn = TurnState::AnsweringQuestions(form);
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| app.render(frame)).unwrap();
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("Safe question"));
        assert!(rendered.contains("Visible label"));
        assert!(rendered.contains("red description"));
        assert!(!rendered.contains("bad-question"));
        assert!(!rendered.contains("bad-label"));
        assert!(!rendered.contains('\x1b'));
    }
}

#[test]
fn question_note_scrolls_horizontally_to_keep_the_cursor_visible() {
    use crate::state::{QuestionForm, QuestionFormMode};
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let request = PendingQuestionRequest {
        id: InteractionId(9),
        questions: vec![Question {
            question: "Add details".to_owned(),
            options: vec![QuestionOption {
                label: "A".to_owned(),
                description: "First option".to_owned(),
            }],
            recommended_options: vec![],
        }],
    };
    let mut form = QuestionForm::new(request);
    form.mode = QuestionFormMode::Note;
    form.drafts[0]
        .note
        .set(format!("{}VISIBLE-TAIL", "x".repeat(80)));
    let mut app = test_app(Vec::new());
    app.turn = TurnState::AnsweringQuestions(form);
    let backend = ratatui::backend::TestBackend::new(40, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| app.render(frame)).unwrap();

    let cursor = terminal.backend().cursor_position();
    assert!(cursor.x < 40);
    assert!(cursor.y < 20);
    assert!(terminal.backend().to_string().contains("VISIBLE-TAIL"));
}

#[test]
fn escape_closes_question_error_without_reaching_the_form() {
    use crate::state::{QuestionForm, QuestionFormMode};
    use atra_protocol::{InteractionId, PendingQuestionRequest, Question, QuestionOption};

    let request = PendingQuestionRequest {
        id: InteractionId(9),
        questions: vec![Question {
            question: "First?".to_owned(),
            options: vec![QuestionOption {
                label: "A".to_owned(),
                description: "First option".to_owned(),
            }],
            recommended_options: vec![],
        }],
    };
    let mut form = QuestionForm::new(request);
    form.mode = QuestionFormMode::Confirm;
    let mut app = test_app(Vec::new());
    app.turn = TurnState::AnsweringQuestions(form);
    app.error = Some(anyhow::anyhow!("answer failed"));
    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    assert!(terminal.backend().to_string().contains("answer failed"));

    let (effects, mut received) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &effects)
        .unwrap();

    assert!(app.error.is_none());
    assert!(matches!(
        app.turn,
        TurnState::AnsweringQuestions(ref form) if form.mode == QuestionFormMode::Confirm
    ));
    assert!(received.try_recv().is_err());
}
