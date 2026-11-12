use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::provider::ProviderApi;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AgentMode {
    #[default]
    Default,
    Plan,
}

impl AgentMode {
    pub fn from_name(name: Option<&str>) -> Self {
        match name {
            Some(name) if name.eq_ignore_ascii_case("plan") => Self::Plan,
            _ => Self::Default,
        }
    }

    pub(crate) fn prompt(self, prompt_override: Option<&str>) -> Option<&str> {
        prompt_override.or(match self {
            Self::Default => None,
            Self::Plan => Some(crate::agent::options::PLAN_MODE_PROMPT),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiReasoningMode {
    Pro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiReasoningContext {
    Auto,
    AllTurns,
    CurrentTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct OpenAiReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<OpenAiReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<OpenAiReasoningMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<OpenAiReasoningContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<OpenAiReasoningSummary>,
}

#[derive(Debug, Clone)]
pub struct CustomProviderConfig {
    pub api: ProviderApi,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum CodemodeCache {
    Enabled(bool),
    Directory(#[serde(deserialize_with = "deserialize_cache_directory")] PathBuf),
}

impl Default for CodemodeCache {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

impl CodemodeCache {
    pub(crate) fn directory(&self, config_dir: &Path) -> Option<PathBuf> {
        match self {
            Self::Enabled(true) => Some(config_dir.join("cache/codemode")),
            Self::Enabled(false) => None,
            Self::Directory(directory) => Some(config_dir.join(directory)),
        }
    }
}

fn deserialize_cache_directory<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let directory = <PathBuf as serde::Deserialize>::deserialize(deserializer)?;
    if directory.as_os_str().is_empty() {
        return Err(serde::de::Error::custom(
            "cache directory must not be empty",
        ));
    }
    Ok(directory)
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CodemodeConfig {
    pub enabled: bool,
    pub inline_budget: usize,
    pub cache: CodemodeCache,
}

impl Default for CodemodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            inline_budget: 3000,
            cache: CodemodeCache::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// Application-owned identity captured with effective configuration during execution.
    pub switch: Option<String>,
    pub extensions: crate::extension::ExtensionConfig,
    pub default_thinking_level: Option<String>,
    pub openai_reasoning: Option<OpenAiReasoningConfig>,
    pub agent_mode: AgentMode,
    pub mode_prompt_override: Option<String>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub no_tools: Option<bool>,
    /// Forces new delegations to background without changing admitted work.
    pub agent_background: bool,
    pub no_context_files: Option<bool>,
    pub context_window: Option<u64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub compact_enabled: Option<bool>,
    pub custom_providers: HashMap<String, CustomProviderConfig>,
}

impl RuntimeConfig {
    pub fn default_thinking_level(&self) -> Option<&str> {
        self.default_thinking_level.as_deref()
    }

    pub fn resolve_context_window(&self) -> u64 {
        self.context_window.unwrap_or(128_000)
    }

    pub fn resolve_reserve_tokens(&self) -> u64 {
        self.reserve_tokens.unwrap_or(16_384)
    }

    pub fn resolve_keep_recent_tokens(&self) -> u64 {
        self.keep_recent_tokens.unwrap_or(20_000)
    }

    pub fn resolve_compact_enabled(&self) -> bool {
        self.compact_enabled.unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentMode, CodemodeCache, CodemodeConfig, RuntimeConfig};
    use serde_json::json;

    #[test]
    fn codemode_config_uses_boolean_switch_and_camel_case_budget() {
        for enabled in [false, true] {
            for budget in [0, 17, 3000] {
                let value = json!({"enabled": enabled, "inlineBudget": budget});
                let config: CodemodeConfig = serde_json::from_value(value.clone()).unwrap();
                assert_eq!(config.enabled, enabled);
                assert_eq!(config.inline_budget, budget);
                assert_eq!(
                    serde_json::to_value(config).unwrap(),
                    json!({"enabled": enabled, "inlineBudget": budget, "cache": true})
                );
            }
        }
        assert!(!CodemodeConfig::default().enabled);
        assert!(!RuntimeConfig::default().extensions.codemode.enabled);
        for value in [
            json!({}),
            json!({"inlineBudget": 17}),
            json!({"cache": false}),
            json!({"cache": "/tmp/phi-cache"}),
        ] {
            let config: CodemodeConfig = serde_json::from_value(value).unwrap();
            assert!(!config.enabled);
        }
        let config: CodemodeConfig = serde_json::from_value(json!({"enabled": true})).unwrap();
        assert!(config.enabled);
        assert_eq!(config.inline_budget, 3000);
        for invalid in [
            json!({"mode": "on"}),
            json!({"enabled": "true"}),
            json!({"enabled": 1}),
            json!({"enabled": null}),
            json!({"inline_budget": 0}),
            json!({"inlineBudget": -1}),
            json!({"inlineBudget": 1.5}),
            json!({"unknown": true}),
        ] {
            assert!(
                serde_json::from_value::<CodemodeConfig>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn codemode_cache_accepts_switch_or_directory() {
        for cache in [
            json!(true),
            json!(false),
            json!("/tmp/phi-cache"),
            json!("cache/custom"),
        ] {
            let config: CodemodeConfig = serde_json::from_value(json!({"cache": cache})).unwrap();
            assert_eq!(serde_json::to_value(config).unwrap()["cache"], cache);
        }
        let config: CodemodeConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(serde_json::to_value(config).unwrap()["cache"], true);
        for cache in [json!(null), json!(0), json!({}), json!([]), json!("")] {
            assert!(serde_json::from_value::<CodemodeConfig>(json!({"cache": cache})).is_err());
        }
    }

    #[test]
    fn codemode_cache_resolves_against_global_config_directory() {
        let config_dir = std::path::Path::new("/tmp/phi-config");
        assert_eq!(
            CodemodeCache::default().directory(config_dir),
            Some(config_dir.join("cache/codemode"))
        );
        assert_eq!(CodemodeCache::Enabled(false).directory(config_dir), None);
        assert_eq!(
            CodemodeCache::Directory("custom".into()).directory(config_dir),
            Some(config_dir.join("custom"))
        );
        let absolute = config_dir.join("absolute");
        assert_eq!(
            CodemodeCache::Directory(absolute.clone()).directory(config_dir),
            Some(absolute)
        );
    }

    #[test]
    fn agent_mode_resolves_only_builtin_plan_semantics() {
        assert_eq!(AgentMode::from_name(Some("plan")), AgentMode::Plan);
        assert_eq!(AgentMode::from_name(Some("custom")), AgentMode::Default);
        assert_eq!(AgentMode::from_name(None), AgentMode::Default);
    }
}
