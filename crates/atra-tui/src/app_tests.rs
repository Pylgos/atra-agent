use super::*;

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
        history_path: PathBuf::new(),
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
        input: "next".to_owned(),
        input_cursor: 4,
        input_history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
        word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
        status: "Ready".to_owned(),
        approval: None,
        renaming: false,
        model_picker: None,
        new_thread_model: None,
        login_required: false,
        selection_start: None,
        selection_end: None,
        transcript_layout: TranscriptLayout { rows: Vec::new() },
        sidebar: Rect::default(),
        turn_pending: false,
        transcript_mode: TranscriptMode::Coding,
        focus: FocusPane::Input,
        transcript_scroll: 0,
        detail_scroll: 0,
        selected_request: None,
        raw_request: false,
        expanded_tools: HashSet::new(),
        selected_item: None,
        transcript_area: Rect::default(),
        transcript_scrollbar_area: Rect::default(),
        transcript_max_scroll: 0,
        transcript_scrollbar_thumb_start: 0,
        transcript_scrollbar_thumb_len: 0,
        transcript_scrollbar_drag_offset: None,
        input_area: Rect::default(),
        request_list_area: Rect::default(),
        detail_area: Rect::default(),
        item_areas: Vec::new(),
        transcript_item_ranges: Vec::new(),
    };

    terminal.draw(|frame| app.render(frame)).unwrap();

    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn layout_mapping_does_not_insert_soft_wraps() {
    let mut items = vec![TranscriptEntry::new(TranscriptItem::ToolResult {
        result: serde_json::Value::String("abcdefgh\nsecond".to_owned()),
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
fn collapsed_tool_result_keeps_edges_and_can_expand() {
    let mut items = vec![TranscriptEntry::new(TranscriptItem::ToolResult {
        result: serde_json::Value::String("status\none\ntwo\nthree\nfour\nfive\nsix".to_owned()),
    })];

    prepare_transcript(&mut items, &HashSet::new(), 80);
    let collapsed = transcript_lines(&items, None, Some(0), 80, 0..usize::MAX);
    let collapsed = collapsed
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(collapsed.contains("status\n  one"));
    assert!(collapsed.contains("3 lines omitted"));
    assert!(collapsed.contains("five\n  six"));
    assert_eq!(transcript_ranges(&items).1.len(), 1);

    prepare_transcript(&mut items, &HashSet::from([0]), 80);
    let expanded = transcript_lines(&items, None, Some(0), 80, 0..usize::MAX);
    let expanded = expanded
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(expanded.contains("two\n  three\n  four"));
}
