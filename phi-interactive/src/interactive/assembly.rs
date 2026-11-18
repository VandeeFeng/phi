use super::components::input::UiAction;
use super::components::picker::{model, session, switch};
use super::run;
use crate::cli::Cli;
use crate::commands::{BuiltinCommand, CommandInvocation, CommandRegistry};
use crate::config::Settings;
use crate::runtime::RuntimeDeclarations;
use crate::runtime::attachment::AgentAttachment;
use crate::runtime::selection::SessionSelection;
use crate::session::Session;
use phi_sdk::extension::CommandTarget;
use std::cell::RefCell;
use std::rc::Rc;

// Recovery must not enable modules omitted by assembly.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct InteractiveFeatures {
    pub(super) models: model::ModelFeatures,
    pub(super) switches: switch::SwitchFeatures,
    pub(super) sessions: session::SessionFeatures,
    pub(super) navigation: bool,
    pub(super) activities: bool,
    pub(super) mcp: bool,
    pub(super) authentication: bool,
    pub(super) memory: bool,
    pub(super) extensions: bool,
    pub(super) agent_mentions: bool,
    pub(super) workflow_observation: bool,
}

impl InteractiveFeatures {
    pub(super) fn command(self, command: &CommandInvocation) -> bool {
        match command {
            CommandInvocation::Extension { .. } => self.extensions,
            CommandInvocation::Builtin { command } => match command {
                BuiltinCommand::NewSession
                | BuiltinCommand::ResumePicker(_)
                | BuiltinCommand::Switch(_)
                | BuiltinCommand::Reload
                | BuiltinCommand::Workflow => true,
                BuiltinCommand::Model(command) => self.models.command(command),
                BuiltinCommand::RewindPicker => self.navigation,
                BuiltinCommand::Tasks => self.activities,
                BuiltinCommand::Login(_) => self.authentication,
                BuiltinCommand::McpAuth(_) | BuiltinCommand::Mcp => self.mcp,
                BuiltinCommand::MemoryCheck(_)
                | BuiltinCommand::MemoryRefresh
                | BuiltinCommand::MemoryReview(_) => self.memory,
            },
        }
    }

    pub(super) fn action(self, action: &UiAction) -> bool {
        if let Some(supported) = self
            .switches
            .action(action)
            .or_else(|| self.sessions.action(action))
        {
            return supported;
        }
        match action {
            UiAction::RunExtensionKeybinding(_) => self.extensions,
            UiAction::Pick(value) if value.starts_with("login:") => self.authentication,
            UiAction::Pick(value) if value.starts_with("rewind:") => self.navigation,
            UiAction::Pick(value) if value.starts_with("resume:") => true,
            UiAction::Pick(value) if value.starts_with("mcp:") => self.mcp,
            UiAction::Pick(_) | UiAction::CycleSwitch(_) | UiAction::AdjustThinkingLevel(_) => true,
            UiAction::DeletePick(_)
            | UiAction::CycleSwitchField { .. }
            | UiAction::SetSwitchAlias { .. } => false,
            UiAction::McpElicitationSubmit(_) | UiAction::McpElicitationOpenUrl(_) => self.mcp,
            UiAction::ToggleTasks
            | UiAction::BackgroundRunningBash
            | UiAction::CloseBashSelector
            | UiAction::BackgroundBash { .. }
            | UiAction::StopBash { .. }
            | UiAction::OpenBashOutput { .. }
            | UiAction::OpenTaskOwner(_)
            | UiAction::CancelSubagent(_)
            | UiAction::EnterSubagent(_) => self.activities,
            UiAction::OpenMemoryAnchor(_) => self.navigation && self.memory,
            UiAction::None
            | UiAction::SurfaceKey { .. }
            | UiAction::Resize
            | UiAction::Submit { .. }
            | UiAction::ToggleAllBlocksExpanded
            | UiAction::PermissionAllowOnce
            | UiAction::PermissionAllowAlways
            | UiAction::PermissionDeny
            | UiAction::McpElicitationDecline
            | UiAction::McpElicitationCancel
            | UiAction::CancelRun
            | UiAction::CloseActiveOverlay
            | UiAction::Quit => true,
        }
    }

    pub(super) fn bindings(
        self,
        resolver: phi_tui::KeyBindingResolver<UiAction>,
    ) -> phi_tui::KeyBindingResolver<UiAction> {
        resolver
            .bindings()
            .iter()
            .filter(|binding| self.action(&binding.action))
            .fold(phi_tui::KeyBindingResolver::new(), |resolver, binding| {
                resolver.bind(
                    binding.context.clone(),
                    binding.key.clone(),
                    binding.action.clone(),
                )
            })
    }

    pub(super) fn filter(self, mut declarations: RuntimeDeclarations) -> RuntimeDeclarations {
        let registry = CommandRegistry::new(&declarations.capabilities.extension_commands);
        registry.retain_supported(&mut declarations.commands, |invocation| {
            self.command(invocation)
        });
        declarations
            .capabilities
            .extension_commands
            .retain(|command| match &command.target {
                CommandTarget::Prompt => true,
                CommandTarget::Handler => self.extensions,
                CommandTarget::Application { .. } => {
                    CommandRegistry::new(std::slice::from_ref(command))
                        .resolve(&format!("/{}", command.name))
                        .ok()
                        .flatten()
                        .is_some_and(|invocation| self.command(&invocation))
                }
            });
        if !self.extensions {
            declarations.capabilities.extension_keybindings.clear();
        }
        declarations
    }
}

pub(super) struct InteractiveComponents<'a> {
    pub(super) models: model::ModelPicker,
    pub(super) switches: switch::SwitchPicker,
    pub(super) sessions: session::SessionPicker,
    pub(super) memory: Option<&'a super::memory::MemoryAdministration>,
    pub(super) navigation: Option<&'a crate::runtime::preparation::SessionWorkflows>,
    pub(super) mcp_auth: Option<&'a crate::config::McpAuthDiagnostics>,
    pub(super) mcp_read_only: bool,
    pub(super) mcp_show_status: bool,
    pub(super) mcp_notify: bool,
    pub(super) authentication: Option<super::commands::Authentication>,
    pub(super) activities: Option<super::components::panel::activities::TaskRefresh>,
    pub(super) extensions: Option<super::extensions::ExtensionTaskQueue>,
    pub(super) shell: Option<super::commands::ShellCommands>,
    pub(super) agent_mentions: Option<super::components::mentions::AgentMentions>,
    pub(super) workflow_observation: Option<super::components::workflow::WorkflowRefresh>,
}

impl<'a> InteractiveComponents<'a> {
    pub(super) fn new(
        models: model::ModelPicker,
        switches: switch::SwitchPicker,
        sessions: session::SessionPicker,
    ) -> Self {
        Self {
            models,
            switches,
            sessions,
            memory: None,
            navigation: None,
            mcp_auth: None,
            mcp_read_only: true,
            mcp_show_status: false,
            mcp_notify: false,
            authentication: None,
            activities: None,
            extensions: None,
            shell: None,
            agent_mentions: None,
            workflow_observation: None,
        }
    }

    pub(super) fn gateway() -> Self {
        Self::new(
            model::ModelPicker::runtime(),
            switch::SwitchPicker::new(),
            session::SessionPicker::new(SessionSelection::Hosted),
        )
    }

    pub(super) fn features(&self) -> InteractiveFeatures {
        InteractiveFeatures {
            models: self.models.features(),
            switches: self.switches.features(),
            sessions: self.sessions.features(),
            memory: self.memory.is_some(),
            navigation: self.navigation.is_some(),
            mcp: self.mcp_auth.is_some(),
            authentication: self.authentication.is_some(),
            activities: self.activities.is_some(),
            extensions: self.extensions.is_some(),
            agent_mentions: self.agent_mentions.is_some(),
            workflow_observation: self.workflow_observation.is_some(),
        }
    }
}

pub async fn run_interactive(
    sessions: std::sync::Arc<crate::runtime::SessionAssociations>,
    memory: super::memory::MemoryAdministration,
    mcp_auth: crate::config::McpAuthDiagnostics,
    workflows: crate::runtime::preparation::SessionWorkflows,
    models: crate::runtime::preparation::ModelChoices,
    agent_instance: AgentAttachment,
    cli: &Cli,
    settings: &mut Settings,
    session: &mut Session,
    startup_warning: Option<String>,
    startup_error: Option<String>,
) -> anyhow::Result<()> {
    let draft = Rc::new(RefCell::new(models.picker_seed()));
    let mut components = InteractiveComponents::new(
        model::ModelPicker::catalog(models.clone(), draft.clone())
            .with_updates(model::CatalogUpdates::new(models.clone()))
            .with_default_configuration(model::DefaultModelConfiguration::new(draft.clone())),
        switch::SwitchPicker::new().with_configuration(switch::SwitchEditor::new(models, draft)),
        session::SessionPicker::new(SessionSelection::Embedded(workflows.clone()))
            .with_deletion(session::SessionDeletion::new(sessions.clone())),
    );
    components.memory = Some(&memory);
    components.navigation = Some(&workflows);
    components.mcp_auth = Some(&mcp_auth);
    components.mcp_read_only = crate::security::ConfigAccess::for_cli(cli)
        .ensure_writable(
            &crate::config::settings::mcp_config_file_path(),
            crate::security::trusts_remote_files(settings),
        )
        .is_err();
    if let Some(settings) = &settings.mcp_settings {
        components.mcp_show_status = settings.show_status;
        components.mcp_notify = settings.notify;
    }
    components.authentication = Some(super::commands::Authentication::default());
    components.activities = Some(super::components::panel::activities::TaskRefresh::default());
    components.extensions = Some(super::extensions::ExtensionTaskQueue::default());
    components.shell = Some(super::commands::ShellCommands::default());
    components.agent_mentions = Some(super::components::mentions::AgentMentions::default());
    components.workflow_observation = Some(super::components::workflow::WorkflowRefresh::default());
    run(
        components,
        Some(sessions),
        agent_instance,
        session,
        startup_warning,
        startup_error,
    )
    .await
}

pub(crate) async fn run_gateway(cli: &Cli) -> anyhow::Result<()> {
    let service = crate::cli::web::existing()
        .await?
        .ok_or_else(|| anyhow::anyhow!("Gateway is not running; start phi gateway on this host"))?;
    let client = service.client()?;
    let selected =
        crate::cli::web::select_session(&client, cli.session.as_deref(), &std::env::current_dir()?)
            .await?;
    let attachment = AgentAttachment::connect_gateway(client, &selected.id).await?;
    let mut session = attachment.session().await?;
    let components = InteractiveComponents::gateway();
    run(components, None, attachment, &mut session, None, None).await
}

#[cfg(test)]
mod tests {
    use super::super::components::input::UiAction;
    use super::super::state::{PickerKind, UiState};
    use super::*;
    use crate::runtime::preparation::SessionWorkflows;

    #[test]
    fn gateway_features_keep_shared_commands_without_local_administration() {
        use crate::commands::SlashCommand;
        use crate::runtime::attachment::AgentCapabilities;

        let features = InteractiveComponents::gateway().features();
        let declarations = features.filter(RuntimeDeclarations {
            generation_id: "server".into(),
            capabilities: AgentCapabilities {
                extension_commands: [
                    ("handler", CommandTarget::Handler, "test"),
                    ("template", CommandTarget::Prompt, "test"),
                    (
                        "memory-check",
                        CommandTarget::Application {
                            action: "memory-check".into(),
                        },
                        phi_sdk::extension::BuiltinExtension::Memory.path(),
                    ),
                ]
                .into_iter()
                .map(
                    |(name, target, source)| phi_sdk::extension::RegisteredCommand {
                        name: name.into(),
                        description: None,
                        source: source.into(),
                        target,
                        arguments: Some(Default::default()),
                    },
                )
                .collect(),
                extension_keybindings: vec![phi_sdk::extension::RegisteredKeybinding {
                    id: serde_json::from_value(serde_json::json!({"runtime": 1, "binding": 1}))
                        .unwrap(),
                    context: "editor".into(),
                    key: "ctrl+j".into(),
                }],
                ..Default::default()
            },
            commands: [
                "reload",
                "new",
                "resume",
                "switch",
                "model",
                "workflow",
                "skill:review",
                "login",
                "rewind",
                "mcp",
                "mcp-auth",
                "memory-check",
                "tasks",
                "handler",
                "template",
            ]
            .into_iter()
            .map(|name| SlashCommand::builtin(name, name))
            .collect(),
        });
        assert_eq!(
            declarations
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            [
                "reload",
                "new",
                "resume",
                "switch",
                "model",
                "workflow",
                "skill:review",
                "template"
            ]
        );
        assert_eq!(declarations.generation_id, "server");
        assert!(declarations.capabilities.extension_keybindings.is_empty());
        let filtered = serde_json::to_value(&declarations).unwrap();
        assert_eq!(
            serde_json::to_value(features.filter(declarations.clone())).unwrap(),
            filtered
        );
        assert_eq!(declarations.capabilities.extension_commands.len(), 1);
        assert!(matches!(
            declarations.capabilities.extension_commands[0].target,
            CommandTarget::Prompt
        ));
        let bindings = features.bindings(super::super::components::input::key_bindings());
        assert!(
            bindings
                .resolve(phi_tui::InputContext::Editor, &phi_tui::KeyInput::Ctrl('t'))
                .is_none()
        );
        assert_eq!(
            bindings.resolve(phi_tui::InputContext::Editor, &phi_tui::KeyInput::CtrlDown),
            Some(UiAction::CycleSwitch(1))
        );
        for action in [
            UiAction::CycleSwitch(1),
            UiAction::AdjustThinkingLevel(1),
            UiAction::Pick("resume:session".into()),
            UiAction::Pick("switch:background".into()),
            UiAction::Pick("switch:additional".into()),
            UiAction::McpElicitationDecline,
            UiAction::McpElicitationCancel,
        ] {
            assert!(features.action(&action));
        }
        for action in [
            UiAction::ToggleTasks,
            UiAction::Pick("switch:configure".into()),
            UiAction::DeletePick("resume:session".into()),
            UiAction::McpElicitationSubmit(Some(serde_json::json!({}))),
            UiAction::McpElicitationOpenUrl("https://example.com".into()),
        ] {
            assert!(!features.action(&action));
            assert!(InteractiveFeatures::all_enabled().action(&action));
        }
    }

    #[tokio::test]
    async fn recovery_retains_assembled_command_limits() {
        use crate::commands::SlashCommand;
        use crate::runtime::RuntimeDeclarations;
        use crate::runtime::attachment::AgentCapabilities;
        const CAPABILITY_COUNT: usize = 6;

        let (root, policy, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let memory = policy.memory_administration();
        let auth = policy.mcp_auth_diagnostics();
        for capability_mask in 0..(1 << CAPABILITY_COUNT) {
            let [
                memory_enabled,
                navigation_enabled,
                mcp_enabled,
                authentication_enabled,
                activities_enabled,
                extensions_enabled,
            ] = std::array::from_fn::<_, CAPABILITY_COUNT, _>(|index| {
                capability_mask & (1 << index) != 0
            });
            let mut components = InteractiveComponents::new(
                model::ModelPicker::runtime(),
                switch::SwitchPicker::new(),
                session::SessionPicker::new(SessionSelection::Embedded(workflows.clone())),
            );
            components.memory = memory_enabled.then_some(&memory);
            components.navigation = navigation_enabled.then_some(&workflows);
            components.mcp_auth = mcp_enabled.then_some(&auth);
            components.authentication =
                authentication_enabled.then(super::super::commands::Authentication::default);
            components.activities = activities_enabled
                .then(super::super::components::panel::activities::TaskRefresh::default);
            components.extensions =
                extensions_enabled.then(super::super::extensions::ExtensionTaskQueue::default);
            let features = components.features();
            assert_eq!(
                features.action(&UiAction::OpenMemoryAnchor("anchor".into())),
                memory_enabled && navigation_enabled
            );
            let commands = [
                ("new", true),
                ("model", true),
                ("rewind", navigation_enabled),
                ("memory-check", memory_enabled),
                ("mcp", mcp_enabled),
                ("mcp-auth", mcp_enabled),
                ("login", authentication_enabled),
                ("tasks", activities_enabled),
                ("handler", extensions_enabled),
            ];
            let mut state = UiState {
                features,
                ..UiState::default()
            };
            state.editor.set_text("unsent draft");
            let mut capabilities = AgentCapabilities::default();
            for generation in ["initial", "recovered"] {
                super::super::controller::install_runtime_declarations(
                    &mut capabilities,
                    &mut state,
                    RuntimeDeclarations {
                        generation_id: generation.into(),
                        capabilities: AgentCapabilities {
                            extension_commands: vec![
                                phi_sdk::extension::RegisteredCommand {
                                    name: "memory-check".into(),
                                    description: None,
                                    source: phi_sdk::extension::BuiltinExtension::Memory
                                        .path()
                                        .into(),
                                    target: CommandTarget::Application {
                                        action: "memory-check".into(),
                                    },
                                    arguments: Some(Default::default()),
                                },
                                phi_sdk::extension::RegisteredCommand {
                                    name: "handler".into(),
                                    description: None,
                                    source: "test".into(),
                                    target: CommandTarget::Handler,
                                    arguments: Some(Default::default()),
                                },
                            ],
                            ..Default::default()
                        },
                        commands: commands
                            .iter()
                            .map(|(name, _)| SlashCommand::builtin(*name, *name))
                            .collect(),
                    },
                );
                assert_eq!(
                    state
                        .slash_commands
                        .iter()
                        .map(|command| command.name.as_str())
                        .collect::<Vec<_>>(),
                    commands
                        .iter()
                        .filter_map(|(name, enabled)| enabled.then_some(*name))
                        .collect::<Vec<_>>()
                );
                assert_eq!(state.editor.text(), "unsent draft");
                let registry = CommandRegistry::new(&capabilities.extension_commands);
                for command in &state.slash_commands {
                    let invocation = registry
                        .resolve(&format!("/{}", command.name))
                        .expect("an advertised application command must have valid syntax")
                        .expect("an advertised application command must remain dispatchable");
                    assert!(state.features.command(&invocation));
                }
                assert_eq!(
                    state
                        .runtime
                        .key_bindings()
                        .resolve(phi_tui::InputContext::Editor, &phi_tui::KeyInput::Ctrl('t')),
                    activities_enabled.then_some(UiAction::ToggleTasks)
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn picker_capabilities_compose_independently() {
        let (root, policy, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let models = policy.model_choices_owner();
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let agent = Box::pin(workflows.start_session(session, false))
            .await
            .unwrap();
        let attachment = AgentAttachment::new(
            &agent,
            runtime.sessions.session_access(agent.instance().id()),
            runtime.preparation.clone(),
        );
        let session = attachment.session().await.unwrap();

        for capabilities in 0..16 {
            let catalog_updates = capabilities & 1 != 0;
            let default_configuration = capabilities & 2 != 0;
            let switch_configuration = capabilities & 4 != 0;
            let session_deletion = capabilities & 8 != 0;
            let draft = Rc::new(RefCell::new(models.picker_seed()));
            let mut model_picker = model::ModelPicker::catalog(models.clone(), draft.clone());
            if catalog_updates {
                model_picker =
                    model_picker.with_updates(model::CatalogUpdates::new(models.clone()));
            }
            if default_configuration {
                model_picker = model_picker.with_default_configuration(
                    model::DefaultModelConfiguration::new(draft.clone()),
                );
            } else {
                draft.borrow_mut().default_model = None;
            }
            let mut switch_picker = switch::SwitchPicker::new();
            if switch_configuration {
                switch_picker = switch_picker
                    .with_configuration(switch::SwitchEditor::new(models.clone(), draft.clone()));
            }
            let mut session_picker =
                session::SessionPicker::new(SessionSelection::Embedded(workflows.clone()));
            if session_deletion {
                session_picker = session_picker
                    .with_deletion(session::SessionDeletion::new(runtime.sessions.clone()));
            }
            let pickers = InteractiveComponents::new(model_picker, switch_picker, session_picker);
            let features = pickers.features();
            assert_eq!(
                features.command(&CommandInvocation::Builtin {
                    command: BuiltinCommand::Model(crate::commands::ModelCommand::Update),
                }),
                catalog_updates
            );
            let declarations = features.filter(RuntimeDeclarations {
                generation_id: "test".into(),
                capabilities: Default::default(),
                commands: CommandRegistry::new(&[]).declarations(Vec::new(), &Default::default()),
            });
            for (name, accepts_arguments) in [
                ("model", catalog_updates),
                ("resume", true),
                ("switch", true),
            ] {
                let command = declarations
                    .commands
                    .iter()
                    .find(|command| command.name == name)
                    .unwrap();
                assert_eq!(command.arguments.is_some(), accepts_arguments, "{name}");
            }
            assert_eq!(
                features.action(&UiAction::Pick("switch:configure".into())),
                switch_configuration
            );
            assert_eq!(
                features.action(&UiAction::DeletePick("resume:saved".into())),
                session_deletion
            );
            assert!(features.action(&UiAction::Pick("model:local:test".into())));
            assert!(features.action(&UiAction::Pick("switch:plan".into())));
            let mut state = UiState {
                features,
                ..UiState::default()
            };
            pickers.models.open(&attachment, &mut state, &session).await;
            assert!(matches!(
                state.picker.as_ref().unwrap().kind,
                PickerKind::Model
            ));
            if !default_configuration {
                assert!(
                    draft.borrow().default_model.is_none(),
                    "selection must not create configuration"
                );
            }
            let intent = pickers
                .switches
                .pick("switch:configure", &mut state, &session);
            assert_eq!(intent.is_some(), switch_configuration);
            pickers
                .sessions
                .delete(session.id.as_str(), &attachment, &mut state, &session)
                .await;
            assert!(matches!(
                &state.conversation.blocks.last().unwrap().block.body,
                phi_tui::BlockBody::Notify(notice)
                    if notice.message == if session_deletion {
                        "cannot delete current session"
                    } else {
                        "Session deletion is unavailable in this interface."
                    }
            ));
        }
        runtime
            .sessions
            .stop(
                agent.instance().id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
impl InteractiveFeatures {
    pub(super) const fn all_enabled() -> Self {
        Self {
            models: model::ModelFeatures {
                catalog_updates: true,
            },
            switches: switch::SwitchFeatures {
                configuration: true,
            },
            sessions: session::SessionFeatures { deletion: true },
            navigation: true,
            activities: true,
            mcp: true,
            authentication: true,
            memory: true,
            extensions: true,
            agent_mentions: true,
            workflow_observation: true,
        }
    }
}
