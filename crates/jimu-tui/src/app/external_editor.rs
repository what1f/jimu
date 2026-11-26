//! External `$VISUAL` / `$EDITOR` prompt editing (`Ctrl+G` / `/edit`).

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Resolve the editor command + initial arguments from `$VISUAL`, `$EDITOR`, or platform fallback.
pub(super) fn resolve_editor_command() -> Result<(String, Vec<String>), String> {
    let raw = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_owned()
            } else {
                "vi".to_owned()
            }
        });
    let parts = split_shell_words(&raw)?;
    let Some((program, args)) = parts.split_first() else {
        return Err("No external editor configured".to_owned());
    };
    Ok((program.clone(), args.to_vec()))
}

/// Launch the resolved external editor on a temporary Markdown draft file and return the edited text.
///
/// The caller must suspend raw mode / alternate screen and drop any stdin event reader before
/// invoking this function, then resume the terminal afterward.
pub(super) fn run_external_editor(initial: &str, cwd: Option<&Path>) -> Result<String, String> {
    let (program, args) = resolve_editor_command()?;
    let temp = TempDraftFile::create(initial)?;

    let mut cmd = Command::new(&program);
    cmd.args(&args).arg(&temp.path);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let status = cmd
        .status()
        .map_err(|err| format!("Failed to launch editor `{program}`: {err}"))?;
    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("Editor `{program}` exited with status {code}"),
            None => format!("Editor `{program}` terminated by signal"),
        });
    }

    let edited = fs::read_to_string(&temp.path)
        .map_err(|err| format!("Failed to read edited prompt: {err}"))?;
    Ok(normalize_edited_text(initial, edited))
}

fn normalize_edited_text(initial: &str, edited: String) -> String {
    let normalized = edited.replace("\r\n", "\n");
    // Editors like vim/nano routinely append a single trailing newline on save. Strip it unless
    // the original draft already ended with a newline or the user intentionally added extra lines.
    if !initial.ends_with('\n') && normalized.ends_with('\n') && !normalized.ends_with("\n\n") {
        normalized[..normalized.len() - 1].to_owned()
    } else {
        normalized
    }
}

struct TempDraftFile {
    path: PathBuf,
}

impl TempDraftFile {
    fn create(initial: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        for _ in 0..32 {
            let seq = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("prompt-{pid}-{seq}.md"));
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&path) {
                Ok(mut file) => {
                    file.write_all(initial.as_bytes())
                        .map_err(|err| format!("Failed to write temp prompt file: {err}"))?;
                    file.flush()
                        .map_err(|err| format!("Failed to flush temp prompt file: {err}"))?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("Failed to create temp prompt file: {err}")),
            }
        }
        Err("Failed to allocate unique temp prompt file".to_owned())
    }
}

impl Drop for TempDraftFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn split_shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut has_token = false;

    #[cfg_attr(not(windows), allow(clippy::while_let_on_iterator))]
    let mut chars = input.chars().peekable();
    #[cfg_attr(not(windows), allow(clippy::while_let_on_iterator))]
    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            has_token = true;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            has_token = true;
            continue;
        }
        if in_double {
            match ch {
                '"' => in_double = false,
                #[cfg(not(windows))]
                '\\' => escaped = true,
                #[cfg(windows)]
                '\\' if chars.peek() == Some(&'"') => {
                    if let Some(q) = chars.next() {
                        current.push(q);
                    }
                }
                _ => current.push(ch),
            }
            has_token = true;
            continue;
        }
        match ch {
            #[cfg(not(windows))]
            '\\' => {
                escaped = true;
                has_token = true;
            }
            #[cfg(not(windows))]
            '\'' => {
                in_single = true;
                has_token = true;
            }
            '"' => {
                in_double = true;
                has_token = true;
            }
            c if c.is_whitespace() => {
                if has_token {
                    words.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            _ => {
                current.push(ch);
                has_token = true;
            }
        }
    }
    if in_single || in_double || escaped {
        return Err(format!("Invalid editor command syntax: `{input}`"));
    }
    if has_token {
        words.push(current);
    }
    Ok(words)
}
