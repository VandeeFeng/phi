use super::*;
use crate::core::lifecycle::{CompletionEvent, CompletionOutcome, TeammateIdleOutcome};
use crate::harness::agentic_loop::{CompletionDecision, prepare_completion_feedback};
use crate::harness::task::{TaskAccessError, TeammateTasks};
use crate::permission::runtime::PermissionRuntime;
use futures::future::BoxFuture;

const MAX_CONTINUATION_BYTES: usize = 16 * 1024;

pub(crate) enum TeammateDecision {
    Idle,
    Waiting,
    Continue(PreparedTurnPrompt),
}

pub(crate) struct TeammateCheck {
    tasks: TeammateTasks,
    preparation: PromptPreparation,
    permission: Option<PermissionRuntime>,
    scope: CoreEventScope,
    remaining_continuations: usize,
}

impl TeammateCheck {
    pub(crate) async fn run(self) -> anyhow::Result<(TeammateTasks, TeammateDecision)> {
        let decision = match &self.permission {
            Some(permission) if self.remaining_continuations > 0 => {
                self.idle_work(permission).await?
            }
            _ => TeammateDecision::Idle,
        };
        self.tasks.validate()?;
        Ok((self.tasks, decision))
    }

    async fn idle_work(&self, permission: &PermissionRuntime) -> anyhow::Result<TeammateDecision> {
        let Some(candidate) = self.tasks.idle_candidate().await? else {
            return Ok(TeammateDecision::Waiting);
        };
        if crate::core::interaction::with_interaction_scope(
            self.scope.clone(),
            crate::agent::tools::task::authorize_task(permission, &candidate.request()),
        )
        .await
        .is_err()
        {
            return Ok(TeammateDecision::Idle);
        }
        let Some(id) = self.tasks.take_idle_work(candidate).await? else {
            return Ok(TeammateDecision::Waiting);
        };
        self.prepare(format!(
            "Task {} is assigned to you. Use Task with action get to inspect it, action update to start work, \
             and explicitly mark it completed when finished. Do not infer completion from a turn ending.",
            id.as_str(),
        )).await
    }

    async fn prepare(&self, feedback: String) -> anyhow::Result<TeammateDecision> {
        if self.remaining_continuations == 0 {
            anyhow::bail!("teammate automatic continuation limit reached");
        }
        if feedback.trim().is_empty() || feedback.len() > MAX_CONTINUATION_BYTES {
            anyhow::bail!("teammate continuation feedback must be nonempty and bounded");
        }
        let prompt = TurnPrompt {
            prompt: feedback,
            source: UserPromptSubmitSource::Internal,
            tool_policy: Default::default(),
            injected_context: Vec::new(),
            images: Vec::new(),
        };
        Ok(
            match self
                .preparation
                .clone()
                .prepare(prompt, UserPromptDispatch::Immediate)
                .await?
            {
                CoreTurnPreparation::Ready(prompt) => TeammateDecision::Continue(prompt),
                CoreTurnPreparation::Handled { .. } => TeammateDecision::Idle,
            },
        )
    }
}

impl AgentInstanceState {
    pub(super) fn completion_check(
        &self,
    ) -> impl FnOnce(
        CompletionEvent,
    ) -> Option<BoxFuture<'static, anyhow::Result<CompletionDecision>>>
    + use<> {
        let working_dir = self.working_dir().to_path_buf();
        let delegated_child = self.delegated_child;
        let hooks = self.generation.hooks.clone();
        let expander = self.generation.child_factory.prompt_expander.clone();
        let tasks = self
            .orchestration
            .as_ref()
            .map(|scope| scope.access()?.teammate_tasks(hooks.clone()))
            .transpose()
            .map(Option::flatten);
        move |event| {
            if matches!(&tasks, Err(TaskAccessError::InactiveCaller)) {
                return None;
            }
            let preparation = PromptPreparation {
                hooks: hooks.clone(),
                expander,
                transcript: Arc::new(event.transcript.clone()),
                working_dir,
                delegated_child,
            };
            Some(Box::pin(async move {
                let tasks = tasks?;
                let boundary = match &tasks {
                    Some(tasks) => tasks.check_boundary().await?,
                    None => TeammateIdleOutcome::Allow,
                };
                let caller_check = matches!(&boundary, TeammateIdleOutcome::Allow);
                let outcome = match boundary {
                    TeammateIdleOutcome::Stop => {
                        if let Some(tasks) = &tasks {
                            tasks.validate()?;
                        }
                        return Ok(CompletionDecision::Stop);
                    }
                    TeammateIdleOutcome::Continue { feedback } => {
                        CompletionOutcome::Continue { feedback }
                    }
                    TeammateIdleOutcome::Allow => hooks.completion_check(&event).await?,
                };
                let decision = match outcome {
                    CompletionOutcome::Allow => CompletionDecision::Finish,
                    CompletionOutcome::Fail { reason } => {
                        anyhow::bail!("completion check failed: {reason}")
                    }
                    CompletionOutcome::Continue { feedback } => {
                        match prepare_completion_feedback(&event, feedback, preparation).await? {
                            CoreTurnPreparation::Ready(prompt) => {
                                CompletionDecision::Continue(prompt)
                            }
                            CoreTurnPreparation::Handled { .. } if !caller_check => {
                                CompletionDecision::Finish
                            }
                            CoreTurnPreparation::Handled { .. } => {
                                anyhow::bail!(
                                    "completion feedback was handled without continuation"
                                )
                            }
                        }
                    }
                };
                if let Some(tasks) = tasks {
                    tasks.validate()?;
                }
                Ok(decision)
            }))
        }
    }

    pub(in crate::harness::instance) fn take_teammate_stop(&mut self) -> bool {
        std::mem::take(&mut self.teammate_stop_requested)
    }

    pub(crate) fn teammate_check(
        &self,
        remaining_continuations: usize,
    ) -> anyhow::Result<Option<TeammateCheck>> {
        let Some(agent) = &self.delegated_agent else {
            return Ok(None);
        };
        let Some(scope) = &self.orchestration else {
            return Ok(None);
        };
        let access = scope.access()?;
        let tasks = access.teammate_tasks(self.generation.hooks.clone())?;
        let runtime = self.generation.runtime.clone().tool_policy(
            agent.definition.tools_policy().map(<[String]>::to_vec),
            crate::extension::delegated_tool_denials(
                agent.definition.disallowed_tools_policy().to_vec(),
            ),
        );
        let permission = runtime
            .is_tool_available(crate::agent::tools::task::TASK_TOOL_NAME)
            .then(|| self.generation.resources.permission().clone());
        Ok(tasks.map(|tasks| TeammateCheck {
            tasks,
            preparation: self.prompt_preparation(),
            permission,
            scope: CoreEventScope {
                session_id: Some(self.transcript.id.clone()),
                agent_id: Some(access.caller_id().to_string().into()),
                ..Default::default()
            },
            remaining_continuations,
        }))
    }

    pub(crate) fn publish_idle_report(
        &self,
        report: String,
    ) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
        let scope = self
            .orchestration
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Agent has no live registration"))?;
        Ok(tokio::spawn(async move {
            scope.publish_idle_report(report).await?;
            Ok(())
        }))
    }

    pub(crate) fn start_teammate_continuation(
        &mut self,
        mut prompt: PreparedTurnPrompt,
    ) -> Result<SubmissionDisposition, PhiTurnError> {
        if !self.is_idle() || self.needs_progress() {
            return Err(PhiTurnError::Busy);
        }
        let notices = self.validate_prompt(&mut prompt)?;
        let disposition = self.start_specialized_turn(prompt, TranscriptPromptRole::Agent)?;
        self.generation.hooks.record_notices(notices);
        Ok(disposition)
    }

    pub(crate) fn stop_teammate(
        &self,
    ) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
        let scope = self
            .orchestration
            .clone()
            .ok_or_else(|| anyhow::anyhow!("teammate has no live registration"))?;
        let access = scope.access()?;
        let instance = {
            let registry = access.manager.lock_live()?;
            let member =
                access.caller(&registry, crate::harness::manager::AgentOperation::Observe)?;
            if !matches!(
                member.snapshot().kind,
                crate::harness::manager::AgentKind::Teammate { .. }
            ) {
                anyhow::bail!("idle stop requires a live teammate");
            }
            member.instance.clone()
        };
        Ok(tokio::spawn(async move {
            scope.stop(instance).await?;
            Ok(())
        }))
    }
}
