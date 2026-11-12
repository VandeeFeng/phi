//! Application security policy, independent of configuration parsing and resource discovery.
//! SDK file/process enforcement remains separate from these application write guards.

use std::{path::Path, sync::OnceLock};

use crate::cli::Cli;

static PROCESS_ACCESS: OnceLock<ConfigAccess> = OnceLock::new();
static REMOTE_FILE_TRUST: OnceLock<bool> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum ConfigAccess {
    #[default]
    Local,
    RpcReadOnly,
}

impl ConfigAccess {
    pub(crate) fn for_cli(cli: &Cli) -> Self {
        if cli.rpc {
            Self::RpcReadOnly
        } else {
            Self::Local
        }
    }

    pub(crate) fn ensure_writable(self, path: &Path, trusted: bool) -> anyhow::Result<()> {
        let mandatory = path.file_name().is_some_and(|name| name == "settings.json")
            || path.starts_with(crate::config::settings::auth_dir());
        if matches!(self, Self::RpcReadOnly) && (!trusted || mandatory) {
            anyhow::bail!(
                "configuration is read-only in RPC mode: {}; edit it locally, then reload resources",
                path.display()
            );
        }
        Ok(())
    }
}

pub(crate) fn initialize(cli: &Cli) -> anyhow::Result<()> {
    PROCESS_ACCESS
        .set(ConfigAccess::for_cli(cli))
        .map_err(|_| anyhow::anyhow!("configuration access policy is already initialized"))
}

pub(crate) fn initialize_file_trust(trusted: bool) -> anyhow::Result<()> {
    REMOTE_FILE_TRUST
        .set(trusted)
        .map_err(|_| anyhow::anyhow!("remote file trust is already initialized"))
}

pub(crate) fn trusts_remote_files(settings: &crate::config::Settings) -> bool {
    REMOTE_FILE_TRUST
        .get()
        .copied()
        .unwrap_or(settings.trust_remote)
}

pub(crate) fn is_read_only() -> bool {
    matches!(PROCESS_ACCESS.get(), Some(ConfigAccess::RpcReadOnly))
}

pub(crate) fn ensure_local_authentication() -> anyhow::Result<()> {
    anyhow::ensure!(
        !is_read_only(),
        "OAuth login is unavailable in RPC mode; sign in through the interactive CLI"
    );
    Ok(())
}

pub(crate) fn ensure_writable(path: &Path) -> anyhow::Result<()> {
    PROCESS_ACCESS
        .get()
        .copied()
        .unwrap_or_default()
        .ensure_writable(path, REMOTE_FILE_TRUST.get().copied().unwrap_or(false))
}

fn protects_execution(cli: &crate::cli::Cli) -> bool {
    is_read_only() || matches!(ConfigAccess::for_cli(cli), ConfigAccess::RpcReadOnly)
}

pub(crate) fn mcp_transport_policy(
    cli: &crate::cli::Cli,
) -> phi_sdk::extension::mcp::McpTransportPolicy {
    use phi_sdk::extension::mcp::McpTransportPolicy;
    if protects_execution(cli) {
        McpTransportPolicy::CommandOnly
    } else {
        McpTransportPolicy::All
    }
}

pub(crate) fn execution_sandbox(
    cli: &crate::cli::Cli,
    settings: &crate::config::Settings,
    working_dir: &Path,
) -> anyhow::Result<phi_sdk::sandbox::Sandbox> {
    let protected = protects_execution(cli);
    let sandbox = settings.build_sandbox(
        cli.resolve_sandbox(settings) || protected,
        working_dir.to_path_buf(),
    )?;
    if !protected {
        return Ok(sandbox);
    }
    let local_dir = std::env::current_dir()?;
    let project = crate::config::settings::project_scope_dir_for(working_dir);
    let mut roots = vec![
        local_dir.join(settings_write_root(crate::config::settings::phi_path())),
        local_dir.join(crate::config::SettingsLock::path_for(
            &crate::config::settings::settings_file_path(),
        )),
        local_dir.join(crate::config::settings::auth_dir()),
    ];
    roots.extend(project.as_ref().map(|dir| settings_write_root(dir.clone())));
    roots.extend(project.as_ref().map(|dir| {
        local_dir.join(crate::config::SettingsLock::path_for(
            &dir.join("settings.json"),
        ))
    }));
    if !trusts_remote_files(settings) {
        roots.push(local_dir.join(crate::config::settings::phi_path()));
        roots.push(local_dir.join(phi_sdk::workspace::phi_session_root()));
        roots.extend(project);
        if let Some(memory) = settings
            .effective_memory()
            .as_ref()
            .filter(|memory| memory.enabled)
        {
            let settings = phi_sdk::extension::memory::settings_from_config(memory);
            roots.push(local_dir.join(phi_sdk::extension::memory::get_memory_repo_dir(&settings)));
        }
    }
    sandbox.with_write_protection(roots, Vec::new())
}

fn settings_write_root(directory: std::path::PathBuf) -> std::path::PathBuf {
    let settings = directory.join("settings.json");
    // A missing file cannot use the Sandbox's read-only directory mount.
    if settings.exists() {
        settings
    } else {
        directory
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::path::Path;

    use super::*;
    use crate::cli::Cli;

    #[test]
    fn remote_mcp_policy_rejects_url_transports_without_affecting_local_use() {
        use phi_sdk::extension::mcp::McpTransportPolicy;
        let local = Cli::parse_from(["phi"]);
        let remote = Cli::parse_from(["phi", "--yolo", "--rpc"]);
        assert_eq!(mcp_transport_policy(&local), McpTransportPolicy::All);
        assert_eq!(
            mcp_transport_policy(&remote),
            McpTransportPolicy::CommandOnly
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn remote_file_trust_opens_memory_without_changing_local_mode() {
        use crate::config::{SandboxConfig, Settings};
        use phi_sdk::extension::memory::{MemoryConfig, MemoryDirSettings};
        let project =
            std::env::temp_dir().join(format!("phi-memory-policy-{}", uuid::Uuid::new_v4()));
        let repository = project.join("memory");
        std::fs::create_dir_all(repository.join("project/core")).unwrap();
        let content = repository.join("project/core/note.md");
        let state = repository.join("project/anchors.jsonl");
        let mut config = Settings {
            memory: Some(MemoryConfig {
                memory_dir: Some(MemoryDirSettings {
                    local_path: Some(repository.display().to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            sandbox: Some(SandboxConfig {
                enabled: Some(false),
                whitelist: vec![repository.clone()],
                ..Default::default()
            }),
            ..Default::default()
        };
        for (arguments, trusted, allowed) in [
            (vec!["phi", "--yolo", "--rpc"], false, false),
            (vec!["phi", "--yolo", "--rpc"], true, true),
            (vec!["phi"], false, true),
        ] {
            config.trust_remote = trusted;
            let sandbox =
                execution_sandbox(&Cli::parse_from(arguments), &config, &project).unwrap();
            for path in [&content, &state] {
                std::fs::write(path, "original").unwrap();
                let output = sandbox
                    .wrap_program(
                        "sh",
                        &[
                            "-c".into(),
                            "printf changed > \"$1\"".into(),
                            "probe".into(),
                            path.display().to_string(),
                        ],
                    )
                    .output()
                    .await
                    .unwrap();
                assert_eq!(output.status.success(), allowed);
                assert_eq!(
                    std::fs::read_to_string(path).unwrap(),
                    if allowed { "changed" } else { "original" }
                );
            }
        }
        std::fs::remove_dir_all(project).unwrap();
    }

    #[tokio::test]
    async fn remote_execution_policy_protects_each_project_without_changing_local_mode() {
        use crate::config::{SandboxConfig, Settings};
        let root =
            std::env::temp_dir().join(format!("phi-execution-policy-{}", uuid::Uuid::new_v4()));
        for name in ["first", "child"] {
            let project = root.join(name);
            let protected = project.join(".phi");
            std::fs::create_dir_all(&protected).unwrap();
            let path = protected.join("settings.json");
            std::fs::write(&path, "original").unwrap();
            let mut settings = Settings {
                sandbox: Some(SandboxConfig {
                    enabled: Some(false),
                    whitelist: vec![protected.clone()],
                    ..Default::default()
                }),
                ..Default::default()
            };
            let local = Cli::parse_from(["phi"]);
            assert!(
                !execution_sandbox(&local, &settings, &project)
                    .unwrap()
                    .is_enabled()
            );
            let remote = Cli::parse_from(["phi", "--yolo", "--rpc"]);
            for trusted in [false, true] {
                settings.trust_remote = trusted;
                let sandbox = execution_sandbox(&remote, &settings, &project).unwrap();
                assert!(sandbox.is_enabled());
                let output = sandbox
                    .wrap_command("printf changed > .phi/settings.json")
                    .output()
                    .await
                    .unwrap();
                assert!(!output.status.success());
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
                let output = sandbox
                    .wrap_command("printf optional > .phi/mcp.json")
                    .output()
                    .await
                    .unwrap();
                assert_eq!(output.status.success(), trusted);
            }
            std::fs::remove_file(&path).unwrap();
            let sandbox = execution_sandbox(&remote, &settings, &project).unwrap();
            let output = sandbox
                .wrap_command("printf created > .phi/settings.json")
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            assert!(!path.exists());
            let output = sandbox
                .wrap_command("printf public > public.txt")
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rpc_protects_settings_transaction_locks() {
        const CHILD_MARKER: &str = "PHI_SETTINGS_LOCK_PROTECTION_TEST";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let root = std::env::temp_dir().join(format!(
                "phi-settings-lock-protection-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "security::tests::rpc_protects_settings_transaction_locks",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1")
                .env("PHI_CONFIG_DIR", root.join("config"))
                .current_dir(&root)
                .output()
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = std::env::current_dir().unwrap();
        let lock_dir =
            crate::config::SettingsLock::path_for(&crate::config::settings::settings_file_path());
        std::fs::create_dir_all(&lock_dir).unwrap();
        std::fs::write(crate::config::settings::settings_file_path(), "{}").unwrap();
        let remote = Cli::parse_from(["phi", "--rpc"]);
        let settings = crate::config::Settings {
            trust_remote: true,
            ..Default::default()
        };
        let sandbox = execution_sandbox(&remote, &settings, &root).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let lock_path = lock_dir.join("interference.lock");
        let blocked = runtime.block_on(async {
            sandbox
                .wrap_command(&format!("printf changed > {}", lock_path.display()))
                .output()
                .await
                .unwrap()
        });
        assert!(!blocked.status.success());
        assert!(!lock_path.exists());
        let optional = runtime.block_on(async {
            sandbox
                .wrap_command("printf optional > config/mcp.json")
                .output()
                .await
                .unwrap()
        });
        assert!(
            optional.status.success(),
            "{}",
            String::from_utf8_lossy(&optional.stderr)
        );
    }

    #[test]
    fn rpc_denies_config_writes_but_local_commands_remain_editable() {
        let cli = Cli::parse_from(["phi", "--rpc"]);
        let error = ConfigAccess::for_cli(&cli)
            .ensure_writable(Path::new("/tmp/config/settings.json"), true)
            .unwrap_err();
        assert!(error.to_string().contains("read-only"));
        assert!(error.to_string().contains("reload resources"));
        for arguments in [
            vec!["phi"],
            vec!["phi", "config", "--local"],
            vec!["phi", "list"],
        ] {
            let cli = Cli::parse_from(arguments);
            ConfigAccess::for_cli(&cli)
                .ensure_writable(Path::new("/tmp/config/settings.json"), false)
                .unwrap();
        }
    }

    #[test]
    fn remote_config_writers_reject_before_io() {
        const CHILD_FLAG: &str = "PHI_TEST_CONFIG_READ_ONLY";
        if std::env::var_os(CHILD_FLAG).is_none() {
            let root =
                std::env::temp_dir().join(format!("phi-config-access-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "security::tests::remote_config_writers_reject_before_io",
                    "--nocapture",
                ])
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
            return;
        }
        assert_remote_writers_rejected();
    }

    fn assert_builtin_skills_remain_available() {
        let memory = phi_sdk::extension::memory::MemoryConfig {
            enabled: true,
            ..Default::default()
        };
        let skills = crate::resources::prompts::skills::load_for(
            &std::env::current_dir().unwrap(),
            &phi_sdk::extension::builtin_skills(&phi_sdk::config::RuntimeConfig {
                extensions: phi_sdk::extension::ExtensionConfig {
                    memory: Some(memory),
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
        assert!(
            skills
                .skills
                .iter()
                .any(|skill| skill.name == "memory-write"
                    && skill.source == crate::resources::prompts::skills::SkillSource::Builtin)
        );
        assert!(!crate::config::settings::phi_path().exists());
        assert_remote_reload_after_manual_edit();
    }

    fn assert_remote_reload_after_manual_edit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (root, policy, test_runtime) =
                crate::runtime::preparation::tests::session_test_runtime();
            policy.install(&test_runtime.preparation);
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let agent = Box::pin(
                {
                    policy.install(&test_runtime.preparation);
                    crate::runtime::preparation::SessionWorkflows::new(
                        test_runtime.sessions.clone(),
                        test_runtime.preparation.clone(),
                    )
                    .unwrap()
                }
                .start_session(session, false),
            )
            .await
            .unwrap();
            let generation = agent.snapshot().await.runtime_generation_id;
            let path = root.join(".phi/settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let config = r#"{"permission":{"bash":"invalid"}}"#;
            std::fs::write(&path, config).unwrap();
            let error =
                Box::pin(test_runtime.preparation.clone().reload_resources(
                    &test_runtime.sessions.session_access(agent.instance().id()),
                ))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("permission.bash"));
            assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
            test_runtime
                .sessions
                .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                .await
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    fn assert_remote_writers_rejected() {
        use crate::resources::{Override, Resource, ResourceKind, ResourcePolicy, Scope};
        let cli = Cli::parse_from(["phi", "--rpc"]);
        initialize(&cli).unwrap();
        let working_dir = std::env::current_dir().unwrap();
        let mut policy = ResourcePolicy::load_for(&working_dir).unwrap();
        let resource = Resource {
            path: working_dir.join(".phi/prompts/example.md"),
            kind: ResourceKind::Prompts,
            scope: Scope::Project,
        };
        for scope in [Scope::Global, Scope::Project] {
            let error = policy.set(&resource, scope, Override::Load).unwrap_err();
            assert!(error.to_string().contains("read-only"), "{error}");
        }
        assert!(!crate::config::settings::phi_path().exists());
        assert!(!working_dir.join(".phi").exists());
        {
            let path = crate::config::settings::mcp_config_file_path();
            let error = crate::config::settings::replace_config_file(&path, "{}").unwrap_err();
            assert!(error.to_string().contains("read-only"));
        }
        let diagnostics = crate::resources::prompts::skills::initialize_builtin_links(
            &phi_sdk::extension::builtin_skills(&phi_sdk::config::RuntimeConfig::default()),
        );
        assert!(!crate::config::settings::phi_path().exists());
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("failed to create"))
        );
        assert_builtin_skills_remain_available();
        std::fs::create_dir_all(crate::config::settings::phi_path()).unwrap();
        let path = crate::config::settings::settings_file_path();
        std::fs::write(&path, "{\"defaultModel\":\"manually-edited\"}").unwrap();
        assert_eq!(
            crate::config::settings::load()
                .unwrap()
                .default_model
                .as_deref(),
            Some("manually-edited")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"defaultModel\":\"manually-edited\"}"
        );
        initialize_file_trust(true).unwrap();
        assert!(trusts_remote_files(&crate::config::Settings::default()));
        assert!(initialize_file_trust(false).is_err());
        assert!(ensure_writable(&path).is_err());
        assert!(ensure_writable(&crate::config::settings::auth_dir().join("token.json")).is_err());
        assert!(ensure_local_authentication().is_err());
        {
            let mcp = crate::config::settings::mcp_config_file_path();
            crate::config::settings::replace_config_file(&mcp, "{}").unwrap();
            assert_eq!(std::fs::read_to_string(mcp).unwrap(), "{}");
        }
    }
}
