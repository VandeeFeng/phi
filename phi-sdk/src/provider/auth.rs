use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rig::client::CompletionClient;
use rig::completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse};
use rig::providers::chatgpt;
use rig::streaming::StreamingCompletionResponse;
use serde::{Deserialize, Serialize};

const LEASE_SECONDS: i64 = 30 * 24 * 60 * 60;
const STATE_VERSION: u32 = 1;
const LOCK_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
const LOCK_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SIGN_IN_REQUIRED: &str =
    "Phi OAuth authorization required; sign in explicitly on this machine";

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    issued_at: i64,
    deadline: i64,
    last_checked_at: i64,
}

impl Lease {
    fn new(now: i64) -> anyhow::Result<Self> {
        anyhow::ensure!(now >= 0, "invalid authorization clock");
        let deadline = now
            .checked_add(LEASE_SECONDS)
            .ok_or_else(|| anyhow::anyhow!("invalid authorization clock"))?;
        Ok(Self {
            issued_at: now,
            deadline,
            last_checked_at: now,
        })
    }

    fn validate(&self, now: i64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.issued_at >= 0
                && self.issued_at.checked_add(LEASE_SECONDS) == Some(self.deadline)
                && self.last_checked_at >= self.issued_at
                && self.last_checked_at < self.deadline,
            "invalid Phi OAuth lease; {SIGN_IN_REQUIRED}"
        );
        anyhow::ensure!(
            now >= self.last_checked_at,
            "authorization clock moved backwards; restore the clock or sign in locally"
        );
        anyhow::ensure!(
            now < self.deadline,
            "Phi OAuth authorization expired; {SIGN_IN_REQUIRED}"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct AuthState {
    version: u32,
    lease: Lease,
    credentials: serde_json::Value,
}

impl AuthState {
    fn check(&mut self, now: i64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == STATE_VERSION,
            "unsupported Phi OAuth state; {SIGN_IN_REQUIRED}"
        );
        self.lease.validate(now)?;
        validate_credentials(&self.credentials)?;
        self.lease.last_checked_at = now;
        Ok(())
    }
}

fn validate_credentials(credentials: &serde_json::Value) -> anyhow::Result<()> {
    anyhow::ensure!(
        credentials
            .get("access_token")
            .and_then(|value| value.as_str())
            .is_some_and(|token| !token.is_empty()),
        "invalid OAuth credentials; {SIGN_IN_REQUIRED}"
    );
    Ok(())
}

#[derive(Debug)]
struct AuthLock {
    path: PathBuf,
}

impl AuthLock {
    async fn acquire(path: PathBuf, timeout: std::time::Duration) -> anyhow::Result<Self> {
        let wait = async {
            loop {
                match private_directory(&path) {
                    Ok(()) => return Ok(Self { path: path.clone() }),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
                    }
                    Err(error) => return Err(anyhow::Error::from(error)),
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| anyhow::anyhow!(
            "OAuth lock wait timed out at {}; stop other Phi processes before removing an abandoned lock",
            path.display()
        ))?
    }
}

impl Drop for AuthLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir(&self.path) {
            tracing::warn!(%error, path = %self.path.display(), "failed to remove OAuth lock directory");
        }
    }
}

#[derive(Clone)]
pub(super) struct AuthStore {
    path: PathBuf,
    clock: Clock,
}

impl AuthStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            clock: Arc::new(|| chrono::Utc::now().timestamp()),
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".lock");
        path.into()
    }

    async fn lock(&self) -> anyhow::Result<AuthLock> {
        AuthLock::acquire(self.lock_path(), LOCK_WAIT_TIMEOUT).await
    }

    fn load(&self) -> anyhow::Result<AuthState> {
        ensure_regular_file(&self.path)?;
        let bytes = std::fs::read(&self.path).map_err(|_| anyhow::anyhow!(SIGN_IN_REQUIRED))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("legacy or malformed Phi OAuth state; {SIGN_IN_REQUIRED}"))
    }

    fn publish(&self, state: &AuthState) -> anyhow::Result<()> {
        let parent = File::open(
            self.path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("auth path has no parent"))?,
        )?;
        parent.sync_all()?;
        let candidate = Candidate::new(&self.path)?;
        let mut file = private_file(&candidate.path)?;
        serde_json::to_writer(&mut file, state)?;
        file.sync_all()?;
        std::fs::rename(&candidate.path, &self.path)?;
        // Rename is the commit point; a durability warning cannot report a failed login.
        if let Err(error) = parent.sync_all() {
            tracing::warn!(%error, "OAuth state committed but directory synchronization failed");
        }
        Ok(())
    }

    async fn maintain<F, Fut>(&self, login: bool, operation: F) -> anyhow::Result<serde_json::Value>
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let _lock = self.lock().await?;
        // Rig overwrites flat records; only the candidate receives its writes.
        let mut state = self.prepare(login)?;
        let candidate = Candidate::new(&self.path)?;
        if let Some(state) = &state {
            serde_json::to_writer(private_file(&candidate.path)?, &state.credentials)?;
        }
        operation(candidate.path.clone()).await?;
        let credentials = serde_json::from_slice(&std::fs::read(&candidate.path)?)
            .map_err(|_| anyhow::anyhow!("invalid OAuth candidate credentials"))?;
        validate_credentials(&credentials).map_err(AuthorizationError)?;
        let state = self.finish(&mut state, credentials)?;
        self.publish(&state)?;
        Ok(state.credentials)
    }

    fn prepare(&self, login: bool) -> anyhow::Result<Option<AuthState>> {
        if login {
            return Ok(None);
        }
        let mut state = self.load().map_err(AuthorizationError)?;
        state.check((self.clock)()).map_err(AuthorizationError)?;
        self.publish(&state)?;
        Ok(Some(state))
    }

    fn finish(
        &self,
        state: &mut Option<AuthState>,
        credentials: serde_json::Value,
    ) -> anyhow::Result<AuthState> {
        let now = (self.clock)();
        let lease = match state {
            Some(state) => {
                state.check(now).map_err(AuthorizationError)?;
                state.lease.clone()
            }
            None => Lease::new(now)?,
        };
        Ok(AuthState {
            version: STATE_VERSION,
            lease,
            credentials,
        })
    }

    async fn credentials(&self) -> anyhow::Result<serde_json::Value> {
        self.maintain(false, |path| async move {
            oauth_client(path, false)?.authorize().await?;
            Ok(())
        })
        .await
    }

    pub(super) async fn login<F>(&self, on_device_code: F) -> anyhow::Result<()>
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.maintain(true, |path| async move {
            chatgpt::Client::builder()
                .oauth()
                .auth_file(path)
                .allow_device_flow(true)
                .originator("phi")
                .user_agent(format!("phi/{}", env!("CARGO_PKG_VERSION")))
                .on_device_code(move |prompt| {
                    on_device_code(prompt.verification_uri, prompt.user_code)
                })
                .build()?
                .authorize()
                .await?;
            Ok(())
        })
        .await?;
        Ok(())
    }
}

fn ensure_regular_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file(),
            "auth state must be a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn private_file(path: &Path) -> anyhow::Result<File> {
    ensure_regular_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

fn private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

struct Candidate {
    directory: PathBuf,
    path: PathBuf,
}

impl Candidate {
    fn new(destination: &Path) -> anyhow::Result<Self> {
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("auth path has no parent"))?;
        let directory = parent.join(format!(".oauth-{}", uuid::Uuid::new_v4()));
        private_directory(&directory)?;
        Ok(Self {
            path: directory.join("auth.json"),
            directory,
        })
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn oauth_client(path: PathBuf, allow_device_flow: bool) -> anyhow::Result<chatgpt::Client> {
    chatgpt::Client::builder()
        .oauth()
        .auth_file(path)
        .allow_device_flow(allow_device_flow)
        .originator("phi")
        .user_agent(format!("phi/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Into::into)
}

#[derive(Clone)]
pub(super) struct ChatGPTClient {
    store: AuthStore,
}

impl ChatGPTClient {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            store: AuthStore::new(path),
        }
    }

    pub(super) fn completion_model(&self, model: impl Into<String>) -> ChatGPTModel {
        ChatGPTModel {
            store: self.store.clone(),
            model: model.into(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ChatGPTModel {
    store: AuthStore,
    model: String,
}

impl ChatGPTModel {
    async fn authorized_model(
        &self,
    ) -> Result<
        chatgpt::ResponsesCompletionModel<super::transport::ProviderTransport>,
        CompletionError,
    > {
        let credentials = self.store.credentials().await.map_err(auth_error)?;
        let auth = chatgpt::ChatGPTAuth::AccessToken {
            access_token: credentials["access_token"].as_str().unwrap().to_owned(),
            account_id: credentials
                .get("account_id")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
        };
        // Authentication has settled under the store lock; requests cannot refresh again.
        let client = chatgpt::Client::builder()
            .api_key(auth)
            .http_client(super::transport::ProviderTransport::default())
            .originator("phi")
            .user_agent(format!("phi/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(auth_error)?;
        Ok(client.completion_model(self.model.clone()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AuthorizationError(#[source] anyhow::Error);

fn auth_error(error: impl Into<anyhow::Error>) -> CompletionError {
    match error.into().downcast::<AuthorizationError>() {
        Ok(error) => CompletionError::RequestError(Box::new(error)),
        Err(error) => CompletionError::ProviderError(error.to_string()),
    }
}

pub(crate) fn is_authorization_error(error: &CompletionError) -> bool {
    matches!(error, CompletionError::RequestError(source) if source.is::<AuthorizationError>())
}

impl CompletionModel for ChatGPTModel {
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        self.authorized_model().await?.completion(request).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        self.authorized_model().await?.stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> Lease {
        Lease {
            issued_at: 1_000,
            deadline: 1_000 + LEASE_SECONDS,
            last_checked_at: 1_100,
        }
    }

    #[test]
    fn lease_enforces_exact_deadline_and_clock_rollback() {
        let lease = lease();
        assert!(lease.validate(lease.deadline - 1).is_ok());
        assert!(lease.validate(lease.deadline).is_err());
        assert!(lease.validate(lease.deadline + 1).is_err());
        assert!(lease.validate(lease.last_checked_at - 1).is_err());
        assert!(lease.validate(lease.last_checked_at).is_ok());
    }

    struct Fixture {
        directory: PathBuf,
        store: AuthStore,
        now: Arc<std::sync::atomic::AtomicI64>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("phi-oauth-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let now = Arc::new(std::sync::atomic::AtomicI64::new(1_100));
            let clock = now.clone();
            let store = AuthStore {
                path: directory.join("codex.json"),
                clock: Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst)),
            };
            Self {
                directory,
                store,
                now,
            }
        }

        fn set_time(&self, now: i64) {
            self.now.store(now, std::sync::atomic::Ordering::SeqCst);
        }

        fn seed(&self) {
            self.store
                .publish(&AuthState {
                    version: STATE_VERSION,
                    lease: lease(),
                    credentials: credentials("original"),
                })
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn credentials(token: &str) -> serde_json::Value {
        serde_json::json!({ "access_token": token, "refresh_token": "refresh", "expires_at": 9_999_999_999_i64, "account_id": "account" })
    }

    async fn write_credentials(path: PathBuf, token: &str) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_vec(&credentials(token))?)?;
        Ok(())
    }

    #[tokio::test]
    async fn login_uses_fresh_candidate_and_failed_login_preserves_state() {
        let fixture = Fixture::new();
        fixture.seed();
        let original = std::fs::read(&fixture.store.path).unwrap();
        let failed = fixture
            .store
            .maintain(true, |path| async move {
                assert!(!path.exists());
                write_credentials(path, "failed").await?;
                anyhow::bail!("failed login")
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(std::fs::read(&fixture.store.path).unwrap(), original);
        assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 1);
        fixture.set_time(2_000);
        fixture
            .store
            .maintain(true, |path| async move {
                assert!(!path.exists());
                write_credentials(path, "new-login").await
            })
            .await
            .unwrap();
        let state = fixture.store.load().unwrap();
        assert_eq!(state.lease.issued_at, 2_000);
        assert_eq!(state.lease.deadline, 2_000 + LEASE_SECONDS);
        assert_eq!(state.credentials["access_token"], "new-login");
        assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn refresh_preserves_lease_across_reconstruction_and_clock_rollback() {
        let fixture = Fixture::new();
        fixture.seed();
        fixture.set_time(2_000);
        fixture
            .store
            .maintain(false, |path| write_credentials(path, "refreshed"))
            .await
            .unwrap();
        let state = fixture.store.load().unwrap();
        assert_eq!(state.lease.issued_at, lease().issued_at);
        assert_eq!(state.lease.deadline, lease().deadline);
        assert_eq!(state.lease.last_checked_at, 2_000);
        let reconstructed = AuthStore {
            path: fixture.store.path.clone(),
            clock: fixture.store.clock.clone(),
        };
        fixture.set_time(1_999);
        assert!(
            reconstructed
                .credentials()
                .await
                .unwrap_err()
                .to_string()
                .contains("backwards")
        );
        fixture.set_time(2_000);
        assert_eq!(
            reconstructed.credentials().await.unwrap()["access_token"],
            "refreshed"
        );
    }

    #[tokio::test]
    async fn legacy_missing_malformed_and_expired_state_never_reaches_rig() {
        let fixture = Fixture::new();
        for contents in [
            None,
            Some("{}"),
            Some("broken"),
            Some(r#"{"access_token":"legacy"}"#),
        ] {
            if let Some(contents) = contents {
                std::fs::write(&fixture.store.path, contents).unwrap();
            }
            let error = fixture
                .store
                .maintain(false, |_| async {
                    panic!("unauthorized credentials reached Rig")
                })
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("authorization")
                    || error.to_string().contains("OAuth state")
            );
        }
        fixture.seed();
        fixture.set_time(lease().deadline);
        assert!(
            fixture
                .store
                .maintain(false, |_| async { panic!("expired lease reached Rig") })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn revocation_and_expiry_during_refresh_cannot_publish_or_renew() {
        let fixture = Fixture::new();
        fixture.seed();
        fixture.set_time(2_000);
        assert!(
            fixture
                .store
                .maintain(false, |_| async { anyhow::bail!("invalid_grant") })
                .await
                .is_err()
        );
        assert_eq!(
            fixture.store.load().unwrap().lease.deadline,
            lease().deadline
        );
        assert_eq!(
            fixture.store.load().unwrap().credentials["access_token"],
            "original"
        );
        let clock = fixture.now.clone();
        assert!(
            fixture
                .store
                .maintain(false, |path| async move {
                    write_credentials(path, "too-late").await?;
                    clock.store(lease().deadline, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert_eq!(
            fixture.store.load().unwrap().credentials["access_token"],
            "original"
        );
    }

    #[tokio::test]
    async fn rig_maintenance_does_not_fall_back_to_device_login() {
        let fixture = Fixture::new();
        fixture.seed();
        let mut state = fixture.store.load().unwrap();
        state.credentials = serde_json::json!({"access_token": "expired", "expires_at": 1});
        fixture.store.publish(&state).unwrap();
        let error = fixture.store.credentials().await.unwrap_err().to_string();
        assert!(error.contains("sign-in required"), "{error}");
        assert_eq!(
            fixture.store.load().unwrap().lease.deadline,
            lease().deadline
        );
    }

    #[tokio::test]
    async fn all_model_request_paths_check_the_live_lease() {
        let fixture = Fixture::new();
        fixture.seed();
        let client = ChatGPTClient {
            store: fixture.store.clone(),
        };
        let model = client.completion_model("test");
        let request = model.completion_request("test").build();
        let completion =
            crate::provider::generation::PreparedGeneration::new(model.clone(), request.clone())
                .unwrap();
        let stream =
            crate::provider::generation::PreparedGeneration::new(model.clone(), request.clone())
                .unwrap();
        fixture.set_time(lease().deadline);
        let scope = crate::provider::transport::ResponseScope::new(1);
        scope
            .run(async {
                let completion_error = completion.complete().await.unwrap_err();
                let stream_error = stream.start_attempt().await.err().unwrap();
                for error in [completion_error, stream_error] {
                    assert!(error.to_string().contains("expired"));
                    assert!(is_authorization_error(&error));
                    assert_eq!(
                        crate::provider::error_facts(&error).kind,
                        crate::provider::ProviderErrorKind::Authentication
                    );
                    assert!(std::error::Error::source(&error).is_some());
                    assert_eq!(error.provider_response_status(), None);
                }
                fixture.set_time(lease().last_checked_at - 1);
                let error = model.stream(request).await.err().unwrap();
                assert!(is_authorization_error(&error));
                assert_eq!(error.provider_response_status(), None);
            })
            .await;
        assert_eq!(scope.exceeded_status(), None);
    }

    #[test]
    fn credential_io_failures_are_not_authorization_rejections() {
        let error = auth_error(std::io::Error::other("credential store unavailable"));
        assert!(!is_authorization_error(&error));
        assert_eq!(
            crate::provider::error_facts(&error).kind,
            crate::provider::ProviderErrorKind::Unknown
        );
        assert_eq!(error.provider_response_status(), None);
    }

    #[tokio::test]
    async fn independent_stores_serialize_refresh_and_new_login() {
        let fixture = Fixture::new();
        fixture.seed();
        let first = fixture.store.clone();
        let second = AuthStore {
            path: first.path.clone(),
            clock: first.clock.clone(),
        };
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let refresh = tokio::spawn(async move {
            first
                .maintain(false, |path| async move {
                    entered_tx.send(()).unwrap();
                    release_rx.await?;
                    write_credentials(path, "refreshed").await
                })
                .await
        });
        entered_rx.await.unwrap();
        let login = tokio::spawn(async move {
            second
                .maintain(true, |path| write_credentials(path, "new-login"))
                .await
        });
        assert_eq!(
            fixture.store.load().unwrap().credentials["access_token"],
            "original"
        );
        release_tx.send(()).unwrap();
        refresh.await.unwrap().unwrap();
        login.await.unwrap().unwrap();
        assert_eq!(
            fixture.store.load().unwrap().credentials["access_token"],
            "new-login"
        );
        assert!(std::fs::read_dir(&fixture.directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".oauth-")
        }));
    }

    #[tokio::test]
    async fn cancelled_authentication_discards_candidate_and_releases_lock() {
        let fixture = Fixture::new();
        fixture.seed();
        let original = std::fs::read(&fixture.store.path).unwrap();
        let store = fixture.store.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            store
                .maintain(true, |path| async move {
                    write_credentials(path, "cancelled-login").await?;
                    tx.send(()).unwrap();
                    std::future::pending().await
                })
                .await
        });
        rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(std::fs::read(&fixture.store.path).unwrap(), original);
        fixture.store.credentials().await.unwrap();
        assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn auth_lock_serializes_processes_and_waiting_is_cancellable() {
        const CHILD_PATH: &str = "PHI_TEST_AUTH_LOCK_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            let store = AuthStore::new(path.clone());
            assert_eq!(
                private_directory(&store.lock_path()).unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
            std::fs::write(path.with_extension("ready"), "ready").unwrap();
            let _lock = store.lock().await.unwrap();
            std::fs::write(path.with_extension("acquired"), "acquired").unwrap();
            return;
        }
        let fixture = Fixture::new();
        let lock = fixture.store.lock().await.unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "provider::auth::tests::auth_lock_serializes_processes_and_waiting_is_cancellable",
                "--nocapture",
            ])
            .env(CHILD_PATH, &fixture.store.path)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !fixture.store.path.with_extension("ready").exists() {
                tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
            }
        })
        .await
        .unwrap();
        assert!(!fixture.store.path.with_extension("acquired").exists());
        let store = fixture.store.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            entered_tx.send(()).unwrap();
            store.lock().await
        });
        entered_rx.await.unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(fixture.store.lock_path().is_dir());
        drop(lock);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert!(fixture.store.path.with_extension("acquired").exists());
        assert!(!fixture.store.lock_path().exists());
    }

    #[tokio::test]
    async fn auth_lock_timeout_preserves_the_owner_and_allows_later_acquisition() {
        let fixture = Fixture::new();
        let path = fixture.store.lock_path();
        let owner = fixture.store.lock().await.unwrap();
        let error = AuthLock::acquire(path.clone(), std::time::Duration::from_millis(20))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("timed out"));
        assert!(path.is_dir());
        drop(owner);
        assert!(!path.exists());
        drop(fixture.store.lock().await.unwrap());
        assert!(!path.exists());
        private_directory(&path).unwrap();
        assert!(
            AuthLock::acquire(path.clone(), std::time::Duration::from_millis(20))
                .await
                .is_err()
        );
        assert!(path.is_dir());
    }

    #[test]
    fn malformed_lease_never_grants_authorization() {
        for invalid in [
            Lease {
                deadline: lease().deadline + 1,
                ..lease()
            },
            Lease {
                last_checked_at: 999,
                ..lease()
            },
            Lease {
                last_checked_at: lease().deadline,
                ..lease()
            },
        ] {
            assert!(invalid.validate(1_100).is_err());
        }
    }
}
