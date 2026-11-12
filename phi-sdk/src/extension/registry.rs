/// Where an adapter routes a command; declaration does not grant execution authority.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandTarget {
    #[default]
    Handler,
    Prompt,
    Application {
        action: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    #[serde(default)]
    pub target: CommandTarget,
    #[serde(default = "default_arguments")]
    pub arguments: Option<CommandArguments>,
}

fn default_arguments() -> Option<CommandArguments> {
    Some(CommandArguments::default())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandArguments {
    pub usage: String,
    #[serde(default)]
    pub values: Vec<CommandArgument>,
}

impl Default for CommandArguments {
    fn default() -> Self {
        Self::new("[arguments]", &[])
    }
}

impl CommandArguments {
    pub fn new(usage: &str, values: &[&str]) -> Self {
        Self {
            usage: usage.into(),
            values: values
                .iter()
                .map(|value| CommandArgument {
                    value: (*value).into(),
                    alias: None,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandArgument {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtensionRegistry {
    commands: Vec<RegisteredCommand>,
    skills: Vec<super::BuiltinSkill>,
}

impl ExtensionRegistry {
    pub fn commands(&self) -> &[RegisteredCommand] {
        &self.commands
    }

    pub fn extend_commands(&mut self, commands: Vec<RegisteredCommand>) {
        self.commands.extend(commands);
    }

    pub(super) fn set_builtin_declarations(
        &mut self,
        config: &crate::config::RuntimeConfig,
    ) -> anyhow::Result<()> {
        let reserved = super::builtin_command_names();
        for command in &self.commands {
            anyhow::ensure!(
                !reserved.contains(&command.name.as_str()),
                "extension command `{}` conflicts with a reserved builtin command",
                command.name
            );
        }
        self.commands
            .extend(super::builtin::adapter_commands(config));
        self.skills = super::builtin_skills(config);
        Ok(())
    }

    pub(super) fn skills(&self) -> &[super::BuiltinSkill] {
        &self.skills
    }
}
