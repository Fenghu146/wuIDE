//! Resolving "run this file" and "debug this file" into what the terminal and
//! the debugger need.
//!
//! The runner has no pipeline of its own: it turns a file into a command line
//! that the terminal's shell executes, using the same `$ZED_*` variables as
//! tasks do. Everything the user sees before running is a plain string from
//! [`RunCommand::command_line`], and the template it came from is kept next to
//! it so the run dialog can explain the origin.

mod resolve;
mod settings;

pub use resolve::{
    CommandSource, DebugEntry, DebugPlan, RunCommand, RunRequest, debug_adapter_available,
    debug_adapter_available_in, find_executable, language_id_for_path, resolve, resolve_debug,
};
pub use settings::{CodeRunnerSettings, default_debug_map, default_run_map};
