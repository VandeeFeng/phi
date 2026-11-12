use super::catalog::ResolvedToolGovernance;
use crate::config::AgentMode;
use crate::core::turn::TurnToolPolicy;
use serde::Deserialize;
use std::collections::HashSet;

// Shared tool-selection and invocation declarations; parsing never authorizes execution.

pub(crate) fn canonical_tool_name(name: impl AsRef<str>) -> String {
    match name.as_ref() {
        "read" | "Read" => "Read",
        "write" | "Write" => "Write",
        "edit" | "Edit" => "Edit",
        "bash" | "Bash" => "Bash",
        "grep" | "Grep" => "Grep",
        "glob" | "Glob" => "Glob",
        "agent" | "Agent" => "Agent",
        "task" | "Task" => "Task",
        "codemode" | "Codemode" => "Codemode",
        "tool_search" | "toolsearch" | "ToolSearch" => "ToolSearch",
        other => other,
    }
    .to_string()
}

/// A tool setting value, not a tool definition or registration.
/// Selection/removal uses the name; pre-approval retains the invocation pattern.
/// Parsing never authorizes execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRule {
    pub tool: String,
    pub pattern: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolRuleError {
    #[error("expected a tool name or Tool(pattern)")]
    InvalidName,
    #[error("expected Tool(pattern) with a nonempty, balanced pattern")]
    InvalidPattern,
    #[error("MCP tools must be configured in MCP settings")]
    UnsupportedMcp,
}

impl std::str::FromStr for ToolRule {
    type Err = ToolRuleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (tool, pattern) = match value.trim().split_once('(') {
            Some((tool, pattern)) => (tool.trim(), invocation_pattern(pattern)?),
            None => (value.trim(), "**"),
        };
        validate_name(tool)?;
        Ok(Self {
            tool: canonical_tool_name(tool),
            pattern: pattern.to_string(),
        })
    }
}

fn validate_name(tool: &str) -> Result<(), ToolRuleError> {
    if tool.eq_ignore_ascii_case("mcp") || tool.to_ascii_lowercase().starts_with("mcp__") {
        return Err(ToolRuleError::UnsupportedMcp);
    }
    if tool.is_empty()
        || !tool
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character))
    {
        return Err(ToolRuleError::InvalidName);
    }
    Ok(())
}

fn invocation_pattern(value: &str) -> Result<&str, ToolRuleError> {
    let pattern = value
        .strip_suffix(')')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .ok_or(ToolRuleError::InvalidPattern)?;
    let mut depth = 0;
    for character in pattern.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return Err(ToolRuleError::InvalidPattern);
        }
    }
    if depth != 0 {
        return Err(ToolRuleError::InvalidPattern);
    }
    Ok(pattern)
}

impl<'de> Deserialize<'de> for ToolRule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RuleVisitor {
            validate: &|_| Ok(()),
        })
    }
}

struct RuleVisitor<'a> {
    validate: &'a dyn Fn(&ToolRule) -> Result<(), ToolRuleError>,
}

impl serde::de::Visitor<'_> for RuleVisitor<'_> {
    type Value = ToolRule;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a tool name or Tool(pattern) string")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        let rule = value.parse().map_err(E::custom)?;
        (self.validate)(&rule).map_err(E::custom)?;
        Ok(rule)
    }
}

impl TurnToolPolicy {
    pub(crate) fn validate(&self) -> Result<(), ToolRuleError> {
        for (tool, pattern) in &self.allowances {
            validate_name(tool)?;
            invocation_pattern(&format!("{pattern})"))?;
        }
        for tool in &self.removals {
            validate_name(tool)?;
        }
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.allowances.extend(
            other
                .allowances
                .iter()
                .map(|(tool, pattern)| (canonical_tool_name(tool), pattern.clone())),
        );
        self.removals
            .extend(other.removals.iter().map(canonical_tool_name));
        self.allowances.sort();
        self.allowances.dedup();
        self.removals.sort();
        self.removals.dedup();
    }
}

/// Tool setting values parsed from a string list or comma-separated string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolRules(pub Vec<ToolRule>);

impl ToolRules {
    pub fn deserialize_with_validation<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
        validate: &dyn Fn(&ToolRule) -> Result<(), ToolRuleError>,
    ) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(RulesVisitor { validate })
            .map(Self)
    }

    /// Normalizes whole-tool selection/removal, discarding invocation patterns.
    pub fn into_names(self) -> Vec<String> {
        let mut names = self.0.into_iter().map(|rule| rule.tool).collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
    }

    /// Retains invocation patterns as pre-approval intent; grants nothing.
    pub fn into_allowances(self) -> Vec<(String, String)> {
        let mut preapprovals = self
            .0
            .into_iter()
            .map(|rule| (rule.tool, rule.pattern))
            .collect::<Vec<_>>();
        preapprovals.sort();
        preapprovals.dedup();
        preapprovals
    }
}

impl<'de> Deserialize<'de> for ToolRules {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::deserialize_with_validation(deserializer, &|_| Ok(()))
    }
}

struct RulesVisitor<'a> {
    validate: &'a dyn Fn(&ToolRule) -> Result<(), ToolRuleError>,
}

impl<'de> serde::de::DeserializeSeed<'de> for RuleVisitor<'_> {
    type Value = ToolRule;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<ToolRule, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for RulesVisitor<'_> {
    type Value = Vec<ToolRule>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a YAML string list or comma-separated tool names")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        let mut depth = 0;
        value
            .split(|character| {
                match character {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                character == ',' && depth == 0
            })
            .map(|value| {
                let rule = value.parse().map_err(E::custom)?;
                (self.validate)(&rule).map_err(E::custom)?;
                Ok(rule)
            })
            .collect()
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> Result<Self::Value, A::Error> {
        let mut rules = Vec::new();
        while let Some(rule) = sequence.next_element_seed(RuleVisitor {
            validate: self.validate,
        })? {
            rules.push(rule);
        }
        Ok(rules)
    }
}

#[derive(Clone)]
pub(super) struct ToolAvailabilityPolicy {
    selected_tools: Option<HashSet<String>>,
    excluded_tools: HashSet<String>,
    mode_excluded_tools: HashSet<String>,
    pub(super) globally_disabled: bool,
}

impl ToolAvailabilityPolicy {
    pub(super) fn new(globally_disabled: bool) -> Self {
        Self {
            selected_tools: None,
            excluded_tools: HashSet::new(),
            mode_excluded_tools: HashSet::new(),
            globally_disabled,
        }
    }

    pub(super) fn set_agent_policy(
        &mut self,
        selected: Option<Vec<String>>,
        excluded: Vec<String>,
    ) {
        self.selected_tools =
            selected.map(|tools| tools.into_iter().map(canonical_tool_name).collect());
        self.excluded_tools = excluded.into_iter().map(canonical_tool_name).collect();
    }

    pub(super) fn set_mode(&mut self, mode: AgentMode) {
        self.mode_excluded_tools = match mode {
            AgentMode::Default => HashSet::new(),
            AgentMode::Plan => ["Write", "Edit"].into_iter().map(String::from).collect(),
        };
    }

    pub(super) fn is_available(
        &self,
        tool_name: &str,
        governance: &ResolvedToolGovernance,
    ) -> bool {
        if self.globally_disabled || self.mode_excluded_tools.contains(tool_name) {
            return false;
        }
        match governance {
            ResolvedToolGovernance::Phi { owner } => {
                let _ = owner;
                self.is_selected(tool_name)
            }
            ResolvedToolGovernance::Delegated {
                owner,
                capability,
                authority,
            } => {
                let _ = owner;
                authority.is_available(capability)
            }
        }
    }

    fn is_selected(&self, tool_name: &str) -> bool {
        !self.excluded_tools.contains(tool_name)
            && self
                .selected_tools
                .as_ref()
                .is_none_or(|selected| selected.contains(tool_name))
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[derive(Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Header {
        #[serde(default, rename = "allowed-tools")]
        tools: ToolRules,
    }

    #[test]
    fn declarations_preserve_patterns_and_normalize_pool_names() {
        for value in ["[Read, 'Bash(git diff *)']", "read, bash(git diff *)"] {
            let header: Header =
                serde_yaml::from_str(&format!("---\r\nallowed-tools: {value}\r\n")).unwrap();
            assert_eq!(header.tools.clone().into_names(), ["Bash", "Read"]);
            assert_eq!(
                header.tools.into_allowances(),
                vec![
                    ("Bash".into(), "git diff *".into()),
                    ("Read".into(), "**".into()),
                ]
            );
        }
        let rules: ToolRules = serde_yaml::from_str("Read, Bash(echo a,b), Read").unwrap();
        assert_eq!(rules.into_names(), ["Bash", "Read"]);
    }

    #[test]
    fn retired_gateway_is_rejected_in_tool_settings() {
        for name in ["MCP", "mcp"] {
            assert!(matches!(
                name.parse::<ToolRule>(),
                Err(ToolRuleError::UnsupportedMcp)
            ));
            for policy in [
                TurnToolPolicy {
                    allowances: vec![(name.into(), "**".into())],
                    ..Default::default()
                },
                TurnToolPolicy {
                    removals: vec![name.into()],
                    ..Default::default()
                },
            ] {
                assert!(matches!(
                    policy.validate(),
                    Err(ToolRuleError::UnsupportedMcp)
                ));
            }
        }
    }

    #[test]
    fn empty_lists_and_duplicates_have_explicit_set_semantics() {
        let rules: ToolRules = serde_yaml::from_str("[]").unwrap();
        assert!(rules.into_names().is_empty());
        let rules: ToolRules =
            serde_yaml::from_str("[Read, read, 'Bash(echo (a,b))', 'Bash(echo (a,b))']").unwrap();
        assert_eq!(rules.clone().into_names(), ["Bash", "Read"]);
        assert_eq!(
            rules.into_allowances(),
            vec![
                ("Bash".into(), "echo (a,b)".into()),
                ("Read".into(), "**".into()),
            ]
        );
    }

    #[test]
    fn invalid_declarations_keep_field_and_source_location() {
        for value in [
            "[Read, 42]",
            "{Bash: true}",
            "null",
            "[mcp__server__tool]",
            "['Bash()']",
            "['Bash(foo))']",
            "['Bash((foo)']",
            "['Bash(foo)extra']",
            "['']",
            "''",
            "'Read,'",
            "'Read,,Bash'",
        ] {
            let error =
                serde_yaml::from_str::<Header>(&format!("---\r\nallowed-tools: {value}\r\n"))
                    .err()
                    .unwrap()
                    .to_string();
            assert!(error.contains("allowed-tools"), "{error}");
            assert!(error.contains("line 2"), "{error}");
            assert!(error.contains("column"), "{error}");
        }
    }
}

/// Available-tool selection and removal, independent of invocation permission.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicy {
    pub allowed: Option<Vec<String>>,
    pub disallowed: Vec<String>,
}

impl ToolPolicy {
    pub fn is_restricted(&self) -> bool {
        self.allowed.is_some() || !self.disallowed.is_empty()
    }
}

/// Agent access paths; Hidden withdraws reachability, not permission rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExposure {
    #[default]
    Direct,
    Deferred,
    Hidden,
}
