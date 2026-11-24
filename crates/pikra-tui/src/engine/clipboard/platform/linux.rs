use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use super::{ImageData, NativeWriteOutcome, get_text_output, run_capture, run_pipe_in};

const CLI_WRITE_WAIT: Duration = Duration::from_secs(2);
const CLI_PROBE_WAIT: Duration = Duration::from_secs(1);
const CLI_READ_WAIT: Duration = Duration::from_secs(2);
const READBACK_ATTEMPTS: usize = 3;
const READBACK_PAUSE: Duration = Duration::from_millis(100);

struct ToolSpec {
    name: &'static str,
    write_text: &'static [&'static str],
    read_text: &'static [&'static str],
    read_primary: Option<&'static [&'static str]>,
    read_png: Option<&'static [&'static str]>,
}

const WL_SPEC: ToolSpec = ToolSpec {
    name: "wl-copy",
    read_text: &["wl-paste", "--no-newline", "-t", "text"],
    write_text: &["wl-copy"],
    read_primary: None,
    read_png: Some(&["wl-paste", "--no-newline", "-t", "image/png"]),
};

const XCLIP_SPEC: ToolSpec = ToolSpec {
    name: "xclip",
    write_text: &["xclip", "-selection", "clipboard"],
    read_text: &["xclip", "-o", "-selection", "clipboard"],
    read_primary: Some(&["xclip", "-o", "-selection", "primary"]),
    read_png: Some(&["xclip", "-selection", "clipboard", "-t", "image/png", "-o"]),
};

const XSEL_SPEC: ToolSpec = ToolSpec {
    name: "xsel",
    write_text: &["xsel", "--clipboard", "--input"],
    read_text: &["xsel", "--clipboard", "--output"],
    read_primary: Some(&["xsel", "--primary", "--output"]),
    read_png: None,
};

fn env_present(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|value| !value.is_empty())
}

pub(in crate::engine::clipboard) fn x11_display_env_present() -> bool {
    env_present("DISPLAY")
}

fn clipboard_tools() -> impl Iterator<Item = &'static ToolSpec> {
    let wayland = env_present("WAYLAND_DISPLAY") || env_present("WAYLAND_SOCKET");
    let x11 = x11_display_env_present();
    [
        wayland.then_some(&WL_SPEC),
        x11.then_some(&XCLIP_SPEC),
        x11.then_some(&XSEL_SPEC),
    ]
    .into_iter()
    .flatten()
}

pub(in crate::engine::clipboard) fn wayland_data_control_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if std::env::var_os("PIKRA_TUI_CLIPBOARD_NO_DATA_CONTROL").is_some()
            || !(env_present("WAYLAND_DISPLAY") || env_present("WAYLAND_SOCKET"))
        {
            return false;
        }
        run_capture(&["wayland-info"], CLI_PROBE_WAIT).is_ok_and(|(status, output)| {
            status.success()
                && (output
                    .windows(b"zwlr_data_control_manager_v1".len())
                    .any(|part| part == b"zwlr_data_control_manager_v1")
                    || output
                        .windows(b"ext_data_control_manager_v1".len())
                        .any(|part| part == b"ext_data_control_manager_v1"))
        })
    })
}

fn wayland_write_verified(spec: &ToolSpec, text: &str) -> bool {
    for attempt in 0..READBACK_ATTEMPTS {
        if run_capture(spec.read_text, CLI_PROBE_WAIT)
            .is_ok_and(|(status, bytes)| status.success() && bytes == text.as_bytes())
        {
            return true;
        }
        if attempt + 1 < READBACK_ATTEMPTS {
            std::thread::sleep(READBACK_PAUSE);
        }
    }
    false
}

pub(in crate::engine::clipboard) fn set_text_with_outcome(text: &str) -> NativeWriteOutcome {
    let mut outcome = NativeWriteOutcome {
        data_control: wayland_data_control_supported(),
        ..Default::default()
    };
    for spec in clipboard_tools() {
        let wrote = run_pipe_in(spec.write_text, text.as_bytes(), CLI_WRITE_WAIT).is_ok();
        if wrote && (spec.name != "wl-copy" || wayland_write_verified(spec, text)) {
            outcome.cli_ok = true;
            outcome.cli_ok_tools.push(spec.name);
            // Both X11 programs own the same selection.
            if spec.name != "wl-copy" {
                break;
            }
        }
    }
    outcome
}

pub(in crate::engine::clipboard) fn get_text() -> io::Result<Option<String>> {
    read_with_tools(clipboard_tools().map(|spec| spec.read_text)).map(get_text_output)
}

pub(in crate::engine::clipboard) fn get_primary_text() -> io::Result<Option<String>> {
    read_with_tools(clipboard_tools().filter_map(|spec| spec.read_primary)).map(get_text_output)
}

pub(in crate::engine::clipboard) fn get_image() -> io::Result<Option<ImageData>> {
    let data = read_with_tools(clipboard_tools().filter_map(|spec| spec.read_png))?;
    Ok((!data.is_empty()).then(|| ImageData {
        data,
        mime_type: "image/png".to_owned(),
    }))
}

fn read_with_tools(tools: impl Iterator<Item = &'static [&'static str]>) -> io::Result<Vec<u8>> {
    let mut error = None;
    let mut reached_clipboard = false;
    for argv in tools {
        match run_capture(argv, CLI_READ_WAIT) {
            Ok((status, data)) => {
                reached_clipboard = true;
                if status.success() {
                    return Ok(data);
                }
            }
            Err(source) => error = Some(source),
        }
    }
    match error {
        Some(error) if !reached_clipboard => Err(error),
        _ => Ok(Vec::new()),
    }
}
