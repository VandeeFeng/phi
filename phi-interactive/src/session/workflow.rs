use phi_sdk::workflow::{WorkflowInvocationStatus, WorkflowRequest, WorkflowRunId, WorkflowStatus};
use serde::{Deserialize, Serialize};

use super::{ChildSessionLink, Session, SubagentOutcome, storage::WorkflowStorage};

const WORKFLOW_RECORD_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkflowRecord {
    version: u32,
    run_id: WorkflowRunId,
    creator_id: String,
    #[serde(default)]
    source: String,
    pub(crate) name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    phases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<serde_json::Value>,
    pub(crate) state: WorkflowState,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum WorkflowState {
    Prepared,
    LaunchFailed {
        error: String,
    },
    Settled {
        #[serde(with = "SavedExecutionStatus")]
        execution_status: WorkflowStatus,
        value: Option<serde_json::Value>,
        error: Option<String>,
        cleanup_errors: Vec<String>,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        logs: Vec<String>,
        invocations: Vec<WorkflowInvocation>,
        #[serde(default)]
        feedback: WorkflowFeedback,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkflowInvocation {
    pub(crate) id: usize,
    pub(crate) role: String,
    pub(crate) prompt: String,
    pub(crate) label: Option<String>,
    pub(crate) phase: Option<String>,
    #[serde(with = "SavedInvocationStatus")]
    pub(crate) status: WorkflowInvocationStatus,
    pub(crate) worker_instance_id: Option<String>,
    pub(crate) worker_session: Option<ChildSessionLink>,
    pub(crate) value: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowFeedback {
    #[default]
    Pending,
    Admitted,
    CreatorClosed,
}

impl WorkflowRecord {
    pub(crate) fn new(
        id: WorkflowRunId,
        creator: &str,
        request: &WorkflowRequest,
        state: WorkflowState,
    ) -> Self {
        Self {
            version: WORKFLOW_RECORD_VERSION,
            run_id: id,
            creator_id: creator.to_owned(),
            source: request.source().to_owned(),
            name: request.name().to_owned(),
            description: request.description().to_owned(),
            phases: request.phases().to_vec(),
            args: request.args().cloned(),
            state,
        }
    }

    pub(crate) fn load(session: &Session, id: WorkflowRunId) -> anyhow::Result<Self> {
        let record: Self = WorkflowStorage::new(session, id)?.load()?;
        anyhow::ensure!(
            record.version == WORKFLOW_RECORD_VERSION
                && record.run_id == id
                && record.creator_id == session.id.as_str()
                && session.workflow_run_ids.contains(&id),
            "Workflow record identity/version mismatch"
        );
        if let WorkflowState::Settled {
            execution_status,
            invocations,
            ..
        } = &record.state
        {
            anyhow::ensure!(
                matches!(
                    execution_status,
                    WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Stopped
                ),
                "Workflow record has no terminal execution status"
            );
            let mut invocation_ids = std::collections::HashSet::new();
            anyhow::ensure!(
                invocations
                    .iter()
                    .all(|item| invocation_ids.insert(item.id)),
                "Workflow record has duplicate invocation IDs"
            );
            anyhow::ensure!(
                invocations.iter().all(|item| matches!(
                    item.status,
                    WorkflowInvocationStatus::Succeeded
                        | WorkflowInvocationStatus::Failed
                        | WorkflowInvocationStatus::Cancelled
                )),
                "Workflow record has no terminal invocation status"
            );
        }
        Ok(record)
    }
}

impl WorkflowInvocation {
    pub(crate) fn worker<'a>(
        &self,
        children: &'a [ChildSessionLink],
    ) -> Option<&'a ChildSessionLink> {
        let instance = self.worker_instance_id.as_deref()?;
        let worker = self.worker_session.as_ref()?;
        if instance != worker.child_agent_id || instance != worker.child_session_id.as_str() {
            return None;
        }
        children.iter().find(|child| {
            child.child_agent_id == instance
                && child.child_session_id == worker.child_session_id
                && worker
                    .parent_tool_call_id
                    .as_deref()
                    .zip(child.parent_tool_call_id.as_deref())
                    .is_none_or(|(saved, current)| saved == current)
        })
    }

    pub(crate) fn outcome(&self) -> Option<SubagentOutcome> {
        match (self.status, self.value.as_ref(), self.error.as_ref()) {
            (WorkflowInvocationStatus::Succeeded, Some(result), None) => {
                Some(SubagentOutcome::Finished {
                    result: result.clone(),
                    is_error: false,
                })
            }
            (WorkflowInvocationStatus::Failed, None, Some(result)) => {
                Some(SubagentOutcome::Finished {
                    result: result.clone(),
                    is_error: true,
                })
            }
            (WorkflowInvocationStatus::Cancelled, None, _) => Some(SubagentOutcome::Cancelled),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "WorkflowInvocationStatus", rename_all = "snake_case")]
enum SavedInvocationStatus {
    Queued,
    Preparing,
    Running,
    Finalizing,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "WorkflowStatus", rename_all = "snake_case")]
enum SavedExecutionStatus {
    Running,
    Finishing,
    Stopping,
    Completed,
    Failed,
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_links_reject_conflicting_call_ids() {
        let calls = [None, Some("workflow-call"), Some("ordinary-call")];
        for saved_call in calls {
            for parent_call in calls {
                let worker: ChildSessionLink = serde_json::from_value(serde_json::json!({
                    "child_agent_id": "worker", "child_session_id": "worker",
                    "child_working_dir": "/tmp", "parent_tool_call_id": saved_call,
                    "mode": "foreground",
                }))
                .unwrap();
                let invocation = WorkflowInvocation {
                    id: 0,
                    role: "probe".into(),
                    prompt: "inspect".into(),
                    label: None,
                    phase: None,
                    status: WorkflowInvocationStatus::Succeeded,
                    worker_instance_id: Some(worker.child_agent_id.clone()),
                    worker_session: Some(worker.clone()),
                    value: Some("result".into()),
                    error: None,
                };
                let mut parent_link = worker;
                parent_link.parent_tool_call_id = parent_call.map(str::to_owned);
                let children = [parent_link];
                let conflict = matches!((saved_call, parent_call), (Some(saved), Some(parent)) if saved != parent);
                assert_eq!(
                    invocation.worker(&children).is_some(),
                    !conflict,
                    "saved: {saved_call:?}, parent: {parent_call:?}"
                );
            }
        }
    }

    #[test]
    fn version_two_records_round_trip_and_reject_foreign_or_active_history() {
        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-workflow-record-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let id: WorkflowRunId = uuid::Uuid::new_v4().to_string().parse().unwrap();
        session.workflow_run_ids.push(id);
        let storage = WorkflowStorage::new(&session, id).unwrap();
        let legacy = serde_json::json!({
            "version": 2, "run_id": id, "creator_id": session.id, "name": "saved workflow",
            "state": {"status": "settled", "execution_status": "completed", "value": "report", "cleanup_errors": [],
                "invocations": [{"id": 0, "role": "probe", "prompt": "inspect", "status": "succeeded", "value": "result"}]},
        });
        for state in [
            serde_json::json!({"status": "prepared"}),
            serde_json::json!({"status": "launch_failed", "error": "launch failed"}),
            legacy["state"].clone(),
        ] {
            let mut value = legacy.clone();
            value["state"] = state;
            storage.save(&value).unwrap();
            let record = WorkflowRecord::load(&session, id).unwrap();
            storage.save(&record).unwrap();
            let round_trip = WorkflowRecord::load(&session, id).unwrap();
            assert_eq!(
                serde_json::to_value(record).unwrap(),
                serde_json::to_value(round_trip).unwrap()
            );
        }
        for (pointer, replacement) in [
            ("/version", serde_json::json!(0)),
            ("/creator_id", serde_json::json!("unrelated")),
            (
                "/run_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("/state/execution_status", serde_json::json!("running")),
            ("/state/invocations/0/status", serde_json::json!("running")),
            (
                "/state/invocations",
                serde_json::json!([
                    legacy["state"]["invocations"][0],
                    legacy["state"]["invocations"][0],
                ]),
            ),
        ] {
            let mut invalid = legacy.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            storage.save(&invalid).unwrap();
            assert!(WorkflowRecord::load(&session, id).is_err(), "{pointer}");
        }
        std::fs::remove_file(storage.path()).unwrap();
        assert!(WorkflowRecord::load(&session, id).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
