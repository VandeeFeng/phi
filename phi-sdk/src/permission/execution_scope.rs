use crate::core::turn::TurnToolPolicy;
use crate::permission::checker::PermCheck;
use std::future::Future;
use std::sync::{Arc, Mutex};

tokio::task_local! {
    static ACTIVE_POLICY: OuterTurnPolicy;
}

#[derive(Clone)]
pub(crate) struct OuterTurnPolicy {
    checker: Option<PermCheck>,
    tools: Option<Arc<rig::tool::ToolSet>>,
    state: Arc<Mutex<Option<TurnToolPolicy>>>,
}

impl OuterTurnPolicy {
    pub(crate) fn new(checker: Option<PermCheck>, intent: TurnToolPolicy) -> Self {
        let mut policy = TurnToolPolicy::default();
        policy.merge(&intent);
        Self {
            checker,
            tools: None,
            state: Arc::new(Mutex::new(Some(policy))),
        }
    }

    pub(crate) fn with_tools(mut self, tools: Arc<rig::tool::ToolSet>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub(crate) fn is_removed(tools: &Arc<rig::tool::ToolSet>, name: &str) -> bool {
        ACTIVE_POLICY
            .try_with(|scope| {
                scope
                    .tools
                    .as_ref()
                    .is_some_and(|owner| Arc::ptr_eq(owner, tools))
                    && scope
                        .state
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|policy| policy.removals.iter().any(|removed| removed == name))
            })
            .unwrap_or(false)
    }

    pub(crate) async fn run<T>(self, future: impl Future<Output = T>) -> T {
        let retirement = PolicyRetirement(self.clone());
        let result = ACTIVE_POLICY.scope(self, future).await;
        drop(retirement);
        result
    }

    pub(crate) fn retire(&self) {
        *self.state.lock().unwrap() = None;
    }

    pub(crate) fn capture() -> Option<Self> {
        ACTIVE_POLICY.try_with(Clone::clone).ok()
    }

    pub(crate) async fn with_captured_async<T>(
        scope: Option<Self>,
        future: impl Future<Output = T>,
    ) -> T {
        match scope {
            Some(scope) => ACTIVE_POLICY.scope(scope, future).await,
            None => future.await,
        }
    }

    pub(crate) fn with_captured<T>(scope: Option<Self>, work: impl FnOnce() -> T) -> T {
        match scope {
            Some(scope) => ACTIVE_POLICY.sync_scope(scope, work),
            None => work(),
        }
    }

    pub(crate) fn merge_active(intent: &TurnToolPolicy) {
        let _ = ACTIVE_POLICY.try_with(|scope| {
            if let Some(policy) = scope.state.lock().unwrap().as_mut() {
                policy.merge(intent);
            }
        });
    }

    pub(crate) fn current(checker: &PermCheck) -> TurnToolPolicy {
        ACTIVE_POLICY
            .try_with(|scope| {
                if scope
                    .checker
                    .as_ref()
                    .is_some_and(|owner| Arc::ptr_eq(owner, checker))
                {
                    scope.state.lock().unwrap().clone().unwrap_or_default()
                } else {
                    TurnToolPolicy::default()
                }
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::runtime::PermissionRuntime;
    use crate::permission::{PermissionConfig, PermissionMode};

    #[tokio::test]
    async fn concurrent_and_nested_execution_scopes_retire_on_success_and_failure() {
        let runtime = PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfig::default(),
            None,
        ))));
        let read = runtime.outer_turn_policy(TurnToolPolicy {
            allowances: vec![("Read".into(), "**".into())],
            ..Default::default()
        });
        let bash = runtime.outer_turn_policy(TurnToolPolicy {
            allowances: vec![("Bash".into(), "**".into())],
            ..Default::default()
        });
        let read_state = read.clone();
        let bash_state = bash.clone();
        let (success, failure) = tokio::join!(
            read.run(async {
                tokio::task::yield_now().await;
                assert!(
                    runtime
                        .authorize_path("Read", "/multiline\nfile", None)
                        .await
                        .is_ok()
                );
                assert!(runtime.authorize_bash("pwd", None).await.is_err());
                runtime
                    .outer_turn_policy(TurnToolPolicy::default())
                    .run(async {
                        assert!(runtime.authorize_path("Read", "/file", None).await.is_err());
                    })
                    .await;
                assert!(runtime.authorize_path("Read", "/file", None).await.is_ok());
                Ok::<(), ()>(())
            }),
            bash.run(async {
                tokio::task::yield_now().await;
                assert!(
                    runtime
                        .authorize_bash("printf '/repo'\npwd", None)
                        .await
                        .is_ok()
                );
                assert!(runtime.authorize_path("Read", "/file", None).await.is_err());
                Err::<(), ()>(())
            }),
        );
        assert!(success.is_ok());
        assert!(failure.is_err());
        assert!(read_state.state.lock().unwrap().is_none());
        assert!(bash_state.state.lock().unwrap().is_none());
        assert!(runtime.current_tool_policy().allowances.is_empty());
    }

    #[tokio::test]
    async fn runner_retirement_is_final() {
        #[derive(Clone, Copy)]
        enum RunnerEnd {
            Abort,
            Drop,
        }

        let checker = Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfig::default(),
            None,
        )));
        let other_checker = Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfig::default(),
            None,
        )));
        let tools = Arc::new(rig::tool::ToolSet::default());
        let other_tools = Arc::new(rig::tool::ToolSet::default());
        let intent = TurnToolPolicy {
            allowances: vec![("Read".into(), "**".into())],
            removals: vec!["Read".into()],
        };
        for bound_checker in [Some(checker.clone()), None] {
            for bound_tools in [Some(tools.clone()), None] {
                for end in [RunnerEnd::Abort, RunnerEnd::Drop] {
                    let mut scope = OuterTurnPolicy::new(bound_checker.clone(), intent.clone());
                    if let Some(tools) = &bound_tools {
                        scope = scope.with_tools(tools.clone());
                    }
                    let retained = scope.clone();
                    scope
                        .run(async {
                            assert_eq!(
                                OuterTurnPolicy::current(&checker).allowances.is_empty(),
                                bound_checker.is_none()
                            );
                            assert!(
                                OuterTurnPolicy::current(&other_checker)
                                    .allowances
                                    .is_empty()
                            );
                            assert_eq!(
                                OuterTurnPolicy::is_removed(&tools, "Read"),
                                bound_tools.is_some()
                            );
                            assert!(!OuterTurnPolicy::is_removed(&other_tools, "Read"));
                            let captured = OuterTurnPolicy::capture();
                            let (_, event_rx) = tokio::sync::mpsc::channel(1);
                            let runner = crate::harness::agentic_loop::AgentRunner {
                                tool_policy: Some(retained.clone()),
                                round_requests: None,
                                event_rx,
                                steering: Arc::new(
                                    crate::harness::agentic_loop::AgentSteerMailbox::new(
                                        Default::default(),
                                    ),
                                ),
                                task: tokio::spawn(std::future::pending()),
                            };
                            match end {
                                RunnerEnd::Abort => runner.abort(),
                                RunnerEnd::Drop => drop(runner),
                            }
                            assert!(retained.state.lock().unwrap().is_none());
                            OuterTurnPolicy::with_captured(captured, || {
                                OuterTurnPolicy::merge_active(&intent);
                                assert!(OuterTurnPolicy::current(&checker).allowances.is_empty());
                                assert!(!OuterTurnPolicy::is_removed(&tools, "Read"));
                            });
                        })
                        .await;
                    assert!(retained.state.lock().unwrap().is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn blocking_scope_is_explicit_and_retires_on_abort() {
        let checker = Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfig {
                mode: PermissionMode::Ask,
                ..Default::default()
            },
            None,
        )));
        let runtime = PermissionRuntime::new(checker.clone());
        let scope = OuterTurnPolicy::new(
            Some(checker.clone()),
            TurnToolPolicy {
                allowances: vec![("Read".into(), "**".into())],
                removals: Vec::new(),
            },
        );
        let held = scope.clone();
        let blocking_runtime = runtime.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(scope.run(async move {
            let captured = OuterTurnPolicy::capture();
            let checker = checker.clone();
            tokio::task::spawn_blocking(move || {
                let handle = tokio::runtime::Handle::current();
                OuterTurnPolicy::with_captured(captured, || {
                    assert!(
                        handle
                            .block_on(blocking_runtime.authorize_path("Read", "/file", None))
                            .is_ok()
                    );
                });
                assert!(
                    handle
                        .block_on(blocking_runtime.authorize_path("Read", "/file", None))
                        .is_err()
                );
                assert!(OuterTurnPolicy::current(&checker).allowances.is_empty());
            })
            .await
            .unwrap();
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        ready_rx.await.unwrap();
        let (_, event_rx) = tokio::sync::mpsc::channel(1);
        let mut runner = crate::harness::agentic_loop::AgentRunner {
            tool_policy: Some(held.clone()),
            round_requests: None,
            event_rx,
            steering: Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                Default::default(),
            )),
            task,
        };
        runner.abort();
        assert!(held.state.lock().unwrap().is_none());
        assert!((&mut runner.task).await.unwrap_err().is_cancelled());
        OuterTurnPolicy::with_captured(Some(held), || {
            assert!(runtime.current_tool_policy().allowances.is_empty());
        });
        assert!(runtime.authorize_path("Read", "/file", None).await.is_err());
    }
}

struct PolicyRetirement(OuterTurnPolicy);

impl Drop for PolicyRetirement {
    fn drop(&mut self) {
        self.0.retire();
    }
}
