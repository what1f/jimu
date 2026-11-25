use ratatui::layout::{Constraint, Layout, Rect};

use crate::ui::scrollback::appearance::LayoutConfig;

/// Horizontal layout columns for scrollback entries. Selection borders are drawn INTO the outer viewport padding,
/// not as part of this layout. Scrollbar is handled separately.
#[derive(Debug, Clone)]
pub struct HorizontalLayout {
    pub accent: Rect,
    pub left_padding: Rect,
    pub content: Rect,
    pub right_padding: Rect,
}

impl HorizontalLayout {
    /// Accent width is always 1.
    pub const ACCENT: u16 = 1;

    pub fn new(area: Rect, config: &LayoutConfig) -> Self {
        let [accent, left_padding, content, right_padding] = Layout::horizontal([
            Constraint::Length(Self::ACCENT),
            Constraint::Length(config.block_pad_left),
            Constraint::Min(1), // Content takes remaining space
            Constraint::Length(config.block_pad_right),
        ])
        .areas(area);

        Self {
            accent,
            left_padding,
            content,
            right_padding,
        }
    }

    /// `new` with the default config, kept for backwards compatibility.
    pub fn new_default(area: Rect) -> Self {
        Self::new(area, &LayoutConfig::default())
    }

    pub fn chrome_width(config: &LayoutConfig) -> u16 {
        Self::ACCENT + config.block_pad_left + config.block_pad_right
    }

    /// Get the area for rendering entry content (accent through right padding).
    /// This is the area passed to `EntryRenderer`.
    /// Layout: `│A│PL│Content│PR│`
    pub fn entry_content_area(&self) -> Rect {
        Rect {
            x: self.accent.x,
            y: self.accent.y,
            width: self.accent.width
                + self.left_padding.width
                + self.content.width
                + self.right_padding.width,
            height: self.accent.height,
        }
    }

    pub fn accent_area(&self) -> Rect {
        self.accent
    }

    /// Get the content width (for BlockContext).
    pub fn content_width(&self) -> u16 {
        self.content.width
    }

    pub fn entry_area(&self) -> Rect {
        self.entry_content_area()
    }

    /// Get the selection area (extends 1 column into outer padding on both sides). The selection border is drawn INTO
    /// the padding areas.
    pub fn selection_area(&self) -> Rect {
        // Selection extends 1 column left of accent into outer padding and 1 column right of entry into gap_left area
        let x = self.accent.x.saturating_sub(1);
        let width = self.entry_content_area().width + 2; // One column on each side

        Rect {
            x,
            y: self.accent.y,
            width,
            height: self.accent.height,
        }
    }

    /// Create a row-specific layout (same columns, different y/height).
    pub fn for_row(&self, y: u16, height: u16) -> Self {
        Self {
            accent: Rect {
                y,
                height,
                ..self.accent
            },
            left_padding: Rect {
                y,
                height,
                ..self.left_padding
            },
            content: Rect {
                y,
                height,
                ..self.content
            },
            right_padding: Rect {
                y,
                height,
                ..self.right_padding
            },
        }
    }
}
