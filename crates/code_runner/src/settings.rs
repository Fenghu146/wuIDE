use collections::HashMap;
use settings::{RegisterSetting, Settings};

use crate::resolve::{CommandSource, DebugEntry};

/// How a language runs: either a command line from the settings, or the
/// built-in C/C++ plan that links the sources of a directory or builds a CMake
/// project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunCase<'a> {
    Command {
        template: &'a str,
        source: CommandSource,
    },
    CppFamily {
        source: CommandSource,
    },
}

/// The run and debug plans "Run Current File" and "Debug Current File" know
/// about, after merging the user's `code_runner` maps over the built-in
/// defaults.
#[derive(Debug, Clone, RegisterSetting)]
pub struct CodeRunnerSettings {
    run_entries: HashMap<String, RunEntry>,
    debug_entries: HashMap<String, DebugEntry>,
    is_windows: bool,
}

#[derive(Debug, Clone)]
enum RunEntry {
    Command {
        command: String,
        source: CommandSource,
    },
    CppFamily {
        source: CommandSource,
    },
}

impl Default for CodeRunnerSettings {
    fn default() -> Self {
        Self::new(&HashMap::default(), &HashMap::default(), cfg!(windows))
    }
}

impl CodeRunnerSettings {
    /// Merges the user's maps over the built-in defaults.
    pub fn new(
        user_run_map: &HashMap<String, String>,
        user_debug_map: &HashMap<String, DebugEntry>,
        is_windows: bool,
    ) -> Self {
        let mut run_entries = default_run_map(is_windows)
            .into_iter()
            .map(|(language, command)| {
                (
                    language,
                    RunEntry::Command {
                        command,
                        source: CommandSource::BuiltIn,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        // C and C++ have no single command: the runner decides between one
        // file, a directory of sources, and a CMake project.
        for language in CPP_FAMILY {
            run_entries.insert(
                (*language).to_owned(),
                RunEntry::CppFamily {
                    source: CommandSource::BuiltIn,
                },
            );
        }
        for (language, command) in user_run_map {
            if command.trim().is_empty() {
                // An empty entry removes the default, so a language can be
                // opted out of running altogether.
                run_entries.remove(language);
                continue;
            }
            run_entries.insert(
                language.clone(),
                RunEntry::Command {
                    command: command.trim().to_owned(),
                    source: CommandSource::Settings,
                },
            );
        }

        // A user entry that only sets some fields keeps the built-in values of
        // the others.
        let mut debug_entries = default_debug_map(is_windows);
        for (language, entry) in user_debug_map {
            let defaults = defaults_for_debug_entry(language, is_windows);
            debug_entries.insert(
                language.clone(),
                DebugEntry {
                    build: pick(&entry.build, defaults.as_ref().map(|entry| &entry.build))
                        .unwrap_or_default(),
                    program: pick(
                        &entry.program,
                        defaults.as_ref().map(|entry| &entry.program),
                    )
                    .unwrap_or_default(),
                    adapter: pick(
                        &entry.adapter,
                        defaults.as_ref().map(|entry| &entry.adapter),
                    )
                    .unwrap_or_else(|| "GDB".to_owned()),
                    source: CommandSource::Settings,
                },
            );
        }

        Self {
            run_entries,
            debug_entries,
            is_windows,
        }
    }

    /// How a language runs, and where that choice came from.
    pub fn run_case(&self, language: &str) -> Option<RunCase<'_>> {
        self.run_entries.get(language).map(|entry| match entry {
            RunEntry::Command { command, source } => RunCase::Command {
                template: command,
                source: *source,
            },
            RunEntry::CppFamily { source } => RunCase::CppFamily { source: *source },
        })
    }

    /// A command line configured for a language, if the user set one.
    pub fn command(&self, language: &str) -> Option<(&str, CommandSource)> {
        match self.run_entries.get(language)? {
            RunEntry::Command { command, source } => Some((command.as_str(), *source)),
            RunEntry::CppFamily { .. } => None,
        }
    }

    /// The debug plan for a language, and where it came from.
    pub fn debug_entry(&self, language: &str) -> Option<&DebugEntry> {
        self.debug_entries.get(language)
    }

    /// Whether plans are built for Windows, which decides how paths are
    /// quoted and how a program is started.
    pub fn is_windows(&self) -> bool {
        self.is_windows
    }
}

fn pick(value: &str, fallback: Option<&String>) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        fallback.cloned()
    } else {
        Some(value.to_owned())
    }
}

impl Settings for CodeRunnerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.code_runner.clone().unwrap_or_default();

        let run_map = content
            .executor_map
            .unwrap_or_default()
            .into_iter()
            .map(|(language, command)| (language, command.trim().to_owned()))
            .collect::<HashMap<_, _>>();

        let is_windows = cfg!(windows);
        let debug_map = content
            .debug_map
            .unwrap_or_default()
            .into_iter()
            .map(|(language, entry)| {
                let defaults = defaults_for_debug_entry(&language, is_windows);
                let debug_entry = DebugEntry {
                    build: pick(
                        entry.build.as_deref().unwrap_or_default(),
                        defaults.as_ref().map(|entry| &entry.build),
                    )
                    .unwrap_or_default(),
                    program: pick(
                        entry.program.as_deref().unwrap_or_default(),
                        defaults.as_ref().map(|entry| &entry.program),
                    )
                    .unwrap_or_default(),
                    adapter: pick(
                        entry.adapter.as_deref().unwrap_or_default(),
                        defaults.as_ref().map(|entry| &entry.adapter),
                    )
                    .unwrap_or_else(|| "GDB".to_owned()),
                    source: CommandSource::Settings,
                };
                (language, debug_entry)
            })
            .collect::<HashMap<_, _>>();

        Self::new(&run_map, &debug_map, is_windows)
    }
}

/// The languages the built-in C/C++ plan handles.
pub const CPP_FAMILY: [&str; 2] = ["c", "cpp"];

/// Commands for the languages WuIDE supports out of the box.
///
/// The Windows entries differ in how the interpreter is named, and because
/// C/C++ is not in here at all: those are compiled by the runner.
pub fn default_run_map(is_windows: bool) -> HashMap<String, String> {
    let mut map = HashMap::default();

    let python = if is_windows { "python" } else { "python3" };
    map.insert("rust".to_owned(), "cargo run".to_owned());
    map.insert("python".to_owned(), format!("{python} \"$ZED_FILENAME\""));
    map.insert("javascript".to_owned(), "node \"$ZED_FILENAME\"".to_owned());
    map.insert(
        "typescript".to_owned(),
        "node --experimental-strip-types \"$ZED_FILENAME\"".to_owned(),
    );

    map
}

/// Build commands with debug symbols for the languages WuIDE debugs out of
/// the box. The binary is named after the run one plus `.dbg`, so the two never
/// overwrite each other.
pub fn default_debug_map(is_windows: bool) -> HashMap<String, DebugEntry> {
    ["c", "cpp"]
        .into_iter()
        .filter_map(|language| {
            defaults_for_debug_entry(language, is_windows).map(|entry| (language.to_owned(), entry))
        })
        .collect()
}

fn defaults_for_debug_entry(language: &str, is_windows: bool) -> Option<DebugEntry> {
    let suffix = if is_windows { ".exe" } else { "" };
    let (compiler, standard) = match language {
        "c" => ("gcc", ""),
        "cpp" => ("g++", " -std=c++17"),
        _ => return None,
    };
    Some(DebugEntry {
        build: format!(
            "{compiler}{standard} -g -O0 \"$ZED_FILENAME\" -o \"$ZED_STEM.dbg{suffix}\""
        ),
        program: format!("$ZED_STEM.dbg{suffix}"),
        adapter: "GDB".to_owned(),
        source: CommandSource::BuiltIn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn debug_entry(build: &str, program: &str, adapter: &str) -> DebugEntry {
        DebugEntry {
            build: build.to_owned(),
            program: program.to_owned(),
            adapter: adapter.to_owned(),
            source: CommandSource::Settings,
        }
    }

    #[test]
    fn c_and_cpp_use_the_directory_plan_and_other_languages_a_command() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), &HashMap::default(), false);

        assert_eq!(
            settings.run_case("cpp"),
            Some(RunCase::CppFamily {
                source: CommandSource::BuiltIn
            })
        );
        assert_eq!(
            settings.run_case("python"),
            Some(RunCase::Command {
                template: "python3 \"$ZED_FILENAME\"",
                source: CommandSource::BuiltIn
            })
        );
        assert_eq!(settings.command("cpp"), None);
    }

    #[test]
    fn windows_names_the_python_interpreter_without_the_three() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), &HashMap::default(), true);

        assert_eq!(
            settings.command("python"),
            Some(("python \"$ZED_FILENAME\"", CommandSource::BuiltIn))
        );
    }

    #[test]
    fn user_run_entries_replace_the_defaults_and_are_marked_as_settings() {
        let settings = CodeRunnerSettings::new(
            &[("cpp".to_owned(), "clang++ -O2 \"$ZED_FILE\"".to_owned())]
                .into_iter()
                .collect(),
            &HashMap::default(),
            false,
        );

        assert_eq!(
            settings.run_case("cpp"),
            Some(RunCase::Command {
                template: "clang++ -O2 \"$ZED_FILE\"",
                source: CommandSource::Settings
            })
        );
        assert_eq!(
            settings.run_case("rust").map(|case| match case {
                RunCase::Command { source, .. } => source,
                RunCase::CppFamily { source } => source,
            }),
            Some(CommandSource::BuiltIn)
        );
    }

    #[test]
    fn an_empty_run_entry_removes_the_language() {
        let settings = CodeRunnerSettings::new(
            &[("python".to_owned(), "  ".to_owned())]
                .into_iter()
                .collect(),
            &HashMap::default(),
            false,
        );

        assert_eq!(settings.run_case("python"), None);
    }

    #[test]
    fn an_empty_cmake_entry_does_not_remove_the_cmake_plan() {
        let settings = CodeRunnerSettings::new(
            &[("cmake".to_owned(), String::new())].into_iter().collect(),
            &HashMap::default(),
            false,
        );

        assert_eq!(settings.command("cmake"), None);
        assert_eq!(
            settings.run_case("cpp"),
            Some(RunCase::CppFamily {
                source: CommandSource::BuiltIn
            })
        );
    }

    #[test]
    fn debug_binaries_are_named_apart_from_the_run_binaries() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), &HashMap::default(), false);

        let debug = settings.debug_entry("c").expect("c debug entry");
        assert_eq!(
            debug.build,
            "gcc -g -O0 \"$ZED_FILENAME\" -o \"$ZED_STEM.dbg\""
        );
        assert_eq!(debug.program, "$ZED_STEM.dbg");
        assert_eq!(debug.adapter, "GDB");
        assert_eq!(debug.source, CommandSource::BuiltIn);
    }

    #[test]
    fn windows_debug_binaries_get_an_exe_suffix() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), &HashMap::default(), true);

        let debug = settings.debug_entry("cpp").expect("cpp debug entry");
        assert_eq!(
            debug.build,
            "g++ -std=c++17 -g -O0 \"$ZED_FILENAME\" -o \"$ZED_STEM.dbg.exe\""
        );
        assert_eq!(debug.program, "$ZED_STEM.dbg.exe");
    }

    #[test]
    fn languages_without_a_debug_plan_have_none() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), &HashMap::default(), false);

        assert_eq!(settings.debug_entry("python"), None);
    }

    #[test]
    fn a_debug_entry_can_override_the_build_program_and_adapter() {
        let settings = CodeRunnerSettings::new(
            &HashMap::default(),
            &[
                (
                    "cpp".to_owned(),
                    debug_entry("clang++ -g \"$ZED_FILE\" -o out", "out", "CodeLLDB"),
                ),
                ("go".to_owned(), debug_entry("go build -o app", "app", "")),
            ]
            .into_iter()
            .collect(),
            false,
        );

        assert_eq!(
            settings.debug_entry("cpp"),
            Some(&debug_entry(
                "clang++ -g \"$ZED_FILE\" -o out",
                "out",
                "CodeLLDB"
            ))
        );
        assert_eq!(
            settings
                .debug_entry("go")
                .map(|entry| entry.adapter.as_str()),
            Some("GDB")
        );
    }

    #[test]
    fn a_half_filled_debug_entry_keeps_the_built_in_fields() {
        let settings = CodeRunnerSettings::new(
            &HashMap::default(),
            &[("cpp".to_owned(), debug_entry("", "", "CodeLLDB"))]
                .into_iter()
                .collect(),
            false,
        );

        let debug = settings.debug_entry("cpp").expect("cpp keeps its default");
        assert_eq!(
            debug.build,
            "g++ -std=c++17 -g -O0 \"$ZED_FILENAME\" -o \"$ZED_STEM.dbg\""
        );
        assert_eq!(debug.program, "$ZED_STEM.dbg");
        assert_eq!(debug.adapter, "CodeLLDB");
        assert_eq!(debug.source, CommandSource::Settings);
    }
}