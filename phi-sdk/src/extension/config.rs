use std::collections::HashSet;

use super::{BuiltinExtension, memory::MemoryConfig};
use crate::config::CodemodeConfig;

#[derive(Debug, Clone, Default)]
pub struct ExtensionConfig {
    pub disabled: HashSet<BuiltinExtension>,
    pub memory: Option<MemoryConfig>,
    pub codemode: CodemodeConfig,
}

impl ExtensionConfig {
    pub fn is_enabled(&self, extension: BuiltinExtension) -> bool {
        if self.disabled.contains(&extension) {
            return false;
        }
        match extension {
            BuiltinExtension::Memory => self.memory.as_ref().is_none_or(|memory| memory.enabled),
            BuiltinExtension::Codemode => self.codemode.enabled,
            BuiltinExtension::Mcp => true,
        }
    }

    /// Applies exclusions to typed settings before preparing resources.
    pub fn resolve(mut self) -> Self {
        if self.disabled.contains(&BuiltinExtension::Codemode) {
            self.codemode.enabled = false;
        }
        if self.disabled.contains(&BuiltinExtension::Memory) {
            self.memory.get_or_insert_with(Default::default).enabled = false;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn resolution_preserves_settings_and_is_idempotent(
            memory_enabled in prop::option::of(any::<bool>()),
            codemode_enabled in any::<bool>(),
            disabled in any::<[bool; 3]>(),
        ) {
            let config = ExtensionConfig {
                disabled: BuiltinExtension::ALL.into_iter().zip(disabled)
                    .filter_map(|(extension, disabled)| disabled.then_some(extension)).collect(),
                memory: memory_enabled.map(|enabled| MemoryConfig {
                    enabled,
                    ..Default::default()
                }),
                codemode: CodemodeConfig {
                    enabled: codemode_enabled,
                    inline_budget: 17,
                    ..Default::default()
                },
            };
            for extension in BuiltinExtension::ALL {
                let expected = !config.disabled.contains(&extension) && match extension {
                    BuiltinExtension::Memory => memory_enabled.unwrap_or(true),
                    BuiltinExtension::Codemode => codemode_enabled,
                    BuiltinExtension::Mcp => true,
                };
                prop_assert_eq!(config.is_enabled(extension), expected);
            }
            let resolved = config.clone().resolve();
            let repeated = resolved.clone().resolve();
            for extension in BuiltinExtension::ALL {
                prop_assert_eq!(resolved.is_enabled(extension), config.is_enabled(extension));
            }
            prop_assert_eq!(resolved.codemode.enabled, config.is_enabled(BuiltinExtension::Codemode));
            prop_assert_eq!(resolved.memory.as_ref().map(|memory| memory.enabled),
                if config.disabled.contains(&BuiltinExtension::Memory) { Some(false) } else { memory_enabled });
            prop_assert_eq!(resolved.codemode.inline_budget, config.codemode.inline_budget);
            prop_assert_eq!(&resolved.disabled, &config.disabled);
            prop_assert_eq!(serde_json::to_value(&resolved.memory).unwrap(),
                serde_json::to_value(&repeated.memory).unwrap());
            prop_assert_eq!(serde_json::to_value(&resolved.codemode).unwrap(),
                serde_json::to_value(&repeated.codemode).unwrap());
        }
    }
}
