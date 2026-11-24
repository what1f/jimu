//! Link detection for the scrollback render pass.
//!
//! [`scan_lines_for_url_overlays`] detects plain-text URLs and absolute file paths across all block types and collects them into a [`LinkOverlay`].
//! The collected links reach the terminal as `LinkSpan`s.
//! The frame diff (`crate::engine::inline::Terminal::flush_with_links`) emits them as OSC 8 hyperlinks.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use linkify::{LinkFinder, LinkKind};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

/// Semantic destination of a pager link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(Arc<str>),
    File(Arc<Path>),
}

/// Whether the painted text can independently identify its semantic target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LinkPresentation {
    #[default]
    Opaque,
    SelfResolvingPath,
}

/// Output and activation policy for a semantic link target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLinkTarget {
    /// OSC 8 destination for the terminal, or `None` to leave link detection to the terminal's own plain-text scanning.
    pub osc8_url: Option<Arc<str>>,
    /// Target the app opens on activation, or `None` to leave opening to the terminal.
    pub open_target: Option<LinkTarget>,
}

/// Resolve one semantic target using the current terminal context.
pub fn resolve_link_target(target: &LinkTarget) -> Option<ResolvedLinkTarget> {
    resolve_link_target_with_presentation(target, LinkPresentation::Opaque)
}

pub fn resolve_link_target_with_presentation(
    target: &LinkTarget,
    presentation: LinkPresentation,
) -> Option<ResolvedLinkTarget> {
    resolve_link_target_for_context(
        target,
        presentation,
        crate::engine::input::terminal::terminal_context(),
    )
}

/// Resolve one semantic target for both OSC 8 output and app-owned activation.
pub fn resolve_link_target_for_context(
    target: &LinkTarget,
    presentation: LinkPresentation,
    terminal: &crate::engine::input::terminal::TerminalContext,
) -> Option<ResolvedLinkTarget> {
    match target {
        LinkTarget::Url(url) => {
            let filter = crate::engine::input::terminal::hyperlinks::SchemeFilter::Standard;
            crate::engine::render::link_opener::is_safe_to_open(url, filter).then(|| {
                ResolvedLinkTarget {
                    osc8_url: Some(Arc::clone(url)),
                    open_target: Some(LinkTarget::Url(Arc::clone(url))),
                }
            })
        }
        LinkTarget::File(_)
            if terminal.brand == crate::engine::input::terminal::TerminalName::VsCode
                && terminal.is_official_vscode_remote
                && presentation == LinkPresentation::SelfResolvingPath =>
        {
            Some(ResolvedLinkTarget {
                osc8_url: None,
                open_target: None,
            })
        }
        LinkTarget::File(path) => Some(ResolvedLinkTarget {
            osc8_url: file_path_to_url(path),
            open_target: Some(LinkTarget::File(Arc::clone(path))),
        }),
    }
}

/// Resolve the target for app-owned activation.
pub fn resolve_link_open_target(target: &LinkTarget) -> Option<LinkTarget> {
    resolve_link_target(target).and_then(|resolved| resolved.open_target)
}

/// A single link region on screen.
#[derive(Debug, Clone)]
pub struct OverlayLink {
    pub screen_row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub target: LinkTarget,
    pub presentation: LinkPresentation,
    pub id: Option<u32>,
}

/// First id minted for scanner-stamped links.
/// Markdown-mapped links carry small per-document source ids counted up from 0, so scanner ids come from the upper half of the `u32` space.
/// A shared id would make `VisibleLinkMap` and OSC 8 `id=` grouping treat a scanned match and a markdown link to the same URL as one hyperlink.
const SCANNER_ID_BASE: u32 = 1 << 31;

/// Accumulates link positions for post-flush OSC 8 emission.
#[derive(Debug, Clone)]
pub struct LinkOverlay {
    links: Vec<OverlayLink>,
    /// Next id for scanner-emitted wrap fragments.
    /// Markdown-mapped links carry their own ids; this only stamps scanned matches.
    next_id: u32,
}

impl Default for LinkOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkOverlay {
    pub fn new() -> Self {
        Self {
            links: Vec::new(),
            next_id: SCANNER_ID_BASE,
        }
    }

    pub fn push(&mut self, link: OverlayLink) {
        debug_assert!(
            link.col_start <= link.col_end,
            "OverlayLink col_start ({}) > col_end ({})",
            link.col_start,
            link.col_end
        );
        if link.col_start > link.col_end {
            return; // Silently skip inverted ranges in release mode.
        }
        // Keep `next_id` above every id in the overlay so a later scan never re-mints an id carried in via `extend_from`
        if let Some(id) = link.id {
            self.next_id = self.next_id.max(id.saturating_add(1));
        }
        self.links.push(link);
    }

    /// Append all links from `other` (clones each `OverlayLink`).
    /// Each link is routed through [`Self::push`], so inverted ranges are silently dropped in release builds just like the single-link path.
    pub fn extend_from(&mut self, other: &LinkOverlay) {
        self.links.reserve(other.links.len());
        for link in &other.links {
            self.push(link.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    pub fn links(&self) -> &[OverlayLink] {
        &self.links
    }

    /// Returns `true` if an existing link overlaps the given screen region.
    pub fn overlaps(&self, screen_row: u16, col_start: u16, col_end: u16) -> bool {
        self.links
            .iter()
            .any(|l| l.screen_row == screen_row && l.col_start < col_end && col_start < l.col_end)
    }
}

fn link_finder() -> &'static LinkFinder {
    static FINDER: OnceLock<LinkFinder> = OnceLock::new();
    FINDER.get_or_init(|| {
        let mut f = LinkFinder::new();
        f.kinds(&[LinkKind::Url, LinkKind::Email]);
        f
    })
}

/// True when `start..end` is a valid UTF-8 substring of `text`.
fn is_str_range(text: &str, start: usize, end: usize) -> bool {
    start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
}

/// linkify's Email kind yields the bare address; the opener expects `mailto:`.
/// Returns `None` for scp-style remotes (`git@github.com:org/repo`) so those stay plain text instead of becoming a mail link.
/// Also returns `None` when the match range is not a char-boundary substring.
fn linkify_href(text: &str, link: &linkify::Link<'_>) -> Option<String> {
    if !is_str_range(text, link.start(), link.end()) {
        return None;
    }
    match link.kind() {
        LinkKind::Email => {
            if matches!(text.as_bytes().get(link.end()), Some(b':' | b'/')) {
                return None;
            }
            Some(format!("mailto:{}", link.as_str()))
        }
        _ => Some(link.as_str().to_string()),
    }
}

/// One path segment without spaces (`main.rs`, `.cache`, `@scope`). Leading `.`
/// matches dot-directories and `%` matches percent-encoded segments; grok
/// attachment paths can include hidden directories and percent-encoded components.
const PATH_SEGMENT: &str = r"[a-zA-Z0-9_@.%][a-zA-Z0-9._+@%\-]*";

/// Final path segment may contain *internal* spaces for macOS app bundles and similarly named files (`Demo App.app`).
/// Requires a `.ext` suffix after the last space so trailing prose (`…/bar here.`) is not consumed.
const PATH_SEGMENT_SPACED: &str =
    r"[a-zA-Z0-9_@.%][a-zA-Z0-9._+@%\-]*(?: [a-zA-Z0-9._+@%\-]+)+\.[a-zA-Z0-9][a-zA-Z0-9._+@%\-]*";

/// Relative file path (`images/1.png`, `.cache/x.txt`): one or more `/`-joined directory segments plus a filename that has an extension.
/// No leading `/` or `~` (those are the absolute forms).
/// The required extension keeps slashed prose ("and/or", "TCP/IP") out; the caller still checks that the file exists under `cwd`.
fn relative_file_path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pat = format!(
            r"(?:{seg}/)+[a-zA-Z0-9_@%+\-]+(?:\.[a-zA-Z0-9_@%+\-]+)+",
            seg = PATH_SEGMENT,
        );
        regex::Regex::new(&pat).expect("relative file path regex")
    })
}

fn file_path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Leading `~` expands to $HOME. Only the final segment may contain spaces, and that form is preferred so `Demo App.app` is not cut at the space.
        let pat = format!(
            r"~?/(?:{seg}/)+(?:{spaced}|{seg})",
            seg = PATH_SEGMENT,
            spaced = PATH_SEGMENT_SPACED,
        );
        regex::Regex::new(&pat).expect("file path regex")
    })
}

/// Paths wrapped in single or double quotes, including spaces in any segment.
/// Group 1 is the opening quote; group 2 is the path (no surrounding quotes).
/// Caller must verify the character immediately after the path is the same quote.
fn quoted_file_path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The regex matches the opening quote plus the path; the closing quote is checked in code (the regex crate has no backreferences)
        // The path allows spaces in segments and requires at least two `/`-separated components
        // `"/Users/me/My Dir/file.app"` or `'~/Desktop/My Notes/todo.md'`
        let seg = r#"[^/"']+"#;
        let pat = format!(r#"(["'])(~?/(?:{seg}/)+{seg})"#);
        regex::Regex::new(&pat).expect("quoted file path regex")
    })
}

/// Turn a display path (`/abs/…` or `~/…`) into a semantic filesystem target.
/// Relative paths fail; use [`tool_path_file_target`] to join cwd first.
pub fn path_to_file_target(path: &str) -> Option<LinkTarget> {
    tool_path_file_target(path, None)
}

fn file_path_to_url(path: &Path) -> Option<Arc<str>> {
    url::Url::from_file_path(path)
        .ok()
        .map(|u| Arc::from(u.as_str()))
}

/// Semantic target for a Read/Edit path, joining ordinary relative paths to `cwd`.
pub fn tool_path_file_target(path: &str, cwd: Option<&Path>) -> Option<LinkTarget> {
    crate::engine::render::tool_paths::resolve_tool_path_target(path, cwd)
        .map(|path| LinkTarget::File(Arc::from(path)))
}

fn file_link_presentation_for_resolved(
    painted: &str,
    target: &LinkTarget,
    cwd: Option<&Path>,
    resolved: Option<&Path>,
) -> LinkPresentation {
    let LinkTarget::File(target_path) = target else {
        return LinkPresentation::Opaque;
    };
    let painted_path = Path::new(painted);
    let is_absolute = painted_path.is_absolute()
        || matches!(
            painted_path.components().next(),
            Some(std::path::Component::Prefix(_))
        );
    let is_home_relative =
        painted == "~" || painted.starts_with("~/") || painted.starts_with(r"~\");
    if !is_absolute && !is_home_relative && (!painted.contains(['/', '\\']) || cwd.is_none()) {
        return LinkPresentation::Opaque;
    }
    resolved
        .filter(|resolved| *resolved == target_path.as_ref())
        .map_or(LinkPresentation::Opaque, |_| {
            LinkPresentation::SelfResolvingPath
        })
}

/// Classify painted file text only when it independently resolves to `target`.
pub fn file_link_presentation(
    painted: &str,
    target: &LinkTarget,
    cwd: Option<&Path>,
) -> LinkPresentation {
    let resolved = crate::engine::render::tool_paths::resolve_tool_path_target(painted, cwd);
    file_link_presentation_for_resolved(painted, target, cwd, resolved.as_deref())
}

/// Web, mailto, tel, and anchors are `None`. Relative paths prefer a unique `media_paths` suffix (stable across forks); ambiguous matches stay unlinked.
/// Else join to `cwd` and stay inside it. `cwd = None` disables that fallback.
pub fn local_link_to_file_target(
    dest: &str,
    media_paths: &[PathBuf],
    cwd: Option<&Path>,
) -> Option<LinkTarget> {
    let dest = dest.trim();
    if dest.is_empty() || dest.starts_with('#') || dest.contains("://") {
        return None;
    }
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("mailto:") || lower.starts_with("tel:") {
        return None;
    }
    let path = Path::new(dest);
    let target = crate::engine::render::tool_paths::resolve_tool_path_target(dest, None)?;
    let resolved: PathBuf = if target.is_absolute() {
        target
    } else {
        relative_link_target(path, media_paths, cwd)?
    };
    if !resolved.is_file() {
        return None;
    }
    Some(LinkTarget::File(Arc::from(resolved)))
}

/// Unique generated-media match, else a `cwd` path that stays inside `cwd`. Ambiguous media matches resolve to neither.
fn relative_link_target(
    path: &Path,
    media_paths: &[PathBuf],
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let mut hits = media_paths.iter().filter(|p| p.ends_with(path));
    match (hits.next(), hits.next()) {
        (Some(unique), None) => return Some(unique.clone()),
        // Ambiguous generated-media reference (e.g. a fork with duplicate names): resolve to neither rather than opening the wrong file.
        (Some(_), Some(_)) => return None,
        _ => {}
    }
    // Not generated media: resolve against the session cwd, refusing any `..` that would escape it
    let cwd = cwd?;
    let joined = crate::engine::render::tool_paths::normalize_lexically(&cwd.join(path));
    joined
        .starts_with(crate::engine::render::tool_paths::normalize_lexically(cwd))
        .then_some(joined)
}

/// Convert a display-cell column to a `u16` suitable for overlay coordinates.
/// Returns `None` when the column (plus content offset) would overflow `u16`, in which case the caller should skip the link.
fn to_overlay_col(content_x: u16, col: usize) -> Option<u16> {
    let col16 = u16::try_from(col).ok()?;
    content_x.checked_add(col16)
}

/// One visual row of a logical (pre-wrap) line: its screen row plus the byte range its text occupies within the joined logical string.
struct RowSegment {
    screen_row: u16,
    start: usize,
    end: usize,
}

/// Soft-wrapped rows (`Some` joiner) are re-joined before matching so a wrapped path is detected whole; styling boundaries must not truncate a match.
/// Overlaps with existing markdown links are skipped. Relative paths link only on a unique `media_paths` match.
pub fn scan_lines_for_url_overlays<'a>(
    lines: impl Iterator<Item = (u16, &'a Line<'static>, Option<&'a str>)>,
    content_x: u16,
    media_paths: &[PathBuf],
    overlay: &mut LinkOverlay,
) {
    // Joined text and row segments for the logical line currently being accumulated
    // Buffers are reused across groups to avoid per-row allocation on every render frame
    let mut group_text = String::new();
    let mut group_rows: Vec<RowSegment> = Vec::new();

    for (screen_row, line, joiner) in lines {
        // A `None` joiner is a hard break: flush the current group and start a new logical line
        // A `Some` joiner with no accumulated rows (e.g. a wrap continuation scrolled in at the top of the viewport) also starts a new group.
        // Its fragment is scanned standalone
        match joiner {
            Some(j) if !group_rows.is_empty() => group_text.push_str(j),
            _ => {
                scan_logical_line(&group_text, &group_rows, content_x, media_paths, overlay);
                group_text.clear();
                group_rows.clear();
            }
        }
        let start = group_text.len();
        for span in &line.spans {
            group_text.push_str(span.content.as_ref());
        }
        group_rows.push(RowSegment {
            screen_row,
            start,
            end: group_text.len(),
        });
    }
    scan_logical_line(&group_text, &group_rows, content_x, media_paths, overlay);
}

/// Skip rows that already have an overlay individually so a continuation row is still linked. `false` if every row was skipped or a column exceeds `u16`.
fn push_link_segments(
    text: &str,
    rows: &[RowSegment],
    content_x: u16,
    match_range: std::ops::Range<usize>,
    target: &LinkTarget,
    presentation: LinkPresentation,
    overlay: &mut LinkOverlay,
) -> bool {
    let mut segments: Vec<(u16, u16, u16)> = Vec::new();
    for row in rows {
        // Intersect the match with this row's byte range; joiner bytes between rows belong to no row and are clamped away
        let start = match_range.start.max(row.start);
        let end = match_range.end.min(row.end);
        if start >= end {
            continue;
        }
        let Some(before) = text.get(row.start..start) else {
            continue;
        };
        let Some(matched) = text.get(start..end) else {
            continue;
        };
        let col_start = UnicodeWidthStr::width(before);
        let col_end = col_start + UnicodeWidthStr::width(matched);
        let (Some(cs), Some(ce)) = (
            to_overlay_col(content_x, col_start),
            to_overlay_col(content_x, col_end),
        ) else {
            return false;
        };
        if overlay.overlaps(row.screen_row, cs, ce) {
            continue;
        }
        segments.push((row.screen_row, cs, ce));
    }
    if segments.is_empty() {
        return false;
    }
    let id = overlay.next_id;
    overlay.next_id = overlay.next_id.saturating_add(1);
    for (screen_row, col_start, col_end) in segments {
        overlay.push(OverlayLink {
            screen_row,
            col_start,
            col_end,
            target: target.clone(),
            presentation,
            id: Some(id),
        });
    }
    true
}

/// Run URL and file-path detection over one joined logical line and emit per-row overlay regions for every match (see [`push_link_segments`]).
fn scan_logical_line(
    text: &str,
    rows: &[RowSegment],
    content_x: u16,
    media_paths: &[PathBuf],
    overlay: &mut LinkOverlay,
) {
    if text.is_empty() || rows.is_empty() {
        return;
    }
    let scheme_filter = crate::engine::input::terminal::hyperlinks::SchemeFilter::Standard;
    let finder = link_finder();
    let path_re = file_path_regex();
    let quoted_path_re = quoted_file_path_regex();
    let rel_path_re = relative_file_path_regex();

    // Byte ranges consumed by URL links, populated lazily on first safe URL hit to avoid allocation in the common case of lines with no URLs
    let mut url_byte_ranges: Option<Vec<std::ops::Range<usize>>> = None;
    // Byte ranges already turned into file-path overlays (quoted first, then plain) so later passes do not double-link
    let mut path_byte_ranges: Vec<std::ops::Range<usize>> = Vec::new();

    for link in finder.links(text) {
        let Some(url) = linkify_href(text, &link) else {
            continue;
        };
        if !crate::engine::render::link_opener::is_safe_to_open(&url, scheme_filter) {
            continue;
        }
        url_byte_ranges
            .get_or_insert_with(Vec::new)
            .push(link.start()..link.end());

        let target = LinkTarget::Url(Arc::from(url.as_str()));
        push_link_segments(
            text,
            rows,
            content_x,
            link.start()..link.end(),
            &target,
            LinkPresentation::Opaque,
            overlay,
        );
    }

    let range_overlaps_urls = |start: usize, end: usize| -> bool {
        url_byte_ranges
            .as_ref()
            .is_some_and(|ranges| ranges.iter().any(|r| start < r.end && r.start < end))
    };

    // Pass 1: quoted paths (spaces allowed in every segment)
    for caps in quoted_path_re.captures_iter(text) {
        let open_q = caps.get(1).expect("open quote");
        let path_m = caps.get(2).expect("path group");
        // Require a matching closing quote immediately after the path.
        let close_idx = path_m.end();
        let Some(&open_byte) = open_q.as_str().as_bytes().first() else {
            continue;
        };
        if text.as_bytes().get(close_idx) != Some(&open_byte) {
            continue;
        }
        if range_overlaps_urls(path_m.start(), path_m.end()) {
            continue;
        }
        let Some(file_target) = path_to_file_target(path_m.as_str()) else {
            continue;
        };

        // Clickable region is the path text only (not the quotes).
        if push_link_segments(
            text,
            rows,
            content_x,
            path_m.start()..path_m.end(),
            &file_target,
            file_link_presentation(path_m.as_str(), &file_target, None),
            overlay,
        ) {
            path_byte_ranges.push(path_m.start()..path_m.end());
        }
    }

    // Pass 2: unquoted paths (final segment may include spaces and an extension)
    for m in path_re.find_iter(text) {
        if range_overlaps_urls(m.start(), m.end())
            || path_byte_ranges
                .iter()
                .any(|r| m.start() < r.end && r.start < m.end())
        {
            continue;
        }
        if m.start() > 0 {
            let Some(&prev) = m
                .start()
                .checked_sub(1)
                .and_then(|i| text.as_bytes().get(i))
            else {
                continue;
            };
            if prev.is_ascii_alphanumeric()
                || matches!(prev, b'_' | b'.' | b'+' | b'@' | b'-' | b':' | b'/' | b'~')
            {
                continue;
            }
        }
        // Drop trailing sentence punctuation so a path ending a sentence (`…/images/1.jpg.`) links to the file, not `file.jpg.`
        let path = m
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        if path.is_empty() {
            continue;
        }
        let path_end = m.start() + path.len();
        let Some(file_target) = path_to_file_target(path) else {
            continue;
        };

        if push_link_segments(
            text,
            rows,
            content_x,
            m.start()..path_end,
            &file_target,
            file_link_presentation(path, &file_target, None),
            overlay,
        ) {
            path_byte_ranges.push(m.start()..path_end);
        }
    }

    // Pass 3: relative paths that uniquely match a generated media file (so bare `word/word.ext` prose is not over-linkified)
    if !media_paths.is_empty() {
        for m in rel_path_re.find_iter(text) {
            if range_overlaps_urls(m.start(), m.end())
                || path_byte_ranges
                    .iter()
                    .any(|r| m.start() < r.end && r.start < m.end())
            {
                continue;
            }
            if m.start() > 0 {
                let Some(&prev) = m
                    .start()
                    .checked_sub(1)
                    .and_then(|i| text.as_bytes().get(i))
                else {
                    continue;
                };
                if prev.is_ascii_alphanumeric()
                    || matches!(
                        prev,
                        b'_' | b'.' | b'+' | b'@' | b'-' | b':' | b'/' | b'~' | b'%'
                    )
                {
                    continue;
                }
            }
            let path = m
                .as_str()
                .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
            // Bare relative paths in prose stay media-only (no cwd fallback), so ordinary `a/b.ext`-shaped prose is not over-linkified
            let Some(file_target) = local_link_to_file_target(path, media_paths, None) else {
                continue;
            };
            let path_end = m.start() + path.len();

            if push_link_segments(
                text,
                rows,
                content_x,
                m.start()..path_end,
                &file_target,
                LinkPresentation::Opaque,
                overlay,
            ) {
                path_byte_ranges.push(m.start()..path_end);
            }
        }
    }
}
