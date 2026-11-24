use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::layout::Rect;

use super::image::{
    GraphicsProtocol, KITTY_PLACEMENT_ID, build_overlay_image_escapes_for_protocol,
    clear_kitty_image, detect_graphics_protocol, fit_image_to_cells,
    prompt_preview_graphics_protocol,
};

static NEXT_OWNER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static OWNER: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static PLACEMENT: std::cell::Cell<Option<LastPlacement>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LastPlacement {
    owner_id: u64,
    cols: u16,
    rows: u16,
    x: u16,
    y: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ownership {
    Static(u64),
    Clear,
}

#[derive(Debug)]
pub struct Escapes {
    bytes: String,
    ownership: Ownership,
    placement: Option<LastPlacement>,
}

impl Escapes {
    fn paint(bytes: String, owner_id: u64, placement: LastPlacement) -> Self {
        Self {
            bytes,
            ownership: Ownership::Static(owner_id),
            placement: Some(placement),
        }
    }

    fn keep(owner_id: u64, placement: LastPlacement) -> Self {
        Self {
            bytes: String::new(),
            ownership: Ownership::Static(owner_id),
            placement: Some(placement),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.bytes
    }

    pub fn into_string(self) -> String {
        self.bytes
    }

    pub fn commit(self) -> String {
        commit(self.ownership, self.placement);
        self.bytes
    }
}

#[derive(Debug, Default)]
pub struct PostFlush {
    bytes: String,
    ownership: Option<Ownership>,
    placement: Option<LastPlacement>,
}

impl PostFlush {
    pub fn plain(bytes: String) -> Self {
        Self {
            bytes,
            ownership: None,
            placement: None,
        }
    }

    pub fn append(&mut self, other: Self) {
        self.bytes.push_str(&other.bytes);
        if let Some(ownership) = other.ownership {
            self.ownership = Some(ownership);
            self.placement = other.placement;
        }
    }

    pub fn append_plain(&mut self, bytes: &str) {
        self.bytes.push_str(bytes);
    }

    pub fn as_str(&self) -> &str {
        &self.bytes
    }

    pub fn write_to(self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(self.bytes.as_bytes())?;
        if let Some(ownership) = self.ownership {
            commit(ownership, self.placement);
        }
        Ok(())
    }
}

impl From<Escapes> for PostFlush {
    fn from(escapes: Escapes) -> Self {
        Self {
            bytes: escapes.bytes,
            ownership: Some(escapes.ownership),
            placement: escapes.placement,
        }
    }
}

pub fn next_owner_id() -> u64 {
    NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed)
}

/// True while inline pixels are believed live on screen.
pub fn has_committed_owner() -> bool {
    current_owner().is_some()
}

pub fn reset_owner() {
    OWNER.with(|owner| owner.set(None));
    PLACEMENT.with(|placement| placement.set(None));
}

fn current_owner() -> Option<u64> {
    OWNER.with(|owner| owner.get())
}

fn current_placement() -> Option<LastPlacement> {
    PLACEMENT.with(|placement| placement.get())
}

fn commit(ownership: Ownership, placement: Option<LastPlacement>) {
    OWNER.with(|owner| {
        owner.set(match ownership {
            Ownership::Static(id) => Some(id),
            Ownership::Clear => None,
        });
    });
    PLACEMENT.with(|slot| {
        slot.set(match ownership {
            Ownership::Static(_) => placement,
            Ownership::Clear => None,
        });
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn static_image_for_protocol(
    protocol: GraphicsProtocol,
    image_data: &[u8],
    cols: u16,
    rows: u16,
    cell_x: u16,
    cell_y: u16,
    owner_id: u64,
) -> Option<Escapes> {
    let placement = LastPlacement {
        owner_id,
        cols,
        rows,
        x: cell_x,
        y: cell_y,
    };
    if current_owner() == Some(owner_id) && current_placement() == Some(placement) {
        // Prompt treats None as "clear ID 1". Empty Some keeps the pixels.
        return Some(Escapes::keep(owner_id, placement));
    }
    let bytes =
        build_overlay_image_escapes_for_protocol(protocol, image_data, cols, rows, cell_x, cell_y)?;
    Some(Escapes::paint(bytes, owner_id, placement))
}

#[allow(clippy::too_many_arguments)]
pub fn static_image(
    image_data: &[u8],
    cols: u16,
    rows: u16,
    cell_x: u16,
    cell_y: u16,
    owner_id: u64,
) -> Option<Escapes> {
    static_image_for_protocol(
        detect_graphics_protocol(),
        image_data,
        cols,
        rows,
        cell_x,
        cell_y,
        owner_id,
    )
}

pub fn volatile_image(
    image_data: &[u8],
    cols: u16,
    rows: u16,
    cell_x: u16,
    cell_y: u16,
) -> Option<Escapes> {
    let bytes = build_overlay_image_escapes_for_protocol(
        detect_graphics_protocol(),
        image_data,
        cols,
        rows,
        cell_x,
        cell_y,
    )?;
    Some(Escapes {
        bytes,
        ownership: Ownership::Clear,
        placement: None,
    })
}

pub fn static_centered(
    image_data: &[u8],
    img_w: u32,
    img_h: u32,
    overlay_rect: Rect,
    owner_id: u64,
) -> Option<Escapes> {
    let (cols, rows, x, y) = centered_placement(img_w, img_h, overlay_rect)?;
    static_image(image_data, cols, rows, x, y, owner_id)
}

pub fn volatile_centered(
    image_data: &[u8],
    img_w: u32,
    img_h: u32,
    overlay_rect: Rect,
) -> Option<Escapes> {
    let (cols, rows, x, y) = centered_placement(img_w, img_h, overlay_rect)?;
    volatile_image(image_data, cols, rows, x, y)
}

/// Kitty deletes by id; iTerm2 has no delete, but ownership must still reset.
/// Pixels are known-alive only while repainted every frame, so a later identical placement must re-emit or the keep path leaves a blank box.
pub fn clear() -> Option<Escapes> {
    match prompt_preview_graphics_protocol() {
        GraphicsProtocol::Kitty => Some(clear_kitty()),
        GraphicsProtocol::ITerm2 => Some(Escapes {
            bytes: String::new(),
            ownership: Ownership::Clear,
            placement: None,
        }),
        GraphicsProtocol::None => None,
    }
}

pub fn clear_kitty() -> Escapes {
    Escapes {
        bytes: clear_kitty_image(KITTY_PLACEMENT_ID),
        ownership: Ownership::Clear,
        placement: None,
    }
}

fn centered_placement(img_w: u32, img_h: u32, overlay_rect: Rect) -> Option<(u16, u16, u16, u16)> {
    let max_cols = overlay_rect.width.saturating_sub(2);
    let max_rows = overlay_rect.height.saturating_sub(2);
    if max_cols < 4 || max_rows < 2 {
        return None;
    }
    let (cols, rows) = fit_image_to_cells(img_w, img_h, max_cols, max_rows);
    let x = overlay_rect.x + 1 + max_cols.saturating_sub(cols) / 2;
    let y = overlay_rect.y + 1 + max_rows.saturating_sub(rows) / 2;
    Some((cols, rows, x, y))
}
