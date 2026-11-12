use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::pairing::{is_identity, public_key_id, random_credential};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PendingDevice {
    pub(super) device_id: String,
    pub(super) public_key: [u8; 32],
    pub(super) requested_at: u64,
    pub(super) expires_at: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ApprovedDevice {
    pub(super) public_key: [u8; 32],
    pub(super) approved_at: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) expires_at: Option<u64>,
    pub(super) approval_request: String,
    pub(super) token_digest: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PairingDocument {
    pub(super) gateway_id: String,
    pub(super) entry_token: String,
    pub(super) pending: BTreeMap<String, PendingDevice>,
    pub(super) approved: BTreeMap<String, ApprovedDevice>,
}

impl PairingDocument {
    fn initialize() -> anyhow::Result<Self> {
        Ok(Self {
            gateway_id: random_credential()?,
            entry_token: random_credential()?,
            pending: BTreeMap::new(),
            approved: BTreeMap::new(),
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            is_identity(&self.gateway_id) && is_identity(&self.entry_token),
            "invalid Gateway identity state"
        );
        ensure!(
            self.pending.len() <= crate::pairing::MAX_PENDING
                && self.approved.len() <= crate::pairing::MAX_APPROVED,
            "Gateway pairing capacity exceeded"
        );
        let mut pending_devices = BTreeSet::new();
        for (request_id, pending) in &self.pending {
            ensure!(
                is_identity(request_id)
                    && pending.device_id == public_key_id(&pending.public_key)
                    && pending.requested_at < pending.expires_at
                    && pending_devices.insert(&pending.device_id)
                    && !self.approved.contains_key(&pending.device_id),
                "invalid Gateway pending device state"
            );
        }
        let mut digests = BTreeSet::new();
        for (identity, approved) in &self.approved {
            ensure!(
                identity == &public_key_id(&approved.public_key)
                    && is_identity(&approved.approval_request)
                    && approved
                        .expires_at
                        .is_none_or(|deadline| deadline > approved.approved_at),
                "invalid Gateway approved identity"
            );
            if let Some(digest) = &approved.token_digest {
                ensure!(
                    is_identity(digest) && digests.insert(digest),
                    "invalid Gateway device credential state"
                );
            }
        }
        Ok(())
    }
}

struct AuthorityState {
    document: PairingDocument,
    availability: Availability,
}

#[derive(PartialEq, Eq)]
enum Availability {
    Ready,
    FailedClosed,
}

pub(super) struct PairingAuthority {
    directory: PathBuf,
    state: Mutex<AuthorityState>,
    expiry_seconds: u64,
    clock: Box<dyn Fn() -> anyhow::Result<u64> + Send + Sync>,
    _lock: ProcessLock,
}

impl PairingAuthority {
    #[cfg(test)]
    pub(super) fn open(directory: &Path) -> anyhow::Result<Self> {
        Ok(
            Self::open_with_expiry(directory, super::config::DEFAULT_EXPIRY_DAYS)?
                .with_clock(|| Ok(super::TEST_NOW)),
        )
    }

    pub(super) fn open_with_expiry(directory: &Path, days: u32) -> anyhow::Result<Self> {
        ensure!(
            cfg!(unix),
            "durable Gateway storage currently requires Unix"
        );
        create_directory(directory)?;
        let lock_path = directory.join("gateway.lock");
        let lock = ProcessLock::try_acquire(&lock_path)
            .context("Gateway directory is already owned or unavailable")?;
        let path = directory.join("pairing.json");
        let document = match read_optional(&path)? {
            Some(bytes) => parse_document(&bytes)?,
            None => {
                let document = PairingDocument::initialize()?;
                replace_document(directory, &document, None, WriteFault::None)?;
                document
            }
        };
        Ok(Self {
            directory: directory.to_owned(),
            state: Mutex::new(AuthorityState {
                document,
                availability: Availability::Ready,
            }),
            expiry_seconds: u64::from(days) * SECONDS_PER_DAY,
            clock: Box::new(|| Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())),
            _lock: lock,
        })
    }

    pub(super) fn approval_deadline(&self, approved_at: u64) -> anyhow::Result<Option<u64>> {
        if self.expiry_seconds == 0 {
            return Ok(None);
        }
        Ok(Some(
            approved_at
                .checked_add(self.expiry_seconds)
                .context("invalid device approval clock")?,
        ))
    }

    pub(super) fn now(&self) -> anyhow::Result<u64> {
        (self.clock)()
    }

    #[cfg(test)]
    pub(super) fn with_clock(
        mut self,
        clock: impl Fn() -> anyhow::Result<u64> + Send + Sync + 'static,
    ) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub(super) fn snapshot(&self) -> anyhow::Result<PairingDocument> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Gateway authority unavailable"))?;
        self.check_current(&mut state)?;
        Ok(state.document.clone())
    }

    pub(super) fn transition<T>(
        &self,
        change: impl FnOnce(&mut PairingDocument) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.commit(change, WriteFault::None)
    }

    fn commit<T>(
        &self,
        change: impl FnOnce(&mut PairingDocument) -> anyhow::Result<T>,
        fault: WriteFault,
    ) -> anyhow::Result<T> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Gateway authority unavailable"))?;
        self.check_current(&mut state)?;
        let mut candidate = state.document.clone();
        let result = change(&mut candidate)?;
        candidate.validate()?;
        ensure!(
            candidate.gateway_id == state.document.gateway_id
                && candidate.entry_token == state.document.entry_token,
            "Gateway identity cannot change during a transition"
        );
        if candidate == state.document {
            return Ok(result);
        }
        if let Err(failure) =
            replace_document(&self.directory, &candidate, Some(&state.document), fault)
        {
            if failure.replaced {
                state.availability = Availability::FailedClosed;
            }
            return Err(failure.error);
        }
        state.document = candidate;
        Ok(result)
    }

    fn check_current(&self, state: &mut AuthorityState) -> anyhow::Result<()> {
        ensure!(
            state.availability == Availability::Ready,
            "Gateway authority requires restart"
        );
        let current = read_optional(&self.directory.join("pairing.json"))
            .and_then(|bytes| parse_document(&bytes.context("Gateway pairing state disappeared")?));
        if !matches!(current, Ok(ref current) if current == &state.document) {
            state.availability = Availability::FailedClosed;
            anyhow::bail!("Gateway pairing state changed outside its owner; restart required");
        }
        Ok(())
    }
}

pub(super) fn entry_token(directory: &Path) -> anyhow::Result<String> {
    Ok(read_document(directory)?.entry_token)
}

pub(super) fn read_document(directory: &Path) -> anyhow::Result<PairingDocument> {
    let bytes = read_optional(&directory.join("pairing.json"))?
        .context("Gateway pairing state is missing; start phi gateway first")?;
    parse_document(&bytes)
}

fn parse_document(bytes: &[u8]) -> anyhow::Result<PairingDocument> {
    let document: PairingDocument = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("invalid Gateway pairing state"))?;
    document.validate()?;
    Ok(document)
}

fn create_directory(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) =>
        {
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    validate_path(path, PathKind::Directory)
}

#[derive(Clone, Copy)]
enum PathKind {
    Directory,
    File,
}

fn validate_path(path: &Path, kind: PathKind) -> anyhow::Result<()> {
    validate_metadata(&std::fs::symlink_metadata(path)?, kind)
}

fn validate_metadata(metadata: &std::fs::Metadata, kind: PathKind) -> anyhow::Result<()> {
    ensure!(
        match kind {
            PathKind::Directory => metadata.is_dir(),
            PathKind::File => metadata.is_file(),
        },
        "unsafe Gateway path type"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // OS-user authority is the deployment boundary, not a separate tenant.
        let effective_uid = unsafe { libc::geteuid() };
        ensure!(
            metadata.uid() == effective_uid && metadata.mode() & 0o077 == 0,
            "unsafe Gateway path ownership or permissions"
        );
        if matches!(kind, PathKind::File) {
            ensure!(
                metadata.nlink() == 1,
                "Gateway files cannot have hard links"
            );
        }
    }
    Ok(())
}

fn validate_optional_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_metadata(&metadata, PathKind::File),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn read_optional(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    if let Some(parent) = path.parent() {
        match std::fs::symlink_metadata(parent) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) => validate_metadata(&metadata, PathKind::Directory)?,
        }
    }
    validate_optional_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_metadata(&file.metadata()?, PathKind::File)?;
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "Gateway document is too large"
    );
    Ok(Some(bytes))
}

pub(super) struct ProcessLock {
    file: File,
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum LockAccess {
    Create,
    Existing,
}

impl ProcessLock {
    pub(super) fn try_acquire(path: &Path) -> std::io::Result<Self> {
        loop {
            let file = open_lock(path, LockAccess::Create)?;
            if let Some(lock) = Self::try_lock_current(file, path)? {
                return Ok(lock);
            }
        }
    }

    pub(super) fn try_acquire_existing(path: &Path) -> std::io::Result<Option<Self>> {
        match open_lock(path, LockAccess::Existing) {
            Ok(file) => Self::try_lock_current(file, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn try_lock_current(file: File, path: &Path) -> std::io::Result<Option<Self>> {
        let acquired = file.try_lock();
        if !matches_lock(&file, path)? {
            return Ok(None);
        }
        acquired?;
        Ok(Some(Self {
            file,
            path: path.to_owned(),
        }))
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        if matches!(matches_lock(&self.file, &self.path), Ok(true)) {
            if let Err(error) = std::fs::remove_file(&self.path) {
                tracing::warn!(%error, "cannot retire Gateway process lock");
            }
        }
    }
}

fn open_lock(path: &Path, access: LockAccess) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(matches!(access, LockAccess::Create));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    validate_metadata(&file.metadata()?, PathKind::File).map_err(std::io::Error::other)?;
    Ok(file)
}

fn matches_lock(file: &File, path: &Path) -> std::io::Result<bool> {
    let current = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        Ok(opened.dev() == current.dev() && opened.ino() == current.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, current);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable Gateway storage currently requires Unix",
        ))
    }
}

#[derive(Debug)]
struct WriteFailure {
    error: anyhow::Error,
    replaced: bool,
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for WriteFailure {}

#[derive(Clone, Copy)]
enum WriteFault {
    None,
    #[cfg(test)]
    BeforeReplace,
    #[cfg(test)]
    AfterReplace,
}

fn replace_document(
    directory: &Path,
    document: &PairingDocument,
    previous: Option<&PairingDocument>,
    fault: WriteFault,
) -> Result<(), WriteFailure> {
    let path = directory.join("pairing.json");
    replace_checked(
        &path,
        document,
        || {
            let current = read_optional(&path)?
                .map(|bytes| parse_document(&bytes))
                .transpose()?;
            ensure!(
                current.as_ref() == previous,
                "Gateway pairing state changed before replacement"
            );
            Ok(())
        },
        fault,
    )
}

pub(super) fn publish(path: &Path, document: &impl Serialize) -> anyhow::Result<()> {
    replace_checked(
        path,
        document,
        || {
            read_optional(path)?;
            Ok(())
        },
        WriteFault::None,
    )
    .map_err(|failure| failure.error)
}

fn replace_checked(
    path: &Path,
    document: &impl Serialize,
    before_replace: impl FnOnce() -> anyhow::Result<()>,
    fault: WriteFault,
) -> Result<(), WriteFailure> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let staging = directory.join(format!(".gateway-{}.tmp", uuid::Uuid::new_v4()));
    let mut replaced = false;
    let result = (|| -> anyhow::Result<()> {
        validate_path(directory, PathKind::Directory)?;
        validate_optional_file(path)?;
        let bytes = serde_json::to_vec(document)?;
        ensure!(
            bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
            "Gateway document is too large"
        );
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(test)]
        if matches!(fault, WriteFault::BeforeReplace) {
            anyhow::bail!("injected pre-replacement failure");
        }
        before_replace()?;
        std::fs::rename(&staging, path)?;
        replaced = true;
        #[cfg(test)]
        if matches!(fault, WriteFault::AfterReplace) {
            anyhow::bail!("injected directory-sync failure");
        }
        let _ = fault;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if !replaced {
        let _ = std::fs::remove_file(staging);
    }
    result.map_err(|error| WriteFailure { error, replaced })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use proptest::prelude::*;

    pub(in crate::deployment) struct Fixture(pub(in crate::deployment) PathBuf);

    impl Fixture {
        pub(in crate::deployment) fn new() -> Self {
            let directory =
                PathBuf::from("/tmp").join(format!("phi-gateway-store-{}", uuid::Uuid::new_v4()));
            create_directory(&directory).unwrap();
            Self(directory)
        }

        pub(in crate::deployment) fn write(&self, name: &str, bytes: &[u8]) {
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(self.0.join(name))
                .unwrap()
                .write_all(bytes)
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn approve(document: &mut PairingDocument) -> anyhow::Result<()> {
        let public_key = [7; 32];
        document.approved.insert(
            public_key_id(&public_key),
            ApprovedDevice {
                public_key,
                approved_at: 100,
                expires_at: None,
                approval_request: "c".repeat(64),
                token_digest: Some(crate::pairing::credential_digest("test credential")),
            },
        );
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..Default::default() })]

        #[test]
        fn device_identity_and_expiry_are_invariants(
            public_key in any::<[u8; 32]>(),
            requested_at in any::<u64>(),
            expires_at in any::<u64>(),
        ) {
            let mut document = PairingDocument::initialize().unwrap();
            let pending = PendingDevice {
                device_id: public_key_id(&public_key),
                public_key,
                requested_at,
                expires_at,
            };
            document.pending.insert("a".repeat(64), pending.clone());
            prop_assert_eq!(document.validate().is_ok(), requested_at < expires_at);
            document.pending.insert("b".repeat(64), pending);
            prop_assert!(document.validate().is_err());
            document.pending.remove(&"b".repeat(64));
            document.pending.get_mut(&"a".repeat(64)).unwrap().device_id.clear();
            prop_assert!(document.validate().is_err());
            document.pending.clear();
            approve(&mut document).unwrap();
            let approved = document.approved.values_mut().next().unwrap();
            approved.approved_at = requested_at;
            approved.expires_at = Some(expires_at);
            prop_assert_eq!(document.validate().is_ok(), requested_at < expires_at);
            document.approved.values_mut().next().unwrap().expires_at = None;
            prop_assert!(document.validate().is_ok());
        }
    }

    #[test]
    fn concurrent_candidates_never_lose_accepted_devices() {
        let fixture = Fixture::new();
        let authority = PairingAuthority::open(&fixture.0).unwrap();
        let device_count = 16;
        std::thread::scope(|scope| {
            for index in 0..device_count {
                let authority = &authority;
                scope.spawn(move || {
                    let public_key = [index; 32];
                    authority
                        .transition(|document| {
                            document.approved.insert(
                                public_key_id(&public_key),
                                ApprovedDevice {
                                    public_key,
                                    approved_at: 100,
                                    expires_at: None,
                                    approval_request: "c".repeat(64),
                                    token_digest: None,
                                },
                            );
                            Ok(())
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(
            authority.snapshot().unwrap().approved.len(),
            usize::from(device_count)
        );
        drop(authority);
        assert_eq!(
            PairingAuthority::open(&fixture.0)
                .unwrap()
                .snapshot()
                .unwrap()
                .approved
                .len(),
            usize::from(device_count)
        );
    }

    #[test]
    fn lock_excludes_a_second_process() {
        const CHILD_DIRECTORY: &str = "PHI_TEST_GATEWAY_STORE";
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            assert!(PairingAuthority::open(Path::new(&directory)).is_err());
            return;
        }
        let fixture = Fixture::new();
        let authority = PairingAuthority::open(&fixture.0).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deployment::storage::tests::lock_excludes_a_second_process",
            ])
            .env(CHILD_DIRECTORY, &fixture.0)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(authority.snapshot().unwrap().approved.is_empty());
    }

    #[test]
    fn process_lock_preserves_replacement_and_rejects_obsolete_openers() {
        let fixture = Fixture::new();
        let path = fixture.0.join("gateway.lock");
        let original = ProcessLock::try_acquire(&path).unwrap();
        let obsolete = open_lock(&path, LockAccess::Existing).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = ProcessLock::try_acquire(&path).unwrap();
        assert!(
            ProcessLock::try_lock_current(obsolete, &path)
                .unwrap()
                .is_none()
        );
        drop(original);
        assert!(path.exists());
        assert_eq!(
            ProcessLock::try_acquire_existing(&path)
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(replacement);
        assert!(!path.exists());
        fixture.write("gateway.lock", b"");
        drop(ProcessLock::try_acquire_existing(&path).unwrap().unwrap());
        assert!(ProcessLock::try_acquire_existing(&path).unwrap().is_none());
    }

    #[test]
    fn failed_write_cannot_publish_and_uncertain_commit_closes_authority() {
        let fixture = Fixture::new();
        let authority = PairingAuthority::open(&fixture.0).unwrap();
        assert!(
            authority
                .commit(approve, WriteFault::BeforeReplace)
                .is_err()
        );
        assert!(authority.snapshot().unwrap().approved.is_empty());
        assert!(
            parse_document(
                &read_optional(&fixture.0.join("pairing.json"))
                    .unwrap()
                    .unwrap()
            )
            .unwrap()
            .approved
            .is_empty()
        );
        assert!(authority.commit(approve, WriteFault::AfterReplace).is_err());
        assert!(authority.snapshot().is_err());
        assert!(authority.transition(|_| Ok(())).is_err());
        assert!(std::fs::read_dir(&fixture.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        drop(authority);
        assert_eq!(
            PairingAuthority::open(&fixture.0)
                .unwrap()
                .snapshot()
                .unwrap()
                .approved
                .len(),
            1
        );
    }

    #[test]
    fn external_state_changes_require_restart() {
        for during_preparation in [false, true] {
            for replacement in [None, Some(b"{}".as_slice())] {
                let fixture = Fixture::new();
                let authority = PairingAuthority::open(&fixture.0).unwrap();
                let original = std::fs::read(fixture.0.join("pairing.json")).unwrap();
                let replace = || {
                    std::fs::remove_file(fixture.0.join("pairing.json")).unwrap();
                    if let Some(bytes) = replacement {
                        fixture.write("pairing.json", bytes);
                    }
                };
                if !during_preparation {
                    replace();
                }
                assert!(
                    authority
                        .transition(|document| {
                            if during_preparation {
                                replace();
                            }
                            approve(document)
                        })
                        .is_err()
                );
                assert_eq!(
                    read_optional(&fixture.0.join("pairing.json"))
                        .unwrap()
                        .as_deref(),
                    replacement
                );
                assert!(authority.snapshot().is_err());
                fixture.write("pairing.json", &original);
                assert!(authority.snapshot().is_err());
                assert!(authority.transition(approve).is_err());
                drop(authority);
                assert!(PairingAuthority::open(&fixture.0).is_ok());
            }
        }
    }

    #[test]
    fn invalid_candidates_and_corrupt_state_are_rejected() {
        let fixture = Fixture::new();
        let authority = PairingAuthority::open(&fixture.0).unwrap();
        assert!(
            authority
                .transition(|document| {
                    document.entry_token.clear();
                    Ok(())
                })
                .is_err()
        );
        assert!(authority.snapshot().unwrap().approved.is_empty());
        authority.transition(approve).unwrap();
        let encoded = serde_json::to_string(&authority.snapshot().unwrap()).unwrap();
        for field in ["\"pending\":{}", "\"expiresAt\":null"] {
            let duplicate = encoded.replace(field, &format!("{field},{field}"));
            assert!(parse_document(duplicate.as_bytes()).is_err());
        }
        let unknown_field = encoded.replacen('{', "{\"unexpected\":true,", 1);
        let missing_deadline = encoded.replace("\"expiresAt\":null,", "");
        drop(authority);
        for bytes in [
            b"not json".as_slice(),
            b"{}",
            unknown_field.as_bytes(),
            missing_deadline.as_bytes(),
        ] {
            fixture.write("pairing.json", bytes);
            assert!(PairingAuthority::open(&fixture.0).is_err());
            assert_eq!(
                std::fs::read(fixture.0.join("pairing.json")).unwrap(),
                bytes
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_files_and_directories_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for name in ["pairing.json", "gateway.lock"] {
            let fixture = Fixture::new();
            let document = serde_json::to_vec(&PairingDocument::initialize().unwrap()).unwrap();
            let path = fixture.0.join(name);
            fixture.write("target", &document);
            symlink(fixture.0.join("target"), &path).unwrap();
            assert!(read_optional(&path).is_err());
            assert!(PairingAuthority::open(&fixture.0).is_err());
            std::fs::remove_file(&path).unwrap();
            fixture.write(name, &document);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_optional(&path).is_err());
            assert!(PairingAuthority::open(&fixture.0).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::hard_link(&path, fixture.0.join("linked.json")).unwrap();
            assert!(read_optional(&path).is_err());
            assert!(PairingAuthority::open(&fixture.0).is_err());
            std::fs::remove_file(fixture.0.join("linked.json")).unwrap();
            drop(PairingAuthority::open(&fixture.0).unwrap());
            std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(read_optional(&path).is_err());
            assert!(PairingAuthority::open(&fixture.0).is_err());
        }
        let fixture = Fixture::new();
        fixture.write("large.json", &vec![b' '; MAX_DOCUMENT_BYTES as usize + 1]);
        assert!(read_optional(&fixture.0.join("large.json")).is_err());
    }
}
