use std::path::{Path, PathBuf};

use phi_sdk::extension::BuiltinSkill;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
    #[serde(rename = "license")]
    _license: Option<serde_yaml::Value>,
    #[serde(rename = "compatibility")]
    _compatibility: Option<serde_yaml::Value>,
    #[serde(rename = "metadata")]
    _metadata: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub base_dir: PathBuf,
    pub source: SkillSource,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SkillSource {
    Builtin,
    User,
    // Reserved for skills provided by extensions once extension skill symlinks are wired.
    Extension,
}

pub type SkillDiagnostic = crate::resources::ResourceDiagnostic;

#[derive(Debug, Clone, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
    origins: std::collections::HashMap<String, crate::resources::Scope>,
    real_paths: std::collections::HashSet<PathBuf>,
}

pub(crate) fn initialize_builtin_links(declarations: &[BuiltinSkill]) -> Vec<SkillDiagnostic> {
    let mut diagnostics = Vec::new();
    ensure_builtin_skill_links(
        declarations,
        &global_skills_dir(),
        &phi_sdk::workspace::builtin_skills_dir(),
        &mut diagnostics,
    );
    diagnostics
}

#[cfg(test)]
pub(crate) fn load_for(working_dir: &Path, declarations: &[BuiltinSkill]) -> LoadSkillsResult {
    let policy = crate::resources::ResourcePolicy::load_for(working_dir).unwrap();
    load_selected(&policy, declarations)
}

pub(super) fn load_selected(
    policy: &crate::resources::ResourcePolicy,
    declarations: &[BuiltinSkill],
) -> LoadSkillsResult {
    let mut result = LoadSkillsResult::default();
    for (resource, _) in policy.selected(crate::resources::ResourceKind::Skills) {
        load_file(
            &resource.path,
            resolve_skill_source(&resource.path, SkillSource::User),
            &mut result,
            resource.scope,
        );
    }
    if crate::security::is_read_only() {
        load_unlinked_builtins(&mut result, Some(policy), declarations);
    }
    result.skills.retain(|skill| {
        skill.source != SkillSource::Builtin || builtin_skill_enabled(&skill.name, declarations)
    });
    result
}

fn load_unlinked_builtins(
    result: &mut LoadSkillsResult,
    policy: Option<&crate::resources::ResourcePolicy>,
    declarations: &[BuiltinSkill],
) {
    use crate::resources::{Resource, Scope};
    let Ok(entries) = std::fs::read_dir(phi_sdk::workspace::builtin_skills_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let target = global_skills_dir().join(&name).join("SKILL.md");
        let resource = Resource::builtin_skill(&name);
        if target.exists()
            || !builtin_skill_enabled(&name.to_string_lossy(), declarations)
            || policy.is_some_and(|policy| !policy.enabled(&resource))
        {
            continue;
        }
        load_file(
            &entry.path().join("SKILL.md"),
            SkillSource::Builtin,
            result,
            Scope::Global,
        );
    }
}

fn ensure_builtin_skill_links(
    declarations: &[BuiltinSkill],
    user_skills_dir: &Path,
    builtin_skills_dir: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    if crate::security::ensure_writable(user_skills_dir).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(builtin_skills_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let source = entry.path();
        if !source.join("SKILL.md").is_file() {
            continue;
        }
        let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let target = user_skills_dir.join(name);
        if builtin_skill_enabled(name, declarations) {
            ensure_skill_symlink(&source, &target, diagnostics);
        } else {
            remove_owned_skill_symlink(&source, &target, diagnostics);
        }
    }
}

fn builtin_skill_enabled(name: &str, declarations: &[BuiltinSkill]) -> bool {
    declarations
        .iter()
        .find(|skill| skill.name == name)
        .is_none_or(|skill| skill.enabled)
}

#[cfg(unix)]
fn ensure_skill_symlink(source: &Path, target: &Path, diagnostics: &mut Vec<SkillDiagnostic>) {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => return,
        Ok(_) => return,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            diagnostics.push(diagnostic(
                target,
                format!("failed to inspect skill link: {error}"),
            ));
            return;
        }
    }
    if let Some(parent) = target.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        diagnostics.push(diagnostic(
            parent,
            format!("failed to create skills directory: {error}"),
        ));
        return;
    }
    if let Err(error) = std::os::unix::fs::symlink(source, target) {
        diagnostics.push(diagnostic(
            target,
            format!("failed to create skill link: {error}"),
        ));
    }
}

#[cfg(not(unix))]
fn ensure_skill_symlink(_source: &Path, target: &Path, diagnostics: &mut Vec<SkillDiagnostic>) {
    // TODO: add Windows support, likely using directory junctions or a copy fallback.
    diagnostics.push(diagnostic(
        target,
        "builtin skill links are not supported on this platform yet",
    ));
}

fn remove_owned_skill_symlink(
    source: &Path,
    target: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let Ok(metadata) = std::fs::symlink_metadata(target) else {
        return;
    };
    if !metadata.file_type().is_symlink()
        || std::fs::read_link(target).ok().as_deref() != Some(source)
    {
        return;
    }
    if let Err(error) = std::fs::remove_file(target) {
        diagnostics.push(diagnostic(
            target,
            format!("failed to remove skill link: {error}"),
        ));
    }
}

pub(super) fn system_prompt_hook(skills: &[Skill]) -> Option<phi_sdk::hooks::Hook> {
    let skill_prompt = format_for_prompt(skills)?;
    Some(phi_sdk::hooks::Hook::build_system_prompt(move |event| {
        let skill_prompt = skill_prompt.clone();
        async move {
            Ok(phi_sdk::lifecycle::BuildSystemPromptOutcome {
                system_prompt: format!("{}\n\n{skill_prompt}", event.system_prompt),
            })
        }
    }))
}

fn format_for_prompt(skills: &[Skill]) -> Option<String> {
    let visible = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect::<Vec<_>>();
    if visible.is_empty() {
        return None;
    }

    let mut lines = vec![
        String::from("The following skills provide specialized instructions for specific tasks."),
        String::from(
            "Use the read tool to load a skill's file when the task matches its description.",
        ),
        String::from(
            "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
        ),
        String::new(),
        String::from("<available_skills>"),
    ];
    for skill in visible {
        lines.push(String::from("  <skill>"));
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.path.display().to_string())
        ));
        lines.push(String::from("  </skill>"));
    }
    lines.push(String::from("</available_skills>"));
    Some(lines.join("\n"))
}

#[derive(Debug, PartialEq, Eq)]
pub struct SkillBlock<'a> {
    pub name: &'a str,
    pub content: &'a str,
    pub user_message: Option<&'a str>,
}

pub fn parse_block(text: &str) -> Option<SkillBlock<'_>> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, rest) = rest.split_once("\" location=\"")?;
    let (_, body) = rest.split_once("\">\n")?;
    let (content, trailing) = body.rsplit_once("\n</skill>")?;
    let user_message = if trailing.is_empty() {
        None
    } else {
        Some(
            trailing
                .strip_prefix("\n\n")
                .filter(|text| !text.is_empty())?,
        )
    };
    Some(SkillBlock {
        name,
        content,
        user_message,
    })
}

pub fn expand_command(input: &str, skills: &[Skill]) -> Result<Option<String>, String> {
    let Some((command, args)) = crate::commands::slash_command_parts(input) else {
        return Ok(None);
    };
    let Some(name) = command.strip_prefix("skill:") else {
        return Ok(None);
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(&skill.path)
        .map_err(|error| format!("failed to read skill {}: {error}", skill.path.display()))?;
    let body = strip_frontmatter(&content);
    let body = body.trim();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&skill.path.display().to_string()),
        skill.base_dir.display(),
        body
    );
    Ok(Some(if args.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{}", block, args.trim())
    }))
}

fn global_skills_dir() -> PathBuf {
    crate::config::settings::phi_path().join("skills")
}

fn resolve_skill_source(path: &Path, default_source: SkillSource) -> SkillSource {
    let Ok(target) = std::fs::canonicalize(path) else {
        return default_source;
    };
    if target.starts_with(phi_sdk::workspace::builtin_skills_dir()) {
        SkillSource::Builtin
    } else {
        default_source
    }
}

fn load_file(
    path: &Path,
    source: SkillSource,
    result: &mut LoadSkillsResult,
    scope: crate::resources::Scope,
) {
    let real_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if result.real_paths.contains(&real_path) {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let parent_dir_name = base_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("skill");
    let Some(metadata) = parse_frontmatter(&content, path, &mut result.diagnostics) else {
        return;
    };
    let description = metadata
        .description
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let name = metadata
        .name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| parent_dir_name.to_string());

    let valid = push_validation_diagnostics(
        path,
        &name,
        parent_dir_name,
        description.as_deref(),
        &mut result.diagnostics,
    );
    let Some(description) = description else {
        return;
    };
    if !valid {
        return;
    }
    if let Some(existing) = result.skills.iter().find(|skill| skill.name == name) {
        if existing.path == path {
            return;
        }
        let priority = if result.origins.get(&name) == Some(&crate::resources::Scope::Project)
            && scope == crate::resources::Scope::Global
        {
            "project skill takes priority over global skill"
        } else {
            "earlier skill takes priority"
        };
        result.diagnostics.push(diagnostic(
            path,
            format!(
                "duplicate skill `{name}` from file `{}` ignored; {priority}: `{}`",
                path.display(),
                existing.path.display()
            ),
        ));
        return;
    }

    result.real_paths.insert(real_path);
    result.origins.insert(name.clone(), scope);
    result.skills.push(Skill {
        name,
        description,
        path: path.to_path_buf(),
        base_dir,
        source,
        disable_model_invocation: metadata.disable_model_invocation,
    });
}

fn parse_frontmatter(
    content: &str,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<SkillFrontmatter> {
    if !super::has_frontmatter(content) {
        diagnostics.push(diagnostic(
            path,
            "YAML header is required at line 1 column 1",
        ));
        return None;
    }
    match super::parse_frontmatter(content, std::marker::PhantomData::<SkillFrontmatter>) {
        Ok((metadata, _)) => Some(metadata),
        Err(error) => {
            diagnostics.push(diagnostic(path, error));
            None
        }
    }
}

fn push_validation_diagnostics(
    path: &Path,
    name: &str,
    parent_dir_name: &str,
    description: Option<&str>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> bool {
    let before = diagnostics.len();
    if name != parent_dir_name {
        diagnostics.push(diagnostic(
            path,
            format!("name \"{name}\" does not match parent directory \"{parent_dir_name}\""),
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        diagnostics.push(diagnostic(
            path,
            format!("name exceeds {MAX_NAME_LENGTH} characters ({})", name.len()),
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        diagnostics.push(diagnostic(
            path,
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)",
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(diagnostic(path, "name must not start or end with a hyphen"));
    }
    if name.contains("--") {
        diagnostics.push(diagnostic(
            path,
            "name must not contain consecutive hyphens",
        ));
    }
    match description {
        Some(value) if value.len() > MAX_DESCRIPTION_LENGTH => diagnostics.push(diagnostic(
            path,
            format!(
                "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                value.len()
            ),
        )),
        Some(_) => {}
        None => diagnostics.push(diagnostic(path, "description is required")),
    }
    diagnostics.len() == before
}

fn diagnostic(path: &Path, message: impl Into<String>) -> SkillDiagnostic {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    SkillDiagnostic {
        resource: crate::resources::DiagnosticResource::Skill,
        path,
        message: message.into(),
    }
}

fn strip_frontmatter(content: &str) -> String {
    let normalized = normalize_newlines(content);
    if !normalized.starts_with("---") {
        return normalized;
    }
    normalized[3..]
        .find("\n---")
        .map(|end| normalized[3 + end + 4..].trim().to_string())
        .unwrap_or(normalized)
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_tool_fields_are_rejected_with_markdown_location() {
        let path = Path::new("/tmp/phi-skill-validation/SKILL.md");
        for field in [
            "tools",
            "allowed-tools",
            "disallowed-tools",
            "disabled-tools",
            "disallowedTools",
            "allowedTools",
        ] {
            let content = format!(
                "---\r\nname: review\r\ndescription: Review\r\n{field}: [Bash]\r\n---\r\nReview."
            );
            let mut diagnostics = Vec::new();
            assert!(parse_frontmatter(&content, path, &mut diagnostics).is_none());
            assert_eq!(diagnostics[0].path, path);
            assert_eq!(
                diagnostics[0].resource,
                crate::resources::DiagnosticResource::Skill
            );
            let message = &diagnostics[0].message;
            assert!(message.contains(field), "{message}");
            assert!(message.contains("line 4"), "{message}");
            assert!(message.contains("column"), "{message}");
        }
    }

    #[test]
    fn parses_expanded_skill_block() {
        let block = parse_block(
            "<skill name=\"commit\" location=\"/skills/commit/SKILL.md\">\nfull\n</skill>\ninstructions\n</skill>\n\nfix this",
        )
        .unwrap();

        assert_eq!(block.name, "commit");
        assert_eq!(block.content, "full\n</skill>\ninstructions");
        assert_eq!(block.user_message, Some("fix this"));
    }

    #[test]
    fn rejects_malformed_skill_block_trailing_text() {
        assert!(
            parse_block(
                "<skill name=\"commit\" location=\"/skills/commit/SKILL.md\">\ninstructions\n</skill>trailing"
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_loading_skips_directory_cycles() {
        let directory =
            std::env::temp_dir().join(format!("phi-skill-cycles-{}", uuid::Uuid::new_v4()));
        let root = directory.join("skills");
        let skill = root.join("review/SKILL.md");
        let shared = directory.join("shared/external/SKILL.md");
        for (path, name) in [(&skill, "review"), (&shared, "external")] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("---\nname: {name}\ndescription: Review\n---\nReview."),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink(&root, root.join("cycle")).unwrap();
        std::os::unix::fs::symlink(directory.join("shared"), root.join("linked")).unwrap();
        let policy = crate::resources::ResourcePolicy::from_settings(
            &directory,
            crate::resources::ResourceSettings {
                skills: vec![root.to_string_lossy().into_owned()],
                ..Default::default()
            },
            Default::default(),
        )
        .unwrap();
        let result = load_selected(&policy, &[]);
        assert_eq!(result.skills.len(), 2);
        assert!(result.skills.iter().any(|loaded| loaded.path == skill));
        assert!(
            result
                .skills
                .iter()
                .any(|loaded| loaded.path == root.join("linked/external/SKILL.md"))
        );
        assert!(result.diagnostics.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn skill_aliases_keep_the_first_path_without_collision() {
        let root = std::env::temp_dir().join(format!("phi-skill-aliases-{}", uuid::Uuid::new_v4()));
        let original = root.join("shared/review/SKILL.md");
        let alias = root.join("project/review/SKILL.md");
        for path in [&original, &alias] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(
            &original,
            "---\nname: review\ndescription: Review\n---\nShared instructions.",
        )
        .unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let mut result = LoadSkillsResult::default();
        for (path, scope) in [
            (&alias, crate::resources::Scope::Project),
            (&original, crate::resources::Scope::Global),
        ] {
            load_file(path, SkillSource::User, &mut result, scope);
        }
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].path, alias);
        assert_eq!(result.skills[0].base_dir, alias.parent().unwrap());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let expanded = expand_command("/skill:review", &result.skills)
            .unwrap()
            .unwrap();
        assert!(expanded.contains("Shared instructions."));
        assert!(expanded.contains(&alias.to_string_lossy().to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_loaded_skill_wins_for_duplicate_names() {
        let directory = std::env::temp_dir().join(format!("phi-skills-{}", uuid::Uuid::new_v4()));
        let user = directory.join("user/memory-init/SKILL.md");
        let duplicate = directory.join("duplicate/memory-init/SKILL.md");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
        let content = "---\nname: memory-init\ndescription: test skill\n---\n";
        std::fs::write(&user, content).unwrap();
        std::fs::write(&duplicate, content).unwrap();
        let mut result = LoadSkillsResult::default();

        load_file(
            &user,
            SkillSource::User,
            &mut result,
            crate::resources::Scope::Project,
        );
        load_file(
            &duplicate,
            SkillSource::Builtin,
            &mut result,
            crate::resources::Scope::Global,
        );

        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].source, SkillSource::User);
        assert_eq!(result.skills[0].path, user);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(
            result.diagnostics[0]
                .message
                .contains("project skill takes priority")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shared_builtin_skill_links_resolve_as_builtin() {
        let directory = std::env::temp_dir().join(format!("phi-skills-{}", uuid::Uuid::new_v4()));
        let target = directory.join("memory-init");
        std::fs::create_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(
            phi_sdk::workspace::builtin_skills_dir().join("memory-init"),
            &target,
        )
        .unwrap();

        assert_eq!(
            resolve_skill_source(&target.join("SKILL.md"), SkillSource::User),
            SkillSource::Builtin
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn builtin_selection_and_runtime_availability_both_gate_loading() {
        let directory =
            Path::new("/tmp").join(format!("phi-builtin-loading-{}", uuid::Uuid::new_v4()));
        let linked = directory.join("linked/memory-init");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(
            phi_sdk::workspace::builtin_skills_dir().join("memory-init"),
            &linked,
        )
        .unwrap();
        let mut policy = crate::resources::ResourcePolicy::from_settings(
            &directory,
            crate::resources::ResourceSettings {
                skills: vec![directory.join("linked").to_string_lossy().into_owned()],
                ..Default::default()
            },
            Default::default(),
        )
        .unwrap();
        let enabled = [BuiltinSkill {
            name: "memory-init",
            enabled: true,
        }];
        let disabled = [BuiltinSkill {
            name: "memory-init",
            enabled: false,
        }];
        let has_skill = |policy: &crate::resources::ResourcePolicy,
                         declarations: &[BuiltinSkill]| {
            load_selected(policy, declarations)
                .skills
                .iter()
                .any(|skill| skill.name == "memory-init")
        };
        assert!(has_skill(&policy, &enabled));
        assert!(!has_skill(&policy, &disabled));
        let resource = crate::resources::Resource {
            path: "builtin:skills/memory-init/SKILL.md".into(),
            kind: crate::resources::ResourceKind::Skills,
            scope: crate::resources::Scope::Global,
        };
        policy
            .set(
                &resource,
                crate::resources::Scope::Project,
                crate::resources::Override::Unload,
            )
            .unwrap();
        assert!(!has_skill(&policy, &enabled));
        policy
            .set(
                &resource,
                crate::resources::Scope::Project,
                crate::resources::Override::Load,
            )
            .unwrap();
        assert!(has_skill(&policy, &enabled));
        assert!(!has_skill(&policy, &disabled));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn builtin_skill_links_follow_extension_declarations() {
        let directory = std::env::temp_dir().join(format!("phi-skills-{}", uuid::Uuid::new_v4()));
        let user = directory.join("user");
        let builtin = directory.join("builtin");
        for name in ["general", "extension-skill"] {
            let skill = builtin.join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test skill\n---\n"),
            )
            .unwrap();
        }
        let mut diagnostics = Vec::new();

        let disabled = [BuiltinSkill {
            name: "extension-skill",
            enabled: false,
        }];
        let enabled = [BuiltinSkill {
            name: "extension-skill",
            enabled: true,
        }];
        ensure_builtin_skill_links(&disabled, &user, &builtin, &mut diagnostics);
        assert!(user.join("general/SKILL.md").exists());
        assert!(!user.join("extension-skill").exists());

        ensure_builtin_skill_links(&enabled, &user, &builtin, &mut diagnostics);
        assert!(user.join("extension-skill/SKILL.md").exists());

        ensure_builtin_skill_links(&disabled, &user, &builtin, &mut diagnostics);
        assert!(!user.join("extension-skill").exists());
        assert!(user.join("general/SKILL.md").exists());
        assert!(diagnostics.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn builtin_reconciliation_preserves_unowned_skill_links() {
        let directory = std::env::temp_dir().join(format!("phi-skills-{}", uuid::Uuid::new_v4()));
        let builtin = directory.join("builtin/extension-skill");
        let external = directory.join("external/extension-skill");
        let target = directory.join("user/extension-skill");
        std::fs::create_dir_all(&builtin).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(builtin.join("SKILL.md"), "test").unwrap();
        std::fs::write(external.join("SKILL.md"), "test").unwrap();
        std::os::unix::fs::symlink(&external, &target).unwrap();
        let mut diagnostics = Vec::new();
        let enabled = [BuiltinSkill {
            name: "extension-skill",
            enabled: true,
        }];
        let disabled = [BuiltinSkill {
            name: "extension-skill",
            enabled: false,
        }];

        ensure_builtin_skill_links(
            &enabled,
            target.parent().unwrap(),
            builtin.parent().unwrap(),
            &mut diagnostics,
        );
        ensure_builtin_skill_links(
            &disabled,
            target.parent().unwrap(),
            builtin.parent().unwrap(),
            &mut diagnostics,
        );

        assert_eq!(std::fs::read_link(&target).unwrap(), external);
        assert!(diagnostics.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_invalid_yaml_header() {
        let path = PathBuf::from("/tmp/bad-skill/SKILL.md");
        for (content, expected) in [
            ("---\nmetadata: [broken\n---\nbody", "line 3"),
            (
                "---\ndescription: [broken]\n---\nbody",
                "description: invalid type",
            ),
        ] {
            let mut diagnostics = Vec::new();
            assert!(parse_frontmatter(content, &path, &mut diagnostics).is_none());
            let message = &diagnostics[0].message;
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("column"), "{message}");
            if content.contains("description:") {
                assert!(message.contains("line 2 column 14"), "{message}");
            }
        }
    }
}
