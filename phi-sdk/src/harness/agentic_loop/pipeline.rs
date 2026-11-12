use super::runner::{
    AgentRunner, CompletionBudget, CompletionDecision, RequestContext, RoundRequest,
};
use crate::config::RuntimeConfig;
use crate::context::PromptPreparation;
use crate::context::{PreparedCompaction, prepare_compaction};
use crate::core::event::{
    AgentEvent, CoreEvent, CoreEventEnvelope, CoreEventScope, PhiFailure, PhiFailureKind,
};
use crate::core::lifecycle::{CompletionEvent, UserPromptDispatch, UserPromptSubmitSource};
use crate::core::reducer::{CoreRunState, reduce_agent_event};
use crate::core::turn::{CoreTurnPreparation, TurnPrompt};
use crate::hooks::HookRegistry;
use crate::provider::AnyClient;
use crate::transcript::Transcript;
use futures::future::BoxFuture;
use std::collections::VecDeque;
use tokio::sync::oneshot;

// `EventPipeline` is the internal stateful frame multiplexer: it drains
// `AgentEvent`s, reduces them into scoped `CoreEventEnvelope`s, and applies
// transcript side effects. `PhiEventEnvelope` is the stable SDK/RPC projection
// of those core frames.

const MAX_CONTINUATION_BYTES: usize = 16 * 1024;
const MAX_COMPLETION_CONTINUATIONS: usize = 8;

enum RoundWork {
    Context {
        reply: oneshot::Sender<Result<RequestContext, PhiFailure>>,
        target: crate::context::HistoryTarget,
        worker: tokio::task::JoinHandle<anyhow::Result<Option<PreparedCompaction>>>,
    },
    EndTurn {
        reply: oneshot::Sender<()>,
        worker: tokio::task::JoinHandle<crate::core::lifecycle::AgentTurnEndOutcome>,
    },
    Complete {
        budget: CompletionBudget,
        reply: oneshot::Sender<Result<CompletionDecision, PhiFailure>>,
        worker: tokio::task::JoinHandle<anyhow::Result<CompletionDecision>>,
    },
}

impl RoundWork {
    fn abort(&self) {
        match self {
            Self::Context { worker, .. } => worker.abort(),
            Self::Complete { worker, .. } => worker.abort(),
            Self::EndTurn { worker, .. } => worker.abort(),
        }
    }
}

/// Commits runner events and prepares canonical context and completion decisions.
pub(crate) struct EventPipeline {
    runner: AgentRunner,
    scope: CoreEventScope,
    pending_events: VecDeque<CoreEvent>,
    reducer: CoreRunState,
    compaction: Option<CompactionStage>,
    round_request: Option<RoundRequest>,
    round_work: Option<RoundWork>,
    compacted: bool,
    completion_continuations: usize,
    stop_requested: bool,
}

#[derive(Clone)]
struct CompactionStage {
    client: AnyClient,
    target: crate::context::HistoryTarget,
    cfg: RuntimeConfig,
    hooks: HookRegistry,
}

impl EventPipeline {
    pub(crate) fn new(runner: AgentRunner, scope: CoreEventScope) -> Self {
        Self {
            runner,
            scope,
            pending_events: VecDeque::new(),
            reducer: CoreRunState::default(),
            compaction: None,
            round_request: None,
            round_work: None,
            compacted: false,
            completion_continuations: 0,
            stop_requested: false,
        }
    }

    pub(crate) fn with_compaction(
        mut self,
        client: AnyClient,
        target: crate::context::HistoryTarget,
        cfg: RuntimeConfig,
        hooks: HookRegistry,
    ) -> Self {
        self.reducer = CoreRunState::new(&cfg);
        self.compaction = Some(CompactionStage {
            client,
            target,
            cfg,
            hooks,
        });
        self
    }

    pub(crate) fn commit_ready(
        &mut self,
        transcript: &mut Transcript,
    ) -> Vec<crate::core::turn::SubmissionId> {
        self.reduce_ready(transcript);
        self.pending_events
            .iter()
            .filter_map(|event| match event {
                CoreEvent::UserInputInjected { submission_id, .. } => submission_id.clone(),
                CoreEvent::SubmissionDiscarded { submission_id, .. } => Some(submission_id.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn abort(
        &mut self,
        transcript: &mut Transcript,
        provider: &str,
        model: &str,
    ) -> Vec<CoreEventEnvelope> {
        self.runner.event_rx.close();
        self.runner.abort();
        self.reduce_ready(transcript);
        self.cancel_round();
        self.commit_unfinished(transcript, provider, model, "aborted");
        envelope_core_events(&self.scope, self.pending_events.drain(..).collect())
    }

    pub(in crate::harness) fn turn_id(&self) -> Option<&compact_str::CompactString> {
        self.scope.turn_id.as_ref()
    }

    pub(in crate::harness) fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    pub(crate) fn matches_turn(&self, scope: &CoreEventScope) -> bool {
        scope.turn_id.is_some()
            && scope.turn_id == self.scope.turn_id
            && scope.session_id == self.scope.session_id
    }

    pub(crate) fn reserve_steer(
        &self,
    ) -> anyhow::Result<Option<crate::harness::agentic_loop::AgentSteerReservation>> {
        self.runner.reserve_steer()
    }

    pub(crate) async fn close_and_take_pending_steering(
        &self,
    ) -> anyhow::Result<Vec<crate::harness::agentic_loop::AgentSteerSubmission>> {
        self.runner.steering.close_and_take_pending().await
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.runner.event_rx.is_closed() && self.runner.event_rx.is_empty()
    }

    pub(crate) fn push_pending_event(&mut self, event: CoreEvent) {
        self.pending_events.push_back(event);
    }

    pub(crate) async fn drain(&mut self, transcript: &mut Transcript) -> Vec<CoreEventEnvelope> {
        self.reduce_ready(transcript);
        self.finish_pending()
    }

    #[cfg(test)]
    pub(crate) async fn wait(&mut self, transcript: &mut Transcript) -> Vec<CoreEventEnvelope> {
        if self.pending_events.is_empty()
            && let Some(event) = self.runner.event_rx.recv().await
        {
            self.reduce_event(transcript, event);
        }
        self.reduce_ready(transcript);
        self.finish_pending()
    }

    fn reduce_ready(&mut self, transcript: &mut Transcript) {
        while let Ok(event) = self.runner.event_rx.try_recv() {
            self.reduce_event(transcript, event);
        }
    }

    fn reduce_event(&mut self, transcript: &mut Transcript, event: AgentEvent) {
        // Keep committed frames in pipeline state before any post-reduction await.
        if matches!(&event, AgentEvent::Error(_)) {
            self.commit_unfinished(transcript, "", "", "error");
        }
        let mut events = reduce_agent_event(transcript, &mut self.reducer, event);
        for event in &mut events {
            if let CoreEvent::TurnFinished { compacted, .. } = event {
                *compacted = self.compacted;
            }
        }
        self.pending_events.extend(events);
    }

    fn commit_unfinished(
        &mut self,
        transcript: &mut Transcript,
        provider: &str,
        model: &str,
        stop_reason: &str,
    ) {
        let (provider, api, model) = self
            .compaction
            .as_ref()
            .map(|stage| {
                (
                    stage.target.provider.as_str(),
                    stage.target.api,
                    stage.target.model.as_str(),
                )
            })
            .unwrap_or((provider, None, model));
        if let Some(partial) = self.reducer.unfinished_assistant_event(
            self.scope.turn_id.clone().unwrap_or_default(),
            provider,
            api,
            model,
            stop_reason,
        ) {
            self.reduce_event(transcript, partial);
        }
    }

    fn finish_pending(&mut self) -> Vec<CoreEventEnvelope> {
        let events = self.pending_events.drain(..).collect::<Vec<_>>();
        envelope_core_events(&self.scope, events)
    }

    pub(in crate::harness) fn progress_round(
        &mut self,
        transcript: &mut Transcript,
        hooks: &HookRegistry,
        check_completion: impl FnOnce(
            CompletionEvent,
        )
            -> Option<BoxFuture<'static, anyhow::Result<CompletionDecision>>>,
    ) {
        if matches!(
            &self.round_work,
            Some(RoundWork::Complete {
                budget: CompletionBudget::Remaining,
                ..
            })
        ) && self.runner.steering.has_pending().unwrap_or(false)
            && let Some(RoundWork::Complete { reply, worker, .. }) = self.round_work.take()
        {
            worker.abort();
            let _ = reply.send(Ok(CompletionDecision::Finish));
        }
        let finished = match &self.round_work {
            Some(RoundWork::Context { worker, .. }) => worker.is_finished(),
            Some(RoundWork::Complete { worker, .. }) => worker.is_finished(),
            Some(RoundWork::EndTurn { worker, .. }) => worker.is_finished(),
            None => false,
        };
        if finished {
            let work = self.round_work.take().expect("finished round work");
            self.finish_round(work, transcript, hooks);
        }
        if self.round_work.is_some() {
            return;
        }
        let request = self
            .round_request
            .take()
            .or_else(|| self.runner.round_requests.as_mut()?.try_recv().ok());
        let Some(request) = request else {
            return;
        };
        // The request is sent after all events for the preceding round.
        self.reduce_ready(transcript);
        match request {
            RoundRequest::LoadTools(names, reply) => {
                if reply.is_closed() {
                    return;
                }
                let loaded = super::search::read_loaded_tools(transcript);
                let additions = names
                    .into_iter()
                    .filter(|name| !loaded.contains(name))
                    .collect::<std::collections::BTreeSet<_>>();
                if !additions.is_empty() {
                    transcript.append_entry(crate::transcript::TranscriptEntryKind::Event {
                        name: super::search::LOAD_EVENT.into(),
                        data: serde_json::json!({"tools": additions}),
                    });
                }
                let _ = reply.send(());
            }
            RoundRequest::CodemodeStore(reply) => {
                let _ = reply.send(crate::extension::codemode::read_store(transcript));
            }
            RoundRequest::CommitCodemodeStore(writes, reply) => {
                if !writes.set.is_empty() || !writes.delete.is_empty() {
                    transcript.append_entry(crate::transcript::TranscriptEntryKind::Event {
                        name: crate::extension::codemode::STORE_EVENT.into(),
                        data: serde_json::json!({ "set": writes.set, "delete": writes.delete }),
                    });
                }
                let _ = reply.send(());
            }
            RoundRequest::CommitInput(mut prompt, commit, reply) => {
                if reply.is_closed() {
                    return;
                }
                let committed = commit(transcript, &prompt);
                if committed {
                    hooks.record_notices(std::mem::take(&mut prompt.hook_notices));
                    self.pending_events.push_back(CoreEvent::UserInputInjected {
                        submission_id: prompt.submission_id,
                        prompt: prompt.prompt.into(),
                        source: prompt.source,
                    });
                }
                let _ = reply.send(committed);
            }
            RoundRequest::Context(trigger, target, reply) => {
                if reply.is_closed() {
                    return;
                }
                self.stop_requested = false;
                let transcript = transcript.clone();
                let compaction = self.compaction.clone();
                self.round_work = Some(RoundWork::Context {
                    reply,
                    target,
                    worker: tokio::spawn(async move {
                        match compaction {
                            Some(stage) => {
                                prepare_compaction(
                                    &stage.client,
                                    &stage.target.model,
                                    &stage.cfg,
                                    &transcript,
                                    &stage.hooks,
                                    trigger,
                                )
                                .await
                            }
                            None => Ok(None),
                        }
                    }),
                });
            }
            RoundRequest::EndTurn(turn, reply) => {
                if reply.is_closed() {
                    return;
                }
                if !hooks.has_agent_turn_end() {
                    let _ = reply.send(());
                    return;
                }
                let event = crate::core::lifecycle::AgentTurnEndEvent {
                    turn,
                    transcript: transcript.clone(),
                };
                let hooks = hooks.clone();
                self.round_work = Some(RoundWork::EndTurn {
                    reply,
                    worker: tokio::spawn(async move {
                        crate::hooks::emit_agent_turn_end(&hooks, event).await
                    }),
                });
            }
            RoundRequest::Complete(response, budget, reply) => {
                if reply.is_closed() {
                    return;
                }
                let event = CompletionEvent {
                    transcript: transcript.clone(),
                    response: response.clone(),
                    continuations: self.completion_continuations,
                };
                match check_completion(event) {
                    None => {
                        self.round_request = Some(RoundRequest::Complete(response, budget, reply))
                    }
                    Some(check) => {
                        self.round_work = Some(RoundWork::Complete {
                            budget,
                            reply,
                            worker: tokio::spawn(check),
                        });
                    }
                }
            }
        }
    }

    fn finish_round(&mut self, work: RoundWork, transcript: &mut Transcript, hooks: &HookRegistry) {
        use futures::FutureExt;
        match work {
            RoundWork::EndTurn { reply, worker } => {
                let result = worker
                    .now_or_never()
                    .expect("finished worker")
                    .map_err(anyhow::Error::from)
                    .and_then(|outcome| {
                        if outcome.context_edits.is_empty() {
                            return Ok(());
                        }
                        let mut candidate = transcript.clone();
                        for edit in outcome.context_edits {
                            candidate.append_context_edit(&edit.target_id, edit.replacement)?;
                        }
                        *transcript = candidate;
                        Ok(())
                    });
                if let Err(error) = result {
                    hooks.record_notices([crate::hooks::HookNotice::Warning {
                        message: format!("AgentTurnEnd context edits were discarded: {error}"),
                    }]);
                }
                let _ = reply.send(());
            }
            RoundWork::Context {
                reply,
                target,
                worker,
            } => {
                let result = worker
                    .now_or_never()
                    .expect("finished worker")
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                    .and_then(|compaction| {
                        if let Some(compaction) = &compaction {
                            transcript.compress(compaction.summary.clone(), &compaction.head)?;
                            self.compacted = true;
                        }
                        Ok(RequestContext {
                            messages: crate::context::convert_history_for_target(
                                transcript,
                                Some(&target),
                            )?,
                            loaded_tools: super::search::read_loaded_tools(transcript),
                            budget: self.compaction.as_ref().map(|stage| {
                                crate::context::RequestBudget::from_config(&stage.cfg)
                            }),
                            compaction,
                        })
                    })
                    .map_err(failure);
                let _ = reply.send(result);
            }
            RoundWork::Complete { reply, worker, .. } => {
                let result = worker
                    .now_or_never()
                    .expect("finished worker")
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                    .map_err(failure);
                if matches!(&result, Ok(CompletionDecision::Continue(_))) {
                    self.completion_continuations += 1;
                }
                self.stop_requested = matches!(&result, Ok(CompletionDecision::Stop));
                let _ = reply.send(result);
            }
        }
    }

    fn cancel_round(&mut self) {
        if let Some(work) = self.round_work.take() {
            work.abort();
        }
        self.round_request = None;
    }
}

pub(in crate::harness) async fn prepare_completion_feedback(
    event: &CompletionEvent,
    feedback: String,
    preparation: PromptPreparation,
) -> anyhow::Result<CoreTurnPreparation> {
    anyhow::ensure!(
        event.continuations < MAX_COMPLETION_CONTINUATIONS,
        "automatic completion continuation limit reached"
    );
    anyhow::ensure!(
        !feedback.trim().is_empty() && feedback.len() <= MAX_CONTINUATION_BYTES,
        "completion feedback must be nonempty and at most 16 KiB"
    );
    let prompt = TurnPrompt {
        prompt: feedback,
        source: UserPromptSubmitSource::Internal,
        tool_policy: Default::default(),
        injected_context: Vec::new(),
        images: Vec::new(),
    };
    Ok(preparation
        .prepare(prompt, UserPromptDispatch::Immediate)
        .await?)
}

fn failure(error: impl std::fmt::Display) -> PhiFailure {
    PhiFailure::new(PhiFailureKind::Internal, error.to_string(), None)
}

impl Drop for EventPipeline {
    fn drop(&mut self) {
        self.cancel_round();
    }
}

pub(in crate::harness) fn envelope_core_events(
    scope: &CoreEventScope,
    events: Vec<CoreEvent>,
) -> Vec<CoreEventEnvelope> {
    events
        .into_iter()
        .map(|event| CoreEventEnvelope {
            id: uuid::Uuid::new_v4().to_string().into(),
            emitted_at: chrono::Utc::now(),
            scope: scope.clone(),
            event,
        })
        .collect()
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::core::event::{PhiEvent, PhiEventEnvelope};
    use crate::core::turn::QueuedSubmissionDrainMode;
    use std::sync::Arc;

    #[tokio::test]
    async fn deferred_completion_stays_cancellable_without_an_orchestration_owner() {
        let (_sender, event_rx) = tokio::sync::mpsc::channel(1);
        let runner = AgentRunner {
            tool_policy: None,
            round_requests: None,
            event_rx,
            steering: Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                QueuedSubmissionDrainMode::OneAtATime,
            )),
            task: tokio::spawn(std::future::pending()),
        };
        let mut pipeline = EventPipeline::new(runner, CoreEventScope::default());
        let mut transcript = Transcript::new();
        let (reply, mut result) = oneshot::channel();
        pipeline.round_request = Some(RoundRequest::Complete(
            "candidate".into(),
            CompletionBudget::Remaining,
            reply,
        ));

        pipeline.progress_round(&mut transcript, &HookRegistry::new(), |event| {
            assert_eq!(event.response, "candidate");
            assert_eq!(event.continuations, 0);
            None
        });
        assert!(matches!(
            result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let (entered, started) = oneshot::channel();
        let (cancelled, cancellation) = oneshot::channel::<()>();
        pipeline.progress_round(&mut transcript, &HookRegistry::new(), |event| {
            assert_eq!(event.response, "candidate");
            Some(Box::pin(async move {
                let _ = entered.send(());
                let decision = std::future::pending().await;
                let _ = cancelled.send(());
                decision
            }))
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        pipeline.abort(&mut transcript, "provider", "model");
        assert!(result.await.is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), cancellation)
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn wait_delivers_pending_events_after_runner_closes() {
        let (tx, event_rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        let runner = AgentRunner {
            tool_policy: None,
            round_requests: None,
            event_rx,
            steering: std::sync::Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
            )),
            task: tokio::spawn(async {}),
        };
        let mut pipeline = EventPipeline::new(
            runner,
            CoreEventScope {
                turn_id: Some("turn-1".into()),
                ..CoreEventScope::default()
            },
        );
        pipeline.push_pending_event(CoreEvent::TurnStarted {
            submission_id: None,
            prompt: "test".into(),
            source: crate::core::lifecycle::UserPromptSubmitSource::Internal,
        });
        let mut transcript = crate::transcript::Transcript::new();

        let events = pipeline.wait(&mut transcript).await;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scope.turn_id(), Some("turn-1"));
        assert!(matches!(events[0].event, CoreEvent::TurnStarted { .. }));
    }

    #[test]
    fn public_envelope_preserves_core_observation_metadata() {
        let emitted_at = chrono::Utc::now();
        let envelope = CoreEventEnvelope {
            id: "event-1".into(),
            emitted_at,
            scope: CoreEventScope::default(),
            event: CoreEvent::ReasoningDelta {
                text: "test".into(),
            },
        };

        let public = PhiEventEnvelope::from(envelope);

        assert_eq!(public.id, "event-1");
        assert_eq!(public.emitted_at, emitted_at);
    }

    #[test]
    fn public_tool_call_exposes_provider_identity() {
        let core = CoreEventEnvelope {
            id: "event-1".into(),
            emitted_at: chrono::Utc::now(),
            scope: CoreEventScope::default(),
            event: CoreEvent::ToolCallStarted {
                id: "item-1".into(),
                provider_call_id: Some("call-1".into()),
                provider_item_id: Some("item-1".into()),
                name: "read".into(),
                args: serde_json::json!({}),
            },
        };
        let event = serde_json::to_value(PhiEventEnvelope::from(core).event).unwrap();
        assert_eq!(event["provider_call_id"], "call-1");
        assert_eq!(event["provider_item_id"], "item-1");
        assert!(event.get("call_id").is_none());
    }

    #[test]
    fn public_tool_event_preserves_typed_diff_details() {
        let core = CoreEventEnvelope {
            id: "event-1".into(),
            emitted_at: chrono::Utc::now(),
            scope: CoreEventScope::default(),
            event: CoreEvent::ToolCallFinished {
                id: "tool-1".into(),
                output: "edited".into(),
                is_error: false,
                details: Some(crate::core::event::ToolResultDetails {
                    diff: Some(crate::core::event::ToolDiffDetails {
                        hunks: vec![crate::core::event::ToolDiffHunk {
                            changes: vec![crate::core::event::ToolDiffChange {
                                kind: crate::core::event::ToolDiffChangeKind::Insert,
                                old_line: None,
                                new_line: Some(1),
                                text: "changed".to_string(),
                            }],
                        }],
                        patch: Some("@@".to_string()),
                        first_changed_line: Some(1),
                    }),
                    ..Default::default()
                }),
            },
        };

        let PhiEvent::ToolCallFinished { details, .. } = PhiEventEnvelope::from(core).event else {
            panic!("expected tool result event");
        };
        let diff = details.and_then(|details| details.diff).unwrap();
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].changes.len(), 1);
        let change = &diff.hunks[0].changes[0];
        assert_eq!(change.kind, crate::core::event::ToolDiffChangeKind::Insert);
        assert_eq!(change.new_line, Some(1));
        assert_eq!(change.text, "changed");
        assert_eq!(diff.patch.as_deref(), Some("@@"));
        assert_eq!(diff.first_changed_line, Some(1));
    }

    #[tokio::test]
    async fn unfinished_response_retains_generation_attribution() {
        use crate::provider::ProviderApi;
        use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};

        let client =
            crate::provider::create_client("openai", Some("test-key"), &Default::default())
                .unwrap();
        for termination in ["error", "aborted"] {
            let (_sender, event_rx) = tokio::sync::mpsc::channel(1);
            let runner = AgentRunner {
                tool_policy: None,
                round_requests: None,
                event_rx,
                steering: Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                    QueuedSubmissionDrainMode::OneAtATime,
                )),
                task: tokio::spawn(async {}),
            };
            let mut pipeline = EventPipeline::new(
                runner,
                CoreEventScope {
                    turn_id: Some("partial-turn".into()),
                    ..Default::default()
                },
            )
            .with_compaction(
                client.clone(),
                crate::context::HistoryTarget {
                    provider: "selected-provider".into(),
                    api: Some(ProviderApi::OpenAiResponses),
                    model: "selected-model".into(),
                },
                RuntimeConfig {
                    switch: Some("review".into()),
                    ..Default::default()
                },
                HookRegistry::new(),
            );
            let mut transcript = Transcript::new();
            for event in [
                AgentEvent::AssistantStarted {
                    id: "display-only".into(),
                },
                AgentEvent::AssistantDelta {
                    original: "retained content".into(),
                    display: "retained content".into(),
                },
            ] {
                pipeline.reduce_event(&mut transcript, event);
            }
            let events = if termination == "error" {
                pipeline.reduce_event(
                    &mut transcript,
                    AgentEvent::Error(PhiFailure::new(
                        PhiFailureKind::ProviderUnavailable,
                        "connection lost",
                        None,
                    )),
                );
                pipeline.finish_pending()
            } else {
                pipeline.abort(&mut transcript, "fallback-provider", "fallback-model")
            };
            assert_eq!(transcript.entries.len(), 1);
            let TranscriptEntryKind::Message {
                message:
                    AgentMessage::Assistant {
                        id,
                        turn_id,
                        provider,
                        api,
                        model,
                        switch,
                        content,
                        stop_reason,
                        ..
                    },
            } = &transcript.entries[0].kind
            else {
                panic!("missing partial response")
            };
            assert!(
                id.is_none(),
                "display identity must never become a provider ID"
            );
            assert_eq!(turn_id.as_deref(), Some("partial-turn"));
            assert_eq!(provider, "selected-provider");
            assert_eq!(*api, Some(ProviderApi::OpenAiResponses));
            assert_eq!(model, "selected-model");
            assert_eq!(switch.as_deref(), Some("review-1"));
            assert_eq!(stop_reason, termination);
            assert!(
                matches!(content.as_slice(), [ContentBlock::Text { text }] if text == &"retained content")
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(&event.event,
                        CoreEvent::AssistantFinished { id, .. } if id == "display-only"
                    ))
                    .count(),
                1
            );
            if termination == "error" {
                assert!(matches!(
                    events[events.len() - 2].event,
                    CoreEvent::AssistantFinished { .. }
                ));
                assert!(matches!(
                    events.last().unwrap().event,
                    CoreEvent::TurnFailed { .. }
                ));
            }
            assert!(
                pipeline
                    .abort(&mut transcript, "fallback-provider", "fallback-model")
                    .is_empty()
            );
            assert_eq!(transcript.entries.len(), 1);
        }
    }

    #[tokio::test]
    async fn owns_reduced_turn_finished_before_compaction() {
        let (_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let runner = AgentRunner {
            tool_policy: None,
            round_requests: None,
            event_rx,
            steering: std::sync::Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
            )),
            task: tokio::spawn(async {}),
        };
        let mut pipeline = EventPipeline::new(runner, CoreEventScope::default());
        let mut transcript = crate::transcript::Transcript::new();

        pipeline.reduce_event(
            &mut transcript,
            AgentEvent::RunFinished {
                response: "done".into(),
                usage: crate::transcript::Usage::default(),
            },
        );

        assert!(!pipeline.compacted);
        assert!(matches!(
            pipeline.pending_events.back(),
            Some(CoreEvent::TurnFinished { .. })
        ));
    }
}
