// Optional memory workflows are internal even when a given runtime does not activate them.
#![allow(dead_code)]

pub(crate) mod config;
pub(crate) mod graph;
pub(crate) mod hooks;
pub(crate) mod memory_core;
pub(crate) mod memory_git;
mod runtime;
pub(crate) mod service;
pub(crate) use runtime::{MemoryAgentKind, MemoryDispatch, MemorySession, MemorySubmission};
pub(super) use runtime::{
    adapter_commands, command_names, delegated_tool_denials, skill_declarations,
};
pub(crate) mod tape;
pub(crate) mod tools;
pub(crate) mod types;
pub(crate) mod utils;

pub use config::MemoryConfig;
pub use graph::{
    MemoryGraph, MemoryGraphConfidence, MemoryGraphEdge, MemoryGraphEdgeKind,
    MemoryGraphEdgeOrigin, MemoryGraphNode, MemoryGraphNodeKind, MemoryGraphNodeView,
    MemoryGraphQuery, MemoryGraphRecord, MemoryGraphRelation, MemoryGraphRelationConfidence,
    MemoryGraphRelationKind, MemoryGraphSelection, RankedMemoryGraphNode,
};
pub use hooks::{HookAction, HookConfig, HookTrigger, MemoryHooks, memory_inject_prompt};
pub use memory_core::{
    MemoryContext, MemoryInjection, build_memory_context, build_memory_injection,
    build_tape_injection_with_prompt, count_memory_context_files, get_global_memory_dir,
    get_memory_dir, get_memory_repo_dir, is_memory_initialized, list_memory_files,
    precompute_tape_context, settings_from_config, tape_gate_enabled,
};
pub use service::MemoryService;
pub use tape::{
    GitCheckpoint, KeywordAnchorInstruction, SmartTapePrecompute, TapeAnchor, TapeAnchorConfig,
    TapeAnchorMeta, TapeAnchorMode, TapeAnchorType, TapeConfig, TapeContextConfig,
    TapeContextStrategy, TapeKeywordConfig, TapeQuery, TapeService,
};
pub use tools::MemoryToolDisplayFormatter;
pub use types::{MemoryContextKind, MemoryDirSettings, MemoryInjectionMode, MemorySettings};
