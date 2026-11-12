use crate::tools::ToolExposure;
use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpLifecycle {
    #[default]
    Lazy,
    Eager,
    KeepAlive,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpToolPrefix {
    #[default]
    Server,
    Short,
    None,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum McpAuth {
    Oauth,
    Bearer,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpRuntimeConfig {
    #[serde(default)]
    pub lifecycle: McpLifecycle,
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl Default for McpRuntimeConfig {
    fn default() -> Self {
        Self {
            lifecycle: McpLifecycle::Lazy,
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

impl McpRuntimeConfig {
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    #[serde(rename = "toolPrefix")]
    pub tool_prefix: McpToolPrefix,
    #[serde(rename = "idleTimeout")]
    pub idle_timeout_minutes: u64,
    pub exposure: ToolExposure,
    #[serde(rename = "toolExposure")]
    pub tool_exposure: HashMap<String, ToolExposure>,
    #[serde(rename = "showStatus")]
    pub show_status: bool,
    pub notify: bool,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            tool_prefix: McpToolPrefix::Server,
            idle_timeout_minutes: 10,
            exposure: ToolExposure::Deferred,
            tool_exposure: HashMap::new(),
            show_status: false,
            notify: false,
        }
    }
}

impl McpSettings {
    pub fn idle_timeout_secs(&self) -> u64 {
        self.idle_timeout_minutes * 60
    }
}

fn default_idle_timeout_secs() -> u64 {
    600
}

fn idle_timeout_minutes_to_secs<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer).map(|value| value.map(|minutes| minutes * 60))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum McpServerConfig {
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        lifecycle: Option<McpLifecycle>,
        #[serde(
            default,
            rename = "idleTimeout",
            deserialize_with = "idle_timeout_minutes_to_secs"
        )]
        idle_timeout_secs: Option<u64>,
        #[serde(default)]
        runtime: McpRuntimeConfig,
        #[serde(default)]
        exposure: Option<ToolExposure>,
        #[serde(default, rename = "toolExposure")]
        tool_exposure: HashMap<String, ToolExposure>,
        #[serde(default, rename = "exposeResources")]
        expose_resources: Option<bool>,
        #[serde(default)]
        debug: bool,
    },
    Url {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        auth: Option<McpAuth>,
        #[serde(default, rename = "bearerToken")]
        bearer_token: Option<String>,
        #[serde(default, rename = "bearerTokenEnv")]
        bearer_token_env: Option<String>,
        #[serde(default)]
        lifecycle: Option<McpLifecycle>,
        #[serde(
            default,
            rename = "idleTimeout",
            deserialize_with = "idle_timeout_minutes_to_secs"
        )]
        idle_timeout_secs: Option<u64>,
        #[serde(default)]
        runtime: McpRuntimeConfig,
        #[serde(default)]
        exposure: Option<ToolExposure>,
        #[serde(default, rename = "toolExposure")]
        tool_exposure: HashMap<String, ToolExposure>,
        #[serde(default, rename = "exposeResources")]
        expose_resources: Option<bool>,
        #[serde(default)]
        debug: bool,
    },
}

impl McpServerConfig {
    pub fn runtime(&self) -> &McpRuntimeConfig {
        match self {
            Self::Command { runtime, .. } | Self::Url { runtime, .. } => runtime,
        }
    }

    pub fn effective_runtime(&self) -> McpRuntimeConfig {
        match self {
            Self::Command {
                runtime,
                lifecycle,
                idle_timeout_secs,
                ..
            }
            | Self::Url {
                runtime,
                lifecycle,
                idle_timeout_secs,
                ..
            } => McpRuntimeConfig {
                lifecycle: lifecycle.unwrap_or(runtime.lifecycle),
                idle_timeout_secs: idle_timeout_secs.unwrap_or(runtime.idle_timeout_secs),
            },
        }
    }

    pub fn tool_exposure(&self, settings: &McpSettings, tool_name: &str) -> ToolExposure {
        let (exposure, tools) = match self {
            Self::Command {
                exposure,
                tool_exposure,
                ..
            }
            | Self::Url {
                exposure,
                tool_exposure,
                ..
            } => (*exposure, tool_exposure),
        };
        tools
            .get(tool_name)
            .copied()
            .or(exposure)
            .or_else(|| settings.tool_exposure.get(tool_name).copied())
            .unwrap_or(settings.exposure)
    }

    pub(crate) fn has_exposure(&self, settings: &McpSettings, requested: ToolExposure) -> bool {
        let (exposure, tools) = match self {
            Self::Command {
                exposure,
                tool_exposure,
                ..
            }
            | Self::Url {
                exposure,
                tool_exposure,
                ..
            } => (*exposure, tool_exposure),
        };
        exposure.unwrap_or(settings.exposure) == requested
            || tools
                .keys()
                .chain(settings.tool_exposure.keys())
                .any(|name| self.tool_exposure(settings, name) == requested)
    }

    pub fn debug(&self) -> bool {
        match self {
            Self::Command { debug, .. } | Self::Url { debug, .. } => *debug,
        }
    }

    pub fn expose_resources(&self) -> bool {
        match self {
            Self::Command {
                expose_resources, ..
            }
            | Self::Url {
                expose_resources, ..
            } => expose_resources.unwrap_or(true),
        }
    }

    pub(crate) fn metadata_cache_persistable(&self) -> bool {
        match self {
            Self::Command { env, .. } => env.is_empty(),
            Self::Url {
                headers,
                auth,
                bearer_token,
                bearer_token_env,
                ..
            } => {
                headers.is_empty()
                    && auth.is_none()
                    && bearer_token.is_none()
                    && bearer_token_env.is_none()
            }
        }
    }

    pub fn needs_oauth_token(&self) -> bool {
        match self {
            Self::Url {
                auth: Some(McpAuth::Oauth),
                headers,
                bearer_token,
                bearer_token_env,
                ..
            } => {
                !headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
                    && bearer_token.is_none()
                    && bearer_token_env.is_none()
            }
            _ => false,
        }
    }

    pub fn with_settings_defaults(&self, settings: &McpSettings) -> Self {
        let mut config = self.clone();
        let settings_idle_timeout_secs = settings.idle_timeout_secs();
        match &mut config {
            Self::Command {
                runtime,
                lifecycle,
                idle_timeout_secs,
                ..
            }
            | Self::Url {
                runtime,
                lifecycle,
                idle_timeout_secs,
                ..
            } => {
                runtime.idle_timeout_secs = match lifecycle.unwrap_or(runtime.lifecycle) {
                    McpLifecycle::Lazy => settings_idle_timeout_secs,
                    McpLifecycle::Eager | McpLifecycle::KeepAlive => idle_timeout_secs.unwrap_or(0),
                };
            }
        }
        config
    }
}

pub fn format_tool_name(tool_name: &str, server_name: &str, prefix: McpToolPrefix) -> String {
    let server_prefix = tool_server_prefix(server_name, prefix);
    if server_prefix.is_empty() {
        tool_name.to_string()
    } else {
        format!("{server_prefix}_{tool_name}")
    }
}

pub fn format_tool_display_name(
    tool_name: &str,
    server_name: &str,
    prefix: McpToolPrefix,
) -> String {
    let server_prefix = tool_server_prefix(server_name, prefix);
    if server_prefix.is_empty() {
        tool_name.to_string()
    } else {
        format!("{server_prefix}: {tool_name}")
    }
}

fn tool_server_prefix(server_name: &str, prefix: McpToolPrefix) -> String {
    match prefix {
        McpToolPrefix::None => String::new(),
        McpToolPrefix::Short => {
            let short = server_name
                .trim_end_matches("-mcp")
                .trim_end_matches("_mcp")
                .replace('-', "_");
            if short.is_empty() {
                "mcp".to_string()
            } else {
                short
            }
        }
        McpToolPrefix::Server => server_name.replace('-', "_"),
    }
}

#[cfg(test)]
mod exposure_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retired_fields_are_rejected_at_every_config_boundary() {
        for (name, value) in [
            ("directTools", json!(true)),
            ("disabledTools", json!(["read"])),
        ] {
            assert!(serde_json::from_value::<McpSettings>(json!({name: value})).is_err());
            for mut server in [
                json!({"command":"fixture"}),
                json!({"url":"https://fixture"}),
            ] {
                server[name] = value.clone();
                assert!(serde_json::from_value::<McpServerConfig>(server).is_err());
            }
        }
    }

    fn exposure_strategy() -> impl proptest::strategy::Strategy<Value = ToolExposure> {
        proptest::sample::select(vec![
            ToolExposure::Direct,
            ToolExposure::Deferred,
            ToolExposure::Hidden,
        ])
    }

    proptest::proptest! {
        #[test]
        fn exposure_precedence_is_exact_and_transport_independent(
            global_default in exposure_strategy(),
            global_tool in proptest::option::of(exposure_strategy()),
            server_default in proptest::option::of(exposure_strategy()),
            server_tool in proptest::option::of(exposure_strategy()),
        ) {
            let mut settings = McpSettings { exposure: global_default, ..Default::default() };
            if let Some(exposure) = global_tool {
                settings.tool_exposure.insert("target".into(), exposure);
            }
            let fallback = server_default.unwrap_or(global_default);
            let expected = server_tool.or(server_default).or(global_tool).unwrap_or(global_default);
            for mut input in [json!({"command":"fixture"}), json!({"url":"https://fixture"})] {
                input["exposure"] = json!(server_default);
                if let Some(exposure) = server_tool {
                    input["toolExposure"] = json!({"target":exposure});
                }
                let config: McpServerConfig = serde_json::from_value(input).unwrap();
                proptest::prop_assert_eq!(config.tool_exposure(&settings, "target"), expected);
                proptest::prop_assert_eq!(config.tool_exposure(&settings, "TARGET"), fallback);
                for requested in [ToolExposure::Direct, ToolExposure::Deferred, ToolExposure::Hidden] {
                    proptest::prop_assert_eq!(config.has_exposure(&settings, requested), fallback == requested || expected == requested);
                }
            }
        }
    }
}
