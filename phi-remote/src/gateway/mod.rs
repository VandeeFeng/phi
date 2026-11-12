use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, ensure};
use phi_sdk::agent_instance::{AgentInstanceId, AgentInstanceView};
use phi_sdk::agent_manager::AgentManager;
use phi_sdk::interaction::{InteractionEnvelope, InteractionResponse};
use phi_sdk::turn::{SubmissionDispatch, SubmissionDisposition, TurnPrompt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio_util::task::TaskTracker;

use crate::protocol::ErrorCode;
use crate::session_segment;

mod connection;
mod events;
mod projection;
mod routes;
pub(crate) mod scheduler;

const MAX_REQUESTS: usize = 16;
const MAX_BODY_BYTES: usize = 16 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(40);
const GATEWAY_CLOSE_TIMEOUT: Duration = Duration::from_secs(40);

/// Session preparation and persistence stay with the application, not the transport.
pub trait RemoteHost: Send + Sync + 'static {
    type Recovery: Serialize + Send;

    fn recovery(
        &self,
        session: AgentInstanceId,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteSnapshot<Self::Recovery>>> + Send;
    fn recovery_at_boundary(
        &self,
        _session: AgentInstanceId,
        _boundary: String,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteSnapshot<Self::Recovery>>> + Send
    {
        async { anyhow::bail!(ErrorCode::Unavailable) }
    }
    fn configure(
        &self,
        session: AgentInstanceId,
        selection: crate::RemoteSelection,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteRuntimeUpdate<Self::Recovery>>> + Send;
    fn reload(
        &self,
        session: AgentInstanceId,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteRuntimeUpdate<Self::Recovery>>> + Send;
    fn catalog(
        &self,
        session: AgentInstanceId,
    ) -> impl std::future::Future<Output = anyhow::Result<crate::RemoteCatalog>> + Send;
    fn new_session(
        &self,
        source: AgentInstanceId,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteSession>> + Send;
    fn sessions(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<RemoteSession>>> + Send;
    fn create(
        &self,
        working_dir: PathBuf,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteSession>> + Send;
    fn saved(
        &self,
        query: String,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<SavedSession>>> + Send;
    fn resume(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteSession>> + Send;
    fn close(
        &self,
        id: AgentInstanceId,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn submit(
        &self,
        session: AgentInstanceId,
        prompt: TurnPrompt,
        dispatch: SubmissionDispatch,
    ) -> impl std::future::Future<Output = anyhow::Result<RemoteSubmission>> + Send;
    fn message(
        &self,
        session: AgentInstanceId,
        target: AgentInstanceId,
        message: String,
    ) -> impl std::future::Future<
        Output = anyhow::Result<phi_sdk::agent_manager::AgentMessageReceipt>,
    > + Send;
    /// Empty lists distinguish stale questions from foreign targets.
    fn interactions(
        &self,
        session: AgentInstanceId,
    ) -> impl std::future::Future<
        Output = anyhow::Result<Vec<(AgentInstanceId, Vec<InteractionEnvelope>)>>,
    > + Send;
    fn respond(
        &self,
        session: AgentInstanceId,
        agent: AgentInstanceId,
        request: String,
        response: InteractionResponse,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<String>>> + Send;
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSession {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub working_dir: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSession {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub first_user_prompt: Option<String>,
}

#[derive(Clone)]
pub struct RemoteSnapshot<Application = Value> {
    pub session: RemoteSession,
    pub view: AgentInstanceView,
    pub interactions: Vec<RemoteInteraction>,
    pub application: Application,
}

pub struct RemoteRuntimeUpdate<Application> {
    pub session: RemoteSession,
    pub transcript: phi_sdk::transcript::Transcript,
    pub snapshot: phi_sdk::agent_instance::AgentInstanceSnapshot,
    pub application: Application,
}

#[derive(Clone)]
pub struct RemoteInteraction {
    pub agent_id: String,
    pub request: InteractionEnvelope,
}

#[derive(Serialize, Deserialize)]
pub struct RemoteSubmission {
    pub disposition: SubmissionDisposition,
    pub notices: Vec<phi_sdk::hooks::HookNotice>,
}

struct GatewayState<H> {
    manager: AgentManager,
    host: H,
    token: String,
    origins: Vec<crate::network::GatewayOrigin>,
    requests: Arc<Semaphore>,
    controls: Arc<Semaphore>,
    authentication: Option<GatewayAccess>,
    tasks: TaskTracker,
    failed: AtomicBool,
    shutdown: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct GatewayAccess {
    store: Arc<dyn crate::pairing::PairingStore>,
    trusted_proxies: Vec<IpAddr>,
}

impl GatewayAccess {
    pub fn new(store: Arc<dyn crate::pairing::PairingStore>, trusted_proxies: Vec<IpAddr>) -> Self {
        Self {
            store,
            trusted_proxies,
        }
    }
}

/// Dropping an observer never stops an Agent; the application finalizes execution.
pub struct Gateway {
    url: String,
    local_url: String,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    plugin_tasks: crate::plugins::PluginTasks,
    scheduler: scheduler::GatewayScheduler,
    scheduler_task: Option<tokio::task::JoinHandle<()>>,
    plugins: Option<crate::plugins::PluginRegistry>,
    shutdown_failures: Vec<String>,
}

impl Gateway {
    pub async fn serve(
        manager: AgentManager,
        host: impl RemoteHost,
        assets: PathBuf,
        config: crate::GatewayConfig,
        token: String,
    ) -> anyhow::Result<Self> {
        ensure!(
            config.public_origin.is_none(),
            "public ingress requires device authentication"
        );
        Self::serve_inner(manager, host, assets, config, token, None).await
    }

    pub async fn serve_paired(
        manager: AgentManager,
        host: impl RemoteHost,
        assets: PathBuf,
        config: crate::GatewayConfig,
        local_credential: String,
        access: GatewayAccess,
    ) -> anyhow::Result<Self> {
        Self::serve_inner(
            manager,
            host,
            assets,
            config,
            local_credential,
            Some(access),
        )
        .await
    }

    async fn serve_inner(
        manager: AgentManager,
        host: impl RemoteHost,
        _assets: PathBuf,
        config: crate::GatewayConfig,
        token: String,
        authentication: Option<GatewayAccess>,
    ) -> anyhow::Result<Self> {
        ensure!(
            token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "gateway token must contain 64 hexadecimal characters"
        );
        let mut plugins = config.plugins;
        let plugin_routes = plugins.take_routes();
        let listener = TcpListener::bind(config.listen_address)
            .await
            .context("cannot bind phi-remote gateway; another service may already be running")?;
        let address = listener.local_addr()?;
        let local_address = SocketAddr::from((Ipv4Addr::LOCALHOST, address.port()));
        let local_origin = crate::network::GatewayOrigin::local(local_address);
        let browser_origin = config.public_origin.as_ref().unwrap_or(&local_origin);
        let local_url = format!("{}/", local_origin.url);
        let url = format!("{}/", browser_origin.url);
        let mut origins = vec![local_origin];
        origins.extend(config.public_origin);
        let (shutdown, mut closed) = watch::channel(false);
        let state = Arc::new(GatewayState {
            manager,
            host,
            token,
            origins,
            requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
            controls: Arc::new(Semaphore::new(crate::protocol::CONTROL_REQUESTS)),
            authentication: authentication.clone(),
            tasks: TaskTracker::new(),
            failed: AtomicBool::new(false),
            shutdown: shutdown.clone(),
        });
        let connections = authentication
            .map(|access| {
                connection::ConnectionServer::new(
                    access.store,
                    state.token.clone(),
                    state.origins.clone(),
                    access.trusted_proxies,
                    shutdown.clone(),
                    state.clone(),
                )
            })
            .transpose()?;
        let router = routes::router(state.clone(), connections.as_ref(), plugin_routes);
        let (scheduler, scheduler_task) = scheduler::GatewayScheduler::start(shutdown.subscribe());
        let task = tokio::spawn(async move {
            let result = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = closed.wait_for(|stopping| *stopping).await;
            })
            .await;
            let connection_result = if let Some(connections) = connections {
                connections.drain().await
            } else {
                Ok(())
            };
            state.tasks.close();
            state.tasks.wait().await;
            connection_result
                .map_err(|_| std::io::Error::other("Remote connection supervision failed"))?;
            if state.failed.load(Ordering::Acquire) {
                return Err(std::io::Error::other("Remote HTTP supervision failed"));
            }
            result
        });
        let plugin_tasks =
            crate::plugins::PluginTasks::new(shutdown.subscribe(), scheduler.clone());
        Ok(Self {
            url,
            local_url,
            shutdown,
            task: Some(task),
            plugin_tasks,
            scheduler,
            scheduler_task: Some(scheduler_task),
            plugins: Some(plugins),
            shutdown_failures: Vec::new(),
        })
    }

    /// Starts registered background entry points once the deployment is ready.
    /// No client is created unless a plugin requests one through its context.
    pub fn start_plugins(&mut self, directory: &std::path::Path) {
        if self.is_stopping() {
            return;
        }
        if let Some(plugins) = self.plugins.take() {
            self.plugin_tasks.start(plugins, directory, &self.local_url);
        }
    }

    /// Controls plugin background runs while the host continuously polls `wait()`.
    pub fn plugin_control(&self) -> crate::plugins::PluginControl {
        self.plugin_tasks.control()
    }

    pub fn scheduler(&self) -> scheduler::GatewayScheduler {
        self.scheduler.clone()
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn local_url(&self) -> &str {
        &self.local_url
    }
    pub fn is_stopping(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub fn request_shutdown(&self) {
        self.scheduler.close();
        self.shutdown.send_replace(true);
    }

    /// Drives task supervision and plugin controls until transport shutdown completes.
    /// Dropping this future pauses supervision, but does not cancel owned work.
    /// Poll it continuously during service operation; resume it to continue draining.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        if let Some(task) = &mut self.task {
            let result = loop {
                tokio::select! {
                    result = &mut *task => break result,
                    result = async { self.scheduler_task.as_mut().expect("scheduler task missing").await }, if self.scheduler_task.is_some() => {
                        self.scheduler_task = None;
                        self.scheduler.close();
                        match result {
                            Err(error) => self.shutdown_failures.push(format!("Gateway scheduler failed: {error}")),
                            Ok(()) if !*self.shutdown.borrow() => self.shutdown_failures.push("Gateway scheduler stopped unexpectedly".into()),
                            Ok(()) => {},
                        }
                        let _ = self.shutdown.send(true);
                    }
                    () = self.plugin_tasks.next() => {}
                }
            };
            self.task = None;
            self.request_shutdown();
            if let Err(error) = result
                .map_err(anyhow::Error::from)
                .and_then(|result| result.map_err(Into::into))
            {
                self.shutdown_failures.push(error.to_string());
            }
        }
        self.shutdown_failures
            .extend(self.plugin_tasks.drain().await);
        if let Some(task) = &mut self.scheduler_task {
            if let Err(error) = task.await {
                self.shutdown_failures
                    .push(format!("Gateway scheduler failed: {error}"));
            }
            self.scheduler_task = None;
        }
        ensure!(
            self.shutdown_failures.is_empty(),
            "{}",
            self.shutdown_failures.join("; ")
        );
        Ok(())
    }

    /// Drains owned tasks, or aborts and joins tracked tasks after the shutdown grace.
    /// Forced cancellation is reported as failure, not successful plugin cleanup.
    pub async fn close(mut self) -> anyhow::Result<()> {
        self.request_shutdown();
        match tokio::time::timeout(GATEWAY_CLOSE_TIMEOUT, self.wait()).await {
            Ok(result) => result,
            Err(_) => {
                self.shutdown_failures
                    .push("gateway shutdown timed out".into());
                self.shutdown_failures
                    .extend(self.plugin_tasks.abort().await);
                if let Some(task) = self.task.take() {
                    task.abort();
                    let _ = task.await;
                }
                if let Some(task) = self.scheduler_task.take() {
                    task.abort();
                    let _ = task.await;
                }
                anyhow::bail!(self.shutdown_failures.join("; "));
            }
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.request_shutdown();
        self.plugin_tasks.shutdown();
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(task) = &self.scheduler_task {
            task.abort();
        }
    }
}

fn operation_error(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<ErrorCode>()
        .copied()
        .unwrap_or(ErrorCode::Unavailable)
}

async fn hosted_session<H: RemoteHost>(
    state: &GatewayState<H>,
    id: &str,
) -> Result<AgentInstanceId, ErrorCode> {
    session_segment(id).map_err(|_| ErrorCode::InvalidRequest)?;
    let sessions = state.host.sessions().await.map_err(operation_error)?;
    if sessions.iter().any(|session| session.id == id) {
        Ok(AgentInstanceId::new(id))
    } else {
        Err(ErrorCode::NotFound)
    }
}

async fn session<H: RemoteHost>(
    state: &GatewayState<H>,
    id: &str,
) -> anyhow::Result<RemoteSession> {
    session_segment(id)?;
    state
        .host
        .sessions()
        .await?
        .into_iter()
        .find(|session| session.id == id)
        .context("unknown gateway Session")
}
#[cfg(test)]
mod tests;
