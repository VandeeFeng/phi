pub(crate) mod checker;
pub(crate) mod command;
pub(crate) mod execution_scope;
pub(crate) mod pattern;
pub(crate) mod runtime;
pub(crate) mod shell;

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone)]
pub enum ToolPerm {
    Simple(Action),
    Granular(HashMap<String, Action>),
}

impl<'de> Deserialize<'de> for ToolPerm {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let result = if value.is_object() {
            serde_json::from_value(value).map(Self::Granular)
        } else {
            serde_json::from_value(value).map(Self::Simple)
        };
        result.map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfig {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default)]
    pub tools: HashMap<String, ToolPerm>,
    #[serde(rename = "nonInteractiveAsk")]
    pub non_interactive_ask: Option<Action>,
    pub bash: Option<ToolPerm>,
    pub worktree: Option<ToolPerm>,
    pub read: Option<ToolPerm>,
    pub write: Option<ToolPerm>,
    pub edit: Option<ToolPerm>,
    pub mcp: Option<ToolPerm>,
    pub grep: Option<ToolPerm>,
    pub glob: Option<ToolPerm>,
    pub find: Option<ToolPerm>,
    pub ls: Option<ToolPerm>,
    pub write_todo_list: Option<ToolPerm>,
    pub external_directory: Option<HashMap<String, Action>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[default]
    Ask,
    Allow,
    Deny,
    Yolo,
}

impl PermissionConfig {
    pub fn from_value(value: serde_json::Value) -> Option<Self> {
        Self::try_from_value(value).ok()
    }

    pub fn try_from_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        use serde::de::Error;
        serde_json::from_value(value.clone()).map_err(|error| {
            serde_json::Error::custom(format!("{}: {error}", permission_error_path(&value)))
        })
    }
}

fn permission_error_path(value: &serde_json::Value) -> String {
    let Some(fields) = value.as_object() else {
        return "permission".into();
    };
    for (field, value) in fields {
        if serde_json::from_value::<PermissionConfig>(serde_json::json!({field: value})).is_err() {
            if let Some(rules) = value.as_object() {
                for (pattern, action) in rules {
                    let rule = serde_json::json!({field: {pattern: action}});
                    if serde_json::from_value::<PermissionConfig>(rule).is_err() {
                        return format!("permission.{field}[{pattern:?}]");
                    }
                }
            }
            return format!("permission.{field}");
        }
    }
    "permission".into()
}

#[cfg(test)]
mod config_tests {
    use super::PermissionConfig;

    #[test]
    fn permission_config_errors_identify_invalid_fields_and_patterns() {
        for (value, path) in [
            (
                serde_json::json!({"read":"deny", "bash":"aks"}),
                "permission.bash",
            ),
            (
                serde_json::json!({"bash":{"git **":"aks"}}),
                "permission.bash[\"git **\"]",
            ),
            (
                serde_json::json!({"external_directory":{"/tmp/**":"aks"}}),
                "permission.external_directory[\"/tmp/**\"]",
            ),
            (serde_json::json!({"mode":"invalid"}), "permission.mode"),
            (
                serde_json::json!({"mode":"yolo", "bash":"invalid"}),
                "permission.bash",
            ),
            (
                serde_json::json!({"tools":{"custom":"invalid"}}),
                "permission.tools",
            ),
            (serde_json::json!({"glbo":"deny"}), "permission.glbo"),
            (serde_json::json!("aks"), "permission"),
        ] {
            let error = PermissionConfig::try_from_value(value)
                .unwrap_err()
                .to_string();
            assert!(error.contains(path), "{error}");
        }
        for value in [
            serde_json::json!({"mode":"deny"}),
            serde_json::json!({"mode":"yolo", "bash":{"git **":"ask"}}),
            serde_json::json!({"tools":{"custom":{"**":"ask"}}}),
        ] {
            assert!(PermissionConfig::try_from_value(value).is_ok());
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionMode::Ask => write!(f, "ask"),
            PermissionMode::Allow => write!(f, "allow"),
            PermissionMode::Deny => write!(f, "deny"),
            PermissionMode::Yolo => write!(f, "yolo"),
        }
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ask" => Ok(Self::Ask),
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            "yolo" => Ok(Self::Yolo),
            _ => Err("permission mode must be ask, allow, deny or yolo".into()),
        }
    }
}
