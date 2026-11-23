//! Resolving "run this file" into a shell command line.
//!
//! The runner has no pipeline of its own: it turns a file into a command line
//! that the terminal's shell executes, using the same `$ZED_*` variables as
//! tasks do. Everything the user sees before running is a plain string from
//! [`RunCommand::command_line`], and the template it came from is kept next to
//! it so the run dialog can explain the origin.

mod resolve;
mod settings;

pub use resolve::{
    CommandSource, RunCommand, RunRequest, language_id_for_path, resolve,
};
pub use settings::{CodeRunnerSettings, default_executor_map};
