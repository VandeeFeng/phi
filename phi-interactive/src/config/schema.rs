use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::SwitchSettings;

use phi_sdk::extension::BuiltinExtension;
use phi_sdk::extension::mcp::config::{McpServerConfig, McpSettings};
use phi_sdk::extension::memory::MemoryConfig;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct SwitchConfig {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: Option<bool>,
    pub whitelist: Vec<PathBuf>,
    pub blacklist: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub background: Option<bool>,
    pub team: Option<bool>,
    pub workflow: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookTrust {
    Trusted,
    Disabled,
}

#[derive(Debug, Clone)]
pub(crate) struct HookTrustRule {
    pub(super) path: PathBuf,
    pub(super) trust: HookTrust,
}

impl<'de> Deserialize<'de> for HookTrustRule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let (trust, path) = if let Some(path) = value.strip_prefix('-') {
            (HookTrust::Disabled, path)
        } else {
            (
                HookTrust::Trusted,
                value.strip_prefix('+').unwrap_or(&value),
            )
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(serde::de::Error::custom(
                "hook trust paths must be absolute",
            ));
        }
        Ok(Self { path, trust })
    }
}

impl Serialize for HookTrustRule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let prefix = match self.trust {
            HookTrust::Trusted => '+',
            HookTrust::Disabled => '-',
        };
        let path = self
            .path
            .to_str()
            .ok_or_else(|| serde::ser::Error::custom("hook trust paths must be UTF-8"))?;
        serializer.serialize_str(&format!("{prefix}{path}"))
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookSettings {
    enabled: Option<bool>,
    pub(crate) commands: phi_sdk::hooks::CommandHooks,
    pub(crate) trust: Option<Vec<HookTrustRule>>,
}

impl HookSettings {
    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl<'de> Deserialize<'de> for HookSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut declarations =
            serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let enabled = declarations
            .remove("enabled")
            .map(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))
            .transpose()?;
        let trust = declarations
            .remove("trust")
            .map(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))
            .transpose()?;
        let commands = serde_json::from_value(serde_json::Value::Object(declarations))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            enabled,
            commands,
            trust,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    #[serde(rename = "defaultModel")]
    pub default_model: Option<String>,
    pub switches: Option<Vec<SwitchConfig>>,
    #[serde(rename = "defaultProvider")]
    pub default_provider: Option<String>,
    #[serde(rename = "defaultThinkingLevel")]
    pub default_thinking_level: Option<String>,
    #[serde(rename = "openaiReasoning", alias = "openai_reasoning")]
    pub openai_reasoning: Option<phi_sdk::config::OpenAiReasoningConfig>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub no_tools: Option<bool>,
    #[serde(deserialize_with = "deserialize_feature")]
    pub codemode: Option<phi_sdk::config::CodemodeConfig>,
    pub agent: AgentConfig,
    pub no_context_files: Option<bool>,
    pub context_window: Option<u64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub compact_enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_memory")]
    pub memory: Option<MemoryConfig>,
    #[serde(deserialize_with = "deserialize_permission")]
    pub permission: Option<serde_json::Value>,
    #[serde(deserialize_with = "deserialize_feature")]
    pub sandbox: Option<SandboxConfig>,
    pub trust_remote: bool,
    pub show_tool_details: Option<bool>,
    pub extensions: Option<Vec<String>>,
    pub hooks: Option<HookSettings>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub mcp: Option<bool>,
    #[serde(rename = "mcpServers", alias = "mcp_servers")]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
    #[serde(rename = "settings")]
    pub mcp_settings: Option<McpSettings>,
}

fn resolve_feature_enabled(value: &mut serde_json::Value) {
    if let Some(options) = value.as_object_mut() {
        let enabled = !options.is_empty();
        options
            .entry("enabled")
            .or_insert(serde_json::Value::Bool(enabled));
    }
}

fn deserialize_feature<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Option::<serde_json::Value>::deserialize(deserializer)?
        .map(|mut value| {
            resolve_feature_enabled(&mut value);
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        })
        .transpose()
}

fn deserialize_memory<'de, D>(deserializer: D) -> Result<Option<MemoryConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut value = serde_json::Value::deserialize(deserializer)?;
    if let Some(tape) = value.get_mut("tape") {
        resolve_feature_enabled(tape);
    }
    deserialize_feature(value).map_err(serde::de::Error::custom)
}

fn deserialize_permission<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    if let Some(permission) = &value {
        phi_sdk::permission::PermissionConfig::try_from_value(permission.clone())
            .map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

#[cfg(test)]
#[test]
fn hooks_enabled_defaults_and_project_overrides() {
    use serde_json::json;

    assert!(Settings::default().hooks_enabled());
    for global in [None, Some(false), Some(true)] {
        for project in [None, Some(false), Some(true)] {
            let settings = Settings {
                hooks: Some(HookSettings {
                    enabled: global,
                    commands: serde_json::from_value(json!({
                        "SessionStart": [{"hooks": [{"type": "command", "command": "true"}]}]
                    }))
                    .unwrap(),
                    ..Default::default()
                }),
                ..Default::default()
            }
            .merge_project(Settings {
                hooks: project.map(|enabled| HookSettings {
                    enabled: Some(enabled),
                    ..Default::default()
                }),
                ..Default::default()
            });
            assert_eq!(settings.hooks_enabled(), project.or(global).unwrap_or(true));
            assert!(!settings.hooks.unwrap().is_empty());
        }
    }
    for enabled in [
        json!(true),
        json!(false),
        json!("false"),
        json!(0),
        json!(null),
    ] {
        let settings = serde_json::from_value::<Settings>(json!({"hooks": {"enabled": enabled}}));
        if let Some(enabled) = enabled.as_bool() {
            assert_eq!(settings.unwrap().hooks_enabled(), enabled);
        } else {
            assert!(settings.is_err());
        }
    }
}

#[cfg(test)]
#[test]
fn agent_settings_use_booleans_and_merge_each_field() {
    use serde_json::json;

    for field in ["background", "team", "workflow"] {
        for value in [json!("enabled"), json!("disabled"), json!(1), json!("true")] {
            assert!(
                serde_json::from_value::<Settings>(json!({ "agent": {field: value} })).is_err()
            );
        }
    }
    for input in [
        json!({"agent_teams": "enabled"}),
        json!({"agent": {"unknown": true}}),
    ] {
        assert!(serde_json::from_value::<Settings>(input).is_err());
    }
    for global_background in [None, Some(false), Some(true)] {
        for global_team in [None, Some(false), Some(true)] {
            for project_background in [None, Some(false), Some(true)] {
                for project_team in [None, Some(false), Some(true)] {
                    let settings = Settings {
                        agent: AgentConfig {
                            background: global_background,
                            team: global_team,
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                    .merge_project(Settings {
                        agent: AgentConfig {
                            background: project_background,
                            team: project_team,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    assert_eq!(
                        settings.agent.background,
                        project_background.or(global_background)
                    );
                    assert_eq!(settings.agent.team, project_team.or(global_team));
                    assert_eq!(
                        phi_sdk::config::RuntimeConfig::from(&settings).agent_background,
                        project_background.or(global_background).unwrap_or(false)
                    );
                }
            }
        }
    }
    let settings: Settings =
        serde_json::from_value(json!({"agent": {"background": true, "team": false}})).unwrap();
    assert_eq!(settings.agent.background, Some(true));
    assert_eq!(settings.agent.team, Some(false));
    let defaults: Settings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(defaults.agent.background, None);
    assert_eq!(defaults.agent.team, None);
    assert_eq!(defaults.agent.workflow, None);
    assert!(!phi_sdk::config::RuntimeConfig::from(&defaults).agent_background);
    for global in [None, Some(false), Some(true)] {
        for project in [None, Some(false), Some(true)] {
            let global_settings: Settings =
                serde_json::from_value(json!({"agent":{"workflow":global}})).unwrap();
            let project_settings: Settings =
                serde_json::from_value(json!({"agent":{"workflow":project}})).unwrap();
            assert_eq!(
                global_settings
                    .merge_project(project_settings)
                    .agent
                    .workflow,
                project.or(global)
            );
        }
    }
}

pub(crate) fn builtin_override(entries: &[String], extension: BuiltinExtension) -> Option<bool> {
    entries
        .iter()
        .filter_map(|entry| {
            if let Some(pattern) = entry.strip_prefix('!') {
                return glob::Pattern::new(pattern)
                    .ok()
                    .filter(|pattern| pattern.matches(extension.path()))
                    .map(|_| false);
            }
            let target = entry.strip_prefix(['+', '-']).unwrap_or(entry);
            (target == extension.path()).then(|| !entry.starts_with('-'))
        })
        .last()
}

const VALID_THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

impl From<&Settings> for phi_sdk::config::RuntimeConfig {
    fn from(settings: &Settings) -> Self {
        Self {
            switch: None,
            extensions: phi_sdk::extension::ExtensionConfig {
                disabled: BuiltinExtension::ALL
                    .into_iter()
                    .filter(|extension| !settings.builtin_enabled(*extension))
                    .collect(),
                memory: settings.memory.clone(),
                codemode: settings.codemode.clone().unwrap_or_default(),
            }
            .resolve(),
            default_thinking_level: settings.default_thinking_level.clone(),
            openai_reasoning: settings.openai_reasoning.clone(),
            max_tokens: settings.max_tokens,
            temperature: settings.temperature,
            no_tools: settings.no_tools,
            agent_background: settings.agent.background.unwrap_or(false),
            no_context_files: settings.no_context_files,
            context_window: settings.context_window,
            reserve_tokens: settings.reserve_tokens,
            keep_recent_tokens: settings.keep_recent_tokens,
            compact_enabled: settings.compact_enabled,
            ..Default::default()
        }
    }
}

impl Settings {
    pub fn merge_project(self, project: Settings) -> Self {
        self.merge(project)
    }

    pub fn merge(mut self, project: Settings) -> Self {
        self.switches = project.switches.or(self.switches);
        self.default_model = project.default_model.or(self.default_model);
        self.default_provider = project.default_provider.or(self.default_provider);
        self.default_thinking_level = project
            .default_thinking_level
            .or(self.default_thinking_level);
        self.openai_reasoning = project.openai_reasoning.or(self.openai_reasoning);
        self.max_tokens = project.max_tokens.or(self.max_tokens);
        self.temperature = project.temperature.or(self.temperature);
        self.no_tools = project.no_tools.or(self.no_tools);
        self.codemode = project.codemode.or(self.codemode);
        self.agent.background = project.agent.background.or(self.agent.background);
        self.agent.team = project.agent.team.or(self.agent.team);
        self.agent.workflow = project.agent.workflow.or(self.agent.workflow);
        self.no_context_files = project.no_context_files.or(self.no_context_files);
        self.context_window = project.context_window.or(self.context_window);
        self.reserve_tokens = project.reserve_tokens.or(self.reserve_tokens);
        self.keep_recent_tokens = project.keep_recent_tokens.or(self.keep_recent_tokens);
        self.compact_enabled = project.compact_enabled.or(self.compact_enabled);
        self.memory = project.memory.or(self.memory);
        self.permission = project.permission.or(self.permission);
        self.sandbox = project.sandbox.or(self.sandbox);
        self.show_tool_details = project.show_tool_details.or(self.show_tool_details);
        self.extensions = match (self.extensions, project.extensions) {
            (Some(mut global), Some(project)) => {
                global.extend(project);
                Some(global)
            }
            (global, project) => project.or(global),
        };
        if let Some(hooks) = project.hooks {
            let global = self.hooks.get_or_insert_with(Default::default);
            global.enabled = hooks.enabled.or(global.enabled);
        }
        self.skills = project.skills.or(self.skills);
        self.prompts = project.prompts.or(self.prompts);
        self.mcp = project.mcp.or(self.mcp);
        self.mcp_servers = project.mcp_servers.or(self.mcp_servers);
        self.mcp_settings = project.mcp_settings.or(self.mcp_settings);
        self
    }

    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    pub(crate) fn builtin_enabled(&self, extension: BuiltinExtension) -> bool {
        builtin_override(self.extensions.as_deref().unwrap_or_default(), extension).unwrap_or(true)
    }

    pub(crate) fn effective_memory(&self) -> Option<MemoryConfig> {
        phi_sdk::config::RuntimeConfig::from(self).extensions.memory
    }

    pub(crate) fn hooks_enabled(&self) -> bool {
        self.hooks
            .as_ref()
            .and_then(|hooks| hooks.enabled)
            .unwrap_or(true)
    }

    pub fn mcp_enabled(&self) -> bool {
        self.builtin_enabled(BuiltinExtension::Mcp) && self.mcp.unwrap_or(true)
    }

    pub fn default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    pub fn default_switch_config(&self) -> SwitchConfig {
        SwitchSettings::from(self).default_switch_config()
    }

    pub fn custom_switches(&self) -> Vec<SwitchConfig> {
        self.switches.clone().unwrap_or_default()
    }

    #[cfg(test)]
    pub fn edit_switch<R>(
        &mut self,
        name: &str,
        edit: impl FnOnce(&Settings, &mut SwitchConfig) -> R,
    ) -> Option<R> {
        let mut settings = SwitchSettings::from(&*self);
        let result = settings.edit_switch(name, |_, switch| edit(self, switch))?;
        self.switches = settings.switches;
        Some(result)
    }

    pub fn switch_list(&self) -> Vec<SwitchConfig> {
        SwitchSettings::from(self).switch_list()
    }

    pub fn switch_by_name(&self, name: &str) -> Option<SwitchConfig> {
        SwitchSettings::from(self).switch_by_name(name)
    }

    pub fn switch_by_name_or_alias(&self, value: &str) -> Option<SwitchConfig> {
        SwitchSettings::from(self).switch_by_name_or_alias(value)
    }

    #[cfg(test)]
    pub fn switch_mode_names(&self) -> Vec<String> {
        SwitchSettings::from(self).switch_mode_names()
    }

    pub fn runtime_config(
        &self,
        catalog: &phi_sdk::models::ModelCatalog,
    ) -> phi_sdk::config::RuntimeConfig {
        phi_sdk::config::RuntimeConfig {
            custom_providers: catalog.custom_providers().into_iter().collect(),
            ..self.into()
        }
    }

    pub fn sandbox_enabled(&self) -> bool {
        self.sandbox
            .as_ref()
            .and_then(|settings| settings.enabled)
            .unwrap_or(true)
    }

    pub fn build_sandbox(
        &self,
        enabled: bool,
        working_directory: PathBuf,
    ) -> anyhow::Result<phi_sdk::sandbox::Sandbox> {
        let (whitelist, blacklist) = self
            .sandbox
            .as_ref()
            .map(|settings| (settings.whitelist.clone(), settings.blacklist.clone()))
            .unwrap_or_default();
        phi_sdk::sandbox::Sandbox::new(enabled, working_directory)?
            .with_path_rules(whitelist, blacklist)
    }

    pub fn resolve_model_context_window(
        &self,
        catalog: &phi_sdk::models::ModelCatalog,
        provider: &str,
        model: &str,
    ) -> anyhow::Result<u64> {
        if provider.is_empty() || model.is_empty() {
            return Ok(self.context_window.unwrap_or(128_000));
        }
        let descriptor = catalog
            .get(provider, model)
            .ok_or_else(|| anyhow::anyhow!("unknown model {provider}/{model}"))?;
        Ok(self.context_window.unwrap_or(descriptor.context_window))
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

    pub fn validation_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(level) = &self.default_thinking_level
            && !VALID_THINKING_LEVELS.contains(&level.as_str())
        {
            warnings.push(format!(
                "invalid config: defaultThinkingLevel = {level:?}; expected one of: {}",
                VALID_THINKING_LEVELS.join(", ")
            ));
        }
        for switch in self.custom_switches() {
            if let Some(level) = &switch.thinking_level
                && !VALID_THINKING_LEVELS.contains(&level.as_str())
            {
                warnings.push(format!(
                    "invalid config: switch {:?} thinking level = {level:?}; expected one of: {}",
                    switch.name,
                    VALID_THINKING_LEVELS.join(", ")
                ));
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, SwitchConfig};

    #[test]
    fn codemode_settings_preserve_options() {
        use phi_sdk::config::RuntimeConfig;
        use serde_json::json;

        assert!(
            !RuntimeConfig::from(&Settings::default())
                .extensions
                .codemode
                .enabled
        );
        for options in [
            json!({}),
            json!({"inlineBudget": 0}),
            json!({"cache": false}),
            json!({"cache": "/tmp/phi-cache"}),
        ] {
            let settings: Settings = serde_json::from_value(json!({"codemode": options})).unwrap();
            let config = RuntimeConfig::from(&settings).extensions.codemode;
            assert_eq!(
                config.inline_budget,
                options["inlineBudget"].as_u64().unwrap_or(3000) as usize
            );
            assert_eq!(
                serde_json::to_value(config.cache).unwrap(),
                options.get("cache").cloned().unwrap_or(json!(true))
            );
        }
    }

    #[test]
    fn feature_settings_require_options_or_explicit_enablement() {
        use serde_json::json;

        for (feature, options) in [
            ("codemode", json!({"inlineBudget": 0})),
            ("memory", json!({"skill": false})),
            ("memory", json!({"injection": false})),
            ("sandbox", json!({"blacklist": []})),
        ] {
            for options in [json!({}), options] {
                for enabled in [None, Some(false), Some(true)] {
                    let expected = enabled.unwrap_or(!options.as_object().unwrap().is_empty());
                    let mut options = options.clone();
                    if let Some(enabled) = enabled {
                        options["enabled"] = json!(enabled);
                    }
                    let settings: Settings =
                        serde_json::from_value(json!({(feature): options})).unwrap();
                    let actual = match feature {
                        "codemode" => settings.codemode.unwrap().enabled,
                        "memory" => settings.memory.unwrap().enabled,
                        "sandbox" => settings.sandbox_enabled(),
                        _ => unreachable!(),
                    };
                    assert_eq!(actual, expected, "{feature}: {options}");
                }
            }
        }
        for options in [json!({}), json!({"onlyGit": false})] {
            for enabled in [None, Some(false), Some(true)] {
                for memory_enabled in [false, true] {
                    let expected = memory_enabled
                        && enabled.unwrap_or(!options.as_object().unwrap().is_empty());
                    let mut options = options.clone();
                    if let Some(enabled) = enabled {
                        options["enabled"] = json!(enabled);
                    }
                    let settings: Settings = serde_json::from_value(json!({
                        "memory": {"enabled": memory_enabled, "tape": options}
                    }))
                    .unwrap();
                    assert_eq!(settings.memory.unwrap().tape_enabled(), expected);
                }
            }
        }
        assert!(Settings::default().sandbox_enabled());
    }

    #[test]
    fn builtin_exclusions_override_internal_flags_and_merge_by_identity() {
        use phi_sdk::extension::BuiltinExtension;
        let global: Settings = serde_json::from_value(serde_json::json!({
            "extensions": ["extensions/custom.lua", "-builtin:memory", "-builtin:mcp", "-builtin:codemode"],
            "memory": {"enabled":true}, "mcp": true, "codemode": {"enabled":true}
        })).unwrap();
        let merged = global.clone().merge_project(
            serde_json::from_value(serde_json::json!({
                "extensions": ["extensions/project.lua"],
                "memory": {"enabled":true}, "mcp": true, "codemode": {"enabled":true}
            }))
            .unwrap(),
        );
        for extension in BuiltinExtension::ALL {
            assert!(!merged.builtin_enabled(extension));
        }
        assert!(!merged.mcp_enabled());
        assert!(
            !phi_sdk::config::RuntimeConfig::from(&merged)
                .extensions
                .memory
                .unwrap()
                .enabled
        );
        assert!(
            !phi_sdk::config::RuntimeConfig::from(&merged)
                .extensions
                .codemode
                .enabled
        );
        assert!(merged.memory.as_ref().unwrap().enabled);
        assert!(merged.codemode.as_ref().unwrap().enabled);
        assert!(
            merged
                .extensions
                .as_ref()
                .unwrap()
                .contains(&"extensions/custom.lua".into())
        );
        for extension in BuiltinExtension::ALL {
            let enabled = global.clone().merge_project(Settings {
                extensions: Some(vec![format!("+{}", extension.path())]),
                ..Default::default()
            });
            for candidate in BuiltinExtension::ALL {
                assert_eq!(enabled.builtin_enabled(candidate), candidate == extension);
            }
        }
        let excluded: Settings = serde_json::from_value(serde_json::json!({
            "extensions": ["!builtin:*", "+builtin:mcp"]
        }))
        .unwrap();
        assert!(excluded.mcp_enabled());
        assert!(!excluded.builtin_enabled(BuiltinExtension::Memory));
    }

    #[test]
    fn mcp_enablement_preserves_explicit_configuration() {
        for global in [None, Some(false), Some(true)] {
            for project in [None, Some(false), Some(true)] {
                let global: Settings = serde_json::from_value(serde_json::json!({
                    "mcp": global,
                    "mcpServers": {"unused": {"command": "unused"}}
                }))
                .unwrap();
                let project = Settings {
                    mcp: project,
                    ..Settings::default()
                };
                let expected = project.mcp.or(global.mcp).unwrap_or(true);
                let merged = global.merge_project(project);
                assert_eq!(merged.mcp_enabled(), expected);
                assert!(merged.mcp_servers.unwrap().contains_key("unused"));
            }
        }
    }

    #[test]
    fn model_context_uses_explicit_value_then_descriptor() {
        let root = std::env::temp_dir().join(format!("phi-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let user_path = root.join("models.json");
        std::fs::write(
            &user_path,
            br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#,
        )
        .unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("models-store.json"),
            user_paths: vec![user_path],
            offline: false,
        })
        .unwrap();

        assert_eq!(
            Settings::default()
                .resolve_model_context_window(&catalog, "local", "test")
                .unwrap(),
            128_000
        );
        assert_eq!(
            Settings {
                context_window: Some(42_000),
                ..Settings::default()
            }
            .resolve_model_context_window(&catalog, "local", "test")
            .unwrap(),
            42_000
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_and_edits_switches() {
        let mut settings = Settings {
            switches: Some(vec![
                SwitchConfig {
                    name: String::from("fast"),
                    alias: Some(String::from("plan")),
                    ..Default::default()
                },
                SwitchConfig {
                    name: String::from("review"),
                    alias: Some(String::from("1")),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        assert_eq!(
            settings.switch_by_name_or_alias("1").unwrap().name,
            "review"
        );
        assert_eq!(
            settings.switch_by_name_or_alias("plan").unwrap().name,
            "plan"
        );

        settings
            .edit_switch("review", |_, switch| switch.alias = Some(String::from("r")))
            .unwrap();
        settings
            .edit_switch("plan", |_, switch| switch.alias = Some(String::from("p")))
            .unwrap();

        let switches = settings.custom_switches();
        assert_eq!(switches.len(), 3);
        assert_eq!(switches[1].alias.as_deref(), Some("r"));
        assert!(switches[1].provider.is_none());
        assert_eq!(switches[2].name, "plan");
        assert_eq!(switches[2].alias.as_deref(), Some("p"));
        assert!(settings.edit_switch("missing", |_, _| ()).is_none());
        let inherited = settings.clone().merge_project(Settings::default());
        assert_eq!(
            inherited.switch_by_name("review").unwrap().alias.as_deref(),
            Some("r")
        );
        let scoped = settings.merge_project(Settings {
            switches: Some(vec![SwitchConfig {
                name: "default".into(),
                provider: Some("child-provider".into()),
                model: Some("child-model".into()),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let selected = scoped.switch_by_name("default").unwrap();
        assert_eq!(selected.provider.as_deref(), Some("child-provider"));
        assert_eq!(selected.model.as_deref(), Some("child-model"));
    }

    #[test]
    fn switch_policy_preserves_builtin_overrides_and_prompt_inheritance() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "defaultProvider": "seed-provider", "defaultModel": "seed-model",
            "defaultThinkingLevel": "high", "switches": [
                {"name": "plan", "provider": "plan-provider", "mode": "custom", "prompt": "plan-prompt"},
                {"name": "review", "mode": "plan"},
                {"name": "plain"},
                {"name": "custom-source", "mode": "custom", "prompt": "custom-prompt"},
                {"name": "custom-target", "mode": "custom"}
            ]
        })).unwrap();
        let plan = settings.switch_by_name("plan").unwrap();
        assert_eq!(plan.mode.as_deref(), Some("plan"));
        assert_eq!(plan.provider.as_deref(), Some("plan-provider"));
        let review = settings.switch_by_name("review").unwrap();
        assert_eq!(review.provider.as_deref(), Some("seed-provider"));
        assert_eq!(review.model.as_deref(), Some("seed-model"));
        assert_eq!(review.thinking_level.as_deref(), Some("high"));
        assert_eq!(review.prompt.as_deref(), Some("plan-prompt"));
        let plain = settings.switch_by_name("plain").unwrap();
        assert!(plain.provider.is_none() && plain.model.is_none() && plain.prompt.is_none());
        assert_eq!(
            settings
                .switch_by_name("custom-target")
                .unwrap()
                .prompt
                .as_deref(),
            Some("plan-prompt")
        );
        assert_eq!(
            settings.switch_mode_names(),
            ["none", "default", "plan", "custom"]
        );
    }

    #[test]
    fn settings_do_not_accept_legacy_models_list() {
        assert!(serde_json::from_str::<Settings>(r#"{"models":["test"]}"#).is_err());
    }

    #[test]
    fn remote_file_trust_is_explicit_and_global_only() {
        let trusted: Settings = serde_json::from_str(r#"{"trust_remote":true}"#).unwrap();
        assert!(!Settings::default().trust_remote);
        assert!(
            trusted
                .clone()
                .merge_project(Settings::default())
                .trust_remote
        );
        assert!(!Settings::default().merge_project(trusted).trust_remote);
        assert!(serde_json::from_str::<Settings>(r#"{"trust_remote":"true"}"#).is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"trust_remote":["memory"]}"#).is_err());
    }

    #[test]
    fn parses_sandbox_object_config() {
        let enabled: Settings = serde_json::from_str(r#"{"sandbox":{"blacklist":[]}}"#).unwrap();
        let disabled: Settings = serde_json::from_str(r#"{"sandbox":{"enabled":false}}"#).unwrap();

        assert!(enabled.sandbox_enabled());
        assert!(!disabled.sandbox_enabled());
        assert!(serde_json::from_str::<Settings>(r#"{"sandbox":false}"#).is_err());
    }
}
