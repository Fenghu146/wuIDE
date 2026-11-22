//! Repository knowledge shared by humans and agents: the project conventions
//! (AGENTS.md) and a zero-cost structure overview of the open worktrees.

pub mod conventions;
pub mod repo_map_panel;
pub mod snapshot;
pub mod symbol_map;
mod symbol_cache;

pub use repo_map_panel::RepoMapPanel;

use gpui::App;

pub fn init(cx: &mut App) {
    repo_map_panel::init(cx);
}
