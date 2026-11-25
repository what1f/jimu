//! Builds the "Read 10 files, Ran 2 subagents" header label for a folded run of consecutive non-destructive tool calls and subagent lifecycle rows.
//! A run also claims any finished collapsed thoughts inside it.
//! The run classification ([`run_step`]) also lives here, shared by the layout fold, range resolution, and the label walk.
//!
//! The layout pass in `state/layout.rs` detects the runs and marks the header via `EntryLayoutInfo::verb_group_header`.
//! The render loop calls [`verb_group_header_label`] to build the live label each frame.
//! Running entries repaint every tick, so tense and counts update in place; no per-call detail churns beside the label while the run executes.
//!
//! The same bucket vocabulary labels the truncation ("N more") headers: the render loop calls [`truncation_header_label`] with the fold's span.
//! Both walks feed the shared `BucketAccumulator` so the two label families can't drift.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::engine::theme::Theme;
use crate::ui::scrollback::block::RenderBlock;
use crate::ui::scrollback::blocks::SubagentBlockKind;
use crate::ui::scrollback::blocks::tool::{ToolCallBlock, VerbGroupKind};
use crate::ui::scrollback::entry::ScrollbackEntry;
use crate::ui::scrollback::types::DisplayMode;

/// One step of a verb-group run walk.
pub(crate) enum RunStep {
    /// A collapsed verb-groupable tool or subagent entry: joins the run and counts toward the fold threshold ([`RunScan::folds`]).
    Member(VerbGroupKind),
    /// A finished, collapsed, shown thinking entry: claimed into the run (folds to height 0).
    /// It never counts toward the threshold and never appears in the header label.
    ThoughtMember,
    /// An entry that renders its own rows (or none) without joining or breaking the run.
    /// That covers hidden, streaming, user-opened thinking, thinking that carries prompt chrome, and a manually-opened verb-groupable tool.
    Transparent,
    /// Anything else: ends the run.
    Break,
}

/// A manually-opened member is [`RunStep::Transparent`] and keeps its own rows without splitting the run. Thinking
/// never breaks a run: a finished collapsed thought without prompt chrome folds in as
/// [`RunStep::ThoughtMember`].
pub(crate) fn run_step(entry: &ScrollbackEntry, show_thinking: bool) -> RunStep {
    let is_claimable_thinking =
        entry.display_mode == DisplayMode::Collapsed && !entry.is_pending_user_input;
    if let RenderBlock::ToolCall(block) = &entry.block
        && let Some(kind) = block.verb_group_kind()
        && !entry.is_pending_user_input
    {
        if entry.display_mode == DisplayMode::Collapsed {
            RunStep::Member(kind)
        } else {
            // A manually-opened member keeps its own rows without splitting the run, the same treatment as opened thinking
            // Toggling a member of an expanded group therefore never dissolves the group
            RunStep::Transparent
        }
    } else if matches!(entry.block, RenderBlock::Subagent(_)) {
        if entry.display_mode == DisplayMode::Collapsed && !entry.is_pending_user_input {
            RunStep::Member(VerbGroupKind::Subagent)
        } else {
            // Subagent rows are always collapsed single-row entries; prompt chrome keeps them standalone and breaks the run
            RunStep::Break
        }
    } else if entry.block.is_thinking() {
        if show_thinking && !entry.is_running && is_claimable_thinking {
            RunStep::ThoughtMember
        } else {
            RunStep::Transparent
        }
    } else {
        RunStep::Break
    }
}

/// Whether an in-place block swap changes the entry's verb-group kind (e.g. the eager `Other` placeholder refining into a `Read`).
/// Such swaps change fold membership, so the swap site must mark the entry structurally dirty for the layout fold to catch up on the next frame.
pub(crate) fn verb_group_kind_changed(old: &RenderBlock, new: &RenderBlock) -> bool {
    let kind_of = |block: &RenderBlock| match block {
        RenderBlock::ToolCall(tc) => tc.verb_group_kind(),
        _ => None,
    };
    kind_of(old) != kind_of(new)
}

/// Shape of one forward run walk, as reported by [`scan_run_forward`].
pub(crate) struct RunScan {
    /// Member entries counted (tool calls and subagent rows), including a member start entry.
    /// Thought members claim but never count; the fold threshold is members-only.
    pub(crate) members: usize,
    /// Exclusive run end: one past the last claimed entry (member or thought member), so trailing transparent entries stay outside the run.
    pub(crate) end: usize,
    /// Where the walk stopped: the breaking entry's index, or the first index where `entry_at` returned `None`.
    pub(crate) stop: usize,
}

impl RunScan {
    /// Whether the run folds into a verb-group header row. Thought members never count, so a pure-thought run (whose
    /// label would be empty) never folds. The layout fold and `verb_group_range_of` share this predicate so the two
    /// can't drift.
    pub(crate) fn folds(&self) -> bool {
        self.members >= 1
    }
}

/// Returns `None` when the entry at `start` is missing or cannot anchor a run. Members and thought members can
/// anchor; transparent and breaking entries cannot. Anchor eligibility lives in this function's matches, not in
/// caller pre-checks, so a returned scan always has `end > start` and `stop > start`.
pub(crate) fn scan_run_forward<'e>(
    entry_at: impl Fn(usize) -> Option<&'e ScrollbackEntry>,
    start: usize,
    show_thinking: bool,
) -> Option<RunScan> {
    // Members and finished thoughts anchor runs; transparent thinking may sit inside one but cannot start one
    match run_step(entry_at(start)?, show_thinking) {
        RunStep::Member(_) | RunStep::ThoughtMember => {}
        RunStep::Transparent | RunStep::Break => return None,
    }
    let mut members = 0usize;
    let mut end = start;
    let mut i = start;
    while let Some(entry) = entry_at(i) {
        match run_step(entry, show_thinking) {
            RunStep::Member(_) => {
                members += 1;
                end = i + 1;
            }
            RunStep::ThoughtMember => end = i + 1,
            RunStep::Transparent => {}
            RunStep::Break => break,
        }
        i += 1;
    }
    Some(RunScan {
        members,
        end,
        stop: i,
    })
}

/// Aggregated header state for one verb-group run.
pub struct VerbGroupHeaderLabel {
    /// Styled label line rendered on the header row.
    pub line: Line<'static>,
    /// Plain-text label used as the header row's selection and copy text.
    pub text: String,
    /// Any member still running (animated accent and present-tense verbs).
    pub running: bool,
    /// Any member failed (error accent).
    pub failed: bool,
}

/// The single channel that carries a group-header row's aggregated label, mirroring the fold families of
/// `groups::GroupKind`. A header row belongs to exactly one fold, so a row carries at most one label.
pub enum GroupHeaderLabel {
    /// Verb-group run header ("Read 3 files, Searched 2 patterns").
    VerbRun(VerbGroupHeaderLabel),
    /// Labeled truncation ("N more") header ("Ran 6 commands").
    Truncation(VerbGroupHeaderLabel),
}

impl GroupHeaderLabel {
    /// The aggregated label payload, whichever fold family produced it.
    pub fn label(&self) -> &VerbGroupHeaderLabel {
        match self {
            GroupHeaderLabel::VerbRun(label) | GroupHeaderLabel::Truncation(label) => label,
        }
    }
}

/// One aggregation bucket per verb kind, ordered by first appearance in the run.
/// Borrows citation strings from the walked blocks (per-frame, no allocation).
struct Bucket<'e> {
    kind: VerbGroupKind,
    calls: usize,
    /// Distinct-count override: when non-empty its size replaces `calls` as the displayed count.
    /// Holds WebSearch citation URLs (distinct result websites) and subagent child session ids.
    /// The started and terminal rows of one subagent count once; a burst of terminal rows counts each distinct subagent.
    sources: std::collections::HashSet<&'e str>,
    /// Distinct child ids whose Subagent row is still `is_running`. Empty for other kinds.
    running_sources: std::collections::HashSet<&'e str>,
}

/// The label counts members only: folded thoughts contribute nothing here and appear as their own member rows only
/// when the group is expanded. Callers with the fold's span pass its exact end so the label counts precisely the
/// entries the fold claimed.
pub fn verb_group_header_label(
    entries: &[&ScrollbackEntry],
    header_idx: usize,
    end: usize,
    show_thinking: bool,
    theme: &Theme,
) -> VerbGroupHeaderLabel {
    let mut acc = BucketAccumulator::default();

    let end = end.min(entries.len());
    let Some(run) = entries.get(header_idx.min(end)..end) else {
        return acc.into_label(theme);
    };
    for &entry in run {
        let kind = match run_step(entry, show_thinking) {
            RunStep::Member(kind) => kind,
            RunStep::Break => break,
            RunStep::ThoughtMember | RunStep::Transparent => continue,
        };
        acc.push(kind, entry);
    }

    acc.into_label(theme)
}

/// Thoughts occupy participant slots but are never bucketed: like verb-group labels, group labels stay tools-only.
/// Thoughts are the only participants a label may silently omit. anything else would make the label under-describe
/// what the fold conceals.
pub fn truncation_header_label(
    entries: &[&ScrollbackEntry],
    range: std::ops::Range<usize>,
    limit: Option<usize>,
    show_thinking: bool,
    theme: &Theme,
) -> Option<VerbGroupHeaderLabel> {
    let mut acc = BucketAccumulator::default();
    let end = range.end.min(entries.len());
    let mut participants = 0usize;

    let run = entries.get(range.start.min(end)..end)?;
    for &entry in run {
        if limit.is_some_and(|n| participants >= n) {
            break;
        }
        if entry.is_hidden_thinking(show_thinking) {
            continue;
        }
        participants += 1;
        if entry.block.is_thinking() {
            continue;
        }
        match &entry.block {
            RenderBlock::ToolCall(block) => acc.push(block.label_kind()?, entry),
            RenderBlock::Subagent(_) => acc.push(VerbGroupKind::Subagent, entry),
            // A participant the vocabulary can't name would leave the label dishonest about what's hidden
            // Decline so the numerically exact plain count renders instead
            _ => return None,
        }
    }

    if acc.is_empty() {
        return None;
    }
    Some(acc.into_label(theme))
}

/// Shared bucket accumulation and label rendering for the aggregated group headers.
/// Callers own the walk: which entries join and under what classification.
/// This owns per-kind counting, distinct-source overrides, tool outcome counting, and the rendered line.
#[derive(Default)]
struct BucketAccumulator<'e> {
    buckets: Vec<Bucket<'e>>,
    running: bool,
    failed_count: usize,
}

impl<'e> BucketAccumulator<'e> {
    fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    fn push(&mut self, kind: VerbGroupKind, entry: &'e ScrollbackEntry) {
        let pos = match self.buckets.iter().position(|b| b.kind == kind) {
            Some(pos) => pos,
            None => {
                self.buckets.push(Bucket {
                    kind,
                    calls: 0,
                    sources: std::collections::HashSet::new(),
                    running_sources: std::collections::HashSet::new(),
                });
                self.buckets.len() - 1
            }
        };
        let Some(bucket) = self.buckets.get_mut(pos) else {
            return;
        };
        bucket.calls += 1;
        // Both walks only bucket tool-call or subagent rows
        // The block feeds the distinct-count override and failure detection
        match &entry.block {
            RenderBlock::ToolCall(block) => {
                if let ToolCallBlock::WebSearch(b) = block
                    && b.is_success()
                {
                    bucket
                        .sources
                        .extend(b.citations.iter().map(String::as_str));
                }
                if block_failed(block) {
                    self.failed_count += 1;
                }
            }
            RenderBlock::Subagent(sb) => {
                bucket.sources.insert(sb.child_session_id.as_str());
                if entry.is_running {
                    bucket.running_sources.insert(sb.child_session_id.as_str());
                }
                // Cancelled is deliberate, not an error; only Failed feeds the red suffix
                if matches!(sb.kind, SubagentBlockKind::Failed { .. }) {
                    self.failed_count += 1;
                }
            }
            // Unreachable today; release keeps the generic count so the label can't desync from the fold that claimed the entry
            _ => debug_assert!(false, "bucketed entry has a block with no label-extras arm"),
        }

        if entry.is_running {
            self.running = true;
        }
    }

    fn into_label(self, theme: &Theme) -> VerbGroupHeaderLabel {
        let text_style = theme.fg(theme.gray_bright).add_modifier(Modifier::BOLD);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut text = String::new();
        for (i, bucket) in self.buckets.iter().enumerate() {
            let count = if bucket.sources.is_empty() {
                bucket.calls
            } else {
                bucket.sources.len()
            };
            let sep = if i == 0 { "" } else { ", " };
            // Subagent tense is per-bucket: a finished-only set must not inherit group-wide Running.
            let segment = match bucket.kind {
                VerbGroupKind::Subagent => {
                    let running_n = bucket.running_sources.len();
                    let done_n = count.saturating_sub(running_n);
                    if running_n > 0 && done_n > 0 {
                        format!(
                            "{sep}{} {running_n} {}, {done_n} completed",
                            bucket.kind.verb(true),
                            bucket.kind.noun(running_n),
                        )
                    } else {
                        format!(
                            "{sep}{} {count} {}",
                            bucket.kind.verb(running_n > 0),
                            bucket.kind.noun(count),
                        )
                    }
                }
                _ => format!(
                    "{sep}{} {count} {}",
                    bucket.kind.verb(self.running),
                    bucket.kind.noun(count),
                ),
            };
            text.push_str(&segment);
            spans.push(Span::styled(segment, text_style));
        }
        if self.failed_count > 0 {
            let suffix = format!(" · {} failed", self.failed_count);
            text.push_str(&suffix);
            spans.push(Span::styled(suffix, theme.fg(theme.accent_error)));
        }
        VerbGroupHeaderLabel {
            line: Line::from(spans),
            text,
            running: self.running,
            failed: self.failed_count > 0,
        }
    }
}

/// Whether a bucketed block completed with an error.
/// Variants are listed explicitly so a new `ToolCallBlock` variant must decide here too.
/// The action kinds reach labels only through truncation buckets (verb folds exclude them), where their failures count like any other member's.
fn block_failed(block: &ToolCallBlock) -> bool {
    match block {
        ToolCallBlock::Read(b) => !b.is_success(),
        ToolCallBlock::ListDir(b) => !b.is_success(),
        ToolCallBlock::Search(b) => !b.is_success(),
        ToolCallBlock::WebFetch(b) => !b.is_success(),
        ToolCallBlock::WebSearch(b) => !b.is_success(),
        ToolCallBlock::MemorySearch(b) => !b.is_success(),
        ToolCallBlock::IntegrationSearch(b) => !b.is_success(),
        ToolCallBlock::Skill(b) => !b.is_success(),
        ToolCallBlock::Execute(b) => !b.is_success(),
        ToolCallBlock::Edit(b) => !b.is_success(),
        ToolCallBlock::UseTool(b) => !b.is_success(),
        ToolCallBlock::SentMessage(b) => b.is_failure(),
        ToolCallBlock::Other(b) => !b.is_success(),
    }
}
