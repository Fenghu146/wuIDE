//! Repository knowledge shared by humans and agents: the project conventions
//! (AGENTS.md) and a zero-cost structure overview of the open worktrees.

pub mod conventions;
pub mod repo_map_panel;
mod repo_map_settings;
pub mod snapshot;
pub mod symbol_map;
mod symbol_cache;

pub use repo_map_panel::RepoMapPanel;
pub use repo_map_settings::{RepoMapMode, RepoMapSettings};

use gpui::App;
use settings::Settings as _;

pub fn init(cx: &mut App) {
    repo_map_panel::init(cx);
}

/// Whether the repository map may build anything at all.
pub fn builds_map(cx: &App) -> bool {
    RepoMapSettings::get_global(cx).builds_map()
}

/// Whether a new chat may start with a project digest.
pub fn injects_digest(cx: &App) -> bool {
    RepoMapSettings::get_global(cx).injects_digest()
}
