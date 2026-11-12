//! Prompt files and Skills: load/validate resources, build Skill context and expand input.
//! Expansion returns content/tool-setting intent only; AgentInstance retains admitted submissions.
//! SDK owns invocation authorization and execution-policy lifetimes.

use crate::commands::{SlashCommand, SlashCommandSource};
use crate::resources::ResourceDiagnostic;

pub mod files;
pub mod skills;

pub(crate) struct PromptResources {
    skills: skills::LoadSkillsResult,
    prompts: files::LoadPromptsResult,
}

impl PromptResources {
    pub(crate) fn load_with_validation(
        policy: &crate::resources::ResourcePolicy,
        builtin_skills: &[phi_sdk::extension::BuiltinSkill],
        validate: &dyn Fn(&phi_sdk::tools::ToolRule) -> Result<(), phi_sdk::tools::ToolRuleError>,
    ) -> Self {
        Self {
            skills: skills::load_selected(policy, builtin_skills),
            prompts: files::load_with_validation(policy, validate),
        }
    }

    pub(crate) fn system_prompt_hook(&self) -> Option<phi_sdk::hooks::Hook> {
        skills::system_prompt_hook(&self.skills.skills)
    }

    pub(crate) fn revalidated(
        &self,
        validate: &dyn Fn(&phi_sdk::tools::ToolRule) -> Result<(), phi_sdk::tools::ToolRuleError>,
    ) -> Self {
        Self {
            skills: self.skills.clone(),
            prompts: self.prompts.revalidated(validate),
        }
    }

    pub(crate) fn diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.prompts
            .diagnostics
            .iter()
            .chain(&self.skills.diagnostics)
            .cloned()
            .collect()
    }

    pub(crate) fn commands(&self) -> Vec<SlashCommand> {
        let mut commands = self
            .prompts
            .prompts
            .iter()
            .map(|(name, template)| SlashCommand {
                name: name.clone(),
                description: template.description.clone(),
                source: SlashCommandSource::User,
                arguments: Some(Default::default()),
            })
            .collect::<Vec<_>>();
        commands.extend(self.skills.skills.iter().map(|skill| SlashCommand {
            name: format!("skill:{}", skill.name),
            description: skill.description.clone(),
            source: match skill.source {
                skills::SkillSource::Builtin => SlashCommandSource::Builtin,
                skills::SkillSource::User => SlashCommandSource::User,
                skills::SkillSource::Extension => SlashCommandSource::Extension,
            },
            arguments: Some(Default::default()),
        }));
        commands
    }

    pub(crate) fn expand(&self, display: &str) -> Result<PromptExpandResult, String> {
        expand_prompt(display, &self.skills.skills, &self.prompts.prompts)
    }
}

impl phi_sdk::context::PromptExpander for PromptResources {
    fn describe(
        &self,
        event: &phi_sdk::lifecycle::UserPromptSubmitEvent,
    ) -> Option<phi_sdk::lifecycle::UserPromptExpansionEvent> {
        use phi_sdk::lifecycle::UserPromptSubmitSource;
        if matches!(
            event.source,
            UserPromptSubmitSource::Internal | UserPromptSubmitSource::Extension
        ) || crate::commands::workflow_prompt(&event.prompt).is_some()
        {
            return None;
        }
        let (command, args) = crate::commands::slash_command_parts(&event.prompt)?;
        let (original_command, _) = crate::commands::slash_command_parts(&event.original_prompt)?;
        if command != original_command {
            return None;
        }
        let source = if let Some(name) = command.strip_prefix("skill:") {
            let skill = self.skills.skills.iter().find(|skill| skill.name == name)?;
            skill.path.display().to_string()
        } else {
            self.prompts.prompts.get(command)?;
            "prompt".into()
        };
        Some(phi_sdk::lifecycle::UserPromptExpansionEvent {
            prompt: event.prompt.clone(),
            command_name: command.into(),
            command_args: args.into(),
            command_source: source,
        })
    }

    fn expand(
        self: std::sync::Arc<Self>,
        event: phi_sdk::lifecycle::UserPromptExpansionEvent,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<phi_sdk::context::PromptExpansion, String>>
                + Send,
        >,
    > {
        Box::pin(async move {
            let result =
                tokio::task::spawn_blocking(move || PromptResources::expand(&self, &event.prompt))
                    .await
                    .map_err(|error| format!("prompt expansion task failed: {error}"))??;
            Ok(phi_sdk::context::PromptExpansion {
                prompt: result.expanded_prompt,
                tool_policy: result.tool_policy,
            })
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptExpandResult {
    pub expanded_prompt: Option<String>,
    pub tool_policy: phi_sdk::turn::TurnToolPolicy,
}

pub fn expand_prompt(
    display: &str,
    skills: &[skills::Skill],
    prompts: &std::collections::HashMap<String, files::PromptTemplate>,
) -> Result<PromptExpandResult, String> {
    let (expanded_prompt, template) = if let Some(skill) = skills::expand_command(display, skills)?
    {
        (Some(skill), None)
    } else {
        match files::expand_prompt_command(display, prompts) {
            Some((prompt, template)) => (Some(prompt), Some(template)),
            None => (None, None),
        }
    };
    Ok(PromptExpandResult {
        expanded_prompt,
        tool_policy: template
            .map(|template| phi_sdk::turn::TurnToolPolicy {
                allowances: template.allowed_tools.clone(),
                removals: template.disallowed_tools.clone(),
            })
            .unwrap_or_default(),
    })
}

fn parse_frontmatter<'de, T: Default>(
    content: &'de str,
    seed: impl serde::de::DeserializeSeed<'de, Value = T>,
) -> Result<(T, &'de str), String> {
    if !has_frontmatter(content) {
        return Ok((T::default(), content));
    }
    let mut offset = content.find('\n').unwrap() + 1;
    for line in content[offset..].split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let header = seed
                .deserialize(serde_yaml::Deserializer::from_str(&content[..offset]))
                .map_err(|error| format!("invalid YAML header: {error}"))?;
            return Ok((header, &content[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err("YAML header closing delimiter is missing at line 1 column 1".into())
}

fn has_frontmatter(content: &str) -> bool {
    content.starts_with("---\n") || content.starts_with("---\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[derive(Default, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Header {
        description: Option<String>,
    }

    #[test]
    fn preserves_markdown_locations_and_exact_delimiters() {
        let content = "---\r\ndescription: Review\r\n---\r\nbody\r\n";
        let (header, body) =
            parse_frontmatter(content, std::marker::PhantomData::<Header>).unwrap();
        assert_eq!(header.description.as_deref(), Some("Review"));
        assert_eq!(body, "body\r\n");
        let error = parse_frontmatter(
            "---\r\ndescription: Review\r\ntools: []\r\n---\r\nbody",
            std::marker::PhantomData::<Header>,
        )
        .err()
        .unwrap();
        assert!(error.contains("tools"), "{error}");
        assert!(error.contains("line 3"), "{error}");
        assert!(error.contains("column"), "{error}");
        assert!(
            parse_frontmatter(
                "---\ndescription: Review\n---not-a-delimiter",
                std::marker::PhantomData::<Header>,
            )
            .is_err()
        );
        let (_, body) =
            parse_frontmatter("ordinary Markdown", std::marker::PhantomData::<Header>).unwrap();
        assert_eq!(body, "ordinary Markdown");
    }

    #[test]
    fn skill_expansion_never_borrows_a_colliding_prompt_policy() {
        let path = Path::new("/tmp").join(format!("phi-skill-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&path, "---\nname: review\n---\nSkill context").unwrap();
        let skill = skills::Skill {
            name: "review".into(),
            description: "Review".into(),
            path: path.clone(),
            base_dir: Path::new("/tmp").into(),
            source: skills::SkillSource::User,
            disable_model_invocation: false,
        };
        let template = files::PromptTemplate::parse(
            "---\nallowed-tools: [Read]\ndisallowed-tools: [Edit]\n---\nPrompt $ARGUMENTS",
        )
        .unwrap();
        let prompts = std::collections::HashMap::from([
            ("inspect".into(), template.clone()),
            ("skill:review".into(), template),
        ]);
        for separator in [" ", "\t", "\n", "\r\n", "\u{2003}"] {
            let result =
                expand_prompt(&format!("/inspect{separator}files"), &[], &prompts).unwrap();
            let expanded = result.expanded_prompt.unwrap();
            let block = files::parse_custom_command_block(&expanded).unwrap();
            assert_eq!(block.name, "inspect");
            assert_eq!(block.content, "Prompt files");
            assert_eq!(
                result.tool_policy.allowances,
                [("Read".into(), "**".into())]
            );
            assert_eq!(result.tool_policy.removals, ["Edit"]);
            let result = expand_prompt(
                &format!("/skill:review{separator}files"),
                std::slice::from_ref(&skill),
                &prompts,
            )
            .unwrap();
            let expanded = result.expanded_prompt.unwrap();
            assert!(expanded.contains("Skill context"), "{separator:?}");
            assert!(expanded.ends_with("\n\nfiles"), "{separator:?}");
            assert_eq!(result.tool_policy, Default::default());
        }
        std::fs::remove_file(path).unwrap();
    }
}
