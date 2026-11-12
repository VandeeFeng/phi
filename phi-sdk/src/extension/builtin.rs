use super::memory::MemoryHooks;
use crate::agent::options::AgentOptions;
use crate::config::RuntimeConfig;
use crate::core::event::ToolDisplay;
use crate::hooks::Hook;
use crate::permission::runtime::PermissionRuntime;
use crate::tools::{ToolDisplayFormatter, ToolRegistration};

use super::mcp::McpClientManager;

#[derive(Clone)]
pub(crate) struct BuiltinExtensions {
    extensions: Vec<BuiltinRegistration>,
}

impl Default for BuiltinExtensions {
    fn default() -> Self {
        Self {
            extensions: vec![
                BuiltinRegistration::Codemode,
                BuiltinRegistration::Mcp,
                BuiltinRegistration::Memory(None),
            ],
        }
    }
}

pub(crate) struct ExtensionToolContext<'a> {
    pub(crate) config: &'a RuntimeConfig,
    pub(crate) options: &'a AgentOptions,
    pub(crate) permission: Option<PermissionRuntime>,
    pub(crate) mcp: Option<&'a McpClientManager>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinExtension {
    Memory,
    Mcp,
    Codemode,
}

/// A bundled Skill's availability; applications own discovery and installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinSkill {
    pub name: &'static str,
    pub enabled: bool,
}

pub fn builtin_skills(config: &RuntimeConfig) -> Vec<BuiltinSkill> {
    BuiltinExtension::ALL
        .into_iter()
        .flat_map(|extension| {
            let mut skills = match extension {
                BuiltinExtension::Memory => {
                    super::memory::skill_declarations(config.extensions.memory.as_ref())
                }
                _ => Vec::new(),
            };
            if !config.extensions.is_enabled(extension) {
                for skill in &mut skills {
                    skill.enabled = false;
                }
            }
            skills
        })
        .collect()
}

/// Reserved builtin command names, including capabilities disabled in a generation.
pub fn builtin_command_names() -> Vec<&'static str> {
    BuiltinExtension::ALL
        .into_iter()
        .flat_map(|extension| match extension {
            BuiltinExtension::Memory => super::memory::command_names(),
            _ => Vec::new(),
        })
        .collect()
}

pub(super) fn adapter_commands(config: &RuntimeConfig) -> Vec<super::RegisteredCommand> {
    BuiltinExtension::ALL
        .into_iter()
        .filter(|extension| config.extensions.is_enabled(*extension))
        .flat_map(|extension| match extension {
            BuiltinExtension::Memory => {
                super::memory::adapter_commands(config.extensions.memory.as_ref())
            }
            _ => Vec::new(),
        })
        .collect()
}

pub(crate) fn delegated_tool_denials(mut denied: Vec<String>) -> Vec<String> {
    // Keep safety restrictions independent of current capability activation.
    for extension in BuiltinExtension::ALL {
        let restrictions = match extension {
            BuiltinExtension::Memory => super::memory::delegated_tool_denials(),
            _ => &[],
        };
        for &name in restrictions {
            if !denied.iter().any(|tool| tool == name) {
                denied.push(name.into());
            }
        }
    }
    denied
}

impl BuiltinExtension {
    pub const ALL: [Self; 3] = [Self::Memory, Self::Mcp, Self::Codemode];

    pub fn name(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Mcp => "mcp",
            Self::Codemode => "codemode",
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::Memory => "builtin:memory",
            Self::Mcp => "builtin:mcp",
            Self::Codemode => "builtin:codemode",
        }
    }
}

#[derive(Clone)]
enum BuiltinRegistration {
    Codemode,
    Mcp,
    Memory(Option<MemoryHooks>),
}

impl BuiltinRegistration {
    fn kind(&self) -> BuiltinExtension {
        match self {
            Self::Memory(_) => BuiltinExtension::Memory,
            Self::Mcp => BuiltinExtension::Mcp,
            Self::Codemode => BuiltinExtension::Codemode,
        }
    }
}

impl BuiltinExtensions {
    pub(crate) fn with_disabled(
        mut self,
        disabled: &std::collections::HashSet<BuiltinExtension>,
    ) -> Self {
        self.extensions
            .retain(|extension| !disabled.contains(&extension.kind()));
        self
    }

    pub(crate) fn with_memory(mut self, hooks: MemoryHooks) -> Self {
        for extension in &mut self.extensions {
            if let BuiltinRegistration::Memory(memory) = extension {
                *memory = Some(hooks);
                break;
            }
        }
        self
    }

    pub(crate) fn hooks(&self) -> Vec<Hook> {
        self.extensions
            .iter()
            .flat_map(|extension| match extension {
                BuiltinRegistration::Memory(Some(hooks)) => hooks.hooks(),
                _ => Vec::new(),
            })
            .collect()
    }

    pub(super) async fn tools(&self, context: ExtensionToolContext<'_>) -> Vec<ToolRegistration> {
        let mut tools = Vec::new();
        for extension in &self.extensions {
            if !context.config.extensions.is_enabled(extension.kind()) {
                continue;
            }
            match extension {
                BuiltinRegistration::Codemode => {
                    if !context.options.no_tools {
                        tools.push(super::codemode::registration());
                    }
                }
                BuiltinRegistration::Mcp => {
                    if let Some(mcp) = context.mcp {
                        tools.extend(
                            mcp.tool_registrations(context.permission.clone())
                                .await
                                .into_iter()
                                .map(Into::into),
                        );
                    }
                }
                BuiltinRegistration::Memory(_) => {
                    tools.extend(context.options.memory.collect_tools(&context));
                }
            }
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::{MemoryConfig, MemoryDirSettings, TapeConfig};
    use crate::tools::ToolCatalogBuilder;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn delegated_denials_preserve_policy_and_are_idempotent(
            denied in prop::collection::vec(prop_oneof![
                "[a-z_]{1,24}",
                prop::sample::select(vec!["tape_anchor", "memory_sync", "memory_search", "tape_read", "Agent"])
                    .prop_map(str::to_string),
            ], 0..20),
        ) {
            let merged = delegated_tool_denials(denied.clone());
            prop_assert!(merged.starts_with(&denied));
            prop_assert_eq!(delegated_tool_denials(merged.clone()), merged.clone());
            for name in ["tape_anchor", "tape_delete", "tape_reset", "memory_sync"] {
                prop_assert!(merged.iter().any(|tool| tool == name));
            }
            for name in ["memory_search", "tape_read", "Agent"] {
                prop_assert_eq!(merged.iter().any(|tool| tool == name), denied.iter().any(|tool| tool == name));
            }
        }

        #[test]
        fn builtin_skills_follow_extension_configuration(
            memory_present in any::<bool>(),
            memory_enabled in any::<bool>(),
            skill in prop::option::of(any::<bool>()),
            tape_present in any::<bool>(),
            tape_enabled in prop::option::of(any::<bool>()),
            disabled in any::<[bool; 3]>(),
        ) {
            let config = RuntimeConfig {
                extensions: super::super::ExtensionConfig {
                memory: memory_present.then_some(MemoryConfig {
                    enabled: memory_enabled,
                    skill,
                    tape: tape_present.then_some(TapeConfig {
                        enabled: tape_enabled,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                disabled: BuiltinExtension::ALL.into_iter().zip(disabled)
                    .filter_map(|(extension, disabled)| disabled.then_some(extension)).collect(),
                ..Default::default()
                },
                ..Default::default()
            };
            let skills = builtin_skills(&config);
            let enabled = memory_present && memory_enabled && skill != Some(false)
                && !config.extensions.disabled.contains(&BuiltinExtension::Memory);
            prop_assert_eq!(skills.len(), 5);
            for declaration in &skills {
                let expected = enabled && (declaration.name != "memory-digest"
                    || (tape_present && tape_enabled != Some(false)));
                prop_assert_eq!(declaration.enabled, expected);
            }
            let names: std::collections::HashSet<_> = skills.iter().map(|skill| skill.name).collect();
            prop_assert_eq!(names, std::collections::HashSet::from([
                "memory-digest", "memory-import", "memory-init", "memory-write", "planning-with-files",
            ]));
        }

        #[test]
        fn command_declarations_follow_extension_configuration(
            present in any::<bool>(), enabled in any::<bool>(),
            tape in any::<bool>(), disabled in any::<bool>(),
        ) {
            let mut config = RuntimeConfig::default();
            config.extensions.memory = present.then_some(MemoryConfig {
                enabled,
                tape: Some(TapeConfig { enabled: Some(tape), ..Default::default() }),
                ..Default::default()
            });
            if disabled {
                config.extensions.disabled.insert(BuiltinExtension::Memory);
            }
            let commands = adapter_commands(&config);
            let active = present && enabled && !disabled;
            prop_assert_eq!(commands.len(), if active { 3 + usize::from(tape) } else { 0 });
            for command in commands {
                prop_assert!(builtin_command_names().contains(&command.name.as_str()));
                prop_assert_eq!(command.source, BuiltinExtension::Memory.path());
                if command.name == "memory-inject" {
                    prop_assert_eq!(command.target, super::super::CommandTarget::Prompt);
                } else {
                    prop_assert_eq!(command.target, super::super::CommandTarget::Application {
                        action: command.name.clone()
                    });
                }
                prop_assert_eq!(command.arguments.is_some(), command.name != "memory-refresh");
            }
        }

        #[test]
        fn builtin_tools_obey_runtime_gates(
            no_tools in any::<bool>(),
            codemode_enabled in any::<bool>(),
            memory_enabled in any::<bool>(),
            tape_enabled in any::<bool>(),
            disabled in any::<[bool; 3]>(),
        ) {
            let root = std::path::Path::new("/tmp")
                .join(format!("phi-builtin-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let mut config = RuntimeConfig {
                extensions: super::super::ExtensionConfig {
                    disabled: BuiltinExtension::ALL.into_iter().zip(disabled)
                        .filter_map(|(extension, disabled)| disabled.then_some(extension)).collect(),
                    ..Default::default()
                },
                ..Default::default()
            };
            config.extensions.codemode.enabled = codemode_enabled;
            config.extensions.memory = Some(MemoryConfig {
                enabled: memory_enabled,
                memory_dir: Some(MemoryDirSettings {
                    local_path: Some(root.display().to_string()),
                    ..Default::default()
                }),
                tape: Some(TapeConfig {
                    enabled: Some(tape_enabled),
                    only_git: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let options = AgentOptions {
                no_tools,
                memory: crate::extension::memory::MemorySession::create(
                    &config, "test-session".into(), &root,
                ).unwrap(),
                ..Default::default()
            };
            let memory_active = memory_enabled
                && !config.extensions.disabled.contains(&BuiltinExtension::Memory);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let tools = runtime.block_on(super::super::collect_tool_registrations(
                None,
                ExtensionToolContext {
                    config: &config,
                    options: &options,
                    permission: None,
                    mcp: None,
                },
            ));
            prop_assert_eq!(
                tools.iter().any(|registration| registration.metadata.runtime.durable_finalization),
                memory_active && tape_enabled && !no_tools
            );
            let mut catalog = ToolCatalogBuilder::new(no_tools);
            for registration in tools {
                catalog.register(registration);
            }
            let registry = catalog.build().unwrap();
            std::fs::remove_dir_all(root).unwrap();
            prop_assert_eq!(registry.is_available("Codemode"), !no_tools && codemode_enabled
                && !config.extensions.disabled.contains(&BuiltinExtension::Codemode));
            for name in ["memory_search", "memory_check", "memory_sync"] {
                prop_assert_eq!(registry.is_available(name), !no_tools && memory_active);
            }
            for name in ["tape_anchor", "tape_info", "tape_read", "tape_delete", "tape_reset"] {
                prop_assert_eq!(registry.is_available(name), !no_tools && memory_active && tape_enabled);
            }
        }
    }
}

impl ToolDisplayFormatter for BuiltinExtensions {
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>> {
        for extension in &self.extensions {
            if let BuiltinRegistration::Memory(_) = extension
                && let Some(display) = crate::extension::memory::MemoryToolDisplayFormatter
                    .format_tool_display(name, args)?
            {
                return Ok(Some(display));
            }
        }
        Ok(None)
    }
}
