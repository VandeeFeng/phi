use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use reqwest::Client;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

pub(crate) const MESSAGE_UNITS: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const FLOOD_BUDGET: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ERROR_CHARS: usize = 1024;

#[derive(Clone)]
pub(crate) struct Telegram {
    client: Client,
    token: String,
    base: String,
}

#[derive(Deserialize)]
pub(crate) struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Deserialize)]
pub(crate) struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Deserialize)]
pub(crate) struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<Callback>,
}

impl Update {
    pub fn authorized_chat(&self, allowed: &[i64]) -> Option<i64> {
        let (sender, chat) = if let Some(callback) = &self.callback_query {
            (&callback.from, &callback.message.as_ref()?.chat)
        } else {
            let message = self.message.as_ref()?;
            (message.from.as_ref()?, &message.chat)
        };
        (chat.kind == "private"
            && chat.id == sender.id
            && !sender.is_bot
            && allowed.contains(&sender.id))
        .then_some(chat.id)
    }
}

#[derive(Debug)]
pub(crate) struct ApiFailure {
    pub code: u16,
    pub retry_after: Option<Duration>,
    method: String,
    description: String,
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Telegram {} rejected request ({}): {}",
            self.method, self.code, self.description
        )
    }
}
impl std::error::Error for ApiFailure {}

impl Telegram {
    pub fn new(token: String) -> anyhow::Result<Self> {
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token,
            base: "https://api.telegram.org".into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_endpoint(token: String, base: String) -> anyhow::Result<Self> {
        let mut telegram = Self::new(token)?;
        telegram.base = base;
        Ok(telegram)
    }

    pub async fn identity(&self) -> anyhow::Result<User> {
        self.call("getMe", json!({})).await
    }

    pub async fn updates(&self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let body = json!({"offset":offset,"timeout":POLL_TIMEOUT.as_secs(),"limit":20,"allowed_updates":["message","callback_query"]});
        let result = self
            .request("getUpdates", &body, POLL_REQUEST_TIMEOUT)
            .await?;
        serde_json::from_value(result).context("invalid Telegram updates")
    }

    pub async fn send(&self, chat: i64, text: &str, buttons: Option<Value>) -> anyhow::Result<i64> {
        ensure!(
            !text.is_empty() && text.encode_utf16().count() <= MESSAGE_UNITS,
            "invalid Telegram message length"
        );
        let mut body =
            json!({"chat_id":chat,"text":text,"link_preview_options":{"is_disabled":true}});
        if let Some(buttons) = buttons {
            body["reply_markup"] = buttons;
        }
        let message: Message = self.call("sendMessage", body).await?;
        Ok(message.message_id)
    }

    pub async fn answer(&self, callback: &str, text: &str) -> anyhow::Result<()> {
        let _: bool = self
            .call(
                "answerCallbackQuery",
                json!({"callback_query_id":callback,"text":text}),
            )
            .await?;
        Ok(())
    }

    pub async fn clear_buttons(&self, chat: i64, message: i64) -> anyhow::Result<()> {
        let _: Value = self
            .call(
                "editMessageReplyMarkup",
                json!({"chat_id":chat,"message_id":message,"reply_markup":{"inline_keyboard":[]}}),
            )
            .await?;
        Ok(())
    }

    pub async fn typing(&self, chat: i64) -> anyhow::Result<()> {
        self.feedback("sendChatAction", json!({"chat_id":chat,"action":"typing"}))
            .await
    }

    pub async fn react(&self, chat: i64, message: i64, emoji: Option<&str>) -> anyhow::Result<()> {
        let reaction: Vec<_> = emoji
            .map(|emoji| json!({"type":"emoji","emoji":emoji}))
            .into_iter()
            .collect();
        self.feedback(
            "setMessageReaction",
            json!({"chat_id":chat,"message_id":message,"reaction":reaction}),
        )
        .await
    }

    async fn feedback(&self, method: &str, body: Value) -> anyhow::Result<()> {
        self.request(method, &body, FEEDBACK_TIMEOUT).await?;
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        body: &Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let mut response = self
            .client
            .post(format!("{}/bot{}/{method}", self.base, self.token))
            .timeout(timeout)
            .json(body)
            .send()
            .await
            .map_err(|error| self.transport_error(error))
            .context("Telegram transport failed; delivery outcome may be unknown")?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.transport_error(error))
            .context("incomplete Telegram response")?
        {
            ensure!(
                bytes.len() + chunk.len() <= MAX_RESPONSE_BYTES,
                "Telegram response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let mut envelope: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if !status.is_success() || envelope["ok"] != true {
            return Err(self.api_failure(method, status, &envelope).into());
        }
        Ok(envelope["result"].take())
    }

    fn api_failure(
        &self,
        method: &str,
        status: reqwest::StatusCode,
        envelope: &Value,
    ) -> ApiFailure {
        let description = envelope["description"]
            .as_str()
            .unwrap_or("no error description")
            .replace(&self.token, "[redacted]")
            .chars()
            .take(MAX_ERROR_CHARS)
            .flat_map(char::escape_debug)
            .collect();
        ApiFailure {
            code: envelope["error_code"]
                .as_u64()
                .and_then(|code| u16::try_from(code).ok())
                .unwrap_or(status.as_u16()),
            retry_after: envelope["parameters"]["retry_after"]
                .as_u64()
                .map(|seconds| Duration::from_secs(seconds.max(1))),
            method: method.to_owned(),
            description,
        }
    }

    fn transport_error(&self, error: reqwest::Error) -> anyhow::Error {
        let diagnostic = format!("{:#}", anyhow::Error::new(error.without_url()));
        anyhow::anyhow!(diagnostic.replace(&self.token, "[redacted]"))
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> anyhow::Result<T> {
        let deadline = Instant::now() + FLOOD_BUDGET;
        loop {
            let error = match self.request(method, &body, REQUEST_TIMEOUT).await {
                Ok(result) => {
                    return serde_json::from_value(result).context("invalid Telegram response");
                }
                Err(error) => error,
            };
            if let Some(failure) = error.downcast_ref::<ApiFailure>()
                && failure.code == reqwest::StatusCode::TOO_MANY_REQUESTS.as_u16()
            {
                let delay = failure.retry_after.unwrap_or(Duration::from_secs(1));
                if delay <= deadline.saturating_duration_since(Instant::now()) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            return Err(error);
        }
    }
}

pub(crate) fn split_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut units = 0;
    for (index, character) in text.char_indices() {
        if units + character.len_utf16() > MESSAGE_UNITS {
            chunks.push(text[start..index].to_owned());
            start = index;
            units = 0;
        }
        units += character.len_utf16();
    }
    if start < text.len() {
        chunks.push(text[start..].to_owned());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
        #[test]
        fn unicode_chunks_round_trip(text in ".{0,12000}") {
            let chunks = split_text(&text);
            prop_assert_eq!(chunks.concat(), text);
            prop_assert!(chunks.iter().all(|chunk| !chunk.is_empty() && chunk.encode_utf16().count() <= MESSAGE_UNITS));
        }
    }

    #[tokio::test]
    async fn rejections_do_not_retry() {
        use reqwest::StatusCode;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        for (status, accepted) in [
            (StatusCode::BAD_REQUEST, false),
            (StatusCode::OK, false),
            (StatusCode::BAD_GATEWAY, true),
        ] {
            let requests = Arc::new(AtomicUsize::new(0));
            let recorded = requests.clone();
            let app = axum::Router::new().fallback(move || {
                recorded.fetch_add(1, Ordering::SeqCst);
                async move {
                    (
                        status,
                        axum::Json(json!({
                            "ok":accepted, "error_code":400,
                            "description":"Bad Request: test-bot-token invalid\nmessage",
                            "result":{"message_id":1,"chat":{"id":7,"type":"private"}}
                        })),
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let telegram = Telegram::test_endpoint("test-bot-token".into(), endpoint).unwrap();
            let error = telegram.send(7, "hello", None).await.unwrap_err();
            let diagnostic = format!("{error:#}");
            assert!(diagnostic.contains("sendMessage"), "{diagnostic}");
            assert!(diagnostic.contains("Bad Request:"), "{diagnostic}");
            assert!(diagnostic.contains("[redacted]"), "{diagnostic}");
            assert!(!diagnostic.contains("test-bot-token"));
            assert!(!diagnostic.contains('\n'));
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            serving.abort();
        }
    }

    #[tokio::test]
    async fn rate_limit_retries_delivery() {
        use std::sync::{Arc, Mutex};

        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let recorded = deliveries.clone();
        let app = axum::Router::new().fallback(move |axum::Json(body): axum::Json<Value>| {
            let recorded = recorded.clone();
            async move {
                let mut deliveries = recorded.lock().unwrap();
                deliveries.push(body);
                if deliveries.len() == 1 {
                    (
                        reqwest::StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(json!({
                            "ok":false,"error_code":429,"parameters":{"retry_after":1}
                        })),
                    )
                } else {
                    (
                        reqwest::StatusCode::OK,
                        axum::Json(json!({
                            "ok":true,"result":{"message_id":10,"chat":{"id":7,"type":"private"}}
                        })),
                    )
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let telegram = Telegram::test_endpoint("test".into(), endpoint).unwrap();
        let started = Instant::now();
        assert_eq!(telegram.send(7, "hello", None).await.unwrap(), 10);
        assert!(started.elapsed() >= Duration::from_secs(1));
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0], deliveries[1]);
        serving.abort();
    }

    #[tokio::test]
    async fn transport_failure_reports_cause_without_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let token = "sensitive-bot-token";
        let telegram = Telegram::test_endpoint(token.into(), endpoint).unwrap();
        let error = telegram.updates(0).await.err().unwrap();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("connect"), "{diagnostic}");
        assert!(!diagnostic.contains(token));
        assert!(!diagnostic.contains("/bot"));
    }

    #[test]
    fn only_numeric_allowlisted_private_senders_are_authorized() {
        let base = json!({"update_id":1,"message":{"message_id":1,"chat":{"id":7,"type":"private"},"from":{"id":7},"text":"hi"}});
        let update: Update = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(update.authorized_chat(&[7]), Some(7));
        assert_eq!(update.authorized_chat(&[8]), None);
        for (pointer, value) in [
            ("/message/chat/type", json!("group")),
            ("/message/from/id", json!(8)),
            ("/message/from/is_bot", json!(true)),
        ] {
            let mut invalid = base.clone();
            if pointer.ends_with("is_bot") {
                invalid["message"]["from"]["is_bot"] = value;
            } else {
                *invalid.pointer_mut(pointer).unwrap() = value;
            }
            assert_eq!(
                serde_json::from_value::<Update>(invalid)
                    .unwrap()
                    .authorized_chat(&[7, 8]),
                None
            );
        }
        let callback = json!({"update_id":2,"callback_query":{"id":"x","from":{"id":8},"message":base["message"],"data":"p:allow:token"}});
        assert_eq!(
            serde_json::from_value::<Update>(callback)
                .unwrap()
                .authorized_chat(&[7, 8]),
            None
        );
    }
}
