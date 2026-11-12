//! Process arguments, precedence helpers and standalone command/print adapters.
//! Application runtime owns Agent preparation and Session operations; CLI owns entry/exit policy.

pub(crate) mod config;
pub(crate) mod list;
pub(crate) mod print;
pub(crate) mod web;

use clap::{Parser, Subcommand};
use compact_str::CompactString;
use phi_tui::{
    Frame, InputContext, InputOutcome, KeyInput, MountOptions, Picker, RenderContext, RuntimeEvent,
    StyledLine, TerminalGuard, Theme, ThemeColor, TuiRenderer, TuiRuntime, Widget,
};

use crate::config::Settings;
use crate::interactive::{
    RESUME_PICKER_MAX_VISIBLE, ResumeDeleteOutcome, ResumePickerAction, UiPickerState,
    delete_resume_candidate, handle_resume_picker_input, resume_picker,
};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "phi",
    version,
    about = "Phi Coding Agent",
    disable_help_subcommand = true,
    args_conflicts_with_subcommands = true,
    override_usage = "phi [OPTIONS] [--] [MESSAGE]...\n       phi <COMMAND> [OPTIONS]",
    styles = clap::builder::Styles::styled()
        .header(clap::builder::styling::Style::new().bold())
        .usage(clap::builder::styling::Style::new().bold()),
    after_help = "\x1b[1mExamples:\x1b[0m\n  # Interactive mode\n  phi\n\n  # Start with a prompt\n  phi \"Explain this repository\"\n\n  # Print a response and exit\n  phi -p \"Summarize this repository\"\n\n  # Prompt beginning with a dash\n  phi -p -- \"- Summarize these points\"\n\n  # Continue the most recent session\n  phi --continue\n\n  # Start a named session\n  phi --name \"Review phi staged files\"\n\n  # Machine control over stdin/stdout\n  phi --rpc"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(
        long,
        conflicts_with_all = ["print", "resume", "continue_session", "session", "name", "no_session", "message"],
        help = "Run machine-control protocol over stdin/stdout"
    )]
    pub rpc: bool,

    #[arg(
        long,
        conflicts_with_all = ["rpc", "print", "resume", "continue_session", "session", "message"],
        help = "Start or reuse the local gateway and open its web page"
    )]
    pub web: bool,

    #[arg(long, conflicts_with_all = ["local", "web", "rpc", "print", "message", "provider", "model", "name", "no_session", "resume", "continue_session"], help = "Open the TUI as a client of the local gateway")]
    pub gateway: bool,

    #[arg(long, conflicts_with_all = ["gateway", "web", "rpc", "print"], help = "Run the independent local TUI (default)")]
    pub local: bool,

    #[arg(short = 'p', long = "print", help = "Print response and exit")]
    pub print: bool,

    #[arg(
        long,
        help = "Append diagnostic logs to a file (level: RUST_LOG; never shown in conversation)"
    )]
    pub log_file: Option<std::path::PathBuf>,

    #[arg(short = 'c', long = "continue", help = "Continue most recent session")]
    pub continue_session: bool,

    #[arg(short = 'r', long = "resume", help = "Browse and select a session")]
    pub resume: bool,

    #[arg(
        long = "session",
        help = "Use specific session file or ID in standalone mode"
    )]
    pub session: Option<String>,

    #[arg(
        long = "name",
        conflicts_with_all = ["continue_session", "resume", "session"],
        help = "Name the new session"
    )]
    pub name: Option<String>,

    #[arg(long = "no-session", help = "Ephemeral mode, do not save")]
    pub no_session: bool,

    #[arg(long = "provider", env = "PHI_PROVIDER", help = "API provider")]
    pub provider: Option<String>,

    #[arg(long = "model", help = "Model name")]
    pub model: Option<String>,

    #[arg(
        long = "api-key",
        help = "API key for the provider (WARNING: visible to other users via ps/htop; prefer env vars)"
    )]
    pub api_key: Option<String>,

    #[arg(long = "max-tokens", help = "Maximum tokens in response")]
    pub max_tokens: Option<u64>,

    #[arg(long = "temperature", help = "Model temperature (0.0 to 2.0)")]
    pub temperature: Option<f64>,

    #[arg(short = 't', long = "tools", help = "Allowlist specific tools")]
    pub tools: Vec<String>,

    #[arg(
        long = "codemode",
        help = "Enable governed JavaScript tool composition"
    )]
    pub codemode: bool,

    #[arg(
        long,
        help = "Enable Workflow tools and prefer scripted orchestration for complex tasks"
    )]
    pub workflow: bool,

    #[arg(long = "no-tools", help = "Disable all tools")]
    pub no_tools: bool,

    #[arg(
        long = "permission-mode",
        help = "User authorization: ask, allow, deny or yolo"
    )]
    pub permission_mode: Option<phi_sdk::permission::PermissionMode>,

    #[arg(
        long = "yolo",
        conflicts_with = "permission_mode",
        help = "Bypass user permission rules; capability and security checks still apply"
    )]
    pub yolo: bool,

    #[arg(long = "sandbox", help = "Force-enable the bubblewrap (bwrap) sandbox")]
    pub sandbox: bool,

    #[arg(
        long = "no-context-files",
        short = 'n',
        help = "Disable global and project AGENTS.md/CLAUDE.md loading"
    )]
    pub no_context_files: bool,

    #[arg(hide = true)]
    pub message: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Configure skills, prompts, Lua and built-in extensions.
    Config {
        #[arg(long, help = "Start with project resource overrides")]
        local: bool,
    },
    /// List recent saved Sessions in the current working directory.
    List,
    /// Run the local gateway without opening a browser.
    Gateway {
        #[arg(long, help = "Loopback listen port; 0 selects an available port")]
        port: Option<u16>,
        #[command(subcommand)]
        action: Option<GatewayCommand>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum GatewayCommand {
    /// Show local gateway reachability.
    Status,
    /// Stop the gateway and finalize its Sessions.
    Stop,
    /// Reveal the pairing entry token in an interactive terminal.
    Token,
    /// Review and manage paired browser devices.
    Devices {
        #[command(subcommand)]
        action: DeviceCommand,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum DeviceCommand {
    /// List pending requests and approved devices.
    List,
    /// Approve a request by ID, or the only pending request.
    Approve { request_id: Option<String> },
    /// Reject one exact pending request.
    Reject { request_id: String },
    /// Revoke a device by ID, or the only approved device.
    Revoke { device_id: Option<String> },
}

const SCREEN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

fn screen_input_timeout<A>(runtime: &TuiRuntime<A>) -> std::time::Duration {
    if runtime.render_due() {
        return std::time::Duration::ZERO;
    }
    runtime
        .render_wait_duration()
        .unwrap_or(SCREEN_POLL_INTERVAL)
        .min(SCREEN_POLL_INTERVAL)
}

impl Cli {
    pub fn resolve_model(&self, settings: &Settings) -> CompactString {
        self.model
            .as_deref()
            .or(settings.default_model())
            .map(CompactString::new)
            .unwrap_or_default()
    }

    pub fn resolve_provider(&self, settings: &Settings) -> CompactString {
        self.provider
            .as_deref()
            .or(settings.default_provider())
            .map(CompactString::new)
            .unwrap_or_default()
    }

    pub fn startup_warning(
        &self,
        catalog: &phi_sdk::models::ModelCatalog,
        provider: &str,
        model: &str,
    ) -> Option<String> {
        let runtime_config = phi_sdk::config::RuntimeConfig {
            custom_providers: catalog.custom_providers().into_iter().collect(),
            ..Default::default()
        };
        let mut missing = Vec::new();
        if provider.is_empty() {
            missing.push("provider");
        }
        if model.is_empty() {
            missing.push("model");
        }
        if self.api_key.is_none()
            && !phi_sdk::providers::api_key_available(provider, &runtime_config)
        {
            missing.push("api key");
        }
        (!missing.is_empty()).then(|| format!("missing config: {}", missing.join(", ")))
    }

    pub fn resolve_max_tokens(&self, settings: &Settings) -> Option<u64> {
        self.max_tokens.or(settings.max_tokens)
    }

    pub fn resolve_no_context_files(&self, settings: &Settings) -> bool {
        self.no_context_files || settings.no_context_files.unwrap_or(false)
    }

    pub fn resolve_no_tools(&self, settings: &Settings) -> bool {
        self.no_tools || settings.no_tools.unwrap_or(false)
    }

    pub fn resolve_sandbox(&self, settings: &Settings) -> bool {
        self.sandbox || settings.sandbox_enabled()
    }
}

pub fn select_session() -> anyhow::Result<Option<String>> {
    let picker = resume_picker("", "")?;
    if picker.items.is_empty() {
        return Ok(None);
    }

    select_session_id(picker)
}

fn select_session_id(picker: UiPickerState) -> anyhow::Result<Option<String>> {
    let _guard = TerminalGuard::enter()?;
    let mut renderer = TuiRenderer::new(Theme::default());
    let mut runtime = TuiRuntime::new();
    let handle = runtime.mount_widget(
        ResumeScreen {
            picker,
            confirming_delete: None,
        },
        MountOptions::after_root(InputContext::Picker).focused(),
    );

    loop {
        runtime.render_if_requested(&mut renderer, &Frame::default(), &Theme::default())?;
        let RuntimeEvent::Key(key) =
            runtime.poll_event_with_timeout(screen_input_timeout(&runtime))?
        else {
            continue;
        };
        let InputOutcome::Action(action) = runtime.dispatch_key(key) else {
            continue;
        };
        match action {
            ResumePickerAction::None => {}
            ResumePickerAction::Selected(value) => {
                renderer.finish()?;
                return Ok(resume_id(&value).map(ToString::to_string));
            }
            ResumePickerAction::Cancelled => {
                renderer.finish()?;
                return Ok(None);
            }
            ResumePickerAction::Delete(value) => {
                let Some(id) = resume_id(&value) else {
                    continue;
                };
                if matches!(
                    delete_resume_candidate(id, "")?,
                    ResumeDeleteOutcome::Deleted
                ) {
                    let picker = resume_picker("", "")?;
                    let empty = picker.items.is_empty();
                    runtime.update_widget(handle, |screen| {
                        screen.picker = picker;
                        screen.confirming_delete = None;
                    });
                    if empty {
                        renderer.finish()?;
                        return Ok(None);
                    }
                }
            }
        }
    }
}

fn resume_id(value: &str) -> Option<&str> {
    value.strip_prefix("resume:")
}

struct ResumeScreen {
    picker: UiPickerState,
    confirming_delete: Option<String>,
}

impl Widget for ResumeScreen {
    type Action = ResumePickerAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let picker = Picker {
            state: self.picker.clone(),
            max_visible: RESUME_PICKER_MAX_VISIBLE,
        };
        let mut lines = vec![StyledLine::new("", ThemeColor::Default)];
        lines.extend(picker.render(context));
        lines.push(StyledLine::new("", ThemeColor::Default));
        lines
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match handle_resume_picker_input(&mut self.picker, &mut self.confirming_delete, key) {
            ResumePickerAction::None if matches!(key, KeyInput::None) => InputOutcome::Ignored,
            ResumePickerAction::None => InputOutcome::Consumed,
            action => InputOutcome::Action(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command, DeviceCommand, GatewayCommand, Settings};
    use crate::config::SandboxConfig;

    #[test]
    fn permission_mode_resolves_cli_before_settings() {
        use phi_sdk::permission::{PermissionConfig, PermissionMode};
        let settings = PermissionConfig {
            mode: PermissionMode::Yolo,
            ..Default::default()
        };
        assert_eq!(
            crate::resolve_mode(&Cli::parse_from(["phi"]), &settings),
            PermissionMode::Yolo
        );
        for mode in ["ask", "allow", "deny", "yolo"] {
            let cli = Cli::parse_from(["phi", "--permission-mode", mode]);
            assert_eq!(crate::resolve_mode(&cli, &settings).to_string(), mode);
        }
        assert_eq!(
            crate::resolve_mode(
                &Cli::parse_from(["phi", "--yolo"]),
                &PermissionConfig::default()
            ),
            PermissionMode::Yolo
        );
        assert!(Cli::try_parse_from(["phi", "--permission-mode", "invalid"]).is_err());
        assert!(Cli::try_parse_from(["phi", "--yolo", "--permission-mode", "ask"]).is_err());
    }

    #[test]
    fn mounted_resume_screen_keeps_delete_confirmation_until_controller_success() {
        use phi_tui::{
            Frame, InputContext, InputOutcome, KeyInput, MountOptions, RenderArea, SelectItem,
            Theme, TuiRuntime,
        };
        let mut runtime = TuiRuntime::new();
        let handle = runtime.mount_widget(
            super::ResumeScreen {
                picker: super::UiPickerState::with_kind(
                    super::resume_picker("", "").unwrap().kind,
                    "resume",
                    vec![SelectItem::new("resume:id", "saved")],
                ),
                confirming_delete: None,
            },
            MountOptions::after_root(InputContext::Picker).focused(),
        );
        runtime.dispatch_key(KeyInput::Ctrl('d'));
        assert_eq!(
            runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(super::ResumePickerAction::Delete("resume:id".into()))
        );
        assert_eq!(runtime.widget(handle).unwrap().picker.items.len(), 1);
        assert!(runtime.widget(handle).unwrap().confirming_delete.is_some());
        let frame = runtime.compose(
            RenderArea {
                width: 30,
                height: Some(12),
            },
            &Frame::default(),
            &Theme::default(),
        );
        assert!(
            frame
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("delete session?"))
        );
        assert!(
            frame
                .frame
                .lines
                .iter()
                .all(|line| phi_tui::text::display_width(&line.text) <= 30)
        );
        runtime.dispatch_key(KeyInput::Esc);
        assert!(runtime.widget(handle).unwrap().confirming_delete.is_none());
        assert_eq!(
            runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(super::ResumePickerAction::Selected("resume:id".into()))
        );
        assert_eq!(
            runtime.dispatch_key(KeyInput::Esc),
            InputOutcome::Action(super::ResumePickerAction::Cancelled)
        );
    }

    #[test]
    fn rpc_mode_accepts_runtime_options_and_rejects_other_entry_modes() {
        let rpc = Cli::try_parse_from(["phi", "--rpc", "--model", "test", "--yolo", "--workflow"])
            .unwrap();
        assert!(rpc.workflow);
        assert!(rpc.command.is_none());
        assert_eq!(rpc.model.as_deref(), Some("test"));
        for arguments in [
            vec!["phi", "--rpc", "--print"],
            vec!["phi", "--rpc", "--resume"],
            vec!["phi", "--rpc", "--continue"],
            vec!["phi", "--rpc", "--session", "id"],
            vec!["phi", "--rpc", "--name", "name"],
            vec!["phi", "--rpc", "--no-session"],
            vec!["phi", "--rpc", "prompt"],
            vec!["phi", "--rpc", "config"],
            vec!["phi", "--rpc", "list"],
        ] {
            assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn gateway_is_a_subcommand_without_legacy_aliases() {
        assert!(matches!(
            Cli::parse_from(["phi", "gateway"]).command,
            Some(Command::Gateway { .. })
        ));
        for arguments in [
            vec!["phi", "remote"],
            vec!["phi", "--remote"],
            vec!["phi", "--gateway", "--local"],
            vec!["phi", "--view"],
            vec!["phi", "--rpc", "gateway"],
            vec!["phi", "--web", "gateway"],
            vec!["phi", "gateway", "--managed"],
        ] {
            let parsed = Cli::try_parse_from(&arguments);
            if arguments == ["phi", "remote"] {
                assert!(parsed.is_ok_and(|cli| cli.command.is_none()));
            } else {
                assert!(parsed.is_err(), "{arguments:?}");
            }
        }
    }

    #[test]
    fn gateway_access_selects_a_hosted_session_without_remote_endpoints() {
        let cli = Cli::parse_from(["phi", "--gateway", "--session", "hosted-id"]);
        assert!(cli.gateway);
        assert_eq!(cli.session.as_deref(), Some("hosted-id"));
        assert!(matches!(
            Cli::parse_from(["phi", "gateway", "--port", "0"]).command,
            Some(Command::Gateway {
                port: Some(0),
                action: None
            })
        ));
        for args in [
            vec!["phi", "--gateway", "--url", "http://other-host"],
            vec!["phi", "--gateway", "--token", "secret"],
            vec!["phi", "--gateway", "--resume"],
            vec!["phi", "--gateway", "--continue"],
            vec!["phi", "--gateway", "--model", "other"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn web_mode_accepts_runtime_options_but_not_other_entries() {
        let cli = Cli::try_parse_from([
            "phi",
            "--web",
            "--model",
            "test",
            "--no-session",
            "--name",
            "browser",
        ])
        .unwrap();
        assert!(cli.web);
        assert!(cli.no_session);
        for args in [
            vec!["phi", "--web", "--rpc"],
            vec!["phi", "--web", "--print"],
            vec!["phi", "--web", "--resume"],
            vec!["phi", "--web", "--continue"],
            vec!["phi", "--web", "--session", "id"],
            vec!["phi", "--web", "prompt"],
            vec!["phi", "--web", "config"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn retired_loop_flags_are_rejected() {
        for flag in [
            "--loop",
            "--loop-prompt",
            "--loop-plan",
            "--loop-max",
            "--loop-run",
        ] {
            let error = Cli::try_parse_from(["phi", flag]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn retired_hosting_entries_have_no_command_or_flag_aliases() {
        for name in ["serve", "attach", "dashboard", "control"] {
            let input = Cli::try_parse_from(["phi", name]).unwrap();
            assert!(input.command.is_none());
            assert_eq!(input.message, [name]);
        }
        for flag in ["--stdio", "--address", "--agent"] {
            assert!(Cli::try_parse_from(["phi", flag]).is_err());
        }
    }

    #[test]
    fn help_shows_prompt_and_command_usage_without_arguments_section() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("phi [OPTIONS] [--] [MESSAGE]..."));
        assert!(help.contains("phi <COMMAND> [OPTIONS]"));
        assert!(help.contains("Commands:\n"));
        assert!(help.contains("Options:\n"));
        assert!(!help.contains("Arguments:"));
    }

    #[test]
    fn prompts_and_commands_parse_separately() {
        let prompt = "List all .ts files in src/";
        let print = Cli::parse_from(["phi", "-p", prompt]);
        assert!(print.command.is_none());
        assert_eq!(print.message, [prompt]);
        assert!(print.print);

        assert!(matches!(
            Cli::parse_from(["phi", "list"]).command,
            Some(Command::List)
        ));
        assert_eq!(Cli::parse_from(["phi", "--", "list"]).message, ["list"]);
        assert_eq!(Cli::parse_from(["phi", "help"]).message, ["help"]);
        assert_eq!(
            Cli::try_parse_from(["phi", "--help"]).unwrap_err().kind(),
            clap::error::ErrorKind::DisplayHelp
        );
    }

    #[test]
    fn gateway_approval_accepts_optional_request_id() {
        for request_id in [None, Some("request-id")] {
            let mut arguments = vec!["phi", "gateway", "devices", "approve"];
            arguments.extend(request_id);
            let cli = Cli::try_parse_from(arguments).unwrap();
            let Some(Command::Gateway {
                action:
                    Some(GatewayCommand::Devices {
                        action: DeviceCommand::Approve { request_id: parsed },
                    }),
                ..
            }) = cli.command
            else {
                panic!("expected device approval command");
            };
            assert_eq!(parsed.as_deref(), request_id);
        }
    }

    #[test]
    fn gateway_revocation_accepts_optional_device_id() {
        for device_id in [None, Some("device-id")] {
            let mut arguments = vec!["phi", "gateway", "devices", "revoke"];
            arguments.extend(device_id);
            let cli = Cli::try_parse_from(arguments).unwrap();
            let Some(Command::Gateway {
                action:
                    Some(GatewayCommand::Devices {
                        action: DeviceCommand::Revoke { device_id: parsed },
                    }),
                ..
            }) = cli.command
            else {
                panic!("expected device revocation command");
            };
            assert_eq!(parsed.as_deref(), device_id);
        }
    }

    #[test]
    fn named_session_parses_and_rejects_resume_options() {
        let cli = Cli::parse_from(["phi", "--name", "Review phi staged files"]);
        assert_eq!(cli.name.as_deref(), Some("Review phi staged files"));
        for option in ["--continue", "--resume", "--session"] {
            let mut args = vec!["phi", "--name", "test", option];
            if option == "--session" {
                args.push("existing");
            }
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn startup_warning_uses_catalog_provider_auth() {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-startup-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let models = root.join("models.json");
        std::fs::write(
            &models,
            r#"{"providers":{"catalog-auth":{"api":"openai-codex-responses","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#,
        ).unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![models],
            offline: true,
        })
        .unwrap();
        let cli = Cli::parse_from(["phi"]);
        assert!(
            cli.startup_warning(&catalog, "catalog-auth", "test")
                .is_none()
        );
        assert!(
            cli.startup_warning(
                &phi_sdk::models::ModelCatalog::empty(),
                "catalog-auth",
                "test",
            )
            .unwrap()
            .contains("api key")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_sandbox_default_and_config_override() {
        let cli = Cli::parse_from(["phi"]);
        assert!(cli.resolve_sandbox(&Settings::default()));

        let settings = Settings {
            sandbox: Some(SandboxConfig {
                enabled: Some(false),
                ..SandboxConfig::default()
            }),
            ..Settings::default()
        };
        assert!(!cli.resolve_sandbox(&settings));
    }
}
