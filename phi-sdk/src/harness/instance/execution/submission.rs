use super::*;

pub(super) const MAX_QUEUED_SUBMISSIONS: usize = 64;

fn is_passive_message(
    message: &crate::harness::manager::message::AgentMessageEnvelope,
    automatic_turns: AutomaticTurnPolicy,
) -> bool {
    automatic_turns == AutomaticTurnPolicy::UserCancelled
        || matches!(&message.kind, crate::harness::manager::message::AgentMessageKind::TeammateExit { released_tasks } if released_tasks.is_empty())
}

struct MessageAdmission(Arc<crate::harness::manager::message::MessageRoute>);

impl MessageAdmission {
    fn commit(self, transcript: &mut Transcript, prompt: &PreparedTurnPrompt) -> bool {
        self.0
            .commit(|| {
                let message_index = transcript.entries.len();
                record_prepared_prompt(transcript, prompt, TranscriptPromptRole::Agent);
                let entry_id = transcript.entries[message_index].id.to_string();
                let message = &self.0.message;
                transcript.append_entry(crate::transcript::TranscriptEntryKind::Event {
                    name: crate::harness::manager::message::MESSAGE_DELIVERED_EVENT.into(),
                    data: serde_json::json!({ "messageId": message.id, "sender": message.sender,
                        "recipient": message.recipient, "kind": message.kind,
                        "messageEntryId": entry_id }),
                });
                ((), entry_id)
            })
            .is_some()
    }

    fn into_commit(self) -> crate::harness::agentic_loop::AgentInputCommit {
        Box::new(move |transcript, prompt| self.commit(transcript, prompt))
    }
}

impl Drop for MessageAdmission {
    fn drop(&mut self) {
        self.0.release_admission();
    }
}

pub(super) fn start_prepared_turn_with_role(
    turn_prompt: PreparedTurnPrompt,
    session: &mut Transcript,
    runtime: &AnyRuntime,
    hooks: HookRegistry,
    transcript_role: TranscriptPromptRole,
) -> TurnStart {
    record_prepared_prompt(session, &turn_prompt, transcript_role);
    let turn_id: CompactString = uuid::Uuid::new_v4().to_string().into();
    let scope = crate::core::event::CoreEventScope {
        session_id: Some(session.id.clone()),
        turn_id: Some(turn_id.clone()),
        ..crate::core::event::CoreEventScope::default()
    };
    TurnStart {
        submission_id: turn_prompt.submission_id,
        turn_id,
        runner: runtime
            .clone()
            .spawn_runner(hooks, scope, turn_prompt.tool_policy),
        prompt: turn_prompt.prompt,
    }
}

pub(super) fn record_prepared_prompt(
    session: &mut Transcript,
    turn_prompt: &PreparedTurnPrompt,
    role: TranscriptPromptRole,
) {
    match role {
        TranscriptPromptRole::User => turn_prompt.commit(session),
        TranscriptPromptRole::Agent => {
            session.append_agent_message(&turn_prompt.prompt);
            turn_prompt.commit_contexts(session);
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhiTurnError {
    #[error("prompt preparation failed: {source}")]
    Preparation {
        #[source]
        source: PromptPreparationError,
    },
    #[error("agent is already running")]
    Busy,
    #[error("agent submission queue limit ({limit}) reached")]
    QueueFull { limit: usize },
    #[error("agent admission is closed")]
    Closing,
    #[error("prompt preparation was cancelled")]
    Cancelled,
    #[error("invalid turn tool policy: {source}")]
    InvalidToolPolicy {
        #[source]
        source: crate::tools::ToolRuleError,
    },
    #[error("directed delegation requires an idle master agent")]
    DirectedUnavailable,
    #[error("unknown directed Agent `{name}`")]
    UnknownDirectedAgent { name: String },
    #[error("directed delegation requires the Agent tool to be enabled")]
    DirectedToolUnavailable,
    #[error("invalid image at index {index}: {source}")]
    InvalidImage {
        index: usize,
        #[source]
        source: ImageInputError,
    },
    #[error("model {provider}/{model} does not support image input")]
    UnsupportedImageInput { provider: String, model: String },
    #[error("session state is unavailable: {source}")]
    Session {
        #[source]
        source: anyhow::Error,
    },
    #[error("turn dispatch is unavailable: {source}")]
    Dispatch {
        #[source]
        source: anyhow::Error,
    },
}

pub(crate) enum SubmissionIntent {
    Immediate,
    Directed(DirectedDelegation),
    Dispatch(SubmissionDispatch),
}

pub(crate) struct SubmissionPreparation {
    intent: SubmissionIntent,
    memory: MemorySubmission,
    preparation: BoxFuture<'static, Result<CoreTurnPreparation, PhiTurnError>>,
}

impl SubmissionPreparation {
    pub(crate) async fn ready(&mut self) -> Result<CoreTurnPreparation, PhiTurnError> {
        self.preparation.as_mut().await
    }
}

impl AgentInstanceState {
    fn can_start_submission(&self) -> bool {
        self.can_start_main() && self.pending_submissions.is_empty()
    }

    fn reserve_main_steer(
        &self,
    ) -> anyhow::Result<Option<crate::harness::agentic_loop::AgentSteerReservation>> {
        self.main_run
            .as_ref()
            .map(EventPipeline::reserve_steer)
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn prepare_input(
        &mut self,
        mut prompt: TurnPrompt,
        intent: SubmissionIntent,
    ) -> Result<SubmissionPreparation, PhiTurnError> {
        if let SubmissionIntent::Directed(constraint) = &intent {
            if self.delegated_child || !self.is_idle() {
                return Err(PhiTurnError::DirectedUnavailable);
            }
            if self.generation.agents.get(&constraint.agent).is_none() {
                return Err(PhiTurnError::UnknownDirectedAgent {
                    name: constraint.agent.clone(),
                });
            }
            if !self.generation.runtime.is_tool_available("Agent") {
                return Err(PhiTurnError::DirectedToolUnavailable);
            }
        }
        self.validate_turn_prompt(&prompt)?;
        let dispatch = if self.can_start_submission() {
            UserPromptDispatch::Immediate
        } else if let SubmissionIntent::Dispatch(dispatch) = &intent {
            if self.pending_submissions.len() >= MAX_QUEUED_SUBMISSIONS {
                return Err(PhiTurnError::QueueFull {
                    limit: MAX_QUEUED_SUBMISSIONS,
                });
            }
            (*dispatch).into()
        } else {
            return Err(PhiTurnError::Busy);
        };
        let mut preparation = self.prompt_preparation();
        let memory = self
            .generation
            .services
            .memory
            .prepare_submission(
                &self.generation.cfg,
                self.working_dir(),
                if self.delegated_child {
                    MemoryAgentKind::Delegated
                } else {
                    MemoryAgentKind::Primary
                },
                &mut prompt,
                &mut preparation.hooks,
            )
            .map_err(|source| PhiTurnError::Session { source })?;
        let mut finalization = self
            .orchestration
            .as_ref()
            .map(|scope| scope.finalization.subscribe());
        self.preparing_submission = true;
        Ok(SubmissionPreparation {
            intent,
            memory,
            preparation: Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = async {
                        if let Some(finalization) = &mut finalization {
                            let _ = finalization.wait_for(|status| !matches!(
                                status,
                                crate::harness::manager::AgentFinalizationStatus::Open
                            )).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => Err(PhiTurnError::Closing),
                    result = preparation.prepare(prompt, dispatch) => {
                        result.map_err(|source| PhiTurnError::Preparation { source })
                    }
                }
            }),
        })
    }

    pub(crate) fn finish_submission(
        &mut self,
        preparation: SubmissionPreparation,
        result: Result<CoreTurnPreparation, PhiTurnError>,
    ) -> Result<SubmissionDisposition, PhiTurnError> {
        self.preparing_submission = false;
        let mut prompt = match result? {
            CoreTurnPreparation::Ready(prompt) => prompt,
            CoreTurnPreparation::Handled { reason } => {
                return Ok(SubmissionDisposition::Handled { reason });
            }
        };
        let registration = self.orchestration.clone();
        let admission = registration
            .as_ref()
            .map(|scope| scope.finalization.borrow());
        if admission.as_ref().is_some_and(|status| {
            !matches!(
                **status,
                crate::harness::manager::AgentFinalizationStatus::Open
            )
        }) {
            return Err(PhiTurnError::Closing);
        }
        let notices = self.validate_prompt(&mut prompt)?;
        let memory = preparation.memory.dispatch();
        let disposition = if self.can_start_submission() {
            self.start_submission(prompt, memory, preparation.intent)?
        } else {
            let SubmissionIntent::Dispatch(dispatch) = preparation.intent else {
                return Err(PhiTurnError::Busy);
            };
            if self.pending_submissions.len() >= MAX_QUEUED_SUBMISSIONS {
                return Err(PhiTurnError::QueueFull {
                    limit: MAX_QUEUED_SUBMISSIONS,
                });
            }
            let submission_id = uuid::Uuid::new_v4().to_string();
            prompt.submission_id = Some(submission_id.clone());
            let text = prompt.prompt.clone();
            match dispatch {
                SubmissionDispatch::Steer => {
                    let reservation = self
                        .reserve_main_steer()
                        .map_err(|source| PhiTurnError::Dispatch { source })?;
                    let steer = crate::harness::agentic_loop::AgentSteerSubmission {
                        prompt,
                        before_dispatch: Box::new(move || memory.activate()),
                        commit: None,
                    };
                    if let Some(reservation) = reservation {
                        reservation
                            .commit(steer)
                            .map_err(|source| PhiTurnError::Dispatch { source })?;
                    } else {
                        self.queued_steer_submissions.push_back(steer);
                    }
                }
                SubmissionDispatch::FollowUp => {
                    self.follow_up_submissions.push_back(QueuedSubmission {
                        prompt,
                        memory,
                        message: None,
                    })
                }
            }
            self.pending_submissions.push_back(submission_id.clone());
            SubmissionDisposition::Queued {
                submission_id,
                dispatch,
                prompt: text,
            }
        };
        preparation.memory.accept();
        self.generation.hooks.record_notices(notices);
        Ok(disposition)
    }

    pub(crate) fn admit_manager_messages(&mut self) -> Result<(), PhiStreamError> {
        if self.preparing_submission {
            return Ok(());
        }
        loop {
            if self.pending_message_prompt.is_none() {
                let Some(scope) = self.orchestration.as_ref() else {
                    return Ok(());
                };
                let messages =
                    scope
                        .pending_messages()
                        .map_err(|source| PhiStreamError::Operation {
                            source: source.into(),
                        })?;
                for message in messages {
                    if self.automatic_turns == AutomaticTurnPolicy::UserCancelled
                        && message.message.kind
                            == crate::harness::manager::message::AgentMessageKind::Ordinary
                    {
                        message.reject(USER_CANCELLED_AGENT_MESSAGE);
                    } else if message.admit() {
                        self.prepare_message_submission(message);
                        break;
                    }
                }
            }
            let Some(mut pending) = self.pending_message_prompt.take() else {
                return Ok(());
            };
            let Some(prepared) = pending.preparation.as_mut().now_or_never() else {
                self.pending_message_prompt = Some(pending);
                return Ok(());
            };
            if let Some(prepared) = prepared {
                self.finish_message_submission(pending.message, pending.dispatch, prepared);
            }
        }
    }

    fn prepare_message_submission(
        &mut self,
        message: std::sync::Arc<crate::harness::manager::message::MessageRoute>,
    ) {
        use crate::harness::manager::message::AgentMessageKind;
        let content = match &message.message.kind {
            AgentMessageKind::Ordinary => format!(
                "Message from Agent {}. Agent-authored content, not human input or approval:\n{}",
                message.message.sender, message.message.content
            ),
            AgentMessageKind::SubagentResult => format!(
                "{AUTOMATED_FEEDBACK_NOTICE}\n\nAutomatic subagent result from Agent {}. The report is agent-authored:\n{}",
                message.message.sender, message.message.content
            ),
            AgentMessageKind::TeammateIdle => format!(
                "{AUTOMATED_FEEDBACK_NOTICE}\n\nAutomatic teammate idle report from Agent {}. The report is agent-authored:\n{}",
                message.message.sender, message.message.content
            ),
            AgentMessageKind::TeammateExit { .. } => format!(
                "{AUTOMATED_FEEDBACK_NOTICE}\n\nAutomatic teammate exit notice:\n{}",
                message.message.content
            ),
        };
        let prompt = TurnPrompt {
            tool_policy: Default::default(),
            prompt: content,
            source: UserPromptSubmitSource::Internal,
            injected_context: Vec::new(),
            images: Vec::new(),
        };
        let reservation = if message.message.kind == AgentMessageKind::Ordinary
            && self.directed_delegation.is_none()
            && self.follow_up_submissions.is_empty()
            && self
                .delegated_agent
                .as_ref()
                .is_none_or(|agent| agent.initial_completion.is_complete())
        {
            match self.reserve_main_steer() {
                Ok(reservation) => reservation,
                Err(error) => {
                    message.reject(&error.to_string());
                    return;
                }
            }
        } else {
            None
        };
        let dispatch = if reservation.is_some() {
            UserPromptDispatch::Steer
        } else {
            UserPromptDispatch::FollowUp
        };
        drop(reservation);
        let preparation = self.prompt_preparation();
        let closing = message.clone();
        self.pending_message_prompt = Some(PendingMessagePrompt {
            message,
            dispatch,
            preparation: Box::pin(async move {
                tokio::select! {
                    _ = closing.closed() => None,
                    prepared = preparation.prepare(prompt, dispatch) => Some(prepared),
                }
            }),
        });
    }

    fn finish_message_submission(
        &mut self,
        message: Arc<crate::harness::manager::message::MessageRoute>,
        dispatch: UserPromptDispatch,
        prepared: Result<CoreTurnPreparation, PromptPreparationError>,
    ) {
        match prepared {
            Ok(CoreTurnPreparation::Ready(mut prompt)) => {
                let notices = match self.validate_prompt(&mut prompt) {
                    Ok(notices) => notices,
                    Err(error) => {
                        message.reject(&error.to_string());
                        return;
                    }
                };
                if is_passive_message(&message.message, self.automatic_turns)
                    && self.pending_submissions.is_empty()
                {
                    if MessageAdmission(message).commit(&mut self.transcript, &prompt) {
                        self.generation.hooks.record_notices(notices);
                    }
                    return;
                }
                let submission_id = message.message.id.as_str().to_owned();
                prompt.submission_id = Some(submission_id.clone());
                let reservation = if dispatch == UserPromptDispatch::Steer {
                    match self.reserve_main_steer() {
                        Ok(reservation) => reservation,
                        Err(error) => {
                            message.reject(&error.to_string());
                            return;
                        }
                    }
                } else {
                    None
                };
                if let Some(reservation) = reservation {
                    let admission = MessageAdmission(message.clone());
                    if let Err(error) =
                        reservation.commit(crate::harness::agentic_loop::AgentSteerSubmission {
                            prompt,
                            before_dispatch: Box::new(|| Ok(())),
                            commit: Some(admission.into_commit()),
                        })
                    {
                        message.reject(&error.to_string());
                        return;
                    }
                } else {
                    self.follow_up_submissions.push_back(QueuedSubmission {
                        prompt,
                        memory: Default::default(),
                        message: Some(message),
                    });
                }
                self.pending_submissions.push_back(submission_id);
                self.generation.hooks.record_notices(notices);
            }
            Ok(CoreTurnPreparation::Handled { reason }) => message.reject(
                reason
                    .as_deref()
                    .unwrap_or("message handled without canonical commit"),
            ),
            Err(error) => message.reject(&error.to_string()),
        }
    }

    fn start_submission(
        &mut self,
        prompt: PreparedTurnPrompt,
        memory: MemoryDispatch,
        intent: SubmissionIntent,
    ) -> Result<SubmissionDisposition, PhiTurnError> {
        let source = prompt.source;
        memory
            .activate()
            .map_err(|source| PhiTurnError::Session { source })?;
        let disposition = if let SubmissionIntent::Directed(constraint) = intent {
            let admitted = Arc::new(OnceLock::new());
            let runtime = self
                .generation
                .runtime
                .clone()
                .turn_instruction(constraint.instruction())
                .directed_delegation(admitted.clone());
            self.transcript
                .effective_context()
                .map_err(|source| PhiTurnError::Dispatch { source })?;
            let start = start_prepared_turn_with_role(
                prompt,
                &mut self.transcript,
                &runtime,
                self.generation.hooks.clone(),
                TranscriptPromptRole::User,
            );
            self.directed_delegation = Some(ActiveDirectedDelegation {
                turn_id: start.turn_id.to_string(),
                constraint,
                admitted,
                attempted: false,
                instance_id: None,
            });
            self.activate_turn(start, source)
        } else {
            self.start_recorded_turn(prompt, TranscriptPromptRole::User)?
        };
        if !matches!(source, UserPromptSubmitSource::Internal) {
            self.automatic_turns = AutomaticTurnPolicy::Enabled;
        }
        Ok(disposition)
    }

    pub(super) fn dispatch_next_queued_turn(
        &mut self,
        events: &mut Vec<CoreEventEnvelope>,
    ) -> Result<(), PhiTurnError> {
        if self.preparing_submission || self.pending_message_prompt.is_some() {
            return Ok(());
        }
        // Publish initial completion before a queued follow-up can replace its outcome.
        if self
            .delegated_agent
            .as_ref()
            .is_some_and(|agent| !agent.initial_completion.is_complete())
        {
            return self.start_next_subagent_outcome();
        }
        if let Some(steer) = self.queued_steer_submissions.pop_front() {
            let submission_id = steer.prompt.submission_id.clone();
            let result = (steer.before_dispatch)()
                .map_err(|source| PhiTurnError::Dispatch { source })
                .and_then(|()| match steer.commit {
                    Some(commit) => self.start_committed_turn(steer.prompt, commit),
                    None => self
                        .start_recorded_turn(steer.prompt, TranscriptPromptRole::User)
                        .map(|_| ()),
                });
            if result.is_err() {
                self.discard_failed_submission(submission_id, events);
            }
            result?;
            self.seed_queued_steering(events)?;
        } else if let Some(follow_up) = self.follow_up_submissions.pop_front() {
            let submission_id = follow_up.prompt.submission_id.clone();
            let message = follow_up.message.clone();
            let result = (|| {
                follow_up
                    .memory
                    .activate()
                    .map_err(|source| PhiTurnError::Session { source })?;
                if let Some(message) = follow_up.message {
                    self.start_message_turn(follow_up.prompt, message)
                } else {
                    self.start_recorded_turn(follow_up.prompt, TranscriptPromptRole::User)
                        .map(|_| ())
                }
            })();
            if result.is_err() {
                if let Some(message) = message {
                    message.release_admission();
                }
                self.discard_failed_submission(submission_id, events);
            }
            result?;
        } else if !self.start_next_bash_outcome()? && !self.start_next_workflow_feedback()? {
            self.start_next_subagent_outcome()?;
        }
        Ok(())
    }

    fn discard_failed_submission(
        &mut self,
        submission_id: Option<crate::core::turn::SubmissionId>,
        events: &mut Vec<CoreEventEnvelope>,
    ) {
        let Some(submission_id) = submission_id else {
            return;
        };
        if !self.pending_submissions.contains(&submission_id) {
            return;
        }
        self.pending_submissions.retain(|id| id != &submission_id);
        events.extend(envelope_core_events(
            &CoreEventScope {
                session_id: Some(self.transcript.id.clone()),
                ..Default::default()
            },
            vec![CoreEvent::SubmissionDiscarded {
                submission_id,
                reason: crate::core::turn::SubmissionDiscardReason::DispatchFailed,
            }],
        ));
    }

    fn start_message_turn(
        &mut self,
        prompt: PreparedTurnPrompt,
        message: std::sync::Arc<crate::harness::manager::message::MessageRoute>,
    ) -> Result<(), PhiTurnError> {
        if is_passive_message(&message.message, self.automatic_turns) {
            let submission_id = prompt.submission_id.clone();
            MessageAdmission(message).commit(&mut self.transcript, &prompt);
            if let Some(id) = submission_id {
                self.pending_submissions.retain(|pending| pending != &id);
            }
            Ok(())
        } else {
            self.start_committed_turn(prompt, MessageAdmission(message).into_commit())
        }
    }

    fn start_committed_turn(
        &mut self,
        prompt: PreparedTurnPrompt,
        commit: crate::harness::agentic_loop::AgentInputCommit,
    ) -> Result<(), PhiTurnError> {
        self.transcript
            .effective_context()
            .map_err(|source| PhiTurnError::Dispatch { source })?;
        let submission_id = prompt.submission_id.clone();
        if let Some(agent) = &self.delegated_agent {
            let mut scope = agent.scope.clone();
            scope.turn_id = Some(uuid::Uuid::new_v4().to_string().into());
            scope.parent_tool_call_id = None;
            let pipeline = self
                .spawn_agent_run(&agent.definition, &prompt, scope.clone())
                .map_err(|source| PhiTurnError::Dispatch { source })?;
            if commit(&mut self.transcript, &prompt) {
                self.activate_specialized_run(scope, pipeline);
            }
        } else {
            let turn_id: CompactString = uuid::Uuid::new_v4().to_string().into();
            let runner = self.generation.runtime.clone().spawn_runner(
                self.generation.hooks.clone(),
                CoreEventScope {
                    session_id: Some(self.transcript.id.clone()),
                    turn_id: Some(turn_id.clone()),
                    ..Default::default()
                },
                prompt.tool_policy.clone(),
            );
            if commit(&mut self.transcript, &prompt) {
                self.activate_turn(
                    TurnStart {
                        submission_id: prompt.submission_id,
                        turn_id,
                        runner,
                        prompt: prompt.prompt,
                    },
                    prompt.source,
                );
            }
        }
        if let Some(id) = submission_id {
            self.pending_submissions.retain(|pending| pending != &id);
        }
        Ok(())
    }

    pub(super) fn seed_queued_steering(
        &mut self,
        events: &mut Vec<CoreEventEnvelope>,
    ) -> Result<(), PhiTurnError> {
        while !self.queued_steer_submissions.is_empty() {
            let Some(reservation) = self
                .reserve_main_steer()
                .map_err(|source| PhiTurnError::Dispatch { source })?
            else {
                break;
            };
            let steer = self
                .queued_steer_submissions
                .pop_front()
                .expect("queued steer checked above");
            let submission_id = steer.prompt.submission_id.clone();
            if let Err(source) = reservation.commit(steer) {
                self.discard_failed_submission(submission_id, events);
                return Err(PhiTurnError::Dispatch { source });
            }
        }
        Ok(())
    }

    pub(crate) fn pending_submission_ids(&self) -> Vec<crate::core::turn::SubmissionId> {
        self.pending_submissions.iter().cloned().collect()
    }

    pub(super) fn retire_submission_events(&mut self, events: &[CoreEventEnvelope]) {
        for envelope in events {
            match &envelope.event {
                CoreEvent::UserInputInjected {
                    submission_id: Some(id),
                    ..
                }
                | CoreEvent::SubmissionDiscarded {
                    submission_id: id, ..
                } => {
                    self.pending_submissions.retain(|pending| pending != id);
                }
                _ => {}
            }
        }
    }

    pub(super) fn validate_turn_prompt(
        &self,
        turn_prompt: &TurnPrompt,
    ) -> Result<(), PhiTurnError> {
        if self.automatic_turns == AutomaticTurnPolicy::UserCancelled
            && matches!(turn_prompt.source, UserPromptSubmitSource::Internal)
        {
            return Err(PhiTurnError::Dispatch {
                source: anyhow::anyhow!(USER_CANCELLED_AGENT_MESSAGE),
            });
        }
        self.generation
            .runtime
            .validate_turn_policy(&turn_prompt.tool_policy)
            .map_err(|source| PhiTurnError::InvalidToolPolicy { source })?;
        for (index, image) in turn_prompt.images.iter().enumerate() {
            image
                .validate()
                .map_err(|source| PhiTurnError::InvalidImage { index, source })?;
        }
        if !turn_prompt.images.is_empty()
            && !self
                .generation
                .descriptor
                .input
                .contains(&crate::models::ModelInput::Image)
        {
            return Err(PhiTurnError::UnsupportedImageInput {
                provider: self.generation.descriptor.provider.clone(),
                model: self.generation.descriptor.id.clone(),
            });
        }
        Ok(())
    }
}

impl From<SubmissionDispatch> for UserPromptDispatch {
    fn from(dispatch: SubmissionDispatch) -> Self {
        match dispatch {
            SubmissionDispatch::Steer => Self::Steer,
            SubmissionDispatch::FollowUp => Self::FollowUp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{test_client, test_services, turn_prompt};
    use super::*;
    use crate::extension::memory::TapeKeywordConfig;
    use crate::factory::Phi;

    async fn keyword_context(state: &mut AgentInstanceState, input: TurnPrompt) -> Option<String> {
        let mut preparation = state
            .prepare_input(
                input,
                SubmissionIntent::Dispatch(SubmissionDispatch::FollowUp),
            )
            .unwrap();
        let result = preparation.ready().await.unwrap();
        drop(preparation);
        state.preparing_submission = false;
        let CoreTurnPreparation::Ready(mut prompt) = result else {
            return None;
        };
        state.validate_prompt(&mut prompt).unwrap();
        prompt
            .contexts
            .into_iter()
            .find(|context| {
                context.source == crate::extension::memory::MemoryContextKind::KeywordIntro.source()
            })
            .map(|context| context.content)
    }

    #[tokio::test]
    async fn peer_turn_acceptance_preserves_retained_lifecycle() {
        for committed in [false, true] {
            let mut state = test_client().await;
            let completion = crate::harness::instance::child::InitialCompletion::default();
            completion.publish(Ok("initial result".into())).unwrap();
            state.delegated_agent = Some(DelegatedAgent {
                definition: AgentDefinition::new("reviewer", "review code", "Review."),
                scope: CoreEventScope {
                    session_id: Some(state.transcript.id.clone()),
                    turn_id: Some("initial-turn".into()),
                    parent_tool_call_id: Some("initial-call".into()),
                    ..Default::default()
                },
                initial_completion: completion,
                outcome: Some(Ok("initial result".into())),
            });
            state.set_bash_owner_mode(DelegationMode::Foreground);
            let original = serde_json::to_value(&state.transcript).unwrap();
            state
                .start_committed_turn(
                    PreparedTurnPrompt {
                        hook_notices: Vec::new(),
                        submission_id: None,
                        prompt: "peer work".into(),
                        source: UserPromptSubmitSource::Internal,
                        contexts: Vec::new(),
                        user_content: Vec::new(),
                        tool_policy: Default::default(),
                    },
                    Box::new(move |transcript, prompt| {
                        if committed {
                            transcript.append_agent_message(&prompt.prompt);
                        }
                        committed
                    }),
                )
                .unwrap();
            let agent = state.delegated_agent.as_ref().unwrap();
            assert_eq!(state.can_start_main(), !committed);
            assert_eq!(
                agent.initial_completion.get().unwrap().outcome.as_deref(),
                Ok("initial result")
            );
            if committed {
                assert_eq!(state.bash_owner_mode, DelegationMode::Background);
                assert!(agent.scope.parent_tool_call_id.is_none());
                assert_eq!(
                    agent.scope.turn_id.as_deref(),
                    state.active_turn_id().as_deref()
                );
                assert!(agent.outcome.is_none());
            } else {
                assert_eq!(state.bash_owner_mode, DelegationMode::Foreground);
                assert_eq!(agent.scope.turn_id.as_deref(), Some("initial-turn"));
                assert_eq!(serde_json::to_value(&state.transcript).unwrap(), original);
                assert!(agent.outcome.is_some());
            }
            state.abort();
        }
    }

    #[tokio::test]
    async fn hook_tool_policy_is_validated_before_admission() {
        for source in [
            UserPromptSubmitSource::Sdk,
            UserPromptSubmitSource::Internal,
            UserPromptSubmitSource::Extension,
        ] {
            let mut state = test_client().await;
            state
                .generation
                .hooks
                .register_hook(crate::hooks::Hook::user_prompt_submit(|event| async move {
                    Ok(
                        crate::core::lifecycle::UserPromptSubmitOutcome::ContinueWithPolicy {
                            notices: Vec::new(),
                            prompt: event.prompt,
                            injected_context: Vec::new(),
                            tool_policy: crate::turn::TurnToolPolicy {
                                allowances: Vec::new(),
                                removals: vec!["MCP".into()],
                            },
                        },
                    )
                }));
            let mut prompt = turn_prompt("input");
            prompt.source = source;
            assert!(matches!(
                state
                    .submit_input(prompt, SubmissionIntent::Immediate)
                    .await,
                Err(PhiTurnError::InvalidToolPolicy { .. })
            ));
            assert!(!state.transcript.has_user_message());
            assert!(state.pending_submissions.is_empty());
            assert!(state.main_run.is_none());
        }
    }

    #[tokio::test]
    async fn tape_keyword_gate_uses_instance_working_dir() {
        let root = std::env::temp_dir().join(format!("phi-keyword-scope-{}", uuid::Uuid::new_v4()));
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&child)
                .status()
                .unwrap()
                .success()
        );
        let mut state = test_client().await;
        state.generation.cfg.extensions.memory = Some(crate::extension::memory::MemoryConfig {
            tape: Some(crate::extension::memory::TapeConfig {
                enabled: Some(true),
                only_git: Some(true),
                anchor: Some(crate::extension::memory::TapeAnchorConfig {
                    keywords: Some(crate::extension::memory::TapeKeywordConfig {
                        global: vec!["remember".to_string()],
                        project: Vec::new(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        state.generation.services.working_dir = root.clone();
        assert!(
            keyword_context(&mut state, turn_prompt("remember this"))
                .await
                .is_none()
        );
        state.generation.services.working_dir = child;
        assert!(
            keyword_context(&mut state, turn_prompt("remember this"))
                .await
                .is_some()
        );
        state
            .generation
            .cfg
            .extensions
            .disabled
            .insert(crate::extension::BuiltinExtension::Memory);
        assert!(
            keyword_context(&mut state, turn_prompt("remember this"))
                .await
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn delegated_child_rejects_keyword_context() {
        let root = std::env::temp_dir().join(format!("phi-child-memory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let phi = Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .runtime_config(RuntimeConfig {
                extensions: crate::extension::ExtensionConfig {
                    memory: Some(crate::extension::memory::MemoryConfig {
                        tape: Some(crate::extension::memory::TapeConfig {
                            enabled: Some(true),
                            only_git: Some(false),
                            anchor: Some(crate::extension::memory::TapeAnchorConfig {
                                keywords: Some(crate::extension::memory::TapeKeywordConfig {
                                    global: vec!["remember".to_string()],
                                    project: Vec::new(),
                                }),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        memory_dir: Some(crate::extension::memory::MemoryDirSettings {
                            local_path: Some(root.join("memory").display().to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            })
            .no_tools(true)
            .build()
            .unwrap()
            .with_runtime_services(test_services());
        let mut state = phi
            .agent_instance_builder()
            .working_dir(root.clone())
            .delegated_child()
            .build()
            .await
            .unwrap()
            .into_state();
        let mut input = turn_prompt("remember this delegated task");
        input
            .injected_context
            .push(crate::context::InjectedContext {
                source: crate::extension::memory::MemoryContextKind::KeywordIntro.source(),
                content: "untrusted keyword instruction".into(),
            });

        assert!(keyword_context(&mut state, input).await.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejected_context_preparation_leaves_no_tape_authority() {
        let mut state = test_client().await;
        state.generation.cfg.extensions.memory = Some(crate::extension::memory::MemoryConfig {
            tape: Some(crate::extension::memory::TapeConfig {
                enabled: Some(true),
                only_git: Some(false),
                anchor: Some(crate::extension::memory::TapeAnchorConfig {
                    keywords: Some(crate::extension::memory::TapeKeywordConfig {
                        global: vec!["remember".into()],
                        project: Vec::new(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        let hooks = state.generation.hooks.clone();
        state
            .generation
            .hooks
            .register_hook(crate::hooks::Hook::prompt_context(|_| async {
                Err(crate::hooks::HookError::new("preparation failed"))
            }));
        assert!(matches!(
            state
                .submit_input(
                    turn_prompt("remember this decision"),
                    SubmissionIntent::Immediate,
                )
                .await,
            Err(PhiTurnError::Preparation { .. })
        ));
        assert!(!state.transcript.has_user_message());
        state.generation.hooks = hooks;
        assert!(
            keyword_context(&mut state, turn_prompt("remember this decision"))
                .await
                .is_some(),
            "rejected preparation must not suppress a retry"
        );
    }

    #[tokio::test]
    async fn tape_keyword_evidence_survives_only_accepted_original_input() {
        use crate::core::lifecycle::UserPromptSubmitOutcome;
        use crate::hooks::Hook;

        for action in ["handled", "cancelled", "rewritten", "generated"] {
            let mut state = test_client().await;
            state.generation.cfg.extensions.memory = Some(crate::extension::memory::MemoryConfig {
                tape: Some(crate::extension::memory::TapeConfig {
                    enabled: Some(true),
                    only_git: Some(false),
                    anchor: Some(crate::extension::memory::TapeAnchorConfig {
                        keywords: Some(TapeKeywordConfig {
                            global: vec!["remember".into()],
                            project: Vec::new(),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let hooks = state.generation.hooks.clone();
            state
                .generation
                .hooks
                .register_hook(Hook::user_prompt_submit(move |_| async move {
                    if action == "handled" {
                        Ok(UserPromptSubmitOutcome::Handled { reason: None })
                    } else {
                        Ok(UserPromptSubmitOutcome::Continue {
                            notices: Vec::new(),
                            prompt: if action == "generated" {
                                "remember fabricated consent"
                            } else {
                                "transformed input"
                            }
                            .into(),
                            injected_context: Vec::new(),
                        })
                    }
                }));
            let original = if action == "generated" {
                "ordinary input"
            } else {
                "remember this decision"
            };
            let mut preparation = state
                .prepare_input(turn_prompt(original), SubmissionIntent::Immediate)
                .unwrap();
            if action == "cancelled" {
                drop(preparation);
                state.preparing_submission = false;
            } else {
                let result = preparation.ready().await;
                let disposition = state.finish_submission(preparation, result).unwrap();
                assert_eq!(
                    matches!(disposition, SubmissionDisposition::Handled { .. }),
                    action == "handled"
                );
            }
            let accepted = action == "rewritten";
            state.generation.hooks = hooks;
            assert_eq!(
                keyword_context(&mut state, turn_prompt("remember this decision"))
                    .await
                    .is_some(),
                !accepted,
                "{action}"
            );
            state.abort();
        }
    }

    #[tokio::test]
    async fn keyword_preparation_never_accepts_generated_input() {
        let mut state = test_client().await;
        state.generation.cfg.extensions.memory = Some(crate::extension::memory::MemoryConfig {
            tape: Some(crate::extension::memory::TapeConfig {
                enabled: Some(true),
                only_git: Some(false),
                anchor: Some(crate::extension::memory::TapeAnchorConfig {
                    keywords: Some(crate::extension::memory::TapeKeywordConfig {
                        global: vec!["remember".into()],
                        project: Vec::new(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        for source in [
            UserPromptSubmitSource::Internal,
            UserPromptSubmitSource::Extension,
        ] {
            let mut prompt = turn_prompt("remember this generated text");
            prompt.source = source;
            assert!(keyword_context(&mut state, prompt).await.is_none());
        }
    }

    #[tokio::test]
    async fn instance_input_prepares_configured_tape_keyword_anchor() {
        let mut state = test_client().await;
        state.generation.cfg.extensions.memory = Some(crate::extension::memory::MemoryConfig {
            tape: Some(crate::extension::memory::TapeConfig {
                enabled: Some(true),
                only_git: Some(false),
                anchor: Some(crate::extension::memory::TapeAnchorConfig {
                    keywords: Some(crate::extension::memory::TapeKeywordConfig {
                        global: vec!["remember".to_string()],
                        project: Vec::new(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut generated = turn_prompt("remember this plugin instruction");
        generated.source = UserPromptSubmitSource::Extension;
        assert!(keyword_context(&mut state, generated).await.is_none());
        let context = keyword_context(&mut state, turn_prompt("remember this decision"))
            .await
            .expect("expected keyword context");
        assert!(context.contains("tape_anchor"));
        assert!(!state.transcript.has_user_message());
        assert!(
            keyword_context(&mut state, turn_prompt("remember this decision"))
                .await
                .is_some()
        );
    }
}
