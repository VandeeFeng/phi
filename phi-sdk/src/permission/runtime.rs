use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::core::interaction::{
    PermissionDecision, PermissionInteraction, PermissionRequest, PermissionSender,
    active_interaction_scope,
};
use crate::permission::Action;
use crate::permission::checker::{CheckResult, PermCheck};
use crate::permission::command::CommandPermissionRequest;

#[derive(Clone)]
pub struct PermissionRuntime {
    checker: PermCheck,
    interaction_tx: Option<PermissionSender>,
    interactive: bool,
}

impl PermissionRuntime {
    pub(crate) fn open() -> Self {
        let config = crate::permission::PermissionConfig {
            mode: crate::permission::PermissionMode::Allow,
            ..Default::default()
        };
        Self::new(Arc::new(Mutex::new(
            crate::permission::checker::PermissionChecker::new(&config, None),
        )))
    }

    pub fn new(checker: PermCheck) -> Self {
        Self {
            checker,
            interaction_tx: None,
            interactive: false,
        }
    }

    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    pub(crate) fn with_interaction_sender(mut self, sender: Option<PermissionSender>) -> Self {
        self.interaction_tx = sender;
        self
    }

    pub(crate) fn outer_turn_policy(
        &self,
        intent: crate::core::turn::TurnToolPolicy,
    ) -> crate::permission::execution_scope::OuterTurnPolicy {
        crate::permission::execution_scope::OuterTurnPolicy::new(Some(self.checker.clone()), intent)
    }

    pub(crate) fn current_tool_policy(&self) -> crate::core::turn::TurnToolPolicy {
        crate::permission::execution_scope::OuterTurnPolicy::current(&self.checker)
    }

    pub fn load_session_allowlist(&self, allowlist: &[(String, String)]) {
        self.checker
            .lock()
            .unwrap()
            .load_session_allowlist(allowlist);
    }

    pub(crate) fn inherit_session_grants(&self, previous: &Self) {
        let grants = previous.checker.lock().unwrap().session_allowlist();
        self.load_session_allowlist(&grants);
    }

    pub fn add_session_allowlist(&self, tool: impl Into<String>, pattern: &str) {
        self.checker
            .lock()
            .unwrap()
            .add_session_allowlist(tool.into(), pattern);
    }

    pub async fn authorize_tool(
        &self,
        tool: &str,
        input: &str,
        preview: Option<String>,
        origin: Option<String>,
    ) -> anyhow::Result<()> {
        let result = self.check_tool(tool, input);
        self.resolve(result, tool, input.to_string(), preview, origin)
            .await
    }

    pub(crate) async fn authorize_tool_with_exact_consent(
        &self,
        tool: &str,
        input: &str,
        consent_input: Option<&str>,
    ) -> anyhow::Result<()> {
        let result = self.check_tool(tool, input);
        if matches!(result, CheckResult::Ask) && consent_input == Some(input) {
            return Ok(());
        }
        self.resolve(result, tool, input.to_string(), None, None)
            .await
    }

    pub async fn authorize_path(
        &self,
        tool: &str,
        path: &str,
        origin: Option<String>,
    ) -> anyhow::Result<()> {
        self.authorize_path_with_preview(tool, path, origin, || async { Ok(None) })
            .await
    }

    pub async fn authorize_path_with_preview<F, Fut>(
        &self,
        tool: &str,
        path: &str,
        origin: Option<String>,
        preview: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<Option<String>>>,
    {
        let result = self.checker.lock().unwrap().check_path_with_preapprovals(
            tool,
            path,
            &self.current_tool_policy().allowances,
        );
        let preview = if matches!(result, CheckResult::Ask) {
            preview().await?
        } else {
            None
        };
        self.resolve(result, tool, path.to_string(), preview, origin)
            .await
    }

    pub async fn authorize_bash(
        &self,
        command: &str,
        original_command: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut evidence = Vec::new();
        if let Some(original) = original_command.filter(|original| *original != command) {
            evidence.push(original);
        }
        evidence.push(command);
        let result = self
            .checker
            .lock()
            .unwrap()
            .check_shell_evidence_with_preapprovals(
                evidence.iter().copied(),
                &self.current_tool_policy().allowances,
            );
        self.resolve(result, "bash", command.to_string(), None, None)
            .await
    }

    pub async fn authorize_command(
        &self,
        request: &CommandPermissionRequest<'_>,
    ) -> anyhow::Result<()> {
        let result = self
            .checker
            .lock()
            .unwrap()
            .check_command_with_preapprovals(request, &self.current_tool_policy().allowances);
        self.resolve(
            result,
            "bash",
            request.permission_input(),
            None,
            request.origin.map(ToString::to_string),
        )
        .await
    }

    fn check_tool(&self, tool: &str, input: &str) -> CheckResult {
        self.checker.lock().unwrap().check_with_preapprovals(
            tool,
            input,
            &self.current_tool_policy().allowances,
        )
    }

    async fn resolve(
        &self,
        result: CheckResult,
        tool: &str,
        input: String,
        preview: Option<String>,
        origin: Option<String>,
    ) -> anyhow::Result<()> {
        match result {
            CheckResult::Allowed => Ok(()),
            CheckResult::Denied(reason) => anyhow::bail!("Permission denied: {reason}"),
            CheckResult::Ask => self.ask(tool, input, preview, origin).await,
        }
    }

    async fn ask(
        &self,
        tool: &str,
        input: String,
        preview: Option<String>,
        origin: Option<String>,
    ) -> anyhow::Result<()> {
        let interaction_tx = self.interaction_tx.clone();
        let Some(interaction_tx) = interaction_tx.filter(|_| self.interactive) else {
            return match self.checker.lock().unwrap().non_interactive_ask() {
                Action::Allow => Ok(()),
                Action::Ask | Action::Deny => {
                    anyhow::bail!("Permission denied (non-interactive mode)")
                }
            };
        };
        self.send_request(interaction_tx, tool, input, preview, origin)
            .await
    }

    async fn send_request(
        &self,
        interaction_tx: PermissionSender,
        tool: &str,
        input: String,
        preview: Option<String>,
        origin: Option<String>,
    ) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let (applied_tx, applied_rx) = tokio::sync::oneshot::channel();
        interaction_tx
            .send(PermissionRequest {
                scope: active_interaction_scope(),
                permission: PermissionInteraction {
                    tool: tool.to_string(),
                    input,
                    preview,
                    origin,
                },
                reply: reply_tx,
                applied: applied_rx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Permission system unavailable"))?;
        let result = self.apply_decision(tool, reply_rx.await);
        let _ = applied_tx.send(());
        result
    }

    fn apply_decision(
        &self,
        tool: &str,
        decision: Result<PermissionDecision, tokio::sync::oneshot::error::RecvError>,
    ) -> anyhow::Result<()> {
        match decision {
            Ok(PermissionDecision::AllowOnce) => Ok(()),
            Ok(PermissionDecision::AllowAlways(pattern)) => {
                self.checker
                    .lock()
                    .unwrap()
                    .add_session_allowlist(tool.to_string(), &pattern);
                Ok(())
            }
            _ => anyhow::bail!("Permission denied by user"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::PermissionRuntime;
    use crate::core::interaction::PermissionDecision;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::command::CommandPermissionRequest;
    use crate::permission::{Action, PermissionConfig, ToolPerm};

    fn runtime(action: Action) -> PermissionRuntime {
        let config = PermissionConfig {
            bash: Some(ToolPerm::Granular(HashMap::from([(
                "rtk **".to_string(),
                action,
            )]))),
            ..PermissionConfig::default()
        };
        let checker = PermissionChecker::new(&config, None);
        PermissionRuntime::new(Arc::new(Mutex::new(checker)))
    }

    #[tokio::test]
    async fn outer_turn_preapprovals_are_isolated_and_retired() {
        use crate::core::turn::TurnToolPolicy;
        use crate::permission::execution_scope::OuterTurnPolicy;

        let other = runtime(Action::Ask);
        let runtime = runtime(Action::Ask);
        let scope = OuterTurnPolicy::new(
            Some(runtime.checker.clone()),
            TurnToolPolicy {
                allowances: vec![("Bash".into(), "**".into()), ("Read".into(), "**".into())],
                removals: Vec::new(),
            },
        );
        scope
            .run(async {
                assert!(runtime.authorize_bash("rtk /repo/file", None).await.is_ok());
                assert!(
                    runtime
                        .authorize_path("Read", "/outside/file", None)
                        .await
                        .is_ok()
                );
                assert!(other.authorize_bash("rtk /repo/file", None).await.is_err());
                assert!(
                    tokio::spawn({
                        let runtime = runtime.clone();
                        async move { runtime.authorize_bash("rtk /repo/file", None).await }
                    })
                    .await
                    .unwrap()
                    .is_err()
                );
            })
            .await;
        assert!(
            runtime
                .authorize_bash("rtk /repo/file", None)
                .await
                .is_err()
        );
        assert!(
            runtime
                .authorize_path("Read", "/outside/file", None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn outer_turn_preapprovals_use_all_existing_authorization_evidence() {
        use crate::core::turn::TurnToolPolicy;

        for mode in ["ask", "deny", "yolo"] {
            let config = PermissionConfig::try_from_value(serde_json::json!({
                "mode": mode,
                "bash": {"rm **": "deny", "git status **": "ask"},
                "read": {"/protected/**": "deny", "relative/**": "ask"},
                "external_directory": {"/outside/**": "deny"},
                "tools": {"domain": {"blocked.example": "deny", "safe.example": "ask"}}
            }))
            .unwrap();
            let runtime = PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(
                &config,
                Some("/workspace".into()),
            ))));
            runtime
                .outer_turn_policy(TurnToolPolicy {
                    allowances: vec![
                        ("Bash".into(), "git status **".into()),
                        ("Read".into(), "**".into()),
                        ("domain".into(), "safe.example".into()),
                    ],
                    removals: Vec::new(),
                })
                .run(async {
                    let args = vec!["status".into(), "/repo".into()];
                    assert!(
                        runtime
                            .authorize_command(&CommandPermissionRequest::new(
                                "/usr/bin/git",
                                &args
                            ))
                            .await
                            .is_ok()
                    );
                    assert!(
                        runtime
                            .authorize_bash("git status /repo/file", None)
                            .await
                            .is_ok()
                    );
                    assert!(
                        runtime
                            .authorize_path("Read", "relative/file", None)
                            .await
                            .is_ok()
                    );
                    assert!(
                        runtime
                            .authorize_tool("domain", "safe.example", None, None)
                            .await
                            .is_ok()
                    );
                    for result in [
                        runtime
                            .authorize_bash("git status", Some("rm /repo/file"))
                            .await,
                        runtime
                            .authorize_bash("git status && rm /repo/file", None)
                            .await,
                        runtime
                            .authorize_path("Read", "/protected/file", None)
                            .await,
                        runtime.authorize_path("Read", "/outside/file", None).await,
                        runtime
                            .authorize_tool("domain", "blocked.example", None, None)
                            .await,
                        runtime.authorize_tool("unknown", "{}", None, None).await,
                    ] {
                        assert_eq!(result.is_ok(), mode == "yolo");
                    }
                    assert_eq!(
                        runtime.authorize_bash("git push /repo", None).await.is_ok(),
                        mode == "yolo"
                    );
                    assert_eq!(
                        runtime
                            .authorize_bash("git push", Some("git status"))
                            .await
                            .is_ok(),
                        mode == "yolo"
                    );
                })
                .await;
        }
    }

    #[tokio::test]
    async fn noninteractive_ask_uses_policy_without_emitting_requests() {
        for action in [Action::Allow, Action::Ask, Action::Deny] {
            for (interactive, bind_sender) in [(false, true), (true, false)] {
                let config = PermissionConfig {
                    bash: Some(ToolPerm::Simple(Action::Ask)),
                    non_interactive_ask: Some(action),
                    ..PermissionConfig::default()
                };
                let checker = PermissionChecker::new(&config, None);
                let runtime =
                    PermissionRuntime::new(Arc::new(Mutex::new(checker))).interactive(interactive);
                let (sender, mut requests) = tokio::sync::mpsc::channel(1);
                let runtime = runtime.with_interaction_sender(bind_sender.then_some(sender));
                let result = runtime.authorize_tool("bash", "printf", None, None).await;
                assert_eq!(result.is_ok(), action == Action::Allow);
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn allow_always_is_applied_before_the_answer_returns() {
        use crate::core::interaction::InteractionResponse;
        use crate::harness::instance::interaction::InteractionBroker;
        use crate::hooks::HookRegistry;

        let runtime = runtime(Action::Ask).interactive(true);
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let runtime = runtime.with_interaction_sender(Some(sender));
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.authorize_tool("bash", "rtk gain", None, None).await }
        });
        let mut broker = InteractionBroker::new(Some(receiver), None);
        let hooks = HookRegistry::new();
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(request) = broker.poll(&hooks).await {
                    break request;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let answer = broker
            .respond(
                &request.request_id,
                InteractionResponse::Permission(PermissionDecision::AllowAlways("rtk gain".into())),
                &hooks,
            )
            .await
            .unwrap();
        assert_eq!(
            answer.allow_always,
            Some(("bash".into(), "rtk gain".into()))
        );
        assert!(
            runtime
                .interactive(false)
                .authorize_tool("bash", "rtk gain", None, None)
                .await
                .is_ok()
        );
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn routes_ask_through_the_permission_channel() {
        let runtime = runtime(Action::Ask).interactive(true);
        let (permission_tx, mut permission_rx) = tokio::sync::mpsc::channel(1);
        let runtime = runtime.with_interaction_sender(Some(permission_tx));
        let args = vec!["gain".to_string()];
        let request = CommandPermissionRequest::new("rtk", &args);
        let non_interactive = runtime.clone().interactive(false);
        assert!(
            non_interactive
                .authorize_command(&request)
                .await
                .unwrap_err()
                .to_string()
                .contains("non-interactive")
        );
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                let args = vec!["gain".to_string()];
                let request = CommandPermissionRequest::new("rtk", &args);
                runtime.authorize_command(&request).await
            }
        });
        let request = permission_rx.recv().await.unwrap();

        assert_eq!(request.permission.tool, "bash");
        assert_eq!(request.permission.input, "rtk gain");
        request.reply.send(PermissionDecision::AllowOnce).unwrap();
        assert!(task.await.unwrap().is_ok());
    }
}
