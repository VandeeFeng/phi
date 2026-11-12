use crate::pairing::{
    ApprovedReview, ChallengeRequest, DeviceChallenge, DeviceList, DeviceOperation, MAX_APPROVED,
    MAX_PENDING, PENDING_SECONDS, PairingOutcome, PairingStore, PendingReview, constant_matches,
    credential_digest, public_key_id, random_credential,
};
use crate::protocol::ErrorCode;
use anyhow::{Context, ensure};

use super::storage::{ApprovedDevice, PairingAuthority, PairingDocument, PendingDevice};

impl ApprovedDevice {
    fn is_current(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|deadline| now < deadline)
    }

    fn matches_credential(&self, digest: &str) -> bool {
        self.token_digest
            .as_ref()
            .is_some_and(|stored| constant_matches(stored, digest))
    }
}

fn check_admission(
    document: &PairingDocument,
    request: &ChallengeRequest,
    credential: &str,
    now: u64,
) -> anyhow::Result<Option<String>> {
    let approved = document
        .approved
        .get(&public_key_id(&request.public_key))
        .filter(|device| device.is_current(now));
    let digest = credential_digest(credential);
    match request.operation {
        DeviceOperation::Enroll | DeviceOperation::Claim | DeviceOperation::Recover => {
            ensure!(
                constant_matches(&credential_digest(&document.entry_token), &digest),
                ErrorCode::Unauthenticated
            );
        }
        DeviceOperation::Reconnect => {
            let approved = approved.context(ErrorCode::Revoked)?;
            ensure!(
                approved.matches_credential(&digest),
                ErrorCode::StaleCredential
            );
        }
    }
    if request.operation == DeviceOperation::Recover {
        ensure!(approved.is_some(), ErrorCode::Revoked);
    }
    Ok(approved.map(|approved| approved.approval_request.clone()))
}

impl PairingStore for PairingAuthority {
    fn gateway_id(&self) -> anyhow::Result<String> {
        Ok(self.snapshot()?.gateway_id)
    }

    fn admit(
        &self,
        request: &ChallengeRequest,
        credential: &str,
    ) -> anyhow::Result<Option<String>> {
        check_admission(&self.snapshot()?, request, credential, self.now()?)
    }

    fn complete(
        &self,
        challenge: &DeviceChallenge,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<PairingOutcome> {
        let request = ChallengeRequest {
            public_key: challenge.public_key,
            operation: challenge.operation,
            request_id: challenge.request_id.clone(),
        };
        self.transition(|document| {
            ensure!(
                document.gateway_id == challenge.gateway_id
                    && challenge.device_id == public_key_id(&challenge.public_key),
                "device proof domain changed"
            );
            let authorization_id = check_admission(document, &request, credential, now)?;
            if matches!(
                challenge.operation,
                DeviceOperation::Recover | DeviceOperation::Reconnect
            ) {
                ensure!(
                    authorization_id == challenge.authorization_id,
                    ErrorCode::Revoked
                );
            }
            if challenge.operation == DeviceOperation::Claim {
                ensure!(
                    challenge
                        .request_id
                        .as_ref()
                        .and_then(|id| document.pending.get(id))
                        .is_none_or(|pending| pending.expires_at > now),
                    ErrorCode::PairingExpired
                );
            }
            document
                .pending
                .retain(|_, pending| pending.expires_at > now);
            match challenge.operation {
                DeviceOperation::Enroll => {
                    ensure!(
                        authorization_id.is_none(),
                        "device already approved; recover its credential"
                    );
                    document.approved.remove(&challenge.device_id);
                    if let Some((request_id, pending)) = document
                        .pending
                        .iter()
                        .find(|(_, pending)| pending.device_id == challenge.device_id)
                    {
                        return Ok(PairingOutcome::Pending {
                            request_id: request_id.clone(),
                            expires_at: pending.expires_at,
                        });
                    }
                    ensure!(
                        document.pending.len() < MAX_PENDING,
                        ErrorCode::LimitExceeded
                    );
                    let request_id = random_credential()?;
                    let expires_at = now
                        .checked_add(PENDING_SECONDS)
                        .context("invalid pairing clock")?;
                    document.pending.insert(
                        request_id.clone(),
                        PendingDevice {
                            device_id: challenge.device_id.clone(),
                            public_key: challenge.public_key,
                            requested_at: now,
                            expires_at,
                        },
                    );
                    Ok(PairingOutcome::Pending {
                        request_id,
                        expires_at,
                    })
                }
                DeviceOperation::Claim => {
                    let request_id = challenge
                        .request_id
                        .as_ref()
                        .context("pairing request required")?;
                    if let Some(pending) = document.pending.get(request_id) {
                        ensure!(
                            pending.public_key == challenge.public_key,
                            "pairing identity mismatch"
                        );
                        return Ok(PairingOutcome::Pending {
                            request_id: request_id.clone(),
                            expires_at: pending.expires_at,
                        });
                    }
                    let approved = document
                        .approved
                        .get_mut(&challenge.device_id)
                        .context(ErrorCode::PairingRejected)?;
                    ensure!(approved.is_current(now), ErrorCode::Revoked);
                    ensure!(
                        approved.approval_request == *request_id
                            && approved.public_key == challenge.public_key,
                        "pairing approval does not match request"
                    );
                    deliver_credential(&challenge.device_id, approved)
                }
                DeviceOperation::Recover => {
                    let approved = document
                        .approved
                        .get_mut(&challenge.device_id)
                        .context(ErrorCode::Revoked)?;
                    deliver_credential(&challenge.device_id, approved)
                }
                DeviceOperation::Reconnect => Ok(PairingOutcome::Authorized {
                    device_id: challenge.device_id.clone(),
                }),
            }
        })
    }

    fn authenticate(
        &self,
        credential: &str,
    ) -> anyhow::Result<crate::pairing::DeviceAuthorization> {
        ensure!(
            crate::pairing::is_identity(credential),
            "device authentication rejected"
        );
        let digest = credential_digest(credential);
        let document = self.snapshot()?;
        let now = self.now()?;
        document
            .approved
            .into_iter()
            .find(|(_, approved)| approved.is_current(now) && approved.matches_credential(&digest))
            .map(|(identity, approved)| crate::pairing::DeviceAuthorization {
                device_id: identity,
                approval_generation: approved.approval_request,
                token_digest: digest,
            })
            .context("device authentication rejected")
    }

    fn authorize(&self, authorization: &crate::pairing::DeviceAuthorization) -> anyhow::Result<()> {
        let document = self.snapshot()?;
        let now = self.now()?;
        let approved = document
            .approved
            .get(&authorization.device_id)
            .filter(|approved| approved.is_current(now))
            .context(ErrorCode::Revoked)?;
        ensure!(
            approved.approval_request == authorization.approval_generation,
            ErrorCode::Revoked
        );
        ensure!(
            approved.matches_credential(&authorization.token_digest),
            ErrorCode::StaleCredential
        );
        Ok(())
    }

    fn pairing_status(
        &self,
        request_id: &str,
        device_id: &str,
        now: u64,
    ) -> anyhow::Result<crate::pairing::PairingStatus> {
        use crate::pairing::PairingStatus;
        let document = self.snapshot()?;
        if let Some(pending) = document.pending.get(request_id) {
            ensure!(pending.device_id == device_id, "pairing identity mismatch");
            return Ok(if pending.expires_at > now {
                PairingStatus::Pending
            } else {
                PairingStatus::Expired
            });
        }
        Ok(
            match document
                .approved
                .get(device_id)
                .filter(|approved| approved.approval_request == request_id)
            {
                Some(approved) if approved.is_current(now) => PairingStatus::Approved,
                Some(_) => PairingStatus::Expired,
                None => PairingStatus::Rejected,
            },
        )
    }

    fn devices(&self, now: u64) -> anyhow::Result<DeviceList> {
        let document = self.snapshot()?;
        Ok(DeviceList {
            pending: document
                .pending
                .into_iter()
                .filter(|(_, pending)| pending.expires_at > now)
                .map(|(request_id, pending)| PendingReview {
                    request_id,
                    device_id: pending.device_id,
                    requested_at: pending.requested_at,
                    expires_at: pending.expires_at,
                })
                .collect(),
            approved: document
                .approved
                .into_iter()
                .map(|(device_id, approved)| ApprovedReview {
                    device_id,
                    approved_at: approved.approved_at,
                    expires_at: approved.expires_at,
                })
                .collect(),
        })
    }

    fn approve(&self, request_id: &str, now: u64) -> anyhow::Result<()> {
        self.transition(|document| {
            let pending = document
                .pending
                .remove(request_id)
                .context(ErrorCode::NotFound)?;
            ensure!(pending.expires_at > now, ErrorCode::PairingExpired);
            document
                .approved
                .retain(|_, approved| approved.is_current(now));
            ensure!(
                document.approved.len() < MAX_APPROVED,
                ErrorCode::LimitExceeded
            );
            ensure!(
                !document.approved.contains_key(&pending.device_id),
                "device already approved"
            );
            document.approved.insert(
                pending.device_id,
                ApprovedDevice {
                    public_key: pending.public_key,
                    approved_at: now,
                    expires_at: self.approval_deadline(now)?,
                    approval_request: request_id.to_owned(),
                    token_digest: None,
                },
            );
            Ok(())
        })
    }

    fn reject(&self, request_id: &str, now: u64) -> anyhow::Result<()> {
        self.transition(|document| {
            let pending = document
                .pending
                .remove(request_id)
                .context(ErrorCode::NotFound)?;
            ensure!(pending.expires_at > now, ErrorCode::PairingExpired);
            Ok(())
        })
    }

    fn revoke(&self, device_id: &str) -> anyhow::Result<()> {
        self.transition(|document| {
            document
                .approved
                .remove(device_id)
                .context(ErrorCode::NotFound)?;
            Ok(())
        })
    }
}

fn deliver_credential(
    identity: &str,
    approved: &mut ApprovedDevice,
) -> anyhow::Result<PairingOutcome> {
    let token = random_credential()?;
    approved.token_digest = Some(credential_digest(&token));
    Ok(PairingOutcome::Credential {
        device_id: identity.to_owned(),
        token,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::deployment::storage::tests::Fixture;
    use crate::pairing::{DeviceAuthenticator, DeviceProof};
    use ed25519_dalek::{Signer, SigningKey};

    const AUDIENCE: &str = "https://phi.example.com";
    const NOW: u64 = crate::deployment::TEST_NOW;

    fn request(
        key: &SigningKey,
        operation: DeviceOperation,
        request_id: Option<&str>,
    ) -> ChallengeRequest {
        ChallengeRequest {
            public_key: key.verifying_key().to_bytes(),
            operation,
            request_id: request_id.map(str::to_owned),
        }
    }

    fn proof(key: &SigningKey, challenge: &DeviceChallenge, credential: &str) -> DeviceProof {
        DeviceProof {
            challenge_id: challenge.challenge_id.clone(),
            signature: key
                .sign(&challenge.payload(credential).unwrap())
                .to_bytes()
                .to_vec(),
        }
    }

    fn exchange(
        auth: &DeviceAuthenticator<PairingAuthority>,
        key: &SigningKey,
        operation: DeviceOperation,
        request_id: Option<&str>,
        credential: &str,
        now: u64,
    ) -> anyhow::Result<PairingOutcome> {
        let source = "192.0.2.1".parse().unwrap();
        let challenge = auth.challenge(
            source,
            AUDIENCE,
            request(key, operation, request_id),
            credential,
            now,
        )?;
        auth.prove(
            source,
            AUDIENCE,
            proof(key, &challenge, credential),
            credential,
            now,
        )
    }

    fn enroll(
        auth: &DeviceAuthenticator<PairingAuthority>,
        key: &SigningKey,
        entry: &str,
        now: u64,
    ) -> String {
        match exchange(auth, key, DeviceOperation::Enroll, None, entry, now).unwrap() {
            PairingOutcome::Pending { request_id, .. } => request_id,
            _ => panic!("enrollment must require approval"),
        }
    }

    fn credential(outcome: PairingOutcome) -> String {
        match outcome {
            PairingOutcome::Credential { token, .. } => token,
            _ => panic!("approved owner must receive a credential"),
        }
    }

    fn timed_authority(fixture: &Fixture, days: u32, clock: &Arc<AtomicU64>) -> PairingAuthority {
        let clock = clock.clone();
        PairingAuthority::open_with_expiry(&fixture.0, days)
            .unwrap()
            .with_clock(move || Ok(clock.load(Ordering::SeqCst)))
    }

    #[test]
    fn approval_recovery_restart_and_revocation_form_one_authority() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let gateway_id = store.gateway_id().unwrap();
        let key = SigningKey::from_bytes(&[17; 32]);
        let identity = public_key_id(&key.verifying_key().to_bytes());
        let auth = DeviceAuthenticator::new(store.clone());
        let request_id = enroll(&auth, &key, &entry, NOW);
        assert!(store.authenticate(&entry).is_err());
        assert_eq!(store.devices(NOW).unwrap().pending.len(), 1);
        assert!(matches!(
            exchange(
                &auth,
                &key,
                DeviceOperation::Claim,
                Some(&request_id),
                &entry,
                NOW
            )
            .unwrap(),
            PairingOutcome::Pending { .. }
        ));
        store.approve(&request_id, NOW).unwrap();
        assert!(store.approve(&request_id, NOW).is_err());
        let first = credential(
            exchange(
                &auth,
                &key,
                DeviceOperation::Claim,
                Some(&request_id),
                &entry,
                NOW,
            )
            .unwrap(),
        );
        let first_authorization = store.authenticate(&first).unwrap();
        assert_eq!(first_authorization.device_id, identity);
        store.authorize(&first_authorization).unwrap();
        let second = credential(
            exchange(
                &auth,
                &key,
                DeviceOperation::Claim,
                Some(&request_id),
                &entry,
                NOW,
            )
            .unwrap(),
        );
        assert!(store.authenticate(&first).is_err());
        assert_eq!(store.authenticate(&second).unwrap().device_id, identity);
        assert!(store.authorize(&first_authorization).is_err());
        let bytes = std::fs::read_to_string(fixture.0.join("pairing.json")).unwrap();
        assert!(!bytes.contains(&first) && !bytes.contains(&second));
        drop(auth);
        drop(store);
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        assert_eq!(store.gateway_id().unwrap(), gateway_id);
        let auth = DeviceAuthenticator::new(store.clone());
        assert!(matches!(
            exchange(&auth, &key, DeviceOperation::Reconnect, None, &second, NOW).unwrap(),
            PairingOutcome::Authorized { .. }
        ));
        let recovered =
            credential(exchange(&auth, &key, DeviceOperation::Recover, None, &entry, NOW).unwrap());
        assert!(store.authenticate(&second).is_err());
        let recovered_authorization = store.authenticate(&recovered).unwrap();
        assert_eq!(recovered_authorization.device_id, identity);
        store.revoke(&identity).unwrap();
        assert!(store.authenticate(&recovered).is_err());
        assert!(store.authorize(&recovered_authorization).is_err());
        assert!(exchange(&auth, &key, DeviceOperation::Recover, None, &entry, NOW).is_err());
        assert!(
            exchange(
                &auth,
                &key,
                DeviceOperation::Claim,
                Some(&request_id),
                &entry,
                NOW
            )
            .is_err()
        );
        drop(auth);
        drop(store);
        assert!(
            PairingAuthority::open(&fixture.0)
                .unwrap()
                .authenticate(&recovered)
                .is_err()
        );
    }

    #[test]
    fn approval_deadlines_survive_restart_and_require_reapproval() {
        const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
        for days in [0, 1] {
            let fixture = Fixture::new();
            let clock = Arc::new(AtomicU64::new(NOW));
            let store = Arc::new(timed_authority(&fixture, days, &clock));
            let entry = store.snapshot().unwrap().entry_token;
            let key = SigningKey::from_bytes(&[37; 32]);
            let identity = public_key_id(&key.verifying_key().to_bytes());
            let auth = DeviceAuthenticator::new(store.clone());
            let request_id = enroll(&auth, &key, &entry, NOW);
            store.approve(&request_id, NOW).unwrap();
            let token = credential(
                exchange(
                    &auth,
                    &key,
                    DeviceOperation::Claim,
                    Some(&request_id),
                    &entry,
                    NOW,
                )
                .unwrap(),
            );
            let original = store.authenticate(&token).unwrap();
            let deadline = if days == 0 {
                None
            } else {
                Some(NOW + u64::from(days) * SECONDS_PER_DAY)
            };
            assert_eq!(store.devices(NOW).unwrap().approved[0].expires_at, deadline);
            drop(auth);
            drop(store);

            let changed_policy = if days == 0 { 1 } else { 0 };
            let store = Arc::new(timed_authority(&fixture, changed_policy, &clock));
            let auth = DeviceAuthenticator::new(store.clone());
            assert_eq!(store.devices(NOW).unwrap().approved[0].expires_at, deadline);
            let before = deadline.map_or(u64::MAX - 60, |deadline| deadline - 1);
            clock.store(before, Ordering::SeqCst);
            store.authorize(&original).unwrap();
            let recovered = credential(
                exchange(&auth, &key, DeviceOperation::Recover, None, &entry, before).unwrap(),
            );
            let authorization = store.authenticate(&recovered).unwrap();
            assert_eq!(
                store.devices(before).unwrap().approved[0].expires_at,
                deadline
            );
            if days == 0 {
                clock.store(u64::MAX, Ordering::SeqCst);
                store.authorize(&authorization).unwrap();
                assert!(store.authenticate(&recovered).is_ok());
                continue;
            }
            let stale_proof = auth
                .challenge(
                    "192.0.2.1".parse().unwrap(),
                    AUDIENCE,
                    request(&key, DeviceOperation::Recover, None),
                    &entry,
                    before,
                )
                .unwrap();
            let deadline = deadline.unwrap();
            clock.store(deadline, Ordering::SeqCst);
            assert!(store.authenticate(&recovered).is_err());
            assert!(store.authorize(&authorization).is_err());
            for operation in [
                DeviceOperation::Reconnect,
                DeviceOperation::Recover,
                DeviceOperation::Claim,
            ] {
                let (credential, reference) = match operation {
                    DeviceOperation::Reconnect => (&recovered, None),
                    DeviceOperation::Recover => (&entry, None),
                    _ => (&entry, Some(request_id.as_str())),
                };
                assert!(exchange(&auth, &key, operation, reference, credential, deadline).is_err());
            }
            assert!(matches!(
                store
                    .pairing_status(&request_id, &identity, deadline)
                    .unwrap(),
                crate::pairing::PairingStatus::Expired
            ));
            let pending = enroll(&auth, &key, &entry, deadline);
            assert!(store.authenticate(&recovered).is_err());
            store.approve(&pending, deadline).unwrap();
            assert!(
                auth.prove(
                    "192.0.2.1".parse().unwrap(),
                    AUDIENCE,
                    proof(&key, &stale_proof, &entry),
                    &entry,
                    deadline
                )
                .is_err()
            );
            let replacement = credential(
                exchange(
                    &auth,
                    &key,
                    DeviceOperation::Claim,
                    Some(&pending),
                    &entry,
                    deadline,
                )
                .unwrap(),
            );
            assert!(store.authenticate(&replacement).is_ok());
            assert!(store.authorize(&authorization).is_err());
        }
    }

    #[test]
    fn signatures_are_single_use_and_bound_to_context() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[19; 32]);
        let stranger = SigningKey::from_bytes(&[20; 32]);
        let source = "192.0.2.1".parse().unwrap();
        let auth = DeviceAuthenticator::new(store.clone());
        for field in [
            "signature",
            "audience",
            "operation",
            "gateway",
            "boot",
            "connection",
            "key",
            "nonce",
            "source",
            "credential",
            "expiry",
        ] {
            let challenge = auth
                .challenge(
                    source,
                    AUDIENCE,
                    request(&key, DeviceOperation::Enroll, None),
                    &entry,
                    NOW,
                )
                .unwrap();
            let mut signed = challenge.clone();
            match field {
                "audience" => signed.audience = "https://other.example.com".into(),
                "operation" => signed.operation = DeviceOperation::Recover,
                "gateway" => signed.gateway_id = "f".repeat(64),
                "boot" => signed.boot_id = "other-boot".into(),
                "connection" => signed.connection_id = "other-connection".into(),
                "key" => signed.public_key = stranger.verifying_key().to_bytes(),
                "nonce" => signed.nonce = "e".repeat(64),
                _ => {}
            }
            let signer = if field == "signature" {
                &stranger
            } else {
                &key
            };
            let from = if field == "source" {
                "192.0.2.2".parse().unwrap()
            } else {
                source
            };
            let credential = if field == "credential" {
                "b".repeat(64)
            } else {
                entry.clone()
            };
            let now = if field == "expiry" {
                NOW + crate::pairing::CHALLENGE_SECONDS
            } else {
                NOW
            };
            assert!(
                auth.prove(
                    from,
                    AUDIENCE,
                    proof(signer, &signed, &entry),
                    &credential,
                    now
                )
                .is_err(),
                "{field}"
            );
            assert!(
                auth.prove(
                    source,
                    AUDIENCE,
                    proof(&key, &challenge, &entry),
                    &entry,
                    NOW
                )
                .is_err(),
                "consumed: {field}"
            );
        }
        assert!(store.devices(NOW).unwrap().pending.is_empty());
    }

    #[test]
    fn approval_and_claim_require_exact_request_and_key() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let owner = SigningKey::from_bytes(&[21; 32]);
        let other = SigningKey::from_bytes(&[22; 32]);
        let auth = DeviceAuthenticator::new(store.clone());
        let pending = enroll(&auth, &owner, &entry, NOW);
        assert_eq!(enroll(&auth, &owner, &entry, NOW), pending);
        assert!(store.approve(&"a".repeat(64), NOW).is_err());
        assert!(
            exchange(
                &auth,
                &other,
                DeviceOperation::Claim,
                Some(&pending),
                &entry,
                NOW
            )
            .is_err()
        );
        store.approve(&pending, NOW).unwrap();
        assert!(
            exchange(
                &auth,
                &other,
                DeviceOperation::Claim,
                Some(&pending),
                &entry,
                NOW
            )
            .is_err()
        );
        let token = credential(
            exchange(
                &auth,
                &owner,
                DeviceOperation::Claim,
                Some(&pending),
                &entry,
                NOW,
            )
            .unwrap(),
        );
        assert!(exchange(&auth, &other, DeviceOperation::Reconnect, None, &token, NOW).is_err());
        let review = serde_json::to_string(&store.devices(NOW).unwrap()).unwrap();
        assert!(
            !review.contains(&token) && !review.contains(&entry) && !review.contains("publicKey")
        );
    }

    #[test]
    fn rejected_or_expired_requests_cannot_be_claimed_or_approved() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let owner = SigningKey::from_bytes(&[23; 32]);
        let auth = DeviceAuthenticator::new(store.clone());
        let first = enroll(&auth, &owner, &entry, NOW);
        store.reject(&first, NOW).unwrap();
        assert!(store.approve(&first, NOW).is_err());
        assert!(
            exchange(
                &auth,
                &owner,
                DeviceOperation::Claim,
                Some(&first),
                &entry,
                NOW
            )
            .is_err()
        );
        let second = enroll(&auth, &owner, &entry, NOW);
        assert_ne!(first, second);
        assert!(store.approve(&second, NOW + PENDING_SECONDS).is_err());
        assert!(
            exchange(
                &auth,
                &owner,
                DeviceOperation::Claim,
                Some(&second),
                &entry,
                NOW + PENDING_SECONDS
            )
            .is_err()
        );
        assert!(
            store
                .devices(NOW + PENDING_SECONDS)
                .unwrap()
                .pending
                .is_empty()
        );
        assert!(store.devices(NOW).unwrap().approved.is_empty());
    }

    #[test]
    fn restart_discards_challenges_without_discarding_pending_requests() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[24; 32]);
        let auth = DeviceAuthenticator::new(store.clone());
        let pending = enroll(&auth, &key, &entry, NOW);
        let source = "192.0.2.1".parse().unwrap();
        let challenge = auth
            .challenge(
                source,
                AUDIENCE,
                request(&key, DeviceOperation::Claim, Some(&pending)),
                &entry,
                NOW,
            )
            .unwrap();
        drop(auth);
        drop(store);
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let auth = DeviceAuthenticator::new(store.clone());
        assert!(
            auth.prove(
                source,
                AUDIENCE,
                proof(&key, &challenge, &entry),
                &entry,
                NOW
            )
            .is_err()
        );
        assert_eq!(store.devices(NOW).unwrap().pending[0].request_id, pending);
        store.approve(&pending, NOW).unwrap();
        let token = credential(
            exchange(
                &auth,
                &key,
                DeviceOperation::Claim,
                Some(&pending),
                &entry,
                NOW,
            )
            .unwrap(),
        );
        assert!(store.authenticate(&token).is_ok());
    }

    #[test]
    fn challenge_floods_and_wrong_entry_credentials_are_bounded() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[25; 32]);
        let source = "192.0.2.1".parse().unwrap();
        let auth = DeviceAuthenticator::new(store.clone());
        let mut accepted = 0;
        for _ in 0..100 {
            if auth
                .challenge(
                    source,
                    AUDIENCE,
                    request(&key, DeviceOperation::Enroll, None),
                    &entry,
                    NOW,
                )
                .is_ok()
            {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 8);
        assert!(store.devices(NOW).unwrap().pending.is_empty());
        let auth = DeviceAuthenticator::new(store.clone());
        for _ in 0..100 {
            assert!(
                auth.challenge(
                    source,
                    AUDIENCE,
                    request(&key, DeviceOperation::Enroll, None),
                    &"b".repeat(64),
                    NOW
                )
                .is_err()
            );
        }
        assert!(
            auth.challenge(
                source,
                AUDIENCE,
                request(&key, DeviceOperation::Enroll, None),
                &entry,
                NOW
            )
            .is_err()
        );
        assert!(store.devices(NOW).unwrap().pending.is_empty());
    }

    #[test]
    fn authentication_source_capacity_never_evicts_an_active_budget() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[30; 32]);
        let auth = DeviceAuthenticator::new(store.clone());
        let invalid_credential = "b".repeat(64);
        const AUTH_SOURCE_LIMIT: u32 = 1024;
        let first_source = std::net::Ipv4Addr::new(192, 0, 2, 0);
        for index in 0..AUTH_SOURCE_LIMIT {
            let source = std::net::Ipv4Addr::from(u32::from(first_source) + index).into();
            let error = auth
                .challenge(
                    source,
                    AUDIENCE,
                    request(&key, DeviceOperation::Enroll, None),
                    &invalid_credential,
                    NOW,
                )
                .err()
                .unwrap();
            assert_ne!(
                error.downcast_ref::<crate::protocol::ErrorCode>(),
                Some(&crate::protocol::ErrorCode::LimitExceeded)
            );
        }
        let error = auth
            .challenge(
                "198.51.100.1".parse().unwrap(),
                AUDIENCE,
                request(&key, DeviceOperation::Enroll, None),
                &entry,
                NOW,
            )
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<crate::protocol::ErrorCode>(),
            Some(&crate::protocol::ErrorCode::LimitExceeded)
        );
        assert!(
            auth.challenge(
                "192.0.2.0".parse().unwrap(),
                AUDIENCE,
                request(&key, DeviceOperation::Enroll, None),
                &entry,
                NOW,
            )
            .is_ok()
        );
        assert!(store.devices(NOW).unwrap().pending.is_empty());
    }

    #[test]
    fn pending_capacity_requires_expiry_before_new_enrollment() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        for index in 0..MAX_PENDING {
            let key = SigningKey::from_bytes(&[u8::try_from(index).unwrap(); 32]);
            let auth = DeviceAuthenticator::new(store.clone());
            enroll(&auth, &key, &entry, NOW);
        }
        assert_eq!(store.devices(NOW).unwrap().pending.len(), MAX_PENDING);
        let key = SigningKey::from_bytes(&[100; 32]);
        let auth = DeviceAuthenticator::new(store.clone());
        assert!(exchange(&auth, &key, DeviceOperation::Enroll, None, &entry, NOW).is_err());
        let pending = enroll(&auth, &key, &entry, NOW + PENDING_SECONDS);
        assert_eq!(
            store.devices(NOW + PENDING_SECONDS).unwrap().pending[0].request_id,
            pending
        );
        assert_eq!(
            store.devices(NOW + PENDING_SECONDS).unwrap().pending.len(),
            1
        );
    }

    #[test]
    fn old_recovery_proof_cannot_cross_revocation_and_new_approval() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[18; 32]);
        let source = "192.0.2.1".parse().unwrap();
        let auth = DeviceAuthenticator::new(store.clone());
        let first_request = enroll(&auth, &key, &entry, NOW);
        store.approve(&first_request, NOW).unwrap();
        let stale = auth
            .challenge(
                source,
                AUDIENCE,
                request(&key, DeviceOperation::Recover, None),
                &entry,
                NOW,
            )
            .unwrap();
        store
            .revoke(&public_key_id(&key.verifying_key().to_bytes()))
            .unwrap();
        let next_request = enroll(&auth, &key, &entry, NOW + 1);
        assert_ne!(first_request, next_request);
        store.approve(&next_request, NOW + 1).unwrap();
        assert!(
            auth.prove(
                source,
                AUDIENCE,
                proof(&key, &stale, &entry),
                &entry,
                NOW + 1
            )
            .is_err()
        );
        assert!(
            store
                .snapshot()
                .unwrap()
                .approved
                .values()
                .all(|device| device.token_digest.is_none())
        );
    }

    #[test]
    fn a_bound_proof_cannot_move_to_another_socket_or_boot() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[21; 32]);
        let source = "192.0.2.1".parse().unwrap();
        let auth = DeviceAuthenticator::new(store);
        for (boot, socket) in [("other-boot", "socket"), ("boot", "other-socket")] {
            let challenge = auth
                .challenge_bound(
                    source,
                    AUDIENCE,
                    "boot",
                    "socket",
                    request(&key, DeviceOperation::Enroll, None),
                    &entry,
                    NOW,
                )
                .unwrap();
            assert!(
                auth.prove_bound(
                    source,
                    AUDIENCE,
                    boot,
                    socket,
                    proof(&key, &challenge, &entry),
                    &entry,
                    NOW
                )
                .is_err()
            );
            assert!(
                auth.prove_bound(
                    source,
                    AUDIENCE,
                    "boot",
                    "socket",
                    proof(&key, &challenge, &entry),
                    &entry,
                    NOW
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn foreign_proof_consumes_only_the_calling_socket_challenge() {
        let fixture = Fixture::new();
        let store = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
        let entry = store.snapshot().unwrap().entry_token;
        let key = SigningKey::from_bytes(&[29; 32]);
        let source = "192.0.2.1".parse().unwrap();
        let auth = DeviceAuthenticator::new(store.clone());
        let challenge = |socket| {
            auth.challenge_bound(
                source,
                AUDIENCE,
                "boot",
                socket,
                request(&key, DeviceOperation::Enroll, None),
                &entry,
                NOW,
            )
            .unwrap()
        };
        let owner = challenge("owner");
        let caller = challenge("caller");
        assert!(
            auth.prove_bound(
                source,
                AUDIENCE,
                "boot",
                "caller",
                proof(&key, &owner, &entry),
                &entry,
                NOW,
            )
            .is_err()
        );
        assert!(
            auth.prove_bound(
                source,
                AUDIENCE,
                "boot",
                "owner",
                proof(&key, &owner, &entry),
                &entry,
                NOW,
            )
            .is_ok()
        );
        assert!(
            auth.prove_bound(
                source,
                AUDIENCE,
                "boot",
                "caller",
                proof(&key, &caller, &entry),
                &entry,
                NOW,
            )
            .is_err()
        );
        assert_eq!(store.devices(NOW).unwrap().pending.len(), 1);
    }

    mod websocket {
        use super::*;
        use crate::deployment::tests::NoSessions;
        use crate::{Gateway, GatewayAccess, RemoteClient};
        use futures_util::{SinkExt, StreamExt};
        use phi_sdk::agent_manager::AgentManager;
        use serde_json::{Value, json};
        use tokio_tungstenite::{
            MaybeTlsStream, WebSocketStream,
            tungstenite::{Message, client::IntoClientRequest, http::StatusCode},
        };

        struct DelayedStore {
            authority: Arc<PairingAuthority>,
            entered: tokio::sync::Notify,
            release: std::sync::Barrier,
        }

        impl PairingStore for DelayedStore {
            fn gateway_id(&self) -> anyhow::Result<String> {
                self.authority.gateway_id()
            }
            fn admit(
                &self,
                request: &ChallengeRequest,
                credential: &str,
            ) -> anyhow::Result<Option<String>> {
                self.authority.admit(request, credential)
            }
            fn complete(
                &self,
                challenge: &DeviceChallenge,
                credential: &str,
                now: u64,
            ) -> anyhow::Result<PairingOutcome> {
                self.entered.notify_one();
                self.release.wait();
                self.authority.complete(challenge, credential, now)
            }
            fn authenticate(
                &self,
                credential: &str,
            ) -> anyhow::Result<crate::pairing::DeviceAuthorization> {
                self.authority.authenticate(credential)
            }
            fn authorize(
                &self,
                authorization: &crate::pairing::DeviceAuthorization,
            ) -> anyhow::Result<()> {
                self.authority.authorize(authorization)
            }
            fn pairing_status(
                &self,
                request: &str,
                device: &str,
                now: u64,
            ) -> anyhow::Result<crate::pairing::PairingStatus> {
                self.authority.pairing_status(request, device, now)
            }
            fn devices(&self, now: u64) -> anyhow::Result<DeviceList> {
                self.authority.devices(now)
            }
            fn approve(&self, request: &str, now: u64) -> anyhow::Result<()> {
                self.authority.approve(request, now)
            }
            fn reject(&self, request: &str, now: u64) -> anyhow::Result<()> {
                self.authority.reject(request, now)
            }
            fn revoke(&self, device: &str) -> anyhow::Result<()> {
                self.authority.revoke(device)
            }
        }

        type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

        enum ClientKind {
            Native,
            Browser,
        }

        fn upgrade_request(
            gateway: &Gateway,
            kind: ClientKind,
        ) -> tokio_tungstenite::tungstenite::handshake::client::Request {
            let endpoint = format!(
                "ws://{}/ws",
                gateway
                    .local_url()
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
            );
            let mut request = endpoint.into_client_request().unwrap();
            request
                .headers_mut()
                .insert("sec-websocket-protocol", "phi.remote.v1".parse().unwrap());
            if matches!(kind, ClientKind::Browser) {
                request
                    .headers_mut()
                    .insert("host", "phi.example.com".parse().unwrap());
                request
                    .headers_mut()
                    .insert("origin", "https://phi.example.com".parse().unwrap());
                request
                    .headers_mut()
                    .insert("x-forwarded-for", "192.0.2.10".parse().unwrap());
            }
            request
        }

        async fn socket(gateway: &Gateway, kind: ClientKind) -> Socket {
            let (mut socket, _) = tokio_tungstenite::connect_async(upgrade_request(gateway, kind))
                .await
                .unwrap();
            let challenge = receive(&mut socket).await;
            assert_eq!(challenge["event"], "connect.challenge");
            socket
        }

        async fn receive(socket: &mut Socket) -> Value {
            let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(message.to_text().unwrap()).unwrap()
        }

        async fn call(socket: &mut Socket, id: u64, method: &str, params: Value) -> Value {
            socket
                .send(Message::Text(
                    json!({"type":"req","id":id.to_string(),"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let response = receive(socket).await;
            assert_eq!(response["id"], id.to_string());
            response
        }

        async fn authenticate(
            socket: &mut Socket,
            key: &SigningKey,
            kind: &str,
            credential: &str,
            request_id: Option<&str>,
        ) -> Value {
            let mut auth = json!({"kind":kind,"credential":credential,"publicKey":key.verifying_key().to_bytes()});
            if let Some(id) = request_id {
                auth["requestId"] = json!(id);
            }
            let response = call(
                socket,
                1,
                "connect",
                json!({"minProtocol":1,"maxProtocol":1,"auth":auth}),
            )
            .await;
            assert_eq!(response["payload"]["status"], "proofRequired");
            let challenge: DeviceChallenge =
                serde_json::from_value(response["payload"]["challenge"].clone()).unwrap();
            assert!(!challenge.boot_id.is_empty() && !challenge.connection_id.is_empty());
            assert_eq!(challenge.audience, "https://phi.example.com/ws");
            call(
                socket,
                2,
                "connect.prove",
                serde_json::to_value(proof(key, &challenge, credential)).unwrap(),
            )
            .await
        }

        async fn closed(socket: &mut Socket) {
            let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap();
            assert!(matches!(message, Some(Ok(Message::Close(_))) | None));
        }

        async fn test_gateway(fixture: &Fixture) -> (Gateway, Arc<PairingAuthority>) {
            gateway_with_authority(
                fixture,
                PairingAuthority::open_with_expiry(
                    &fixture.0,
                    crate::deployment::config::DEFAULT_EXPIRY_DAYS,
                )
                .unwrap(),
            )
            .await
        }

        async fn gateway_with_authority(
            fixture: &Fixture,
            authority: PairingAuthority,
        ) -> (Gateway, Arc<PairingAuthority>) {
            std::fs::write(fixture.0.join("app.js"), "").unwrap();
            let authority = Arc::new(authority);
            let gateway = Gateway::serve_paired(
                AgentManager::new(),
                NoSessions,
                fixture.0.clone(),
                crate::GatewayConfig::new(
                    "127.0.0.1:0".parse().unwrap(),
                    Some("https://phi.example.com"),
                )
                .unwrap(),
                "c".repeat(64),
                GatewayAccess::new(authority.clone(), vec!["127.0.0.1".parse().unwrap()]),
            )
            .await
            .unwrap();
            (gateway, authority)
        }

        async fn close_code(socket: &mut Socket) -> u16 {
            let message = tokio::time::timeout(std::time::Duration::from_secs(12), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Message::Close(Some(frame)) = message else {
                panic!("expected a typed WebSocket close");
            };
            frame.code.into()
        }

        async fn fragmented_text(socket: &mut Socket, text: String) {
            use tokio_tungstenite::tungstenite::protocol::frame::{
                Frame,
                coding::{Data, OpCode},
            };
            let middle = text.len() / 2;
            for (payload, opcode, final_frame) in [
                (&text.as_bytes()[..middle], Data::Text, false),
                (&text.as_bytes()[middle..], Data::Continue, true),
            ] {
                socket
                    .send(Message::Frame(Frame::message(
                        payload.to_vec(),
                        OpCode::Data(opcode),
                        final_frame,
                    )))
                    .await
                    .unwrap();
            }
        }

        #[tokio::test]
        async fn preauth_source_slots_release_after_hello_and_disconnect() {
            const SOURCE_LIMIT: usize = 8;
            let fixture = Fixture::new();
            let (gateway, _) = test_gateway(&fixture).await;
            let mut sockets = Vec::new();
            for _ in 0..SOURCE_LIMIT {
                sockets.push(socket(&gateway, ClientKind::Native).await);
            }
            let error =
                tokio_tungstenite::connect_async(upgrade_request(&gateway, ClientKind::Native))
                    .await
                    .unwrap_err();
            assert!(
                matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::SERVICE_UNAVAILABLE)
            );
            assert_eq!(
                call(&mut sockets[0], 1, "connect", json!({"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"c".repeat(64)}})).await["payload"]["status"],
                "ready"
            );
            sockets.push(socket(&gateway, ClientKind::Native).await);
            let mut departing = sockets.pop().unwrap();
            departing.close(None).await.unwrap();
            drop(departing);
            let recovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match tokio_tungstenite::connect_async(upgrade_request(
                        &gateway,
                        ClientKind::Native,
                    ))
                    .await
                    {
                        Ok((mut socket, _)) => {
                            assert_eq!(receive(&mut socket).await["event"], "connect.challenge");
                            break socket;
                        }
                        Err(tokio_tungstenite::tungstenite::Error::Http(response))
                            if response.status() == StatusCode::SERVICE_UNAVAILABLE =>
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        Err(error) => panic!("unexpected upgrade failure: {error}"),
                    }
                }
            })
            .await
            .unwrap();
            drop(recovered);
            drop(sockets);
            gateway.close().await.unwrap();
        }

        #[tokio::test]
        async fn preauth_process_capacity_is_bounded_across_proxy_sources() {
            const PROCESS_LIMIT: usize = 64;
            const SOURCE_LIMIT: usize = 8;
            let fixture = Fixture::new();
            let (gateway, _) = test_gateway(&fixture).await;
            let mut sockets = Vec::new();
            for index in 0..PROCESS_LIMIT {
                let mut request = upgrade_request(&gateway, ClientKind::Browser);
                let source = format!("192.0.2.{}", index / SOURCE_LIMIT + 1);
                request
                    .headers_mut()
                    .insert("x-forwarded-for", source.parse().unwrap());
                let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
                assert_eq!(receive(&mut socket).await["event"], "connect.challenge");
                sockets.push(socket);
            }
            let error =
                tokio_tungstenite::connect_async(upgrade_request(&gateway, ClientKind::Native))
                    .await
                    .unwrap_err();
            assert!(
                matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::SERVICE_UNAVAILABLE)
            );
            drop(sockets);
            gateway.close().await.unwrap();
        }

        #[tokio::test]
        async fn pending_waiters_are_exact_bounded_and_status_is_rate_limited() {
            const POLICY_CLOSE: u16 = 1008;
            let fixture = Fixture::new();
            let (gateway, authority) = test_gateway(&fixture).await;
            let entry = authority.snapshot().unwrap().entry_token;
            let key = SigningKey::from_bytes(&[31; 32]);
            let mut first = socket(&gateway, ClientKind::Browser).await;
            let pending = authenticate(&mut first, &key, "enroll", &entry, None).await;
            assert_eq!(pending["payload"]["status"], "pending");
            let request_id = pending["payload"]["requestId"].as_str().unwrap();
            let mut second = socket(&gateway, ClientKind::Browser).await;
            assert_eq!(
                authenticate(&mut second, &key, "enroll", &entry, None).await["payload"]["requestId"],
                request_id
            );
            let mut excess = socket(&gateway, ClientKind::Browser).await;
            assert_eq!(
                authenticate(&mut excess, &key, "enroll", &entry, None).await["error"]["code"],
                "LIMIT_EXCEEDED"
            );
            assert_eq!(close_code(&mut excess).await, POLICY_CLOSE);
            assert_eq!(
                call(&mut first, 3, "pairing.status", json!({})).await["payload"]["status"],
                "pending"
            );
            assert_eq!(
                call(&mut first, 4, "pairing.status", json!({})).await["error"]["code"],
                "LIMIT_EXCEEDED"
            );
            assert_eq!(
                call(
                    &mut first,
                    5,
                    "session.watch",
                    json!({"sessionId":"foreign"})
                )
                .await["error"]["code"],
                "FORBIDDEN"
            );
            let mut unrelated = socket(&gateway, ClientKind::Browser).await;
            let other_key = SigningKey::from_bytes(&[32; 32]);
            let other_pending =
                authenticate(&mut unrelated, &other_key, "enroll", &entry, None).await;
            assert_ne!(other_pending["payload"]["requestId"], request_id);
            let operator = RemoteClient::connect(gateway.local_url(), &"c".repeat(64)).unwrap();
            operator.reject_device(request_id).await.unwrap();
            for waiter in [&mut first, &mut second] {
                let event = receive(waiter).await;
                assert_eq!(event["event"], "pairing.resolved");
                assert_eq!(event["payload"]["requestId"], request_id);
                assert_eq!(event["payload"]["status"], "rejected");
                closed(waiter).await;
            }
            let own_status = call(&mut unrelated, 3, "pairing.status", json!({})).await;
            assert_eq!(
                own_status["payload"]["requestId"],
                other_pending["payload"]["requestId"]
            );
            assert_eq!(own_status["payload"]["status"], "pending");
            gateway.close().await.unwrap();
        }

        #[tokio::test]
        async fn handshake_deadline_and_fragmented_auth_limit_close_the_socket() {
            const AUTH_BYTES: usize = 4 * 1024;
            const REQUEST_BYTES: usize = 16 * 1024;
            const POLICY_CLOSE: u16 = 1008;
            const SIZE_CLOSE: u16 = 1009;
            let fixture = Fixture::new();
            let (gateway, authority) = test_gateway(&fixture).await;
            let mut idle = socket(&gateway, ClientKind::Native).await;
            let mut awaiting_proof = socket(&gateway, ClientKind::Browser).await;
            let handshake_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(12);
            let mut oversized = socket(&gateway, ClientKind::Native).await;
            let text = json!({"type":"req","id":"1","method":"connect","params":{"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"x".repeat(AUTH_BYTES)}}}).to_string();
            fragmented_text(&mut oversized, text).await;
            assert_eq!(close_code(&mut oversized).await, SIZE_CLOSE);
            let mut ready = socket(&gateway, ClientKind::Native).await;
            assert_eq!(
                call(&mut ready, 1, "connect", json!({"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"c".repeat(64)}})).await["payload"]["status"],
                "ready"
            );
            fragmented_text(&mut ready, json!({"type":"req","id":"2","method":"session.list","params":{"padding":"x".repeat(REQUEST_BYTES)}}).to_string()).await;
            assert_eq!(close_code(&mut ready).await, SIZE_CLOSE);
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let key = SigningKey::from_bytes(&[33; 32]);
            let entry = authority.snapshot().unwrap().entry_token;
            assert_eq!(
                call(&mut awaiting_proof, 1, "connect", json!({"minProtocol":1,"maxProtocol":1,"auth":{"kind":"enroll","credential":entry,"publicKey":key.verifying_key().to_bytes()}})).await["payload"]["status"],
                "proofRequired"
            );
            assert_eq!(close_code(&mut idle).await, POLICY_CLOSE);
            assert_eq!(
                tokio::time::timeout_at(handshake_deadline, close_code(&mut awaiting_proof))
                    .await
                    .unwrap(),
                POLICY_CLOSE
            );
            assert!(authority.snapshot().unwrap().pending.is_empty());
            gateway.close().await.unwrap();
        }

        #[tokio::test]
        async fn accepted_pairing_write_survives_socket_loss_and_is_drained() {
            let fixture = Fixture::new();
            std::fs::write(fixture.0.join("app.js"), "").unwrap();
            let authority = Arc::new(PairingAuthority::open(&fixture.0).unwrap());
            let entry = authority.snapshot().unwrap().entry_token;
            let delayed = Arc::new(DelayedStore {
                authority: authority.clone(),
                entered: tokio::sync::Notify::new(),
                release: std::sync::Barrier::new(2),
            });
            let gateway = Gateway::serve_paired(
                AgentManager::new(),
                NoSessions,
                fixture.0.clone(),
                crate::GatewayConfig::new(
                    "127.0.0.1:0".parse().unwrap(),
                    Some("https://phi.example.com"),
                )
                .unwrap(),
                "c".repeat(64),
                GatewayAccess::new(delayed.clone(), vec!["127.0.0.1".parse().unwrap()]),
            )
            .await
            .unwrap();
            let key = SigningKey::from_bytes(&[23; 32]);
            let mut browser = socket(&gateway, ClientKind::Browser).await;
            let response = call(&mut browser, 1, "connect", json!({"minProtocol":1,"maxProtocol":1,"auth":{"kind":"enroll","credential":entry,"publicKey":key.verifying_key().to_bytes()}})).await;
            let challenge: DeviceChallenge =
                serde_json::from_value(response["payload"]["challenge"].clone()).unwrap();
            browser.send(Message::Text(json!({"type":"req","id":"2","method":"connect.prove","params":proof(&key, &challenge, &entry)}).to_string().into())).await.unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                delayed.entered.notified(),
            )
            .await
            .unwrap();
            browser.close(None).await.unwrap();
            drop(browser);
            delayed.release.wait();
            gateway.close().await.unwrap();
            assert_eq!(authority.snapshot().unwrap().pending.len(), 1);
            drop(delayed);
            drop(authority);
            assert_eq!(
                PairingAuthority::open(&fixture.0)
                    .unwrap()
                    .snapshot()
                    .unwrap()
                    .pending
                    .len(),
                1
            );
        }

        async fn http_sessions(gateway: &Gateway, credential: &str) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let address = gateway
                .local_url()
                .trim_start_matches("http://")
                .trim_end_matches('/');
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(format!(
                "GET /api/sessions HTTP/1.1\r\nHost: phi.example.com\r\nOrigin: https://phi.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nAuthorization: Bearer {credential}\r\nConnection: close\r\n\r\n",
            ).as_bytes()).await.unwrap();
            let mut response = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                socket.read_to_string(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            response
        }

        #[tokio::test]
        async fn expired_approval_retires_both_channels_but_not_local_management() {
            let fixture = Fixture::new();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let clock = Arc::new(AtomicU64::new(now));
            let authority = timed_authority(&fixture, 1, &clock);
            let (gateway, store) = gateway_with_authority(&fixture, authority).await;
            let operator = RemoteClient::connect(gateway.local_url(), &"c".repeat(64)).unwrap();
            let entry = store.snapshot().unwrap().entry_token;
            let key = SigningKey::from_bytes(&[38; 32]);
            let auth = DeviceAuthenticator::new(store.clone());
            let pending = enroll(&auth, &key, &entry, now);
            operator.approve_device(&pending).await.unwrap();
            let deadline = operator.devices().await.unwrap().approved[0]
                .expires_at
                .unwrap();
            let mut claimed = socket(&gateway, ClientKind::Browser).await;
            let ready = authenticate(&mut claimed, &key, "claim", &entry, Some(&pending)).await;
            let token = ready["payload"]["deviceCredential"].as_str().unwrap();
            assert!(
                http_sessions(&gateway, token)
                    .await
                    .starts_with("HTTP/1.1 200")
            );
            clock.store(deadline, Ordering::SeqCst);
            assert!(
                http_sessions(&gateway, token)
                    .await
                    .starts_with("HTTP/1.1 401")
            );
            closed(&mut claimed).await;
            for (kind, credential) in [("device", token), ("recover", entry.as_str())] {
                let mut rejected = socket(&gateway, ClientKind::Browser).await;
                let response = call(&mut rejected, 1, "connect", json!({"minProtocol":1,"maxProtocol":1,
                    "auth":{"kind":kind,"credential":credential,"publicKey":key.verifying_key().to_bytes()}
                })).await;
                assert_eq!(response["error"]["code"], "REVOKED");
            }
            assert!(operator.sessions().await.is_ok());
            assert_eq!(
                operator.devices().await.unwrap().approved[0].expires_at,
                Some(deadline)
            );
            gateway.close().await.unwrap();
        }

        #[tokio::test]
        async fn durable_authority_governs_http_and_proof_bound_websockets() {
            let fixture = Fixture::new();
            let (gateway, store) = test_gateway(&fixture).await;
            let entry = store.snapshot().unwrap().entry_token;
            let operator = RemoteClient::connect(gateway.local_url(), &"c".repeat(64)).unwrap();
            let key = SigningKey::from_bytes(&[22; 32]);
            let mut pending = socket(&gateway, ClientKind::Browser).await;
            let result = authenticate(&mut pending, &key, "enroll", &entry, None).await;
            assert_eq!(result["payload"]["status"], "pending");
            let request_id = result["payload"]["requestId"].as_str().unwrap();
            assert_eq!(
                call(
                    &mut pending,
                    3,
                    "session.watch",
                    json!({"sessionId":"foreign"})
                )
                .await["error"]["code"],
                "FORBIDDEN"
            );
            let devices = operator.devices().await.unwrap();
            assert_eq!(devices.pending[0].request_id, request_id);
            operator.approve_device(request_id).await.unwrap();
            let resolution = receive(&mut pending).await;
            assert_eq!(resolution["payload"]["requestId"], request_id);
            assert_eq!(resolution["payload"]["status"], "approved");
            closed(&mut pending).await;
            let mut claimed = socket(&gateway, ClientKind::Browser).await;
            let result = authenticate(&mut claimed, &key, "claim", &entry, Some(request_id)).await;
            assert_eq!(result["payload"]["status"], "ready");
            let token = result["payload"]["deviceCredential"].as_str().unwrap();
            let device_client = RemoteClient::connect(gateway.local_url(), token).unwrap();
            assert!(device_client.devices().await.is_err());
            gateway.close().await.unwrap();
            closed(&mut claimed).await;
            drop(store);

            let (gateway, store) = test_gateway(&fixture).await;
            assert!(
                http_sessions(&gateway, token)
                    .await
                    .starts_with("HTTP/1.1 200")
            );
            for rejected in [&entry, &"c".repeat(64)] {
                assert!(
                    http_sessions(&gateway, rejected)
                        .await
                        .starts_with("HTTP/1.1 401")
                );
            }
            let operator = RemoteClient::connect(gateway.local_url(), &"c".repeat(64)).unwrap();
            let mut remembered = socket(&gateway, ClientKind::Browser).await;
            assert_eq!(
                authenticate(&mut remembered, &key, "device", token, None).await["payload"]["status"],
                "ready"
            );
            let mut recovered = socket(&gateway, ClientKind::Browser).await;
            let replacement = authenticate(&mut recovered, &key, "recover", &entry, None).await;
            assert_eq!(replacement["payload"]["status"], "ready");
            let replacement = replacement["payload"]["deviceCredential"].as_str().unwrap();
            assert_ne!(replacement, token);
            closed(&mut remembered).await;
            assert!(
                http_sessions(&gateway, token)
                    .await
                    .starts_with("HTTP/1.1 401")
            );
            assert!(
                http_sessions(&gateway, replacement)
                    .await
                    .starts_with("HTTP/1.1 200")
            );
            let identity = public_key_id(&key.verifying_key().to_bytes());
            operator.revoke_device(&identity).await.unwrap();
            closed(&mut recovered).await;
            assert!(
                http_sessions(&gateway, replacement)
                    .await
                    .starts_with("HTTP/1.1 401")
            );
            assert!(operator.devices().await.unwrap().approved.is_empty());
            gateway.close().await.unwrap();
            drop(store);

            let (gateway, _store) = test_gateway(&fixture).await;
            for credential in [token, replacement] {
                assert!(
                    http_sessions(&gateway, credential)
                        .await
                        .starts_with("HTTP/1.1 401")
                );
            }
            gateway.close().await.unwrap();
        }
    }
}
