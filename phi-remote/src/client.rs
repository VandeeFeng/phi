use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::protocol::{
    self, ConnectChallenge, ConnectResult, Event, EventName, Method, Request, RequestId, Response,
};
use crate::{RemoteEvent, RemoteSession, RemoteSnapshot, SavedSession, session_segment};

/// A client connection does not own the gateway or its Agents.
#[derive(Clone)]
pub struct RemoteClient {
    http: Client,
    base: Url,
    token: String,
}

impl RemoteClient {
    pub fn connect(endpoint: &str, local_credential: &str) -> anyhow::Result<Self> {
        let base = Url::parse(endpoint)?;
        ensure!(
            base.scheme() == "http"
                && base.host_str() == Some("127.0.0.1")
                && base.port().is_some()
                && base.username().is_empty()
                && base.password().is_none()
                && base.path() == "/"
                && base.query().is_none()
                && base.fragment().is_none(),
            "local Gateway endpoint must be a clean IPv4 loopback HTTP origin"
        );
        ensure!(
            crate::pairing::is_identity(local_credential),
            "invalid local Gateway credential"
        );
        Ok(Self {
            http: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(35))
                .build()?,
            base,
            token: local_credential.to_owned(),
        })
    }

    /// Opens one selected-root observation stream; commands remain REST requests.
    pub async fn watch(&self, id: &str) -> anyhow::Result<crate::RemoteWatch> {
        RemoteWatch::connect(self.clone(), session_segment(id)?).await
    }

    pub(crate) async fn recovery_at_boundary<Application: DeserializeOwned>(
        &self,
        id: &str,
        boundary: &str,
    ) -> anyhow::Result<RemoteSnapshot<Application>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Correlated<Application> {
            event_boundary: String,
            #[serde(flatten)]
            snapshot: crate::snapshot::Snapshot<Application>,
        }
        let mut url = self
            .base
            .join(&format!("api/sessions/{}/snapshot", session_segment(id)?))?;
        url.query_pairs_mut().append_pair("eventBoundary", boundary);
        let response: Correlated<Application> = self.get(url.as_str()).await?;
        ensure!(
            response.event_boundary == boundary,
            "Gateway recovery boundary mismatch"
        );
        let snapshot = response.snapshot.into_internal()?;
        ensure!(
            snapshot.session.id == id && snapshot.view.snapshot.id.as_str() == id,
            "Gateway returned another Session"
        );
        Ok(snapshot)
    }

    pub async fn devices(&self) -> anyhow::Result<crate::pairing::DeviceList> {
        self.get("api/gateway/devices").await
    }

    pub async fn approve_device(&self, request_id: &str) -> anyhow::Result<Value> {
        ensure!(
            crate::pairing::is_identity(request_id),
            "invalid pairing request identifier"
        );
        self.post(
            &format!("api/gateway/devices/{request_id}/approve"),
            serde_json::json!({}),
        )
        .await
    }

    pub async fn reject_device(&self, request_id: &str) -> anyhow::Result<Value> {
        ensure!(
            crate::pairing::is_identity(request_id),
            "invalid pairing request identifier"
        );
        self.post(
            &format!("api/gateway/devices/{request_id}/reject"),
            serde_json::json!({}),
        )
        .await
    }

    pub async fn revoke_device(&self, device_id: &str) -> anyhow::Result<Value> {
        ensure!(
            crate::pairing::is_identity(device_id),
            "invalid device identifier"
        );
        self.post(
            &format!("api/gateway/devices/{device_id}/revoke"),
            serde_json::json!({}),
        )
        .await
    }

    pub async fn sessions(&self) -> anyhow::Result<Vec<RemoteSession>> {
        self.get("api/sessions").await
    }

    pub async fn view(&self, session: &str) -> anyhow::Result<Value> {
        self.get(&format!("api/sessions/{}/view", session_segment(session)?))
            .await
    }

    pub async fn saved(&self, query: &str) -> anyhow::Result<Vec<SavedSession>> {
        let mut url = self.base.join("api/sessions/saved")?;
        url.query_pairs_mut().append_pair("query", query);
        self.get(url.as_str()).await
    }

    pub async fn create(&self, working_dir: &std::path::Path) -> anyhow::Result<RemoteSession> {
        self.post(
            "api/sessions",
            serde_json::json!({"workingDir":working_dir}),
        )
        .await
    }

    pub async fn resume(&self, id: &str) -> anyhow::Result<RemoteSession> {
        session_segment(id)?;
        self.post("api/sessions/resume", serde_json::json!({"id":id}))
            .await
    }

    pub async fn snapshot(&self, id: &str) -> anyhow::Result<RemoteSnapshot> {
        self.recovery(id).await
    }

    pub async fn recovery<Application: DeserializeOwned>(
        &self,
        id: &str,
    ) -> anyhow::Result<RemoteSnapshot<Application>> {
        let snapshot: crate::snapshot::Snapshot<Application> = self
            .get(&format!("api/sessions/{}/snapshot", session_segment(id)?))
            .await?;
        snapshot.into_internal()
    }

    pub async fn configure<Application: DeserializeOwned>(
        &self,
        id: &str,
        selection: crate::RemoteSelection,
    ) -> anyhow::Result<crate::RemoteRuntimeUpdate<Application>> {
        let snapshot: crate::snapshot::RuntimeUpdate<Application> = self
            .post(
                &format!("api/sessions/{}/configure", session_segment(id)?),
                serde_json::to_value(selection)?,
            )
            .await?;
        snapshot.into_internal()
    }

    pub async fn reload<Application: DeserializeOwned>(
        &self,
        id: &str,
    ) -> anyhow::Result<crate::RemoteRuntimeUpdate<Application>> {
        let snapshot: crate::snapshot::RuntimeUpdate<Application> = self
            .post(
                &format!("api/sessions/{}/reload", session_segment(id)?),
                serde_json::json!({}),
            )
            .await?;
        snapshot.into_internal()
    }

    pub async fn catalog(&self, id: &str) -> anyhow::Result<crate::RemoteCatalog> {
        self.get(&format!("api/sessions/{}/catalog", session_segment(id)?))
            .await
    }

    pub async fn new_session(&self, source: &str) -> anyhow::Result<RemoteSession> {
        self.post(
            &format!("api/sessions/{}/new", session_segment(source)?),
            serde_json::json!({}),
        )
        .await
    }

    pub async fn submit(
        &self,
        id: &str,
        prompt: phi_sdk::turn::TurnPrompt,
        dispatch: phi_sdk::turn::SubmissionDispatch,
    ) -> anyhow::Result<crate::RemoteSubmission> {
        self.post(
            &format!("api/sessions/{}/submit", session_segment(id)?),
            serde_json::to_value(crate::wire::SubmitInput::from_sdk(prompt, dispatch)?)?,
        )
        .await
    }

    pub async fn message(&self, id: &str, message: &str) -> anyhow::Result<Value> {
        self.post(
            &format!("api/sessions/{}/message", session_segment(id)?),
            serde_json::json!({"message":message}),
        )
        .await
    }

    pub async fn permission(
        &self,
        id: &str,
        agent: &str,
        request: &str,
        decision: phi_sdk::interaction::PermissionDecision,
    ) -> anyhow::Result<Value> {
        let decision = match decision {
            phi_sdk::interaction::PermissionDecision::AllowOnce => "allow_once",
            phi_sdk::interaction::PermissionDecision::AllowAlways(_) => "allow_always",
            phi_sdk::interaction::PermissionDecision::Deny => "deny",
        };
        self.post(
            &format!("api/sessions/{}/permission", session_segment(id)?),
            serde_json::json!({"agentId":agent,"requestId":request,"decision":decision}),
        )
        .await
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<Value> {
        self.post(
            &format!("api/sessions/{}/cancel", session_segment(id)?),
            serde_json::json!({}),
        )
        .await
    }

    pub async fn close_session(&self, id: &str) -> anyhow::Result<Value> {
        self.post(
            &format!("api/sessions/{}/close", session_segment(id)?),
            serde_json::json!({}),
        )
        .await
    }

    pub async fn stop(&self) -> anyhow::Result<Value> {
        self.post("api/gateway/stop", serde_json::json!({})).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> anyhow::Result<T> {
        let response = self
            .http
            .post(self.base.join(path)?)
            .bearer_auth(&self.token)
            .header("Origin", self.base.origin().ascii_serialization())
            .json(&body)
            .send()
            .await?;
        Self::read_response(response).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let response = self
            .http
            .get(self.base.join(path)?)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        Self::read_response(response).await
    }

    async fn read_response<T: DeserializeOwned>(
        mut response: reqwest::Response,
    ) -> anyhow::Result<T> {
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= crate::protocol::SERVER_BYTES.saturating_sub(bytes.len()),
                "Gateway response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let error: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            anyhow::bail!(
                "Gateway operation failed ({status}): {}",
                error["error"].as_str().unwrap_or("request rejected")
            );
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

const CONNECT_ID: u64 = 1;
const WATCH_ID: u64 = 2;
const READ_TIMEOUT: Duration = protocol::PING_INTERVAL.saturating_add(protocol::PONG_TIMEOUT);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct BufferedEvent {
    event: RemoteEvent,
    bytes: usize,
}

/// One disposable selected-root stream. Dropping it never stops the Agent.
/// Reconnection is explicit and must recover current state, never resend commands.
pub struct RemoteWatch {
    client: RemoteClient,
    session: String,
    socket: Socket,
    sequence: u64,
    buffered: VecDeque<BufferedEvent>,
    buffered_bytes: usize,
    state: ProjectionState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectionState {
    Unrecovered,
    Ready,
    Stale,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selection {
    session_id: String,
    watch_id: RequestId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionEvent {
    session_id: String,
    watch_id: RequestId,
    event: Value,
}

impl RemoteWatch {
    async fn connect(client: RemoteClient, session: &str) -> anyhow::Result<Self> {
        tokio::time::timeout(protocol::HANDSHAKE_TIMEOUT, async {
            let mut endpoint = client.base.clone();
            endpoint
                .set_scheme("ws")
                .map_err(|_| anyhow::anyhow!("invalid Gateway endpoint"))?;
            endpoint.set_path(protocol::WS_PATH);
            let mut request = endpoint.as_str().into_client_request()?;
            request
                .headers_mut()
                .insert("sec-websocket-protocol", protocol::SUBPROTOCOL.parse()?);
            let config = WebSocketConfig::default()
                .max_message_size(Some(protocol::SERVER_BYTES))
                .max_frame_size(Some(protocol::SERVER_BYTES));
            let (mut socket, response) =
                tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                    .await
                    .map_err(|_| anyhow::anyhow!("cannot open Gateway event connection"))?;
            ensure!(
                response
                    .headers()
                    .get("sec-websocket-protocol")
                    .is_some_and(|value| value == protocol::SUBPROTOCOL),
                "Gateway event protocol mismatch"
            );
            let challenge: Event<ConnectChallenge> =
                serde_json::from_value(read_value(&mut socket).await?)?;
            ensure!(
                challenge.event == EventName::ConnectChallenge
                    && challenge.sequence() == 1
                    && challenge.payload.min_protocol <= protocol::VERSION
                    && challenge.payload.max_protocol >= protocol::VERSION,
                "Gateway event protocol mismatch"
            );
            send(
                &mut socket,
                CONNECT_ID,
                Method::Connect,
                json!({
                    "minProtocol":protocol::VERSION,"maxProtocol":protocol::VERSION,
                    "auth":{"kind":"local","credential":client.token},
                }),
            )
            .await?;
            let ready: ConnectResult =
                serde_json::from_value(receive_response(&mut socket, CONNECT_ID).await?)?;
            let ConnectResult::Ready {
                hello,
                device_credential: None,
            } = ready
            else {
                anyhow::bail!("Gateway local authentication rejected");
            };
            ensure!(
                hello.protocol == protocol::VERSION
                    && hello.gateway_id == challenge.payload.gateway_id
                    && hello.boot_id == challenge.payload.boot_id
                    && hello.connection_id == challenge.payload.connection_id,
                "Gateway event connection identity mismatch"
            );
            send(
                &mut socket,
                WATCH_ID,
                Method::SessionWatch,
                json!({"sessionId":session}),
            )
            .await?;
            let selected: Selection =
                serde_json::from_value(receive_response(&mut socket, WATCH_ID).await?)?;
            ensure!(
                selected.session_id == session && selected.watch_id == request_id(WATCH_ID),
                "Gateway watch acknowledgement mismatch"
            );
            Ok(Self {
                client,
                session: session.into(),
                socket,
                sequence: challenge.sequence(),
                buffered: VecDeque::new(),
                buffered_bytes: 0,
                state: ProjectionState::Unrecovered,
            })
        })
        .await
        .context("Gateway event handshake timed out")?
    }

    pub async fn next(&mut self) -> anyhow::Result<RemoteEvent> {
        ensure!(
            self.state == ProjectionState::Ready,
            "Gateway stream needs correlated recovery before applying events"
        );
        if let Some(buffered) = self.buffered.pop_front() {
            self.buffered_bytes -= buffered.bytes;
            return Ok(buffered.event);
        }
        match self.read_event().await {
            Ok(event) => Ok(event.event),
            Err(error) => {
                self.state = ProjectionState::Stale;
                Err(error)
            }
        }
    }

    /// Subscribes before the REST read, discards only covered events, and retains later events.
    /// Failure or cancellation leaves the stream stale; reconnect before applying more deltas.
    pub async fn recovery<Application: DeserializeOwned>(
        &mut self,
    ) -> anyhow::Result<RemoteSnapshot<Application>> {
        ensure!(
            self.state != ProjectionState::Stale,
            "Gateway stream is stale; reconnect and recover"
        );
        self.state = ProjectionState::Stale;
        let result = tokio::time::timeout(protocol::HANDSHAKE_TIMEOUT, self.recover_at_boundary())
            .await
            .context("Gateway recovery event boundary timed out")?;
        if result.is_ok() {
            self.state = ProjectionState::Ready;
        }
        result
    }

    async fn recover_at_boundary<Application: DeserializeOwned>(
        &mut self,
    ) -> anyhow::Result<RemoteSnapshot<Application>> {
        let marker = crate::pairing::random_credential()?;
        let client = self.client.clone();
        let session = self.session.clone();
        let read = client.recovery_at_boundary::<Application>(&session, &marker);
        tokio::pin!(read);
        let mut snapshot = None;
        let mut covered = false;
        // Everything queued before this read precedes its future ordered marker.
        self.buffered.clear();
        self.buffered_bytes = 0;
        loop {
            if covered && let Some(snapshot) = snapshot.take() {
                return Ok(snapshot);
            }
            tokio::select! {
                result = &mut read, if snapshot.is_none() => snapshot = Some(result?),
                received = self.read_event() => {
                    let received = received?;
                    if covered {
                        ensure!(self.buffered.len() < protocol::QUEUE_MESSAGES
                            && received.bytes <= protocol::QUEUE_BYTES.saturating_sub(self.buffered_bytes),
                            "Gateway recovery buffer exceeded");
                        self.buffered_bytes += received.bytes;
                        self.buffered.push_back(received);
                    } else if matches!(&received.event, RemoteEvent::Boundary { id } if id == &marker) {
                        covered = true;
                    }
                }
            }
        }
    }

    async fn read_event(&mut self) -> anyhow::Result<BufferedEvent> {
        let value = read_value(&mut self.socket).await?;
        let bytes = serde_json::to_vec(&value)?.len();
        let event: Event<SessionEvent> = serde_json::from_value(value)?;
        ensure!(
            event.sequence() == self.sequence + 1,
            "Gateway event sequence gap"
        );
        self.sequence = event.sequence();
        ensure!(
            event.event == EventName::SessionEvent
                && event.payload.session_id == self.session
                && event.payload.watch_id == request_id(WATCH_ID),
            "Gateway event selection mismatch"
        );
        Ok(BufferedEvent {
            event: RemoteEvent::decode(event.payload.event, &self.session)?,
            bytes,
        })
    }
}

fn request_id(value: u64) -> RequestId {
    RequestId::new(value).expect("fixed observation request ID")
}

async fn send(socket: &mut Socket, id: u64, method: Method, params: Value) -> anyhow::Result<()> {
    let text = serde_json::to_string(&Request::new(request_id(id), method, params))?;
    tokio::time::timeout(
        protocol::WRITE_TIMEOUT,
        socket.send(Message::Text(text.into())),
    )
    .await
    .context("Gateway event write timed out")?
    .map_err(|_| anyhow::anyhow!("Gateway event write failed"))
}

async fn receive_response(socket: &mut Socket, id: u64) -> anyhow::Result<Value> {
    match serde_json::from_value::<Response<Value>>(read_value(socket).await?)? {
        Response::Success(response) => {
            ensure!(
                response.id == request_id(id),
                "Gateway response identity mismatch"
            );
            Ok(response.payload)
        }
        Response::Failure(response) => {
            ensure!(
                response.id == request_id(id),
                "Gateway response identity mismatch"
            );
            Err(response.error.code.into())
        }
    }
}

async fn read_value(socket: &mut Socket) -> anyhow::Result<Value> {
    loop {
        let message = tokio::time::timeout(READ_TIMEOUT, socket.next())
            .await
            .context("Gateway event heartbeat timed out")?
            .context("Gateway event connection closed")?
            .map_err(|_| anyhow::anyhow!("Gateway event connection failed"))?;
        match message {
            Message::Text(text) => {
                ensure!(
                    text.len() <= protocol::SERVER_BYTES,
                    "Gateway event exceeds limit"
                );
                return Ok(serde_json::from_str(&text)?);
            }
            Message::Ping(_) => {
                tokio::time::timeout(protocol::WRITE_TIMEOUT, socket.flush())
                    .await
                    .context("Gateway heartbeat write timed out")?
                    .map_err(|_| anyhow::anyhow!("Gateway heartbeat write failed"))?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => {
                anyhow::bail!("Gateway event connection closed; projection is stale")
            }
            _ => anyhow::bail!("unsupported Gateway event frame"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Query, WebSocketUpgrade},
        routing::get,
    };
    use tokio::sync::{mpsc, oneshot};

    struct Fixture {
        watch: RemoteWatch,
        frames: mpsc::Sender<Value>,
        reads: mpsc::Receiver<(String, oneshot::Sender<Value>)>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fixture() -> Fixture {
        let (frames, incoming) = mpsc::channel::<Value>(protocol::QUEUE_MESSAGES);
        let (reads, requests) = mpsc::channel(1);
        let incoming = std::sync::Arc::new(std::sync::Mutex::new(Some(incoming)));
        let router = Router::new()
            .route(
                "/ws",
                get(move |upgrade: WebSocketUpgrade| {
                    let incoming = incoming.clone();
                    async move {
                        let mut incoming = incoming.lock().unwrap().take().unwrap();
                        upgrade.on_upgrade(move |mut socket| async move {
                            while let Some(frame) = incoming.recv().await {
                                if socket
                                    .send(axum::extract::ws::Message::Text(
                                        frame.to_string().into(),
                                    ))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                    }
                }),
            )
            .route(
                "/api/sessions/root/snapshot",
                get(
                    move |Query(query): Query<std::collections::HashMap<String, String>>| {
                        let reads = reads.clone();
                        async move {
                            let (reply, response) = oneshot::channel();
                            reads
                                .send((query["eventBoundary"].clone(), reply))
                                .await
                                .unwrap();
                            Json(response.await.unwrap_or(Value::Null))
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = RemoteClient::connect(&format!("http://{address}/"), &"a".repeat(64)).unwrap();
        let config = WebSocketConfig::default()
            .max_message_size(Some(protocol::SERVER_BYTES))
            .max_frame_size(Some(protocol::SERVER_BYTES));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(
            format!("ws://{address}/ws"),
            Some(config),
            false,
        )
        .await
        .unwrap();
        Fixture {
            watch: RemoteWatch {
                client,
                session: "root".into(),
                socket,
                sequence: 1,
                buffered: VecDeque::new(),
                buffered_bytes: 0,
                state: ProjectionState::Ready,
            },
            frames,
            reads: requests,
            task,
        }
    }

    fn event(sequence: u64, event: Value) -> Value {
        serde_json::to_value(
            Event::new(
                EventName::SessionEvent,
                sequence,
                json!({
                    "sessionId":"root","watchId":"2","event":event,
                }),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn snapshot(marker: &str) -> Value {
        let timestamp = "2026-11-05T00:00:00Z";
        json!({
            "eventBoundary":marker,"version":2,
            "session":{"id":"root","name":"test","provider":"local","model":"test","workingDir":"/tmp"},
            "history":{"entries":[],"leafId":null,"createdAt":timestamp,"updatedAt":timestamp,
                "totalTokens":0,"totalCost":0,"totalCacheRead":0,"totalCacheWrite":0},
            "execution":{"status":"idle","activeTurnId":null,"live":null,"error":null,"revision":1},
            "pendingSubmissionIds":[],"interactions":[],"application":{},
        })
    }

    #[tokio::test]
    async fn recovery_correlates_both_arrival_orders_and_keeps_only_later_events() {
        for response_first in [false, true] {
            let mut fixture = fixture().await;
            let frames = fixture.frames.clone();
            let recovered = {
                let mut recovery = Box::pin(fixture.watch.recovery::<Value>());
                let (marker, reply) = tokio::select! {
                    result = &mut recovery => panic!("recovery finished before its REST read: {}", result.is_ok()),
                    request = fixture.reads.recv() => request.unwrap(),
                };
                frames
                    .send(event(2, json!({"kind":"boundary","id":"another-client"})))
                    .await
                    .unwrap();
                frames
                    .send(event(3, json!({"kind":"refresh","reason":"history"})))
                    .await
                    .unwrap();
                if response_first {
                    reply.send(snapshot(&marker)).unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(50), &mut recovery)
                            .await
                            .is_err()
                    );
                    frames
                        .send(event(4, json!({"kind":"boundary","id":marker})))
                        .await
                        .unwrap();
                    frames
                        .send(event(5, json!({"kind":"boundary","id":"later"})))
                        .await
                        .unwrap();
                } else {
                    frames
                        .send(event(4, json!({"kind":"boundary","id":marker})))
                        .await
                        .unwrap();
                    frames
                        .send(event(5, json!({"kind":"boundary","id":"later"})))
                        .await
                        .unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(50), &mut recovery)
                            .await
                            .is_err()
                    );
                    reply.send(snapshot(&marker)).unwrap();
                }
                tokio::time::timeout(Duration::from_secs(1), recovery)
                    .await
                    .unwrap()
                    .unwrap()
            };
            assert_eq!(recovered.session.id, "root");
            assert!(
                matches!(fixture.watch.next().await.unwrap(), RemoteEvent::Boundary { id } if id == "later")
            );
            assert_eq!(fixture.watch.buffered_bytes, 0);
        }
    }

    #[tokio::test]
    async fn recovery_overflow_and_cancellation_make_the_stream_stale() {
        for (count, payload_bytes) in [
            (protocol::QUEUE_MESSAGES + 1, 1),
            (3, protocol::SERVER_BYTES / 4 * 3),
        ] {
            let mut fixture = fixture().await;
            let frames = fixture.frames.clone();
            let reads = &mut fixture.reads;
            let control = async {
                let (marker, reply) = reads.recv().await.unwrap();
                frames
                    .send(event(2, json!({"kind":"boundary","id":marker})))
                    .await
                    .unwrap();
                for index in 0..count {
                    frames
                        .send(event(
                            index as u64 + 3,
                            json!({"kind":"boundary","id":"x".repeat(payload_bytes)}),
                        ))
                        .await
                        .unwrap();
                }
                reply
            };
            let (result, _reply) = tokio::join!(fixture.watch.recovery::<Value>(), control);
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("buffer exceeded")
            );
            assert!(fixture.watch.buffered.len() <= protocol::QUEUE_MESSAGES);
            assert!(fixture.watch.buffered_bytes <= protocol::QUEUE_BYTES);
            assert!(fixture.watch.next().await.is_err());
        }

        let mut fixture = self::fixture().await;
        let reads = &mut fixture.reads;
        let pending = async {
            let (_marker, reply) = reads.recv().await.unwrap();
            reply
        };
        let recovery =
            tokio::time::timeout(Duration::from_millis(50), fixture.watch.recovery::<Value>());
        let (result, _reply) = tokio::join!(recovery, pending);
        assert!(result.is_err());
        assert!(fixture.watch.next().await.is_err());
    }

    #[tokio::test]
    async fn accepted_runtime_responses_are_bounded_without_replaying_operations() {
        use axum::{body::Body, http::StatusCode, response::Response};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        for status in [StatusCode::OK, StatusCode::BAD_REQUEST] {
            for response_bytes in [protocol::SERVER_BYTES, protocol::SERVER_BYTES + 1] {
                let operations = Arc::new(AtomicUsize::new(0));
                let accepted = operations.clone();
                let mut payload = if status == StatusCode::OK {
                    snapshot("unused").to_string()
                } else {
                    json!({"error":"rejected"}).to_string()
                };
                payload.extend(std::iter::repeat_n(' ', response_bytes - payload.len()));
                let payload = axum::body::Bytes::from(payload);
                let router = Router::new().fallback(move |request: axum::extract::Request| {
                    let accepted = accepted.clone();
                    let payload = payload.clone();
                    async move {
                        if request.method() == axum::http::Method::POST {
                            assert_eq!(request.uri().path(), "/api/sessions/root/configure");
                            let body = axum::body::to_bytes(request.into_body(), 1024)
                                .await
                                .unwrap();
                            assert_eq!(
                                serde_json::from_slice::<Value>(&body).unwrap(),
                                json!({"kind":"switch","name":"test"})
                            );
                            accepted.fetch_add(1, Ordering::SeqCst);
                        }
                        let split = payload.len() / 2;
                        let chunks = [payload.slice(..split), payload.slice(split..)];
                        Response::builder()
                            .status(status)
                            .body(Body::from_stream(futures_util::stream::iter(
                                chunks.map(Ok::<_, std::io::Error>),
                            )))
                            .unwrap()
                    }
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let task =
                    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
                let client =
                    RemoteClient::connect(&format!("http://{address}/"), &"a".repeat(64)).unwrap();
                let recovered = client
                    .recovery::<Value>("root")
                    .await
                    .map(|value| value.session.id);
                let configured = client
                    .configure::<Value>(
                        "root",
                        crate::RemoteSelection::Switch {
                            name: "test".into(),
                        },
                    )
                    .await
                    .map(|value| value.session.id);
                assert_eq!(
                    operations.load(Ordering::SeqCst),
                    1,
                    "a lost receipt must not replay acceptance"
                );
                if response_bytes > protocol::SERVER_BYTES {
                    assert!(
                        configured
                            .unwrap_err()
                            .to_string()
                            .contains("response exceeds limit")
                    );
                    assert!(recovered.is_err());
                } else if status == StatusCode::OK {
                    assert_eq!(recovered.unwrap(), "root");
                    assert_eq!(configured.unwrap(), "root");
                } else {
                    assert!(recovered.is_err());
                    assert!(configured.unwrap_err().to_string().contains("rejected"));
                }
                task.abort();
            }
        }
    }

    #[tokio::test]
    async fn identity_gaps_and_mismatched_rest_markers_fail_closed() {
        for change in ["sequence", "watch", "session", "phi-session"] {
            let mut fixture = fixture().await;
            let mut frame = event(2, json!({"kind":"boundary","id":"marker"}));
            match change {
                "sequence" => frame["seq"] = json!(3),
                "watch" => frame["payload"]["watchId"] = json!("3"),
                "session" => frame["payload"]["sessionId"] = json!("foreign"),
                _ => {
                    frame["payload"]["event"] = json!({"kind":"phi","id":"event","emittedAt":"2026-11-05T00:00:00Z",
                    "sessionId":"foreign","event":{"type":"assistant_delta","display":"foreign"}})
                }
            }
            fixture.frames.send(frame).await.unwrap();
            assert!(fixture.watch.next().await.is_err(), "{change}");
            assert!(fixture.watch.next().await.is_err());
        }
        let mut fixture = fixture().await;
        let reads = &mut fixture.reads;
        let control = async {
            let (_, reply) = reads.recv().await.unwrap();
            reply.send(snapshot("wrong-marker")).unwrap();
        };
        let (result, ()) = tokio::join!(fixture.watch.recovery::<Value>(), control);
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("boundary mismatch")
        );
        assert!(fixture.watch.next().await.is_err());
    }
}
