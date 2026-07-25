use std::collections::HashSet;

use super::*;
use crate::transcript::{
    ToolArtifact, layout_transcript, prepare_transcript, transcript_lines, transcript_text,
};
use ratatui::{Terminal, layout::Rect, text::Line};

#[test]
fn sanitizes_terminal_control_sequences() {
    assert_eq!(
        sanitize("safe\x1b[31m red\x1b[0m\x1b]52;c;bad\x07\nnext"),
        "safe red\nnext"
    );
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
    let mut app = App {
        endpoint: PathBuf::new(),
        message_history_path: PathBuf::new(),
        command_history_path: PathBuf::new(),
        threads: vec![
            Thread {
                id: 2,
                display_name: Some("Current work".to_owned()),
                model: "gpt-5.6-sol".to_owned(),
                reasoning_effort: "medium".to_owned(),
            },
            Thread {
                id: 1,
                display_name: None,
                model: "gpt-5.6-sol".to_owned(),
                reasoning_effort: "medium".to_owned(),
            },
        ],
        models: Vec::new(),
        thread_id: Some(2),
        transcript: items,
        events: Vec::new(),
        tool_call_preview: None,
        message_input: {
            let mut input = InputBuffer::new(Vec::new(), true);
            input.set("next".to_owned());
            input
        },
        command_input: InputBuffer::new(Vec::new(), false),
        overlay: Overlay::None,
        word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
        activity: None,
        new_thread_model: None,
        login_required: false,
        view: ViewState::default(),
        layout: ViewLayout::default(),
        turn: TurnState::Idle,
        metrics_stale: false,
    };

    terminal.draw(|frame| app.render(frame)).unwrap();

    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn layout_mapping_does_not_insert_soft_wraps() {
    let mut items = vec![TranscriptEntry::new(TranscriptItem::ToolResult {
        artifacts: vec![ToolArtifact {
            kind: "command_execution".to_owned(),
            data: serde_json::json!({
                "state": "finished",
                "output": "abcdefgh\nsecond",
                "exit_code": 0,
            }),
        }],
    })];

    prepare_transcript(&mut items, &HashSet::new(), 4);
    let layout = layout_transcript(&items, Rect::new(1, 1, 4, 8), 0);

    let text = transcript_text(&items);
    assert_eq!(text, "abcdefgh\nsecond\n");
    assert_eq!(layout.rows[0].cells, vec![0, 1]);
    assert_eq!(&text[0..8], "abcdefgh");
}

#[test]
fn markdown_and_partial_patch_render_before_completion() {
    let mut items = vec![
        TranscriptEntry::message(
            Author::Assistant,
            "# Result\n\n| File | State |\n|---|---|\n| a.rs | **changed** |\n\n```rust\nfn main() {}\n```"
                .to_owned(),
        ),
        TranscriptEntry::new(TranscriptItem::ToolCall {
            name: "apply_patch".to_owned(),
            arguments: Some(serde_json::Value::String(
                "*** Begin Patch\n*** Environment ID: local\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch"
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
fn approval_response_updates_the_matching_request() {
    let mut transcript = Vec::new();
    push_transcript_item(
        &mut transcript,
        TranscriptItem::Approval {
            id: 7,
            tool: Some("exec_command".to_owned()),
            allowed: None,
        },
    );
    push_transcript_item(
        &mut transcript,
        TranscriptItem::Approval {
            id: 7,
            tool: None,
            allowed: Some(false),
        },
    );

    assert_eq!(transcript.len(), 1);
    assert!(matches!(
        transcript[0].item,
        TranscriptItem::Approval {
            id: 7,
            allowed: Some(false),
            ..
        }
    ));
}
