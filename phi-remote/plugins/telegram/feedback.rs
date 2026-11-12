use std::time::Duration;

use tokio::task::JoinHandle;

use super::api::Telegram;

const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const MAX_TYPING_FAILURES: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaction {
    Queued,
    Working,
    Waiting,
    Done,
    Error,
    Stopped,
    Clear,
}

impl Reaction {
    pub fn emoji(self) -> Option<&'static str> {
        match self {
            Self::Queued => Some("\u{1f440}"),
            Self::Working => Some("\u{1f914}"),
            Self::Waiting => Some("\u{270d}"),
            Self::Done => Some("\u{1f44d}"),
            Self::Error => Some("\u{1f631}"),
            Self::Stopped => Some("\u{1f634}"),
            Self::Clear => None,
        }
    }
}

pub(crate) struct Feedback {
    message: i64,
    reaction: Option<Reaction>,
    typing: JoinHandle<()>,
}

impl Feedback {
    pub async fn start(telegram: &Telegram, chat: i64, message: i64) -> Self {
        let client = telegram.clone();
        let typing = tokio::spawn(async move {
            let mut interval = tokio::time::interval(TYPING_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut failures = 0;
            loop {
                interval.tick().await;
                match client.typing(chat).await {
                    Ok(()) => failures = 0,
                    Err(error) => {
                        failures += 1;
                        eprintln!("Telegram typing feedback failed: {error:#}");
                        if failures >= MAX_TYPING_FAILURES {
                            break;
                        }
                    }
                }
            }
        });
        let mut feedback = Self {
            message,
            reaction: None,
            typing,
        };
        feedback.update(telegram, chat, Reaction::Queued).await;
        feedback
    }

    pub async fn update(&mut self, telegram: &Telegram, chat: i64, reaction: Reaction) {
        if self.reaction == Some(reaction) {
            return;
        }
        // Cosmetic failures must not interrupt execution or retry on every snapshot.
        self.reaction = Some(reaction);
        if let Err(error) = telegram.react(chat, self.message, reaction.emoji()).await {
            eprintln!("Telegram reaction feedback failed: {error:#}");
        }
    }

    pub fn stop_typing(&self) {
        self.typing.abort();
    }

    pub async fn shutdown(&mut self) {
        self.typing.abort();
        let _ = (&mut self.typing).await;
    }
}

impl Drop for Feedback {
    fn drop(&mut self) {
        self.stop_typing();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, extract::State};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn typing_starts_and_is_aborted_when_feedback_is_dropped() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/bottest/sendChatAction",
                axum::routing::post(
                    |State(sender): State<tokio::sync::mpsc::UnboundedSender<Value>>,
                     Json(body): Json<Value>| async move {
                        sender.send(body).unwrap();
                        Json(json!({"ok":true,"result":true}))
                    },
                ),
            )
            .fallback(|| async { Json(json!({"ok":true,"result":true})) })
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let telegram = Telegram::test_endpoint("test".into(), endpoint).unwrap();
        let feedback = Feedback::start(&telegram, 7, 10).await;
        let action = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(action, json!({"chat_id":7,"action":"typing"}));
        let typing = feedback.typing.abort_handle();
        drop(feedback);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !typing.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        serving.abort();
    }
}
