use std::collections::BTreeMap;

use anyhow::ensure;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub(crate) use super::super::store::Store;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Config {
    pub app_id: String,
    pub app_secret: String,
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allowed_groups: Vec<String>,
}

impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            valid_id(&self.app_id, "cli_")
                && !self.app_secret.is_empty()
                && self.app_secret.len() <= 256
                && self.app_secret.bytes().all(|byte| byte.is_ascii_graphic()),
            "invalid Feishu appId or appSecret"
        );
        ensure!(
            !self.allowed_users.is_empty()
                && self.allowed_users.iter().all(|id| valid_id(id, "ou_"))
                && self.allowed_groups.iter().all(|id| valid_id(id, "oc_")),
            "allowedUsers must contain open IDs (ou_); allowedGroups must contain chat IDs (oc_)"
        );
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct State {
    pub app_id: Option<String>,
    pub seen: BTreeMap<String, u64>,
    pub inflight: Option<String>,
    pub routes: BTreeMap<String, Route>,
    #[serde(default)]
    pub direct_chats: BTreeMap<String, String>,
    pub chats: BTreeMap<String, Binding>,
    pub outbox: Vec<Delivery>,
    pub sending: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Binding {
    pub session: String,
    pub cursor: Option<String>,
    pub waiting: bool,
    pub permission_notified: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Route {
    pub user: String,
    pub group: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Delivery {
    pub chat: String,
    pub text: String,
}

pub(crate) fn valid_id(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
        && value.len() > prefix.len()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
