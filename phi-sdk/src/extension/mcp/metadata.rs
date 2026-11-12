use std::collections::HashMap;

use super::cache::{McpCachePolicy, McpMetadataCache, McpProtocolHint};
use super::config::McpServerConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ConnectionGeneration(pub(super) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum McpMetadataSource {
    Persisted,
    Connection(ConnectionGeneration),
}

#[derive(Clone, Debug)]
pub(super) struct McpMetadataSnapshot {
    pub(super) tools: Vec<rmcp::model::Tool>,
    pub(super) resources: Vec<rmcp::model::Resource>,
    pub(super) prompts: Vec<rmcp::model::Prompt>,
    pub(super) source: McpMetadataSource,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum McpMetadataAccess {
    Startup,
    ActiveConnection(ConnectionGeneration),
    RefreshIfMissing(Option<ConnectionGeneration>),
}

struct ActiveMetadata {
    generation: ConnectionGeneration,
    snapshot: McpMetadataSnapshot,
}

pub(super) struct McpMetadataCatalog {
    persisted: McpMetadataCache,
    active: HashMap<String, ActiveMetadata>,
}

impl McpMetadataCatalog {
    pub(super) fn new(persisted: McpMetadataCache) -> Self {
        Self {
            persisted,
            active: HashMap::new(),
        }
    }

    pub(super) fn snapshot(
        &self,
        server: &str,
        config: &McpServerConfig,
        access: McpMetadataAccess,
    ) -> Option<McpMetadataSnapshot> {
        match access {
            McpMetadataAccess::Startup => self.persisted_snapshot(server, config),
            McpMetadataAccess::ActiveConnection(generation) => {
                self.active_snapshot(server, generation)
            }
            McpMetadataAccess::RefreshIfMissing(generation) => generation
                .and_then(|generation| self.active_snapshot(server, generation))
                .or_else(|| self.persisted_snapshot(server, config)),
        }
    }

    pub(super) fn set_active(
        &mut self,
        server: &str,
        generation: ConnectionGeneration,
        config: &McpServerConfig,
        tools: (Vec<rmcp::model::Tool>, McpCachePolicy),
        resources: (Vec<rmcp::model::Resource>, McpCachePolicy),
        prompts: (Vec<rmcp::model::Prompt>, McpCachePolicy),
    ) -> bool {
        if self
            .active
            .get(server)
            .is_some_and(|active| active.generation != generation)
        {
            return false;
        }
        let snapshot = McpMetadataSnapshot {
            tools: tools.0.clone(),
            resources: resources.0.clone(),
            prompts: prompts.0.clone(),
            source: McpMetadataSource::Connection(generation),
        };
        self.persisted
            .update_metadata(server, config, tools, resources, prompts);
        self.active.insert(
            server.to_string(),
            ActiveMetadata {
                generation,
                snapshot,
            },
        );
        true
    }

    pub(super) fn update_tools(
        &mut self,
        server: &str,
        generation: ConnectionGeneration,
        tools: Vec<rmcp::model::Tool>,
        policy: McpCachePolicy,
    ) -> bool {
        let Some(active) = self
            .active
            .get_mut(server)
            .filter(|active| active.generation == generation)
        else {
            return false;
        };
        active.snapshot.tools = tools.clone();
        self.persisted.update_tools(server, tools, policy);
        true
    }

    pub(super) fn update_resources(
        &mut self,
        server: &str,
        generation: ConnectionGeneration,
        resources: Vec<rmcp::model::Resource>,
        policy: McpCachePolicy,
    ) -> bool {
        let Some(active) = self
            .active
            .get_mut(server)
            .filter(|active| active.generation == generation)
        else {
            return false;
        };
        active.snapshot.resources = resources.clone();
        self.persisted.update_resources(server, resources, policy);
        true
    }

    pub(super) fn update_prompts(
        &mut self,
        server: &str,
        generation: ConnectionGeneration,
        prompts: Vec<rmcp::model::Prompt>,
        policy: McpCachePolicy,
    ) -> bool {
        let Some(active) = self
            .active
            .get_mut(server)
            .filter(|active| active.generation == generation)
        else {
            return false;
        };
        active.snapshot.prompts = prompts.clone();
        self.persisted.update_prompts(server, prompts, policy);
        true
    }

    pub(super) fn remove_active(&mut self, server: &str, generation: ConnectionGeneration) {
        if self
            .active
            .get(server)
            .is_some_and(|active| active.generation == generation)
        {
            self.active.remove(server);
        }
    }

    pub(super) fn protocol_hint(
        &self,
        server: &str,
        config: &McpServerConfig,
    ) -> Option<McpProtocolHint> {
        self.persisted.protocol_hint(server, config)
    }

    pub(super) fn set_protocol_hint(
        &mut self,
        server: &str,
        config: &McpServerConfig,
        protocol: McpProtocolHint,
    ) {
        self.persisted.set_protocol_hint(server, config, protocol);
    }

    pub(super) fn has_reusable_direct_metadata(
        &self,
        server: &str,
        config: &McpServerConfig,
        generation: Option<ConnectionGeneration>,
    ) -> bool {
        generation.is_some_and(|generation| self.active_snapshot(server, generation).is_some())
            || self.persisted.has_reusable_direct_metadata(server, config)
    }

    pub(super) fn persisted_clone(&self) -> McpMetadataCache {
        self.persisted.clone()
    }

    fn active_snapshot(
        &self,
        server: &str,
        generation: ConnectionGeneration,
    ) -> Option<McpMetadataSnapshot> {
        self.active
            .get(server)
            .filter(|active| active.generation == generation)
            .map(|active| active.snapshot.clone())
    }

    fn persisted_snapshot(
        &self,
        server: &str,
        config: &McpServerConfig,
    ) -> Option<McpMetadataSnapshot> {
        let (tools, resources, prompts) = self.persisted.snapshot_for(server, config)?;
        Some(McpMetadataSnapshot {
            tools,
            resources,
            prompts,
            source: McpMetadataSource::Persisted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CacheScope;

    #[test]
    fn active_generation_accepts_transient_metadata_and_rejects_stale_updates() {
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "test",
            "exposeResources": true
        }))
        .unwrap();
        let mut catalog = McpMetadataCatalog::new(McpMetadataCache::default());
        let generation = ConnectionGeneration(2);
        let resource = rmcp::model::Resource::new("file:///private", "Private");
        assert!(catalog.set_active(
            "server",
            generation,
            &config,
            (Vec::new(), McpCachePolicy::transient()),
            (vec![resource], McpCachePolicy::transient()),
            (Vec::new(), McpCachePolicy::transient()),
        ));

        let snapshot = catalog
            .snapshot(
                "server",
                &config,
                McpMetadataAccess::ActiveConnection(generation),
            )
            .unwrap();
        assert_eq!(snapshot.resources[0].uri, "file:///private");
        assert!(!catalog.update_tools(
            "server",
            ConnectionGeneration(1),
            Vec::new(),
            McpCachePolicy::new(1, CacheScope::Public),
        ));
    }
}
