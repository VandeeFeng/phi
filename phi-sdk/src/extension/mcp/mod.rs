use crate::core::interaction::McpElicitationSender;
pub use crate::core::interaction::{McpElicitationRequest, McpElicitationResponse};
use crate::extension::{ExtensionId, ExtensionToolRegistration};
use crate::permission::runtime::PermissionRuntime;
use crate::tools::{ToolAuthority, ToolAuthorization, ToolExposure, ToolMetadata};
use cache::McpMetadataCache;
use config::{McpLifecycle, McpServerConfig, McpSettings};
use connection::{
    McpConnectAttempt, McpConnectStart, McpConnection, McpConnectionLease, McpConnectionPool,
    McpConnectionTasks, RemovedConnection,
};
use futures::StreamExt;
use metadata::{
    ConnectionGeneration, McpMetadataAccess, McpMetadataCatalog, McpMetadataSnapshot,
    McpMetadataSource as RuntimeMetadataSource,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::JoinHandle;
use tool::McpRegisteredTool;

pub mod cache;
pub mod client;
pub mod config;
mod connection;
pub(crate) mod elicitation;
mod metadata;
pub mod tool;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpTransportPolicy {
    #[default]
    All,
    CommandOnly,
}

impl McpTransportPolicy {
    fn allows(self, config: &McpServerConfig) -> bool {
        self == Self::All || matches!(config, McpServerConfig::Command { .. })
    }
}

/// Comparable MCP tool declarations, independent of connection status and cache freshness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolContracts(
    std::collections::BTreeMap<String, (ToolExposure, String, serde_json::Value)>,
);

impl McpToolContracts {
    /// Rejects Agent/Prompt settings targeting individual MCP tools in this generation.
    pub fn validate_tool_rule(
        &self,
        rule: &crate::tools::ToolRule,
    ) -> Result<(), crate::tools::ToolRuleError> {
        if self.0.contains_key(&rule.tool) {
            return Err(crate::tools::ToolRuleError::UnsupportedMcp);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct McpClientManager {
    inner: Arc<McpRuntimeInner>,
    interaction_tx: Option<McpElicitationSender>,
    tool_specs: Option<Arc<Vec<McpToolSpec>>>,
}

struct McpRuntimeInner {
    configs: HashMap<String, McpServerConfig>,
    settings: McpSettings,
    sandbox: Option<crate::sandbox::Sandbox>,
    transport_policy: McpTransportPolicy,
    connections: McpConnectionPool,
    failures: Mutex<HashMap<String, Instant>>,
    metadata: Mutex<McpMetadataCatalog>,
    capabilities: std::sync::RwLock<HashMap<String, McpCapabilityContract>>,
    event_tx: broadcast::Sender<McpRuntimeEvent>,
    elicitation: elicitation::McpElicitationRouter,
    maintenance_started: AtomicBool,
    background_started: AtomicBool,
}

struct PreparedConnection {
    connection: McpConnection,
    start_tasks: watch::Sender<bool>,
    config: McpServerConfig,
    tools: client::McpListed<rmcp::model::Tool>,
    resources: client::McpListed<rmcp::model::Resource>,
    prompts: client::McpListed<rmcp::model::Prompt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpRuntimeEvent {
    Connected {
        server: String,
    },
    Disconnected {
        server: String,
        reason: String,
    },
    CacheUpdated {
        server: String,
        tool_count: usize,
    },
    MetadataUpdated {
        server: String,
        kind: McpMetadataKind,
    },
    ToolCatalogChanged {
        server: String,
    },
    ConnectionFailed {
        server: String,
        error: String,
    },
    MetadataRefreshFailed {
        server: String,
        kind: McpMetadataKind,
        error: String,
    },
    SubscriptionFailed {
        server: String,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpMetadataKind {
    Tools,
    Resources,
    Prompts,
}

#[derive(Debug, Clone)]
pub struct McpServerSnapshot {
    pub name: String,
    pub status: McpServerStatus,
    pub metadata_source: Option<McpMetadataSource>,
    pub tools: Vec<McpToolSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMetadataSource {
    Persisted,
    Connection,
}

#[derive(Debug, Clone)]
pub struct McpToolSnapshot {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub exposure: ToolExposure,
}

#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub connected: bool,
    pub state: McpConnectionState,
    pub lifecycle: McpLifecycle,
    pub idle_timeout_secs: u64,
    pub tool_count: usize,
    pub cached: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpConnectionState {
    Blocked,
    Connected,
    Idle,
    Failed,
    NeedsAuth,
    Connecting,
}

#[derive(Clone)]
struct McpToolSpec {
    server_name: String,
    tool_name: String,
    registered_name: String,
    display_name: String,
    description: String,
    input_schema: serde_json::Value,
    exposure: ToolExposure,
}

#[derive(PartialEq, Eq)]
struct McpCapabilityContract {
    description: String,
    input_schema: serde_json::Value,
}

const MCP_CAPABILITY_SEPARATOR: char = '\0';
const STARTUP_CONNECTION_CONCURRENCY: usize = 10;

fn mcp_tool_capability(server_name: &str, tool_name: &str) -> String {
    format!("{server_name}{MCP_CAPABILITY_SEPARATOR}{tool_name}")
}

fn parse_mcp_tool_capability(capability: &str) -> Option<(&str, &str)> {
    capability.split_once(MCP_CAPABILITY_SEPARATOR)
}

impl ToolAuthority for McpClientManager {
    fn is_available(&self, capability: &str) -> bool {
        self.inner
            .capabilities
            .read()
            .is_ok_and(|capabilities| capabilities.contains_key(capability))
            && parse_mcp_tool_capability(capability)
                .is_some_and(|(server, tool)| self.is_tool_enabled(server, tool))
    }

    fn ensure_available(&self, capability: &str) -> Result<(), String> {
        let (server_name, tool_name) = parse_mcp_tool_capability(capability)
            .ok_or_else(|| "invalid MCP tool capability".to_string())?;
        self.ensure_tool_enabled(server_name, tool_name)
            .map_err(|error| error.to_string())
    }
}

fn builtin_tool_names() -> HashSet<String> {
    [
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "glob",
        "agent",
        "mcp",
        "toolsearch",
        "tool_search",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn snapshot_tools(
    config: &McpServerConfig,
    settings: &McpSettings,
    snapshot: McpMetadataSnapshot,
) -> Vec<McpToolSnapshot> {
    let mut tools = snapshot.tools;
    tools.extend(resource_tools_from_resources(snapshot.resources));
    tools
        .into_iter()
        .map(|tool| {
            let name = tool.name.to_string();
            McpToolSnapshot {
                exposure: config.tool_exposure(settings, &name),
                description: tool.description.unwrap_or_default().to_string(),
                input_schema: serde_json::to_value(&tool.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
                name,
            }
        })
        .collect()
}

fn resource_tools_from_resources(resources: Vec<rmcp::model::Resource>) -> Vec<rmcp::model::Tool> {
    resources
        .into_iter()
        .map(|resource| {
            let mut tool = rmcp::model::Tool::default();
            tool.name = resource_tool_name(&resource.name).into();
            tool.title = resource.title.clone();
            tool.description = Some(
                resource
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Read resource: {}", resource.uri))
                    .into(),
            );
            tool.input_schema = std::sync::Arc::new(serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::json!("object"),
            )]));
            tool
        })
        .collect()
}

fn metadata_subscription_filter(
    capabilities: &rmcp::model::ServerCapabilities,
    expose_resources: bool,
) -> rmcp::model::SubscriptionFilter {
    let mut filter = rmcp::model::SubscriptionFilter::builder()
        .tools_list_changed()
        .prompts_list_changed();
    if expose_resources {
        filter = filter.resources_list_changed();
    }
    filter.build().supported_by(capabilities)
}

fn resource_tool_name(name: &str) -> String {
    let mut output = String::from("get_");
    let mut last_was_sep = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            output.push('_');
            last_was_sep = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    if output == "get" || output == "get_" {
        "get_resource".to_string()
    } else {
        output
    }
}

async fn wait_for_connection_commit(start: &mut watch::Receiver<bool>) -> bool {
    if *start.borrow() {
        return true;
    }
    start.changed().await.is_ok() && *start.borrow()
}

impl McpClientManager {
    pub async fn connect_all(configs: &HashMap<String, McpServerConfig>) -> Self {
        Self::connect_all_with_settings(configs, McpSettings::default()).await
    }

    pub async fn connect_all_with_settings(
        configs: &HashMap<String, McpServerConfig>,
        settings: McpSettings,
    ) -> Self {
        let manager = Self::load_with_settings(configs, settings);
        manager.start().await;
        manager
    }

    pub fn load_with_settings(
        configs: &HashMap<String, McpServerConfig>,
        settings: McpSettings,
    ) -> Self {
        Self::load_with_environment(configs, settings, None, McpTransportPolicy::All)
    }

    /// Reuses connections only when the complete execution environment matches.
    pub fn with_sandbox(self, sandbox: crate::sandbox::Sandbox) -> Self {
        let policy = self.inner.transport_policy;
        self.with_execution_environment(sandbox, policy)
    }

    pub fn with_execution_environment(
        self,
        sandbox: crate::sandbox::Sandbox,
        transport_policy: McpTransportPolicy,
    ) -> Self {
        if self.inner.sandbox.as_ref() == Some(&sandbox)
            && self.inner.transport_policy == transport_policy
        {
            return self;
        }
        let mut manager = Self::load_with_environment(
            &self.inner.configs,
            self.inner.settings.clone(),
            Some(sandbox),
            transport_policy,
        );
        manager.interaction_tx = self.interaction_tx;
        manager
    }

    fn load_with_environment(
        configs: &HashMap<String, McpServerConfig>,
        settings: McpSettings,
        sandbox: Option<crate::sandbox::Sandbox>,
        transport_policy: McpTransportPolicy,
    ) -> Self {
        let configs = configs
            .iter()
            .map(|(name, config)| (name.clone(), config.with_settings_defaults(&settings)))
            .collect::<HashMap<_, _>>();
        let cache = McpMetadataCache::load(&configs);
        Self {
            inner: Arc::new(McpRuntimeInner {
                configs,
                settings,
                sandbox,
                transport_policy,
                connections: McpConnectionPool::new(),
                failures: Mutex::new(HashMap::new()),
                metadata: Mutex::new(McpMetadataCatalog::new(cache)),
                capabilities: std::sync::RwLock::new(HashMap::new()),
                event_tx: broadcast::channel(256).0,
                elicitation: elicitation::McpElicitationRouter::default(),
                maintenance_started: AtomicBool::new(false),
                background_started: AtomicBool::new(false),
            }),
            interaction_tx: None,
            tool_specs: None,
        }
    }

    /// Starts pooled connection/metadata work once, independently of any observing client.
    pub fn start_background(&self) {
        if self.inner.background_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            let mut servers = manager.tool_refresh_candidates().await;
            servers.extend(manager.startup_connection_candidates());
            servers.sort();
            servers.dedup();
            let mut tasks = tokio::task::JoinSet::new();
            let concurrency = Arc::new(tokio::sync::Semaphore::new(STARTUP_CONNECTION_CONCURRENCY));
            for server in servers {
                let manager = manager.clone();
                let concurrency = concurrency.clone();
                tasks.spawn(async move {
                    let Ok(_permit) = concurrency.acquire_owned().await else {
                        return;
                    };
                    let _ = manager.connect(&server).await;
                });
            }
            while tasks.join_next().await.is_some() {}
        });
        self.start_maintenance();
    }

    /// Freezes candidate declarations without freezing pooled connection state or tool authority.
    pub async fn snapshot_tool_contracts(&self) -> (Self, McpToolContracts) {
        let specs = match &self.tool_specs {
            Some(specs) => specs.as_ref().clone(),
            None => self.tool_specs().await,
        };
        let contracts = Self::contracts_for(&specs);
        let mut manager = self.clone();
        manager.tool_specs = Some(Arc::new(specs));
        (manager, contracts)
    }

    fn contracts_for(specs: &[McpToolSpec]) -> McpToolContracts {
        McpToolContracts(
            specs
                .iter()
                .map(|tool| {
                    (
                        tool.registered_name.clone(),
                        (
                            tool.exposure,
                            tool.description.clone(),
                            tool.input_schema.clone(),
                        ),
                    )
                })
                .collect(),
        )
    }

    pub async fn tool_contracts(&self) -> McpToolContracts {
        Self::contracts_for(&self.tool_specs().await)
    }

    pub async fn start(&self) {
        self.connect_startup_servers().await;
        self.start_maintenance();
    }

    async fn connect_startup_servers(&self) {
        for name in self.startup_connection_candidates() {
            if let Err(error) = self.connect(&name).await {
                tracing::warn!(target: "mcp", "Failed to connect to MCP server '{}': {error}", name);
            }
        }
    }

    pub fn start_maintenance(&self) {
        if self.inner.maintenance_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.spawn_maintenance_loop();
    }

    pub fn subscribe(&self) -> broadcast::Receiver<McpRuntimeEvent> {
        self.inner.event_tx.subscribe()
    }

    pub(crate) fn with_interaction_sender(mut self, sender: Option<McpElicitationSender>) -> Self {
        self.interaction_tx = sender;
        self
    }

    pub async fn mcp_tool_display_names(&self) -> HashMap<String, String> {
        self.tool_specs()
            .await
            .into_iter()
            .map(|tool| (tool.registered_name, tool.display_name))
            .collect()
    }

    pub fn startup_connection_candidates(&self) -> Vec<String> {
        let mut servers = self
            .inner
            .configs
            .iter()
            .filter(|(_, config)| {
                self.inner.transport_policy.allows(config)
                    && matches!(
                        config.effective_runtime().lifecycle,
                        McpLifecycle::Eager | McpLifecycle::KeepAlive
                    )
            })
            .map(|(server, _)| server.clone())
            .collect::<Vec<_>>();
        servers.sort();
        servers
    }

    pub async fn tool_refresh_candidates(&self) -> Vec<String> {
        let generations = self.inner.connections.generations().await;
        let metadata = self.inner.metadata.lock().await;
        let mut servers = self
            .inner
            .configs
            .iter()
            .filter(|(server, config)| {
                self.inner.transport_policy.allows(config)
                    && config.has_exposure(&self.inner.settings, ToolExposure::Direct)
                    && !metadata.has_reusable_direct_metadata(
                        server,
                        config,
                        generations.get(*server).copied(),
                    )
            })
            .map(|(server, _)| server.clone())
            .collect::<Vec<_>>();
        servers.sort();
        servers
    }

    pub fn infer_mcp_tool_display_name(&self, registered_name: &str) -> Option<String> {
        let mut servers = self.inner.configs.iter().collect::<Vec<_>>();
        servers.sort_by_key(|(server_name, _)| std::cmp::Reverse(server_name.len()));
        for (server_name, _config) in servers {
            let prefix = config::format_tool_name("", server_name, self.inner.settings.tool_prefix);
            let Some(tool_name) = registered_name.strip_prefix(&prefix) else {
                continue;
            };
            if tool_name.is_empty() {
                continue;
            }
            return Some(config::format_tool_display_name(
                tool_name,
                server_name,
                self.inner.settings.tool_prefix,
            ));
        }
        None
    }

    pub async fn collect_tools(&self) -> Vec<rig::tool::DynamicTool> {
        self.tool_registrations(Some(PermissionRuntime::open()))
            .await
            .into_iter()
            .filter(|registration| registration.metadata.runtime.exposure == ToolExposure::Direct)
            .map(|registration| registration.tool)
            .collect()
    }

    pub(crate) async fn tool_registrations(
        &self,
        permission: Option<PermissionRuntime>,
    ) -> Vec<ExtensionToolRegistration> {
        let specs = match &self.tool_specs {
            Some(specs) => specs.as_ref().clone(),
            None => self.tool_specs().await,
        };
        let authority: Arc<dyn ToolAuthority> = Arc::new(self.clone());
        specs
            .into_iter()
            .map(|tool| {
                let capability = mcp_tool_capability(&tool.server_name, &tool.tool_name);
                let namespace = tool.server_name.clone();
                let dynamic_tool = McpRegisteredTool {
                    manager: self.clone(),
                    permission: permission.clone(),
                    server_name: tool.server_name,
                    tool_name: tool.tool_name,
                    registered_name: tool.registered_name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                }
                .into_dynamic_tool();
                let mut registration = ExtensionToolRegistration::delegated(
                    ExtensionId::mcp(),
                    dynamic_tool,
                    capability,
                    authority.clone(),
                )
                .with_metadata(
                    ToolMetadata::default()
                        .with_exposure(tool.exposure)
                        .with_authorization(ToolAuthorization::McpOperation),
                );
                registration.namespace = Some(namespace);
                registration
            })
            .collect()
    }

    pub(crate) async fn deferred_registrations(
        &self,
        permission: PermissionRuntime,
    ) -> Vec<ExtensionToolRegistration> {
        let mut live = self.clone();
        live.tool_specs = None;
        futures::stream::iter(self.inner.configs.iter().filter(|(_, config)| {
            self.inner.transport_policy.allows(config)
                && config.has_exposure(&self.inner.settings, ToolExposure::Deferred)
        }))
        .for_each_concurrent(STARTUP_CONNECTION_CONCURRENCY, |(server, _)| {
            let manager = &live;
            async move {
                if let Err(error) = manager.metadata_for_server(server).await {
                    tracing::warn!(target: "mcp", "{server} deferred discovery failed: {error}");
                }
            }
        })
        .await;
        live.tool_registrations(Some(permission))
            .await
            .into_iter()
            .filter(|registration| registration.metadata.runtime.exposure == ToolExposure::Deferred)
            .collect()
    }

    pub async fn server_snapshots(&self) -> Vec<McpServerSnapshot> {
        let statuses = self
            .status_entries()
            .await
            .into_iter()
            .map(|status| (status.name.clone(), status))
            .collect::<HashMap<_, _>>();
        let mut servers = Vec::new();
        for (name, config) in &self.inner.configs {
            let snapshot = self.current_metadata_for_server(name).await.ok().flatten();
            let metadata_source = snapshot.as_ref().map(|snapshot| match snapshot.source {
                RuntimeMetadataSource::Persisted => McpMetadataSource::Persisted,
                RuntimeMetadataSource::Connection(_) => McpMetadataSource::Connection,
            });
            let tools = snapshot
                .map(|snapshot| snapshot_tools(config, &self.inner.settings, snapshot))
                .unwrap_or_default();
            if let Some(status) = statuses.get(name) {
                servers.push(McpServerSnapshot {
                    name: name.clone(),
                    status: status.clone(),
                    metadata_source,
                    tools,
                });
            }
        }
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        servers
    }

    pub async fn status(&self) -> serde_json::Value {
        let servers = self
            .status_entries()
            .await
            .into_iter()
            .map(|status| {
                serde_json::json!({
                    "name": status.name,
                    "connected": status.connected,
                    "state": status.state,
                    "lifecycle": format!("{:?}", status.lifecycle).to_lowercase(),
                    "idle_timeout_secs": status.idle_timeout_secs,
                    "tool_count": status.tool_count,
                    "cached": status.cached,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "servers": servers })
    }

    pub async fn status_entries(&self) -> Vec<McpServerStatus> {
        self.close_idle_connections().await;
        let connections = self.inner.connections.generations().await;
        let connecting = self.inner.connections.connecting().await;
        let failures = self.inner.failures.lock().await;
        let metadata = self.inner.metadata.lock().await;
        self.inner
            .configs
            .iter()
            .map(|(name, cfg)| {
                let persisted = metadata.snapshot(name, cfg, McpMetadataAccess::Startup);
                let snapshot = connections
                    .get(name)
                    .and_then(|generation| {
                        metadata.snapshot(
                            name,
                            cfg,
                            McpMetadataAccess::ActiveConnection(*generation),
                        )
                    })
                    .or_else(|| persisted.clone());
                let allowed = self.inner.transport_policy.allows(cfg);
                let snapshot = snapshot.filter(|_| allowed);
                let connected = allowed && connections.contains_key(name);
                McpServerStatus {
                    name: name.clone(),
                    connected,
                    state: if !allowed {
                        McpConnectionState::Blocked
                    } else if connected {
                        McpConnectionState::Connected
                    } else if cfg.needs_oauth_token() {
                        McpConnectionState::NeedsAuth
                    } else if connecting.contains(name) {
                        McpConnectionState::Connecting
                    } else if failures.contains_key(name) {
                        McpConnectionState::Failed
                    } else {
                        McpConnectionState::Idle
                    },
                    lifecycle: cfg.effective_runtime().lifecycle,
                    idle_timeout_secs: cfg.effective_runtime().idle_timeout_secs,
                    tool_count: snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.tools.len())
                        .unwrap_or_default(),
                    cached: allowed
                        && (persisted.is_some()
                            || snapshot.as_ref().is_some_and(|snapshot| {
                                matches!(snapshot.source, RuntimeMetadataSource::Persisted)
                            })),
                }
            })
            .collect()
    }

    pub async fn list_tools(&self, server_name: &str) -> anyhow::Result<serde_json::Value> {
        let tools = self.discoverable_tools_for_server(server_name).await?;
        Ok(serde_json::json!({ "server": server_name, "tools": tools }))
    }

    pub async fn search_tools(&self, query: &str) -> anyhow::Result<serde_json::Value> {
        let query = query.to_lowercase();
        let mut matches = Vec::new();
        for (server_name, config) in &self.inner.configs {
            if !self.inner.transport_policy.allows(config) {
                continue;
            }
            for tool in self.discoverable_tools_for_server(server_name).await? {
                let name = tool.name.to_string();
                let description = tool.description.clone().unwrap_or_default().to_string();
                let haystack = format!("{server_name} {name} {description}").to_lowercase();
                if haystack.contains(&query) {
                    matches.push(serde_json::json!({
                        "server": server_name,
                        "name": name,
                        "description": description,
                    }));
                }
            }
        }
        Ok(serde_json::json!({ "query": query, "tools": matches }))
    }

    pub async fn describe_tool(
        &self,
        server_name: &str,
        tool_name: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let tools = self.discoverable_tools_for_server(server_name).await?;
        let tool = tools
            .into_iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| {
                anyhow::anyhow!("Unknown MCP tool '{tool_name}' on server '{server_name}'")
            })?;
        Ok(serde_json::json!({ "server": server_name, "tool": tool }))
    }

    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: Option<rmcp::model::JsonObject>,
    ) -> anyhow::Result<rmcp::model::CallToolResult> {
        self.ensure_tool_enabled(server_name, tool_name)?;
        if let Some(uri) = self.resource_uri_for_tool(server_name, tool_name).await? {
            return self.read_resource(server_name, &uri).await;
        }
        match self
            .call_tool_once(server_name, tool_name, args.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                self.disconnect(server_name, "call_tool_error").await;
                tracing::debug!(target: "mcp", "Retrying MCP tool after reconnect: {server_name}/{tool_name}");
                self.call_tool_once(server_name, tool_name, args)
                    .await
                    .map_err(|retry_error| {
                        anyhow::anyhow!(
                            "MCP tool call failed after reconnect: {retry_error}; original error: {error}"
                        )
                    })
            }
        }
    }

    async fn call_tool_once(
        &self,
        server_name: &str,
        tool_name: &str,
        args: Option<rmcp::model::JsonObject>,
    ) -> anyhow::Result<rmcp::model::CallToolResult> {
        let lease = self.begin_call(server_name).await?;
        let _elicitation = self
            .inner
            .elicitation
            .begin_call(
                server_name,
                self.interaction_tx.clone(),
                crate::core::interaction::active_interaction_scope(),
                crate::core::interaction::active_tool_call_id(),
            )
            .await;
        let params = args
            .map(|a| {
                rmcp::model::CallToolRequestParams::new(tool_name.to_string()).with_arguments(a)
            })
            .unwrap_or_else(|| rmcp::model::CallToolRequestParams::new(tool_name.to_string()));
        let result = lease.peer.call_tool(params).await;
        self.mark_generation_used(server_name, lease.generation)
            .await;
        Ok(result?)
    }

    pub async fn connect(&self, server_name: &str) -> anyhow::Result<()> {
        self.ensure_server_allowed(server_name)?;
        let connect_lock = self.inner.connections.connect_lock(server_name).await;
        let _guard = connect_lock.lock().await;
        let attempt = match self.inner.connections.begin_connect(server_name).await? {
            McpConnectStart::Connected => return Ok(()),
            McpConnectStart::Attempt(attempt) => attempt,
        };
        let prepared = match self.prepare_connection(server_name, attempt).await {
            Ok(prepared) => prepared,
            Err(error) => return self.fail_connect(server_name, attempt, error).await,
        };
        let PreparedConnection {
            connection,
            start_tasks,
            config,
            tools,
            resources,
            prompts,
        } = prepared;
        let generation = connection.generation;
        let tool_count = tools.items.len();
        self.inner
            .connections
            .commit(server_name, attempt, connection)
            .await?;
        if !self.inner.metadata.lock().await.set_active(
            server_name,
            generation,
            &config,
            (tools.items, tools.policy),
            (resources.items, resources.policy),
            (prompts.items, prompts.policy),
        ) {
            self.inner
                .connections
                .remove_generation(server_name, generation, "stale_metadata")
                .await;
            anyhow::bail!("stale MCP connection metadata for '{server_name}'");
        }
        self.inner.failures.lock().await.remove(server_name);
        let catalog_changed = self.refresh_capabilities().await;
        let _ = start_tasks.send(true);
        tracing::info!(target: "mcp", "Connected to MCP server '{}'", server_name);
        self.emit_connected(server_name, tool_count, catalog_changed);
        self.save_metadata(server_name).await;
        Ok(())
    }

    async fn prepare_connection(
        &self,
        server_name: &str,
        attempt: McpConnectAttempt,
    ) -> anyhow::Result<PreparedConnection> {
        let config = self
            .inner
            .configs
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server '{server_name}'"))?;
        let protocol_hint = self
            .inner
            .metadata
            .lock()
            .await
            .protocol_hint(server_name, &config);
        let (change_sender, metadata_changes) = client::metadata_change_channel();
        let handler = client::McpClientHandler::new(
            server_name.to_string(),
            self.inner.elicitation.clone(),
            change_sender.clone(),
            protocol_hint,
        );
        let handle = client::McpClientHandle::connect(
            server_name.to_string(),
            &config,
            handler,
            protocol_hint,
            self.inner.sandbox.as_ref(),
        )
        .await?;
        let peer = handle.peer();
        if let Some(protocol_hint) = client::protocol_hint(&peer) {
            self.inner
                .metadata
                .lock()
                .await
                .set_protocol_hint(server_name, &config, protocol_hint);
        }
        let client::McpListedMetadata {
            tools,
            resources,
            prompts,
        } = client::list_metadata(&peer, server_name, config.expose_resources()).await?;
        let subscription = self.open_subscription(&config, &peer).await?;
        let (start_tasks, task_start) = watch::channel(false);
        let mut tasks = vec![self.spawn_metadata_listener(
            server_name.to_string(),
            attempt.generation,
            metadata_changes,
            task_start.clone(),
        )];
        if let Some(subscription) = subscription {
            tasks.push(self.spawn_subscription(
                server_name.to_string(),
                attempt.generation,
                subscription,
                change_sender,
                task_start,
            ));
        }
        Ok(PreparedConnection {
            connection: McpConnection {
                generation: attempt.generation,
                handle,
                _tasks: McpConnectionTasks(tasks),
                last_used: Instant::now(),
                in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            start_tasks,
            config,
            tools,
            resources,
            prompts,
        })
    }

    async fn fail_connect(
        &self,
        server_name: &str,
        attempt: McpConnectAttempt,
        error: anyhow::Error,
    ) -> anyhow::Result<()> {
        let record_failure = self.inner.connections.fail(server_name, attempt).await;
        self.inner
            .metadata
            .lock()
            .await
            .remove_active(server_name, attempt.generation);
        if !record_failure {
            anyhow::bail!("Connection to MCP server '{server_name}' cancelled");
        }
        self.inner
            .failures
            .lock()
            .await
            .insert(server_name.to_string(), Instant::now());
        self.emit(McpRuntimeEvent::ConnectionFailed {
            server: server_name.to_string(),
            error: error.to_string(),
        });
        Err(error)
    }

    pub async fn cancel_connect(&self, server_name: &str) {
        self.inner.connections.cancel_connect(server_name).await;
    }

    pub async fn clear_cancelled_connect(&self, server_name: &str) {
        self.inner.connections.clear_cancelled(server_name).await;
    }

    pub async fn failure_retry_after_secs(&self, server_name: &str) -> Option<u64> {
        const FAILURE_BACKOFF_SECS: u64 = 30;
        let failures = self.inner.failures.lock().await;
        let elapsed = failures.get(server_name)?.elapsed().as_secs();
        (elapsed < FAILURE_BACKOFF_SECS).then_some(FAILURE_BACKOFF_SECS - elapsed)
    }

    pub async fn shutdown(self) {
        for removed in self.inner.connections.shutdown().await {
            self.finish_disconnect(removed).await;
        }
    }

    pub(crate) async fn discoverable_tools_for_server(
        &self,
        server_name: &str,
    ) -> anyhow::Result<Vec<rmcp::model::Tool>> {
        let snapshot = self.metadata_for_server(server_name).await?;
        Ok(self.available_tools_from_snapshot(server_name, snapshot))
    }

    fn available_tools_from_snapshot(
        &self,
        server_name: &str,
        snapshot: McpMetadataSnapshot,
    ) -> Vec<rmcp::model::Tool> {
        let mut tools = snapshot.tools;
        tools.extend(resource_tools_from_resources(snapshot.resources));
        tools
            .into_iter()
            .filter(|tool| self.is_tool_enabled(server_name, &tool.name))
            .collect()
    }

    async fn metadata_for_server(&self, server_name: &str) -> anyhow::Result<McpMetadataSnapshot> {
        if let Some(snapshot) = self.current_metadata_for_server(server_name).await? {
            return Ok(snapshot);
        }
        self.refresh_metadata(server_name).await?;
        self.current_metadata_for_server(server_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("MCP metadata refresh produced no snapshot"))
    }

    async fn current_metadata_for_server(
        &self,
        server_name: &str,
    ) -> anyhow::Result<Option<McpMetadataSnapshot>> {
        self.ensure_server_allowed(server_name)?;
        let config = self
            .inner
            .configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server '{server_name}'"))?;
        let access =
            McpMetadataAccess::RefreshIfMissing(self.connection_generation(server_name).await);
        Ok(self
            .inner
            .metadata
            .lock()
            .await
            .snapshot(server_name, config, access))
    }

    pub async fn refresh_metadata(
        &self,
        server_name: &str,
    ) -> anyhow::Result<Vec<rmcp::model::Tool>> {
        self.ensure_server_allowed(server_name)?;
        let config = self
            .inner
            .configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server '{server_name}'"))?;
        let (generation, peer, connected_now) = self.ensure_connected(server_name).await?;
        if connected_now {
            let snapshot = self
                .current_metadata_for_server(server_name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("MCP connection produced no metadata"))?;
            return Ok(snapshot.tools);
        }
        let client::McpListedMetadata {
            tools,
            resources,
            prompts,
        } = client::list_metadata(&peer, server_name, config.expose_resources()).await?;
        self.mark_generation_used(server_name, generation).await;
        let result = tools.items.clone();
        if !self.inner.metadata.lock().await.set_active(
            server_name,
            generation,
            config,
            (tools.items, tools.policy),
            (resources.items, resources.policy),
            (prompts.items, prompts.policy),
        ) {
            anyhow::bail!("stale MCP metadata refresh for '{server_name}'");
        }
        self.emit(McpRuntimeEvent::CacheUpdated {
            server: server_name.to_string(),
            tool_count: result.len(),
        });
        let catalog_changed = self.refresh_capabilities().await;
        if catalog_changed {
            self.emit_catalog_changed(server_name);
        }
        self.save_metadata(server_name).await;
        Ok(result)
    }

    async fn open_subscription(
        &self,
        config: &McpServerConfig,
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    ) -> anyhow::Result<Option<rmcp::service::Subscription>> {
        let info = peer
            .peer_info()
            .ok_or_else(|| anyhow::anyhow!("MCP handshake returned no server information"))?;
        if !client::is_modern_peer(peer) {
            return Ok(None);
        }
        let filter = metadata_subscription_filter(&info.capabilities, config.expose_resources());
        if filter == rmcp::model::SubscriptionFilter::default() {
            return Ok(None);
        }
        Ok(Some(peer.listen(filter).await?))
    }

    fn spawn_subscription(
        &self,
        server_name: String,
        generation: ConnectionGeneration,
        mut subscription: rmcp::service::Subscription,
        changes: client::MetadataChangeSender,
        mut task_start: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            if !wait_for_connection_commit(&mut task_start).await {
                return;
            }
            loop {
                match subscription.next().await {
                    Ok(Some(notification)) => changes.signal_notification(&notification),
                    Ok(None) => {
                        manager
                            .disconnect_generation(&server_name, generation, "subscription_ended")
                            .await;
                        break;
                    }
                    Err(error) => {
                        manager.emit(McpRuntimeEvent::SubscriptionFailed {
                            server: server_name.clone(),
                            error: error.to_string(),
                        });
                        manager
                            .disconnect_generation(&server_name, generation, "subscription_failed")
                            .await;
                        break;
                    }
                }
            }
        })
    }

    fn spawn_metadata_listener(
        &self,
        server_name: String,
        generation: ConnectionGeneration,
        listener: client::MetadataChangeListener,
        mut task_start: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            if !wait_for_connection_commit(&mut task_start).await {
                return;
            }
            loop {
                let changes = listener.changed().await;
                manager
                    .refresh_notified_metadata(&server_name, generation, changes)
                    .await;
            }
        })
    }

    async fn refresh_notified_metadata(
        &self,
        server_name: &str,
        generation: ConnectionGeneration,
        changes: client::MetadataChangeSet,
    ) {
        let Some((current_generation, peer)) = self.inner.connections.peer(server_name).await
        else {
            return;
        };
        if current_generation != generation {
            return;
        }
        if changes.tools {
            self.refresh_notified_tools(server_name, generation, &peer)
                .await;
        }
        if changes.resources {
            self.refresh_notified_resources(server_name, generation, &peer)
                .await;
        }
        if changes.prompts {
            self.refresh_notified_prompts(server_name, generation, &peer)
                .await;
        }
        self.mark_generation_used(server_name, generation).await;
    }

    async fn refresh_notified_tools(
        &self,
        server_name: &str,
        generation: ConnectionGeneration,
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    ) {
        let result = client::list_tools(peer).await.map_err(anyhow::Error::from);
        if self
            .apply_notified_tools(server_name, generation, result)
            .await
        {
            self.save_metadata(server_name).await;
        }
    }

    async fn apply_notified_tools(
        &self,
        server_name: &str,
        generation: ConnectionGeneration,
        result: anyhow::Result<client::McpListed<rmcp::model::Tool>>,
    ) -> bool {
        let tools = match result {
            Ok(tools) => tools,
            Err(error) => {
                self.emit_refresh_failure(server_name, McpMetadataKind::Tools, error);
                return false;
            }
        };
        let tool_count = tools.items.len();
        if !self.inner.metadata.lock().await.update_tools(
            server_name,
            generation,
            tools.items,
            tools.policy,
        ) {
            return false;
        }
        self.emit(McpRuntimeEvent::CacheUpdated {
            server: server_name.to_string(),
            tool_count,
        });
        if self.refresh_capabilities().await {
            self.emit_catalog_changed(server_name);
        }
        true
    }

    async fn refresh_notified_resources(
        &self,
        server_name: &str,
        generation: ConnectionGeneration,
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    ) {
        let Some(config) = self.inner.configs.get(server_name) else {
            return;
        };
        if !config.expose_resources() {
            return;
        }
        let result = client::normalize_optional_list(
            client::list_resources(peer).await,
            peer,
            server_name,
            "resource",
        );
        match result {
            Ok(resources) => {
                if !self.inner.metadata.lock().await.update_resources(
                    server_name,
                    generation,
                    resources.items,
                    resources.policy,
                ) {
                    return;
                }
                self.emit(McpRuntimeEvent::MetadataUpdated {
                    server: server_name.to_string(),
                    kind: McpMetadataKind::Resources,
                });
                if self.refresh_capabilities().await {
                    self.emit_catalog_changed(server_name);
                }
                self.save_metadata(server_name).await;
            }
            Err(error) => self.emit_refresh_failure(server_name, McpMetadataKind::Resources, error),
        }
    }

    async fn refresh_notified_prompts(
        &self,
        server_name: &str,
        generation: ConnectionGeneration,
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    ) {
        let result = client::normalize_optional_list(
            client::list_prompts(peer).await,
            peer,
            server_name,
            "prompt",
        );
        match result {
            Ok(prompts) => {
                if !self.inner.metadata.lock().await.update_prompts(
                    server_name,
                    generation,
                    prompts.items,
                    prompts.policy,
                ) {
                    return;
                }
                self.emit(McpRuntimeEvent::MetadataUpdated {
                    server: server_name.to_string(),
                    kind: McpMetadataKind::Prompts,
                });
                self.save_metadata(server_name).await;
            }
            Err(error) => self.emit_refresh_failure(server_name, McpMetadataKind::Prompts, error),
        }
    }

    fn emit_connected(&self, server_name: &str, tool_count: usize, catalog_changed: bool) {
        self.emit(McpRuntimeEvent::Connected {
            server: server_name.to_string(),
        });
        self.emit(McpRuntimeEvent::CacheUpdated {
            server: server_name.to_string(),
            tool_count,
        });
        if catalog_changed {
            self.emit_catalog_changed(server_name);
        }
    }

    fn emit_catalog_changed(&self, server_name: &str) {
        self.emit(McpRuntimeEvent::ToolCatalogChanged {
            server: server_name.to_string(),
        });
    }

    fn emit_refresh_failure(
        &self,
        server_name: &str,
        kind: McpMetadataKind,
        error: impl std::fmt::Display,
    ) {
        tracing::warn!(target: "mcp", "Failed to refresh MCP {kind:?} metadata for '{server_name}': {error}");
        self.emit(McpRuntimeEvent::MetadataRefreshFailed {
            server: server_name.to_string(),
            kind,
            error: error.to_string(),
        });
    }

    async fn save_metadata(&self, server_name: &str) {
        let persisted = self.inner.metadata.lock().await.persisted_clone();
        if let Err(error) = persisted.save_server(&self.inner.configs, server_name) {
            tracing::warn!(target: "mcp", "Failed to save MCP metadata cache: {error}");
        }
    }

    async fn available_tool_snapshots(&self) -> Vec<(String, McpToolSnapshot)> {
        let mut tools = Vec::new();
        for (server, config) in &self.inner.configs {
            let Ok(Some(snapshot)) = self.current_metadata_for_server(server).await else {
                continue;
            };
            tools.extend(
                snapshot_tools(config, &self.inner.settings, snapshot)
                    .into_iter()
                    .filter(|tool| tool.exposure != ToolExposure::Hidden)
                    .map(|tool| (server.clone(), tool)),
            );
        }
        tools
    }

    fn update_capabilities(&self, tools: &[(String, McpToolSnapshot)]) -> bool {
        let capabilities = tools
            .iter()
            .map(|(server, tool)| {
                (
                    mcp_tool_capability(server, &tool.name),
                    McpCapabilityContract {
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                    },
                )
            })
            .collect();
        let Ok(mut current) = self.inner.capabilities.write() else {
            return false;
        };
        let changed = *current != capabilities;
        *current = capabilities;
        changed
    }

    async fn refresh_capabilities(&self) -> bool {
        self.update_capabilities(&self.available_tool_snapshots().await)
    }

    async fn tool_specs(&self) -> Vec<McpToolSpec> {
        let snapshots = self.available_tool_snapshots().await;
        self.update_capabilities(&snapshots);
        let mut tools = Vec::new();
        let mut names = builtin_tool_names();
        for (server, tool) in snapshots {
            let registered_name =
                config::format_tool_name(&tool.name, &server, self.inner.settings.tool_prefix);
            if !names.insert(registered_name.to_ascii_lowercase()) {
                tracing::warn!(target: "mcp", "Skipping duplicate MCP tool '{registered_name}'");
                continue;
            }
            tools.push(McpToolSpec {
                display_name: config::format_tool_display_name(
                    &tool.name,
                    &server,
                    self.inner.settings.tool_prefix,
                ),
                server_name: server,
                tool_name: tool.name,
                registered_name,
                exposure: tool.exposure,
                description: tool.description,
                input_schema: tool.input_schema,
            });
        }
        tools
    }

    fn is_tool_enabled(&self, server_name: &str, tool_name: &str) -> bool {
        self.inner
            .configs
            .get(server_name)
            .map(|config| {
                self.inner.transport_policy.allows(config)
                    && config.tool_exposure(&self.inner.settings, tool_name) != ToolExposure::Hidden
            })
            .unwrap_or(false)
    }

    fn ensure_server_allowed(&self, server_name: &str) -> anyhow::Result<()> {
        let config = self
            .inner
            .configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server '{server_name}'"))?;
        anyhow::ensure!(
            self.inner.transport_policy.allows(config),
            "URL MCP is blocked by the execution transport policy: '{server_name}'; use a sandboxed command transport"
        );
        Ok(())
    }

    fn ensure_tool_enabled(&self, server_name: &str, tool_name: &str) -> anyhow::Result<()> {
        self.ensure_server_allowed(server_name)?;
        if self.is_tool_enabled(server_name, tool_name) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "MCP tool '{tool_name}' on server '{server_name}' is disabled"
            ))
        }
    }

    async fn resource_uri_for_tool(
        &self,
        server_name: &str,
        tool_name: &str,
    ) -> anyhow::Result<Option<String>> {
        let config = self
            .inner
            .configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server '{server_name}'"))?;
        if !config.expose_resources() {
            return Ok(None);
        }
        let resources = self.metadata_for_server(server_name).await?.resources;
        Ok(resources
            .into_iter()
            .find(|resource| resource_tool_name(&resource.name) == tool_name)
            .map(|resource| resource.uri.clone()))
    }

    async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> anyhow::Result<rmcp::model::CallToolResult> {
        let lease = self.begin_call(server_name).await?;
        let result = lease
            .peer
            .read_resource(rmcp::model::ReadResourceRequestParams::new(uri.to_string()))
            .await?;
        self.mark_generation_used(server_name, lease.generation)
            .await;
        Ok(rmcp::model::CallToolResult::success(
            result
                .contents
                .into_iter()
                .map(rmcp::model::ContentBlock::resource)
                .collect(),
        ))
    }

    async fn ensure_connected(
        &self,
        server_name: &str,
    ) -> anyhow::Result<(
        ConnectionGeneration,
        rmcp::service::Peer<rmcp::service::RoleClient>,
        bool,
    )> {
        self.close_idle_connections().await;
        if let Some((generation, peer)) = self.inner.connections.peer(server_name).await {
            return Ok((generation, peer, false));
        }
        self.connect(server_name).await?;
        self.inner
            .connections
            .peer(server_name)
            .await
            .map(|(generation, peer)| (generation, peer, true))
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' is not connected"))
    }

    async fn connection_generation(&self, server_name: &str) -> Option<ConnectionGeneration> {
        self.inner.connections.generation(server_name).await
    }

    async fn disconnect(&self, server_name: &str, reason: &str) {
        if let Some(removed) = self.inner.connections.remove(server_name, reason).await {
            self.finish_disconnect(removed).await;
        }
    }

    async fn disconnect_generation(
        &self,
        server_name: &str,
        generation: ConnectionGeneration,
        reason: &str,
    ) {
        if let Some(removed) = self
            .inner
            .connections
            .remove_generation(server_name, generation, reason)
            .await
        {
            self.finish_disconnect(removed).await;
        }
    }

    async fn finish_disconnect(&self, removed: RemovedConnection) {
        self.inner
            .metadata
            .lock()
            .await
            .remove_active(&removed.server, removed.generation);
        self.refresh_capabilities().await;
        self.emit(McpRuntimeEvent::Disconnected {
            server: removed.server,
            reason: removed.reason,
        });
    }

    async fn begin_call(&self, server_name: &str) -> anyhow::Result<McpConnectionLease> {
        self.close_idle_connections().await;
        self.connect(server_name).await?;
        self.inner
            .connections
            .checkout(server_name)
            .await
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' is not connected"))
    }

    async fn mark_generation_used(&self, server_name: &str, generation: ConnectionGeneration) {
        self.inner
            .connections
            .mark_used(server_name, Some(generation))
            .await;
    }

    async fn close_idle_connections(&self) {
        for removed in self
            .inner
            .connections
            .remove_idle(&self.inner.configs)
            .await
        {
            self.finish_disconnect(removed).await;
        }
    }

    async fn reconnect_keep_alive_servers(&self) {
        let server_names = self
            .inner
            .configs
            .iter()
            .filter(|(_, config)| config.effective_runtime().lifecycle == McpLifecycle::KeepAlive)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();

        for server_name in server_names {
            if self.failure_retry_after_secs(&server_name).await.is_some() {
                continue;
            }
            if self
                .inner
                .connections
                .generation(&server_name)
                .await
                .is_none()
            {
                let _ = self.connect(&server_name).await;
            }
        }
    }

    fn spawn_maintenance_loop(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if manager.inner.connections.is_shutdown().await {
                    break;
                }
                manager.close_idle_connections().await;
                manager.reconnect_keep_alive_servers().await;
            }
        });
    }

    fn emit(&self, event: McpRuntimeEvent) {
        tracing::debug!(target: "mcp", ?event, "MCP runtime event");
        let _ = self.inner.event_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::McpRuntimeConfig;

    #[tokio::test]
    async fn startup_discovery_needs_uncached_direct_contracts() {
        for exposure in [
            None,
            Some(ToolExposure::Direct),
            Some(ToolExposure::Deferred),
            Some(ToolExposure::Hidden),
        ] {
            let mut config = test_config();
            if let McpServerConfig::Command {
                exposure: default, ..
            } = &mut config
            {
                *default = exposure;
            }
            assert_eq!(
                config.tool_exposure(&McpSettings::default(), "probe"),
                exposure.unwrap_or(ToolExposure::Deferred)
            );
            let configs = HashMap::from([("server".into(), config.clone())]);
            let manager = test_manager(
                configs.clone(),
                McpMetadataCache::default(),
                McpSettings::default(),
            );
            assert!(manager.tool_registrations(None).await.is_empty());
            assert_eq!(
                manager.tool_refresh_candidates().await.is_empty(),
                exposure != Some(ToolExposure::Direct)
            );
            assert!(manager.startup_connection_candidates().is_empty());
            assert!(manager.inner.connections.generations().await.is_empty());

            let mut cache = McpMetadataCache::default();
            cache.update_metadata(
                "server",
                &config,
                cached(Vec::new()),
                cached(Vec::new()),
                cached(Vec::new()),
            );
            let cached_manager = test_manager(configs, cache, McpSettings::default());
            assert!(cached_manager.tool_refresh_candidates().await.is_empty());
        }
    }

    #[tokio::test]
    async fn declaration_validation_uses_snapshot_names_without_prefix_guessing() {
        for prefix in [config::McpToolPrefix::None, config::McpToolPrefix::Server] {
            let config = test_config();
            let mut cache = McpMetadataCache::default();
            cache.update_metadata(
                "server",
                &config,
                cached(vec![test_tool("search")]),
                cached(Vec::new()),
                cached(Vec::new()),
            );
            let manager = test_manager(
                HashMap::from([("server".to_string(), config)]),
                cache,
                McpSettings {
                    tool_prefix: prefix,
                    ..Default::default()
                },
            );
            let (_, contracts) = manager.snapshot_tool_contracts().await;
            let name = config::format_tool_name("search", "server", prefix);
            let rule = format!("{name}(query *)").parse().unwrap();
            assert!(contracts.validate_tool_rule(&rule).is_err());
            assert!(
                contracts
                    .validate_tool_rule(&"Read".parse().unwrap())
                    .is_ok()
            );
            assert!(
                contracts
                    .validate_tool_rule(&"custom".parse().unwrap())
                    .is_ok()
            );
            assert!(manager.inner.connections.generations().await.is_empty());
        }
    }

    #[tokio::test]
    async fn captured_exposure_agrees_across_registry_and_owner() {
        let mut config = test_config();
        if let McpServerConfig::Command {
            exposure,
            tool_exposure,
            ..
        } = &mut config
        {
            *exposure = Some(ToolExposure::Deferred);
            tool_exposure.insert("direct".into(), ToolExposure::Direct);
            tool_exposure.insert("blocked".into(), ToolExposure::Hidden);
        }
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            &config,
            cached(vec![
                test_tool("direct"),
                test_tool("deferred"),
                test_tool("blocked"),
            ]),
            cached(vec![rmcp::model::Resource::new("fixture://guide", "Guide")]),
            cached(Vec::new()),
        );
        let manager = test_manager(
            HashMap::from([("server".into(), config)]),
            cache,
            McpSettings::default(),
        );
        let (snapshot, contracts) = manager.snapshot_tool_contracts().await;
        assert!(
            contracts
                .validate_tool_rule(&"server_deferred".parse().unwrap())
                .is_err()
        );
        let mut catalog = crate::tools::ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        for registration in snapshot
            .tool_registrations(Some(PermissionRuntime::open()))
            .await
        {
            catalog.register(registration.into());
        }
        let registry = catalog.build().unwrap();
        let names = registry
            .provider_declarations()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "server_direct"));
        assert!(!names.iter().any(|name| name == "server_deferred"));
        assert!(registry.is_script_callable("server_deferred"));
        assert!(registry.is_script_callable("server_get_guide"));
        assert!(!registry.is_script_callable("server_blocked"));
        let management = manager.server_snapshots().await;
        assert!(
            management[0]
                .tools
                .iter()
                .any(|tool| { tool.name == "blocked" && tool.exposure == ToolExposure::Hidden })
        );
        let declarations = registry.script_declarations();
        for tool in management[0]
            .tools
            .iter()
            .filter(|tool| tool.exposure != ToolExposure::Hidden)
        {
            let name = format!("server_{}", tool.name);
            let declared = declarations
                .iter()
                .find(|entry| entry.name == name)
                .unwrap();
            assert_eq!(declared.description, tool.description);
            assert_eq!(declared.input_schema.as_ref(), Some(&tool.input_schema));
            assert!(manager.is_available(&mcp_tool_capability("server", &tool.name)));
        }
        assert!(!manager.is_available(&mcp_tool_capability("server", "blocked")));
        assert_eq!(manager.collect_tools().await.len(), 1);
        assert!(manager.is_available(&mcp_tool_capability("server", "deferred")));
        let (_, restored_contracts) = snapshot.snapshot_tool_contracts().await;
        assert_eq!(restored_contracts, contracts);
        assert!(manager.inner.connections.generations().await.is_empty());
    }

    #[tokio::test]
    async fn captured_contracts_detect_schema_changes() {
        let config = test_config();
        let schema = serde_json::json!({
            "$defs": { "query": { "type": "string" } },
            "oneOf": [
                { "$ref": "#/$defs/query" },
                { "type": "array", "items": { "$ref": "#/$defs/query" } }
            ]
        });
        let mut original = test_tool("search");
        original.input_schema = Arc::new(serde_json::from_value(schema.clone()).unwrap());
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            &config,
            cached(vec![original]),
            cached(Vec::new()),
            cached(Vec::new()),
        );
        let manager = test_manager(
            HashMap::from([("server".to_string(), config.clone())]),
            cache,
            McpSettings::default(),
        );
        let (snapshot, contracts) = manager.snapshot_tool_contracts().await;
        let mut changed = test_tool("search");
        changed.input_schema = Arc::new(serde_json::Map::from_iter([(
            "type".to_string(),
            serde_json::json!("string"),
        )]));
        assert!(manager.inner.metadata.lock().await.set_active(
            "server",
            ConnectionGeneration(1),
            &config,
            cached(vec![changed]),
            cached(Vec::new()),
            cached(Vec::new()),
        ));

        assert!(manager.refresh_capabilities().await);
        assert!(!manager.refresh_capabilities().await);
        assert_ne!(manager.tool_contracts().await, contracts);
        assert_eq!(snapshot.snapshot_tool_contracts().await.1, contracts);
        assert_eq!(
            McpClientManager::contracts_for(snapshot.tool_specs.as_ref().unwrap()),
            contracts
        );
        let definitions = rig::tool::ToolSet::from_dynamic_tools(
            snapshot
                .tool_registrations(None)
                .await
                .into_iter()
                .map(|registration| registration.tool)
                .collect(),
        )
        .get_tool_definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].parameters, schema);
        assert!(!manager.refresh_capabilities().await);
    }

    #[tokio::test]
    async fn disconnect_revokes_transient_capabilities() {
        let config = test_config();
        let manager = test_manager(
            HashMap::from([("server".to_string(), config.clone())]),
            McpMetadataCache::default(),
            McpSettings::default(),
        );
        let generation = ConnectionGeneration(1);
        let private = cache::McpCachePolicy::new(60_000, rmcp::model::CacheScope::Private);
        assert!(manager.inner.metadata.lock().await.set_active(
            "server",
            generation,
            &config,
            (vec![test_tool("private")], private),
            (Vec::new(), private),
            (Vec::new(), private),
        ));
        let capability = mcp_tool_capability("server", "private");
        manager.inner.capabilities.write().unwrap().insert(
            capability.clone(),
            McpCapabilityContract {
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        );
        assert!(manager.is_available(&capability));

        manager
            .finish_disconnect(RemovedConnection {
                server: "server".to_string(),
                generation,
                reason: "test".to_string(),
            })
            .await;

        assert!(!manager.is_available(&capability));
    }

    #[tokio::test]
    async fn notified_refresh_updates_on_success_and_preserves_on_failure() {
        let config = test_config();
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            &config,
            cached(vec![test_tool("old")]),
            cached(Vec::new()),
            cached(Vec::new()),
        );
        let manager = test_manager(
            HashMap::from([("server".to_string(), config.clone())]),
            cache,
            McpSettings::default(),
        );
        let generation = ConnectionGeneration(1);
        let _ = manager.inner.metadata.lock().await.set_active(
            "server",
            generation,
            &config,
            cached(vec![test_tool("old")]),
            cached(Vec::new()),
            cached(Vec::new()),
        );
        let mut events = manager.subscribe();

        assert!(
            manager
                .apply_notified_tools("server", generation, Ok(listed(vec![test_tool("new")])),)
                .await
        );
        assert!(
            !manager
                .apply_notified_tools(
                    "server",
                    generation,
                    Err(anyhow::anyhow!("temporary failure")),
                )
                .await
        );
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            emitted
                .iter()
                .any(|event| matches!(event, McpRuntimeEvent::CacheUpdated { tool_count: 1, .. }))
        );
        assert!(emitted.iter().any(|event| matches!(
            event,
            McpRuntimeEvent::MetadataRefreshFailed {
                kind: McpMetadataKind::Tools,
                ..
            }
        )));
        let snapshot = manager
            .inner
            .metadata
            .lock()
            .await
            .snapshot(
                "server",
                &config,
                McpMetadataAccess::ActiveConnection(generation),
            )
            .unwrap();
        assert_eq!(snapshot.tools[0].name, "new");
    }

    #[tokio::test]
    async fn url_transport_policy_blocks_requests_and_cached_tool_authority() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "url": format!("http://{}", listener.local_addr().unwrap()), "exposure": "direct", "lifecycle": "eager",
        }))
        .unwrap();
        let configs = HashMap::from([("external".into(), config.clone())]);
        let sandbox = crate::sandbox::Sandbox::new(false, std::env::temp_dir()).unwrap();
        let local = McpClientManager::load_with_settings(&configs, McpSettings::default())
            .with_sandbox(sandbox.clone());
        let blocked = local
            .clone()
            .with_execution_environment(sandbox.clone(), McpTransportPolicy::CommandOnly);
        assert!(!Arc::ptr_eq(&local.inner, &blocked.inner));
        assert!(Arc::ptr_eq(
            &blocked.inner,
            &blocked.clone().with_sandbox(sandbox).inner
        ));
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "external",
            &config,
            cached(vec![test_tool("write")]),
            cached(Vec::new()),
            cached(Vec::new()),
        );
        *blocked.inner.metadata.lock().await = McpMetadataCatalog::new(cache);
        assert!(blocked.startup_connection_candidates().is_empty());
        blocked.start().await;
        for result in [
            blocked.connect("external").await,
            blocked.refresh_metadata("external").await.map(|_| ()),
            blocked
                .call_tool("external", "write", None)
                .await
                .map(|_| ()),
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("URL MCP is blocked")
            );
        }
        let snapshots = blocked.server_snapshots().await;
        assert_eq!(snapshots[0].status.state, McpConnectionState::Blocked);
        assert!(snapshots[0].tools.is_empty());
        assert!(blocked.collect_tools().await.is_empty());
        assert!(blocked.tool_refresh_candidates().await.is_empty());
        assert!(
            blocked.search_tools("write").await.unwrap()["tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(local.ensure_server_allowed("external").is_ok());
        blocked.shutdown().await;
    }

    #[test]
    fn connection_pools_are_scoped_to_the_execution_environment() {
        let configs = HashMap::new();
        let manager = McpClientManager::load_with_settings(&configs, McpSettings::default());
        let sandbox = crate::sandbox::Sandbox::new(false, std::env::temp_dir()).unwrap();
        let scoped = manager.clone().with_sandbox(sandbox.clone());
        assert!(!Arc::ptr_eq(&manager.inner, &scoped.inner));
        let reused = scoped.clone().with_sandbox(sandbox);
        assert!(Arc::ptr_eq(&scoped.inner, &reused.inner));
        let restricted = crate::sandbox::Sandbox::new(true, std::env::temp_dir()).unwrap();
        let isolated = scoped.clone().with_sandbox(restricted);
        assert!(!Arc::ptr_eq(&scoped.inner, &isolated.inner));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn changing_protection_rules_never_reuses_a_connection_pool() {
        let root = std::env::temp_dir().join(format!("phi-mcp-pool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".phi")).unwrap();
        let sandbox = crate::sandbox::Sandbox::new(true, &root).unwrap();
        let manager = McpClientManager::load_with_settings(&HashMap::new(), McpSettings::default())
            .with_sandbox(sandbox.clone());
        let protected = sandbox
            .with_write_protection(vec![root.join(".phi")], Vec::new())
            .unwrap();
        let isolated = manager.clone().with_sandbox(protected.clone());
        assert!(!Arc::ptr_eq(&manager.inner, &isolated.inner));
        assert!(Arc::ptr_eq(
            &isolated.inner,
            &isolated.clone().with_sandbox(protected).inner
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn command_connections_enforce_protection_on_connect_and_refresh() {
        let root =
            std::env::temp_dir().join(format!("phi-mcp-protection-{}", uuid::Uuid::new_v4()));
        let protected = root.join(".phi");
        std::fs::create_dir_all(&protected).unwrap();
        let settings = protected.join("settings.json");
        let marker = root.join("started");
        std::fs::write(&settings, "original").unwrap();
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "sh", "args": ["-c", "printf changed > \"$1\"; printf '%s:%s\\n' \"$PWD\" \"$PHI_MCP_PROBE\" >> \"$2\"", "phi-mcp-test", settings, marker],
            "cwd": protected, "env": {"PHI_MCP_PROBE": "started"}, "lifecycle": "eager",
        })).unwrap();
        let sandbox = crate::sandbox::Sandbox::new(false, &root)
            .unwrap()
            .with_write_protection(vec![protected.clone()], Vec::new())
            .unwrap();
        let manager = McpClientManager::load_with_settings(
            &HashMap::from([("probe".into(), config)]),
            McpSettings::default(),
        )
        .with_execution_environment(sandbox, McpTransportPolicy::CommandOnly);
        manager.start().await;
        assert!(manager.connect("probe").await.is_err());
        assert!(manager.refresh_metadata("probe").await.is_err());
        assert_eq!(std::fs::read_to_string(settings).unwrap(), "original");
        let expected = format!("{}:started\n", protected.display()).repeat(3);
        assert_eq!(std::fs::read_to_string(marker).unwrap(), expected);
        manager.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_manager(
        configs: HashMap<String, McpServerConfig>,
        cache: McpMetadataCache,
        settings: McpSettings,
    ) -> McpClientManager {
        McpClientManager {
            inner: Arc::new(McpRuntimeInner {
                configs,
                settings,
                sandbox: None,
                transport_policy: McpTransportPolicy::All,
                connections: McpConnectionPool::new(),
                failures: Mutex::new(HashMap::new()),
                metadata: Mutex::new(McpMetadataCatalog::new(cache)),
                capabilities: std::sync::RwLock::new(HashMap::new()),
                event_tx: broadcast::channel(8).0,
                elicitation: elicitation::McpElicitationRouter::default(),
                maintenance_started: AtomicBool::new(false),
                background_started: AtomicBool::new(false),
            }),
            interaction_tx: None,
            tool_specs: None,
        }
    }

    fn test_config() -> McpServerConfig {
        McpServerConfig::Command {
            command: "test".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            lifecycle: None,
            idle_timeout_secs: None,
            runtime: McpRuntimeConfig::default(),
            exposure: Some(ToolExposure::Direct),
            tool_exposure: HashMap::new(),
            expose_resources: None,
            debug: false,
        }
    }

    fn cached<T>(items: Vec<T>) -> (Vec<T>, cache::McpCachePolicy) {
        (
            items,
            cache::McpCachePolicy::new(60_000, rmcp::model::CacheScope::Public),
        )
    }

    fn listed<T>(items: Vec<T>) -> client::McpListed<T> {
        client::McpListed {
            items,
            policy: cache::McpCachePolicy::new(60_000, rmcp::model::CacheScope::Public),
        }
    }

    fn test_tool(name: &str) -> rmcp::model::Tool {
        let mut tool = rmcp::model::Tool::default();
        tool.name = name.to_string().into();
        tool
    }
}
