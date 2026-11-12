use anyhow::{Context, ensure};
use phi_sdk::agent_instance::{
    AgentInstance, AgentInstanceId, AgentInstanceStatus, AgentInstanceView,
};
use phi_sdk::agent_manager::{AgentAccess, AgentMemberSnapshot};
use phi_sdk::interaction::{InteractionEnvelope, InteractionRequest, PermissionInteraction};
use phi_sdk::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind, UserContentBlock};
use serde::Serialize;
use serde_json::Value;

use super::{GatewayState, RemoteHost, RemoteSession, session};

const MAX_AGENTS: usize = 64;
const TEXT_LIMIT: usize = 1000;
const SESSION_LABEL_LIMIT: usize = 48;
const RECENT_LIMIT: usize = 6;
const CONVERSATION_BYTES: usize = 256 * 1024;
const MAX_PERMISSIONS: usize = 32;
const MAX_PERMISSION_BYTES: usize = 8 * 1024;

fn office<H>(state: &GatewayState<H>, id: &str) -> anyhow::Result<OfficeAccess> {
    let id = AgentInstanceId::new(id);
    Ok(OfficeAccess {
        root: state.manager.get(&id)?,
        access: state.manager.for_agent(&id)?,
    })
}

pub(super) async fn read_view<H: RemoteHost>(
    state: &GatewayState<H>,
    id: &str,
) -> anyhow::Result<Value> {
    let labels = session(state, id).await?;
    let view = match state.manager.get(&AgentInstanceId::new(id)) {
        Ok(_) => {
            let interactions = state.host.interactions(AgentInstanceId::new(id)).await?;
            office(state, id)?.observe(&labels, &interactions).await?
        }
        Err(phi_sdk::agent_manager::AgentManagerError::Unknown { .. }) => OfficeView {
            workspace_path: workspace_path(
                &labels.working_dir,
                &phi_sdk::workspace::expand_home_path("~"),
            ),
            session_name: labels.name,
            session_id: String::new(),
            agents: Vec::new(),
            conversation: Conversation::default(),
            permissions: Vec::new(),
        },
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::to_value(view)?)
}

pub(super) fn permission_fits_display(permission: &PermissionInteraction) -> bool {
    permission.input.len() <= MAX_PERMISSION_BYTES
        && permission
            .preview
            .as_ref()
            .is_none_or(|preview| preview.len() <= MAX_PERMISSION_BYTES)
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Conversation {
    messages: Vec<ConversationMessage>,
    live_text: String,
    truncated: bool,
}

#[derive(Serialize)]
struct ConversationMessage {
    id: String,
    role: &'static str,
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionQuestion {
    agent_id: String,
    anchor_id: String,
    can_allow: bool,
    request_id: String,
    tool: String,
    input: String,
    preview: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OfficeView {
    workspace_path: String,
    session_name: String,
    session_id: String,
    agents: Vec<OfficeAgent>,
    conversation: Conversation,
    permissions: Vec<PermissionQuestion>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OfficeAgent {
    id: String,
    name: String,
    role: String,
    model: String,
    status: &'static str,
    can_message: bool,
    activity: Vec<OfficeActivity>,
}

#[derive(Serialize)]
struct OfficeActivity {
    time: String,
    text: String,
}

pub(super) fn clipped(text: &str) -> String {
    let mut characters = text.chars();
    let mut result: String = characters.by_ref().take(TEXT_LIMIT).collect();
    if characters.next().is_some() {
        result.push('…');
    }
    result
}

fn bounded_text(text: &str, budget: usize) -> String {
    let mut end = text.len().min(budget);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn project_conversation(view: &AgentInstanceView) -> Conversation {
    let live = view
        .snapshot
        .live_run
        .as_ref()
        .map(|run| run.assistant_text.as_str())
        .unwrap_or_default();
    let live_text = bounded_text(live, CONVERSATION_BYTES / 2);
    let mut result = Conversation {
        truncated: live_text.len() < live.len(),
        live_text,
        ..Default::default()
    };
    let mut budget = CONVERSATION_BYTES - result.live_text.len();
    for entry in view.transcript.current_branch().into_iter().rev() {
        let (role, text) = match &entry.kind {
            TranscriptEntryKind::Failure { failure, .. } => ("error", failure.message.clone()),
            TranscriptEntryKind::Message { message } => match message {
                AgentMessage::User { content } => (
                    "user",
                    content
                        .iter()
                        .filter_map(|block| match block {
                            UserContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                AgentMessage::Assistant { content, .. } => (
                    "assistant",
                    content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                AgentMessage::ToolResult {
                    tool_name,
                    is_error,
                    ..
                } => (
                    "tool",
                    format!(
                        "{} · {}",
                        clipped(tool_name),
                        if *is_error { "failed" } else { "finished" }
                    ),
                ),
                _ => continue,
            },
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        if budget == 0 {
            result.truncated = true;
            break;
        }
        let visible = bounded_text(&text, budget);
        result.truncated |= visible.len() < text.len();
        budget -= visible.len();
        result.messages.push(ConversationMessage {
            id: entry.id.to_string(),
            role,
            text: visible,
        });
    }
    result.messages.reverse();
    result
}

fn project(view: AgentInstanceView, name: String, model: Option<&str>) -> OfficeAgent {
    let snapshot = view.snapshot;
    let mut activity = Vec::new();
    let mut recorded_model = None;
    for entry in view.transcript.current_branch().into_iter().rev() {
        let TranscriptEntryKind::Message { message } = &entry.kind else {
            continue;
        };
        let text = match message {
            AgentMessage::User { content } => content.iter().find_map(|block| match block {
                UserContentBlock::Text { text } => Some(format!("You: {}", clipped(text))),
                _ => None,
            }),
            AgentMessage::Agent { content } => content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(format!("Agent: {}", clipped(text))),
                _ => None,
            }),
            AgentMessage::Assistant {
                content,
                provider,
                model,
                ..
            } => {
                if recorded_model.is_none() {
                    recorded_model = Some(format!("{provider} / {model}"));
                }
                content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(clipped(text)),
                    _ => None,
                })
            }
            AgentMessage::ToolResult {
                tool_name,
                is_error,
                ..
            } => Some(format!(
                "Tool: {} · {}",
                clipped(tool_name),
                if *is_error { "failed" } else { "finished" }
            )),
            _ => None,
        };
        if let Some(text) = text {
            activity.push(OfficeActivity {
                time: entry.timestamp.to_string(),
                text,
            });
        }
        if activity.len() == RECENT_LIMIT {
            break;
        }
    }
    activity.reverse();
    if let Some(run) = &snapshot.live_run {
        if let Some(tool) = &run.tool {
            activity.push(OfficeActivity {
                time: "Now".into(),
                text: format!("Tool: {}", clipped(&tool.name)),
            });
        } else if !run.assistant_text.is_empty() {
            activity.push(OfficeActivity {
                time: "Now".into(),
                text: clipped(&run.assistant_text),
            });
        }
    }
    OfficeAgent {
        id: snapshot.id.to_string(),
        name: clipped(&name),
        role: snapshot
            .definition
            .as_deref()
            .map(clipped)
            .unwrap_or_else(|| "Primary agent".into()),
        model: model
            .map(clipped)
            .or(recorded_model)
            .unwrap_or_else(|| "Not observed".into()),
        status: match snapshot.status {
            AgentInstanceStatus::Idle => "idle",
            AgentInstanceStatus::Running => "working",
            AgentInstanceStatus::RequiresAction => "blocked",
            AgentInstanceStatus::Failed => "error",
            AgentInstanceStatus::Stopping => "stopping",
            AgentInstanceStatus::Stopped => "stopped",
        },
        can_message: !matches!(
            snapshot.status,
            AgentInstanceStatus::Stopping | AgentInstanceStatus::Stopped
        ),
        activity,
    }
}

fn workspace_path(path: &std::path::Path, home: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".into(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

struct OfficeAccess {
    root: AgentInstance,
    access: AgentAccess,
}

impl OfficeAccess {
    fn permission_path(
        &self,
        members: &[AgentMemberSnapshot],
        target: &str,
    ) -> anyhow::Result<Vec<String>> {
        let mut current = target;
        let mut path = Vec::new();
        while current != self.root.id().as_str() {
            ensure!(path.len() < MAX_AGENTS, "invalid agent lineage");
            let member = members
                .iter()
                .find(|member| member.instance.id.as_str() == current)
                .context("target is outside this office")?;
            path.push(current.to_owned());
            current = member
                .created_by
                .as_ref()
                .context("target is outside this office")?
                .as_str();
        }
        path.reverse();
        Ok(path)
    }

    async fn observe(
        &self,
        session: &RemoteSession,
        interactions: &[(AgentInstanceId, Vec<InteractionEnvelope>)],
    ) -> anyhow::Result<OfficeView> {
        let members = self.access.list()?;
        let children: Vec<_> = members
            .iter()
            .filter(|member| member.created_by.as_ref() == Some(self.root.id()))
            .cloned()
            .collect();
        ensure!(
            children.len() < MAX_AGENTS,
            "office exceeds the {MAX_AGENTS}-agent observation limit"
        );
        let root = self.access.view(self.root.id()).await?;
        let conversation = project_conversation(&root);
        let mut permissions = Vec::new();
        for (agent, pending) in interactions {
            for envelope in pending {
                let (tool, input, preview, can_allow) = match &envelope.request {
                    InteractionRequest::Permission(permission) => {
                        let can_allow = permission_fits_display(permission);
                        (
                            clipped(&permission.tool),
                            bounded_text(&permission.input, MAX_PERMISSION_BYTES),
                            permission
                                .preview
                                .as_deref()
                                .map(|preview| bounded_text(preview, MAX_PERMISSION_BYTES)),
                            can_allow,
                        )
                    }
                    InteractionRequest::McpElicitation(_) => (
                        "Unsupported interaction".into(),
                        "MCP elicitation is not supported in this browser. Deny to cancel it."
                            .into(),
                        None,
                        false,
                    ),
                };
                ensure!(
                    permissions.len() < MAX_PERMISSIONS,
                    "too many pending permissions"
                );
                permissions.push(PermissionQuestion {
                    anchor_id: self
                        .permission_path(&members, agent.as_str())?
                        .first()
                        .cloned()
                        .unwrap_or_else(|| self.root.id().to_string()),
                    agent_id: agent.to_string(),
                    request_id: envelope.request_id.clone(),
                    tool,
                    input,
                    preview,
                    can_allow,
                });
            }
        }
        let session_name = if session.name.trim().is_empty() {
            root.transcript
                .current_branch()
                .into_iter()
                .flat_map(|entry| match &entry.kind {
                    TranscriptEntryKind::Message {
                        message: AgentMessage::User { content },
                    } => content.as_slice(),
                    _ => &[],
                })
                .find_map(|block| match block {
                    UserContentBlock::Text { text } if !text.trim().is_empty() => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(SESSION_LABEL_LIMIT)
                .collect()
        } else {
            clipped(&session.name)
        };
        let workspace_path = workspace_path(
            &root.snapshot.working_dir,
            &phi_sdk::workspace::expand_home_path("~"),
        );
        let mut agents = vec![project(root, "Phi".into(), Some(&session.model))];
        for child in children {
            let name = child
                .alias
                .map(|alias| alias.as_str().to_owned())
                .or(child.instance.definition.clone())
                .unwrap_or_else(|| "Agent".into());
            agents.push(project(
                self.access.view(&child.instance.id).await?,
                name,
                None,
            ));
        }
        for child in self.root.subagent_snapshots().await? {
            let creation = child.status.creation;
            if agents
                .iter()
                .any(|agent| agent.id == creation.instance_id().as_str())
            {
                continue;
            }
            ensure!(
                agents.len() < MAX_AGENTS,
                "office exceeds the {MAX_AGENTS}-agent observation limit"
            );
            let status = match child.status.outcome {
                Some(Ok(_)) => "stopped",
                Some(Err(_)) => "error",
                None => "unavailable",
            };
            agents.push(OfficeAgent {
                id: creation.instance_id().to_string(),
                name: creation
                    .alias()
                    .map(|alias| alias.as_str().to_owned())
                    .unwrap_or_else(|| clipped(creation.definition())),
                role: clipped(creation.definition()),
                model: "Not observed".into(),
                status,
                can_message: false,
                activity: Vec::new(),
            });
        }
        Ok(OfficeView {
            workspace_path,
            session_name,
            session_id: self.root.id().to_string(),
            agents,
            conversation,
            permissions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{labels, sdk_office};
    use super::*;
    use phi_sdk::agent_instance::AgentInstanceSnapshot;
    use serde_json::json;

    #[test]
    fn workspace_label_shortens_only_paths_inside_home() {
        let home = std::path::Path::new("/home/phi");
        for (path, expected) in [
            ("/home/phi", "~"),
            ("/home/phi/project", "~/project"),
            ("/home/philosophy/project", "/home/philosophy/project"),
            ("/tmp/project", "/tmp/project"),
        ] {
            assert_eq!(workspace_path(std::path::Path::new(path), home), expected);
        }
    }

    #[tokio::test]
    async fn public_api_scope_excludes_foreign_agents() {
        let root = std::env::temp_dir().join(format!("phi-remote-scope-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (manager, instance, phi) = sdk_office(&root, "http://127.0.0.1:1/v1").await;
        let office = OfficeAccess {
            access: manager.for_agent(instance.id()).unwrap(),
            root: instance,
        };
        let other = manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let child = office
            .access
            .register_child(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .delegated_child()
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let grandchild = manager
            .for_agent(&child.instance.id)
            .unwrap()
            .register_child(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .delegated_child()
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let members = office.access.list().unwrap();
        assert_eq!(members.len(), 3);
        assert!(
            members
                .iter()
                .all(|member| member.instance.id != *other.id())
        );
        assert_eq!(
            office
                .permission_path(&members, grandchild.instance.id.as_str())
                .unwrap(),
            vec![
                child.instance.id.to_string(),
                grandchild.instance.id.to_string()
            ]
        );
        assert!(
            office
                .permission_path(&members, other.id().as_str())
                .is_err()
        );
        let view = office.observe(&labels(), &[]).await.unwrap();
        assert_eq!(view.agents.len(), 2);
        assert_eq!(view.session_name, "Office session");
        assert_eq!(view.session_id, office.root.id().as_str());
        assert!(
            !view
                .agents
                .iter()
                .any(|agent| agent.id == other.id().as_str())
        );
        assert!(
            view.agents
                .iter()
                .any(|agent| agent.id == child.instance.id.as_str())
        );
        assert!(
            office
                .access
                .send_message(other.id(), "Do not deliver")
                .await
                .is_err()
        );
        assert_eq!(office.root.snapshot().status, AgentInstanceStatus::Idle);
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn projection_excludes_private_execution_details() {
        for (state, expected, can_message) in [
            (AgentInstanceStatus::Idle, "idle", true),
            (AgentInstanceStatus::Running, "working", true),
            (AgentInstanceStatus::RequiresAction, "blocked", true),
            (AgentInstanceStatus::Failed, "error", true),
            (AgentInstanceStatus::Stopping, "stopping", false),
            (AgentInstanceStatus::Stopped, "stopped", false),
        ] {
            let mut transcript = phi_sdk::transcript::Transcript::new();
            transcript.append_user_message("Build the office");
            for (id, is_error) in [("completed", false), ("failed", true)] {
                transcript.append_assistant_message(
                    None,
                    None,
                    vec![ContentBlock::ToolCall {
                        id: id.into(),
                        name: "Read".into(),
                        args: json!({"path":"private tool argument"}),
                        provider_call_id: None,
                        provider_item_id: None,
                    }],
                    "local".into(),
                    phi_sdk::providers::ProviderApi::OpenAiCompletions,
                    "test".into(),
                    Default::default(),
                    "tool_use".into(),
                );
                transcript.append_tool_result(id, "private tool output", is_error, None);
            }
            transcript.append_entry(TranscriptEntryKind::Failure {
                turn_id: Some("failed-turn".into()),
                failure: phi_sdk::PhiFailure::new(
                    phi_sdk::PhiFailureKind::ProviderUnavailable,
                    "connection lost",
                    None,
                ),
            });
            let snapshot = AgentInstanceSnapshot {
                id: transcript.id.as_str().into(),
                definition: None,
                working_dir: "/private/workspace".into(),
                status: state,
                active_turn_id: None,
                live_run: Some(phi_sdk::agent_instance::LiveRunSnapshot {
                    assistant_text: "Visible response".into(),
                    reasoning_text: "private reasoning".into(),
                    tool: Some(phi_sdk::agent_instance::LiveToolSnapshot {
                        id: "tool".into(),
                        name: "Read".into(),
                        args: json!({"secret":"private tool argument"}),
                        display: None,
                    }),
                }),
                progress: None,
                bash_executions: Vec::new(),
                pending_interactions: Vec::new(),
                latest_error: None,
                revision: 0,
            };
            let source = AgentInstanceView {
                pending_submission_ids: Vec::new(),
                transcript,
                snapshot,
            };
            let conversation = project_conversation(&source);
            assert_eq!(conversation.live_text, "Visible response");
            assert_eq!(
                conversation
                    .messages
                    .iter()
                    .map(|message| (message.role, message.text.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("user", "Build the office"),
                    ("tool", "Read · finished"),
                    ("tool", "Read · failed"),
                    ("error", "connection lost")
                ]
            );
            let serialized = serde_json::to_string(&conversation).unwrap();
            for excluded in [
                "private reasoning",
                "private tool argument",
                "private tool output",
            ] {
                assert!(!serialized.contains(excluded));
            }
            let view = project(source, "Phi".into(), Some("local / test"));
            assert_eq!(view.status, expected);
            assert_eq!(view.can_message, can_message);
            let serialized = serde_json::to_string(&view).unwrap();
            for excluded in [
                "private reasoning",
                "private tool argument",
                "/private/workspace",
            ] {
                assert!(!serialized.contains(excluded));
            }
        }
        assert_eq!(
            clipped(&"界".repeat(TEXT_LIMIT + 1)).chars().count(),
            TEXT_LIMIT + 1
        );
    }
}
