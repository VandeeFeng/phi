use std::time::{Duration, Instant};

use crate::runtime::WorkspaceView;
use crate::runtime::attachment::AgentAttachment;
use crate::session::Session;

use super::state::UiState;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(super) fn git_branch<'a>(state: &'a UiState, session: &Session) -> Option<&'a str> {
    state
        .workspace
        .as_ref()
        .filter(|view| view.working_dir == std::path::Path::new(session.working_dir.as_str()))
        .and_then(|view| view.git_branch.as_deref())
}

#[derive(Default)]
pub(super) struct WorkspaceRefresh {
    attachment_id: Option<phi_sdk::agent_instance::AgentInstanceId>,
    session_id: Option<String>,
    next_refresh: Option<Instant>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<WorkspaceView>>>,
}

impl WorkspaceRefresh {
    pub(super) async fn poll_target(
        &mut self,
        attachment: &AgentAttachment,
        target: super::render::RenderTarget<'_>,
    ) -> bool {
        match target {
            super::render::RenderTarget::Main { state, session }
            | super::render::RenderTarget::Subagent {
                terminal: state,
                session,
                ..
            } => self.poll(attachment, session, state).await,
        }
    }

    async fn poll(
        &mut self,
        attachment: &AgentAttachment,
        session: &Session,
        state: &mut UiState,
    ) -> bool {
        let mut changed = false;
        if self.attachment_id.as_ref() != Some(attachment.id())
            || self.session_id.as_deref() != Some(session.id.as_str())
        {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.attachment_id = Some(attachment.id().clone());
            self.session_id = Some(session.id.to_string());
            self.next_refresh = None;
            changed |= state.workspace.take().is_some();
        }
        if self.task.as_ref().is_some_and(|task| task.is_finished()) {
            if let Ok(Ok(view)) = self.task.take().unwrap().await
                && view.working_dir == std::path::Path::new(session.working_dir.as_str())
                && state.workspace.as_ref() != Some(&view)
            {
                state.workspace = Some(view);
                changed = true;
            }
            self.next_refresh = Some(Instant::now() + REFRESH_INTERVAL);
        }
        if self.task.is_none() && self.next_refresh.is_none_or(|next| Instant::now() >= next) {
            let attachment = attachment.clone();
            let session_id = session.id.to_string();
            self.task = Some(tokio::spawn(async move {
                attachment.workspace(&session_id).await
            }));
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_follows_display_target_and_discards_stale_results() {
        let (directory, policy, runtime) =
            crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let child_directory = directory.join("child");
        std::fs::create_dir_all(&child_directory).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet", "--initial-branch", "child-branch"])
                .arg(&child_directory)
                .status()
                .unwrap()
                .success()
        );
        let mut parent = crate::session::new("local", "test", 128_000, &directory);
        let mut child = crate::session::new("local", "test", 128_000, &child_directory);
        child.bind_test_parent(&parent);
        child.append_user_message("saved child");
        crate::session::storage::save_session(&child).unwrap();
        parent.children.push(
            (&crate::session::SubagentChildLink {
                child_session_id: child.id.clone(),
                child_agent_id: "saved-child".into(),
                child_working_dir: child.working_dir.clone(),
                definition: None,
                alias: None,
                parent_tool_call_id: None,
                admission: phi_sdk::agents::SubagentAdmission::Accepted,
                outcome: crate::session::SubagentOutcome::Interrupted,
                mode: None,
            })
                .into(),
        );
        let workflows = crate::runtime::preparation::SessionWorkflows::new(
            runtime.sessions.clone(),
            runtime.preparation.clone(),
        )
        .unwrap();
        let binding = workflows
            .start_session(parent.clone(), false)
            .await
            .unwrap();
        let attachment = AgentAttachment::new(
            &binding,
            runtime.sessions.session_access(binding.instance().id()),
            runtime.preparation.clone(),
        );
        assert!(attachment.workspace("unknown-child").await.is_err());
        let other_session = crate::session::new("local", "test", 128_000, &directory);
        let other_binding = workflows
            .start_session(other_session.clone(), false)
            .await
            .unwrap();
        let other_attachment = AgentAttachment::new(
            &other_binding,
            runtime
                .sessions
                .session_access(other_binding.instance().id()),
            runtime.preparation.clone(),
        );
        let mut refresh = WorkspaceRefresh {
            attachment_id: Some(attachment.id().clone()),
            session_id: Some("previous".into()),
            next_refresh: None,
            task: None,
        };
        let mut state = UiState::default();
        let mut conversation =
            super::super::components::conversation::ConversationView::child_from_session(
                &child, None,
            );
        for (attachment, session) in [
            (&attachment, &child),
            (&attachment, &parent),
            (&attachment, &child),
            (&other_attachment, &other_session),
        ] {
            let stale = WorkspaceView {
                working_dir: session.working_dir.as_str().into(),
                git_branch: Some("stale".into()),
            };
            state.workspace = Some(stale.clone());
            refresh.task = Some(tokio::spawn(async move { Ok(stale) }));
            while !refresh.task.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
            let target = if session.id == child.id {
                super::super::render::RenderTarget::Subagent {
                    conversation: &mut conversation,
                    session,
                    compact_enabled: None,
                    terminal: &mut state,
                }
            } else {
                super::super::render::RenderTarget::Main {
                    state: &mut state,
                    session,
                }
            };
            assert!(refresh.poll_target(attachment, target).await);
            assert!(state.workspace.is_none());
            tokio::time::timeout(Duration::from_secs(3), async {
                while state.workspace.is_none() {
                    tokio::task::yield_now().await;
                    refresh.poll(attachment, session, &mut state).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                state.workspace.as_ref().unwrap().working_dir,
                std::path::Path::new(session.working_dir.as_str())
            );
            assert_eq!(
                git_branch(&state, session),
                if session.id == child.id {
                    Some("child-branch")
                } else {
                    None
                }
            );
            let accepted = state.workspace.clone();
            let foreign = WorkspaceView {
                working_dir: directory.join("foreign"),
                git_branch: Some("foreign".into()),
            };
            state.workspace = Some(foreign.clone());
            assert_eq!(git_branch(&state, session), None);
            state.workspace = accepted.clone();
            refresh.task = Some(tokio::spawn(async move { Ok(foreign) }));
            while !refresh.task.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
            assert!(!refresh.poll(attachment, session, &mut state).await);
            assert_eq!(state.workspace, accepted);
        }
        assert!(
            runtime
                .manager
                .get(&phi_sdk::agent_instance::AgentInstanceId::new(
                    child.id.as_str()
                ))
                .is_err()
        );
        drop(refresh);
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

impl Drop for WorkspaceRefresh {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
