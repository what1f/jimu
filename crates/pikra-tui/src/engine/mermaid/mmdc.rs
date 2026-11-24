//! Optional mermaid-cli PNG renderer with a per-invocation deadline.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::subprocess::{SubprocessError, run_with_timeout};
use super::{MermaidEngine, MermaidError, MermaidTheme, RenderParams, RenderedDiagram};
use crate::engine::render::media_directory::RenderDirectory;

/// Default wall-clock budget for an `mmdc` invocation.
pub const DEFAULT_MMDC_TIMEOUT: Duration = Duration::from_millis(1500);

/// Locate the `mmdc` binary on `PATH`, if installed.
pub fn detect_mmdc() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(if cfg!(windows) { "mmdc.exe" } else { "mmdc" }))
        .find(|path| {
            let Ok(meta) = path.metadata() else {
                return false;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                meta.is_file()
            }
        })
}

/// An engine that shells out to `mmdc` (mermaid-cli).
/// Off by default: construct it explicitly (it requires Node and headless Chromium).
/// Use [`MmdcEngine::detect`] to build one only if `mmdc` is present.
pub struct MmdcEngine {
    bin: PathBuf,
    timeout: Duration,
}

impl MmdcEngine {
    /// Build an engine that runs the `mmdc` binary at `bin`.
    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            timeout: DEFAULT_MMDC_TIMEOUT,
        }
    }

    /// Build an engine if (and only if) `mmdc` is found on `PATH`.
    pub fn detect() -> Option<Self> {
        detect_mmdc().map(Self::new)
    }

    /// Override the wall-clock timeout (default [`DEFAULT_MMDC_TIMEOUT`]).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The resolved `mmdc` binary path.
    pub fn binary(&self) -> &Path {
        &self.bin
    }
}

impl MermaidEngine for MmdcEngine {
    fn render(&self, source: &str, params: &RenderParams) -> Result<RenderedDiagram, MermaidError> {
        // IO failures below map to `Rasterize` (a render-pipeline failure), not `Unsupported`
        // A spawn failure stays `Unsupported`: the engine itself is unavailable
        let dir = RenderDirectory::new()
            .map_err(|e| MermaidError::Rasterize(format!("could not create temp dir: {e}")))?;
        let input = dir.0.join("diagram.mmd");
        let output = dir.0.join("diagram.png");

        // Create atomically with 0600 (no umask/chmod TOCTOU window)
        // The parent tempdir is already 0700
        write_private(&input, source)
            .map_err(|e| MermaidError::Rasterize(format!("could not write source: {e}")))?;

        let mut cmd = Command::new(&self.bin);
        cmd.arg("--input")
            .arg(&input)
            .arg("--output")
            .arg(&output)
            .arg("--outputFormat")
            .arg("png")
            .arg("--backgroundColor")
            .arg(
                params
                    .background
                    .map(|color| color.to_hex())
                    .unwrap_or_else(|| "transparent".to_owned()),
            )
            .arg("--scale")
            .arg(
                if params.target_width_px == 0 {
                    params.scale
                } else {
                    1.0
                }
                .to_string(),
            )
            .arg("--theme")
            .arg(theme_arg(params.theme))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .envs(crate::engine::tty::pager_env());

        let size = params.target_width_px.max(params.min_width_px);
        if size > 0 {
            cmd.arg("--size").arg(size.to_string());
        }

        // Source goes via the temp file, so no stdin payload.
        run_with_timeout(cmd, None, self.timeout).map_err(map_subprocess_error)?;

        use std::io::Read;
        // PNG scanlines can contain four 16-bit channels per pixel.
        const MAX_PNG_BYTES: u64 = super::MAX_OUTPUT_MEGAPIXELS as u64 * 8_000_000 + 1_048_576;
        let mut png = Vec::new();
        std::fs::File::open(&output)
            .and_then(|file| file.take(MAX_PNG_BYTES + 1).read_to_end(&mut png))
            .map_err(|e| MermaidError::Layout(format!("could not read diagram: {e}")))?;
        if png.len() as u64 > MAX_PNG_BYTES {
            return Err(MermaidError::Rasterize(
                "diagram exceeds the size limit".into(),
            ));
        }
        let Some(header) = png.get(..33) else {
            return Err(MermaidError::Rasterize("incomplete PNG".into()));
        };
        if !header.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR") {
            return Err(MermaidError::Rasterize("invalid PNG".into()));
        }
        let width_px = u32::from_be_bytes(header[16..20].try_into().unwrap());
        let height_px = u32::from_be_bytes(header[20..24].try_into().unwrap());
        if width_px == 0
            || height_px == 0
            || u64::from(width_px) * u64::from(height_px)
                > u64::from(super::MAX_OUTPUT_MEGAPIXELS) * 1_000_000
            || (params.max_height_px > 0 && height_px > params.max_height_px)
        {
            return Err(MermaidError::Rasterize(
                "diagram dimensions exceed the limit".into(),
            ));
        }
        Ok(RenderedDiagram {
            png,
            width_px,
            height_px,
        })
    }
}

fn theme_arg(theme: MermaidTheme) -> &'static str {
    match theme {
        MermaidTheme::Light => "default",
        MermaidTheme::Dark => "dark",
    }
}

/// Map a subprocess failure onto the engine error taxonomy.
/// A spawn failure means `mmdc` is unavailable ([`MermaidError::Unsupported`]).
/// A non-zero exit is a render/layout failure; a wait failure is a pipeline error ([`MermaidError::Rasterize`]).
fn map_subprocess_error(e: SubprocessError) -> MermaidError {
    match e {
        SubprocessError::Spawn(e) => {
            MermaidError::Unsupported(format!("could not spawn mmdc: {e}"))
        }
        SubprocessError::Timeout => MermaidError::Timeout,
        SubprocessError::NonZeroExit(status) => {
            MermaidError::Layout(format!("mmdc exited with {status}"))
        }
        SubprocessError::Wait(e) => MermaidError::Rasterize(format!("mmdc wait failed: {e}")),
    }
}

/// Write `contents` to `path`, creating it atomically with owner-only (0600) permissions on unix so there is no umask/chmod TOCTOU window.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}
