use serde::{Deserialize, Serialize};

use super::hooks::HookConfig;
use super::tape::{TapeConfig, types::TapeAnchorConfig};
use super::types::{MemoryDirSettings, MemoryInjectionMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub memory_dir: Option<MemoryDirSettings>,
    #[serde(default)]
    pub tape: Option<TapeConfig>,
    #[serde(default, deserialize_with = "super::types::deserialize_injection")]
    pub injection: Option<MemoryInjectionMode>,
    #[serde(default)]
    pub hooks: Option<HookConfig>,
    #[serde(default)]
    pub skill: Option<bool>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_dir: None,
            tape: None,
            injection: None,
            hooks: None,
            skill: None,
        }
    }
}

pub(crate) fn merge_tape_config(default: Option<TapeConfig>, config: TapeConfig) -> TapeConfig {
    let Some(default) = default else {
        return config;
    };
    TapeConfig {
        enabled: Some(config.enabled.unwrap_or(true)),
        only_git: config.only_git.or(default.only_git),
        exclude_dirs: if config.exclude_dirs.is_empty() {
            default.exclude_dirs
        } else {
            config.exclude_dirs
        },
        context: config.context.or(default.context),
        anchor: Some(match (default.anchor, config.anchor) {
            (Some(default), Some(config)) => TapeAnchorConfig {
                label_prefix: config.label_prefix.or(default.label_prefix),
                mode: config.mode.or(default.mode),
                keywords: config.keywords.or(default.keywords),
            },
            (Some(default), None) => default,
            (None, Some(config)) => config,
            (None, None) => TapeAnchorConfig::default(),
        }),
    }
}

impl MemoryConfig {
    pub fn tape_enabled(&self) -> bool {
        self.enabled
            && self
                .tape
                .as_ref()
                .is_some_and(|tape| tape.enabled != Some(false))
    }

    pub fn skill_enabled(&self) -> bool {
        self.skill != Some(false)
    }
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_requires_an_explicit_mode() {
        use serde_json::json;

        for value in [
            json!({}),
            json!({"injection": false}),
            json!({"injection": null}),
        ] {
            let config: MemoryConfig = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(config.injection, None);
            let settings = super::super::memory_core::settings_from_config(&config);
            assert_eq!(settings.injection, None);
            let settings: super::super::MemorySettings = serde_json::from_value(value).unwrap();
            assert_eq!(settings.injection, None);
        }
        let config: MemoryConfig =
            serde_json::from_value(json!({"injection": "message-append"})).unwrap();
        assert_eq!(config.injection, Some(MemoryInjectionMode::MessageAppend));
        assert_eq!(
            super::super::memory_core::default_settings().injection,
            None
        );
        for invalid in [json!(true), json!("off"), json!(0), json!({})] {
            assert!(serde_json::from_value::<MemoryConfig>(json!({"injection": invalid})).is_err());
        }
    }

    #[test]
    fn resolves_memory_feature_enablement() {
        assert!(!MemoryConfig::default().tape_enabled());
        assert!(
            MemoryConfig {
                tape: Some(TapeConfig::default()),
                ..MemoryConfig::default()
            }
            .tape_enabled()
        );
        assert!(
            !MemoryConfig {
                enabled: false,
                tape: Some(TapeConfig::default()),
                ..MemoryConfig::default()
            }
            .tape_enabled()
        );
        assert!(
            !MemoryConfig {
                tape: Some(TapeConfig {
                    enabled: Some(false),
                    ..TapeConfig::default()
                }),
                ..MemoryConfig::default()
            }
            .tape_enabled()
        );
        assert!(
            !MemoryConfig {
                skill: Some(false),
                ..MemoryConfig::default()
            }
            .skill_enabled()
        );
    }

    #[test]
    fn merge_tape_config_respects_explicit_enablement() {
        for (configured, expected) in [(None, true), (Some(false), false)] {
            let merged = merge_tape_config(
                Some(
                    crate::extension::memory::memory_core::default_settings()
                        .tape
                        .unwrap(),
                ),
                TapeConfig {
                    enabled: configured,
                    ..TapeConfig::default()
                },
            );

            assert_eq!(merged.enabled, Some(expected));
        }
    }
}
