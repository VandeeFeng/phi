//! Selected history and execution data for Gateway clients, not an execution checkpoint.

use phi_sdk::agent_instance::{
    AgentInstanceId, AgentInstanceSnapshot, AgentInstanceStatus, AgentInstanceView, LiveRunSnapshot,
};
use phi_sdk::interaction::{InteractionEnvelope, InteractionRequest};
use phi_sdk::tools::{NestedToolCall, ToolResultDetails};
use phi_sdk::transcript::{
    AgentMessage, ContentBlock, ContextScope, ContextSource, ImageSource, SharedText, Transcript,
    TranscriptEntry, TranscriptEntryKind, Usage, UserContentBlock,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{RemoteInteraction, RemoteSession, RemoteSnapshot};

const SNAPSHOT_VERSION: u32 = 2;
const NESTED_TOOL_EVENT: &str = "nested_tool_call";
const OMITTED_EVENT: &str = "gateway_omitted";

pub(crate) fn decode_live_content(value: Value) -> anyhow::Result<Vec<ContentBlock>> {
    Ok(serde_json::from_value::<Vec<Content>>(value)?
        .into_iter()
        .map(Into::into)
        .collect())
}

pub(crate) fn live_content(content: Vec<ContentBlock>) -> impl Serialize {
    content.into_iter().map(Content::from).collect::<Vec<_>>()
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot<Application> {
    version: u32,
    session: RemoteSession,
    history: History,
    execution: Execution,
    pending_submission_ids: Vec<String>,
    interactions: Vec<Interaction>,
    application: Application,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeUpdate<Application> {
    version: u32,
    session: RemoteSession,
    history: History,
    execution: Execution,
    application: Application,
}

impl<Application> From<crate::RemoteRuntimeUpdate<Application>> for RuntimeUpdate<Application> {
    fn from(update: crate::RemoteRuntimeUpdate<Application>) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            session: update.session,
            history: update.transcript.into(),
            execution: update.snapshot.into(),
            application: update.application,
        }
    }
}

impl<Application> RuntimeUpdate<Application> {
    pub(crate) fn into_internal(self) -> anyhow::Result<crate::RemoteRuntimeUpdate<Application>> {
        anyhow::ensure!(
            self.version == SNAPSHOT_VERSION,
            "unsupported Gateway runtime update version"
        );
        let snapshot = self.execution.into_snapshot(&self.session);
        Ok(crate::RemoteRuntimeUpdate {
            transcript: self.history.into_transcript(&self.session.id),
            session: self.session,
            snapshot,
            application: self.application,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Execution {
    status: AgentInstanceStatus,
    active_turn_id: Option<String>,
    live: Option<LiveRunSnapshot>,
    error: Option<phi_sdk::PhiFailure>,
    revision: u64,
}

impl From<AgentInstanceSnapshot> for Execution {
    fn from(state: AgentInstanceSnapshot) -> Self {
        Self {
            status: state.status,
            active_turn_id: state.active_turn_id,
            live: state.live_run,
            error: state.latest_error,
            revision: state.revision,
        }
    }
}

impl Execution {
    fn into_snapshot(self, session: &RemoteSession) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: AgentInstanceId::new(&session.id),
            definition: None,
            working_dir: session.working_dir.clone(),
            status: self.status,
            active_turn_id: self.active_turn_id,
            live_run: self.live,
            progress: None,
            bash_executions: Vec::new(),
            pending_interactions: Vec::new(),
            latest_error: self.error,
            revision: self.revision,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interaction {
    agent_id: String,
    request_id: String,
    emitted_at: String,
    request: InteractionRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    entries: Vec<Entry>,
    leaf_id: Option<String>,
    created_at: String,
    updated_at: String,
    total_tokens: u64,
    total_cost: f64,
    total_cache_read: u64,
    total_cache_write: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    id: String,
    parent_id: Option<String>,
    head: Option<String>,
    timestamp: String,
    content: EntryContent,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EntryContent {
    Message {
        message: Message,
    },
    NestedToolCall {
        call: NestedToolCall,
    },
    Failure {
        turn_id: Option<String>,
        failure: phi_sdk::PhiFailure,
    },
    Compaction {
        summary: String,
        tokens_before: u64,
    },
    Reset {
        handoff: Option<SharedText>,
    },
    Omitted,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum Message {
    User {
        content: Vec<UserContentBlock>,
    },
    Agent {
        content: Vec<Content>,
    },
    Context {
        source: ContextSource,
        scope: ContextScope,
        content: Vec<Content>,
    },
    Assistant {
        content: Vec<Content>,
        provider: String,
        model: String,
        usage: Usage,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<Content>,
        is_error: bool,
        details: Option<ToolResultDetails>,
    },
    System {
        content: SharedText,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Content {
    Text {
        text: SharedText,
    },
    Image {
        source: ImageSource,
    },
    Thinking {
        text: SharedText,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
}

impl<Application> From<RemoteSnapshot<Application>> for Snapshot<Application> {
    fn from(snapshot: RemoteSnapshot<Application>) -> Self {
        let state = snapshot.view.snapshot;
        Self {
            version: SNAPSHOT_VERSION,
            session: snapshot.session,
            history: snapshot.view.transcript.into(),
            execution: state.into(),
            pending_submission_ids: snapshot.view.pending_submission_ids,
            interactions: snapshot
                .interactions
                .into_iter()
                .map(|interaction| Interaction {
                    agent_id: interaction.agent_id,
                    request_id: interaction.request.request_id,
                    emitted_at: interaction.request.emitted_at.to_rfc3339(),
                    request: interaction.request.request,
                })
                .collect(),
            application: snapshot.application,
        }
    }
}

impl<Application> Snapshot<Application> {
    pub(crate) fn into_internal(self) -> anyhow::Result<RemoteSnapshot<Application>> {
        anyhow::ensure!(
            self.version == SNAPSHOT_VERSION,
            "unsupported Gateway snapshot version"
        );
        let interactions: Vec<_> = self
            .interactions
            .into_iter()
            .map(|interaction| {
                let mut request = InteractionEnvelope {
                    request_id: interaction.request_id,
                    emitted_at: interaction.emitted_at.parse()?,
                    scope: Default::default(),
                    request: interaction.request,
                };
                request.scope.agent_id = Some(interaction.agent_id.as_str().into());
                request.scope.session_id = request.scope.agent_id.clone();
                Ok(RemoteInteraction {
                    agent_id: interaction.agent_id,
                    request,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        let mut state = self.execution.into_snapshot(&self.session);
        state.pending_interactions = interactions
            .iter()
            .filter(|interaction| interaction.agent_id == self.session.id)
            .map(|interaction| interaction.request.clone())
            .collect();
        Ok(RemoteSnapshot {
            session: self.session,
            view: AgentInstanceView {
                pending_submission_ids: self.pending_submission_ids,
                transcript: self.history.into_transcript(state.id.as_str()),
                snapshot: state,
            },
            interactions,
            application: self.application,
        })
    }
}

impl From<Transcript> for History {
    fn from(transcript: Transcript) -> Self {
        Self {
            entries: transcript
                .entries
                .into_iter()
                .map(|entry| Entry {
                    id: entry.id.to_string(),
                    parent_id: entry.parent_id.map(|id| id.to_string()),
                    head: entry.head.map(|id| id.to_string()),
                    timestamp: entry.timestamp.to_string(),
                    content: match entry.kind {
                        TranscriptEntryKind::Message { message } => EntryContent::Message {
                            message: message.into(),
                        },
                        TranscriptEntryKind::Event { name, data } if name == NESTED_TOOL_EVENT => {
                            serde_json::from_value(data)
                                .map(|call| EntryContent::NestedToolCall { call })
                                .unwrap_or(EntryContent::Omitted)
                        }
                        TranscriptEntryKind::Event { .. }
                        | TranscriptEntryKind::ContextEdit { .. } => EntryContent::Omitted,
                        TranscriptEntryKind::Failure { turn_id, failure } => {
                            EntryContent::Failure {
                                turn_id: turn_id.map(|id| id.to_string()),
                                failure,
                            }
                        }
                        TranscriptEntryKind::Compaction {
                            summary,
                            tokens_before,
                        } => EntryContent::Compaction {
                            summary: summary.to_string(),
                            tokens_before,
                        },
                        TranscriptEntryKind::Reset { handoff } => EntryContent::Reset { handoff },
                    },
                })
                .collect(),
            leaf_id: transcript.leaf_id.map(|id| id.to_string()),
            created_at: transcript.created_at.to_string(),
            updated_at: transcript.updated_at.to_string(),
            total_tokens: transcript.total_tokens,
            total_cost: transcript.total_cost,
            total_cache_read: transcript.total_cache_read,
            total_cache_write: transcript.total_cache_write,
        }
    }
}

impl History {
    fn into_transcript(self, id: &str) -> Transcript {
        Transcript {
            id: id.into(),
            entries: self
                .entries
                .into_iter()
                .map(|entry| TranscriptEntry {
                    id: entry.id.into(),
                    parent_id: entry.parent_id.map(Into::into),
                    head: entry.head.map(Into::into),
                    timestamp: entry.timestamp.into(),
                    kind: match entry.content {
                        EntryContent::Message { message } => TranscriptEntryKind::Message {
                            message: message.into(),
                        },
                        EntryContent::NestedToolCall { call } => TranscriptEntryKind::Event {
                            name: NESTED_TOOL_EVENT.into(),
                            data: serde_json::to_value(call)
                                .expect("nested tool data is serializable"),
                        },
                        EntryContent::Compaction {
                            summary,
                            tokens_before,
                        } => TranscriptEntryKind::Compaction {
                            summary: summary.into(),
                            tokens_before,
                        },
                        EntryContent::Reset { handoff } => TranscriptEntryKind::Reset { handoff },
                        EntryContent::Failure { turn_id, failure } => {
                            TranscriptEntryKind::Failure {
                                turn_id: turn_id.map(Into::into),
                                failure,
                            }
                        }
                        // Keep ancestry intact without transferring extension-owned payloads.
                        EntryContent::Omitted => TranscriptEntryKind::Event {
                            name: OMITTED_EVENT.into(),
                            data: Value::Null,
                        },
                    },
                })
                .collect(),
            switch_snapshots: Vec::new(),
            leaf_id: self.leaf_id.map(Into::into),
            created_at: self.created_at.into(),
            updated_at: self.updated_at.into(),
            total_tokens: self.total_tokens,
            total_cost: self.total_cost,
            total_cache_read: self.total_cache_read,
            total_cache_write: self.total_cache_write,
        }
    }
}

impl From<AgentMessage> for Message {
    fn from(message: AgentMessage) -> Self {
        match message {
            AgentMessage::User { content } => Self::User { content },
            AgentMessage::Agent { content } => Self::Agent {
                content: content.into_iter().map(Into::into).collect(),
            },
            AgentMessage::Context {
                source,
                scope,
                content,
            } => Self::Context {
                source,
                scope,
                content: content.into_iter().map(Into::into).collect(),
            },
            AgentMessage::Assistant {
                content,
                provider,
                model,
                usage,
                ..
            } => Self::Assistant {
                content: content.into_iter().map(Into::into).collect(),
                provider: provider.to_string(),
                model: model.to_string(),
                usage,
            },
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
                details,
                ..
            } => Self::ToolResult {
                tool_call_id: tool_call_id.to_string(),
                tool_name: tool_name.to_string(),
                content: content.into_iter().map(Into::into).collect(),
                is_error,
                details,
            },
            AgentMessage::System { content } => Self::System { content },
        }
    }
}

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        match message {
            Message::User { content } => Self::User { content },
            Message::Agent { content } => Self::Agent {
                content: content.into_iter().map(Into::into).collect(),
            },
            Message::Context {
                source,
                scope,
                content,
            } => Self::Context {
                source,
                scope,
                content: content.into_iter().map(Into::into).collect(),
            },
            Message::Assistant {
                content,
                provider,
                model,
                usage,
            } => Self::Assistant {
                id: None,
                turn_id: None,
                switch: None,
                stop_reason: "".into(),
                content: content.into_iter().map(Into::into).collect(),
                provider: provider.into(),
                api: None,
                model: model.into(),
                usage,
            },
            Message::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
                details,
            } => Self::ToolResult {
                provider_call_id: None,
                provider_item_id: None,
                tool_call_id: tool_call_id.into(),
                tool_name: tool_name.into(),
                content: content.into_iter().map(Into::into).collect(),
                is_error,
                details,
            },
            Message::System { content } => Self::System { content },
        }
    }
}

impl From<ContentBlock> for Content {
    fn from(content: ContentBlock) -> Self {
        match content {
            ContentBlock::Text { text } => Self::Text { text },
            ContentBlock::Image { source } => Self::Image { source },
            ContentBlock::Thinking { text, .. } => Self::Thinking { text },
            ContentBlock::ToolCall { id, name, args, .. } => Self::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                args,
            },
        }
    }
}

impl From<Content> for ContentBlock {
    fn from(content: Content) -> Self {
        match content {
            Content::Text { text } => Self::Text { text },
            Content::Image { source } => Self::Image { source },
            Content::Thinking { text } => Self::Thinking {
                text,
                metadata: None,
            },
            Content::ToolCall { id, name, args } => Self::ToolCall {
                id: id.into(),
                name: name.into(),
                args,
                provider_call_id: None,
                provider_item_id: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::transcript::{
        ReasoningMetadata, ReasoningMetadataContent, SwitchSnapshot, SwitchSnapshotRecord,
    };
    use proptest::prelude::*;
    use serde_json::json;

    fn snapshot(transcript: Transcript) -> RemoteSnapshot {
        let id = transcript.id.to_string();
        RemoteSnapshot {
            session: RemoteSession {
                id: id.clone(),
                name: "test".into(),
                provider: "local".into(),
                model: "test".into(),
                working_dir: "/tmp".into(),
            },
            view: AgentInstanceView {
                transcript,
                pending_submission_ids: vec!["queued-input".into()],
                snapshot: AgentInstanceSnapshot {
                    id: AgentInstanceId::new(id),
                    definition: None,
                    working_dir: "/tmp".into(),
                    status: AgentInstanceStatus::Running,
                    active_turn_id: Some("turn".into()),
                    live_run: Some(LiveRunSnapshot {
                        assistant_text: "live text".into(),
                        reasoning_text: "live reasoning".into(),
                        tool: None,
                    }),
                    progress: None,
                    bash_executions: Vec::new(),
                    pending_interactions: Vec::new(),
                    latest_error: None,
                    revision: 7,
                },
            },
            interactions: Vec::new(),
            application: json!({}),
        }
    }

    fn round_trip(transcript: Transcript) -> RemoteSnapshot {
        let wire = Snapshot::from(snapshot(transcript));
        serde_json::from_value::<Snapshot<Value>>(serde_json::to_value(wire).unwrap())
            .unwrap()
            .into_internal()
            .unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn snapshot_preserves_structured_failure(
            message in ".{0,128}",
            turn_id in prop::option::of("[a-z]{1,20}"),
            runtime_failed in any::<bool>(),
            provider_status in prop::option::of(any::<u16>()),
            kind in prop::sample::select(vec![
                phi_sdk::PhiFailureKind::Authentication,
                phi_sdk::PhiFailureKind::RateLimited,
                phi_sdk::PhiFailureKind::ProviderUnavailable,
                phi_sdk::PhiFailureKind::ProviderRejected,
                phi_sdk::PhiFailureKind::Cancelled,
                phi_sdk::PhiFailureKind::Internal,
            ]),
            reason in prop::option::of(prop::sample::select(vec![
                phi_sdk::CancellationReason::UserRequested,
                phi_sdk::CancellationReason::Interrupted,
                phi_sdk::CancellationReason::AgentStopped,
            ])),
        ) {
            let mut failure = phi_sdk::PhiFailure::new(kind, message, provider_status);
            failure.cancellation_reason = reason;
            let mut source = snapshot(Transcript::new());
            source.view.snapshot.status = if runtime_failed {
                AgentInstanceStatus::Failed
            } else {
                AgentInstanceStatus::Idle
            };
            source.view.snapshot.active_turn_id = None;
            source.view.snapshot.live_run = None;
            let current_failure = runtime_failed.then(|| failure.clone());
            source.view.snapshot.latest_error = current_failure.clone();
            let failure_id = source.view.transcript.append_entry(TranscriptEntryKind::Failure {
                turn_id: turn_id.as_deref().map(Into::into),
                failure: failure.clone(),
            });
            let wire = serde_json::to_value(Snapshot::from(source)).unwrap();
            let recovered = serde_json::from_value::<Snapshot<Value>>(wire)
                .unwrap()
                .into_internal()
                .unwrap();
            prop_assert_eq!(recovered.view.snapshot.latest_error, current_failure);
            prop_assert_eq!(recovered.view.transcript.leaf_id.as_ref(), Some(&failure_id));
            let TranscriptEntryKind::Failure { turn_id: stored_turn, failure: stored } =
                &recovered.view.transcript.entries.last().unwrap().kind else {
                    return Err(TestCaseError::fail("failure history was omitted"));
                };
            prop_assert_eq!(stored_turn.as_deref(), turn_id.as_deref());
            prop_assert_eq!(stored, &failure);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn runtime_update_preserves_acceptance_without_claiming_queue_observation(
            messages in prop::collection::vec("[^\\p{C}]{1,80}", 0..16),
            revision in any::<u64>(),
            generation in "[a-z]{1,40}",
            version in any::<u32>(),
        ) {
            let mut transcript = Transcript::new();
            for message in &messages {
                transcript.append_user_message(message);
            }
            let expected = serde_json::to_value(&transcript).unwrap();
            let mut source = snapshot(transcript);
            source.view.snapshot.revision = revision;
            let update = crate::RemoteRuntimeUpdate {
                session: source.session,
                transcript: source.view.transcript,
                snapshot: source.view.snapshot,
                application: json!({"generation":generation}),
            };
            let mut value = serde_json::to_value(RuntimeUpdate::from(update)).unwrap();
            prop_assert!(value.get("pendingSubmissionIds").is_none());
            prop_assert!(value.get("interactions").is_none());
            let recovered = serde_json::from_value::<RuntimeUpdate<Value>>(value.clone())
                .unwrap()
                .into_internal()
                .unwrap();
            prop_assert_eq!(serde_json::to_value(&recovered.transcript).unwrap(), expected);
            prop_assert_eq!(recovered.snapshot.id.as_str(), recovered.session.id);
            prop_assert_eq!(recovered.snapshot.revision, revision);
            prop_assert_eq!(recovered.application["generation"].as_str(), Some(generation.as_str()));
            prop_assert_eq!(recovered.snapshot.status, AgentInstanceStatus::Running);
            prop_assert!(recovered.snapshot.pending_interactions.is_empty());
            value["version"] = json!(version);
            let decoded = serde_json::from_value::<RuntimeUpdate<Value>>(value).unwrap().into_internal();
            prop_assert_eq!(decoded.is_ok(), version == SNAPSHOT_VERSION);
        }
    }

    #[test]
    fn snapshot_omits_internal_metadata_but_preserves_tool_content() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("inspect");
        transcript.append_assistant_message(
            Some("private-response-id".into()),
            Some("private-turn-id".into()),
            vec![
                ContentBlock::Text {
                    text: "checking".into(),
                },
                ContentBlock::Thinking {
                    text: "visible reasoning".into(),
                    metadata: Some(ReasoningMetadata {
                        id: Some("private-reasoning-id".into()),
                        content: vec![ReasoningMetadataContent::Encrypted {
                            data: "private-encrypted-data".into(),
                        }],
                    }),
                },
                ContentBlock::ToolCall {
                    id: "call".into(),
                    name: "bash".into(),
                    args: json!({"command":"cargo test"}),
                    provider_call_id: Some("private-provider-call".into()),
                    provider_item_id: Some("private-provider-item".into()),
                },
            ],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage {
                input: 12,
                output: 3,
                cost: 0.5,
                ..Default::default()
            },
            "private-stop-reason".into(),
        );
        transcript.append_tool_result(
            "call",
            "tests passed",
            false,
            Some(ToolResultDetails::default()),
        );
        let result_id = transcript.leaf_id.clone().unwrap();
        transcript.append_entry(TranscriptEntryKind::Event {
            name: "private-event".into(),
            data: json!({"implementation":"private-event-data"}),
        });
        transcript.switch_snapshots.push(SwitchSnapshotRecord {
            id: "private-switch-record".into(),
            switch: SwitchSnapshot {
                name: "default".into(),
                provider: "local".into(),
                model: "test".into(),
                thinking_level: None,
                mode: "default".into(),
                prompt: Some("private-switch-prompt".into()),
            },
        });
        let expected_tokens = transcript.estimated_context_tokens();
        let wire = serde_json::to_value(Snapshot::from(snapshot(transcript.clone()))).unwrap();
        let encoded = serde_json::to_string(&wire).unwrap();
        for private in [
            "private-",
            "provider_call_id",
            "provider_item_id",
            "switch_snapshots",
            "bash_executions",
            "progress",
        ] {
            assert!(!encoded.contains(private), "{private}");
        }
        assert!(wire.get("view").is_none());
        let recovered = serde_json::from_value::<Snapshot<Value>>(wire)
            .unwrap()
            .into_internal()
            .unwrap();
        assert_eq!(
            recovered.view.transcript.estimated_context_tokens(),
            expected_tokens
        );
        assert_eq!(recovered.view.transcript.leaf_id, transcript.leaf_id);
        assert_eq!(
            recovered.view.transcript.current_branch().len(),
            transcript.current_branch().len()
        );
        assert_eq!(recovered.view.transcript.total_cost, 0.5);
        assert_eq!(recovered.view.pending_submission_ids, ["queued-input"]);
        assert_eq!(
            recovered.view.snapshot.live_run.unwrap().assistant_text,
            "live text"
        );
        let exchange = recovered.view.transcript.tool_exchange(&result_id).unwrap();
        assert_eq!(exchange.call.id, "call");
        assert_eq!(exchange.call.args, json!({"command":"cargo test"}));
        assert!(!exchange.result.is_error);
        assert!(exchange.result.details.is_some());
    }

    #[test]
    fn snapshot_preserves_nested_calls_and_scoped_interactions() {
        let call = NestedToolCall {
            id: "nested".into(),
            parent_tool_call_id: "outer".into(),
            name: "read".into(),
            status: phi_sdk::tools::NestedCallStatus::Ok,
            arguments: Some(json!({"path":"README.md"})),
            duration_ms: 12,
            error: None,
            details: None,
        };
        let mut transcript = Transcript::new();
        transcript.append_entry(TranscriptEntryKind::Event {
            name: NESTED_TOOL_EVENT.into(),
            data: serde_json::to_value(&call).unwrap(),
        });
        let mut source = snapshot(transcript);
        for agent in [source.session.id.clone(), "child".into()] {
            let mut request = InteractionEnvelope {
                request_id: format!("request-{agent}"),
                emitted_at: chrono::Utc::now(),
                scope: Default::default(),
                request: InteractionRequest::Permission(
                    phi_sdk::interaction::PermissionInteraction {
                        tool: "bash".into(),
                        input: "cargo test".into(),
                        preview: Some("preview".into()),
                        origin: Some("hook".into()),
                    },
                ),
            };
            request.scope.session_id = Some("private-scope-session".into());
            request.scope.turn_id = Some("private-scope-turn".into());
            request.scope.agent_id = Some("private-scope-agent".into());
            request.scope.parent_agent_id = Some("private-scope-parent".into());
            request.scope.parent_tool_call_id = Some("private-scope-tool-call".into());
            source.interactions.push(RemoteInteraction {
                agent_id: agent,
                request,
            });
        }
        let expected = source
            .interactions
            .iter()
            .map(|interaction| serde_json::to_value(&interaction.request.request).unwrap())
            .collect::<Vec<_>>();
        let value = serde_json::to_value(Snapshot::from(source)).unwrap();
        assert!(
            !serde_json::to_string(&value)
                .unwrap()
                .contains("private-scope-")
        );
        let recovered = serde_json::from_value::<Snapshot<Value>>(value)
            .unwrap()
            .into_internal()
            .unwrap();
        assert_eq!(recovered.interactions.len(), 2);
        assert_eq!(recovered.view.snapshot.pending_interactions.len(), 1);
        for (interaction, expected) in recovered.interactions.iter().zip(expected) {
            let scope = &interaction.request.scope;
            assert_eq!(
                scope.agent_id.as_deref(),
                Some(interaction.agent_id.as_str())
            );
            assert_eq!(
                scope.session_id.as_deref(),
                Some(interaction.agent_id.as_str())
            );
            assert!(scope.turn_id.is_none());
            assert!(scope.parent_agent_id.is_none());
            assert!(scope.parent_tool_call_id.is_none());
            assert_eq!(
                serde_json::to_value(&interaction.request.request).unwrap(),
                expected
            );
        }
        let TranscriptEntryKind::Event { name, data } = &recovered.view.transcript.entries[0].kind
        else {
            panic!("missing nested call");
        };
        assert_eq!(name, NESTED_TOOL_EVENT);
        assert_eq!(data, &serde_json::to_value(call).unwrap());
    }

    #[test]
    fn unknown_snapshot_version_is_rejected() {
        let mut value = serde_json::to_value(Snapshot::from(snapshot(Transcript::new()))).unwrap();
        value["version"] = json!(SNAPSHOT_VERSION + 1);
        assert!(
            serde_json::from_value::<Snapshot<Value>>(value)
                .unwrap()
                .into_internal()
                .is_err()
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn history_round_trip_preserves_branches_and_context(first in "[^\\p{C}]{1,80}", second in "[^\\p{C}]{1,80}", handoff in "[a-z]{1,40}") {
            let mut transcript = Transcript::new();
            transcript.append_user_message(&first);
            let fork = transcript.leaf_id.clone();
            transcript.append_user_message("alternate branch");
            transcript.select_branch(fork.as_deref()).unwrap();
            transcript.append_user_message(&second);
            let head = transcript.leaf_id.clone().unwrap();
            transcript.compress("compacted context".into(), &head).unwrap();
            transcript.append_context_message(ContextSource::FileMention, ContextScope::Turn, "file contents");
            let expected = transcript.estimated_context_tokens();
            let recovered = round_trip(transcript.clone());
            prop_assert_eq!(recovered.view.transcript.estimated_context_tokens(), expected);
            prop_assert_eq!(recovered.view.transcript.entries.len(), transcript.entries.len());
            prop_assert_eq!(&recovered.view.transcript.leaf_id, &transcript.leaf_id);
            prop_assert_eq!(serde_json::to_value(recovered.view.transcript.current_branch()).unwrap(), serde_json::to_value(transcript.current_branch()).unwrap());
            transcript.reset_context(Some(handoff)).unwrap();
            transcript.append_user_message(&second);
            let expected = transcript.estimated_context_tokens();
            let recovered = round_trip(transcript.clone());
            prop_assert_eq!(recovered.view.transcript.estimated_context_tokens(), expected);
            prop_assert_eq!(serde_json::to_value(recovered.view.transcript.current_branch()).unwrap(), serde_json::to_value(transcript.current_branch()).unwrap());
        }
    }
}
