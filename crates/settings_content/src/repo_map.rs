use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// How much work the repository map does for a project.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum RepoMapMode {
    /// Build nothing: no map, no snapshot on disk, and no digest in a chat.
    Off,
    /// Build the map only when you ask for it: opening the Repo Map panel, or
    /// asking an agent for the map. No digest is added to a chat.
    #[default]
    Basic,
    /// Also build the map in the background when a project opens, so the panel
    /// has data the first time you open it, and put a project digest in the
    /// first message of a new chat.
    Full,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, MergeFrom, PartialEq)]
pub struct RepoMapSettingsContent {
    /// How much the repository map does for each project.
    ///
    /// `"off"` builds nothing. `"basic"` builds the map only when you open the
    /// Repo Map panel or ask an agent for it, and never adds a digest to a chat.
    /// `"full"` also builds the map in the background when a project opens and
    /// puts a project digest in the first message of a new chat.
    ///
    /// The map parses the project with tree-sitter: about 15–20 seconds for a
    /// large repository the first time (this one has 19.5k files), under a
    /// second afterwards from its parse cache. The snapshot it writes is small
    /// (0.58 MiB for the same repository); the cost is mostly the parse, and in
    /// `"full"` the digest it adds to a first message.
    ///
    /// Default: basic
    pub mode: Option<RepoMapMode>,
}
