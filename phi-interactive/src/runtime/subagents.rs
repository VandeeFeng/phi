//! Child runtime preparation and Session association; SDK owns admission and execution.

use std::sync::Arc;

use crate::session::Session;
#[cfg(test)]
use phi_sdk::agent_instance::AgentInstanceId;

use super::session::AgentSession;
use super::{AgentRuntime, PreparedAgentRuntime, associations::SessionAssociations};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SubagentView {
    pub(crate) session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compact_enabled: Option<bool>,
    pub(crate) live_run: Option<phi_sdk::agent_instance::LiveRunSnapshot>,
}

#[derive(Clone)]
pub(super) struct ChildRuntimePolicy {
    pub(super) sessions: std::sync::Weak<SessionAssociations>,
}

impl phi_sdk::agents::SubagentPreparation for ChildRuntimePolicy {
    fn prepare(
        &self,
        creation: phi_sdk::agents::AgentCreation,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<phi_sdk::agents::PreparedSubagent, String>>
                + Send,
        >,
    > {
        let policy = self.clone();
        Box::pin(async move {
            policy
                .prepare_child(creation, cancellation)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

impl ChildRuntimePolicy {
    async fn prepare_child_session(
        &self,
        parent: &AgentSession,
        child: &mut Session,
        task_list: Option<phi_sdk::tasks::TaskListBinding>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let (session, runtime) = parent.runtime_seed().await;
        child.provider = session.provider;
        child.model = session.model;
        child.context_window = session.context_window;
        child.current_switch = session.current_switch;
        runtime.prepare_child(child, self.clone(), task_list).await
    }

    async fn prepare_child(
        self,
        creation: phi_sdk::agents::AgentCreation,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<phi_sdk::agents::PreparedSubagent> {
        let sessions = self
            .sessions
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Session associations are unavailable"))?;
        let parent = sessions.find(creation.creator_id())?.ok_or_else(|| {
            anyhow::anyhow!(
                "missing Session association for agent `{}`",
                creation.creator_id()
            )
        })?;
        let parent_session = parent.session().await;
        let mut child = crate::session::new(
            &parent_session.provider,
            &parent_session.model,
            parent_session.context_window,
            creation.working_dir(),
        );
        child.id = creation.instance_id().as_str().into();
        child.name = format!("agent:{}", creation.definition()).into();
        child.bind_task_list(creation.task_list())?;
        child.current_switch = parent_session.current_switch.clone();
        let prepared = self
            .prepare_child_session(&parent, &mut child, creation.task_list().cloned())
            .await?;
        let (candidate, runtime) = prepared.into_parts();
        let association = Arc::new(ChildAssociation {
            sessions,
            parent,
            cancellation,
            prepared: std::sync::Mutex::new(Some((child, runtime))),
            child: std::sync::Mutex::new(None),
        });
        Ok(phi_sdk::agents::PreparedSubagent::new(
            candidate,
            association,
        ))
    }
}

struct ChildAssociation {
    sessions: Arc<SessionAssociations>,
    parent: Arc<AgentSession>,
    cancellation: tokio::sync::watch::Receiver<bool>,
    prepared: std::sync::Mutex<Option<(Session, AgentRuntime)>>,
    child: std::sync::Mutex<Option<Arc<AgentSession>>>,
}

impl phi_sdk::agents::SubagentAssociation for ChildAssociation {
    fn associate(
        &self,
        instance: phi_sdk::agent_instance::AgentInstance,
        creation: phi_sdk::agents::AgentCreation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.bind(instance, creation)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn rollback(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let child = self
                .child
                .lock()
                .map_err(|_| "child association is unavailable".to_string())?
                .take();
            if let Some(child) = child {
                self.sessions
                    .discard_child_session(&child)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }
}

impl ChildAssociation {
    async fn bind(
        &self,
        instance: phi_sdk::agent_instance::AgentInstance,
        creation: phi_sdk::agents::AgentCreation,
    ) -> anyhow::Result<()> {
        let (mut session, runtime) = self
            .prepared
            .lock()
            .map_err(|_| anyhow::anyhow!("child runtime is unavailable"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("child runtime already associated"))?;
        anyhow::ensure!(
            instance.id() == creation.instance_id()
                && self.parent.instance().id() == creation.creator_id()
                && session.id == creation.instance_id().as_str()
                && instance.task_list() == creation.task_list()
                && std::path::Path::new(session.working_dir.as_str()) == creation.working_dir(),
            "prepared child does not match creation"
        );
        session.bind_creation(&creation, &self.parent.session().await)?;
        let mut cancellation = self.cancellation.clone();
        let child = self
            .sessions
            .associate_child_session(
                creation.creator_id(),
                session,
                runtime,
                instance,
                &mut cancellation,
            )
            .await?;
        *self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("child association is unavailable"))? =
            Some(child.clone());
        // Published child links must already resolve, including during Workflow preparation.
        self.parent
            .record_child_intent(crate::session::ChildSessionLink::from_creation(&creation))
            .await?;
        self.sessions
            .start_agent_session(
                &child,
                phi_sdk::lifecycle::SessionStartReason::New,
                Some(creation.creator_id().as_str()),
            )
            .await;
        anyhow::ensure!(!*cancellation.borrow(), "child admission cancelled");
        Ok(())
    }
}

#[cfg(test)]
impl super::associations::SessionAccess {
    pub(super) async fn child_completion(
        &self,
        instance_id: &phi_sdk::agent_instance::AgentInstanceId,
    ) -> anyhow::Result<phi_sdk::agents::SubagentCompletion> {
        let child = self.child(instance_id.as_str()).await?.binding()?;
        Ok(child
            .instance()
            .complete_delegated_agent(tokio::sync::watch::channel(false).1)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use std::time::Duration;

    use super::super::{
        AgentInstanceBuildOptions, AgentRuntimeFactory, PhiBuildOptions, RuntimeRebuildOptions,
        RuntimeResources, TestRuntime,
    };

    struct PromptPreparation {
        policy: ChildRuntimePolicy,
        factory: Arc<AgentRuntimeFactory>,
        entered: tokio::sync::mpsc::UnboundedSender<phi_sdk::agents::AgentCreation>,
        ended: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl phi_sdk::agents::SubagentPreparation for PromptPreparation {
        fn prepare(
            &self,
            creation: phi_sdk::agents::AgentCreation,
            cancellation: tokio::sync::watch::Receiver<bool>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<phi_sdk::agents::PreparedSubagent, String>>
                    + Send,
            >,
        > {
            let sessions = self.policy.sessions.upgrade().unwrap();
            let factory = self.factory.clone();
            let entered = self.entered.clone();
            let ended = self.ended.clone();
            Box::pin(async move {
                let result = async {
                    let parent = sessions
                        .find(creation.creator_id())?
                        .ok_or_else(|| anyhow::anyhow!("parent Session is unavailable"))?;
                    assert!(parent.session().await.children.iter().all(|link| {
                        link.child_session_id.as_str() != creation.instance_id().as_str()
                    }));
                    let mut session = crate::session::new(
                        "local",
                        "test",
                        128_000,
                        &std::env::current_dir().unwrap(),
                    );
                    session.id = creation.instance_id().as_str().into();
                    session.working_dir = creation.working_dir().to_string_lossy().into();
                    session.bind_task_list(creation.task_list())?;
                    let prompt_creation = creation.clone();
                    let prompt_hook = phi_sdk::hooks::Hook::user_prompt_submit(move |_| {
                        let creation = prompt_creation.clone();
                        let entered = entered.clone();
                        async move {
                            entered.send(creation.clone()).unwrap();
                            if creation.prompt() == "cancel-input" {
                                std::future::pending::<()>().await;
                            }
                            Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Handled {
                                reason: Some("intercepted task".into()),
                            })
                        }
                    });
                    let session_id = session.id.clone();
                    let obstruct = creation.prompt() == "cancel-input";
                    let end_hook = phi_sdk::hooks::Hook::session_end(move |_| {
                        ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if obstruct {
                            let saved =
                                crate::session::storage::load_session_id(&session_id).unwrap();
                            let end_path = crate::session::storage::session_path(&saved);
                            std::fs::remove_file(&end_path).unwrap();
                            std::fs::create_dir(&end_path).unwrap();
                        }
                        async { Ok(Vec::new()) }
                    });
                    let cli = Cli::parse_from(["phi"]);
                    let instance = AgentInstanceBuildOptions::for_child(
                        &session,
                        creation.task_list().cloned(),
                    );
                    let runtime_config = parent.runtime().runtime_config();
                    let prepared = factory
                        .prepare(RuntimeRebuildOptions {
                            phi: PhiBuildOptions {
                                cli: &cli,
                                provider: "local",
                                model: "test",
                                runtime_cfg: runtime_config,
                                context: phi_sdk::context::load_with_options_for(
                                    false,
                                    None,
                                    vec![creation.working_dir().join(".phi/agents")],
                                    creation.working_dir(),
                                ),
                                agent_options: crate::AgentBuildOptions {
                                    no_tools: true,
                                    ..Default::default()
                                },
                                hooks: vec![prompt_hook, end_hook],
                            },
                            instance,
                        })
                        .await?;
                    let (candidate, runtime) = prepared.into_parts();
                    Ok(phi_sdk::agents::PreparedSubagent::new(
                        candidate,
                        Arc::new(ChildAssociation {
                            sessions,
                            parent,
                            cancellation,
                            prepared: std::sync::Mutex::new(Some((session, runtime))),
                            child: std::sync::Mutex::new(None),
                        }),
                    ))
                }
                .await;
                result.map_err(|error: anyhow::Error| error.to_string())
            })
        }
    }

    #[tokio::test]
    async fn child_preparation_rejects_missing_parent_configuration() {
        let (root, configuration, runtime) =
            super::super::preparation::tests::session_test_runtime();
        configuration.install(&runtime.preparation);
        let cli = Cli::parse_from(["phi"]);
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        let settings = crate::config::Settings::default();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let parent = runtime
            .sessions
            .create(
                &runtime.preparation,
                session.clone(),
                RuntimeRebuildOptions {
                    phi: PhiBuildOptions {
                        cli: &cli,
                        provider: "local",
                        model: "test",
                        runtime_cfg: settings.runtime_config(&catalog),
                        context: phi_sdk::context::load(true),
                        agent_options: crate::AgentBuildOptions {
                            no_tools: true,
                            ..Default::default()
                        },
                        hooks: Vec::new(),
                    },
                    instance: AgentInstanceBuildOptions::from_session(&session),
                },
                phi_sdk::lifecycle::SessionStartReason::Startup,
                false,
            )
            .await
            .unwrap();
        let policy = ChildRuntimePolicy {
            sessions: Arc::downgrade(&runtime.sessions),
        };
        let result = policy
            .prepare_child_session(&parent, &mut session, None)
            .await;
        assert!(matches!(result, Err(error) if error.to_string().contains("source configuration")));
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn child_preparation_preserves_environment_until_reload() {
        use super::super::preparation::tests::{install_reload_catalog, run_isolated_test};
        if run_isolated_test(concat!(
            module_path!(),
            "::child_preparation_preserves_environment_until_reload"
        )) {
            return;
        }
        let (root, _, _) = super::super::preparation::tests::session_test_runtime();
        let extension = root.join(".phi/extensions/accepted.lua");
        let skill = root.join(".phi/skills/review/SKILL.md");
        let prompt = root.join(".phi/prompts/review.md");
        for resource in [&extension, &skill, &prompt] {
            std::fs::create_dir_all(resource.parent().unwrap()).unwrap();
        }
        std::fs::write(
            &extension,
            "register_command('accepted', {handler = function() end})",
        )
        .unwrap();
        std::fs::write(
            &skill,
            "---\nname: review\ndescription: accepted skill\n---\naccepted skill body",
        )
        .unwrap();
        std::fs::write(&prompt, "accepted prompt").unwrap();
        std::fs::write(root.join("models.json"), br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000},"selected":{"contextWindow":64000}}}}}"#).unwrap();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        install_reload_catalog(&root, &catalog);
        let settings_manager = crate::config::SettingsManager::from_settings(
            &root,
            Default::default(),
            serde_json::from_value(serde_json::json!({
                "temperature": 0.2, "permission": {"bash":"ask"}, "no_tools": true,
            }))
            .unwrap(),
        )
        .unwrap();
        let settings = settings_manager.settings();
        let cli = Arc::new(Cli::parse_from(["phi"]));
        let resources = AgentRuntimeFactory::resources_for(
            &cli,
            &settings,
            &settings_manager,
            catalog.clone(),
            &root,
        )
        .unwrap();
        let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
        test_runtime.preparation.initialize(
            &root,
            Arc::new(Cli::parse_from(["phi"])),
            settings,
            settings_manager,
            Vec::new(),
            catalog.clone(),
        );
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        session
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: "bash".into(),
                pattern: "printf parent-only".into(),
            });
        let parent = super::super::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .start_session(session, false)
        .await
        .unwrap();
        test_runtime
            .preparation
            .configure_runtime(
                &test_runtime.sessions.session_access(parent.instance().id()),
                super::super::preparation::RuntimeSelection::Model {
                    selection: "selected".into(),
                },
            )
            .await
            .unwrap();
        let policy = ChildRuntimePolicy {
            sessions: Arc::downgrade(&test_runtime.sessions),
        };
        let child_dir = root.join("child");
        std::fs::create_dir_all(child_dir.join(".phi")).unwrap();
        std::fs::write(child_dir.join(".phi/settings.json"), "invalid JSON").unwrap();
        for (settings_content, working_dir) in [
            (
                r#"{"defaultModel":"missing","temperature":0.8,"skills":["!**"]}"#,
                &root,
            ),
            ("invalid JSON", &child_dir),
        ] {
            std::fs::write(root.join(".phi/settings.json"), settings_content).unwrap();
            std::fs::write(&extension, "invalid Lua source").unwrap();
            std::fs::write(
                &skill,
                "---\nname: review\ndescription: unaccepted skill\n---\nchanged skill",
            )
            .unwrap();
            std::fs::write(&prompt, "unaccepted prompt").unwrap();
            let mut child =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            child.working_dir = working_dir.to_string_lossy().into();
            let prepared = policy
                .prepare_child_session(&parent, &mut child, None)
                .await
                .unwrap();
            assert_eq!(child.model, "selected");
            assert_eq!(child.context_window, 64_000);
            let directory = prepared
                .runtime
                .sandbox()
                .wrap_program("pwd", &[])
                .output()
                .await
                .unwrap();
            assert!(directory.status.success());
            assert_eq!(
                String::from_utf8(directory.stdout).unwrap().trim(),
                working_dir.to_str().unwrap()
            );
            assert_eq!(prepared.runtime.runtime_config().temperature, Some(0.2));
            assert!(prepared.runtime.extensions().has_command("accepted"));
            assert!(!Arc::ptr_eq(
                &parent.runtime().extensions(),
                &prepared.runtime.extensions()
            ));
            assert_eq!(
                prepared
                    .runtime
                    .expand_prompt("/review")
                    .unwrap()
                    .expanded_prompt
                    .as_deref(),
                Some("<command name=\"review\">\naccepted prompt\n</command>")
            );
            assert!(
                prepared
                    .runtime
                    .commands()
                    .iter()
                    .any(|command| command.name == "skill:review"
                        && command.description == "accepted skill")
            );
            let effect = phi_sdk::extension::ExtensionEffect::SpawnProcess {
                extension: "probe".into(),
                source: crate::config::settings::phi_path()
                    .join("probe.lua")
                    .to_string_lossy()
                    .into(),
                context: "Command".into(),
                program: "printf".into(),
                args: vec!["parent-only".into()],
                sandboxed: true,
            };
            assert!(
                parent
                    .runtime()
                    .probe_effect_permission(&effect)
                    .await
                    .is_ok()
            );
            assert!(
                prepared
                    .runtime
                    .probe_effect_permission(&effect)
                    .await
                    .is_err()
            );
        }
        std::fs::write(
            root.join(".phi/settings.json"),
            r#"{"defaultProvider":"local","defaultModel":"test","temperature":0.75}"#,
        )
        .unwrap();
        std::fs::write(
            &extension,
            "register_command('reloaded', {handler = function() end})",
        )
        .unwrap();
        test_runtime
            .preparation
            .reload_resources(&test_runtime.sessions.session_access(parent.instance().id()))
            .await
            .unwrap();
        let mut child =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        child.working_dir = root.to_string_lossy().into();
        let prepared = policy
            .prepare_child_session(&parent, &mut child, None)
            .await
            .unwrap();
        assert_eq!(prepared.runtime.runtime_config().temperature, Some(0.75));
        std::fs::write(root.join(".phi/settings.json"), r#"{"temperature":0.9}"#).unwrap();
        parent
            .runtime()
            .settings_manager()
            .unwrap()
            .reload()
            .unwrap();
        assert_eq!(
            prepared
                .runtime
                .settings_manager()
                .unwrap()
                .settings()
                .temperature,
            Some(0.9)
        );
        assert_eq!(prepared.runtime.runtime_config().temperature, Some(0.75));
        assert_eq!(parent.runtime().runtime_config().temperature, Some(0.75));
        assert!(prepared.runtime.extensions().has_command("reloaded"));
        assert_eq!(
            prepared
                .runtime
                .expand_prompt("/review")
                .unwrap()
                .expanded_prompt
                .as_deref(),
            Some("<command name=\"review\">\nunaccepted prompt\n</command>")
        );
        test_runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn child_preparation_outlives_fixture_and_preserves_final_sessions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (root, _, test_runtime) = super::super::preparation::tests::session_test_runtime();
        std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
        std::fs::write(
            root.join(".phi/agents/probe.md"),
            "---\nname: probe\ndescription: Probe\n---\nInspect.",
        )
        .unwrap();
        std::fs::write(
            root.join(".phi/settings.json"),
            r#"{"defaultProvider":"local","defaultModel":"test","agent":{"background":true},"memory":{"enabled":false},"extensions":["!*.lua"]}"#,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_path = root.join("child-models.json");
        std::fs::write(&model_path, serde_json::json!({"providers":{"local":{
            "api":"openai-completions","baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
            "models":{"test":{"contextWindow":128000}}
        }}}).to_string()).unwrap();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("child-catalog.json"),
                user_paths: vec![model_path],
                offline: true,
            })
            .unwrap(),
        );
        let provider = tokio::spawn(async move {
            loop {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
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
                            break;
                        }
                    }
                }
                let reply = serde_json::json!({"id":"completion","model":"test","choices":[{
                    "delta":{"content":"child result"},"finish_reason":"stop"
                }]});
                let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
                connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        test_runtime
            .preparation
            .enable_subagents(&test_runtime.sessions);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
        let settings = settings_manager.settings();
        test_runtime.preparation.initialize(
            &root,
            Arc::new(Cli::parse_from(["phi"])),
            settings,
            settings_manager,
            Vec::new(),
            catalog.clone(),
        );
        let parent = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .start_session(session, true)
        .await
        .unwrap();
        assert_eq!(
            parent.instance().task_list().unwrap().list_id(),
            parent.instance().id().as_str()
        );
        assert_eq!(
            parent.session().await.task_list_id.as_deref(),
            Some(parent.instance().id().as_str())
        );
        let access = test_runtime.sessions.session_access(parent.instance().id());
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &parent,
            access.clone(),
            test_runtime.preparation.clone(),
        );
        let manager = test_runtime.manager.clone();
        let weak_runtime = Arc::downgrade(&test_runtime);
        let weak_sessions = Arc::downgrade(&test_runtime.sessions);
        drop(test_runtime);
        assert!(weak_runtime.upgrade().is_none());

        let started = parent
            .instance()
            .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest::new(
                "probe", "inspect",
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if parent
                    .instance()
                    .subagent_status(started.instance_id().clone())
                    .await
                    .unwrap()
                    .is_some_and(|status| {
                        status.admission == phi_sdk::agents::SubagentAdmission::Accepted
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let completion = tokio::time::timeout(
            Duration::from_secs(10),
            access.child_completion(started.instance_id()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(completion.outcome.is_ok(), "{:?}", completion.outcome);
        assert!(attachment.child_attachment("missing-task").await.is_err());
        let child = attachment
            .child_attachment(started.instance_id().as_str())
            .await
            .unwrap();
        assert_eq!(child.id().as_str(), started.instance_id().as_str());
        assert_eq!(
            child.session().await.unwrap().task_list_id,
            parent.session().await.task_list_id
        );
        assert!(
            child
                .child_attachment(started.instance_id().as_str())
                .await
                .is_err()
        );
        {
            let child_instance = manager.get(started.instance_id()).unwrap();
            let nested = child_instance
                .spawn_subagent(
                    phi_sdk::agent_manager::AgentSpawnRequest::new(
                        "probe",
                        "nested configured work",
                    )
                    .mode(phi_sdk::agent_manager::DelegationMode::Foreground),
                )
                .await
                .unwrap();
            assert_eq!(
                nested.mode(),
                phi_sdk::agent_manager::DelegationMode::Background
            );
            tokio::time::timeout(
                Duration::from_secs(10),
                child_instance.wait_for_quiescence(None),
            )
            .await
            .unwrap()
            .unwrap();
        }
        let view = access
            .subagent_view(started.instance_id().as_str())
            .await
            .unwrap();
        assert_eq!(view.session.creator_id(), Some(parent.instance().id()));
        assert!(!view.session.transcript.entries.is_empty());
        let child_path = crate::session::storage::session_path(&view.session);
        let parent_session = parent.session().await;
        assert_eq!(
            child_path,
            crate::session::storage::session_path(&parent_session)
                .parent()
                .unwrap()
                .join(parent_session.id.as_str())
                .join("subagents")
                .join(format!("agent-{}.jsonl", started.instance_id().as_str()))
        );
        assert_eq!(view.session.definition.as_deref(), Some("probe"));
        let team_root = root.join("teams");
        let team = manager
            .initialize_team(
                parent.instance().id(),
                phi_sdk::agent_manager::TeamStorageRoot::new(team_root.clone()).unwrap(),
            )
            .await
            .unwrap();
        manager
            .set_team_capability(&team, phi_sdk::agent_manager::TeamCapability::Enabled)
            .unwrap();
        let teammate = parent
            .instance()
            .spawn_subagent(
                phi_sdk::agent_manager::AgentSpawnRequest::new("probe", "inspect as teammate")
                    .alias(phi_sdk::agent_manager::AgentAlias::new("reviewer").unwrap()),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = parent
                    .instance()
                    .subagent_status(teammate.instance_id().clone())
                    .await
                    .unwrap();
                if let Some(status) = status
                    && status.admission != phi_sdk::agents::SubagentAdmission::Pending
                {
                    assert_eq!(
                        status.admission,
                        phi_sdk::agents::SubagentAdmission::Accepted
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        access
            .child_completion(teammate.instance_id())
            .await
            .unwrap();
        let teammate_session = access
            .subagent_view(teammate.instance_id().as_str())
            .await
            .unwrap()
            .session;
        assert_eq!(teammate_session.agent_kind, *teammate.kind());
        assert_eq!(teammate_session.definition.as_deref(), Some("probe"));
        assert_eq!(
            teammate_session.alias.as_ref().map(|alias| alias.as_str()),
            Some("reviewer")
        );
        assert!(teammate_session.sidechain.is_none());
        let teammate_path = crate::session::storage::session_path(&teammate_session);
        assert_eq!(
            teammate_path.parent(),
            crate::session::storage::session_path(&parent_session).parent()
        );
        assert!(
            teammate_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(
                    &chrono::DateTime::parse_from_rfc3339(&teammate_session.created_at)
                        .unwrap()
                        .format("%Y%m%dT%H%M%SZ")
                        .to_string()
                )
        );
        let team_directory = team_root
            .join(phi_sdk::agent_manager::team_namespace(parent.instance().id().as_str()).unwrap());
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(team_directory.join("config.json")).unwrap())
                .unwrap();
        assert!(
            settings["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|member| member["instance_id"] == teammate.instance_id().as_str())
        );
        manager.shutdown().await.unwrap();
        assert!(!team_directory.exists());
        let saved_teammate =
            crate::session::storage::load_session_id(teammate.instance_id().as_str()).unwrap();
        assert_eq!(saved_teammate.agent_kind, teammate_session.agent_kind);
        assert_eq!(
            crate::session::storage::session_path(&saved_teammate),
            teammate_path
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while weak_sessions.upgrade().unwrap().has_associations().unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let parent_session = parent.session().await;
        let parent_path = crate::session::storage::session_path(&parent_session);
        for path in [&parent_path, &child_path] {
            let lock = crate::session::storage::lock_session_path(path).unwrap();
            drop(lock);
            let saved = crate::session::storage::load_session(path.to_str().unwrap()).unwrap();
            let canonical = if path == &parent_path {
                parent.instance().transcript().await.unwrap()
            } else {
                view.session.transcript.clone()
            };
            assert_eq!(
                serde_json::to_value(saved.transcript).unwrap(),
                serde_json::to_value(canonical).unwrap()
            );
        }
        assert!(
            attachment
                .child_attachment(started.instance_id().as_str())
                .await
                .is_err()
        );
        drop(child);
        drop(attachment);
        drop(access);
        drop(parent);
        drop(manager);
        assert!(
            weak_sessions.upgrade().is_none(),
            "child preparation retained associations"
        );
        provider.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn empty_initial_prompt_compensation_preserves_file_evidence_and_failed_save_lock() {
        let root = std::env::temp_dir().join(format!("phi-empty-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
        std::fs::write(
            root.join(".phi/agents/probe.md"),
            "---\nname: probe\ndescription: Probe\n---\nInspect.",
        )
        .unwrap();
        let model_path = root.join("models.json");
        std::fs::write(&model_path, br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#).unwrap();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("catalog.json"),
                user_paths: vec![model_path],
                offline: true,
            })
            .unwrap(),
        );
        let settings = crate::config::Settings::default();
        let resources = RuntimeResources::new_for(
            phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
            Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
            catalog.clone(),
            phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(),
            &root,
        )
        .extension_paths(Vec::new());
        let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
        let (entered, mut prompts) = tokio::sync::mpsc::unbounded_channel();
        let ended = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let preparation = Arc::new(PromptPreparation {
            policy: crate::runtime::subagents::ChildRuntimePolicy {
                sessions: Arc::downgrade(&test_runtime.sessions),
            },
            factory: test_runtime.preparation.runtime_factory().unwrap(),
            entered,
            ended: ended.clone(),
        });
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let cli = Cli::parse_from(["phi"]);
        let mut instance = AgentInstanceBuildOptions::from_session(&session);
        instance.subagent_preparation = Some(preparation);
        let parent = test_runtime
            .sessions
            .create(
                &test_runtime.preparation,
                session.clone(),
                RuntimeRebuildOptions {
                    phi: PhiBuildOptions {
                        cli: &cli,
                        provider: "local",
                        model: "test",
                        runtime_cfg: settings.runtime_config(&catalog),
                        context: phi_sdk::context::load_with_options_for(
                            false,
                            None,
                            vec![root.join(".phi/agents")],
                            &root,
                        ),
                        agent_options: crate::AgentBuildOptions::default(),
                        hooks: Vec::new(),
                    },
                    instance,
                },
                phi_sdk::lifecycle::SessionStartReason::New,
                true,
            )
            .await
            .unwrap();
        for (index, prompt) in ["handled-input", "cancel-input"].into_iter().enumerate() {
            let started = parent
                .instance()
                .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest::new(
                    "probe", prompt,
                ))
                .await
                .unwrap();
            let creation = tokio::time::timeout(Duration::from_secs(5), prompts.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                creation.instance_id().as_str(),
                started.instance_id().as_str()
            );
            assert_eq!(creation.creator_id(), parent.instance().id());
            assert_eq!(creation.task_list(), parent.instance().task_list());
            let child_id = AgentInstanceId::new(creation.instance_id().as_str());
            let before =
                crate::session::storage::load_session_id(creation.instance_id().as_str()).unwrap();
            let path = crate::session::storage::session_path(&before);
            assert!(before.transcript.entries.is_empty());
            assert_eq!(before.creator_id(), Some(creation.creator_id()));
            let saved_metadata = serde_json::to_value(&before).unwrap();
            assert_eq!(
                saved_metadata["agent_kind"]["creator"],
                creation.creator_id().as_str()
            );
            assert_eq!(saved_metadata["definition"], creation.definition());
            assert!(saved_metadata.get("parent_task").is_none());
            if prompt == "cancel-input" {
                let child = test_runtime.sessions.get(&child_id).unwrap();
                assert_eq!(child.creator_id(), Some(creation.creator_id()));
                assert!(matches!(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        parent
                            .instance()
                            .cancel_subagent(creation.instance_id().clone())
                    )
                    .await
                    .unwrap(),
                    Err(phi_sdk::agent_instance::AgentInstanceError::Operation { .. })
                ));
                let rejected = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let status = parent
                            .instance()
                            .subagent_status(creation.instance_id().clone())
                            .await
                            .unwrap()
                            .unwrap();
                        if status.outcome.is_some() {
                            break status;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(test_runtime.sessions.find(&child_id).unwrap().is_some());
                assert!(crate::session::storage::lock_session_path(&path).is_err());
                assert!(
                    child
                        .instance()
                        .finalization()
                        .unwrap()
                        .view
                        .unwrap()
                        .transcript
                        .entries
                        .is_empty()
                );
                assert!(child.snapshot().await.persistence_error.is_some());
                std::fs::remove_dir(&path).unwrap();
                crate::session::storage::save_session(&before).unwrap();
                let (retried, reconciled) = tokio::join!(
                    test_runtime
                        .sessions
                        .stop(&child_id, phi_sdk::lifecycle::SessionEndReason::Quit),
                    test_runtime.sessions.finalize_agent_session(&child)
                );
                retried.unwrap();
                reconciled.unwrap();
                assert_eq!(
                    parent
                        .instance()
                        .subagent_status(creation.instance_id().clone())
                        .await
                        .unwrap()
                        .unwrap()
                        .outcome,
                    rejected.outcome
                );
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while test_runtime.sessions.find(&child_id).unwrap().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(test_runtime.manager.get(&child_id).is_err());
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let status = parent
                        .instance()
                        .subagent_status(creation.instance_id().clone())
                        .await
                        .unwrap()
                        .unwrap();
                    if status.admission == phi_sdk::agents::SubagentAdmission::Rejected {
                        break status;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(status.outcome.unwrap().is_err());
            test_runtime
                .sessions
                .session_access(parent.instance().id())
                .checkpoint()
                .await
                .unwrap();
            let saved =
                crate::session::storage::load_session_checkpoint(creation.instance_id().as_str())
                    .unwrap();
            assert!(saved.transcript.entries.is_empty());
            assert_eq!(saved.agent_kind, before.agent_kind);
            assert_eq!(saved.created_at, before.created_at);
            assert_eq!(ended.load(std::sync::atomic::Ordering::SeqCst), index + 1);
            let parent_saved =
                crate::session::storage::load_session_id(parent.instance().id().as_str()).unwrap();
            assert!(
                parent_saved
                    .children
                    .iter()
                    .any(|link| link.child_agent_id == creation.instance_id().as_str())
            );
        }
        test_runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        assert!(test_runtime.sessions.list().await.unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
