pub use phi_sdk::transcript::{
    AgentMessage, ContentBlock, TranscriptEntry, TranscriptEntryKind, UserContentBlock,
};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionAllowEntry {
    pub tool: String,
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(flatten)]
    pub transcript: phi_sdk::transcript::Transcript,
    pub name: CompactString,
    pub context_window: u64,
    pub model: CompactString,
    #[serde(default = "default_switch")]
    pub current_switch: CompactString,
    pub provider: CompactString,
    pub working_dir: CompactString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_list_id: Option<CompactString>,
    pub agent_kind: phi_sdk::agents::AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<phi_sdk::agent_manager::AgentAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidechain: Option<SessionSidechain>,
    #[serde(default)]
    pub children: Vec<ChildSessionLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_run_ids: Vec<phi_sdk::workflow::WorkflowRunId>,
    #[serde(default)]
    pub permission_allowlist: Vec<PermissionAllowEntry>,
    #[serde(default)]
    pub total_estimated_tokens: u64,
}

pub(super) const MAX_SIDECHAIN_DEPTH: usize = 32;
const MAX_SESSION_ID_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSidechain {
    working_dir: CompactString,
    ancestors: Vec<CompactString>,
}

impl SessionSidechain {
    pub(super) fn directory(&self) -> std::path::PathBuf {
        let mut directory = phi_sdk::workspace::session_scope_dir(&self.working_dir);
        for ancestor in &self.ancestors {
            directory = directory.join(ancestor.as_str()).join("subagents");
        }
        directory
    }

    fn for_parent(parent: &Session) -> Self {
        let mut sidechain = parent.sidechain.clone().unwrap_or_else(|| Self {
            working_dir: parent.working_dir.clone(),
            ancestors: Vec::new(),
        });
        sidechain.ancestors.push(parent.id.clone());
        sidechain
    }
}

pub(super) fn valid_session_id(value: &str) -> bool {
    value.len() <= MAX_SESSION_ID_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSessionLink {
    pub child_session_id: CompactString,
    pub child_agent_id: String,
    pub child_working_dir: CompactString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<phi_sdk::workflow::WorkflowRunId>,
    // Terminal Workflow records own results; parent journals retain only links.
    #[serde(skip)]
    pub(crate) recovered_outcome: Option<SubagentOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentChildLink {
    pub child_session_id: CompactString,
    pub child_agent_id: String,
    pub child_working_dir: CompactString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub outcome: SubagentOutcome,
    pub admission: phi_sdk::agents::SubagentAdmission,
    #[serde(skip)]
    pub(crate) mode: Option<phi_sdk::agent_manager::DelegationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubagentOutcome {
    Running,
    Finished { result: String, is_error: bool },
    Cancelled,
    Interrupted,
}

impl ChildSessionLink {
    pub(crate) fn from_creation(creation: &phi_sdk::agents::AgentCreation) -> Self {
        Self {
            child_session_id: creation.instance_id().as_str().into(),
            child_agent_id: creation.instance_id().as_str().into(),
            child_working_dir: creation.working_dir().to_string_lossy().as_ref().into(),
            definition: Some(creation.definition().to_owned()),
            alias: creation.alias().map(|alias| alias.as_str().to_owned()),
            parent_tool_call_id: None,
            workflow_run_id: creation.workflow_run_id(),
            recovered_outcome: None,
        }
    }
}

impl From<&SubagentChildLink> for ChildSessionLink {
    fn from(link: &SubagentChildLink) -> Self {
        Self {
            child_session_id: link.child_session_id.clone(),
            child_agent_id: link.child_agent_id.clone(),
            child_working_dir: link.child_working_dir.clone(),
            definition: link.definition.clone(),
            alias: link.alias.clone(),
            parent_tool_call_id: link.parent_tool_call_id.clone(),
            workflow_run_id: None,
            recovered_outcome: None,
        }
    }
}

impl From<&ChildSessionLink> for SubagentChildLink {
    fn from(link: &ChildSessionLink) -> Self {
        Self {
            child_session_id: link.child_session_id.clone(),
            child_agent_id: link.child_agent_id.clone(),
            child_working_dir: link.child_working_dir.clone(),
            definition: link.definition.clone(),
            alias: link.alias.clone(),
            parent_tool_call_id: link.parent_tool_call_id.clone(),
            outcome: link
                .recovered_outcome
                .clone()
                .unwrap_or(SubagentOutcome::Interrupted),
            admission: phi_sdk::agents::SubagentAdmission::Accepted,
            mode: None,
        }
    }
}

impl From<&phi_sdk::agents::SubagentStatus> for SubagentChildLink {
    fn from(status: &phi_sdk::agents::SubagentStatus) -> Self {
        let creation = &status.creation;
        Self {
            child_session_id: creation.instance_id().as_str().into(),
            child_agent_id: creation.instance_id().as_str().into(),
            child_working_dir: creation.working_dir().to_string_lossy().as_ref().into(),
            definition: Some(creation.definition().to_owned()),
            alias: creation.alias().map(|alias| alias.as_str().to_owned()),
            parent_tool_call_id: status.parent_tool_call_id.clone(),
            outcome: status
                .outcome
                .as_ref()
                .map(Into::into)
                .unwrap_or(SubagentOutcome::Running),
            admission: status.admission,
            mode: Some(creation.mode()),
        }
    }
}

impl SubagentChildLink {
    pub(crate) fn display_name(&self) -> String {
        match (&self.alias, &self.definition) {
            (Some(alias), Some(definition)) => format!("{alias} ({definition})"),
            (Some(alias), None) => alias.clone(),
            (None, Some(definition)) => definition.clone(),
            (None, None) => self.child_agent_id.clone(),
        }
    }
}

impl From<&Result<String, phi_sdk::PhiFailure>> for SubagentOutcome {
    fn from(outcome: &Result<String, phi_sdk::PhiFailure>) -> Self {
        match outcome {
            Ok(result) => Self::Finished {
                result: result.clone(),
                is_error: false,
            },
            Err(failure) if failure.is_user_cancelled() => Self::Cancelled,
            Err(failure) if failure.kind == phi_sdk::PhiFailureKind::Cancelled => Self::Interrupted,
            Err(failure) => Self::Finished {
                result: failure.message.clone(),
                is_error: true,
            },
        }
    }
}

impl Session {
    pub(crate) fn bind_task_list(
        &mut self,
        binding: Option<&phi_sdk::tasks::TaskListBinding>,
    ) -> anyhow::Result<()> {
        let list_id = binding.map(|binding| binding.list_id());
        anyhow::ensure!(
            self.task_list_id
                .as_deref()
                .is_none_or(|saved| Some(saved) == list_id),
            "saved task list conflicts with the prepared Instance binding"
        );
        self.task_list_id = list_id.map(Into::into);
        Ok(())
    }

    pub(crate) fn set_transcript(&mut self, transcript: phi_sdk::transcript::Transcript) {
        self.total_estimated_tokens = transcript.estimated_context_tokens();
        self.transcript = transcript;
    }

    pub(crate) fn creator_id(&self) -> Option<&phi_sdk::agent_instance::AgentInstanceId> {
        self.agent_kind.creator()
    }

    pub(crate) fn is_delegated(&self) -> bool {
        self.creator_id().is_some()
    }

    pub(crate) fn bind_creation(
        &mut self,
        creation: &phi_sdk::agents::AgentCreation,
        parent: &Session,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.id.as_str() == creation.instance_id().as_str()
                && parent.id.as_str() == creation.creator_id().as_str(),
            "Session does not match captured creation"
        );
        self.agent_kind = creation.kind().clone();
        self.definition = Some(creation.definition().into());
        self.alias = creation.alias().cloned();
        self.sidechain = matches!(self.agent_kind, phi_sdk::agents::AgentKind::Subagent { .. })
            .then(|| SessionSidechain::for_parent(parent));
        self.validate_storage()
    }

    pub(crate) fn validate_parent(&self, parent: &Session) -> anyhow::Result<()> {
        self.validate_storage()?;
        anyhow::ensure!(
            self.creator_id().map(|id| id.as_str()) == Some(parent.id.as_str())
                && self
                    .sidechain
                    .as_ref()
                    .is_none_or(|sidechain| { sidechain == &SessionSidechain::for_parent(parent) }),
            "child Session does not match parent storage association"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn bind_test_parent(&mut self, parent: &Session) {
        self.agent_kind = phi_sdk::agents::AgentKind::Subagent {
            creator: phi_sdk::agent_instance::AgentInstanceId::new(parent.id.as_str()),
        };
        self.sidechain = Some(SessionSidechain::for_parent(parent));
    }

    pub(crate) fn validate_storage(&self) -> anyhow::Result<()> {
        use phi_sdk::agents::AgentKind;
        anyhow::ensure!(valid_session_id(&self.id), "invalid Session ID");
        if let Some(creator) = self.creator_id() {
            anyhow::ensure!(
                valid_session_id(creator.as_str()) && creator.as_str() != self.id.as_str(),
                "invalid Session creator"
            );
        }
        match (&self.agent_kind, &self.sidechain) {
            (AgentKind::Subagent { creator }, Some(sidechain)) => {
                anyhow::ensure!(
                    std::path::Path::new(sidechain.working_dir.as_str()).is_absolute()
                        && !sidechain.ancestors.is_empty()
                        && sidechain.ancestors.len() <= MAX_SIDECHAIN_DEPTH
                        && sidechain.ancestors.last().map(|id| id.as_str())
                            == Some(creator.as_str())
                        && sidechain
                            .ancestors
                            .iter()
                            .all(|id| valid_session_id(id) && id != &self.id),
                    "invalid Session sidechain"
                );
                let mut unique = std::collections::HashSet::new();
                anyhow::ensure!(
                    sidechain.ancestors.iter().all(|id| unique.insert(id)),
                    "cyclic Session sidechain"
                );
            }
            (AgentKind::Subagent { .. }, None) => anyhow::bail!("missing Session sidechain"),
            (_, Some(_)) => anyhow::bail!("only ordinary subagents have Session sidechains"),
            (_, None) => {}
        }
        Ok(())
    }
}

impl Deref for Session {
    type Target = phi_sdk::transcript::Transcript;

    fn deref(&self) -> &Self::Target {
        &self.transcript
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.transcript
    }
}

pub(crate) fn default_switch() -> CompactString {
    CompactString::new("default")
}

pub(crate) fn new(
    provider: &str,
    model: &str,
    context_window: u64,
    working_dir: &std::path::Path,
) -> Session {
    Session {
        transcript: phi_sdk::transcript::Transcript::new(),
        name: CompactString::new(""),
        context_window,
        model: CompactString::new(model),
        current_switch: default_switch(),
        provider: CompactString::new(provider),
        working_dir: CompactString::new(working_dir.to_string_lossy()),
        task_list_id: None,
        agent_kind: phi_sdk::agents::AgentKind::Master,
        definition: None,
        alias: None,
        sidechain: None,
        children: Vec::new(),
        workflow_run_ids: Vec::new(),
        permission_allowlist: Vec::new(),
        total_estimated_tokens: 0,
    }
}
