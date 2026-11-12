use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{protocol as policy, protocol::ErrorCode};

pub const CHALLENGE_SECONDS: u64 = policy::CHALLENGE_LIFETIME.as_secs();
pub const PENDING_SECONDS: u64 = 600;
pub const MAX_PENDING: usize = 64;
pub const MAX_APPROVED: usize = 256;
pub const MAX_AUTH_BODY: usize = 4096;
const PROOF_DOMAIN: &str = "phi.gateway.device-proof";
const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceOperation {
    Enroll,
    Claim,
    Reconnect,
    Recover,
}

impl DeviceOperation {
    fn auth_kind(self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Claim => "claim",
            Self::Reconnect => "device",
            Self::Recover => "recover",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChallengeRequest {
    pub public_key: [u8; 32],
    pub operation: DeviceOperation,
    pub request_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceChallenge {
    pub gateway_id: String,
    pub boot_id: String,
    pub connection_id: String,
    pub audience: String,
    pub challenge_id: String,
    pub nonce: String,
    pub device_id: String,
    pub public_key: [u8; 32],
    pub operation: DeviceOperation,
    pub request_id: Option<String>,
    pub expires_at: u64,
    pub authorization_id: Option<String>,
}

impl DeviceChallenge {
    pub fn payload(&self, credential: &str) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&serde_json::json!([
            PROOF_DOMAIN,
            PROTOCOL_VERSION,
            self.operation.auth_kind(),
            self.gateway_id,
            self.boot_id,
            self.connection_id,
            self.audience,
            self.challenge_id,
            self.nonce,
            self.device_id,
            self.public_key,
            self.operation,
            self.request_id,
            self.expires_at,
            self.authorization_id,
            credential_digest(credential)
        ]))?)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceProof {
    pub challenge_id: String,
    pub signature: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PairingOutcome {
    Pending { request_id: String, expires_at: u64 },
    Authorized { device_id: String },
    Credential { device_id: String, token: String },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingReview {
    pub request_id: String,
    pub device_id: String,
    pub requested_at: u64,
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovedReview {
    pub device_id: String,
    pub approved_at: u64,
    pub expires_at: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceList {
    pub pending: Vec<PendingReview>,
    pub approved: Vec<ApprovedReview>,
}

#[derive(Clone)]
pub struct DeviceAuthorization {
    pub device_id: String,
    pub approval_generation: String,
    pub token_digest: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PairingStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

/// The backend commits authorization before returning any successful outcome.
pub trait PairingStore: Send + Sync + 'static {
    fn gateway_id(&self) -> anyhow::Result<String>;
    fn admit(&self, request: &ChallengeRequest, credential: &str)
    -> anyhow::Result<Option<String>>;
    fn complete(
        &self,
        challenge: &DeviceChallenge,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<PairingOutcome>;
    fn authenticate(&self, credential: &str) -> anyhow::Result<DeviceAuthorization>;
    fn authorize(&self, authorization: &DeviceAuthorization) -> anyhow::Result<()>;
    fn pairing_status(
        &self,
        request_id: &str,
        device_id: &str,
        now: u64,
    ) -> anyhow::Result<PairingStatus>;
    fn devices(&self, now: u64) -> anyhow::Result<DeviceList>;
    fn approve(&self, request_id: &str, now: u64) -> anyhow::Result<()>;
    fn reject(&self, request_id: &str, now: u64) -> anyhow::Result<()>;
    fn revoke(&self, device_id: &str) -> anyhow::Result<()>;
}

struct OutstandingChallenge {
    challenge: DeviceChallenge,
    deadline: Instant,
    source: IpAddr,
    credential_digest: String,
}

struct SourceBudget {
    started: Instant,
    attempts: usize,
}

#[derive(Default)]
struct ProofState {
    challenges: HashMap<String, OutstandingChallenge>,
    sources: HashMap<IpAddr, SourceBudget>,
}

pub struct DeviceAuthenticator<S: ?Sized> {
    store: Arc<S>,
    state: Mutex<ProofState>,
}

impl<S: PairingStore + ?Sized> DeviceAuthenticator<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            state: Mutex::new(ProofState::default()),
        }
    }

    pub fn challenge(
        &self,
        source: IpAddr,
        audience: &str,
        request: ChallengeRequest,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<DeviceChallenge> {
        self.challenge_bound(source, audience, "", "", request, credential, now)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "explicit signed connection context"
    )]
    pub fn challenge_bound(
        &self,
        source: IpAddr,
        audience: &str,
        boot_id: &str,
        connection_id: &str,
        request: ChallengeRequest,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<DeviceChallenge> {
        self.budget(source)?;
        ensure!(is_identity(credential), "invalid device authentication");
        ensure!(
            request.request_id.as_ref().is_none_or(|id| is_identity(id)),
            "invalid pairing reference"
        );
        ensure!(
            (request.operation == DeviceOperation::Claim) == request.request_id.is_some(),
            "invalid pairing operation reference"
        );
        let key =
            VerifyingKey::from_bytes(&request.public_key).context("invalid device public key")?;
        ensure!(!key.is_weak(), "invalid device public key");
        let authorization_id = self.store.admit(&request, credential)?;
        let challenge = DeviceChallenge {
            gateway_id: self.store.gateway_id()?,
            boot_id: boot_id.to_owned(),
            connection_id: connection_id.to_owned(),
            audience: audience.to_owned(),
            challenge_id: random_credential()?,
            nonce: random_credential()?,
            device_id: public_key_id(&request.public_key),
            public_key: request.public_key,
            authorization_id,
            operation: request.operation,
            request_id: request.request_id,
            expires_at: now
                .checked_add(CHALLENGE_SECONDS)
                .context("invalid authentication clock")?,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("device authentication unavailable"))?;
        state
            .challenges
            .retain(|_, value| value.deadline > Instant::now() && value.challenge.expires_at > now);
        ensure!(
            state.challenges.len() < policy::CHALLENGES,
            ErrorCode::LimitExceeded
        );
        ensure!(
            state
                .challenges
                .values()
                .filter(|value| value.source == source)
                .count()
                < policy::CHALLENGES_PER_SOURCE,
            ErrorCode::LimitExceeded
        );
        ensure!(
            connection_id.is_empty()
                || !state.challenges.values().any(|value| {
                    value.challenge.boot_id == boot_id
                        && value.challenge.connection_id == connection_id
                }),
            "connection already has a challenge"
        );
        state.challenges.insert(
            challenge.challenge_id.clone(),
            OutstandingChallenge {
                challenge: challenge.clone(),
                deadline: Instant::now() + Duration::from_secs(CHALLENGE_SECONDS),
                source,
                credential_digest: credential_digest(credential),
            },
        );
        Ok(challenge)
    }

    pub fn prove(
        &self,
        source: IpAddr,
        audience: &str,
        proof: DeviceProof,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<PairingOutcome> {
        self.prove_bound(source, audience, "", "", proof, credential, now)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "explicit signed connection context"
    )]
    pub fn prove_bound(
        &self,
        source: IpAddr,
        audience: &str,
        boot_id: &str,
        connection_id: &str,
        proof: DeviceProof,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<PairingOutcome> {
        let outstanding = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("device authentication unavailable"))?;
            let challenge_id = if connection_id.is_empty() {
                proof.challenge_id.clone()
            } else {
                state
                    .challenges
                    .iter()
                    .find(|(_, value)| {
                        value.challenge.boot_id == boot_id
                            && value.challenge.connection_id == connection_id
                    })
                    .map(|(id, _)| id.clone())
                    .context("device challenge missing or consumed")?
            };
            state
                .challenges
                .remove(&challenge_id)
                .context("device challenge missing or consumed")?
        };
        let challenge = outstanding.challenge;
        ensure!(
            challenge.challenge_id == proof.challenge_id
                && outstanding.source == source
                && outstanding.deadline > Instant::now()
                && challenge.expires_at > now
                && challenge.audience == audience
                && challenge.boot_id == boot_id
                && challenge.connection_id == connection_id
                && constant_matches(
                    &outstanding.credential_digest,
                    &credential_digest(credential)
                ),
            "device challenge rejected"
        );
        let signature =
            Signature::from_slice(&proof.signature).context("invalid device signature")?;
        VerifyingKey::from_bytes(&challenge.public_key)?
            .verify_strict(&challenge.payload(credential)?, &signature)
            .map_err(|_| anyhow::anyhow!("device proof rejected"))?;
        self.store.complete(&challenge, credential, now)
    }

    pub(crate) fn retire(&self, boot_id: &str, connection_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.challenges.retain(|_, value| {
                value.challenge.boot_id != boot_id || value.challenge.connection_id != connection_id
            });
        }
    }

    pub(crate) fn store(&self) -> &S {
        &self.store
    }

    fn budget(&self, source: IpAddr) -> anyhow::Result<()> {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("device authentication unavailable"))?;
        state
            .sources
            .retain(|_, budget| now.duration_since(budget.started) < policy::AUTH_WINDOW);
        ensure!(
            state.sources.contains_key(&source) || state.sources.len() < policy::AUTH_SOURCES,
            ErrorCode::LimitExceeded
        );
        let budget = state.sources.entry(source).or_insert(SourceBudget {
            started: now,
            attempts: 0,
        });
        ensure!(
            budget.attempts < policy::AUTH_ATTEMPTS,
            ErrorCode::LimitExceeded
        );
        budget.attempts += 1;
        Ok(())
    }
}

pub fn public_key_id(key: &[u8; 32]) -> String {
    format!("{:x}", Sha256::digest(key))
}
pub fn credential_digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
pub fn constant_matches(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}
pub fn is_identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
pub fn random_credential() -> anyhow::Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Gateway random source unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_bytes_and_signature_match_the_shared_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/proof-v1.json")).unwrap();
        let challenge: DeviceChallenge =
            serde_json::from_value(fixture["challenge"].clone()).unwrap();
        let credential = fixture["credential"].as_str().unwrap();
        let payload = challenge.payload(credential).unwrap();
        assert_eq!(payload, fixture["payload"].as_str().unwrap().as_bytes());
        assert_eq!(credential_digest(credential), fixture["credentialDigest"]);
        assert_eq!(public_key_id(&challenge.public_key), challenge.device_id);
        let signature: Vec<u8> = serde_json::from_value(fixture["signature"].clone()).unwrap();
        VerifyingKey::from_bytes(&challenge.public_key)
            .unwrap()
            .verify_strict(&payload, &Signature::from_slice(&signature).unwrap())
            .unwrap();
    }

    #[test]
    fn wire_outcomes_use_one_explicit_camel_case_contract() {
        let pending = serde_json::to_value(PairingOutcome::Pending {
            request_id: "request".into(),
            expires_at: 100,
        })
        .unwrap();
        assert_eq!(
            pending,
            serde_json::json!({"status":"pending","requestId":"request","expiresAt":100})
        );
        let authorized = serde_json::to_value(PairingOutcome::Authorized {
            device_id: "device".into(),
        })
        .unwrap();
        assert_eq!(
            authorized,
            serde_json::json!({"status":"authorized","deviceId":"device"})
        );
        assert!(
            serde_json::from_value::<PairingOutcome>(
                serde_json::json!({"status":"authorized","deviceId":"device","role":"admin"})
            )
            .is_err()
        );
    }
}
