use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Configuration for running and debugging the file in the editor with a
/// single click, via the "Run Current File" and "Debug Current File" actions
/// and the buttons on the editor tab.
///
/// On Windows the run step decodes the program's output as UTF-8, because the
/// terminal only renders UTF-8. A program that writes something else — typically
/// an MSVC build of GBK source files without `/utf-8` — shows garbled Chinese;
/// give the compiler `/utf-8` instead. Build steps keep the console's own code
/// page, so compiler messages that were written for it still read correctly.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CodeRunnerSettingsContent {
    /// Whether running, debugging and compiling the file in the editor are
    /// available: the buttons on the editor tab, the Run menu entries and their
    /// key bindings.
    ///
    /// Turned off, the run and debug entry points are hidden and their commands
    /// do nothing, while `executor_map` and `debug_map` are kept as they are.
    ///
    /// Default: true
    pub enabled: Option<bool>,

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
    /// (the CMake project directory for `cmake`). Every value is escaped for
    /// the double quotes around it, so `"$ZED_FILENAME"` is safe for names
    /// with `$`, backticks, quotes, spaces and non-ASCII characters; keep the
    /// double quotes around the variables.
    ///
    /// Keys are language ids. `python`, `javascript` and `typescript` have a
    /// built-in command, and any other key is matched against the file
    /// extension, so `"lua": "lua \"$ZED_FILENAME\""` runs `.lua` files. An
    /// empty value removes the built-in command or plan for that language.
    ///
    /// C and C++ (`c`, `cpp`), Rust, Go and Java have built-in plans instead of
    /// one command, because they build first or pick their command from the
    /// files around the current one: C/C++ compile a file, link a directory, or
    /// build a CMake project; Rust runs the crate with `cargo` or compiles the
    /// file with `rustc`; Java compiles into `out` and runs the class named
    /// after the file. An entry for one of them replaces its plan with that
    /// command line, and `cmake` replaces the built-in CMake plan for C/C++
    /// files in a project with a `CMakeLists.txt`.
    ///
    /// A plan runs one step after another, each in its own terminal, and stops
    /// at the first failure, so a failed build never runs a stale binary.
    ///
    /// Default:
    ///
    /// ```json
    /// {
    ///   "code_runner": {
    ///     "executor_map": {
    ///       "python": "python3 \"$ZED_FILENAME\"",
    ///       "javascript": "node \"$ZED_FILENAME\"",
    ///       "typescript": "node --experimental-strip-types \"$ZED_FILENAME\""
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// On Windows `python` is used instead of `python3`.
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
