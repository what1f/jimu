//! X10 mouse reassembly filter for the input event channel.
//! See [`X10ReassemblyFilter`].

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent};

/// An input event timestamped when it was read from the terminal.
#[derive(Clone, Debug, PartialEq)]
pub struct TimedInputEvent {
    pub event: Event,
    pub arrived_at: Instant,
}

impl TimedInputEvent {
    pub fn now(event: Event) -> Self {
        Self {
            event,
            arrived_at: Instant::now(),
        }
    }
}

/// These coordinates are fixed by the encoding and never reflect the true position.
/// Terminal bounds therefore cannot distinguish a mangled report from a genuine event on a large terminal.
/// The pair decodes from contiguous bytes in a single terminal read, so the completion must arrive within [`MAX_COMPLETION_GAP`] of the candidate.
#[derive(Default)]
pub struct X10ReassemblyFilter {
    /// Candidate mangled report held awaiting its displaced row byte.
    held: Option<HeldReport>,
}

/// A magic-shape mouse event held as a mangled-report candidate; typed so reconstruction cannot see a non-mouse event.
struct HeldReport {
    mouse: MouseEvent,
    arrived_at: Instant,
}

/// Maximum arrival gap between the mangled mouse event and its displaced row byte.
/// The pair is parsed from adjacent bytes of one read, so real pairs arrive effectively simultaneously.
/// Human typing after a coincidentally magic-shaped genuine event is orders of magnitude slower.
const MAX_COMPLETION_GAP: Duration = Duration::from_millis(50);

impl X10ReassemblyFilter {
    pub fn new() -> Self {
        Self { held: None }
    }

    /// Process a batch of events, reassembling mangled X10 reports.
    /// A held candidate persists across calls so a pair the reader thread split across two batches is still reconstructed.
    pub fn filter(&mut self, events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
        let mut result = Vec::with_capacity(events.len());

        for ev in events {
            if let Some(held) = self.held.take() {
                let within_gap =
                    ev.arrived_at.saturating_duration_since(held.arrived_at) <= MAX_COMPLETION_GAP;
                match displaced_row_byte(&ev.event) {
                    Some(row_byte) if within_gap => {
                        result.push(reconstruct(&held, row_byte));
                        continue;
                    }
                    _ => result.push(TimedInputEvent {
                        event: Event::Mouse(held.mouse),
                        arrived_at: held.arrived_at,
                    }),
                }
            }

            match ev.event {
                Event::Mouse(m) if is_mangled_shape(&m) => {
                    self.held = Some(HeldReport {
                        mouse: m,
                        arrived_at: ev.arrived_at,
                    });
                }
                _ => result.push(ev),
            }
        }

        result
    }
}

/// Whether this mouse event matches the mis-parse shape.
/// The column is the UTF-8 lead byte (`0xC2`/`0xC3` minus 33) and the row is a continuation byte (`0x80..=0xBF` minus 33).
/// The shape is independent of the true position.
fn is_mangled_shape(m: &MouseEvent) -> bool {
    matches!(m.column, 161 | 162) && (95..=158).contains(&m.row)
}

/// The displaced row byte, if this event is one: a bare printable/Latin-1 `Char` press, or `Backspace` for row byte `0x7F`.
/// Crossterm reports uppercase coordinate bytes with SHIFT.
/// Coordinate bytes are always >= 33 (1-based coordinate + 32).
fn displaced_row_byte(event: &Event) -> Option<u16> {
    let Event::Key(ke) = event else {
        return None;
    };
    if ke.kind != KeyEventKind::Press
        || !(ke.modifiers == KeyModifiers::NONE || ke.modifiers == KeyModifiers::SHIFT)
    {
        return None;
    }
    match ke.code {
        KeyCode::Char(c) if (0x21..=0xFF).contains(&(c as u32)) => Some(c as u16),
        KeyCode::Backspace if ke.modifiers == KeyModifiers::NONE => Some(0x7F),
        _ => None,
    }
}

/// Invert the mis-parse: recombine the UTF-8 pair crossterm split across the held event's coordinates into the true column.
/// Decode the displaced byte as the true row (both 0-based, mirroring crossterm's `-33`).
fn reconstruct(held: &HeldReport, row_byte: u16) -> TimedInputEvent {
    let lead = held.mouse.column + 33;
    let continuation = held.mouse.row + 33;
    let column_byte = ((lead & 0x03) << 6) | (continuation & 0x3F);
    TimedInputEvent {
        event: Event::Mouse(MouseEvent {
            kind: held.mouse.kind,
            column: column_byte - 33,
            row: row_byte - 33,
            modifiers: held.mouse.modifiers,
        }),
        arrived_at: held.arrived_at,
    }
}
