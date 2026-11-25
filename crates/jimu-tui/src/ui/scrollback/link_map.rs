//! VisibleLinkMap: per-frame map of clickable link regions on screen.
//!
//! Populated during the scrollback render pass from the `LinkOverlay` (markdown hyperlinks) and from web_search / web_fetch citation URLs.
//! Used by the mouse handler for click-to-open.

use ratatui::layout::Rect;

use crate::engine::render::osc8::{LinkOverlay, LinkTarget};

/// A clickable link region on screen.
/// A single logical link may span multiple screen rows when word-wrap splits it.
/// Each row segment is a separate `Rect` in `rects`.
#[derive(Debug, Clone)]
pub struct VisibleLink {
    pub rects: Vec<Rect>,
    pub target: LinkTarget,
    pub id: Option<u32>,
}

impl VisibleLink {
    /// Check whether screen position `(col, row)` falls inside any of this link's row segments.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        self.rects
            .iter()
            .any(|r| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
    }

    /// True when painted cell width equals the URL's display width (bare URL text on screen, not a short label or wide citation block).
    pub fn looks_like_bare_url_text(&self) -> bool {
        let LinkTarget::Url(url) = &self.target else {
            return false;
        };
        let painted: usize = self.rects.iter().map(|r| usize::from(r.width)).sum();
        painted == unicode_width::UnicodeWidthStr::width(url.as_ref())
    }
}

/// Per-frame map of visible link regions, with generation-based staleness.
#[derive(Debug, Default)]
pub struct VisibleLinkMap {
    links: Vec<VisibleLink>,
    generation: u64,
}

impl VisibleLinkMap {
    /// Find the link at a given screen position, if any.
    pub fn link_at(&self, col: u16, row: u16) -> Option<&VisibleLink> {
        self.links.iter().find(|link| link.contains(col, row))
    }

    /// Whether this map is stale relative to the current scrollback generation.
    pub fn is_stale(&self, current_generation: u64) -> bool {
        self.generation != current_generation
    }

    /// Rebuild the link map from a `LinkOverlay` and citation URLs. Same `id` alone is not enough: markdown ids restart
    /// per document, so two visible messages can both carry `id=0` for different URLs.
    pub fn rebuild(
        &mut self,
        generation: u64,
        overlay: &LinkOverlay,
        citation_links: Vec<VisibleLink>,
    ) {
        self.rebuild_for_context(
            generation,
            overlay,
            citation_links,
            crate::engine::input::terminal::terminal_context(),
        );
    }

    fn rebuild_for_context(
        &mut self,
        generation: u64,
        overlay: &LinkOverlay,
        citation_links: Vec<VisibleLink>,
        terminal: &crate::engine::input::terminal::TerminalContext,
    ) {
        self.links.clear();
        self.generation = generation;
        self.links
            .reserve(overlay.links().len() + citation_links.len());
        self.push_overlay_links(overlay, /* merge_from */ 0, terminal);
        self.links
            .extend(citation_links.into_iter().filter_map(|mut link| {
                link.target = crate::engine::render::osc8::resolve_link_target_for_context(
                    &link.target,
                    crate::engine::render::osc8::LinkPresentation::Opaque,
                    terminal,
                )?
                .open_target?;
                Some(link)
            }));
    }

    /// Append overlay links without changing generation. The same-id, same-target merge applies only *within this
    /// append*. Callers that re-append the same source every frame must [`Self::truncate`] back to the desired prefix
    /// length first. Otherwise each frame's links will accumulate.
    pub fn append_from_overlay(&mut self, overlay: &LinkOverlay) {
        let start_len = self.links.len();
        self.push_overlay_links(
            overlay,
            start_len,
            crate::engine::input::terminal::terminal_context(),
        );
    }

    /// Push overlay segments, merging same-id, same-target links only with entries at indices `>= merge_from` (0 for rebuild; map length for append).
    fn push_overlay_links(
        &mut self,
        overlay: &LinkOverlay,
        merge_from: usize,
        terminal: &crate::engine::input::terminal::TerminalContext,
    ) {
        self.links.reserve(overlay.links().len());
        for link in overlay.links() {
            let Some(target) = crate::engine::render::osc8::resolve_link_target_for_context(
                &link.target,
                link.presentation,
                terminal,
            )
            .and_then(|resolved| resolved.open_target) else {
                continue;
            };
            let width = link.col_end.saturating_sub(link.col_start);
            if width == 0 {
                continue;
            }
            let rect = Rect::new(link.col_start, link.screen_row, width, 1);
            if let Some(id) = link.id
                && self.links.len() > merge_from
                && let Some(prev) = self.links.last_mut()
                && prev.id == Some(id)
                && prev.target == target
            {
                prev.rects.push(rect);
            } else {
                self.links.push(VisibleLink {
                    rects: vec![rect],
                    target,
                    id: link.id,
                });
            }
        }
    }

    /// Truncate to the first `n` links (used to drop previously-appended overlay links before re-appending for the current frame).
    pub fn truncate(&mut self, n: usize) {
        self.links.truncate(n);
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    pub fn links(&self) -> &[VisibleLink] {
        &self.links
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}
