use settings::{RegisterSetting, Settings};

const DEFAULT_MAX_TOTAL_MB: u32 = 64;
const DEFAULT_RETENTION_DAYS: u32 = 7;

#[derive(Clone, Debug, RegisterSetting)]
pub struct SessionSnapshotSettings {
    pub enabled: bool,
    pub max_total_mb: u32,
    pub retention_days: u32,
}

impl SessionSnapshotSettings {
    pub fn max_total_bytes(&self) -> u64 {
        u64::from(self.max_total_mb) * 1024 * 1024
    }

    pub fn retention_seconds(&self) -> i64 {
        i64::from(self.retention_days) * 24 * 60 * 60
    }
}

impl Settings for SessionSnapshotSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.session_snapshots.clone().unwrap_or_default();
        Self {
            enabled: content.enabled.unwrap_or(true),
            max_total_mb: content
                .max_total_mb
                .unwrap_or(DEFAULT_MAX_TOTAL_MB)
                .clamp(1, 4096),
            retention_days: content
                .retention_days
                .unwrap_or(DEFAULT_RETENTION_DAYS)
                .clamp(1, 365),
        }
    }
}
