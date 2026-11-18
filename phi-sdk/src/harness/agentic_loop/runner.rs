use super::response::{
    AccountedUsage, assistant_image_source, collect_tool_calls, flush_display_delta,
    tool_call_event, tool_item_id, transcript_content, transcript_usage,
};
use super::tools::ToolBatchItem;
use super::{AgentRuntime, AgenticLoop};
use crate::context::CompactionTrigger;
use crate::core::event::{
    AgentEvent, PhiFailure, PhiFailureKind, ProviderRecovery, RecoveryReason,
};
use crate::core::lifecycle::{AgentRunEvent, AgentTurnEvent, ToolBatchEvent};
use crate::core::turn::{PreparedTurnPrompt, QueuedSubmissionDrainMode};
use crate::hooks::{
    HookRegistry, emit_after_provider_response, emit_agent_end, emit_agent_start,
    emit_agent_turn_start, emit_before_provider_request, emit_context, emit_post_tool_batch,
    emit_stop_failure, notify_completion,
};
use crate::provider::generation::{self, GenerationFinish, PreparedGeneration};
use crate::transcript::{Transcript, Usage};
use compact_str::CompactString;
use futures::StreamExt;
use rig::completion::{AssistantContent, CompletionError, CompletionModel, Message, PromptError};
use rig::streaming::StreamedAssistantContent;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub(crate) struct RequestContext {
    pub(crate) messages: Vec<Message>,
    pub(crate) loaded_tools: std::collections::BTreeSet<String>,
    pub(crate) budget: Option<crate::context::RequestBudget>,
    pub(crate) compaction: Option<crate::context::PreparedCompaction>,
}

pub(crate) enum CompletionDecision {
    Finish,
    Continue(PreparedTurnPrompt),
    Stop,
}

pub(crate) enum CompletionBudget {
    Remaining,
    Exhausted,
}

pub(crate) enum RoundRequest {
    LoadTools(Vec<String>, oneshot::Sender<()>),
    CodemodeStore(oneshot::Sender<std::collections::BTreeMap<String, serde_json::Value>>),
    CommitCodemodeStore(phi_codemode::StoreWrites, oneshot::Sender<()>),
    Context(
        CompactionTrigger,
        crate::context::HistoryTarget,
        oneshot::Sender<Result<RequestContext, PhiFailure>>,
    ),
    EndTurn(crate::core::lifecycle::AgentTurnEvent, oneshot::Sender<()>),
    Complete(
        String,
        CompletionBudget,
        oneshot::Sender<Result<CompletionDecision, PhiFailure>>,
    ),
    CommitInput(PreparedTurnPrompt, AgentInputCommit, oneshot::Sender<bool>),
}

#[derive(Clone)]
pub(crate) struct RoundControl(mpsc::Sender<RoundRequest>);

impl RoundControl {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<RoundRequest>) {
        let (sender, receiver) = mpsc::channel(1);
        (Self(sender), receiver)
    }

    pub(super) async fn load_tools(&self, names: Vec<String>) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::LoadTools(names, reply))
            .await
            .map_err(|_| unavailable().to_string())?;
        result.await.map_err(|_| unavailable().to_string())
    }

    pub(super) async fn codemode_store(
        &self,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::CodemodeStore(reply))
            .await
            .map_err(|_| unavailable().to_string())?;
        result.await.map_err(|_| unavailable().to_string())
    }

    pub(super) async fn commit_codemode_store(
        &self,
        writes: phi_codemode::StoreWrites,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::CommitCodemodeStore(writes, reply))
            .await
            .map_err(|_| unavailable().to_string())?;
        result.await.map_err(|_| unavailable().to_string())
    }

    pub(crate) async fn context(
        &self,
        trigger: CompactionTrigger,
        target: crate::context::HistoryTarget,
    ) -> Result<RequestContext, PhiFailure> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::Context(trigger, target, reply))
            .await
            .map_err(|_| unavailable())?;
        result.await.map_err(|_| unavailable())?
    }

    async fn commit_input(
        &self,
        prompt: PreparedTurnPrompt,
        commit: AgentInputCommit,
    ) -> Result<bool, PhiFailure> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::CommitInput(prompt, commit, reply))
            .await
            .map_err(|_| unavailable())?;
        result.await.map_err(|_| unavailable())
    }

    async fn end_turn(
        &self,
        turn: crate::core::lifecycle::AgentTurnEvent,
    ) -> Result<(), PromptError> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::EndTurn(turn, reply))
            .await
            .map_err(|_| prompt_error(unavailable().into()))?;
        result.await.map_err(|_| prompt_error(unavailable().into()))
    }

    pub(crate) async fn complete(
        &self,
        response: String,
        budget: CompletionBudget,
    ) -> Result<CompletionDecision, PhiFailure> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(RoundRequest::Complete(response, budget, reply))
            .await
            .map_err(|_| unavailable())?;
        result.await.map_err(|_| unavailable())?
    }
}

fn unavailable() -> PhiFailure {
    PhiFailure::new(
        PhiFailureKind::Internal,
        "Agent execution owner is unavailable",
        None,
    )
}

pub(crate) type AgentInputCommit =
    Box<dyn FnOnce(&mut Transcript, &PreparedTurnPrompt) -> bool + Send + Sync>;

pub(crate) struct AgentSteerSubmission {
    pub(crate) prompt: PreparedTurnPrompt,
    pub(crate) before_dispatch: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
    pub(crate) commit: Option<AgentInputCommit>,
}

struct AgentSteerState {
    accepting: bool,
    reservations: usize,
    steering: VecDeque<AgentSteerSubmission>,
}

pub(crate) struct AgentSteerMailbox {
    state: Mutex<AgentSteerState>,
    changed: tokio::sync::Notify,
    drain_mode: QueuedSubmissionDrainMode,
}

pub(crate) struct AgentSteerReservation {
    mailbox: Arc<AgentSteerMailbox>,
    active: bool,
}

impl AgentSteerMailbox {
    pub(crate) fn new(drain_mode: QueuedSubmissionDrainMode) -> Self {
        Self {
            state: Mutex::new(AgentSteerState {
                accepting: true,
                reservations: 0,
                steering: VecDeque::new(),
            }),
            changed: tokio::sync::Notify::new(),
            drain_mode,
        }
    }

    pub(crate) fn reserve(self: &Arc<Self>) -> anyhow::Result<Option<AgentSteerReservation>> {
        let mut state = self.state()?;
        if !state.accepting {
            return Ok(None);
        }
        state.reservations += 1;
        Ok(Some(AgentSteerReservation {
            mailbox: self.clone(),
            active: true,
        }))
    }

    pub(crate) fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(!self.state()?.steering.is_empty())
    }

    pub(crate) fn drain(&self) -> anyhow::Result<Vec<AgentSteerSubmission>> {
        let mut state = self.state()?;
        Ok(drain_steering(&mut state.steering, self.drain_mode))
    }

    pub(crate) async fn drain_or_close(&self) -> anyhow::Result<Vec<AgentSteerSubmission>> {
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self.state()?;
                if !state.steering.is_empty() {
                    return Ok(drain_steering(&mut state.steering, self.drain_mode));
                }
                if state.reservations == 0 {
                    state.accepting = false;
                    return Ok(Vec::new());
                }
            }
            changed.await;
        }
    }

    pub(crate) async fn close_and_take_pending(&self) -> anyhow::Result<Vec<AgentSteerSubmission>> {
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self.state()?;
                state.accepting = false;
                if state.reservations == 0 {
                    return Ok(state.steering.drain(..).collect());
                }
            }
            changed.await;
        }
    }

    fn state(&self) -> anyhow::Result<std::sync::MutexGuard<'_, AgentSteerState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("agent control mailbox is unavailable"))
    }
}

fn drain_steering(
    steering: &mut VecDeque<AgentSteerSubmission>,
    mode: QueuedSubmissionDrainMode,
) -> Vec<AgentSteerSubmission> {
    match mode {
        QueuedSubmissionDrainMode::OneAtATime => steering.pop_front().into_iter().collect(),
        QueuedSubmissionDrainMode::AllPending => steering.drain(..).collect(),
    }
}

impl AgentSteerReservation {
    pub(crate) fn commit(mut self, submission: AgentSteerSubmission) -> anyhow::Result<()> {
        let mut state = self.mailbox.state()?;
        state.reservations -= 1;
        state.steering.push_back(submission);
        self.active = false;
        self.mailbox.changed.notify_one();
        Ok(())
    }
}

impl Drop for AgentSteerReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut state) = self.mailbox.state() {
            state.reservations -= 1;
            self.mailbox.changed.notify_one();
        }
    }
}

pub struct AgentRunner {
    pub(crate) tool_policy: Option<crate::permission::execution_scope::OuterTurnPolicy>,
    pub event_rx: mpsc::Receiver<AgentEvent>,
    pub(crate) round_requests: Option<mpsc::Receiver<RoundRequest>>,
    pub(crate) steering: Arc<AgentSteerMailbox>,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl AgentRunner {
    pub fn abort(&self) {
        if let Some(policy) = &self.tool_policy {
            policy.retire();
        }
        self.task.abort();
    }

    pub(crate) fn reserve_steer(&self) -> anyhow::Result<Option<AgentSteerReservation>> {
        self.steering.reserve()
    }
}

impl Drop for AgentRunner {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Default)]
struct ProviderRecoveryState {
    retries: usize,
    overflow_recovered: bool,
}

const DEFAULT_MAX_TURNS: usize = 100;
const MAX_PROVIDER_RETRIES: usize = 3;
const PROVIDER_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// One inner agent-loop turn within a core turn: a model response and, when
/// requested, its tool batch whose results feed the next agent turn.
pub struct AgentTurn {
    pub id: String,
    pub index: usize,
}

impl AgentTurn {
    pub fn new(index: usize) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            index,
        }
    }
}

impl<M> AgenticLoop<M>
where
    M: CompletionModel + Clone + 'static,
{
    pub fn new(runtime: AgentRuntime<M>, hooks: &HookRegistry) -> Self {
        Self {
            runtime,
            hooks: hooks.clone(),
            interaction_scope: crate::core::event::CoreEventScope::default(),
            max_turns: DEFAULT_MAX_TURNS,
            tool_policy: Default::default(),
        }
    }

    pub fn interaction_scope(mut self, scope: crate::core::event::CoreEventScope) -> Self {
        self.interaction_scope = scope;
        self
    }

    pub(crate) fn tool_policy(mut self, policy: crate::core::turn::TurnToolPolicy) -> Self {
        self.tool_policy = policy;
        self
    }

    pub fn max_turns(mut self, max_turns: Option<usize>) -> Self {
        if let Some(max_turns) = max_turns {
            self.max_turns = max_turns;
        }
        self
    }

    async fn prepare_request(
        &mut self,
        control: &RoundControl,
        turn: &AgentTurn,
        trigger: CompactionTrigger,
    ) -> Result<PreparedGeneration<M>, PromptError> {
        let target = crate::context::HistoryTarget {
            provider: self.runtime.provider_id().to_owned(),
            api: self.runtime.provider_api(),
            model: self.runtime.model_id().to_owned(),
        };
        let context = control
            .context(trigger, target.clone())
            .await
            .map_err(|failure| prompt_error(failure.into()))?;
        self.runtime
            .restore_loaded_tools(context.loaded_tools)
            .await
            .map_err(prompt_error)?;
        let compacted = context.compaction.is_some();
        if let Some(compaction) = context.compaction {
            compaction.publish(&self.hooks).await;
        }
        let dynamic_context = emit_context(&self.hooks, &turn.id).await;
        let build_request = |mut messages: Vec<Message>| {
            let prompt = messages.pop().ok_or_else(|| {
                prompt_error(anyhow::anyhow!("canonical request context is empty"))
            })?;
            let history = crate::context::append_dynamic_context(messages, dynamic_context.clone());
            self.runtime
                .request(prompt, history)
                .map_err(PromptError::from)
        };
        let mut request = build_request(context.messages)?;
        if let Some(budget) = context.budget
            && budget.exceeds(&request).map_err(prompt_error)?
        {
            if !compacted {
                let context = control
                    .context(CompactionTrigger::Budget, target)
                    .await
                    .map_err(|failure| prompt_error(failure.into()))?;
                if let Some(compaction) = context.compaction {
                    compaction.publish(&self.hooks).await;
                }
                request = build_request(context.messages)?;
            }
            if budget.exceeds(&request).map_err(prompt_error)? {
                return Err(prompt_error(anyhow::anyhow!(
                    "estimated request context exceeds the model window after compaction and output reservation"
                )));
            }
        }
        self.runtime
            .prepare_generation(request)
            .map_err(PromptError::from)
    }

    fn lifecycle_run_event(&self) -> AgentRunEvent {
        AgentRunEvent {
            session_id: self
                .interaction_scope
                .session_id
                .as_ref()
                .map(ToString::to_string),
            agent_id: self
                .interaction_scope
                .agent_id
                .as_ref()
                .map(ToString::to_string),
            parent_agent_id: self
                .interaction_scope
                .parent_agent_id
                .as_ref()
                .map(ToString::to_string),
            parent_tool_call_id: self
                .interaction_scope
                .parent_tool_call_id
                .as_ref()
                .map(ToString::to_string),
        }
    }

    fn lifecycle_turn_event(&self, turn: &AgentTurn) -> AgentTurnEvent {
        AgentTurnEvent {
            run: self.lifecycle_run_event(),
            turn_id: turn.id.clone(),
            index: turn.index,
        }
    }

    pub fn spawn_events(self) -> AgentRunner {
        self.spawn_events_with_prompt(None)
    }

    pub(crate) fn spawn_events_with_prompt(
        mut self,
        system_prompt: Option<crate::core::lifecycle::BuildSystemPromptEvent>,
    ) -> AgentRunner {
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        let steering = Arc::new(AgentSteerMailbox::new(
            crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
        ));
        let run_steering = steering.clone();
        let (control, round_requests) = RoundControl::channel();
        let scope = self.interaction_scope.clone();
        let policy = self
            .runtime
            .tools()
            .outer_turn_policy(self.tool_policy.clone());
        let active_policy = policy.clone();
        let task = tokio::spawn(policy.run(crate::core::interaction::with_interaction_scope(
            scope,
            async move {
                if let Some(event) = system_prompt {
                    let prompt = crate::context::finalize_system_prompt(&self.hooks, event).await;
                    self.runtime = self.runtime.system_prompt(Some(prompt));
                } else {
                    self.runtime.prepare_system_prompt(&self.hooks).await;
                }
                let result = self
                    .run_events(event_tx.clone(), run_steering, control)
                    .await;
                if let Err(error) = &result {
                    emit_stop_failure(&self.hooks, &error.to_string()).await;
                }
                emit_agent_end(&self.hooks, self.lifecycle_run_event()).await;
                let event = match result {
                    Ok((response, usage)) => AgentEvent::RunFinished {
                        response: response.into(),
                        usage,
                    },
                    Err(error) => AgentEvent::Error(phi_failure(error)),
                };
                let _ = event_tx.send(event).await;
            },
        )));

        AgentRunner {
            tool_policy: Some(active_policy),
            event_rx,
            round_requests: Some(round_requests),
            steering,
            task,
        }
    }

    async fn run_events(
        &mut self,
        event_tx: mpsc::Sender<AgentEvent>,
        steering: Arc<AgentSteerMailbox>,
        control: RoundControl,
    ) -> Result<(String, Usage), PromptError> {
        emit_agent_start(&self.hooks, self.lifecycle_run_event()).await;
        let mut total_usage = AccountedUsage::default();

        for turn_index in 0..self.max_turns {
            let can_continue = turn_index + 1 < self.max_turns;
            let turn = AgentTurn::new(turn_index);
            emit_agent_turn_start(&self.hooks, self.lifecycle_turn_event(&turn)).await;
            let mut recovery = ProviderRecoveryState::default();
            let mut context_trigger = CompactionTrigger::Threshold;
            let (stream, text, tool_calls, display_message_id) = 'attempts: loop {
                let request = self
                    .prepare_request(&control, &turn, context_trigger)
                    .await?;
                emit_before_provider_request(&self.hooks, &turn.id).await;
                let mut stream = match request.start_attempt().await {
                    Ok(stream) => stream,
                    Err(error) => {
                        if let Some(trigger) = self
                            .recover_provider(&error, &mut recovery, &event_tx)
                            .await
                        {
                            context_trigger = trigger;
                            continue 'attempts;
                        }
                        control.end_turn(self.lifecycle_turn_event(&turn)).await?;
                        return Err(error.into());
                    }
                };
                let mut response_text = String::new();
                let mut reasoning_text = String::new();
                let mut reasoning_active = false;
                let mut emitted_tool_call_ids = HashSet::new();
                let mut emitted_tool_calls = Vec::new();
                let mut pending_display = String::new();
                let mut display_index = 0;
                let display_message_id = uuid::Uuid::new_v4().to_string();
                let _ = event_tx
                    .send(AgentEvent::AssistantStarted {
                        id: CompactString::from(display_message_id.clone()),
                    })
                    .await;

                while let Some(item) = stream.next().await {
                    let item = match item {
                        Ok(item) => item,
                        Err(error) => {
                            flush_display_delta(
                                &self.hooks,
                                &event_tx,
                                &turn.id,
                                &display_message_id,
                                &mut display_index,
                                &mut pending_display,
                                "",
                                true,
                            )
                            .await;
                            let _ = event_tx
                                .send(AgentEvent::AssistantInterrupted {
                                    turn_id: turn.id.clone().into(),
                                    provider: self.runtime.provider_id().into(),
                                    api: self.runtime.provider_api(),
                                    model: self.runtime.model_id().into(),
                                })
                                .await;
                            for call in &emitted_tool_calls {
                                let _ = event_tx
                                    .send(AgentEvent::ToolResult {
                                        id: tool_item_id(call).into(),
                                        entry_id: Transcript::new_entry_id(),
                                        output:
                                            "provider response interrupted before tool execution"
                                                .into(),
                                        is_error: true,
                                        details: None,
                                    })
                                    .await;
                            }
                            if let Some(trigger) = self
                                .recover_provider(&error, &mut recovery, &event_tx)
                                .await
                            {
                                context_trigger = trigger;
                                continue 'attempts;
                            }
                            control.end_turn(self.lifecycle_turn_event(&turn)).await?;
                            return Err(error.into());
                        }
                    };
                    match item {
                        StreamedAssistantContent::Text(text) => {
                            if reasoning_active {
                                let _ = event_tx.send(AgentEvent::ReasoningDone).await;
                                reasoning_active = false;
                            }
                            response_text.push_str(&text.text);
                            let _ = event_tx
                                .send(AgentEvent::AssistantDelta {
                                    original: text.text.clone().into(),
                                    display: "".into(),
                                })
                                .await;
                            flush_display_delta(
                                &self.hooks,
                                &event_tx,
                                &turn.id,
                                &display_message_id,
                                &mut display_index,
                                &mut pending_display,
                                &text.text,
                                false,
                            )
                            .await;
                        }
                        StreamedAssistantContent::Reasoning { reasoning, .. } => {
                            let text = reasoning.display_text();
                            if !text.is_empty() {
                                if !reasoning_text.is_empty() {
                                    reasoning_text.push('\n');
                                }
                                reasoning_text.push_str(&text);
                                reasoning_active = true;
                                let _ = event_tx
                                    .send(AgentEvent::Reasoning(CompactString::from(
                                        reasoning_text.clone(),
                                    )))
                                    .await;
                            }
                        }
                        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                            reasoning_text.push_str(&reasoning);
                            reasoning_active = true;
                            let _ = event_tx
                                .send(AgentEvent::Reasoning(CompactString::from(
                                    reasoning_text.clone(),
                                )))
                                .await;
                        }
                        StreamedAssistantContent::ToolCall { tool_call, .. } => {
                            if reasoning_active {
                                let _ = event_tx.send(AgentEvent::ReasoningDone).await;
                                reasoning_active = false;
                            }
                            emitted_tool_call_ids.insert(tool_call.id.clone());
                            emitted_tool_calls.push(tool_call.clone());
                            let _ = event_tx.send(tool_call_event(&tool_call)).await;
                        }
                        StreamedAssistantContent::Final(_) => {}
                        StreamedAssistantContent::ToolCallDelta { .. } => {}
                        StreamedAssistantContent::Unknown(_) => {}
                    }
                }

                if reasoning_active {
                    let _ = event_tx.send(AgentEvent::ReasoningDone).await;
                }
                flush_display_delta(
                    &self.hooks,
                    &event_tx,
                    &turn.id,
                    &display_message_id,
                    &mut display_index,
                    &mut pending_display,
                    "",
                    true,
                )
                .await;
                emit_after_provider_response(&self.hooks, &turn.id).await;

                let choice = stream.choice.clone();
                let text = if response_text.is_empty() {
                    let text = generation::text_blocks(&choice)
                        .collect::<Vec<_>>()
                        .join("\n");
                    // Some adapters flatten reasoning into text; do not publish it again as an answer.
                    if text.trim() == reasoning_text.trim() {
                        String::new()
                    } else {
                        let _ = event_tx
                            .send(AgentEvent::AssistantDelta {
                                original: text.clone().into(),
                                display: "".into(),
                            })
                            .await;
                        flush_display_delta(
                            &self.hooks,
                            &event_tx,
                            &turn.id,
                            &display_message_id,
                            &mut display_index,
                            &mut pending_display,
                            &text,
                            true,
                        )
                        .await;
                        text
                    }
                } else {
                    response_text
                };
                let tool_calls = collect_tool_calls(&choice);
                for content in choice.iter() {
                    match content {
                        AssistantContent::ToolCall(call)
                            if !emitted_tool_call_ids.contains(&call.id) =>
                        {
                            let _ = event_tx.send(tool_call_event(call)).await;
                        }
                        AssistantContent::Image(image) => {
                            if let Some(source) = assistant_image_source(image) {
                                let _ = event_tx.send(AgentEvent::AssistantImage { source }).await;
                            }
                        }
                        _ => {}
                    }
                }
                break (stream, text, tool_calls, display_message_id);
            };
            let canonical_content = transcript_content(&stream.choice);
            let assistant_message_id = stream.identity().message_id;
            let response_usage = match (stream.response.as_ref(), self.runtime.provider_api()) {
                (Some(terminal), Some(api)) => {
                    crate::provider::usage::observe_usage(terminal, api, usize::MAX)
                }
                _ => crate::provider::usage::ObservedUsage::from(stream.usage()),
            };
            let usage = transcript_usage(
                self.runtime.provider_id(),
                self.runtime.provider_api(),
                self.runtime.cost(),
                &response_usage,
                &text,
            );
            let finish_reason = GenerationFinish::from_reason(
                stream
                    .response
                    .as_ref()
                    .and_then(|response| response.finish_reason.as_ref()),
                self.runtime.provider_api(),
            );
            let stop_reason = CompactString::new(finish_reason.as_str());
            total_usage.record(&usage);
            let _ = event_tx
                .send(AgentEvent::AssistantFinished {
                    id: CompactString::from(display_message_id),
                    provider_message_id: assistant_message_id.map(CompactString::from),
                    turn_id: CompactString::from(turn.id.clone()),
                    provider: CompactString::from(self.runtime.provider_id()),
                    api: self.runtime.provider_api(),
                    model: CompactString::from(self.runtime.model_id()),
                    content: canonical_content,
                    response: CompactString::from(text.clone()),
                    display: CompactString::new(""),
                    usage: usage.into_usage(),
                    stop_reason,
                    has_tool_calls: !tool_calls.is_empty(),
                })
                .await;

            if !matches!(
                finish_reason,
                GenerationFinish::Stop | GenerationFinish::ToolCalls
            ) {
                let error = match &finish_reason {
                    GenerationFinish::Length => "provider response was truncated",
                    GenerationFinish::ContentFilter => "provider response was filtered",
                    _ => "provider response has an unknown finish reason",
                };
                for call in &tool_calls {
                    let _ = event_tx
                        .send(AgentEvent::ToolResult {
                            id: tool_item_id(call).into(),
                            entry_id: Transcript::new_entry_id(),
                            output: error.into(),
                            is_error: true,
                            details: None,
                        })
                        .await;
                }
                control.end_turn(self.lifecycle_turn_event(&turn)).await?;
                if can_continue
                    && matches!(finish_reason, GenerationFinish::Length)
                    && !tool_calls.is_empty()
                    && !self.runtime.is_directed()
                {
                    dispatch_steering(steering.drain().map_err(prompt_error)?, &event_tx, &control)
                        .await?;
                    continue;
                }
                return Err(CompletionError::ResponseError(error.into()).into());
            }

            if tool_calls.is_empty() {
                control.end_turn(self.lifecycle_turn_event(&turn)).await?;
                if can_continue
                    && dispatch_steering(
                        steering.drain().map_err(prompt_error)?,
                        &event_tx,
                        &control,
                    )
                    .await?
                {
                    continue;
                }
                let decision = control
                    .complete(
                        text.clone(),
                        if can_continue {
                            CompletionBudget::Remaining
                        } else {
                            CompletionBudget::Exhausted
                        },
                    )
                    .await
                    .map_err(|failure| prompt_error(failure.into()))?;
                if can_continue
                    && dispatch_steering(
                        steering.drain().map_err(prompt_error)?,
                        &event_tx,
                        &control,
                    )
                    .await?
                {
                    continue;
                }
                match decision {
                    CompletionDecision::Continue(prompt) => {
                        self.runtime
                            .validate_turn_policy(&prompt.tool_policy)
                            .map_err(|error| prompt_error(error.into()))?;
                        let policy = prompt.tool_policy.clone();
                        let committed = control
                            .commit_input(
                                prompt,
                                Box::new(|transcript, prompt| {
                                    prompt.commit(transcript);
                                    true
                                }),
                            )
                            .await
                            .map_err(|failure| prompt_error(failure.into()))?;
                        if !committed {
                            return Err(prompt_error(anyhow::anyhow!(
                                "completion feedback was not committed"
                            )));
                        }
                        crate::permission::execution_scope::OuterTurnPolicy::merge_active(&policy);
                        continue;
                    }
                    CompletionDecision::Finish | CompletionDecision::Stop => {}
                }
                if can_continue {
                    let pending = steering.drain_or_close().await.map_err(prompt_error)?;
                    if dispatch_steering(pending, &event_tx, &control).await? {
                        continue;
                    }
                }
                notify_completion(&self.hooks, &text);
                return Ok((text, total_usage.into_usage()));
            }

            let batch = self
                .execute_tool_batch(&tool_calls, &event_tx, &control)
                .await;
            for item in &batch {
                let _ = event_tx.send(item.event()).await;
            }

            let batch_result = emit_post_tool_batch(
                &self.hooks,
                ToolBatchEvent {
                    tool_calls: batch.iter().map(ToolBatchItem::lifecycle_value).collect(),
                },
            )
            .await;
            let delegation_failure = if self.runtime.is_directed() {
                batch.iter().find_map(ToolBatchItem::failure)
            } else {
                None
            };
            control.end_turn(self.lifecycle_turn_event(&turn)).await?;
            if let Some(error) = delegation_failure {
                return Err(prompt_error(anyhow::anyhow!(error)));
            }

            let directed_background = self.runtime.is_directed()
                && self.runtime.directed_mode()
                    != Some(crate::harness::manager::preparation::DelegationMode::Foreground);
            let terminate = !batch.is_empty() && batch.iter().all(ToolBatchItem::terminates);
            if batch_result.block || directed_background || terminate {
                if terminate && !batch_result.block && !directed_background && can_continue {
                    let pending = steering.drain_or_close().await.map_err(prompt_error)?;
                    if dispatch_steering(pending, &event_tx, &control).await? {
                        continue;
                    }
                }
                let response = batch_result.reason.unwrap_or_default();
                notify_completion(&self.hooks, &response);
                return Ok((response, total_usage.into_usage()));
            }

            if can_continue {
                let pending = steering.drain().map_err(prompt_error)?;
                dispatch_steering(pending, &event_tx, &control).await?;
            }
        }

        Err(PromptError::MaxTurnsError {
            max_turns: self.max_turns,
            chat_history: Box::new(Vec::new()),
            prompt: Box::new(Message::user("maximum turns exceeded")),
        })
    }

    fn recovery_event(
        &self,
        reason: RecoveryReason,
        attempt: usize,
        delay: std::time::Duration,
    ) -> AgentEvent {
        AgentEvent::ProviderRecovery {
            recovery: ProviderRecovery {
                reason,
                attempt,
                max_attempts: match reason {
                    RecoveryReason::ProviderRetry => MAX_PROVIDER_RETRIES,
                    RecoveryReason::ContextOverflow => 1,
                },
                delay_ms: delay.as_millis() as u64,
                provider: self.runtime.provider_id().to_string(),
                model: self.runtime.model_id().to_string(),
            },
        }
    }

    async fn recover_provider(
        &self,
        error: &CompletionError,
        recovery: &mut ProviderRecoveryState,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Option<CompactionTrigger> {
        let facts = crate::provider::error_facts(error);
        if facts.context_overflow {
            if recovery.overflow_recovered {
                return None;
            }
            recovery.overflow_recovered = true;
            let _ = events
                .send(self.recovery_event(
                    RecoveryReason::ContextOverflow,
                    1,
                    std::time::Duration::ZERO,
                ))
                .await;
            return Some(CompactionTrigger::Overflow);
        }
        let kind = completion_failure_kind(facts.kind);
        if recovery.retries == MAX_PROVIDER_RETRIES
            || !matches!(
                kind,
                PhiFailureKind::RateLimited | PhiFailureKind::ProviderUnavailable
            )
        {
            return None;
        }
        let delay = facts
            .retry_after
            .unwrap_or_else(|| PROVIDER_RETRY_DELAY * 2_u32.pow(recovery.retries as u32));
        recovery.retries += 1;
        let _ = events
            .send(self.recovery_event(RecoveryReason::ProviderRetry, recovery.retries, delay))
            .await;
        tokio::time::sleep(delay).await;
        Some(CompactionTrigger::Threshold)
    }
}

async fn dispatch_steering(
    steering: Vec<AgentSteerSubmission>,
    event_tx: &mpsc::Sender<AgentEvent>,
    control: &RoundControl,
) -> Result<bool, PromptError> {
    let mut dispatched = false;
    for steer in steering {
        dispatched |= dispatch_steer(steer, event_tx, control).await?;
    }
    Ok(dispatched)
}

async fn dispatch_steer(
    steer: AgentSteerSubmission,
    event_tx: &mpsc::Sender<AgentEvent>,
    control: &RoundControl,
) -> Result<bool, PromptError> {
    if let Err(error) = (steer.before_dispatch)() {
        if let Some(submission_id) = steer.prompt.submission_id {
            let _ = event_tx
                .send(AgentEvent::SubmissionDiscarded { submission_id })
                .await;
        }
        return Err(prompt_error(error));
    }
    let prompt = steer.prompt;
    if let Some(commit) = steer.commit {
        let submission_id = prompt.submission_id.clone();
        let committed = control
            .commit_input(prompt, commit)
            .await
            .map_err(|failure| prompt_error(failure.into()))?;
        if !committed && let Some(submission_id) = submission_id {
            let _ = event_tx
                .send(AgentEvent::SubmissionDiscarded { submission_id })
                .await;
        }
        return Ok(committed);
    }
    crate::permission::execution_scope::OuterTurnPolicy::merge_active(&prompt.tool_policy);
    let _ = event_tx
        .send(AgentEvent::UserInputInjected {
            submission_id: prompt.submission_id,
            prompt: CompactString::from(prompt.prompt),
            source: prompt.source,
            contexts: prompt.contexts,
            content: prompt.user_content.clone(),
        })
        .await;
    Ok(true)
}

fn prompt_error(error: anyhow::Error) -> PromptError {
    PromptError::CompletionError(CompletionError::RequestError(error.into()))
}

fn phi_failure(error: PromptError) -> PhiFailure {
    let provider_status = error
        .provider_response_status()
        .map(|status| status.as_u16());
    let kind = match &error {
        PromptError::CompletionError(error) => {
            completion_failure_kind(crate::provider::error_facts(error).kind)
        }
        PromptError::MaxTurnsError { .. } => PhiFailureKind::MaxTurns,
        PromptError::PromptCancelled { .. } => PhiFailureKind::Cancelled,
        PromptError::UnknownToolCall { .. } => PhiFailureKind::InvalidToolCall,
        PromptError::MemoryError(_) => PhiFailureKind::Internal,
    };
    if kind == PhiFailureKind::Cancelled {
        PhiFailure::cancelled(crate::CancellationReason::Interrupted, error.to_string())
    } else {
        PhiFailure::new(kind, error.to_string(), provider_status)
    }
}

fn completion_failure_kind(kind: crate::provider::ProviderErrorKind) -> PhiFailureKind {
    use crate::provider::ProviderErrorKind;

    match kind {
        ProviderErrorKind::Authentication | ProviderErrorKind::AccessDenied => {
            PhiFailureKind::Authentication
        }
        ProviderErrorKind::RateLimited => PhiFailureKind::RateLimited,
        ProviderErrorKind::Unavailable => PhiFailureKind::ProviderUnavailable,
        ProviderErrorKind::Rejected => PhiFailureKind::ProviderRejected,
        ProviderErrorKind::InvalidResponse => PhiFailureKind::InvalidResponse,
        ProviderErrorKind::InvalidRequest => PhiFailureKind::Internal,
        ProviderErrorKind::Unknown => PhiFailureKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;
    use crate::core::lifecycle::UserPromptSubmitSource;
    use crate::core::turn::PreparedTurnPrompt;
    use crate::transcript::UserContentBlock;
    #[test]
    fn classifies_prompt_failures_and_retryability() {
        let max_turns = phi_failure(PromptError::MaxTurnsError {
            max_turns: 1,
            chat_history: Box::new(Vec::new()),
            prompt: Box::new(Message::user("test")),
        });
        let internal = phi_failure(PromptError::CompletionError(CompletionError::RequestError(
            anyhow::anyhow!("steering unavailable").into(),
        )));
        let ambiguous_provider = phi_failure(PromptError::CompletionError(
            CompletionError::ProviderError("provider error".to_string()),
        ));
        let unavailable = PhiFailure::new(
            PhiFailureKind::ProviderUnavailable,
            "provider unavailable",
            Some(503),
        );

        assert_eq!(max_turns.kind, PhiFailureKind::MaxTurns);
        assert!(!max_turns.is_retryable());
        assert_eq!(internal.kind, PhiFailureKind::Internal);
        assert_eq!(ambiguous_provider.kind, PhiFailureKind::Unknown);
        assert!(!ambiguous_provider.is_retryable());
        assert!(unavailable.is_retryable());
        for (status, expected) in [
            (
                http::StatusCode::UNAUTHORIZED,
                PhiFailureKind::Authentication,
            ),
            (http::StatusCode::FORBIDDEN, PhiFailureKind::Authentication),
            (
                http::StatusCode::TOO_MANY_REQUESTS,
                PhiFailureKind::RateLimited,
            ),
            (
                http::StatusCode::REQUEST_TIMEOUT,
                PhiFailureKind::ProviderUnavailable,
            ),
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                PhiFailureKind::ProviderUnavailable,
            ),
            (
                http::StatusCode::BAD_REQUEST,
                PhiFailureKind::ProviderRejected,
            ),
        ] {
            let failure = phi_failure(PromptError::CompletionError(
                CompletionError::from_http_response(status, "context_length_exceeded"),
            ));
            assert_eq!(failure.kind, expected);
            assert_eq!(failure.provider_status, Some(status.as_u16()));
        }
        for message in [
            "Http client error: error decoding response body",
            "Stream ended",
        ] {
            let failure = phi_failure(PromptError::CompletionError(
                CompletionError::ProviderError(message.into()),
            ));
            assert_eq!(failure.kind, PhiFailureKind::ProviderUnavailable);
            assert!(failure.is_retryable());
        }
    }

    #[tokio::test]
    async fn outer_turn_steer_unions_only_injected_intents_in_both_drain_modes() {
        use crate::core::turn::{QueuedSubmissionDrainMode, TurnToolPolicy};
        use crate::permission::checker::PermissionChecker;
        use crate::permission::execution_scope::OuterTurnPolicy;
        use std::sync::Mutex;

        for mode in [
            QueuedSubmissionDrainMode::OneAtATime,
            QueuedSubmissionDrainMode::AllPending,
        ] {
            let checker = Arc::new(Mutex::new(PermissionChecker::new(
                &Default::default(),
                None,
            )));
            let mailbox = Arc::new(AgentSteerMailbox::new(mode));
            for (id, policy) in [
                (
                    "allow",
                    TurnToolPolicy {
                        allowances: vec![("Read".into(), "**".into())],
                        ..Default::default()
                    },
                ),
                ("ordinary", TurnToolPolicy::default()),
                (
                    "remove",
                    TurnToolPolicy {
                        removals: vec!["read".into()],
                        ..Default::default()
                    },
                ),
            ] {
                mailbox
                    .reserve()
                    .unwrap()
                    .unwrap()
                    .commit(AgentSteerSubmission {
                        prompt: PreparedTurnPrompt {
                            hook_notices: Vec::new(),
                            submission_id: Some(id.into()),
                            prompt: id.into(),
                            source: UserPromptSubmitSource::Sdk,
                            contexts: Vec::new(),
                            user_content: vec![UserContentBlock::Text { text: id.into() }],
                            tool_policy: policy,
                        },
                        before_dispatch: Box::new(|| Ok(())),
                        commit: None,
                    })
                    .unwrap();
            }
            let initial = TurnToolPolicy {
                removals: vec!["Bash".into()],
                ..Default::default()
            };
            OuterTurnPolicy::new(Some(checker.clone()), initial)
                .run(async {
                    assert!(OuterTurnPolicy::current(&checker).allowances.is_empty());
                    let (sender, mut events) = mpsc::channel(4);
                    let pending = mailbox.drain().unwrap();
                    assert_eq!(
                        pending.len(),
                        if mode == QueuedSubmissionDrainMode::AllPending {
                            3
                        } else {
                            1
                        }
                    );
                    dispatch_steering(pending, &sender, &RoundControl::channel().0)
                        .await
                        .unwrap();
                    assert_eq!(OuterTurnPolicy::current(&checker).allowances.len(), 1);
                    loop {
                        let pending = mailbox.drain().unwrap();
                        if pending.is_empty() {
                            break;
                        }
                        dispatch_steering(pending, &sender, &RoundControl::channel().0)
                            .await
                            .unwrap();
                    }
                    let active = OuterTurnPolicy::current(&checker);
                    assert_eq!(active.removals, ["Bash", "Read"]);
                    assert_eq!(active.allowances, [("Read".into(), "**".into())]);
                    OuterTurnPolicy::merge_active(&active);
                    assert_eq!(OuterTurnPolicy::current(&checker), active);
                    assert_eq!(std::iter::from_fn(|| events.try_recv().ok()).count(), 3);
                    let failed = AgentSteerSubmission {
                        prompt: PreparedTurnPrompt {
                            hook_notices: Vec::new(),
                            submission_id: Some("failed-policy".into()),
                            prompt: "failed".into(),
                            source: UserPromptSubmitSource::Sdk,
                            contexts: Vec::new(),
                            user_content: Vec::new(),
                            tool_policy: TurnToolPolicy {
                                removals: vec!["Grep".into()],
                                ..Default::default()
                            },
                        },
                        before_dispatch: Box::new(|| Err(anyhow::anyhow!("dispatch rejected"))),
                        commit: None,
                    };
                    assert!(
                        dispatch_steer(failed, &sender, &RoundControl::channel().0)
                            .await
                            .is_err()
                    );
                    assert_eq!(OuterTurnPolicy::current(&checker), active);
                })
                .await;
        }
    }

    #[tokio::test]
    async fn peer_input_commit_keeps_policy() {
        use crate::core::turn::TurnToolPolicy;
        use crate::permission::checker::PermissionChecker;
        use crate::permission::execution_scope::OuterTurnPolicy;
        use std::sync::Mutex;

        for committed in [false, true] {
            let checker = Arc::new(Mutex::new(PermissionChecker::new(
                &Default::default(),
                None,
            )));
            OuterTurnPolicy::new(Some(checker.clone()), TurnToolPolicy::default())
                .run(async {
                    let submission = AgentSteerSubmission {
                        prompt: PreparedTurnPrompt {
                            hook_notices: Vec::new(),
                            submission_id: Some("peer-input".into()),
                            prompt: "peer context".into(),
                            source: UserPromptSubmitSource::Internal,
                            contexts: Vec::new(),
                            user_content: Vec::new(),
                            tool_policy: TurnToolPolicy {
                                allowances: vec![("Read".into(), "**".into())],
                                removals: vec!["Bash".into()],
                            },
                        },
                        before_dispatch: Box::new(|| Ok(())),
                        commit: Some(Box::new(move |transcript, prompt| {
                            if committed {
                                transcript.append_agent_message(&prompt.prompt);
                            }
                            committed
                        })),
                    };
                    let (sender, mut events) = mpsc::channel(1);
                    let (control, mut requests) = RoundControl::channel();
                    let (result, transcript) =
                        tokio::join!(dispatch_steer(submission, &sender, &control), async {
                            let RoundRequest::CommitInput(prompt, commit, reply) =
                                requests.recv().await.unwrap()
                            else {
                                panic!("expected actor commit request")
                            };
                            let mut transcript = Transcript::new();
                            reply.send(commit(&mut transcript, &prompt)).unwrap();
                            transcript
                        });
                    assert_eq!(result.unwrap(), committed);
                    assert_eq!(transcript.entries.len(), usize::from(committed));
                    assert_eq!(
                        OuterTurnPolicy::current(&checker),
                        TurnToolPolicy::default()
                    );
                    if committed {
                        assert!(events.try_recv().is_err());
                    } else {
                        assert!(matches!(events.try_recv(),
                            Ok(AgentEvent::SubmissionDiscarded { submission_id })
                                if submission_id == "peer-input"));
                    }
                })
                .await;
        }
    }

    #[tokio::test]
    async fn steering_runs_injection_hook_and_becomes_next_prompt() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let injected = Arc::new(AtomicBool::new(false));
        let injection_flag = injected.clone();
        let steering = Arc::new(AgentSteerMailbox::new(
            crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
        ));
        steering
            .reserve()
            .unwrap()
            .unwrap()
            .commit(AgentSteerSubmission {
                prompt: PreparedTurnPrompt {
                    hook_notices: Vec::new(),
                    tool_policy: Default::default(),
                    submission_id: Some("steer-admission".into()),
                    prompt: "change direction".to_string(),
                    source: UserPromptSubmitSource::Interactive,
                    contexts: Vec::new(),
                    user_content: vec![UserContentBlock::Text {
                        text: "change direction".into(),
                    }],
                },
                before_dispatch: Box::new(move || {
                    injection_flag.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                commit: None,
            })
            .unwrap();
        let pending = steering.drain().unwrap();
        let next = dispatch_steering(pending, &event_tx, &RoundControl::channel().0)
            .await
            .unwrap();

        assert!(next);
        assert!(injected.load(Ordering::SeqCst));
        assert!(matches!(
            event_rx.recv().await,
            Some(AgentEvent::UserInputInjected { submission_id: Some(id), prompt, .. })
                if id == "steer-admission" && prompt == "change direction"
        ));
        assert!(
            dispatch_steer(
                AgentSteerSubmission {
                    prompt: PreparedTurnPrompt {
                        hook_notices: Vec::new(),
                        tool_policy: Default::default(),
                        submission_id: Some("failed-steer".into()),
                        prompt: "discarded".into(),
                        source: UserPromptSubmitSource::Sdk,
                        contexts: Vec::new(),
                        user_content: vec![UserContentBlock::Text {
                            text: "discarded".into()
                        }],
                    },
                    before_dispatch: Box::new(|| Err(anyhow::anyhow!("dispatch failed"))),
                    commit: None,
                },
                &event_tx,
                &RoundControl::channel().0
            )
            .await
            .is_err()
        );
        assert!(matches!(event_rx.recv().await,
            Some(AgentEvent::SubmissionDiscarded { submission_id }) if submission_id == "failed-steer"));
    }

    #[tokio::test]
    async fn steering_reservation_keeps_final_safe_point_open() {
        let steering = Arc::new(AgentSteerMailbox::new(
            crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
        ));
        let reservation = steering.reserve().unwrap().unwrap();
        let waiting = steering.clone();
        let drain = tokio::spawn(async move { waiting.drain_or_close().await.unwrap() });

        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        reservation
            .commit(AgentSteerSubmission {
                prompt: PreparedTurnPrompt {
                    hook_notices: Vec::new(),
                    submission_id: None,
                    tool_policy: Default::default(),
                    prompt: "reserved".to_string(),
                    source: UserPromptSubmitSource::Sdk,
                    contexts: Vec::new(),
                    user_content: vec![UserContentBlock::Text {
                        text: "reserved".into(),
                    }],
                },
                before_dispatch: Box::new(|| Ok(())),
                commit: None,
            })
            .unwrap();
        assert_eq!(drain.await.unwrap().len(), 1);
    }
}
