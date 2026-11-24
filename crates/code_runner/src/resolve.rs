use std::path::{Path, PathBuf};

use task::{TaskContext, TaskVariables, VariableName, substitute_variables_in_str};

use crate::settings::CodeRunnerSettings;

/// Where the command that is about to run came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSource {
    /// WuIDE's default for the language.
    BuiltIn,
    /// `code_runner.executor_map` or `code_runner.debug_map` in the settings.
    Settings,
    /// Edited in the run dialog for this run only.
    Edited,
}

/// How a language is built for and started in the debugger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugEntry {
    /// Shell command line that builds the binary with debug symbols.
    pub build: String,
    /// Path of that binary, relative to the build's directory.
    pub program: String,
    /// Debug adapter to start.
    pub adapter: String,
    pub source: CommandSource,
}

/// A debug build and the session that follows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugPlan {
    pub language: String,
    /// File name only, used for labels.
    pub filename: String,
    /// The build that has to succeed before the session starts.
    pub build: RunCommand,
    /// Absolute path of the binary to debug.
    pub program: PathBuf,
    pub adapter: String,
}

impl DebugPlan {
    pub fn label(&self) -> String {
        format!("debug {}", self.filename)
    }
}

/// A command line that is ready to be handed to the terminal's shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCommand {
    pub language: String,
    /// File name only, used for the terminal tab label.
    pub filename: String,
    /// The command as configured, before `$ZED_*` substitution.
    pub template: String,
    /// The command that runs, with `$ZED_*` substituted.
    pub command_line: String,
    /// Directory the command runs in.
    pub cwd: PathBuf,
    pub source: CommandSource,
}

impl RunCommand {
    pub fn label(&self) -> String {
        format!("run {}", self.filename)
    }

    /// Replaces the command line with one the user edited in the run dialog.
    pub fn with_edited_command_line(mut self, command_line: String) -> Self {
        self.command_line = command_line;
        self.source = CommandSource::Edited;
        self
    }
}

/// Everything the resolution depends on, gathered from the editor.
pub struct RunRequest<'a> {
    /// Absolute path of the file to run.
    pub file: &'a Path,
    pub worktree_root: &'a Path,
    /// The closest directory above the file that holds a `CMakeLists.txt`,
    /// if the file belongs to a CMake project.
    pub cmake_root: Option<&'a Path>,
}

/// Maps a file to the language id used as a key in the `code_runner` maps.
///
/// Known languages get their language id; anything else falls back to the
/// lowercased extension, which is what lets a user add a command for a file
/// type WuIDE does not know about.
pub fn language_id_for_path(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let language = match extension.as_str() {
        "c" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "cp" => "cpp",
        "rs" => "rust",
        "py" | "pyw" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" | "tsx" => "typescript",
        _ => extension.as_str(),
    };
    Some(language.to_owned())
}

/// Picks the command for a file and substitutes its variables.
///
/// C and C++ files that belong to a CMake project build the project instead of
/// compiling the single file, because the file alone rarely links there.
pub fn resolve(request: &RunRequest<'_>, settings: &CodeRunnerSettings) -> Option<RunCommand> {
    let language = language_id_for_path(request.file)?;

    let mut key = language.as_str();
    let mut cwd = request
        .file
        .parent()
        .unwrap_or(request.worktree_root)
        .to_path_buf();
    if matches!(language.as_str(), "c" | "cpp")
        && let Some(cmake_root) = request.cmake_root
        && settings.command("cmake").is_some()
    {
        key = "cmake";
        cwd = cmake_root.to_path_buf();
    }

    let (template, source) = settings.command(key)?;
    let context = task_context(request, &cwd, &language);
    let command_line = substitute_variables_in_str(template, &context)?;

    Some(RunCommand {
        language,
        filename: file_name(request.file),
        template: template.to_owned(),
        command_line,
        cwd,
        source,
    })
}

/// Picks the debug build for a file and the binary the session should launch.
///
/// Debug plans compile the single file even inside a CMake project: the binary a
/// project build produces is not known here, and guessing it is worse than
/// letting the user point `code_runner.debug_map` at their target.
pub fn resolve_debug(request: &RunRequest<'_>, settings: &CodeRunnerSettings) -> Option<DebugPlan> {
    let language = language_id_for_path(request.file)?;
    let entry = settings.debug_entry(&language)?;
    if entry.build.trim().is_empty() || entry.program.trim().is_empty() {
        return None;
    }

    let cwd = request
        .file
        .parent()
        .unwrap_or(request.worktree_root)
        .to_path_buf();
    let context = task_context(request, &cwd, &language);
    let build_line = substitute_variables_in_str(&entry.build, &context)?;
    let program_line = substitute_variables_in_str(&entry.program, &context)?;
    let program = if Path::new(&program_line).is_absolute() {
        PathBuf::from(program_line)
    } else {
        cwd.join(program_line)
    };

    Some(DebugPlan {
        filename: file_name(request.file),
        language: language.clone(),
        build: RunCommand {
            language,
            filename: file_name(request.file),
            template: entry.build.clone(),
            command_line: build_line,
            cwd,
            source: entry.source,
        },
        program,
        adapter: entry.adapter.clone(),
    })
}

/// Whether the debug adapter can start here.
///
/// Only `GDB` is checked, because it is the adapter the built-in debug plans
/// use and it has to be on `PATH`; adapters that download themselves or are
/// provided by extensions are assumed to be available.
pub fn debug_adapter_available(adapter: &str) -> bool {
    debug_adapter_available_in(adapter, &search_path())
}

pub fn debug_adapter_available_in(adapter: &str, paths: &[PathBuf]) -> bool {
    match adapter {
        "GDB" => find_executable("gdb", paths).is_some(),
        _ => true,
    }
}

pub fn find_executable(name: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .flat_map(|directory| {
            [
                directory.join(name),
                directory.join(format!("{name}.exe")),
            ]
        })
        .find(|candidate| candidate.is_file())
}

fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

fn file_name(file: &Path) -> String {
    file.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn task_context(request: &RunRequest<'_>, cwd: &Path, language: &str) -> TaskContext {
    let file_name = request.file.file_name().map(|name| name.to_string_lossy());
    let stem = request.file.file_stem().map(|stem| stem.to_string_lossy());

    let mut task_variables = TaskVariables::default();
    task_variables.insert(
        VariableName::File,
        request.file.to_string_lossy().into_owned(),
    );
    if let Some(file_name) = file_name {
        task_variables.insert(VariableName::Filename, file_name.into_owned());
    }
    if let Some(stem) = stem {
        task_variables.insert(VariableName::Stem, stem.into_owned());
    }
    if let Some(dirname) = request.file.parent() {
        task_variables.insert(
            VariableName::Dirname,
            dirname.to_string_lossy().into_owned(),
        );
    }
    task_variables.insert(
        VariableName::WorktreeRoot,
        request.worktree_root.to_string_lossy().into_owned(),
    );
    task_variables.insert(VariableName::Language, language.to_owned());

    TaskContext {
        cwd: Some(cwd.to_path_buf()),
        task_variables,
        project_env: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;
    use pretty_assertions::assert_eq;

    fn settings(entries: &[(&str, &str)], is_windows: bool) -> CodeRunnerSettings {
        let entries = entries
            .iter()
            .map(|(language, command)| (language.to_string(), command.to_string()))
            .collect::<HashMap<_, _>>();
        CodeRunnerSettings::new(&entries, &HashMap::default(), is_windows)
    }

    #[test]
    fn single_file_cpp_compiles_next_to_the_source_and_runs_it() {
        let file = Path::new(r"C:\work\my file.cpp");
        let root = Path::new(r"C:\work");
        let command = resolve(
            &RunRequest {
                file,
                worktree_root: root,
                cmake_root: None,
            },
            &settings(&[], true),
        )
        .expect("cpp has a default command");

        assert_eq!(command.language, "cpp");
        assert_eq!(command.source, CommandSource::BuiltIn);
        assert_eq!(
            command.command_line,
            r#"g++ -std=c++17 "my file.cpp" -o "my file.exe" && & ".\my file.exe""#
        );
        assert_eq!(command.cwd, Path::new(r"C:\work"));
        assert_eq!(command.label(), "run my file.cpp");
    }

    #[test]
    fn single_file_c_is_compiled_with_gcc_and_runs_without_an_extension_on_unix() {
        let command = resolve(
            &RunRequest {
                file: Path::new("/work/main.c"),
                worktree_root: Path::new("/work"),
                cmake_root: None,
            },
            &settings(&[], false),
        )
        .expect("c has a default command");

        assert_eq!(
            command.command_line,
            r#"gcc "main.c" -o "main" && "./main""#
        );
    }

    #[test]
    fn files_in_a_cmake_project_build_the_project_in_its_root() {
        let command = resolve(
            &RunRequest {
                file: Path::new("/work/project/src/main.cpp"),
                worktree_root: Path::new("/work"),
                cmake_root: Some(Path::new("/work/project")),
            },
            &settings(&[], false),
        )
        .expect("cmake has a default command");

        assert_eq!(
            command.command_line,
            "cmake -S . -B build && cmake --build build"
        );
        assert_eq!(command.cwd, Path::new("/work/project"));
    }

    #[test]
    fn a_cmake_entry_from_settings_wins_over_the_built_in_one() {
        let command = resolve(
            &RunRequest {
                file: Path::new("/work/project/src/main.cpp"),
                worktree_root: Path::new("/work"),
                cmake_root: Some(Path::new("/work/project")),
            },
            &settings(&[("cmake", "ninja -C build")], false),
        )
        .expect("cmake has a command");

        assert_eq!(command.command_line, "ninja -C build");
        assert_eq!(command.source, CommandSource::Settings);
    }

    #[test]
    fn unknown_extensions_use_the_extension_as_the_language_key() {
        let command = resolve(
            &RunRequest {
                file: Path::new("/work/script.lua"),
                worktree_root: Path::new("/work"),
                cmake_root: None,
            },
            &settings(&[("lua", "lua \"$ZED_FILENAME\"")], false),
        )
        .expect("the user added a lua command");

        assert_eq!(command.language, "lua");
        assert_eq!(command.command_line, "lua \"script.lua\"");
    }

    #[test]
    fn files_without_a_command_do_not_resolve() {
        let result = resolve(
            &RunRequest {
                file: Path::new("/work/notes.md"),
                worktree_root: Path::new("/work"),
                cmake_root: None,
            },
            &settings(&[], false),
        );

        assert_eq!(result, None);
    }

    #[test]
    fn rust_python_and_node_are_recognized() {
        let settings = settings(&[], false);
        for (file, expected) in [
            ("/work/src/main.rs", "cargo run"),
            ("/work/tool.py", r#"python3 "tool.py""#),
            ("/work/index.mjs", r#"node "index.mjs""#),
            (
                "/work/app.tsx",
                r#"node --experimental-strip-types "app.tsx""#,
            ),
        ] {
            let command = resolve(
                &RunRequest {
                    file: Path::new(file),
                    worktree_root: Path::new("/work"),
                    cmake_root: None,
                },
                &settings,
            )
            .unwrap_or_else(|| panic!("{file} should resolve"));
            assert_eq!(command.command_line, expected, "{file}");
        }
    }

    #[test]
    fn an_unknown_task_variable_fails_to_resolve_instead_of_running_a_broken_line() {
        let result = resolve(
            &RunRequest {
                file: Path::new("/work/main.py"),
                worktree_root: Path::new("/work"),
                cmake_root: None,
            },
            &settings(&[("python", "python3 \"$ZED_SYMBOL\"")], false),
        );

        assert_eq!(result, None);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(
            language_id_for_path(Path::new("/work/MAIN.CPP")),
            Some("cpp".to_owned())
        );
        assert_eq!(
            language_id_for_path(Path::new("/work/Main.Py")),
            Some("python".to_owned())
        );
        assert_eq!(language_id_for_path(Path::new("/work/Makefile")), None);
    }

    #[test]
    fn a_debug_plan_builds_with_symbols_and_launches_the_dbg_binary() {
        let plan = resolve_debug(
            &RunRequest {
                file: Path::new(r"C:\work\my file.cpp"),
                worktree_root: Path::new(r"C:\work"),
                cmake_root: None,
            },
            &settings(&[], true),
        )
        .expect("cpp has a default debug plan");

        assert_eq!(
            plan.build.command_line,
            r#"g++ -std=c++17 -g -O0 "my file.cpp" -o "my file.dbg.exe""#
        );
        assert_eq!(plan.build.cwd, Path::new(r"C:\work"));
        assert_eq!(plan.program, Path::new(r"C:\work\my file.dbg.exe"));
        assert_eq!(plan.adapter, "GDB");
        assert_eq!(plan.label(), "debug my file.cpp");
    }

    #[test]
    fn the_debug_binary_never_collides_with_the_run_binary() {
        let request = RunRequest {
            file: Path::new("/work/main.c"),
            worktree_root: Path::new("/work"),
            cmake_root: None,
        };
        let settings = settings(&[], false);
        let run = resolve(&request, &settings).expect("run command");
        let debug = resolve_debug(&request, &settings).expect("debug plan");

        assert_eq!(debug.program, Path::new("/work/main.dbg"));
        assert_ne!(
            run.cwd.join("main"),
            debug.program,
            "run and debug must write different files"
        );
    }

    #[test]
    fn a_debug_plan_uses_the_configured_build_with_the_file_directory_fallback() {
        let mut debug_map = HashMap::default();
        debug_map.insert(
            "cpp".to_owned(),
            DebugEntry {
                build: "clang++ -g \"$ZED_FILENAME\" -o out".to_owned(),
                program: "$ZED_DIRNAME/out".to_owned(),
                adapter: "GDB".to_owned(),
                source: CommandSource::Settings,
            },
        );
        let settings = CodeRunnerSettings::new(&HashMap::default(), &debug_map, false);

        let plan = resolve_debug(
            &RunRequest {
                file: Path::new("/work/main.cpp"),
                worktree_root: Path::new("/work"),
                cmake_root: Some(Path::new("/work")),
            },
            &settings,
        )
        .expect("cpp has a debug plan");

        // An absolute `program` is kept as it is, and the file's own directory
        // is where the build runs even inside a CMake project.
        assert_eq!(plan.program, Path::new("/work/out"));
        assert_eq!(plan.build.cwd, Path::new("/work"));
        assert_eq!(plan.build.source, CommandSource::Settings);
    }

    #[test]
    fn languages_without_a_debug_plan_do_not_resolve_to_debug() {
        let plan = resolve_debug(
            &RunRequest {
                file: Path::new("/work/main.py"),
                worktree_root: Path::new("/work"),
                cmake_root: None,
            },
            &settings(&[], false),
        );

        assert_eq!(plan, None);
    }

    #[test]
    fn gdb_is_looked_up_on_the_given_search_path() {
        let directory = std::env::temp_dir().join(format!("code-runner-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp directory");
        let gdb = directory.join("gdb.exe");
        std::fs::write(&gdb, b"not really gdb").expect("fake gdb");

        assert_eq!(
            find_executable("gdb", std::slice::from_ref(&directory)),
            Some(gdb)
        );
        assert_eq!(find_executable("gdb", &[std::env::temp_dir()]), None);
        assert_eq!(find_executable("gdb", &[]), None);

        std::fs::remove_dir_all(&directory).expect("clean up");
    }

    #[test]
    fn only_the_gdb_adapter_needs_a_binary_on_the_path() {
        assert_eq!(debug_adapter_available_in("GDB", &[]), false);
        assert_eq!(debug_adapter_available_in("CodeLLDB", &[]), true);
        assert_eq!(debug_adapter_available_in("gdb", &[]), true);
    }
}
