use std::collections::HashSet;

use super::*;
use crate::transcript::{
    Author, ToolArtifact, TranscriptItem, TranscriptState, layout_transcript, prepare_transcript,
    sanitize, transcript_lines, transcript_text,
};
use crate::ui::{preserve_transcript_viewport, render_model_picker};
use crate::{
    layout::SelectionPoint,
    runtime::Effect,
    state::{ModelPicker, ModelPickerStage, Overlay, ThreadPicker, ThreadPickerState},
};
use atra_protocol::CommandExecutionArtifact;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, layout::Rect, text::Line};

fn test_app(items: Vec<TranscriptEntry>) -> App {
    App {
        endpoint: PathBuf::new(),
        message_history_path: PathBuf::new(),
        command_history_path: PathBuf::new(),
        threads: vec![
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
        ],
        models: Vec::new(),
        target: Target::Thread {
            id: atra_protocol::ThreadId(2),
            view: ThreadView::Live,
        },
        transcript: TranscriptState::new(items, Vec::new()),
        message_input: {
            let mut input = InputBuffer::new(Vec::new(), true);
            input.set("next".to_owned());
            input
        },
        command_input: InputBuffer::new(Vec::new(), false),
        overlay: Overlay::None,
        word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
        activity: None,
        login_required: false,
        view: ViewState::default(),
        layout: ViewLayout::default(),
        turn: TurnState::Idle,
        metrics_stale: false,
        rate_limits: serde_json::Value::Array(Vec::new()),
        rate_limit_refresh_pending: false,
        processes: Vec::new(),
        process_refresh_pending: false,
    }
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
fn ignores_rate_limits_loaded_for_a_previous_provider() {
    let mut app = test_app(Vec::new());
    app.threads[0].provider = "ollama".to_owned();
    app.rate_limit_refresh_pending = true;
    let snapshots = serde_json::json!([{"limit_id": "codex"}]);
    let (effects, _pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.update(
        TurnUpdate::RateLimitsLoaded {
            provider: "codex".to_owned(),
            result: Ok(snapshots),
        },
        &effects,
    )
    .unwrap();

    assert_eq!(app.rate_limits, serde_json::json!([]));
    assert!(!app.rate_limit_refresh_pending);
}

#[test]
fn deleting_the_last_thread_closes_the_picker() {
    let mut app = test_app(Vec::new());
    app.threads.truncate(1);
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 0,
        state: ThreadPickerState::Deleting,
    });
    let thread_id = app.threads[0].id;
    let (effects, _pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.update(
        TurnUpdate::ThreadDeleted {
            thread_id,
            result: Ok(()),
        },
        &effects,
    )
    .unwrap();

    assert!(app.threads.is_empty());
    assert!(matches!(app.overlay, Overlay::None));
    assert!(matches!(app.target, Target::New { .. }));
}

#[test]
fn empty_thread_picker_closes_before_handling_input() {
    let mut app = test_app(Vec::new());
    app.threads.clear();
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 0,
        state: ThreadPickerState::Browsing,
    });
    let (effects, mut pending_effects) = tokio::sync::mpsc::unbounded_channel();

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &effects)
        .unwrap();

    assert!(matches!(app.overlay, Overlay::None));
    assert!(matches!(
        app.activity,
        Some(Activity::Info(ref message)) if message == "No threads are available"
    ));
    assert!(pending_effects.try_recv().is_err());
}

#[test]
fn thread_picker_ignores_input_while_deleting() {
    let mut app = test_app(Vec::new());
    app.overlay = Overlay::ThreadPicker(ThreadPicker {
        selected: 0,
        state: ThreadPickerState::ConfirmingDelete,
    });
    let deleted_thread = app.threads[0].id;
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
    app.turn = TurnState::Running;
    app.handle_key(ctrl_c, &effects).unwrap();
    assert_eq!(app.message_input.value, "message");
    assert!(matches!(app.turn, TurnState::Running));
    assert!(pending_effects.try_recv().is_err());

    app.message_input.set("message".to_owned());
    app.handle_key(ctrl_c, &effects).unwrap();
    assert!(matches!(app.turn, TurnState::Running));
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
    app.turn = TurnState::Running;
    app.handle_key(ctrl_c, &effects).unwrap();
    assert_eq!(app.command_input.value, "checkpoint");
    assert!(matches!(app.turn, TurnState::Running));

    app.command_input.set("checkpoint".to_owned());
    app.turn = TurnState::Idle;
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
