use std::path::{Path, PathBuf};

use task::{TaskContext, TaskVariables, VariableName, substitute_variables_in_str};

use crate::settings::{CodeRunnerSettings, RunCase};

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

/// Why a file could not be turned into something that runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The extension is not a language the runner knows or has an entry for.
    NoCommand { language: String },
    /// A C or C++ directory where nothing defines `main`.
    NoMain { directory: PathBuf },
    /// A tool the plan needs is not installed.
    MissingTool { tool: String },
    /// A `$ZED_*` variable in a configured command has no value.
    UnresolvedVariable,
    /// The editor has no local file to run, such as an untitled buffer.
    NoFile,
    /// The project has no directory on disk to look in.
    NoProjectDirectory,
    /// `code_runner.executor_map` has an entry, but the run dialog was closed
    /// without a command.
    EmptyCommand,
}

/// What a step of a run plan is for. The label of the step says which one it is,
/// and the kind lets the caller tell a build apart from the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// `cmake -S . -B build`, only when the build directory does not exist yet.
    Configure,
    /// Compiles the sources, or `cmake --build build`.
    Build,
    /// Starts the program.
    Run,
}

/// One command the runner spawns on its own, so its exit code and output are
/// that step's alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStep {
    pub kind: StepKind,
    /// Terminal tab label. Stable for a file so the terminal is reused.
    pub label: String,
    /// The command line, with every `$ZED_*` variable substituted.
    pub command_line: String,
    pub cwd: PathBuf,
}

/// The program a CMake build produces, which is only known once the build has
/// run: multi-config generators put it in a subdirectory such as `build/Debug`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmakeBinary {
    pub build_dir: PathBuf,
    /// The `add_executable` target, when the project has exactly one.
    pub target: Option<String>,
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

/// Builds a file for the debugger and the binary the session should launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugPlan {
    pub language: String,
    /// File name only, used for labels.
    pub filename: String,
    /// The build that has to succeed before the session starts.
    pub build: RunPlan,
    /// How to find the binary once the build has run.
    pub program: Program,
    pub adapter: String,
}

impl DebugPlan {
    pub fn label(&self) -> String {
        format!("debug {}", self.filename)
    }
}

/// Where the debugger's program is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Program {
    /// A path that is known before building.
    Path(PathBuf),
    /// A CMake target that appears in the build directory.
    Cmake(CmakeBinary),
}

/// Everything it takes to run or debug one file, as separate steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPlan {
    pub language: String,
    /// File name only, used for the terminal tab labels.
    pub filename: String,
    /// The directory the plan belongs to: the file's own directory, or the
    /// root of the CMake project.
    pub root: PathBuf,
    pub steps: Vec<RunStep>,
    /// Set when the plan ends with a CMake target whose path is only known
    /// after the build; the caller resolves it and appends the run step.
    pub pending_binary: Option<CmakeBinary>,
    pub source: CommandSource,
    /// Why the plan looks the way it does, such as compiling the whole
    /// directory because the current file has no `main`. Shown in the run
    /// dialog so the choice is not a mystery.
    pub note: Option<String>,
}

impl RunPlan {
    pub fn label(&self) -> String {
        format!("run {}", self.filename)
    }

    /// The step that starts the program, once it is in the plan.
    pub fn run_step(&self) -> Option<&RunStep> {
        self.steps.iter().find(|step| step.kind == StepKind::Run)
    }

    /// Replaces the plan with a single command the user edited. An edited line
    /// runs on its own: the build steps it replaced are gone with it.
    pub fn with_edited_command_line(mut self, command_line: String) -> Self {
        let cwd = self
            .steps
            .last()
            .map(|step| step.cwd.clone())
            .unwrap_or_default();
        self.pending_binary = None;
        self.steps = vec![RunStep {
            kind: StepKind::Run,
            label: self.label(),
            command_line,
            cwd,
        }];
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
    /// The `.c`/`.cpp` files in the file's own directory, so a file without a
    /// `main` can be linked with the one that has it.
    pub directory: DirectorySources,
    /// Whether `cmake` can be found, because a CMake project cannot be built
    /// without it and saying so beats a shell error.
    pub cmake_available: bool,
}

impl RunRequest<'_> {
    fn current_file_defines_main(&self) -> bool {
        self.directory.defines_main(self.file)
    }
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

/// The extensions that count as sources of a language, for finding the sibling
/// files a C or C++ program is made of.
pub fn source_extensions(language: &str) -> &'static [&'static str] {
    match language {
        "c" => &["c"],
        "cpp" => &["cc", "cpp", "cxx", "c++", "cp"],
        _ => &[],
    }
}

/// Whether `text` defines the entry point. A cheap textual check is enough
/// here: the compiler is the authority, and a false negative only costs the
/// single-file command that a `main`-less file could not use anyway.
pub fn defines_main(text: &str) -> bool {
    let mut search_from = 0;
    while let Some(offset) = text[search_from..].find("main") {
        let start = search_from + offset;
        search_from = start + "main".len();

        // `my_main`, `main_loop` and `object.main` are not the entry point.
        let before = text[..start].chars().next_back();
        if before.is_some_and(|character| {
            character.is_alphanumeric() || character == '_' || character == '.' || character == ':'
        }) {
            continue;
        }

        let signature = text[search_from..].trim_start();
        let Some(parameters) = signature.strip_prefix('(') else {
            continue;
        };
        let Some(closing) = parameters.find(')') else {
            continue;
        };
        let rest = parameters[closing + 1..].trim_start();
        let rest = rest
            .strip_prefix("noexcept")
            .map_or(rest, |striped| striped.trim_start());
        // A body makes it a definition; `int main();` only declares it.
        if rest.starts_with('{') {
            return true;
        }
    }
    false
}

/// The sources of one language in a directory, and which of them define the
/// entry point.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectorySources {
    pub sources: Vec<PathBuf>,
    pub with_main: Vec<PathBuf>,
}

impl DirectorySources {
    /// Whether `file` is one of the sources that define `main`.
    pub fn defines_main(&self, file: &Path) -> bool {
        self.with_main.iter().any(|source| source == file)
    }
}

/// Reads a directory so the runner can tell a single-file program from one
/// built out of several sources. Unreadable files are treated as sources
/// without `main`, which keeps the compile command complete.
pub fn scan_directory(directory: &Path, language: &str) -> DirectorySources {
    const MAX_SOURCE_SIZE: u64 = 1024 * 1024;

    let extensions = source_extensions(language);
    if extensions.is_empty() {
        return DirectorySources::default();
    }

    let Ok(entries) = std::fs::read_dir(directory) else {
        return DirectorySources::default();
    };
    let mut sources = Vec::new();
    let mut with_main = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let matches_language = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                let extension = extension.to_ascii_lowercase();
                extensions.contains(&extension.as_str())
            });
        if !matches_language {
            continue;
        }
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() <= MAX_SOURCE_SIZE)
            && std::fs::read_to_string(&path).is_ok_and(|text| defines_main(&text))
        {
            with_main.push(path.clone());
        }
        sources.push(path);
    }
    sources.sort();
    with_main.sort();
    DirectorySources { sources, with_main }
}

/// Picks the steps for a file and substitutes their variables.
///
/// C and C++ files that belong to a CMake project build the project instead of
/// compiling the single file, because a file alone rarely links there. Outside
/// a CMake project a file without `main` is built together with the sibling
/// sources that do define it, and refused with [`RunError::NoMain`] when none
/// of them do.
pub fn resolve(
    request: &RunRequest<'_>,
    settings: &CodeRunnerSettings,
) -> Result<RunPlan, RunError> {
    let language = language_id_for_path(request.file).ok_or_else(|| RunError::NoCommand {
        language: "this file".to_owned(),
    })?;
    let entry = settings
        .run_case(&language)
        .ok_or_else(|| RunError::NoCommand {
            language: language.clone(),
        })?;

    match entry {
        RunCase::Command { template, source } => {
            let cwd = file_directory(request);
            single_command_plan(
                request,
                &language,
                template,
                source,
                cwd,
                settings.is_windows(),
            )
        }
        RunCase::CppFamily { source } => {
            if let Some(cmake_root) = request.cmake_root {
                // A command the user pointed at CMake replaces the built-in
                // configure-and-build plan, as it did before the plan existed.
                if let Some((template, template_source)) = settings.command("cmake") {
                    return single_command_plan(
                        request,
                        &language,
                        template,
                        template_source,
                        cmake_root.to_path_buf(),
                        settings.is_windows(),
                    );
                }
                cmake_plan(
                    request,
                    &language,
                    cmake_root,
                    source,
                    settings.is_windows(),
                )
            } else {
                compile_plan(request, &language, source, settings.is_windows())
            }
        }
    }
}

/// Picks the debug build for a file and the binary the session should launch.
pub fn resolve_debug(
    request: &RunRequest<'_>,
    settings: &CodeRunnerSettings,
) -> Result<DebugPlan, RunError> {
    let language = language_id_for_path(request.file).ok_or_else(|| RunError::NoCommand {
        language: "this file".to_owned(),
    })?;
    let entry = settings
        .debug_entry(&language)
        .ok_or_else(|| RunError::NoCommand {
            language: language.clone(),
        })?;
    if entry.build.trim().is_empty() || entry.program.trim().is_empty() {
        return Err(RunError::NoCommand { language });
    }

    let is_windows = settings.is_windows();
    let cwd = file_directory(request);

    // A CMake project builds the whole project, and the binary it produces is
    // found after the build by its target name.
    if matches!(language.as_str(), "c" | "cpp")
        && let Some(cmake_root) = request.cmake_root
    {
        if !request.cmake_available {
            return Err(RunError::MissingTool {
                tool: "cmake".to_owned(),
            });
        }
        let plan = cmake_plan(request, &language, cmake_root, entry.source, is_windows)?;
        let binary = CmakeBinary {
            build_dir: cmake_build_dir(cmake_root),
            target: cmake_target(cmake_root),
        };
        return Ok(DebugPlan {
            language,
            filename: file_name(request.file),
            build: plan,
            program: Program::Cmake(binary),
            adapter: entry.adapter.clone(),
        });
    }

    // Outside CMake the file is compiled with debug symbols, together with the
    // siblings that make up the program.
    let program = program_sources(request);
    if entry.source == CommandSource::BuiltIn
        && program.is_none()
        && !request.current_file_defines_main()
    {
        return Err(RunError::NoMain { directory: cwd });
    }

    let (build_line, program) = match (&program, entry.source) {
        // The built-in plan compiles the sources the run plan compiles, so a
        // file without `main` is built together with the one that has it. The
        // command is built here rather than substituted into a template that
        // quotes a single file name.
        (program, CommandSource::BuiltIn) => {
            let (sources, stem) = match program {
                Some(program) => (program.sources.clone(), project_name(&cwd)),
                None => (vec![file_name(request.file)], stem_of(request.file)),
            };
            let suffix = if is_windows { ".exe" } else { "" };
            let standard = if language == "cpp" { " -std=c++17" } else { "" };
            let binary = format!("{stem}.dbg{suffix}");
            (
                format!(
                    "{}{standard} -g -O0 {} -o {}",
                    compiler_for(&language),
                    quote_list(&sources, is_windows),
                    quote(&binary, is_windows)
                ),
                cwd.join(binary),
            )
        }
        (_, _) => {
            let context = task_context(request, &cwd, &language);
            let build_line = substitute_variables_in_str(&entry.build, &context)
                .ok_or(RunError::UnresolvedVariable)?;
            let program_line = substitute_variables_in_str(&entry.program, &context)
                .ok_or(RunError::UnresolvedVariable)?;
            let program = if Path::new(&program_line).is_absolute() {
                PathBuf::from(program_line)
            } else {
                cwd.join(program_line)
            };
            (build_line, program)
        }
    };

    Ok(DebugPlan {
        filename: file_name(request.file),
        language: language.clone(),
        build: RunPlan {
            language,
            filename: file_name(request.file),
            root: cwd.clone(),
            steps: vec![RunStep {
                kind: StepKind::Build,
                label: format!("build {}", file_name(request.file)),
                command_line: build_line,
                cwd,
            }],
            pending_binary: None,
            source: entry.source,
            note: None,
        },
        program: Program::Path(program),
        adapter: entry.adapter.clone(),
    })
}

/// What a Windows run step does before it starts the program: PowerShell decodes
/// a child process's stdout with `[Console]::OutputEncoding`, and .NET takes that
/// from the console's code page the first time it needs it, which is 936 on a
/// Chinese Windows. A program that writes UTF-8 then reaches the terminal as GBK
/// mojibake — `张三` arrives as `寮犱笁` — because the terminal only renders
/// UTF-8. Assigning the property is the only fix that lands in time: running
/// `chcp 65001` first does not, since .NET caches the encoding it started with.
const WINDOWS_UTF8_OUTPUT: &str =
    "try{[Console]::OutputEncoding=[Text.UTF8Encoding]::new()}catch{}; ";

/// Puts a Windows program behind [`WINDOWS_UTF8_OUTPUT`].
///
/// Do not add it to build steps: a compiler on a Chinese system writes its
/// messages in GBK, so decoding those as UTF-8 would garble them. Build steps
/// keep the console's own code page, which is what the messages were written
/// for.
fn with_utf8_program_output(command_line: String, is_windows: bool) -> String {
    if is_windows {
        format!("{WINDOWS_UTF8_OUTPUT}{command_line}")
    } else {
        command_line
    }
}

/// A plan that is one command line the user configured, such as `python "$ZED_FILENAME"`.
fn single_command_plan(
    request: &RunRequest<'_>,
    language: &str,
    template: &str,
    source: CommandSource,
    cwd: PathBuf,
    is_windows: bool,
) -> Result<RunPlan, RunError> {
    let context = task_context(request, &cwd, language);
    let command_line =
        substitute_variables_in_str(template, &context).ok_or(RunError::UnresolvedVariable)?;
    let filename = file_name(request.file);

    Ok(RunPlan {
        language: language.to_owned(),
        root: cwd.clone(),
        steps: vec![RunStep {
            kind: StepKind::Run,
            label: format!("run {filename}"),
            command_line: with_utf8_program_output(command_line, is_windows),
            cwd,
        }],
        filename,
        pending_binary: None,
        source,
        note: None,
    })
}

/// Compiles the file, or every source in its directory, and then runs the
/// binary. Build and run are separate steps so each has its own exit code.
fn compile_plan(
    request: &RunRequest<'_>,
    language: &str,
    source: CommandSource,
    is_windows: bool,
) -> Result<RunPlan, RunError> {
    let directory = file_directory(request);
    let filename = file_name(request.file);
    let compiler = compiler_for(language);
    let standard = if language == "cpp" { " -std=c++17" } else { "" };
    let suffix = if is_windows { ".exe" } else { "" };

    let program = program_sources(request);
    let (sources, binary_stem, note) = match program {
        Some(program) => {
            let note = format!(
                "{} has main, so all {} {} files in {} are compiled together and run as one program.",
                program.main,
                program.sources.len(),
                language,
                project_name(&directory)
            );
            (program.sources, project_name(&directory), Some(note))
        }
        None => {
            if !request.current_file_defines_main() && source == CommandSource::BuiltIn {
                return Err(RunError::NoMain { directory });
            }
            (
                vec![filename.clone()],
                request
                    .file
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                None,
            )
        }
    };

    let binary = format!("{binary_stem}{suffix}");
    let sources = quote_list(&sources, is_windows);
    let build_line = format!(
        "{compiler}{standard} {sources} -o {}",
        quote(&binary, is_windows)
    );
    let run_line = run_line_for(&binary, is_windows);

    Ok(RunPlan {
        language: language.to_owned(),
        filename: filename.clone(),
        root: directory.clone(),
        steps: vec![
            RunStep {
                kind: StepKind::Build,
                label: format!("build {filename}"),
                command_line: build_line,
                cwd: directory.clone(),
            },
            RunStep {
                kind: StepKind::Run,
                label: format!("run {filename}"),
                command_line: with_utf8_program_output(run_line, is_windows),
                cwd: directory,
            },
        ],
        pending_binary: None,
        source,
        note,
    })
}

/// Configures the project when it has no build directory yet, builds it, and
/// leaves the target binary for the caller to find once the build has run.
fn cmake_plan(
    request: &RunRequest<'_>,
    language: &str,
    cmake_root: &Path,
    source: CommandSource,
    _is_windows: bool,
) -> Result<RunPlan, RunError> {
    if !request.cmake_available {
        return Err(RunError::MissingTool {
            tool: "cmake".to_owned(),
        });
    }

    let build_dir = cmake_build_dir(cmake_root);
    let mut steps = Vec::new();
    if !build_dir.is_dir() {
        steps.push(RunStep {
            kind: StepKind::Configure,
            label: format!("configure {}", project_name(cmake_root)),
            command_line: "cmake -S . -B build".to_owned(),
            cwd: cmake_root.to_path_buf(),
        });
    }
    steps.push(RunStep {
        kind: StepKind::Build,
        label: format!("build {}", project_name(cmake_root)),
        command_line: "cmake --build build".to_owned(),
        cwd: cmake_root.to_path_buf(),
    });

    Ok(RunPlan {
        language: language.to_owned(),
        filename: file_name(request.file),
        root: cmake_root.to_path_buf(),
        steps,
        pending_binary: Some(CmakeBinary {
            build_dir,
            target: cmake_target(cmake_root),
        }),
        source,
        note: Some(format!(
            "{} belongs to a CMake project; building {} and running its executable.",
            file_name(request.file),
            project_name(cmake_root)
        )),
    })
}

/// Finds the program a finished CMake build produced and starts it in the
/// project root, so the program reads files the way it does from a shell there.
pub fn cmake_run_step(
    project_root: &Path,
    binary: &CmakeBinary,
    is_windows: bool,
) -> Result<RunStep, RunError> {
    let program = find_cmake_binary(&binary.build_dir, binary.target.as_deref()).ok_or(
        RunError::NoCommand {
            language: format!(
                "the CMake project at {} (its build produced no executable named by an add_executable target)",
                project_root.display()
            ),
        },
    )?;
    Ok(RunStep {
        kind: StepKind::Run,
        label: format!("run {}", project_name(project_root)),
        command_line: with_utf8_program_output(run_program_line(&program, is_windows), is_windows),
        cwd: project_root.to_path_buf(),
    })
}

/// The executable a CMake build produced: the named target if the project has
/// one, otherwise the first executable in the build directory's shallowest
/// level.
pub fn find_cmake_binary(build_dir: &Path, target: Option<&str>) -> Option<PathBuf> {
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    collect_executables(build_dir, 0, target, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, path)| path).next()
}

fn collect_executables(
    directory: &Path,
    depth: usize,
    target: Option<&str>,
    found: &mut Vec<(usize, PathBuf)>,
) {
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_executables(&path, depth + 1, target, found);
            continue;
        }
        if !is_executable_file(&path) {
            continue;
        }
        if let Some(target) = target
            && path.file_stem().is_none_or(|stem| stem != target)
        {
            continue;
        }
        found.push((depth, path));
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        // Unix executables usually have no extension at all.
        return !cfg!(windows) && path.is_file();
    };
    match extension.to_ascii_lowercase().as_str() {
        "exe" => cfg!(windows),
        // Build directories are full of these; they are never the program.
        "o" | "obj" | "a" | "lib" | "so" | "dylib" | "dll" | "pdb" | "ilk" | "exp" | "cmake"
        | "txt" | "json" | "ninja" | "make" | "log" | "c" | "cc" | "cpp" | "h" | "hpp" | "d" => {
            false
        }
        _ => !cfg!(windows) && path.is_file(),
    }
}

/// Reads the first `add_executable` target name out of a project's
/// `CMakeLists.txt`, which is what the build will call the program.
pub fn cmake_target(project_root: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(project_root.join("CMakeLists.txt")).ok()?;
    let lowercase = contents.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(offset) = lowercase[search_from..].find("add_executable") {
        let start = search_from + offset + "add_executable".len();
        search_from = start;
        // A commented-out target is not the target that gets built.
        let line_start = contents[..start].rfind('\n').map_or(0, |index| index + 1);
        if contents[line_start..start].contains('#') {
            continue;
        }
        let directive = &contents[start..];
        let directive = directive.trim_start();
        let Some(arguments) = directive.strip_prefix('(') else {
            continue;
        };
        let name = arguments
            .trim_start()
            .split(|character: char| {
                character.is_whitespace() || character == ')' || character == '('
            })
            .next()
            .unwrap_or_default();
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    None
}

fn cmake_build_dir(project_root: &Path) -> PathBuf {
    project_root.join("build")
}

fn project_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned())
}

fn compiler_for(language: &str) -> &'static str {
    match language {
        "c" => "gcc",
        _ => "g++",
    }
}

fn run_program_line(program: &Path, is_windows: bool) -> String {
    let program = program.to_string_lossy();
    if is_windows {
        format!("& {}", quote(&program, is_windows))
    } else {
        quote(&program, is_windows)
    }
}

fn run_line_for(binary: &str, is_windows: bool) -> String {
    if is_windows {
        format!("& {}", quote(&format!(".\\{binary}"), is_windows))
    } else {
        format!("\"./{binary}\"")
    }
}

/// The sources that make up the program when the current file has no `main`
/// but the directory has one. `None` means the file is the whole program.
struct ProgramSources {
    /// Every source of the directory, by file name.
    sources: Vec<String>,
    /// The source that defines the entry point, by file name.
    main: String,
}

fn program_sources(request: &RunRequest<'_>) -> Option<ProgramSources> {
    if request.current_file_defines_main() {
        return None;
    }
    let main = request.directory.with_main.first()?;
    Some(ProgramSources {
        sources: request
            .directory
            .sources
            .iter()
            .map(|source| file_name(source))
            .collect(),
        main: file_name(main),
    })
}

fn quote(value: &str, is_windows: bool) -> String {
    if is_windows {
        format!("\"{}\"", value.replace('"', "`\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn quote_list(values: &[String], is_windows: bool) -> String {
    values
        .iter()
        .map(|value| quote(value, is_windows))
        .collect::<Vec<_>>()
        .join(" ")
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

/// Whether a tool a plan needs is installed.
pub fn tool_available(name: &str) -> bool {
    find_executable(name, &search_path()).is_some()
}

pub fn find_executable(name: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .flat_map(|directory| [directory.join(name), directory.join(format!("{name}.exe"))])
        .find(|candidate| candidate.is_file())
}

fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

fn file_directory(request: &RunRequest<'_>) -> PathBuf {
    request
        .file
        .parent()
        .unwrap_or(request.worktree_root)
        .to_path_buf()
}

fn stem_of(file: &Path) -> String {
    file.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
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

    fn directory_sources(
        sources: &[&str],
        with_main: &[&str],
        directory: &Path,
    ) -> DirectorySources {
        DirectorySources {
            sources: sources.iter().map(|name| directory.join(name)).collect(),
            with_main: with_main.iter().map(|name| directory.join(name)).collect(),
        }
    }

    fn request<'a>(
        file: &'a Path,
        root: &'a Path,
        directory: DirectorySources,
        cmake_root: Option<&'a Path>,
        cmake_available: bool,
    ) -> RunRequest<'a> {
        RunRequest {
            file,
            worktree_root: root,
            cmake_root,
            directory,
            cmake_available,
        }
    }

    fn temp_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "code-runner-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");
        directory
    }

    /// A Windows run line as the plan must build it: the UTF-8 setting, then the
    /// command. A step that stops asking for UTF-8 output fails the comparisons
    /// that use this.
    fn windows_run_line(command: &str) -> String {
        format!("{WINDOWS_UTF8_OUTPUT}{command}")
    }

    #[test]
    fn a_file_with_main_is_compiled_alone_and_then_run() {
        let root = Path::new(r"C:\work");
        let file = root.join("main.cpp");
        let plan = resolve(
            &request(
                &file,
                root,
                directory_sources(&["main.cpp", "helper.cpp"], &["main.cpp"], root),
                None,
                false,
            ),
            &settings(&[], true),
        )
        .expect("cpp always has a plan");

        assert_eq!(
            plan.steps,
            vec![
                RunStep {
                    kind: StepKind::Build,
                    label: "build main.cpp".to_owned(),
                    command_line: r#"g++ -std=c++17 "main.cpp" -o "main.exe""#.to_owned(),
                    cwd: root.to_path_buf(),
                },
                RunStep {
                    kind: StepKind::Run,
                    label: "run main.cpp".to_owned(),
                    command_line: windows_run_line(r#"& ".\main.exe""#),
                    cwd: root.to_path_buf(),
                },
            ]
        );
        assert_eq!(plan.pending_binary, None);
        assert_eq!(plan.note, None);
    }

    #[test]
    fn a_windows_run_step_asks_for_utf8_output_before_it_starts_the_program() {
        // PowerShell decodes a child's stdout with the console code page (936 on
        // a Chinese Windows), so a UTF-8 program would reach the terminal as GBK
        // mojibake unless the run step sets the encoding itself first.
        let root = Path::new(r"C:\work");
        let file = root.join("main.cpp");
        let plan = resolve(
            &request(
                &file,
                root,
                directory_sources(&["main.cpp"], &["main.cpp"], root),
                None,
                false,
            ),
            &settings(&[], true),
        )
        .expect("cpp always has a plan");

        assert_eq!(
            plan.run_step()
                .expect("the plan runs the program")
                .command_line,
            r#"try{[Console]::OutputEncoding=[Text.UTF8Encoding]::new()}catch{}; & ".\main.exe""#
        );
        assert!(
            !plan.steps[0]
                .command_line
                .contains("[Console]::OutputEncoding"),
            "the build step keeps the console's own code page: {}",
            plan.steps[0].command_line
        );
    }

    #[test]
    fn a_file_without_main_compiles_the_whole_directory() {
        let root = Path::new("/work/Union_Find");
        let file = root.join("class.cpp");
        let plan = resolve(
            &request(
                &file,
                root,
                directory_sources(&["class.cpp", "main.cpp"], &["main.cpp"], root),
                None,
                false,
            ),
            &settings(&[], false),
        )
        .expect("the directory has a main");

        assert_eq!(
            plan.steps[0].command_line,
            r#"g++ -std=c++17 'class.cpp' 'main.cpp' -o 'Union_Find'"#
        );
        assert_eq!(plan.steps[1].command_line, r#""./Union_Find""#);
        assert_eq!(plan.steps[1].kind, StepKind::Run);
        assert!(
            plan.note
                .as_deref()
                .is_some_and(|note| note.contains("main.cpp has main")),
            "the note explains why the whole directory is compiled: {:?}",
            plan.note
        );
    }

    #[test]
    fn a_directory_without_any_main_is_refused_instead_of_failing_to_link() {
        let root = Path::new("/work/notes");
        let file = root.join("class.cpp");
        let plan = resolve(
            &request(
                &file,
                root,
                directory_sources(&["class.cpp", "other.cpp"], &[], root),
                None,
                false,
            ),
            &settings(&[], false),
        );

        assert_eq!(
            plan,
            Err(RunError::NoMain {
                directory: root.to_path_buf()
            })
        );
    }

    #[test]
    fn a_configured_command_runs_the_file_alone_even_without_main() {
        let root = Path::new("/work/notes");
        let file = root.join("class.cpp");
        let plan = resolve(
            &request(
                &file,
                root,
                directory_sources(&["class.cpp"], &[], root),
                None,
                false,
            ),
            &settings(&[("cpp", "clang++ \"$ZED_FILENAME\"")], false),
        )
        .expect("the user's command wins");

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].command_line, "clang++ \"class.cpp\"");
        assert_eq!(plan.source, CommandSource::Settings);
    }

    #[test]
    fn a_cmake_project_configures_when_needed_builds_and_runs_its_target() {
        let project = temp_directory("cmake-configure");
        std::fs::write(
            project.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.20)\nproject(Union)\nadd_executable(\n  union_run\n  main.cpp\n)\n",
        )
        .expect("write CMakeLists.txt");
        let file = project.join("class.cpp");

        let plan = resolve(
            &request(
                &file,
                &project,
                DirectorySources::default(),
                Some(&project),
                true,
            ),
            &settings(&[], false),
        )
        .expect("cmake projects resolve");

        assert_eq!(
            plan.steps
                .iter()
                .map(|step| (step.kind, step.command_line.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (StepKind::Configure, "cmake -S . -B build"),
                (StepKind::Build, "cmake --build build"),
            ]
        );
        assert_eq!(
            plan.pending_binary,
            Some(CmakeBinary {
                build_dir: project.join("build"),
                target: Some("union_run".to_owned()),
            })
        );
        assert_eq!(plan.root, project);
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn an_existing_build_directory_skips_the_configure_step() {
        let project = temp_directory("cmake-existing");
        std::fs::create_dir_all(project.join("build")).expect("build directory");
        std::fs::write(
            project.join("CMakeLists.txt"),
            "add_executable(app main.cpp)\n",
        )
        .expect("write CMakeLists.txt");
        let file = project.join("main.cpp");

        let plan = resolve(
            &request(
                &file,
                &project,
                DirectorySources::default(),
                Some(&project),
                true,
            ),
            &settings(&[], false),
        )
        .expect("cmake projects resolve");

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].kind, StepKind::Build);
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn a_missing_cmake_is_reported_instead_of_leaving_a_shell_error() {
        let project = temp_directory("cmake-missing");
        let file = project.join("main.cpp");
        let plan = resolve(
            &request(
                &file,
                &project,
                DirectorySources::default(),
                Some(&project),
                false,
            ),
            &settings(&[], false),
        );

        assert_eq!(
            plan,
            Err(RunError::MissingTool {
                tool: "cmake".to_owned()
            })
        );
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn a_cmake_run_step_finds_the_binary_a_multi_config_build_produced() {
        let project = temp_directory("cmake-binary");
        let debug_dir = project.join("build").join("Debug");
        std::fs::create_dir_all(&debug_dir).expect("build directory");
        let binary = debug_dir.join("union_run.exe");
        std::fs::write(&binary, b"not really an executable").expect("binary");
        std::fs::write(project.join("build").join("CMakeCache.txt"), b"cache").expect("cache");

        let step = cmake_run_step(
            &project,
            &CmakeBinary {
                build_dir: project.join("build"),
                target: Some("union_run".to_owned()),
            },
            true,
        )
        .expect("the built binary is found");

        assert_eq!(step.kind, StepKind::Run);
        assert_eq!(step.cwd, project);
        assert_eq!(
            step.command_line,
            windows_run_line(&format!(r#"& "{}""#, binary.display()))
        );
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn a_cmake_project_without_a_findable_binary_says_so() {
        let project = temp_directory("cmake-no-binary");
        std::fs::create_dir_all(project.join("build")).expect("build directory");

        let step = cmake_run_step(
            &project,
            &CmakeBinary {
                build_dir: project.join("build"),
                target: None,
            },
            true,
        );

        assert!(step.is_err(), "an empty build directory has no program");
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn the_cmake_target_is_read_from_the_project_file() {
        let project = temp_directory("cmake-target");
        std::fs::write(
            project.join("CMakeLists.txt"),
            "# add_executable(commented main.cpp)\nproject(x)\nADD_EXECUTABLE  ( my app.cpp )\n",
        )
        .expect("write CMakeLists.txt");

        assert_eq!(cmake_target(&project), Some("my".to_owned()));
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn unknown_extensions_use_the_extension_as_the_language_key() {
        let root = Path::new("/work");
        let file = root.join("script.lua");
        let plan = resolve(
            &request(&file, root, DirectorySources::default(), None, false),
            &settings(&[("lua", "lua \"$ZED_FILENAME\"")], false),
        )
        .expect("the user added a lua command");

        assert_eq!(plan.steps[0].command_line, "lua \"script.lua\"");
        assert_eq!(plan.language, "lua");
    }

    #[test]
    fn files_without_a_command_do_not_resolve() {
        let root = Path::new("/work");
        let file = root.join("notes.md");
        let result = resolve(
            &request(&file, root, DirectorySources::default(), None, false),
            &settings(&[], false),
        );

        assert_eq!(
            result,
            Err(RunError::NoCommand {
                language: "md".to_owned()
            })
        );
    }

    #[test]
    fn an_unknown_task_variable_fails_to_resolve_instead_of_running_a_broken_line() {
        let root = Path::new("/work");
        let file = root.join("main.py");
        let result = resolve(
            &request(&file, root, DirectorySources::default(), None, false),
            &settings(&[("python", "python3 \"$ZED_SYMBOL\"")], false),
        );

        assert_eq!(result, Err(RunError::UnresolvedVariable));
    }

    #[test]
    fn rust_python_and_node_are_recognized() {
        let root = Path::new("/work");
        let settings = settings(&[], false);
        for (file, expected) in [
            ("src/main.rs", "cargo run"),
            ("tool.py", r#"python3 "tool.py""#),
            ("index.mjs", r#"node "index.mjs""#),
            ("app.tsx", r#"node --experimental-strip-types "app.tsx""#),
        ] {
            let file = root.join(file);
            let plan = resolve(
                &request(&file, root, DirectorySources::default(), None, false),
                &settings,
            )
            .unwrap_or_else(|error| panic!("{file:?} should resolve: {error:?}"));
            assert_eq!(plan.steps.len(), 1, "{file:?}");
            assert_eq!(plan.steps[0].command_line, expected, "{file:?}");
        }
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
    fn main_is_recognized_only_as_a_definition() {
        assert!(defines_main("int main() {\n  return 0;\n}\n"));
        assert!(defines_main("int main(void) { return 0; }"));
        assert!(defines_main(
            "int main(int argc, char **argv) noexcept { return 0; }"
        ));
        assert!(!defines_main("int main();"));
        assert!(!defines_main("int my_main() { return 0; }"));
        assert!(!defines_main("void run() { object.main(); }"));
        assert!(!defines_main("class UnionFind {\n  int find(int x);\n};"));
    }

    #[test]
    fn a_debug_plan_compiles_the_sources_the_run_plan_compiles() {
        let root = Path::new(r"C:\work\Union_Find");
        let file = root.join("class.cpp");
        let debug = resolve_debug(
            &request(
                &file,
                root,
                directory_sources(&["class.cpp", "main.cpp"], &["main.cpp"], root),
                None,
                false,
            ),
            &settings(&[], true),
        )
        .expect("cpp has a default debug plan");

        assert_eq!(
            debug.build.steps[0].command_line,
            r#"g++ -std=c++17 -g -O0 "class.cpp" "main.cpp" -o "Union_Find.dbg.exe""#
        );
        assert_eq!(
            debug.program,
            Program::Path(root.join("Union_Find.dbg.exe"))
        );
        assert_eq!(debug.adapter, "GDB");
        assert_eq!(debug.label(), "debug class.cpp");
    }

    #[test]
    fn a_debug_plan_of_a_single_file_launches_its_own_binary() {
        let debug = resolve_debug(
            &RunRequest {
                file: Path::new(r"C:\work\my file.cpp"),
                worktree_root: Path::new(r"C:\work"),
                cmake_root: None,
                directory: directory_sources(
                    &["my file.cpp"],
                    &["my file.cpp"],
                    Path::new(r"C:\work"),
                ),
                cmake_available: false,
            },
            &settings(&[], true),
        )
        .expect("cpp has a default debug plan");

        assert_eq!(
            debug.build.steps[0].command_line,
            r#"g++ -std=c++17 -g -O0 "my file.cpp" -o "my file.dbg.exe""#
        );
        assert_eq!(
            debug.program,
            Program::Path(Path::new(r"C:\work\my file.dbg.exe").to_path_buf())
        );
    }

    #[test]
    fn debugging_a_cmake_project_builds_it_and_finds_the_target() {
        let project = temp_directory("debug-cmake");
        std::fs::write(
            project.join("CMakeLists.txt"),
            "add_executable(union_run main.cpp)\n",
        )
        .expect("write CMakeLists.txt");
        let file = project.join("main.cpp");

        let debug = resolve_debug(
            &request(
                &file,
                &project,
                DirectorySources::default(),
                Some(&project),
                true,
            ),
            &settings(&[], false),
        )
        .expect("cmake projects can be debugged");

        assert_eq!(
            debug.build.steps.last().map(|step| step.kind),
            Some(StepKind::Build)
        );
        assert_eq!(
            debug.program,
            Program::Cmake(CmakeBinary {
                build_dir: project.join("build"),
                target: Some("union_run".to_owned()),
            })
        );
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn languages_without_a_debug_plan_do_not_resolve_to_debug() {
        let result = resolve_debug(
            &RunRequest {
                file: Path::new("/work/main.py"),
                worktree_root: Path::new("/work"),
                cmake_root: None,
                directory: DirectorySources::default(),
                cmake_available: false,
            },
            &settings(&[], false),
        );

        assert_eq!(
            result,
            Err(RunError::NoCommand {
                language: "python".to_owned()
            })
        );
    }

    #[test]
    fn a_directory_scan_finds_the_sources_and_the_one_with_main() {
        let directory = temp_directory("scan");
        std::fs::write(
            directory.join("class.cpp"),
            "#include <vector>\nclass A {};\n",
        )
        .expect("class.cpp");
        std::fs::write(directory.join("main.cpp"), "int main() {\n  return 0;\n}\n")
            .expect("main.cpp");
        std::fs::write(directory.join("notes.txt"), "not a source").expect("notes.txt");

        let scan = scan_directory(&directory, "cpp");

        assert_eq!(
            scan.sources
                .iter()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["class.cpp".to_owned(), "main.cpp".to_owned()]
        );
        assert!(scan.defines_main(&directory.join("main.cpp")));
        assert!(!scan.defines_main(&directory.join("class.cpp")));

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn gdb_is_looked_up_on_the_given_search_path() {
        let directory = temp_directory("gdb");
        let gdb = directory.join("gdb.exe");
        std::fs::write(&gdb, b"not really gdb").expect("fake gdb");

        assert_eq!(
            find_executable("gdb", std::slice::from_ref(&directory)),
            Some(gdb)
        );
        assert_eq!(find_executable("gdb", &[]), None);

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn only_the_gdb_adapter_needs_a_binary_on_the_path() {
        assert_eq!(debug_adapter_available_in("GDB", &[]), false);
        assert_eq!(debug_adapter_available_in("CodeLLDB", &[]), true);
        assert_eq!(debug_adapter_available_in("gdb", &[]), true);
    }
}
