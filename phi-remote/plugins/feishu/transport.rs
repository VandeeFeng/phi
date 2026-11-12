use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use reqwest::Url;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::super::backoff::Backoff;
use super::api::{API_SUCCESS, Feishu, MAX_PAYLOAD_BYTES};

pub(crate) const CHANNEL_CAPACITY: usize = 32;
const MAX_PARTS: usize = 128;
const MAX_CACHE_BYTES: usize = 8 * MAX_PAYLOAD_BYTES;
const FRAGMENT_TTL: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_RECONNECT: Duration = Duration::from_secs(1);
const MAX_RECONNECT: Duration = Duration::from_secs(30);
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
const CONTROL: i32 = 0;
const DATA: i32 = 1;
const ACK_OK: &[u8] = b"{\"code\":200}";
const DISCOVERY_BUSY: i64 = 1;
const DISCOVERY_INTERNAL_ERROR: i64 = 1000040343;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, PartialEq, prost::Message)]
struct Frame {
    #[prost(uint64, required, tag = "1")]
    sequence: u64,
    #[prost(uint64, required, tag = "2")]
    log: u64,
    #[prost(int32, required, tag = "3")]
    service: i32,
    #[prost(int32, required, tag = "4")]
    method: i32,
    #[prost(message, repeated, tag = "5")]
    headers: Vec<Header>,
    #[prost(string, optional, tag = "6")]
    encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    payload_type: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    log_new: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Header {
    #[prost(string, required, tag = "1")]
    key: String,
    #[prost(string, required, tag = "2")]
    value: String,
}

impl Frame {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.key == name)
            .map(|header| header.value.as_str())
    }

    fn ping(service: i32) -> Self {
        Self {
            service,
            method: CONTROL,
            headers: vec![Header {
                key: "type".into(),
                value: "ping".into(),
            }],
            ..Default::default()
        }
    }

    fn ack(mut self) -> Self {
        self.payload = Some(ACK_OK.to_vec());
        self
    }
}

pub(crate) struct Incoming {
    pub payload: Vec<u8>,
    pub acknowledgement: Acknowledgement,
}

pub(crate) struct Acknowledgement {
    generation: u64,
    frame: Frame,
}

#[derive(Debug)]
struct Terminal;
impl std::fmt::Display for Terminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Feishu rejected connection configuration; check app credentials, event subscription and connection limits")
    }
}
impl std::error::Error for Terminal {}

struct Connection {
    url: Url,
    service: i32,
    ping_interval: Duration,
}

impl Connection {
    fn parse(value: &serde_json::Value) -> anyhow::Result<Self> {
        let code = value["code"]
            .as_i64()
            .context("missing discovery response code")?;
        if matches!(code, DISCOVERY_BUSY | DISCOVERY_INTERNAL_ERROR) {
            bail!("Feishu discovery temporarily unavailable");
        }
        if code != API_SUCCESS {
            return Err(Terminal.into());
        }
        let url = Url::parse(
            value["data"]["URL"]
                .as_str()
                .context("missing Feishu WS URL")?,
        )?;
        validate_url(&url)?;
        let service = url
            .query_pairs()
            .find(|(key, _)| key == "service_id")
            .context("missing service ID")?
            .1
            .parse::<i32>()?;
        ensure!(service > 0, "invalid service ID");
        let seconds = value["data"]["ClientConfig"]["PingInterval"]
            .as_u64()
            .context("missing ping interval")?;
        ensure!((1..=3600).contains(&seconds), "invalid ping interval");
        Ok(Self {
            url,
            service,
            ping_interval: Duration::from_secs(seconds),
        })
    }
}

fn validate_url(url: &Url) -> anyhow::Result<()> {
    ensure!(
        url.scheme() == "wss"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".feishu.cn"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.port_or_known_default() == Some(443),
        "untrusted Feishu WebSocket endpoint"
    );
    Ok(())
}

pub(crate) async fn run(
    api: Feishu,
    incoming: mpsc::Sender<Incoming>,
    mut acknowledgements: mpsc::Receiver<Acknowledgement>,
) -> anyhow::Result<()> {
    let mut generation = 0u64;
    let mut retry = Backoff::new(INITIAL_RECONNECT, MAX_RECONNECT);
    loop {
        generation = generation
            .checked_add(1)
            .context("connection generation overflow")?;
        let connected = async {
            let connection = Connection::parse(&api.endpoint().await?)?;
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_PAYLOAD_BYTES))
                .max_frame_size(Some(MAX_PAYLOAD_BYTES));
            let (socket, _) = tokio::time::timeout(
                WRITE_TIMEOUT,
                tokio_tungstenite::connect_async_with_config(
                    connection.url.as_str(),
                    Some(config),
                    false,
                ),
            )
            .await
            .map_err(|_| anyhow::anyhow!("Feishu connection timed out"))?
            .map_err(|_| anyhow::anyhow!("Feishu connection failed"))?;
            eprintln!("Feishu connected; remote client ready.");
            session(
                socket,
                connection,
                generation,
                &incoming,
                &mut acknowledgements,
            )
            .await
        };
        let started = Instant::now();
        match connected.await {
            Ok(()) => return Ok(()),
            Err(error) if error.downcast_ref::<Terminal>().is_some() => return Err(error),
            Err(_) => {
                ensure!(!incoming.is_closed(), "bridge event receiver closed");
                if started.elapsed() > STABLE_CONNECTION {
                    retry.reset();
                }
                let delay = retry.next_delay();
                eprintln!(
                    "Feishu connection interrupted; reconnecting in {} seconds",
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn write(socket: &mut Socket, frame: Frame) -> anyhow::Result<()> {
    tokio::time::timeout(
        WRITE_TIMEOUT,
        socket.send(Message::Binary(frame.encode_to_vec().into())),
    )
    .await??;
    Ok(())
}

async fn session(
    mut socket: Socket,
    connection: Connection,
    generation: u64,
    incoming: &mpsc::Sender<Incoming>,
    acknowledgements: &mut mpsc::Receiver<Acknowledgement>,
) -> anyhow::Result<()> {
    let mut ping_interval = connection.ping_interval;
    let mut heartbeat = tokio::time::interval(ping_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_received = Instant::now();
    let mut fragments = Fragments::default();
    loop {
        tokio::select! {
            frame = socket.next() => {
                let message = frame.context("Feishu disconnected")??;
                last_received = Instant::now();
                match message {
                    Message::Binary(bytes) => {
                        let frame = Frame::decode(bytes)?;
                        ensure!(frame.service == connection.service, "wrong Feishu service");
                        match (frame.method, frame.header("type")) {
                            (CONTROL, Some("pong")) => {
                                if let Some(seconds) = frame.payload.as_deref()
                                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
                                    .and_then(|value| value["PingInterval"].as_u64())
                                    .filter(|seconds| (1..=3600).contains(seconds)) {
                                        let updated = Duration::from_secs(seconds);
                                        if updated != ping_interval {
                                            ping_interval = updated;
                                            heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + ping_interval, ping_interval);
                                            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                        }
                                    }
                            }
                            (DATA, Some("event")) => {
                                if let Some(payload) = fragments.merge(&frame)? {
                                    incoming.try_send(Incoming { payload, acknowledgement: Acknowledgement { generation, frame } })
                                        .map_err(|_| anyhow::anyhow!("Feishu ingress queue unavailable; delivery not acknowledged"))?;
                                }
                            }
                            _ => {}
                        }
                    }
                    Message::Ping(bytes) => { tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Pong(bytes))).await??; }
                    Message::Close(_) => bail!("Feishu connection closed"),
                    _ => {}
                }
            }
            acknowledgement = acknowledgements.recv() => {
                let Some(acknowledgement) = acknowledgement else { return Ok(()); };
                if acknowledgement.generation == generation {
                    write(&mut socket, acknowledgement.frame.ack()).await?;
                }
            }
            _ = heartbeat.tick() => {
                ensure!(last_received.elapsed() < ping_interval * 2 + WRITE_TIMEOUT, "Feishu heartbeat timed out");
                fragments.expire();
                write(&mut socket, Frame::ping(connection.service)).await?;
            }
        }
    }
}

#[derive(Default)]
struct Fragments {
    pending: HashMap<String, Parts>,
}

struct Parts {
    created: Instant,
    pieces: Vec<Option<Vec<u8>>>,
}

impl Fragments {
    fn expire(&mut self) {
        self.pending
            .retain(|_, parts| parts.created.elapsed() < FRAGMENT_TTL);
    }

    fn merge(&mut self, frame: &Frame) -> anyhow::Result<Option<Vec<u8>>> {
        self.expire();
        let sum = frame.header("sum").unwrap_or("1").parse::<usize>()?;
        let sequence = frame.header("seq").unwrap_or("0").parse::<usize>()?;
        ensure!(
            (1..=MAX_PARTS).contains(&sum) && sequence < sum,
            "invalid fragment metadata"
        );
        let payload = frame.payload.as_deref().context("missing event payload")?;
        ensure!(
            payload.len() <= MAX_PAYLOAD_BYTES,
            "event payload exceeds limit"
        );
        if sum == 1 {
            return Ok(Some(payload.to_vec()));
        }
        let id = frame
            .header("message_id")
            .context("missing fragment message ID")?;
        ensure!(!id.is_empty() && id.len() <= 256, "invalid fragment ID");
        ensure!(
            self.pending.contains_key(id) || self.pending.len() < CHANNEL_CAPACITY,
            "fragment cache full"
        );
        let cached_bytes: usize = self
            .pending
            .values()
            .flat_map(|parts| parts.pieces.iter().flatten())
            .map(Vec::len)
            .sum();
        ensure!(
            cached_bytes + payload.len() <= MAX_CACHE_BYTES,
            "fragment byte budget exceeded"
        );
        let parts = self.pending.entry(id.into()).or_insert_with(|| Parts {
            created: Instant::now(),
            pieces: vec![None; sum],
        });
        ensure!(parts.pieces.len() == sum, "inconsistent fragment count");
        if let Some(previous) = &parts.pieces[sequence] {
            ensure!(previous == payload, "conflicting duplicate fragment");
        } else {
            parts.pieces[sequence] = Some(payload.to_vec());
        }
        ensure!(
            parts.pieces.iter().flatten().map(Vec::len).sum::<usize>() <= MAX_PAYLOAD_BYTES,
            "assembled payload exceeds limit"
        );
        if parts.pieces.iter().any(Option::is_none) {
            return Ok(None);
        }
        let parts = self
            .pending
            .remove(id)
            .context("missing fragment assembly")?;
        Ok(Some(parts.pieces.into_iter().flatten().flatten().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fragment(sequence: usize, sum: usize, bytes: &[u8]) -> Frame {
        Frame {
            method: DATA,
            service: 1,
            headers: vec![
                Header {
                    key: "type".into(),
                    value: "event".into(),
                },
                Header {
                    key: "message_id".into(),
                    value: "message".into(),
                },
                Header {
                    key: "sum".into(),
                    value: sum.to_string(),
                },
                Header {
                    key: "seq".into(),
                    value: sequence.to_string(),
                },
            ],
            payload: Some(bytes.into()),
            ..Default::default()
        }
    }

    #[test]
    fn ping_matches_the_official_sdk_protobuf_layout() {
        // Required zero-valued fields must stay on the wire (proto2).
        assert_eq!(
            Frame::ping(1).encode_to_vec(),
            b"\x08\x00\x10\x00\x18\x01\x20\x00\x2a\x0c\x0a\x04type\x12\x04ping"
        );
    }

    #[test]
    fn fragments_are_ordered_bounded_and_conflicts_fail_closed() {
        let mut cache = Fragments::default();
        assert!(cache.merge(&fragment(1, 2, b"world")).unwrap().is_none());
        assert!(cache.merge(&fragment(1, 2, b"world")).unwrap().is_none());
        assert_eq!(
            cache.merge(&fragment(0, 2, b"hello ")).unwrap().unwrap(),
            b"hello world"
        );
        assert!(cache.merge(&fragment(2, 2, b"bad")).is_err());
        assert!(cache.merge(&fragment(0, MAX_PARTS + 1, b"bad")).is_err());
        cache.merge(&fragment(0, 2, b"first")).unwrap();
        assert!(cache.merge(&fragment(0, 2, b"different")).is_err());
    }

    #[test]
    fn endpoint_discovery_never_leaks_credentials_to_untrusted_hosts() {
        for url in [
            "ws://msg.feishu.cn/ws",
            "wss://feishu.cn.evil.com/ws",
            "wss://127.0.0.1/ws",
            "wss://user:secret@msg.feishu.cn/ws",
            "wss://msg.feishu.cn:8443/ws",
        ] {
            assert!(validate_url(&Url::parse(url).unwrap()).is_err());
        }
        assert!(
            validate_url(&Url::parse("wss://msg-frontier.feishu.cn/ws?service_id=1").unwrap())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn transient_discovery_retries_but_invalid_credentials_stop() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let app = axum::Router::new().route(
            "/callback/ws/endpoint",
            axum::routing::post(move || {
                let code = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    1000040343
                } else {
                    514
                };
                async move { axum::Json(serde_json::json!({"code":code})) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = Feishu::test_endpoint(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (incoming, _events) = mpsc::channel(CHANNEL_CAPACITY);
        let (_acks, responses) = mpsc::channel(CHANNEL_CAPACITY);
        let error = tokio::time::timeout(Duration::from_secs(4), run(api, incoming, responses))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.downcast_ref::<Terminal>().is_some());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn websocket_heartbeats_continue_until_durable_acknowledgement_arrives() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (alive, confirmed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let first = fragment(0, 2, b"{\"schema\":");
            let mut last = fragment(1, 2, b"\"2.0\"}");
            last.sequence = 42;
            last.log = 99;
            socket
                .send(Message::Binary(last.encode_to_vec().into()))
                .await
                .unwrap();
            socket
                .send(Message::Binary(first.encode_to_vec().into()))
                .await
                .unwrap();
            let started = Instant::now();
            let mut pings = 0;
            let mut alive = Some(alive);
            loop {
                let bytes = tokio::time::timeout(Duration::from_secs(4), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .into_data();
                let frame = Frame::decode(bytes).unwrap();
                if frame.method == CONTROL {
                    assert_eq!(frame.header("type"), Some("ping"));
                    pings += 1;
                    let pong = Frame {
                        service: 1,
                        method: CONTROL,
                        headers: vec![Header {
                            key: "type".into(),
                            value: "pong".into(),
                        }],
                        payload: Some(b"{\"PingInterval\":1}".to_vec()),
                        ..Default::default()
                    };
                    socket
                        .send(Message::Binary(pong.encode_to_vec().into()))
                        .await
                        .unwrap();
                    if pings == 2 {
                        assert!(started.elapsed() >= Duration::from_millis(750));
                        alive.take().unwrap().send(()).unwrap();
                    }
                } else {
                    assert!(
                        pings >= 2,
                        "event acknowledged before the simulated durable claim"
                    );
                    assert_eq!(frame.sequence, 0);
                    assert_eq!(frame.header("seq"), Some("0"));
                    assert_eq!(frame.payload.as_deref(), Some(ACK_OK));
                    socket.close(None).await.unwrap();
                    break;
                }
            }
        });
        let (socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (incoming, mut events) = mpsc::channel(CHANNEL_CAPACITY);
        let (acks, mut responses) = mpsc::channel(CHANNEL_CAPACITY);
        let connection = Connection {
            url: Url::parse(&url).unwrap(),
            service: 1,
            ping_interval: Duration::from_secs(1),
        };
        let task =
            tokio::spawn(
                async move { session(socket, connection, 7, &incoming, &mut responses).await },
            );
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.payload, b"{\"schema\":\"2.0\"}");
        confirmed.await.unwrap();
        acks.send(event.acknowledgement).await.unwrap();
        server.await.unwrap();
        assert!(task.await.unwrap().is_err());
    }

    proptest! {
        #[test]
        fn protobuf_ack_preserves_request_correlation(sequence in any::<u64>(), log in any::<u64>(), payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let mut frame = fragment(0, 1, &payload);
            frame.sequence = sequence;
            frame.log = log;
            let decoded = Frame::decode(frame.encode_to_vec().as_slice()).unwrap();
            let ack = decoded.ack();
            prop_assert_eq!(ack.sequence, sequence);
            prop_assert_eq!(ack.log, log);
            prop_assert_eq!(ack.headers, frame.headers);
            prop_assert_eq!(ack.payload.as_deref(), Some(ACK_OK));
        }
    }
}
