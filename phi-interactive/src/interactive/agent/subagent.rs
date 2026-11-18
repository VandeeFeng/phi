use std::time::{Duration, Instant};

use phi_sdk::agent_instance::AgentInstanceId;
use phi_sdk::interaction::InteractionEnvelope;
use phi_sdk::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};
use phi_tui::{BlockStatus, NotifyKind};

use crate::interactive::components::conversation::BlockId;
use crate::interactive::components::conversation::ConversationView;
use crate::interactive::components::format::format_agent_title;
use crate::interactive::render::RenderTarget;
#[cfg(test)]
use crate::interactive::state::SubagentStatus;
use crate::interactive::state::{SubagentView, UiState};
use crate::runtime::attachment::{AgentAttachment, AgentChanges};
use crate::session::{Session, SubagentChildLink, SubagentOutcome};

pub(crate) struct SubagentRun {
    link: SubagentChildLink,
    block: Option<BlockId>,
    revision: Option<(String, usize, String)>,
    headers_requested: bool,
    detail: Option<(Session, ConversationView)>,
    detail_pending: bool,
    compact_enabled: Option<bool>,
}

impl SubagentRun {
    fn new(state: &mut UiState, link: SubagentChildLink) -> Self {
        let background = link.mode == Some(phi_sdk::agent_manager::DelegationMode::Background)
            || foreground_wait_released(state, &link);
        let block = if background {
            None
        } else {
            Some(project_summary_block(state, &link))
        };
        let run = Self {
            link,
            block,
            revision: None,
            headers_requested: false,
            detail: None,
            detail_pending: false,
            compact_enabled: None,
        };
        project_status(state, &run);
        run
    }

    fn restore_summary(&mut self, state: &mut UiState) {
        self.block = self.block.map(|_| project_summary_block(state, &self.link));
        self.revision = None;
        self.headers_requested = false;
        project_status(state, self);
    }

    pub(crate) fn toggle_all_collapsible_blocks(&mut self) -> bool {
        self.detail
            .as_mut()
            .is_some_and(|(_, state)| state.toggle_all_collapsible_blocks())
    }

    pub(crate) fn is_working(&self) -> bool {
        self.link.outcome == SubagentOutcome::Running
    }
}

const MAX_INPUT_WAIT: Duration = Duration::from_millis(50);

struct ChildObservation {
    attachment: AgentAttachment,
    changes: AgentChanges,
}

#[derive(Default)]
pub(crate) struct SubagentProjection {
    runs: Vec<SubagentRun>,
    attachments: Vec<ChildObservation>,
    selected: Option<String>,
    parent_id: Option<AgentInstanceId>,
    registration: Option<phi_sdk::agent_manager::AgentRegistrationId>,
    changes: Option<AgentChanges>,
}

impl SubagentProjection {
    pub(crate) async fn poll(
        &mut self,
        main: &AgentAttachment,
        state: &mut UiState,
    ) -> anyhow::Result<bool> {
        self.reset_for(main.id());
        self.restore_projection(state);
        if !state.features.activities {
            return Ok(false);
        }
        let registration = main.registration_id()?;
        if self.registration != registration {
            self.attachments.clear();
            self.changes = None;
            self.registration = registration;
        }
        let initial = self.changes.is_none();
        let changes = self.changes.get_or_insert_with(|| main.subscribe_changes());
        let mut changed = initial | changes.changed();
        for child in &mut self.attachments {
            changed |= child.changes.changed();
        }
        for run in &mut self.runs {
            if run.revision.is_none() && !trace_expanded(state, run.block) {
                run.headers_requested = false;
            }
        }
        let detail_requested = self.runs.iter().any(|run| {
            self.needs_trace(state, &run.link.child_agent_id)
                || self.selected.as_deref() == Some(run.link.child_agent_id.as_str())
                    && run.detail_pending
        });
        if !changed && !detail_requested {
            return Ok(false);
        }
        let links = match main.subagents().await {
            Ok(links) => links,
            Err(error) => {
                self.changes = None;
                return Err(error);
            }
        };
        self.attach_live_children(main, &links).await;
        Ok(self.poll_views(main, state, links).await)
    }

    async fn poll_views(
        &mut self,
        main: &AgentAttachment,
        state: &mut UiState,
        links: Vec<SubagentChildLink>,
    ) -> bool {
        let mut changed = false;
        for link in links {
            changed |= self.project_summary(state, link.clone());
            if self.needs_view(state, &link) {
                changed |= self.load_view(main, state, link).await;
            }
        }
        changed
    }

    async fn load_view(
        &mut self,
        main: &AgentAttachment,
        state: &mut UiState,
        link: SubagentChildLink,
    ) -> bool {
        if let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.link.child_agent_id == link.child_agent_id)
        {
            run.headers_requested = true;
        }
        match main.subagent_view(&link.child_agent_id).await {
            Ok(view) => self.project_view(state, link, view),
            Err(error) => {
                state.notify(
                    NotifyKind::Warning,
                    format!("failed to load subagent {}: {error}", link.child_agent_id),
                );
                true
            }
        }
    }

    fn needs_view(&self, state: &UiState, link: &SubagentChildLink) -> bool {
        if link.admission == phi_sdk::agents::SubagentAdmission::Pending {
            return false;
        }
        let Some(run) = self
            .runs
            .iter()
            .find(|run| run.link.child_agent_id == link.child_agent_id)
        else {
            return false;
        };
        let running = link.outcome == SubagentOutcome::Running;
        running && run.block.is_some()
            || self.needs_trace(state, &link.child_agent_id)
            || self.selected.as_deref() == Some(link.child_agent_id.as_str())
                && (running
                    || run.detail_pending
                    || run
                        .detail
                        .as_ref()
                        .is_some_and(|(_, detail)| detail.run_active_projection()))
    }

    fn needs_trace(&self, state: &UiState, id: &str) -> bool {
        self.runs
            .iter()
            .find(|run| run.link.child_agent_id == id)
            .is_some_and(|run| {
                run.revision.is_none() && !run.headers_requested && trace_expanded(state, run.block)
            })
    }

    pub(crate) fn restore_summaries(&mut self, state: &mut UiState, parent: &Session) {
        self.reset_for(&AgentInstanceId::new(parent.id.as_str()));
        self.restore_projection(state);
        for link in &parent.children {
            let block = match self
                .runs
                .iter()
                .find(|run| run.link.child_agent_id == link.child_agent_id)
            {
                Some(run) => run.block,
                None => {
                    let run = SubagentRun::new(state, link.into());
                    let block = run.block;
                    self.runs.push(run);
                    block
                }
            };
            if let Some(block) = block {
                state.conversation.restore_subagent_position(block);
            }
        }
    }

    fn project_summary(&mut self, state: &mut UiState, link: SubagentChildLink) -> bool {
        if link.admission == phi_sdk::agents::SubagentAdmission::Pending {
            return false;
        }
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.link.child_agent_id == link.child_agent_id)
        else {
            let run = SubagentRun::new(state, link);
            if run.block.is_none() {
                notify_child_status(state, &run.link);
            }
            self.runs.push(run);
            return true;
        };
        let background = link.mode == Some(phi_sdk::agent_manager::DelegationMode::Background)
            || foreground_wait_released(state, &link);
        let detached = if background && let Some(block) = run.block.take() {
            state.conversation.remove_subagent_trace(block);
            run.revision = None;
            run.headers_requested = false;
            notify_child_status(state, &link);
            true
        } else {
            false
        };
        if run.link == link {
            return detached;
        }
        if run.link.outcome != link.outcome {
            if !detached && run.is_working() {
                notify_child_status(state, &link);
            }
            run.revision = None;
            run.headers_requested = false;
            if let Some(block) = run.block {
                state
                    .conversation
                    .project_subagent_thinking(block, false, false);
            }
        }
        run.link = link;
        project_status(state, run);
        true
    }

    fn restore_projection(&mut self, state: &mut UiState) {
        if !self.runs.is_empty() && state.subagent_views.is_empty() {
            for run in &mut self.runs {
                run.restore_summary(state);
            }
            self.changes = None;
        }
    }

    fn reset_for(&mut self, id: &AgentInstanceId) {
        if self.parent_id.as_ref() != Some(id) {
            self.runs.clear();
            self.attachments.clear();
            self.selected = None;
            self.parent_id = Some(id.clone());
            self.changes = None;
        }
    }

    async fn attach_live_children(&mut self, main: &AgentAttachment, links: &[SubagentChildLink]) {
        let mut pending =
            live_child_links(main.clone(), links).collect::<std::collections::VecDeque<_>>();
        let mut visited = std::collections::HashSet::new();
        while let Some((parent, instance_id, id)) = pending.pop_front() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let attachment = match self.attachments.iter().find(|owner| {
                owner.attachment.id() == &id
                    && owner.attachment.snapshot().status
                        != phi_sdk::agent_instance::AgentInstanceStatus::Stopped
            }) {
                Some(owner) => owner.attachment.clone(),
                None => {
                    let Ok(owner) = parent.child_attachment(&instance_id).await else {
                        continue;
                    };
                    self.attachments.push(ChildObservation {
                        changes: owner.subscribe_snapshot_changes(),
                        attachment: owner.clone(),
                    });
                    owner
                }
            };
            if let Ok(children) = attachment.subagents().await {
                pending.extend(live_child_links(attachment.clone(), &children));
            }
        }
        self.attachments.retain(|owner| {
            owner.attachment.snapshot().status
                != phi_sdk::agent_instance::AgentInstanceStatus::Stopped
        });
    }

    fn project_session(
        &mut self,
        state: &mut UiState,
        link: SubagentChildLink,
        child: &Session,
    ) -> bool {
        let changed = self.project_summary(state, link.clone());
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.link.child_agent_id == link.child_agent_id)
        else {
            return changed;
        };
        changed | refresh_trace(state, run, child)
    }

    fn project_view(
        &mut self,
        state: &mut UiState,
        link: SubagentChildLink,
        view: crate::runtime::SubagentView,
    ) -> bool {
        let instance_id = link.child_agent_id.clone();
        let mut changed = self.project_session(state, link, &view.session);
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.link.child_agent_id == instance_id)
        else {
            return changed;
        };
        let thinking = run.is_working()
            && view
                .live_run
                .as_ref()
                .is_some_and(|live| !live.reasoning_text.is_empty());
        if let Some(block) = run.block {
            changed |= state
                .conversation
                .project_subagent_thinking(block, thinking, thinking);
            if let Some(tool) = view.live_run.as_ref().and_then(|live| live.tool.as_ref()) {
                changed |= state
                    .conversation
                    .project_subagent_tool_started(block, &tool.id, &tool.name, &tool.args);
            }
        }
        if self.selected.as_deref() == Some(&instance_id) {
            match refresh_detail(run, view.session, view.live_run.as_ref()) {
                Ok(detail_changed) => {
                    changed |= detail_changed || run.compact_enabled != view.compact_enabled;
                    run.compact_enabled = view.compact_enabled;
                }
                Err(error) => {
                    state.notify(
                        NotifyKind::Warning,
                        format!("failed to project subagent: {error}"),
                    );
                    changed = true;
                }
            }
        }
        changed
    }

    pub(crate) async fn open(
        &mut self,
        id: String,
        main: &AgentAttachment,
    ) -> anyhow::Result<bool> {
        let view = main.subagent_view(&id).await?;
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.link.child_agent_id == id)
        else {
            return Ok(false);
        };
        refresh_detail(run, view.session, view.live_run.as_ref())?;
        run.compact_enabled = view.compact_enabled;
        for run in &mut self.runs {
            if run.link.child_agent_id != id {
                run.detail = None;
            }
        }
        self.selected = Some(id);
        Ok(true)
    }

    pub(crate) fn switch_to_main(&mut self) {
        self.selected = None;
        for run in &mut self.runs {
            run.detail = None;
            run.detail_pending = false;
        }
    }

    pub(crate) fn active_subagent(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut SubagentRun> {
        let selected = self.selected.as_deref()?;
        self.runs
            .iter_mut()
            .find(|run| run.link.child_agent_id == selected)
    }

    pub(crate) async fn cancel(&mut self, id: &str, main: &AgentAttachment) -> anyhow::Result<()> {
        let instance_id = serde_json::from_value(serde_json::Value::String(id.into()))?;
        main.cancel_subagent(instance_id).await?;
        Ok(())
    }

    pub(crate) async fn interaction_owner(
        &self,
        main: &AgentAttachment,
        pending: Option<&InteractionEnvelope>,
    ) -> anyhow::Result<(AgentAttachment, Vec<InteractionEnvelope>)> {
        for owner in
            std::iter::once(main).chain(self.attachments.iter().map(|child| &child.attachment))
        {
            let requests = owner.pending_interactions().await?;
            let selected = match pending {
                Some(pending) => requests
                    .iter()
                    .any(|request| request.request_id == pending.request_id),
                None => !requests.is_empty(),
            };
            if selected {
                return Ok((owner.clone(), requests));
            }
        }
        Ok((main.clone(), Vec::new()))
    }

    pub(crate) fn render_target<'a>(
        &'a mut self,
        state: &'a mut UiState,
        parent: &'a Session,
    ) -> RenderTarget<'a> {
        if state.interaction.is_some() {
            return main_render_target(state, parent);
        }
        if let Some(run) = self.active_mut()
            && let Some((session, detail)) = &mut run.detail
        {
            return RenderTarget::Subagent {
                conversation: detail,
                session,
                compact_enabled: run.compact_enabled,
                terminal: state,
            };
        }
        main_render_target(state, parent)
    }

    pub(crate) fn advance_clocks(&mut self, state: &mut UiState, now: Instant) -> bool {
        let target = self
            .active_mut()
            .and_then(|run| run.detail.as_mut())
            .map(|(_, state)| state)
            .unwrap_or(&mut state.conversation);
        target.advance_clocks(now)
    }

    pub(crate) fn input_timeout(&self, state: &UiState, now: Instant) -> Duration {
        if state.runtime.render_due() {
            return Duration::ZERO;
        }
        let conversation = self
            .runs
            .iter()
            .find(|run| Some(run.link.child_agent_id.as_str()) == self.selected.as_deref())
            .and_then(|run| run.detail.as_ref())
            .map(|(_, view)| view)
            .unwrap_or(&state.conversation);
        conversation
            .next_clock_deadline()
            .into_iter()
            .map(|deadline| deadline.saturating_duration_since(now))
            .chain(state.runtime.render_wait_duration())
            .chain(std::iter::once(MAX_INPUT_WAIT))
            .min()
            .unwrap_or(MAX_INPUT_WAIT)
    }
}

fn main_render_target<'a>(state: &'a mut UiState, session: &'a Session) -> RenderTarget<'a> {
    RenderTarget::Main { state, session }
}

fn live_child_links(
    parent: AgentAttachment,
    links: &[SubagentChildLink],
) -> impl Iterator<Item = (AgentAttachment, String, AgentInstanceId)> + '_ {
    links.iter().map(move |link| {
        (
            parent.clone(),
            link.child_agent_id.clone(),
            AgentInstanceId::new(link.child_agent_id.as_str()),
        )
    })
}

fn refresh_trace(state: &mut UiState, run: &mut SubagentRun, child: &Session) -> bool {
    let Some(block) = run.block else {
        return false;
    };
    let revision = (
        child.updated_at.to_string(),
        child.entries.len(),
        child.leaf_id.as_deref().unwrap_or_default().to_string(),
    );
    if run.revision.as_ref() == Some(&revision) {
        return false;
    }
    run.revision = Some(revision);
    project_child_headers(state, block, child);
    true
}

fn trace_expanded(state: &UiState, block: Option<BlockId>) -> bool {
    block.is_some_and(|block| state.conversation.block_is_expanded(block))
}

fn refresh_detail(
    run: &mut SubagentRun,
    mut child: Session,
    live: Option<&phi_sdk::agent_instance::LiveRunSnapshot>,
) -> anyhow::Result<bool> {
    run.detail_pending = true;
    let working = run.is_working();
    let mut changed = true;
    if let Some((session, detail)) = &mut run.detail {
        let previous = (
            detail.source().clone(),
            detail.projection_revision,
            detail.usage,
            detail.run_active_projection(),
        );
        detail.observe_child(&child, live, working)?;
        changed = previous
            != (
                detail.source().clone(),
                detail.projection_revision,
                detail.usage,
                detail.run_active_projection(),
            );
        crate::interactive::release_transcript_snapshot(&mut child);
        *session = child;
    } else {
        let mut detail = ConversationView::default();
        detail.observe_child(&child, live, working)?;
        crate::interactive::release_transcript_snapshot(&mut child);
        run.detail = Some((child, detail));
    }
    run.detail_pending = false;
    Ok(changed)
}

fn outcome_status(outcome: &SubagentOutcome) -> (BlockStatus, &'static str) {
    match outcome {
        SubagentOutcome::Running => (BlockStatus::Running, "running"),
        SubagentOutcome::Finished {
            is_error: false, ..
        } => (BlockStatus::Success, "complete"),
        SubagentOutcome::Cancelled => (BlockStatus::Error, "cancelled"),
        SubagentOutcome::Interrupted => (BlockStatus::Error, "interrupted"),
        SubagentOutcome::Finished { is_error: true, .. } => (BlockStatus::Error, "failed"),
    }
}

fn foreground_wait_released(state: &UiState, link: &SubagentChildLink) -> bool {
    link.outcome == SubagentOutcome::Running
        && link.parent_tool_call_id.as_deref().is_some_and(|call_id| {
            state.conversation.blocks.iter().any(|block| {
                block.parent_id.is_none()
                    && block.tool_call_id.as_deref() == Some(call_id)
                    && block.block.status != BlockStatus::Running
            })
        })
}

fn notify_child_status(state: &mut UiState, link: &SubagentChildLink) {
    let (kind, status) = match &link.outcome {
        SubagentOutcome::Running => (NotifyKind::Info, "running in background"),
        SubagentOutcome::Cancelled => (NotifyKind::Info, "stopped"),
        SubagentOutcome::Interrupted => (NotifyKind::Warning, "interrupted"),
        SubagentOutcome::Finished { is_error: true, .. } => (NotifyKind::Warning, "failed"),
        SubagentOutcome::Finished {
            is_error: false, ..
        } => (NotifyKind::Info, "finished"),
    };
    state.notify(
        kind,
        format!(
            "subagent {} ({}) {status}",
            link.display_name(),
            link.child_agent_id
        ),
    );
}

fn project_summary_block(state: &mut UiState, link: &SubagentChildLink) -> BlockId {
    state.conversation.project_subagent_container(
        link.parent_tool_call_id.as_deref(),
        format_agent_title(&link.display_name()),
    )
}

fn project_status(state: &mut UiState, run: &SubagentRun) {
    let (block_status, suffix) = outcome_status(&run.link.outcome);
    let view = SubagentView::from(&run.link);
    if let Some(block) = run.block {
        state.conversation.project_subagent_status(
            block,
            format_agent_title(&view.name),
            &format!(
                "instanceId: {}\nsessionId: {}\nworkspace: {}\nstatus: {suffix}",
                run.link.child_agent_id, run.link.child_session_id, run.link.child_working_dir
            ),
            block_status,
        );
    }
    if let Some(current) = state
        .subagent_views
        .iter_mut()
        .find(|current| current.id == view.id)
    {
        *current = view;
    } else {
        state.subagent_views.push(view);
    }
}

fn project_child_headers(state: &mut UiState, parent: BlockId, child: &Session) {
    for entry in child.current_branch() {
        if let TranscriptEntryKind::Message { message } = &entry.kind {
            project_message_headers(state, parent, message);
        }
    }
}

fn project_message_headers(state: &mut UiState, parent: BlockId, message: &AgentMessage) {
    match message {
        AgentMessage::Assistant { content, .. } => {
            for block in content {
                if matches!(block, ContentBlock::Thinking { .. }) {
                    state
                        .conversation
                        .project_subagent_thinking(parent, false, true);
                }
                if let ContentBlock::ToolCall { id, name, args, .. } = block {
                    state
                        .conversation
                        .project_subagent_tool_started(parent, id, name, args);
                }
            }
        }
        AgentMessage::ToolResult {
            tool_call_id,
            is_error,
            ..
        } => {
            state
                .conversation
                .project_subagent_tool_finished(parent, tool_call_id, *is_error);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use phi_sdk::transcript::{SharedText, Usage};

    #[test]
    fn terminal_wait_uses_render_and_clock_deadlines() {
        let mut state = UiState::default();
        state.runtime = phi_tui::TuiRuntime::new();
        let now = Instant::now();
        let manager = SubagentProjection::default();
        assert_eq!(manager.input_timeout(&state, now), MAX_INPUT_WAIT);
        assert_eq!(
            manager.input_timeout(&state, now + Duration::from_millis(90)),
            MAX_INPUT_WAIT
        );
        state.conversation.project_main_run_started();
        assert_eq!(
            manager.input_timeout(&state, state.conversation.next_clock_deadline().unwrap()),
            Duration::ZERO
        );
        state.runtime.request_immediate_render();
        assert_eq!(manager.input_timeout(&state, now), Duration::ZERO);
    }

    fn child_fixture(parent: &Session) -> (SubagentChildLink, Session) {
        let mut child = crate::session::new(
            &parent.provider,
            &parent.model,
            parent.context_window,
            &std::env::current_dir().unwrap(),
        );
        child.name = "agent:probe".into();
        child.working_dir = "/tmp/phi-child".into();
        child.bind_test_parent(&parent);
        let link = SubagentChildLink {
            child_session_id: child.id.clone(),
            child_agent_id: "child".into(),
            child_working_dir: child.working_dir.clone(),
            definition: Some("probe".into()),
            alias: None,
            parent_tool_call_id: Some("agent-call".into()),
            admission: phi_sdk::agents::SubagentAdmission::Accepted,
            outcome: SubagentOutcome::Running,
            mode: Some(phi_sdk::agent_manager::DelegationMode::Foreground),
        };
        (link, child)
    }

    #[test]
    fn terminal_notification_distinguishes_cancel_failure_and_success() {
        use phi_sdk::{CancellationReason, PhiFailure, PhiFailureKind};

        for (result, kind, label, status) in [
            (
                Err(PhiFailure::cancelled(
                    CancellationReason::UserRequested,
                    "cancelled",
                )),
                NotifyKind::Info,
                "stopped",
                SubagentStatus::Cancelled,
            ),
            (
                Err(PhiFailure::cancelled(
                    CancellationReason::Interrupted,
                    "interrupted",
                )),
                NotifyKind::Warning,
                "interrupted",
                SubagentStatus::Interrupted,
            ),
            (
                Err(PhiFailure::cancelled(
                    CancellationReason::AgentStopped,
                    "stopped",
                )),
                NotifyKind::Warning,
                "interrupted",
                SubagentStatus::Interrupted,
            ),
            (
                Err(PhiFailure::new(
                    PhiFailureKind::Cancelled,
                    "unknown source",
                    None,
                )),
                NotifyKind::Warning,
                "interrupted",
                SubagentStatus::Interrupted,
            ),
            (
                Err(PhiFailure::new(PhiFailureKind::Internal, "error", None)),
                NotifyKind::Warning,
                "failed",
                SubagentStatus::Failed,
            ),
            (
                Ok("done".into()),
                NotifyKind::Info,
                "finished",
                SubagentStatus::Success,
            ),
        ] {
            let parent =
                crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
            let (link, _) = child_fixture(&parent);
            let mut projection = SubagentProjection::default();
            let mut state = UiState::default();
            assert!(projection.project_summary(&mut state, link.clone()));
            let mut terminal = link;
            terminal.outcome = SubagentOutcome::from(&result);
            assert!(projection.project_summary(&mut state, terminal.clone()));
            assert!(!projection.project_summary(&mut state, terminal));
            assert_eq!(state.subagent_views[0].status, status);
            let notices = state
                .conversation
                .blocks
                .iter()
                .filter_map(|block| {
                    if let phi_tui::BlockBody::Notify(notice) = &block.block.body {
                        Some(notice)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].kind, kind);
            assert!(notices[0].message.ends_with(&format!(" {label}")));
        }
    }

    #[test]
    fn background_agents_have_no_trace_or_implicit_detail_reads() {
        for call_id in [Some("agent-call"), Some("workflow-call"), None] {
            for outcome in [
                SubagentOutcome::Running,
                SubagentOutcome::Finished {
                    result: "done".into(),
                    is_error: false,
                },
            ] {
                let parent =
                    crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
                let (mut link, child) = child_fixture(&parent);
                link.parent_tool_call_id = call_id.map(str::to_owned);
                link.mode = Some(phi_sdk::agent_manager::DelegationMode::Background);
                link.outcome = outcome;
                let mut projection = SubagentProjection::default();
                let mut state = UiState::default();
                assert!(projection.project_summary(&mut state, link.clone()));
                assert!(!projection.needs_view(&state, &link));
                assert!(!projection.project_summary(&mut state, link.clone()));
                assert_eq!(state.conversation.blocks.len(), 1);
                assert_eq!(state.subagent_views, [SubagentView::from(&link)]);

                projection.selected = Some(link.child_agent_id.clone());
                projection.runs[0].detail_pending = true;
                assert!(projection.needs_view(&state, &link));
                projection.project_session(&mut state, link.clone(), &child);
                projection.switch_to_main();
                assert!(!projection.needs_view(&state, &link));

                if link.outcome == SubagentOutcome::Running {
                    link.outcome = SubagentOutcome::Finished {
                        result: "done".into(),
                        is_error: false,
                    };
                    assert!(projection.project_summary(&mut state, link.clone()));
                    assert_eq!(state.conversation.blocks.len(), 2);
                }
                assert!(!projection.project_summary(&mut state, link.clone()));
                assert!(!projection.needs_view(&state, &link));
                assert_eq!(state.subagent_views[0].status, SubagentStatus::Success);
                assert!(
                    state
                        .conversation
                        .blocks
                        .iter()
                        .all(|block| block.local_notification)
                );
                assert!(
                    matches!(&state.conversation.blocks.last().unwrap().block.body,
                    phi_tui::BlockBody::Notify(notice) if notice.message.ends_with("finished"))
                );
            }
        }
    }

    #[test]
    fn released_foreground_wait_removes_trace_and_stops_detail_reads() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (link, child) = child_fixture(&parent);
        let mut projection = SubagentProjection::default();
        let mut state = UiState::default();
        let tool = state.conversation.project_tool_call_started(
            "agent-call",
            "Agent",
            "{}",
            Default::default(),
        );
        projection.project_session(&mut state, link.clone(), &child);
        assert!(projection.needs_view(&state, &link));
        let container = projection.runs[0].block.unwrap();
        state.conversation.project_subagent_tool_started(
            container,
            "read",
            "Read",
            &serde_json::json!({"path": "/tmp/example"}),
        );
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .any(|block| block.parent_id == Some(container))
        );
        state.conversation.project_tool_call_finished(
            "agent-call",
            "agent tool call interrupted; subagent is still running",
            true,
            None,
        );
        assert!(projection.project_summary(&mut state, link.clone()));
        assert!(!projection.needs_view(&state, &link));
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| block.id == tool || block.local_notification)
        );
        assert_eq!(state.conversation.blocks.len(), 2);
        assert!(!projection.project_summary(&mut state, link.clone()));
        projection.project_session(&mut state, link, &child);
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| block.id == tool || block.local_notification)
        );
    }

    #[test]
    fn parent_cancel_distinguishes_pending_tool_from_launch_receipt() {
        for mode in ["foreground", "background"] {
            let parent =
                crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
            let (link, child) = child_fixture(&parent);
            let mut manager = SubagentProjection::default();
            let mut state = UiState::default();
            state.conversation.project_main_run_started();
            let tool = state.conversation.project_tool_call_started(
                "agent-call",
                "Agent",
                &serde_json::json!({"mode": mode}).to_string(),
                Default::default(),
            );
            assert!(manager.project_session(&mut state, link.clone(), &child));
            assert_eq!(state.conversation.blocks[0].id, tool);
            assert_eq!(
                state.conversation.blocks[0].block.status,
                BlockStatus::Running
            );
            if mode == "background" {
                assert!(state.conversation.project_tool_call_finished(
                    "agent-call",
                    "launch receipt",
                    false,
                    None,
                ));
            }

            state.project_run_cancelled();
            refresh_detail(&mut manager.runs[0], child.clone(), None).unwrap();
            manager.selected = Some(link.child_agent_id.clone());
            manager.switch_to_main();
            manager.project_session(&mut state, link, &child);

            assert_eq!(state.conversation.blocks[0].id, tool);
            assert_eq!(
                state.conversation.blocks[0].block.status,
                if mode == "background" {
                    BlockStatus::Success
                } else {
                    BlockStatus::Error
                }
            );
            assert_eq!(state.subagent_views[0].status, SubagentStatus::Running);
            assert!(manager.runs[0].is_working());
            assert!(manager.runs[0].detail.is_none());
            assert!(manager.active_subagent().is_none());
            if mode == "background" {
                let mut cancelled = manager.runs[0].link.clone();
                cancelled.outcome = SubagentOutcome::Cancelled;
                assert!(manager.project_summary(&mut state, cancelled));
                assert_eq!(state.subagent_views[0].status, SubagentStatus::Cancelled);
                assert_eq!(
                    state.conversation.blocks[0].block.status,
                    BlockStatus::Success
                );
            }
        }
    }

    #[test]
    fn admission_controls_projection() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (mut link, _) = child_fixture(&parent);
        link.admission = phi_sdk::agents::SubagentAdmission::Pending;
        let mut manager = SubagentProjection::default();
        let mut state = UiState::default();
        assert!(!manager.needs_view(&state, &link));
        assert!(!manager.project_summary(&mut state, link.clone()));

        link.admission = phi_sdk::agents::SubagentAdmission::Accepted;
        assert!(manager.project_summary(&mut state, link.clone()));
        manager.selected = Some(link.child_agent_id.clone());
        manager.runs[0].detail_pending = true;
        let mut pending = link.clone();
        pending.admission = phi_sdk::agents::SubagentAdmission::Pending;
        assert!(!manager.needs_view(&state, &pending));
        assert!(manager.needs_view(&state, &link));
        link.admission = phi_sdk::agents::SubagentAdmission::Rejected;
        link.outcome = SubagentOutcome::Finished {
            result: "startup failed".into(),
            is_error: true,
        };
        assert!(manager.needs_view(&state, &link));
        let mut rejected = SubagentProjection::default();
        assert!(rejected.project_summary(&mut state, link));
        assert_eq!(state.subagent_views[0].status, SubagentStatus::Failed);
    }

    fn append_child_activity(child: &mut Session) {
        child.append_assistant_message(
            None,
            None,
            vec![
                ContentBlock::Thinking {
                    text: "private reasoning".into(),
                    metadata: None,
                },
                ContentBlock::Text {
                    text: SharedText::new("private assistant body"),
                },
                ContentBlock::ToolCall {
                    id: "call".into(),
                    name: "Bash".into(),
                    args: serde_json::json!({"command": "ls -la"}),
                    provider_call_id: None,
                    provider_item_id: None,
                },
            ],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage::default(),
            "tool_use".into(),
        );
        child.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::ToolResult {
                tool_call_id: "call".into(),
                tool_name: "Bash".into(),
                provider_call_id: None,
                provider_item_id: None,
                content: vec![ContentBlock::Text {
                    text: SharedText::new("private tool body"),
                }],
                is_error: false,
                details: None,
            },
        });
    }

    #[test]
    fn resumed_workflow_workers_restore_recorded_terminal_status() {
        use crate::session::storage::{WorkflowStorage, load_session, save_session, session_path};

        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-worker-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        parent.working_dir = root.to_string_lossy().into();
        let (mut link, child) = child_fixture(&parent);
        link.child_agent_id = child.id.to_string();
        link.parent_tool_call_id = Some("workflow-call".into());
        parent.children.push((&link).try_into().unwrap());
        let run_id: phi_sdk::workflow::WorkflowRunId =
            uuid::Uuid::new_v4().to_string().parse().unwrap();
        parent.workflow_run_ids.push(run_id);
        parent.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::ToolResult {
                tool_call_id: "workflow-call".into(),
                tool_name: "Workflow".into(),
                provider_call_id: None,
                provider_item_id: None,
                content: vec![ContentBlock::Text {
                    text: serde_json::json!({"run_id": run_id, "status": "accepted"})
                        .to_string()
                        .into(),
                }],
                is_error: false,
                details: None,
            },
        });
        save_session(&parent).unwrap();
        let storage = WorkflowStorage::new(&parent, run_id).unwrap();
        for (status, value, error, expected) in [
            (
                "succeeded",
                Some("private output"),
                None,
                SubagentStatus::Success,
            ),
            ("succeeded", Some(""), None, SubagentStatus::Success),
            (
                "failed",
                None,
                Some("worker failed"),
                SubagentStatus::Failed,
            ),
            (
                "cancelled",
                None,
                Some("stopped"),
                SubagentStatus::Cancelled,
            ),
            ("running", None, None, SubagentStatus::Interrupted),
            ("succeeded", None, None, SubagentStatus::Interrupted),
            (
                "succeeded",
                Some("output"),
                Some("error"),
                SubagentStatus::Interrupted,
            ),
        ] {
            storage
                .save(&serde_json::json!({
                    "version": 2, "creator_id": parent.id, "run_id": run_id, "name": "saved workflow",
                    "state": {"status": "settled", "execution_status": "completed", "cleanup_errors": [], "invocations": [{
                        "id": 0, "role": "probe", "prompt": "inspect",
                        "worker_instance_id": link.child_agent_id,
                        "worker_session": parent.children[0],
                        "status": status, "value": value, "error": error,
                    }]},
                }))
                .unwrap();
            let restored = load_session(session_path(&parent).to_str().unwrap()).unwrap();
            let mut state = UiState::default();
            let mut projection = SubagentProjection::default();
            projection.restore_summaries(&mut state, &restored);
            assert_eq!(state.subagent_views[0].status, expected, "{status}");
            assert!(!projection.project_summary(&mut state, (&restored.children[0]).into()));
            assert_eq!(state.subagent_views[0].status, expected);
            assert!(state.activity_agents.is_empty());
            assert!(!projection.runs[0].is_working());
            assert!(
                !serde_json::to_string(&restored.children)
                    .unwrap()
                    .contains("private output")
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restored_workers_keep_call_order_before_parent_continuation() {
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        parent.append_assistant_message(
            None,
            None,
            vec![ContentBlock::ToolCall {
                id: "workflow-call".into(),
                name: "Workflow".into(),
                args: serde_json::json!({}),
                provider_call_id: None,
                provider_item_id: None,
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage::default(),
            "tool_use".into(),
        );
        parent.append_assistant_message(
            None,
            None,
            vec![ContentBlock::Text {
                text: "parent continuation".into(),
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage::default(),
            "stop".into(),
        );
        for id in ["first-worker", "second-worker"] {
            let (mut link, _) = child_fixture(&parent);
            link.child_agent_id = id.into();
            link.parent_tool_call_id = Some("workflow-call".into());
            parent.children.push((&link).try_into().unwrap());
        }
        let mut state = UiState::default();
        state.rebuild_history(&parent);
        state.notify(NotifyKind::Info, "resumed session");
        let mut projection = SubagentProjection::default();
        projection.restore_summaries(&mut state, &parent);
        let blocks = &state.conversation.blocks;
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0].tool_call_id.as_deref(), Some("workflow-call"));
        assert_eq!(Some(blocks[1].id), projection.runs[0].block);
        assert_eq!(Some(blocks[2].id), projection.runs[1].block);
        assert!(
            matches!(&blocks[3].block.body, phi_tui::BlockBody::Text(text) if text == "parent continuation")
        );
        assert!(blocks[4].local_notification);
    }

    #[test]
    fn recovery_does_not_repeat_child_notifications() {
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (mut link, _) = child_fixture(&parent);
        link.mode = Some(phi_sdk::agent_manager::DelegationMode::Background);
        parent.children.push((&link).try_into().unwrap());
        let mut projection = SubagentProjection::default();
        let mut state = UiState::default();
        projection.reset_for(&AgentInstanceId::new(parent.id.as_str()));
        projection.project_summary(&mut state, link.clone());

        for outcome in [
            SubagentOutcome::Running,
            SubagentOutcome::Finished {
                result: "done".into(),
                is_error: false,
            },
        ] {
            link.outcome = outcome;
            projection.project_summary(&mut state, link.clone());
            let notice_ids: Vec<_> = state
                .conversation
                .blocks
                .iter()
                .map(|block| block.id)
                .collect();
            state.recover_history(&parent);
            projection.restore_summaries(&mut state, &parent);
            assert_eq!(projection.runs[0].link, link);
            assert_eq!(state.subagent_views, [SubagentView::from(&link)]);
            assert!(!projection.project_summary(&mut state, link.clone()));
            assert_eq!(
                state
                    .conversation
                    .blocks
                    .iter()
                    .map(|block| block.id)
                    .collect::<Vec<_>>(),
                notice_ids
            );
        }
        assert_eq!(state.conversation.blocks.len(), 2);

        let mut resumed = UiState::default();
        projection.restore_summaries(&mut resumed, &parent);
        assert!(resumed.conversation.blocks.is_empty());
        assert_eq!(resumed.subagent_views, [SubagentView::from(&link)]);
    }

    #[test]
    fn recovery_preserves_selected_child_observation_and_detail() {
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (link, mut child) = child_fixture(&parent);
        append_child_activity(&mut child);
        parent.children.push((&link).try_into().unwrap());
        let mut projection = SubagentProjection::default();
        let mut state = UiState::default();
        projection.reset_for(&AgentInstanceId::new(parent.id.as_str()));
        projection.selected = Some(link.child_agent_id.clone());
        projection.project_view(
            &mut state,
            link.clone(),
            crate::runtime::SubagentView {
                session: child,
                compact_enabled: Some(true),
                live_run: None,
            },
        );
        let original_block = projection.runs[0].block.unwrap();
        let detail_source = projection.runs[0]
            .detail
            .as_ref()
            .unwrap()
            .1
            .source()
            .clone();

        state.recover_history(&parent);
        projection.restore_summaries(&mut state, &parent);

        assert_eq!(projection.runs[0].link, link);
        assert_eq!(
            projection.active_subagent(),
            Some(link.child_agent_id.as_str())
        );
        assert_eq!(projection.runs[0].compact_enabled, Some(true));
        assert_eq!(
            projection.runs[0].detail.as_ref().unwrap().1.source(),
            &detail_source
        );
        assert_ne!(projection.runs[0].block, Some(original_block));
        assert_eq!(state.subagent_views, [SubagentView::from(&link)]);
        assert!(!projection.project_summary(&mut state, link));
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| !block.local_notification)
        );
    }

    #[test]
    fn changing_parent_resets_child_observations() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (mut link, _) = child_fixture(&parent);
        link.mode = Some(phi_sdk::agent_manager::DelegationMode::Background);
        let mut projection = SubagentProjection::default();
        let mut state = UiState::default();
        projection.reset_for(&AgentInstanceId::new(parent.id.as_str()));
        projection.project_summary(&mut state, link.clone());
        projection.selected = Some(link.child_agent_id.clone());

        let mut replacement =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        link.outcome = SubagentOutcome::Finished {
            result: "done".into(),
            is_error: false,
        };
        replacement.children.push((&link).try_into().unwrap());
        state.rebuild_history(&replacement);
        projection.restore_summaries(&mut state, &replacement);

        assert_eq!(projection.active_subagent(), None);
        assert_eq!(projection.runs.len(), 1);
        assert_eq!(
            projection.runs[0].link,
            SubagentChildLink::from(&replacement.children[0])
        );
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| !block.local_notification)
        );
    }

    #[test]
    fn history_round_trip_preserves_delegation_presentation() {
        use phi_sdk::agent_manager::DelegationMode;

        for mode in [DelegationMode::Foreground, DelegationMode::Background] {
            let mut parent =
                crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
            let (mut link, _) = child_fixture(&parent);
            link.mode = Some(mode);
            parent.children.push((&link).try_into().unwrap());
            let saved = serde_json::to_value(&parent).unwrap();
            let restored: Session = serde_json::from_value(saved).unwrap();
            let mut projection = SubagentProjection::default();
            let mut state = UiState::default();
            projection.restore_summaries(&mut state, &restored);

            assert_eq!(state.subagent_views[0].mode, Some(mode));
            assert_eq!(
                state.conversation.blocks.len(),
                usize::from(mode == DelegationMode::Foreground)
            );
            assert!(
                state
                    .conversation
                    .blocks
                    .iter()
                    .all(|block| block.agent_container)
            );
            projection.restore_summaries(&mut state, &restored);
            assert_eq!(projection.runs.len(), 1);
        }
    }

    #[test]
    fn historical_agent_restoration_precedes_resume_notice() {
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (mut link, _) = child_fixture(&parent);
        link.parent_tool_call_id = None;
        parent.children.push((&link).try_into().unwrap());
        let mut state = UiState::default();
        state.notify(NotifyKind::Info, "resumed session");
        let mut projection = SubagentProjection::default();
        projection.restore_summaries(&mut state, &parent);
        assert!(state.conversation.blocks[0].agent_container);
        assert!(state.conversation.blocks[1].local_notification);
        projection.restore_summaries(&mut state, &parent);
        assert_eq!(state.conversation.blocks.len(), 2);
    }

    #[test]
    fn historical_summary_defers_trace_until_expansion() {
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (mut link, mut child) = child_fixture(&parent);
        append_child_activity(&mut child);
        link.outcome = SubagentOutcome::Interrupted;
        let projection_id = link.child_agent_id.clone();
        parent.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::ToolResult {
                tool_call_id: "agent-call".into(),
                tool_name: "Agent".into(),
                provider_call_id: None,
                provider_item_id: None,
                content: vec![ContentBlock::Text {
                    text: SharedText::new(
                        r#"{"agentId":"unrelated","agent":"misleading-receipt"}"#,
                    ),
                }],
                is_error: false,
                details: None,
            },
        });
        parent.children.push((&link).try_into().unwrap());
        let mut manager = SubagentProjection::default();
        let mut state = UiState::default();
        manager.restore_summaries(&mut state, &parent);
        assert_eq!(state.conversation.blocks.len(), 1);
        assert_eq!(state.conversation.blocks[0].block.title, "[agent] probe");
        assert_eq!(state.subagent_views[0].status, SubagentStatus::Interrupted);
        assert!(!manager.needs_trace(&state, &projection_id));
        assert!(!manager.needs_view(&state, &SubagentChildLink::from(&parent.children[0])));
        assert!(manager.runs[0].detail.is_none());
        manager.selected = Some(projection_id.clone());
        assert!(!manager.project_summary(&mut state, link.clone()));
        assert_eq!(manager.runs.len(), 1);
        assert_eq!(state.subagent_views.len(), 1);
        assert_eq!(manager.active_subagent(), Some("child"));
        assert!(matches!(&state.conversation.blocks[0].block.body,
            phi_tui::BlockBody::Group(text) if text.contains("instanceId: child")));
        assert!(!manager.project_summary(&mut state, link));
        manager.switch_to_main();
        manager.restore_summaries(&mut state, &parent);
        state.conversation.toggle_all_collapsible_blocks();
        assert!(manager.needs_trace(&state, &projection_id));
        let view = crate::runtime::SubagentView {
            session: child,
            compact_enabled: None,
            live_run: None,
        };
        let remote_view = serde_json::from_value(serde_json::to_value(&view).unwrap()).unwrap();
        let link = SubagentChildLink::from(&parent.children[0]);
        assert!(manager.project_view(&mut state, link.clone(), view));
        assert!(!manager.project_view(&mut state, link, remote_view));
        assert!(!manager.needs_trace(&state, &projection_id));
        assert!(manager.runs[0].detail.is_none());
        assert_eq!(state.conversation.blocks.len(), 3);
        assert_eq!(
            state.conversation.blocks[1].parent_id,
            manager.runs[0].block
        );
        assert_eq!(
            state.conversation.blocks[1].block.status,
            BlockStatus::Success
        );
        assert_eq!(
            state.conversation.blocks[1].block.expansion,
            phi_tui::BlockExpansion::Fixed
        );
        assert!(
            matches!(&state.conversation.blocks[1].block.body, phi_tui::BlockBody::Thinking(text) if text.is_empty())
        );
        assert!(
            matches!(&state.conversation.blocks[2].block.body, phi_tui::BlockBody::Output(text) if text.is_empty())
        );
        assert!(
            matches!(&state.conversation.blocks[0].block.body, phi_tui::BlockBody::Group(text) if !text.contains("private"))
        );
        state.conversation.toggle_all_collapsible_blocks();
        state.conversation.toggle_all_collapsible_blocks();
        assert!(!manager.needs_trace(&state, &projection_id));
        let block_count = state.conversation.blocks.len();
        manager.restore_summaries(&mut state, &parent);
        assert!(manager.runs[0].revision.is_some());
        assert_eq!(state.conversation.blocks.len(), block_count);

        let mut resumed = UiState::default();
        manager.restore_summaries(&mut resumed, &parent);
        assert_eq!(resumed.conversation.blocks.len(), 1);
        assert_eq!(resumed.subagent_views.len(), 1);
        assert!(manager.runs[0].revision.is_none());
        assert!(manager.runs[0].detail.is_none());
        assert!(!manager.needs_view(&resumed, &SubagentChildLink::from(&parent.children[0])));
        manager.restore_summaries(&mut resumed, &parent);
        assert_eq!(resumed.conversation.blocks.len(), 1);
        resumed.conversation.toggle_all_collapsible_blocks();
        assert!(manager.needs_view(&resumed, &SubagentChildLink::from(&parent.children[0])));
    }

    #[test]
    fn canonical_reads_preserve_call_identity_and_reattach_without_consuming_history() {
        let mut parent =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        parent.append_assistant_message(
            None,
            None,
            vec![ContentBlock::ToolCall {
                id: "agent-call".into(),
                name: "Agent".into(),
                args: serde_json::json!({"role": "probe"}),
                provider_call_id: None,
                provider_item_id: None,
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage::default(),
            "tool_use".into(),
        );
        let (mut link, mut child) = child_fixture(&parent);
        append_child_activity(&mut child);
        let before = serde_json::to_string(&child).unwrap();
        let mut state = UiState::default();
        state.editor.replace_text("retained editor");
        let mut first = SubagentProjection::default();
        let original_block = state.conversation.project_tool_call_started(
            "agent-call",
            "Agent",
            "{}",
            Default::default(),
        );
        assert!(first.project_session(&mut state, link.clone(), &child));
        assert_eq!(state.subagent_views[0].status, SubagentStatus::Running);
        assert_eq!(
            state.conversation.blocks[0].block.status,
            BlockStatus::Running
        );
        assert_eq!(
            state.conversation.blocks[1].block.status,
            BlockStatus::Running
        );
        parent.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::ToolResult {
                tool_call_id: "agent-call".into(),
                tool_name: "Agent".into(),
                provider_call_id: None,
                provider_item_id: None,
                content: vec![ContentBlock::Text {
                    text: SharedText::new(r#"{"agentId":"child"}"#),
                }],
                is_error: false,
                details: None,
            },
        });
        assert!(!first.project_session(&mut state, link.clone(), &child));
        link.alias = Some("security".into());
        assert!(first.project_session(&mut state, link.clone(), &child));
        assert_eq!(
            state.conversation.blocks[1].block.title,
            "[agent] security (probe)"
        );
        assert_eq!(state.subagent_views[0].alias.as_deref(), Some("security"));
        assert!(first.runs[0].detail.is_none());
        assert_ne!(first.runs[0].block, Some(original_block));
        assert_eq!(state.conversation.blocks[0].id, original_block);
        assert_eq!(Some(state.conversation.blocks[1].id), first.runs[0].block);
        assert_eq!(state.conversation.blocks.len(), 4);
        assert!(
            matches!(&state.conversation.blocks[3].block.body, phi_tui::BlockBody::Output(text) if text.is_empty())
        );
        state.conversation.project_tool_result(
            original_block,
            r#"{"agentId":"child"}"#,
            false,
            None,
        );
        assert_eq!(
            state.conversation.blocks[1].block.status,
            BlockStatus::Running
        );
        assert_eq!(
            state.conversation.blocks[1].block.expansion,
            phi_tui::BlockExpansion::Collapsed
        );
        assert!(
            state.conversation.blocks[0]
                .block
                .title
                .starts_with("Agent")
        );
        refresh_detail(&mut first.runs[0], child.clone(), None).unwrap();
        first.selected = Some(link.child_agent_id.clone());
        let detail = &first.runs[0].detail.as_ref().unwrap().1;
        assert!(detail.blocks.iter().any(|block| matches!(&block.block.body,
                phi_tui::BlockBody::Output(text) if text.contains("private tool body"))));
        assert!(detail.blocks.iter().any(|block| matches!(&block.block.body,
                phi_tui::BlockBody::Text(text) if text.contains("private assistant body"))));
        assert!(state.conversation.toggle_all_collapsible_blocks());
        link.outcome = SubagentOutcome::Finished {
            result: "private outcome".into(),
            is_error: false,
        };
        assert!(first.project_session(&mut state, link.clone(), &child));
        assert_eq!(first.active_subagent(), Some("child"));
        assert_eq!(
            state.conversation.blocks[1].block.expansion,
            phi_tui::BlockExpansion::Expanded
        );
        assert_eq!(
            state.conversation.blocks[1].block.title,
            "[agent] security (probe)"
        );
        assert_eq!(state.subagent_views[0].status, SubagentStatus::Success);
        assert_eq!(state.editor.text(), "retained editor");
        first.switch_to_main();
        assert!(first.runs[0].detail.is_none());
        parent.append_assistant_message(
            None,
            None,
            vec![ContentBlock::Text {
                text: "parent continuation".into(),
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage::default(),
            "stop".into(),
        );
        let mut reattached = SubagentProjection::default();
        let mut reattached_state = UiState::default();
        reattached_state.rebuild_history(&parent);
        let replayed_tool = reattached_state.conversation.blocks[0].id;
        let continuation = reattached_state.conversation.blocks[1].id;
        parent.children.push((&link).try_into().unwrap());
        reattached.restore_summaries(&mut reattached_state, &parent);
        assert_eq!(reattached.runs[0].link.display_name(), "security (probe)");
        assert_eq!(reattached_state.conversation.blocks.len(), 3);
        assert!(!reattached.needs_view(&reattached_state, &link));
        reattached.selected = Some("child".into());
        assert!(reattached.project_session(&mut reattached_state, link, &child));
        assert_eq!(reattached.runs.len(), 1);
        assert_eq!(reattached_state.subagent_views.len(), 1);
        assert_eq!(reattached_state.subagent_views[0].id, "child");
        assert_eq!(reattached.active_subagent(), Some("child"));
        assert_eq!(reattached_state.conversation.blocks.len(), 5);
        assert_eq!(reattached_state.conversation.blocks[0].id, replayed_tool);
        assert_eq!(
            reattached_state.conversation.blocks[0]
                .tool_call_id
                .as_deref(),
            Some("agent-call")
        );
        assert_eq!(
            Some(reattached_state.conversation.blocks[1].id),
            reattached.runs[0].block
        );
        assert_eq!(reattached_state.conversation.blocks[4].id, continuation);
        assert_eq!(
            state
                .conversation
                .blocks
                .iter()
                .filter(|block| block.agent_container || block.parent_id.is_some())
                .count(),
            3
        );
        assert_eq!(before, serde_json::to_string(&child).unwrap());
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PermissionPhase {
        InitialForeground,
        InitialBackground,
        CancelledForeground,
        RetainedFollowup,
    }

    #[tokio::test]
    async fn foreground_child_is_visible_before_tool_result() {
        verify_live_child_permission(PermissionPhase::InitialForeground).await;
    }

    #[tokio::test]
    async fn background_child_keeps_permissions_without_a_trace() {
        verify_live_child_permission(PermissionPhase::InitialBackground).await;
    }

    #[tokio::test]
    async fn foreground_child_cancel_returns_tool_failure() {
        verify_live_child_permission(PermissionPhase::CancelledForeground).await;
    }

    #[tokio::test]
    async fn late_poll_finds_retained_permission() {
        verify_live_child_permission(PermissionPhase::RetainedFollowup).await;
    }

    async fn verify_live_child_permission(phase: PermissionPhase) {
        use crate::cli::Cli;
        use crate::config::Settings;
        use crate::runtime::preparation::{SessionWorkflows, tests::TestConfiguration};
        use crate::runtime::{AgentRuntimeFactory, RuntimeResources, TestRuntime};
        use clap::Parser;
        use phi_sdk::agent_manager::DelegationMode;
        use serde_json::{Value, json};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let root =
            std::path::Path::new("/tmp").join(format!("phi-retained-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
        std::fs::write(
            root.join(".phi/agents/probe.md"),
            "---\nname: probe\ndescription: probe\nmodel: child\ntools: [Bash]\n---\nReview work.",
        )
        .unwrap();
        std::fs::write(
            root.join(".phi/settings.json"),
            json!({
                "defaultProvider":"local", "defaultModel":"test", "memory":{"enabled":false},
                "sandbox":{"enabled":false}, "permission":{"bash":"ask","tools":{"agent":"allow"}},
                "extensions":["!*.lua"]
            })
            .to_string(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mode = if phase == PermissionPhase::InitialBackground {
            DelegationMode::Background
        } else {
            DelegationMode::Foreground
        };
        let server = tokio::spawn(async move {
            let mut child_requests = 0;
            loop {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let request: Value = loop {
                    let mut buffer = [0; 4096];
                    let count = connection.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= start + 4 + length {
                            break serde_json::from_slice(&bytes[start + 4..start + 4 + length])
                                .unwrap();
                        }
                    }
                };
                let (delta, reason) = if request["model"] == "child" {
                    child_requests += 1;
                    if child_requests == 1 && phase == PermissionPhase::RetainedFollowup {
                        (json!({"content":"initial work finished"}), "stop")
                    } else {
                        (
                            json!({"tool_calls":[{"index":0,"id":"ask-bash","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"printf retained-worker\"}"}}]}),
                            "tool_calls",
                        )
                    }
                } else if phase != PermissionPhase::RetainedFollowup
                    && !request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|message| message["role"] == "tool")
                {
                    (
                        json!({"tool_calls":[{"index":0,"id":"agent-call","type":"function","function":{"name":"Agent","arguments":json!({"action":"create","role":"probe","mode":mode,"prompt":"first work"}).to_string()}}]}),
                        "tool_calls",
                    )
                } else {
                    (json!({"content":"parent summary"}), "stop")
                };
                let chunk = json!({"id":"completion","model":request["model"],"choices":[{"delta":delta,"finish_reason":reason}]});
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = connection.write_all(response.as_bytes()).await;
            }
        });
        std::fs::write(root.join("models.json"), json!({"providers":{"local":{"api":"openai-completions","baseUrl":format!("http://{address}/v1"),"models":{"test":{"contextWindow":128000},"child":{"contextWindow":128000}}}}}).to_string()).unwrap();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        let cli = Arc::new(Cli::parse_from(["phi"]));
        let mut config = Settings::default();
        config.default_provider = Some("local".into());
        config.default_model = Some("test".into());
        config.memory = Some(serde_json::from_value(json!({"enabled":false})).unwrap());
        config.permission = Some(json!({"bash":"ask","tools":{"agent":"allow"}}));
        let resources = RuntimeResources::new_for(
            phi_sdk::PhiRuntimeServices::new()
                .model_catalog(catalog.clone())
                .permission_policy(
                    phi_sdk::permission::PermissionConfig {
                        mode: phi_sdk::permission::PermissionMode::Allow,
                        ..Default::default()
                    },
                    Some(root.clone()),
                )
                .interactive(true),
            Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
            catalog.clone(),
            phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(),
            &root,
        );
        let runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
        runtime.preparation.enable_subagents(&runtime.sessions);
        TestConfiguration::new(cli, config, Default::default(), Vec::new(), catalog)
            .install(&runtime.preparation);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let parent = SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone())
            .unwrap()
            .start_session(session, false)
            .await
            .unwrap();
        let creation = if phase != PermissionPhase::RetainedFollowup {
            parent
                .instance()
                .submit_directed(
                    phi_sdk::turn::TurnPrompt {
                        prompt: if mode == DelegationMode::Background {
                            "@probe -b first work"
                        } else {
                            "@probe -f first work"
                        }
                        .into(),
                        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                        tool_policy: Default::default(),
                        injected_context: Vec::new(),
                        images: Vec::new(),
                    },
                    phi_sdk::turn::DirectedDelegation {
                        agent: "probe".into(),
                        mode: Some(mode),
                    },
                )
                .await
                .unwrap();
            let admission = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(snapshot) = parent
                        .instance()
                        .subagent_snapshots()
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|snapshot| {
                            snapshot.status.admission
                                == phi_sdk::agents::SubagentAdmission::Accepted
                        })
                    {
                        assert!(snapshot.status.outcome.is_none());
                        break snapshot.status.creation;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(
                admission.is_ok(),
                "child was not admitted: {:?}",
                parent.instance().snapshot()
            );
            admission.unwrap()
        } else {
            let creation = parent
                .instance()
                .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest::new(
                    "probe",
                    "first work",
                ))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !parent
                    .instance()
                    .subagent_status(creation.instance_id().clone())
                    .await
                    .unwrap()
                    .is_some_and(|status| status.outcome.is_some())
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            runtime
                .manager
                .for_agent(parent.instance().id())
                .unwrap()
                .send_message(creation.instance_id(), "followup")
                .await
                .unwrap();
            creation
        };
        assert_eq!(parent.session().await.children[0].mode, creation.mode());
        let child = runtime.sessions.get(creation.instance_id()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while child.instance().snapshot().pending_interactions.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let attachment = AgentAttachment::new(
            &parent,
            runtime.sessions.session_access(parent.instance().id()),
            runtime.preparation.clone(),
        );
        let mut parent_view = parent.session().await;
        parent_view.set_transcript(attachment.transcript().await.unwrap());
        let mut projection = SubagentProjection::default();
        let mut state = UiState::default();
        state.features.activities = true;
        state.rebuild_history(&parent_view);
        let foreground = matches!(
            phase,
            PermissionPhase::InitialForeground | PermissionPhase::CancelledForeground
        );
        if foreground {
            let links = attachment.subagents().await.unwrap();
            let link = links
                .iter()
                .find(|link| link.child_agent_id == creation.instance_id().as_str())
                .unwrap();
            assert_eq!(link.admission, phi_sdk::agents::SubagentAdmission::Accepted);
            assert_eq!(link.outcome, SubagentOutcome::Running);
            assert!(parent_view.current_branch().iter().all(|entry| !matches!(
                &entry.kind,
                TranscriptEntryKind::Message { message: AgentMessage::ToolResult { tool_call_id, .. } }
                    if tool_call_id.as_str() == "agent-call"
            )));
        }
        projection.poll(&attachment, &mut state).await.unwrap();
        if phase == PermissionPhase::InitialBackground {
            let link = attachment
                .subagents()
                .await
                .unwrap()
                .into_iter()
                .find(|link| link.child_agent_id == creation.instance_id().as_str())
                .unwrap();
            assert_eq!(link.outcome, SubagentOutcome::Running);
            assert!(projection.runs[0].block.is_none());
            assert!(projection.runs[0].detail.is_none());
            assert!(!projection.needs_view(&state, &link));
            assert!(
                state
                    .conversation
                    .blocks
                    .iter()
                    .all(|block| !block.agent_container && block.parent_id.is_none())
            );
            assert!(state.conversation.blocks.iter().any(|block| matches!(&block.block.body, phi_tui::BlockBody::Notify(notice) if notice.message.ends_with("running in background"))));
            assert!(
                projection
                    .open(link.child_agent_id.clone(), &attachment)
                    .await
                    .unwrap()
            );
            assert!(projection.runs[0].detail.is_some());
            assert!(projection.runs[0].block.is_none());
            assert!(projection.needs_view(&state, &link));
            projection.switch_to_main();
            assert!(projection.runs[0].detail.is_none());
            assert!(!projection.needs_view(&state, &link));
        }
        if foreground {
            assert_eq!(
                state.subagent_views.len(),
                1,
                "SDK and application expose the running child, but UI omitted it"
            );
            assert_eq!(state.subagent_views[0].status, SubagentStatus::Running);
            let tool = state
                .conversation
                .blocks
                .iter()
                .find(|block| block.tool_call_id.as_deref() == Some("agent-call"))
                .unwrap();
            assert_eq!(tool.block.status, BlockStatus::Running);
            crate::interactive::select_subagent_view(
                &attachment,
                &mut state,
                creation.instance_id().as_str(),
            )
            .await;
            assert!(
                projection
                    .open(creation.instance_id().as_str().into(), &attachment)
                    .await
                    .unwrap()
            );
            assert_eq!(
                projection.active_subagent(),
                Some(creation.instance_id().as_str())
            );
        }
        let (_, interactions) = projection
            .interaction_owner(&attachment, None)
            .await
            .unwrap();
        assert!(
            !interactions.is_empty(),
            "initial completion hid a live child permission"
        );
        if foreground {
            state.conversation.restore_live_run(&attachment.snapshot());
            let mut events = attachment.subscribe();
            let expected_output = if phase == PermissionPhase::InitialForeground {
                attachment.abort().await.unwrap();
                "agent tool call interrupted; subagent is still running"
            } else {
                projection
                    .cancel(creation.instance_id().as_str(), &attachment)
                    .await
                    .unwrap();
                "subagent cancelled"
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    super::super::agent::drain_agent_events(
                        &attachment,
                        &mut events,
                        &mut state,
                        &mut parent_view,
                    )
                    .await;
                    if !state.conversation.run_active_projection() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let tool = state
                .conversation
                .blocks
                .iter()
                .find(|block| block.tool_call_id.as_deref() == Some("agent-call"))
                .unwrap();
            assert_eq!(tool.block.status, BlockStatus::Error);
            assert!(matches!(&tool.block.body, phi_tui::BlockBody::Output(text)
            if text.starts_with(expected_output)
                && text.contains(if phase == PermissionPhase::InitialForeground {
                    "Observe the existing execution"
                } else {
                    "Do not automatically retry or recreate"
                })));
            parent_view.set_transcript(attachment.transcript().await.unwrap());
            let mut resumed = UiState::default();
            resumed.rebuild_history(&parent_view);
            let restored = resumed
                .conversation
                .blocks
                .iter()
                .find(|block| block.tool_call_id.as_deref() == Some("agent-call"))
                .unwrap();
            assert_eq!(restored.block.status, tool.block.status);
            assert!(matches!((&restored.block.body, &tool.block.body),
                (phi_tui::BlockBody::Output(restored), phi_tui::BlockBody::Output(live))
                    if **restored == **live));
            projection.poll(&attachment, &mut state).await.unwrap();
            if phase == PermissionPhase::InitialForeground {
                assert_eq!(state.subagent_views[0].status, SubagentStatus::Running);
                assert!(!child.instance().snapshot().pending_interactions.is_empty());
                assert!(projection.runs[0].block.is_none());
                assert!(
                    state
                        .conversation
                        .blocks
                        .iter()
                        .all(|block| !block.agent_container && block.parent_id.is_none())
                );
            } else {
                assert_eq!(state.subagent_views[0].status, SubagentStatus::Cancelled);
                let container = projection.runs[0].block.unwrap();
                assert_eq!(
                    state
                        .conversation
                        .blocks
                        .iter()
                        .find(|block| block.id == container)
                        .unwrap()
                        .block
                        .status,
                    BlockStatus::Error
                );
            }
        }
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_selected_view_advances_timing_without_hidden_ticks() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (link, child) = child_fixture(&parent);
        let mut manager = SubagentProjection::default();
        let mut state = UiState::default();
        let main = state.conversation.project_tool_call_started(
            "main",
            "Bash",
            "{}",
            phi_sdk::tools::ToolDisplay::default(),
        );
        let started = state.conversation.tool_block_meta[&main]
            .tool_timer
            .unwrap()
            .started_at;
        manager.project_summary(&mut state, link.clone());
        manager.selected = Some(link.child_agent_id);
        refresh_detail(&mut manager.runs[0], child, None).unwrap();
        let detail = &mut manager.runs[0].detail.as_mut().unwrap().1;
        let tool = detail.project_tool_call_started(
            "child",
            "Bash",
            "{}",
            phi_sdk::tools::ToolDisplay::default(),
        );
        let observed =
            detail.tool_block_meta[&tool].tool_timer.unwrap().started_at + Duration::from_secs(2);
        assert!(manager.advance_clocks(&mut state, observed));
        assert_eq!(
            state.conversation.blocks[state.conversation.block_positions[&main]]
                .block
                .footer
                .as_ref()
                .unwrap()
                .text
                .trim(),
            "Elapsed 0.0s"
        );
        assert!(!manager.advance_clocks(&mut state, observed));
        manager.switch_to_main();
        assert!(manager.advance_clocks(&mut state, started + Duration::from_secs(60)));
        assert_eq!(
            state.conversation.blocks[state.conversation.block_positions[&main]]
                .block
                .footer
                .as_ref()
                .unwrap()
                .text
                .trim(),
            "Elapsed 60.0s"
        );
    }

    #[test]
    fn failed_detail_refresh_retains_view_and_sets_retry() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (link, mut child) = child_fixture(&parent);
        let mut manager = SubagentProjection::default();
        let mut state = UiState::default();
        manager.project_summary(&mut state, link.clone());
        manager.selected = Some(link.child_agent_id.clone());
        child.append_user_message("retained");
        refresh_detail(&mut manager.runs[0], child.clone(), None).unwrap();
        let detail = &manager.runs[0].detail.as_ref().unwrap().1;
        let source = detail.source().clone();
        let id = detail.blocks[0].id;
        let leaf = child.leaf_id.clone();
        child.leaf_id = Some("missing".into());
        assert!(refresh_detail(&mut manager.runs[0], child.clone(), None).is_err());
        assert!(manager.runs[0].detail_pending);
        assert!(manager.needs_view(&state, &link));
        let detail = &manager.runs[0].detail.as_ref().unwrap().1;
        assert_eq!(detail.source(), &source);
        assert_eq!(detail.blocks[0].id, id);
        child.leaf_id = leaf;
        assert!(!refresh_detail(&mut manager.runs[0], child.clone(), None).unwrap());
        assert!(!manager.runs[0].detail_pending);
        child.append_user_message("retry");
        assert!(refresh_detail(&mut manager.runs[0], child, None).unwrap());
        let (session, detail) = manager.runs[0].detail.as_ref().unwrap();
        assert_eq!(detail.source(), &source);
        assert_eq!(detail.blocks[0].id, id);
        assert_eq!(detail.blocks.len(), 2);
        assert!(session.entries.is_empty());
    }

    #[test]
    fn child_refresh_and_return_preserve_parent_draft_and_retained_window() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (link, mut child) = child_fixture(&parent);
        let mut manager = SubagentProjection::default();
        let mut state = UiState::default();
        state.editor.set_text("parent draft");
        state.conversation.blocks_expanded = Some(false);
        manager.project_summary(&mut state, link.clone());
        let parent_block = state.conversation.blocks[0].id;
        state.conversation.buffer_initialized = true;
        state.conversation.buffer_first_block = Some(parent_block);
        state.conversation.painted_history_revision = 12;
        manager.selected = Some(link.child_agent_id);
        refresh_detail(&mut manager.runs[0], child.clone(), None).unwrap();
        let detail = &mut manager.runs[0].detail.as_mut().unwrap().1;
        detail.blocks_expanded = Some(true);
        detail.buffer_initialized = true;
        detail.painted_history_revision = 9;
        let source = detail.source().clone();
        append_child_activity(&mut child);
        refresh_detail(&mut manager.runs[0], child, None).unwrap();
        let (session, detail) = manager.runs[0].detail.as_ref().unwrap();
        assert!(session.entries.is_empty());
        assert!(!detail.blocks.is_empty());
        assert_eq!(detail.blocks_expanded, Some(true));
        assert!(detail.buffer_initialized);
        assert_eq!(detail.painted_history_revision, 9);
        assert_eq!(detail.source(), &source);
        manager.switch_to_main();
        assert!(manager.runs[0].detail.is_none());
        assert_eq!(state.editor.text(), "parent draft");
        assert_eq!(state.conversation.blocks_expanded, Some(false));
        assert_eq!(state.conversation.buffer_first_block, Some(parent_block));
        assert_eq!(state.conversation.painted_history_revision, 12);
        assert_eq!(state.conversation.blocks[0].id, parent_block);
    }

    #[tokio::test]
    async fn selected_child_recovers_live_and_completed_details_without_leaking_parent_payloads() {
        let parent = crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let (mut link, mut child) = child_fixture(&parent);
        let mut manager = SubagentProjection::default();
        let mut state = UiState::default();
        let mut live = phi_sdk::agent_instance::LiveRunSnapshot {
            assistant_text: "partial private assistant".into(),
            reasoning_text: "partial private reasoning".into(),
            tool: None,
        };
        let view = |live| crate::runtime::SubagentView {
            session: child.clone(),
            compact_enabled: Some(false),
            live_run: Some(live),
        };
        assert!(manager.project_view(&mut state, link.clone(), view(live.clone())));
        assert!(manager.runs[0].detail.is_none());
        assert_eq!(
            state.conversation.blocks[1].parent_id,
            manager.runs[0].block
        );
        assert_eq!(
            state.conversation.blocks[1].block.expansion,
            phi_tui::BlockExpansion::Fixed
        );
        assert!(
            matches!(&state.conversation.blocks[1].block.body, phi_tui::BlockBody::Thinking(text) if text.is_empty())
        );
        assert!(!manager.project_view(&mut state, link.clone(), view(live.clone())));
        manager.selected = Some(link.child_agent_id.clone());
        assert!(manager.project_view(&mut state, link.clone(), view(live.clone())));
        assert!(!manager.project_view(&mut state, link.clone(), view(live.clone())));
        assert_eq!(manager.runs[0].compact_enabled, Some(false));
        let mut refreshed = view(live.clone());
        refreshed.compact_enabled = Some(true);
        assert!(manager.project_view(&mut state, link.clone(), refreshed));
        assert_eq!(manager.runs[0].compact_enabled, Some(true));
        let detail = &manager.runs[0].detail.as_ref().unwrap().1;
        assert!(detail.run_active_projection());
        assert!(detail.blocks.iter().any(|block| matches!(&block.block.body,
                phi_tui::BlockBody::Text(text) if text.contains("partial private assistant"))));
        assert!(detail.blocks.iter().any(|block| matches!(&block.block.body,
                phi_tui::BlockBody::Thinking(text) if text.contains("partial private reasoning"))));
        live.assistant_text.push_str(" continued");
        assert!(manager.project_view(&mut state, link.clone(), view(live.clone())));
        live.reasoning_text.clear();
        live.tool = Some(phi_sdk::agent_instance::LiveToolSnapshot {
            id: "live-call".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": "ls -la"}),
            display: None,
        });
        assert!(manager.project_view(&mut state, link.clone(), view(live)));
        assert_eq!(state.conversation.blocks.len(), 3);
        assert_eq!(
            state.conversation.blocks[1].block.status,
            BlockStatus::Success
        );
        assert!(
            matches!(&state.conversation.blocks[1].block.body, phi_tui::BlockBody::Thinking(text) if text.is_empty())
        );
        assert!(
            matches!(&state.conversation.blocks[2].block.body, phi_tui::BlockBody::Output(text) if text.is_empty())
        );
        assert_eq!(child.entries.len(), 0);
        append_child_activity(&mut child);
        link.outcome = SubagentOutcome::Finished {
            result: "done".into(),
            is_error: false,
        };
        manager.project_summary(&mut state, link.clone());
        assert!(!manager.needs_trace(&state, &link.child_agent_id));
        assert!(manager.needs_view(&state, &link));
        manager.project_view(
            &mut state,
            link.clone(),
            crate::runtime::SubagentView {
                session: child,
                compact_enabled: None,
                live_run: None,
            },
        );
        assert!(!manager.needs_view(&state, &link));
        assert!(manager.runs[0].compact_enabled.is_none());
        let detail = &manager.runs[0].detail.as_ref().unwrap().1;
        assert!(!detail.run_active_projection());
        assert!(detail.blocks.iter().any(|block| matches!(&block.block.body,
                phi_tui::BlockBody::Output(text) if text.contains("private tool body"))));
        manager.switch_to_main();
        assert!(manager.runs[0].detail.is_none());
    }
}
