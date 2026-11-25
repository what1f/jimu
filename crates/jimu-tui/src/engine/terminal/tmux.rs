//! Callers must pass an already-sanitized payload.
//! These helpers only double ESC (`0x1b`).
//! They do not strip CAN (`0x18`), SUB (`0x1a`), C1 ST (`0x9c`), or BEL (`0x07`), any of which abort or terminate DCS/OSC in tmux/xterm.

use super::TerminalContext;

/// Wrap `sequence` in `\x1bPtmux;…\x1b\\`, doubling embedded ESC bytes.
#[must_use]
pub fn tmux_passthrough(sequence: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sequence.len() + 16);
    out.extend_from_slice(b"\x1bPtmux;");
    for &byte in sequence {
        if byte == 0x1b {
            out.push(0x1b);
        }
        out.push(byte);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// String form of [`tmux_passthrough`].
/// Builds the envelope in UTF-8; only ASCII is inserted and ESC doubling cannot split a multibyte sequence.
#[must_use]
pub fn tmux_passthrough_str(sequence: &str) -> String {
    let mut out = String::with_capacity(sequence.len() + 16);
    out.push_str("\x1bPtmux;");
    for ch in sequence.chars() {
        if ch == '\x1b' {
            out.push('\x1b');
        }
        out.push(ch);
    }
    out.push_str("\x1b\\");
    out
}

/// True when the immediate emulator is tmux 3.3 or later, the minimum for reliable DCS passthrough.
#[must_use]
pub fn passthrough_available(ctx: &TerminalContext) -> bool {
    ctx.is_tmux_backed() && ctx.is_tmux_version_or_later(3, 3)
}

/// OSC 11 is wrapped only in passthrough-capable tmux that is not an editor `:terminal`.
#[must_use]
pub fn should_wrap_osc11(ctx: &TerminalContext) -> bool {
    passthrough_available(ctx) && ctx.embedded_editor.is_none()
}

/// DEC 2026 synchronized output is skipped only when tmux is the immediate terminal, in any tmux version: tmux repaints
/// the whole pane when a block closes and already synchronizes its own output toward the outer terminal, so the
/// pane-level wrapper only multiplies traffic. Inside an editor `:terminal` the editor's emulator is the immediate
/// terminal even though `TMUX` is inherited, so the wrapper stays.
#[must_use]
pub fn should_emit_synchronized_output(ctx: &TerminalContext) -> bool {
    !(ctx.is_tmux_backed() && ctx.embedded_editor.is_none())
}
