use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::pairing::{self, DeviceOperation, DeviceProof, PairingStatus};
use crate::protocol::{
    self, Connect, ConnectAuth, ConnectChallenge, ConnectResult, Event, EventName, Method,
    PairingResolution, Request, RequestId, Response,
};
use anyhow::{Context, bail, ensure};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::{origin, session_path};
use crate::plugins::store::Store;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Device {
    pub secret: [u8; 32],
    pub gateway: Option<String>,
    pub credential: Option<String>,
}

impl Device {
    pub fn generate() -> anyhow::Result<Self> {
        let mut secret = [0; 32];
        getrandom::fill(&mut secret)
            .map_err(|_| anyhow::anyhow!("device key generation failed"))?;
        Ok(Self {
            secret,
            gateway: None,
            credential: None,
        })
    }
}

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_AUTH_BYTES: usize = 4096;

use super::MAX_RESPONSE_BYTES;
const WATCH_REQUEST_ID: u64 = 3;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Copy)]
pub(crate) enum Pairing<'a> {
    #[cfg(test)]
    Manual(&'a str),
    Local(&'a crate::deployment::LocalPairing),
}

pub(crate) async fn authenticate(
    endpoint: &str,
    device: &mut Device,
    store: &Store,
    enrollment: Option<Pairing<'_>>,
) -> anyhow::Result<()> {
    let mut socket = authenticated_socket(endpoint, device, Some(store), enrollment).await?;
    let _ = socket.close(None).await;
    Ok(())
}

async fn authenticated_socket(
    endpoint: &str,
    device: &mut Device,
    store: Option<&Store>,
    enrollment: Option<Pairing<'_>>,
) -> anyhow::Result<Socket> {
    let base = origin(endpoint)?;
    let token = match enrollment {
        #[cfg(test)]
        Some(Pairing::Manual(token)) => Some(token),
        Some(Pairing::Local(local)) => Some(local.token()),
        None => None,
    };
    let key = SigningKey::from_bytes(&device.secret);
    let public_key = key.verifying_key().to_bytes();
    let credential = token
        .or(device.credential.as_deref())
        .context("run pair before starting the bridge")?
        .to_owned();
    ensure!(
        pairing::is_identity(&credential),
        "invalid pairing token or device credential"
    );
    let mut operation = if token.is_some() {
        DeviceOperation::Enroll
    } else {
        DeviceOperation::Reconnect
    };
    let mut request_id = None;
    loop {
        let mut socket = connect(&base).await?;
        let event: Event<ConnectChallenge> =
            serde_json::from_str(&receive(&mut socket, AUTH_TIMEOUT).await?)?;
        ensure!(
            event.event == EventName::ConnectChallenge && event.sequence() == 1,
            "invalid Gateway challenge event"
        );
        let initial_sequence = event.sequence();
        let hello = event.payload;
        ensure!(
            hello.min_protocol <= protocol::VERSION
                && hello.max_protocol >= protocol::VERSION
                && pairing::is_identity(&hello.gateway_id)
                && pairing::is_identity(&hello.boot_id)
                && pairing::is_identity(&hello.connection_id)
                && pairing::is_identity(&hello.nonce)
                && deadline_valid(hello.expires_at, pairing::CHALLENGE_SECONDS)?
                && device
                    .gateway
                    .as_ref()
                    .is_none_or(|id| *id == hello.gateway_id)
                && match enrollment {
                    Some(Pairing::Local(local)) => local.gateway_id() == hello.gateway_id,
                    _ => true,
                },
            "Gateway identity or protocol mismatch"
        );
        let auth = match operation {
            DeviceOperation::Enroll => ConnectAuth::Enroll {
                credential: credential.clone(),
                public_key,
            },
            DeviceOperation::Claim => ConnectAuth::Claim {
                credential: credential.clone(),
                public_key,
                request_id: request_id.clone().context("missing pairing request")?,
            },
            DeviceOperation::Reconnect => ConnectAuth::Device {
                credential: credential.clone(),
                public_key,
            },
            DeviceOperation::Recover => {
                bail!("credential recovery is not supported by this client")
            }
        };
        send(
            &mut socket,
            1,
            Method::Connect,
            Connect {
                min_protocol: protocol::VERSION,
                max_protocol: protocol::VERSION,
                auth,
            },
        )
        .await?;
        let ConnectResult::ProofRequired { challenge } = result(&mut socket, 1).await? else {
            bail!("Gateway did not request device proof")
        };
        ensure!(
            challenge.gateway_id == hello.gateway_id
                && challenge.boot_id == hello.boot_id
                && challenge.connection_id == hello.connection_id
                && challenge.audience == base.join("ws")?.as_str()
                && challenge.public_key == public_key
                && challenge.device_id == pairing::public_key_id(&public_key)
                && challenge.operation == operation
                && challenge.request_id == request_id
                && deadline_valid(challenge.expires_at, pairing::CHALLENGE_SECONDS)?
                && pairing::is_identity(&challenge.challenge_id)
                && pairing::is_identity(&challenge.nonce)
                && challenge
                    .authorization_id
                    .as_ref()
                    .is_none_or(|id| pairing::is_identity(id)),
            "device proof does not match the connection"
        );
        let signature = key
            .sign(&challenge.payload(&credential)?)
            .to_bytes()
            .to_vec();
        send(
            &mut socket,
            2,
            Method::ConnectProve,
            DeviceProof {
                challenge_id: challenge.challenge_id,
                signature,
            },
        )
        .await?;
        match result(&mut socket, 2).await? {
            ConnectResult::Pending {
                request_id: pending,
                device_id,
                expires_at,
            } => {
                ensure!(
                    operation == DeviceOperation::Enroll
                        && pairing::is_identity(&pending)
                        && device_id == pairing::public_key_id(&public_key)
                        && deadline_valid(expires_at, pairing::PENDING_SECONDS)?,
                    "invalid pairing request"
                );
                device.gateway = Some(hello.gateway_id.clone());
                store
                    .context("pairing requires persistent storage")?
                    .save("device.json", device)?;
                if let Some(Pairing::Local(local)) = enrollment {
                    local.approve(&pending, &device_id).await?;
                } else {
                    eprintln!("Approve on the Gateway host: phi gateway devices approve {pending}");
                }
                let mut sequence = initial_sequence;
                loop {
                    let remaining = expires_at.saturating_sub(now()?);
                    ensure!(remaining > 0, "pairing request expired");
                    let resolution: Event<PairingResolution> = serde_json::from_str(
                        &receive(&mut socket, Duration::from_secs(remaining)).await?,
                    )?;
                    sequence += 1;
                    ensure!(
                        resolution.event == EventName::PairingResolved
                            && resolution.sequence() == sequence
                            && resolution.payload.request_id == pending
                            && resolution.payload.device_id == device_id,
                        "pairing resolution mismatch"
                    );
                    match resolution.payload.status {
                        PairingStatus::Approved => break,
                        PairingStatus::Pending => continue,
                        PairingStatus::Rejected => bail!("pairing rejected by operator"),
                        PairingStatus::Expired => bail!("pairing request expired"),
                    }
                }
                let _ = socket.close(None).await;
                request_id = Some(pending);
                operation = DeviceOperation::Claim;
            }
            ConnectResult::Ready {
                hello: ready,
                device_credential,
            } => {
                ensure!(
                    operation != DeviceOperation::Enroll
                        && ready.protocol == protocol::VERSION
                        && ready.gateway_id == hello.gateway_id
                        && ready.boot_id == hello.boot_id
                        && ready.connection_id == hello.connection_id
                        && ready.capabilities.iter().any(|capability| matches!(
                            capability,
                            protocol::Capability::HostedSessions
                        ))
                        && !ready.capabilities.iter().any(|capability| matches!(
                            capability,
                            protocol::Capability::LocalManagement
                        )),
                    "invalid device authorization"
                );
                if let Some(new_credential) = device_credential {
                    ensure!(
                        pairing::is_identity(&new_credential),
                        "invalid issued device credential"
                    );
                    device.credential = Some(new_credential);
                } else {
                    ensure!(
                        operation == DeviceOperation::Reconnect,
                        "missing device credential"
                    );
                }
                device.gateway = Some(hello.gateway_id);
                if let Some(store) = store {
                    store.save("device.json", device)?;
                }
                return Ok(socket);
            }
            _ => bail!("unexpected device authentication result"),
        }
    }
}

pub(crate) struct DisplayWatch {
    socket: Socket,
    session: String,
    sequence: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selection {
    session_id: String,
    watch_id: RequestId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DisplayEnvelope {
    session_id: String,
    watch_id: RequestId,
    event: Value,
}

impl DisplayWatch {
    pub async fn open(endpoint: &str, device: &Device, session: &str) -> anyhow::Result<Self> {
        session_path(session, "snapshot")?;
        let mut device = Device {
            secret: device.secret,
            gateway: device.gateway.clone(),
            credential: device.credential.clone(),
        };
        let mut socket = authenticated_socket(endpoint, &mut device, None, None).await?;
        send(
            &mut socket,
            WATCH_REQUEST_ID,
            Method::SessionWatch,
            serde_json::json!({"sessionId":session}),
        )
        .await?;
        let response: Response<Selection> =
            serde_json::from_str(&receive(&mut socket, AUTH_TIMEOUT).await?)?;
        let Response::Success(response) = response else {
            bail!("Gateway watch rejected")
        };
        ensure!(
            Some(response.id) == RequestId::new(WATCH_REQUEST_ID)
                && response.payload.session_id == session
                && Some(response.payload.watch_id) == RequestId::new(WATCH_REQUEST_ID),
            "Gateway watch identity mismatch"
        );
        Ok(Self {
            socket,
            session: session.into(),
            sequence: 1,
        })
    }

    pub async fn next(&mut self) -> anyhow::Result<Value> {
        loop {
            match self
                .socket
                .next()
                .await
                .context("Gateway display stream disconnected")??
            {
                Message::Text(text) => {
                    let event: Event<DisplayEnvelope> = serde_json::from_str(&text)?;
                    ensure!(
                        event.event == EventName::SessionEvent
                            && Some(event.sequence()) == self.sequence.checked_add(1)
                            && event.payload.session_id == self.session
                            && Some(event.payload.watch_id) == RequestId::new(WATCH_REQUEST_ID),
                        "Gateway display stream identity or sequence mismatch"
                    );
                    self.sequence = event.sequence();
                    let value = &event.payload.event;
                    ensure!(
                        value["kind"] != "phi" || value["sessionId"] == self.session,
                        "Gateway display event Session mismatch"
                    );
                    return Ok(event.payload.event);
                }
                Message::Ping(bytes) => self.socket.send(Message::Pong(bytes)).await?,
                Message::Pong(_) => {}
                _ => bail!("Gateway display stream closed"),
            }
        }
    }
}

fn deadline_valid(expires_at: u64, lifetime: u64) -> anyhow::Result<bool> {
    let now = now()?;
    Ok(expires_at > now && expires_at <= now.saturating_add(lifetime))
}

fn now() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

async fn connect(base: &Url) -> anyhow::Result<Socket> {
    let mut url = base.join("ws")?;
    url.set_scheme("ws")
        .map_err(|_| anyhow::anyhow!("invalid WS scheme"))?;
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", protocol::SUBPROTOCOL.parse()?);
    request
        .headers_mut()
        .insert("Origin", base.origin().ascii_serialization().parse()?);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RESPONSE_BYTES))
        .max_frame_size(Some(MAX_RESPONSE_BYTES));
    let (socket, response) = tokio::time::timeout(
        AUTH_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await??;
    ensure!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .is_some_and(|value| value == protocol::SUBPROTOCOL),
        "Gateway subprotocol mismatch"
    );
    Ok(socket)
}

async fn receive(socket: &mut Socket, deadline: Duration) -> anyhow::Result<String> {
    tokio::time::timeout(deadline, async {
        loop {
            match socket.next().await.context("Gateway disconnected")?? {
                Message::Text(text) => {
                    ensure!(
                        text.len() <= MAX_AUTH_BYTES,
                        "authentication frame exceeds limit"
                    );
                    return Ok(text.to_string());
                }
                Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await?,
                Message::Pong(_) => {}
                _ => bail!("unexpected authentication frame"),
            }
        }
    })
    .await
    .context("Gateway authentication timed out")?
}

async fn send<P: serde::Serialize>(
    socket: &mut Socket,
    id: u64,
    method: Method,
    params: P,
) -> anyhow::Result<()> {
    let text = serde_json::to_string(&Request::new(
        RequestId::new(id).context("invalid request ID")?,
        method,
        params,
    ))?;
    ensure!(
        text.len() <= MAX_AUTH_BYTES,
        "authentication request exceeds limit"
    );
    tokio::time::timeout(AUTH_TIMEOUT, socket.send(Message::Text(text.into()))).await??;
    Ok(())
}

async fn result(socket: &mut Socket, id: u64) -> anyhow::Result<ConnectResult> {
    let response: Response<ConnectResult> =
        serde_json::from_str(&receive(socket, AUTH_TIMEOUT).await?)?;
    match response {
        Response::Success(response) => {
            ensure!(
                Some(response.id) == RequestId::new(id),
                "Gateway response ID mismatch"
            );
            Ok(response.payload)
        }
        Response::Failure(response) => {
            ensure!(
                Some(response.id) == RequestId::new(id),
                "Gateway response ID mismatch"
            );
            bail!("Gateway authentication rejected: {}", response.error.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[expect(
        clippy::result_large_err,
        reason = "tungstenite handshake callback requires its response error type"
    )]
    async fn device_enrollment_claim_and_reconnect_are_proof_bound_and_persisted() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        use protocol::{Capability, Hello, Limits};
        use tokio_tungstenite::tungstenite::handshake::server::{
            Request as UpgradeRequest, Response as UpgradeResponse,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let audience = format!("{endpoint}/ws");
        let serving = tokio::spawn(async move {
            for (connection, operation) in [
                DeviceOperation::Enroll,
                DeviceOperation::Claim,
                DeviceOperation::Reconnect,
                DeviceOperation::Reconnect,
            ]
            .into_iter()
            .enumerate()
            {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &UpgradeRequest, mut response: UpgradeResponse| {
                        assert_eq!(
                            request.headers()["sec-websocket-protocol"],
                            protocol::SUBPROTOCOL
                        );
                        assert!(request.headers().contains_key("origin"));
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            protocol::SUBPROTOCOL.parse().unwrap(),
                        );
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let challenge = ConnectChallenge {
                    min_protocol: 1,
                    max_protocol: 1,
                    gateway_id: "c".repeat(64),
                    boot_id: "d".repeat(64),
                    connection_id: "e".repeat(64),
                    nonce: "f".repeat(64),
                    expires_at: now().unwrap() + 60,
                };
                socket
                    .send(Message::Text(
                        serde_json::to_string(
                            &Event::new(EventName::ConnectChallenge, 1, &challenge).unwrap(),
                        )
                        .unwrap()
                        .into(),
                    ))
                    .await
                    .unwrap();
                let connect: Request<Connect> =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(connect.method, Method::Connect);
                let (credential, public_key, request_id) = match connect.params.auth {
                    ConnectAuth::Enroll {
                        credential,
                        public_key,
                    } if operation == DeviceOperation::Enroll => (credential, public_key, None),
                    ConnectAuth::Claim {
                        credential,
                        public_key,
                        request_id,
                    } if operation == DeviceOperation::Claim => {
                        (credential, public_key, Some(request_id))
                    }
                    ConnectAuth::Device {
                        credential,
                        public_key,
                    } if operation == DeviceOperation::Reconnect => (credential, public_key, None),
                    _ => panic!("client used wrong authentication kind"),
                };
                assert_eq!(
                    credential,
                    if operation == DeviceOperation::Reconnect {
                        "b".repeat(64)
                    } else {
                        "a".repeat(64)
                    }
                );
                let proof = pairing::DeviceChallenge {
                    gateway_id: challenge.gateway_id.clone(),
                    boot_id: challenge.boot_id.clone(),
                    connection_id: challenge.connection_id.clone(),
                    audience: audience.clone(),
                    challenge_id: "1".repeat(64),
                    nonce: "2".repeat(64),
                    device_id: pairing::public_key_id(&public_key),
                    public_key,
                    operation,
                    request_id,
                    expires_at: now().unwrap() + 60,
                    authorization_id: None,
                };
                let payload = proof.payload(&credential).unwrap();
                socket
                    .send(Message::Text(
                        serde_json::to_string(&Response::success(
                            connect.id,
                            ConnectResult::ProofRequired { challenge: proof },
                        ))
                        .unwrap()
                        .into(),
                    ))
                    .await
                    .unwrap();
                let request: Request<DeviceProof> =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request.method, Method::ConnectProve);
                assert_eq!(request.params.challenge_id, "1".repeat(64));
                VerifyingKey::from_bytes(&public_key)
                    .unwrap()
                    .verify(
                        &payload,
                        &Signature::from_slice(&request.params.signature).unwrap(),
                    )
                    .unwrap();
                if operation == DeviceOperation::Enroll {
                    let pending = ConnectResult::Pending {
                        request_id: "3".repeat(64),
                        device_id: pairing::public_key_id(&public_key),
                        expires_at: now().unwrap() + 60,
                    };
                    socket
                        .send(Message::Text(
                            serde_json::to_string(&Response::success(request.id, pending))
                                .unwrap()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    let resolution = PairingResolution {
                        request_id: "3".repeat(64),
                        device_id: pairing::public_key_id(&public_key),
                        status: PairingStatus::Approved,
                    };
                    socket
                        .send(Message::Text(
                            serde_json::to_string(
                                &Event::new(EventName::PairingResolved, 2, resolution).unwrap(),
                            )
                            .unwrap()
                            .into(),
                        ))
                        .await
                        .unwrap();
                } else {
                    let ready = ConnectResult::Ready {
                        hello: Hello {
                            protocol: 1,
                            gateway_id: challenge.gateway_id,
                            boot_id: challenge.boot_id,
                            connection_id: challenge.connection_id,
                            capabilities: vec![Capability::HostedSessions],
                            limits: Limits::default(),
                        },
                        device_credential: (operation == DeviceOperation::Claim)
                            .then(|| "b".repeat(64)),
                    };
                    socket
                        .send(Message::Text(
                            serde_json::to_string(&Response::success(request.id, ready))
                                .unwrap()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
                if connection == 3 {
                    let request: Request<Value> = serde_json::from_str(
                        socket.next().await.unwrap().unwrap().to_text().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(request.method, Method::SessionWatch);
                    assert_eq!(request.params["sessionId"], "session-1");
                    socket
                        .send(Message::Text(
                            serde_json::to_string(&Response::success(
                                request.id,
                                serde_json::json!({"sessionId":"session-1","watchId":request.id}),
                            ))
                            .unwrap()
                            .into(),
                        ))
                        .await
                        .unwrap();
                    for (sequence, session) in [(2, "session-1"), (4, "session-1")] {
                        let event = Event::new(EventName::SessionEvent, sequence,
                            serde_json::json!({"sessionId":session,"watchId":request.id,"event":{
                                "kind":"phi","sessionId":session,"event":{"type":"assistant_delta","display":"x".repeat(5000)}
                            }})).unwrap();
                        socket
                            .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                            .await
                            .unwrap();
                    }
                }
            }
        });
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut device = Device::generate().unwrap();
        authenticate(
            &endpoint,
            &mut device,
            &store,
            Some(Pairing::Manual(&"a".repeat(64))),
        )
        .await
        .unwrap();
        let mut restored: Device = store.load("device.json").unwrap().unwrap();
        assert_eq!(restored.gateway.as_deref(), Some("c".repeat(64).as_str()));
        assert_eq!(
            restored.credential.as_deref(),
            Some("b".repeat(64).as_str())
        );
        authenticate(&endpoint, &mut restored, &store, None)
            .await
            .unwrap();
        let mut watch = DisplayWatch::open(&endpoint, &restored, "session-1")
            .await
            .unwrap();
        assert_eq!(
            watch.next().await.unwrap()["event"]["display"]
                .as_str()
                .unwrap()
                .len(),
            5000
        );
        assert!(watch.next().await.is_err());
        serving.await.unwrap();
    }

    #[tokio::test]
    #[expect(clippy::result_large_err, reason = "tungstenite callback contract")]
    async fn reconnect_rejects_a_different_gateway_before_sending_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(stream,
                |_: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response.headers_mut().insert("sec-websocket-protocol", protocol::SUBPROTOCOL.parse().unwrap());
                    Ok(response)
                }).await.unwrap();
            let challenge = ConnectChallenge {
                min_protocol: 1,
                max_protocol: 1,
                gateway_id: "c".repeat(64),
                boot_id: "d".repeat(64),
                connection_id: "e".repeat(64),
                nonce: "f".repeat(64),
                expires_at: now().unwrap() + 60,
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(
                        &Event::new(EventName::ConnectChallenge, 1, challenge).unwrap(),
                    )
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let frame = socket.next().await;
            assert!(!matches!(frame, Some(Ok(Message::Text(_)))));
        });
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut device = Device::generate().unwrap();
        device.gateway = Some("9".repeat(64));
        device.credential = Some("a".repeat(64));
        let error = authenticate(&endpoint, &mut device, &store, None)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Gateway identity or protocol mismatch")
        );
        assert_eq!(device.gateway.as_deref(), Some("9".repeat(64).as_str()));
        serving.await.unwrap();
    }
}
