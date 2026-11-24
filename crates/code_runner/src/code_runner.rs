//! Resolving "run this file" and "debug this file" into the steps the terminal
//! and the debugger need.
//!
//! The runner has no pipeline of its own: it turns a file into one or more
//! command lines that the terminal's shell executes, using the same `$ZED_*`
//! variables as tasks do. A build and the program it produces are separate
//! steps, so each has its own exit code and output, and no step depends on a
//! shell `&&` that PowerShell 5.1 does not even have.

mod resolve;
mod settings;

pub use resolve::{
    CmakeBinary, CommandSource, DebugEntry, DebugPlan, DirectorySources, Program, RunError,
    RunPlan, RunRequest, RunStep, StepKind, Toolchain, cmake_run_step, cmake_target,
    debug_adapter_available, debug_adapter_available_in, defines_main, find_cmake_binary,
    find_executable, language_id_for_path, resolve, resolve_debug, scan_directory,
    source_extensions, tool_available,
};
pub use settings::{CPP_FAMILY, CodeRunnerSettings, RunCase, default_debug_map, default_run_map};
