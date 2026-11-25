//! Derived group model for the scrollback's view-time folds.
//!
//! One scan owns every grouping decision: verb-group runs claim their entries first, then group truncation ("N more") runs over the rest.
//! The scan produces [`GroupSpan`]s, the authoritative description of every fold.
//! [`project_to_layout`] is the single writer that turns spans into the per-entry `EntryLayoutInfo` flags the renderer and navigation consume.
//! Keeping the decision (scan) and the flag writes (projection) in one module means consumers never observe a fold shape the model doesn't describe.
//!
//! The spans are stored on the layout cache (see `LayoutCache::groups`) and rebuilt whenever the folds are re-applied.
//! Like the per-entry flags, they go stale between an incremental entry append and the next structural rebuild (`gaps_may_be_dirty` covers both).
//!
//! Per-entry run classification ([`run_step`]) and the rendered header label stay in [`super::verb_group`]; this module owns run *shapes*.

use std::collections::HashSet;
use std::ops::Range;

use indexmap::IndexMap;

use super::types::EntryLayoutInfo;
use super::verb_group::{RunStep, run_step, scan_run_forward};
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::entry::{EntryId, ScrollbackEntry};
use crate::ui::scrollback::types::DisplayMode;

/// One folded region of the transcript, in entry indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSpan {
    /// Entries the fold walked.
    /// For verb runs this ends one past the last claimed entry (trailing transparent entries stay outside).
    /// For truncation it is the whole dense run, visible tail included, and may end with trailing hidden-thinking entries the walk skipped over.
    pub range: Range<usize>,
    /// Which fold produced this span and its count data.
    pub kind: GroupKind,
    /// Whether the user manually expanded this group (keyed by the first entry's ID in `ScrollbackState::expanded_groups`).
    pub expanded: bool,
}

/// The two fold families. Both render a synthetic header row; they differ in when they fold and what the header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// Eagerly folded run of verb-groupable members (tool calls and subagent rows): the aggregated "Read 2 skills" header.
    /// `members` counts label-bearing members only; claimed thoughts fold in but never count.
    VerbRun { members: usize },
    /// Budget truncation of an over-long dense run: the "N more" header. `participants` counts the entries eligible to
    /// hide (hidden thinking excluded). `hidden` is how many of them the collapsed state conceals (`participants -
    /// max_visible`, positive by the fold gate).
    Truncation { participants: usize, hidden: usize },
}

/// Binary-search sorted, disjoint spans for the one containing `idx`.
pub fn span_containing(spans: &[GroupSpan], idx: usize) -> Option<&GroupSpan> {
    let pos = spans.partition_point(|s| s.range.end <= idx);
    spans.get(pos).filter(|s| s.range.contains(&idx))
}

/// Reset every entry's group flags, scan for spans, and project them onto the layout slice.
/// Returns the spans for the caller to store on the layout cache.
/// Reads the `group_tool_verbs` / `show_thinking_blocks` settings once so both fold families and the projection agree within a rebuild.
pub(super) fn apply(
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    layout_cache: &mut [EntryLayoutInfo],
    max_visible: usize,
    expanded_groups: &HashSet<EntryId>,
    appearance: &crate::ui::scrollback::appearance::AppearanceConfig,
) -> Vec<GroupSpan> {
    for info in layout_cache.iter_mut() {
        if info.is_expanded_verb_header() {
            info.height = info.height.saturating_sub(1);
        }
        info.group_header_count = 0;
        info.group_collapse_header = false;
        info.verb_group_header = false;
    }
    let group_tool_verbs = appearance.group_tool_verbs;
    let show_thinking = appearance.show_thinking_blocks;
    let spans = scan(
        entries,
        max_visible,
        expanded_groups,
        group_tool_verbs,
        show_thinking,
    );
    project_to_layout(&spans, entries, layout_cache, show_thinking);
    spans
}

/// Scan the transcript for every fold, in the order the folds take precedence: verb runs claim entries first, truncation runs over the rest.
/// Claimed entries break truncation runs.
/// Returns spans sorted by start index; spans never overlap.
pub(super) fn scan(
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    max_visible: usize,
    expanded_groups: &HashSet<EntryId>,
    group_tool_verbs: bool,
    show_thinking: bool,
) -> Vec<GroupSpan> {
    let (mut spans, claimed) =
        scan_verb_runs(entries, expanded_groups, group_tool_verbs, show_thinking);
    spans.extend(scan_truncations(
        entries,
        max_visible,
        expanded_groups,
        show_thinking,
        &claimed,
    ));
    // Both scans emit in ascending order over disjoint ranges; interleave.
    spans.sort_unstable_by_key(|s| s.range.start);
    spans
}

/// Find maximal runs of verb-groupable member entries (plus any finished collapsed thoughts among them) that fold per `RunScan::folds`.
/// The `group_tool_verbs` setting gates the whole scan.
/// Also returns the claimed-entry mask (members and thought members of folding runs): the truncation scan treats claimed entries as run breakers.
fn scan_verb_runs(
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    expanded_groups: &HashSet<EntryId>,
    group_tool_verbs: bool,
    show_thinking: bool,
) -> (Vec<GroupSpan>, Vec<bool>) {
    let n = entries.len();
    let mut spans = Vec::new();
    let mut claimed = vec![false; n];
    if n == 0 || !group_tool_verbs {
        return (spans, claimed);
    }

    let entry_at = |i: usize| entries.get_index(i).map(|(_, e)| e);
    let mut i = 0;
    while i < n {
        // Trailing transparent thinking stays outside the run (`scan.end`).
        let Some(scan) = scan_run_forward(entry_at, i, show_thinking) else {
            i += 1;
            continue;
        };
        if !scan.folds() {
            i = scan.stop;
            continue;
        }

        // Which entries inside the run get claimed must agree with the member arms in `scan_run_forward`
        // Transparent entries stay unclaimed inside the span and keep rendering their own rows
        let Some(claimed_run) = claimed.get_mut(i..scan.end) else {
            i = scan.end;
            continue;
        };
        for (offset, slot) in claimed_run.iter_mut().enumerate() {
            if matches!(
                run_step(
                    entry_at(i + offset).expect("index within entries"),
                    show_thinking
                ),
                RunStep::Member(_) | RunStep::ThoughtMember
            ) {
                *slot = true;
            }
        }

        let first_id = *entries.get_index(i).expect("index within entries").0;
        spans.push(GroupSpan {
            range: i..scan.end,
            kind: GroupKind::VerbRun {
                members: scan.members,
            },
            expanded: expanded_groups.contains(&first_id),
        });
        i = scan.end;
    }
    (spans, claimed)
}

/// Whether a groupable entry may join a dense (non-verb) run. A turn-terminal marker closes the turn and never
/// joins, even after a stop-hook batch collapses it. Otherwise expand/collapse and "N more" would walk across the
/// turn and key off the previous header. `collapsed_only` is Mode B (collapsed entries only).
pub(super) fn can_join_dense_run(entry: &ScrollbackEntry, collapsed_only: bool) -> bool {
    entry.block.is_groupable()
        && !entry.block.is_turn_terminal_marker()
        && (!collapsed_only || entry.display_mode == DisplayMode::Collapsed)
}

/// Collapsed groupable entries may join a truncation run.
/// Hidden thinking is excluded so tools elect their own "N more" header.
fn participates_in_truncation(entry: &ScrollbackEntry, show_thinking: bool) -> bool {
    can_join_dense_run(entry, /*collapsed_only=*/ true) && !entry.is_hidden_thinking(show_thinking)
}

/// Find consecutive runs of collapsed, groupable entries longer than `max_visible + 1`. An interspersed thought
/// thus can't split a run and suppress truncation.
fn scan_truncations(
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    max_visible: usize,
    expanded_groups: &HashSet<EntryId>,
    show_thinking: bool,
    claimed: &[bool],
) -> Vec<GroupSpan> {
    let mut spans = Vec::new();
    if max_visible == 0 || entries.is_empty() {
        return spans;
    }

    let n = entries.len();
    let mut i = 0;
    while i < n {
        let (_, entry) = entries.get_index(i).unwrap();
        let Some(&is_claimed) = claimed.get(i) else {
            i += 1;
            continue;
        };
        if is_claimed || !participates_in_truncation(entry, show_thinking) {
            i += 1;
            continue;
        }

        let group_start = i;
        let mut group_len = 1;
        let mut j = i + 1;
        while j < n {
            let (_, e) = entries.get_index(j).unwrap();
            if claimed.get(j) != Some(&false) {
                break;
            }
            if participates_in_truncation(e, show_thinking) {
                group_len += 1;
            } else if !e.is_hidden_thinking(show_thinking) {
                break;
            }
            j += 1;
        }
        let group_end = j;

        if group_len <= max_visible + 1 {
            i = group_end;
            continue;
        }

        let first_id = *entries.get_index(group_start).unwrap().0;
        spans.push(GroupSpan {
            range: group_start..group_end,
            kind: GroupKind::Truncation {
                participants: group_len,
                hidden: group_len - max_visible,
            },
            expanded: expanded_groups.contains(&first_id),
        });
        i = group_end;
    }
    spans
}

/// The single writer of group heights, gaps, and header flags.
/// Every layout consequence of a fold happens here, driven only by the spans.
/// Verb runs also read the per-entry `run_step` classification; their transparent entries keep their own rows.
pub(super) fn project_to_layout(
    spans: &[GroupSpan],
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    layout_cache: &mut [EntryLayoutInfo],
    show_thinking: bool,
) {
    for span in spans {
        match span.kind {
            GroupKind::VerbRun { members } => {
                project_verb_run(span, members, entries, layout_cache, show_thinking);
            }
            GroupKind::Truncation {
                participants,
                hidden,
            } => {
                project_truncation(
                    span,
                    participants,
                    hidden,
                    entries,
                    layout_cache,
                    show_thinking,
                );
            }
        }
    }
}

/// Collapsed: the header is `height=1`; other claimed entries are `height=0`. Gaps zero only within the run; the
/// last claimed entry keeps the boundary gap to the following entry.
fn project_verb_run(
    span: &GroupSpan,
    members: usize,
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    layout_cache: &mut [EntryLayoutInfo],
    show_thinking: bool,
) {
    let last_claimed = span.range.end - 1;
    for idx in span.range.clone() {
        let (_, e) = entries.get_index(idx).unwrap();
        let Some(cached) = layout_cache.get_mut(idx) else {
            continue;
        };
        match run_step(e, show_thinking) {
            RunStep::Member(_) | RunStep::ThoughtMember => {}
            RunStep::Transparent => {
                if !span.expanded {
                    cached.gap_after = 0;
                }
                continue;
            }
            // Unreachable: a span's range never spans a Break (arm kept for match exhaustiveness)
            RunStep::Break => continue,
        }
        if idx == span.range.start {
            cached.verb_group_header = true;
            cached.group_collapse_header = span.expanded;
            cached.group_header_count = members.min(u16::MAX as usize) as u16;
            if span.expanded {
                cached.height = cached.with_verb_header_row(cached.height);
            } else {
                cached.height = 1;
            }
            // A singleton run's header is also its last claimed entry: it keeps the pairwise boundary gap, else the header glues to what follows
            if idx != last_claimed {
                cached.gap_after = 0;
            }
        } else if !span.expanded {
            cached.height = 0;
            if idx != last_claimed {
                cached.gap_after = 0;
            }
        }
    }
}

/// Collapsed: the first participating entry becomes the "N more" header (count excludes the header itself). A new
/// transparency category here must update that walk too.
fn project_truncation(
    span: &GroupSpan,
    participants: usize,
    hidden: usize,
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    layout_cache: &mut [EntryLayoutInfo],
    show_thinking: bool,
) {
    if span.expanded {
        let Some(cached) = layout_cache.get_mut(span.range.start) else {
            return;
        };
        cached.group_collapse_header = true;
        cached.group_header_count = (participants - 1).min(u16::MAX as usize) as u16;
        cached.height = 1;
        cached.gap_after = 0;
        return;
    }

    let mut seen = 0;
    for idx in span.range.clone() {
        let (_, e) = entries.get_index(idx).unwrap();
        if e.is_hidden_thinking(show_thinking) {
            continue;
        }
        let Some(cached) = layout_cache.get_mut(idx) else {
            continue;
        };
        if seen == 0 {
            cached.height = 1;
            cached.gap_after = 0;
            cached.group_header_count = (hidden - 1).min(u16::MAX as usize) as u16;
        } else if seen < hidden {
            cached.height = 0;
            cached.gap_after = 0;
            cached.group_header_count = 0;
        }
        seen += 1;
    }
}
