//! Application configuration schema, validation, merging and persisted settings access.
//! Application runtime owns preparation/commit; adapters retain only configuration edit drafts.

use std::collections::HashMap;
use std::sync::Arc;

mod manager;
mod schema;
pub(crate) mod settings;
mod switches;

pub(crate) use manager::SettingsManager;
pub(crate) use settings::SettingsLock;
pub(crate) use switches::SwitchSettings;

pub use schema::*;

#[derive(Clone)]
pub(crate) struct McpAuthDiagnostics {
    servers: Arc<HashMap<String, bool>>,
}

impl McpAuthDiagnostics {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            servers: Arc::new(
                settings
                    .mcp_servers
                    .iter()
                    .flat_map(|servers| servers.iter())
                    .map(|(name, definition)| {
                        (
                            name.clone(),
                            matches!(
                                definition,
                                phi_sdk::extension::mcp::config::McpServerConfig::Url {
                                    auth: Some(phi_sdk::extension::mcp::config::McpAuth::Oauth),
                                    ..
                                }
                            ),
                        )
                    })
                    .collect(),
            ),
        }
    }

    pub(crate) fn diagnostic(&self, server_name: &str) -> String {
        match self.servers.get(server_name) {
            None => format!("MCP server \"{server_name}\" not found in config"),
            Some(false) => {
                format!("MCP server \"{server_name}\" does not use OAuth authentication")
            }
            Some(true) => format!(
                "MCP OAuth is not implemented for \"{server_name}\"; configure explicit authorization headers or a bearer token locally, then run /reload"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_auth_diagnostics_keep_startup_facts_without_exposing_definitions() {
        let mut settings: Settings = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "oauth": {"url": "http://localhost/fixture", "auth": "oauth", "headers": {"Authorization": "fixture-secret"}},
                "plain": {"command": "unused"}
            }
        })).unwrap();
        let diagnostics = McpAuthDiagnostics::new(&settings);
        {
            assert!(
                diagnostics
                    .diagnostic("oauth")
                    .contains("MCP OAuth is not implemented")
            );
            assert_eq!(
                diagnostics.diagnostic("plain"),
                "MCP server \"plain\" does not use OAuth authentication"
            );
            assert_eq!(
                diagnostics.diagnostic("missing"),
                "MCP server \"missing\" not found in config"
            );
        }
        let initial = diagnostics.diagnostic("oauth");
        assert!(!initial.contains("fixture-secret"));
        assert!(!initial.contains("localhost"));
        settings.mcp_servers = None;
        assert_eq!(diagnostics.diagnostic("oauth"), initial);
    }
}
