use super::state::{MenuKind, PickerKind, PickerState, UiState};
use phi_tui::{BlockRole, NotifyKind, SelectItem};

use std::process::Stdio;

use super::{MemoryAdministration, MemoryNotice};
use crate::commands::MemoryCheckOptions;
use crate::config::McpAuthDiagnostics;
use crate::runtime::attachment::AgentAttachment;

pub(super) struct ShellCommands {
    executable: std::ffi::OsString,
}

impl Default for ShellCommands {
    fn default() -> Self {
        Self {
            executable: std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()),
        }
    }
}

impl ShellCommands {
    pub(super) async fn execute(
        &self,
        display: &str,
        state: &mut UiState,
        attachment: &AgentAttachment,
    ) -> bool {
        let Some(command) = display.strip_prefix('!') else {
            return false;
        };
        let command = command.trim();
        if command.is_empty() {
            state.notify(NotifyKind::Warning, "empty shell command");
            return true;
        }

        if state.features.extensions
            && let Ok(Some(reason)) = attachment.extension_before_command(display).await
        {
            state.notify(NotifyKind::Warning, reason);
            let _ = attachment.extension_after_command(display, true).await;
            return true;
        }

        state.conversation.push_text_block(BlockRole::User, display);
        let block = state
            .conversation
            .project_tool_block("shell", command, None);
        match self.run(command).await {
            Ok(output) => {
                state
                    .conversation
                    .set_tool_output(block, blank_if_empty(output));
            }
            Err(error) => {
                state.conversation.set_tool_output(block, error.to_string());
                state.conversation.set_tool_failed(block);
            }
        }
        if state.features.extensions {
            let _ = attachment.extension_after_command(display, true).await;
        }
        true
    }

    async fn run(&self, command: &str) -> anyhow::Result<String> {
        let output = tokio::process::Command::new(&self.executable)
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let mut result = String::new();
        result.push_str(&String::from_utf8_lossy(&output.stdout));
        if !output.stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            result.push_str(&format!(
                "\nExit code: {}",
                output.status.code().unwrap_or(-1)
            ));
        }
        Ok(result)
    }
}

fn blank_if_empty(output: String) -> String {
    if output.is_empty() {
        String::from("(no output)")
    } else {
        output
    }
}

pub(super) fn handle_mcp_auth_command(
    server_name: &str,
    diagnostics: &McpAuthDiagnostics,
    state: &mut UiState,
) {
    if server_name.is_empty() {
        state.notify(NotifyKind::Error, "Usage: /mcp-auth <server-name>");
        return;
    }
    state.notify(NotifyKind::Error, diagnostics.diagnostic(server_name));
}

fn project_memory_notice(state: &mut UiState, notice: MemoryNotice) {
    match notice {
        MemoryNotice::Warning { message } => state.notify(NotifyKind::Warning, message),
        MemoryNotice::Info { message } => state.notify(NotifyKind::Info, message),
    }
}

pub(super) fn handle_memory_check_command(
    options: &MemoryCheckOptions,
    memory: &MemoryAdministration,
    state: &mut UiState,
) {
    for notice in memory.check(options) {
        project_memory_notice(state, notice);
    }
}

pub(super) fn handle_memory_refresh_command(memory: &MemoryAdministration, state: &mut UiState) {
    project_memory_notice(state, memory.refresh());
}

pub(super) fn handle_memory_review_command(
    limit: usize,
    memory: &MemoryAdministration,
    state: &mut UiState,
) {
    match memory.review(limit) {
        Ok(Some(review)) => super::overlays::open_memory_review(
            state,
            super::components::panel::memory::MemoryReviewState::new(review),
        ),
        Ok(None) => state.notify(NotifyKind::Warning, "tape memory is disabled in settings"),
        Err(error) => state.notify(NotifyKind::Error, error.to_string()),
    }
}

#[derive(Default)]
pub(super) struct Authentication {
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Authentication {
    pub(super) async fn execute(&mut self, provider: &str, state: &mut UiState) {
        if let Err(error) = crate::security::ensure_local_authentication() {
            state.notify(NotifyKind::Error, error.to_string());
            return;
        }
        if self.task.is_some() {
            state.notify(NotifyKind::Warning, "OAuth login is already in progress");
            return;
        }
        if provider.is_empty() {
            state.open_picker(
                MenuKind::Other,
                PickerState::with_kind(
                    PickerKind::Login,
                    "select login provider",
                    vec![
                        SelectItem::with_label("login:openai", "OpenAI", "API key"),
                        SelectItem::with_label(
                            "login:openai-codex",
                            "OpenAI Codex",
                            "Subscription",
                        ),
                    ],
                ),
            );
            state.runtime.request_render();
            return;
        }
        if provider == "openai" {
            state.notify(
                NotifyKind::Info,
                "OpenAI uses API keys; configure OPENAI_API_KEY.",
            );
            return;
        }
        if provider != "openai-codex" {
            state.notify(
                NotifyKind::Warning,
                format!("unsupported login provider: {provider}"),
            );
            return;
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let provider = provider.to_string();
        let login = tokio::spawn(async move {
            phi_sdk::providers::login_chatgpt(&provider, move |url, code| {
                let _ = tx.send((url, code));
            })
            .await
        });

        match rx.recv().await {
            Some((url, code)) => {
                self.task = Some(login);
                show_chatgpt_device_code(state, &url, &code).await;
            }
            None => report_login_result(state, login.await),
        }
    }

    pub(super) async fn poll(&mut self, state: &mut UiState) -> bool {
        if !self.task.as_ref().is_some_and(|task| task.is_finished()) {
            return false;
        }
        let task = self.task.take().unwrap();
        report_login_result(state, task.await);
        true
    }
}

fn report_login_result(
    state: &mut UiState,
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(())) => state.notify(NotifyKind::Info, "OpenAI Codex authorized for 30 days"),
        Ok(Err(error)) => state.notify(NotifyKind::Error, format!("login failed: {error}")),
        Err(error) => state.notify(NotifyKind::Error, format!("login task failed: {error}")),
    }
}

async fn show_chatgpt_device_code(state: &mut UiState, url: &str, code: &str) {
    state.notify(
        NotifyKind::Info,
        format!("Open this URL: {url}\nEnter code: {code}"),
    );
    if let Err(error) = open_browser(url).await {
        state.notify(
            NotifyKind::Warning,
            format!("failed to open browser: {error}"),
        );
    }
}

async fn open_browser(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = tokio::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = tokio::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };

    #[cfg(not(target_os = "windows"))]
    command.arg(url);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn remote_login_is_rejected_before_picker_or_auth_effects() {
        use clap::Parser;
        const CHILD_FLAG: &str = "PHI_TEST_REMOTE_LOGIN";
        if std::env::var_os(CHILD_FLAG).is_none() {
            let root =
                std::env::temp_dir().join(format!("phi-login-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "interactive::commands::tests::remote_login_is_rejected_before_picker_or_auth_effects",
                    "--nocapture",
                ])
                .env(CHILD_FLAG, "1")
                .env("PHI_CONFIG_DIR", root.join("config"))
                .env("PHI_AUTH_DIR", root.join("auth"))
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
        let cli = crate::cli::Cli::parse_from(["phi", "--rpc"]);
        crate::security::initialize(&cli).unwrap();
        crate::security::initialize_file_trust(true).unwrap();
        let mut state = super::UiState::default();
        let mut authentication = super::Authentication::default();
        authentication.execute("", &mut state).await;
        assert!(
            state.picker.is_none(),
            "remote participation must reject the login picker"
        );
        authentication.execute("openai-codex", &mut state).await;
        assert!(authentication.task.is_none());
        assert!(!crate::config::settings::auth_dir().exists());
        #[cfg(target_os = "linux")]
        assert_remote_auth_state_is_protected().await;
    }

    #[cfg(target_os = "linux")]
    async fn assert_remote_auth_state_is_protected() {
        use clap::Parser;
        let auth_dir = crate::config::settings::auth_dir();
        std::fs::create_dir_all(&auth_dir).unwrap();
        let path = auth_dir.join("openai-codex.json");
        std::fs::write(&path, "canonical-auth-state").unwrap();
        let settings = crate::config::settings::settings_file_path();
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{}").unwrap();
        let cli = crate::cli::Cli::parse_from(["phi", "--yolo", "--rpc"]);
        let settings = crate::config::Settings {
            sandbox: Some(crate::config::SandboxConfig {
                enabled: Some(false),
                whitelist: vec![auth_dir],
                ..Default::default()
            }),
            ..Default::default()
        };
        let sandbox =
            crate::security::execution_sandbox(&cli, &settings, &std::env::current_dir().unwrap())
                .unwrap();
        let output = sandbox
            .wrap_program(
                "sh",
                &[
                    "-c".into(),
                    "printf overwritten > \"$1\"".into(),
                    "probe".into(),
                    path.display().to_string(),
                ],
            )
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "canonical-auth-state"
        );
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
        assert_eq!(std::fs::read_to_string("public.txt").unwrap(), "public");
    }

    #[tokio::test]
    async fn shell_preserves_output_and_exit_status() {
        let shell = super::ShellCommands {
            executable: "/bin/sh".into(),
        };
        for (command, expected) in [
            ("printf output", "output"),
            ("printf error >&2", "error"),
            (
                "printf output; printf error >&2; exit 3",
                "output\nerror\nExit code: 3",
            ),
        ] {
            assert_eq!(shell.run(command).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn login_completion_is_observed_without_blocking_the_ui() {
        let mut state = super::UiState::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut authentication = super::Authentication {
            task: Some(tokio::spawn(async move {
                rx.await?;
                Ok(())
            })),
        };
        assert!(!authentication.poll(&mut state).await);
        state = super::UiState::default();
        authentication.execute("", &mut state).await;
        assert!(state.picker.is_none());
        assert!(matches!(
            &state.conversation.blocks.last().unwrap().block.body,
            phi_tui::BlockBody::Notify(notice)
                if notice.message == "OAuth login is already in progress"
        ));
        tx.send(()).unwrap();
        while !authentication.task.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(authentication.poll(&mut state).await);
        assert!(authentication.task.is_none());
        assert!(!authentication.poll(&mut state).await);
    }
}
