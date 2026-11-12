use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::provider::ProviderApi;

pub const MODEL_CATALOG_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_CUSTOM_MODEL_CONTEXT_WINDOW: u64 = 128_000;
pub const DEFAULT_CUSTOM_MODEL_MAX_TOKENS: u64 = 16_384;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDescriptor {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: ProviderApi,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub cost: ModelCost,
}

pub type ThinkingLevelMap = BTreeMap<ThinkingLevel, Option<String>>;
pub type ProviderModels = BTreeMap<String, ModelDescriptor>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.as_str() == value)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl ModelDescriptor {
    pub fn supported_thinking_levels(&self) -> Vec<ThinkingLevel> {
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| self.supports_thinking_level(*level))
            .collect()
    }

    pub fn clamp_thinking_level(&self, requested: ThinkingLevel) -> ThinkingLevel {
        let supported = self.supported_thinking_levels();
        supported
            .iter()
            .copied()
            .find(|level| *level >= requested)
            .or_else(|| supported.last().copied())
            .unwrap_or(ThinkingLevel::Off)
    }

    pub fn resolve_thinking_level(&self, requested: &str) -> String {
        let selected = self
            .clamp_thinking_level(ThinkingLevel::parse(requested).unwrap_or(ThinkingLevel::Off));
        self.thinking_level_map
            .as_ref()
            .and_then(|levels| levels.get(&selected))
            .and_then(Clone::clone)
            .unwrap_or_else(|| selected.as_str().to_string())
    }

    fn supports_thinking_level(&self, level: ThinkingLevel) -> bool {
        if !self.reasoning {
            return level == ThinkingLevel::Off;
        }
        let mapped = self
            .thinking_level_map
            .as_ref()
            .and_then(|levels| levels.get(&level));
        if mapped == Some(&None) {
            return false;
        }
        !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max) || mapped.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    Text,
    Image,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<ModelCostTier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    pub input_tokens_above: u64,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogManifest {
    pub schema_version: u32,
    pub generated_at: String,
    pub providers: BTreeMap<String, ManifestProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestProvider {
    pub source: CatalogSource,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserCatalogConfig {
    pub providers: BTreeMap<String, UserProviderConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct UserProviderConfig {
    pub api: Option<ProviderApi>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub models: BTreeMap<String, ModelOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ModelOverride {
    pub reasoning: Option<bool>,
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Option<Vec<ModelInput>>,
    pub context_window: Option<u64>,
    #[serde(rename = "maxTokens")]
    pub max_output_tokens: Option<u64>,
    pub cost: Option<ModelCostOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ModelCostOverride {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub tiers: Option<Vec<ModelCostTier>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogStore {
    pub schema_version: u32,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceState>,
    #[serde(default)]
    pub providers: BTreeMap<String, StoredProvider>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SourceState {
    pub checked_at: Option<i64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoredProvider {
    pub models: ProviderModels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogSource {
    #[serde(rename = "models.dev")]
    ModelsDev,
    #[serde(rename = "openai-codex")]
    OpenAiCodex,
    #[serde(rename = "openrouter")]
    OpenRouter,
}

impl CatalogSource {
    pub(crate) const ALL: [Self; 3] = [Self::ModelsDev, Self::OpenAiCodex, Self::OpenRouter];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ModelsDev => "models.dev",
            Self::OpenAiCodex => "openai-codex",
            Self::OpenRouter => "openrouter",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasoning_descriptor(levels: ThinkingLevelMap) -> ModelDescriptor {
        ModelDescriptor {
            provider: "openai".to_string(),
            id: "reasoning-model".to_string(),
            name: "Reasoning Model".to_string(),
            api: ProviderApi::OpenAiResponses,
            reasoning: true,
            thinking_level_map: Some(levels),
            input: vec![ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 16_384,
            cost: ModelCost::default(),
        }
    }

    #[test]
    fn thinking_levels_clamp_and_non_reasoning_models_turn_off() {
        let descriptor = reasoning_descriptor(BTreeMap::from([
            (ThinkingLevel::Low, None),
            (ThinkingLevel::Medium, None),
            (ThinkingLevel::High, Some("provider-high".to_string())),
            (ThinkingLevel::Xhigh, None),
            (ThinkingLevel::Max, Some("provider-max".to_string())),
        ]));

        assert_eq!(
            descriptor.supported_thinking_levels(),
            [
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
        assert_eq!(
            descriptor.clamp_thinking_level(ThinkingLevel::Low),
            ThinkingLevel::High
        );
        assert_eq!(descriptor.resolve_thinking_level("low"), "provider-high");
        assert_eq!(descriptor.resolve_thinking_level("xhigh"), "provider-max");
        assert_eq!(descriptor.resolve_thinking_level("unknown"), "off");

        let mut without_off = descriptor.clone();
        let levels = without_off.thinking_level_map.as_mut().unwrap();
        levels.insert(ThinkingLevel::Off, None);
        levels.insert(ThinkingLevel::Minimal, None);
        assert_eq!(
            without_off.resolve_thinking_level("unknown"),
            "provider-high"
        );

        let mut non_reasoning = reasoning_descriptor(BTreeMap::new());
        non_reasoning.reasoning = false;
        assert_eq!(non_reasoning.resolve_thinking_level("high"), "off");
        assert_eq!(
            non_reasoning.supported_thinking_levels(),
            [ThinkingLevel::Off]
        );
    }

    proptest::proptest! {
        #[test]
        fn clamped_thinking_is_supported_and_preserves_supported_requests(
            enabled in proptest::collection::vec(proptest::bool::ANY, ThinkingLevel::ALL.len()),
            reasoning in proptest::bool::ANY,
        ) {
            let levels = ThinkingLevel::ALL.into_iter().zip(enabled)
                .map(|(level, enabled)| (level, enabled.then(|| level.as_str().to_string())))
                .collect();
            let mut descriptor = reasoning_descriptor(levels);
            descriptor.reasoning = reasoning;
            let supported = descriptor.supported_thinking_levels();
            for requested in ThinkingLevel::ALL {
                let selected = descriptor.clamp_thinking_level(requested);
                if supported.is_empty() {
                    proptest::prop_assert_eq!(selected, ThinkingLevel::Off);
                } else {
                    proptest::prop_assert!(supported.contains(&selected));
                }
                if supported.contains(&requested) {
                    proptest::prop_assert_eq!(selected, requested);
                }
                let expected = supported.iter().copied().filter(|level| *level >= requested)
                    .min().or_else(|| supported.iter().copied().max())
                    .unwrap_or(ThinkingLevel::Off);
                proptest::prop_assert_eq!(selected, expected);
                proptest::prop_assert_eq!(descriptor.resolve_thinking_level(requested.as_str()), selected.as_str());
            }
        }
    }
}
