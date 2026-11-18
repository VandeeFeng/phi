mod cli;
mod commands;
mod config;
mod file_lock;
mod interactive;
mod logging;
mod resources;
mod rpc;
mod runtime;
mod security;
mod session;

use clap::Parser;
use config::settings;
use phi_sdk::extension;
use phi_sdk::lifecycle::{ResourcesDiscoverReason, SessionEndReason};
use phi_sdk::permission::{PermissionConfig, PermissionMode};

#[derive(Clone, Default)]
pub(crate) struct AgentBuildOptions {
    pub(crate) no_tools: bool,
    pub(crate) workflow: bool,
    pub(crate) codemode: Option<phi_sdk::config::CodemodeConfig>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) temperature: Option<f64>,
}

pub(crate) fn agent_options_from_cli(
    cli: &cli::Cli,
    settings: &config::Settings,
) -> AgentBuildOptions {
    AgentBuildOptions {
        no_tools: cli.resolve_no_tools(settings),
        workflow: cli.workflow || settings.agent.workflow.unwrap_or(false),
        codemode: cli.codemode.then(|| phi_sdk::config::CodemodeConfig {
            enabled: true,
            ..settings.codemode.clone().unwrap_or_default()
        }),
        max_tokens: cli.resolve_max_tokens(settings),
        temperature: cli.temperature.map(|temp| temp.clamp(0.0, 2.0)),
    }
}

pub(crate) fn resolve_mode(cli: &cli::Cli, permission: &PermissionConfig) -> PermissionMode {
    if cli.yolo {
        PermissionMode::Yolo
    } else {
        cli.permission_mode.unwrap_or(permission.mode)
    }
}

fn missing_auth_message(provider: &str) -> Option<String> {
    if !matches!(provider, "chatgpt" | "openai-codex") {
        return None;
    }
    let path = settings::provider_auth_file(provider);
    (!path.exists()).then(|| {
        format!(
            "{provider} auth missing: run /login {provider} (auth file: {})",
            path.display()
        )
    })
}

async fn ensure_model_catalog(
    catalog: &phi_sdk::models::ModelCatalog,
    store_path: &std::path::Path,
    store_existed: bool,
) -> anyhow::Result<()> {
    if !catalog.requires_bootstrap() {
        return Ok(());
    }
    if catalog.offline() {
        catalog.bootstrap_default().await?;
        return Ok(());
    }
    if store_existed {
        eprintln!("Rebuilding model catalog because the local cache is incomplete or invalid.");
    } else {
        eprintln!("Initializing model catalog on the first Phi run.");
    }
    eprintln!("This cache powers model selection, capabilities, limits, and cost estimates.");
    eprintln!("Downloading model metadata from OpenAI Codex, models.dev, and OpenRouter...");
    let result = catalog.bootstrap_default().await?;
    let model_count = catalog
        .provider_ids()
        .iter()
        .map(|provider| catalog.model_ids(provider).len())
        .sum::<usize>();
    eprintln!(
        "Created {} with {model_count} models from {}.",
        store_path.display(),
        result.updated.join(" and ")
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let startup_dir = std::env::current_dir()?;
    let mut cli = cli::Cli::parse();
    if matches!(cli.command, Some(cli::Command::Gateway { .. })) {
        cli.web = true;
    }
    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,rig=off,rmcp=off"));
    tracing::subscriber::set_global_default(logging::subscriber(
        cli.log_file.as_deref(),
        log_filter,
    )?)?;
    security::initialize(&cli)?;
    if let Some(cli::Command::Gateway {
        action: Some(action),
        port,
    }) = &cli.command
    {
        anyhow::ensure!(
            port.is_none(),
            "--port applies only when starting the gateway"
        );
        return cli::web::manage(action).await;
    }
    if cli.gateway {
        return interactive::run_gateway(&cli).await;
    }
    if let Some(cli::Command::Config { local }) = &cli.command {
        return cli::config::run(*local);
    }
    if matches!(cli.command, Some(cli::Command::List)) {
        return cli::list::run();
    }
    if cli.web {
        if let Some(view) = cli::web::existing().await? {
            println!("Phi web: {}", view.url());
            if !matches!(cli.command, Some(cli::Command::Gateway { .. })) {
                let client = view.client()?;
                cli::web::select_session(&client, None, &startup_dir).await?;
                if cli::web::needs_setup() {
                    cli::web::setup_web()?;
                } else {
                    cli::web::open_browser(view.url().to_owned()).await;
                }
            }
            return Ok(());
        }
        anyhow::ensure!(
            matches!(cli.command, Some(cli::Command::Gateway { .. })),
            "Gateway is not running; start phi gateway first"
        );
    }
    let mut saved_session = None;
    let mut session_lock = None;
    if cli.resume && cli.session.is_none() && !cli.continue_session {
        match cli::select_session()? {
            Some(selected) => {
                let (session, lock) =
                    session::storage::SavedSessionPath::resolve_id(&selected)?.open()?;
                saved_session = Some(session);
                session_lock = Some(lock);
            }
            None => {
                eprintln!("No session selected.");
                return Ok(());
            }
        }
    }
    if cli.continue_session
        && cli.session.is_none()
        && let Ok(sessions) = session::discovery::find_recent_sessions(1)
        && let Some(summary) = sessions.into_iter().next()
    {
        let (session, lock) =
            session::storage::SavedSessionPath::resolve_id(&summary.id)?.open()?;
        saved_session = Some(session);
        session_lock = Some(lock);
    }
    if let Some(session_id) = &cli.session {
        let (session, lock) = session::storage::SavedSessionPath::resolve(session_id)?.open()?;
        saved_session = Some(session);
        session_lock = Some(lock);
    }
    let working_directory = match &saved_session {
        Some(session) => std::path::PathBuf::from(session.working_dir.as_str()),
        None if matches!(cli.command, Some(cli::Command::Gateway { .. })) => {
            phi_sdk::workspace::expand_home_path("~")
        }
        None => startup_dir.clone(),
    };
    let settings_manager = config::SettingsManager::load_for(&working_directory)?;
    let interactive = !cli.rpc && !cli.print && !cli.web;
    if interactive
        && settings_manager
            .hook_workspace_trust(&working_directory)?
            .is_none()
    {
        let root = phi_sdk::workspace::WorkspaceMeta::discover(&working_directory, None)?
            .root
            .canonicalize()?;
        let decision = interactive::confirm_hook_workspace(&root)?.ok_or_else(|| {
            anyhow::anyhow!("workspace Hook trust cancelled for {}", root.display())
        })?;
        settings_manager.set_hook_workspace_trust(&working_directory, decision)?;
    }
    let hook_trust = settings_manager.hook_workspace_trust(&working_directory)?;
    let mut settings = settings_manager.settings();
    security::initialize_file_trust(settings.trust_remote)?;
    let offline = settings::offline_mode()?;
    let catalog_options = settings::model_catalog_options(offline);
    let store_path = catalog_options.store_path.clone();
    let store_existed = store_path.exists();
    let model_catalog = std::sync::Arc::new(phi_sdk::models::ModelCatalog::load(catalog_options)?);
    ensure_model_catalog(&model_catalog, &store_path, store_existed).await?;
    let mut startup_warnings = settings.validation_warnings();
    startup_warnings.extend(model_catalog.diagnostics().iter().map(|diagnostic| {
        format!(
            "model catalog {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        )
    }));
    let sandbox = security::execution_sandbox(&cli, &settings, &working_directory)?;
    let permission_services =
        runtime::AgentRuntimeFactory::permission_services(&cli, &settings, &working_directory)?
            .model_catalog(model_catalog.clone());
    let resource_policy = settings_manager.resource_policy_for(&working_directory)?;
    let extension_paths = resource_policy.extension_paths();
    let mut extension_load_failed = false;
    let extensions = extension::ExtensionRuntime::try_load_with_paths_and_services(
        extension_paths.clone(),
        runtime::AgentRuntimeFactory::extension_runtime_services(
            &permission_services,
            sandbox.clone(),
        )
        .hook_policy(match hook_trust {
            Some(config::HookTrust::Disabled) => extension::ExtensionHookPolicy::Disabled,
            _ => extension::ExtensionHookPolicy::Enabled,
        }),
    )
    .unwrap_or_else(|error| {
        startup_warnings.push(format!("phi extension load failed: {error}"));
        extension_load_failed = true;
        extension::ExtensionRuntime::load()
    });
    let extensions = std::sync::Arc::new(extensions);
    runtime::AgentRuntimeFactory::validate_hook_trust(
        &working_directory,
        hook_trust,
        &cli,
        Some(&settings),
        &extensions,
    )?;
    extensions
        .discover_resources_for(ResourcesDiscoverReason::Startup, &working_directory)
        .await;
    startup_warnings.extend(
        resources::prompts::skills::initialize_builtin_links(&extension::builtin_skills(
            &phi_sdk::config::RuntimeConfig::from(&settings),
        ))
        .into_iter()
        .map(|diagnostic| {
            resources::format_diagnostic("Skill conflicts", &diagnostic.path, &diagnostic.message)
        }),
    );

    let mut session = saved_session.unwrap_or_else(|| {
        session::new(
            &cli.resolve_provider(&settings),
            &cli.resolve_model(&settings),
            settings.context_window.unwrap_or(128_000),
            &working_directory,
        )
    });
    if let Some(name) = &cli.name {
        session.name = name.into();
    }

    let startup_error = if matches!(cli.command, Some(cli::Command::Gateway { .. })) {
        None
    } else {
        runtime::preparation::resolve_startup(&cli, &settings, &mut session, &model_catalog)?;
        if let Some(warning) =
            cli.startup_warning(&model_catalog, &session.provider, &session.model)
        {
            startup_warnings.push(warning);
        }
        missing_auth_message(session.provider.as_str())
    };
    if cli.rpc {
        let session_directory = phi_sdk::workspace::session_scope_dir(session.working_dir.as_str());
        session::storage::cleanup_stale_session_locks(&session_directory)?;
    }
    let mcp_manager = runtime::AgentRuntimeFactory::mcp_manager(&cli, &settings, sandbox.clone());
    if !interactive && let Some(manager) = &mcp_manager {
        manager.start().await;
    }

    let permission_services = if cli.print {
        permission_services.interactive(false)
    } else {
        permission_services
    };
    let resources = runtime::RuntimeResources::with_policy(
        permission_services,
        extensions,
        model_catalog.clone(),
        sandbox,
        resource_policy,
        extension_paths,
    )
    .extension_load_failed(extension_load_failed)
    .mcp_manager(mcp_manager.clone());
    let workflows = std::sync::Arc::new(phi_sdk::workflow::WorkflowRuntime::default());
    let agent_manager = phi_sdk::agent_manager::AgentManager::new();
    let sessions = runtime::SessionAssociations::new(agent_manager.clone(), workflows);
    let preparation =
        runtime::RuntimePreparation::new(runtime::AgentRuntimeFactory::new(resources));
    let cli = std::sync::Arc::new(cli);
    preparation.enable_subagents(&sessions);
    let models = preparation.initialize(
        &startup_dir,
        cli.clone(),
        settings.clone(),
        settings_manager,
        Vec::new(),
        model_catalog,
    );
    if cli.rpc {
        let rpc = rpc::RpcSession::open(sessions.clone(), preparation, models);
        return match rpc {
            Ok(mut rpc) => {
                rpc.run().await;
                rpc.finish(with_mcp_shutdown(mcp_manager, async {
                    finalize_sessions(&sessions, Ok(None)).await.map(|_| ())
                }))
                .await
            }
            Err(error) => {
                with_mcp_shutdown(mcp_manager, async {
                    finalize_sessions(&sessions, Err(error)).await.map(|_| ())
                })
                .await
            }
        };
    }
    let mut gateway_entry = None;
    let result = async {
        if cli.print {
            let workflows =
                runtime::preparation::SessionWorkflows::new(sessions.clone(), preparation)?;
            cli::print::run_print(
                workflows,
                sessions.clone(),
                &session,
                cli.message.join(" "),
                !cli.no_session,
                session_lock,
            )
            .await
            .map(Some)
        } else if cli.web {
            for warning in &startup_warnings {
                eprintln!("{warning}");
            }
            if let Some(error) = &startup_error {
                eprintln!("{error}");
            }
            let workflows =
                runtime::preparation::SessionWorkflows::new(sessions.clone(), preparation)?;
            let service = gateway_entry
                .insert(cli::web::prepare(agent_manager.clone(), workflows, &cli).await?);
            cli::web::run(service).await?;
            Ok(None)
        } else {
            let initial_message = cli.message.join(" ");
            if !initial_message.is_empty() {
                session.append_user_message(&initial_message);
            }
            let memory = interactive::MemoryAdministration::new(
                settings.effective_memory().as_ref(),
                &startup_dir,
            );
            let mcp_auth = config::McpAuthDiagnostics::new(&settings);
            let workflows =
                runtime::preparation::SessionWorkflows::new(sessions.clone(), preparation.clone())?;
            let binding = workflows
                .start_with_lock(session.clone(), true, session_lock)
                .await?;
            let attachment = runtime::attachment::AgentAttachment::new(
                &binding,
                sessions.session_access(binding.instance().id()),
                preparation,
            );
            interactive::run_interactive(
                sessions.clone(),
                memory,
                mcp_auth,
                workflows,
                models,
                attachment,
                &cli,
                &mut settings,
                &mut session,
                (!startup_warnings.is_empty()).then(|| startup_warnings.join("\n")),
                startup_error,
            )
            .await?;
            Ok(None)
        }
    }
    .await;
    let mut result = with_mcp_shutdown(mcp_manager, finalize_sessions(&sessions, result)).await;
    if let Some(gateway) = gateway_entry
        && let Err(error) = gateway.close().await
    {
        result = Err(match result {
            Ok(_) => error,
            Err(previous) => previous.context(format!("gateway shutdown failed: {error:#}")),
        });
    }
    if cli.print {
        cli::print::finish_print(&sessions, result)
    } else {
        result?;
        Ok(())
    }
}

async fn with_mcp_shutdown<T>(
    manager: Option<phi_sdk::extension::mcp::McpClientManager>,
    operation: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let result = operation.await;
    if let Some(manager) = manager {
        manager.shutdown().await;
    }
    result
}

async fn finalize_sessions(
    sessions: &std::sync::Arc<runtime::SessionAssociations>,
    result: anyhow::Result<Option<String>>,
) -> anyhow::Result<Option<String>> {
    match (result, sessions.shutdown(SessionEndReason::Quit).await) {
        (Ok(output), Ok(_)) => Ok(output),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(shutdown)) => {
            Err(error.context(format!("Session finalization also failed: {shutdown:#}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_cli_enables_without_mutating_settings() {
        use clap::Parser;
        for configured in [None, Some(false), Some(true)] {
            for enabled in [false, true] {
                for no_tools in [false, true] {
                    let mut arguments = vec!["phi"];
                    if enabled {
                        arguments.push("--workflow");
                    }
                    if no_tools {
                        arguments.push("--no-tools");
                    }
                    let cli = cli::Cli::parse_from(arguments);
                    let mut settings = config::Settings::default();
                    settings.agent.workflow = configured;
                    let options = agent_options_from_cli(&cli, &settings);
                    assert_eq!(options.workflow, enabled || configured.unwrap_or(false));
                    assert_eq!(options.no_tools, no_tools);
                    assert_eq!(settings.agent.workflow, configured);
                }
            }
        }
    }

    #[test]
    fn codemode_cli_preserves_config_and_no_tools_precedence() {
        use phi_sdk::config::{CodemodeCache, CodemodeConfig};
        let configurations =
            [None]
                .into_iter()
                .chain([false, true].into_iter().flat_map(|enabled| {
                    [
                        CodemodeCache::Enabled(true),
                        CodemodeCache::Enabled(false),
                        CodemodeCache::Directory("/tmp/phi-cache".into()),
                    ]
                    .map(move |cache| {
                        Some(CodemodeConfig {
                            enabled,
                            inline_budget: 17,
                            cache,
                        })
                    })
                }));
        for configured in configurations {
            for enabled in [false, true] {
                for no_tools in [false, true] {
                    let mut args = vec!["phi"];
                    if enabled {
                        args.push("--codemode");
                    }
                    if no_tools {
                        args.push("--no-tools");
                    }
                    let cli = cli::Cli::parse_from(args);
                    let settings = config::Settings {
                        codemode: configured.clone(),
                        ..Default::default()
                    };
                    let options = agent_options_from_cli(&cli, &settings);
                    assert_eq!(options.no_tools, no_tools);
                    assert_eq!(options.codemode.is_some(), enabled);
                    let effective = options.codemode.unwrap_or_else(|| {
                        phi_sdk::config::RuntimeConfig::from(&settings)
                            .extensions
                            .codemode
                    });
                    let expected = configured.clone().unwrap_or_default();
                    assert_eq!(effective.enabled, enabled || expected.enabled);
                    assert_eq!(effective.inline_budget, expected.inline_budget);
                    assert_eq!(effective.cache, expected.cache);
                }
            }
        }
    }

    #[tokio::test]
    async fn mcp_cleanup_preserves_results() {
        use phi_sdk::extension::mcp::McpClientManager;

        for result in [
            Ok(Some("completed")),
            Err(anyhow::anyhow!("execution failed")),
        ] {
            let servers = serde_json::from_value(serde_json::json!({
                "probe": {"command": "/usr/bin/false", "args": []}
            }))
            .unwrap();
            let manager = McpClientManager::load_with_settings(&servers, Default::default());
            let observer = manager.clone();
            let expected = result.as_ref().cloned().map_err(ToString::to_string);
            let actual = with_mcp_shutdown(Some(manager), async { result }).await;
            assert_eq!(actual.map_err(|error| error.to_string()), expected);
            let error = observer.connect("probe").await.unwrap_err();
            assert!(error.to_string().contains("shut down"), "{error:#}");
        }
        assert_eq!(with_mcp_shutdown(None, async { Ok(17) }).await.unwrap(), 17);
    }

    #[tokio::test]
    async fn process_finalization_retains_errors_and_allows_retry() {
        let hook = phi_sdk::hooks::Hook::session_end(|_| async {
            Ok(vec![phi_sdk::hooks::HookNotice::Info {
                message: "finalization observed".into(),
            }])
        });
        let (root, policy, test_runtime) =
            runtime::preparation::tests::session_test_runtime_with_hooks(vec![hook]);
        let sessions = test_runtime.sessions.clone();
        let mut session = session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let binding = {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(session, true)
        .await
        .unwrap();
        binding
            .reset_context(Some("retained history".into()))
            .await
            .unwrap();
        test_runtime
            .sessions
            .session_access(binding.instance().id())
            .checkpoint()
            .await
            .unwrap();
        let path = session::storage::session_path(&binding.session().await);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        drop(policy);
        drop(test_runtime);

        let error = finalize_sessions(&sessions, Err(anyhow::anyhow!("execution failed")))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Session finalization also failed")
        );
        assert_eq!(error.root_cause().to_string(), "execution failed");
        assert!(sessions.get(binding.instance().id()).is_ok());
        assert!(session::storage::lock_session_path(&path).is_err());
        let notices = sessions.take_notices().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].agent_id, *binding.instance().id());
        assert!(matches!(notices[0].hook_notices.as_slice(),
            [phi_sdk::hooks::HookNotice::Info { message }] if message == "finalization observed"));

        std::fs::remove_dir(&path).unwrap();
        finalize_sessions(&sessions, Ok(None)).await.unwrap();
        assert!(sessions.get(binding.instance().id()).is_err());
        assert!(sessions.take_notices().unwrap().is_empty());
        let saved =
            session::storage::load_session_checkpoint(binding.instance().id().as_str()).unwrap();
        assert_eq!(
            serde_json::to_value(saved.transcript).unwrap(),
            serde_json::to_value(binding.instance().transcript().await.unwrap()).unwrap(),
        );
        drop(session::storage::lock_session_path(&path).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn process_finalizes_sessions_after_success_or_failure() {
        for result in [
            Ok(Some("completed".to_string())),
            Err(anyhow::anyhow!("execution failed")),
        ] {
            let (root, policy, test_runtime) = runtime::preparation::tests::session_test_runtime();
            let sessions = test_runtime.sessions.clone();
            let mut session =
                session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let binding = {
                policy.install(&test_runtime.preparation);
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
            }
            .start_session(session, true)
            .await
            .unwrap();
            binding
                .reset_context(Some("retained history".into()))
                .await
                .unwrap();
            let path = session::storage::session_path(&binding.session().await);
            assert!(session::storage::lock_session_path(&path).is_err());
            drop(policy);
            drop(test_runtime);

            let expected = result.as_ref().cloned().map_err(ToString::to_string);
            let actual = finalize_sessions(&sessions, result).await;
            assert_eq!(actual.map_err(|error| error.to_string()), expected);
            assert!(sessions.get(binding.instance().id()).is_err());
            assert_eq!(
                binding.instance().snapshot().status,
                phi_sdk::agent_instance::AgentInstanceStatus::Stopped,
            );
            let saved = session::storage::load_session_checkpoint(binding.instance().id().as_str())
                .unwrap();
            assert_eq!(
                serde_json::to_value(saved.transcript).unwrap(),
                serde_json::to_value(binding.instance().transcript().await.unwrap()).unwrap(),
            );
            drop(session::storage::lock_session_path(&path).unwrap());
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
