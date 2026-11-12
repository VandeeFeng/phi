use serde::{Deserialize, Serialize};

use crate::utils::workspace::ProjectMeta;

use super::tape::TapeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryContextKind {
    Static,
    Graph,
    KeywordIntro,
}

impl MemoryContextKind {
    pub fn source(self) -> crate::transcript::ContextSource {
        let kind = match self {
            Self::Static => "static",
            Self::Graph => "graph",
            Self::KeywordIntro => "keyword_intro",
        };
        crate::transcript::ContextSource::Extension(crate::transcript::ContextKey::new(
            crate::extension::BuiltinExtension::Memory.path(),
            kind,
        ))
    }

    pub fn from_source(source: &crate::transcript::ContextSource) -> Option<Self> {
        let crate::transcript::ContextSource::Extension(key) = source else {
            return None;
        };
        if key.name() != crate::extension::BuiltinExtension::Memory.path() {
            return None;
        }
        match key.kind() {
            "static" => Some(Self::Static),
            "graph" => Some(Self::Graph),
            "keyword_intro" => Some(Self::KeywordIntro),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryFrontmatter {
    pub description: String,
    pub limit: Option<usize>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFile {
    pub path: String,
    pub frontmatter: MemoryFrontmatter,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryInjectionMode {
    MessageAppend,
}

pub(super) fn deserialize_injection<'de, D>(
    deserializer: D,
) -> Result<Option<MemoryInjectionMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum InjectionSetting {
        Mode(MemoryInjectionMode),
        Enabled(bool),
    }

    match Option::<InjectionSetting>::deserialize(deserializer)? {
        None | Some(InjectionSetting::Enabled(false)) => Ok(None),
        Some(InjectionSetting::Mode(mode)) => Ok(Some(mode)),
        Some(InjectionSetting::Enabled(true)) => Err(serde::de::Error::custom(
            "injection must be false or \"message-append\"",
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySettings {
    pub enabled: Option<bool>,
    pub repo_url: Option<String>,
    pub local_path: Option<String>,
    pub hooks: Option<super::hooks::HookConfig>,
    #[serde(default, deserialize_with = "deserialize_injection")]
    pub injection: Option<MemoryInjectionMode>,
    pub tape: Option<TapeConfig>,
    pub memory_dir: Option<MemoryDirSettings>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDirSettings {
    pub repo_url: Option<String>,
    pub local_path: Option<String>,
    pub global_memory: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMeta {
    #[serde(flatten)]
    pub project_meta: ProjectMeta,
    pub initialized: bool,
    pub memory_path: String,
    pub project: MemoryScopeMeta,
    pub global: GlobalMemoryScopeMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryScopeMeta {
    pub scope: String,
    pub dir: String,
    pub exists: bool,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalMemoryScopeMeta {
    pub scope: String,
    pub dir: Option<String>,
    pub exists: bool,
    pub file_count: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{ContextKey, ContextSource};

    #[test]
    fn memory_sources_preserve_domain_kinds() {
        for (kind, label) in [
            (MemoryContextKind::Static, "static"),
            (MemoryContextKind::Graph, "graph"),
            (MemoryContextKind::KeywordIntro, "keyword_intro"),
        ] {
            let source = kind.source();
            assert_eq!(MemoryContextKind::from_source(&source), Some(kind));
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                serde_json::json!({
                    "extension": { "name": "builtin:memory", "kind": label }
                })
            );
            assert_eq!(
                MemoryContextKind::from_source(&ContextSource::Extension(ContextKey::new(
                    "other", label
                ))),
                None
            );
        }
        assert_eq!(MemoryContextKind::from_source(&ContextSource::Hook), None);
        assert_eq!(
            MemoryContextKind::from_source(&ContextSource::Extension(ContextKey::new(
                "builtin:memory",
                "unknown"
            ))),
            None
        );
    }
}
