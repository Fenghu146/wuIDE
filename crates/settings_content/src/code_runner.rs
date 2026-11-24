use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Configuration for running and debugging the file in the editor with a
/// single click, via the "Run Current File" and "Debug Current File" actions
/// and the buttons on the editor tab.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CodeRunnerSettingsContent {
    /// One shell command line per language, used by "Run Current File".
    ///
    /// The command is handed to the terminal's shell as-is, after `$ZED_*` task
    /// variables are substituted, so it can both build and run the program.
    /// Runs reuse one terminal, so the command and its output stay visible
    /// there; "Run Current File…" opens the command for editing first.
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
    /// so `"lua": "lua \"$ZED_FILENAME\""` runs `.lua` files. An empty value
    /// removes the built-in command for that language.
    ///
    /// The commands are joined with `&&`, which needs a shell that supports it:
    /// PowerShell 7+, cmd, and POSIX shells do. Windows PowerShell 5.1 does not,
    /// so write two statements with `;` there.
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

    /// How to build and start a debug session per language, used by "Debug
    /// Current File".
    ///
    /// `build` is one shell command line that has to produce a binary with
    /// debug symbols, and `program` says where that binary lands, relative to
    /// the build's directory. Debug builds use their own binary name so they
    /// never overwrite the binary a plain run uses. `adapter` names the debug
    /// adapter and defaults to `GDB`, which needs `gdb` in `PATH`.
    ///
    /// Keys are language ids, matched the way `executor_map` matches them.
    /// C and C++ have built-in entries; other languages have none until you add
    /// them, so the debug button only shows where a plan exists.
    ///
    /// Default:
    ///
    /// ```json
    /// {
    ///   "code_runner": {
    ///     "debug_map": {
    ///       "cpp": {
    ///         "build": "g++ -g -O0 -std=c++17 \"$ZED_FILENAME\" -o \"$ZED_STEM.dbg\"",
    ///         "program": "$ZED_STEM.dbg"
    ///       }
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// On Windows `program` and the build's output are written with an `.exe`
    /// suffix.
    pub debug_map: Option<HashMap<String, DebugEntrySettingsContent>>,
}

/// How to build and launch one language in the debugger.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct DebugEntrySettingsContent {
    /// Shell command line that builds the binary with debug symbols.
    ///
    /// Default: the compiler of the language with `-g -O0`.
    pub build: Option<String>,
    /// Path of the binary the build produces, relative to the directory the
    /// command runs in. Passed to the debug adapter as the program to launch.
    ///
    /// Default: `$ZED_STEM.dbg`, with `.exe` on Windows.
    pub program: Option<String>,
    /// Name of the debug adapter to start, as registered by WuIDE or an
    /// extension.
    ///
    /// Default: `GDB`
    pub adapter: Option<String>,
}
