use crate::tools::{ToolRule, ToolRuleError, ToolRules, canonical_tool_name};
use ignore::WalkBuilder;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type ToolRuleValidator<'a> = dyn Fn(&ToolRule) -> Result<(), ToolRuleError> + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AgentValidationReason {
    #[error("invalid name")]
    InvalidName,
    #[error("description is empty")]
    EmptyDescription,
    #[error("prompt is empty")]
    EmptyPrompt,
    #[error("model is empty")]
    EmptyModel,
    #[error("max_turns must be greater than zero")]
    ZeroMaxTurns,
    #[error("allowed and denied tools overlap")]
    ConflictingToolPolicy,
    #[error("invalid Agent tool setting")]
    InvalidToolPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentDefinitionOrigin {
    Rust,
    File(PathBuf),
}

impl std::fmt::Display for AgentDefinitionOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rust => formatter.write_str("Rust declaration"),
            Self::File(path) => write!(formatter, "file `{}`", path.display()),
        }
    }
}

/// A reusable Agent role resolved when the `Phi` factory is built.
/// `name` selects the definition; instance aliases and Team leadership are separate.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    name: String,
    description: String,
    prompt: String,
    tools: Option<Vec<String>>,
    disallowed_tools: Vec<String>,
    model: Option<String>,
    max_turns: Option<usize>,
}

impl AgentDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            prompt: prompt.into(),
            tools: None,
            disallowed_tools: Vec::new(),
            model: None,
            max_turns: None,
        }
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = Some(clean_tools(tools.into_iter().map(Into::into).collect()));
        self
    }

    pub fn disallowed_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.disallowed_tools = clean_tools(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn tools_policy(&self) -> Option<&[String]> {
        self.tools.as_deref()
    }

    pub fn disallowed_tools_policy(&self) -> &[String] {
        &self.disallowed_tools
    }

    pub fn model_override(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn max_turns_limit(&self) -> Option<usize> {
        self.max_turns
    }

    pub(crate) fn validate(&self) -> Result<(), AgentValidationReason> {
        if !is_valid_name(&self.name) {
            return Err(AgentValidationReason::InvalidName);
        }
        if self.description.trim().is_empty() {
            return Err(AgentValidationReason::EmptyDescription);
        }
        if self.prompt.trim().is_empty() {
            return Err(AgentValidationReason::EmptyPrompt);
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(AgentValidationReason::EmptyModel);
        }
        if self.max_turns == Some(0) {
            return Err(AgentValidationReason::ZeroMaxTurns);
        }
        self.validate_tool_rules(&|_| Ok(()))
            .map_err(|_| AgentValidationReason::InvalidToolPolicy)?;
        if self.tools.as_ref().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| self.disallowed_tools.binary_search(tool).is_ok())
        }) {
            return Err(AgentValidationReason::ConflictingToolPolicy);
        }
        Ok(())
    }

    fn validate_tool_rules(&self, validate: &ToolRuleValidator<'_>) -> Result<(), ToolRuleError> {
        for name in self.tools.iter().flatten().chain(&self.disallowed_tools) {
            validate(&name.parse()?)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentDiagnosticKind {
    InvalidDefinition,
    DuplicateDefinition,
    ReadFailure,
    ParseFailure,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentDiagnostic {
    pub kind: AgentDiagnosticKind,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub(crate) enum AgentCatalogError {
    InvalidAgent {
        name: String,
        reason: AgentValidationReason,
    },
    DuplicateAgent {
        name: String,
        existing: AgentDefinitionOrigin,
        duplicate: AgentDefinitionOrigin,
    },
}

#[derive(Debug)]
struct AgentCandidate {
    definition: AgentDefinition,
    source: AgentSource,
}

#[derive(Debug, Clone)]
struct AgentSource {
    origin: AgentDefinitionOrigin,
    header: Option<Arc<str>>,
}

impl AgentSource {
    fn validate(&self, validate: &ToolRuleValidator<'_>) -> Result<(), AgentDiagnostic> {
        let (AgentDefinitionOrigin::File(path), Some(header)) = (&self.origin, &self.header) else {
            return Ok(());
        };
        serde::de::DeserializeSeed::deserialize(
            AgentFrontmatterVisitor { validate },
            serde_yaml::Deserializer::from_str(header),
        )
        .map(|_| ())
        .map_err(|error| AgentDiagnostic {
            kind: AgentDiagnosticKind::ParseFailure,
            path: path.clone(),
            message: error.to_string(),
        })
    }
}

/// The immutable resolved Agent definitions available during runtime preparation.
#[derive(Debug, Clone, Default)]
pub struct AgentCatalog {
    agents: Vec<AgentDefinition>,
    origins: HashMap<String, AgentSource>,
}

impl AgentCatalog {
    #[cfg(test)]
    pub(crate) fn new(agents: Vec<AgentDefinition>) -> Self {
        Self::from_candidates(
            agents
                .into_iter()
                .map(|definition| AgentCandidate {
                    definition,
                    source: AgentSource {
                        origin: AgentDefinitionOrigin::Rust,
                        header: None,
                    },
                })
                .collect(),
        )
    }

    fn from_candidates(mut candidates: Vec<AgentCandidate>) -> Self {
        candidates.sort_by(|left, right| left.definition.name.cmp(&right.definition.name));
        let origins = candidates
            .iter()
            .map(|candidate| (candidate.definition.name.clone(), candidate.source.clone()))
            .collect();
        let agents = candidates
            .into_iter()
            .map(|candidate| candidate.definition)
            .collect();
        Self { agents, origins }
    }

    pub(crate) fn validate_tool_declarations(
        &mut self,
        validate: &ToolRuleValidator<'_>,
    ) -> Result<Vec<AgentDiagnostic>, ToolRuleError> {
        for definition in &self.agents {
            if matches!(
                self.origins[definition.name()].origin,
                AgentDefinitionOrigin::Rust
            ) {
                definition.validate_tool_rules(validate)?;
            }
        }
        let mut diagnostics = Vec::new();
        self.agents.retain(|definition| {
            if let Err(diagnostic) = self.origins[definition.name()].validate(validate) {
                diagnostics.push(diagnostic);
                return false;
            }
            true
        });
        self.origins
            .retain(|name, _| self.agents.iter().any(|agent| agent.name() == name));
        Ok(diagnostics)
    }

    pub fn all(&self) -> &[AgentDefinition] {
        &self.agents
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    pub(crate) fn with_definitions(
        mut self,
        definitions: impl IntoIterator<Item = AgentDefinition>,
    ) -> Result<Self, AgentCatalogError> {
        for definition in definitions {
            self.insert(AgentCandidate {
                definition,
                source: AgentSource {
                    origin: AgentDefinitionOrigin::Rust,
                    header: None,
                },
            })?;
        }
        self.agents
            .sort_by(|left, right| left.name.cmp(&right.name));
        Ok(self)
    }

    fn insert(&mut self, candidate: AgentCandidate) -> Result<(), AgentCatalogError> {
        let name = candidate.definition.name.clone();
        candidate
            .definition
            .validate()
            .map_err(|reason| AgentCatalogError::InvalidAgent {
                name: name.clone(),
                reason,
            })?;
        if let Some(existing) = self.origins.get(&name) {
            return Err(AgentCatalogError::DuplicateAgent {
                name,
                existing: existing.origin.clone(),
                duplicate: candidate.source.origin,
            });
        }
        self.origins.insert(name, candidate.source);
        self.agents.push(candidate.definition);
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentLoadResult {
    pub catalog: AgentCatalog,
    pub diagnostics: Vec<AgentDiagnostic>,
}

#[derive(Debug, Default)]
struct AgentFrontmatter {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    max_turns: Option<usize>,
    tools: Option<Vec<String>>,
    disabled_tools: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for AgentFrontmatter {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(AgentFrontmatterVisitor {
            validate: &|_| Ok(()),
        })
    }
}

struct AgentFrontmatterVisitor<'a> {
    validate: &'a ToolRuleValidator<'a>,
}

impl<'de> serde::de::DeserializeSeed<'de> for AgentFrontmatterVisitor<'_> {
    type Value = AgentFrontmatter;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for AgentFrontmatterVisitor<'_> {
    type Value = AgentFrontmatter;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Agent declarations")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut header = AgentFrontmatter::default();
        let mut seen = HashSet::new();
        while let Some(field) = map.next_key_seed(AgentField { seen: &mut seen })? {
            match field.as_str() {
                "name" => header.name = map.next_value()?,
                "description" => header.description = map.next_value()?,
                "model" => header.model = map.next_value()?,
                "max_turns" => header.max_turns = map.next_value()?,
                "tools" | "disabled-tools" => {
                    let tools = map
                        .next_value_seed(AgentToolRules(self.validate))?
                        .into_names();
                    if field == "tools" {
                        header.tools = Some(tools);
                    } else {
                        header.disabled_tools = Some(tools);
                    }
                }
                _ => unreachable!("Agent field was validated before reading its value"),
            }
        }
        Ok(header)
    }
}

struct AgentField<'a> {
    seen: &'a mut HashSet<String>,
}

impl<'de> serde::de::DeserializeSeed<'de> for AgentField<'_> {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_string(self)
    }
}

impl serde::de::Visitor<'_> for AgentField<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an Agent metadata field")
    }

    fn visit_str<E: serde::de::Error>(self, field: &str) -> Result<String, E> {
        const FIELDS: &[&str] = &[
            "name",
            "description",
            "model",
            "max_turns",
            "tools",
            "disabled-tools",
        ];
        if !FIELDS.contains(&field) {
            return Err(E::unknown_field(field, FIELDS));
        }
        if !self.seen.insert(field.to_string()) {
            return Err(E::custom(format!("duplicate field `{field}`")));
        }
        Ok(field.to_string())
    }
}

struct AgentToolRules<'a>(&'a ToolRuleValidator<'a>);

impl<'de> serde::de::DeserializeSeed<'de> for AgentToolRules<'_> {
    type Value = ToolRules;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<ToolRules, D::Error> {
        ToolRules::deserialize_with_validation(deserializer, self.0)
    }
}

fn clean_tools(values: Vec<String>) -> Vec<String> {
    let mut tools = values
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .map(canonical_tool_name)
        .collect::<Vec<_>>();
    tools.sort();
    tools.dedup();
    tools
}

pub fn load() -> AgentLoadResult {
    load_from_roots(Vec::new())
}

pub fn load_from_roots(roots: Vec<PathBuf>) -> AgentLoadResult {
    let mut by_name = HashMap::<String, AgentCandidate>::new();
    let mut diagnostics = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        let mut paths = markdown_files(&root);
        paths.sort();
        for path in paths {
            load_agent_candidate(&path, &mut by_name, &mut diagnostics);
        }
    }

    AgentLoadResult {
        catalog: AgentCatalog::from_candidates(by_name.into_values().collect()),
        diagnostics,
    }
}

fn load_agent_candidate(
    path: &Path,
    by_name: &mut HashMap<String, AgentCandidate>,
    diagnostics: &mut Vec<AgentDiagnostic>,
) {
    match parse_agent_candidate(path) {
        Ok(candidate) => insert_file_candidate(path, candidate, by_name, diagnostics),
        Err(error) => diagnostics.push(AgentDiagnostic {
            kind: error.kind,
            path: path.to_path_buf(),
            message: error.message,
        }),
    }
}

fn insert_file_candidate(
    path: &Path,
    candidate: AgentCandidate,
    by_name: &mut HashMap<String, AgentCandidate>,
    diagnostics: &mut Vec<AgentDiagnostic>,
) {
    let definition = &candidate.definition;
    if let Some(existing) = by_name.get(definition.name()) {
        diagnostics.push(AgentDiagnostic {
            kind: AgentDiagnosticKind::DuplicateDefinition,
            path: path.to_path_buf(),
            message: format!(
                "duplicate agent `{}` from file `{}` ignored; {} has higher priority",
                definition.name(),
                path.display(),
                existing.source.origin
            ),
        });
        return;
    }
    by_name.insert(definition.name.clone(), candidate);
}

fn markdown_files(root: &Path) -> Vec<PathBuf> {
    WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect()
}

#[derive(Debug)]
struct AgentFileError {
    kind: AgentDiagnosticKind,
    message: String,
}

impl AgentFileError {
    fn new(kind: AgentDiagnosticKind, error: impl std::fmt::Display) -> Self {
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
fn parse_agent_file(path: &Path) -> Result<AgentDefinition, AgentFileError> {
    parse_agent_candidate(path).map(|candidate| candidate.definition)
}

fn parse_agent_candidate(path: &Path) -> Result<AgentCandidate, AgentFileError> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| AgentFileError::new(AgentDiagnosticKind::ReadFailure, error))?;
    let (frontmatter, prompt) = split_frontmatter(&content)
        .map_err(|error| AgentFileError::new(AgentDiagnosticKind::ParseFailure, error))?;
    let header = Arc::from(frontmatter);
    let frontmatter: AgentFrontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|error| AgentFileError::new(AgentDiagnosticKind::ParseFailure, error))?;
    let name = required_field(frontmatter.name, "name")
        .map_err(|error| AgentFileError::new(AgentDiagnosticKind::InvalidDefinition, error))?;
    let description = required_field(frontmatter.description, "description")
        .map_err(|error| AgentFileError::new(AgentDiagnosticKind::InvalidDefinition, error))?;
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(AgentFileError::new(
            AgentDiagnosticKind::InvalidDefinition,
            format_args!("agent `{name}` prompt body is empty"),
        ));
    }

    let mut definition = AgentDefinition {
        tools: frontmatter.tools,
        disallowed_tools: frontmatter.disabled_tools.unwrap_or_default(),
        model: frontmatter.model,
        max_turns: frontmatter.max_turns,
        ..AgentDefinition::new(name, description, prompt)
    };
    if let Some(tools) = &mut definition.tools {
        tools.retain(|tool| !definition.disallowed_tools.contains(tool));
    }
    definition.validate().map_err(|reason| {
        AgentFileError::new(
            AgentDiagnosticKind::InvalidDefinition,
            format_args!("invalid agent `{}`: {reason}", definition.name()),
        )
    })?;
    Ok(AgentCandidate {
        definition,
        source: AgentSource {
            origin: AgentDefinitionOrigin::File(path.to_path_buf()),
            header: Some(header),
        },
    })
}

fn split_frontmatter(content: &str) -> anyhow::Result<(&str, &str)> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow::anyhow!("missing YAML frontmatter"))?;
    let mut offset = content.len() - rest.len();
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((&content[..offset], &content[offset + line.len()..]));
        }
        offset += line.len();
    }
    anyhow::bail!("unterminated YAML frontmatter at line 1 column 1")
}

fn required_field(value: Option<String>, name: &str) -> anyhow::Result<String> {
    let value = value.unwrap_or_default().trim().to_string();
    if value.is_empty() {
        anyhow::bail!("missing required `{name}` field");
    }
    Ok(value)
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tool_policy_deterministically() {
        let definition = AgentDefinition::new("reviewer", "Review", "Review code")
            .tools(["read", "Bash", "Read", " "])
            .disallowed_tools(["write", "Write"]);

        assert_eq!(definition.tools_policy().unwrap(), ["Bash", "Read"]);
        assert_eq!(definition.disallowed_tools_policy(), ["Write"]);
        assert!(definition.validate().is_ok());
    }

    #[test]
    fn classifies_file_failures() {
        let root = temp_root("diagnostics");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("parse.md"), "missing frontmatter").unwrap();
        std::fs::write(
            root.join("invalid.md"),
            "---\nname: reviewer\n---\nReview code.",
        )
        .unwrap();

        let loaded = load_from_roots(vec![root.clone()]);
        let read_error = parse_agent_file(&root.join("missing.md")).unwrap_err();

        assert_eq!(read_error.kind, AgentDiagnosticKind::ReadFailure);
        assert!(loaded.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == AgentDiagnosticKind::ParseFailure
                && diagnostic.path.ends_with("parse.md")
        }));
        assert!(loaded.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == AgentDiagnosticKind::InvalidDefinition
                && diagnostic.path.ends_with("invalid.md")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preserves_root_priority_and_reports_duplicate_origins() {
        let first_root = temp_root("first-root");
        let second_root = temp_root("second-root");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let winner = first_root.join("z.md");
        let duplicate = second_root.join("a.md");
        write_agent(&winner, "reviewer", "First root");
        write_agent(&duplicate, "reviewer", "Second root");

        let loaded = load_from_roots(vec![first_root.clone(), second_root.clone()]);
        let diagnostic = loaded.diagnostics.first().unwrap();

        assert_eq!(
            loaded.catalog.get("reviewer").unwrap().description(),
            "First root"
        );
        assert_eq!(diagnostic.kind, AgentDiagnosticKind::DuplicateDefinition);
        assert_eq!(diagnostic.path, duplicate);
        assert!(diagnostic.message.contains(&winner.display().to_string()));
        std::fs::remove_dir_all(first_root).unwrap();
        std::fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn sorts_paths_within_each_root() {
        let root = temp_root("path-order");
        std::fs::create_dir_all(&root).unwrap();
        write_agent(&root.join("z.md"), "reviewer", "Later path");
        write_agent(&root.join("a.md"), "reviewer", "Earlier path");

        let loaded = load_from_roots(vec![root.clone()]);

        assert_eq!(
            loaded.catalog.get("reviewer").unwrap().description(),
            "Earlier path"
        );
        assert_eq!(loaded.diagnostics.len(), 1);
        assert!(loaded.diagnostics[0].path.ends_with("z.md"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_tool_settings_reject_retired_gateway() {
        let root = temp_root("retired-gateway-settings");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reviewer.md");
        for (field, value) in [("tools", "[Read, mcp]"), ("disabled-tools", "[MCP]")] {
            std::fs::write(
                &path,
                format!("---\nname: reviewer\ndescription: Review\n{field}: {value}\n---\nReview."),
            )
            .unwrap();
            let error = parse_agent_file(&path).unwrap_err();
            assert!(error.message.contains("MCP settings"), "{error:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rust_agent_tool_settings_reject_direct_mcp_names() {
        for field in ["tools", "disabled-tools"] {
            for name in ["MCP", "repo_search", "mcp__repo__search"] {
                let definition = AgentDefinition::new("reviewer", "Review", "Review.");
                let definition = if field == "tools" {
                    definition.tools([name])
                } else {
                    definition.disallowed_tools([name])
                };
                let mut catalog = AgentCatalog::new(vec![definition]);
                let result = catalog.validate_tool_declarations(&|rule| {
                    if rule.tool == "repo_search" {
                        return Err(ToolRuleError::UnsupportedMcp);
                    }
                    Ok(())
                });
                assert!(result.is_err(), "{field}: {name}");
                assert_eq!(catalog.all().len(), 1);
            }
        }
    }

    #[test]
    fn agent_tool_settings_validate() {
        let root = temp_root("tool-fields");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reviewer.md");
        let header = "---\r\nname: reviewer\r\ndescription: Review\r\n";
        std::fs::write(
            &path,
            format!("{header}tools: [Read, Grep]\r\ndisabled-tools: [Bash]\r\nmax_turns: 3\r\n---\r\nReview."),
        )
        .unwrap();
        let definition = parse_agent_file(&path).unwrap();
        assert_eq!(definition.tools_policy().unwrap(), ["Grep", "Read"]);
        assert_eq!(definition.disallowed_tools_policy(), ["Bash"]);
        assert_eq!(definition.max_turns_limit(), Some(3));
        for field in [
            "disallowedTools: [Bash]",
            "tools: [Read, 42]",
            "tools: [mcp__server__tool]",
            "tools: {Bash: false}",
            "tools:\r\n  Bash: false",
        ] {
            std::fs::write(&path, format!("{header}{field}\r\n---\r\nReview.")).unwrap();
            let error = parse_agent_file(&path).unwrap_err();
            let expected_line = if field.contains('\n') {
                "line 5"
            } else {
                "line 4"
            };
            assert!(error.message.contains(expected_line), "{}", error.message);
            assert!(error.message.contains("column"), "{}", error.message);
            assert_eq!(
                error.message.matches(" at line ").count(),
                1,
                "{}",
                error.message
            );
            assert!(
                !error.message.contains("tools: tools:"),
                "{}",
                error.message
            );
            assert!(
                error.message.contains(field.split(':').next().unwrap()),
                "{}",
                error.message
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generation_validation_uses_loaded_agent_source_and_keeps_valid_definitions() {
        let root = temp_root("generation-validation");
        std::fs::create_dir_all(&root).unwrap();
        write_agent(&root.join("valid.md"), "valid", "Valid");
        let path = root.join("reviewer.md");
        for field in ["tools", "disabled-tools"] {
            for value in [
                "[Read, 'search(query *)']",
                "Read, search(query *)",
                "\r\n  - Read\r\n  - search",
            ] {
                let content = format!(
                    "---\r\nname: reviewer\r\ndescription: Review\r\n{field}: {value}\r\n---\r\nReview."
                );
                std::fs::write(&path, &content).unwrap();
                let mut loaded = load_from_roots(vec![root.clone()]);
                std::fs::write(&path, "changed after load").unwrap();
                let diagnostics = loaded
                    .catalog
                    .validate_tool_declarations(&|rule| {
                        if rule.tool == "search" {
                            return Err(ToolRuleError::UnsupportedMcp);
                        }
                        Ok(())
                    })
                    .unwrap();
                assert!(loaded.catalog.get("reviewer").is_none());
                assert!(loaded.catalog.get("valid").is_some());
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].path, path);
                assert!(
                    diagnostics[0].message.contains(field),
                    "{}",
                    diagnostics[0].message
                );
                let expected_line = if value.starts_with('\r') {
                    "line 6"
                } else {
                    "line 4"
                };
                assert!(
                    diagnostics[0].message.contains(expected_line),
                    "{}",
                    diagnostics[0].message
                );
                assert!(diagnostics[0].message.contains("column"));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_removals_use_whole_tools() {
        let root = temp_root("pool-specifiers");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reviewer.md");
        std::fs::write(&path, "---\nname: reviewer\ndescription: Review\ntools: [Read, Bash]\ndisabled-tools: [Bash(git push *)]\n---\nReview.").unwrap();
        let definition = parse_agent_file(&path).unwrap();
        assert_eq!(definition.tools_policy().unwrap(), ["Read"]);
        assert_eq!(definition.disallowed_tools_policy(), ["Bash"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn markdown_declaration_duplicates_and_empty_pools_preserve_source_semantics() {
        let root = temp_root("declaration-semantics");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reviewer.md");
        let header = "---\r\nname: reviewer\r\ndescription: Review\r\n";
        for field in ["tools", "disabled-tools"] {
            std::fs::write(
                &path,
                format!("{header}{field}: [Read]\r\n{field}: [Bash]\r\n---\r\nReview."),
            )
            .unwrap();
            let error = parse_agent_file(&path).unwrap_err();
            assert!(
                error.message.contains("duplicate field"),
                "{}",
                error.message
            );
            assert!(error.message.contains(field), "{}", error.message);
            assert!(error.message.contains("line 5"), "{}", error.message);
        }
        for declarations in ["tools: []", "tools: [Read, read]\r\ndisabled-tools: [Read]"] {
            std::fs::write(&path, format!("{header}{declarations}\r\n---\r\nReview.")).unwrap();
            let definition = parse_agent_file(&path).unwrap();
            assert_eq!(definition.tools_policy(), Some([].as_slice()));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frontmatter_requires_an_exact_closing_delimiter() {
        assert!(split_frontmatter("---\nname: reviewer\n---suffix\nReview.").is_err());
        assert!(split_frontmatter("---\n---suffix\nReview.").is_err());
        for newline in ["\n", "\r\n"] {
            let content = format!("---{newline}name: reviewer{newline}---{newline}Review.");
            let (header, body) = split_frontmatter(&content).unwrap();
            assert_eq!(body, "Review.");
            let parsed: AgentFrontmatter = serde_yaml::from_str(header).unwrap();
            assert_eq!(parsed.name.as_deref(), Some("reviewer"));
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("phi-agent-{label}-{}", uuid::Uuid::new_v4()))
    }

    fn write_agent(path: &Path, name: &str, description: &str) {
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\nRun carefully."),
        )
        .unwrap();
    }
}
