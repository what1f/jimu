//! Re-exports [`ClipboardProvider`] and [`InternalClipboard`] from the textarea, and adds [`SystemClipboard`] backed by platform clipboard tools.
//!
//! A copy writes every active leg (native / tmux / OSC 52); [`ClipboardDelivery`] is the evidence the write reached its destination.

pub(crate) mod platform;
mod trust;

#[cfg(feature = "clipboard-image")]
pub mod resize;
#[cfg(feature = "clipboard-image")]
pub mod validate;

pub(crate) use platform::is_remote_session;
pub use platform::{ImageData, OSASCRIPT_WAIT, mime_from_bytes};

pub use crate::engine::textarea::{ClipboardProvider, InternalClipboard};
pub use trust::{
    ClipboardDelivery, ClipboardEnvironment, NativeClipboardPreflight, Osc52Capability,
    expected_delivery, native_clipboard_preflight,
};

use std::sync::OnceLock;

/// Why an attachment probe could not return a usable attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardProbeDropReason {
    PasteboardChangedBeforeRead,
    PasteboardChangedAfterRead,
    BracketedPayloadMismatch,
    BracketedOriginReadFailed,
    ReadFailed,
    Timeout,
    PersistFailed,
    Panicked,
}

use crate::engine::terminal::{MultiplexerKind, TerminalContext};

/// Env var overriding where the copy backup file is written (supports `~`).
pub const JIMU_TUI_COPY_FILE_ENV: &str = "JIMU_TUI_COPY_FILE";

/// Cached result of the remote-session check (env vars don't change at runtime).
fn is_remote() -> bool {
    static REMOTE: OnceLock<bool> = OnceLock::new();
    *REMOTE.get_or_init(platform::is_remote_session)
}

/// Cached result of the container-without-display check.
fn is_container_no_display() -> bool {
    static CONTAINER: OnceLock<bool> = OnceLock::new();
    *CONTAINER.get_or_init(platform::is_containerized_without_display)
}

/// A clipboard relay can advertise that it handles OSC 52 locally. Over SSH only `TERM` propagates, so brands look incapable.
/// `LC_JIMU_TUI_OSC52_SINK` survives default OpenSSH `SendEnv`/`AcceptEnv` of `LC_*`.
pub fn osc52_sink_active() -> bool {
    static SINK: OnceLock<bool> = OnceLock::new();
    *SINK.get_or_init(|| {
        std::env::var_os("JIMU_TUI_OSC52_SINK").is_some()
            || std::env::var_os("LC_JIMU_TUI_OSC52_SINK").is_some()
    })
}

/// `JIMU_TUI_CLIPBOARD_NO_OSC52` forces OSC 52 off everywhere, including Linux always-emit and the wrap sink. For hosts that paint OSC 52 as garbage.
pub fn osc52_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("JIMU_TUI_CLIPBOARD_NO_OSC52").is_some())
}

/// Cached clipboard route resolved at first use from the terminal context.
pub fn clipboard_route() -> &'static ClipboardRoute {
    static ROUTE: OnceLock<ClipboardRoute> = OnceLock::new();
    ROUTE.get_or_init(|| {
        let ctx = crate::engine::terminal::terminal_context();
        resolve_clipboard_route(ctx)
    })
}

/// Wayland data-control availability as a display/telemetry label: `"yes"`/`"no"` on Wayland sessions, `"n/a"` elsewhere.
pub fn wayland_data_control_label() -> &'static str {
    match crate::engine::host::DisplayServer::current() {
        crate::engine::host::DisplayServer::Wayland => {
            if platform::wayland_data_control_supported() {
                "yes"
            } else {
                "no"
            }
        }
        _ => "n/a",
    }
}

/// Describes the clipboard write strategy for the current environment.
///
/// `Display` formats as `+`-separated active legs (e.g. "native+osc52").
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardRoute {
    /// Always attempt a native clipboard write.
    pub native: bool,
    /// Mirror into the tmux paste buffer via `tmux load-buffer`.
    pub tmux_buffer: bool,
    /// Emit an OSC 52 escape sequence toward the outer terminal.
    pub osc52: bool,
    /// Wrap OSC 52 in the tmux DCS passthrough envelope.
    /// True only when tmux is the IMMEDIATE terminal (tmux-backed and not inside an editor `:terminal`).
    /// This flag is not a clipboard "leg" and is excluded from `Display`.
    pub osc52_tmux_passthrough: bool,
}

impl std::fmt::Display for ClipboardRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (flag, label) in [
            (self.native, "native"),
            (self.tmux_buffer, "tmux"),
            (self.osc52, "osc52"),
        ] {
            if flag {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(label)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// `osc52` reads cached ambient markers. Tmux is normally true regardless of SSH; `JIMU_TUI_CLIPBOARD_NO_OSC52` forces it off everywhere.
pub fn resolve_clipboard_route(ctx: &TerminalContext) -> ClipboardRoute {
    resolve_clipboard_route_with(
        ctx,
        ClipboardRouteOpts {
            no_osc52: osc52_disabled(),
            wrap_sink: osc52_sink_active(),
        },
    )
}

/// Test/production overrides for pure clipboard-route resolution.
#[derive(Clone, Copy)]
struct ClipboardRouteOpts {
    no_osc52: bool,
    wrap_sink: bool,
}

/// Pure clipboard-route resolution (kill-switch / wrap-sink injected for tests).
fn resolve_clipboard_route_with(ctx: &TerminalContext, opts: ClipboardRouteOpts) -> ClipboardRoute {
    let is_tmux = ctx.multiplexer == MultiplexerKind::Tmux;
    // Linux always emits OSC 52. macOS/Windows only in tmux/SSH/container or when a wrap sink captures it.
    // `JIMU_TUI_CLIPBOARD_NO_OSC52` wins over every automatic path.
    let osc52 = !opts.no_osc52
        && (cfg!(target_os = "linux")
            || is_tmux
            || is_remote()
            || is_container_no_display()
            || opts.wrap_sink);
    ClipboardRoute {
        native: true,
        tmux_buffer: is_tmux,
        osc52,
        // Editor :terminal's immediate emulator is libvterm, not tmux, so don't wrap there
        // No point in tmux passthrough when OSC 52 itself is disabled.
        osc52_tmux_passthrough: osc52 && is_tmux && ctx.embedded_editor.is_none(),
    }
}

/// Write text into the tmux paste buffer via `tmux load-buffer -`.
///
/// Returns `true` only when spawn and child exit succeed.
fn write_tmux_buffer(text: &str) -> bool {
    use std::process::{Command, Stdio};

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        // Spooled stdin, not a pipe: a payload past the pipe buffer would block the UI thread if a wedged tmux server stops draining stdin
        // The bounded wait below also needs stdin already closed
        let stdin = platform::spool_for_stdin(text.as_bytes())?;
        let mut cmd = Command::new("tmux");
        cmd.args(["load-buffer", "-"])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::engine::tty::detach_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // short-lived clipboard helper, waited on below
        let mut child = cmd.spawn()?;
        // Bounded wait: a wedged tmux server must not freeze the UI thread.
        let status = platform::wait_with_deadline(&mut child, std::time::Duration::from_secs(2))?;
        if !status.success() {
            return Err(format!("tmux load-buffer exited with {status}").into());
        }
        Ok(())
    })();
    result.is_ok()
}

/// Uses the host platform clipboard, the tmux buffer, and the terminal clipboard protocol.
///
/// In tmux-backed environments, clipboard writes follow the full three-leg contract: native clipboard, tmux buffer, and OSC 52.
#[derive(Debug)]
pub struct SystemClipboard;

impl SystemClipboard {
    /// Runs the full write route and classifies the result via the environment-based delivery policy.
    pub fn try_set(text: &str) -> ClipboardDelivery {
        let legs = clipboard_write_with_route(text, clipboard_route());
        decision_for_legs(&legs, text).delivery()
    }
}

impl ClipboardProvider for SystemClipboard {
    fn get(&mut self) -> Option<String> {
        platform::get_text().ok().flatten()
    }

    fn set(&mut self, text: &str) {
        let _ = clipboard_write_with_route(text, clipboard_route());
    }
}

/// Per-leg outcome of a routed clipboard write.
pub(crate) struct ClipboardWriteLegs {
    /// Whether the route enabled the native leg.
    pub(crate) route_native: bool,
    pub(crate) wl_copy_ok: bool,
    pub(crate) cli_ok: bool,
    pub(crate) arboard_ok: bool,
    /// Wayland data-control was available for the native leg (environment probe).
    /// The arboard write is focus-free and authoritative only when `arboard_ok` also holds (see `trust::trusted_native`).
    pub(crate) data_control: bool,
    pub(crate) tmux_ok: bool,
    pub(crate) osc52_ok: bool,
}

fn clipboard_write_with_route(text: &str, route: &ClipboardRoute) -> ClipboardWriteLegs {
    let mut legs = ClipboardWriteLegs {
        route_native: route.native,
        wl_copy_ok: false,
        cli_ok: false,
        arboard_ok: false,
        data_control: false,
        tmux_ok: false,
        osc52_ok: false,
    };

    if route.native {
        let outcome = platform::set_text_with_outcome(text);
        legs.cli_ok = outcome.cli_ok;
        legs.arboard_ok = outcome.arboard_ok;
        legs.data_control = outcome.data_control;
        legs.wl_copy_ok = outcome.cli_ok_tools.contains(&"wl-copy");
    }

    if route.tmux_buffer {
        legs.tmux_ok = write_tmux_buffer(text);
    }

    if route.osc52 {
        legs.osc52_ok = platform::set_text_osc52(text, route.osc52_tmux_passthrough).is_ok();
    }

    legs
}

/// Result of a clipboard write with toast info for the caller to display.
#[derive(Debug)]
pub struct CopyResult {
    /// Feedback identifier for the host to localize.
    pub feedback: ClipboardFeedback,
    /// Toast duration in ticks (30fps: 30 ticks is ~1s, 120 is ~4s).
    pub ticks: u8,
    /// Evidence that the write reached the destination named by the UI.
    pub delivery: ClipboardDelivery,
}

/// Kind of clipboard feedback (success route, unverified send, or failure).
///
/// The host maps the feedback to its localized labels.
#[derive(Debug, Clone, Copy, Eq, PartialEq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ClipboardFeedback {
    /// Plain successful copy (native clipboard).
    Copied,
    /// Successful copy mirrored into the tmux paste buffer.
    CopiedTmux,
    /// Successful copy via OSC 52 in a container without display.
    CopiedOscContainer,
    /// Successful copy via OSC 52 over SSH/remote.
    CopiedOscRemote,
    /// OSC 52 emitted over SSH, but the outer terminal's support is unknown.
    UnverifiedOscRemote,
    /// OSC 52 emitted from a displayless container with unknown outer support.
    UnverifiedOscContainer,
    /// VS Code over SSH/remote with non-ASCII text: OSC 52 may garble it.
    VsCodeSshNonAscii,
    /// No route reached the user's local clipboard from a remote/container topology.
    FailedRemote,
    /// All trusted clipboard backends failed in a local topology.
    Failed,
}

impl ClipboardFeedback {
    pub(crate) fn delivery(self) -> ClipboardDelivery {
        match self {
            Self::Copied
            | Self::CopiedTmux
            | Self::CopiedOscContainer
            | Self::CopiedOscRemote
            | Self::VsCodeSshNonAscii => ClipboardDelivery::Confirmed,
            Self::UnverifiedOscRemote | Self::UnverifiedOscContainer => {
                ClipboardDelivery::Unverified
            }
            Self::FailedRemote | Self::Failed => ClipboardDelivery::Failed,
        }
    }

    /// Toast duration in ticks (30fps: 30 ticks is ~1s, 120 is ~4s).
    fn ticks(self) -> u8 {
        match self {
            Self::Copied => 30,
            Self::CopiedTmux
            | Self::CopiedOscContainer
            | Self::CopiedOscRemote
            | Self::UnverifiedOscRemote
            | Self::UnverifiedOscContainer
            | Self::VsCodeSshNonAscii
            | Self::FailedRemote
            | Self::Failed => 120,
        }
    }

    fn to_result(self) -> CopyResult {
        CopyResult {
            feedback: self,
            ticks: self.ticks(),
            delivery: self.delivery(),
        }
    }
}

fn clipboard_environment(legs: &ClipboardWriteLegs) -> ClipboardEnvironment {
    ClipboardEnvironment {
        brand: crate::engine::terminal::terminal_context().brand,
        host_os: crate::engine::host::HostOs::current(),
        display_server: crate::engine::host::DisplayServer::current(),
        remote: is_remote(),
        container: is_container_no_display(),
        osc52_sink: osc52_sink_active(),
        wayland_data_control: legs.data_control,
        wl_copy_available: legs.wl_copy_ok,
    }
}

fn decision_for_legs(legs: &ClipboardWriteLegs, text: &str) -> ClipboardFeedback {
    trust::resolve_copy_decision(legs, text, clipboard_environment(legs))
}

/// Write text and return feedback for the host.
pub fn copy_text(text: &str) -> CopyResult {
    let route = clipboard_route();
    let legs = clipboard_write_with_route(text, route);
    decision_for_legs(&legs, text).to_result()
}

/// Write text through the native clipboard, tmux, and terminal routes as available.
pub fn copy_text_to_clipboard(text: &str) -> ClipboardDelivery {
    SystemClipboard::try_set(text)
}

/// Where a copy landed after [`copy_text_or_file`].
#[derive(Debug)]
pub enum CopyDelivery {
    /// Trusted clipboard backend accepted the write.
    /// `file` is the always-written backup copy (`None` only when the file write itself failed; that never fails the copy).
    Clipboard {
        result: CopyResult,
        file: Option<std::path::PathBuf>,
    },
    /// Clipboard failed; text was written to this path instead.
    File { path: std::path::PathBuf },
    /// Clipboard and file fallback both failed.
    Failed {
        clipboard: CopyResult,
        file_error: std::io::Error,
    },
}

impl CopyDelivery {
    /// `true` when the user can retrieve the text (clipboard or file).
    pub fn success(&self) -> bool {
        !matches!(self, Self::Failed { .. })
    }

    /// Ask the host to format the localized message for the selected delivery.
    pub fn toast_message(&self, mut localize: impl FnMut(CopyNotice<'_>) -> String) -> String {
        localize(match self {
            Self::Clipboard { result, file } => match (result.delivery, file) {
                (ClipboardDelivery::Unverified, Some(path)) => CopyNotice::UnverifiedWithBackup {
                    feedback: result.feedback,
                    path,
                },
                _ => CopyNotice::Clipboard(result.feedback),
            },
            Self::File { path } => CopyNotice::File(path),
            Self::Failed { clipboard, .. } => CopyNotice::Clipboard(clipboard.feedback),
        })
    }

    /// Toast duration in ticks for [`Self::toast_message`].
    pub fn toast_ticks(&self) -> u8 {
        match self {
            Self::Clipboard { result, .. } => result.ticks,
            Self::File { .. } => 120,
            Self::Failed { clipboard, .. } => clipboard.ticks,
        }
    }
}

/// Inputs for the host's localized clipboard notice.
#[derive(Clone, Copy, Debug)]
pub enum CopyNotice<'a> {
    Clipboard(ClipboardFeedback),
    UnverifiedWithBackup {
        feedback: ClipboardFeedback,
        path: &'a std::path::Path,
    },
    File(&'a std::path::Path),
}

/// The environment override takes priority over the host-provided configuration directory.
pub fn default_copy_fallback_path(
    config_home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Ok(raw) = std::env::var(JIMU_TUI_COPY_FILE_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(std::path::PathBuf::from(
                shellexpand::tilde(trimmed).as_ref(),
            ));
        }
    }
    config_home.map(|home| home.join("last-copy.txt"))
}

/// Abbreviate a backup path for display.
pub fn display_copy_path(path: &std::path::Path) -> String {
    crate::engine::util::abbreviate_path(&path.to_string_lossy()).into_owned()
}

/// Tilde-expand and create parents. Unix mode `0600`: copied text can be sensitive and the default path is predictable.
pub fn write_text_to_copy_file(
    text: &str,
    path: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let expanded = std::path::PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref());
    if let Some(parent) = expanded.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    write_owner_only(&expanded, text)?;
    Ok(expanded)
}

/// Unix `0600`, including tightening a pre-existing `0644` file (create-time mode does not). Non-unix is a plain write.
fn write_owner_only(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    std::fs::write(path, text)
}

/// `NotFound` when no path resolves, rather than a predictable temp location. Unix creates a missing parent `0700`.
pub fn write_copy_fallback(
    text: &str,
    config_home: Option<&std::path::Path>,
) -> std::io::Result<std::path::PathBuf> {
    let Some(path) = default_copy_fallback_path(config_home) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Clipboard backup location is unavailable",
        ));
    };
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    write_text_to_copy_file(text, &path)
}

/// Compose a [`CopyDelivery`] from the clipboard toast and the (always attempted) backup-file write.
/// Pure so the matrix is unit-testable without firing real clipboard legs.
fn resolve_delivery(
    clipboard: CopyResult,
    file: std::io::Result<std::path::PathBuf>,
) -> CopyDelivery {
    if clipboard.delivery.reported_success() {
        return CopyDelivery::Clipboard {
            result: clipboard,
            file: file.ok(),
        };
    }
    match file {
        Ok(path) => CopyDelivery::File { path },
        Err(file_error) => CopyDelivery::Failed {
            clipboard,
            file_error,
        },
    }
}

/// Always write the backup file too. It is the recovery path when SSH cannot reach the local clipboard; a failed file write does not fail a successful clipboard leg.
pub fn copy_text_or_file(text: &str, config_home: Option<&std::path::Path>) -> CopyDelivery {
    let clipboard = copy_text(text);
    let file = write_copy_fallback(text, config_home);
    resolve_delivery(clipboard, file)
}

/// Format the text count and line count with the host's localized template.
pub fn clipboard_stats_suffix(text: &str, localize: impl FnOnce(usize, usize) -> String) -> String {
    localize(text.len(), text.lines().count())
}

/// CLIPBOARD text read failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardTextReadError;

/// Read CLIPBOARD text while distinguishing emptiness from failure.
pub fn system_clipboard_read_text() -> Result<Option<String>, ClipboardTextReadError> {
    platform::get_text().map_err(|_| ClipboardTextReadError)
}

/// Read CLIPBOARD text for simple callers that treat failure as no text.
pub fn system_clipboard_get() -> Option<String> {
    system_clipboard_read_text().ok().flatten()
}

/// Read X11 PRIMARY text for an unmodified Linux middle-button press.
#[cfg(target_os = "linux")]
pub fn system_primary_selection_get() -> Option<String> {
    if !platform::x11_display_env_present() {
        return None;
    }
    platform::get_primary_text()
        .ok()
        .flatten()
        .filter(|text| !text.is_empty())
}

#[cfg(target_os = "linux")]
fn is_native_x11(display_server: crate::engine::host::DisplayServer) -> bool {
    display_server == crate::engine::host::DisplayServer::X11
}

/// Runtime gate for the empty Ctrl+V guidance: native X11 with a display present.
pub fn x11_primary_guidance_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        is_native_x11(crate::engine::host::DisplayServer::current())
            && platform::x11_display_env_present()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Non-empty after trim.
pub fn clipboard_text_is_pasteable(text: Option<&str>) -> bool {
    text.is_some_and(|t| !t.trim().is_empty())
}

fn lone_http_url_trimmed(t: &str) -> bool {
    !t.is_empty() && !t.contains('\n') && (t.starts_with("http://") || t.starts_with("https://"))
}

/// True when the text is a lone `http(s)://` URL with no newlines (normal link paste).
pub fn looks_like_lone_http_url(text: &str) -> bool {
    lone_http_url_trimmed(text.trim())
}

/// Non-empty plain text where the `«class furl»` probe is unnecessary (UTF-8 already carries paths or prose).
/// Does **not** suppress `get_image`; see [`clipboard_attachment_probe_needed`].
pub(crate) fn plain_text_skips_furl_probe(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    if lone_http_url_trimmed(trimmed) {
        return true;
    }
    if trimmed.contains("file://") {
        return true;
    }
    if trimmed.len() >= 4096 {
        return true;
    }
    if trimmed.lines().count() > 4 && !trimmed.contains("://") {
        return true;
    }
    !trimmed.contains("://")
}

/// False only for a lone `http(s)://` URL. Prose and `file://` still probe so a captioned "Copy Image" keeps working.
pub fn clipboard_attachment_probe_needed(clipboard_text: Option<&str>) -> bool {
    match clipboard_text {
        None => true,
        Some(text) => {
            let t = text.trim();
            t.is_empty() || !lone_http_url_trimmed(t)
        }
    }
}

/// Probe when empty, short, or containing `://`. A lone `https://` skips the macOS `osascript` cost.
pub fn paste_payload_needs_clipboard_attachment_probe(payload: &str) -> bool {
    if payload.is_empty() {
        return true;
    }
    let t = payload.trim();
    if lone_http_url_trimmed(t) {
        return false;
    }
    if t.len() >= 4096 {
        return false;
    }
    let line_count = t.lines().count().max(1);
    line_count <= 4 || t.contains("://")
}

/// Terminals rewrite `\n` to `\r` on paste. A mismatch is an IME commit or a diverged tmux buffer, not a clipboard paste.
/// Reads clipboard text; call only off the event loop.
pub fn bracketed_payload_came_from_clipboard(payload: &str) -> bool {
    bracketed_payload_came_from_clipboard_result(payload).unwrap_or(false)
}

/// Typed version of [`bracketed_payload_came_from_clipboard`].
pub fn bracketed_payload_came_from_clipboard_result(
    payload: &str,
) -> Result<bool, ClipboardTextReadError> {
    system_clipboard_read_text()
        .map(|text| bracketed_payload_matches_clipboard_text(payload, text.as_deref()))
}

/// Pure comparison behind [`bracketed_payload_came_from_clipboard`].
/// `None` means no UTF-8 string on the pasteboard (image-only pasteboards).
pub fn bracketed_payload_matches_clipboard_text(
    payload: &str,
    clipboard_text: Option<&str>,
) -> bool {
    // Terminals rewrite pasted newlines as `\r` (Windows clipboards carry `\r\n`); some copy sources append a trailing newline the paste omits
    fn normalized(s: &str) -> String {
        s.replace("\r\n", "\n")
            .replace('\r', "\n")
            .trim_end()
            .to_owned()
    }
    match clipboard_text {
        None => payload.trim().is_empty(),
        Some(text) => normalized(payload) == normalized(text),
    }
}

/// Routing plan for attachment pasteboard probes (testable without subprocesses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentProbeRoute {
    /// Lone `http(s)://` link paste: no `osascript`.
    Skip,
    /// Empty pasteboard UTF-8: `furl` first, then image if needed.
    FileUrlsThenImage,
    /// Non-empty UTF-8: image only (caption / Copy Image).
    ImageOnly,
}

/// Decide which pasteboard probes to run for a Cmd+V clipboard read.
pub fn attachment_probe_route(clipboard_text: Option<&str>) -> AttachmentProbeRoute {
    let trimmed = clipboard_text.map(str::trim).unwrap_or("");
    if !trimmed.is_empty() && lone_http_url_trimmed(trimmed) {
        return AttachmentProbeRoute::Skip;
    }
    if trimmed.is_empty() || !plain_text_skips_furl_probe(trimmed) {
        AttachmentProbeRoute::FileUrlsThenImage
    } else {
        AttachmentProbeRoute::ImageOnly
    }
}

/// `ImageOnly` may skip when the snapshot reports no raster. `FileUrlsThenImage` is never gated: the snapshot cannot vouch for `public.file-url`.
fn should_run_attachment_probe(
    route: AttachmentProbeRoute,
    snapshot_supported: bool,
    snapshot_available: bool,
    snapshot_has_image: bool,
) -> bool {
    match route {
        AttachmentProbeRoute::Skip => false,
        AttachmentProbeRoute::FileUrlsThenImage => true,
        AttachmentProbeRoute::ImageOnly => {
            // Skip only when the snapshot is available and shows no raster (unavailable can't rule one out).
            let snapshot_rules_out_raster =
                snapshot_supported && snapshot_available && !snapshot_has_image;
            !snapshot_rules_out_raster
        }
    }
}

/// `None` skips the probe. `Some(change_count)` is this gate's own snapshot baseline — a later read can land after a clipboard change.
/// Cheap enough for the event loop; the heavy probe is deferred.
pub fn attachment_probe_gate(clipboard_text: Option<&str>) -> Option<Option<u64>> {
    // One snapshot read; a None change_count means the snapshot was unavailable, so the gate must not rule out a raster
    let (snapshot_change_count, snapshot_has_image) = clipboard_image_snapshot();
    should_run_attachment_probe(
        attachment_probe_route(clipboard_text),
        clipboard_image_probe_supported(),
        snapshot_change_count.is_some(),
        snapshot_has_image,
    )
    .then_some(snapshot_change_count)
}

/// Whether [`system_clipboard_probe_attachments`] would run the osascript probe for `clipboard_text` (thin bool view of [`attachment_probe_gate`]).
pub fn attachment_probe_would_run(clipboard_text: Option<&str>) -> bool {
    attachment_probe_gate(clipboard_text).is_some()
}

/// `Timeout` when the read's helper process was killed at its deadline; every other read error is `ReadFailed`.
fn read_drop_reason(error: &std::io::Error) -> ClipboardProbeDropReason {
    if error.kind() == std::io::ErrorKind::TimedOut {
        ClipboardProbeDropReason::Timeout
    } else {
        ClipboardProbeDropReason::ReadFailed
    }
}

/// Probe image / file-url pasteboard types per [`attachment_probe_route`].
pub fn system_clipboard_probe_attachments(
    clipboard_text: Option<&str>,
) -> Result<(Option<ImageData>, Option<String>), ClipboardProbeDropReason> {
    // Native snapshot (sub-ms, no subprocess) skips the osascript image probe for a text paste when the pasteboard holds no raster
    // This is the single gate site
    if !attachment_probe_would_run(clipboard_text) {
        return Ok((None, None));
    }
    match attachment_probe_route(clipboard_text) {
        AttachmentProbeRoute::Skip => Ok((None, None)),
        AttachmentProbeRoute::FileUrlsThenImage => {
            let att = system_clipboard_get_attachments()?;
            if att.file_urls.is_some() {
                return Ok((None, att.file_urls));
            }
            Ok((att.image, None))
        }
        AttachmentProbeRoute::ImageOnly => {
            system_clipboard_get_image_result().map(|image| (image, None))
        }
    }
}

/// Why a probe attached nothing; `image` is a raster it read and then discarded.
#[derive(Debug)]
pub struct ProbeDrop {
    pub reason: ClipboardProbeDropReason,
    pub image: Option<ImageData>,
    /// Error text for the completion's toast; only a failed session persist has one.
    pub message: Option<String>,
}

/// Bracket one pasteboard read with the `changeCount` guard so a paste never attaches what was not on the board at `baseline`
/// (today [`attachment_probe_gate`]'s snapshot after the `pbpaste` text read, so a copy landing during that read is not caught).
/// A move before the read drops unread; a move during it drops what was read and outranks the read's own failure, except a
/// deadline kill, where the hang is the signal. A `None` baseline (no `changeCount` on this platform) leaves the read unguarded.
pub fn guarded_pasteboard_read(
    baseline: Option<u64>,
    mut change_count: impl FnMut() -> Option<u64>,
    read: impl FnOnce() -> Result<(Option<ImageData>, Option<String>), ClipboardProbeDropReason>,
) -> Result<(Option<ImageData>, Option<String>), ProbeDrop> {
    let dropped = |reason, image| ProbeDrop {
        reason,
        image,
        message: None,
    };
    let Some(baseline) = baseline else {
        return read().map_err(|reason| dropped(reason, None));
    };
    if change_count() != Some(baseline) {
        return Err(dropped(
            ClipboardProbeDropReason::PasteboardChangedBeforeRead,
            None,
        ));
    }
    let outcome = read();
    // A count that stops answering mid-paste cannot vouch for the board either, so it counts as a move
    if !matches!(outcome, Err(ClipboardProbeDropReason::Timeout))
        && change_count() != Some(baseline)
    {
        let image = outcome.ok().and_then(|(image, _)| image);
        return Err(dropped(
            ClipboardProbeDropReason::PasteboardChangedAfterRead,
            image,
        ));
    }
    outcome.map_err(|reason| dropped(reason, None))
}

/// Read file URLs and image from the system clipboard in one macOS `osascript`.
///
/// Other hosts use their platform clipboard tools.
fn system_clipboard_get_attachments() -> Result<AttachmentsProbeResult, ClipboardProbeDropReason> {
    let (image, file_urls) =
        platform::get_attachments().map_err(|error| read_drop_reason(&error))?;
    Ok(AttachmentsProbeResult { file_urls, image })
}

/// Result of [`system_clipboard_get_attachments`].
#[derive(Debug)]
struct AttachmentsProbeResult {
    file_urls: Option<String>,
    image: Option<ImageData>,
}

/// One pasteboard snapshot `(change_count, has_pasteable_image)` read in a single native pass (macOS native, sub-millisecond, no data read).
/// `(None, false)` off-macOS or when AppKit cannot be loaded.
pub fn clipboard_image_snapshot() -> (Option<u64>, bool) {
    platform::clipboard_image_snapshot()
}

/// Cheap pasteboard `changeCount` read (one native message, no type scan, no data read).
/// The focus-driven clipboard-image tip checks this first: a delta here is what gates the heavier [`clipboard_image_snapshot`] classification.
/// `None` off-macOS.
pub fn clipboard_change_count() -> Option<u64> {
    platform::clipboard_change_count()
}

/// Whether the fast image probe exists on this platform.
/// Gates the focus-driven clipboard-image tip so non-macOS never probes.
pub fn clipboard_image_probe_supported() -> bool {
    platform::clipboard_image_probe_supported()
}

/// One-time AppKit `dlopen` off the event loop so the first focus-gain probe does not stall a frame. Probe stays synchronous.
pub fn prewarm_image_probe() {
    use std::sync::Once;
    static WARMED: Once = Once::new();
    if !clipboard_image_probe_supported() {
        return;
    }
    WARMED.call_once(|| {
        std::thread::spawn(platform::clipboard_prewarm);
    });
}

/// Read an image while preserving an empty-versus-error distinction.
fn system_clipboard_get_image_result() -> Result<Option<ImageData>, ClipboardProbeDropReason> {
    platform::get_image().map_err(|error| read_drop_reason(&error))
}

/// Read an image from the system clipboard; direct callers treat errors as empty.
pub fn system_clipboard_get_image() -> Option<ImageData> {
    system_clipboard_get_image_result().unwrap_or(None)
}

/// Read encoded image bytes supplied by the host.
#[cfg(feature = "clipboard-image")]
pub trait ClipboardImageProvider {
    fn read_image(&self) -> std::io::Result<Option<Vec<u8>>>;
}

#[cfg(feature = "clipboard-image")]
#[derive(Debug, thiserror::Error)]
pub enum ClipboardImageError {
    #[error(transparent)]
    Read(#[from] std::io::Error),
    #[error(transparent)]
    Image(#[from] validate::ImageValidateError),
}

/// Read one clipboard image and validate its encoded contents before returning it.
#[cfg(feature = "clipboard-image")]
pub fn read_image(
    provider: &dyn ClipboardImageProvider,
) -> Result<Option<ImageData>, ClipboardImageError> {
    Ok(provider
        .read_image()?
        .map(ImageData::from_bytes)
        .transpose()?)
}

#[cfg(feature = "clipboard-image")]
impl ImageData {
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, validate::ImageValidateError> {
        let (_, _, mime) = validate::validate_image_bytes(&data)?;
        Ok(Self {
            data,
            mime_type: mime.to_owned(),
        })
    }
    pub fn base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&self.data)
    }
}
