use collections::{HashMap, HashSet};
use settings::{RegisterSetting, Settings};

use crate::resolve::CommandSource;

/// The commands "Run Current File" knows about, after merging the user's
/// `code_runner.executor_map` over the built-in defaults.
#[derive(Debug, Clone, RegisterSetting)]
pub struct CodeRunnerSettings {
    executor_map: HashMap<String, String>,
    /// Keys that came from the settings file rather than from the defaults,
    /// so the run dialog can say where a command came from.
    overrides: HashSet<String>,
}

impl Default for CodeRunnerSettings {
    fn default() -> Self {
        Self::new(&HashMap::default(), cfg!(windows))
    }
}

impl CodeRunnerSettings {
    /// Merges a user provided map over the built-in defaults.
    pub fn new(user_map: &HashMap<String, String>, is_windows: bool) -> Self {
        let mut executor_map = default_executor_map(is_windows);
        let mut overrides = HashSet::default();

        for (language, command) in user_map {
            if command.trim().is_empty() {
                // An empty entry removes the default, so a language can be
                // opted out of running altogether.
                executor_map.remove(language);
                continue;
            }
            executor_map.insert(language.clone(), command.trim().to_owned());
            overrides.insert(language.clone());
        }

        Self {
            executor_map,
            overrides,
        }
    }

    /// The command template for a language, and where it came from.
    pub fn command(&self, language: &str) -> Option<(&str, CommandSource)> {
        let command = self.executor_map.get(language)?;
        let source = if self.overrides.contains(language) {
            CommandSource::Settings
        } else {
            CommandSource::BuiltIn
        };
        Some((command.as_str(), source))
    }
}

impl Settings for CodeRunnerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let executor_map = content
            .code_runner
            .clone()
            .and_then(|code_runner| code_runner.executor_map)
            .unwrap_or_default();
        Self::new(&executor_map, cfg!(windows))
    }
}

/// Commands for the languages the IDE supports out of the box.
///
/// The Windows entries differ in three ways: the compiler writes a `.exe`, the
/// executable has to be run with the PowerShell call operator (`&`) because it
/// is quoted, and `python` is the interpreter name there.
pub fn default_executor_map(is_windows: bool) -> HashMap<String, String> {
    let mut map = HashMap::default();

    let (c, cpp, rust, python, javascript, typescript) = if is_windows {
        (
            "gcc \"$ZED_FILENAME\" -o \"$ZED_STEM.exe\" && & \".\\$ZED_STEM.exe\"",
            "g++ -std=c++17 \"$ZED_FILENAME\" -o \"$ZED_STEM.exe\" && & \".\\$ZED_STEM.exe\"",
            "cargo run",
            "python \"$ZED_FILENAME\"",
            "node \"$ZED_FILENAME\"",
            "node --experimental-strip-types \"$ZED_FILENAME\"",
        )
    } else {
        (
            "gcc \"$ZED_FILENAME\" -o \"$ZED_STEM\" && \"./$ZED_STEM\"",
            "g++ -std=c++17 \"$ZED_FILENAME\" -o \"$ZED_STEM\" && \"./$ZED_STEM\"",
            "cargo run",
            "python3 \"$ZED_FILENAME\"",
            "node \"$ZED_FILENAME\"",
            "node --experimental-strip-types \"$ZED_FILENAME\"",
        )
    };

    map.insert("c".to_owned(), c.to_owned());
    map.insert("cpp".to_owned(), cpp.to_owned());
    map.insert("rust".to_owned(), rust.to_owned());
    map.insert("python".to_owned(), python.to_owned());
    map.insert("javascript".to_owned(), javascript.to_owned());
    map.insert("typescript".to_owned(), typescript.to_owned());
    map.insert(
        "cmake".to_owned(),
        "cmake -S . -B build && cmake --build build".to_owned(),
    );

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn user_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(language, command)| (language.to_string(), command.to_string()))
            .collect()
    }

    #[test]
    fn user_entries_override_defaults_and_are_marked_as_settings() {
        let settings = CodeRunnerSettings::new(&user_map(&[("cpp", "clang++ -O2 \"$ZED_FILE\"")]), false);

        assert_eq!(
            settings.command("cpp"),
            Some(("clang++ -O2 \"$ZED_FILE\"", CommandSource::Settings))
        );
        assert_eq!(
            settings.command("rust").map(|(_, source)| source),
            Some(CommandSource::BuiltIn)
        );
    }

    #[test]
    fn empty_user_entry_removes_the_language() {
        let settings = CodeRunnerSettings::new(&user_map(&[("python", "  ")]), false);

        assert_eq!(settings.command("python"), None);
    }

    #[test]
    fn windows_defaults_run_the_binary_with_the_call_operator() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), true);

        assert_eq!(
            settings.command("cpp").map(|(command, _)| command),
            Some("g++ -std=c++17 \"$ZED_FILENAME\" -o \"$ZED_STEM.exe\" && & \".\\$ZED_STEM.exe\"")
        );
        assert_eq!(
            settings.command("python").map(|(command, _)| command),
            Some("python \"$ZED_FILENAME\"")
        );
    }

    #[test]
    fn unix_defaults_write_and_run_a_bare_binary() {
        let settings = CodeRunnerSettings::new(&HashMap::default(), false);

        assert_eq!(
            settings.command("c").map(|(command, _)| command),
            Some("gcc \"$ZED_FILENAME\" -o \"$ZED_STEM\" && \"./$ZED_STEM\"")
        );
        assert_eq!(
            settings.command("python").map(|(command, _)| command),
            Some("python3 \"$ZED_FILENAME\"")
        );
    }
}
