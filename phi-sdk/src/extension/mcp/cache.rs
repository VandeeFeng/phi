use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::model::CacheScope;
use serde::{Deserialize, Serialize};

use super::config::McpServerConfig;

const CACHE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpMetadataCache {
    version: u32,
    pub servers: HashMap<String, McpServerMetadata>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerMetadata {
    pub config_hash: u64,
    #[serde(default)]
    pub(crate) protocol: Option<McpProtocolHint>,
    pub tools: Option<McpCachedList<rmcp::model::Tool>>,
    pub resources: Option<McpCachedList<rmcp::model::Resource>>,
    pub prompts: Option<McpCachedList<rmcp::model::Prompt>>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpProtocolHint {
    Legacy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpCachedList<T> {
    pub expires_at_ms: u64,
    pub scope: CacheScope,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, Copy)]
pub struct McpCachePolicy {
    pub ttl_ms: u64,
    pub scope: CacheScope,
}

impl Default for McpMetadataCache {
    fn default() -> Self {
        Self {
            version: CACHE_SCHEMA_VERSION,
            servers: HashMap::new(),
        }
    }
}

impl McpCachePolicy {
    pub fn new(ttl_ms: u64, scope: CacheScope) -> Self {
        Self { ttl_ms, scope }
    }

    pub fn transient() -> Self {
        Self::new(0, CacheScope::Private)
    }
}

impl McpMetadataCache {
    pub fn load(configs: &HashMap<String, McpServerConfig>) -> Self {
        let Ok(content) = std::fs::read_to_string(cache_path()) else {
            return Self::default();
        };
        let mut cache = decode_cache(&content);
        cache.retain_compatible(configs);
        cache
    }

    pub fn save_server(
        &self,
        configs: &HashMap<String, McpServerConfig>,
        server_name: &str,
    ) -> anyhow::Result<()> {
        let path = cache_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut persisted = self.persistable(configs);
        let updated = persisted.servers.remove(server_name);
        let existing = std::fs::read_to_string(&path)
            .ok()
            .map(|content| decode_cache(&content))
            .unwrap_or_default();
        let merged = merge_server_cache(existing, updated, configs, server_name);
        let tmp_path = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&tmp_path, serde_json::to_vec_pretty(&merged)?)?;
        #[cfg(windows)]
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub(crate) fn snapshot_for(
        &self,
        server_name: &str,
        config: &McpServerConfig,
    ) -> Option<(
        Vec<rmcp::model::Tool>,
        Vec<rmcp::model::Resource>,
        Vec<rmcp::model::Prompt>,
    )> {
        let metadata = self.metadata_for(server_name, config)?;
        let tools = reusable_items(&metadata.tools);
        let resources = config
            .expose_resources()
            .then(|| reusable_items(&metadata.resources))
            .flatten();
        let prompts = reusable_items(&metadata.prompts);
        if tools.is_none() && resources.is_none() && prompts.is_none() {
            return None;
        }
        Some((
            tools.unwrap_or_default(),
            resources.unwrap_or_default(),
            prompts.unwrap_or_default(),
        ))
    }

    pub(crate) fn has_reusable_direct_metadata(
        &self,
        server_name: &str,
        config: &McpServerConfig,
    ) -> bool {
        self.metadata_for(server_name, config)
            .is_some_and(|metadata| {
                reusable_items(&metadata.tools).is_some()
                    && (!config.expose_resources() || reusable_items(&metadata.resources).is_some())
            })
    }

    pub(crate) fn protocol_hint(
        &self,
        server_name: &str,
        config: &McpServerConfig,
    ) -> Option<McpProtocolHint> {
        let metadata = self.metadata_for(server_name, config)?;
        let reusable = reusable_items(&metadata.tools).is_some()
            || reusable_items(&metadata.resources).is_some()
            || reusable_items(&metadata.prompts).is_some();
        reusable.then_some(metadata.protocol).flatten()
    }

    pub(crate) fn set_protocol_hint(
        &mut self,
        server_name: &str,
        config: &McpServerConfig,
        protocol: McpProtocolHint,
    ) {
        let metadata = self
            .servers
            .entry(server_name.to_string())
            .or_insert_with(|| McpServerMetadata {
                config_hash: config_hash(config),
                protocol: None,
                tools: None,
                resources: None,
                prompts: None,
            });
        if metadata.config_hash == config_hash(config) {
            metadata.protocol = Some(protocol);
        }
    }

    pub fn update_metadata(
        &mut self,
        server_name: &str,
        config: &McpServerConfig,
        tools: (Vec<rmcp::model::Tool>, McpCachePolicy),
        resources: (Vec<rmcp::model::Resource>, McpCachePolicy),
        prompts: (Vec<rmcp::model::Prompt>, McpCachePolicy),
    ) {
        let protocol = self
            .metadata_for(server_name, config)
            .and_then(|metadata| metadata.protocol);
        self.servers.insert(
            server_name.to_string(),
            McpServerMetadata {
                config_hash: config_hash(config),
                protocol,
                tools: Some(McpCachedList::new(tools.0, tools.1)),
                resources: Some(McpCachedList::new(resources.0, resources.1)),
                prompts: Some(McpCachedList::new(prompts.0, prompts.1)),
            },
        );
    }

    pub fn update_tools(
        &mut self,
        server_name: &str,
        tools: Vec<rmcp::model::Tool>,
        policy: McpCachePolicy,
    ) {
        if let Some(metadata) = self.servers.get_mut(server_name) {
            metadata.tools = Some(McpCachedList::new(tools, policy));
        }
    }

    pub fn update_resources(
        &mut self,
        server_name: &str,
        resources: Vec<rmcp::model::Resource>,
        policy: McpCachePolicy,
    ) {
        if let Some(metadata) = self.servers.get_mut(server_name) {
            metadata.resources = Some(McpCachedList::new(resources, policy));
        }
    }

    pub fn update_prompts(
        &mut self,
        server_name: &str,
        prompts: Vec<rmcp::model::Prompt>,
        policy: McpCachePolicy,
    ) {
        if let Some(metadata) = self.servers.get_mut(server_name) {
            metadata.prompts = Some(McpCachedList::new(prompts, policy));
        }
    }

    fn metadata_for(
        &self,
        server_name: &str,
        config: &McpServerConfig,
    ) -> Option<&McpServerMetadata> {
        self.servers
            .get(server_name)
            .filter(|metadata| metadata.config_hash == config_hash(config))
    }

    fn retain_compatible(&mut self, configs: &HashMap<String, McpServerConfig>) {
        self.servers.retain(|name, metadata| {
            configs.get(name).is_some_and(|config| {
                config.metadata_cache_persistable() && metadata.config_hash == config_hash(config)
            })
        });
    }

    fn persistable(&self, configs: &HashMap<String, McpServerConfig>) -> Self {
        let servers = self
            .servers
            .iter()
            .filter_map(|(name, metadata)| {
                let config = configs.get(name)?;
                if !config.metadata_cache_persistable() {
                    return None;
                }
                let metadata = metadata.persistable();
                metadata.has_entries().then(|| (name.clone(), metadata))
            })
            .collect();
        Self {
            version: CACHE_SCHEMA_VERSION,
            servers,
        }
    }
}

impl<T> McpCachedList<T> {
    fn new(items: Vec<T>, policy: McpCachePolicy) -> Self {
        Self {
            expires_at_ms: now_ms().saturating_add(policy.ttl_ms),
            scope: policy.scope,
            items,
        }
    }

    fn is_fresh(&self) -> bool {
        self.expires_at_ms > now_ms()
    }
}

impl McpServerMetadata {
    fn persistable(&self) -> Self {
        Self {
            config_hash: self.config_hash,
            protocol: self.protocol,
            tools: persistable_entry(&self.tools),
            resources: persistable_entry(&self.resources),
            prompts: persistable_entry(&self.prompts),
        }
    }

    fn has_entries(&self) -> bool {
        self.tools.is_some() || self.resources.is_some() || self.prompts.is_some()
    }
}

fn persistable_entry<T: Clone>(entry: &Option<McpCachedList<T>>) -> Option<McpCachedList<T>> {
    entry
        .as_ref()
        .filter(|entry| entry.scope == CacheScope::Public && entry.is_fresh())
        .cloned()
}

fn reusable_items<T: Clone>(entry: &Option<McpCachedList<T>>) -> Option<Vec<T>> {
    persistable_entry(entry).map(|entry| entry.items)
}

fn cache_path() -> PathBuf {
    crate::utils::path::phi_config_dir().join("mcp-cache.json")
}

fn config_hash(config: &McpServerConfig) -> u64 {
    let mut hasher = DefaultHasher::new();
    match config {
        McpServerConfig::Command {
            command,
            args,
            env,
            cwd,
            expose_resources,
            ..
        } => {
            "command".hash(&mut hasher);
            command.hash(&mut hasher);
            args.hash(&mut hasher);
            sorted_keys(env).hash(&mut hasher);
            cwd.hash(&mut hasher);
            expose_resources.hash(&mut hasher);
        }
        McpServerConfig::Url {
            url,
            headers,
            auth,
            bearer_token_env,
            expose_resources,
            ..
        } => {
            "url".hash(&mut hasher);
            url.hash(&mut hasher);
            sorted_keys(headers).hash(&mut hasher);
            auth.hash(&mut hasher);
            bearer_token_env.hash(&mut hasher);
            expose_resources.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn sorted_keys(map: &HashMap<String, String>) -> Vec<&String> {
    let mut keys = map.keys().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn merge_server_cache(
    mut existing: McpMetadataCache,
    updated: Option<McpServerMetadata>,
    configs: &HashMap<String, McpServerConfig>,
    server_name: &str,
) -> McpMetadataCache {
    existing.retain_compatible(configs);
    existing.servers.remove(server_name);
    if let Some(updated) = updated {
        existing.servers.insert(server_name.to_string(), updated);
    }
    existing.persistable(configs)
}

fn decode_cache(content: &str) -> McpMetadataCache {
    serde_json::from_str::<McpMetadataCache>(content)
        .ok()
        .filter(|cache| cache.version == CACHE_SCHEMA_VERSION)
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_snapshot_filters_private_metadata_per_category() {
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "test",
            "exposeResources": true
        }))
        .unwrap();
        let private = McpCachePolicy::new(60_000, CacheScope::Private);
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            &config,
            (vec![test_tool("public")], public_policy()),
            (
                vec![rmcp::model::Resource::new("file:///private", "Private")],
                private,
            ),
            (Vec::new(), private),
        );

        let snapshot = cache.snapshot_for("server", &config).unwrap();
        assert_eq!(snapshot.0[0].name, "public");
        assert!(snapshot.1.is_empty());
        assert!(snapshot.2.is_empty());

        cache.update_tools("server", vec![test_tool("private")], private);
        assert!(cache.snapshot_for("server", &config).is_none());
        assert!(!cache.has_reusable_direct_metadata("server", &config));
    }

    #[test]
    fn reusable_direct_metadata_requires_exposed_resource_cache() {
        let config = test_config();
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            &config,
            (Vec::new(), public_policy()),
            (Vec::new(), McpCachePolicy::transient()),
            (Vec::new(), McpCachePolicy::transient()),
        );

        assert!(!cache.has_reusable_direct_metadata("server", &config));
        cache.update_resources("server", Vec::new(), public_policy());
        assert!(cache.has_reusable_direct_metadata("server", &config));
    }

    #[test]
    fn protocol_hint_is_reused_only_with_fresh_metadata() {
        let config = test_config();
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            &config,
            (Vec::new(), public_policy()),
            (Vec::new(), McpCachePolicy::transient()),
            (Vec::new(), McpCachePolicy::transient()),
        );
        cache.set_protocol_hint("server", &config, McpProtocolHint::Legacy);

        assert_eq!(
            cache.protocol_hint("server", &config),
            Some(McpProtocolHint::Legacy)
        );

        cache.update_tools(
            "server",
            Vec::new(),
            McpCachePolicy::new(0, CacheScope::Public),
        );
        assert_eq!(cache.protocol_hint("server", &config), None);
    }

    #[test]
    fn server_cache_merge_preserves_other_server_updates() {
        let config = test_config();
        let configs = HashMap::from([
            ("first".to_string(), config.clone()),
            ("second".to_string(), config.clone()),
        ]);
        let mut existing = McpMetadataCache::default();
        for (server, tool) in [("first", "old-first"), ("second", "newer-second")] {
            existing.update_metadata(
                server,
                &config,
                (vec![test_tool(tool)], public_policy()),
                (Vec::new(), public_policy()),
                (Vec::new(), public_policy()),
            );
        }
        let mut current = McpMetadataCache::default();
        current.update_metadata(
            "first",
            &config,
            (vec![test_tool("new-first")], public_policy()),
            (Vec::new(), public_policy()),
            (Vec::new(), public_policy()),
        );

        let merged =
            merge_server_cache(existing, current.servers.remove("first"), &configs, "first");

        assert_eq!(
            merged.servers["first"].tools.as_ref().unwrap().items[0].name,
            "new-first"
        );
        assert_eq!(
            merged.servers["second"].tools.as_ref().unwrap().items[0].name,
            "newer-second"
        );
    }

    #[test]
    fn persisted_cache_enforces_identity_scope_and_freshness() {
        for value in [
            serde_json::json!({ "command": "test", "env": { "PROFILE": "prod" } }),
            serde_json::json!({ "url": "https://example.com", "headers": { "X-Tenant": "prod" } }),
            serde_json::json!({ "url": "https://example.com", "bearerToken": "secret" }),
            serde_json::json!({ "url": "https://example.com", "bearerTokenEnv": "TOKEN" }),
            serde_json::json!({ "url": "https://example.com", "auth": "oauth" }),
        ] {
            let config: McpServerConfig = serde_json::from_value(value).unwrap();
            let configs = HashMap::from([("server".to_string(), config.clone())]);
            let mut cache = cache_with(&config);
            assert!(cache.persistable(&configs).servers.is_empty());
            cache.retain_compatible(&configs);
            assert!(cache.servers.is_empty());
        }

        let config = test_config();
        let configs = HashMap::from([("server".to_string(), config.clone())]);
        let persisted = cache_with(&config).persistable(&configs);
        let metadata = &persisted.servers["server"];
        assert!(metadata.tools.is_some());
        assert!(metadata.resources.is_none());
        assert!(metadata.prompts.is_none());
    }

    fn cache_with(config: &McpServerConfig) -> McpMetadataCache {
        let mut cache = McpMetadataCache::default();
        cache.update_metadata(
            "server",
            config,
            (vec![test_tool("public")], public_policy()),
            (Vec::new(), McpCachePolicy::transient()),
            (Vec::new(), McpCachePolicy::new(0, CacheScope::Public)),
        );
        cache
    }

    fn public_policy() -> McpCachePolicy {
        McpCachePolicy::new(60_000, CacheScope::Public)
    }

    fn test_config() -> McpServerConfig {
        serde_json::from_value(serde_json::json!({ "command": "test" })).unwrap()
    }

    fn test_tool(name: &str) -> rmcp::model::Tool {
        let mut tool = rmcp::model::Tool::default();
        tool.name = name.to_string().into();
        tool
    }
}
