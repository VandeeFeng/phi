//! Application resource discovery, selection policy, validation and prompt expansion.
//! Application owns resource scope/generation and business effects; TUI owns selection/presentation.
//! Resource loading is also available to standalone configuration without creating an Agent.

mod policy;
pub(crate) mod prompts;

pub use policy::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticResource {
    #[default]
    Prompt,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ResourceDiagnostic {
    #[serde(default)]
    pub(crate) resource: DiagnosticResource,
    pub(crate) path: std::path::PathBuf,
    pub(crate) message: String,
}

pub(crate) fn directory_ancestry(
    path: &std::path::Path,
    ancestors: &[std::path::PathBuf],
) -> Option<Vec<std::path::PathBuf>> {
    let canonical = path.canonicalize().ok()?;
    if ancestors.contains(&canonical) {
        return None;
    }
    Some(ancestors.iter().cloned().chain([canonical]).collect())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentDefinitionView {
    pub(crate) name: String,
    pub(crate) description: String,
}

pub(crate) fn format_diagnostic(title: &str, path: &std::path::Path, message: &str) -> String {
    format!(
        "[{title}]\n  {}\n  {}",
        path.display(),
        message.replace('\n', "\n  ")
    )
}
