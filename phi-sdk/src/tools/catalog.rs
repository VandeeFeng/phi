use super::policy::{ToolAvailabilityPolicy, ToolExposure, canonical_tool_name};
use crate::config::{AgentMode, CodemodeConfig};
use crate::core::event::ToolResultDetails;
use crate::permission::runtime::PermissionRuntime;
use compact_str::CompactString;
use rig::completion::ToolDefinition;
use rig::tool::{DynamicTool, Tool, ToolContext, ToolResult, ToolSet};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

pub(crate) struct ToolResultFinalizeContext<'a> {
    pub tool_name: &'a str,
    pub session_id: Option<&'a CompactString>,
    pub session_entry_id: &'a CompactString,
}

/// Commits durable tool effects before the result enters provider history.
/// A failure therefore becomes the same tool error for the model, transcript, and adapters.
pub(crate) trait ToolResultFinalizer: Send + Sync {
    fn finalize(&self, context: ToolResultFinalizeContext<'_>) -> Result<(), String>;
}

#[derive(Clone, Debug, Default)]
pub struct ToolMetadata {
    pub prompt: ToolPromptMetadata,
    pub runtime: ToolRuntimeMetadata,
}

impl ToolMetadata {
    pub fn prompt(prompt: ToolPromptMetadata) -> Self {
        Self {
            prompt,
            ..Self::default()
        }
    }

    pub fn sequential(prompt: ToolPromptMetadata) -> Self {
        Self {
            prompt,
            runtime: ToolRuntimeMetadata {
                execution_mode: ToolExecutionMode::Sequential,
                ..ToolRuntimeMetadata::default()
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolPromptMetadata {
    pub snippet: Option<String>,
    pub guidelines: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ToolRuntimeMetadata {
    pub execution_mode: ToolExecutionMode,
    pub exposure: ToolExposure,
    pub durable_finalization: bool,
    pub terminate_on_success: bool,
    pub(crate) authorization: Option<ToolAuthorization>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolAuthorization {
    Invocation,
    ResolvedPath,
    CommandEvidence,
    McpOperation,
    AgentOperation,
    TaskOperation,
    MemorySync,
    TapeMutation,
}

impl ToolMetadata {
    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.runtime.exposure = exposure;
        self
    }

    pub(crate) fn with_authorization(mut self, authorization: ToolAuthorization) -> Self {
        self.runtime.authorization = Some(authorization);
        self
    }

    pub fn with_durable_finalization(mut self) -> Self {
        self.runtime.durable_finalization = true;
        self
    }
}

impl ToolPromptMetadata {
    pub fn new(snippet: impl Into<String>) -> Self {
        Self {
            snippet: Some(snippet.into()),
            guidelines: Vec::new(),
        }
    }

    pub fn with_guidelines(
        mut self,
        guidelines: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.guidelines = guidelines.into_iter().map(Into::into).collect();
        self
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct ToolAuthorityId(Arc<str>);

impl ToolAuthorityId {
    pub(crate) fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }
}

pub(crate) trait ToolAuthority: Send + Sync {
    fn is_available(&self, capability: &str) -> bool;
    fn ensure_available(&self, capability: &str) -> Result<(), String>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ToolOwner {
    #[default]
    Phi,
}

#[derive(Clone)]
pub(super) enum ResolvedToolGovernance {
    Phi {
        owner: ToolOwner,
    },
    Delegated {
        owner: ToolAuthorityId,
        capability: String,
        authority: Arc<dyn ToolAuthority>,
    },
}

pub(crate) enum ToolRegistrationGovernance {
    Phi {
        owner: ToolOwner,
    },
    Delegated {
        owner: ToolAuthorityId,
        capability: String,
        authority: Arc<dyn ToolAuthority>,
    },
}

pub(crate) struct ToolRegistration {
    name: String,
    namespace: Option<String>,
    install: Box<dyn FnOnce(&mut ToolSet) + Send>,
    pub(crate) source: String,
    pub(crate) metadata: ToolMetadata,
    pub(crate) governance: ToolRegistrationGovernance,
    pub(crate) override_existing: bool,
    finalizer: Option<Arc<dyn ToolResultFinalizer>>,
}

impl ToolRegistration {
    pub(crate) fn with_finalizer(mut self, finalizer: Arc<dyn ToolResultFinalizer>) -> Self {
        self.metadata = self.metadata.with_durable_finalization();
        self.finalizer = Some(finalizer);
        self
    }

    pub(crate) fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    pub(crate) fn phi<T>(
        tool: T,
        source: impl Into<String>,
        owner: ToolOwner,
        metadata: ToolMetadata,
    ) -> Self
    where
        T: Tool + 'static,
    {
        Self::new(
            T::NAME,
            move |tools| {
                tools.add_tool(tool);
            },
            source,
            metadata,
            ToolRegistrationGovernance::Phi { owner },
        )
    }

    pub(crate) fn phi_dynamic(
        tool: DynamicTool,
        source: impl Into<String>,
        owner: ToolOwner,
        metadata: ToolMetadata,
    ) -> Self {
        let name = tool.name().to_string();
        Self::new(
            name,
            move |tools| {
                tools.add_dynamic_tool(tool);
            },
            source,
            metadata,
            ToolRegistrationGovernance::Phi { owner },
        )
    }

    pub(crate) fn delegated(
        tool: DynamicTool,
        source: impl Into<String>,
        owner: ToolAuthorityId,
        capability: String,
        authority: Arc<dyn ToolAuthority>,
        metadata: ToolMetadata,
    ) -> Self {
        let name = tool.name().to_string();
        Self::new(
            name,
            move |tools| {
                tools.add_dynamic_tool(tool);
            },
            source,
            metadata,
            ToolRegistrationGovernance::Delegated {
                owner,
                capability,
                authority,
            },
        )
    }

    fn new(
        name: impl AsRef<str>,
        install: impl FnOnce(&mut ToolSet) + Send + 'static,
        source: impl Into<String>,
        metadata: ToolMetadata,
        governance: ToolRegistrationGovernance,
    ) -> Self {
        Self {
            name: canonical_tool_name(name),
            namespace: None,
            install: Box::new(install),
            source: source.into(),
            metadata,
            governance,
            override_existing: false,
            finalizer: None,
        }
    }
}

impl From<crate::extension::ExtensionToolRegistration> for ToolRegistration {
    fn from(registration: crate::extension::ExtensionToolRegistration) -> Self {
        let source = format!("extension:{}", registration.owner.as_ref());
        let owner = ToolAuthorityId::new(registration.owner.as_ref());
        let mut resolved = Self::delegated(
            registration.tool,
            source,
            owner,
            registration.capability,
            registration.authority,
            registration.metadata,
        );
        if let Some(namespace) = registration.namespace {
            resolved = resolved.with_namespace(namespace);
        }
        resolved.override_existing = registration.override_existing;
        resolved
    }
}

struct PreparedToolRegistration {
    name: String,
    namespace: Option<String>,
    metadata: ToolMetadata,
    governance: ToolRegistrationGovernance,
    finalizer: Option<Arc<dyn ToolResultFinalizer>>,
}

#[derive(Clone)]
struct ResolvedTool {
    tools: Arc<ToolSet>,
    name: String,
    namespace: Option<String>,
    definition: ToolDefinition,
    metadata: ToolMetadata,
    governance: ResolvedToolGovernance,
    finalizer: Option<Arc<dyn ToolResultFinalizer>>,
}

fn discovery_definition(entry: &ResolvedTool) -> phi_codemode::ToolDefinition {
    let mut description = entry.definition.description.clone();
    for guideline in &entry.metadata.prompt.guidelines {
        description.push_str(&format!("\n- {guideline}"));
    }
    phi_codemode::ToolDefinition {
        name: entry.name.clone(),
        description,
        input_schema: Some(entry.definition.parameters.clone()),
        output_schema: None,
    }
}

pub(crate) struct ToolCatalogBuilder {
    registrations: Vec<ToolRegistration>,
    globally_disabled: bool,
    permission: Option<PermissionRuntime>,
}

impl ToolCatalogBuilder {
    pub(crate) fn new(globally_disabled: bool) -> Self {
        Self {
            registrations: Vec::new(),
            globally_disabled,
            permission: Some(PermissionRuntime::open()),
        }
    }

    pub(crate) fn permission(&mut self, permission: Option<PermissionRuntime>) {
        self.permission = permission;
    }

    pub(crate) fn register(&mut self, registration: ToolRegistration) {
        self.registrations.push(registration);
    }

    pub(crate) fn build(self) -> anyhow::Result<ToolRegistry> {
        let registrations = resolve_registrations(self.registrations)?;
        let permission = self.permission.ok_or_else(|| {
            anyhow::anyhow!("tool catalog requires an explicit permission policy")
        })?;
        for registration in &registrations {
            anyhow::ensure!(
                registration.metadata.runtime.authorization.is_some(),
                "tool `{}` has no authorization declaration",
                registration.name
            );
            anyhow::ensure!(
                registration.metadata.runtime.durable_finalization
                    == registration.finalizer.is_some(),
                "tool `{}` requires a registration-bound durable finalizer",
                registration.name
            );
        }
        let mut prepared = Vec::with_capacity(registrations.len());
        let mut toolset = ToolSet::default();
        for registration in registrations {
            prepared.push(PreparedToolRegistration {
                name: registration.name.clone(),
                namespace: registration.namespace,
                metadata: registration.metadata,
                governance: registration.governance,
                finalizer: registration.finalizer,
            });
            (registration.install)(&mut toolset);
        }
        let toolset = Arc::new(toolset);
        let entries = resolve_tools(&toolset, prepared)?;
        Ok(ToolRegistry::from_entries(
            toolset,
            entries,
            self.globally_disabled,
            permission,
        ))
    }
}

fn resolve_tools(
    tools: &Arc<ToolSet>,
    registrations: Vec<PreparedToolRegistration>,
) -> anyhow::Result<Vec<ResolvedTool>> {
    let mut definitions = tools
        .get_tool_definitions()
        .into_iter()
        .map(|definition| (canonical_tool_name(&definition.name), definition))
        .collect::<HashMap<_, _>>();
    let mut entries = Vec::with_capacity(registrations.len());
    for registration in registrations {
        let name = registration.name;
        let definition = definitions
            .remove(&name)
            .ok_or_else(|| anyhow::anyhow!("tool `{name}` produced no definition"))?;
        let governance = match registration.governance {
            ToolRegistrationGovernance::Phi { owner } => ResolvedToolGovernance::Phi { owner },
            ToolRegistrationGovernance::Delegated {
                owner,
                capability,
                authority,
            } => ResolvedToolGovernance::Delegated {
                owner,
                capability,
                authority,
            },
        };
        entries.push(ResolvedTool {
            tools: tools.clone(),
            name,
            namespace: registration.namespace,
            definition,
            metadata: registration.metadata,
            governance,
            finalizer: registration.finalizer,
        });
    }
    if let Some(name) = definitions.keys().next() {
        anyhow::bail!("tool definition `{name}` has no registration");
    }
    Ok(entries)
}

fn resolve_registrations(
    registrations: Vec<ToolRegistration>,
) -> anyhow::Result<Vec<ToolRegistration>> {
    let mut indexes = HashMap::new();
    let mut authorities: HashMap<ToolAuthorityId, Arc<dyn ToolAuthority>> = HashMap::new();
    let mut resolved: Vec<ToolRegistration> = Vec::new();
    for registration in registrations {
        validate_authority(&registration, &mut authorities)?;
        let name = registration.name.clone();
        let Some(&index) = indexes.get(&name) else {
            indexes.insert(name, resolved.len());
            resolved.push(registration);
            continue;
        };
        let previous = &resolved[index];
        if matches!(
            (previous_extension_owner(previous), previous_extension_owner(&registration)),
            (Some(left), Some(right)) if left != right
        ) {
            anyhow::bail!(
                "tool `{name}` from {} conflicts with {}; extensions cannot override each other",
                registration.source,
                previous.source
            );
        }
        if registration.override_existing {
            tracing::warn!(
                "tool `{name}` from {} overrides {}",
                registration.source,
                previous.source
            );
            resolved[index] = registration;
        } else if previous.override_existing && previous_extension_owner(previous).is_some() {
            tracing::warn!(
                "tool `{name}` from {} overrides {}",
                previous.source,
                registration.source
            );
        } else {
            anyhow::bail!(
                "tool `{name}` from {} conflicts with {}; set override = true to replace it",
                registration.source,
                previous.source
            );
        }
    }
    Ok(resolved)
}

fn validate_authority(
    registration: &ToolRegistration,
    authorities: &mut HashMap<ToolAuthorityId, Arc<dyn ToolAuthority>>,
) -> anyhow::Result<()> {
    let ToolRegistrationGovernance::Delegated {
        owner, authority, ..
    } = &registration.governance
    else {
        return Ok(());
    };
    if let Some(existing) = authorities.get(owner)
        && !Arc::ptr_eq(existing, authority)
    {
        anyhow::bail!(
            "extension `{}` registered multiple tool authorities",
            owner.0
        );
    }
    authorities.insert(owner.clone(), authority.clone());
    Ok(())
}

fn previous_extension_owner(registration: &ToolRegistration) -> Option<&ToolAuthorityId> {
    match &registration.governance {
        ToolRegistrationGovernance::Delegated { owner, .. } => Some(owner),
        ToolRegistrationGovernance::Phi {
            owner: ToolOwner::Phi,
        } => None,
    }
}

fn append_schema_text(schema: &serde_json::Value, text: &mut String) {
    if let Some(description) = schema
        .get("description")
        .and_then(serde_json::Value::as_str)
    {
        text.push(' ');
        text.push_str(description);
    }
    if let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        for (name, property) in properties {
            text.push(' ');
            text.push_str(name);
            append_schema_text(property, text);
        }
    }
    if let Some(items) = schema.get("items") {
        append_schema_text(items, text);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = schema.get(key).and_then(serde_json::Value::as_array) {
            for variant in variants {
                append_schema_text(variant, text);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ScriptToolResult(pub(crate) serde_json::Value);

#[derive(Clone)]
pub(crate) struct ToolTurnEnd;

pub(crate) struct ToolCallOutput {
    pub(crate) result: ToolResult,
    pub(crate) details: Option<ToolResultDetails>,
    pub(crate) script_value: Option<serde_json::Value>,
    pub(crate) terminate_on_success: bool,
}

struct ToolSearchIndex {
    documents: Vec<bm25::Document<String>>,
    engine: bm25::SearchEngine<String>,
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<ToolSet>,
    entries: Vec<ResolvedTool>,
    indexes: HashMap<String, usize>,
    permission: PermissionRuntime,
    availability: ToolAvailabilityPolicy,
    codemode: CodemodeConfig,
    mcp_manager: Option<crate::extension::mcp::McpClientManager>,
    loaded_tools: std::collections::BTreeSet<String>,
    search_cache: Arc<Mutex<Option<ToolSearchIndex>>>,
}

impl ToolRegistry {
    fn from_entries(
        tools: Arc<ToolSet>,
        entries: Vec<ResolvedTool>,
        globally_disabled: bool,
        permission: PermissionRuntime,
    ) -> Self {
        let indexes = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.name.clone(), index))
            .collect();
        Self {
            tools,
            entries,
            indexes,
            permission,
            availability: ToolAvailabilityPolicy::new(globally_disabled),
            codemode: CodemodeConfig::default(),
            mcp_manager: None,
            loaded_tools: Default::default(),
            search_cache: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn outer_turn_policy(
        &self,
        intent: crate::core::turn::TurnToolPolicy,
    ) -> crate::permission::execution_scope::OuterTurnPolicy {
        self.permission
            .outer_turn_policy(intent)
            .with_tools(self.tools.clone())
    }

    pub(crate) fn validate_turn_policy(
        &self,
        policy: &crate::core::turn::TurnToolPolicy,
    ) -> Result<(), crate::tools::ToolRuleError> {
        policy.validate()?;
        for name in policy
            .allowances
            .iter()
            .map(|(name, _)| name)
            .chain(&policy.removals)
        {
            if self.entry(name).is_some_and(|entry| {
                matches!(&entry.governance, ResolvedToolGovernance::Delegated { owner, .. } if owner.0.as_ref() == "mcp")
            }) {
                return Err(crate::tools::ToolRuleError::UnsupportedMcp);
            }
        }
        Ok(())
    }

    pub fn with_policy(mut self, selected: Option<Vec<String>>, excluded: Vec<String>) -> Self {
        self.availability.set_agent_policy(selected, excluded);
        self
    }

    pub(crate) fn with_mcp_manager(
        mut self,
        manager: Option<crate::extension::mcp::McpClientManager>,
    ) -> Self {
        self.mcp_manager = manager;
        self
    }

    pub(crate) async fn discovery_registry(&self) -> anyhow::Result<Self> {
        let Some(manager) = self
            .mcp_manager
            .as_ref()
            .filter(|_| !self.availability.globally_disabled)
        else {
            return Ok(self.clone());
        };
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.permission(Some(self.permission.clone()));
        for registration in manager
            .deferred_registrations(self.permission.clone())
            .await
        {
            if self.entry(registration.tool.name()).is_none() {
                catalog.register(registration.into());
            }
        }
        if catalog.registrations.is_empty() {
            return Ok(self.clone());
        }
        let additions = catalog.build()?;
        // Preserve outer-turn policy identity; each scoped entry owns its dispatch set.
        let mut scoped = self.clone();
        for entry in additions.entries {
            scoped
                .indexes
                .insert(entry.name.clone(), scoped.entries.len());
            scoped.entries.push(entry);
        }
        Ok(scoped)
    }

    pub(crate) async fn loaded_registry(
        &self,
        names: std::collections::BTreeSet<String>,
    ) -> anyhow::Result<Self> {
        let mut projected = if names.iter().any(|name| self.entry(name).is_none()) {
            self.discovery_registry().await?
        } else {
            self.clone()
        };
        projected.loaded_tools = names;
        Ok(projected)
    }

    pub(crate) fn search_tools(
        &self,
        tools: &[phi_codemode::ToolDefinition],
        query: &str,
        namespace: Option<&str>,
        limit: usize,
    ) -> Vec<phi_codemode::ToolDefinition> {
        let candidates = tools
            .iter()
            .filter(|tool| {
                namespace.is_none_or(|namespace| {
                    self.tool_namespace(&tool.name).is_some_and(|name| {
                        name == namespace || phi_codemode::to_codemode_identifier(name) == namespace
                    })
                })
            })
            .collect::<Vec<_>>();
        let documents = candidates
            .iter()
            .map(|tool| {
                let mut text = format!(
                    "{} {} {} {}",
                    tool.name,
                    tool.name.replace('_', " "),
                    tool.description,
                    self.tool_namespace(&tool.name).unwrap_or_default()
                );
                if let Some(schema) = &tool.input_schema {
                    append_schema_text(schema, &mut text);
                }
                bm25::Document::new(tool.name.clone(), text)
            })
            .collect::<Vec<_>>();
        if documents.is_empty() {
            return Vec::new();
        }
        let mut cache = self
            .search_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = match cache.as_mut() {
            Some(index) if index.documents == documents => index,
            _ => {
                let engine = bm25::SearchEngineBuilder::with_documents(
                    bm25::Language::English,
                    documents.clone(),
                )
                .build();
                cache.insert(ToolSearchIndex { documents, engine })
            }
        };
        index
            .engine
            .search(query, limit.min(candidates.len()))
            .into_iter()
            .filter_map(|found| {
                candidates
                    .iter()
                    .find(|tool| tool.name == found.document.id)
                    .map(|tool| (*tool).clone())
            })
            .collect()
    }

    pub(crate) fn deferred_declarations(&self) -> Vec<phi_codemode::ToolDefinition> {
        self.entries
            .iter()
            .filter(|entry| {
                self.is_available(&entry.name)
                    && entry.metadata.runtime.exposure == ToolExposure::Deferred
                    && !self.is_declared(&entry.name)
            })
            .map(discovery_definition)
            .collect()
    }

    pub(crate) fn with_codemode_config(mut self, config: CodemodeConfig) -> Self {
        self.codemode = config;
        self
    }

    pub(crate) fn codemode_cache_dir(&self) -> Option<PathBuf> {
        self.codemode
            .cache
            .directory(&crate::utils::path::phi_config_dir())
    }

    pub(crate) fn with_mode(mut self, mode: AgentMode) -> Self {
        self.availability.set_mode(mode);
        self
    }

    pub(crate) fn is_available(&self, name: &str) -> bool {
        let name = canonical_tool_name(name);
        let removed =
            crate::permission::execution_scope::OuterTurnPolicy::is_removed(&self.tools, &name);
        !removed
            && self.entry(&name).is_some_and(|entry| {
                entry.metadata.runtime.exposure != ToolExposure::Hidden
                    && self
                        .availability
                        .is_available(&entry.name, &entry.governance)
            })
    }

    pub(crate) fn is_declared(&self, name: &str) -> bool {
        self.is_available(name)
            && self.entry(name).is_some_and(|entry| {
                entry.metadata.runtime.exposure == ToolExposure::Direct
                    || self.loaded_tools.contains(&entry.name)
            })
    }

    pub fn prompt_summary(&self) -> String {
        let lines = self
            .entries
            .iter()
            .filter(|entry| self.is_declared(&entry.name))
            .map(|entry| {
                let snippet = entry
                    .metadata
                    .prompt
                    .snippet
                    .clone()
                    .or_else(|| short_tool_description(&entry.definition.description));
                match snippet {
                    Some(snippet) => format!("- {}: {snippet}", entry.definition.name),
                    None => format!("- {}", entry.definition.name),
                }
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            "(none)".to_string()
        } else {
            lines.join("\n")
        }
    }

    pub fn prompt_guidelines(&self) -> String {
        self.entries
            .iter()
            .filter(|entry| self.is_declared(&entry.name))
            .flat_map(|entry| entry.metadata.prompt.guidelines.iter())
            .map(|guideline| format!("- {guideline}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn provider_declarations(&self) -> Vec<ToolDefinition> {
        self.entries
            .iter()
            .filter(|entry| self.is_declared(&entry.name))
            .map(|entry| {
                let mut definition = entry.definition.clone();
                if entry.name == "Codemode"
                    && matches!(
                        entry.governance,
                        ResolvedToolGovernance::Phi {
                            owner: ToolOwner::Phi
                        }
                    )
                {
                    definition.description = crate::extension::codemode::description(
                        &self
                            .script_declarations()
                            .into_iter()
                            .filter(|tool| {
                                self.entry(&tool.name).is_some_and(|entry| {
                                    entry.metadata.runtime.exposure == ToolExposure::Direct
                                })
                            })
                            .collect::<Vec<_>>(),
                        self.codemode.inline_budget,
                    );
                }
                definition
            })
            .collect()
    }

    pub(crate) fn tool_namespace(&self, name: &str) -> Option<&str> {
        self.entry(name)?.namespace.as_deref()
    }

    pub(crate) fn is_script_callable(&self, name: &str) -> bool {
        let name = canonical_tool_name(name);
        !matches!(name.as_str(), "Codemode" | "ToolSearch")
            && self.is_available(&name)
            && self.entry(&name).is_some_and(|entry| {
                !entry.metadata.runtime.durable_finalization
                    && entry.metadata.runtime.authorization != Some(ToolAuthorization::TapeMutation)
            })
    }

    pub(crate) fn script_declarations(&self) -> Vec<phi_codemode::ToolDefinition> {
        self.entries
            .iter()
            .filter(|entry| self.is_script_callable(&entry.name))
            .map(discovery_definition)
            .collect()
    }

    pub(crate) fn finalize_result(
        &self,
        context: ToolResultFinalizeContext<'_>,
    ) -> Result<(), String> {
        self.entry(context.tool_name)
            .and_then(|entry| entry.finalizer.as_ref())
            .map_or(Ok(()), |finalizer| finalizer.finalize(context))
    }

    pub(crate) async fn call(
        &self,
        tool_name: &str,
        args: &str,
        mut context: ToolContext,
    ) -> Result<ToolCallOutput, String> {
        let canonical_name = canonical_tool_name(tool_name);
        if !self.is_available(&canonical_name) {
            return Err(format!("tool `{tool_name}` is not allowed for this agent"));
        }
        let entry = self
            .entry(&canonical_name)
            .ok_or_else(|| format!("unknown tool `{tool_name}`"))?;
        if let ResolvedToolGovernance::Delegated {
            capability,
            authority,
            ..
        } = &entry.governance
        {
            authority.ensure_available(capability)?;
        }
        match entry.metadata.runtime.authorization {
            Some(ToolAuthorization::Invocation) => {
                self.permission
                    .authorize_tool(&canonical_name, args, None, None)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Some(_) => {}
            None => {
                return Err(format!(
                    "tool `{canonical_name}` has no authorization declaration"
                ));
            }
        }
        let result = entry
            .tools
            .execute(&canonical_name, args, &mut context)
            .await;
        let details = context.result::<ToolResultDetails>().cloned();
        let script_value = context
            .result::<ScriptToolResult>()
            .map(|value| value.0.clone());
        Ok(ToolCallOutput {
            result,
            details,
            script_value,
            terminate_on_success: entry.metadata.runtime.terminate_on_success
                || context.result::<ToolTurnEnd>().is_some(),
        })
    }

    fn entry(&self, tool_name: &str) -> Option<&ResolvedTool> {
        self.indexes
            .get(&canonical_tool_name(tool_name))
            .map(|index| &self.entries[*index])
    }

    pub(crate) fn execution_mode(&self, tool_name: &str) -> ToolExecutionMode {
        self.entry(tool_name)
            .map(|entry| entry.metadata.runtime.execution_mode)
            .unwrap_or_default()
    }
}

fn short_tool_description(description: &str) -> Option<String> {
    let summary = description.split('\n').next().unwrap_or_default().trim();
    if summary.is_empty() {
        return None;
    }
    let mut summary = summary.trim_end_matches('.').to_string();
    const MAX_CHARS: usize = 160;
    if summary.chars().count() > MAX_CHARS {
        summary = summary.chars().take(MAX_CHARS - 1).collect::<String>();
        summary.push('…');
    }
    Some(summary)
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use compact_str::CompactString;
    use rig::tool::DynamicTool;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_registry(definitions: Vec<ToolDefinition>) -> ToolRegistry {
        let mut catalog = ToolCatalogBuilder::new(false);
        for definition in definitions {
            let prompt = match definition.name.as_str() {
                "Read" => ToolPromptMetadata::new("Read file contents")
                    .with_guidelines(["Use Read before editing."]),
                "Bash" => ToolPromptMetadata::new("Execute bash commands")
                    .with_guidelines(["Use Bash for tests."]),
                _ => ToolPromptMetadata::default(),
            };
            catalog.register(ToolRegistration::phi_dynamic(
                DynamicTool::new(
                    definition.name,
                    definition.description,
                    definition.parameters,
                    |_context, _args| Box::pin(async { Ok(rig::tool::ToolOutput::text("called")) }),
                ),
                "test",
                ToolOwner::Phi,
                ToolMetadata::prompt(prompt).with_authorization(ToolAuthorization::Invocation),
            ));
        }
        catalog.build().unwrap()
    }

    fn tool_definition(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    fn tool_names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .provider_declarations()
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    }

    #[tokio::test]
    async fn outer_turn_removal_agrees_across_projections_and_dispatch() {
        let mut registry = test_registry(vec![
            tool_definition("Read", "Read"),
            tool_definition("Bash", "Bash"),
        ]);
        registry.permission = PermissionRuntime::new(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfig {
                    mode: crate::permission::PermissionMode::Yolo,
                    ..Default::default()
                },
                None,
            ),
        )));
        let scope = registry.outer_turn_policy(crate::core::turn::TurnToolPolicy {
            allowances: vec![("Read".into(), "**".into())],
            removals: vec!["read".into()],
        });
        scope
            .run(async {
                assert!(!registry.is_available("Read"));
                assert_eq!(tool_names(&registry), ["Bash"]);
                assert!(!registry.prompt_summary().contains("Read"));
                assert!(!registry.prompt_guidelines().contains("Read"));
                assert!(
                    registry
                        .call("read", "{}", ToolContext::new())
                        .await
                        .is_err()
                );
                assert!(
                    registry
                        .call("Bash", "{}", ToolContext::new())
                        .await
                        .is_ok()
                );
            })
            .await;
        assert!(registry.is_available("Read"));
        assert!(
            registry
                .call("Read", "{}", ToolContext::new())
                .await
                .is_ok()
        );
        for policy in [
            crate::core::turn::TurnToolPolicy {
                removals: vec!["mcp__server__tool".into()],
                ..Default::default()
            },
            crate::core::turn::TurnToolPolicy {
                allowances: vec![("Read".into(), "".into())],
                ..Default::default()
            },
        ] {
            assert!(registry.validate_turn_policy(&policy).is_err());
        }
    }

    #[test]
    fn resolved_registration_requires_an_authorization_declaration() {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(ToolRegistration::phi_dynamic(
            test_tool("unclassified", "Unclassified"),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default(),
        ));
        assert!(
            catalog.build().is_err(),
            "missing authorization declaration was accepted"
        );
    }

    #[test]
    fn catalog_rejects_missing_runtime_even_with_a_declared_adapter() {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.permission(None);
        catalog.register(ToolRegistration::phi_dynamic(
            test_tool("adapter", "Adapter"),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::ResolvedPath),
        ));
        assert!(catalog.build().is_err());
    }

    #[tokio::test]
    async fn dispatch_authorization_wraps_declared_tools() {
        let mut registry = test_registry(vec![tool_definition("custom", "Custom tool")]);
        let checker = crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfig {
                mode: crate::permission::PermissionMode::Deny,
                ..Default::default()
            },
            None,
        );
        registry.permission = PermissionRuntime::new(Arc::new(std::sync::Mutex::new(checker)));

        let error = match registry.call("custom", "{}", ToolContext::new()).await {
            Ok(_) => panic!("permission denial must prevent tool execution"),
            Err(error) => error,
        };

        assert!(error.contains("denied"));
    }

    #[tokio::test]
    async fn mode_policy_filters_visibility_and_dispatch() {
        let definitions = || {
            vec![
                tool_definition("Read", "Read"),
                tool_definition("Write", "Write"),
                tool_definition("Edit", "Edit"),
                tool_definition("Bash", "Bash"),
            ]
        };
        let default = test_registry(definitions()).with_mode(AgentMode::Default);
        assert_eq!(tool_names(&default), ["Read", "Write", "Edit", "Bash"]);
        assert!(default.is_available("write"));

        let plan = test_registry(definitions())
            .with_policy(
                Some(vec!["Read".into(), "Write".into(), "Bash".into()]),
                Vec::new(),
            )
            .with_mode(AgentMode::Plan);
        assert_eq!(tool_names(&plan), ["Read", "Bash"]);
        assert!(!plan.is_available("write"));
        assert!(!plan.prompt_summary().contains("Write"));
        assert!(plan.call("Write", "{}", ToolContext::new()).await.is_err());
    }

    struct TestAuthority {
        visible: bool,
        authorized: bool,
    }

    fn test_tool(name: &str, description: &str) -> DynamicTool {
        DynamicTool::new(
            canonical_tool_name(name),
            description,
            serde_json::json!({ "type": "object" }),
            |_context, _args| Box::pin(async { Ok(rig::tool::ToolOutput::text("called")) }),
        )
    }

    impl ToolAuthority for TestAuthority {
        fn is_available(&self, _capability: &str) -> bool {
            self.visible
        }

        fn ensure_available(&self, _capability: &str) -> Result<(), String> {
            self.authorized
                .then_some(())
                .ok_or_else(|| "authority denied tool".to_string())
        }
    }

    fn delegated_registry(authority: TestAuthority) -> ToolRegistry {
        let mut registry = test_registry(vec![
            tool_definition("Read", "Read full description."),
            tool_definition("server_search", "MCP search"),
        ]);
        let index = registry.indexes["server_search"];
        registry.entries[index].governance = ResolvedToolGovernance::Delegated {
            owner: ToolAuthorityId::new("mcp"),
            capability: "server\0search".to_string(),
            authority: Arc::new(authority),
        };
        registry
    }

    #[tokio::test]
    async fn delegated_governance_and_global_disable_fail_closed() {
        let visible = delegated_registry(TestAuthority {
            visible: true,
            authorized: true,
        })
        .with_policy(
            Some(vec!["Read".to_string()]),
            vec!["server_search".to_string()],
        );
        assert!(visible.is_available("server_search"));
        assert_eq!(tool_names(&visible), ["Read", "server_search"]);
        assert!(visible.prompt_summary().contains("server_search"));
        for policy in [
            crate::core::turn::TurnToolPolicy {
                removals: vec!["server_search".into()],
                ..Default::default()
            },
            crate::core::turn::TurnToolPolicy {
                allowances: vec![("server_search".into(), "**".into())],
                ..Default::default()
            },
        ] {
            assert!(matches!(
                visible.validate_turn_policy(&policy),
                Err(crate::tools::ToolRuleError::UnsupportedMcp)
            ));
        }
        assert!(
            !delegated_registry(TestAuthority {
                visible: false,
                authorized: true,
            })
            .is_available("server_search")
        );

        let denied = delegated_registry(TestAuthority {
            visible: true,
            authorized: false,
        });
        assert_eq!(
            denied
                .call("server_search", "{}", ToolContext::new())
                .await
                .err()
                .unwrap(),
            "authority denied tool"
        );

        let mut disabled = visible;
        disabled.availability.globally_disabled = true;
        assert_eq!(disabled.prompt_summary(), "(none)");
        assert!(disabled.provider_declarations().is_empty());
        for tool_name in ["Read", "server_search"] {
            assert!(
                disabled
                    .call(tool_name, "{}", ToolContext::new())
                    .await
                    .is_err()
            );
        }
    }

    struct CountingFinalizer(AtomicUsize);

    impl ToolResultFinalizer for CountingFinalizer {
        fn finalize(&self, _context: ToolResultFinalizeContext<'_>) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn phi_test_registration(
        name: &'static str,
        source: &'static str,
        owner: ToolOwner,
    ) -> ToolRegistration {
        ToolRegistration::phi_dynamic(
            test_tool(name, source),
            source,
            owner,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        )
    }

    #[test]
    fn catalog_rejects_ambiguous_collisions() {
        let mut duplicate = ToolCatalogBuilder::new(false);
        duplicate.register(phi_test_registration("Read", "builtin", ToolOwner::Phi));
        duplicate.register(phi_test_registration("read", "memory", ToolOwner::Phi));
        assert!(
            duplicate
                .build()
                .err()
                .unwrap()
                .to_string()
                .contains("override = true")
        );

        let mut extensions = ToolCatalogBuilder::new(false);
        let authority = Arc::new(TestAuthority {
            visible: true,
            authorized: true,
        });
        let mut lua = ToolRegistration::delegated(
            test_tool("search", "Lua search"),
            "extension:lua",
            ToolAuthorityId::new("lua"),
            "search".into(),
            authority.clone(),
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        );
        lua.override_existing = true;
        extensions.register(lua);
        extensions.register(ToolRegistration::delegated(
            test_tool("search", "MCP search"),
            "extension:mcp",
            ToolAuthorityId::new("mcp"),
            "search".into(),
            authority,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        assert!(
            extensions
                .build()
                .err()
                .unwrap()
                .to_string()
                .contains("extensions cannot override")
        );
    }

    #[test]
    fn durable_tools_require_registration_bound_finalizers() {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(ToolRegistration::phi_dynamic(
            test_tool("durable", "durable tool"),
            "extension:archive",
            ToolOwner::Phi,
            ToolMetadata::default()
                .with_authorization(ToolAuthorization::Invocation)
                .with_durable_finalization(),
        ));
        assert!(
            catalog
                .build()
                .err()
                .unwrap()
                .to_string()
                .contains("registration-bound")
        );
    }

    proptest::proptest! {
        #[test]
        fn finalizers_follow_resolved_tools(calls in proptest::collection::vec(0_usize..3, 0..32)) {
            let mut catalog = ToolCatalogBuilder::new(false);
            let finalizers = [
                Arc::new(CountingFinalizer(AtomicUsize::new(0))),
                Arc::new(CountingFinalizer(AtomicUsize::new(0))),
            ];
            for (name, finalizer) in ["archive", "index"].into_iter().zip(&finalizers) {
                catalog.register(phi_test_registration(name, name, ToolOwner::Phi)
                    .with_finalizer(finalizer.clone()));
            }
            catalog.register(phi_test_registration("read", "reader", ToolOwner::Phi));
            let registry = catalog.build().unwrap();
            let entry_id = CompactString::new("preallocated-entry");
            let mut expected = [0; 2];
            for index in calls {
                registry.finalize_result(ToolResultFinalizeContext {
                    tool_name: ["archive", "index", "read"][index],
                    session_id: None,
                    session_entry_id: &entry_id,
                }).unwrap();
                if index < expected.len() { expected[index] += 1; }
            }
            for (finalizer, expected) in finalizers.iter().zip(expected) {
                proptest::prop_assert_eq!(finalizer.0.load(Ordering::Relaxed), expected);
            }
        }
    }

    #[test]
    fn finalizer_failure_preserves_other_tool_finalizers() {
        struct FailedFinalizer;
        impl ToolResultFinalizer for FailedFinalizer {
            fn finalize(&self, context: ToolResultFinalizeContext<'_>) -> Result<(), String> {
                assert_eq!(context.tool_name, "archive");
                assert_eq!(context.session_entry_id, "entry");
                Err("archive persistence failed".into())
            }
        }
        let mut catalog = ToolCatalogBuilder::new(false);
        let independent = Arc::new(CountingFinalizer(AtomicUsize::new(0)));
        catalog.register(
            phi_test_registration("archive", "archive", ToolOwner::Phi)
                .with_finalizer(Arc::new(FailedFinalizer)),
        );
        catalog.register(
            phi_test_registration("index", "index", ToolOwner::Phi)
                .with_finalizer(independent.clone()),
        );
        let registry = catalog.build().unwrap();
        let entry_id = CompactString::new("entry");
        let context = |tool_name| ToolResultFinalizeContext {
            tool_name,
            session_id: None,
            session_entry_id: &entry_id,
        };
        assert_eq!(
            registry.finalize_result(context("archive")),
            Err("archive persistence failed".into())
        );
        assert_eq!(independent.0.load(Ordering::Relaxed), 0);
        registry.finalize_result(context("index")).unwrap();
        assert_eq!(independent.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn override_replaces_definition_metadata_governance_and_finalization() {
        let mut catalog = ToolCatalogBuilder::new(false);
        let finalizer = Arc::new(CountingFinalizer(AtomicUsize::new(0)));
        catalog.register(
            ToolRegistration::phi_dynamic(
                test_tool("tape_anchor", "builtin definition"),
                "memory",
                ToolOwner::Phi,
                ToolMetadata::sequential(ToolPromptMetadata::new("builtin metadata"))
                    .with_authorization(ToolAuthorization::TapeMutation),
            )
            .with_finalizer(finalizer.clone()),
        );
        let mut replacement = ToolRegistration::delegated(
            test_tool("tape_anchor", "replacement definition"),
            "extension:mcp",
            ToolAuthorityId::new("mcp"),
            "server\0anchor".to_string(),
            Arc::new(TestAuthority {
                visible: false,
                authorized: true,
            }),
            ToolMetadata::prompt(ToolPromptMetadata::new("replacement metadata"))
                .with_authorization(ToolAuthorization::Invocation),
        );
        replacement.override_existing = true;
        catalog.register(replacement);
        let registry = catalog.build().unwrap();
        let entry = registry.entry("tape_anchor").unwrap();
        assert_eq!(entry.definition.description, "replacement definition");
        assert_eq!(
            entry.metadata.prompt.snippet.as_deref(),
            Some("replacement metadata")
        );
        assert!(!entry.metadata.runtime.durable_finalization);
        assert_eq!(
            entry.metadata.runtime.authorization,
            Some(ToolAuthorization::Invocation)
        );
        assert!(matches!(
            &entry.governance,
            ResolvedToolGovernance::Delegated { owner, .. }
                if owner == &ToolAuthorityId::new("mcp")
        ));
        assert!(!registry.is_available("tape_anchor"));

        let entry_id = CompactString::new("entry");
        registry
            .finalize_result(ToolResultFinalizeContext {
                tool_name: "tape_anchor",
                session_id: None,
                session_entry_id: &entry_id,
            })
            .unwrap();
        assert_eq!(finalizer.0.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn catalog_dispatches_aliases_and_model_safe_errors() {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(ToolRegistration::phi_dynamic(
            test_tool("read", "custom read"),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        catalog.register(ToolRegistration::phi_dynamic(
            DynamicTool::new(
                "Fail",
                "failing tool",
                serde_json::json!({ "type": "object" }),
                |_context, _args| {
                    Box::pin(async {
                        Err(rig::tool::ToolExecutionError::provider("operator detail")
                            .with_model_feedback("safe feedback"))
                    })
                },
            ),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        let registry = catalog.build().unwrap();

        assert_eq!(registry.provider_declarations()[0].name, "Read");
        assert_eq!(
            registry
                .call("read", "{}", ToolContext::new())
                .await
                .unwrap()
                .result
                .output()
                .as_text(),
            Some("called")
        );
        let failure = registry
            .call("Fail", "{}", ToolContext::new())
            .await
            .unwrap();
        assert!(failure.result.is_error());
        assert_eq!(failure.result.output().as_text(), Some("safe feedback"));
    }

    #[tokio::test]
    async fn search_cache_follows_loading_and_outer_turn_removals() {
        let mut registry = test_registry(vec![
            tool_definition("first", "Search documents"),
            tool_definition("second", "Search documents"),
        ]);
        for entry in &mut registry.entries {
            entry.metadata.runtime.exposure = ToolExposure::Deferred;
        }
        let names = |registry: &ToolRegistry| {
            let mut names = registry
                .search_tools(&registry.deferred_declarations(), "documents", None, 20)
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(&registry), ["first", "second"]);
        let snapshot = registry
            .search_cache
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .documents
            .as_ptr();
        assert_eq!(names(&registry.clone()), ["first", "second"]);
        assert_eq!(
            snapshot,
            registry
                .search_cache
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .documents
                .as_ptr()
        );
        let loaded = registry
            .loaded_registry(std::collections::BTreeSet::from(["first".into()]))
            .await
            .unwrap();
        assert_eq!(names(&loaded), ["second"]);
        assert_eq!(names(&registry), ["first", "second"]);
        registry
            .outer_turn_policy(crate::core::turn::TurnToolPolicy {
                removals: vec!["first".into()],
                ..Default::default()
            })
            .run(async {
                assert_eq!(names(&registry), ["second"]);
            })
            .await;
        assert_eq!(names(&registry), ["first", "second"]);
        assert!(
            names(
                &registry
                    .clone()
                    .with_policy(None, vec!["first".into(), "second".into()])
            )
            .is_empty()
        );
        assert_eq!(names(&registry), ["first", "second"]);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(24))]
        #[test]
        fn cached_tool_search_matches_fresh_candidates(
            changes in proptest::collection::vec((0_u8..8, 0_usize..3, 0_usize..3), 1..12)
        ) {
            let definitions = || vec![
                tool_definition("first", "Search documents"),
                tool_definition("second", "Search documents archives"),
                tool_definition("third", "Search archives"),
            ];
            let mut registry = test_registry(definitions());
            registry.entries[0].namespace = Some("documents".into());
            registry.entries[1].namespace = Some("documents".into());
            registry.entries[2].namespace = Some("archives".into());
            let mut tools = registry.script_declarations();
            for (mask, changed, namespace) in changes {
                tools[changed].description = if mask & 1 == 0 { "Search documents" } else { "Search archives" }.into();
                tools[changed].input_schema = Some(serde_json::json!({
                    "properties": {"query": {"description": format!("keyword{mask}")}}
                }));
                let selected = tools.iter().enumerate().filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, tool)| tool.clone()).collect::<Vec<_>>();
                let namespace = [None, Some("documents"), Some("archives")][namespace];
                let mut fresh = test_registry(definitions());
                for (entry, original) in fresh.entries.iter_mut().zip(&registry.entries) {
                    entry.namespace = original.namespace.clone();
                }
                let normalize = |matches: Vec<phi_codemode::ToolDefinition>| {
                    let mut matches = matches.into_iter().map(|tool| (tool.name, tool.description, tool.input_schema)).collect::<Vec<_>>();
                    matches.sort_by(|left, right| left.0.cmp(&right.0));
                    matches
                };
                for query in ["documents", "archives", "keyword1", "missing"] {
                    proptest::prop_assert_eq!(
                        normalize(registry.search_tools(&selected, query, namespace, 20)),
                        normalize(fresh.search_tools(&selected, query, namespace, 20))
                    );
                }
            }
        }
    }

    #[test]
    fn search_tracks_candidates_namespaces_and_schema_changes() {
        let mut registry = test_registry(vec![
            tool_definition("first", "Search documents"),
            tool_definition("second", "Search documents"),
        ]);
        registry.entries[0].namespace = Some("first-server".into());
        registry.entries[1].namespace = Some("second-server".into());
        let mut tools = registry.script_declarations();
        let names = |matches: Vec<phi_codemode::ToolDefinition>| {
            matches
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(registry.search_tools(&tools, "documents", Some("first_server"), 1)),
            ["first"]
        );
        assert_eq!(
            names(registry.search_tools(&tools, "documents", Some("second-server"), 1)),
            ["second"]
        );
        assert!(
            registry
                .search_tools(&tools, "documents", Some("absent"), 1)
                .is_empty()
        );
        assert_eq!(
            names(registry.search_tools(&tools[1..], "documents", None, 1)),
            ["second"]
        );
        tools[0].input_schema = Some(serde_json::json!({
            "properties": {"query": {"description": "schemaonlykeyword"}}
        }));
        assert_eq!(
            names(registry.search_tools(&tools, "schemaonlykeyword", None, 1)),
            ["first"]
        );
        tools[0].input_schema = None;
        assert!(
            registry
                .search_tools(&tools, "schemaonlykeyword", None, 1)
                .is_empty()
        );
    }

    #[test]
    fn prompt_sections_exclude_tools_and_guidelines() {
        let registry = test_registry(vec![
            tool_definition("Read", "Read full description."),
            tool_definition("Bash", "Bash full description."),
        ])
        .with_policy(None, vec!["Bash".to_string()]);

        assert_eq!(registry.prompt_summary(), "- Read: Read file contents");
        assert_eq!(registry.prompt_guidelines(), "- Use Read before editing.");
        assert!(!registry.prompt_summary().contains("Bash"));
        assert!(!registry.prompt_guidelines().contains("Bash"));
    }
}
