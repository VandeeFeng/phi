use std::collections::HashSet;
use std::pin::Pin;

use futures_util::FutureExt;
use phi_sdk::agent_manager::AgentRegistrationId;
use phi_sdk::workflow::{WorkflowRunId, WorkflowStatus};
use phi_tui::{StyledLine, ThemeColor};

use crate::interactive::state::UiState;
use crate::runtime::attachment::AgentAttachment;
use crate::runtime::session::WorkflowView;

const MAX_VISIBLE_RUNS: usize = 3;
const MAX_SUMMARY_CHARS: usize = 80;

#[derive(Default)]
pub(in crate::interactive) struct WorkflowStatusView {
    views: Vec<WorkflowView>,
    dismissed: HashSet<WorkflowRunId>,
    loaded: bool,
}

impl WorkflowStatusView {
    fn replace(&mut self, views: Vec<WorkflowView>) -> bool {
        let views: Vec<_> = views
            .into_iter()
            .filter(|view| !self.dismissed.contains(&view.snapshot.id()))
            .collect();
        if self.loaded && self.views == views {
            return false;
        }
        self.views = views;
        self.loaded = true;
        true
    }

    fn observe(&mut self, root: &AgentAttachment) {
        if let Ok(Some(registration)) = root.registration_id()
            && let Ok(views) = root.workflow_views(registration)
        {
            self.replace(views);
        }
    }

    pub(in crate::interactive) fn observed_runs(
        &mut self,
        root: &AgentAttachment,
    ) -> HashSet<WorkflowRunId> {
        self.observe(root);
        self.views.iter().map(|view| view.snapshot.id()).collect()
    }

    pub(in crate::interactive) fn dismiss_settled(
        &mut self,
        root: &AgentAttachment,
        observed_runs: &HashSet<WorkflowRunId>,
    ) {
        self.observe(root);
        self.dismissed.extend(
            self.views
                .iter()
                .filter(|view| {
                    observed_runs.contains(&view.snapshot.id()) && terminal(view.snapshot.status())
                })
                .map(|view| view.snapshot.id()),
        );
        self.views
            .retain(|view| !self.dismissed.contains(&view.snapshot.id()));
    }

    pub(in crate::interactive) fn lines(&self, width: u16) -> Vec<StyledLine> {
        if self.views.is_empty() {
            return Vec::new();
        }
        let mut views: Vec<_> = self.views.iter().collect();
        views.sort_by_key(|view| match view.snapshot.status() {
            WorkflowStatus::Running | WorkflowStatus::Finishing | WorkflowStatus::Stopping => 0,
            WorkflowStatus::Failed => 1,
            WorkflowStatus::Stopped => 2,
            WorkflowStatus::Completed => 3,
        });
        let running = views
            .iter()
            .filter(|view| !terminal(view.snapshot.status()))
            .count();
        let settled: usize = views.iter().map(|view| view.snapshot.completed()).sum();
        let requested: usize = views
            .iter()
            .map(|view| view.snapshot.invocations().len())
            .sum();
        let mut lines = Vec::new();
        if running > 0 {
            lines.push(
                StyledLine::new(
                    format!("Workflows · {running} running · {settled}/{requested} settled"),
                    ThemeColor::Dim,
                )
                .fit(width),
            );
        }
        for view in views.iter().take(MAX_VISIBLE_RUNS) {
            let snapshot = &view.snapshot;
            let (marker, color, label) = match snapshot.status() {
                WorkflowStatus::Running => (
                    "[-]",
                    ThemeColor::Warning,
                    snapshot.phase().filter(|phase| !phase.trim().is_empty()),
                ),
                WorkflowStatus::Finishing => ("[-]", ThemeColor::Warning, Some("Finishing")),
                WorkflowStatus::Stopping => ("[-]", ThemeColor::Warning, Some("Stopping")),
                WorkflowStatus::Completed => ("[x]", ThemeColor::Success, Some("completed")),
                WorkflowStatus::Failed => ("[!]", ThemeColor::Error, Some("failed")),
                WorkflowStatus::Stopped => ("[x]", ThemeColor::Error, Some("stopped")),
            };
            let mut summary = view.name.clone();
            if let Some(label) = label {
                summary.push_str(&format!(" · {label}"));
            }
            if snapshot.status() == WorkflowStatus::Failed {
                if let Some(error) = snapshot.error() {
                    summary.push_str(" · ");
                    summary.push_str(&short_text(error));
                }
            } else {
                summary.push_str(&format!(
                    " · {}/{} settled",
                    snapshot.completed(),
                    snapshot.invocations().len()
                ));
                if snapshot.status() == WorkflowStatus::Completed
                    && let Some(value) = snapshot.value()
                {
                    let conclusion = match value {
                        serde_json::Value::String(text) => {
                            std::borrow::Cow::Borrowed(text.as_str())
                        }
                        other => std::borrow::Cow::Owned(other.to_string()),
                    };
                    if !conclusion.trim().is_empty() {
                        summary.push_str(" · ");
                        summary.push_str(&short_text(&conclusion));
                    }
                }
                if !terminal(snapshot.status()) && snapshot.active() > 0 {
                    summary.push_str(&format!(" · {} active", snapshot.active()));
                }
            }
            let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
            lines.push(
                StyledLine::segments([
                    ("  ".to_owned(), ThemeColor::Dim),
                    (marker.to_owned(), color),
                    (format!(" {summary}"), ThemeColor::Default),
                ])
                .fit(width),
            );
        }
        if views.len() > MAX_VISIBLE_RUNS {
            lines.push(
                StyledLine::new(
                    format!("  +{} workflows", views.len() - MAX_VISIBLE_RUNS),
                    ThemeColor::Dim,
                )
                .fit(width),
            );
        }
        lines
    }
}

fn terminal(status: WorkflowStatus) -> bool {
    matches!(
        status,
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Stopped
    )
}

fn short_text(value: &str) -> String {
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = single_line.chars();
    let summary: String = chars.by_ref().take(MAX_SUMMARY_CHARS).collect();
    if chars.next().is_some() {
        format!("{summary}…")
    } else {
        summary
    }
}

#[derive(Default)]
pub(in crate::interactive) struct WorkflowRefresh {
    registration: Option<AgentRegistrationId>,
    notification: Option<Pin<Box<tokio::sync::futures::OwnedNotified>>>,
}

impl WorkflowRefresh {
    pub(in crate::interactive) fn poll(
        &mut self,
        root: &AgentAttachment,
        state: &mut UiState,
    ) -> bool {
        let registration = root.registration_id().ok().flatten();
        let rebound = self.registration != registration;
        if rebound {
            self.registration = registration;
            self.notification = root.workflow_changed().ok().map(Box::pin);
            state.workflows = WorkflowStatusView::default();
        }
        let notified = self
            .notification
            .as_mut()
            .is_some_and(|notification| notification.as_mut().now_or_never().is_some());
        if notified {
            self.notification = root.workflow_changed().ok().map(Box::pin);
        }
        if rebound || notified || !state.workflows.loaded {
            let views = registration
                .and_then(|registration| root.workflow_views(registration).ok())
                .unwrap_or_default();
            return state.workflows.replace(views) || rebound;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn status_respects_admission() {
        use phi_sdk::hooks::Hook;
        use phi_sdk::lifecycle::{SessionEndReason, UserPromptSubmitOutcome};
        use phi_sdk::workflow::{WorkflowLimits, WorkflowRequest};

        let submission_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let submission_release = std::sync::Arc::new(tokio::sync::Notify::new());
        let (root, policy, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap();
        let config = crate::config::Settings::default();
        let cli = <crate::cli::Cli as clap::Parser>::parse_from(["phi"]);
        let binding = runtime
            .sessions
            .create(
                &runtime.preparation,
                session.clone(),
                crate::runtime::RuntimeRebuildOptions {
                    phi: crate::runtime::PhiBuildOptions {
                        cli: &cli,
                        provider: "local",
                        model: "test",
                        runtime_cfg: config.runtime_config(&catalog),
                        context: phi_sdk::context::load_with_options_for(
                            false,
                            None,
                            Vec::new(),
                            &root,
                        ),
                        agent_options: Default::default(),
                        hooks: vec![Hook::user_prompt_submit({
                            let submission_started = submission_started.clone();
                            let submission_release = submission_release.clone();
                            move |event| {
                                let submission_started = submission_started.clone();
                                let submission_release = submission_release.clone();
                                async move {
                                    if event.prompt == "delayed" {
                                        submission_started.notify_one();
                                        submission_release.notified().await;
                                    }
                                    Ok(if matches!(event.prompt.as_str(), "accepted" | "delayed") {
                                        UserPromptSubmitOutcome::Continue {
                                            notices: Vec::new(),
                                            prompt: event.prompt,
                                            injected_context: Vec::new(),
                                        }
                                    } else {
                                        UserPromptSubmitOutcome::Handled { reason: None }
                                    })
                                }
                            }
                        })],
                    },
                    instance: crate::runtime::AgentInstanceBuildOptions::from_session(&session),
                },
                phi_sdk::lifecycle::SessionStartReason::New,
                false,
            )
            .await
            .unwrap();
        let attachment = AgentAttachment::new(
            &binding,
            runtime.sessions.session_access(binding.instance().id()),
            runtime.preparation.clone(),
        );
        let request = WorkflowRequest::parse(
            "export const meta = {name: 'status', description: 'status lifetime'}; return 'done';",
            None,
        )
        .await
        .unwrap();
        let mut state = UiState::default();
        state.features.workflow_observation = true;
        let mut refresh = WorkflowRefresh::default();
        for _ in 0..2 {
            let previous = state.workflows.observed_runs(&attachment);
            let run = binding
                .launch_workflow(
                    runtime.workflows.clone(),
                    request.clone(),
                    None,
                    WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(),
                    None,
                )
                .await
                .unwrap();
            let mut subscription = run.subscribe();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !terminal(subscription.snapshot().status()) {
                    subscription.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            assert!(refresh.poll(&attachment, &mut state));
            state.workflows.dismiss_settled(&attachment, &previous);
            assert_eq!(state.workflows.views.len(), 1);
            assert_eq!(state.workflows.views[0].snapshot.id(), run.id());
            assert!(
                state
                    .workflows
                    .lines(80)
                    .iter()
                    .any(|line| line.text.contains("done"))
            );
        }
        for (prompt, directed, expected) in [
            ("handled", None, 1),
            (
                "rejected",
                Some(phi_sdk::turn::DirectedDelegation {
                    agent: "missing".into(),
                    mode: None,
                }),
                1,
            ),
            ("accepted", None, 0),
        ] {
            crate::interactive::agent::submit_prompt(
                phi_sdk::turn::TurnPrompt {
                    prompt: prompt.into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    images: Vec::new(),
                    injected_context: Vec::new(),
                    tool_policy: Default::default(),
                },
                prompt.into(),
                prompt.into(),
                &mut state,
                &mut session,
                &attachment,
                phi_sdk::turn::SubmissionDispatch::Steer,
                directed,
            )
            .await;
            refresh.poll(&attachment, &mut state);
            assert_eq!(state.workflows.views.len(), expected, "{prompt}");
        }
        let pending = WorkflowRequest::parse(
            "export const meta = {name: 'pending', description: 'wait for stop'}; return await new Promise(() => {});",
            None,
        )
        .await
        .unwrap();
        let run = binding
            .launch_workflow(
                runtime.workflows.clone(),
                pending,
                None,
                WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(),
                None,
            )
            .await
            .unwrap();
        assert!(refresh.poll(&attachment, &mut state));
        assert_eq!(
            state.workflows.views[0].snapshot.status(),
            WorkflowStatus::Running
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                crate::interactive::agent::submit_prompt(
                    phi_sdk::turn::TurnPrompt {
                        prompt: "delayed".into(),
                        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                        images: Vec::new(),
                        injected_context: Vec::new(),
                        tool_policy: Default::default(),
                    },
                    "delayed".into(),
                    "delayed".into(),
                    &mut state,
                    &mut session,
                    &attachment,
                    phi_sdk::turn::SubmissionDispatch::Steer,
                    None,
                ),
                async {
                    submission_started.notified().await;
                    binding
                        .stop_workflow(binding.instance().registration_id().unwrap(), run.id())
                        .await
                        .unwrap();
                    submission_release.notify_one();
                }
            );
        })
        .await
        .unwrap();
        refresh.poll(&attachment, &mut state);
        assert!(state.workflows.views.is_empty());
        runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn error_summary_is_one_bounded_line() {
        for word in ["detail ", "错误\r\n", "é\t"] {
            for count in 0..=MAX_SUMMARY_CHARS * 2 {
                let error = word.repeat(count);
                let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
                let summary = short_text(&error);
                assert!(!summary.contains(['\n', '\r', '\t']));
                assert!(summary.chars().count() <= MAX_SUMMARY_CHARS + 1);
                if normalized.chars().count() > MAX_SUMMARY_CHARS {
                    assert!(summary.ends_with('…'));
                } else {
                    assert_eq!(summary, normalized);
                }
            }
        }
    }

    #[test]
    fn empty_status_has_no_rows() {
        assert!(WorkflowStatusView::default().lines(80).is_empty());
    }
}
