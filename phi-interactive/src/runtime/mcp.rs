//! Application MCP contract replacement; connection and metadata work stays in the SDK.

use phi_sdk::tools::ToolExposure;
use std::sync::{Arc, Weak};
use std::time::Duration;

use phi_sdk::agent_instance::AgentInstanceStatus;
use phi_sdk::extension::mcp::McpRuntimeEvent;

use super::session::AgentSession;

const CONTRACT_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpServerView {
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) connected: bool,
    pub(crate) cached: bool,
    pub(crate) tools: Vec<McpToolView>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpToolView {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) exposure: ToolExposure,
    pub(crate) schema_length: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum McpReconnectResult {
    Refreshed { tool_count: usize },
    RetryAfter { seconds: u64 },
}

impl From<phi_sdk::extension::mcp::McpServerSnapshot> for McpServerView {
    fn from(server: phi_sdk::extension::mcp::McpServerSnapshot) -> Self {
        Self {
            name: server.name,
            status: format!("{:?}", server.status.state).to_lowercase(),
            connected: server.status.connected,
            cached: server.status.cached,
            tools: server
                .tools
                .into_iter()
                .map(|tool| McpToolView {
                    name: tool.name,
                    description: tool.description,
                    exposure: tool.exposure,
                    schema_length: serde_json::to_string(&tool.input_schema)
                        .map(|schema| schema.len())
                        .unwrap_or_default(),
                })
                .collect(),
        }
    }
}

pub(super) fn observe(agent: &Arc<AgentSession>) {
    let weak = Arc::downgrade(agent);
    let changed = agent.runtime_changed.clone();
    let snapshots = agent.instance().subscribe_snapshots();
    tokio::spawn(observe_generations(weak, changed, snapshots));
}

async fn observe_generations(
    weak: Weak<AgentSession>,
    changed: Arc<tokio::sync::Notify>,
    mut snapshots: phi_sdk::agent_instance::AgentSnapshotSubscription,
) {
    loop {
        let Some(agent) = weak.upgrade() else {
            return;
        };
        let runtime = agent.runtime().clone();
        let projected_events = agent.mcp_events.clone();
        drop(agent);
        let manager = runtime.mcp_manager();
        let mut events = manager.as_ref().map(|manager| manager.subscribe());
        if let Some(manager) = &manager {
            manager.start_background();
        }
        let mut pending = manager.is_some();
        loop {
            if matches!(snapshots.snapshot().status, AgentInstanceStatus::Stopped) {
                return;
            }
            if pending
                && matches!(
                    snapshots.snapshot().status,
                    AgentInstanceStatus::Idle | AgentInstanceStatus::Failed
                )
            {
                match reconcile(&weak, &runtime).await {
                    Ok(()) => pending = false,
                    Err(error) => tracing::warn!("MCP contract replacement failed: {error}"),
                }
            }
            tokio::select! {
                _ = changed.notified() => break,
                result = snapshots.changed() => { if result.is_err() { return; } },
                event = async {
                    match &mut events {
                        Some(events) => events.recv().await,
                        None => std::future::pending().await,
                    }
                } => match event {
                    Ok(event) => {
                        pending |= matches!(&event, McpRuntimeEvent::ToolCatalogChanged { .. });
                        let _ = projected_events.send(event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => pending = true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tokio::time::sleep(CONTRACT_RETRY_DELAY), if pending => {},
            }
        }
    }
}

async fn reconcile(weak: &Weak<AgentSession>, runtime: &super::AgentRuntime) -> anyhow::Result<()> {
    let Some(agent) = weak.upgrade() else {
        return Ok(());
    };
    let Some(installed) = &runtime.mcp_contracts else {
        return Ok(());
    };
    let Some(manager) = runtime.mcp_manager() else {
        return Ok(());
    };
    if installed == &manager.tool_contracts().await {
        return Ok(());
    }
    let session = agent.recovery().await?.session;
    let prepared = runtime.prepare_replacement(&session).await?;
    agent
        .replace_runtime_if_generation(prepared, session, None, true, Some(runtime.generation_id()))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use clap::Parser;
    use phi_sdk::extension::mcp::McpClientManager;

    use super::super::preparation::tests::session_test_runtime;
    use super::super::{AgentRuntimeFactory, RuntimeResources, TestConfiguration, TestRuntime};
    use super::AgentSession;
    use phi_sdk::tools::ToolExposure;

    const INITIAL_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;
    const CHANGED_SCHEMA: &str = r#"{"type":"object","properties":{"value":{"type":"string"}}}"#;
    const SERVER: &str = r#"
import json, os, sys, time
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request:
        continue
    if method == 'initialize':
        result = {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'fixture', 'version': '1'}}
    elif method == 'tools/list':
        hold_path = sys.argv[1] + '.hold'
        if os.path.exists(hold_path):
            open(sys.argv[1] + '.requested', 'w').close()
            while os.path.exists(hold_path):
                time.sleep(0.01)
        with open(sys.argv[1]) as source:
            schema = json.load(source)
        tool_name = schema.pop('_tool_name', 'probe')
        empty_tools = schema.pop('_empty_tools', False)
        result = {'tools': [] if empty_tools else [{'name': tool_name, 'description': 'fixture tool', 'inputSchema': schema}]}
    elif method == 'resources/list':
        result = {'resources': []}
    elif method == 'prompts/list':
        result = {'prompts': []}
    else:
        print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32601, 'message': 'method not found'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#;

    struct Fixture {
        root: PathBuf,
        policy: TestConfiguration,
        test_runtime: Arc<TestRuntime>,
        manager: McpClientManager,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_hooks(Vec::new())
        }

        fn with_hooks(hooks: Vec<phi_sdk::hooks::Hook>) -> Self {
            Self::with_cli(hooks, crate::cli::Cli::parse_from(["phi"]))
        }

        fn with_cli(hooks: Vec<phi_sdk::hooks::Hook>, cli: crate::cli::Cli) -> Self {
            Self::with_mcp_tools(hooks, cli, ToolExposure::Direct)
        }

        fn with_mcp_tools(
            hooks: Vec<phi_sdk::hooks::Hook>,
            cli: crate::cli::Cli,
            exposure: ToolExposure,
        ) -> Self {
            let (root, _, _) = session_test_runtime();
            std::fs::write(root.join("schema.json"), INITIAL_SCHEMA).unwrap();
            std::fs::write(root.join("mcp.py"), SERVER).unwrap();
            {
                std::fs::create_dir_all(root.join(".phi/extensions")).unwrap();
                std::fs::write(root.join(".phi/extensions/probe.lua"), "return {}").unwrap();
            }
            let configs = serde_json::from_value(serde_json::json!({
                "fixture": {"command": "python3", "args": [root.join("mcp.py"), root.join("schema.json")], "exposure": exposure}
            })).unwrap();
            let cli = Arc::new(cli);
            let mut settings = crate::config::Settings::default();
            let sandbox = crate::security::execution_sandbox(&cli, &settings, &root).unwrap();
            let manager = McpClientManager::load_with_settings(&configs, Default::default())
                .with_sandbox(sandbox.clone());
            let catalog = Arc::new(
                phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                    store_path: root.join("store.json"),
                    user_paths: vec![root.join("models.json")],
                    offline: true,
                })
                .unwrap(),
            );
            settings.default_provider = Some("local".into());
            settings.default_model = Some("test".into());
            settings.permission = Some(serde_json::json!({"bash": "ask"}));
            let agent_options = crate::agent_options_from_cli(&cli, &settings);
            let policy =
                TestConfiguration::new(cli, settings, agent_options, hooks, catalog.clone());
            let resources = RuntimeResources::new_for(
                phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
                Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
                catalog,
                sandbox,
                &root,
            )
            .mcp_manager(Some(manager.clone()));
            let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
            policy.install(&test_runtime.preparation);
            Self {
                root,
                policy,
                test_runtime,
                manager,
            }
        }

        async fn start(&self, own_grant: bool) -> Arc<AgentSession> {
            self.start_with_persistence(own_grant, false).await
        }

        async fn start_with_persistence(
            &self,
            own_grant: bool,
            persist: bool,
        ) -> Arc<AgentSession> {
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = self.root.to_string_lossy().as_ref().into();
            if persist {
                session.transcript.append_user_message("saved history");
                session.transcript.append_assistant_message(
                    None,
                    None,
                    Vec::new(),
                    "test".into(),
                    phi_sdk::providers::ProviderApi::OpenAiCompletions,
                    "local".into(),
                    Default::default(),
                    "stop".into(),
                );
            }
            if own_grant {
                session
                    .permission_allowlist
                    .push(crate::session::PermissionAllowEntry {
                        tool: "bash".into(),
                        pattern: "printf own-grant".into(),
                    });
            }
            Box::pin(
                {
                    self.policy.install(&self.test_runtime.preparation);
                    crate::runtime::preparation::SessionWorkflows::new(
                        self.test_runtime.sessions.clone(),
                        self.test_runtime.preparation.clone(),
                    )
                    .unwrap()
                }
                .start_session(session, persist),
            )
            .await
            .unwrap()
        }

        async fn change_schema(&self) {
            std::fs::write(self.root.join("schema.json"), CHANGED_SCHEMA).unwrap();
            self.manager.refresh_metadata("fixture").await.unwrap();
        }

        async fn finish(self) {
            self.test_runtime
                .sessions
                .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                .await
                .unwrap();
            self.manager.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    fn seed_reload_files(fixture: &Fixture) {
        std::fs::create_dir_all(fixture.root.join(".phi")).unwrap();
        let config = crate::config::settings::phi_path();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::copy(fixture.root.join("models.json"), config.join("models.json")).unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(
            crate::config::settings::model_catalog_options(true),
        )
        .unwrap();
        let model = catalog.get("local", "test").unwrap();
        let sources = [
            (
                "models.dev",
                vec!["anthropic", "deepseek", "google", "openai"],
            ),
            ("openai-codex", vec!["openai-codex"]),
            ("openrouter", vec!["openrouter"]),
        ];
        let mut providers = serde_json::Map::new();
        let mut states = serde_json::Map::new();
        for (source, names) in sources {
            let partition = names
                .into_iter()
                .map(|name| {
                    let mut descriptor = model.clone();
                    descriptor.provider = name.into();
                    descriptor.api = match name {
                        "anthropic" => phi_sdk::providers::ProviderApi::AnthropicMessages,
                        "google" => phi_sdk::providers::ProviderApi::GeminiGenerateContent,
                        "deepseek" => phi_sdk::providers::ProviderApi::DeepSeekChat,
                        "openai" => phi_sdk::providers::ProviderApi::OpenAiResponses,
                        "openai-codex" => phi_sdk::providers::ProviderApi::OpenAiCodexResponses,
                        _ => phi_sdk::providers::ProviderApi::OpenAiCompletions,
                    };
                    (
                        name.to_string(),
                        std::collections::BTreeMap::from([("test".to_string(), descriptor)]),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            states.insert(source.into(), serde_json::json!({"sha256": test_sha256(&serde_json::to_vec(&partition).unwrap())}));
            for (name, models) in partition {
                providers.insert(name, serde_json::json!({"models": models}));
            }
        }
        std::fs::write(
            config.join("models-store.json"),
            serde_json::to_vec(
                &serde_json::json!({"schemaVersion":2,"sources":states,"providers":providers}),
            )
            .unwrap(),
        )
        .unwrap();
        let seeded = phi_sdk::models::ModelCatalog::load(
            crate::config::settings::model_catalog_options(true),
        )
        .unwrap();
        assert!(
            !seeded.requires_bootstrap(),
            "providers={:?}, diagnostics={:?}",
            seeded.provider_ids(),
            seeded.diagnostics()
        );
        std::fs::write(fixture.root.join(".phi/settings.json"), r#"{"defaultProvider":"local","defaultModel":"test","permission":{"bash":"ask"},"switches":[{"name":"default","thinking_level":"high"}]}"#).unwrap();
        std::fs::write(config.join("mcp.json"), serde_json::to_vec(&serde_json::json!({"mcpServers":{"fixture":{"command":"python3","args":[fixture.root.join("mcp.py"),fixture.root.join("schema.json")],"exposure":"direct"}}})).unwrap()).unwrap();
    }

    fn test_sha256(bytes: &[u8]) -> String {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sha256sum")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }

    fn run_isolated(test_name: &str) -> bool {
        const CHILD_FLAG: &str = "PHI_TEST_MCP_PANEL_SAVE";
        if std::env::var_os(CHILD_FLAG).is_none() {
            let root = std::env::temp_dir().join(format!("phi-mcp-panel-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD_FLAG, "1")
                .env("PHI_CONFIG_DIR", root.join("config"))
                .env("PHI_SESSION_DIR", root.join("sessions"))
                .env("PHI_OFFLINE", "1")
                .current_dir(&root)
                .output()
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return true;
        }
        false
    }

    #[tokio::test]
    async fn markdown_mcp_diagnostics_follow_committed_generation() {
        if run_isolated("runtime::mcp::tests::markdown_mcp_diagnostics_follow_committed_generation")
        {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        fixture.manager.refresh_metadata("fixture").await.unwrap();
        let agent_dir = fixture.root.join(".phi/agents");
        let prompt_dir = fixture.root.join(".phi/prompts");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&prompt_dir).unwrap();
        let invalid_agent = agent_dir.join("reviewer.md");
        let invalid_prompt = prompt_dir.join("invalid.md");
        std::fs::write(&invalid_agent, "---\r\nname: reviewer\r\ndescription: Review\r\ntools: [Read, fixture_probe]\r\n---\r\nReview.").unwrap();
        std::fs::write(
            &invalid_prompt,
            "---\r\ndescription: Review\r\ndisallowed-tools: [fixture_probe]\r\n---\r\nReview.",
        )
        .unwrap();
        std::fs::write(prompt_dir.join("valid.md"), "Valid $ARGUMENTS").unwrap();
        let agent = fixture.start(true).await;
        let initial = agent.recovery().await.unwrap();
        let capabilities = &initial.declarations.capabilities;
        assert!(capabilities.agent_diagnostics.iter().any(|message| {
            message.contains(invalid_agent.to_str().unwrap())
                && message.contains("tools")
                && message.contains("line 4")
        }));
        assert!(capabilities.prompt_diagnostics.iter().any(|diagnostic| {
            diagnostic.path == invalid_prompt
                && diagnostic.message.contains("disallowed-tools")
                && diagnostic.message.contains("line 3")
        }));
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            fixture
                .test_runtime
                .sessions
                .session_access(agent.instance().id()),
            fixture.test_runtime.preparation.clone(),
        );
        assert!(
            attachment
                .expand_prompt("/valid files")
                .unwrap()
                .expanded_prompt
                .unwrap()
                .contains("Valid files")
        );
        assert!(
            attachment
                .expand_prompt("/invalid")
                .unwrap()
                .expanded_prompt
                .is_none()
        );
        std::fs::remove_file(&invalid_agent).unwrap();
        std::fs::remove_file(&invalid_prompt).unwrap();
        let settings_path = fixture.root.join(".phi/settings.json");
        let settings = std::fs::read_to_string(&settings_path).unwrap();
        std::fs::write(&settings_path, "{invalid").unwrap();
        assert!(Box::pin(attachment.reload_resources()).await.is_err());
        let retained = agent.recovery().await.unwrap();
        assert_eq!(
            retained.snapshot.runtime_generation_id,
            initial.snapshot.runtime_generation_id
        );
        assert_eq!(
            serde_json::to_value(retained.declarations.capabilities).unwrap(),
            serde_json::to_value(capabilities).unwrap()
        );
        std::fs::write(&settings_path, settings).unwrap();
        Box::pin(attachment.reload_resources()).await.unwrap();
        let accepted = agent.recovery().await.unwrap();
        assert_ne!(
            accepted.snapshot.runtime_generation_id,
            initial.snapshot.runtime_generation_id
        );
        assert!(
            accepted
                .declarations
                .capabilities
                .agent_diagnostics
                .is_empty()
        );
        assert!(
            accepted
                .declarations
                .capabilities
                .prompt_diagnostics
                .is_empty()
        );
        assert_eq!(
            accepted.session.permission_allowlist,
            initial.session.permission_allowlist
        );
        fixture.finish().await;
    }

    #[tokio::test]
    async fn scoped_mcp_save_and_finalization_do_not_retain_fixture() {
        if run_isolated(
            "runtime::mcp::tests::scoped_mcp_save_and_finalization_do_not_retain_fixture",
        ) {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        let agent = fixture.start_with_persistence(true, true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        changed_generation(&agent, &initial).await;
        let access = fixture
            .test_runtime
            .sessions
            .session_access(agent.instance().id());
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            access.clone(),
            fixture.test_runtime.preparation.clone(),
        );
        let before = agent.snapshot().await.runtime_generation_id;
        let servers = attachment.mcp_servers().await.unwrap();
        assert!(servers[0].tools[0].exposure == ToolExposure::Direct);
        let selection = super::super::preparation::McpServerToolSelection {
            name: "fixture".into(),
            tools: vec![super::super::preparation::McpToolSelection {
                name: "probe".into(),
                exposure: ToolExposure::Deferred,
                was_exposure: ToolExposure::Direct,
            }],
        };
        let Fixture {
            root,
            test_runtime,
            manager,
            ..
        } = fixture;
        let preparation = test_runtime.preparation.clone();
        let weak_runtime = Arc::downgrade(&test_runtime);
        drop(test_runtime);
        assert!(weak_runtime.upgrade().is_none());
        attachment.save_mcp_tools(vec![selection]).await.unwrap();
        assert_eq!(agent.snapshot().await.runtime_generation_id, before);
        assert_eq!(
            attachment.mcp_servers().await.unwrap()[0].tools[0].exposure,
            ToolExposure::Direct
        );
        let reloaded = preparation.reload_resources(&access).await.unwrap();
        assert_ne!(reloaded.declarations.generation_id, before);
        assert_eq!(
            reloaded.snapshot.runtime_generation_id,
            reloaded.declarations.generation_id
        );
        assert!(
            attachment.mcp_servers().await.unwrap()[0].tools[0].exposure != ToolExposure::Direct
        );
        let mut events = attachment.subscribe();
        let recovery = attachment.recover_events(&mut events).await.unwrap();
        assert_eq!(
            recovery.declarations.generation_id,
            reloaded.declarations.generation_id
        );
        agent.instance().stop().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while access.binding().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(attachment.session().await.is_err());
        let saved =
            crate::session::storage::load_session_checkpoint(attachment.id().as_str()).unwrap();
        assert_eq!(
            serde_json::to_value(&saved.transcript).unwrap(),
            serde_json::to_value(agent.instance().transcript().await.unwrap()).unwrap()
        );
        manager.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn panel_save_and_reload_share_session_scope_and_preserve_other_agents() {
        if run_isolated(
            "runtime::mcp::tests::panel_save_and_reload_share_session_scope_and_preserve_other_agents",
        ) {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        let agent = fixture.start_with_persistence(true, true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        changed_generation(&agent, &initial).await;
        let other = fixture.start(false).await;
        let other_generation = other.snapshot().await.runtime_generation_id;
        {
            let startup_extensions = std::env::current_dir().unwrap().join(".phi/extensions");
            std::fs::create_dir_all(&startup_extensions).unwrap();
            std::fs::write(
                startup_extensions.join("invalid.lua"),
                "this is not valid lua",
            )
            .unwrap();
        }
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            fixture
                .test_runtime
                .sessions
                .session_access(agent.instance().id()),
            fixture.test_runtime.preparation.clone(),
        );
        let snapshot = attachment.mcp_servers().await.unwrap();
        assert_eq!(snapshot[0].tools.len(), 1);
        let selection = super::super::preparation::McpServerToolSelection {
            name: "fixture".into(),
            tools: vec![super::super::preparation::McpToolSelection {
                name: "probe".into(),
                exposure: ToolExposure::Deferred,
                was_exposure: ToolExposure::Direct,
            }],
        };
        let grants = agent.session().await.permission_allowlist;
        let before = std::fs::read(crate::config::settings::mcp_config_file_path()).unwrap();
        let mut invalid = selection.clone();
        invalid.tools[0].name = "removed".into();
        assert!(
            attachment
                .save_mcp_tools(vec![invalid])
                .await
                .unwrap_err()
                .to_string()
                .contains("catalog changed")
        );
        assert_eq!(
            std::fs::read(crate::config::settings::mcp_config_file_path()).unwrap(),
            before
        );
        let settings_path = fixture.root.join(".phi/settings.json");
        let settings = std::fs::read(&settings_path).unwrap();
        std::fs::write(&settings_path, r#"{"permission":{"bash":"invalid"}}"#).unwrap();
        let generation = agent.snapshot().await.runtime_generation_id;
        let factory = fixture.test_runtime.preparation.runtime_factory().unwrap();
        attachment
            .save_mcp_tools(vec![selection.clone()])
            .await
            .unwrap();
        assert_ne!(
            std::fs::read(crate::config::settings::mcp_config_file_path()).unwrap(),
            before
        );
        assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        assert!(Arc::ptr_eq(
            &factory,
            &fixture.test_runtime.preparation.runtime_factory().unwrap()
        ));
        std::fs::write(&settings_path, settings).unwrap();
        attachment.save_mcp_tools(vec![selection]).await.unwrap();
        assert_eq!(agent.session().await.permission_allowlist, grants);
        assert_eq!(
            attachment.mcp_servers().await.unwrap()[0].tools[0].exposure,
            ToolExposure::Direct
        );
        assert_eq!(
            other.snapshot().await.runtime_generation_id,
            other_generation
        );
        assert!(other.mcp_servers().await[0].tools[0].exposure == ToolExposure::Direct);
        let generation = agent.snapshot().await.runtime_generation_id;
        let result = Box::pin(attachment.reload_resources()).await.unwrap();
        assert_eq!(
            result.snapshot.runtime_generation_id,
            result.declarations.generation_id
        );
        assert!(result.snapshot.persistence_error.is_none());
        let reloaded = result.session;
        assert_ne!(agent.snapshot().await.runtime_generation_id, generation);
        assert_eq!(reloaded.permission_allowlist, grants);
        assert_eq!(
            attachment.mcp_servers().await.unwrap()[0].tools[0].exposure,
            ToolExposure::Deferred
        );
        assert_eq!(
            attachment
                .recovery()
                .await
                .unwrap()
                .declarations
                .capabilities
                .thinking_level
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            other.snapshot().await.runtime_generation_id,
            other_generation
        );
        assert!(super::super::session::tests::session_command_allowed(&agent, "own-grant").await);
        let titles = attachment
            .mcp_tool_names(vec!["fixture_removed".into(), "unrelated".into()])
            .await
            .unwrap();
        assert_eq!(
            titles.get("fixture_removed").map(String::as_str),
            Some("fixture: removed")
        );
        assert!(!titles.contains_key("unrelated"));
        let mut events = attachment.subscribe_mcp().unwrap().unwrap();
        let session_path = crate::session::storage::session_path(&agent.session().await);
        fixture
            .test_runtime
            .sessions
            .session_access(agent.instance().id())
            .checkpoint()
            .await
            .unwrap();
        std::fs::remove_file(&session_path).unwrap();
        std::fs::create_dir(&session_path).unwrap();
        let generation = agent.snapshot().await.runtime_generation_id;
        attachment
            .save_mcp_tools(vec![super::super::preparation::McpServerToolSelection {
                name: "fixture".into(),
                tools: vec![super::super::preparation::McpToolSelection {
                    name: "probe".into(),
                    was_exposure: ToolExposure::Deferred,
                    exposure: ToolExposure::Direct,
                }],
            }])
            .await
            .unwrap();
        assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        attachment
            .save_mcp_tools(vec![super::super::preparation::McpServerToolSelection {
                name: "fixture".into(),
                tools: vec![super::super::preparation::McpToolSelection {
                    name: "probe".into(),
                    exposure: ToolExposure::Hidden,
                    was_exposure: ToolExposure::Direct,
                }],
            }])
            .await
            .unwrap();
        assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        std::fs::remove_dir(&session_path).unwrap();
        fixture
            .test_runtime
            .sessions
            .session_access(agent.instance().id())
            .checkpoint()
            .await
            .unwrap();
        assert_eq!(agent.session().await.permission_allowlist, grants);
        assert_eq!(
            attachment.mcp_servers().await.unwrap()[0].tools[0].exposure,
            ToolExposure::Deferred,
        );
        agent
            .mcp_manager()
            .unwrap()
            .refresh_metadata("fixture")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_file(
            std::env::current_dir()
                .unwrap()
                .join(".phi/extensions/invalid.lua"),
        )
        .unwrap();
        fixture.finish().await;
        retained_reload_completion().await;
    }

    #[tokio::test]
    async fn panel_save_preserves_live_selection_after_navigation() {
        if run_isolated("runtime::mcp::tests::panel_save_preserves_live_selection_after_navigation")
        {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        fixture.manager.refresh_metadata("fixture").await.unwrap();
        let workflows = crate::runtime::preparation::SessionWorkflows::new(
            fixture.test_runtime.sessions.clone(),
            fixture.test_runtime.preparation.clone(),
        )
        .unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("navigation anchor");
        let agent = workflows.start_session(session, false).await.unwrap();
        let old_leaf = agent.session().await.leaf_id.clone().unwrap();
        let access = fixture
            .test_runtime
            .sessions
            .session_access(agent.instance().id());
        fixture
            .test_runtime
            .preparation
            .configure_runtime(
                &access,
                crate::runtime::preparation::RuntimeSelection::Switch {
                    name: "plan".into(),
                },
            )
            .await
            .unwrap();
        let expected = agent.runtime().runtime_config();
        fixture
            .test_runtime
            .preparation
            .navigate_branch(
                &access,
                crate::runtime::preparation::BranchNavigation::Entry {
                    entry_id: old_leaf.to_string(),
                },
            )
            .await
            .unwrap();
        let before = agent.session().await;
        let generation = agent.runtime().generation_id().to_string();
        let factory = fixture.test_runtime.preparation.runtime_factory().unwrap();
        let selection = super::super::preparation::McpServerToolSelection {
            name: "fixture".into(),
            tools: vec![super::super::preparation::McpToolSelection {
                name: "probe".into(),
                exposure: ToolExposure::Deferred,
                was_exposure: ToolExposure::Direct,
            }],
        };
        fixture
            .test_runtime
            .preparation
            .save_mcp_tools(&access, vec![selection])
            .await
            .unwrap();
        let actual = agent.runtime().runtime_config();
        assert_eq!(actual.switch, expected.switch);
        assert_eq!(actual.agent_mode, expected.agent_mode);
        assert_eq!(
            actual.default_thinking_level,
            expected.default_thinking_level
        );
        assert_eq!(actual.mode_prompt_override, expected.mode_prompt_override);
        let after = agent.session().await;
        assert_eq!(after.current_switch, "plan");
        assert_eq!(after.provider, before.provider);
        assert_eq!(after.model, before.model);
        assert_eq!(after.entries.len(), before.entries.len());
        assert_eq!(after.leaf_id, before.leaf_id);
        assert_eq!(agent.runtime().generation_id(), generation);
        assert!(Arc::ptr_eq(
            &factory,
            &fixture.test_runtime.preparation.runtime_factory().unwrap(),
        ));
        assert_eq!(
            agent.mcp_servers().await[0].tools[0].exposure,
            ToolExposure::Direct
        );
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(crate::config::settings::mcp_config_file_path()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved["mcpServers"]["fixture"]["toolExposure"]["probe"],
            "deferred"
        );
        fixture
            .test_runtime
            .preparation
            .reload_resources(&access)
            .await
            .unwrap();
        assert_eq!(
            agent.mcp_servers().await[0].tools[0].exposure,
            ToolExposure::Deferred
        );
        fixture.finish().await;
    }

    #[tokio::test]
    async fn panel_save_distinguishes_pending_and_changed_catalogs() {
        if run_isolated(
            "runtime::mcp::tests::panel_save_distinguishes_pending_and_changed_catalogs",
        ) {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        let hold_path = fixture.root.join("schema.json.hold");
        std::fs::write(&hold_path, "").unwrap();
        let outgoing = fixture.start(false).await;
        fixture
            .test_runtime
            .preparation
            .reload_resources(
                &fixture
                    .test_runtime
                    .sessions
                    .session_access(outgoing.instance().id()),
            )
            .await
            .unwrap();
        fixture
            .test_runtime
            .sessions
            .stop(
                outgoing.instance().id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
            .unwrap();
        let agent = fixture.start(false).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        let manager = agent.mcp_manager().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !fixture.root.join("schema.json.requested").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("MCP discovery did not reach the controlled barrier");
        let pending = agent.mcp_servers().await;
        assert!(pending[0].metadata_source.is_none());
        assert!(pending[0].tools.is_empty());
        let path = crate::config::settings::mcp_config_file_path();
        let original = std::fs::read(&path).unwrap();
        let access = fixture
            .test_runtime
            .sessions
            .session_access(agent.instance().id());
        let mut selection = super::super::preparation::McpServerToolSelection {
            name: "fixture".into(),
            tools: vec![super::super::preparation::McpToolSelection {
                name: "probe".into(),
                exposure: ToolExposure::Deferred,
                was_exposure: ToolExposure::Direct,
            }],
        };
        let error = fixture
            .test_runtime
            .preparation
            .save_mcp_tools(&access, vec![selection.clone()])
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("metadata is unavailable"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(agent.snapshot().await.runtime_generation_id, initial);
        std::fs::remove_file(hold_path).unwrap();
        manager.refresh_metadata("fixture").await.unwrap();
        changed_generation(&agent, &initial).await;
        let observed = agent.mcp_servers().await;
        assert!(observed[0].metadata_source.is_some());
        assert_eq!(observed[0].tools.len(), 1);
        assert_eq!(observed[0].tools[0].name, selection.tools[0].name);
        assert_eq!(
            observed[0].tools[0].exposure,
            selection.tools[0].was_exposure
        );
        fixture
            .test_runtime
            .preparation
            .save_mcp_tools(&access, vec![selection.clone()])
            .await
            .unwrap();
        assert_eq!(
            agent.mcp_servers().await[0].tools[0].exposure,
            ToolExposure::Direct
        );
        selection.tools[0].was_exposure = ToolExposure::Deferred;
        selection.tools[0].exposure = ToolExposure::Direct;
        let saved = std::fs::read(&path).unwrap();
        let mut generation = agent.snapshot().await.runtime_generation_id;
        for schema in [
            r#"{"type":"object","_tool_name":"renamed"}"#,
            r#"{"type":"object","_empty_tools":true}"#,
        ] {
            std::fs::write(fixture.root.join("schema.json"), schema).unwrap();
            agent
                .mcp_manager()
                .unwrap()
                .refresh_metadata("fixture")
                .await
                .unwrap();
            assert!(agent.mcp_servers().await[0].metadata_source.is_some());
            generation = changed_generation(&agent, &generation).await;
            let error = fixture
                .test_runtime
                .preparation
                .save_mcp_tools(&access, vec![selection.clone()])
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("MCP tool catalog changed"),
                "{error:#}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), saved);
            assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn reload_and_panel_save_preserve_child_association() {
        if run_isolated("runtime::mcp::tests::reload_and_panel_save_preserve_child_association") {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        fixture
            .test_runtime
            .preparation
            .enable_subagents(&fixture.test_runtime.sessions);
        std::fs::create_dir_all(fixture.root.join(".phi/agents")).unwrap();
        std::fs::write(
            fixture.root.join(".phi/agents/probe.md"),
            "---\nname: probe\ndescription: Probe\n---\nInspect.",
        )
        .unwrap();
        fixture.manager.refresh_metadata("fixture").await.unwrap();
        for save in [false, true] {
            let agent = fixture.start_with_persistence(false, true).await;
            if save {
                let servers = agent.mcp_servers().await;
                assert_eq!(servers[0].tools.len(), 1);
                assert_eq!(servers[0].tools[0].name, "probe");
                let selection = super::super::preparation::McpServerToolSelection {
                    name: servers[0].name.clone(),
                    tools: servers[0]
                        .tools
                        .iter()
                        .map(|tool| super::super::preparation::McpToolSelection {
                            name: tool.name.clone(),
                            exposure: ToolExposure::Deferred,
                            was_exposure: tool.exposure,
                        })
                        .collect(),
                };
                fixture
                    .test_runtime
                    .preparation
                    .clone()
                    .save_mcp_tools(
                        &fixture
                            .test_runtime
                            .sessions
                            .session_access(agent.instance().id()),
                        vec![selection],
                    )
                    .await
                    .unwrap();
            } else {
                Box::pin(
                    fixture.test_runtime.preparation.clone().reload_resources(
                        &fixture
                            .test_runtime
                            .sessions
                            .session_access(agent.instance().id()),
                    ),
                )
                .await
                .unwrap();
            }
            let started = agent
                .instance()
                .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest::new(
                    "probe", "Inspect.",
                ))
                .await
                .unwrap();
            let child_id =
                phi_sdk::agent_instance::AgentInstanceId::new(started.instance_id().as_str());
            let child = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(child) = fixture.test_runtime.sessions.get(&child_id) {
                        break child;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("replacement lost child association");
            let session = child.session().await;
            assert!(child.persists());
            assert_eq!(
                session.creator_id().unwrap().as_str(),
                started.creator_id().as_str()
            );
            assert!(crate::session::storage::session_path(&session).exists());
            fixture
                .test_runtime
                .sessions
                .stop(
                    agent.instance().id(),
                    phi_sdk::lifecycle::SessionEndReason::Quit,
                )
                .await
                .unwrap();
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn protected_reload_reads_manual_edits_without_configuration_writes() {
        if run_isolated(
            "runtime::mcp::tests::protected_reload_reads_manual_edits_without_configuration_writes",
        ) {
            return;
        }
        let cli = crate::cli::Cli::parse_from(["phi", "--rpc"]);
        let fixture = Fixture::with_cli(Vec::new(), cli.clone());
        seed_reload_files(&fixture);
        crate::security::initialize(&cli).unwrap();
        let agent = fixture.start(true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        changed_generation(&agent, &initial).await;
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            fixture
                .test_runtime
                .sessions
                .session_access(agent.instance().id()),
            fixture.test_runtime.preparation.clone(),
        );
        let path = fixture.root.join(".phi/settings.json");
        let settings = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"high\"", "\"low\"");
        std::fs::write(&path, &settings).unwrap();
        Box::pin(attachment.reload_resources()).await.unwrap();
        assert_eq!(agent.capabilities().thinking_level.as_deref(), Some("low"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), settings);
        assert!(
            attachment
                .save_mcp_tools(Vec::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("read-only")
        );
        fixture.finish().await;
    }

    async fn retained_reload_completion() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let hook = phi_sdk::hooks::Hook::session_end({
            let entered = entered.clone();
            let release = release.clone();
            move |event| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    if matches!(event.reason, phi_sdk::lifecycle::SessionEndReason::Reload) {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Ok(Vec::new())
                }
            }
        });
        let fixture = Fixture::with_hooks(vec![hook]);
        seed_reload_files(&fixture);
        let agent = fixture.start(true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        changed_generation(&agent, &initial).await;
        let factory = fixture.test_runtime.preparation.runtime_factory().unwrap();
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            fixture
                .test_runtime
                .sessions
                .session_access(agent.instance().id()),
            fixture.test_runtime.preparation.clone(),
        );
        {
            let waiter = tokio::spawn(async move { attachment.reload_resources().await });
            tokio::time::timeout(Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
        }
        assert!(Arc::ptr_eq(
            &factory,
            &fixture.test_runtime.preparation.runtime_factory().unwrap()
        ));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            while Arc::ptr_eq(
                &factory,
                &fixture.test_runtime.preparation.runtime_factory().unwrap(),
            ) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(agent.capabilities().thinking_level.as_deref(), Some("high"));
        fixture.finish().await;
    }

    async fn changed_generation(agent: &AgentSession, previous: &str) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let generation = agent.snapshot().await.runtime_generation_id;
                if generation != previous {
                    return generation;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP contract replacement did not complete")
    }

    #[tokio::test]
    async fn catalog_refresh_revalidates_frozen_prompts_without_file_reads() {
        if run_isolated(
            "runtime::mcp::tests::catalog_refresh_revalidates_frozen_prompts_without_file_reads",
        ) {
            return;
        }
        let fixture = Fixture::new();
        let prompts = fixture.root.join(".phi/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        let path = prompts.join("inspect.md");
        std::fs::write(
            &path,
            "---\r\nallowed-tools: [fixture_probe]\r\n---\r\nFrozen prompt",
        )
        .unwrap();
        std::fs::write(prompts.join("stable.md"), "Stable prompt").unwrap();
        std::fs::write(
            fixture.root.join("schema.json"),
            r#"{"type":"object","_tool_name":"other"}"#,
        )
        .unwrap();
        fixture.manager.refresh_metadata("fixture").await.unwrap();
        let agent = fixture.start(false).await;
        let generation = agent.snapshot().await.runtime_generation_id;
        assert!(
            agent
                .runtime()
                .expand_prompt("/inspect")
                .unwrap()
                .expanded_prompt
                .is_some()
        );
        std::fs::remove_dir_all(&prompts).unwrap();
        fixture.change_schema().await;
        let generation = changed_generation(&agent, &generation).await;
        let runtime = agent.runtime().clone();
        assert!(
            runtime
                .expand_prompt("/inspect")
                .unwrap()
                .expanded_prompt
                .is_none()
        );
        assert!(
            runtime
                .expand_prompt("/stable")
                .unwrap()
                .expanded_prompt
                .unwrap()
                .contains("Stable prompt")
        );
        let diagnostic = runtime
            .prompt_diagnostics()
            .into_iter()
            .find(|diagnostic| diagnostic.path == path)
            .unwrap();
        assert!(diagnostic.message.contains("allowed-tools"));
        assert!(diagnostic.message.contains("line 2"));
        std::fs::write(
            fixture.root.join("schema.json"),
            r#"{"type":"object","_tool_name":"other"}"#,
        )
        .unwrap();
        fixture.manager.refresh_metadata("fixture").await.unwrap();
        changed_generation(&agent, &generation).await;
        assert!(
            agent
                .runtime()
                .expand_prompt("/inspect")
                .unwrap()
                .expanded_prompt
                .unwrap()
                .contains("Frozen prompt")
        );
        assert!(agent.runtime().prompt_diagnostics().is_empty());
        fixture.finish().await;
    }

    #[tokio::test]
    async fn deferred_mcp_contracts_are_independent_of_codemode_enablement() {
        for arguments in [vec!["phi"], vec!["phi", "--codemode"]] {
            let cli = crate::cli::Cli::parse_from(arguments);
            let fixture = Fixture::with_mcp_tools(Vec::new(), cli, ToolExposure::Deferred);
            fixture.manager.refresh_metadata("fixture").await.unwrap();
            let agent = fixture.start(false).await;
            let runtime = agent.runtime().clone();
            let initial = agent.snapshot().await.runtime_generation_id;
            assert!(
                runtime
                    .mcp_contracts
                    .as_ref()
                    .unwrap()
                    .validate_tool_rule(&"fixture_probe".parse().unwrap())
                    .is_err()
            );
            assert!(agent.mcp_servers().await[0].tools[0].exposure != ToolExposure::Direct);
            fixture.change_schema().await;
            super::reconcile(&Arc::downgrade(&agent), &runtime)
                .await
                .unwrap();
            assert_ne!(agent.snapshot().await.runtime_generation_id, initial);
            fixture.finish().await;
        }
    }

    #[tokio::test]
    async fn catalog_refresh_completes_without_a_client_and_preserves_own_grants() {
        let fixture = Fixture::new();
        let agent = fixture.start(true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        let accepted = changed_generation(&agent, &initial).await;
        fixture.manager.refresh_metadata("fixture").await.unwrap();
        let runtime = agent.runtime().clone();
        super::reconcile(&Arc::downgrade(&agent), &runtime)
            .await
            .unwrap();
        assert_eq!(agent.snapshot().await.runtime_generation_id, accepted);
        let other = fixture.start(false).await;
        let other_generation = other.snapshot().await.runtime_generation_id;
        fixture.change_schema().await;
        let changed = changed_generation(&agent, &accepted).await;
        let other_changed = changed_generation(&other, &other_generation).await;
        assert_ne!(changed, other_changed);
        {
            assert!(
                super::super::session::tests::session_command_allowed(&agent, "own-grant").await
            );
            assert!(
                !super::super::session::tests::session_command_allowed(&other, "own-grant").await
            );
        }
        fixture.finish().await;
    }

    #[tokio::test]
    async fn catalog_refresh_freezes_sources_while_explicit_reload_validates_files() {
        if run_isolated(
            "runtime::mcp::tests::catalog_refresh_freezes_sources_while_explicit_reload_validates_files",
        ) {
            return;
        }
        let fixture = Fixture::new();
        seed_reload_files(&fixture);
        let agent = fixture.start(true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        let accepted = changed_generation(&agent, &initial).await;
        let extension = fixture.root.join(".phi/extensions/probe.lua");
        std::fs::write(&extension, "this is not valid lua").unwrap();
        fixture.change_schema().await;
        let refreshed = changed_generation(&agent, &accepted).await;
        assert!(super::super::session::tests::session_command_allowed(&agent, "own-grant").await);
        let access = fixture
            .test_runtime
            .sessions
            .session_access(agent.instance().id());
        let error = fixture
            .test_runtime
            .preparation
            .reload_resources(&access)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("syntax error"), "{error:#}");
        assert_eq!(agent.snapshot().await.runtime_generation_id, refreshed);
        assert!(super::super::session::tests::session_command_allowed(&agent, "own-grant").await);
        std::fs::write(extension, "return {}").unwrap();
        fixture
            .test_runtime
            .preparation
            .reload_resources(&access)
            .await
            .unwrap();
        assert_ne!(agent.snapshot().await.runtime_generation_id, refreshed);
        fixture.finish().await;
    }

    #[tokio::test]
    async fn pending_input_defers_catalog_acceptance_until_sdk_readiness() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let fixture = Fixture::with_hooks(vec![phi_sdk::hooks::Hook::user_prompt_submit({
            let entered = entered.clone();
            let release = release.clone();
            move |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Handled { reason: None })
                }
            }
        })]);
        let agent = fixture.start(false).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        let accepted = changed_generation(&agent, &initial).await;
        let instance = agent.instance();
        let submit = tokio::spawn(async move {
            instance
                .submit(phi_sdk::turn::TurnPrompt {
                    tool_policy: Default::default(),
                    prompt: "pending input".into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        fixture.change_schema().await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                changed_generation(&agent, &accepted),
            )
            .await
            .is_err()
        );
        assert_eq!(agent.snapshot().await.runtime_generation_id, accepted);
        release.notify_one();
        assert!(matches!(
            submit.await.unwrap().unwrap(),
            phi_sdk::turn::SubmissionDisposition::Handled { .. }
        ));
        changed_generation(&agent, &accepted).await;
        fixture.finish().await;
    }

    #[tokio::test]
    async fn obsolete_catalog_candidate_cannot_replace_a_newer_runtime() {
        let fixture = Fixture::new();
        let agent = fixture.start(true).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        changed_generation(&agent, &initial).await;
        let runtime = agent.runtime().clone();
        let session = agent.recovery().await.unwrap().session;
        let obsolete = runtime.prepare_replacement(&session).await.unwrap();
        let current = runtime.prepare_replacement(&session).await.unwrap();
        agent
            .replace_runtime(current, session.clone(), None, false)
            .await
            .unwrap();
        let accepted = agent.snapshot().await.runtime_generation_id;
        assert!(
            agent
                .replace_runtime_if_generation(
                    obsolete,
                    session,
                    None,
                    false,
                    Some(runtime.generation_id())
                )
                .await
                .is_err()
        );
        assert_eq!(agent.snapshot().await.runtime_generation_id, accepted);
        fixture.finish().await;
    }

    #[tokio::test]
    async fn reconnect_completes_metadata_and_runtime_contract() {
        let fixture = Fixture::new();
        let agent = fixture.start(false).await;
        let initial = agent.snapshot().await.runtime_generation_id;
        let accepted = changed_generation(&agent, &initial).await;
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            fixture
                .test_runtime
                .sessions
                .session_access(agent.instance().id()),
            fixture.test_runtime.preparation.clone(),
        );
        assert!(matches!(
            attachment.reconnect_mcp("fixture").await.unwrap(),
            super::McpReconnectResult::Refreshed { tool_count: 1 }
        ));
        assert_eq!(agent.snapshot().await.runtime_generation_id, accepted);
        std::fs::write(fixture.root.join("schema.json"), CHANGED_SCHEMA).unwrap();
        let reconnecting = tokio::spawn({
            let attachment = attachment.clone();
            async move { attachment.reconnect_mcp("fixture").await }
        });
        drop(attachment);
        let result = tokio::time::timeout(Duration::from_secs(10), reconnecting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            super::McpReconnectResult::Refreshed { tool_count: 1 }
        ));
        changed_generation(&agent, &accepted).await;
        fixture.finish().await;
    }
}
