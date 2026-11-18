use super::config::MemoryConfig;
use super::graph::bm25::{Bm25MemorySearch, Bm25MemorySource};
use super::graph::{MemoryGraph, MemoryGraphNode, MemoryGraphQuery, anchor_node_id};
use super::memory_core;
use super::memory_git::{LocalMemoryGit, MemoryGit};
use super::service::MemoryService;
use super::tape::context::{DEFAULT_FORMATTED_ENTRY_CONTENT_CHARS, format_entry_line};
use super::tape::gate::{TapeMutationGate, TapeMutationState, consume_keyword_anchor};
use super::tape::types::{AnchorRange, DateRange, TapeScope, TapeSessionScanOptions};
use super::tape::{TapeAnchor, TapeAnchorMeta, TapeAnchorType, TapeQuery, TapeService};
use super::types::MemorySettings;
use crate::agent::tools::ToolError;
use crate::core::event::ToolDisplay;
use crate::permission::runtime::PermissionRuntime;
use crate::tools::{
    ToolAuthorization, ToolDisplayFormatter, ToolMetadata, ToolOwner, ToolRegistration,
    ToolResultFinalizeContext, ToolResultFinalizer, canonical_tool_name,
};
use compact_str::CompactString;
use rig::tool::Tool;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const MEMORY_SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SEARCH_PATTERN_LENGTH: usize = 200;
const MAX_SEARCH_RESULTS: usize = 50;

pub struct TapeAnchorTool {
    gate: Arc<TapeMutationGate>,
}

pub struct TapeInfoTool {
    tape: Arc<TapeService>,
}

pub struct TapeReadTool {
    tape: Arc<TapeService>,
}

pub struct TapeDeleteTool {
    tape: Arc<TapeService>,
    gate: Arc<TapeMutationGate>,
}

pub struct TapeResetTool {
    tape: Arc<TapeService>,
    memory: Arc<MemoryService>,
    gate: Arc<TapeMutationGate>,
}

pub struct MemorySyncTool {
    git: LocalMemoryGit,
    permission: Option<PermissionRuntime>,
}

pub struct MemorySearchTool {
    memory_dir: PathBuf,
    global_dir: Option<PathBuf>,
    memory: Option<Arc<MemoryService>>,
    search: Bm25MemorySearch,
}

pub struct MemoryCheckTool {
    memory_dir: PathBuf,
    global_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryToolDisplayFormatter;

#[derive(Deserialize)]
pub struct TapeAnchorArgs {
    pub name: CompactString,
    pub summary: Option<String>,
    pub purpose: Option<String>,
}

#[derive(Deserialize)]
pub struct TapeInfoArgs {}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TapeSearchView {
    Index,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeReadArgs {
    pub scan: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    pub limit: Option<usize>,
    pub since_anchor: Option<String>,
    pub last_anchor: Option<bool>,
    pub between_anchors: Option<AnchorRange>,
    pub between_dates: Option<DateRange>,
    pub entry_scope: Option<TapeScope>,
    pub anchor_scope: Option<TapeScope>,
    pub max_content_chars: Option<Option<usize>>,
}

#[derive(Deserialize)]
pub struct TapeDeleteArgs {
    pub id: Option<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Deserialize)]
pub struct TapeResetArgs {}

#[derive(Deserialize)]
pub struct MemorySyncArgs {
    pub action: MemorySyncAction,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySyncAction {
    Pull,
    Push,
    Status,
}

impl MemorySyncAction {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
            Self::Status => "status",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum MemorySearchArgs {
    Default(DefaultMemorySearchArgs),
    Tape(TapeMemorySearchArgs),
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultMemorySearchArgs {
    pub query: Option<String>,
    pub grep: Option<String>,
    pub rg: Option<String>,
    pub scope: Option<MemorySearchScope>,
    pub path: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapeMemorySearchArgs {
    pub query: Option<String>,
    pub view: Option<TapeSearchView>,
    pub session_id: Option<CompactString>,
    pub name_contains: Option<String>,
    pub summary_contains: Option<String>,
    pub purpose_contains: Option<String>,
    pub limit: Option<usize>,
    #[serde(rename = "includeRelations")]
    pub include_relations: Option<bool>,
    #[serde(rename = "relationDepth")]
    pub relation_depth: Option<usize>,
    pub include_orphans: Option<bool>,
}

#[derive(Debug, Clone)]
struct MemorySearchRequest {
    scope: MemorySearchScope,
    path: String,
    query: Option<String>,
    grep: Option<String>,
    rg: Option<String>,
}

#[derive(Debug, Clone)]
struct SearchCorpus {
    roots: Vec<SearchRoot>,
    sources: Vec<Bm25MemorySource<SearchRoot>>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemorySearchScope {
    Project,
    Global,
    All,
}

#[derive(Deserialize)]
pub struct MemoryCheckArgs {
    pub directory: Option<String>,
}

impl TapeAnchorTool {
    pub fn new(gate: Arc<TapeMutationGate>) -> Self {
        Self { gate }
    }
}

impl TapeInfoTool {
    pub fn new(tape: Arc<TapeService>) -> Self {
        Self { tape }
    }
}

impl TapeReadTool {
    pub fn new(tape: Arc<TapeService>) -> Self {
        Self { tape }
    }
}

impl TapeDeleteTool {
    pub fn new(tape: Arc<TapeService>, gate: Arc<TapeMutationGate>) -> Self {
        Self { tape, gate }
    }
}

impl TapeResetTool {
    pub fn new(
        tape: Arc<TapeService>,
        memory: Arc<MemoryService>,
        gate: Arc<TapeMutationGate>,
    ) -> Self {
        Self { tape, memory, gate }
    }
}

impl MemorySyncTool {
    pub fn new(
        memory_dir: PathBuf,
        repo_url: Option<String>,
        permission: Option<PermissionRuntime>,
    ) -> Self {
        Self {
            git: LocalMemoryGit::new(memory_dir, repo_url),
            permission,
        }
    }
}

impl MemorySearchTool {
    pub fn new(
        memory_dir: PathBuf,
        global_dir: Option<PathBuf>,
        memory: Option<Arc<MemoryService>>,
    ) -> Self {
        Self {
            memory_dir,
            global_dir,
            memory,
            search: Bm25MemorySearch::default(),
        }
    }
}

impl MemoryCheckTool {
    pub fn new(memory_dir: PathBuf, global_dir: Option<PathBuf>) -> Self {
        Self {
            memory_dir,
            global_dir,
        }
    }
}

impl ToolDisplayFormatter for MemoryToolDisplayFormatter {
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>> {
        if name == "tape_anchor" {
            return Ok(Some(ToolDisplay {
                title: Some("Tape anchor".to_string()),
                input: Some(format_tape_anchor_input(args)),
                title_suffix: None,
                expand: false,
                content: Some("Anchor created:".to_string()),
            }));
        }
        let Some(display_name) = memory_tool_display_name(name) else {
            return Ok(None);
        };
        let args = format_display_args(name, args);
        Ok(Some(ToolDisplay {
            title: Some(display_name.to_string()),
            input: Some(args),
            ..ToolDisplay::default()
        }))
    }
}

pub(crate) struct TapeToolResultFinalizer {
    tape: Arc<TapeService>,
    gate_state: Arc<Mutex<TapeMutationState>>,
}

impl TapeToolResultFinalizer {
    pub(crate) fn new(tape: Arc<TapeService>, gate_state: Arc<Mutex<TapeMutationState>>) -> Self {
        Self { tape, gate_state }
    }
}

impl ToolResultFinalizer for TapeToolResultFinalizer {
    fn finalize(&self, context: ToolResultFinalizeContext<'_>) -> Result<(), String> {
        // Tape tools stage anchors during execution because the transcript entry ID is assigned here.
        let mut state = self
            .gate_state
            .lock()
            .map_err(|_| "tape tool result gate is unavailable".to_string())?;
        // Taking first guarantees every finalization error clears the staged mutation.
        let pending = state
            .pending_mutation
            .take()
            .ok_or_else(|| "tape tool did not stage an anchor".to_string())?;
        let session_id = context
            .session_id
            .ok_or_else(|| "tape tool result has no session identity".to_string())?;
        let trigger = pending.meta.as_ref().and_then(|meta| meta.trigger);
        self.tape
            .record_anchor_at_entry(
                session_id.clone(),
                context.session_entry_id.clone(),
                pending,
            )
            .map_err(|error| format!("failed to persist tape_anchor anchor: {error}"))?;
        // Keyword authorization is consumed only after the durable effect succeeds.
        if let Some(trigger) = trigger {
            consume_keyword_anchor(&mut state, trigger);
        }
        Ok(())
    }
}

fn tool_registration<T>(tool: T, metadata: &HashMap<String, ToolMetadata>) -> ToolRegistration
where
    T: Tool + 'static,
{
    let metadata = metadata
        .get(&canonical_tool_name(T::NAME))
        .cloned()
        .unwrap_or_default();
    let authorization = match T::NAME {
        "tape_anchor" | "tape_delete" | "tape_reset" => ToolAuthorization::TapeMutation,
        "memory_sync" => ToolAuthorization::MemorySync,
        _ => ToolAuthorization::Invocation,
    };
    ToolRegistration::phi(
        tool,
        "memory",
        ToolOwner::Phi,
        metadata.with_authorization(authorization),
    )
}

pub(crate) fn collect_memory_tools(
    config: &MemoryConfig,
    session_id: Option<CompactString>,
    tape_mutation_state: Option<Arc<Mutex<TapeMutationState>>>,
    memory_service: Option<Arc<MemoryService>>,
    permission: Option<PermissionRuntime>,
    metadata: &HashMap<String, ToolMetadata>,
) -> Vec<ToolRegistration> {
    if !config.enabled {
        return Vec::new();
    }
    let settings = memory_core::settings_from_config(config);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let memory_repo_dir = memory_core::get_memory_repo_dir(&settings);
    let global_dir = memory_core::get_global_memory_dir(&settings);
    let memory = memory_service.or_else(|| {
        MemoryService::create(config, session_id.unwrap_or_default(), &cwd)
            .ok()
            .map(Arc::new)
    });
    let Some(memory_dir) = memory
        .as_ref()
        .map(|service| service.memory_project_dir().to_path_buf())
        .or_else(|| memory_core::get_memory_dir(&settings, &cwd).ok())
    else {
        return Vec::new();
    };
    let mut registrations = vec![
        tool_registration(
            MemorySyncTool::new(
                memory_repo_dir,
                config
                    .memory_dir
                    .as_ref()
                    .and_then(|memory_dir| memory_dir.repo_url.clone()),
                permission.clone(),
            ),
            metadata,
        ),
        tool_registration(
            MemorySearchTool::new(memory_dir.clone(), global_dir.clone(), memory.clone()),
            metadata,
        ),
        tool_registration(
            MemoryCheckTool::new(memory_dir.clone(), global_dir),
            metadata,
        ),
    ];
    if let Some(memory) = memory {
        registrations.extend(collect_tape_tools(
            memory,
            Arc::new(settings.clone()),
            tape_mutation_state,
            permission,
            metadata,
        ));
    }
    registrations
}

fn collect_tape_tools(
    memory: Arc<MemoryService>,
    settings: Arc<MemorySettings>,
    tape_mutation_state: Option<Arc<Mutex<TapeMutationState>>>,
    permission: Option<PermissionRuntime>,
    metadata: &HashMap<String, ToolMetadata>,
) -> Vec<ToolRegistration> {
    let Some(tape) = memory.tape().cloned() else {
        return Vec::new();
    };
    let gate_state =
        tape_mutation_state.unwrap_or_else(|| Arc::new(Mutex::new(TapeMutationState::default())));
    let gate = Arc::new(TapeMutationGate::new(
        settings.clone(),
        gate_state.clone(),
        permission,
    ));
    let finalizer = Arc::new(TapeToolResultFinalizer::new(tape.clone(), gate_state));
    vec![
        tool_registration(TapeAnchorTool::new(gate.clone()), metadata).with_finalizer(finalizer),
        tool_registration(TapeInfoTool::new(tape.clone()), metadata),
        tool_registration(TapeReadTool::new(tape.clone()), metadata),
        tool_registration(TapeDeleteTool::new(tape.clone(), gate.clone()), metadata),
        tool_registration(
            TapeResetTool::new(tape.clone(), memory.clone(), gate.clone()),
            metadata,
        ),
    ]
}

impl Tool for TapeAnchorTool {
    const NAME: &'static str = "tape_anchor";

    type Error = ToolError;
    type Args = TapeAnchorArgs;
    type Output = String;

    fn description(&self) -> String {
        "Create an anchor checkpoint in the tape.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "summary": { "type": "string" },
                "purpose": { "type": "string" }
            },
            "required": ["name"]
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: TapeAnchorArgs,
    ) -> Result<String, ToolError> {
        let resolved = self.gate.authorize_anchor(&args.name).await?;
        let anchor = TapeAnchor::new(args.name, TapeAnchorType::Normal).with_meta(TapeAnchorMeta {
            workspace: None,
            trigger: Some(resolved.trigger),
            keywords: resolved.keywords,
            summary: args.summary,
            purpose: args.purpose,
            git: None,
        });
        let output = serde_json::to_string_pretty(&anchor).map_err(to_tool_error)?;
        self.gate.stage_anchor(anchor)?;
        Ok(output)
    }
}

impl Tool for TapeInfoTool {
    const NAME: &'static str = "tape_info";

    type Error = ToolError;
    type Args = TapeInfoArgs;
    type Output = String;

    fn description(&self) -> String {
        "Get tape summary and anchor count for memory mode.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        _args: TapeInfoArgs,
    ) -> Result<String, ToolError> {
        let anchors = self
            .tape
            .search(TapeQuery::default())
            .map_err(to_tool_error)?;
        Ok(format!("tape anchors: {}", anchors.len()))
    }
}

impl Tool for TapeReadTool {
    const NAME: &'static str = "tape_read";

    type Error = ToolError;
    type Args = TapeReadArgs;
    type Output = String;

    fn description(&self) -> String {
        "Read transcript evidence selected by anchor, time, or raw text scan. Use memory_search with mode=tape and query to discover related anchors first.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scan": { "type": "string" },
                "types": { "type": "array", "items": { "type": "string", "enum": ["message", "custom", "compaction"] } },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                "sinceAnchor": { "type": "string" },
                "lastAnchor": { "type": "boolean" },
                "betweenAnchors": { "type": "object", "properties": { "start": { "type": "string" }, "end": { "type": "string" } } },
                "betweenDates": { "type": "object", "properties": { "start": { "type": "string" }, "end": { "type": "string" } } },
                "entryScope": { "type": "string", "enum": ["session", "project"] },
                "anchorScope": { "type": "string", "enum": ["session", "project"] },
                "maxContentChars": { "type": ["integer", "null"], "minimum": 80 }
            }
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: TapeReadArgs,
    ) -> Result<String, ToolError> {
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let entries = self
            .tape
            .scan_persisted_entries(&TapeSessionScanOptions {
                scan: args.scan.clone(),
                types: args.types,
                limit: Some(limit),
                since_anchor: args.since_anchor,
                last_anchor: args.last_anchor,
                between_anchors: args.between_anchors,
                between_dates: args.between_dates,
                entry_scope: Some(args.entry_scope.unwrap_or(TapeScope::Project)),
                anchor_scope: Some(args.anchor_scope.unwrap_or(TapeScope::Session)),
            })
            .map_err(to_tool_error)?;
        let max_content_chars = args
            .max_content_chars
            .unwrap_or(Some(DEFAULT_FORMATTED_ENTRY_CONTENT_CHARS));
        let lines = entries
            .iter()
            .filter_map(|entry| format_entry_line(entry, max_content_chars))
            .collect::<Vec<_>>();
        serde_json::to_string_pretty(&serde_json::json!({ "entries": lines }))
            .map_err(to_tool_error)
    }
}

impl Tool for TapeDeleteTool {
    const NAME: &'static str = "tape_delete";

    type Error = ToolError;
    type Args = TapeDeleteArgs;
    type Output = String;

    fn description(&self) -> String {
        "Delete anchors by exact id.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Single anchor id to delete" },
                "ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Multiple anchor ids to delete"
                }
            }
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: TapeDeleteArgs,
    ) -> Result<String, ToolError> {
        let requested_ids = collect_anchor_delete_ids(args);
        if requested_ids.is_empty() {
            return Ok(
                "No anchor ids provided. Use memory_search with mode=tape first to find anchors."
                    .to_string(),
            );
        }

        self.gate.authorize_delete(&requested_ids).await?;
        let removed = self
            .tape
            .delete_anchor_entries(&requested_ids)
            .map_err(to_tool_error)?;

        if removed.is_empty() {
            return Ok(format!("Anchor not found: {}", requested_ids.join(", ")));
        }

        let deleted_ids = removed
            .iter()
            .map(|anchor| anchor.id.clone())
            .collect::<Vec<_>>();
        serde_json::to_string_pretty(&serde_json::json!({
            "requestedIds": requested_ids,
            "ids": deleted_ids,
            "deleted": true,
            "deletedCount": removed.len(),
            "anchors": removed,
        }))
        .map_err(to_tool_error)
    }
}

fn collect_anchor_delete_ids(args: TapeDeleteArgs) -> Vec<String> {
    let mut seen = HashSet::new();
    args.id
        .into_iter()
        .chain(args.ids)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect()
}

impl Tool for TapeResetTool {
    const NAME: &'static str = "tape_reset";

    type Error = ToolError;
    type Args = TapeResetArgs;
    type Output = String;

    fn description(&self) -> String {
        "Clear Tape anchors and rebuild MemoryGraph.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        _args: TapeResetArgs,
    ) -> Result<String, ToolError> {
        self.gate.require_permission("tape_reset", "reset").await?;
        self.tape.reset().map_err(to_tool_error)?;
        let graph = self.memory.refresh().map_err(to_tool_error)?;
        serde_json::to_string_pretty(&serde_json::json!({
            "reset": true,
            "graphNodes": graph.nodes.len(),
        }))
        .map_err(to_tool_error)
    }
}

impl Tool for MemorySyncTool {
    const NAME: &'static str = "memory_sync";

    type Error = ToolError;
    type Args = MemorySyncArgs;
    type Output = String;

    fn description(&self) -> String {
        "Synchronize the memory git repository. Use status to inspect changes. Do not run pull or push unless the user explicitly asks for sync/pull/push.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["pull", "push", "status"] }
            },
            "required": ["action"]
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: MemorySyncArgs,
    ) -> Result<String, ToolError> {
        self.permission
            .as_ref()
            .ok_or_else(|| {
                ToolError::Msg(
                    "Memory synchronization requires an explicit permission policy".into(),
                )
            })?
            .authorize_tool("memory_sync", args.action.as_str(), None, None)
            .await
            .map_err(|error| ToolError::Msg(error.to_string()))?;
        match args.action {
            MemorySyncAction::Status => {
                let result = self.git.status().map_err(to_tool_error)?;
                if !result.success {
                    return Err(to_tool_error(result.stdout));
                }
                Ok(if result.stdout.trim().is_empty() {
                    "No uncommitted changes".to_string()
                } else {
                    result.stdout
                })
            }
            MemorySyncAction::Pull => {
                let result = self.git.pull().map_err(to_tool_error)?;
                if result.success {
                    Ok(result.message)
                } else {
                    Err(to_tool_error(result.message))
                }
            }
            MemorySyncAction::Push => {
                let result = self.git.push().map_err(to_tool_error)?;
                if result.success {
                    Ok(result.message)
                } else {
                    Err(to_tool_error(result.message))
                }
            }
        }
    }
}

impl Tool for MemorySearchTool {
    const NAME: &'static str = "memory_search";

    type Error = ToolError;
    type Args = MemorySearchArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search Markdown memory files or tape records. mode is required: default searches Markdown memory with query, grep, or rg; tape searches anchors with query or tape-specific filters. The two modes accept different parameters.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "mode": { "const": "default" },
                        "query": { "type": "string" },
                        "grep": { "type": "string", "description": "Custom grep regex; ignored when rg is provided." },
                        "rg": { "type": "string", "description": "Custom ripgrep regex; takes precedence over grep." },
                        "scope": { "type": "string", "enum": ["project", "global", "all"] },
                        "path": { "type": "string", "description": "Relative directory or Markdown file under each selected memory root; defaults to core. Use . for the entire root." }
                    },
                    "required": ["mode"],
                    "anyOf": [{ "required": ["query"] }, { "required": ["grep"] }, { "required": ["rg"] }],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "mode": { "const": "tape" },
                        "query": { "type": "string" },
                        "view": { "type": "string", "enum": ["index"], "description": "Return the bounded project memory graph directory." },
                        "session_id": { "type": "string" },
                        "name_contains": { "type": "string" },
                        "summary_contains": { "type": "string" },
                        "purpose_contains": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1 },
                        "includeRelations": { "type": "boolean", "description": "Return a relation subgraph for matched anchors." },
                        "relationDepth": { "type": "integer", "minimum": 0, "maximum": 4, "description": "Maximum graph expansion depth." },
                        "include_orphans": { "type": "boolean", "description": "Include anchors whose original session file no longer exists." },
                    },
                    "required": ["mode"],
                    "additionalProperties": false
                }
            ]
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: MemorySearchArgs,
    ) -> Result<String, ToolError> {
        let args = match args {
            MemorySearchArgs::Default(args) => args,
            MemorySearchArgs::Tape(args) => return self.search_tape(&args),
        };

        let request = MemorySearchRequest::from_args(&args)?;
        let scope = request.scope;
        let corpus = self.resolve_search_corpus(&request)?;
        let roots = &corpus.roots;
        let path = request.path.as_str();
        let includes_project = roots.iter().any(|root| root.label == "project");
        if roots.is_empty() {
            return Ok(format!(
                "Memory path not found: {path}. Use path=\".\" to search the full memory root."
            ));
        }
        let mut sections = Vec::new();
        let mut matched_files = Vec::<String>::new();

        if let Some(query) = request.query.as_deref() {
            if includes_project {
                self.append_project_index_matches(
                    &mut sections,
                    &mut matched_files,
                    query,
                    scope,
                    path,
                )?;
            }

            let results = self.search.search(&corpus.sources, query, 20);
            if !results.is_empty() {
                sections.push(format!("## BM25 ranking: {query}"));
                for (index, result) in results.iter().enumerate() {
                    let display_path = result.scope.display_path(&result.path);
                    push_unique(&mut matched_files, display_path.clone());
                    sections.push(format!(
                        "{}. {} (score: {:.3})",
                        index + 1,
                        display_path,
                        result.score
                    ));
                }
            }
        }

        for root in roots {
            let section_prefix = if scope == MemorySearchScope::All {
                format!("{} ", root.label)
            } else {
                String::new()
            };
            let search_path = root.search_path(path)?;
            if let Some(query) = request.query.as_deref() {
                let pattern = regex::escape(query);
                append_command_matches(
                    &mut sections,
                    &mut matched_files,
                    root,
                    &format!("## {section_prefix}Tags matching: {query}"),
                    "grep",
                    [
                        "-rn".to_string(),
                        "--include=*.md".to_string(),
                        "-m".to_string(),
                        MAX_SEARCH_RESULTS.to_string(),
                        "-E".to_string(),
                        format!(r"^\s*-\s*{pattern}"),
                        search_path.display().to_string(),
                    ],
                )
                .await?;
                append_command_matches(
                    &mut sections,
                    &mut matched_files,
                    root,
                    &format!("## {section_prefix}Description matching: {query}"),
                    "grep",
                    [
                        "-rn".to_string(),
                        "--include=*.md".to_string(),
                        "-m".to_string(),
                        MAX_SEARCH_RESULTS.to_string(),
                        "-E".to_string(),
                        format!(r"^description:\s*.*{pattern}"),
                        search_path.display().to_string(),
                    ],
                )
                .await?;
            }
            if let Some(grep) = request.grep.as_deref() {
                append_command_matches(
                    &mut sections,
                    &mut matched_files,
                    root,
                    &format!("## {section_prefix}Custom grep: {grep}"),
                    "grep",
                    [
                        "-rn".to_string(),
                        "--include=*.md".to_string(),
                        "-m".to_string(),
                        MAX_SEARCH_RESULTS.to_string(),
                        "-E".to_string(),
                        grep.to_string(),
                        search_path.display().to_string(),
                    ],
                )
                .await?;
            }
            if let Some(rg) = request.rg.as_deref() {
                append_command_matches(
                    &mut sections,
                    &mut matched_files,
                    root,
                    &format!("## {section_prefix}Custom ripgrep: {rg}"),
                    "rg",
                    [
                        "--with-filename".to_string(),
                        "-t".to_string(),
                        "md".to_string(),
                        "-m".to_string(),
                        MAX_SEARCH_RESULTS.to_string(),
                        rg.to_string(),
                        search_path.display().to_string(),
                    ],
                )
                .await?;
            }
        }

        let search_label = request.label();
        if sections.is_empty() {
            return Ok(format!("No results found for \"{search_label}\"."));
        }
        Ok(format!(
            "Found {} file(s) matching \"{search_label}\":\n\n{}\n\nUse read to view full content.",
            matched_files.len(),
            sections.join("\n")
        ))
    }
}

#[derive(Debug, Clone)]
struct SearchRoot {
    label: &'static str,
    dir: PathBuf,
}

impl SearchRoot {
    fn search_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        let path = Path::new(path);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ToolError::Msg(format!(
                "Invalid memory path: {}",
                path.display()
            )));
        }
        Ok(self.dir.join(path))
    }

    fn display_path(&self, file_path: &Path) -> String {
        file_path.display().to_string()
    }
}

impl MemorySearchRequest {
    fn from_args(args: &DefaultMemorySearchArgs) -> Result<Self, ToolError> {
        let grep = if args.rg.is_some() {
            None
        } else {
            args.grep.clone()
        };
        let path = args.path.as_deref().unwrap_or("core");
        let target = Path::new(path);
        if path.is_empty()
            || target.is_absolute()
            || target.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ToolError::Msg(format!("Invalid memory path: {path}")));
        }
        if args.query.is_none() && grep.is_none() && args.rg.is_none() {
            return Err(ToolError::Msg(
                "Provide query, grep, or rg to search memory files.".to_string(),
            ));
        }
        for pattern in [grep.as_deref(), args.rg.as_deref()].into_iter().flatten() {
            if pattern.len() > MAX_SEARCH_PATTERN_LENGTH {
                return Err(ToolError::Msg(format!(
                    "Search pattern too long ({}). Max length is {MAX_SEARCH_PATTERN_LENGTH}.",
                    pattern.len()
                )));
            }
        }
        Ok(Self {
            scope: args.scope.unwrap_or(MemorySearchScope::Project),
            path: path.to_string(),
            query: args.query.clone(),
            grep,
            rg: args.rg.clone(),
        })
    }

    fn label(&self) -> &str {
        self.query
            .as_deref()
            .or(self.grep.as_deref())
            .or(self.rg.as_deref())
            .unwrap_or_default()
    }
}

impl MemorySearchTool {
    fn resolve_search_corpus(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<SearchCorpus, ToolError> {
        let mut roots = Vec::new();
        let mut sources = Vec::new();
        for root in self.existing_search_roots(request.scope) {
            let search_path = root.search_path(&request.path)?;
            if !is_memory_search_path(&search_path) {
                continue;
            }
            if request.query.is_some() {
                for file in memory_core::list_memory_files(&search_path).map_err(to_tool_error)? {
                    sources.push(Bm25MemorySource {
                        file_path: file,
                        scope: root.clone(),
                    });
                }
            }
            roots.push(root);
        }
        Ok(SearchCorpus { roots, sources })
    }

    fn search_tape(&self, args: &TapeMemorySearchArgs) -> Result<String, ToolError> {
        let memory = self.memory.as_ref().ok_or_else(|| {
            ToolError::Msg("Project memory service is unavailable for this session".to_string())
        })?;
        let tape = memory.tape().ok_or_else(|| {
            ToolError::Msg("Tape mode is unavailable for this session".to_string())
        })?;
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        if args.view == Some(TapeSearchView::Index) {
            return self.index_view(memory, limit);
        }
        let include_relations = args.include_relations.unwrap_or(false);
        let anchors = tape
            .search_anchors(super::tape::types::TapeAnchorScanOptions {
                session_id: args.session_id.clone(),
                name: args.name_contains.clone(),
                summary: args.summary_contains.clone(),
                purpose: args.purpose_contains.clone(),
                limit: None,
                include_orphans: args.include_orphans,
                ..Default::default()
            })
            .map_err(to_tool_error)?;
        let graph = if include_relations {
            memory.graph().map_err(to_tool_error)?
        } else {
            MemoryGraph::default()
        };
        let result = rank_anchors(
            &graph,
            &anchors,
            args.query.as_deref(),
            limit,
            include_relations,
            args.relation_depth.unwrap_or(2).min(4),
        );
        serde_json::to_string_pretty(&result).map_err(to_tool_error)
    }

    fn index_view(&self, memory: &MemoryService, limit: usize) -> Result<String, ToolError> {
        let graph = memory.reconcile().map_err(to_tool_error)?;
        let nodes = graph
            .nodes
            .iter()
            .take(limit)
            .map(|node| {
                serde_json::json!({
                    "id": node.id(),
                    "kind": node.kind(),
                    "label": node.label(),
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_string_pretty(&serde_json::json!({
            "nodes": nodes,
            "edgeCount": graph.edges.len(),
        }))
        .map_err(to_tool_error)
    }

    fn append_project_index_matches(
        &self,
        sections: &mut Vec<String>,
        matched_files: &mut Vec<String>,
        query: &str,
        scope: MemorySearchScope,
        search_path: &str,
    ) -> Result<(), ToolError> {
        let memory = self.memory.as_ref().ok_or_else(|| {
            ToolError::Msg("Project memory graph is unavailable for this session".to_string())
        })?;
        let query = query.to_lowercase();
        let mut matches = memory
            .reconcile()
            .map_err(to_tool_error)?
            .nodes
            .into_iter()
            .filter_map(|node| match node {
                MemoryGraphNode::MemoryFile {
                    path,
                    title,
                    description,
                    tags,
                    ..
                } if memory_path_matches(&path, search_path) => {
                    let searchable = format!("{path}\n{title}\n{description}\n{}", tags.join("\n"));
                    searchable.to_lowercase().contains(&query).then_some(path)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        matches.sort();
        if matches.is_empty() {
            return Ok(());
        }
        let prefix = (scope == MemorySearchScope::All).then_some("project ");
        sections.push(format!(
            "## {}Memory index matching: {query}",
            prefix.unwrap_or_default()
        ));
        for path in matches.into_iter().take(50) {
            let path = memory.memory_project_dir().join(path).display().to_string();
            push_unique(matched_files, path.clone());
            sections.push(path);
        }
        Ok(())
    }

    fn existing_search_roots(&self, scope: MemorySearchScope) -> Vec<SearchRoot> {
        let mut roots = Vec::new();
        if matches!(scope, MemorySearchScope::Project | MemorySearchScope::All) {
            roots.push(SearchRoot {
                label: "project",
                dir: self.memory_dir.clone(),
            });
        }
        if matches!(scope, MemorySearchScope::Global | MemorySearchScope::All) {
            if let Some(global_dir) = &self.global_dir {
                if *global_dir != self.memory_dir {
                    roots.push(SearchRoot {
                        label: "global",
                        dir: global_dir.clone(),
                    });
                }
            }
        }
        roots.into_iter().filter(|root| root.dir.exists()).collect()
    }
}

async fn append_command_matches<A: AsRef<[String]>>(
    sections: &mut Vec<String>,
    matched_files: &mut Vec<String>,
    root: &SearchRoot,
    title: &str,
    command: &str,
    args: A,
) -> Result<(), ToolError> {
    let output = timeout(
        MEMORY_SEARCH_TIMEOUT,
        Command::new(command).args(args.as_ref()).output(),
    )
    .await
    .map_err(|_| ToolError::Msg(format!("{command} search timed out")))?
    .map_err(to_tool_error)?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(ToolError::Msg(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| format_command_match(root, line))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Ok(());
    }
    for line in &lines {
        let file_path = line.split_once(':').map(|(path, _)| path).unwrap_or(line);
        push_unique(matched_files, file_path.to_string());
    }
    sections.push(String::new());
    sections.push(title.to_string());
    sections.extend(lines.into_iter().take(MAX_SEARCH_RESULTS));
    Ok(())
}

fn format_command_match(root: &SearchRoot, line: &str) -> Option<String> {
    let (file_path, content) = line.split_once(':')?;
    let display_path = root.display_path(Path::new(file_path));
    Some(format!("{display_path}: {}", content.trim()))
}

fn push_unique(items: &mut Vec<String>, item: String) {
    if !items.contains(&item) {
        items.push(item);
    }
}

fn is_memory_search_path(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    !metadata.file_type().is_symlink()
        && (metadata.is_dir()
            || (metadata.is_file() && path.extension().is_some_and(|ext| ext == "md")))
}

fn memory_path_matches(path: &str, search_path: &str) -> bool {
    let search_path = search_path.trim_end_matches('/');
    search_path == "."
        || path == search_path
        || path
            .strip_prefix(search_path)
            .is_some_and(|path| path.starts_with('/'))
}

impl Tool for MemoryCheckTool {
    const NAME: &'static str = "memory_check";

    type Error = ToolError;
    type Args = MemoryCheckArgs;
    type Output = String;

    fn description(&self) -> String {
        "Check current project memory folder structure".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "directory": { "type": "string" }
            }
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: MemoryCheckArgs,
    ) -> Result<String, ToolError> {
        let root = args
            .directory
            .as_ref()
            .map_or(self.memory_dir.clone(), |dir| self.memory_dir.join(dir));
        if !root.exists() {
            return Ok(format!("Memory directory not found: {}", root.display()));
        }
        if !is_safe_child(&self.memory_dir, &root) {
            return Err(ToolError::Msg(format!(
                "Invalid memory directory: {}",
                root.display()
            )));
        }
        let files = memory_core::list_memory_files(&root).map_err(to_tool_error)?;
        let mut lines = vec![
            format!("Memory directory structure"),
            format!("Path: {}", root.display()),
            format!("Memory files ({}):", files.len()),
        ];
        lines.extend(files.iter().map(|file| format!("  {}", file.display())));
        if let Some(global_dir) = &self.global_dir {
            lines.push(format!("Global memory: {}", global_dir.display()));
        }
        Ok(lines.join("\n"))
    }
}

fn is_safe_child(parent: &Path, child: &Path) -> bool {
    if !child.exists() {
        return true;
    }
    let Ok(parent) = parent.canonicalize() else {
        return false;
    };
    let Ok(child) = child.canonicalize() else {
        return false;
    };
    child.starts_with(parent)
}

fn rank_anchors(
    graph: &MemoryGraph,
    anchors: &[TapeAnchor],
    query: Option<&str>,
    limit: usize,
    include_relations: bool,
    relation_depth: usize,
) -> serde_json::Value {
    let matches = anchors
        .iter()
        .filter(|anchor| anchor_matches_query(anchor, query))
        .collect::<Vec<_>>();
    let recent_matches = &matches[matches.len().saturating_sub(limit)..];
    if !include_relations {
        return serde_json::json!(recent_matches);
    }

    let prompt = query.unwrap_or_default().trim();
    let anchor_ids = anchors
        .iter()
        .map(|anchor| anchor_node_id(&anchor.id))
        .collect::<Vec<_>>();
    let selection = graph.select(&MemoryGraphQuery {
        prompt: prompt.to_string(),
        seed_ids: prompt.is_empty().then_some(anchor_ids).unwrap_or_default(),
        max_seeds: limit.min(10),
        max_depth: relation_depth,
        ..MemoryGraphQuery::default()
    });
    if selection.nodes.is_empty() {
        return serde_json::json!({
            "anchors": recent_matches,
            "graph": {
                "seeds": selection.seeds,
                "nodes": [],
                "edges": selection.edges,
            },
        });
    }
    let score_by_id = selection
        .nodes
        .iter()
        .map(|node| (node.node.id.as_str(), node.score))
        .collect::<HashMap<_, _>>();
    let mut ranked = anchors
        .iter()
        .filter(|anchor| score_by_id.contains_key(anchor_node_id(&anchor.id).as_str()))
        .cloned()
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        let left_score = score_by_id[anchor_node_id(&left.id).as_str()];
        let right_score = score_by_id[anchor_node_id(&right.id).as_str()];
        right_score
            .total_cmp(&left_score)
            .then_with(|| right.timestamp.cmp(&left.timestamp))
    });
    ranked.truncate(limit);

    serde_json::json!({
        "anchors": ranked,
        "graph": {
            "seeds": selection.seeds,
            "nodes": selection.nodes.into_iter().map(|node| serde_json::json!({
                "id": node.node.id,
                "kind": node.node.kind,
                "label": node.node.label,
                "score": node.score,
                "distance": node.distance,
            })).collect::<Vec<_>>(),
            "edges": selection.edges,
        },
    })
}

fn anchor_matches_query(anchor: &TapeAnchor, query: Option<&str>) -> bool {
    let query = query.unwrap_or_default().trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let meta = anchor.meta.as_ref();
    [
        anchor.name.as_str(),
        meta.and_then(|meta| meta.summary.as_deref())
            .unwrap_or_default(),
        meta.and_then(|meta| meta.purpose.as_deref())
            .unwrap_or_default(),
        &meta.map(|meta| meta.keywords.join(" ")).unwrap_or_default(),
    ]
    .join("\n")
    .to_lowercase()
    .contains(&query)
}

fn memory_tool_display_name(name: &str) -> Option<&'static str> {
    match name {
        "memory_sync" => Some("Memory sync"),
        "memory_search" => Some("Memory search"),
        "memory_check" => Some("Memory check"),
        "tape_info" => Some("Tape info"),
        "tape_read" => Some("Tape read"),
        "tape_delete" => Some("Tape delete"),
        "tape_reset" => Some("Tape reset"),
        "tape_anchor" => Some("Tape anchor"),
        _ => None,
    }
}

fn format_tape_anchor_input(args: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.trim().to_string();
    };
    value
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn format_display_args(name: &str, args: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.trim().to_string();
    };
    let Some(object) = value.as_object() else {
        return value.to_string();
    };
    let Some(keys) = display_arg_keys(name, object) else {
        return String::new();
    };
    let mut filtered = serde_json::Map::new();
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        if value.is_null()
            || matches!(value, serde_json::Value::Array(items) if items.is_empty())
            || matches!(value, serde_json::Value::String(text) if text.trim().is_empty())
            || is_default_display_arg(name, key, value)
        {
            continue;
        }
        filtered.insert((*key).to_string(), value.clone());
    }
    if filtered.is_empty() {
        return String::new();
    }
    serde_json::Value::Object(filtered).to_string()
}

fn is_default_display_arg(name: &str, key: &str, value: &serde_json::Value) -> bool {
    match (name, key) {
        ("memory_search", "mode") => value.as_str() == Some("default"),
        ("memory_search", "limit") => value.as_u64() == Some(20),
        _ => false,
    }
}

fn display_arg_keys(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Option<&'static [&'static str]> {
    match name {
        "memory_sync" => Some(&["action"]),
        "memory_search" => Some(
            if args.get("mode").and_then(|value| value.as_str()) == Some("tape") {
                &["mode", "query", "view", "limit"]
            } else {
                &["grep", "query", "rg"]
            },
        ),
        "memory_check" => Some(&["directory"]),
        "tape_read" => Some(&[
            "scan",
            "types",
            "limit",
            "sinceAnchor",
            "lastAnchor",
            "betweenAnchors",
            "betweenDates",
        ]),
        "tape_delete" => Some(&["id", "ids"]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::tape::TapeAnchorTrigger;
    use crate::extension::memory::tape::gate::KeywordAnchorInstruction;
    use crate::extension::memory::tape::types::TapeAnchorScanOptions;
    use crate::permission::checker::PermCheck;

    #[test]
    fn memory_tool_display_separates_labels_inputs_and_result_prefixes() {
        for (name, args, title, input, content) in [
            (
                "tape_anchor",
                r#"{"name":"checkpoint"}"#,
                "Tape anchor",
                "checkpoint",
                Some("Anchor created:"),
            ),
            (
                "memory_search",
                r#"{"query":"design","limit":20}"#,
                "Memory search",
                r#"{"query":"design"}"#,
                None,
            ),
            ("tape_info", "{}", "Tape info", "", None),
        ] {
            let display = MemoryToolDisplayFormatter
                .format_tool_display(name, args)
                .unwrap()
                .unwrap();
            assert_eq!(display.title.as_deref(), Some(title));
            assert_eq!(display.input.as_deref(), Some(input));
            assert_eq!(display.content.as_deref(), content);
        }
    }

    fn manual_settings() -> MemorySettings {
        serde_json::from_value(serde_json::json!({
            "enabled": true,
            "tape": { "anchor": { "mode": "manual" } }
        }))
        .unwrap()
    }

    fn checker_fixture() -> PermCheck {
        Arc::new(Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfig::default(),
                None,
            ),
        ))
    }

    fn anchor(id: &str, name: &str, timestamp: &str) -> TapeAnchor {
        let mut anchor =
            TapeAnchor::new(name, TapeAnchorType::Normal).with_summary(format!("{name} design"));
        anchor.id = id.into();
        anchor.session_id = "session".into();
        anchor.timestamp = timestamp.into();
        anchor
    }

    #[tokio::test]
    async fn memory_sync_action_denial_precedes_git_effects() {
        let config = crate::permission::PermissionConfig::try_from_value(serde_json::json!({"mode":"allow", "tools":{"memory_sync":{"pull":"deny", "push":"deny", "status":"deny"}}})).unwrap();
        let checker = crate::permission::checker::PermissionChecker::new(&config, None);
        let permission = PermissionRuntime::new(Arc::new(Mutex::new(checker)));
        let root = std::env::temp_dir().join(format!("phi-denied-sync-{}", uuid::Uuid::new_v4()));
        let tool = MemorySyncTool::new(root.clone(), None, Some(permission));
        for action in [
            MemorySyncAction::Status,
            MemorySyncAction::Pull,
            MemorySyncAction::Push,
        ] {
            let result = tool
                .call(
                    &mut rig::tool::ToolContext::new(),
                    MemorySyncArgs { action },
                )
                .await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Permission denied")
            );
        }
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn denied_delete_and_reset_preserve_anchors() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-anchor-permission-{}", uuid::Uuid::new_v4()));
        let memory = Arc::new(MemoryService::new(&directory, "test").unwrap());
        let tape = Arc::new(TapeService::create(&directory, "test", "session", "/tmp").unwrap());
        let anchor = tape
            .create_anchor_without_transcript("checkpoint", TapeAnchorType::Normal, None)
            .unwrap();
        let config = crate::permission::PermissionConfig::try_from_value(
            serde_json::json!({"mode":"allow", "tools":{"tape_delete":"deny", "tape_reset":"deny"}}),
        ).unwrap();
        let permission = PermissionRuntime::new(Arc::new(Mutex::new(
            crate::permission::checker::PermissionChecker::new(&config, None),
        )));
        let gate = Arc::new(TapeMutationGate::new(
            Arc::new(manual_settings()),
            Arc::new(Mutex::new(TapeMutationState::default())),
            Some(permission),
        ));
        let delete = TapeDeleteTool::new(tape.clone(), gate.clone());
        assert!(
            delete
                .call(
                    &mut rig::tool::ToolContext::new(),
                    TapeDeleteArgs {
                        id: Some(anchor.id.to_string()),
                        ids: Vec::new(),
                    }
                )
                .await
                .is_err()
        );
        let reset = TapeResetTool::new(tape.clone(), memory, gate);
        assert!(
            reset
                .call(&mut rig::tool::ToolContext::new(), TapeResetArgs {})
                .await
                .is_err()
        );
        assert_eq!(tape.get_anchor_store().get_all_anchors().unwrap().len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn automatic_and_keyword_anchors_cannot_bypass_explicit_deny() {
        let config = crate::permission::PermissionConfig::try_from_value(
            serde_json::json!({"mode":"allow", "tools":{"tape_anchor":"deny"}}),
        )
        .unwrap();
        let permission = PermissionRuntime::new(Arc::new(Mutex::new(
            crate::permission::checker::PermissionChecker::new(&config, None),
        )));
        let state = Arc::new(Mutex::new(TapeMutationState::default()));
        let gate = TapeMutationGate::new(
            Arc::new(
                serde_json::from_value(serde_json::json!({"tape":{"anchor":{"mode":"auto"}}}))
                    .unwrap(),
            ),
            state.clone(),
            Some(permission),
        );
        assert!(gate.authorize_anchor("automatic").await.is_err());
        state.lock().unwrap().pending_keyword_anchor = Some(KeywordAnchorInstruction {
            primary: "save".into(),
            matched: vec!["save".into()],
            anchor_name: "keyword".into(),
            message: "trusted".into(),
        });
        assert!(gate.authorize_anchor("keyword").await.is_err());
    }

    #[tokio::test]
    async fn manual_mode_uses_existing_permission_rules_and_confirmation() {
        let permission = PermissionRuntime::new(checker_fixture()).interactive(true);
        let (permission_tx, mut permission_rx) = tokio::sync::mpsc::channel(1);
        let permission = permission.with_interaction_sender(Some(permission_tx));
        let gate = Arc::new(TapeMutationGate::new(
            Arc::new(manual_settings()),
            Arc::new(Mutex::new(TapeMutationState::default())),
            Some(permission),
        ));
        let tool = TapeAnchorTool::new(gate);
        let call = tokio::spawn(async move {
            tool.call(
                &mut rig::tool::ToolContext::new(),
                TapeAnchorArgs {
                    name: "task/checkpoint".into(),
                    summary: None,
                    purpose: None,
                },
            )
            .await
        });

        let request = permission_rx.recv().await.unwrap();
        assert_eq!(request.permission.tool, "tape_anchor");
        assert_eq!(request.permission.input, "task/checkpoint");
        request
            .reply
            .send(crate::core::interaction::PermissionDecision::AllowOnce)
            .unwrap();
        call.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn keyword_anchor_bypasses_manual_permission_confirmation() {
        let gate_state = Arc::new(Mutex::new(TapeMutationState {
            pending_keyword_anchor: Some(KeywordAnchorInstruction {
                primary: "checkpoint".to_string(),
                matched: vec!["checkpoint".to_string()],
                anchor_name: "task/checkpoint".to_string(),
                message: "checkpoint".to_string(),
            }),
            ..TapeMutationState::default()
        }));
        let gate = Arc::new(TapeMutationGate::new(
            Arc::new(manual_settings()),
            gate_state.clone(),
            Some(PermissionRuntime::new(checker_fixture())),
        ));
        let tool = TapeAnchorTool::new(gate);
        assert!(
            tool.call(
                &mut rig::tool::ToolContext::new(),
                TapeAnchorArgs {
                    name: "task/checkpoint-other".into(),
                    summary: None,
                    purpose: None,
                }
            )
            .await
            .is_err()
        );
        assert!(gate_state.lock().unwrap().pending_mutation.is_none());
        assert!(gate_state.lock().unwrap().pending_keyword_anchor.is_some());

        tool.call(
            &mut rig::tool::ToolContext::new(),
            TapeAnchorArgs {
                name: "task/checkpoint".into(),
                summary: None,
                purpose: None,
            },
        )
        .await
        .unwrap();

        let state = gate_state.lock().unwrap();
        assert_eq!(
            match state.pending_mutation.as_ref() {
                Some(anchor) => {
                    anchor.meta.as_ref().and_then(|meta| meta.trigger)
                }
                _ => None,
            },
            Some(TapeAnchorTrigger::Keyword)
        );
    }

    #[test]
    fn tape_result_finalizer_binds_anchor_to_preallocated_entry() {
        let tape = Arc::new(TapeService::default());
        let gate_state = Arc::new(Mutex::new(TapeMutationState {
            pending_mutation: Some(TapeAnchor::new("anchor", TapeAnchorType::Normal)),
            ..TapeMutationState::default()
        }));
        let finalizer = TapeToolResultFinalizer::new(tape.clone(), gate_state);
        let session_id = CompactString::new("session-1");
        let entry_id = CompactString::new("entry-1");

        finalizer
            .finalize(ToolResultFinalizeContext {
                tool_name: "tape_anchor",
                session_id: Some(&session_id),
                session_entry_id: &entry_id,
            })
            .unwrap();

        let anchors = tape.search(TapeQuery::default()).unwrap();
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].session_id, session_id);
        assert_eq!(anchors[0].session_entry_id, entry_id);
    }

    #[test]
    fn failed_finalization_keeps_keyword_authorization_and_clears_staged_anchor() {
        let tape = Arc::new(TapeService::default());
        let gate_state = Arc::new(Mutex::new(TapeMutationState {
            pending_keyword_anchor: Some(KeywordAnchorInstruction {
                primary: "anchor".to_string(),
                matched: vec!["anchor".to_string()],
                anchor_name: "anchor".to_string(),
                message: "anchor".to_string(),
            }),
            pending_mutation: Some(TapeAnchor::new("anchor", TapeAnchorType::Normal)),
            ..TapeMutationState::default()
        }));
        let finalizer = TapeToolResultFinalizer::new(tape, gate_state.clone());
        let entry_id = CompactString::new("entry-1");

        assert!(
            finalizer
                .finalize(ToolResultFinalizeContext {
                    tool_name: "tape_anchor",
                    session_id: None,
                    session_entry_id: &entry_id,
                })
                .is_err()
        );

        let state = gate_state.lock().unwrap();
        assert!(state.pending_mutation.is_none());
        assert!(state.pending_keyword_anchor.is_some());
    }

    #[tokio::test]
    async fn durable_anchor_failure_preserves_exact_consent_until_retry_succeeds() {
        let root = std::env::temp_dir().join(format!("phi-tape-finalize-{}", uuid::Uuid::new_v4()));
        let tape = Arc::new(TapeService::create(&root, "test", "session", "/tmp").unwrap());
        let state = Arc::new(Mutex::new(TapeMutationState {
            pending_keyword_anchor: Some(KeywordAnchorInstruction {
                primary: "checkpoint".into(),
                matched: vec!["checkpoint".into()],
                anchor_name: "task/checkpoint".into(),
                message: "trusted".into(),
            }),
            ..Default::default()
        }));
        let gate = Arc::new(TapeMutationGate::new(
            Arc::new(manual_settings()),
            state.clone(),
            Some(PermissionRuntime::new(checker_fixture())),
        ));
        let tool = TapeAnchorTool::new(gate);
        let finalizer = TapeToolResultFinalizer::new(tape.clone(), state.clone());
        let path = root.join("test__anchors.jsonl");
        std::fs::create_dir_all(&path).unwrap();
        for fail in [true, false] {
            tool.call(
                &mut rig::tool::ToolContext::new(),
                TapeAnchorArgs {
                    name: "task/checkpoint".into(),
                    summary: None,
                    purpose: None,
                },
            )
            .await
            .unwrap();
            let result = finalizer.finalize(ToolResultFinalizeContext {
                tool_name: "tape_anchor",
                session_id: Some(&CompactString::new("session")),
                session_entry_id: &CompactString::new("entry"),
            });
            assert_eq!(result.is_err(), fail, "{result:?}");
            assert!(state.lock().unwrap().pending_mutation.is_none());
            assert_eq!(state.lock().unwrap().pending_keyword_anchor.is_some(), fail);
            if fail {
                assert!(
                    result
                        .unwrap_err()
                        .contains("failed to persist tape_anchor")
                );
                std::fs::remove_dir(&path).unwrap();
                assert!(
                    tape.search_anchors(TapeAnchorScanOptions {
                        include_orphans: Some(true),
                        ..Default::default()
                    })
                    .unwrap()
                    .is_empty()
                );
            }
        }
        let anchors = tape
            .search_anchors(TapeAnchorScanOptions {
                include_orphans: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].session_entry_id, "entry");
        assert!(
            tool.call(
                &mut rig::tool::ToolContext::new(),
                TapeAnchorArgs {
                    name: "task/checkpoint".into(),
                    summary: None,
                    purpose: None,
                }
            )
            .await
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memory_search_modes_reject_incompatible_args() {
        let default = serde_json::from_value::<MemorySearchArgs>(serde_json::json!({
            "mode": "default",
            "query": "graph",
            "path": "core",
        }))
        .unwrap();
        assert!(matches!(default, MemorySearchArgs::Default(_)));

        let tape = serde_json::from_value::<MemorySearchArgs>(serde_json::json!({
            "mode": "tape",
            "query": "graph",
            "view": "index",
        }))
        .unwrap();
        assert!(matches!(
            tape,
            MemorySearchArgs::Tape(TapeMemorySearchArgs { .. })
        ));

        assert!(
            serde_json::from_value::<MemorySearchArgs>(serde_json::json!({
                "mode": "tape",
                "rg": "graph",
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn project_query_combines_graph_bm25_and_yaml_matches() {
        let directory =
            std::env::temp_dir().join(format!("phi-sdk-memory-search-{}", uuid::Uuid::new_v4()));
        let memory_dir = directory.join("memory");
        std::fs::create_dir_all(memory_dir.join("core")).unwrap();
        std::fs::write(
            memory_dir.join("core/design.md"),
            "---\ndescription: graph design\ntags:\n  - graph\n---\nGraph content",
        )
        .unwrap();
        let memory = Arc::new(MemoryService::new(&memory_dir, "project").unwrap());
        let tool = MemorySearchTool::new(memory_dir, None, Some(memory));

        let output = tool
            .call(
                &mut rig::tool::ToolContext::new(),
                MemorySearchArgs::Default(DefaultMemorySearchArgs {
                    query: Some("graph".to_string()),
                    scope: Some(MemorySearchScope::Project),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        assert!(output.contains("Memory index matching"));
        assert!(output.contains("BM25 ranking"));
        assert!(output.contains("Tags matching"));
        assert!(output.contains("Description matching"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn query_cache_respects_scope_path_and_external_edits() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-memory-scope-{}", uuid::Uuid::new_v4()));
        let project = directory.join("project");
        let global = directory.join("global");
        for root in [&project, &global] {
            std::fs::create_dir_all(root.join("core/nested")).unwrap();
            std::fs::write(root.join("core/root.md"), "needle").unwrap();
            std::fs::write(root.join("core/nested/child.md"), "needle").unwrap();
        }
        let memory = Arc::new(MemoryService::new(&project, "project").unwrap());
        let tool = MemorySearchTool::new(project.clone(), Some(global.clone()), Some(memory));
        for (scope, path, expected) in [
            (
                MemorySearchScope::Global,
                "core",
                vec![
                    global.join("core/root.md"),
                    global.join("core/nested/child.md"),
                ],
            ),
            (
                MemorySearchScope::Project,
                "core/nested",
                vec![project.join("core/nested/child.md")],
            ),
            (
                MemorySearchScope::All,
                "core/root.md",
                vec![project.join("core/root.md"), global.join("core/root.md")],
            ),
            (
                MemorySearchScope::Global,
                "core/root.md",
                vec![global.join("core/root.md")],
            ),
        ] {
            for _ in 0..2 {
                let output = tool
                    .call(
                        &mut rig::tool::ToolContext::new(),
                        MemorySearchArgs::Default(DefaultMemorySearchArgs {
                            query: Some("needle".into()),
                            scope: Some(scope),
                            path: Some(path.into()),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap();
                for root in [&project, &global] {
                    for relative in ["core/root.md", "core/nested/child.md"] {
                        let file = root.join(relative);
                        assert_eq!(
                            output.contains(&file.display().to_string()),
                            expected.contains(&file),
                            "{output}"
                        );
                    }
                }
            }
        }
        std::fs::write(global.join("core/root.md"), "absent").unwrap();
        let output = tool
            .call(
                &mut rig::tool::ToolContext::new(),
                MemorySearchArgs::Default(DefaultMemorySearchArgs {
                    query: Some("needle".into()),
                    scope: Some(MemorySearchScope::Global),
                    path: Some("core/root.md".into()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(output.contains("No results found"), "{output}");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn path_selects_core_a_directory_or_a_memory_file() {
        let directory =
            std::env::temp_dir().join(format!("phi-sdk-memory-search-{}", uuid::Uuid::new_v4()));
        let memory_dir = directory.join("memory");
        std::fs::create_dir_all(memory_dir.join("plan")).unwrap();
        std::fs::write(memory_dir.join("plan/graph.md"), "Graph design").unwrap();
        let tool = MemorySearchTool::new(memory_dir, None, None);

        let mut context = rig::tool::ToolContext::new();
        let core_output = tool
            .call(
                &mut context,
                MemorySearchArgs::Default(DefaultMemorySearchArgs {
                    rg: Some("graph".to_string()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(core_output.contains("Memory path not found: core"));

        let root_output = tool
            .call(
                &mut context,
                MemorySearchArgs::Default(DefaultMemorySearchArgs {
                    rg: Some("Graph".to_string()),
                    path: Some(".".to_string()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(root_output.contains("plan/graph.md"));

        let file_output = tool
            .call(
                &mut context,
                MemorySearchArgs::Default(DefaultMemorySearchArgs {
                    rg: Some("Graph".to_string()),
                    path: Some("plan/graph.md".to_string()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(file_output.contains("plan/graph.md"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tape_query_matches_before_limiting_candidates() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-tape-query-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let memory = Arc::new(
            MemoryService::create(
                &MemoryConfig {
                    memory_dir: Some(crate::extension::memory::types::MemoryDirSettings {
                        local_path: Some(directory.join("memory").to_string_lossy().into_owned()),
                        ..Default::default()
                    }),
                    tape: Some(crate::extension::memory::tape::TapeConfig {
                        only_git: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                "session",
                &directory,
            )
            .unwrap(),
        );
        let tape = memory.tape().unwrap();
        let matching = tape
            .create_anchor_without_transcript("needle", TapeAnchorType::Normal, None)
            .unwrap();
        for _ in 0..25 {
            tape.create_anchor_without_transcript("other", TapeAnchorType::Normal, None)
                .unwrap();
        }
        let tool = MemorySearchTool::new(
            memory.memory_project_dir().to_path_buf(),
            None,
            Some(memory),
        );
        let output = tool
            .search_tape(&TapeMemorySearchArgs {
                query: Some("needle".into()),
                limit: Some(1),
                include_orphans: Some(true),
                ..Default::default()
            })
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["id"], matching.id.as_str());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_relation_selection_preserves_query_filter() {
        let anchors = vec![
            anchor("first", "graph design", "2026-01-01T00:00:00Z"),
            anchor("second", "latest", "2026-01-01T00:01:00Z"),
        ];
        let graph = MemoryGraph::default();
        for query in [None, Some("graph"), Some("absent")] {
            let plain = rank_anchors(&graph, &anchors, query, 1, false, 1);
            let related = rank_anchors(&graph, &anchors, query, 1, true, 1);
            assert_eq!(related["anchors"], plain, "query: {query:?}");
        }
    }

    #[test]
    fn tape_mode_without_relations_filters_by_query() {
        let anchors = vec![
            anchor("first", "graph design", "2026-01-01T00:00:00Z"),
            anchor("second", "latest", "2026-01-01T00:01:00Z"),
        ];

        let graph = MemoryGraph::default();
        let result = rank_anchors(&graph, &anchors, Some("graph"), 1, false, 1);

        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(result[0]["id"], "first");
    }
}

fn to_tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Msg(error.to_string())
}
