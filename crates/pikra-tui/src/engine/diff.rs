//! Diff hunk construction for terminal display.
//!
//! Turns edit-tool output into line-tagged [`DiffHunk`]s for the pager to render, and back into unified-diff text.
//! The input is structured edit records, ACP `ToolCall` payloads, or plain before/after text.

#![deny(clippy::indexing_slicing)]

use serde::Deserialize;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DiffEdit {
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// One match resolved against the original and updated file. A replace-all
/// operation may produce several matches with independent line numbers.
#[derive(Clone, Debug, Deserialize)]
pub struct DiffEditDetail {
    #[serde(flatten)]
    pub edit: DiffEdit,
    pub old_line: usize,
    pub new_line: usize,
    pub context_before: String,
    pub context_after: String,
    #[serde(default)]
    pub line_prefix: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub text: String,
    pub lo: usize,
    pub ln: usize,
    pub tag: ChangeTag,
}

pub type DiffHunk = Vec<DiffLine>;

/// Unchanged lines kept on each side of a change
const MAX_CONTEXT: usize = 3;

/// Whole-file diffs run on the render thread; past this `similar` returns a coarser but still correct diff
const WHOLE_FILE_DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Render stand-alone replacements with line numbers relative to each edit.
pub fn build_diff_hunks(edits: &[DiffEdit]) -> Vec<DiffHunk> {
    edits
        .iter()
        .filter_map(|edit| build_hunk(edit, 1, 1, "", "", ""))
        .collect()
}

pub fn build_located_diff_hunks(details: &[DiffEditDetail]) -> Vec<DiffHunk> {
    details
        .iter()
        .filter_map(|detail| {
            build_hunk(
                &detail.edit,
                detail.old_line,
                detail.new_line,
                &detail.context_before,
                &detail.context_after,
                &detail.line_prefix,
            )
        })
        .collect()
}

fn build_hunk(
    edit: &DiffEdit,
    old_line: usize,
    new_line: usize,
    context_before: &str,
    context_after: &str,
    line_prefix: &str,
) -> Option<DiffHunk> {
    let mut diff_lines: DiffHunk = Vec::new();
    let before_lines: Vec<String> = if context_before.is_empty() {
        vec![]
    } else {
        context_before
            .split_inclusive('\n')
            .map(|s| s.to_string())
            .collect()
    };
    let n_before = before_lines.len();
    for (i, line_text) in before_lines.into_iter().enumerate() {
        // The last context_before line sits just above old_line/new_line, hence the +1
        let from_end = n_before.saturating_sub(i + 1);
        let lo = old_line.saturating_sub(from_end + 1);
        let ln = new_line.saturating_sub(from_end + 1);
        diff_lines.push(DiffLine {
            text: line_text,
            lo,
            ln,
            tag: ChangeTag::Equal,
        });
    }
    let (mut lo, mut ln) = (old_line, new_line);
    let empty_to_empty = edit.old_string.is_empty() && edit.new_string.is_empty();
    let mid_file = !context_before.is_empty() || !context_after.is_empty();
    let new_text: &str = if empty_to_empty && mid_file {
        // Blank-line insertion: both sides empty but a line was inserted.
        // Represent the blank line so TextDiff can see the insertion.
        // Empty-to-empty with no context is an empty file write and must produce no diff lines, not a fabricated one-line insertion
        "\n"
    } else {
        &edit.new_string
    };
    // When old_string/new_string start mid-line (after indentation), the diff lines are missing the leading whitespace that context lines have
    // Prepend `line_prefix` to each change on the first file line
    // Once a change contains a newline, subsequent lines are full file lines and don't need the prefix
    let prefix = line_prefix;
    let has_prefix = !prefix.is_empty();
    let mut prefix_applied_delete = false;
    let mut prefix_applied_insert = false;
    let diff = TextDiff::from_lines(edit.old_string.as_str(), new_text);
    for change in diff.iter_all_changes() {
        let tag = change.tag();
        let mut text = change.value().to_owned();
        if has_prefix {
            let needs_prefix = match tag {
                ChangeTag::Delete => !prefix_applied_delete,
                ChangeTag::Insert => !prefix_applied_insert,
                ChangeTag::Equal => !prefix_applied_delete && !prefix_applied_insert,
            };
            if needs_prefix {
                text.insert_str(0, prefix);
            }
            match tag {
                ChangeTag::Delete | ChangeTag::Equal => prefix_applied_delete = true,
                ChangeTag::Insert => prefix_applied_insert = true,
            }
        }
        diff_lines.push(DiffLine { text, lo, ln, tag });
        match tag {
            ChangeTag::Equal => {
                lo = lo.saturating_add(1);
                ln = ln.saturating_add(1);
            }
            ChangeTag::Delete => {
                lo = lo.saturating_add(1);
            }
            ChangeTag::Insert => {
                ln = ln.saturating_add(1);
            }
        }
    }

    if !context_after.is_empty() {
        for line in context_after.split_inclusive('\n') {
            diff_lines.push(DiffLine {
                text: line.to_owned(),
                lo,
                ln,
                tag: ChangeTag::Equal,
            });
            lo = lo.saturating_add(1);
            ln = ln.saturating_add(1);
        }
    }

    let total_len = diff_lines.len();
    let mut start;
    let mut end = total_len;
    if diff_lines.iter().all(|entry| entry.tag == ChangeTag::Equal) {
        start = end; // The slice becomes empty, so no hunk is pushed
    } else {
        let equal_before = diff_lines
            .iter()
            .take_while(|entry| entry.tag == ChangeTag::Equal)
            .count();
        let equal_after = diff_lines
            .iter()
            .rev()
            .take_while(|entry| entry.tag == ChangeTag::Equal)
            .count();
        start = equal_before.saturating_sub(MAX_CONTEXT);
        end = total_len.saturating_sub(equal_after.saturating_sub(MAX_CONTEXT));
    }

    while start < end {
        let Some(entry) = diff_lines.get(start) else {
            break;
        };
        if entry.tag == ChangeTag::Equal && entry.text.trim_ascii().is_empty() {
            start += 1;
        } else {
            break;
        }
    }
    while start < end {
        let Some(entry) = end.checked_sub(1).and_then(|i| diff_lines.get(i)) else {
            break;
        };
        if entry.tag == ChangeTag::Equal && entry.text.trim_ascii().is_empty() {
            end -= 1;
        } else {
            break;
        }
    }

    if start < end
        && let Some(slice) = diff_lines.get(start..end)
    {
        return Some(slice.to_vec());
    }
    None
}

/// Builds diff hunks from before/after text alone, with no structured edit records.
/// The ACP `ToolCallContent::Diff` fallback uses this for pre-execution previews and for backends that send whole files.
/// Changes more than twice `MAX_CONTEXT` unchanged lines apart go into separate hunks, so a whole-file diff never shows the unchanged middle.
pub fn diff_hunks_from_strings(old_text: &str, new_text: &str, start_line: usize) -> Vec<DiffHunk> {
    let diff = TextDiff::configure()
        .timeout(WHOLE_FILE_DIFF_TIMEOUT)
        .diff_lines(old_text, new_text);

    diff.grouped_ops(MAX_CONTEXT)
        .iter()
        .map(|group| {
            let mut hunk = DiffHunk::new();
            for op in group {
                let mut lo = op.old_range().start.saturating_add(start_line);
                let mut ln = op.new_range().start.saturating_add(start_line);
                for change in diff.iter_changes(op) {
                    let tag = change.tag();
                    hunk.push(DiffLine {
                        text: change.value().to_owned(),
                        lo,
                        ln,
                        tag,
                    });
                    match tag {
                        ChangeTag::Equal => {
                            lo = lo.saturating_add(1);
                            ln = ln.saturating_add(1);
                        }
                        ChangeTag::Delete => lo = lo.saturating_add(1),
                        ChangeTag::Insert => ln = ln.saturating_add(1),
                    }
                }
            }
            hunk
        })
        .collect()
}

/// Fold consecutive edits of the same file in post-state `ln` coordinates so merged hunks don't repeat context or show intermediate states.
/// A later edit of a shown context line replaces that Equal row; a line edited twice collapses to `-original +final`.
/// If shared `ln` coordinates cannot describe the pair truthfully, keep separate hunks — the bail exists so the pager never renders wrong content.
pub fn stitch_overlapping_hunks(hunks: Vec<DiffHunk>) -> Vec<DiffHunk> {
    let mut out: Vec<DiffHunk> = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        if let Some(last) = out.last_mut()
            && let Some(stitched) = stitch_hunk_pair(last, &hunk)
        {
            *last = stitched;
            continue;
        }
        out.push(hunk);
    }
    out
}

/// Returns the post-state (`ln`) range that a hunk's rendered (Equal and Insert) rows cover.
fn render_range(hunk: &DiffHunk) -> Option<(usize, usize)> {
    let mut range: Option<(usize, usize)> = None;
    for line in hunk {
        if line.tag == ChangeTag::Delete {
            continue;
        }
        range = Some(match range {
            None => (line.ln, line.ln),
            Some((min, max)) => (min.min(line.ln), max.max(line.ln)),
        });
    }
    range
}

/// Returns the index of the row in `hunk` that renders post-state line `ln` (Equal or Insert).
fn render_pos(hunk: &DiffHunk, ln: usize) -> Option<usize> {
    hunk.iter()
        .position(|l| l.tag != ChangeTag::Delete && l.ln == ln)
}

fn trimmed(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

/// Folds `b` into `a` when both describe one contiguous post-state region; `None` keeps the pair as separate hunks.
fn stitch_hunk_pair(a: &DiffHunk, b: &DiffHunk) -> Option<DiffHunk> {
    let (a_min, a_max) = render_range(a)?;
    let (b_min, _) = render_range(b)?;
    if b_min < a_min || b_min > a_max + 1 {
        return None;
    }

    let mut out = a.clone();
    let mut max_ln = a_max;
    let mut i = 0;
    while i < b.len() {
        let Some(row) = b.get(i) else { break };
        if row.ln > max_ln {
            // Past the stitched coverage, `b` is the sole source for this tail, so splice its remaining rows in verbatim
            // Rendered rows must stay contiguous
            let tail = b.get(i..)?;
            for rest in tail {
                if rest.tag != ChangeTag::Delete {
                    if rest.ln != max_ln + 1 {
                        return None;
                    }
                    max_ln = rest.ln;
                }
                out.push(rest.clone());
            }
            break;
        }
        match row.tag {
            ChangeTag::Equal => {
                let pos = render_pos(&out, row.ln)?;
                if trimmed(&out.get(pos)?.text) != trimmed(&row.text) {
                    return None;
                }
                i += 1;
            }
            ChangeTag::Delete => {
                // Only single-line replacement pairs keep line counts (and therefore every later `ln`) truthful
                let next = b.get(i + 1)?;
                if next.tag != ChangeTag::Insert || next.ln != row.ln {
                    return None;
                }
                let pos = render_pos(&out, row.ln)?;
                if trimmed(&out.get(pos)?.text) != trimmed(&row.text) {
                    return None;
                }
                match out.get(pos)?.tag {
                    // A context line the later call edited: show its -/+ pair.
                    ChangeTag::Equal => {
                        if let Some(slot) = out.get_mut(pos) {
                            *slot = row.clone();
                        }
                        out.insert(pos + 1, next.clone());
                    }
                    // Same line edited twice: keep the earlier delete (if any), drop the intermediate text, keep the final insert
                    ChangeTag::Insert => {
                        if let Some(slot) = out.get_mut(pos) {
                            *slot = next.clone();
                        }
                    }
                    ChangeTag::Delete => unreachable!("render_pos skips deletes"),
                }
                i += 2;
            }
            // An unpaired insert inside the covered range grows the line count
            ChangeTag::Insert => return None,
        }
    }
    Some(out)
}

/// Extract resolved edit details from an ACP ToolCall JSON payload, then try
/// its Diff metadata, and finally diff the before/after text.
pub fn extract_edit_hunks(tool_call: &Value) -> (Vec<DiffHunk>, usize) {
    fn located(details: &Value) -> Option<Vec<DiffHunk>> {
        let edits = Vec::<DiffEditDetail>::deserialize(details).ok()?;
        Some(build_located_diff_hunks(&edits))
    }
    fn counted(hunks: Vec<DiffHunk>) -> (Vec<DiffHunk>, usize) {
        let count = hunks.len().max(1);
        (hunks, count)
    }

    if let Some(raw) = tool_call.get("rawOutput")
        && raw.get("type").and_then(Value::as_str) == Some("SearchReplace")
        && let Some(hunks) = raw.pointer("/EditsApplied/edits/details").and_then(located)
    {
        return counted(hunks);
    }

    for content in tool_call
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if content.get("type").and_then(Value::as_str) != Some("diff") {
            continue;
        }
        if let Some(details) = content.pointer("/_meta/details")
            && details.as_array().is_some_and(|rows| !rows.is_empty())
            && let Some(hunks) = located(details)
        {
            return counted(hunks);
        }
        if let Some(new_text) = content.get("newText").and_then(Value::as_str) {
            let start_line = content
                .pointer("/_meta/new_line")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(1);
            let old_text = content
                .get("oldText")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return counted(diff_hunks_from_strings(old_text, new_text, start_line));
        }
    }
    (vec![], 1)
}

/// Generate a unified diff patch string from diff hunks.
/// Produces output suitable for `git apply` or clipboard sharing:
pub fn diff_hunks_to_patch(path: &str, hunks: &[DiffHunk]) -> String {
    if hunks.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));

    for hunk in hunks {
        if hunk.is_empty() {
            continue;
        }

        // Compute hunk header: @@ -old_start,old_count +new_start,new_count @@
        let old_start = hunk
            .iter()
            .filter(|l| l.tag != ChangeTag::Insert)
            .map(|l| l.lo)
            .next()
            .unwrap_or(1);
        let new_start = hunk
            .iter()
            .filter(|l| l.tag != ChangeTag::Delete)
            .map(|l| l.ln)
            .next()
            .unwrap_or(1);
        let old_count = hunk.iter().filter(|l| l.tag != ChangeTag::Insert).count();
        let new_count = hunk.iter().filter(|l| l.tag != ChangeTag::Delete).count();

        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));

        for line in hunk {
            let prefix = match line.tag {
                ChangeTag::Equal => ' ',
                ChangeTag::Insert => '+',
                ChangeTag::Delete => '-',
            };
            let text = line.text.trim_end_matches(['\r', '\n']);
            out.push(prefix);
            out.push_str(text);
            out.push('\n');
        }
    }

    out
}
