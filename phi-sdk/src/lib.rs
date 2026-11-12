pub use crate::core::event::{
    CancellationReason, PhiEvent, PhiEventEnvelope, PhiFailure, PhiFailureKind, PhiUsage,
    ProviderRecovery, RecoveryReason,
};
pub use crate::factory::{Phi, PhiBuildError, PhiBuilder, PhiQueryError, PhiRuntimeServices};
pub use crate::harness::instance::builder::{
    AgentInstanceBuildError, AgentInstanceBuilder, AgentInstanceCandidate, AgentInstanceOptions,
};
pub use crate::harness::instance::handle::{PhiStream, PhiStreamError, Query};

mod agent;
pub mod config;
pub mod context;
mod core;
pub mod extension;
mod harness;
pub mod hooks;

/// Lifecycle boundary inputs and the decisions or contributions their owners accept.
pub mod lifecycle {
    pub use crate::core::lifecycle::{
        AgentRunEvent, AgentTurnEndEvent, AgentTurnEndOutcome, AgentTurnEvent,
        BuildSystemPromptEvent, BuildSystemPromptOutcome, CompletionEvent, CompletionOutcome,
        ContextEditDraft, NotificationEvent, NotificationKind, PostToolFailureOutcome,
        PostToolUseOutcome, PreToolUseOutcome, PromptContextEvent, PromptContextOutcome,
        ProviderContextEvent, ProviderContextOutcome, ResourcesDiscoverReason, SessionEndEvent,
        SessionEndReason, SessionStartEvent, SessionStartReason, SessionTreeReason,
        TaskCompletedEvent, TaskCompletionTrigger, TaskCreatedEvent, TaskGateOutcome,
        TaskLifecycleContext, TaskLifecycleStatus, TaskLifecycleTask, TeammateIdleEvent,
        TeammateIdleOutcome, ToolFailureEvent, ToolResultEvent, ToolUseEvent, UserPromptDispatch,
        UserPromptExpansionEvent, UserPromptExpansionOutcome, UserPromptSubmitEvent,
        UserPromptSubmitOutcome, UserPromptSubmitSource,
    };
}

pub mod messages;
pub mod models;
pub mod permission;
mod provider;
pub mod providers {
    pub use crate::provider::{ProviderApi, api_key_available, login_chatgpt};
}
mod factory;
pub mod sandbox;
pub mod tools;
pub mod transcript;
mod utils;

pub mod workspace {
    pub use crate::utils::path::{
        builtin_skills_dir, encode_working_dir_key, expand_home_path, phi_config_dir,
        phi_session_root, session_scope_dir,
    };
    pub use crate::utils::workspace::{
        ProjectMeta, get_project_meta, git_root, git_worktree_id, new_worktree_path,
        project_scope_root,
    };
}

pub mod agent_manager {
    pub use crate::harness::manager::AgentAccess;
    pub use crate::harness::manager::AgentDiscovery;
    pub use crate::harness::manager::AgentFinalization;
    pub use crate::harness::manager::AgentFinalizationStatus;
    pub use crate::harness::manager::AgentManager;
    pub use crate::harness::manager::AgentManagerError;
    pub use crate::harness::manager::AgentMemberSnapshot;
    pub use crate::harness::manager::AgentOperation;
    pub use crate::harness::manager::AgentRegistrationId;
    pub use crate::harness::manager::AgentTeamId;
    pub use crate::harness::manager::message::AgentMessageDelivery;
    pub use crate::harness::manager::message::AgentMessageEnvelope;
    pub use crate::harness::manager::message::AgentMessageId;
    pub use crate::harness::manager::message::AgentMessageKind;
    pub use crate::harness::manager::message::AgentMessageReceipt;
    pub use crate::harness::manager::message::AgentMessageUpdate;
    pub use crate::harness::manager::message::AgentMessageUpdateWait;
    pub use crate::harness::manager::message::AgentMessageUpdates;
    pub use crate::harness::manager::message::AgentMessageWait;
    pub use crate::harness::manager::message::MAX_AGENT_WAIT_MS;
    pub use crate::harness::manager::message::agent_wait_timeout;
    pub use crate::harness::manager::preparation::{AgentSpawnRequest, DelegationMode};
    pub use crate::harness::manager::team::{
        TeamCapability, TeamNamespaceError, TeamStorageRoot, TeamStorageRootError, team_namespace,
    };
    pub use crate::harness::manager::{AgentAlias, InvalidAgentAlias};
}

/// Harness-owned background script execution through existing Agent APIs.
pub mod workflow {
    pub use crate::harness::workflow::{
        WorkflowError, WorkflowInvocationSnapshot, WorkflowInvocationStatus, WorkflowLaunch,
        WorkflowLimits, WorkflowRequest, WorkflowRun, WorkflowRunId, WorkflowRuntime,
        WorkflowSnapshot, WorkflowStatus, WorkflowSubscription, WorkflowWorkerSnapshot,
    };
}

pub mod tasks {
    pub use crate::harness::task::{
        TaskItemView, TaskListBinding, TaskListBindingError, TaskListView, TaskStatusView,
        TaskSubscription,
    };
}

pub mod agent_instance {
    pub use crate::factory::AgentRuntimeResources;
    pub use crate::harness::instance::AgentInstanceView;
    pub use crate::harness::instance::bash::{
        BashExecution, BashMode, BashOutputSubscription, BashStatus,
    };
    pub use crate::harness::instance::builder::BranchSelection;
    pub use crate::harness::instance::builder::RuntimeCandidate;
    pub use crate::harness::instance::handle::AgentInstance;
    pub use crate::harness::instance::handle::AgentInstanceError;
    pub use crate::harness::instance::handle::AgentInstanceEvent;
    pub use crate::harness::instance::handle::AgentInstanceId;
    pub use crate::harness::instance::handle::AgentInstanceStatus;
    pub use crate::harness::instance::handle::LiveRunSnapshot;
    pub use crate::harness::instance::handle::LiveToolSnapshot;
    pub use crate::harness::instance::handle::{AgentInstanceSnapshot, AgentSnapshotSubscription};
    pub use crate::harness::instance::{ExecutionProgress, ExecutionStatus, ExecutionTool};
}

pub mod agents {
    pub use crate::agent::custom::{
        AgentCatalog, AgentDefinition, AgentDefinitionOrigin, AgentDiagnostic, AgentDiagnosticKind,
        AgentLoadResult, AgentValidationReason, load, load_from_roots,
    };
    pub use crate::harness::instance::child::SubagentActivity;
    pub use crate::harness::instance::child::SubagentAdmission;
    pub use crate::harness::instance::child::SubagentSnapshot;
    pub use crate::harness::instance::child::SubagentStatus;
    pub use crate::harness::instance::execution::PhiSubagentError;
    pub use crate::harness::manager::AgentKind;
    pub use crate::harness::manager::preparation::AgentCreation;
    pub use crate::harness::manager::preparation::PreparedSubagent;
    pub use crate::harness::manager::preparation::SubagentAssociation;
    pub use crate::harness::manager::preparation::SubagentCompletion;
    pub use crate::harness::manager::preparation::SubagentPreparation;
}

pub mod interaction {
    pub use crate::core::interaction::{
        InteractionAnswer, InteractionEnvelope, InteractionRequest, InteractionResponse,
        PermissionDecision, PermissionInteraction,
    };
}

pub mod turn {
    pub use crate::context::InjectedContext;
    pub use crate::core::turn::{
        ImageInput, ImageInputError, QueuedSubmissionDrainMode, SubmissionDiscardReason,
        SubmissionId, TurnPrompt, TurnToolPolicy,
    };
    pub use crate::harness::instance::builder::DirectedDelegation;
    pub use crate::harness::instance::execution::PhiTurnError;
    pub use crate::harness::instance::handle::SubmissionDispatch;
    pub use crate::harness::instance::handle::SubmissionDisposition;
}
