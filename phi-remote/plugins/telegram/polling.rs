use std::time::Duration;

use tokio::time::Instant;

use super::super::backoff::Backoff;
use super::api::{ApiFailure, Telegram, Update};

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
const EMPTY_INITIAL: Duration = Duration::from_millis(50);
const EMPTY_MAX: Duration = Duration::from_secs(1);

pub(super) struct Poller {
    telegram: Telegram,
    empty_backoff: Backoff,
    empty_seen: bool,
    next_poll: Instant,
}

impl Poller {
    pub(super) fn new(telegram: Telegram) -> Self {
        Self {
            telegram,
            empty_backoff: Backoff::new(EMPTY_INITIAL, EMPTY_MAX),
            empty_seen: false,
            next_poll: Instant::now(),
        }
    }

    pub(super) async fn next(&mut self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let mut retry = Backoff::new(RETRY_INITIAL, RETRY_MAX);
        loop {
            tokio::time::sleep_until(self.next_poll).await;
            let started = Instant::now();
            match self.telegram.updates(offset).await {
                Ok(updates) => {
                    let delay = if updates.is_empty() {
                        let delay = if self.empty_seen {
                            self.empty_backoff.next_delay()
                        } else {
                            Duration::ZERO
                        };
                        self.empty_seen = true;
                        delay
                    } else {
                        self.empty_seen = false;
                        self.empty_backoff.reset();
                        Duration::ZERO
                    };
                    self.next_poll = started + delay;
                    return Ok(updates);
                }
                Err(error) => {
                    let failure = error.downcast_ref::<ApiFailure>();
                    if failure.is_some_and(|failure| {
                        let status = reqwest::StatusCode::from_u16(failure.code).ok();
                        status.is_some_and(|status| {
                            status.is_client_error()
                                && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                                && status != reqwest::StatusCode::REQUEST_TIMEOUT
                        })
                    }) {
                        return Err(error.context("check bot token, webhook configuration and competing polling processes"));
                    }
                    let delay = failure
                        .and_then(|failure| failure.retry_after)
                        .unwrap_or_else(|| retry.next_delay());
                    self.empty_seen = false;
                    self.empty_backoff.reset();
                    eprintln!(
                        "Telegram polling failed: {error:#}; retrying read in {} seconds",
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;
                    self.next_poll = Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct Fixture {
        poller: Poller,
        requests: Arc<Mutex<Vec<Instant>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new(responses: Vec<Value>) -> Self {
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let app = axum::Router::new().fallback(move |axum::Json(body): axum::Json<Value>| {
                let responses = responses.clone();
                let recorded = recorded.clone();
                async move {
                    assert_eq!(body["timeout"], 30);
                    assert_eq!(body["offset"], 42);
                    recorded.lock().unwrap().push(Instant::now());
                    axum::Json(
                        responses
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("unexpected poll"),
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                poller: Poller::new(Telegram::test_endpoint("test".into(), endpoint).unwrap()),
                requests,
                task,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn empty() -> Value {
        json!({"ok":true,"result":[]})
    }

    #[tokio::test]
    async fn errors_back_off_and_success_resets_retry() {
        let error = json!({"ok":false,"error_code":503});
        let mut fixture =
            Fixture::new(vec![error.clone(), error.clone(), empty(), error, empty()]).await;
        fixture.poller.next(42).await.unwrap();
        fixture.poller.next(42).await.unwrap();
        let requests = fixture.requests.lock().unwrap();
        assert!(requests[1] - requests[0] >= RETRY_INITIAL);
        assert!(requests[2] - requests[1] >= RETRY_INITIAL * 2);
        assert!(requests[4] - requests[3] >= RETRY_INITIAL);
        assert!(requests[4] - requests[3] < RETRY_INITIAL * 2);
    }

    #[tokio::test]
    async fn rate_limit_is_honored_and_authentication_failure_is_terminal() {
        let mut fixture = Fixture::new(vec![
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":2}}),
            empty(),
            json!({"ok":false,"error_code":401}),
        ])
        .await;
        fixture.poller.next(42).await.unwrap();
        assert!(fixture.poller.next(42).await.is_err());
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1] - requests[0] >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn fast_empty_polls_are_throttled_and_messages_reset_them() {
        let update =
            json!({"ok":true,"result":[{"update_id":42,"message":null,"callback_query":null}]});
        let mut fixture = Fixture::new(vec![empty(), empty(), empty(), update, empty()]).await;
        for _ in 0..5 {
            fixture.poller.next(42).await.unwrap();
        }
        let requests = fixture.requests.lock().unwrap();
        assert!(requests[2] - requests[1] >= EMPTY_INITIAL);
        assert!(requests[3] - requests[2] >= EMPTY_INITIAL * 2);
        assert!(!fixture.poller.empty_seen || fixture.poller.next_poll <= Instant::now());
    }

    #[tokio::test]
    async fn cancelling_backoff_stops_further_requests() {
        let mut fixture = Fixture::new(vec![json!({"ok":false,"error_code":503})]).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), fixture.poller.next(42))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }
}
