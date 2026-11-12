use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::store::{Config, valid_id};

pub(crate) const API_SUCCESS: i64 = 0;
pub(crate) const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
pub(crate) const DEDUP_MILLIS: u64 = 24 * 60 * 60 * 1000;
const CLOCK_SKEW_MILLIS: u64 = 5 * 60 * 1000;
const TEXT_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
const PRESENTATION_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct Feishu {
    client: Client,
    base: String,
    app_id: String,
    app_secret: String,
    token: Arc<Mutex<Option<Token>>>,
    presentation_backoff: Arc<Mutex<Option<Instant>>>,
}

struct Token {
    value: String,
    expires: Instant,
}

impl Feishu {
    pub fn new(app_id: String, app_secret: String) -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .user_agent("phi-feishu/0.1")
                .build()?,
            base: "https://open.feishu.cn".into(),
            app_id,
            app_secret,
            token: Arc::new(Mutex::new(None)),
            presentation_backoff: Arc::new(Mutex::new(None)),
        })
    }

    #[cfg(test)]
    pub fn test_endpoint(endpoint: &str) -> Self {
        let mut api = Self::new("cli_test".into(), "test".into()).unwrap();
        api.base = endpoint.into();
        api
    }

    async fn access_token(&self) -> anyhow::Result<String> {
        let mut cached = self.token.lock().await;
        if let Some(token) = cached
            .as_ref()
            .filter(|token| token.expires > Instant::now())
        {
            return Ok(token.value.clone());
        }
        let response = self
            .client
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.base
            ))
            .json(&json!({"app_id":self.app_id,"app_secret":self.app_secret}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Feishu token request failed"))?;
        let value = body(response).await?;
        check_code(&value)?;
        let token = value["tenant_access_token"]
            .as_str()
            .context("missing Feishu access token")?;
        ensure!(
            !token.is_empty() && token.len() <= 4096,
            "invalid Feishu access token"
        );
        let lifetime = value["expire"]
            .as_u64()
            .context("missing Feishu token lifetime")?;
        ensure!(
            (120..=86400).contains(&lifetime),
            "invalid Feishu token lifetime"
        );
        *cached = Some(Token {
            value: token.into(),
            expires: Instant::now() + Duration::from_secs(lifetime) - TOKEN_MARGIN,
        });
        Ok(token.into())
    }

    pub async fn identity(&self) -> anyhow::Result<String> {
        let response = self
            .client
            .get(format!("{}/open-apis/bot/v3/info", self.base))
            .bearer_auth(self.access_token().await?)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Feishu bot identity request failed"))?;
        let value = body(response).await?;
        check_code(&value)?;
        let bot = value
            .get("bot")
            .or_else(|| value["data"].get("bot"))
            .context("missing Feishu bot identity")?;
        let id = bot["open_id"].as_str().context("missing bot open ID")?;
        ensure!(valid_id(id, "ou_"), "invalid bot open ID");
        Ok(id.into())
    }

    pub async fn endpoint(&self) -> anyhow::Result<Value> {
        let response = self
            .client
            .post(format!("{}/callback/ws/endpoint", self.base))
            .json(&json!({"AppID":self.app_id,"AppSecret":self.app_secret}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Feishu endpoint discovery failed"))?;
        body(response).await
    }

    pub async fn presentation_request(
        &self,
        method: Method,
        path: &str,
        payload: Value,
    ) -> anyhow::Result<Value> {
        ensure!(
            !self
                .presentation_backoff
                .lock()
                .await
                .is_some_and(|until| until > Instant::now()),
            "Feishu presentation backing off"
        );
        let response = self
            .client
            .request(method, format!("{}/open-apis/{path}", self.base))
            .bearer_auth(self.access_token().await?)
            .json(&payload)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Feishu presentation outcome unknown"))?;
        let limited = response.status() == StatusCode::TOO_MANY_REQUESTS;
        if limited {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(1, 3600);
            *self.presentation_backoff.lock().await =
                Some(Instant::now() + Duration::from_secs(seconds));
        }
        let result = body(response).await;
        let code = match &result {
            Ok(value) => value["code"].as_i64(),
            Err(error) => error
                .downcast_ref::<HttpRejection>()
                .and_then(|error| error.code),
        };
        if !limited && matches!(code, Some(99991400 | 99991401 | 99991403 | 429)) {
            *self.presentation_backoff.lock().await = Some(Instant::now() + PRESENTATION_BACKOFF);
        }
        let value = result?;
        check_code(&value)?;
        Ok(value)
    }

    pub async fn send_card(&self, recipient: &str, card: &str) -> anyhow::Result<()> {
        let target_type = receive_id_type(recipient)?;
        self.presentation_request(Method::POST, &format!("im/v1/messages?receive_id_type={target_type}"),
            json!({"receive_id":recipient,"msg_type":"interactive","content":json!({"type":"card","data":{"card_id":card}}).to_string()})).await?;
        Ok(())
    }

    pub async fn send(&self, recipient: &str, text: &str) -> anyhow::Result<()> {
        let target_type = receive_id_type(recipient)?;
        ensure!(text.len() <= TEXT_BYTES, "invalid Feishu delivery");
        let response = self.client.post(format!("{}/open-apis/im/v1/messages?receive_id_type={target_type}", self.base))
            .bearer_auth(self.access_token().await?)
            .json(&json!({"receive_id":recipient,"msg_type":"text","content":json!({"text":text}).to_string()}))
            .send().await.map_err(|_| anyhow::anyhow!("Feishu send outcome unknown; not retried"))?;
        check_code(&body(response).await.context("Feishu reply send failed")?)
    }
}

fn receive_id_type(recipient: &str) -> anyhow::Result<&'static str> {
    if valid_id(recipient, "ou_") {
        Ok("open_id")
    } else if valid_id(recipient, "oc_") {
        Ok("chat_id")
    } else {
        anyhow::bail!("invalid Feishu recipient")
    }
}

#[derive(Debug)]
struct HttpRejection {
    status: StatusCode,
    code: Option<i64>,
}

impl std::fmt::Display for HttpRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Feishu returned HTTP {}", self.status)?;
        if let Some(code) = self.code {
            write!(formatter, ", API code {code}")?;
        }
        formatter.write_str("; operation not retried")
    }
}

impl std::error::Error for HttpRejection {}

async fn body(mut response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("incomplete Feishu response"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_PAYLOAD_BYTES,
            "Feishu response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice::<Value>(&bytes);
    if status != StatusCode::OK {
        return Err(HttpRejection {
            status,
            code: value.as_ref().ok().and_then(|value| value["code"].as_i64()),
        }
        .into());
    }
    value.map_err(|_| anyhow::anyhow!("invalid Feishu response"))
}

fn check_code(value: &Value) -> anyhow::Result<()> {
    let code = value["code"]
        .as_i64()
        .context("missing Feishu response code")?;
    ensure!(
        code == API_SUCCESS,
        "Feishu API rejected request (code {code}); not retried"
    );
    Ok(())
}

pub(crate) fn now_millis() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub(crate) fn split_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.len() + character.len_utf8() > TEXT_BYTES {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Deserialize)]
pub(crate) struct Envelope {
    pub schema: String,
    pub header: Header,
    pub event: Value,
}

#[derive(Deserialize)]
pub(crate) struct Header {
    pub event_id: String,
    pub event_type: String,
    pub app_id: String,
}

#[derive(Deserialize)]
pub(crate) struct MessageEvent {
    pub sender: Sender,
    pub message: ChatMessage,
}

#[derive(Deserialize)]
pub(crate) struct Sender {
    pub sender_id: Identity,
    pub sender_type: String,
}

#[derive(Deserialize)]
pub(crate) struct Identity {
    #[serde(default)]
    pub open_id: String,
}

#[derive(Deserialize)]
pub(crate) struct ChatMessage {
    pub message_id: String,
    pub chat_id: String,
    pub chat_type: String,
    pub message_type: String,
    pub content: String,
    pub create_time: String,
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

#[derive(Deserialize)]
pub(crate) struct Mention {
    pub key: String,
    pub id: Identity,
}

impl MessageEvent {
    pub fn authorize(&self, config: &Config, bot: &str, now: u64) -> Result<(), &'static str> {
        let message = &self.message;
        let user = &self.sender.sender_id.open_id;
        let created = message
            .create_time
            .parse::<u64>()
            .map_err(|_| "invalid message timestamp")?;
        if self.sender.sender_type != "user" || user == bot {
            return Err("sender is not a human user");
        }
        if !config.allowed_users.contains(user) {
            return Err("sender not in allowedUsers; use this application's user open ID");
        }
        if !valid_id(user, "ou_")
            || !valid_id(&message.chat_id, "oc_")
            || !valid_id(&message.message_id, "om_")
        {
            return Err("unsupported user, chat or message ID format");
        }
        if created <= now.saturating_sub(DEDUP_MILLIS) {
            return Err("message older than 24 hours; check host clock and message timestamp");
        }
        if created > now.saturating_add(CLOCK_SKEW_MILLIS) {
            return Err("message timestamp is in the future; check host clock");
        }
        match message.chat_type.as_str() {
            "p2p" => Ok(()),
            "group" => {
                if !config.allowed_groups.contains(&message.chat_id) {
                    return Err("group not in allowedGroups");
                }
                let mentioned = serde_json::from_str::<Value>(&message.content)
                    .ok()
                    .is_some_and(|content| {
                        content["text"].as_str().is_some_and(|text| {
                            message.mentions.iter().any(|mention| {
                                mention.id.open_id == bot
                                    && !mention.key.is_empty()
                                    && text.contains(&mention.key)
                            })
                        })
                    });
                if !mentioned {
                    return Err("group message does not @mention this bot");
                }
                Ok(())
            }
            _ => Err("unsupported chat type"),
        }
    }

    pub fn text(&self, bot: &str) -> anyhow::Result<Option<String>> {
        if self.message.message_type != "text" {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&self.message.content)
            .map_err(|_| anyhow::anyhow!("invalid Feishu text content"))?;
        let mut text = value["text"]
            .as_str()
            .context("missing text content")?
            .to_owned();
        ensure!(text.len() <= TEXT_BYTES, "Feishu input exceeds text limit");
        for mention in &self.message.mentions {
            if mention.id.open_id == bot && !mention.key.is_empty() {
                text = text.replace(&mention.key, "");
            }
        }
        Ok(Some(text.trim().into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn config() -> Config {
        Config {
            app_id: "cli_test".into(),
            app_secret: "test".into(),
            allowed_users: vec!["ou_user".into()],
            allowed_groups: vec!["oc_group".into()],
        }
    }

    fn event(chat_type: &str) -> MessageEvent {
        serde_json::from_value(json!({"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
            "message":{"message_id":"om_test","chat_id":"oc_group","chat_type":chat_type,
            "message_type":"text","create_time":"100000000","content":"{\"text\":\"@_user_1 hello\"}",
            "mentions":[{"key":"@_user_1","id":{"open_id":"ou_bot"}}]}})).unwrap()
    }

    #[test]
    fn authorization_requires_sender_group_actual_bot_mention_and_fresh_message() {
        let config = config();
        let now = 100000000;
        let mut event = event("group");
        assert!(event.authorize(&config, "ou_bot", now).is_ok());
        assert_eq!(event.text("ou_bot").unwrap().as_deref(), Some("hello"));
        event.message.mentions[0].id.open_id = "ou_other".into();
        assert!(event.authorize(&config, "ou_bot", now).is_err());
        event.message.chat_type = "p2p".into();
        assert!(event.authorize(&config, "ou_bot", now).is_ok());
        event.sender.sender_type = "app".into();
        assert!(event.authorize(&config, "ou_bot", now).is_err());
        event.sender.sender_type = "user".into();
        event.sender.sender_id.open_id = "ou_intruder".into();
        assert!(event.authorize(&config, "ou_bot", now).is_err());
        event.sender.sender_id.open_id = "ou_user".into();
        assert!(
            event
                .authorize(&config, "ou_bot", now + DEDUP_MILLIS)
                .is_err()
        );
        assert!(
            event
                .authorize(&config, "ou_bot", now - CLOCK_SKEW_MILLIS - 1)
                .is_err()
        );
        event.message.chat_type = "group".into();
        event.message.mentions[0].id.open_id = "ou_bot".into();
        event.message.chat_id = "oc_unlisted".into();
        assert!(event.authorize(&config, "ou_bot", now).is_err());
    }

    #[tokio::test]
    async fn http_errors_report_api_codes_without_exposing_response_content() {
        let app = axum::Router::new()
            .route(
                "/permission",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({
                            "code":99991672,"msg":"private-response-secret",
                            "error":{"details":"private-response-secret"}
                        })),
                    )
                }),
            )
            .route(
                "/invalid",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::BAD_GATEWAY,
                        "private-response-secret",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Client::new();
        for (path, expected) in [
            ("permission", "API code 99991672"),
            ("invalid", "502 Bad Gateway"),
        ] {
            let response = client
                .get(format!("{endpoint}/{path}"))
                .send()
                .await
                .unwrap();
            let error = body(response).await.unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private-response-secret"));
            assert!(error.contains("not retried"));
        }
        server.abort();
    }

    #[tokio::test]
    async fn presentation_rate_limits_back_off_without_blocking_ordinary_delivery() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (code, status) in [
            (99991400, StatusCode::OK),
            (99991403, StatusCode::BAD_REQUEST),
            (429, StatusCode::TOO_MANY_REQUESTS),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = requests.clone();
            let app = axum::Router::new()
                .route(
                    "/open-apis/auth/v3/tenant_access_token/internal",
                    axum::routing::post(|| async {
                        axum::Json(json!({"code":0,"tenant_access_token":"token","expire":7200}))
                    }),
                )
                .route(
                    "/open-apis/limited",
                    axum::routing::post(move || {
                        let observed = observed.clone();
                        async move {
                            observed.fetch_add(1, Ordering::SeqCst);
                            (
                                status,
                                axum::Json(json!({"code":code,"msg":"private error"})),
                            )
                        }
                    }),
                )
                .route(
                    "/open-apis/im/v1/messages",
                    axum::routing::post(|| async { axum::Json(json!({"code":0})) }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let api = Feishu::test_endpoint(&format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            for _ in 0..2 {
                let error = api
                    .presentation_request(Method::POST, "limited", json!({}))
                    .await
                    .unwrap_err();
                assert!(!error.to_string().contains("private error"));
            }
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            api.send("oc_chat", "ordinary fallback").await.unwrap();
            server.abort();
        }
    }

    #[test]
    fn rejection_reasons_are_actionable_and_exclude_event_content() {
        let config = config();
        let now = 100000000;
        let mut event = event("p2p");
        event.sender.sender_id.open_id = "ou_private_identifier".into();
        event.message.content = "{\"text\":\"private message and password\"}".into();
        assert_eq!(
            event.authorize(&config, "ou_bot", now),
            Err("sender not in allowedUsers; use this application's user open ID")
        );
        event.sender.sender_id.open_id = "ou_user".into();
        event.message.create_time = "not-a-time".into();
        assert_eq!(
            event.authorize(&config, "ou_bot", now),
            Err("invalid message timestamp")
        );
        event.message.create_time = now.to_string();
        event.message.chat_type = "group".into();
        event.message.chat_id = "oc_not_allowed".into();
        assert_eq!(
            event.authorize(&config, "ou_bot", now),
            Err("group not in allowedGroups")
        );
        event.message.chat_id = "oc_group".into();
        assert_eq!(
            event.authorize(&config, "ou_bot", now),
            Err("group message does not @mention this bot")
        );
    }

    proptest! {
        #[test]
        fn splitting_preserves_unicode_and_bounds(chunks in prop::collection::vec(any::<char>(), 0..40000)) {
            let text: String = chunks.into_iter().collect();
            let parts = split_text(&text);
            prop_assert!(parts.iter().all(|part| !part.is_empty() && part.len() <= TEXT_BYTES));
            prop_assert_eq!(parts.concat(), text);
        }
    }
}
