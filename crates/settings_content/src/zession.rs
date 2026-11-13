use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

use crate::LanguageModelSelection;

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZessionSettingsContent {
    pub repos: Option<ZessionReposSettingsContent>,
    pub auto_index: Option<bool>,
    pub store_max_age_minutes: Option<u64>,
    pub index_model: Option<LanguageModelSelection>,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZessionReposSettingsContent {
    pub allow: Option<Vec<String>>,
    pub access: Option<HashMap<String, ZessionRepoAccessContent>>,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ZessionRepoAccessContent {
    Read,
    #[default]
    Write,
}
