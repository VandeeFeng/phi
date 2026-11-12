use super::*;
use crate::agent::tools::AgentToolOutput;
use crate::extension::delegated_tool_denials;

use crate::core::event::{PhiFailure, PhiFailureKind};
use crate::harness::agentic_loop::AgenticLoop;
use crate::harness::manager::policy::MAX_CHILD_WORKERS_PER_PARENT;
use crate::harness::manager::preparation::{
    DelegationMode, SubagentCompletion, prepare_working_dir,
};
pub(super) const RECENT_CHILD_LIMIT: usize = 64;

// Instance-local delegation, child observations and canonical outcome commits; Manager owns coordination.

enum CreationReply {
    Direct(tokio::sync::oneshot::Sender<Result<AgentCreation, PhiSubagentError>>),
    Tool(tokio::sync::oneshot::Sender<Result<AgentToolOutput, String>>),
}

impl CreationReply {
    fn is_closed(&self) -> bool {
        match self {
            Self::Direct(reply) => reply.is_closed(),
            Self::Tool(reply) => reply.is_closed(),
        }
    }

    fn reject(self, error: PhiSubagentError) {
        match self {
            Self::Direct(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Tool(reply) => {
                let _ = reply.send(Err(error.to_string()));
            }
        }
    }
}

pub(super) struct PendingCreation {
    request: AgentSpawnRequest,
    scope: CoreEventScope,
    reply: CreationReply,
    directory: tokio::task::JoinHandle<Result<std::path::PathBuf, PhiSubagentError>>,
}

impl PendingCreation {
    fn new(
        request: AgentSpawnRequest,
        scope: CoreEventScope,
        reply: CreationReply,
        parent: std::path::PathBuf,
    ) -> Self {
        let directory = tokio::spawn(prepare_working_dir(parent, request.working_dir.clone()));
        Self {
            request,
            scope,
            reply,
            directory,
        }
    }

    fn is_ready(&self) -> bool {
        self.directory.is_finished() || self.reply.is_closed()
    }

    fn reject(self, error: PhiSubagentError) {
        self.directory.abort();
        self.reply.reject(error);
    }
}

impl DirectedDelegation {
    pub(super) fn instruction(&self) -> String {
        let mode = self.mode.unwrap_or(DelegationMode::Background);
        let binding = serde_json::json!({ "role": self.agent, "mode": mode });
        let delivery = match mode {
            DelegationMode::Foreground => {
                "Wait for the child's result in the Agent tool response, then summarize it for the user. Do not invoke any more tools. Runtime policy may still require background execution; if a launch receipt is returned instead, this turn ends and the result arrives asynchronously."
            }
            DelegationMode::Background => {
                "After admission succeeds, this turn ends and the result arrives asynchronously."
            }
        };
        format!(
            "[Directed delegation for this turn only]\n\
             The user's @ prompt requests delegation, not a direct answer from you. \
             Invoke the Agent tool exactly once with these fixed arguments: {binding}. \
             The mode is fixed; do not change it based on the task text. \
             Only -f/-b immediately after the @agent name is mode syntax; everything after it is task text. \
             Formulate the child's prompt from the user's task and choose an existing working_dir from their intent. \
             Omit working_dir to use your current working directory. Clarify ambiguous directory/worktree intent rather than guessing. \
             Do not treat the @ prefix as a tool name or a shell command. \
             A response without the matching Agent call is a delegation failure. \
             Delegate using the tool; do not perform the child's task yourself. \
             {delivery} \
             On any delegation error, stop and report it; do not retry, query child history, \
             use other tools, or fall back to another directory."
        )
    }
}

impl AgentInstanceState {
    pub(super) fn resolve_directed_outcome(&mut self, events: &mut Vec<CoreEventEnvelope>) {
        let Some(directed) = self.directed_delegation.as_ref() else {
            return;
        };
        let terminal = events.iter().position(|envelope| {
            envelope.scope.turn_id.as_deref() == Some(directed.turn_id.as_str())
                && matches!(
                    envelope.event,
                    CoreEvent::TurnFinished { .. } | CoreEvent::TurnFailed { .. }
                )
        });
        let closed = self.main_run.as_ref().is_some_and(|run| run.is_closed());
        if terminal.is_none() && !closed {
            return;
        }
        if directed.admitted.get().is_none() {
            let mut message = format!(
                "directed prompt did not delegate to Agent `{}`",
                directed.constraint.agent
            );
            if let Some(CoreEvent::TurnFailed { failure }) =
                terminal.map(|index| &events[index].event)
            {
                message.push_str(&format!(": {}", failure.message));
            }
            self.transcript.append_entry(crate::transcript::TranscriptEntryKind::Event {
                name: "phi_sdk.directed_delegation_failed".into(),
                data: serde_json::json!({ "turn_id": directed.turn_id, "agent": directed.constraint.agent }),
            });
            let failure = CoreEvent::TurnFailed {
                failure: crate::core::event::PhiFailure::new(
                    crate::core::event::PhiFailureKind::Delegation,
                    message,
                    None,
                ),
            };
            if let Some(index) = terminal {
                events[index].event = failure;
            } else {
                events.push(CoreEventEnvelope {
                    id: uuid::Uuid::new_v4().to_string().into(),
                    emitted_at: chrono::Utc::now(),
                    scope: CoreEventScope {
                        session_id: Some(self.transcript.id.clone()),
                        turn_id: Some(directed.turn_id.clone().into()),
                        ..Default::default()
                    },
                    event: failure,
                });
            }
        }
        self.directed_delegation = None;
    }

    pub(crate) fn spawn_subagent(
        &mut self,
        request: AgentSpawnRequest,
        reply: tokio::sync::oneshot::Sender<Result<AgentCreation, PhiSubagentError>>,
    ) {
        let scope = CoreEventScope {
            session_id: Some(self.transcript.id.clone()),
            agent_id: Some(self.transcript.id.clone()),
            ..CoreEventScope::default()
        };
        self.prepare_creation(request, scope, CreationReply::Direct(reply));
    }

    fn prepare_creation(
        &mut self,
        request: AgentSpawnRequest,
        scope: CoreEventScope,
        reply: CreationReply,
    ) {
        if reply.is_closed() {
            return;
        }
        if let Err(error) = self.validate_creation_request(&request, &scope) {
            reply.reject(error);
            return;
        }
        if self.pending_creations.len() >= MAX_CHILD_WORKERS_PER_PARENT {
            reply.reject(PhiSubagentError::ChildLimit);
            return;
        }
        self.pending_creations.push_back(PendingCreation::new(
            request,
            scope,
            reply,
            self.working_dir().to_path_buf(),
        ));
    }

    pub(super) async fn progress_pending_creations(&mut self) {
        while self
            .pending_creations
            .front()
            .is_some_and(PendingCreation::is_ready)
        {
            let pending = self
                .pending_creations
                .pop_front()
                .expect("checked pending request");
            if pending.reply.is_closed() {
                pending.reject(interrupted_creation());
                continue;
            }
            self.finish_creation(pending).await;
        }
    }

    async fn finish_creation(&mut self, pending: PendingCreation) {
        let tool_origin = matches!(pending.reply, CreationReply::Tool(_));
        let turn_id = pending.scope.turn_id.clone();
        let directory = pending
            .directory
            .await
            .unwrap_or_else(|_| Err(interrupted_creation()));
        match directory.and_then(|directory| {
            if tool_origin && self.validate_tool_scope(&pending.scope).is_err() {
                return Err(interrupted_creation());
            }
            self.spawn_managed_subagent(pending.request, pending.scope, tool_origin, directory)
        }) {
            Ok(started) => self.reply_to_creation(pending.reply, started, turn_id.as_deref()),
            Err(error) => pending.reply.reject(error),
        }
    }

    fn reply_to_creation(
        &mut self,
        reply: CreationReply,
        started: AgentCreation,
        turn_id: Option<&str>,
    ) {
        match reply {
            CreationReply::Direct(reply) => {
                let _ = reply.send(Ok(started));
            }
            CreationReply::Tool(reply) => {
                if let Some(active) = self.directed_delegation.as_mut()
                    && turn_id == Some(active.turn_id.as_str())
                {
                    active.instance_id = Some(started.instance_id().clone());
                }
                self.agent_tool_replies
                    .insert(started.instance_id().clone(), reply);
            }
        }
    }

    pub(super) fn cancel_pending_creations(&mut self) {
        for pending in self.pending_creations.drain(..) {
            if matches!(pending.reply, CreationReply::Tool(_))
                && let Some(tool) = pending.scope.parent_tool_call_id.as_ref()
            {
                self.cancelled_admission_tools
                    .push((tool.to_string(), crate::CancellationReason::Interrupted));
            }
            pending.reject(interrupted_creation());
        }
    }
}

fn interrupted_creation() -> PhiSubagentError {
    PhiSubagentError::Management {
        source: Box::new(crate::harness::manager::AgentManagerError::AdmissionInterrupted),
    }
}

pub(super) struct AdmittedSubagentOutcome {
    pub prompt: String,
}

impl AgentInstanceState {
    pub(crate) fn has_child_workers(&self) -> bool {
        !self.child_workers.is_empty()
    }

    pub(crate) fn definition_name(&self) -> Option<&str> {
        self.delegated_agent
            .as_ref()
            .map(|agent| agent.definition.name())
    }

    pub(super) fn spawn_agent_run(
        &self,
        agent: &AgentDefinition,
        prompt: &PreparedTurnPrompt,
        scope: CoreEventScope,
    ) -> anyhow::Result<EventPipeline> {
        if scope.session_id.is_none() {
            anyhow::bail!("agent run scope requires a session id");
        }
        let runner = self.generation.runtime.clone().spawn_agent_runner(
            agent,
            self.generation.hooks.clone(),
            &self.generation.client,
            &self.generation.catalog,
            &self.generation.agent_options,
            &self.generation.cfg,
            &self.generation.services.working_dir,
            scope.clone(),
            prompt.tool_policy.clone(),
        )?;
        let model_name = agent
            .model_override()
            .unwrap_or(&self.generation.descriptor.id)
            .to_owned();
        let mut cfg = self.generation.cfg.clone();
        if let Some(descriptor) = self
            .generation
            .catalog
            .get(&self.generation.descriptor.provider, &model_name)
        {
            cfg.context_window = self
                .generation
                .child_factory
                .cfg
                .context_window
                .or(Some(descriptor.context_window));
        }
        let mut pipeline = EventPipeline::new(runner, scope).with_compaction(
            self.generation.client.clone(),
            crate::context::HistoryTarget {
                provider: self.generation.descriptor.provider.clone(),
                api: Some(self.generation.descriptor.api),
                model: model_name,
            },
            cfg,
            self.generation.hooks.clone(),
        );
        pipeline.push_pending_event(CoreEvent::TurnStarted {
            submission_id: prompt.submission_id.clone(),
            prompt: prompt.prompt.clone().into(),
            source: prompt.source,
        });
        Ok(pipeline)
    }

    async fn prepare_initial_prompt(
        &mut self,
        prompt: &str,
        completion: &crate::harness::instance::child::InitialCompletion,
    ) -> Result<PreparedTurnPrompt, PhiSubagentError> {
        let turn_prompt = TurnPrompt {
            tool_policy: Default::default(),
            prompt: prompt.into(),
            source: UserPromptSubmitSource::Internal,
            injected_context: Vec::new(),
            images: Vec::new(),
        };
        let preparation = self.prompt_preparation();
        let mut cancellation = completion.cancellation();
        let mut finalization = self
            .orchestration
            .as_ref()
            .map(|scope| scope.finalization.subscribe());
        tokio::select! {
            biased;
            _ = crate::harness::manager::preparation::wait_for_cancellation(&mut cancellation) => Err(PhiSubagentError::PromptCancelled),
            _ = async {
                if let Some(finalization) = &mut finalization {
                    let _ = finalization.wait_for(|status| !matches!(status, crate::harness::manager::AgentFinalizationStatus::Open)).await;
                } else { std::future::pending::<()>().await; }
            } => Err(PhiSubagentError::PromptCancelled),
            result = async {
                let turn_prompt = match preparation.prepare(turn_prompt, UserPromptDispatch::Immediate).await
                    .map_err(|error| PhiSubagentError::PromptHandled { reason: error.to_string() })? {
                    CoreTurnPreparation::Ready(turn_prompt) => turn_prompt,
                    CoreTurnPreparation::Handled { reason } => return Err(PhiSubagentError::PromptHandled {
                        reason: reason.unwrap_or_else(|| "prompt intercepted".into()),
                    }),
                };
                Ok(turn_prompt)
            } => result,
        }
    }

    pub(crate) async fn start_delegated_agent(
        &mut self,
        name: &str,
        prompt: &str,
        parent_agent_id: &str,
        parent_tool_call_id: Option<compact_str::CompactString>,
        completion: &crate::harness::instance::child::InitialCompletion,
    ) -> Result<SubmissionDisposition, PhiSubagentError> {
        if !self.delegated_child {
            return Err(PhiSubagentError::PreparationUnavailable);
        }
        if !self.can_start_main() || self.is_delegated_agent() {
            return Err(PhiSubagentError::ParentTurnRunning);
        }
        let agent = self.resolve_subagent(name)?.clone();
        let turn_id: compact_str::CompactString = uuid::Uuid::new_v4().to_string().into();
        let scope = CoreEventScope {
            session_id: Some(self.transcript.id.clone()),
            turn_id: Some(turn_id.clone()),
            agent_id: Some(self.transcript.id.clone()),
            parent_agent_id: Some(parent_agent_id.into()),
            parent_tool_call_id,
            ..CoreEventScope::default()
        };
        self.start_initial_turn(agent, prompt, scope, completion)
            .await
    }

    async fn start_initial_turn(
        &mut self,
        definition: AgentDefinition,
        prompt: &str,
        scope: CoreEventScope,
        completion: &crate::harness::instance::child::InitialCompletion,
    ) -> Result<SubmissionDisposition, PhiSubagentError> {
        let mut prepared_prompt = self.prepare_initial_prompt(prompt, completion).await?;
        let cancellation = completion.cancellation();
        let cancelled = cancellation.borrow();
        let registration = self.orchestration.clone();
        let finalization = registration
            .as_ref()
            .map(|scope| scope.finalization.borrow());
        if *cancelled
            || finalization.as_ref().is_some_and(|status| {
                !matches!(
                    **status,
                    crate::harness::manager::AgentFinalizationStatus::Open
                )
            })
        {
            return Err(PhiSubagentError::PromptCancelled);
        }
        let notices = self
            .validate_prompt(&mut prepared_prompt)
            .map_err(|error| PhiSubagentError::PromptHandled {
                reason: error.to_string(),
            })?;
        let turn_id = scope.turn_id.clone().expect("task run has a turn ID");
        let pipeline = self
            .spawn_agent_run(&definition, &prepared_prompt, scope.clone())
            .map_err(|source| PhiSubagentError::Start { source })?;
        prepared_prompt.commit(&mut self.transcript);
        self.generation.hooks.record_notices(notices);
        self.delegated_agent = Some(DelegatedAgent {
            definition,
            scope,
            initial_completion: completion.clone(),
            outcome: None,
        });
        self.start_main(pipeline);
        Ok(SubmissionDisposition::Started {
            turn_id: turn_id.to_string(),
            prompt: prepared_prompt.prompt,
        })
    }

    fn validate_creation_request(
        &self,
        request: &AgentSpawnRequest,
        scope: &CoreEventScope,
    ) -> Result<(), PhiSubagentError> {
        if self.orchestration.is_none() {
            return Err(PhiSubagentError::PreparationUnavailable);
        }
        let caller = self.transcript.id.as_str();
        if scope.session_id.as_deref() != Some(caller)
            || scope.agent_id.as_deref().is_some_and(|id| id != caller)
        {
            return Err(management_error(
                crate::harness::manager::AgentManagerError::Denied {
                    caller: caller.into(),
                    operation: crate::harness::manager::policy::AgentOperation::CreateAgent,
                    target: scope.agent_id.as_deref().unwrap_or(caller).into(),
                },
            ));
        }
        self.resolve_subagent(&request.role)?;
        if self.child_workers.len() >= crate::harness::manager::policy::MAX_CHILD_WORKERS_PER_PARENT
        {
            return Err(PhiSubagentError::ChildLimit);
        }
        if self.admitted_subagent_outcomes.len() + self.child_workers.len()
            >= MAX_QUEUED_SUBMISSIONS
        {
            return Err(PhiSubagentError::ChildLimit);
        }
        Ok(())
    }

    fn spawn_managed_subagent(
        &mut self,
        request: AgentSpawnRequest,
        mut scope: CoreEventScope,
        agent_origin: bool,
        directory: std::path::PathBuf,
    ) -> Result<AgentCreation, PhiSubagentError> {
        self.validate_creation_request(&request, &scope)?;
        scope.agent_id = Some(self.transcript.id.clone());
        let reserved = self
            .orchestration
            .as_ref()
            .expect("bound instance")
            .reserve(
                request,
                directory,
                scope.parent_tool_call_id.clone(),
                agent_origin,
            )
            .map_err(management_error)?;
        let creation = reserved.creation().clone();
        self.track_child(&creation, reserved.completion());
        self.start_managed_worker(&creation, scope, reserved);
        Ok(creation)
    }

    pub(crate) fn observe_started_child(
        &mut self,
        creation: AgentCreation,
        completion: crate::harness::instance::child::InitialCompletion,
        child: crate::harness::instance::AgentInstance,
    ) -> Result<(), PhiSubagentError> {
        if self.admitted_subagent_outcomes.len() + self.child_workers.len()
            >= MAX_QUEUED_SUBMISSIONS
        {
            return Err(PhiSubagentError::ChildLimit);
        }
        if self.child_workers.len() >= crate::harness::manager::policy::MAX_CHILD_WORKERS_PER_PARENT
        {
            return Err(PhiSubagentError::ChildLimit);
        }
        self.track_child(&creation, completion.clone());
        self.record_subagent_admission(creation.instance_id());
        let sender = self.child_worker_sender.clone();
        let tracked = creation.clone();
        let (cancellation, mut cancelled) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let observed = tokio::select! {
                biased;
                _ = crate::harness::manager::preparation::wait_for_cancellation(&mut cancelled) => {
                    completion.cancel();
                    child.await_initial_completion(&completion).await
                }
                result = child.await_initial_completion(&completion) => result,
            }
            .map_err(|error| error.to_string());
            let outcome = child_worker_outcome(
                completion.get(),
                observed.and_then(|_| {
                    completion
                        .get()
                        .ok_or_else(|| "child completion unavailable".into())
                }),
                completion
                    .is_cancelled()
                    .then(|| completion.cancellation_reason()),
            );
            let _ = sender.send(super::ChildWorkerMessage::Finished(ChildResult {
                creation,
                outcome,
            }));
        });
        self.child_workers.insert(
            tracked.instance_id().clone(),
            super::ChildWorker {
                creation: tracked,
                handle,
                cancellation,
            },
        );
        Ok(())
    }

    fn track_child(
        &mut self,
        creation: &AgentCreation,
        completion: crate::harness::instance::child::InitialCompletion,
    ) {
        self.child_observations
            .entry(creation.instance_id().clone())
            .and_modify(|observed| observed.completion = completion.clone())
            .or_insert_with(|| super::ChildObservation {
                status: SubagentStatus {
                    creation: creation.clone(),
                    parent_tool_call_id: creation.parent_tool_call_id().map(str::to_owned),
                    admission: SubagentAdmission::Pending,
                    outcome: None,
                },
                completion,
            });
    }

    fn resolve_subagent(&self, name: &str) -> Result<&AgentDefinition, PhiSubagentError> {
        self.generation
            .agents
            .get(name)
            .ok_or_else(|| PhiSubagentError::UnknownAgent {
                name: name.into(),
                available: self
                    .generation
                    .agents
                    .all()
                    .iter()
                    .map(|agent| agent.name().to_string())
                    .collect(),
            })
    }

    fn start_managed_worker(
        &mut self,
        creation: &AgentCreation,
        scope: CoreEventScope,
        reserved: crate::harness::manager::coordinator::ReservedSubagent,
    ) {
        let preparation = self.generation.subagent_preparation.clone();
        let phi = self.generation.child_factory.clone();
        let completion = reserved.completion();
        let creation = creation.clone();
        let sender = self.child_worker_sender.clone();
        let tracked_task = creation.clone();
        let (cancellation, cancel_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut accepted = false;
            let run = async {
                if *cancel_rx.borrow() {
                    return Err(SUBAGENT_CANCELLATION_MESSAGE.to_string());
                }
                let child = match preparation {
                    Some(preparation) => {
                        reserved
                            .admit_prepared(preparation, cancel_rx.clone())
                            .await
                    }
                    None => reserved.admit_sdk(&phi, cancel_rx.clone()).await,
                }
                .map_err(|error| error.to_string())?;
                accepted = true;
                let _ = sender.send(super::ChildWorkerMessage::Admitted(
                    creation.instance_id().clone(),
                ));
                child.finish().await
            };
            let result = crate::core::interaction::with_interaction_scope(scope, run).await;
            let cancelled = (*cancel_rx.borrow() || completion.is_cancelled())
                .then(|| completion.cancellation_reason());
            let canonical = accepted.then(|| completion.get()).flatten();
            let outcome = child_worker_outcome(canonical, result, cancelled);
            let _ = sender.send(super::ChildWorkerMessage::Finished(ChildResult {
                creation,
                outcome,
            }));
        });
        self.child_workers.insert(
            tracked_task.instance_id().clone(),
            super::ChildWorker {
                creation: tracked_task,
                handle,
                cancellation,
            },
        );
    }

    pub(crate) fn progress_child_workers(&mut self) {
        self.drain_child_worker_messages();
        let finished = self
            .child_workers
            .iter()
            .filter(|(_, slot)| slot.handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        // A worker can publish its result between the first drain and the finished scan.
        self.drain_child_worker_messages();
        for id in finished {
            if let Some(slot) = self.child_workers.remove(&id) {
                self.record_subagent_result(ChildResult {
                    creation: slot.creation,
                    outcome: Err(PhiFailure::new(
                        PhiFailureKind::Internal,
                        "child coordination worker terminated without a result",
                        None,
                    )),
                });
            }
        }
    }

    fn drain_child_worker_messages(&mut self) {
        while let Ok(message) = self.child_worker_messages.try_recv() {
            match message {
                super::ChildWorkerMessage::Admitted(instance_id) => {
                    self.record_subagent_admission(&instance_id)
                }
                super::ChildWorkerMessage::Finished(result) => {
                    if self
                        .child_workers
                        .remove(result.creation.instance_id())
                        .is_some()
                    {
                        self.record_subagent_result(result);
                    }
                }
            }
        }
    }

    fn record_subagent_admission(&mut self, instance_id: &AgentInstanceId) {
        let Some(observed) = self.child_observations.get_mut(instance_id) else {
            return;
        };
        let status = &mut observed.status;
        if status.admission != SubagentAdmission::Pending {
            return;
        }
        status.admission = SubagentAdmission::Accepted;
        let creation = &status.creation;
        if let Some(active) = self.directed_delegation.as_mut()
            && active.instance_id.as_ref() == Some(instance_id)
        {
            let _ = active.admitted.set(creation.mode());
        }
        // Foreground calls must wait for execution, not just admission.
        if creation.mode() == DelegationMode::Background
            && let Some(reply) = self.agent_tool_replies.remove(instance_id)
        {
            let _ = reply.send(Ok(AgentToolOutput::Created(creation.clone())));
        }
        self.observer_subagent_activity
            .push_back(SubagentActivity::Started {
                instance_id: instance_id.as_str().to_string(),
                name: creation.definition().into(),
                prompt: creation.prompt().into(),
            });
    }

    pub(crate) fn service_agent_tool_requests(&mut self) {
        let mut requests = Vec::new();
        if let Some(receiver) = self.generation.agent_requests.as_mut() {
            while let Ok(request) = receiver.try_recv() {
                requests.push(request);
            }
        }
        for request in requests {
            self.start_agent_tool_request(request);
        }
    }

    pub(super) fn start_agent_tool_request(&mut self, request: AgentToolRequest) {
        if request.reply.is_closed() {
            return;
        }
        let directed = self.directed_delegation.as_ref().is_some_and(|active| {
            request.scope.session_id.as_deref() == Some(self.transcript.id.as_str())
                && request.scope.turn_id.as_deref() == Some(active.turn_id.as_str())
        });
        if directed {
            let active = self.directed_delegation.as_ref().expect("checked above");
            let error = if active.attempted {
                Some("directed prompt already delegated to an Agent")
            } else if !matches!(&request.args, crate::agent::tools::AgentArgs::Create(args)
            if args.role == active.constraint.agent
                && match active.constraint.mode {
                    Some(mode) => args.mode == Some(mode),
                    None => args.mode != Some(DelegationMode::Foreground),
                })
            {
                Some("Agent call does not match the directed prompt")
            } else {
                None
            };
            if let Some(error) = error {
                let _ = request.reply.send(Err(error.into()));
                return;
            }
        }
        match self.validate_tool_scope(&request.scope) {
            Ok(()) => {}
            Err(error) => {
                let _ = request.reply.send(Err(error));
                return;
            }
        };
        let task_request = match request.args {
            crate::agent::tools::AgentArgs::Create(args) => args,
            crate::agent::tools::AgentArgs::Manage(args) => {
                self.manage_agent_tool(args, request.reply);
                return;
            }
        };
        if directed {
            self.directed_delegation
                .as_mut()
                .expect("checked above")
                .attempted = true;
        }
        self.prepare_creation(
            task_request,
            request.scope,
            CreationReply::Tool(request.reply),
        );
    }

    fn manage_agent_tool(
        &self,
        args: crate::agent::tools::AgentManagementArgs,
        mut reply: tokio::sync::oneshot::Sender<Result<AgentToolOutput, String>>,
    ) {
        let access = self
            .orchestration
            .as_ref()
            .ok_or(crate::agent_manager::AgentManagerError::Unavailable)
            .and_then(|scope| scope.access());
        let access = match access {
            Ok(access) => access,
            Err(error) => {
                let _ = reply.send(Err(error.to_string()));
                return;
            }
        };
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = reply.closed() => {},
                result = async {
                    use crate::agent::tools::AgentManagementArgs;
                    match args {
                        AgentManagementArgs::ListAgents {} => access.list().map(AgentToolOutput::Members),
                        AgentManagementArgs::SendMessage { recipient, content } => {
                            access.send_message(&recipient, content).await.map(|receipt| {
                                AgentToolOutput::MessageSent(receipt.message().recipient.clone())
                            })
                        }
                    }
                } => {
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
            }
        });
    }

    pub(crate) fn drain_subagent_activity(&mut self) -> Vec<SubagentActivity> {
        self.observer_subagent_activity.drain(..).collect()
    }

    pub(crate) fn subagent_status(&self, instance_id: &AgentInstanceId) -> Option<SubagentStatus> {
        self.child_observations
            .get(instance_id)
            .map(|observed| observed.status.clone())
    }

    pub(crate) fn subagent_snapshots(&self) -> Vec<SubagentSnapshot> {
        let mut snapshots = self
            .child_observations
            .values()
            .map(|observed| SubagentSnapshot {
                status: observed.status.clone(),
                initial_turn_id: observed.completion.initial_turn_id(),
                executor_registration_id: observed.completion.executor_registration_id(),
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.status
                .creation
                .instance_id()
                .as_str()
                .cmp(right.status.creation.instance_id().as_str())
        });
        snapshots
    }

    pub(super) fn record_subagent_result(&mut self, result: ChildResult) {
        let ChildResult { creation, outcome } = result;
        if !self.record_subagent_outcome(&creation, &outcome) {
            return;
        }
        self.observer_subagent_activity
            .push_back(SubagentActivity::Finished {
                instance_id: creation.instance_id().as_str().into(),
                name: creation.definition().into(),
                succeeded: outcome.is_ok(),
            });
        if let Some(reply) = self.agent_tool_replies.remove(creation.instance_id()) {
            let _ = reply.send(
                outcome
                    .clone()
                    .map(AgentToolOutput::Response)
                    .map_err(|failure| subagent_failure_output(&failure)),
            );
        }
        if creation.result_routing()
            == crate::harness::manager::preparation::AgentResultRouting::Creator
            && creation.mode() == DelegationMode::Background
            && self
                .child_observations
                .get(creation.instance_id())
                .is_some_and(|observed| observed.status.admission == SubagentAdmission::Accepted)
        {
            self.queue_subagent_outcome(&creation, &outcome);
        }
        self.completed_child_ids
            .push_back(creation.instance_id().clone());
        self.prune_completed_children();
    }

    pub(crate) fn is_delegated_agent(&self) -> bool {
        self.delegated_agent.is_some()
    }

    pub(crate) fn delegated_completion(&self) -> Option<Result<String, PhiFailure>> {
        let agent = self.delegated_agent.as_ref()?;
        if self.main_run.is_some()
            || !self.child_workers.is_empty()
            || !self.pending_creations.is_empty()
            || !self.admitted_subagent_outcomes.is_empty()
        {
            return None;
        }
        Some(agent.outcome.clone().unwrap_or_else(|| {
            Err(PhiFailure::new(
                PhiFailureKind::Internal,
                "delegated Agent ended without a result",
                None,
            ))
        }))
    }

    pub(crate) fn cancel_subagent(
        &mut self,
        instance_id: &AgentInstanceId,
        reason: crate::CancellationReason,
    ) -> crate::harness::manager::control::ChildCancellation {
        let Some(observed) = self.child_observations.get(instance_id) else {
            return Default::default();
        };
        let completion = observed.completion.clone();
        match completion.admission() {
            SubagentAdmission::Rejected => return Default::default(),
            SubagentAdmission::Accepted if completion.is_complete() => return Default::default(),
            _ => {}
        }
        let changed = completion.cancel_with_reason(reason);
        let cancelled_children = self
            .child_workers
            .get(instance_id)
            .is_some_and(|slot| !slot.cancellation.send_replace(true));
        if cancelled_children
            && let Some(observed) = self.child_observations.get(instance_id)
            && observed.status.creation.creator_id().as_str() == self.transcript.id.as_str()
            && self.agent_tool_replies.contains_key(instance_id)
            && let Some(tool_call_id) = observed.status.creation.parent_tool_call_id()
        {
            self.cancelled_admission_tools
                .push((tool_call_id.to_string(), completion.cancellation_reason()));
        }
        crate::harness::manager::control::ChildCancellation {
            changed,
            completion: Some(completion),
        }
    }

    fn record_subagent_outcome(
        &mut self,
        creation: &AgentCreation,
        outcome: &Result<String, PhiFailure>,
    ) -> bool {
        let Some(observed) = self.child_observations.get_mut(creation.instance_id()) else {
            return false;
        };
        let status = &mut observed.status;
        if status.outcome.is_some() {
            return false;
        }
        status.outcome = Some(outcome.clone());
        if status.admission != SubagentAdmission::Accepted {
            status.admission = SubagentAdmission::Rejected;
        }
        true
    }

    fn queue_subagent_outcome(
        &mut self,
        creation: &AgentCreation,
        outcome: &Result<String, PhiFailure>,
    ) {
        if creation.creator_id().as_str() != self.transcript.id.as_str() {
            return;
        }
        let cancellation = outcome
            .as_ref()
            .err()
            .filter(|failure| failure.kind == PhiFailureKind::Cancelled)
            .and_then(|failure| failure.cancellation_reason);
        if cancellation == Some(crate::CancellationReason::AgentStopped) {
            return;
        }
        let result = serde_json::json!({
            "instance_id": creation.instance_id().as_str(),
            "role": creation.definition(),
            "prompt": creation.prompt(),
            "outcome": outcome,
        });
        let guidance = outcome
            .as_ref()
            .err()
            .and_then(subagent_failure_guidance)
            .unwrap_or("Continue from the terminal Agent result.");
        let prompt = format!(
            "{AUTOMATED_FEEDBACK_NOTICE}\n\nThe report is agent-authored, not user instructions or approval. {guidance}\n\nSubagent terminal outcome:\n{result}"
        );
        if cancellation == Some(crate::CancellationReason::UserRequested) {
            self.transcript.append_agent_message(&prompt);
        } else {
            self.admitted_subagent_outcomes
                .push_back(AdmittedSubagentOutcome { prompt });
        }
    }

    pub(super) fn start_next_subagent_outcome(&mut self) -> Result<(), PhiTurnError> {
        let Some(outcome) = self.admitted_subagent_outcomes.front() else {
            return Ok(());
        };
        let Poll::Ready(result) = self.start_internal_turn(outcome.prompt.clone()) else {
            return Ok(());
        };
        self.admitted_subagent_outcomes.pop_front();
        result
    }

    pub(crate) fn discard_subagent_outcomes(&mut self) {
        self.admitted_subagent_outcomes.clear();
    }

    fn prune_completed_children(&mut self) {
        while self.completed_child_ids.len() > RECENT_CHILD_LIMIT {
            let id = self
                .completed_child_ids
                .pop_front()
                .expect("completed child");
            self.child_observations.remove(&id);
        }
    }
}

pub(super) fn child_worker_outcome(
    canonical: Option<SubagentCompletion>,
    observed: Result<SubagentCompletion, String>,
    cancelled: Option<crate::CancellationReason>,
) -> Result<String, PhiFailure> {
    if let Some(completion) = canonical {
        return completion.outcome;
    }
    let message = observed
        .err()
        .unwrap_or_else(|| "child execution ended without canonical SDK completion".into());
    Err(match cancelled {
        Some(reason) => PhiFailure::cancelled(reason, message),
        None => PhiFailure::new(PhiFailureKind::Internal, message, None),
    })
}

impl AgentInstanceState {
    pub(super) fn finalize_agent_admissions(&mut self) {
        let agent_id = self.transcript.id.clone();
        for observed in self
            .child_observations
            .values()
            .filter(|observed| observed.status.creation.creator_id().as_str() == agent_id)
        {
            if let Some(id) = observed.status.creation.parent_tool_call_id() {
                let cancellation = observed
                    .completion
                    .is_cancelled()
                    .then(|| observed.completion.cancellation_reason());
                let (output, is_error) =
                    interrupted_admission_result(&observed.status, cancellation);
                append_unresolved_tool_result(&mut self.transcript, id, &output, is_error);
            }
        }
        for (_, reply) in self.agent_tool_replies.drain() {
            let _ = reply.send(Err("agent tool call interrupted".into()));
        }
        for (id, reason) in std::mem::take(&mut self.cancelled_admission_tools) {
            append_unresolved_tool_result(
                &mut self.transcript,
                &id,
                &admission_failure_output(reason),
                true,
            );
        }
    }

    pub(super) fn cancel_pending_admissions(&mut self) {
        let ids = self
            .child_observations
            .values()
            .map(|observed| &observed.status)
            .filter(|status| {
                status.admission == SubagentAdmission::Pending
                    && status.creation.creator_id().as_str() == self.transcript.id.as_str()
            })
            .map(|status| status.creation.instance_id().clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(slot) = self.child_workers.get(&id)
                && let Some(observed) = self.child_observations.get(&id)
            {
                let admission = observed.completion.cancel_pending(&slot.cancellation);
                if admission == SubagentAdmission::Accepted {
                    self.record_subagent_admission(&id);
                    continue;
                }
                if let Some(tool) = self
                    .child_observations
                    .get(&id)
                    .and_then(|observed| observed.status.creation.parent_tool_call_id())
                {
                    self.cancelled_admission_tools
                        .push((tool.into(), observed.completion.cancellation_reason()));
                }
            }
            self.cancel_subagent(&id, crate::CancellationReason::Interrupted);
            if let Some(reply) = self.agent_tool_replies.remove(&id) {
                let _ = reply.send(Err(SUBAGENT_CANCELLATION_MESSAGE.into()));
            }
        }
    }

    pub(super) fn has_unresolved_tool_call(transcript: &Transcript, tool_call_id: &str) -> bool {
        use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};
        for entry in transcript.current_branch().into_iter().rev() {
            match &entry.kind {
                TranscriptEntryKind::Message {
                    message:
                        AgentMessage::ToolResult {
                            tool_call_id: id, ..
                        },
                } if id == tool_call_id => return false,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant { content, .. },
                } if content.iter().any(|block| {
                    matches!(block,
                        ContentBlock::ToolCall { id, .. } if id == tool_call_id
                    )
                }) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

fn management_error(source: crate::harness::manager::AgentManagerError) -> PhiSubagentError {
    PhiSubagentError::Management {
        source: Box::new(source),
    }
}

fn interrupted_admission_result(
    status: &SubagentStatus,
    cancellation: Option<crate::CancellationReason>,
) -> (String, bool) {
    if status.admission == SubagentAdmission::Accepted {
        match (status.creation.mode(), &status.outcome) {
            (DelegationMode::Background, _) => {
                return (
                    AgentToolOutput::Created(status.creation.clone()).into_model_output(),
                    false,
                );
            }
            (DelegationMode::Foreground, Some(Ok(output))) => return (output.clone(), false),
            (DelegationMode::Foreground, None) => {
                let output = match cancellation {
                    Some(reason) => subagent_failure_output(&PhiFailure::cancelled(
                        reason,
                        "subagent cancellation requested",
                    )),
                    None => "agent tool call interrupted; subagent is still running. Only the foreground wait was interrupted, not the child execution. Observe the existing execution and use its eventual result; do not start duplicate work. If it later fails or is interrupted, inspect its outcome and current state before safely recovering.".into(),
                };
                return (output, true);
            }
            (DelegationMode::Foreground, Some(Err(_))) => {}
        }
    }
    let message = status
        .outcome
        .as_ref()
        .and_then(|outcome| outcome.as_ref().err())
        .map(subagent_failure_output)
        .unwrap_or_else(|| {
            admission_failure_output(cancellation.unwrap_or(crate::CancellationReason::Interrupted))
        });
    (message, true)
}

fn admission_failure_output(reason: crate::CancellationReason) -> String {
    subagent_failure_output(&PhiFailure::cancelled(
        reason,
        "subagent admission was interrupted",
    ))
}

fn subagent_failure_guidance(failure: &PhiFailure) -> Option<&'static str> {
    if failure.kind != PhiFailureKind::Cancelled {
        return None;
    }
    Some(match failure.cancellation_reason {
        Some(crate::CancellationReason::UserRequested) => CANCELLED_EXECUTION_GUIDANCE,
        Some(crate::CancellationReason::AgentStopped) => {
            "The Agent was stopped. Do not restart this work without a new user request."
        }
        _ => {
            "The execution or admission was interrupted; this does not establish user cancellation. If the work is still required, inspect its outcome and current state, then safely resume or retry. Avoid duplicating an existing execution or non-idempotent side effects; do not bypass permissions or execution limits."
        }
    })
}

fn subagent_failure_output(failure: &PhiFailure) -> String {
    match subagent_failure_guidance(failure) {
        Some(guidance) => format!("{}\n\n{guidance}", failure.message),
        None => failure.message.clone(),
    }
}

fn append_unresolved_tool_result(
    transcript: &mut Transcript,
    id: &str,
    output: &str,
    is_error: bool,
) {
    if AgentInstanceState::has_unresolved_tool_call(transcript, id) {
        transcript.append_tool_result(id, output, is_error, None);
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhiSubagentError {
    #[error("{source}")]
    Management {
        #[source]
        source: Box<crate::harness::manager::AgentManagerError>,
    },
    #[error("child runtime preparation is unavailable for the requested directory")]
    PreparationUnavailable,
    #[error("child coordination limit reached")]
    ChildLimit,
    #[error("delegation depth limit reached")]
    DepthLimit,
    #[error("invalid subagent working directory `{}`: {source}", path.display())]
    WorkingDirectory {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the parent turn is already running")]
    ParentTurnRunning,
    #[error("unknown agent `{name}`; available agents: {}", available.join(", "))]
    UnknownAgent {
        name: String,
        available: Vec<String>,
    },
    #[error("subagent prompt was handled: {reason}")]
    PromptHandled { reason: String },
    #[error("subagent prompt preparation was cancelled")]
    PromptCancelled,
    #[error("failed to start subagent: {source}")]
    Start {
        #[source]
        source: anyhow::Error,
    },
}

impl AnyRuntime {
    pub fn spawn_agent_runner(
        self,
        definition: &AgentDefinition,
        hooks: HookRegistry,
        client: &AnyClient,
        catalog: &crate::models::ModelCatalog,
        options: &AgentOptions,
        cfg: &RuntimeConfig,
        working_dir: &std::path::Path,
        scope: crate::core::event::CoreEventScope,
        tool_policy: crate::core::turn::TurnToolPolicy,
    ) -> anyhow::Result<AgentRunner> {
        let system_prompt = crate::context::build_subagent_system_prompt(
            definition,
            cfg.agent_mode,
            cfg.mode_prompt_override.as_deref(),
            working_dir,
        );
        let descriptor = definition
            .model_override()
            .map(|model| {
                catalog.get(client.provider_id(), model).ok_or_else(|| {
                    anyhow::anyhow!("unknown model {}/{model}", client.provider_id())
                })
            })
            .transpose()?;
        let runtime = self.with_model(client, descriptor.as_ref(), options, cfg)?;
        Ok(crate::harness::agentic_loop::dispatch_runtime!(
            runtime,
            |runtime| {
                AgenticLoop::new(
                    runtime.tool_policy(
                        definition.tools_policy().map(<[String]>::to_vec),
                        delegated_tool_denials(definition.disallowed_tools_policy().to_vec()),
                    ),
                    &hooks,
                )
                .interaction_scope(scope)
                .max_turns(definition.max_turns_limit())
                .tool_policy(tool_policy)
                .spawn_events_with_prompt(Some(
                    crate::core::lifecycle::BuildSystemPromptEvent {
                        system_prompt,
                        cwd: Some(working_dir.display().to_string()),
                    },
                ))
            }
        ))
    }
}

#[cfg(test)]
mod specialization_tests {
    use super::*;

    #[tokio::test]
    async fn initial_hook_policy_is_validated_before_commit() {
        let mut state = super::super::tests::test_client().await;
        state
            .generation
            .hooks
            .register_hook(crate::hooks::Hook::user_prompt_submit(|event| async move {
                Ok(
                    crate::core::lifecycle::UserPromptSubmitOutcome::ContinueWithPolicy {
                        notices: Vec::new(),
                        prompt: event.prompt,
                        injected_context: Vec::new(),
                        tool_policy: crate::core::turn::TurnToolPolicy {
                            removals: vec!["MCP".into()],
                            ..Default::default()
                        },
                    },
                )
            }));
        let result = state
            .start_initial_turn(
                AgentDefinition::new("probe", "inspect", "Inspect."),
                "initial input",
                CoreEventScope {
                    session_id: Some(state.transcript.id.clone()),
                    turn_id: Some("initial-turn".into()),
                    ..Default::default()
                },
                &Default::default(),
            )
            .await;
        assert!(
            matches!(result, Err(PhiSubagentError::PromptHandled { ref reason }) if reason.contains("MCP tools")),
            "{result:?}"
        );
        assert!(state.transcript.entries.is_empty());
        assert!(state.main_run.is_none());
        assert!(state.delegated_agent.is_none());
    }

    #[test]
    fn specialization_preserves_definition_and_subagent_tool_denials() {
        let definition = AgentDefinition::new("probe", "inspect", "Inspect.")
            .tools(["Agent", "Read"])
            .disallowed_tools(["Bash"]);
        let denied = delegated_tool_denials(definition.disallowed_tools_policy().to_vec());
        for tool in [
            "Bash",
            "tape_anchor",
            "tape_delete",
            "tape_reset",
            "memory_sync",
        ] {
            assert!(denied.iter().any(|name| name == tool));
        }
        assert!(!denied.iter().any(|name| name == "Agent"));
        let definition = definition.disallowed_tools(["Agent"]);
        assert!(
            delegated_tool_denials(definition.disallowed_tools_policy().to_vec())
                .iter()
                .any(|name| name == "Agent")
        );
    }
}

#[cfg(test)]
mod error_tests {
    use anyhow::Context;

    use crate::harness::instance::builder::AgentInstanceBuildError;

    #[test]
    fn typed_boundary_preserves_original_error_chain() {
        let source = std::fs::read("/path/that/does/not/exist")
            .context("failed to load runtime resource")
            .unwrap_err();
        let error = AgentInstanceBuildError::Runtime { source };

        let AgentInstanceBuildError::Runtime { source } = error else {
            panic!("expected runtime error");
        };
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        assert!(format!("{source:?}").contains("failed to load runtime resource"));
    }
}
