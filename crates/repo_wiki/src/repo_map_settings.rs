use settings::{RegisterSetting, Settings};

pub use settings::RepoMapMode;

/// How much work the repository map is allowed to do.
#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct RepoMapSettings {
    pub mode: RepoMapMode,
}

impl RepoMapSettings {
    /// Whether a map may be built at all.
    pub fn builds_map(&self) -> bool {
        self.mode != RepoMapMode::Off
    }

    /// Whether a new chat starts with a project digest.
    pub fn injects_digest(&self) -> bool {
        self.mode == RepoMapMode::Full
    }

    /// Whether the map is built as soon as a project opens.
    pub fn prewarms(&self) -> bool {
        self.mode == RepoMapMode::Full
    }
}

impl Settings for RepoMapSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let mode = content
            .repo_map
            .as_ref()
            .and_then(|repo_map| repo_map.mode);
        Self {
            mode: mode.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::SettingsContent;

    fn settings_with_mode(mode: Option<RepoMapMode>) -> RepoMapSettings {
        RepoMapSettings::from_settings(&SettingsContent {
            repo_map: Some(settings::RepoMapSettingsContent { mode }),
            ..SettingsContent::default()
        })
    }

    #[test]
    fn a_project_without_the_setting_gets_basic() {
        let settings = settings_with_mode(None);

        assert_eq!(settings.mode, RepoMapMode::Basic);
        assert!(settings.builds_map());
        assert!(
            !settings.injects_digest(),
            "a new chat must not start with a digest unless the user asked for it"
        );
    }

    #[test]
    fn off_builds_nothing_and_never_injects() {
        let settings = settings_with_mode(Some(RepoMapMode::Off));

        assert!(!settings.builds_map());
        assert!(!settings.injects_digest());
        assert!(!settings.prewarms());
    }

    #[test]
    fn full_is_the_only_mode_that_injects_a_digest() {
        let settings = settings_with_mode(Some(RepoMapMode::Full));

        assert!(settings.builds_map());
        assert!(settings.injects_digest());
        assert!(settings.prewarms());
    }
}
