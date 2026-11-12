use std::collections::{HashMap, HashSet};

use phi_sdk::tools::{ToolRule, ToolRuleError, ToolRules};

type ToolRuleValidator<'a> = dyn Fn(&ToolRule) -> Result<(), ToolRuleError> + 'a;
use serde::Deserializer;
use serde::de::{Error, MapAccess, Visitor};
use std::path::Path;

pub type PromptDiagnostic = crate::resources::ResourceDiagnostic;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CustomCommandBlock<'a> {
    pub(crate) name: String,
    pub(crate) content: &'a str,
}

pub(crate) fn parse_custom_command_block(text: &str) -> Option<CustomCommandBlock<'_>> {
    let (header, body) = text.split_once('\n')?;
    let encoded_name = header.strip_prefix("<command name=")?.strip_suffix('>')?;
    let name = serde_json::from_str(encoded_name).ok()?;
    let content = body.strip_suffix("\n</command>")?;
    Some(CustomCommandBlock { name, content })
}

pub(super) fn expand_prompt_command<'a>(
    input: &str,
    prompts: &'a HashMap<String, PromptTemplate>,
) -> Option<(String, &'a PromptTemplate)> {
    let (name, args) = crate::commands::slash_command_parts(input)?;
    let content = prompts.get(name)?;
    let expanded = expand_command(args, content);
    Some((
        format!(
            "<command name={}>\n{}\n</command>",
            serde_json::to_string(name).ok()?,
            expanded
        ),
        content,
    ))
}

#[derive(Debug, Clone, Default)]
pub struct LoadPromptsResult {
    pub prompts: HashMap<String, PromptTemplate>,
    pub diagnostics: Vec<PromptDiagnostic>,
    origins: HashMap<String, (std::path::PathBuf, crate::resources::Scope)>,
    sources: Vec<PromptSource>,
}

#[derive(Debug, Clone)]
struct PromptSource {
    name: String,
    path: std::path::PathBuf,
    scope: crate::resources::Scope,
    content: Result<std::sync::Arc<str>, String>,
}

impl LoadPromptsResult {
    pub(super) fn revalidated(&self, validate: &ToolRuleValidator<'_>) -> Self {
        let mut result = Self::default();
        for source in &self.sources {
            insert_source(source, &mut result, validate);
        }
        result.sources = self.sources.clone();
        result
    }
}

#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub body: String,
    pub description: String,
    pub allowed_tools: Vec<(String, String)>,
    pub disallowed_tools: Vec<String>,
}

impl PromptTemplate {
    #[cfg(test)]
    pub fn parse(content: &str) -> Result<Self, String> {
        Self::parse_with_validation(content, &|_| Ok(()))
    }

    fn parse_with_validation(
        content: &str,
        validate: &ToolRuleValidator<'_>,
    ) -> Result<Self, String> {
        let (header, body) = super::parse_frontmatter(content, PromptHeaderVisitor { validate })?;
        Ok(Self {
            description: header
                .description
                .unwrap_or_else(|| prompt_description(body)),
            body: body.to_string(),
            allowed_tools: header.allowed_tools.into_allowances(),
            disallowed_tools: header.disallowed_tools.into_names(),
        })
    }
}

#[derive(Default)]
struct PromptHeader {
    description: Option<String>,
    allowed_tools: ToolRules,
    disallowed_tools: ToolRules,
}

struct PromptToolRules<'a>(&'a ToolRuleValidator<'a>);

impl<'de> serde::de::DeserializeSeed<'de> for PromptToolRules<'_> {
    type Value = ToolRules;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<ToolRules, D::Error> {
        ToolRules::deserialize_with_validation(deserializer, self.0)
    }
}

struct PromptField<'a> {
    seen: &'a mut HashSet<String>,
}

impl<'de> serde::de::DeserializeSeed<'de> for PromptField<'_> {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_string(self)
    }
}

impl Visitor<'_> for PromptField<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a Prompt metadata field")
    }

    fn visit_str<E: Error>(self, field: &str) -> Result<Self::Value, E> {
        match field {
            "tools" | "disabled-tools" | "allowedTools" | "disallowedTools" | "disabledTools"
            | "allowed_tools" | "disallowed_tools" | "disabled_tools" => Err(E::custom(format!(
                "unsupported Prompt field `{field}`; use allowed-tools or disallowed-tools"
            ))),
            _ if !self.seen.insert(field.to_string()) => {
                Err(E::custom(format!("duplicate field `{field}`")))
            }
            _ => Ok(field.to_string()),
        }
    }
}

struct PromptHeaderVisitor<'a> {
    validate: &'a ToolRuleValidator<'a>,
}

impl<'de> serde::de::DeserializeSeed<'de> for PromptHeaderVisitor<'_> {
    type Value = PromptHeader;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<PromptHeader, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for PromptHeaderVisitor<'_> {
    type Value = PromptHeader;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Prompt metadata with allowed-tools and disallowed-tools declarations")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut header = PromptHeader::default();
        let mut fields = HashSet::new();
        while let Some(field) = map.next_key_seed(PromptField { seen: &mut fields })? {
            match field.as_str() {
                "description" => header.description = Some(map.next_value()?),
                "allowed-tools" | "disallowed-tools" => {
                    let tools = map.next_value_seed(PromptToolRules(self.validate))?;
                    if field == "allowed-tools" {
                        header.allowed_tools = tools;
                    } else {
                        header.disallowed_tools = tools;
                    }
                }
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(header)
    }
}

pub(crate) fn load_with_validation(
    policy: &crate::resources::ResourcePolicy,
    validate: &ToolRuleValidator<'_>,
) -> LoadPromptsResult {
    let mut result = LoadPromptsResult::default();
    for (resource, root) in policy.selected(crate::resources::ResourceKind::Prompts) {
        load_file(&root, &resource.path, &mut result, resource.scope, validate);
    }
    result
}

pub fn expand_command(args: &str, template: &PromptTemplate) -> String {
    let body = &template.body;
    if body.contains("$ARGUMENTS") {
        return body.replace("$ARGUMENTS", args.trim());
    }
    if args.trim().is_empty() {
        body.clone()
    } else {
        format!("{}\n\n{}", body.trim_end(), args.trim())
    }
}

fn load_file(
    root: &Path,
    path: &Path,
    result: &mut LoadPromptsResult,
    scope: crate::resources::Scope,
    validate: &ToolRuleValidator<'_>,
) {
    let Some(name) = command_name(root, path) else {
        return;
    };
    let content = if crate::commands::is_builtin_command(&name) {
        Err(format!(
            "prompt command /{name} conflicts with a built-in command; skipped"
        ))
    } else {
        std::fs::read_to_string(path)
            .map(std::sync::Arc::from)
            .map_err(|error| format!("cannot read Prompt: {error}"))
    };
    let source = PromptSource {
        name,
        path: path.to_path_buf(),
        scope,
        content,
    };
    insert_source(&source, result, validate);
    result.sources.push(source);
}

fn insert_source(
    source: &PromptSource,
    result: &mut LoadPromptsResult,
    validate: &ToolRuleValidator<'_>,
) {
    let PromptSource {
        name,
        path,
        scope,
        content,
    } = source;
    let template = content
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|content| PromptTemplate::parse_with_validation(content, validate));
    match template {
        Err(message) => result.diagnostics.push(PromptDiagnostic {
            resource: crate::resources::DiagnosticResource::Prompt,
            path: path.to_path_buf(),
            message,
        }),
        Ok(template) => {
            if let Some((existing, existing_scope)) = result.origins.get(name) {
                if existing == path {
                    return;
                }
                let priority = if *existing_scope == crate::resources::Scope::Project
                    && *scope == crate::resources::Scope::Global
                {
                    "project prompt takes priority over global prompt"
                } else {
                    "earlier prompt takes priority"
                };
                result.diagnostics.push(PromptDiagnostic {
                    resource: crate::resources::DiagnosticResource::Prompt,
                    path: path.to_path_buf(),
                    message: format!(
                        "duplicate prompt `/{name}` from file `{}` ignored; {priority}: `{}`",
                        path.display(),
                        existing.display()
                    ),
                });
                return;
            }
            result
                .origins
                .insert(name.clone(), (path.to_path_buf(), *scope));
            result.prompts.insert(name.clone(), template);
        }
    }
}

fn command_name(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?.with_extension("");
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn prompt_description(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("custom prompt")
        .chars()
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_policy(project: &Path, global: Option<&Path>) -> crate::resources::ResourcePolicy {
        use crate::resources::{ResourcePolicy, ResourceSettings};
        ResourcePolicy::from_settings(
            project,
            ResourceSettings {
                prompts: global
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
                ..Default::default()
            },
            ResourceSettings {
                prompts: vec![project.to_string_lossy().into_owned()],
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn invalid_declarations_are_reported_without_discarding_valid_prompts() {
        let root = Path::new("/tmp").join(format!("phi-prompts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("valid.md"), "Review $ARGUMENTS").unwrap();
        for (index, field, value) in [
            (0, "allowed-tools", "[Read, 42]"),
            (1, "allowed-tools", "['Bash()']"),
            (2, "disallowed-tools", "[MCP]"),
            (3, "tools", "[Read]"),
            (4, "disabled-tools", "[Read]"),
            (5, "allowedTools", "[Read]"),
            (6, "allowed-tools", "\r\n  Bash: false"),
        ] {
            let path = root.join(format!("bad-{index}.md"));
            std::fs::write(
                &path,
                format!("---\r\ndescription: Review\r\n{field}: {value}\r\n---\r\nbody"),
            )
            .unwrap();
            let result = load_with_validation(&prompt_policy(&root, None), &|_| Ok(()));
            assert!(result.prompts.contains_key("valid"));
            assert!(!result.prompts.contains_key(&format!("bad-{index}")));
            let diagnostic = result
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.path == path)
                .unwrap();
            assert_eq!(
                diagnostic.resource,
                crate::resources::DiagnosticResource::Prompt
            );
            assert!(diagnostic.message.contains(field), "{}", diagnostic.message);
            assert_eq!(
                diagnostic.message.matches(" at line ").count(),
                1,
                "{}",
                diagnostic.message
            );
            assert!(
                !diagnostic.message.contains(&format!("{field}: {field}:")),
                "{}",
                diagnostic.message
            );
            assert!(
                diagnostic.message.contains(if value.starts_with('\r') {
                    "line 4"
                } else {
                    "line 3"
                }),
                "{}",
                diagnostic.message
            );
            assert!(
                diagnostic.message.contains("column"),
                "{}",
                diagnostic.message
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frozen_prompt_revalidation_preserves_priority_and_source_diagnostics() {
        let root = Path::new("/tmp").join(format!("phi-prompts-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let global = root.join("global");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        let path = project.join("review.md");
        std::fs::write(
            &path,
            "---\r\nallowed-tools: [repo_search]\r\n---\r\nproject",
        )
        .unwrap();
        std::fs::write(global.join("review.md"), "global").unwrap();
        let loaded = load_with_validation(&prompt_policy(&project, Some(&global)), &|_| Ok(()));
        assert_eq!(loaded.prompts["review"].body, "project");
        assert_eq!(loaded.diagnostics.len(), 1);
        assert!(
            loaded.diagnostics[0]
                .message
                .contains("project prompt takes priority")
        );
        std::fs::remove_dir_all(&root).unwrap();
        let validated = loaded.revalidated(&|rule| {
            if rule.tool == "repo_search" {
                return Err(ToolRuleError::UnsupportedMcp);
            }
            Ok(())
        });
        assert_eq!(validated.prompts["review"].body, "global");
        assert_eq!(validated.diagnostics.len(), 1);
        assert_eq!(validated.diagnostics[0].path, path);
        assert!(validated.diagnostics[0].message.contains("allowed-tools"));
        assert!(validated.diagnostics[0].message.contains("line 2"));
        let restored = validated.revalidated(&|_| Ok(()));
        assert_eq!(restored.prompts["review"].body, "project");
        assert_eq!(restored.diagnostics.len(), 1);
        assert!(
            restored.diagnostics[0]
                .message
                .contains("project prompt takes priority")
        );
    }

    #[test]
    fn generation_validation_preserves_field_and_item_locations() {
        let validate = |rule: &ToolRule| {
            if rule.tool == "search" {
                return Err(ToolRuleError::UnsupportedMcp);
            }
            Ok(())
        };
        for field in ["allowed-tools", "disallowed-tools"] {
            for declaration in ["[Read, 'search(query *)']", "Read, search(query *)"] {
                let content =
                    format!("---\r\ndescription: Review\r\n{field}: {declaration}\r\n---\r\nbody");
                let error = PromptTemplate::parse_with_validation(&content, &validate).unwrap_err();
                assert!(error.contains(field), "{error}");
                assert!(error.contains("MCP settings"), "{error}");
                assert!(error.contains("line 3"), "{error}");
                assert!(error.contains("column"), "{error}");
            }
        }
        let error = PromptTemplate::parse_with_validation(
            "---\nallowed-tools:\n  - Read\n  - search\n---\nbody",
            &validate,
        )
        .unwrap_err();
        assert!(error.contains("line 4"), "{error}");
        assert!(
            PromptTemplate::parse_with_validation(
                "---\nallowed-tools: [Read, custom]\n---\nbody",
                &validate,
            )
            .is_ok()
        );
    }

    #[test]
    fn templates_retain_validated_declarations_without_reparsing_at_expansion() {
        for value in [
            "[Read, 'Bash(echo a,b)', read]",
            "Read, Bash(echo a,b), read",
        ] {
            let content = format!(
                "---\r\ndescription: Review\r\nargument-hint: files\r\nmetadata: {{custom: true}}\r\nallowed-tools: {value}\r\ndisallowed-tools: [Edit, 'Bash(git push *)']\r\n---\r\nReview $ARGUMENTS\r\n"
            );
            let template = PromptTemplate::parse(&content).unwrap();
            assert_eq!(template.description, "Review");
            assert_eq!(
                template.allowed_tools,
                [
                    ("Bash".into(), "echo a,b".into()),
                    ("Read".into(), "**".into())
                ]
            );
            assert_eq!(template.disallowed_tools, ["Bash", "Edit"]);
            assert_eq!(expand_command("files", &template), "Review files\r\n");
        }
        let template =
            PromptTemplate::parse("---\nallowed-tools: []\ndisallowed-tools: []\n---\nReview.")
                .unwrap();
        assert!(template.allowed_tools.is_empty());
        assert!(template.disallowed_tools.is_empty());
        let error = PromptTemplate::parse(
            "---\r\nallowed-tools: [Read]\r\nallowed-tools: [Bash]\r\n---\r\nbody",
        )
        .unwrap_err();
        assert!(error.contains("duplicate field"), "{error}");
        assert!(error.contains("allowed-tools"), "{error}");
        assert!(error.contains("line 3"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn prompt_loading_skips_cycles_but_keeps_directory_aliases() {
        let root = std::env::temp_dir().join(format!("phi-prompt-cycles-{}", uuid::Uuid::new_v4()));
        let prompts = root.join("prompts");
        let shared = root.join("shared");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(prompts.join("local.md"), "local").unwrap();
        std::fs::write(shared.join("shared.md"), "shared").unwrap();
        std::os::unix::fs::symlink(&prompts, prompts.join("cycle")).unwrap();
        for alias in ["first", "second"] {
            std::os::unix::fs::symlink(&shared, prompts.join(alias)).unwrap();
        }
        let result = load_with_validation(&prompt_policy(&prompts, None), &|_| Ok(()));
        let mut names = result
            .prompts
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["first:shared", "local", "second:shared"]);
        assert!(result.diagnostics.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
