use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use super::{HookTrust, HookTrustRule, Settings, settings};
use crate::resources::ResourcePolicy;

#[derive(Debug)]
struct SettingsState {
    global: Settings,
    project: Settings,
    merged: Settings,
}

#[derive(Clone, Debug)]
pub(crate) struct SettingsManager {
    working_dir: Arc<PathBuf>,
    project_scope: Option<PathBuf>,
    state: Arc<RwLock<SettingsState>>,
}

impl SettingsManager {
    #[cfg(test)]
    pub(crate) fn load() -> anyhow::Result<Self> {
        Self::load_for(&std::env::current_dir()?)
    }

    pub(crate) fn load_for(working_dir: &Path) -> anyhow::Result<Self> {
        let (global, project) = settings::read_sources(working_dir)?;
        Self::from_settings(working_dir, global, project)
    }

    pub(crate) fn from_settings(
        working_dir: &Path,
        global: Settings,
        project: Settings,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            working_dir: Arc::new(std::path::absolute(working_dir)?),
            project_scope: settings::project_scope_dir_for(working_dir),
            state: Arc::new(RwLock::new(Self::resolve(working_dir, global, project)?)),
        })
    }

    fn resolve(
        working_dir: &Path,
        global: Settings,
        project: Settings,
    ) -> anyhow::Result<SettingsState> {
        anyhow::ensure!(
            project
                .hooks
                .as_ref()
                .is_none_or(|hooks| hooks.trust.is_none()),
            "hook workspace trust is only supported in global settings"
        );
        anyhow::ensure!(
            project
                .hooks
                .as_ref()
                .is_none_or(super::schema::HookSettings::is_empty),
            "command hooks are only supported in global settings; project hooks require explicit trust"
        );
        ResourcePolicy::from_settings(working_dir, global.clone().into(), project.clone().into())?;
        let merged = global.clone().merge_project(project.clone());
        Ok(SettingsState {
            global,
            project,
            merged,
        })
    }

    pub(crate) fn hook_workspace_trust(
        &self,
        working_dir: &Path,
    ) -> anyhow::Result<Option<HookTrust>> {
        let root = phi_sdk::workspace::project_scope_root(working_dir).canonicalize()?;
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.merged.hooks_enabled() {
            return Ok(Some(HookTrust::Disabled));
        }
        let mut decision = None;
        for rule in state
            .global
            .hooks
            .as_ref()
            .and_then(|hooks| hooks.trust.as_ref())
            .into_iter()
            .flatten()
        {
            if rule.path == root {
                if rule.trust == HookTrust::Disabled {
                    return Ok(Some(HookTrust::Disabled));
                }
                decision = Some(HookTrust::Trusted);
            }
        }
        Ok(decision)
    }

    pub(crate) fn set_hook_workspace_trust(
        &self,
        working_dir: &Path,
        decision: HookTrust,
    ) -> anyhow::Result<()> {
        self.check_scope(working_dir)?;
        let root = phi_sdk::workspace::project_scope_root(working_dir).canonicalize()?;
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let trust = Self::update_file(&settings::settings_file_path(), |document| {
            let mut global: Settings =
                serde_json::from_value(serde_json::Value::Object(document.clone()))?;
            let hooks = global.hooks.get_or_insert_with(Default::default);
            let trust = hooks.trust.get_or_insert_with(Vec::new);
            trust.retain(|rule| rule.path != root);
            trust.push(HookTrustRule {
                path: root,
                trust: decision,
            });
            let trust = trust.clone();
            Self::resolve(&self.working_dir, global, state.project.clone())?;
            let hooks = document
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}));
            hooks
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("hooks must be a JSON object"))?
                .insert("trust".into(), serde_json::to_value(&trust)?);
            Ok(trust)
        })?;
        state
            .global
            .hooks
            .get_or_insert_with(Default::default)
            .trust = Some(trust.clone());
        state
            .merged
            .hooks
            .get_or_insert_with(Default::default)
            .trust = Some(trust);
        Ok(())
    }

    pub(crate) fn matches_scope(&self, working_dir: &Path) -> bool {
        settings::project_scope_dir_for(working_dir) == self.project_scope
    }

    pub(crate) fn reload(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (global, project) = settings::read_sources(&self.working_dir)?;
        *state = Self::resolve(&self.working_dir, global, project)?;
        Ok(())
    }

    pub(crate) fn settings(&self) -> Settings {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merged
            .clone()
    }

    pub(crate) fn candidate_for(
        &self,
        working_dir: &Path,
    ) -> anyhow::Result<(Settings, Arc<ResourcePolicy>)> {
        self.check_scope(working_dir)?;
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok((state.merged.clone(), self.policy_from(&state)?))
    }

    fn check_scope(&self, working_dir: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.matches_scope(working_dir),
            "settings manager scope does not match {}",
            working_dir.display()
        );
        Ok(())
    }

    pub(crate) fn resource_policy_for(
        &self,
        working_dir: &Path,
    ) -> anyhow::Result<Arc<ResourcePolicy>> {
        self.check_scope(working_dir)?;
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.policy_from(&state)
    }

    fn policy_from(&self, state: &SettingsState) -> anyhow::Result<Arc<ResourcePolicy>> {
        Ok(Arc::new(ResourcePolicy::from_settings(
            &self.working_dir,
            state.global.clone().into(),
            state.project.clone().into(),
        )?))
    }

    pub(crate) fn update_file<T>(
        path: &Path,
        update: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        // Persistence reads the latest document, not the accepted runtime configuration.
        settings::update_settings_file(path, update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_reload_reads_settings_and_updates_shared_consumers() {
        const CHILD_MARKER: &str = "PHI_SETTINGS_MANAGER_TEST";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let root =
                Path::new("/tmp").join(format!("phi-settings-manager-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "config::manager::tests::only_explicit_reload_reads_settings_and_updates_shared_consumers",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1")
                .env("PHI_CONFIG_DIR", root.join("global"))
                .current_dir(&root)
                .output()
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = std::env::current_dir().unwrap();
        let global = settings::settings_file_path();
        let project = root.join(".phi/settings.json");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(
            &global,
            r#"{"defaultModel":"global","skills":["+skills/global/SKILL.md"]}"#,
        )
        .unwrap();
        std::fs::write(&project, r#"{"defaultModel":"project"}"#).unwrap();
        let manager = SettingsManager::load_for(&root).unwrap();
        let consumer = manager.clone();
        let previous = manager.settings();
        std::fs::write(&global, "invalid JSON").unwrap();
        std::fs::write(&project, "invalid JSON").unwrap();
        assert_eq!(manager.settings().default_model(), Some("project"));
        assert!(consumer.resource_policy_for(&root).is_ok());
        assert!(manager.reload().is_err());
        assert_eq!(manager.settings().default_model(), Some("project"));
        std::fs::write(&global, r#"{"defaultModel":"refreshed"}"#).unwrap();
        std::fs::write(&project, "{}").unwrap();
        manager.reload().unwrap();
        assert_eq!(consumer.settings().default_model(), Some("refreshed"));
        assert_eq!(previous.default_model(), Some("project"));
    }

    #[test]
    fn hook_workspace_approval_persists_without_refreshing_settings() {
        const CHILD_MARKER: &str = "PHI_HOOK_TRUST_TEST";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let root = Path::new("/tmp").join(format!("phi-hook-trust-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "config::manager::tests::hook_workspace_approval_persists_without_refreshing_settings", "--nocapture"])
                .env(CHILD_MARKER, "1")
                .env("PHI_CONFIG_DIR", root.join("global"))
                .current_dir(&root).output().unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = std::env::current_dir().unwrap();
        let repository = root.join("repository");
        let nested = repository.join("src");
        let other = root.join("other");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        let path = settings::settings_file_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"defaultModel":"before"}"#).unwrap();
        let manager = SettingsManager::load_for(&repository).unwrap();
        let consumer = manager.clone();
        assert_eq!(manager.hook_workspace_trust(&nested).unwrap(), None);
        std::fs::write(&path, r#"{"hooks":{"trust":["relative"]}}"#).unwrap();
        assert!(
            manager
                .set_hook_workspace_trust(&nested, HookTrust::Trusted)
                .is_err()
        );
        assert_eq!(consumer.hook_workspace_trust(&nested).unwrap(), None);
        let latest = serde_json::json!({
            "defaultModel": "after",
            "hooks": {"trust": [format!("-{}", other.display())], "SessionStart": [{"hooks": [{"type": "command", "command": "echo changed"}]}]}
        });
        std::fs::write(&path, latest.to_string()).unwrap();
        for decision in [
            HookTrust::Trusted,
            HookTrust::Disabled,
            HookTrust::Trusted,
            HookTrust::Disabled,
        ] {
            manager.set_hook_workspace_trust(&nested, decision).unwrap();
            assert_eq!(
                consumer.hook_workspace_trust(&repository).unwrap(),
                Some(decision)
            );
            assert_eq!(
                SettingsManager::load_for(&nested)
                    .unwrap()
                    .hook_workspace_trust(&repository)
                    .unwrap(),
                Some(decision)
            );
        }
        assert_eq!(
            consumer.hook_workspace_trust(&other).unwrap(),
            Some(HookTrust::Disabled)
        );
        assert!(
            consumer
                .set_hook_workspace_trust(&other, HookTrust::Trusted)
                .is_err()
        );
        assert_eq!(consumer.settings().default_model(), Some("before"));
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["defaultModel"], latest["defaultModel"]);
        assert_eq!(
            persisted["hooks"]["SessionStart"],
            latest["hooks"]["SessionStart"]
        );
        assert_eq!(
            persisted["hooks"]["trust"],
            serde_json::json!([
                format!("-{}", other.display()),
                format!("-{}", repository.canonicalize().unwrap().display())
            ])
        );
    }

    #[test]
    fn empty_project_feature_blocks_override_enabled_global_settings() {
        use serde_json::json;

        let root = Path::new("/tmp").join(format!("phi-settings-{}", uuid::Uuid::new_v4()));
        let global: Settings = serde_json::from_value(json!({
            "codemode": {"inlineBudget": 17},
            "memory": {"tape": {"onlyGit": false}},
            "sandbox": {"whitelist": []}
        }))
        .unwrap();
        assert!(global.codemode.as_ref().unwrap().enabled);
        assert!(global.memory.as_ref().unwrap().tape_enabled());
        assert!(global.sandbox_enabled());
        for memory in [json!({}), json!({"tape": {}})] {
            let project: Settings = serde_json::from_value(json!({
                "codemode": {}, "memory": memory, "sandbox": {}
            }))
            .unwrap();
            let manager = SettingsManager::from_settings(&root, global.clone(), project).unwrap();
            let settings = manager.settings();
            assert!(!settings.codemode.unwrap().enabled);
            assert!(!settings.memory.unwrap().tape_enabled());
            assert_eq!(settings.sandbox.unwrap().enabled, Some(false));
        }
    }

    #[test]
    fn hook_workspace_trust_is_global_only() {
        let root = Path::new("/tmp").join(format!("phi-hook-trust-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let global: Settings = serde_json::from_value(serde_json::json!({
            "hooks": {"trust": [root], "SessionStart": [{"hooks": [
                {"type": "command", "command": "true"}
            ]}]}
        }))
        .unwrap();
        let manager =
            SettingsManager::from_settings(&root, global.clone(), Settings::default()).unwrap();
        assert!(!manager.settings().hooks.unwrap().is_empty());
        assert_eq!(
            manager.hook_workspace_trust(&root).unwrap(),
            Some(HookTrust::Trusted)
        );
        for rules in [vec!['+', '-'], vec!['-', '+']] {
            let global = serde_json::from_value(serde_json::json!({"hooks": {"trust": rules.into_iter().map(|prefix| format!("{prefix}{}", root.display())).collect::<Vec<_>>()}})).unwrap();
            let manager =
                SettingsManager::from_settings(&root, global, Settings::default()).unwrap();
            assert_eq!(
                manager.hook_workspace_trust(&root).unwrap(),
                Some(HookTrust::Disabled)
            );
        }
        for trust in [
            serde_json::json!([]),
            serde_json::json!([format!("-{}", root.display())]),
        ] {
            let project =
                serde_json::from_value(serde_json::json!({"hooks": {"trust": trust}})).unwrap();
            assert!(SettingsManager::from_settings(&root, global.clone(), project).is_err());
        }
        for trust in [
            serde_json::json!(["relative"]),
            serde_json::json!(["+relative"]),
            serde_json::json!(["-"]),
            serde_json::json!(["++/tmp/project"]),
            serde_json::json!(true),
            serde_json::json!([42]),
        ] {
            assert!(
                serde_json::from_value::<Settings>(serde_json::json!({"hooks": {"trust": trust}}))
                    .is_err()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_hooks_require_global_registration() {
        let root = Path::new("/tmp").join(format!("phi-hook-settings-{}", uuid::Uuid::new_v4()));
        let global: Settings = serde_json::from_value(serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "exit 2"}
            ]}]}
        }))
        .unwrap();
        let error =
            SettingsManager::from_settings(&root, Settings::default(), global.clone()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only supported in global settings")
        );
        let project: Settings = serde_json::from_value(serde_json::json!({"hooks": {}})).unwrap();
        let manager = SettingsManager::from_settings(&root, global, project).unwrap();
        assert!(!manager.settings().hooks.unwrap().is_empty());
    }

    #[test]
    fn shared_manager_keeps_sources_and_rejects_wrong_scope() {
        let root = Path::new("/tmp").join(format!("phi-settings-{}", uuid::Uuid::new_v4()));
        let manager = SettingsManager::from_settings(
            &root,
            serde_json::from_value(serde_json::json!({
                "defaultModel": "global", "skills": ["+skills/global/SKILL.md"]
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "defaultModel": "project", "skills": ["+skills/project/SKILL.md"]
            }))
            .unwrap(),
        )
        .unwrap();
        let state = manager.state.read().unwrap();
        assert_eq!(state.global.default_model(), Some("global"));
        assert_eq!(state.project.default_model(), Some("project"));
        drop(state);
        let mut settings = manager.settings();
        assert_eq!(settings.default_model(), Some("project"));
        settings.default_model = Some("edited".into());
        assert_eq!(manager.settings().default_model(), Some("project"));
        assert!(manager.resource_policy_for(&root.join("other")).is_err());
    }
}
