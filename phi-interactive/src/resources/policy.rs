use std::path::{Path, PathBuf};

use crate::config::settings;
use anyhow::Context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Extensions,
    Skills,
    Prompts,
}

const RECURSIVE_GLOB: &str = "**";
const BUILTIN_PREFIX: &str = "builtin:";
const BUILTIN_SKILLS_PREFIX: &str = "builtin:skills/";

const RESOURCE_KINDS: [ResourceKind; 3] = [
    ResourceKind::Extensions,
    ResourceKind::Skills,
    ResourceKind::Prompts,
];

impl ResourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Extensions => "Extensions",
            Self::Skills => "Skills",
            Self::Prompts => "Prompts",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResourceSettings {
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub memory: Option<phi_sdk::extension::memory::MemoryConfig>,
}

impl From<crate::config::Settings> for ResourceSettings {
    fn from(config: crate::config::Settings) -> Self {
        Self {
            extensions: config.extensions.unwrap_or_default(),
            skills: config.skills.unwrap_or_default(),
            prompts: config.prompts.unwrap_or_default(),
            memory: config.memory,
        }
    }
}

impl ResourceSettings {
    fn entries(&self, kind: ResourceKind) -> &[String] {
        match kind {
            ResourceKind::Extensions => &self.extensions,
            ResourceKind::Skills => &self.skills,
            ResourceKind::Prompts => &self.prompts,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

pub(crate) fn resource_roots_for(working_dir: &Path, kind: &str) -> Vec<(Scope, PathBuf)> {
    roots_for(
        settings::project_scope_dir_for(working_dir).as_deref(),
        &settings::phi_path(),
        kind,
    )
}

pub(crate) fn is_under_resource_roots(path: &Path, roots: &[(Scope, PathBuf)]) -> bool {
    let cwd = std::env::current_dir().unwrap_or_default();
    roots.iter().any(|(_, root)| {
        let absolute = if root.is_absolute() {
            root.clone()
        } else {
            cwd.join(root)
        };
        path.starts_with(absolute)
    })
}

fn roots_for(project_dir: Option<&Path>, global_dir: &Path, kind: &str) -> Vec<(Scope, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(dir) = project_dir
        && !settings::same_directory(dir, global_dir)
    {
        roots.push((Scope::Project, dir.join(kind)));
    }
    roots.push((Scope::Global, global_dir.join(kind)));
    roots
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Override {
    Inherit,
    Load,
    Unload,
}

#[derive(Clone, Debug)]
pub struct Resource {
    pub path: PathBuf,
    pub kind: ResourceKind,
    pub scope: Scope,
}

impl Resource {
    pub(crate) fn builtin_skill(name: &std::ffi::OsStr) -> Self {
        Self {
            path: PathBuf::from(BUILTIN_SKILLS_PREFIX)
                .join(name)
                .join("SKILL.md"),
            kind: ResourceKind::Skills,
            scope: Scope::Global,
        }
    }

    pub(crate) fn is_builtin(&self) -> bool {
        self.builtin_extension().is_some() || self.builtin_skill_identity().is_some()
    }

    fn builtin_skill_identity(&self) -> Option<PathBuf> {
        if self.kind != ResourceKind::Skills {
            return None;
        }
        if self
            .path
            .to_str()
            .is_some_and(|path| path.starts_with(BUILTIN_SKILLS_PREFIX))
        {
            return Some(self.path.clone());
        }
        let canonical = self.path.canonicalize().ok()?;
        let relative = canonical
            .strip_prefix(phi_sdk::workspace::builtin_skills_dir())
            .ok()?;
        Some(PathBuf::from(BUILTIN_SKILLS_PREFIX).join(relative))
    }

    pub(crate) fn builtin_extension(&self) -> Option<phi_sdk::extension::BuiltinExtension> {
        if self.kind != ResourceKind::Extensions {
            return None;
        }
        phi_sdk::extension::BuiltinExtension::ALL
            .into_iter()
            .find(|extension| self.path == Path::new(extension.path()))
    }

    pub fn name(&self) -> String {
        if let Some(extension) = self.builtin_extension() {
            return extension.name().to_string();
        }
        if self.kind == ResourceKind::Skills {
            return self
                .path
                .parent()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
        }
        self.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

struct ResourceMatcher<'a> {
    path: PathBuf,
    base: &'a Path,
    kind: ResourceKind,
}

impl ResourceMatcher<'_> {
    fn matches(&self, entry: &str) -> bool {
        if let Some(pattern) = entry.strip_prefix('!') {
            glob_match(&self.path, pattern, self.base, self.kind)
        } else {
            exact_match(
                &self.path,
                entry.strip_prefix(['+', '-']).unwrap_or(entry),
                self.base,
                self.kind,
            )
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResourcePolicy {
    global: ResourceSettings,
    project: ResourceSettings,
    global_dir: PathBuf,
    project_dir: PathBuf,
}

impl ResourcePolicy {
    #[cfg(test)]
    pub(crate) fn load_for(working_dir: &Path) -> anyhow::Result<Self> {
        Ok((*crate::config::SettingsManager::load_for(working_dir)?
            .resource_policy_for(working_dir)?)
        .clone())
    }

    pub(crate) fn from_settings(
        working_dir: &Path,
        global: ResourceSettings,
        project: ResourceSettings,
    ) -> anyhow::Result<Self> {
        let global_dir = settings::phi_path();
        let project_dir = settings::project_scope_dir_for(working_dir);
        validate_patterns(&global)?;
        validate_patterns(&project)?;
        let global_dir = if global_dir.is_absolute() {
            normalize_path(global_dir)
        } else {
            normalize_path(working_dir.join(global_dir))
        };
        Ok(Self {
            global,
            project,
            project_dir: project_dir.unwrap_or_else(|| global_dir.clone()),
            global_dir,
        })
    }

    fn has_project_scope(&self) -> bool {
        self.project_dir != self.global_dir
    }

    fn base(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Global => &self.global_dir,
            Scope::Project => &self.project_dir,
        }
    }

    fn settings(&self, scope: Scope) -> &ResourceSettings {
        match scope {
            Scope::Global => &self.global,
            Scope::Project => &self.project,
        }
    }

    fn absolute_path(&self, resource: &Resource) -> PathBuf {
        let path = &resource.path;
        if let Some(identity) = resource.builtin_skill_identity() {
            return identity;
        }
        if path
            .to_str()
            .is_some_and(|path| path.starts_with(BUILTIN_PREFIX))
        {
            return path.to_path_buf();
        }
        if path.is_absolute() {
            normalize_path(path.to_path_buf())
        } else {
            normalize_path(
                self.project_dir
                    .parent()
                    .unwrap_or(&self.project_dir)
                    .join(path),
            )
        }
    }

    fn rule_matcher(&self, resource: &Resource, scope: Scope) -> ResourceMatcher<'_> {
        ResourceMatcher {
            path: self.absolute_path(resource),
            base: self.base(scope),
            kind: resource.kind,
        }
    }

    pub fn override_for(&self, resource: &Resource, scope: Scope) -> Override {
        let matcher = self.rule_matcher(resource, scope);
        self.settings(scope)
            .entries(resource.kind)
            .iter()
            .filter_map(|entry| {
                let state = if entry.starts_with('+') {
                    Override::Load
                } else if entry.starts_with('-') {
                    Override::Unload
                } else {
                    return None;
                };
                matcher.matches(entry).then_some(state)
            })
            .last()
            .unwrap_or(Override::Inherit)
    }

    fn enabled_in_scope(&self, resource: &Resource, scope: Scope, inherited: bool) -> bool {
        if let Some(extension) = resource.builtin_extension() {
            return crate::config::builtin_override(
                self.settings(scope).entries(resource.kind),
                extension,
            )
            .unwrap_or(inherited);
        }
        let matcher = self.rule_matcher(resource, scope);
        let matches = |prefix| {
            self.settings(scope)
                .entries(resource.kind)
                .iter()
                .any(|entry| entry.starts_with(prefix) && matcher.matches(entry))
        };
        let excluded = matches('!');
        let included = matches('+');
        let disabled = matches('-');
        (if included {
            true
        } else if excluded {
            false
        } else {
            inherited
        }) && !disabled
    }

    pub(crate) fn unavailable_reason(
        &self,
        resource: &Resource,
        scope: Scope,
    ) -> Option<&'static str> {
        let path = resource.builtin_skill_identity()?;
        let builtin_settings = |settings: &ResourceSettings| crate::config::Settings {
            extensions: Some(settings.extensions.clone()),
            memory: settings.memory.clone(),
            ..Default::default()
        };
        let mut settings = builtin_settings(&self.global);
        if scope == Scope::Project {
            settings = settings.merge(builtin_settings(&self.project));
        }
        let config = phi_sdk::config::RuntimeConfig::from(&settings);
        let unavailable = phi_sdk::extension::builtin_skills(&config)
            .iter()
            .any(|skill| {
                !skill.enabled
                    && path.parent().and_then(Path::file_name)
                        == Some(std::ffi::OsStr::new(skill.name))
            });
        unavailable.then_some(
            if !config
                .extensions
                .is_enabled(phi_sdk::extension::BuiltinExtension::Memory)
            {
                "memory disabled"
            } else {
                "builtin skill disabled"
            },
        )
    }

    pub fn globally_enabled(&self, resource: &Resource) -> bool {
        self.enabled_in_scope(resource, Scope::Global, true)
    }

    pub fn enabled(&self, resource: &Resource) -> bool {
        let inherited = if resource.scope == Scope::Global {
            self.globally_enabled(resource)
        } else {
            true
        };
        if self.has_project_scope() {
            self.enabled_in_scope(resource, Scope::Project, inherited)
        } else {
            inherited
        }
    }

    pub fn set(
        &mut self,
        resource: &Resource,
        scope: Scope,
        state: Override,
    ) -> anyhow::Result<()> {
        if scope == Scope::Project && !self.has_project_scope() {
            anyhow::bail!("project resources are unavailable in the home directory");
        }
        let path = match scope {
            Scope::Global => self.global_dir.join("settings.json"),
            Scope::Project => self.project_dir.join("settings.json"),
        };
        let key = resource.kind.key();
        let matcher = self.rule_matcher(resource, scope);
        let target = matcher
            .path
            .strip_prefix(matcher.base)
            .unwrap_or(&matcher.path)
            .to_string_lossy()
            .into_owned();
        let entries = crate::config::SettingsManager::update_file(&path, |root| {
            let mut entries: Vec<String> = root
                .get(key)
                .filter(|value| !value.is_null())
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()
                .with_context(|| format!("invalid {key} settings in {}", path.display()))?
                .unwrap_or_default();
            entries.retain(|entry| !entry.starts_with(['+', '-']) || !matcher.matches(entry));
            if state != Override::Inherit {
                let sign = if state == Override::Load { '+' } else { '-' };
                entries.push(format!("{sign}{target}"));
            }
            root.insert(key.into(), serde_json::to_value(&entries)?);
            Ok(entries)
        })?;
        let settings = match scope {
            Scope::Global => &mut self.global,
            Scope::Project => &mut self.project,
        };
        match resource.kind {
            ResourceKind::Extensions => settings.extensions = entries,
            ResourceKind::Skills => settings.skills = entries,
            ResourceKind::Prompts => settings.prompts = entries,
        }
        Ok(())
    }

    pub fn extension_paths(&self) -> Vec<PathBuf> {
        let mut seen = std::collections::HashSet::new();
        self.selected(ResourceKind::Extensions)
            .into_iter()
            .map(|(item, _)| item.path)
            .filter(|path| seen.insert(path.clone()))
            .collect()
    }

    pub(crate) fn selected(&self, kind: ResourceKind) -> Vec<(Resource, PathBuf)> {
        let roots = self.roots(kind);
        let explicit = self.explicit_resources(kind);
        let mut selected = Vec::new();
        for (scope, root) in &roots {
            let mut items = Vec::new();
            discover_dir(root, kind, *scope, &mut items, Discovery::Selected, &[]);
            items.sort_by(|left, right| left.path.cmp(&right.path));
            selected.extend(items.into_iter().map(|item| (item, root.clone())));
            selected.extend(
                explicit
                    .iter()
                    .filter(|(item, _)| {
                        item.scope == *scope && !is_under_resource_roots(&item.path, &roots)
                    })
                    .cloned(),
            );
        }
        selected.retain(|(item, _)| self.enabled(item));
        selected
    }

    pub fn explicit_resources(&self, kind: ResourceKind) -> Vec<(Resource, PathBuf)> {
        let mut found = Vec::new();
        for scope in [Scope::Global, Scope::Project] {
            if scope == Scope::Project && !self.has_project_scope() {
                continue;
            }
            for entry in self.settings(scope).entries(kind) {
                if entry.starts_with(['+', '-', '!']) || entry.starts_with(BUILTIN_PREFIX) {
                    continue;
                }
                let resolved = resolve_entry(self.base(scope), entry);
                for path in expand_resource_glob(&resolved) {
                    let path = normalize_path(path);
                    let root = if path.is_dir() {
                        path.clone()
                    } else {
                        path.parent().unwrap_or(self.base(scope)).to_path_buf()
                    };
                    let mut items = Vec::new();
                    if path.is_dir() {
                        discover_dir(&path, kind, scope, &mut items, Discovery::Explicit, &[]);
                    } else {
                        discover_file(&path, kind, scope, &mut items);
                    }
                    found.extend(items.into_iter().map(|item| (item, root.clone())));
                }
            }
        }
        found
    }

    fn roots(&self, kind: ResourceKind) -> Vec<(Scope, PathBuf)> {
        let project_dir = self
            .has_project_scope()
            .then_some(self.project_dir.as_path());
        roots_for(project_dir, &self.global_dir, kind.key())
    }

    pub fn discover(&self) -> Vec<Resource> {
        let mut items = Vec::new();
        for kind in RESOURCE_KINDS {
            for (scope, dir) in self.roots(kind) {
                discover_dir(&dir, kind, scope, &mut items, Discovery::Default, &[]);
            }
            items.extend(
                self.explicit_resources(kind)
                    .into_iter()
                    .map(|(item, _)| item),
            );
        }
        if let Ok(entries) = std::fs::read_dir(phi_sdk::workspace::builtin_skills_dir()) {
            for entry in entries.flatten() {
                let target = self.global_dir.join("skills").join(entry.file_name());
                if entry.path().join("SKILL.md").is_file()
                    && std::fs::symlink_metadata(&target)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    items.push(Resource::builtin_skill(&entry.file_name()));
                }
            }
        }
        for item in &mut items {
            if item.kind == ResourceKind::Skills {
                item.path = self.absolute_path(item);
            }
        }
        items.sort_by(|a, b| {
            let key = |item: &Resource| {
                (
                    item.scope == Scope::Project,
                    match item.kind {
                        ResourceKind::Extensions => 0,
                        ResourceKind::Skills => 1,
                        ResourceKind::Prompts => 2,
                    },
                    item.path.clone(),
                )
            };
            key(a).cmp(&key(b))
        });
        items.dedup_by(|a, b| a.path == b.path && a.kind == b.kind && a.scope == b.scope);
        items.extend(
            phi_sdk::extension::BuiltinExtension::ALL
                .into_iter()
                .map(|extension| Resource {
                    path: PathBuf::from(extension.path()),
                    kind: ResourceKind::Extensions,
                    scope: Scope::Global,
                }),
        );
        items
    }
}

fn validate_patterns(settings: &ResourceSettings) -> anyhow::Result<()> {
    for kind in RESOURCE_KINDS {
        for entry in settings.entries(kind) {
            if entry.starts_with(['+', '-']) {
                continue;
            }
            let pattern = entry.strip_prefix('!').unwrap_or(entry);
            glob::Pattern::new(pattern)
                .with_context(|| format!("invalid {} pattern {entry:?}", kind.key()))?;
        }
    }
    Ok(())
}

fn resolve_entry(base: &Path, entry: &str) -> PathBuf {
    let path = phi_sdk::workspace::expand_home_path(entry);
    normalize_path(if path.is_absolute() {
        path
    } else {
        base.join(path)
    })
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn exact_match(path: &Path, entry: &str, base: &Path, kind: ResourceKind) -> bool {
    let target = if path
        .to_str()
        .is_some_and(|path| path.starts_with(BUILTIN_PREFIX))
    {
        PathBuf::from(entry)
    } else {
        resolve_entry(base, entry)
    };
    path == target || kind == ResourceKind::Skills && path.parent() == Some(target.as_path())
}

fn glob_match(path: &Path, pattern: &str, base: &Path, kind: ResourceKind) -> bool {
    let Ok(relative_pattern) = glob::Pattern::new(pattern) else {
        return false;
    };
    if path.to_string_lossy().starts_with(BUILTIN_PREFIX) {
        return pattern.starts_with(BUILTIN_PREFIX)
            && (relative_pattern.matches_path(path)
                || kind == ResourceKind::Skills
                    && path
                        .parent()
                        .is_some_and(|parent| relative_pattern.matches_path(parent)));
    }
    let absolute_pattern = resolve_entry(base, pattern);
    let Ok(absolute_pattern) = glob::Pattern::new(&absolute_pattern.to_string_lossy()) else {
        return false;
    };
    let relative = path.strip_prefix(base).unwrap_or(path);
    let options = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    [
        Some(relative),
        Some(path),
        path.file_name().map(Path::new),
        (kind == ResourceKind::Skills)
            .then(|| path.parent())
            .flatten(),
        (kind == ResourceKind::Skills)
            .then(|| relative.parent())
            .flatten(),
    ]
    .into_iter()
    .flatten()
    .any(|candidate| {
        relative_pattern.matches_path_with(candidate, options)
            || absolute_pattern.matches_path_with(candidate, options)
    })
}

fn expand_resource_glob(pattern: &Path) -> Vec<PathBuf> {
    let components = pattern.iter().collect::<Vec<_>>();
    let Some(wildcard) = components.iter().position(|part| {
        let part = part.to_string_lossy();
        glob::Pattern::escape(&part) != part
    }) else {
        return glob::glob(&pattern.to_string_lossy())
            .into_iter()
            .flatten()
            .flatten()
            .collect();
    };
    if glob::Pattern::new(&pattern.to_string_lossy()).is_err() {
        return Vec::new();
    }
    let prefix = components[..wildcard].iter().collect::<PathBuf>();
    let mut found = Vec::new();
    for path in glob::glob(&prefix.to_string_lossy())
        .into_iter()
        .flatten()
        .flatten()
    {
        expand_glob_from(&path, &components[wildcard..], &mut found);
    }
    found.sort();
    found
}

fn expand_glob_from(path: &Path, components: &[&std::ffi::OsStr], found: &mut Vec<PathBuf>) {
    let Some((component, remaining)) = components.split_first() else {
        found.push(path.to_path_buf());
        return;
    };
    if *component != RECURSIVE_GLOB {
        let pattern = PathBuf::from(glob::Pattern::escape(&path.to_string_lossy())).join(component);
        for child in glob::glob(&pattern.to_string_lossy())
            .into_iter()
            .flatten()
            .flatten()
        {
            if child.is_symlink() && child.is_dir() {
                continue;
            }
            expand_glob_from(&child, remaining, found);
        }
        return;
    }
    if !path.is_dir() {
        return;
    }
    let consecutive = components
        .iter()
        .take_while(|part| **part == RECURSIVE_GLOB)
        .count();
    let remaining = &components[consecutive..];
    if !remaining.is_empty() {
        expand_glob_from(path, remaining, found);
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for child in entries.flatten().filter_map(|entry| {
        entry
            .file_type()
            .ok()
            .filter(|kind| kind.is_dir())
            .map(|_| entry.path())
    }) {
        if remaining.is_empty() {
            found.push(child.clone());
        }
        expand_glob_from(&child, components, found);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Discovery {
    Default,
    Explicit,
    Selected,
}

fn discover_dir(
    dir: &Path,
    kind: ResourceKind,
    scope: Scope,
    items: &mut Vec<Resource>,
    discovery: Discovery,
    ancestors: &[PathBuf],
) {
    let Some(ancestors) = super::directory_ancestry(dir, ancestors) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    if discovery == Discovery::Selected
        && kind == ResourceKind::Skills
        && dir.join("SKILL.md").is_file()
    {
        discover_file(&dir.join("SKILL.md"), kind, scope, items);
        return;
    }
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() && (kind != ResourceKind::Extensions || discovery == Discovery::Explicit) {
            discover_dir(&path, kind, scope, items, discovery, &ancestors);
        } else {
            discover_file(&path, kind, scope, items);
        }
    }
}

fn discover_file(path: &Path, kind: ResourceKind, scope: Scope, items: &mut Vec<Resource>) {
    if path.is_file()
        && match kind {
            ResourceKind::Skills => path.file_name().is_some_and(|name| name == "SKILL.md"),
            ResourceKind::Prompts => path.extension().is_some_and(|ext| ext == "md"),
            ResourceKind::Extensions => path.extension().is_some_and(|ext| ext == "lua"),
        }
    {
        items.push(Resource {
            path: path.to_path_buf(),
            kind,
            scope,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #[test]
        fn builtin_skill_rules_preserve_namespace(name in "[a-z][a-z0-9-]{0,20}") {
            let mut policy = ResourcePolicy {
                global: ResourceSettings::default(),
                project: ResourceSettings::default(),
                global_dir: PathBuf::from("/tmp/phi-resource-properties/global"),
                project_dir: PathBuf::from("/tmp/phi-resource-properties/project/.phi"),
            };
            let builtin = Resource {
                path: format!("builtin:skills/{name}/SKILL.md").into(),
                kind: ResourceKind::Skills,
                scope: Scope::Global,
            };
            let user = Resource {
                path: policy.project_dir.join(format!("skills/{name}/SKILL.md")),
                scope: Scope::Project,
                ..builtin.clone()
            };
            proptest::prop_assert!(builtin.is_builtin());
            proptest::prop_assert!(!user.is_builtin());
            for kind in [ResourceKind::Extensions, ResourceKind::Prompts] {
                let other = Resource { kind, ..builtin.clone() };
                proptest::prop_assert!(!other.is_builtin());
            }
            for legacy in [
                format!("skills/{name}/SKILL.md"),
                policy.global_dir.join(format!("skills/{name}/SKILL.md")).display().to_string(),
                "skills/**".into(),
                "**".into(),
            ] {
                policy.global.skills = vec![format!("-{legacy}"), format!("!{legacy}")];
                proptest::prop_assert!(policy.enabled(&builtin));
                proptest::prop_assert_eq!(policy.override_for(&builtin, Scope::Global), Override::Inherit);
                policy.global.skills.insert(0, "!builtin:skills/**".into());
                policy.global.skills.push(format!("+{legacy}"));
                proptest::prop_assert!(!policy.enabled(&builtin));
            }
            for exclusion in ["!builtin:skills/**", "!builtin:skills/*/SKILL.md"] {
                policy.global.skills = vec![exclusion.into()];
                proptest::prop_assert!(!policy.enabled(&builtin));
                proptest::prop_assert!(policy.enabled(&user));
                policy.global.skills.push(format!("+builtin:skills/{name}/SKILL.md"));
                proptest::prop_assert!(policy.enabled(&builtin));
                policy.project.skills = vec![format!("-builtin:skills/{name}/SKILL.md")];
                proptest::prop_assert!(!policy.enabled(&builtin));
                proptest::prop_assert!(policy.enabled(&user));
                policy.project.skills.clear();
            }
        }

        #[test]
        fn builtin_rules_preserve_precedence(
            name in "[a-z][a-z0-9-]{0,20}",
            rules in proptest::collection::vec((0u8..3, 0usize..2), 0..12),
        ) {
            let mut policy = ResourcePolicy {
                global: ResourceSettings::default(),
                project: ResourceSettings::default(),
                global_dir: PathBuf::from("/tmp/phi-resource-properties/global"),
                project_dir: PathBuf::from("/tmp/phi-resource-properties/project/.phi"),
            };
            let resource = Resource::builtin_skill(std::ffi::OsStr::new(&name));
            let targets = [
                format!("builtin:skills/{name}/SKILL.md"),
                format!("builtin:skills/{name}"),
            ];
            let mut excluded = false;
            let mut included = false;
            let mut disabled = false;
            let mut expected_override = Override::Inherit;
            for (rule, target) in rules {
                let prefix = match rule {
                    0 => {
                        excluded = true;
                        '!'
                    }
                    1 => {
                        included = true;
                        expected_override = Override::Load;
                        '+'
                    }
                    _ => {
                        disabled = true;
                        expected_override = Override::Unload;
                        '-'
                    }
                };
                policy.global.skills.push(format!("{prefix}{}", targets[target]));
            }
            proptest::prop_assert_eq!(
                policy.globally_enabled(&resource),
                (included || !excluded) && !disabled,
            );
            proptest::prop_assert_eq!(
                policy.override_for(&resource, Scope::Global),
                expected_override,
            );
        }
    }

    #[test]
    fn builtin_source_paths_are_not_selection_identities() {
        let source = phi_sdk::workspace::builtin_skills_dir().join("memory-init/SKILL.md");
        let resource = Resource::builtin_skill(std::ffi::OsStr::new("memory-init"));
        let mut policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings::default(),
            global_dir: PathBuf::from("/tmp/phi-resource-properties/global"),
            project_dir: PathBuf::from("/tmp/phi-resource-properties/project/.phi"),
        };
        for target in [source.as_path(), source.parent().unwrap()] {
            policy.global.skills = vec![format!("-{}", target.display())];
            assert!(policy.enabled(&resource));
            assert_eq!(
                policy.override_for(&resource, Scope::Global),
                Override::Inherit
            );
            policy.global.skills = vec![
                "!builtin:skills/**".into(),
                format!("+{}", target.display()),
            ];
            assert!(!policy.enabled(&resource));
        }
    }

    #[test]
    fn builtin_skill_selection_is_independent_of_availability() {
        let root = Path::new("/tmp").join(format!("phi-skill-dependency-{}", uuid::Uuid::new_v4()));
        let mut policy = ResourcePolicy {
            global: serde_json::from_value::<crate::config::Settings>(serde_json::json!({
                "memory": { "enabled": true },
                "skills": ["+builtin:skills/memory-init/SKILL.md"]
            }))
            .unwrap()
            .into(),
            project: ResourceSettings::default(),
            global_dir: root.join("global"),
            project_dir: root.join("project/.phi"),
        };
        let skill = Resource {
            path: "builtin:skills/memory-init/SKILL.md".into(),
            kind: ResourceKind::Skills,
            scope: Scope::Global,
        };
        let memory = Resource {
            path: "builtin:memory".into(),
            kind: ResourceKind::Extensions,
            scope: Scope::Global,
        };
        assert!(policy.globally_enabled(&skill));
        assert!(policy.enabled(&skill));
        policy
            .set(&memory, Scope::Global, Override::Unload)
            .unwrap();
        assert!(policy.globally_enabled(&skill));
        assert!(policy.enabled(&skill));
        assert_eq!(
            policy.unavailable_reason(&skill, Scope::Global),
            Some("memory disabled")
        );
        assert_eq!(
            policy.global.skills,
            vec!["+builtin:skills/memory-init/SKILL.md"]
        );
        policy.set(&memory, Scope::Project, Override::Load).unwrap();
        assert!(policy.enabled(&skill));
        assert_eq!(policy.unavailable_reason(&skill, Scope::Project), None);
        assert_eq!(
            policy.unavailable_reason(&skill, Scope::Global),
            Some("memory disabled")
        );
        policy.set(&skill, Scope::Project, Override::Load).unwrap();
        policy
            .set(&memory, Scope::Project, Override::Unload)
            .unwrap();
        assert!(policy.enabled(&skill));
        assert_eq!(
            policy.unavailable_reason(&skill, Scope::Project),
            Some("memory disabled")
        );
        assert_eq!(policy.override_for(&skill, Scope::Project), Override::Load);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundled_skills_are_discovered_without_installed_links() {
        let root = Path::new("/tmp").join(format!("phi-bundled-skills-{}", uuid::Uuid::new_v4()));
        let global_dir = root.join("global");
        let mut policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings::default(),
            global_dir: global_dir.clone(),
            project_dir: root.join("project/.phi"),
        };
        let path = global_dir.join("skills/memory-init/SKILL.md");
        let resource = policy
            .discover()
            .into_iter()
            .find(|resource| resource.path == Path::new("builtin:skills/memory-init/SKILL.md"))
            .unwrap();
        assert!(!global_dir.exists());
        assert!(policy.enabled(&resource));
        policy
            .set(&resource, Scope::Global, Override::Unload)
            .unwrap();
        assert!(!policy.enabled(&resource));
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(global_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved["skills"],
            serde_json::json!(["-builtin:skills/memory-init/SKILL.md"])
        );
        assert!(!path.exists());
        assert!(policy.explicit_resources(ResourceKind::Skills).is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "user skill").unwrap();
        let discovered = policy.discover();
        assert!(!discovered.iter().any(|item| item.path == resource.path));
        let user = discovered.iter().find(|item| item.path == path).unwrap();
        assert!(policy.enabled(user));
        assert_eq!(policy.unavailable_reason(user, Scope::Global), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builtin_selection_round_trips_without_becoming_a_lua_path() {
        use phi_sdk::extension::BuiltinExtension;
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-builtin-policy-{}", uuid::Uuid::new_v4()));
        let global_dir = root.join("global");
        let project_dir = root.join("project/.phi");
        std::fs::create_dir_all(global_dir.join("extensions")).unwrap();
        std::fs::write(global_dir.join("extensions/custom.lua"), "return {}").unwrap();
        let original =
            r#"{"extensions":["-extensions/review.lua"],"mcp":true,"codemode":{"enabled":true}}"#;
        std::fs::write(global_dir.join("settings.json"), original).unwrap();
        let mut policy = ResourcePolicy {
            global: serde_json::from_str::<Settings>(original).unwrap().into(),
            project: ResourceSettings::default(),
            global_dir: global_dir.clone(),
            project_dir: project_dir.clone(),
        };
        let resources = policy.discover();
        assert_eq!(
            resources
                .iter()
                .rev()
                .take(BuiltinExtension::ALL.len())
                .filter(|resource| resource.builtin_extension().is_some())
                .count(),
            BuiltinExtension::ALL.len()
        );
        let native = resources
            .iter()
            .find(|resource| resource.builtin_extension() == Some(BuiltinExtension::Mcp))
            .unwrap();
        policy.set(native, Scope::Global, Override::Unload).unwrap();
        assert!(!policy.enabled(native));
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(global_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved["extensions"],
            serde_json::json!(["-extensions/review.lua", "-builtin:mcp"])
        );
        assert_eq!(saved["mcp"], true);
        assert_eq!(saved["codemode"]["enabled"], true);
        policy.set(native, Scope::Project, Override::Load).unwrap();
        assert!(policy.enabled(native));
        let saved: Settings = settings::load_file(&project_dir.join("settings.json")).unwrap();
        assert_eq!(saved.extensions.unwrap(), ["+builtin:mcp"]);
        policy
            .set(native, Scope::Project, Override::Inherit)
            .unwrap();
        assert!(!policy.enabled(native));
        assert_eq!(
            policy.extension_paths(),
            [global_dir.join("extensions/custom.lua")]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_roots_prioritize_project_without_duplicating_global() {
        let home = std::env::temp_dir().join(format!("phi-roots-{}", uuid::Uuid::new_v4()));
        let global = home.join(".phi");
        let project = home.join("project/.phi");
        for kind in ["agents", "skills", "prompts"] {
            assert_eq!(
                roots_for(None, &global, kind),
                vec![(Scope::Global, global.join(kind))]
            );
            assert_eq!(
                roots_for(Some(&project), &global, kind),
                vec![
                    (Scope::Project, project.join(kind)),
                    (Scope::Global, global.join(kind)),
                ]
            );
        }
        assert_eq!(
            roots_for(Some(&global), &global, "skills"),
            vec![(Scope::Global, global.join("skills"))]
        );
    }

    #[test]
    fn child_resources_use_explicit_working_directory_without_switching_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let child = std::env::temp_dir().join(format!("phi-child-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&child).unwrap();
        let roots = resource_roots_for(&child, "agents");
        assert_eq!(
            roots.first(),
            Some(&(Scope::Project, child.join(".phi/agents")))
        );
        {
            let extension = child.join(".phi/extensions/child.lua");
            std::fs::create_dir_all(extension.parent().unwrap()).unwrap();
            std::fs::write(&extension, "return {}").unwrap();
            let policy = ResourcePolicy::load_for(&child).unwrap();
            assert!(policy.extension_paths().contains(&extension));
            assert!(
                crate::config::SettingsManager::load_for(&child)
                    .unwrap()
                    .resource_policy_for(&child)
                    .unwrap()
                    .extension_paths()
                    .contains(&extension)
            );
        }
        assert_eq!(std::env::current_dir().unwrap(), cwd);
        std::fs::remove_dir_all(child).unwrap();
    }

    #[test]
    fn global_disable_and_project_override_round_trip() {
        let root = std::env::temp_dir().join(format!("phi-config-test-{}", uuid::Uuid::new_v4()));
        let global_dir = root.join("global");
        let project_dir = root.join("project/.phi");
        let mut policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings::default(),
            global_dir: global_dir.clone(),
            project_dir: project_dir.clone(),
        };
        let resource = Resource {
            path: global_dir.join("skills/example/SKILL.md"),
            kind: ResourceKind::Skills,
            scope: Scope::Global,
        };
        assert!(policy.enabled(&resource));
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("settings.json"),
            r#"{"defaultModel":"test","permission":{"glob":{"**":"deny"}}}"#,
        )
        .unwrap();
        policy
            .set(&resource, Scope::Global, Override::Unload)
            .unwrap();
        assert!(!policy.enabled(&resource));
        policy
            .set(&resource, Scope::Project, Override::Load)
            .unwrap();
        assert!(policy.enabled(&resource));
        policy
            .set(&resource, Scope::Project, Override::Inherit)
            .unwrap();
        assert!(!policy.enabled(&resource));
        let global_data = std::fs::read(global_dir.join("settings.json")).unwrap();
        let global_value: serde_json::Value = serde_json::from_slice(&global_data).unwrap();
        assert_eq!(global_value["permission"]["glob"]["**"], "deny");
        let global: Settings = serde_json::from_slice(&global_data).unwrap();
        assert_eq!(global.default_model.as_deref(), Some("test"));
        let project: Settings =
            serde_json::from_slice(&std::fs::read(project_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(global.skills.unwrap(), vec!["-skills/example/SKILL.md"]);
        assert!(project.skills.unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    use crate::config::Settings;

    #[test]
    fn keeps_existing_keys_in_place_and_appends_new_keys() {
        let root = std::env::temp_dir().join(format!("phi-order-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        let original = r#"{"defaultModel":"test","skills":["-skills/old/SKILL.md"],"permission":{"mcp":"deny"}}"#;
        std::fs::write(&path, original).unwrap();
        let mut policy = ResourcePolicy {
            global: serde_json::from_str::<Settings>(original).unwrap().into(),
            project: ResourceSettings::default(),
            global_dir: root.clone(),
            project_dir: root.join("project/.phi"),
        };
        let skill = Resource {
            path: root.join("skills/new/SKILL.md"),
            kind: ResourceKind::Skills,
            scope: Scope::Global,
        };
        policy.set(&skill, Scope::Global, Override::Load).unwrap();
        let extension = Resource {
            path: root.join("extensions/example.lua"),
            kind: ResourceKind::Extensions,
            scope: Scope::Global,
        };
        policy
            .set(&extension, Scope::Global, Override::Unload)
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let keys = [
            "\"defaultModel\"",
            "\"skills\"",
            "\"permission\"",
            "\"extensions\"",
        ];
        let positions = keys.map(|key| written.find(key).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(written.contains("-skills/old/SKILL.md"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toggling_external_resources_preserves_discovery() {
        let root =
            std::env::temp_dir().join(format!("phi-resource-toggle-{}", uuid::Uuid::new_v4()));
        let global_dir = root.join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        for (kind, relative) in [
            (ResourceKind::Extensions, "external/extension.lua"),
            (ResourceKind::Prompts, "external/prompt.md"),
            (ResourceKind::Skills, "external/skill/SKILL.md"),
        ] {
            let resource = Resource {
                path: root.join(relative),
                kind,
                scope: Scope::Global,
            };
            std::fs::create_dir_all(resource.path.parent().unwrap()).unwrap();
            std::fs::write(&resource.path, "test").unwrap();
            let value = serde_json::json!({kind.key(): [resource.path]});
            std::fs::write(
                global_dir.join("settings.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            let mut policy = ResourcePolicy {
                global: serde_json::from_value::<Settings>(value).unwrap().into(),
                project: ResourceSettings::default(),
                global_dir: global_dir.clone(),
                project_dir: root.join("project/.phi"),
            };
            for state in [Override::Unload, Override::Load, Override::Inherit] {
                policy.set(&resource, Scope::Global, state).unwrap();
                policy.global =
                    crate::config::settings::load_file(&global_dir.join("settings.json"))
                        .unwrap()
                        .into();
                assert!(
                    policy
                        .discover()
                        .iter()
                        .any(|item| item.path == resource.path)
                );
                assert_eq!(policy.enabled(&resource), state != Override::Unload);
                assert_eq!(policy.override_for(&resource, Scope::Global), state);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_toggle_replaces_equivalent_directory_overrides() {
        let root = std::env::temp_dir().join(format!("phi-skill-toggle-{}", uuid::Uuid::new_v4()));
        let global_dir = root.join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        for entry in ["-skills/review", "-./skills/review/SKILL.md"] {
            let resource = Resource {
                path: global_dir.join("skills/review/SKILL.md"),
                kind: ResourceKind::Skills,
                scope: Scope::Global,
            };
            let value = serde_json::json!({"skills": [entry]});
            std::fs::write(
                global_dir.join("settings.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            let mut policy = ResourcePolicy {
                global: serde_json::from_value::<Settings>(value).unwrap().into(),
                project: ResourceSettings::default(),
                global_dir: global_dir.clone(),
                project_dir: root.join("project/.phi"),
            };
            assert!(!policy.enabled(&resource));
            policy
                .set(&resource, Scope::Global, Override::Load)
                .unwrap();
            assert!(policy.enabled(&resource), "{entry}");
            assert_eq!(policy.global.skills, ["+skills/review/SKILL.md"]);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_toggle_preserves_edits_since_policy_load() {
        let root = std::env::temp_dir().join(format!("phi-resource-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings::default(),
            global_dir: root.clone(),
            project_dir: root.join("project/.phi"),
        };
        std::fs::write(
            root.join("settings.json"),
            r#"{"prompts":["external/review.md","!prompts/*.md"]}"#,
        )
        .unwrap();
        let resource = Resource {
            path: root.join("prompts/review.md"),
            kind: ResourceKind::Prompts,
            scope: Scope::Global,
        };
        policy
            .set(&resource, Scope::Global, Override::Load)
            .unwrap();
        let saved = crate::config::settings::load_file(&root.join("settings.json")).unwrap();
        assert_eq!(
            saved.prompts.unwrap(),
            ["external/review.md", "!prompts/*.md", "+prompts/review.md"]
        );
        let accepted = policy.global.prompts.clone();
        let invalid = r#"{"prompts":{"invalid":true}}"#;
        let path = root.join("settings.json");
        std::fs::write(&path, invalid).unwrap();
        assert!(
            policy
                .set(&resource, Scope::Global, Override::Unload)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), invalid);
        assert_eq!(policy.global.prompts, accepted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn discovery_skips_cycles_but_keeps_directory_aliases() {
        let root =
            std::env::temp_dir().join(format!("phi-resource-cycles-{}", uuid::Uuid::new_v4()));
        for kind in RESOURCE_KINDS {
            let directory = root.join(kind.key());
            let shared = root.join(format!("shared-{}", kind.key()));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::create_dir_all(&shared).unwrap();
            let filename = match kind {
                ResourceKind::Extensions => "probe.lua",
                ResourceKind::Skills => "SKILL.md",
                ResourceKind::Prompts => "probe.md",
            };
            std::fs::write(directory.join(filename), "local").unwrap();
            std::fs::write(shared.join(filename), "shared").unwrap();
            std::os::unix::fs::symlink(&directory, directory.join("cycle")).unwrap();
            for alias in ["first", "second"] {
                std::os::unix::fs::symlink(&shared, directory.join(alias)).unwrap();
            }
            let mut resources = Vec::new();
            discover_dir(
                &directory,
                kind,
                Scope::Project,
                &mut resources,
                Discovery::Explicit,
                &[],
            );
            assert_eq!(resources.len(), 3, "{kind:?}");
            for path in [
                directory.join(filename),
                directory.join("first").join(filename),
                directory.join("second").join(filename),
            ] {
                assert!(resources.iter().any(|resource| resource.path == path));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recursive_globs_skip_symlinked_directories() {
        let root = std::env::temp_dir().join(format!("phi-glob-cycles-{}", uuid::Uuid::new_v4()));
        for kind in RESOURCE_KINDS {
            for depth in [0, 2] {
                let directory = root.join(format!("{}-{depth}", kind.key()));
                let shared = root.join(format!("shared-{}-{depth}", kind.key()));
                let nested = (0..depth).fold(directory.clone(), |path, _| path.join("nested"));
                let filename = match kind {
                    ResourceKind::Extensions => "probe.lua",
                    ResourceKind::Skills => "SKILL.md",
                    ResourceKind::Prompts => "probe.md",
                };
                for path in [
                    directory.join(filename),
                    nested.join("leaf").join(filename),
                    shared.join(filename),
                ] {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "resource").unwrap();
                }
                std::os::unix::fs::symlink(&directory, nested.join("cycle")).unwrap();
                for alias in ["first", "second"] {
                    std::os::unix::fs::symlink(&shared, directory.join(alias)).unwrap();
                }
                std::os::unix::fs::symlink(directory.join("missing"), directory.join("broken"))
                    .unwrap();
                let mut expected_directories = vec![nested.join("leaf")];
                let mut ancestor = directory.clone();
                for _ in 0..depth {
                    ancestor.push("nested");
                    expected_directories.push(ancestor.clone());
                }
                expected_directories.sort();
                assert_eq!(
                    expand_resource_glob(&directory.join(RECURSIVE_GLOB)),
                    expected_directories
                );
                for scope in [Scope::Global, Scope::Project] {
                    let mut selected = ResourceSettings::default();
                    let entries = vec![format!("{}/**/**/{filename}", directory.display())];
                    match kind {
                        ResourceKind::Extensions => selected.extensions = entries,
                        ResourceKind::Skills => selected.skills = entries,
                        ResourceKind::Prompts => selected.prompts = entries,
                    }
                    let (global, project) = match scope {
                        Scope::Global => (selected, ResourceSettings::default()),
                        Scope::Project => (ResourceSettings::default(), selected),
                    };
                    let policy = ResourcePolicy {
                        global,
                        project,
                        global_dir: root.join("global"),
                        project_dir: root.join("project/.phi"),
                    };
                    let found = policy.explicit_resources(kind);
                    let mut expected =
                        vec![directory.join(filename), nested.join("leaf").join(filename)];
                    expected.sort();
                    assert_eq!(
                        found
                            .iter()
                            .map(|(resource, _)| &resource.path)
                            .collect::<Vec<_>>(),
                        expected.iter().collect::<Vec<_>>(),
                        "{kind:?}, {scope:?}, depth {depth}"
                    );
                    assert!(
                        found
                            .iter()
                            .all(|(resource, source_root)| resource.scope == scope
                                && resource.path.parent() == Some(source_root.as_path()))
                    );
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn glob_traversal_requires_explicit_symlink_roots() {
        let root = std::env::temp_dir().join(format!("phi-glob-links-{}", uuid::Uuid::new_v4()));
        let shared = root.join("shared");
        let directory = root.join("resources");
        let linked = directory.join("linked");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(shared.join("review.md"), "Shared review").unwrap();
        std::os::unix::fs::symlink(&shared, &linked).unwrap();
        for pattern in ["**/*.md", "*/review.md", "*/**/*.md", "**/linked/*.md"] {
            assert!(
                expand_resource_glob(&directory.join(pattern)).is_empty(),
                "{pattern}"
            );
        }
        assert_eq!(expand_resource_glob(&linked), [linked.clone()]);
        assert_eq!(
            expand_resource_glob(&linked.join("**/*.md")),
            [linked.join("review.md")]
        );
        let policy = ResourcePolicy {
            global: ResourceSettings {
                prompts: vec![linked.to_string_lossy().into_owned()],
                ..Default::default()
            },
            project: ResourceSettings::default(),
            global_dir: root.clone(),
            project_dir: root.join("project/.phi"),
        };
        let found = policy.explicit_resources(ResourceKind::Prompts);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.path, linked.join("review.md"));
        assert_eq!(found[0].1, linked);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_globs_preserve_acyclic_pattern_semantics() {
        let root = std::env::temp_dir().join(format!("phi-glob-patterns-{}", uuid::Uuid::new_v4()));
        for relative in [
            "review1.md",
            "review2.md",
            "[notes].md",
            ".hidden/review3.md",
            "team-a/review1.md",
            "team-a/team-b/review2.md",
            "team-a/empty/.keep",
            "team-b/review2.md",
            "team-b/probe.lua",
            "literal[dir]/review1.md",
            "unicode-\u{00e9}/review2.md",
        ] {
            let path = root.join("external").join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "resource").unwrap();
        }
        let mut patterns = [
            "external/*.md",
            "external/*/review?.md",
            "external/**/review[12].md",
            "external/**/[[]notes].md",
            "external/**",
            "external/*/**",
            "external/**/**/review*.md",
            "external/**/team-*/**/*.md",
            "external/literal[[]dir]/**/*.md",
            "external/**/missing/*.md",
        ]
        .map(String::from)
        .to_vec();
        for first in ["*", "**", "team-*"] {
            for second in ["*", "**", "team-*"] {
                for tail in ["*.md", "**", "**/review?.md"] {
                    patterns.push(format!("external/{first}/{second}/{tail}"));
                }
            }
        }
        for pattern in patterns {
            let resolved = resolve_entry(&root, &pattern);
            let mut expected = Vec::new();
            for path in glob::glob(&resolved.to_string_lossy()).unwrap().flatten() {
                let source_root = if path.is_dir() {
                    path.clone()
                } else {
                    path.parent().unwrap().to_owned()
                };
                let mut resources = Vec::new();
                if path.is_dir() {
                    discover_dir(
                        &path,
                        ResourceKind::Prompts,
                        Scope::Global,
                        &mut resources,
                        Discovery::Explicit,
                        &[],
                    );
                } else {
                    discover_file(&path, ResourceKind::Prompts, Scope::Global, &mut resources);
                }
                expected.extend(
                    resources
                        .into_iter()
                        .map(|resource| (resource.path, source_root.clone())),
                );
            }
            let policy = ResourcePolicy {
                global: ResourceSettings {
                    prompts: vec![pattern.clone()],
                    ..Default::default()
                },
                project: ResourceSettings::default(),
                global_dir: root.clone(),
                project_dir: root.join("project/.phi"),
            };
            let found = policy
                .explicit_resources(ResourceKind::Prompts)
                .into_iter()
                .map(|(resource, source_root)| (resource.path, source_root))
                .collect::<Vec<_>>();
            assert_eq!(found, expected, "{pattern}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_and_glob_entries_discover_external_resources() {
        let root = std::env::temp_dir().join(format!("phi-pattern-test-{}", uuid::Uuid::new_v4()));
        let global_dir = root.join("global");
        let external = root.join("external");
        for path in [
            external.join("skills/a/SKILL.md"),
            external.join("skills/b/SKILL.md"),
            external.join("prompts/one.md"),
            external.join("prompts/two.md"),
            external.join("extensions/example.lua"),
            external.join("extensions/nested/another.lua"),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "test").unwrap();
        }
        let policy = ResourcePolicy {
            global: ResourceSettings {
                skills: vec![external.join("skills").to_string_lossy().into_owned()],
                prompts: vec![format!("{}/*.md", external.join("prompts").display())],
                extensions: vec![
                    external.join("extensions").to_string_lossy().into_owned(),
                    external
                        .join("extensions/example.lua")
                        .to_string_lossy()
                        .into_owned(),
                ],
                ..Default::default()
            },
            project: ResourceSettings::default(),
            global_dir,
            project_dir: root.join("project/.phi"),
        };
        let found = policy.discover();
        assert_eq!(
            found
                .iter()
                .filter(|item| item.kind == ResourceKind::Skills && item.path.starts_with(&external))
                .count(),
            2
        );
        assert_eq!(
            found
                .iter()
                .filter(|item| item.kind == ResourceKind::Prompts)
                .count(),
            2
        );
        assert_eq!(
            found
                .iter()
                .filter(|item| item.kind == ResourceKind::Extensions
                    && item.builtin_extension().is_none())
                .count(),
            2
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn glob_exclusion_and_exact_overrides_have_fixed_precedence() {
        let root = std::env::temp_dir().join(format!("phi-pattern-test-{}", uuid::Uuid::new_v4()));
        let base = root.join("global");
        let skill = Resource {
            path: base.join("skills/experimental/SKILL.md"),
            kind: ResourceKind::Skills,
            scope: Scope::Global,
        };
        let mut policy = ResourcePolicy {
            global: ResourceSettings {
                skills: vec!["!skills/*/SKILL.md".into()],
                ..ResourceSettings::default()
            },
            project: ResourceSettings::default(),
            global_dir: base,
            project_dir: root.join("project/.phi"),
        };
        assert!(!policy.enabled(&skill));
        policy
            .global
            .skills
            .push("+skills/experimental/SKILL.md".into());
        assert!(policy.enabled(&skill));
        policy
            .global
            .skills
            .push("-skills/experimental/SKILL.md".into());
        assert!(!policy.enabled(&skill));
        policy
            .project
            .skills
            .push(format!("+{}", skill.path.display()));
        assert!(policy.enabled(&skill));
        policy
            .project
            .skills
            .push(format!("-{}", skill.path.display()));
        assert!(!policy.enabled(&skill));
        policy.project.skills.clear();
        policy
            .project
            .skills
            .push(format!("!{}", skill.path.parent().unwrap().display()));
        assert!(!policy.enabled(&skill));
    }

    #[test]
    fn discovers_project_prompts_under_phi_directory_only() {
        let root = std::env::temp_dir().join(format!("phi-prompt-root-{}", uuid::Uuid::new_v4()));
        let project = root.join("project/.phi");
        let project_prompt = project.join("prompts/review.md");
        let old_prompt = root.join("project/prompts/review.md");
        for path in [&project_prompt, &old_prompt] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "review").unwrap();
        }
        let policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings::default(),
            global_dir: root.join("global/.phi"),
            project_dir: project,
        };

        let found = policy.discover();
        assert!(found.iter().any(|resource| resource.path == project_prompt));
        assert!(!found.iter().any(|resource| resource.path == old_prompt));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_parent_glob_filters_configured_project_prompts() {
        let root = std::env::temp_dir().join(format!("phi-pattern-test-{}", uuid::Uuid::new_v4()));
        let path = root.join("prompts/experimental.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "test").unwrap();
        let policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings {
                prompts: vec![
                    "../prompts/*.md".into(),
                    "!../prompts/experimental.md".into(),
                ],
                ..ResourceSettings::default()
            },
            global_dir: root.join("global"),
            project_dir: root.join(".phi"),
        };
        let resource = policy
            .discover()
            .into_iter()
            .find(|item| item.path == path)
            .unwrap();
        assert!(!policy.enabled(&resource));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_project_prompt_respects_exclusion_and_override() {
        let project_dir = std::env::current_dir().unwrap().join(".phi");
        let resource = Resource {
            path: PathBuf::from("prompts/example.md"),
            kind: ResourceKind::Prompts,
            scope: Scope::Project,
        };
        let mut policy = ResourcePolicy {
            global: ResourceSettings::default(),
            project: ResourceSettings {
                prompts: vec!["!../prompts/*.md".into()],
                ..ResourceSettings::default()
            },
            global_dir: project_dir.join("global"),
            project_dir,
        };
        assert!(!policy.enabled(&resource));
        policy.project.prompts.push("+../prompts/example.md".into());
        assert!(policy.enabled(&resource));
        assert_eq!(
            policy.override_for(&resource, Scope::Project),
            Override::Load
        );
    }
}
