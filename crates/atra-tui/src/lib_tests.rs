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
        TranscriptItem {
            role: "You",
            text: "hello".to_owned(),
        },
        TranscriptItem {
            role: "Atra",
            text: "a deliberately wrapped response".to_owned(),
        },
    ];
    let backend = ratatui::backend::TestBackend::new(42, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App {
        endpoint: PathBuf::new(),
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
        thread_id: Some(2),
        transcript: items,
        input: "next".to_owned(),
        status: "Ready".to_owned(),
        approval: None,
        renaming: false,
        model_picker: None,
        login_required: false,
        selection_start: None,
        selection_end: None,
        transcript_layout: TranscriptLayout {
            text: String::new(),
            rows: Vec::new(),
        },
        sidebar: Rect::default(),
    };

    terminal.draw(|frame| app.render(frame)).unwrap();

    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn layout_mapping_does_not_insert_soft_wraps() {
    let items = vec![TranscriptItem {
        role: "Atra",
        text: "abcdefgh\nsecond".to_owned(),
    }];

    let layout = layout_transcript(&items, Rect::new(1, 1, 4, 8), 0);

    assert_eq!(layout.text, "abcdefgh\nsecond\n");
    assert_eq!(layout.rows[0].cells, vec![0, 1, 2, 3]);
    assert_eq!(layout.rows[1].cells, vec![4, 5, 6, 7]);
    assert_eq!(&layout.text[0..8], "abcdefgh");
}
