//! OSC 7501 — Program Status Protocol (Mitchell Hashimoto, Oct 2026).
//!
//! Wire format:
//! `ESC ] 7501 ; key=value:key=value... ESC \`
//!
//! Supported keys:
//! - `state=` (required): `idle` | `working` | `blocked` | `done` | `error` | `clear`
//! - `kind=` (only valid with `state=blocked`): `permission` | `question` | `auth`
//! - `progress=` (0..=100, valid with `working` / `blocked`)
//! - `id=` (optional hierarchical path, up to 8 `/`-separated segments, <= 128 bytes)
//! - `app=` (optional ASCII app identifier, <= 32 bytes)
//! - `title=` (optional UTF-8 title, base64-encoded, <= 192 decoded bytes)
//! - `msg=` (optional UTF-8 message, base64-encoded, <= 2048 decoded bytes)
//!
//! Hard protocol limits:
//! - Total wire sequence length <= 4096 bytes (including OSC introducer & ST).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use std::io::{self, Write};

const MAX_SEQUENCE_BYTES: usize = 4096;
const MAX_MSG_DECODED_BYTES: usize = 2048;
const MAX_TITLE_DECODED_BYTES: usize = 192;
const MAX_APP_BYTES: usize = 32;
const MAX_ID_BYTES: usize = 128;
const MAX_ID_SEGMENTS: usize = 8;

/// Primary program lifecycle state defined by OSC 7501.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
    Clear,
}

impl ProgramState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Error => "error",
            Self::Clear => "clear",
        }
    }
}

/// Sub-categorization of `state=blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

impl BlockedKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// One OSC 7501 status report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramState,
    pub kind: Option<BlockedKind>,
    pub progress: Option<u8>,
    pub id: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

impl ProgramStatus {
    pub fn new(state: ProgramState) -> Self {
        Self {
            state,
            kind: None,
            progress: None,
            id: None,
            app: None,
            title: None,
            msg: None,
        }
    }

    pub fn idle() -> Self {
        Self::new(ProgramState::Idle)
    }

    pub fn working() -> Self {
        Self::new(ProgramState::Working)
    }

    pub fn blocked(kind: BlockedKind) -> Self {
        let mut s = Self::new(ProgramState::Blocked);
        s.kind = Some(kind);
        s
    }

    pub fn done() -> Self {
        Self::new(ProgramState::Done)
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self::new(ProgramState::Error).with_msg(msg)
    }

    pub fn clear() -> Self {
        Self::new(ProgramState::Clear)
    }

    pub fn with_progress(mut self, progress: u8) -> Self {
        self.progress = Some(progress.min(100));
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn with_app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_msg(mut self, msg: impl Into<String>) -> Self {
        self.msg = Some(msg.into());
        self
    }

    /// Encode this report into a compliant OSC 7501 escape sequence (`\x1b]7501;...\x1b\\`).
    ///
    /// Enforces all protocol byte limits so terminals never drop the report.
    pub fn encode(&self) -> String {
        let mut prefix_parts = Vec::with_capacity(6);
        prefix_parts.push(format!("state={}", self.state.as_str()));

        if self.state == ProgramState::Blocked
            && let Some(kind) = self.kind
        {
            prefix_parts.push(format!("kind={}", kind.as_str()));
        }

        if matches!(self.state, ProgramState::Working | ProgramState::Blocked)
            && let Some(progress) = self.progress
        {
            prefix_parts.push(format!("progress={}", progress.min(100)));
        }

        if let Some(id) = self.id.as_deref().and_then(sanitize_id) {
            prefix_parts.push(format!("id={id}"));
        }

        if let Some(app) = self.app.as_deref().and_then(sanitize_app) {
            prefix_parts.push(format!("app={app}"));
        }

        if let Some(title) = self.title.as_deref().filter(|s| !s.is_empty()) {
            let truncated = truncate_utf8_bytes(title, MAX_TITLE_DECODED_BYTES);
            let encoded = STANDARD.encode(truncated.as_bytes());
            prefix_parts.push(format!("title={encoded}"));
        }

        let header = "\x1b]7501;";
        let footer = "\x1b\\";
        let base_body = prefix_parts.join(":");

        if let Some(msg) = self.msg.as_deref().filter(|s| !s.is_empty()) {
            let fixed_len = header.len() + base_body.len() + ":msg=".len() + footer.len();
            if fixed_len < MAX_SEQUENCE_BYTES {
                let max_b64 = MAX_SEQUENCE_BYTES - fixed_len;
                let max_raw = ((max_b64 / 4) * 3).min(MAX_MSG_DECODED_BYTES);
                if max_raw > 0 {
                    let truncated = truncate_utf8_bytes(msg, max_raw);
                    let encoded = STANDARD.encode(truncated.as_bytes());
                    return format!("{header}{base_body}:msg={encoded}{footer}");
                }
            }
        }

        let mut out = format!("{header}{base_body}{footer}");
        if out.len() > MAX_SEQUENCE_BYTES {
            out = format!("{header}state={}{footer}", self.state.as_str());
        }
        out
    }

    /// Encode this report, wrapping in tmux DCS passthrough if running inside tmux.
    pub fn encode_for_terminal(&self, in_tmux: bool) -> String {
        let raw = self.encode();
        if in_tmux {
            wrap_tmux_passthrough(&raw)
        } else {
            raw
        }
    }
}

/// Stateful, deduplicating OSC 7501 reporter owned by the TUI host.
#[derive(Debug, Clone)]
pub struct ProgramStatusReporter {
    enabled: bool,
    app: Option<String>,
    id: Option<String>,
    in_tmux: bool,
    last_wire: Option<String>,
}

impl Default for ProgramStatusReporter {
    fn default() -> Self {
        Self::new(true, None)
    }
}

impl ProgramStatusReporter {
    pub fn new(enabled: bool, app: Option<String>) -> Self {
        let in_tmux = std::env::var_os("TMUX").is_some();
        Self {
            enabled,
            app: app.and_then(|a| sanitize_app(&a)),
            id: None,
            in_tmux,
            last_wire: None,
        }
    }

    pub fn with_id(mut self, id: Option<String>) -> Self {
        self.id = id.and_then(|v| sanitize_id(&v));
        self
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Build the next wire sequence if it differs from the last emitted report.
    pub fn next_sequence(&mut self, mut status: ProgramStatus) -> Option<String> {
        if !self.enabled {
            return None;
        }
        if status.app.is_none() {
            status.app = self.app.clone();
        }
        if status.id.is_none() {
            status.id = self.id.clone();
        }
        let wire = status.encode_for_terminal(self.in_tmux);
        if self.last_wire.as_deref() == Some(wire.as_str()) {
            return None;
        }
        self.last_wire = Some(wire.clone());
        Some(wire)
    }

    /// Emit a status update directly to `writer` if changed.
    pub fn emit<W: Write>(&mut self, writer: &mut W, status: ProgramStatus) -> io::Result<()> {
        if let Some(seq) = self.next_sequence(status) {
            writer.write_all(seq.as_bytes())?;
            writer.flush()?;
        }
        Ok(())
    }

    /// Emit `state=clear` if any status was previously reported.
    pub fn clear<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if !self.enabled || self.last_wire.is_none() {
            return Ok(());
        }
        let mut status = ProgramStatus::clear();
        status.id = self.id.clone();
        let wire = status.encode_for_terminal(self.in_tmux);
        self.last_wire = None;
        writer.write_all(wire.as_bytes())?;
        writer.flush()
    }
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn sanitize_app(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic() && !matches!(c, ':' | '=' | ';'))
        .take(MAX_APP_BYTES)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn sanitize_id(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut segments = 0usize;
    for seg in raw.split('/').filter(|s| !s.is_empty()) {
        if segments >= MAX_ID_SEGMENTS {
            break;
        }
        let clean: String = seg
            .chars()
            .filter(|c| c.is_ascii_graphic() && !matches!(c, ':' | '=' | ';' | '/'))
            .collect();
        if clean.is_empty() {
            continue;
        }
        let extra = if out.is_empty() {
            clean.len()
        } else {
            1 + clean.len()
        };
        if out.len() + extra > MAX_ID_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&clean);
        segments += 1;
    }
    (!out.is_empty()).then_some(out)
}

fn wrap_tmux_passthrough(seq: &str) -> String {
    let escaped = seq.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}
