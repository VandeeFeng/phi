use crate::core::event::CoreEventScope;
use crate::harness::task::{
    TaskAssignment, TaskChanges, TaskDetails, TaskId, TaskRequest, TaskResult, TaskStatus,
};
use crate::permission::execution_scope::OuterTurnPolicy;
use crate::permission::runtime::PermissionRuntime;
use rig::tool::{Tool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const DEFAULT_PAGE_SIZE: usize = 100;

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCreateArgs {
    pub(crate) subject: String,
    pub(crate) description: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) active_form: Option<String>,
    #[serde(default)]
    pub(crate) metadata: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskGetArgs {
    pub(crate) task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskListArgs {
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) after: Option<String>,
    #[serde(default = "default_page_size")]
    pub(crate) limit: usize,
}

fn default_page_size() -> usize {
    DEFAULT_PAGE_SIZE
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskUpdateStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

#[derive(Debug, Default)]
pub(crate) enum TaskOwnerSelector {
    #[default]
    Unchanged,
    Assign(String),
    Release,
}

impl<'de> Deserialize<'de> for TaskOwnerSelector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<String>::deserialize(deserializer)
            .map(|owner| owner.map_or(Self::Release, Self::Assign))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskUpdateArgs {
    pub(crate) task_id: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) subject: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) active_form: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) status: Option<TaskUpdateStatus>,
    #[serde(default)]
    pub(crate) owner: TaskOwnerSelector,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) add_blocks: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) add_blocked_by: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) metadata: Option<Map<String, Value>>,
}

fn parse_task_id(id: String) -> Result<TaskId, ToolExecutionError> {
    TaskId::try_from(id).map_err(|error| tool_error(error.to_string()))
}

impl From<TaskCreateArgs> for TaskRequest {
    fn from(args: TaskCreateArgs) -> Self {
        Self::Create {
            subject: args.subject,
            description: args.description,
            active_form: args.active_form,
            metadata: args.metadata,
        }
    }
}

impl TryFrom<TaskGetArgs> for TaskRequest {
    type Error = ToolExecutionError;

    fn try_from(args: TaskGetArgs) -> Result<Self, Self::Error> {
        Ok(Self::Get(parse_task_id(args.task_id)?))
    }
}

impl TryFrom<TaskListArgs> for TaskRequest {
    type Error = ToolExecutionError;

    fn try_from(args: TaskListArgs) -> Result<Self, Self::Error> {
        Ok(Self::List {
            after: args.after.map(parse_task_id).transpose()?,
            limit: args.limit,
        })
    }
}

impl TryFrom<TaskUpdateArgs> for TaskRequest {
    type Error = ToolExecutionError;

    fn try_from(args: TaskUpdateArgs) -> Result<Self, Self::Error> {
        let task_id = parse_task_id(args.task_id)?;
        if matches!(args.status, Some(TaskUpdateStatus::Deleted)) {
            if args.subject.is_some()
                || args.description.is_some()
                || args.active_form.is_some()
                || !matches!(args.owner, TaskOwnerSelector::Unchanged)
                || args.add_blocks.is_some()
                || args.add_blocked_by.is_some()
                || args.metadata.is_some()
            {
                return Err(tool_error("deleted accepts only task_id and status"));
            }
            return Ok(Self::Delete(task_id));
        }
        Ok(Self::Update {
            task_id,
            changes: TaskChanges {
                subject: args.subject,
                description: args.description,
                active_form: args.active_form,
                status: args.status.map(|status| match status {
                    TaskUpdateStatus::Pending => TaskStatus::Pending,
                    TaskUpdateStatus::InProgress => TaskStatus::InProgress,
                    TaskUpdateStatus::Completed => TaskStatus::Completed,
                    TaskUpdateStatus::Deleted => unreachable!("handled deletion"),
                }),
                owner: match args.owner {
                    TaskOwnerSelector::Unchanged => TaskAssignment::Unchanged,
                    TaskOwnerSelector::Assign(selector) => {
                        TaskAssignment::Assign(selector.as_str().into())
                    }
                    TaskOwnerSelector::Release => TaskAssignment::Release,
                },
                add_blocks: args
                    .add_blocks
                    .map(|ids| ids.into_iter().map(parse_task_id).collect::<Result<_, _>>())
                    .transpose()?,
                add_blocked_by: args
                    .add_blocked_by
                    .map(|ids| ids.into_iter().map(parse_task_id).collect::<Result<_, _>>())
                    .transpose()?,
                metadata: args.metadata,
            },
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum TaskArgs {
    Create(TaskCreateArgs),
    Get(TaskGetArgs),
    List(TaskListArgs),
    Update(TaskUpdateArgs),
}

impl TryFrom<TaskArgs> for TaskRequest {
    type Error = ToolExecutionError;

    fn try_from(args: TaskArgs) -> Result<Self, Self::Error> {
        match args {
            TaskArgs::Create(args) => Ok(args.into()),
            TaskArgs::Get(args) => args.try_into(),
            TaskArgs::List(args) => args.try_into(),
            TaskArgs::Update(args) => args.try_into(),
        }
    }
}

pub(crate) const TASK_TOOL_NAME: &str = "Task";

pub(crate) async fn authorize_task(
    permission: &PermissionRuntime,
    request: &TaskRequest,
) -> anyhow::Result<()> {
    permission
        .authorize_tool(
            TASK_TOOL_NAME,
            &permission_input(request).to_string(),
            None,
            None,
        )
        .await
}

fn permission_input(request: &TaskRequest) -> Value {
    match request {
        TaskRequest::Create {
            subject,
            description,
            active_form,
            metadata,
        } => {
            let mut input =
                json!({"action": "create", "subject": subject, "description": description});
            insert_optional(&mut input, "active_form", active_form);
            if !metadata.is_empty() {
                input["metadata"] = json!(metadata);
            }
            input
        }
        TaskRequest::Get(id) => json!({"action": "get", "task_id": id}),
        TaskRequest::List { after, limit } => {
            let mut input = json!({"action": "list", "limit": limit});
            insert_optional(&mut input, "after", after);
            input
        }
        TaskRequest::Update { task_id, changes } => {
            let mut input = json!({"action": "update", "task_id": task_id});
            insert_optional(&mut input, "subject", &changes.subject);
            insert_optional(&mut input, "description", &changes.description);
            insert_optional(&mut input, "active_form", &changes.active_form);
            insert_optional(&mut input, "status", &changes.status);
            insert_optional(&mut input, "add_blocks", &changes.add_blocks);
            insert_optional(&mut input, "add_blocked_by", &changes.add_blocked_by);
            insert_optional(&mut input, "metadata", &changes.metadata);
            match &changes.owner {
                TaskAssignment::Unchanged => {}
                TaskAssignment::Assign(instance_id) => {
                    input["owner"] = json!({"instance_id": instance_id})
                }
                TaskAssignment::Release => input["owner"] = Value::Null,
            }
            input
        }
        TaskRequest::Delete(id) => json!({"action": "update", "task_id": id, "status": "deleted"}),
    }
}

fn insert_optional(input: &mut Value, field: &str, value: &Option<impl Serialize>) {
    if let Some(value) = value {
        input[field] = json!(value);
    }
}

fn tool_output(result: TaskResult) -> Value {
    match result {
        TaskResult::Created { id, subject } => json!({"task": {"id": id, "subject": subject}}),
        TaskResult::Found(task) => json!({"task": task.map(task_detail)}),
        TaskResult::Listed { tasks, next } => {
            let tasks = tasks.into_iter().map(|details| {
                let mut summary = json!({
                    "id": details.task.id, "subject": details.task.subject, "status": details.task.status,
                    "blocked_by": details.task.blocked_by, "unresolved": details.unresolved, "eligible": details.eligible,
                });
                insert_optional(&mut summary, "owner", &details.task.owner);
                insert_optional(&mut summary, "active_form", &details.task.active_form);
                summary
            }).collect::<Vec<_>>();
            let mut output = json!({"tasks": tasks});
            insert_optional(&mut output, "next", &next);
            output
        }
        TaskResult::Updated(details) => {
            json!({"success": true, "task_id": details.task.id.clone(), "task": task_detail(details)})
        }
        TaskResult::Deleted(id) => json!({"success": true, "task_id": id, "deleted": true}),
    }
}

fn task_detail(details: TaskDetails) -> Value {
    let mut task = json!(details.task);
    task["blocks"] = json!(details.blocks);
    task["unresolved"] = json!(details.unresolved);
    task["eligible"] = json!(details.eligible);
    task
}

pub(crate) struct TaskToolRequest {
    pub(crate) operation: TaskRequest,
    pub(crate) scope: CoreEventScope,
    pub(crate) policy: Option<OuterTurnPolicy>,
    pub(crate) lifetime: Arc<oneshot::Sender<()>>,
    pub(crate) reply: oneshot::Sender<Result<TaskResult, String>>,
}

pub(crate) type TaskToolSender = mpsc::UnboundedSender<TaskToolRequest>;

#[derive(Clone)]
pub(crate) struct TaskDispatcher {
    sender: TaskToolSender,
}

impl TaskDispatcher {
    pub(crate) fn new(sender: TaskToolSender) -> Self {
        Self { sender }
    }

    async fn call(&self, operation: TaskRequest) -> Result<Value, ToolExecutionError> {
        let mut scope = crate::core::interaction::active_interaction_scope();
        scope.parent_tool_call_id = crate::core::interaction::active_tool_call_id().map(Into::into);
        let (lifetime, active) = oneshot::channel();
        let (reply, result) = oneshot::channel();
        self.sender
            .send(TaskToolRequest {
                operation,
                scope,
                policy: OuterTurnPolicy::capture(),
                lifetime: Arc::new(lifetime),
                reply,
            })
            .map_err(|_| tool_error("Task tool dispatcher is unavailable"))?;
        let output = result
            .await
            .map_err(|_| tool_error("Task tool result channel closed"))?
            .map(tool_output)
            .map_err(tool_error);
        drop(active);
        output
    }
}

fn tool_error(message: impl Into<String>) -> ToolExecutionError {
    let message = message.into();
    ToolExecutionError::other(&message)
        .with_model_feedback(message)
        .with_retryable(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_inputs_validate_actions_and_preserve_patch_intent() {
        for input in [
            json!({}),
            json!({"action": "delete", "task_id": "1"}),
            json!({"action": "create", "subject": "Review"}),
            json!({"action": "get"}),
            json!({"action": "update"}),
        ] {
            assert!(
                serde_json::from_value::<TaskArgs>(input.clone()).is_err(),
                "{input}"
            );
        }
        for action in ["create", "get", "list", "update"] {
            let mut input = match action {
                "create" => json!({"subject": "Review", "description": "Inspect"}),
                "list" => json!({}),
                _ => json!({"task_id": "1"}),
            };
            input["action"] = json!(action);
            let forbidden = match action {
                "create" => "owner",
                "get" | "list" => "status",
                _ => "after",
            };
            input[forbidden] = json!("1");
            assert!(
                serde_json::from_value::<TaskArgs>(input.clone()).is_err(),
                "{input}"
            );
        }
        for field in [
            "subject",
            "description",
            "active_form",
            "owner",
            "add_blocks",
            "add_blocked_by",
            "metadata",
        ] {
            let mut input = json!({"action": "update", "task_id": "1", "status": "deleted"});
            input[field] = match field {
                "owner" => Value::Null,
                "metadata" => json!({}),
                "add_blocks" | "add_blocked_by" => json!([]),
                _ => json!("changed"),
            };
            let args: TaskArgs = serde_json::from_value(input).unwrap();
            assert!(TaskRequest::try_from(args).is_err(), "{field}");
        }

        for input in [
            json!({"action": "update", "task_id": "1"}),
            json!({"action": "update", "task_id": "1", "owner": null}),
            json!({"action": "update", "task_id": "1", "owner": "security", "metadata": {"obsolete": null}}),
        ] {
            let args: TaskArgs = serde_json::from_value(input.clone()).unwrap();
            let request = TaskRequest::try_from(args).unwrap();
            let mut expected = input;
            if expected["owner"].is_string() {
                expected["owner"] = json!({"instance_id": "security"});
            }
            assert_eq!(permission_input(&request), expected);
        }
        for field in [
            "subject",
            "description",
            "active_form",
            "status",
            "add_blocks",
            "add_blocked_by",
            "metadata",
        ] {
            assert!(
                serde_json::from_value::<TaskArgs>(
                    json!({"action": "update", "task_id": "1", (field): null})
                )
                .is_err(),
                "{field}"
            );
        }
        for (field, value) in [
            ("taskId", json!("1")),
            ("list_id", json!("other")),
            ("caller", json!("lead")),
            ("path", json!("/tmp/other")),
            ("status", json!("running")),
            ("owner", json!({"instance_id": "worker"})),
        ] {
            assert!(
                serde_json::from_value::<TaskArgs>(
                    json!({"action": "update", "task_id": "1", (field): value})
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<TaskArgs>(json!({"action": "get", "task_id": 1})).is_err()
        );
        assert!(
            serde_json::from_value::<TaskArgs>(json!({"action": "list", "after": null})).is_err()
        );
        let args: TaskArgs = serde_json::from_value(json!({"action": "list"})).unwrap();
        assert!(matches!(
            TaskRequest::try_from(args).unwrap(),
            TaskRequest::List {
                limit: DEFAULT_PAGE_SIZE,
                ..
            }
        ));
        for invalid in ["0", "01", "../1", "18446744073709551616"] {
            for input in [
                json!({"action": "get", "task_id": invalid}),
                json!({"action": "list", "after": invalid}),
            ] {
                let args: TaskArgs = serde_json::from_value(input).unwrap();
                assert!(TaskRequest::try_from(args).is_err());
            }
        }
    }

    #[tokio::test]
    async fn task_permission_preserves_resolved_owner_and_patch_intent() {
        let owner = "agent-security";
        let request = TaskRequest::Update {
            task_id: "1".to_owned().try_into().unwrap(),
            changes: TaskChanges {
                owner: TaskAssignment::Assign(owner.into()),
                metadata: Some(Map::new()),
                add_blocks: Some(Vec::new()),
                ..Default::default()
            },
        };
        let permission = PermissionRuntime::new(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfig::default(),
                None,
            ),
        )))
        .interactive(true);
        let (sender, mut interactions) = mpsc::channel(1);
        let permission = permission.with_interaction_sender(Some(sender));
        let work = tokio::spawn(async move { authorize_task(&permission, &request).await });
        let interaction = interactions.recv().await.unwrap();
        assert_eq!(interaction.permission.tool, TaskTool::NAME);
        assert_eq!(
            serde_json::from_str::<Value>(&interaction.permission.input).unwrap(),
            json!({
                "action": "update", "task_id": "1", "owner": {"instance_id": owner}, "metadata": {}, "add_blocks": [],
            })
        );
        interaction
            .reply
            .send(crate::core::interaction::PermissionDecision::AllowOnce)
            .unwrap();
        let _applied = interaction.applied.await;
        work.await.unwrap().unwrap();
        let release = TaskRequest::try_from(
            serde_json::from_value::<TaskArgs>(
                json!({"action": "update", "task_id": "1", "owner": null}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            permission_input(&release),
            json!({"action": "update", "task_id": "1", "owner": null})
        );
        let deletion = TaskRequest::try_from(
            serde_json::from_value::<TaskArgs>(
                json!({"action": "update", "task_id": "1", "status": "deleted"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(deletion, TaskRequest::Delete(_)));
        assert_eq!(
            permission_input(&deletion),
            json!({"action": "update", "task_id": "1", "status": "deleted"})
        );
    }
}

pub(crate) struct TaskTool(pub(crate) TaskDispatcher);

impl Tool for TaskTool {
    const NAME: &'static str = TASK_TOOL_NAME;
    type Error = ToolExecutionError;
    type Args = TaskArgs;
    type Output = Value;

    fn description(&self) -> String {
        concat!(
            "Manage shared work records with action: create, get, list or update. Tasks do not start or stop Agents. ",
            "create returns a pending, unowned task; use task.id for later calls. get returns detail or task: null. list returns summaries; pass next as after for pagination. ",
            "update patches supplied fields only. Self-claim requires unowned, unfinished, unblocked work; ownership alone does not change status. ",
            "Set status in_progress when starting and successfully update to completed before reporting completion. Replies, turn endings and active_form never complete work. Completed tasks can be reopened. ",
            "add_blocks/add_blocked_by add same-list dependencies. Metadata merges top-level keys; null values remove keys. ",
            "To delete, supply only action update, task_id and status deleted; dependency references are pruned."
        ).into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "get", "list", "update"]},
                "task_id": {"type": "string", "pattern": "^[1-9][0-9]*$"},
                "after": {"type": "string", "pattern": "^[1-9][0-9]*$"},
                "limit": {"type": "integer", "minimum": 1, "maximum": DEFAULT_PAGE_SIZE, "default": DEFAULT_PAGE_SIZE},
                "subject": {"type": "string", "minLength": 1},
                "description": {"type": "string"},
                "active_form": {"type": "string", "description": "In-progress display wording only; use status, not this field, to mark completion"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "deleted"], "description": "Task state. Set in_progress when starting and completed when finished, before reporting completion. Omission preserves the current status."},
                "owner": {"type": ["string", "null"], "minLength": 1, "description": "Live instance ID or same-Team alias; null releases ownership"},
                "add_blocks": {"type": "array", "items": {"type": "string", "pattern": "^[1-9][0-9]*$"}},
                "add_blocked_by": {"type": "array", "items": {"type": "string", "pattern": "^[1-9][0-9]*$"}},
                "metadata": {"type": "object"}
            },
            "required": ["action"],
            "oneOf": [
                {"properties": {"action": {"const": "create"}}, "required": ["subject", "description"],
                    "propertyNames": {"enum": ["action", "subject", "description", "active_form", "metadata"]}},
                {"properties": {"action": {"const": "get"}}, "required": ["task_id"],
                    "propertyNames": {"enum": ["action", "task_id"]}},
                {"properties": {"action": {"const": "list"}},
                    "propertyNames": {"enum": ["action", "after", "limit"]}},
                {"properties": {"action": {"const": "update"}}, "required": ["task_id"],
                    "propertyNames": {"enum": ["action", "task_id", "subject", "description", "active_form", "status", "owner", "add_blocks", "add_blocked_by", "metadata"]}}
            ],
            "allOf": [{"if": {"properties": {"status": {"const": "deleted"}}, "required": ["status"]},
                "then": {"maxProperties": 3}}],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _: &mut rig::tool::ToolContext,
        args: Self::Args,
    ) -> Result<Value, Self::Error> {
        self.0.call(args.try_into()?).await
    }
}
