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
        TranscriptItem::new(Role::User, "hello".to_owned()),
        TranscriptItem::new(
            Role::Assistant,
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
        transcript_layout: TranscriptLayout {
            text: String::new(),
            rows: Vec::new(),
        },
        sidebar: Rect::default(),
        turn_pending: false,
        transcript_mode: TranscriptMode::Coding,
        focus: FocusPane::Input,
        transcript_scroll: 0,
        transcript_horizontal_scroll: 0,
        detail_scroll: 0,
        selected_request: None,
        raw_request: false,
        expanded_tools: HashSet::new(),
        selected_tool: None,
        transcript_area: Rect::default(),
        request_list_area: Rect::default(),
        detail_area: Rect::default(),
        tool_areas: Vec::new(),
    };

    terminal.draw(|frame| app.render(frame)).unwrap();

    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn layout_mapping_does_not_insert_soft_wraps() {
    let items = vec![TranscriptItem::new(
        Role::Assistant,
        "abcdefgh\nsecond".to_owned(),
    )];

    let layout = layout_transcript(&items, Rect::new(1, 1, 4, 8), 0, 0);

    assert_eq!(layout.text, "abcdefgh\nsecond\n");
    assert_eq!(layout.rows[0].cells, vec![0, 1, 2, 3]);
    assert_eq!(&layout.text[0..8], "abcdefgh");
}

#[test]
fn markdown_and_partial_patch_render_before_completion() {
    let items = vec![
        TranscriptItem::new(
            Role::Assistant,
            "# Result\n\n| File | State |\n|---|---|\n| a.rs | **changed** |\n\n```rust\nfn main() {}\n```"
                .to_owned(),
        ),
        TranscriptItem::new(
            Role::Tool,
            "apply_patch *** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new"
                .to_owned(),
        ),
    ];

    let (lines, _) = transcript_lines(&items, None, &HashSet::new(), None);
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
    let items = vec![TranscriptItem::new(
        Role::ToolResult,
        "status\none\ntwo\nthree\nfour\nfive\nsix".to_owned(),
    )];

    let (collapsed, ranges) = transcript_lines(&items, None, &HashSet::new(), Some(0));
    let collapsed = collapsed
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(collapsed.contains("status\none"));
    assert!(collapsed.contains("3 lines omitted"));
    assert!(collapsed.contains("five\nsix"));
    assert_eq!(ranges.len(), 1);

    let (expanded, _) = transcript_lines(&items, None, &HashSet::from([0]), Some(0));
    let expanded = expanded
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(expanded.contains("two\nthree\nfour"));
}
