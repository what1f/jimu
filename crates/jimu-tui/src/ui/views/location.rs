// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Unstyled location pieces for the welcome top bar and the dashboard header.
//! The session header reuses [`worktree_badge`] and [`branch_label`] only.
//! Pure over the per-cwd git probe; nothing here spawns `git`.

use std::path::Path;

use ratatui::text::Span;

use crate::engine::theme::Theme;
use crate::ui::views::git_info;

/// `worktree ` painted one step fainter than the branch so the path stays the strongest word.
pub(crate) fn worktree_badge(theme: &Theme) -> Span<'static> {
    Span::styled("worktree ", theme.faint())
}

/// The branch as the location line shows it: git reports a detached HEAD as an empty name.
pub(crate) fn branch_label(branch: String) -> String {
    if branch.is_empty() {
        "detached".to_owned()
    } else {
        branch
    }
}

/// The unstyled pieces of a location line, so each surface (welcome top bar, dashboard header) can style them on its own.
pub(crate) struct LocationParts {
    /// The checked-out branch, `detached` for a detached HEAD, `None` outside a git repo.
    pub branch: Option<String>,
    pub is_worktree: bool,
    /// The abbreviated, middle-shortened cwd.
    pub cwd_display: String,
}

/// The dashboard header passes its staged `app.cwd` so the line tracks a `/cd` immediately, before (or even if) `Effect::SetWorkingDir` moves the process cwd.
/// Safe to call during render: it reads the per-cwd git cache and never blocks or spawns `git`.
pub(crate) fn location_parts(cwd: &Path) -> LocationParts {
    location_parts_from(cwd, git_info::cwd_git_info_lazy(cwd))
}

/// `info` is taken by value so the branch moves out instead of being cloned on every frame.
fn location_parts_from(cwd: &Path, info: Option<git_info::CwdGitInfo>) -> LocationParts {
    let cwd_display = crate::engine::util::display_location_path(cwd);
    let is_worktree = info.as_ref().is_some_and(|i| i.is_worktree);
    let branch = info.and_then(|i| i.branch).map(branch_label);
    LocationParts {
        branch,
        is_worktree,
        cwd_display,
    }
}
