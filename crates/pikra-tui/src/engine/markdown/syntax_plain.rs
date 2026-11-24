//! Plain-text code fences for builds without syntax highlighting.

pub struct Syntect;

impl Syntect {
    pub fn new(_theme_bytes: &[u8]) -> Self {
        Self
    }
}

/// No highlighted segments can be produced without a syntax backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntectStyle {}

pub(crate) fn to_anstyle(style: SyntectStyle) -> anstyle::Style {
    match style {}
}

type HighlightedLines = Vec<Vec<(SyntectStyle, String)>>;

pub(crate) fn syntax_highlight_raw(
    _syn: Option<&Syntect>,
    _fence_info: &str,
    _text: &str,
) -> Option<HighlightedLines> {
    None
}

pub(crate) struct OpenCodeHighlighter;

impl OpenCodeHighlighter {
    pub(crate) fn new(_syn: &Syntect) -> Self {
        Self
    }

    pub(crate) fn highlight_block(
        &mut self,
        _syn: &Syntect,
        _fence_info: &str,
        _start_in_tail: usize,
        _body_reaches_eof: bool,
        _text: &str,
    ) -> Option<HighlightedLines> {
        None
    }
}
