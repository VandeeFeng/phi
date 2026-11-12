//! Version-1 Remote contracts, independent of socket and UI ownership.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const VERSION: u32 = 1;
pub const WS_PATH: &str = "/ws";
pub const SUBPROTOCOL: &str = "phi.remote.v1";
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const AUTH_BYTES: usize = 4 * 1024;
pub(crate) const REQUEST_BYTES: usize = 16 * 1024;
pub(crate) const SERVER_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const QUEUE_MESSAGES: usize = 256;
pub(crate) const QUEUE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const CONNECTIONS: usize = 256;
pub(crate) const PREAUTH_CONNECTIONS: usize = 64;
pub(crate) const PREAUTH_PER_SOURCE: usize = 8;
pub(crate) const CHALLENGE_LIFETIME: Duration = Duration::from_secs(60);
pub(crate) const CHALLENGES: usize = 256;
pub(crate) const CHALLENGES_PER_SOURCE: usize = 8;
pub(crate) const AUTH_SOURCES: usize = 1024;
pub(crate) const AUTH_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const AUTH_ATTEMPTS: usize = 30;
pub(crate) const WAITERS_PER_REQUEST: usize = 2;
pub(crate) const ORDINARY_PER_CONNECTION: usize = 16;
pub(crate) const CONTROL_PER_CONNECTION: usize = 4;
pub(crate) const CONTROL_REQUESTS: usize = 16;
pub(crate) const STATUS_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(25);
pub(crate) const PONG_TIMEOUT: Duration = Duration::from_secs(25);
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(25);
pub(crate) const CLOSE_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub auth_bytes: usize,
    pub request_bytes: usize,
    pub server_bytes: usize,
    pub queue_messages: usize,
    pub queue_bytes: usize,
    pub ordinary_requests: usize,
    pub control_requests: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            auth_bytes: AUTH_BYTES,
            request_bytes: REQUEST_BYTES,
            server_bytes: SERVER_BYTES,
            queue_messages: QUEUE_MESSAGES,
            queue_bytes: QUEUE_BYTES,
            ordinary_requests: ORDINARY_PER_CONNECTION,
            control_requests: CONTROL_PER_CONNECTION,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hello {
    pub protocol: u32,
    pub gateway_id: String,
    pub boot_id: String,
    pub connection_id: String,
    pub capabilities: Vec<Capability>,
    pub limits: Limits,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Capability {
    HostedSessions,
    LocalManagement,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ConnectResult {
    ProofRequired {
        challenge: crate::pairing::DeviceChallenge,
    },
    Ready {
        hello: Hello,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_credential: Option<String>,
    },
    Pending {
        request_id: String,
        device_id: String,
        expires_at: u64,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingResolution {
    pub request_id: String,
    pub device_id: String,
    pub status: crate::pairing::PairingStatus,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionParams {
    pub session_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestId(u64);

impl RequestId {
    pub fn new(value: u64) -> Option<Self> {
        (value > 0 && value <= MAX_SAFE_INTEGER).then_some(Self(value))
    }
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let parsed = value.parse::<u64>().ok().and_then(Self::new);
        parsed
            .filter(|id| id.0.to_string() == value)
            .ok_or_else(|| de::Error::custom("invalid Remote request ID"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    #[serde(rename = "connect")]
    Connect,
    #[serde(rename = "connect.prove")]
    ConnectProve,
    #[serde(rename = "pairing.status")]
    PairingStatus,
    #[serde(rename = "session.watch")]
    SessionWatch,
    #[serde(rename = "session.unwatch")]
    SessionUnwatch,
}

impl Method {
    pub(crate) fn is_watch(self) -> bool {
        matches!(self, Self::SessionWatch | Self::SessionUnwatch)
    }
}

#[derive(Serialize, Deserialize)]
enum RequestTag {
    #[serde(rename = "req")]
    Request,
}

#[derive(Serialize, Deserialize)]
enum ResponseTag {
    #[serde(rename = "res")]
    Response,
}

#[derive(Serialize, Deserialize)]
enum EventTag {
    #[serde(rename = "event")]
    Event,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request<P> {
    #[serde(rename = "type")]
    kind: RequestTag,
    pub id: RequestId,
    pub method: Method,
    pub params: P,
}

impl<P> Request<P> {
    pub fn new(id: RequestId, method: Method, params: P) -> Self {
        Self {
            kind: RequestTag::Request,
            id,
            method,
            params,
        }
    }
}

struct OutcomeFlag<const VALUE: bool>;

impl<const VALUE: bool> Serialize for OutcomeFlag<VALUE> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(VALUE)
    }
}

impl<'de, const VALUE: bool> Deserialize<'de> for OutcomeFlag<VALUE> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? == VALUE {
            Ok(Self)
        } else {
            Err(de::Error::custom("invalid Remote response outcome"))
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Success<P> {
    #[serde(rename = "type")]
    kind: ResponseTag,
    pub id: RequestId,
    ok: OutcomeFlag<true>,
    pub payload: P,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    #[serde(rename = "type")]
    kind: ResponseTag,
    pub id: RequestId,
    ok: OutcomeFlag<false>,
    pub error: RemoteError,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<P> {
    Success(Success<P>),
    Failure(Failure),
}

impl<P> Response<P> {
    pub fn success(id: RequestId, payload: P) -> Self {
        Self::Success(Success {
            kind: ResponseTag::Response,
            id,
            ok: OutcomeFlag,
            payload,
        })
    }

    pub fn failure(id: RequestId, error: RemoteError) -> Self {
        Self::Failure(Failure {
            kind: ResponseTag::Response,
            id,
            ok: OutcomeFlag,
            error,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    ProtocolMismatch,
    Unauthenticated,
    PairingPending,
    PairingRejected,
    PairingExpired,
    Revoked,
    StaleCredential,
    Forbidden,
    NotFound,
    Busy,
    LimitExceeded,
    Unavailable,
    Internal,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ErrorCode {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventName {
    #[serde(rename = "connect.challenge")]
    ConnectChallenge,
    #[serde(rename = "pairing.resolved")]
    PairingResolved,
    #[serde(rename = "session.changed")]
    SessionChanged,
    #[serde(rename = "session.event")]
    SessionEvent,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event<P> {
    #[serde(rename = "type")]
    kind: EventTag,
    pub event: EventName,
    #[serde(deserialize_with = "deserialize_sequence")]
    seq: u64,
    pub payload: P,
}

fn deserialize_sequence<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value > 0 && value <= MAX_SAFE_INTEGER {
        Ok(value)
    } else {
        Err(de::Error::custom("invalid Remote event sequence"))
    }
}

impl<P> Event<P> {
    pub fn sequence(&self) -> u64 {
        self.seq
    }

    pub fn new(event: EventName, seq: u64, payload: P) -> Option<Self> {
        (seq > 0 && seq <= MAX_SAFE_INTEGER).then_some(Self {
            kind: EventTag::Event,
            event,
            seq,
            payload,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Connect {
    pub min_protocol: u32,
    pub max_protocol: u32,
    pub auth: ConnectAuth,
}

impl Connect {
    pub fn negotiate(&self) -> Result<u32, ErrorCode> {
        if self.min_protocol <= VERSION && self.max_protocol >= VERSION {
            Ok(VERSION)
        } else {
            Err(ErrorCode::ProtocolMismatch)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ConnectAuth {
    Local {
        credential: String,
    },
    Device {
        credential: String,
        public_key: [u8; 32],
    },
    Enroll {
        credential: String,
        public_key: [u8; 32],
    },
    Claim {
        credential: String,
        public_key: [u8; 32],
        request_id: String,
    },
    Recover {
        credential: String,
        public_key: [u8; 32],
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectChallenge {
    pub min_protocol: u32,
    pub max_protocol: u32,
    pub gateway_id: String,
    pub boot_id: String,
    pub connection_id: String,
    pub nonce: String,
    pub expires_at: u64,
}

#[derive(Default)]
pub struct RequestOrder {
    high_water: Option<RequestId>,
}

impl RequestOrder {
    /// Reserve before dispatch, including requests rejected by admission.
    pub fn reserve(&mut self, id: RequestId) -> Result<(), ErrorCode> {
        if self.high_water.is_some_and(|previous| id <= previous) {
            return Err(ErrorCode::InvalidRequest);
        }
        self.high_water = Some(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    #[test]
    fn shared_envelope_fixtures_round_trip_and_reject_ambiguity() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../fixtures/remote-v1.json")).unwrap();
        for value in fixtures["valid"].as_array().unwrap() {
            let encoded = match value["type"].as_str().unwrap() {
                "req" => serde_json::to_value(
                    serde_json::from_value::<Request<Value>>(value.clone()).unwrap(),
                )
                .unwrap(),
                "res" => serde_json::to_value(
                    serde_json::from_value::<Response<Value>>(value.clone()).unwrap(),
                )
                .unwrap(),
                "event" => serde_json::to_value(
                    serde_json::from_value::<Event<Value>>(value.clone()).unwrap(),
                )
                .unwrap(),
                _ => unreachable!(),
            };
            assert_eq!(&encoded, value);
        }
        for value in fixtures["connectValid"].as_array().unwrap() {
            let connect: Connect = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(connect.negotiate(), Ok(VERSION));
            assert_eq!(serde_json::to_value(connect).unwrap(), *value);
        }
        for value in fixtures["connectInvalid"].as_array().unwrap() {
            assert!(serde_json::from_value::<Connect>(value.clone()).is_err());
        }
        for value in fixtures["invalid"].as_array().unwrap() {
            assert!(serde_json::from_value::<Request<Value>>(value.clone()).is_err());
            assert!(serde_json::from_value::<Response<Value>>(value.clone()).is_err());
            assert!(serde_json::from_value::<Event<Value>>(value.clone()).is_err());
        }
    }

    #[test]
    fn authentication_is_strict_and_protocol_does_not_silently_fall_back() {
        let mut value = json!({"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"test-only"}});
        assert_eq!(
            serde_json::from_value::<Connect>(value.clone())
                .unwrap()
                .negotiate(),
            Ok(VERSION)
        );
        value["minProtocol"] = json!(2);
        assert_eq!(
            serde_json::from_value::<Connect>(value.clone())
                .unwrap()
                .negotiate(),
            Err(ErrorCode::ProtocolMismatch)
        );
        value["auth"]["publicKey"] = json!(vec![0; 32]);
        assert!(serde_json::from_value::<Connect>(value).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
        #[test]
        fn request_reservation_never_moves_backwards(values in prop::collection::vec(1..=MAX_SAFE_INTEGER, 0..100)) {
            let mut order = RequestOrder::default();
            let mut maximum = 0;
            for value in values {
                let result = order.reserve(RequestId::new(value).unwrap());
                prop_assert_eq!(result.is_ok(), value > maximum);
                maximum = maximum.max(value);
            }
        }
    }
}
