use std::collections::BTreeMap;

use anyhow::ensure;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub(crate) use super::super::store::Store;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Config {
    pub bot_token: String,
    pub allowed_users: Vec<i64>,
}

impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.bot_token.is_empty()
                && self
                    .bot_token
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-')),
            "invalid Telegram bot token"
        );
        ensure!(
            !self.allowed_users.is_empty() && self.allowed_users.iter().all(|id| *id > 0),
            "allowedUsers must contain numeric positive user IDs"
        );
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct State {
    pub bot_id: Option<i64>,
    pub offset: i64,
    pub inflight: Option<i64>,
    pub chats: BTreeMap<i64, Binding>,
    pub outbox: Vec<Delivery>,
    pub sending: Option<i64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Binding {
    pub session: String,
    pub cursor: Option<String>,
    pub waiting: bool,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Question {
    pub agent: String,
    pub request: String,
    pub message_id: Option<i64>,
    pub can_allow: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Delivery {
    pub chat: i64,
    pub text: String,
    pub question: Option<String>,
}
