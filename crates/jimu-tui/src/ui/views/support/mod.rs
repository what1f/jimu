pub(super) mod actions;
pub(super) mod preview_overlay;
pub(super) mod text_selection;

use std::sync::atomic::{AtomicBool, Ordering};

static VIM_NAVIGATION: AtomicBool = AtomicBool::new(false);

pub(super) fn vim_navigation() -> bool {
    VIM_NAVIGATION.load(Ordering::Relaxed)
}

pub(super) fn set_vim_navigation(enabled: bool) {
    VIM_NAVIGATION.store(enabled, Ordering::Relaxed);
}

pub(super) mod syntax {
    use ratatui::{style::Style, text::Span};
    use std::path::Path;

    #[cfg(feature = "syntax")]
    pub use crate::engine::render::syntax::get_syntect;
    #[cfg(feature = "syntax")]
    pub type Highlighter = Option<syntect::easy::HighlightLines<'static>>;
    #[cfg(not(feature = "syntax"))]
    pub type Highlighter = ();

    pub fn by_path(path: &Path) -> Highlighter {
        #[cfg(feature = "syntax")]
        {
            get_syntect().highlight_lines_by_file_path(path)
        }
        #[cfg(not(feature = "syntax"))]
        {
            let _ = path;
        }
    }
    pub fn by_token(token: &str) -> Highlighter {
        #[cfg(feature = "syntax")]
        {
            get_syntect().highlight_lines_for_token(token)
        }
        #[cfg(not(feature = "syntax"))]
        {
            let _ = token;
        }
    }
    pub fn paint(text: &str, highlighter: &mut Highlighter, fallback: Style) -> Vec<Span<'static>> {
        #[cfg(feature = "syntax")]
        {
            crate::engine::render::syntax::highlight_line(
                text,
                highlighter,
                get_syntect(),
                fallback,
            )
        }
        #[cfg(not(feature = "syntax"))]
        {
            let _ = highlighter;
            vec![Span::styled(text.to_owned(), fallback)]
        }
    }
}

pub(super) mod tool_label {
    pub fn titleize_segment(segment: &str) -> String {
        segment
            .split(['_', '-', ' '])
            .filter(|word| !word.is_empty())
            .map(|word| {
                let mut chars = word.chars();
                let first = chars.next().unwrap_or_default();
                first.to_uppercase().chain(chars).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(super) fn markdown_syntax() -> Option<&'static crate::engine::markdown::Syntect> {
    #[cfg(feature = "syntax")]
    {
        Some(syntax::get_syntect())
    }
    #[cfg(not(feature = "syntax"))]
    {
        None
    }
}
