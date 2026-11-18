use crate::config::Settings;
use anyhow::Error as CommandError;
use phi_sdk::extension::{CommandTarget, RegisteredCommand};

pub(crate) use phi_sdk::extension::{CommandArgument, CommandArguments};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SlashCommandSource {
    Builtin,
    User,
    Extension,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SlashCommand {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) source: SlashCommandSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arguments: Option<CommandArguments>,
}

impl SlashCommand {
    pub(crate) fn builtin(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(name, description, SlashCommandSource::Builtin)
    }

    pub(crate) fn extension(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(name, description, SlashCommandSource::Extension)
    }

    fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        source: SlashCommandSource,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            source,
            arguments: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelCommand {
    Select,
    Update,
    Set(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryScope {
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryCheckOptions {
    pub(crate) scope: MemoryScope,
    pub(crate) max_tree_lines: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BuiltinCommand {
    Model(ModelCommand),
    Login(String),
    McpAuth(String),
    Mcp,
    Tasks,
    Workflow,
    Switch(String),
    NewSession,
    Reload,
    ResumePicker(String),
    RewindPicker,
    MemoryCheck(MemoryCheckOptions),
    MemoryRefresh,
    MemoryReview(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandInvocation {
    Builtin { command: BuiltinCommand },
    Extension { name: String, args: String },
}

pub(crate) struct CommandRegistry<'a> {
    extension_commands: &'a [RegisteredCommand],
}

impl<'a> CommandRegistry<'a> {
    pub(crate) fn new(extension_commands: &'a [RegisteredCommand]) -> Self {
        Self { extension_commands }
    }

    pub(crate) fn resolve(&self, input: &str) -> anyhow::Result<Option<CommandInvocation>> {
        let Some((name, args)) = slash_command_parts(input) else {
            return Ok(None);
        };
        if let Some(spec) = BUILTIN_COMMANDS.iter().find(|spec| spec.name == name) {
            return spec
                .parse(args.trim())
                .map(|command| command.map(|command| CommandInvocation::Builtin { command }));
        }
        self.resolve_extension(name, args)
    }

    pub(crate) fn declarations(
        &self,
        resources: Vec<SlashCommand>,
        settings: &Settings,
    ) -> Vec<SlashCommand> {
        let mut commands = BUILTIN_COMMANDS
            .iter()
            .filter(|spec| spec.is_enabled(settings))
            .map(|spec| spec.declaration(settings))
            .collect::<Vec<_>>();
        commands.extend(resources);
        commands.extend(self.extension_commands.iter().map(|command| {
            let description = command.description.as_deref().unwrap_or(&command.source);
            let mut entry = if command.source.starts_with("builtin:") {
                SlashCommand::builtin(&command.name, description)
            } else {
                SlashCommand::extension(&command.name, description)
            };
            entry.arguments = command.arguments.clone();
            entry
        }));
        commands
    }

    pub(crate) fn retain_supported(
        &self,
        commands: &mut Vec<SlashCommand>,
        supports: impl Fn(&CommandInvocation) -> bool,
    ) {
        commands.retain_mut(|command| {
            if let Some(supported) = retain_builtin(command, &|command| {
                supports(&CommandInvocation::Builtin { command })
            }) {
                return supported;
            }
            self.resolve_extension(&command.name, "")
                .ok()
                .flatten()
                .is_none_or(|invocation| supports(&invocation))
        });
    }

    fn resolve_extension(
        &self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<Option<CommandInvocation>> {
        if name.starts_with("skill:") {
            return Ok(None);
        }
        let Some(declaration) = self
            .extension_commands
            .iter()
            .find(|command| command.name == name)
        else {
            return Ok(None);
        };
        if declaration.arguments.is_none() && !args.trim().is_empty() {
            anyhow::bail!("Usage: /{name}");
        }
        let command = match &declaration.target {
            CommandTarget::Handler => {
                return Ok(Some(CommandInvocation::Extension {
                    name: name.into(),
                    args: args.into(),
                }));
            }
            CommandTarget::Prompt => return Ok(None),
            CommandTarget::Application { action }
                if declaration.source == phi_sdk::extension::BuiltinExtension::Memory.path() =>
            {
                let Some(command) = memory_command(action, args) else {
                    return Ok(None);
                };
                command
            }
            CommandTarget::Application { .. } => return Ok(None),
        };
        Ok(Some(CommandInvocation::Builtin { command }))
    }
}

pub(crate) fn slash_command_parts(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    Some(rest.split_once(char::is_whitespace).unwrap_or((rest, "")))
}

pub(crate) fn is_builtin_command(name: &str) -> bool {
    BUILTIN_COMMANDS.iter().any(|spec| spec.name == name)
        || phi_sdk::extension::builtin_command_names().contains(&name)
}

const MODEL_UPDATE: &str = "update";
const WORKFLOW: &str = "workflow";
const LOGIN_PROVIDERS: &[(&str, Option<&str>)] =
    &[("openai-codex", Some("codex")), ("openai", Some("chatgpt"))];

struct CommandSpec {
    name: &'static str,
    description: &'static str,
    syntax: CommandSyntax,
    enabled: CommandEnabled,
}

enum CommandSyntax {
    NoArguments(fn() -> BuiltinCommand),
    Arguments {
        usage: &'static str,
        values: fn(&Settings) -> Vec<CommandArgument>,
        parse: fn(&str) -> Result<Option<BuiltinCommand>, CommandError>,
    },
}

enum CommandEnabled {
    Always,
    Mcp,
    Workflow,
}

const BUILTIN_COMMANDS: &[CommandSpec] = &[
    command(
        "new",
        "start a new session",
        CommandSyntax::NoArguments(|| BuiltinCommand::NewSession),
    ),
    command(
        "switch",
        "choose the active switch",
        CommandSyntax::Arguments {
            usage: "<name|alias>",
            values: switch_arguments,
            parse: |args| Ok(Some(BuiltinCommand::Switch(args.into()))),
        },
    ),
    command(
        "resume",
        "resume or delete a saved session",
        CommandSyntax::Arguments {
            usage: "<session query>",
            values: no_values,
            parse: |args| Ok(Some(BuiltinCommand::ResumePicker(args.into()))),
        },
    ),
    command(
        "rewind",
        "return to an earlier point in this session",
        CommandSyntax::NoArguments(|| BuiltinCommand::RewindPicker),
    ),
    command(
        "reload",
        "reload settings, extensions, prompts, and context",
        CommandSyntax::NoArguments(|| BuiltinCommand::Reload),
    ),
    command(
        "model",
        "choose or update the active model",
        CommandSyntax::Arguments {
            usage: "[<provider/model> | update]",
            values: |_| vec![argument(MODEL_UPDATE, None)],
            parse: parse_model,
        },
    ),
    command(
        "login",
        "authenticate a subscription provider",
        CommandSyntax::Arguments {
            usage: "<provider>",
            values: |_| {
                LOGIN_PROVIDERS
                    .iter()
                    .map(|(value, alias)| argument(value, *alias))
                    .collect()
            },
            parse: parse_login,
        },
    ),
    CommandSpec {
        enabled: CommandEnabled::Mcp,
        ..command(
            "mcp",
            "open MCP server status and refresh panel",
            CommandSyntax::NoArguments(|| BuiltinCommand::Mcp),
        )
    },
    command(
        "tasks",
        "open the work checklist",
        CommandSyntax::NoArguments(|| BuiltinCommand::Tasks),
    ),
    CommandSpec {
        enabled: CommandEnabled::Workflow,
        ..command(
            WORKFLOW,
            "request Workflow for a complex, parallel or multistage task",
            CommandSyntax::Arguments {
                usage: "<task>",
                values: no_values,
                parse: |args| Ok(args.is_empty().then_some(BuiltinCommand::Workflow)),
            },
        )
    },
    CommandSpec {
        enabled: CommandEnabled::Mcp,
        ..command(
            "mcp-auth",
            "show OAuth token setup instructions for an MCP server",
            CommandSyntax::Arguments {
                usage: "<server-name>",
                values: mcp_servers,
                parse: |args| Ok(Some(BuiltinCommand::McpAuth(args.into()))),
            },
        )
    },
];

const fn command(
    name: &'static str,
    description: &'static str,
    syntax: CommandSyntax,
) -> CommandSpec {
    CommandSpec {
        name,
        description,
        syntax,
        enabled: CommandEnabled::Always,
    }
}

impl CommandSpec {
    fn parse(&self, args: &str) -> Result<Option<BuiltinCommand>, CommandError> {
        match self.syntax {
            CommandSyntax::NoArguments(build) => {
                if !args.is_empty() {
                    return Err(CommandError::msg(format!("Usage: /{}", self.name)));
                }
                Ok(Some(build()))
            }
            CommandSyntax::Arguments { parse, .. } => parse(args),
        }
    }

    fn declaration(&self, settings: &Settings) -> SlashCommand {
        let mut command = SlashCommand::builtin(self.name, self.description);
        if let CommandSyntax::Arguments { usage, values, .. } = self.syntax {
            command.arguments = Some(CommandArguments {
                usage: usage.into(),
                values: values(settings),
            });
        }
        command
    }

    fn is_enabled(&self, settings: &Settings) -> bool {
        match self.enabled {
            CommandEnabled::Always => true,
            CommandEnabled::Mcp => phi_sdk::config::RuntimeConfig::from(settings)
                .extensions
                .is_enabled(phi_sdk::extension::BuiltinExtension::Mcp),
            CommandEnabled::Workflow => settings.agent.workflow.unwrap_or(false),
        }
    }
}

fn retain_builtin(
    command: &mut SlashCommand,
    supports: &impl Fn(BuiltinCommand) -> bool,
) -> Option<bool> {
    let spec = BUILTIN_COMMANDS
        .iter()
        .find(|spec| spec.name == command.name)?;
    let supported = spec.parse("").ok().flatten().is_some_and(supports);
    if supported && let Some(arguments) = &mut command.arguments {
        let had_values = !arguments.values.is_empty();
        arguments.values.retain(|argument| {
            spec.parse(&argument.value)
                .ok()
                .flatten()
                .is_none_or(supports)
        });
        if had_values && arguments.values.is_empty() {
            command.arguments = None;
        }
    }
    Some(supported)
}

pub(crate) fn workflow_prompt(input: &str) -> Option<&str> {
    let (name, args) = slash_command_parts(input)?;
    (name == WORKFLOW).then_some(args.trim())
}

fn memory_command(action: &str, args: &str) -> Option<BuiltinCommand> {
    const DEFAULT_TREE_LINES: usize = 25;
    const DEFAULT_REVIEW_LIMIT: usize = 50;
    const MIN_REVIEW_LIMIT: usize = 1;
    const MAX_REVIEW_LIMIT: usize = 100;
    Some(match action {
        "memory-check" => {
            let tokens = args.split_whitespace().collect::<Vec<_>>();
            BuiltinCommand::MemoryCheck(MemoryCheckOptions {
                scope: if tokens
                    .iter()
                    .any(|token| matches!(*token, "-g" | "--global" | "global"))
                {
                    MemoryScope::Global
                } else {
                    MemoryScope::Project
                },
                max_tree_lines: tokens
                    .iter()
                    .filter_map(|token| token.parse::<usize>().ok())
                    .find(|value| *value > 0)
                    .unwrap_or(DEFAULT_TREE_LINES),
            })
        }
        "memory-refresh" => BuiltinCommand::MemoryRefresh,
        "memory-review" => BuiltinCommand::MemoryReview(
            args.trim()
                .parse::<usize>()
                .unwrap_or(DEFAULT_REVIEW_LIMIT)
                .clamp(MIN_REVIEW_LIMIT, MAX_REVIEW_LIMIT),
        ),
        _ => return None,
    })
}

fn parse_model(args: &str) -> Result<Option<BuiltinCommand>, CommandError> {
    let command = match args {
        "" => ModelCommand::Select,
        MODEL_UPDATE => ModelCommand::Update,
        argument => match argument.split_once('/') {
            Some((provider, model))
                if !provider.is_empty()
                    && !model.is_empty()
                    && !argument.contains(char::is_whitespace) =>
            {
                ModelCommand::Set(format!("model:{provider}:{model}"))
            }
            _ => {
                return Err(CommandError::msg(format!(
                    "unknown model command: {argument}"
                )));
            }
        },
    };
    Ok(Some(BuiltinCommand::Model(command)))
}

fn parse_login(args: &str) -> Result<Option<BuiltinCommand>, CommandError> {
    let provider = LOGIN_PROVIDERS
        .iter()
        .find(|(_, alias)| *alias == Some(args))
        .map(|(value, _)| *value)
        .unwrap_or(args);
    Ok(Some(BuiltinCommand::Login(provider.into())))
}

fn argument(value: &str, alias: Option<&str>) -> CommandArgument {
    CommandArgument {
        value: value.into(),
        alias: alias.map(String::from),
    }
}

fn no_values(_: &Settings) -> Vec<CommandArgument> {
    Vec::new()
}

fn switch_arguments(settings: &Settings) -> Vec<CommandArgument> {
    settings
        .switch_list()
        .into_iter()
        .map(|switch| CommandArgument {
            value: switch.name,
            alias: switch.alias,
        })
        .collect()
}

fn mcp_servers(settings: &Settings) -> Vec<CommandArgument> {
    let mut names = settings
        .mcp_servers
        .as_ref()
        .map(|servers| servers.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .map(|value| CommandArgument { value, alias: None })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_follow_loaded_settings_and_extension_arity() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "switches": [{"name": "coding", "alias": "c"}],
            "mcpServers": {"zeta": {"command": "zeta"}, "alpha": {"command": "alpha"}}
        }))
        .unwrap();
        let extensions = [RegisteredCommand {
            name: "no-args".into(),
            description: None,
            source: "test.lua".into(),
            target: CommandTarget::Handler,
            arguments: None,
        }];
        let registry = CommandRegistry::new(&extensions);
        let commands = registry.declarations(
            vec![SlashCommand {
                name: "review".into(),
                description: "review code".into(),
                source: SlashCommandSource::User,
                arguments: Some(Default::default()),
            }],
            &settings,
        );
        let switches = commands
            .iter()
            .find(|command| command.name == "switch")
            .unwrap()
            .arguments
            .as_ref()
            .unwrap();
        assert_eq!(
            switches
                .values
                .iter()
                .map(|argument| argument.value.as_str())
                .collect::<Vec<_>>(),
            ["default", "plan", "coding"]
        );
        assert_eq!(switches.values[2].alias.as_deref(), Some("c"));
        let auth = commands
            .iter()
            .find(|command| command.name == "mcp-auth")
            .unwrap()
            .arguments
            .as_ref()
            .unwrap();
        assert_eq!(
            auth.values
                .iter()
                .map(|argument| argument.value.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        for name in ["new", "no-args"] {
            assert!(
                commands
                    .iter()
                    .find(|command| command.name == name)
                    .unwrap()
                    .arguments
                    .is_none()
            );
        }
        assert!(commands.iter().any(|command| command.name == "review"));
        assert!(registry.resolve("/review").unwrap().is_none());
    }

    #[test]
    fn disabled_commands_remain_reserved() {
        let settings: Settings = serde_json::from_value(
            serde_json::json!({"extensions":["-builtin:mcp","-builtin:memory"]}),
        )
        .unwrap();
        let commands = CommandRegistry::new(&[]).declarations(Vec::new(), &settings);
        assert!(commands.iter().any(|command| command.name == "reload"));
        assert!(
            !commands.iter().any(
                |command| command.name.starts_with("mcp") || command.name.starts_with("memory")
            )
        );
        for name in [
            "model",
            "mcp",
            "workflow",
            "memory-check",
            "memory-refresh",
            "memory-review",
            "memory-inject",
        ] {
            assert!(is_builtin_command(name));
        }
        for name in [
            "memory-check",
            "memory-refresh",
            "memory-review",
            "memory-inject",
        ] {
            assert!(
                CommandRegistry::new(&[])
                    .resolve(&format!("/{name}"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn workflow_requires_settings_and_preserves_prompt_route() {
        for configured in [None, Some(false), Some(true)] {
            let mut settings = Settings::default();
            settings.agent.workflow = configured;
            assert_eq!(
                CommandRegistry::new(&[])
                    .declarations(Vec::new(), &settings)
                    .iter()
                    .any(|command| command.name == "workflow"),
                configured == Some(true)
            );
        }
        assert_eq!(workflow_prompt("/workflow"), Some(""));
        assert_eq!(
            workflow_prompt("/workflow\tinspect code\n"),
            Some("inspect code")
        );
        assert_eq!(workflow_prompt("/workflow-extra inspect"), None);
        assert_eq!(workflow_prompt("inspect code"), None);
        let extensions = [WORKFLOW, "skill:review"].map(|name| RegisteredCommand {
            name: name.into(),
            description: None,
            source: "test.lua".into(),
            target: CommandTarget::Handler,
            arguments: Some(Default::default()),
        });
        let registry = CommandRegistry::new(&extensions);
        assert_eq!(
            registry.resolve("/workflow").unwrap(),
            Some(CommandInvocation::Builtin {
                command: BuiltinCommand::Workflow,
            })
        );
        for input in [
            "/workflow\tinspect",
            "/skill:review",
            "/skill:review inspect",
        ] {
            assert!(registry.resolve(input).unwrap().is_none(), "{input}");
        }
    }

    #[test]
    fn model_selection_preserves_provider_and_nested_model_id() {
        let registry = CommandRegistry::new(&[]);
        for model in ["sonnet", "vendor/sonnet"] {
            assert_eq!(
                registry.resolve(&format!("/model local/{model}")).unwrap(),
                Some(CommandInvocation::Builtin {
                    command: BuiltinCommand::Model(ModelCommand::Set(format!(
                        "model:local:{model}"
                    )))
                })
            );
        }
        for input in [
            "/model /sonnet",
            "/model local/",
            "/model local/sonnet extra",
        ] {
            assert!(registry.resolve(input).is_err(), "{input}");
        }
    }

    #[test]
    fn builtin_syntax_rejects_invalid_arguments() {
        let registry = CommandRegistry::new(&[]);
        assert_eq!(
            registry.resolve("/model update").unwrap(),
            Some(CommandInvocation::Builtin {
                command: BuiltinCommand::Model(ModelCommand::Update)
            })
        );
        assert_eq!(
            registry
                .resolve("/model local:test")
                .unwrap_err()
                .to_string(),
            "unknown model command: local:test"
        );
        for input in [
            "/new extra",
            "/reload extra",
            "/rewind extra",
            "/tasks extra",
            "/mcp extra",
        ] {
            assert!(registry.resolve(input).is_err(), "{input}");
        }
        assert!(registry.resolve("/not-a-command extra").unwrap().is_none());
    }

    #[test]
    fn extension_dispatch_requires_matching_target_source_and_arity() {
        for source in [
            "test.lua",
            phi_sdk::extension::BuiltinExtension::Memory.path(),
        ] {
            for target in [
                CommandTarget::Handler,
                CommandTarget::Prompt,
                CommandTarget::Application {
                    action: "memory-refresh".into(),
                },
                CommandTarget::Application {
                    action: "unknown".into(),
                },
            ] {
                for arguments in [None, Some(CommandArguments::default())] {
                    let extensions = [RegisteredCommand {
                        name: "inspect".into(),
                        description: None,
                        source: source.into(),
                        target: target.clone(),
                        arguments: arguments.clone(),
                    }];
                    let registry = CommandRegistry::new(&extensions);
                    for args in ["", "payload"] {
                        let result = registry.resolve(&format!("/inspect {args}"));
                        if arguments.is_none() && !args.is_empty() {
                            assert!(result.is_err());
                            continue;
                        }
                        let expected = match &target {
                            CommandTarget::Handler => Some(CommandInvocation::Extension {
                                name: "inspect".into(),
                                args: args.into(),
                            }),
                            CommandTarget::Application { action }
                                if source
                                    == phi_sdk::extension::BuiltinExtension::Memory.path()
                                    && action == "memory-refresh" =>
                            {
                                Some(CommandInvocation::Builtin {
                                    command: BuiltinCommand::MemoryRefresh,
                                })
                            }
                            _ => None,
                        };
                        assert_eq!(result.unwrap(), expected, "{source} {target:?}");
                    }
                }
            }
        }
        for input in ["/web", "/inspect", "/skill:review", "inspect payload"] {
            assert!(CommandRegistry::new(&[]).resolve(input).unwrap().is_none());
        }
    }

    #[test]
    fn memory_arguments_are_parsed_before_dispatch() {
        let extensions = ["memory-check", "memory-review"].map(|name| RegisteredCommand {
            name: name.into(),
            description: None,
            source: "builtin:memory".into(),
            target: CommandTarget::Application {
                action: name.into(),
            },
            arguments: Some(Default::default()),
        });
        let registry = CommandRegistry::new(&extensions);
        for (args, scope, lines) in [
            ("", MemoryScope::Project, 25),
            ("--global 7", MemoryScope::Global, 7),
            ("-g 0 2", MemoryScope::Global, 2),
            ("global invalid", MemoryScope::Global, 25),
        ] {
            assert_eq!(
                registry.resolve(&format!("/memory-check {args}")).unwrap(),
                Some(CommandInvocation::Builtin {
                    command: BuiltinCommand::MemoryCheck(MemoryCheckOptions {
                        scope,
                        max_tree_lines: lines
                    })
                })
            );
        }
        for (args, limit) in [
            ("", 50),
            ("invalid", 50),
            ("0", 1),
            ("101", 100),
            ("42", 42),
        ] {
            assert_eq!(
                registry.resolve(&format!("/memory-review {args}")).unwrap(),
                Some(CommandInvocation::Builtin {
                    command: BuiltinCommand::MemoryReview(limit)
                })
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]
        #[test]
        fn declared_candidates_and_aliases_resolve_with_slash_whitespace(
            separator in "[ \\t\\n\\r\\u{2003}]{1,6}",
            candidate in 0usize..2,
            use_alias in proptest::bool::ANY,
        ) {
            let registry = CommandRegistry::new(&[]);
            let commands = registry.declarations(Vec::new(), &Settings::default());
            let login = commands.iter().find(|command| command.name == "login").unwrap().arguments.as_ref().unwrap();
            let argument = &login.values[candidate];
            let value = if use_alias { argument.alias.as_deref().unwrap() } else { &argument.value };
            let resolved = registry.resolve(&format!("/login{separator}{value}{separator}")).unwrap();
            proptest::prop_assert_eq!(resolved, Some(CommandInvocation::Builtin { command: BuiltinCommand::Login(argument.value.clone()) }));
            let extensions = [RegisteredCommand {
                name: "inspect".into(), description: None, source: "test.lua".into(),
                target: CommandTarget::Handler, arguments: Some(Default::default()),
            }];
            let input = format!("/inspect{separator}files  \nnext");
            let Some(CommandInvocation::Extension { name, args }) = CommandRegistry::new(&extensions).resolve(&input).unwrap() else {
                proptest::prop_assert!(false, "registered handler must be dispatched");
                return Ok(());
            };
            proptest::prop_assert_eq!(name, "inspect");
            proptest::prop_assert_eq!(args.trim_start(), "files  \nnext");
            proptest::prop_assert!(CommandRegistry::new(&extensions).resolve("/inspect-extra files").unwrap().is_none());
        }
    }
}
