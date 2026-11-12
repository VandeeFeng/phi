use super::{AgentAlias, AgentKind};
use crate::core::event::PhiFailure;
use crate::transcript::Transcript;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

/// Initial result delivery; independent of child lifetime and history storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMode {
    /// Hold the Agent tool result until completion; do not enqueue initial feedback.
    Foreground,
    /// Return a launch receipt and enqueue completion for a later parent turn.
    #[default]
    Background,
}

/// Creation intent; caller and tool identities are supplied by trusted execution scope.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnRequest {
    pub role: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<AgentAlias>,
    #[serde(default)]
    pub working_dir: Option<std::path::PathBuf>,
    /// Omission defers to caller defaults; runtime policy can force background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<DelegationMode>,
}

impl AgentSpawnRequest {
    pub fn new(role: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            prompt: prompt.into(),
            alias: None,
            working_dir: None,
            mode: None,
        }
    }

    pub fn mode(mut self, mode: DelegationMode) -> Self {
        self.mode = Some(mode);
        self
    }

    pub fn alias(mut self, alias: AgentAlias) -> Self {
        self.alias = Some(alias);
        self
    }

    pub fn working_dir(mut self, working_dir: impl Into<std::path::PathBuf>) -> Self {
        self.working_dir = Some(working_dir.into());
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentResultRouting {
    #[default]
    Creator,
    Workflow(crate::harness::workflow::WorkflowRunId),
}

/// Captured creation facts; independent of checklist records and invocation completion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentCreation {
    instance_id: crate::harness::instance::AgentInstanceId,
    kind: AgentKind,
    definition: String,
    alias: Option<AgentAlias>,
    prompt: String,
    working_dir: PathBuf,
    #[serde(skip)]
    task_list: Option<crate::tasks::TaskListBinding>,
    #[serde(skip)]
    reservation: Option<uuid::Uuid>,
    parent_tool_call_id: Option<compact_str::CompactString>,
    mode: DelegationMode,
    #[serde(skip)]
    result_routing: AgentResultRouting,
}

impl AgentCreation {
    pub(crate) fn new(
        creator_id: crate::harness::instance::AgentInstanceId,
        request: &AgentSpawnRequest,
        working_dir: PathBuf,
        task_list: Option<crate::tasks::TaskListBinding>,
    ) -> Self {
        Self {
            instance_id: crate::harness::instance::AgentInstanceId::new(
                uuid::Uuid::new_v4().to_string(),
            ),
            // A requested name or Team ID cannot establish teammate identity.
            kind: AgentKind::Subagent {
                creator: creator_id,
            },
            definition: request.role.clone(),
            alias: request.alias.clone(),
            prompt: request.prompt.clone(),
            working_dir,
            task_list,
            reservation: None,
            parent_tool_call_id: None,
            mode: request.mode.unwrap_or_default(),
            result_routing: AgentResultRouting::Creator,
        }
    }

    pub(super) fn bind_workflow(&mut self, run: crate::harness::workflow::WorkflowRunId) {
        self.result_routing = AgentResultRouting::Workflow(run);
    }

    pub(crate) fn result_routing(&self) -> AgentResultRouting {
        self.result_routing
    }

    pub fn workflow_run_id(&self) -> Option<crate::harness::workflow::WorkflowRunId> {
        match self.result_routing {
            AgentResultRouting::Workflow(run) => Some(run),
            AgentResultRouting::Creator => None,
        }
    }

    pub(super) fn bind_teammate(&mut self, team: super::AgentTeamId, token: uuid::Uuid) {
        self.kind = AgentKind::Teammate {
            creator: self.creator_id().clone(),
            team,
        };
        self.reservation = Some(token);
        self.mode = DelegationMode::Background;
    }

    pub(super) fn with_mode(mut self, mode: DelegationMode) -> Self {
        self.mode = mode;
        self
    }

    pub(super) fn bind_alias_reservation(&mut self, token: uuid::Uuid) {
        self.reservation = Some(token);
    }

    pub(super) fn reservation(&self) -> Option<uuid::Uuid> {
        self.reservation
    }

    pub(crate) fn with_tool_call(mut self, id: Option<compact_str::CompactString>) -> Self {
        self.parent_tool_call_id = id;
        self
    }

    pub(crate) fn parent_tool_call_id(&self) -> Option<&str> {
        self.parent_tool_call_id.as_deref()
    }

    pub fn instance_id(&self) -> &crate::harness::instance::AgentInstanceId {
        &self.instance_id
    }

    pub fn creator_id(&self) -> &crate::harness::instance::AgentInstanceId {
        self.kind.creator().expect("child creation has a creator")
    }

    pub fn mode(&self) -> DelegationMode {
        self.mode
    }

    pub fn kind(&self) -> &AgentKind {
        &self.kind
    }

    pub fn alias(&self) -> Option<&AgentAlias> {
        self.alias.as_ref()
    }

    pub fn definition(&self) -> &str {
        &self.definition
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }

    pub fn task_list(&self) -> Option<&crate::tasks::TaskListBinding> {
        self.task_list.as_ref()
    }
}

// Canonical directory preparation and isolated SDK child assembly, outside registry locks.

pub(crate) fn resolve_working_dir(
    parent: &std::path::Path,
    requested: Option<&std::path::Path>,
) -> Result<PathBuf, crate::harness::instance::execution::PhiSubagentError> {
    let path = requested
        .map(|path| parent.join(path))
        .unwrap_or_else(|| parent.to_path_buf());
    let result = match requested {
        Some(selected) if selected.as_os_str().is_empty() => {
            Err(std::io::Error::other("working directory is empty"))
        }
        _ => canonical_directory(&path),
    };
    result.map_err(|source| {
        crate::harness::instance::execution::PhiSubagentError::WorkingDirectory { path, source }
    })
}

pub(crate) async fn prepare_working_dir(
    parent: PathBuf,
    requested: Option<PathBuf>,
) -> Result<PathBuf, crate::harness::instance::execution::PhiSubagentError> {
    let path = requested
        .as_ref()
        .map(|path| parent.join(path))
        .unwrap_or_else(|| parent.clone());
    tokio::task::spawn_blocking(move || resolve_working_dir(&parent, requested.as_deref()))
        .await
        .map_err(|source| {
            crate::harness::instance::execution::PhiSubagentError::WorkingDirectory {
                path,
                source: std::io::Error::other(source),
            }
        })?
}

pub(crate) async fn revalidate_directory(
    directory: &std::path::Path,
) -> Result<(), crate::harness::instance::execution::PhiSubagentError> {
    let path = directory.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let canonical = resolve_working_dir(&path, None)?;
        if canonical != path {
            return Err(
                crate::harness::instance::execution::PhiSubagentError::WorkingDirectory {
                    path,
                    source: std::io::Error::other("working directory changed during preparation"),
                },
            );
        }
        Ok(())
    })
    .await
    .map_err(|source| {
        crate::harness::instance::execution::PhiSubagentError::WorkingDirectory {
            path: directory.to_path_buf(),
            source: std::io::Error::other(source),
        }
    })?
}

fn canonical_directory(path: &std::path::Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "working directory is not a directory",
        ));
    }
    Ok(canonical)
}

pub(crate) async fn prepare_sdk_child(
    phi: &crate::factory::Phi,
    creation: &AgentCreation,
) -> Result<crate::AgentInstanceCandidate, crate::AgentInstanceBuildError> {
    let phi = phi
        .clone()
        .with_runtime_services(phi.runtime_services.isolated_session());
    let mut transcript = Transcript::new();
    transcript.id = creation.instance_id().as_str().into();
    let mut builder = phi
        .agent_instance_builder()
        .delegated_child()
        .working_dir(creation.working_dir().to_path_buf())
        .transcript(transcript);
    if let Some(binding) = creation.task_list() {
        builder = builder.task_list(binding.clone());
    }
    builder.build().await
}

pub(crate) async fn wait_for_cancellation(cancellation: &mut tokio::sync::watch::Receiver<bool>) {
    while !*cancellation.borrow_and_update() {
        if cancellation.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

// Candidate preparation and staged association; Manager retains startup authority.

/// Prepared application runtime and a staged association; startup remains SDK-owned.
pub struct PreparedSubagent {
    pub(crate) candidate: crate::AgentInstanceCandidate,
    pub(crate) association: std::sync::Arc<dyn SubagentAssociation>,
}

impl PreparedSubagent {
    pub fn new(
        candidate: crate::AgentInstanceCandidate,
        association: std::sync::Arc<dyn SubagentAssociation>,
    ) -> Self {
        Self {
            candidate,
            association,
        }
    }
}

/// Associates resources with an idle registered instance before Manager starts execution.
pub trait SubagentAssociation: Send + Sync {
    fn associate(
        &self,
        instance: crate::harness::instance::handle::AgentInstance,
        creation: AgentCreation,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    fn rollback(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

/// Supplies a complete child candidate and staged association; Manager owns startup.
pub trait SubagentPreparation: Send + Sync {
    fn prepare(
        &self,
        creation: AgentCreation,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedSubagent, String>> + Send>>;
}

/// Immutable initial completion; history remains owned by the instance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubagentCompletion {
    pub outcome: Result<String, PhiFailure>,
}

#[cfg(test)]
mod directory_contract_tests {
    use super::{AgentSpawnRequest, DelegationMode};

    #[test]
    fn spawn_request_accepts_directory_but_not_runtime_context() {
        let request = serde_json::json!({"role": "reviewer", "prompt": "inspect"});
        let parsed: AgentSpawnRequest = serde_json::from_value(request.clone()).unwrap();
        assert_eq!(parsed.working_dir, None);
        assert_eq!(parsed.mode, None);
        for mode in [DelegationMode::Foreground, DelegationMode::Background] {
            let mut selected = request.clone();
            selected["mode"] = serde_json::to_value(mode).unwrap();
            let parsed: AgentSpawnRequest = serde_json::from_value(selected).unwrap();
            assert_eq!(parsed.mode, Some(mode));
        }
        for mode in [serde_json::json!(true), serde_json::json!("unsupported")] {
            let mut invalid = request.clone();
            invalid["mode"] = mode;
            assert!(serde_json::from_value::<AgentSpawnRequest>(invalid).is_err());
        }
        let mut selected = request.clone();
        selected["working_dir"] = serde_json::json!("../plain-directory");
        let parsed: AgentSpawnRequest = serde_json::from_value(selected).unwrap();
        assert_eq!(
            parsed.working_dir.as_deref(),
            Some(std::path::Path::new("../plain-directory"))
        );
        for field in ["caller", "kind", "parent_tool_call_id", "unknown_field"] {
            let mut invalid = request.clone();
            invalid[field] = serde_json::json!("unsupported");
            assert!(serde_json::from_value::<AgentSpawnRequest>(invalid).is_err());
        }
    }
}
