use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Configuration for running the file in the editor with a single click,
/// via the "Run Current File" action (`wu::RunCurrentFile`) and the run button
/// on the editor tab.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CodeRunnerSettingsContent {
    /// One shell command line per language, used by "Run Current File".
    ///
    /// The command is handed to the terminal's shell as-is, after `$ZED_*` task
    /// variables are substituted, so it can both build and run the program.
    /// Anything you type here is exactly what the run dialog shows before
    /// running; entries you do not override come from the built-in defaults
    /// below, which the run dialog labels as "built-in".
    ///
    /// Available variables are the usual task variables: `$ZED_FILE`,
    /// `$ZED_FILENAME`, `$ZED_DIRNAME`, `$ZED_STEM`, `$ZED_WORKTREE_ROOT`,
    /// `$ZED_LANGUAGE`. `$ZED_FILENAME` and `$ZED_STEM` are relative to the
    /// directory the command runs in, which is the directory of the file
    /// (the CMake project directory for `cmake`).
    ///
    /// Keys are language ids: `c`, `cpp`, `rust`, `python`, `javascript`,
    /// `typescript`, and `cmake` for C/C++ files that belong to a project with
    /// a `CMakeLists.txt`. Any other key is matched against the file extension,
    /// so `"lua": "lua \"$ZED_FILENAME\""` runs `.lua` files.
    ///
    /// On Windows the shell is PowerShell, where a quoted path is only run with
    /// the call operator: `& ".\a.exe"`. Change these defaults if your shell
    /// differs.
    ///
    /// Default:
    ///
    /// ```json
    /// {
    ///   "code_runner": {
    ///     "executor_map": {
    ///       "c": "gcc \"$ZED_FILENAME\" -o \"$ZED_STEM\" && \"./$ZED_STEM\"",
    ///       "cpp": "g++ -std=c++17 \"$ZED_FILENAME\" -o \"$ZED_STEM\" && \"./$ZED_STEM\"",
    ///       "rust": "cargo run",
    ///       "python": "python3 \"$ZED_FILENAME\"",
    ///       "javascript": "node \"$ZED_FILENAME\"",
    ///       "typescript": "node --experimental-strip-types \"$ZED_FILENAME\"",
    ///       "cmake": "cmake -S . -B build && cmake --build build"
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// On Windows the executables are written with an `.exe` suffix and run with
    /// `&`, `python` is used instead of `python3`, and the CMake command is the
    /// same.
    pub executor_map: Option<HashMap<String, String>>,
}
