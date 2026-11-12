use anyhow::{Context, ensure};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{FeishuConfig, Plugin, PluginRegistry, TelegramConfig, feishu, telegram, web};

const WEB: &str = "builtin:web";
const TELEGRAM: &str = "builtin:telegram";
const FEISHU: &str = "builtin:feishu";

#[derive(Default, Deserialize)]
#[serde(try_from = "Vec<String>")]
pub(crate) struct Selection {
    enabled: HashSet<String>,
}

impl TryFrom<Vec<String>> for Selection {
    type Error = anyhow::Error;

    fn try_from(entries: Vec<String>) -> anyhow::Result<Self> {
        let mut selection = Self::default();
        for entry in entries {
            let name = entry.strip_prefix(['+', '-']).unwrap_or(&entry);
            ensure!(
                [WEB, TELEGRAM, FEISHU].contains(&name),
                "unknown builtin remote plugin"
            );
            if entry.starts_with('-') {
                selection.enabled.remove(name);
            } else {
                selection.enabled.insert(name.to_owned());
            }
        }
        Ok(selection)
    }
}

pub(crate) struct Configuration {
    selection: Selection,
    telegram: Option<TelegramConfig>,
    feishu: Option<FeishuConfig>,
}

impl Configuration {
    pub(crate) fn new(
        selection: Selection,
        telegram: Option<TelegramConfig>,
        feishu: Option<FeishuConfig>,
    ) -> anyhow::Result<Self> {
        if let Some(config) = &telegram {
            config.validate()?;
        }
        if let Some(config) = &feishu {
            config.validate()?;
        }
        if selection.enabled.contains(TELEGRAM) {
            ensure!(telegram.is_some(), "telegram configuration is required");
        }
        if selection.enabled.contains(FEISHU) {
            ensure!(feishu.is_some(), "feishu configuration is required");
        }
        ensure!(
            cfg!(unix)
                || !selection
                    .enabled
                    .iter()
                    .any(|name| name == TELEGRAM || name == FEISHU),
            "remote messaging plugins require Unix"
        );
        Ok(Self {
            selection,
            telegram,
            feishu,
        })
    }

    pub(crate) fn register(
        self,
        directory: &Path,
        assets: PathBuf,
    ) -> anyhow::Result<PluginRegistry> {
        let mut registry = PluginRegistry::new();
        if self.selection.enabled.contains(WEB) {
            registry.register(WEB, Plugin::new().with_routes(web::router(assets)))?;
        }
        #[cfg(unix)]
        {
            if self.selection.enabled.contains(TELEGRAM) || self.selection.enabled.contains(FEISHU)
            {
                super::store::prepare_directory(&directory.join("plugins"))?;
            }
            if self.selection.enabled.contains(TELEGRAM) {
                let config = self
                    .telegram
                    .context("telegram configuration is required")?;
                let directory = directory.join("plugins/telegram");
                super::store::Store::open(&directory)?.workspace()?;
                registry.register(
                    TELEGRAM,
                    Plugin::new().with_task(move |context| {
                        let config = config.clone();
                        let directory = directory.clone();
                        async move { telegram::run(config, &directory, context).await }
                    }),
                )?;
            }
            if self.selection.enabled.contains(FEISHU) {
                let config = self.feishu.context("feishu configuration is required")?;
                let directory = directory.join("plugins/feishu");
                super::store::Store::open(&directory)?.workspace()?;
                registry.register(
                    FEISHU,
                    Plugin::new().with_task(move |context| {
                        let config = config.clone();
                        let directory = directory.clone();
                        async move { feishu::run(config, &directory, context).await }
                    }),
                )?;
            }
        }
        #[cfg(not(unix))]
        let _ = (directory, self.telegram, self.feishu);
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn builtin_storage_is_flat_with_one_lock_per_plugin() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let selection = Selection::try_from(vec![TELEGRAM.into(), FEISHU.into()]).unwrap();
        let config = Configuration::new(
            selection,
            Some(
                serde_json::from_value(serde_json::json!({"botToken":"test","allowedUsers":[1]}))
                    .unwrap(),
            ),
            Some(
                serde_json::from_value(serde_json::json!({
                    "appId":"cli_test","appSecret":"test-secret","allowedUsers":["ou_user"]
                }))
                .unwrap(),
            ),
        )
        .unwrap();
        let registry = config
            .register(directory.path(), directory.path().join("assets"))
            .unwrap();
        let plugins_directory = directory.path().join("plugins");
        assert!(!plugins_directory.join("bridge.lock").exists());
        let (shutdown, closed) = tokio::sync::watch::channel(false);
        let (scheduler, scheduler_task) = crate::gateway::scheduler::test_scheduler(closed.clone());
        let mut tasks = super::super::PluginTasks::new(closed, scheduler);
        tasks.start(registry, directory.path(), "http://127.0.0.1:1/");
        for _ in 0..2 {
            tasks.next().await;
        }
        assert!(tasks.drain().await.is_empty());
        shutdown.send(true).unwrap();
        scheduler_task.await.unwrap();
        for plugin in ["telegram", "feishu"] {
            let path = plugins_directory.join(plugin);
            assert!(path.join("device.json").is_file(), "{plugin}");
            assert!(path.join("workspace").is_dir(), "{plugin}");
            assert!(path.join("bridge.lock").is_file(), "{plugin}");
            assert!(!path.join("gateway").exists(), "{plugin}");
            assert!(super::super::store::Store::open(&path).is_ok());
        }
    }

    proptest! {
        #[test]
        fn selection_defaults_and_order(selections in prop::collection::vec((0usize..3, 0usize..3), 0..64)) {
            let names = [WEB, TELEGRAM, FEISHU];
            let prefixes = ["+", "-", ""];
            let entries = selections.iter()
                .map(|&(plugin, action)| format!("{}{}", prefixes[action], names[plugin]))
                .collect::<Vec<_>>();
            let selection = Selection::try_from(entries).unwrap();
            for (index, name) in names.into_iter().enumerate() {
                let expected = selections.iter().rev()
                    .find(|&&(selected, _)| selected == index)
                    .is_some_and(|&(_, action)| action != 1);
                prop_assert_eq!(selection.enabled.contains(name), expected);
            }
        }
    }
}
