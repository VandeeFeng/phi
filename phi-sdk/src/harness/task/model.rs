use crate::harness::instance::AgentInstanceId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::{Component, PathBuf};

const MAX_LIST_ID_BYTES: usize = 128;
const MAX_SUBJECT_BYTES: usize = 1024;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_FORM_BYTES: usize = 1024;
const MAX_OWNER_ID_BYTES: usize = 256;
const MAX_DEPENDENCIES: usize = 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TaskValidationError {
    #[error("invalid task field {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("task field {field} exceeds its limit of {limit}")]
    TooLarge { field: &'static str, limit: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct TaskId(String);

impl TaskId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TaskId {
    type Error = TaskValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.starts_with('0')
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || value.parse::<std::num::NonZeroU64>().is_err()
        {
            return Err(TaskValidationError::InvalidField {
                field: "id",
                reason: "expected a canonical positive decimal u64 string",
            });
        }
        Ok(Self(value))
    }
}

impl From<TaskId> for String {
    fn from(value: TaskId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct TaskListId(String);

impl TaskListId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TaskListId {
    type Error = TaskValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() > MAX_LIST_ID_BYTES {
            return Err(TaskValidationError::TooLarge {
                field: "list_id",
                limit: MAX_LIST_ID_BYTES,
            });
        }
        if !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(TaskValidationError::InvalidField {
                field: "list_id",
                reason: "expected an alphanumeric namespace with optional hyphens, underscores or dots",
            });
        }
        Ok(Self(value))
    }
}

impl From<TaskListId> for String {
    fn from(value: TaskListId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskOwner {
    pub(crate) instance_id: AgentInstanceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Task {
    pub(crate) id: TaskId,
    pub(crate) subject: String,
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active_form: Option<String>,
    pub(crate) status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<TaskOwner>,
    #[serde(default)]
    pub(crate) blocked_by: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub(crate) metadata: Map<String, Value>,
}

impl Task {
    pub(crate) fn validate(&self) -> Result<(), TaskValidationError> {
        if self.subject.trim().is_empty() {
            return Err(TaskValidationError::InvalidField {
                field: "subject",
                reason: "must not be empty",
            });
        }
        for (field, length, limit) in [
            ("subject", self.subject.len(), MAX_SUBJECT_BYTES),
            ("description", self.description.len(), MAX_DESCRIPTION_BYTES),
            (
                "active_form",
                self.active_form.as_ref().map_or(0, String::len),
                MAX_ACTIVE_FORM_BYTES,
            ),
            ("blocked_by", self.blocked_by.len(), MAX_DEPENDENCIES),
        ] {
            if length > limit {
                return Err(TaskValidationError::TooLarge { field, limit });
            }
        }
        if let Some(owner) = &self.owner {
            let id = owner.instance_id.as_str();
            if id.trim().is_empty() || id.chars().any(char::is_control) {
                return Err(TaskValidationError::InvalidField {
                    field: "owner",
                    reason: "expected a non-empty instance reference without control characters",
                });
            }
            if id.len() > MAX_OWNER_ID_BYTES {
                return Err(TaskValidationError::TooLarge {
                    field: "owner",
                    limit: MAX_OWNER_ID_BYTES,
                });
            }
        }
        let mut dependencies = HashSet::new();
        for dependency in &self.blocked_by {
            if dependency == &self.id || !dependencies.insert(dependency) {
                return Err(TaskValidationError::InvalidField {
                    field: "blocked_by",
                    reason: "self-references and duplicate prerequisites are not allowed",
                });
            }
        }
        let metadata_bytes =
            serde_json::to_vec(&self.metadata).map_err(|_| TaskValidationError::InvalidField {
                field: "metadata",
                reason: "must be a serializable JSON object",
            })?;
        if metadata_bytes.len() > MAX_METADATA_BYTES {
            return Err(TaskValidationError::TooLarge {
                field: "metadata",
                limit: MAX_METADATA_BYTES,
            });
        }
        Ok(())
    }
}

/// Trusted storage/list intent; it contains no live execution or management authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskListBinding {
    root: PathBuf,
    list_id: TaskListId,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskListBindingError {
    #[error("task storage root must be absolute and contain no parent components")]
    InvalidRoot,
    #[error("invalid task list namespace: {reason}")]
    InvalidListId { reason: String },
}

impl TaskListBinding {
    pub fn new(
        root: impl Into<PathBuf>,
        list_id: impl Into<String>,
    ) -> Result<Self, TaskListBindingError> {
        let root = root.into();
        if !root.is_absolute()
            || root
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(TaskListBindingError::InvalidRoot);
        }
        let list_id = TaskListId::try_from(list_id.into()).map_err(|error| {
            TaskListBindingError::InvalidListId {
                reason: error.to_string(),
            }
        })?;
        Ok(Self { root, list_id })
    }

    pub fn list_id(&self) -> &str {
        self.list_id.as_str()
    }

    pub(super) fn directory(&self) -> PathBuf {
        self.root.join(self.list_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task_record() -> serde_json::Value {
        json!({
            "id": "2",
            "subject": "Review authentication",
            "description": "Inspect authentication boundaries.",
            "status": "pending",
            "blocked_by": ["1"]
        })
    }

    #[test]
    fn task_ids_roundtrip_as_canonical_positive_decimal_strings() {
        for number in 1..=2048_u64 {
            let text = number.to_string();
            let id: TaskId = serde_json::from_value(json!(text)).unwrap();
            assert_eq!(serde_json::to_value(id).unwrap(), json!(text));
            for invalid in [format!("0{text}"), format!("+{text}"), format!("{text}/1")] {
                assert!(serde_json::from_value::<TaskId>(json!(invalid)).is_err());
            }
        }
        for invalid in [
            "",
            "0",
            "-1",
            "1.0",
            " 1",
            "1 ",
            "../1",
            "١",
            "18446744073709551616",
        ] {
            assert!(serde_json::from_value::<TaskId>(json!(invalid)).is_err());
        }
        assert!(serde_json::from_value::<TaskId>(json!(1)).is_err());
        let maximum: TaskId = serde_json::from_value(json!(u64::MAX.to_string())).unwrap();
        assert_eq!(
            serde_json::to_value(maximum).unwrap(),
            json!(u64::MAX.to_string())
        );
    }

    #[test]
    fn binding_requires_explicit_root_and_validated_namespace_without_io() {
        for root in ["", ".", "tasks", "/tmp/../tasks"] {
            assert!(matches!(
                TaskListBinding::new(root, "shared"),
                Err(TaskListBindingError::InvalidRoot)
            ));
        }
        for namespace in ["", "..", "../other", "a/b"] {
            assert!(matches!(
                TaskListBinding::new("/tmp/phi-tasks", namespace),
                Err(TaskListBindingError::InvalidListId { .. })
            ));
        }
        let root = PathBuf::from("/tmp").join(format!("phi-task-binding-{}", uuid::Uuid::new_v4()));
        let binding = TaskListBinding::new(&root, "shared").unwrap();
        assert_eq!(binding.list_id(), "shared");
        assert!(!root.exists());
    }

    #[test]
    fn list_ids_are_bounded_single_path_components() {
        for valid in [
            "session-12345678",
            "12345678-1234-1234-1234-123456789abc",
            "shared_list.1",
        ] {
            let id: TaskListId = serde_json::from_value(json!(valid)).unwrap();
            assert_eq!(serde_json::to_value(id).unwrap(), json!(valid));
        }
        let maximum = "a".repeat(MAX_LIST_ID_BYTES);
        assert!(serde_json::from_value::<TaskListId>(json!(maximum)).is_ok());
        for invalid in [
            "", ".", "..", "../tasks", "/tasks", "a/b", "a\\b", "a\0b", "a b", "中文", ".lock",
        ] {
            assert!(serde_json::from_value::<TaskListId>(json!(invalid)).is_err());
        }
        assert!(
            serde_json::from_value::<TaskListId>(json!("a".repeat(MAX_LIST_ID_BYTES + 1))).is_err()
        );
    }

    #[test]
    fn task_records_roundtrip_with_optional_fields_omitted() {
        let task: Task = serde_json::from_value(task_record()).unwrap();
        task.validate().unwrap();
        assert_eq!(task.id.as_str(), "2");
        assert_eq!(task.subject, "Review authentication");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.owner.is_none());
        let stored = serde_json::to_value(task).unwrap();
        assert_eq!(stored, task_record());
        for invalid in ["deleted", "unsupported_status"] {
            let mut record = task_record();
            record["status"] = json!(invalid);
            assert!(serde_json::from_value::<Task>(record).is_err());
        }
    }

    #[test]
    fn task_payloads_reject_unsafe_or_unbounded_fields() {
        let mut invalid = Vec::new();
        for (field, value) in [
            ("subject", json!("  ")),
            ("subject", json!("a".repeat(MAX_SUBJECT_BYTES + 1))),
            ("description", json!("a".repeat(MAX_DESCRIPTION_BYTES + 1))),
            ("active_form", json!("a".repeat(MAX_ACTIVE_FORM_BYTES + 1))),
            ("owner", json!({"instance_id": ""})),
            ("owner", json!({"instance_id": "invalid\nowner"})),
            (
                "owner",
                json!({"instance_id": "a".repeat(MAX_OWNER_ID_BYTES + 1)}),
            ),
            ("blocked_by", json!(["2"])),
            ("blocked_by", json!(["1", "1"])),
            ("blocked_by", json!(vec!["1"; MAX_DEPENDENCIES + 1])),
            ("metadata", json!({"data": "a".repeat(MAX_METADATA_BYTES)})),
        ] {
            let mut record = task_record();
            record[field] = value;
            invalid.push(record);
        }
        for record in invalid {
            assert!(
                serde_json::from_value::<Task>(record)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for (field, value) in [
            ("metadata", json!([])),
            ("metadata", json!(null)),
            ("owner", json!("security")),
            ("unknown_field", json!("unexpected")),
        ] {
            let mut record = task_record();
            record[field] = value;
            assert!(serde_json::from_value::<Task>(record).is_err());
        }
    }
}
