//! Platform clipboard backends used by the pager clipboard routing code.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::engine::{terminal::stderr, tty};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub(super) use linux::{get_image, get_text, set_text_with_outcome};
#[cfg(target_os = "linux")]
pub(super) use linux::{get_primary_text, wayland_data_control_supported, x11_display_env_present};
#[cfg(target_os = "macos")]
pub(super) use macos::{
    clipboard_change_count, clipboard_image_probe_supported, clipboard_image_snapshot,
    clipboard_prewarm, get_attachments,
};
#[cfg(target_os = "macos")]
pub(super) use macos::{get_image, get_text, set_text_with_outcome};

/// Encoded image data read from the system clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// Detect an encoded image type from its header.
pub fn mime_from_bytes(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.starts_with(b"II\x2a\x00") || data.starts_with(b"MM\x00\x2a") {
        "image/tiff"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP".as_slice()) {
        "image/webp"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
}

#[derive(Default)]
pub(super) struct NativeWriteOutcome {
    pub(super) cli_ok_tools: Vec<&'static str>,
    pub(super) cli_ok: bool,
    pub(super) arboard_ok: bool,
    pub(super) data_control: bool,
}

pub const OSASCRIPT_WAIT: Duration = Duration::from_secs(8);

pub(super) fn wait_with_deadline(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    match tty::wait_child_bounded(child, timeout) {
        Ok(Some(status)) => return Ok(status),
        Err(error) if tty::is_child_wait_identity_uncertain(&error) => return Err(error),
        result => {
            // SAFETY: detached Unix commands start a new group led by the child, still owned here.
            #[cfg(unix)]
            if let Ok(pid) = libc::pid_t::try_from(child.id())
                && pid > 1
            {
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
            result?;
        }
    }
    Err(io::ErrorKind::TimedOut.into())
}

/// Private scratch directory for transferring clipboard bytes without blocking on a pipe write.
pub(crate) struct ScratchDir(PathBuf);

impl ScratchDir {
    pub(crate) fn new() -> io::Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        loop {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "pikra-tui-{}-{}-{sequence}",
                std::process::id(),
                timestamp.as_nanos(),
            ));
            match builder.create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn spool_for_stdin(data: &[u8]) -> io::Result<File> {
    use std::io::{Seek, SeekFrom};

    let dir = ScratchDir::new()?;
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(dir.path().join("stdin"))?;
    file.write_all(data)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn run_pipe_in(argv: &[&str], data: &[u8], timeout: Duration) -> io::Result<()> {
    let (program, args) = argv.split_first().ok_or(io::ErrorKind::InvalidInput)?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(spool_for_stdin(data)?)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    tty::detach_command(&mut cmd);
    let status = wait_with_deadline(&mut cmd.spawn()?, timeout)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} exited with {status}")))
    }
}

fn run_capture(argv: &[&str], timeout: Duration) -> io::Result<(ExitStatus, Vec<u8>)> {
    let (program, args) = argv.split_first().ok_or(io::ErrorKind::InvalidInput)?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    tty::detach_command(&mut cmd);

    let deadline = Instant::now() + timeout;
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().expect("stdout was configured as piped");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = std::thread::Builder::new().spawn(move || {
        let mut output = Vec::new();
        let result = stdout.read_to_end(&mut output).map(|_| output);
        let _ = tx.send(result);
    }) {
        let _ = wait_with_deadline(&mut child, Duration::ZERO);
        return Err(error);
    }
    let status = wait_with_deadline(&mut child, timeout)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = rx.recv_timeout(remaining).map_err(|error| match error {
        std::sync::mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
        std::sync::mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
    })??;
    Ok((status, output))
}

fn get_text_output(bytes: Vec<u8>) -> Option<String> {
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

fn osc52_sequence(text: &str, tmux_passthrough: bool) -> Vec<u8> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if tmux_passthrough {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\").into_bytes()
    } else {
        format!("\x1b]52;c;{encoded}\x07").into_bytes()
    }
}

pub(super) fn set_text_osc52(text: &str, tmux_passthrough: bool) -> io::Result<()> {
    let seq = osc52_sequence(text, tmux_passthrough);
    stderr::with_locked_stderr(|stderr| {
        stderr.write_all(&seq)?;
        stderr.flush()
    })
}

pub(crate) fn is_remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
}

pub(super) fn is_containerized_without_display() -> bool {
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return false;
    }
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::env::var_os("container").is_some()
}

#[cfg(not(target_os = "linux"))]
pub(super) fn wayland_data_control_supported() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub(super) fn get_attachments() -> io::Result<(Option<ImageData>, Option<String>)> {
    get_image().map(|image| (image, None))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn clipboard_image_snapshot() -> (Option<u64>, bool) {
    (None, false)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn clipboard_change_count() -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub(super) fn clipboard_image_probe_supported() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub(super) fn clipboard_prewarm() {}

#[cfg(windows)]
pub(super) fn get_text() -> io::Result<Option<String>> {
    let (status, data) = run_capture(
        &[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::Write([string](Get-Clipboard -Raw))",
        ],
        Duration::from_secs(2),
    )?;
    if !status.success() {
        return Err(io::Error::other("Clipboard read failed"));
    }
    Ok(get_text_output(data))
}

#[cfg(windows)]
pub(super) fn set_text_with_outcome(text: &str) -> NativeWriteOutcome {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let cli_ok = run_pipe_in(
        &[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference = 'Stop'; Set-Clipboard -Value ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::In.ReadToEnd())))",
        ],
        encoded.as_bytes(),
        Duration::from_secs(2),
    )
    .is_ok();
    NativeWriteOutcome {
        cli_ok,
        ..Default::default()
    }
}

#[cfg(windows)]
pub(super) fn get_image() -> io::Result<Option<ImageData>> {
    use base64::Engine as _;

    let (status, bytes) = run_capture(
        &[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms; if ($img = [Windows.Forms.Clipboard]::GetImage()) { $buf = New-Object IO.MemoryStream; try { $img.Save($buf, [Drawing.Imaging.ImageFormat]::Png); [Console]::Write([Convert]::ToBase64String($buf.ToArray())) } finally { $buf.Dispose(); $img.Dispose() } }",
        ],
        OSASCRIPT_WAIT,
    )?;
    if !status.success() {
        return Err(io::Error::other("Clipboard image read failed"));
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(bytes)
        .map_err(io::Error::other)?;
    Ok(Some(ImageData {
        data,
        mime_type: "image/png".to_owned(),
    }))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) fn get_text() -> io::Result<Option<String>> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) fn set_text_with_outcome(_: &str) -> NativeWriteOutcome {
    NativeWriteOutcome::default()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) fn get_image() -> io::Result<Option<ImageData>> {
    Ok(None)
}
