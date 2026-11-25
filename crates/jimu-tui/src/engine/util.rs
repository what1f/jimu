use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Human-readable size in powers of 1024: integral bytes (`512 B`), one
/// decimal above (`1.5 MB`). Every output fits nine columns.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: &[&str] = &["KB", "MB", "GB", "TB", "PB"];
    let mut val = bytes as f64 / 1024.0;
    for unit in UNITS {
        if val < 1023.95 {
            return format!("{val:.1} {unit}");
        }
        val /= 1024.0;
    }
    format!("{val:.1} EB")
}

/// A closed stdout (when piping output through `head`) is a clean stop, not a failure.
pub fn ignore_broken_pipe(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Path to `pager.toml` in the host's configuration directory.
pub fn pager_toml_path(config_home: &Path) -> PathBuf {
    config_home.join("pager.toml")
}

/// Use a home-relative label for the host's default directory, or `$CONFIG_HOME` for an override.
pub fn display_config_home_prefix_for(home: &Path, default: &Path) -> String {
    if home == default || default.canonicalize().is_ok_and(|path| path == home) {
        abbreviate_path(&default.to_string_lossy()).into_owned()
    } else {
        "$CONFIG_HOME".to_owned()
    }
}

/// User-facing path relative to the host's configuration directory.
pub fn display_user_config_path(home: &Path, default: &Path, relative: impl AsRef<Path>) -> String {
    let rel = relative.as_ref();
    let prefix = display_config_home_prefix_for(home, default);
    if rel.as_os_str().is_empty() {
        return prefix;
    }
    format!("{prefix}/{}", rel.display())
}

/// Abbreviate an absolute path under the user's home directory.
pub fn abbreviate_path(path: &str) -> Cow<'_, str> {
    abbreviate_path_with_config_home(path, None)
}

/// Prefer the host's configuration directory when supplied, then the user's home directory.
pub fn abbreviate_path_with_config_home<'a>(
    path: &'a str,
    config_home: Option<(&Path, &Path)>,
) -> Cow<'a, str> {
    let path_buf = Path::new(path);
    if let Some((home, default)) = config_home
        && let Ok(rest) = path_buf.strip_prefix(home)
    {
        return Cow::Owned(display_user_config_path(home, default, rest));
    }
    if let Some(home) = std::env::home_dir() {
        // Path::strip_prefix is separator-aware; a string prefix plus `starts_with('/')` misses Windows `\` remainders after USERPROFILE
        if !home.as_os_str().is_empty()
            && let Ok(rest) = path_buf.strip_prefix(&home)
        {
            if rest.as_os_str().is_empty() {
                return Cow::Borrowed("~");
            }
            return Cow::Owned(format!("~/{}", rest.display()));
        }
    }
    Cow::Borrowed(path)
}

/// Location-chrome path: [`abbreviate_path`] then last-two-component shortening.
/// Clipboard and other callers keep [`abbreviate_path`].
pub fn display_location_path(path: impl AsRef<Path>) -> String {
    let lossy = path.as_ref().to_string_lossy();
    crate::engine::location_path::shorten_location_path(&abbreviate_path(&lossy)).into_owned()
}

/// True when `path` is under the host's user configuration directory.
pub fn is_under_user_config_home(path: &Path, config_home: &Path) -> bool {
    path.starts_with(config_home)
}

/// Compact duration: `5.2s`, `32s`, `2m5s`, `1h2m`.
pub fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 10 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{mins}m{secs}s");
    }
    let hours = mins / 60;
    let remaining_mins = mins % 60;
    format!("{hours}h{remaining_mins}m")
}

/// Coarse recency for age columns: `just now`, `5m`, `3h`, `2d`, `1mo`, `1y`.
/// Buckets stay wide so the column does not churn at second granularity.
pub fn format_time_ago(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        let mins = secs / 60;
        return format!("{mins}m");
    }
    if secs < 86400 {
        let hours = secs / 3600;
        return format!("{hours}h");
    }
    let days = secs / 86400;
    if days < 30 {
        return format!("{days}d");
    }
    if days < 365 {
        let months = days / 30;
        return format!("{months}mo");
    }
    let years = days / 365;
    format!("{years}y")
}

/// Wall-clock [`SystemTime`] from unix-epoch millis.
/// A non-positive value is the `#[serde(default)]` sentinel for a missing timestamp and reads as now.
pub fn system_time_from_unix_ms(unix_ms: i64) -> SystemTime {
    if unix_ms <= 0 {
        return SystemTime::now();
    }
    UNIX_EPOCH
        .checked_add(Duration::from_millis(unix_ms as u64))
        .unwrap_or_else(SystemTime::now)
}

/// Project a monotonic [`Instant`] onto the wall clock, so live local anchors and on-disk timestamps compare in one space.
/// On-disk timestamps can predate boot, and so have no `Instant`.
/// The skew between the two `now()` samples is below [`format_time_ago`]'s minute granularity.
pub fn system_time_from_instant(instant: Instant) -> SystemTime {
    SystemTime::now()
        .checked_sub(instant.elapsed())
        .unwrap_or_else(SystemTime::now)
}

pub fn parse_schedule_interval_secs(human: &str) -> Option<u64> {
    let s = human.trim_start();
    if !s.starts_with("every ") {
        return None;
    }
    let rest = s.get(6..)?.trim_start();
    let (num_str, unit) = if let Some(sp) = rest.find(char::is_whitespace) {
        (rest.get(..sp)?, rest.get(sp + 1..)?)
    } else if rest.len() >= 2 {
        let (d, u) = rest.split_at(rest.len() - 1);
        (d, u)
    } else {
        return None;
    };
    let n: u64 = num_str.parse().ok()?;
    let unit = unit.trim();
    let secs_per = match unit {
        "s" | "second" | "seconds" => 1,
        "m" | "minute" | "minutes" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    Some(n * secs_per)
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Compact `N ago` age relative to `now`; future timestamps saturate to `0s ago`.
/// [`format_time_ago`] buckets coarser and drops the `ago`.
pub fn format_age(created_at: i64, now: i64) -> String {
    let delta = now.saturating_sub(created_at).max(0);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

/// Truncate to at most `max_width` display columns (CJK counts 2), ending with `…` when cut; a zero budget yields an empty string.
pub fn truncate_to_width(s: &str, max_width: usize) -> Cow<'_, str> {
    if byte_offset_at_width(s, max_width) == s.len() {
        return Cow::Borrowed(s);
    }
    if max_width == 0 {
        return Cow::Borrowed("");
    }
    let end = byte_offset_at_width(s, max_width - 1);
    let Some(prefix) = s.get(..end) else {
        return Cow::Borrowed(s);
    };
    Cow::Owned(format!("{prefix}…"))
}

/// Byte offset at which display width would exceed `max_width`, or `s.len()`.
pub fn byte_offset_at_width(s: &str, max_width: usize) -> usize {
    let mut width = 0;
    for (i, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return i;
        }
        width += cw;
    }
    s.len()
}

/// Left-align `s` in `width` display columns.
/// `format!`'s `{:<width$}` pads by char count, which shears columns after wide (e.g. CJK) glyphs.
pub fn pad_to_width(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(s.width());
    let mut out = String::with_capacity(s.len() + pad);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

/// Group a count's digits with commas for display: `1234567` becomes `"1,234,567"`.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
