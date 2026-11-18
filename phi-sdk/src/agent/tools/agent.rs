use crate::harness::manager::preparation::DelegationMode;
use rig::tool::{Tool, ToolExecutionError};
use tokio::sync::{mpsc, oneshot};

pub enum AgentArgs {
    Create(crate::harness::manager::preparation::AgentSpawnRequest),
    Manage(AgentManagementArgs),
}

impl<'de> serde::Deserialize<'de> for AgentArgs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde_json::Value;

        // Providers pass parsed arguments; raw JSON keys are no longer available here.
        let Value::Object(mut fields) = Value::deserialize(deserializer)? else {
            return Err(serde::de::Error::custom(
                "expected an Agent argument object",
            ));
        };
        if fields.get("action").and_then(Value::as_str) == Some("create") {
            fields.remove("action");
            serde_json::from_value(Value::Object(fields)).map(Self::Create)
        } else {
            serde_json::from_value(Value::Object(fields)).map(Self::Manage)
        }
        .map_err(serde::de::Error::custom)
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentManagementArgs {
    #[serde(rename = "list")]
    ListAgents {},
    #[serde(rename = "message")]
    SendMessage {
        recipient: crate::harness::instance::AgentInstanceId,
        content: String,
    },
}

#[derive(Debug)]
pub(crate) enum AgentToolOutput {
    Created(crate::harness::manager::preparation::AgentCreation),
    Members(Vec<crate::harness::manager::AgentMemberSnapshot>),
    MessageSent(crate::harness::instance::AgentInstanceId),
    Response(String),
}

impl AgentToolOutput {
    pub(crate) fn into_model_output(self) -> String {
        match self {
            Self::Created(creation) => serde_json::json!({
                "status": "async_launched",
                "agentId": creation.instance_id().as_str(),
                "agent": creation.definition(),
                "nextStep": "The initial result will arrive automatically as a follow-up. Continue independent work, or end your current turn to receive it. Do not poll or sleep. Use action=message with agentId as recipient for follow-up work or coordination, or use the name of your ordinary child.",
            })
            .to_string(),
            Self::Members(members) => serde_json::json!(
                members
                    .into_iter()
                    .map(crate::harness::manager::AgentDiscovery::from)
                    .collect::<Vec<_>>()
            )
            .to_string(),
            Self::MessageSent(recipient) => serde_json::json!({
                "state": "sent",
                "recipient": recipient,
            })
            .to_string(),
            Self::Response(response) if response.is_empty() => "(no output)".into(),
            Self::Response(response) => response,
        }
    }
}

pub(crate) struct AgentToolRequest {
    pub(crate) args: AgentArgs,
    pub(crate) scope: crate::core::event::CoreEventScope,
    pub(crate) reply: oneshot::Sender<Result<AgentToolOutput, String>>,
}

pub(crate) type AgentToolSender = mpsc::UnboundedSender<AgentToolRequest>;

pub struct AgentTool {
    sender: AgentToolSender,
    agents: Vec<(String, String)>,
    permission: Option<crate::permission::runtime::PermissionRuntime>,
}

impl AgentTool {
    pub fn new(
        sender: AgentToolSender,
        agents: &crate::agent::custom::AgentCatalog,
        permission: Option<crate::permission::runtime::PermissionRuntime>,
    ) -> Self {
        Self {
            sender,
            agents: agents
                .all()
                .iter()
                .map(|agent| (agent.name().to_string(), agent.description().to_string()))
                .collect(),
            permission,
        }
    }
}

impl AgentArgs {
    fn permission_action(&self) -> &'static str {
        match self {
            Self::Create(_) => "create",
            Self::Manage(action) => match action {
                AgentManagementArgs::ListAgents {} => "list_agents",
                AgentManagementArgs::SendMessage { .. } => "send_message",
            },
        }
    }
}

impl Tool for AgentTool {
    const NAME: &'static str = "Agent";
    type Error = ToolExecutionError;
    type Args = AgentArgs;
    type Output = String;

    fn description(&self) -> String {
        let guidance = concat!(
            "Create Agents, discover members, and send messages.\n\n",
            "- Create: action=create with role and a concrete prompt; optionally alias, working_dir and mode. Foreground waits for the result; background returns a launch receipt. With Teams enabled, the lead sets alias for a teammate; omit it for an ordinary subagent.\n",
            "- Discover: action=list lists authorized members.\n",
            "- Communicate: action=message with recipient and content sends actionable follow-up work, questions, blockers or findings, not routine status updates. It may start or extend the recipient's execution while preserving history and configuration. Prefer the returned agentId as recipient. Sending returns acceptance, not a reply or work result.\n\n",
            "Background results and teammate idle reports arrive automatically in later turns. Ordinary coordination may join an active turn at safe points; otherwise it starts a later turn. Continue independent work, or end your current turn to wait. Do not poll list, use Bash sleep, or send duplicate result/status messages. A user-cancelled Agent refuses messages until a direct user or host request resumes it; do not create a replacement to bypass cancellation.\n\n",
            "Tasks are optional and separate: creating an Agent does not create or claim a Task. For recorded work, include the shared-list Task ID and instructions in prompt; assign ownership through Task tools. Messages and Team membership grant no additional permissions."
        );
        if self.agents.is_empty() {
            return guidance.into();
        }
        let definitions = self
            .agents
            .iter()
            .map(|(name, description)| format!("- {name}: {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{guidance}\n\nAvailable role definitions:\n{definitions}")
    }

    fn parameters(&self) -> serde_json::Value {
        let mut role = serde_json::json!({ "type": "string", "description": "Registered definition name for a new instance" });
        if !self.agents.is_empty() {
            role["enum"] =
                serde_json::json!(self.agents.iter().map(|(name, _)| name).collect::<Vec<_>>());
        }
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["create", "list", "message"] },
                "role": role,
                "alias": { "type": "string", "minLength": 1, "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]*$", "description": "New instance name, unique within its Team; creation only" },
                "prompt": { "type": "string", "description": "Work instructions and context; optionally include a shared-list Task ID" },
                "working_dir": { "type": "string", "minLength": 1, "description": "Existing directory relative to the immediate parent; creation only" },
                "mode": { "type": "string", "enum": ["foreground", "background"], "description": "Creation only. Default: background for master callers, foreground for delegated callers. Background policy and teammates override foreground." },
                "recipient": { "type": "string", "minLength": 1, "description": "Authorized instance ID, same-Team alias, or named ordinary child of this caller" },
                "content": { "type": "string", "description": "Actionable follow-up work or coordination; may start or extend execution. Not routine status updates, user consent or a permission grant" }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: AgentArgs,
    ) -> Result<String, ToolExecutionError> {
        self.permission
            .as_ref()
            .ok_or_else(|| {
                delegation_error("Agent invocation requires an explicit permission policy")
            })?
            .authorize_tool("agent", args.permission_action(), None, None)
            .await
            .map_err(|error| delegation_error(error.to_string()))?;
        let (reply, result) = oneshot::channel();
        let mut scope = crate::core::interaction::active_interaction_scope();
        scope.parent_tool_call_id = crate::core::interaction::active_tool_call_id().map(Into::into);
        self.sender
            .send(AgentToolRequest { args, scope, reply })
            .map_err(|_| delegation_error("Agent tool dispatcher is unavailable"))?;
        let output = result
            .await
            .map_err(|_| delegation_error("Agent tool result channel closed"))?
            .map_err(delegation_error)?;
        if matches!(&output, AgentToolOutput::Created(creation)
            if creation.mode() == DelegationMode::Background)
        {
            context.insert_result(crate::tools::ToolTurnEnd);
        }
        Ok(output.into_model_output())
    }
}

fn delegation_error(message: impl Into<String>) -> ToolExecutionError {
    let message = message.into();
    ToolExecutionError::other(&message)
        .with_model_feedback(message)
        .with_retryable(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_catalog() -> crate::agent::custom::AgentCatalog {
        crate::agent::custom::AgentCatalog::new(vec![crate::agent::custom::AgentDefinition::new(
            "probe",
            "Probe behavior.",
            "Inspect.",
        )])
    }

    #[tokio::test]
    async fn only_background_launch_ends_turn() {
        use crate::harness::manager::preparation::{
            AgentCreation, AgentSpawnRequest, DelegationMode,
        };

        let creation = AgentCreation::new(
            crate::harness::instance::AgentInstanceId::new("parent"),
            &AgentSpawnRequest::new("probe", "work").mode(DelegationMode::Background),
            "/tmp".into(),
            None,
        );
        let (sender, mut requests) = mpsc::unbounded_channel();
        let config = crate::permission::PermissionConfig::try_from_value(
            serde_json::json!({"mode":"allow"}),
        )
        .unwrap();
        let checker = crate::permission::checker::PermissionChecker::new(&config, None);
        let permission = crate::permission::runtime::PermissionRuntime::new(std::sync::Arc::new(
            std::sync::Mutex::new(checker),
        ));
        let tool = AgentTool::new(sender, &probe_catalog(), Some(permission));
        for (input, output, terminates) in [
            (
                serde_json::json!({"action":"create", "role":"probe", "prompt":"work", "mode":"foreground"}),
                Ok(AgentToolOutput::Created(creation)),
                true,
            ),
            (
                serde_json::json!({"action":"create", "role":"probe", "prompt":"work", "mode":"foreground"}),
                Ok(AgentToolOutput::Response("result".into())),
                false,
            ),
            (
                serde_json::json!({"action":"list"}),
                Ok(AgentToolOutput::Members(Vec::new())),
                false,
            ),
            (
                serde_json::json!({"action":"message", "recipient":"peer", "content":"work"}),
                Ok(AgentToolOutput::MessageSent(
                    crate::harness::instance::AgentInstanceId::new("peer"),
                )),
                false,
            ),
            (
                serde_json::json!({"action":"create", "role":"probe", "prompt":"work", "mode":"background"}),
                Err("creation failed".into()),
                false,
            ),
        ] {
            let mut context = rig::tool::ToolContext::new();
            let failed = output.is_err();
            let (result, ()) = tokio::join!(
                tool.call(&mut context, serde_json::from_value(input).unwrap()),
                async {
                    requests.recv().await.unwrap().reply.send(output).unwrap();
                },
            );
            assert_eq!(result.is_err(), failed);
            assert_eq!(
                context.result::<crate::tools::ToolTurnEnd>().is_some(),
                terminates
            );
        }
    }

    #[tokio::test]
    async fn semantic_permission_denial_prevents_manager_dispatch() {
        let (sender, mut requests) = mpsc::unbounded_channel();
        for (action, input) in [
            (
                "create",
                serde_json::json!({"action":"create", "role":"probe", "prompt":"work"}),
            ),
            ("list_agents", serde_json::json!({"action":"list"})),
            (
                "send_message",
                serde_json::json!({"action":"message", "recipient":"peer", "content":"message"}),
            ),
        ] {
            let config = crate::permission::PermissionConfig::try_from_value(
                serde_json::json!({"mode":"allow", "tools":{"agent":{(action):"deny"}}}),
            )
            .unwrap();
            let checker = crate::permission::checker::PermissionChecker::new(&config, None);
            let permission = crate::permission::runtime::PermissionRuntime::new(
                std::sync::Arc::new(std::sync::Mutex::new(checker)),
            );
            let tool = AgentTool::new(sender.clone(), &probe_catalog(), Some(permission));
            let args: AgentArgs = serde_json::from_value(input).unwrap();
            assert_eq!(args.permission_action(), action);
            let scope = tool.permission.as_ref().unwrap().outer_turn_policy(
                crate::core::turn::TurnToolPolicy {
                    allowances: vec![("Agent".into(), action.into())],
                    removals: Vec::new(),
                },
            );
            let result = scope
                .run(tool.call(&mut rig::tool::ToolContext::new(), args))
                .await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Permission denied")
            );
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn rejects_invalid_operations() {
        use serde_json::{Value, json};

        for input in [
            json!({"action":"create", "role":"probe", "prompt":"work", "alias":null, "working_dir":null, "mode":null}),
            json!({"action":"list"}),
            json!({"action":"message", "recipient":"peer", "content":"work"}),
        ] {
            assert!(serde_json::from_value::<AgentArgs>(input.clone()).is_ok());
            for field in [
                "action",
                "role",
                "prompt",
                "alias",
                "working_dir",
                "mode",
                "recipient",
                "content",
                "caller",
                "executor",
                "message_id",
                "timeout_ms",
            ] {
                let mut invalid = input.clone();
                if input.get(field).is_none() {
                    invalid[field] = Value::Null;
                } else if matches!(
                    field,
                    "action" | "role" | "prompt" | "recipient" | "content"
                ) {
                    invalid.as_object_mut().unwrap().remove(field);
                } else {
                    continue;
                }
                assert!(
                    serde_json::from_value::<AgentArgs>(invalid.clone()).is_err(),
                    "{invalid}"
                );
            }
        }
        for invalid in [
            json!({"action":"unsupported"}),
            json!({"action":null}),
            json!([{"action":"list"}]),
        ] {
            assert!(
                serde_json::from_value::<AgentArgs>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn creation_fields_and_defaults() {
        use crate::harness::manager::preparation::DelegationMode;
        use serde_json::json;

        let tool = AgentTool::new(
            mpsc::unbounded_channel().0,
            &crate::agent::custom::AgentCatalog::new(vec![
                crate::agent::custom::AgentDefinition::new(
                    "code-reviewer",
                    "Review code for correctness and security.",
                    "Inspect code.",
                ),
            ]),
            None,
        );
        let schema = tool.parameters();
        assert_eq!(
            schema["properties"]["role"]["enum"],
            serde_json::json!(["code-reviewer"])
        );
        let guidance = tool.description();
        assert!(guidance.contains("- code-reviewer: Review code for correctness and security."));
        assert!(!guidance.contains("Inspect code."));

        let input = json!({
            "action": "create", "role": "code-reviewer", "prompt": "Review authentication.",
            "alias": null, "working_dir": null, "mode": null
        });
        let AgentArgs::Create(request) = serde_json::from_value(input.clone()).unwrap() else {
            panic!("expected creation");
        };
        assert_eq!(request.role, "code-reviewer");
        assert_eq!(request.prompt, "Review authentication.");
        assert!(request.alias.is_none() && request.working_dir.is_none() && request.mode.is_none());

        let mut named = input;
        named["alias"] = json!("security");
        named["working_dir"] = json!("worktree");
        named["mode"] = json!("foreground");
        let AgentArgs::Create(request) = serde_json::from_value(named.clone()).unwrap() else {
            panic!("expected creation");
        };
        assert_eq!(request.alias.unwrap().as_str(), "security");
        assert_eq!(
            request.working_dir.unwrap(),
            std::path::Path::new("worktree")
        );
        assert_eq!(request.mode, Some(DelegationMode::Foreground));
        named["alias"] = json!("../other");
        assert!(serde_json::from_value::<AgentArgs>(named).is_err());
    }
}
