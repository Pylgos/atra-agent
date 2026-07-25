use std::ops::Range;

use ratatui::layout::Rect;

#[derive(Clone, Copy)]
pub(crate) struct SelectionPoint {
    pub(crate) offset: usize,
}

pub(crate) struct MappedRow {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) cells: Vec<usize>,
    pub(crate) end: usize,
}

#[derive(Default)]
pub(crate) struct TranscriptLayout {
    pub(crate) rows: Vec<MappedRow>,
}

#[derive(Default)]
pub(crate) struct ViewLayout {
    pub(crate) transcript: TranscriptLayout,
    pub(crate) transcript_area: Rect,
    pub(crate) transcript_scrollbar_area: Rect,
    pub(crate) transcript_max_scroll: usize,
    pub(crate) transcript_scrollbar_thumb_start: u16,
    pub(crate) transcript_scrollbar_thumb_len: u16,
    pub(crate) transcript_scrollbar_drag_offset: Option<u16>,
    pub(crate) input_area: Rect,
    pub(crate) request_list_area: Rect,
    pub(crate) detail_area: Rect,
    pub(crate) item_areas: Vec<(usize, Rect)>,
    pub(crate) transcript_item_ranges: Vec<(usize, Range<usize>)>,
}
