use std::collections::HashSet;

use super::*;
use crate::transcript::{
    ToolArtifact, layout_transcript, prepare_transcript, transcript_lines, transcript_text,
};
use atra_protocol::CommandExecutionArtifact;
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
        target: Target::Thread {
            id: 2,
            view: ThreadView::Live,
        },
        transcript: items,
        events: Vec::new(),
        tool_call_previews: HashMap::new(),
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
        processes: Vec::new(),
        process_refresh_pending: false,
    };

    terminal.draw(|frame| app.render(frame)).unwrap();

    insta::assert_snapshot!(terminal.backend().to_string());
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
fn markdown_and_partial_patch_render_before_completion() {
    let mut items = vec![
        TranscriptEntry::message(
            Author::Assistant,
            "# Result\n\n| File | State |\n|---|---|\n| a.rs | **changed** |\n\n```rust\nfn main() {}\n```"
                .to_owned(),
        ),
        TranscriptEntry::new(TranscriptItem::ToolCall {
            name: "apply_patch".to_owned(),
            arguments: Some(serde_json::json!({
                "runner": "local",
                "patch": "*** Update File: src/main.rs\n@@\n-old\n+new",
            })),
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
