use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::ensure;
use axum::{
    Router,
    extract::{
        ConnectInfo, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response as HttpResponse},
    routing::get,
};
use futures_util::{
    FutureExt, SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use phi_sdk::agent_instance::AgentInstanceEvent;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, watch};
use tokio::time::Instant;
use tokio_util::task::{TaskTracker, task_tracker::TaskTrackerToken};

use super::{GatewayState, RemoteHost};
use crate::network::{GatewayIngress, GatewayOrigin, IngressKind, admit_ingress};
use crate::pairing::{
    ChallengeRequest, DeviceAuthenticator, DeviceAuthorization, DeviceOperation, DeviceProof,
    PairingOutcome, PairingStatus, PairingStore, constant_matches, credential_digest,
    public_key_id, random_credential,
};
use crate::protocol as policy;
use crate::protocol::{
    self, Capability, Connect, ConnectAuth, ConnectChallenge, ConnectResult, EmptyParams,
    ErrorCode, Event, EventName, Hello, Method, PairingResolution, RemoteError, Request, RequestId,
    RequestOrder, Response,
};

const DISPATCH_CAPACITY: usize = 64;
const POLICY_CLOSE: u16 = 1008;
const SIZE_CLOSE: u16 = 1009;
const RESTART_CLOSE: u16 = 1012;
const NORMAL_CLOSE: u16 = 1000;

type Authenticator = DeviceAuthenticator<dyn PairingStore>;

struct Outbound {
    text: String,
    close_after: Option<u16>,
    authorization: Option<DeviceAuthorization>,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientIngress {
    Native,
    Browser,
}

struct RootWatch {
    id: RequestId,
    session_id: String,
    task: tokio::task::AbortHandle,
}

impl Drop for RootWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Connection {
    active_watch: Option<RootWatch>,
    latest_watch: Option<RequestId>,
    source: IpAddr,
    audience: String,
    ingress: ClientIngress,
    stage: Stage,
    deadline: Instant,
    next_sequence: u64,
    outbound: mpsc::Sender<Outbound>,
    byte_budget: Arc<Semaphore>,
    close: watch::Sender<Option<u16>>,
    expires: watch::Sender<Option<Instant>>,
    inbound_limit: Arc<AtomicUsize>,
    preauth: Option<OwnedSemaphorePermit>,
    controls: Arc<Semaphore>,
    _total: OwnedSemaphorePermit,
}

enum Stage {
    Connect,
    Proof {
        credential: String,
        challenge: crate::pairing::DeviceChallenge,
    },
    Pending {
        request_id: String,
        device_id: String,
        expires_at: u64,
        last_status: Option<Instant>,
    },
    Local,
    Device(DeviceAuthorization),
    Closed,
}

impl Connection {
    fn authenticate(&mut self, stage: Stage) {
        self.stage = stage;
        self.preauth.take();
        let _ = self.expires.send(None);
        self.inbound_limit
            .store(policy::REQUEST_BYTES, Ordering::Release);
    }

    fn retire(&mut self, code: u16) {
        self.active_watch = None;
        self.stage = Stage::Closed;
        let _ = self.close.send(Some(code));
    }

    fn enqueue(&mut self, value: impl Serialize, close_after: Option<u16>) {
        let Ok(text) = serde_json::to_string(&value) else {
            self.retire(POLICY_CLOSE);
            return;
        };
        if text.len() > policy::SERVER_BYTES {
            self.retire(SIZE_CLOSE);
            return;
        }
        let Ok(bytes) = self
            .byte_budget
            .clone()
            .try_acquire_many_owned(text.len() as u32)
        else {
            self.retire(POLICY_CLOSE);
            return;
        };
        if self
            .outbound
            .try_send(Outbound {
                text,
                close_after,
                authorization: match &self.stage {
                    Stage::Device(authorization) => Some(authorization.clone()),
                    _ => None,
                },
                _bytes: bytes,
            })
            .is_err()
        {
            self.retire(POLICY_CLOSE);
        }
    }

    fn event(&mut self, name: EventName, payload: impl Serialize, close_after: Option<u16>) {
        let Some(event) = Event::new(name, self.next_sequence, payload) else {
            self.retire(POLICY_CLOSE);
            return;
        };
        self.next_sequence += 1;
        self.enqueue(event, close_after);
    }

    fn reject(&mut self, id: RequestId, code: ErrorCode, close_after: Option<u16>) {
        self.enqueue(
            Response::<Value>::failure(id, remote_error(code)),
            close_after,
        );
        if close_after.is_some() {
            self.stage = Stage::Closed;
        }
    }
}

struct ConnectionGuard {
    id: String,
    owner: Weak<ConnectionServer>,
    _lifetime: TaskTrackerToken,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            let registry = owner.clone();
            let id = self.id.clone();
            let _ = owner.tasks.spawn_blocking(move || registry.remove(&id));
        }
    }
}

struct AdmittedSocket {
    guard: ConnectionGuard,
    outgoing: mpsc::Receiver<Outbound>,
    closed: watch::Receiver<Option<u16>>,
    expires: watch::Receiver<Option<Instant>>,
    inbound_limit: Arc<AtomicUsize>,
}

struct Work {
    connection_id: String,
    request: Request<Value>,
    bytes: usize,
}

struct SessionWork {
    connection_id: String,
    request: Request<Value>,
    _connection: OwnedSemaphorePermit,
    _process: OwnedSemaphorePermit,
}

enum SessionOutcome {
    Watch {
        session_id: String,
        events: broadcast::Receiver<AgentInstanceEvent>,
    },
    Unwatch,
}

pub(crate) struct ConnectionServer {
    authenticator: Authenticator,
    gateway_id: String,
    boot_id: String,
    local_credential: String,
    origins: Vec<GatewayOrigin>,
    trusted_proxies: Vec<IpAddr>,
    connections: Mutex<HashMap<String, Connection>>,
    total: Arc<Semaphore>,
    preauth: Arc<Semaphore>,
    controls: Arc<Semaphore>,
    work: mpsc::Sender<Work>,
    control_work: mpsc::Sender<Work>,
    shutdown: watch::Sender<bool>,
    tasks: TaskTracker,
    failed: AtomicBool,
}

impl ConnectionServer {
    pub(super) fn new<H: RemoteHost>(
        store: Arc<dyn PairingStore>,
        local_credential: String,
        origins: Vec<GatewayOrigin>,
        trusted_proxies: Vec<IpAddr>,
        shutdown: watch::Sender<bool>,
        state: Arc<GatewayState<H>>,
    ) -> anyhow::Result<Arc<Self>> {
        let gateway_id = store.gateway_id()?;
        let (sender, mut receiver) = mpsc::channel(DISPATCH_CAPACITY);
        let (control_work, mut control_receiver) = mpsc::channel(policy::CONTROL_REQUESTS);
        let server = Arc::new(Self {
            authenticator: Authenticator::new(store),
            gateway_id,
            boot_id: random_credential()?,
            local_credential,
            origins,
            trusted_proxies,
            connections: Mutex::new(HashMap::new()),
            total: Arc::new(Semaphore::new(policy::CONNECTIONS)),
            preauth: Arc::new(Semaphore::new(policy::PREAUTH_CONNECTIONS)),
            controls: Arc::new(Semaphore::new(policy::CONTROL_REQUESTS)),
            work: sender,
            control_work,
            shutdown,
            tasks: TaskTracker::new(),
            failed: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&server);
        let mut stopping = server.shutdown.subscribe();
        server.tasks.spawn(async move {
            let mut maintenance = tokio::time::interval(Duration::from_secs(1));
            loop {
                let work = tokio::select! {
                    biased;
                    _ = async { let _ = stopping.wait_for(|value| *value).await; } => {
                        while let Ok(work) = control_receiver.try_recv() {
                            if let Some(server) = weak.upgrade() { server.run_work(work, state.clone()).await; }
                        }
                        while let Ok(work) = receiver.try_recv() {
                            if let Some(server) = weak.upgrade() { server.run_work(work, state.clone()).await; }
                        }
                        break;
                    }
                    work = control_receiver.recv() => work,
                    work = receiver.recv() => work,
                    _ = maintenance.tick() => {
                        if let Some(server) = weak.upgrade() {
                            let owner = server.clone();
                            if tokio::task::spawn_blocking(move || owner.maintain()).await.is_err() { server.fail(); }
                        }
                        continue;
                    }
                };
                let Some(work) = work else { break };
                if let Some(server) = weak.upgrade() {
                    server.run_work(work, state.clone()).await;
                } else {
                    break;
                }
            }
        });
        Ok(server)
    }

    pub(crate) fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(protocol::WS_PATH, get(upgrade))
            .with_state(self.clone())
    }

    pub(crate) async fn drain(&self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(true);
        self.tasks.close();
        self.tasks.wait().await;
        ensure!(
            !self.failed.load(Ordering::Acquire),
            "Remote connection supervision failed"
        );
        Ok(())
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        let _ = self.shutdown.send(true);
    }

    async fn run_work<H: RemoteHost>(self: &Arc<Self>, work: Work, state: Arc<GatewayState<H>>) {
        let owner = self.clone();
        match tokio::task::spawn_blocking(move || owner.dispatch(work)).await {
            Ok(Some(work)) => {
                let owner = self.clone();
                self.tasks.spawn(async move {
                    let result = std::panic::AssertUnwindSafe(async {
                        match work.request.method {
                            Method::SessionWatch => {
                                let params: protocol::SessionParams =
                                    decode(work.request.params.clone())?;
                                let id = super::hosted_session(&state, &params.session_id).await?;
                                let instance = match state.manager.get(&id) {
                                    Ok(instance) => instance,
                                    Err(_) => {
                                        state
                                            .host
                                            .recovery(id.clone())
                                            .await
                                            .map_err(super::operation_error)?;
                                        state.manager.get(&id).map_err(|_| ErrorCode::NotFound)?
                                    }
                                };
                                Ok(SessionOutcome::Watch {
                                    session_id: params.session_id,
                                    events: instance.subscribe(),
                                })
                            }
                            Method::SessionUnwatch => {
                                let _: EmptyParams = decode(work.request.params.clone())?;
                                Ok(SessionOutcome::Unwatch)
                            }
                            _ => Err(ErrorCode::InvalidRequest),
                        }
                    })
                    .catch_unwind()
                    .await;
                    let Ok(result) = result else {
                        owner.fail();
                        return;
                    };
                    let supervisor = owner.clone();
                    if tokio::task::spawn_blocking(move || owner.complete_session(work, result))
                        .await
                        .is_err()
                    {
                        supervisor.fail();
                    }
                });
            }
            Ok(None) => {}
            Err(_) => self.fail(),
        }
    }

    fn complete_session(
        self: &Arc<Self>,
        work: SessionWork,
        result: Result<SessionOutcome, ErrorCode>,
    ) {
        let Ok(mut connections) = self.connections.lock() else {
            self.fail();
            return;
        };
        let Some(connection) = connections.get_mut(&work.connection_id) else {
            return;
        };
        if !matches!(connection.stage, Stage::Local | Stage::Device(_)) {
            return;
        }
        if *self.shutdown.borrow() {
            connection.retire(RESTART_CLOSE);
        } else if self.authorized(connection).is_err() {
            connection.retire(POLICY_CLOSE);
        } else {
            let result = if matches!(
                work.request.method,
                Method::SessionWatch | Method::SessionUnwatch
            ) && connection.latest_watch != Some(work.request.id)
            {
                Err(ErrorCode::Unavailable)
            } else {
                result
            };
            let mut source = None;
            let response = match result {
                Ok(SessionOutcome::Unwatch) => {
                    connection.active_watch = None;
                    Response::success(work.request.id, json!({}))
                }
                Ok(SessionOutcome::Watch { session_id, events }) => {
                    connection.active_watch = None;
                    let value = json!({"sessionId":session_id,"watchId":work.request.id});
                    source = Some((session_id, events));
                    Response::success(work.request.id, value)
                }
                Err(code) => Response::failure(work.request.id, remote_error(code)),
            };
            match serde_json::to_vec(&response) {
                Ok(bytes) if bytes.len() <= policy::SERVER_BYTES => {
                    connection.enqueue(response, None);
                    if !matches!(connection.stage, Stage::Closed)
                        && let Some((session_id, mut events)) = source
                    {
                        let weak = Arc::downgrade(self);
                        let connection_id = work.connection_id.clone();
                        let watch_id = work.request.id;
                        let task = self.tasks.spawn(async move {
                            loop {
                                let event = events.recv().await;
                                let Some(owner) = weak.upgrade() else { break };
                                let id = connection_id.clone();
                                let supervisor = owner.clone();
                                match tokio::task::spawn_blocking(move || {
                                    owner.deliver_root(&id, watch_id, event)
                                })
                                .await
                                {
                                    Ok(true) => {}
                                    Ok(false) => break,
                                    Err(_) => {
                                        supervisor.fail();
                                        break;
                                    }
                                }
                            }
                        });
                        connection.active_watch = Some(RootWatch {
                            id: watch_id,
                            session_id,
                            task: task.abort_handle(),
                        });
                    }
                }
                Ok(_) => connection.reject(work.request.id, ErrorCode::LimitExceeded, None),
                Err(_) => connection.reject(work.request.id, ErrorCode::Internal, None),
            }
        }
    }

    fn deliver_root(
        &self,
        id: &str,
        watch_id: RequestId,
        event: Result<AgentInstanceEvent, broadcast::error::RecvError>,
    ) -> bool {
        let event = event.map(super::events::project);
        let Ok(mut connections) = self.connections.lock() else {
            self.fail();
            return false;
        };
        let Some(connection) = connections.get_mut(id) else {
            return false;
        };
        let Some(watch) = connection
            .active_watch
            .as_ref()
            .filter(|watch| watch.id == watch_id)
        else {
            return false;
        };
        let session_id = watch.session_id.clone();
        if *self.shutdown.borrow() {
            connection.retire(RESTART_CLOSE);
        } else if self.authorized(connection).is_err() || event.is_err() {
            connection.retire(POLICY_CLOSE);
        } else if let Ok(event) = event {
            connection.event(
                EventName::SessionEvent,
                json!({"sessionId":session_id,"watchId":watch_id,"event":event}),
                None,
            );
        }
        !matches!(connection.stage, Stage::Closed)
    }

    fn register(
        self: &Arc<Self>,
        ingress: GatewayIngress,
        client_ingress: ClientIngress,
    ) -> anyhow::Result<AdmittedSocket> {
        ensure!(!*self.shutdown.borrow(), "Gateway is stopping");
        let total = self.total.clone().try_acquire_owned()?;
        let preauth = self.preauth.clone().try_acquire_owned()?;
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| anyhow::anyhow!("Gateway unavailable"))?;
        ensure!(
            connections
                .values()
                .filter(|connection| connection.source == ingress.source
                    && connection.preauth.is_some())
                .count()
                < policy::PREAUTH_PER_SOURCE,
            "source connection limit"
        );
        let connection_id = random_credential()?;
        let (outbound, receiver) = mpsc::channel(policy::QUEUE_MESSAGES);
        let (close, closed) = watch::channel(None);
        let deadline = Instant::now() + policy::HANDSHAKE_TIMEOUT;
        let (expires, expiry) = watch::channel(Some(deadline));
        let inbound_limit = Arc::new(AtomicUsize::new(policy::AUTH_BYTES));
        let mut connection = Connection {
            active_watch: None,
            latest_watch: None,
            source: ingress.source,
            audience: format!("{}{}", ingress.audience, protocol::WS_PATH),
            ingress: client_ingress,
            stage: Stage::Connect,
            deadline,
            next_sequence: 1,
            outbound,
            byte_budget: Arc::new(Semaphore::new(policy::QUEUE_BYTES)),
            close,
            expires,
            inbound_limit: inbound_limit.clone(),
            preauth: Some(preauth),
            controls: Arc::new(Semaphore::new(policy::CONTROL_PER_CONNECTION)),
            _total: total,
        };
        connection.event(
            EventName::ConnectChallenge,
            ConnectChallenge {
                min_protocol: protocol::VERSION,
                max_protocol: protocol::VERSION,
                gateway_id: self.gateway_id.clone(),
                boot_id: self.boot_id.clone(),
                connection_id: connection_id.clone(),
                nonce: random_credential()?,
                expires_at: clock()? + policy::HANDSHAKE_TIMEOUT.as_secs(),
            },
            None,
        );
        connections.insert(connection_id.clone(), connection);
        Ok(AdmittedSocket {
            guard: ConnectionGuard {
                id: connection_id,
                owner: Arc::downgrade(self),
                _lifetime: self.tasks.token(),
            },
            outgoing: receiver,
            closed,
            expires: expiry,
            inbound_limit,
        })
    }

    fn remove(&self, id: &str) {
        self.authenticator.retire(&self.boot_id, id);
        match self.connections.lock() {
            Ok(mut connections) => {
                connections.remove(id);
            }
            Err(_) => self.fail(),
        }
    }

    fn hello(&self, connection_id: &str, connection: &Connection) -> Hello {
        Hello {
            protocol: protocol::VERSION,
            gateway_id: self.gateway_id.clone(),
            boot_id: self.boot_id.clone(),
            connection_id: connection_id.to_owned(),
            capabilities: if matches!(connection.stage, Stage::Local) {
                vec![Capability::HostedSessions, Capability::LocalManagement]
            } else {
                vec![Capability::HostedSessions]
            },
            limits: policy::Limits::default(),
        }
    }

    fn dispatch(&self, work: Work) -> Option<SessionWork> {
        let Ok(mut connections) = self.connections.lock() else {
            self.fail();
            return None;
        };
        let Some(mut connection) = connections.remove(&work.connection_id) else {
            return None;
        };
        let id = work.request.id;
        if matches!(connection.stage, Stage::Closed) {
            connections.insert(work.connection_id, connection);
            return None;
        }
        if *self.shutdown.borrow() {
            connection.reject(id, ErrorCode::Unavailable, Some(RESTART_CLOSE));
        } else if !matches!(connection.stage, Stage::Local | Stage::Device(_))
            && Instant::now() >= connection.deadline
        {
            let code = if matches!(connection.stage, Stage::Pending { .. }) {
                ErrorCode::PairingExpired
            } else {
                ErrorCode::Unauthenticated
            };
            connection.reject(id, code, Some(POLICY_CLOSE));
        } else if !matches!(connection.stage, Stage::Local | Stage::Device(_))
            && work.bytes > policy::AUTH_BYTES
        {
            connection.reject(id, ErrorCode::LimitExceeded, Some(SIZE_CLOSE));
        } else if work.request.method.is_watch()
            && matches!(connection.stage, Stage::Local | Stage::Device(_))
        {
            let admission = self.authorized(&connection).and_then(|()| {
                let (local, process) = (&connection.controls, &self.controls);
                let local = local
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ErrorCode::Busy)?;
                let process = process
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ErrorCode::Busy)?;
                Ok((local, process))
            });
            match admission {
                Ok((local, process)) => {
                    connection.latest_watch = Some(id);
                    connections.insert(work.connection_id.clone(), connection);
                    return Some(SessionWork {
                        connection_id: work.connection_id,
                        request: work.request,
                        _connection: local,
                        _process: process,
                    });
                }
                Err(code) => connection.reject(id, code, None),
            }
        } else {
            let result = self.handle(
                &work.connection_id,
                &mut connection,
                &connections,
                work.request,
            );
            self.reconcile(&mut connections);
            match result {
                Ok(value) => {
                    if *self.shutdown.borrow() {
                        connection.retire(RESTART_CLOSE);
                    } else if self.authorized(&connection).is_err() {
                        connection.reject(id, ErrorCode::Revoked, Some(POLICY_CLOSE));
                    } else {
                        connection.enqueue(Response::success(id, value), None);
                    }
                }
                Err(code) => {
                    let terminal = !matches!(
                        connection.stage,
                        Stage::Local | Stage::Device(_) | Stage::Pending { .. }
                    );
                    connection.reject(id, code, terminal.then_some(POLICY_CLOSE));
                }
            }
        }
        connections.insert(work.connection_id, connection);
        None
    }

    fn authorized(&self, connection: &Connection) -> Result<(), ErrorCode> {
        if let Stage::Device(authorization) = &connection.stage {
            self.authenticator
                .store()
                .authorize(authorization)
                .map_err(authority_error)?;
        }
        Ok(())
    }

    fn handle(
        &self,
        connection_id: &str,
        connection: &mut Connection,
        others: &HashMap<String, Connection>,
        request: Request<Value>,
    ) -> Result<Value, ErrorCode> {
        match &connection.stage {
            Stage::Connect if request.method == Method::Connect => {
                let connect: Connect = decode(request.params)?;
                connect.negotiate()?;
                if let ConnectAuth::Local { credential } = connect.auth {
                    if connection.ingress != ClientIngress::Native
                        || !constant_matches(
                            &credential_digest(&credential),
                            &credential_digest(&self.local_credential),
                        )
                    {
                        return Err(ErrorCode::Unauthenticated);
                    }
                    connection.authenticate(Stage::Local);
                    return encode(ConnectResult::Ready {
                        hello: self.hello(connection_id, connection),
                        device_credential: None,
                    });
                }
                let (operation, credential, public_key, request_id) = match connect.auth {
                    ConnectAuth::Device {
                        credential,
                        public_key,
                    } => (DeviceOperation::Reconnect, credential, public_key, None),
                    ConnectAuth::Enroll {
                        credential,
                        public_key,
                    } => (DeviceOperation::Enroll, credential, public_key, None),
                    ConnectAuth::Claim {
                        credential,
                        public_key,
                        request_id,
                    } => (
                        DeviceOperation::Claim,
                        credential,
                        public_key,
                        Some(request_id),
                    ),
                    ConnectAuth::Recover {
                        credential,
                        public_key,
                    } => (DeviceOperation::Recover, credential, public_key, None),
                    ConnectAuth::Local { .. } => unreachable!(),
                };
                let challenge = self
                    .authenticator
                    .challenge_bound(
                        connection.source,
                        &connection.audience,
                        &self.boot_id,
                        connection_id,
                        ChallengeRequest {
                            public_key,
                            operation,
                            request_id,
                        },
                        &credential,
                        clock().map_err(|_| ErrorCode::Unavailable)?,
                    )
                    .map_err(authentication_error)?;
                let result = serde_json::to_value(ConnectResult::ProofRequired {
                    challenge: challenge.clone(),
                })
                .map_err(|_| ErrorCode::Internal)?;
                connection.stage = Stage::Proof {
                    credential,
                    challenge,
                };
                Ok(result)
            }
            Stage::Proof { .. } if request.method == Method::ConnectProve => {
                let proof: DeviceProof = decode(request.params)?;
                let Stage::Proof {
                    credential,
                    challenge,
                } = std::mem::replace(&mut connection.stage, Stage::Closed)
                else {
                    unreachable!()
                };
                let identity = public_key_id(&challenge.public_key);
                let waiters = others.values().filter(|other| matches!(&other.stage, Stage::Pending { device_id, .. } if device_id == &identity)).count();
                if waiters >= policy::WAITERS_PER_REQUEST
                    && matches!(
                        challenge.operation,
                        DeviceOperation::Enroll | DeviceOperation::Claim
                    )
                {
                    return Err(ErrorCode::LimitExceeded);
                }
                let now = clock().map_err(|_| ErrorCode::Unavailable)?;
                let outcome = self
                    .authenticator
                    .prove_bound(
                        connection.source,
                        &connection.audience,
                        &self.boot_id,
                        connection_id,
                        proof,
                        &credential,
                        now,
                    )
                    .map_err(authentication_error)?;
                match outcome {
                    PairingOutcome::Pending {
                        request_id,
                        expires_at,
                    } => {
                        connection.deadline =
                            Instant::now() + Duration::from_secs(expires_at.saturating_sub(now));
                        let _ = connection.expires.send(Some(connection.deadline));
                        connection.stage = Stage::Pending {
                            request_id: request_id.clone(),
                            device_id: identity.clone(),
                            expires_at,
                            last_status: None,
                        };
                        encode(ConnectResult::Pending {
                            request_id,
                            device_id: identity,
                            expires_at,
                        })
                    }
                    PairingOutcome::Authorized { device_id }
                    | PairingOutcome::Credential { device_id, .. }
                        if device_id != identity =>
                    {
                        Err(ErrorCode::Internal)
                    }
                    outcome => {
                        let delivered = match outcome {
                            PairingOutcome::Credential { token, .. } => Some(token),
                            PairingOutcome::Authorized { .. } => None,
                            _ => unreachable!(),
                        };
                        let generation = challenge
                            .authorization_id
                            .or(challenge.request_id)
                            .ok_or(ErrorCode::Unauthenticated)?;
                        let authorization = DeviceAuthorization {
                            device_id: identity,
                            approval_generation: generation,
                            token_digest: credential_digest(
                                delivered.as_deref().unwrap_or(&credential),
                            ),
                        };
                        self.authenticator
                            .store()
                            .authorize(&authorization)
                            .map_err(authority_error)?;
                        connection.authenticate(Stage::Device(authorization));
                        encode(ConnectResult::Ready {
                            hello: self.hello(connection_id, connection),
                            device_credential: delivered,
                        })
                    }
                }
            }
            Stage::Pending { .. } if request.method == Method::PairingStatus => {
                let _: EmptyParams = decode(request.params)?;
                let Stage::Pending {
                    request_id,
                    device_id,
                    expires_at,
                    last_status,
                } = &mut connection.stage
                else {
                    unreachable!()
                };
                if last_status.is_some_and(|last| last.elapsed() < policy::STATUS_INTERVAL) {
                    return Err(ErrorCode::LimitExceeded);
                }
                *last_status = Some(Instant::now());
                let now = clock().map_err(|_| ErrorCode::Unavailable)?;
                let status = self.pending_status(request_id, device_id, *expires_at, now)?;
                encode(PairingResolution {
                    request_id: request_id.clone(),
                    device_id: device_id.clone(),
                    status,
                })
            }
            Stage::Local | Stage::Device(_) => Err(ErrorCode::Forbidden),
            Stage::Pending { .. } => Err(ErrorCode::Forbidden),
            _ => Err(ErrorCode::InvalidRequest),
        }
    }

    fn pending_status(
        &self,
        request_id: &str,
        device_id: &str,
        expires_at: u64,
        now: u64,
    ) -> Result<PairingStatus, ErrorCode> {
        if now >= expires_at {
            return Ok(PairingStatus::Expired);
        }
        self.authenticator
            .store()
            .pairing_status(request_id, device_id, now)
            .map_err(authority_error)
    }

    fn reconcile(&self, connections: &mut HashMap<String, Connection>) {
        let now = match clock() {
            Ok(now) => now,
            Err(_) => {
                for connection in connections.values_mut() {
                    connection.retire(POLICY_CLOSE);
                }
                return;
            }
        };
        for connection in connections.values_mut() {
            match &connection.stage {
                Stage::Device(authorization)
                    if self.authenticator.store().authorize(authorization).is_err() =>
                {
                    connection.retire(POLICY_CLOSE)
                }
                Stage::Pending {
                    request_id,
                    device_id,
                    expires_at,
                    ..
                } => {
                    let status = self.pending_status(request_id, device_id, *expires_at, now);
                    match status {
                        Ok(PairingStatus::Pending) => {}
                        Ok(status) => {
                            let resolution = PairingResolution {
                                request_id: request_id.clone(),
                                device_id: device_id.clone(),
                                status,
                            };
                            connection.event(
                                EventName::PairingResolved,
                                resolution,
                                Some(NORMAL_CLOSE),
                            );
                            connection.stage = Stage::Closed;
                        }
                        Err(_) => connection.retire(POLICY_CLOSE),
                    }
                }
                _ => {}
            }
        }
    }

    fn maintain(&self) {
        if let Ok(mut connections) = self.connections.lock() {
            for connection in connections.values_mut() {
                if matches!(connection.stage, Stage::Connect | Stage::Proof { .. })
                    && Instant::now() >= connection.deadline
                {
                    connection.retire(POLICY_CLOSE);
                }
            }
            self.reconcile(&mut connections);
        } else {
            self.fail();
        }
    }

    async fn authorize_outbound(
        self: &Arc<Self>,
        authorization: Option<DeviceAuthorization>,
    ) -> bool {
        let Some(authorization) = authorization else {
            return true;
        };
        let owner = self.clone();
        matches!(
            tokio::task::spawn_blocking(move || owner
                .authenticator
                .store()
                .authorize(&authorization))
            .await,
            Ok(Ok(()))
        )
    }

    async fn socket(self: Arc<Self>, admission: AdmittedSocket, socket: WebSocket) {
        let AdmittedSocket {
            guard,
            outgoing,
            closed,
            expires,
            inbound_limit,
        } = admission;
        let id = &guard.id;
        let (sink, stream) = socket.split();
        let (pong, pongs) = watch::channel(Vec::new());
        let (reader_closed, writer_closed) = watch::channel(None);
        let mut writer = Box::pin(write_socket(
            self.clone(),
            sink,
            outgoing,
            closed,
            writer_closed,
            self.shutdown.subscribe(),
            pongs,
            expires,
        ));
        let reader = read_socket(self.clone(), id, stream, pong, inbound_limit);
        tokio::pin!(reader);
        tokio::select! {
            _ = &mut writer => {},
            code = &mut reader => {
                let _ = reader_closed.send(Some(code));
                let _ = writer.await;
            },
        }
    }
}

async fn upgrade(
    State(server): State<Arc<ConnectionServer>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> HttpResponse {
    let accepted = (|| {
        ensure!(uri.query().is_none(), "WebSocket query rejected");
        let mut protocols = headers.get_all("sec-websocket-protocol").iter();
        ensure!(
            protocols
                .next()
                .is_some_and(|value| value == protocol::SUBPROTOCOL)
                && protocols.next().is_none(),
            "unsupported WebSocket protocol"
        );
        let ingress = admit_ingress(
            &server.origins,
            &server.trusted_proxies,
            peer.ip(),
            &headers,
        )?;
        ensure!(
            ingress.kind != IngressKind::Public || headers.contains_key("origin"),
            "public browser origin required"
        );
        let client_ingress =
            if ingress.kind == IngressKind::Local && !headers.contains_key("origin") {
                ClientIngress::Native
            } else {
                ClientIngress::Browser
            };
        Ok::<_, anyhow::Error>((ingress, client_ingress))
    })();
    let Ok((ingress, client_ingress)) = accepted else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let registry = server.clone();
    let admission = server
        .tasks
        .spawn_blocking(move || registry.register(ingress, client_ingress))
        .await;
    let Ok(Ok(admission)) = admission else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .protocols([protocol::SUBPROTOCOL])
        .max_message_size(policy::REQUEST_BYTES)
        .max_frame_size(policy::REQUEST_BYTES)
        .on_failed_upgrade(|_| {})
        .on_upgrade(move |socket| async move {
            let owner = server.clone();
            if server
                .tasks
                .spawn(async move { owner.socket(admission, socket).await })
                .await
                .is_err()
            {
                server.fail();
            }
        })
}

async fn read_socket(
    server: Arc<ConnectionServer>,
    id: &str,
    mut stream: SplitStream<WebSocket>,
    pong: watch::Sender<Vec<u8>>,
    inbound_limit: Arc<AtomicUsize>,
) -> u16 {
    let mut order = RequestOrder::default();
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => {
                if text.len() > inbound_limit.load(Ordering::Acquire) {
                    return SIZE_CLOSE;
                }
                let Ok(request) = serde_json::from_str::<Request<Value>>(&text) else {
                    return POLICY_CLOSE;
                };
                if order.reserve(request.id).is_err() {
                    return POLICY_CLOSE;
                }
                let work = Work {
                    connection_id: id.to_owned(),
                    request,
                    bytes: text.len(),
                };
                let queue = if work.request.method.is_watch() {
                    &server.control_work
                } else {
                    &server.work
                };
                if queue.try_send(work).is_err() {
                    return POLICY_CLOSE;
                }
            }
            Ok(Message::Pong(value)) => {
                let _ = pong.send(value.to_vec());
            }
            Ok(Message::Ping(_)) => {}
            Ok(Message::Close(_)) => return NORMAL_CLOSE,
            Ok(Message::Binary(_)) => return POLICY_CLOSE,
            Err(error) => {
                return match error
                    .into_inner()
                    .downcast::<tokio_tungstenite::tungstenite::Error>()
                {
                    Ok(error)
                        if matches!(*error, tokio_tungstenite::tungstenite::Error::Capacity(_)) =>
                    {
                        SIZE_CLOSE
                    }
                    _ => POLICY_CLOSE,
                };
            }
        }
    }
    NORMAL_CLOSE
}

async fn write_socket(
    server: Arc<ConnectionServer>,
    mut sink: SplitSink<WebSocket, Message>,
    mut outgoing: mpsc::Receiver<Outbound>,
    mut closed: watch::Receiver<Option<u16>>,
    mut reader_closed: watch::Receiver<Option<u16>>,
    mut shutdown: watch::Receiver<bool>,
    mut pongs: watch::Receiver<Vec<u8>>,
    mut expires: watch::Receiver<Option<Instant>>,
) {
    let mut ping = tokio::time::interval_at(
        Instant::now() + policy::PING_INTERVAL,
        policy::PING_INTERVAL,
    );
    let mut expected_pong: Option<(Vec<u8>, Instant)> = None;
    let mut ping_number = 0u64;
    let code = loop {
        let pong_deadline = expected_pong.as_ref().map(|(_, deadline)| *deadline);
        let auth_deadline = *expires.borrow();
        tokio::select! {
            biased;
            code = async { closed.wait_for(|code| code.is_some()).await.map(|code| code.unwrap_or(POLICY_CLOSE)).unwrap_or(POLICY_CLOSE) } => break code,
            code = async { reader_closed.wait_for(|code| code.is_some()).await.map(|code| code.unwrap_or(NORMAL_CLOSE)).unwrap_or(NORMAL_CLOSE) } => break code,
            _ = async { let _ = shutdown.wait_for(|value| *value).await; } => break RESTART_CLOSE,
            result = expires.changed() => { if result.is_err() { break NORMAL_CLOSE; } }
            _ = async { match auth_deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => break POLICY_CLOSE,
            _ = async { match pong_deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => break POLICY_CLOSE,
            result = pongs.changed() => {
                if result.is_err() { break NORMAL_CLOSE; }
                if expected_pong.as_ref().is_some_and(|(expected, _)| *expected == *pongs.borrow()) { expected_pong = None; }
            }
            message = outgoing.recv() => {
                let Some(message) = message else { break NORMAL_CLOSE };
                if !matches!(tokio::time::timeout(policy::WRITE_TIMEOUT, server.authorize_outbound(message.authorization)).await, Ok(true)) {
                    break POLICY_CLOSE;
                }
                let result = tokio::time::timeout(policy::WRITE_TIMEOUT, sink.send(Message::Text(message.text.into()))).await;
                if !matches!(result, Ok(Ok(()))) { break POLICY_CLOSE; }
                if let Some(code) = message.close_after { break code; }
            }
            _ = ping.tick(), if expected_pong.is_none() => {
                ping_number += 1;
                let payload = ping_number.to_be_bytes().to_vec();
                let result = tokio::time::timeout(policy::WRITE_TIMEOUT, sink.send(Message::Ping(payload.clone().into()))).await;
                if !matches!(result, Ok(Ok(()))) { break POLICY_CLOSE; }
                expected_pong = Some((payload, Instant::now() + policy::PONG_TIMEOUT));
            }
        }
    };
    let _ = tokio::time::timeout(
        policy::CLOSE_GRACE,
        sink.send(Message::Close(Some(CloseFrame {
            code,
            reason: "".into(),
        }))),
    )
    .await;
}

fn clock() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn decode<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, ErrorCode> {
    serde_json::from_value(params).map_err(|_| ErrorCode::InvalidRequest)
}

fn encode(value: impl Serialize) -> Result<Value, ErrorCode> {
    serde_json::to_value(value).map_err(|_| ErrorCode::Internal)
}

fn authority_error(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<ErrorCode>()
        .copied()
        .unwrap_or(ErrorCode::Unavailable)
}

fn authentication_error(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<ErrorCode>()
        .copied()
        .unwrap_or(ErrorCode::Unauthenticated)
}

fn remote_error(code: ErrorCode) -> RemoteError {
    let message = match code {
        ErrorCode::ProtocolMismatch => "Remote protocol mismatch",
        ErrorCode::Unauthenticated => "Remote authentication rejected",
        ErrorCode::Revoked | ErrorCode::StaleCredential => "Device authorization invalidated",
        ErrorCode::Forbidden => "Remote capability is not permitted",
        ErrorCode::LimitExceeded => "Remote limit exceeded",
        ErrorCode::Unavailable => "Gateway unavailable",
        ErrorCode::PairingPending => "Device approval pending",
        ErrorCode::PairingRejected => "Device approval rejected",
        ErrorCode::PairingExpired => "Device approval expired",
        ErrorCode::NotFound => "Remote resource not found",
        ErrorCode::Busy => "Gateway busy",
        ErrorCode::InvalidRequest => "Invalid Remote request",
        ErrorCode::Internal => "Remote operation failed",
    };
    RemoteError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> (
        Connection,
        mpsc::Receiver<Outbound>,
        watch::Receiver<Option<u16>>,
    ) {
        let (outbound, outgoing) = mpsc::channel(policy::QUEUE_MESSAGES);
        let (close, closed) = watch::channel(None);
        let deadline = Instant::now() + policy::HANDSHAKE_TIMEOUT;
        let (expires, _) = watch::channel(None);
        (
            Connection {
                active_watch: None,
                latest_watch: None,
                source: "127.0.0.1".parse().unwrap(),
                audience: "http://127.0.0.1:3141/ws".into(),
                ingress: ClientIngress::Native,
                stage: Stage::Local,
                deadline,
                next_sequence: 1,
                outbound,
                byte_budget: Arc::new(Semaphore::new(policy::QUEUE_BYTES)),
                close,
                expires,
                inbound_limit: Arc::new(AtomicUsize::new(policy::REQUEST_BYTES)),
                preauth: None,
                controls: Arc::new(Semaphore::new(policy::CONTROL_PER_CONNECTION)),
                _total: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
            },
            outgoing,
            closed,
        )
    }

    struct StreamAuthority(Mutex<Option<DeviceAuthorization>>);

    impl PairingStore for StreamAuthority {
        fn gateway_id(&self) -> anyhow::Result<String> {
            Ok("gateway".into())
        }
        fn admit(&self, _: &ChallengeRequest, _: &str) -> anyhow::Result<Option<String>> {
            anyhow::bail!("not enrolled")
        }
        fn complete(
            &self,
            _: &crate::pairing::DeviceChallenge,
            _: &str,
            _: u64,
        ) -> anyhow::Result<PairingOutcome> {
            anyhow::bail!("not enrolled")
        }
        fn authenticate(&self, _: &str) -> anyhow::Result<DeviceAuthorization> {
            anyhow::bail!("not enrolled")
        }
        fn authorize(&self, authorization: &DeviceAuthorization) -> anyhow::Result<()> {
            let current = self.0.lock().unwrap();
            ensure!(
                current.as_ref().is_some_and(|current| {
                    current.device_id == authorization.device_id
                        && current.approval_generation == authorization.approval_generation
                        && current.token_digest == authorization.token_digest
                }),
                "authorization invalidated"
            );
            Ok(())
        }
        fn pairing_status(&self, _: &str, _: &str, _: u64) -> anyhow::Result<PairingStatus> {
            Ok(PairingStatus::Rejected)
        }
        fn devices(&self, _: u64) -> anyhow::Result<crate::pairing::DeviceList> {
            anyhow::bail!("not supported")
        }
        fn approve(&self, _: &str, _: u64) -> anyhow::Result<()> {
            anyhow::bail!("not supported")
        }
        fn reject(&self, _: &str, _: u64) -> anyhow::Result<()> {
            anyhow::bail!("not supported")
        }
        fn revoke(&self, _: &str) -> anyhow::Result<()> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    fn server(store: Arc<StreamAuthority>) -> Arc<ConnectionServer> {
        let (work, _) = mpsc::channel(DISPATCH_CAPACITY);
        let (control_work, _) = mpsc::channel(policy::CONTROL_REQUESTS);
        let (shutdown, _) = watch::channel(false);
        Arc::new(ConnectionServer {
            authenticator: Authenticator::new(store),
            gateway_id: "gateway".into(),
            boot_id: "boot".into(),
            local_credential: "local".into(),
            origins: Vec::new(),
            trusted_proxies: Vec::new(),
            connections: Mutex::new(HashMap::new()),
            total: Arc::new(Semaphore::new(policy::CONNECTIONS)),
            preauth: Arc::new(Semaphore::new(policy::PREAUTH_CONNECTIONS)),
            controls: Arc::new(Semaphore::new(policy::CONTROL_REQUESTS)),
            work,
            control_work,
            shutdown,
            tasks: TaskTracker::new(),
            failed: AtomicBool::new(false),
        })
    }

    type ClientSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn writer_socket(
        server: Arc<ConnectionServer>,
        outgoing: mpsc::Receiver<Outbound>,
        closed: watch::Receiver<Option<u16>>,
        receive_bytes: Option<u32>,
    ) -> (
        ClientSocket,
        watch::Sender<Vec<u8>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (reader_close, reader_closed) = watch::channel(None);
        let (pong, pongs) = watch::channel(Vec::new());
        let (expiry, expires) = watch::channel(None);
        let writer = Arc::new(Mutex::new(Some((
            outgoing,
            closed,
            reader_closed,
            pongs,
            expires,
            reader_close,
            expiry,
        ))));
        let router = Router::new().route(
            "/ws",
            get(move |upgrade: WebSocketUpgrade| {
                let owner = server.clone();
                let writer = writer.lock().unwrap().take().unwrap();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        owner
                            .tasks
                            .clone()
                            .spawn(async move {
                                let (sink, _stream) = socket.split();
                                let _signals = (writer.5, writer.6);
                                write_socket(
                                    owner.clone(),
                                    sink,
                                    writer.0,
                                    writer.1,
                                    writer.2,
                                    owner.shutdown.subscribe(),
                                    writer.3,
                                    writer.4,
                                )
                                .await;
                            })
                            .await
                            .unwrap();
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = tokio::net::TcpSocket::new_v4().unwrap();
        if let Some(receive_bytes) = receive_bytes {
            client.set_recv_buffer_size(receive_bytes).unwrap();
        }
        let stream = client.connect(address).await.unwrap();
        let (socket, _) = tokio_tungstenite::client_async(
            format!("ws://{address}/ws"),
            tokio_tungstenite::MaybeTlsStream::Plain(stream),
        )
        .await
        .unwrap();
        (socket, pong, serving)
    }

    fn session_work(id: u64, method: Method) -> SessionWork {
        SessionWork {
            connection_id: "client".into(),
            request: Request::new(RequestId::new(id).unwrap(), method, json!({})),
            _connection: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
            _process: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        }
    }

    #[tokio::test]
    async fn lagged_root_stream_retires_instead_of_skipping_events() {
        let server = server(Arc::new(StreamAuthority(Mutex::new(None))));
        let (mut connection, mut outgoing, mut closed) = connection();
        connection.latest_watch = RequestId::new(2);
        server
            .connections
            .lock()
            .unwrap()
            .insert("client".into(), connection);
        let (source, events) = broadcast::channel(1);
        for id in ["lost", "retained"] {
            source
                .send(AgentInstanceEvent::ViewBoundary { id: id.into() })
                .unwrap();
        }
        server.complete_session(
            session_work(2, Method::SessionWatch),
            Ok(SessionOutcome::Watch {
                session_id: "root".into(),
                events,
            }),
        );
        let ack: Value = serde_json::from_str(&outgoing.recv().await.unwrap().text).unwrap();
        assert_eq!(ack["payload"]["watchId"], "2");
        tokio::time::timeout(
            Duration::from_secs(1),
            closed.wait_for(|code| code.is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(*closed.borrow(), Some(POLICY_CLOSE));
        assert!(outgoing.try_recv().is_err());
        assert!(
            server.connections.lock().unwrap()["client"]
                .active_watch
                .is_none()
        );
        server.drain().await.unwrap();
        assert_eq!(source.receiver_count(), 0);
    }

    #[tokio::test]
    async fn obsolete_watch_completions_cannot_replace_or_clear_the_selection() {
        let server = server(Arc::new(StreamAuthority(Mutex::new(None))));
        let (mut connection, mut outgoing, closed) = connection();
        connection.latest_watch = RequestId::new(4);
        server
            .connections
            .lock()
            .unwrap()
            .insert("client".into(), connection);
        let (selected, events) = broadcast::channel(4);
        server.complete_session(
            session_work(4, Method::SessionWatch),
            Ok(SessionOutcome::Watch {
                session_id: "selected".into(),
                events,
            }),
        );
        let ack: Value = serde_json::from_str(&outgoing.recv().await.unwrap().text).unwrap();
        assert_eq!(ack["payload"]["watchId"], "4");
        let (obsolete, events) = broadcast::channel(1);
        for (id, method, outcome) in [
            (
                2,
                Method::SessionWatch,
                SessionOutcome::Watch {
                    session_id: "obsolete".into(),
                    events,
                },
            ),
            (3, Method::SessionUnwatch, SessionOutcome::Unwatch),
        ] {
            server.complete_session(session_work(id, method), Ok(outcome));
            let response: Value =
                serde_json::from_str(&outgoing.recv().await.unwrap().text).unwrap();
            assert_eq!(response["error"]["code"], "UNAVAILABLE");
        }
        assert_eq!(obsolete.receiver_count(), 0);
        assert!(!server.deliver_root(
            "client",
            RequestId::new(2).unwrap(),
            Err(broadcast::error::RecvError::Closed)
        ));
        selected
            .send(AgentInstanceEvent::ViewBoundary {
                id: "current".into(),
            })
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), outgoing.recv())
            .await
            .unwrap()
            .unwrap();
        let event: Value = serde_json::from_str(&event.text).unwrap();
        assert_eq!(event["payload"]["sessionId"], "selected");
        assert_eq!(event["payload"]["watchId"], "4");
        assert_eq!(event["payload"]["event"]["id"], "current");
        assert_eq!(*closed.borrow(), None);
        server
            .connections
            .lock()
            .unwrap()
            .get_mut("client")
            .unwrap()
            .latest_watch = RequestId::new(5);
        server.complete_session(
            session_work(5, Method::SessionUnwatch),
            Ok(SessionOutcome::Unwatch),
        );
        server.drain().await.unwrap();
    }

    #[tokio::test]
    async fn socket_writer_rejects_queued_frames_after_authority_changes() {
        use tokio_tungstenite::tungstenite::Message as ClientMessage;

        let authorization = DeviceAuthorization {
            device_id: "device".into(),
            approval_generation: "approved".into(),
            token_digest: "issued-token".into(),
        };
        for (replacement, expected_delivery) in [
            (Some(authorization.clone()), true),
            (None, false),
            (
                Some(DeviceAuthorization {
                    token_digest: "rotated-token".into(),
                    ..authorization.clone()
                }),
                false,
            ),
            (
                Some(DeviceAuthorization {
                    approval_generation: "replacement".into(),
                    ..authorization.clone()
                }),
                false,
            ),
        ] {
            let store = Arc::new(StreamAuthority(Mutex::new(Some(authorization.clone()))));
            let server = server(store.clone());
            let (mut connection, outgoing, closed) = connection();
            connection.stage = Stage::Device(authorization.clone());
            assert!(server.authorized(&connection).is_ok());
            connection.event(EventName::SessionEvent, json!({"private":"queued"}), None);
            connection.stage = replacement
                .clone()
                .map(Stage::Device)
                .unwrap_or(Stage::Closed);
            *store.0.lock().unwrap() = replacement;
            let (mut socket, pong, serving) =
                writer_socket(server.clone(), outgoing, closed, None).await;
            let message = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if expected_delivery {
                let ClientMessage::Text(text) = message else {
                    panic!("current authorization must deliver queued frames");
                };
                let event: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(event["payload"]["private"], "queued");
                server.shutdown.send(true).unwrap();
                let message = tokio::time::timeout(Duration::from_secs(1), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(message, ClientMessage::Close(Some(frame)) if u16::from(frame.code) == RESTART_CLOSE)
                );
            } else {
                assert!(
                    matches!(message, ClientMessage::Close(Some(frame)) if u16::from(frame.code) == POLICY_CLOSE)
                );
            }
            assert_eq!(
                connection.byte_budget.available_permits(),
                policy::QUEUE_BYTES
            );
            drop(pong);
            serving.abort();
            server.drain().await.unwrap();
        }
    }

    #[tokio::test]
    async fn socket_heartbeat_requires_the_matching_pong() {
        use tokio_tungstenite::tungstenite::Message as ClientMessage;

        enum PongReply {
            Matching,
            Mismatched,
            Missing,
        }
        for reply in [
            PongReply::Matching,
            PongReply::Mismatched,
            PongReply::Missing,
        ] {
            let server = server(Arc::new(StreamAuthority(Mutex::new(None))));
            let (_connection, outgoing, closed) = connection();
            let (mut socket, pong, serving) =
                writer_socket(server.clone(), outgoing, closed, None).await;
            tokio::task::yield_now().await;
            tokio::time::pause();
            tokio::time::advance(policy::PING_INTERVAL).await;
            tokio::time::resume();
            let message = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let ClientMessage::Ping(payload) = message else {
                panic!("writer must send a heartbeat before its Pong deadline");
            };
            match reply {
                PongReply::Matching => pong.send(payload.to_vec()).unwrap(),
                PongReply::Mismatched => pong.send(b"another-heartbeat".to_vec()).unwrap(),
                PongReply::Missing => {}
            }
            tokio::task::yield_now().await;
            tokio::time::pause();
            tokio::time::advance(policy::PONG_TIMEOUT).await;
            tokio::time::resume();
            let message = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            match reply {
                PongReply::Matching => {
                    assert!(matches!(message, ClientMessage::Ping(next) if next != payload));
                    server.shutdown.send(true).unwrap();
                }
                PongReply::Mismatched | PongReply::Missing => {
                    assert!(
                        matches!(message, ClientMessage::Close(Some(frame)) if u16::from(frame.code) == POLICY_CLOSE)
                    );
                }
            }
            drop(pong);
            serving.abort();
            server.drain().await.unwrap();
        }
    }

    #[tokio::test]
    async fn stalled_socket_writer_times_out_and_releases_queued_bytes() {
        const RECEIVE_BYTES: u32 = 1024;
        let server = server(Arc::new(StreamAuthority(Mutex::new(None))));
        let (mut connection, outgoing, closed) = connection();
        connection.enqueue(json!("x".repeat(policy::SERVER_BYTES / 2)), None);
        connection.enqueue(json!({"later":"must stay bounded"}), None);
        let (socket, pong, serving) =
            writer_socket(server.clone(), outgoing, closed, Some(RECEIVE_BYTES)).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while connection.outbound.capacity() != policy::QUEUE_MESSAGES - 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer must take the first frame but stall before taking the next");
        assert!(connection.byte_budget.available_permits() < policy::QUEUE_BYTES);
        server.tasks.close();
        tokio::time::pause();
        tokio::time::timeout(
            policy::WRITE_TIMEOUT + policy::CLOSE_GRACE + Duration::from_secs(1),
            server.tasks.wait(),
        )
        .await
        .expect("stalled writer must retire without waiting for the reader");
        tokio::time::resume();
        assert!(connection.outbound.is_closed());
        assert_eq!(
            connection.byte_budget.available_permits(),
            policy::QUEUE_BYTES
        );
        drop((socket, pong));
        serving.abort();
        server.drain().await.unwrap();
    }

    #[test]
    fn a_slow_connection_cannot_exceed_the_message_budget() {
        let (mut connection, outgoing, closed) = connection();
        for _ in 0..policy::QUEUE_MESSAGES {
            connection.enqueue(Value::Null, None);
        }
        assert_eq!(outgoing.len(), policy::QUEUE_MESSAGES);
        assert_eq!(*closed.borrow(), None);
        connection.enqueue(Value::Null, None);
        assert_eq!(*closed.borrow(), Some(POLICY_CLOSE));
        assert_eq!(outgoing.len(), policy::QUEUE_MESSAGES);
    }

    proptest::proptest! {
        #[test]
        fn queued_and_inflight_frames_share_a_byte_budget(payload in ".{0,128}") {
            let (mut connection, mut outgoing, closed) = connection();
            let frame_bytes = serde_json::to_vec(&payload).unwrap().len();
            let budget = frame_bytes * 2;
            connection.byte_budget = Arc::new(Semaphore::new(budget));
            connection.enqueue(&payload, None);
            let in_writer = outgoing.try_recv().unwrap();
            connection.enqueue(&payload, None);
            proptest::prop_assert_eq!(connection.byte_budget.available_permits(), 0);
            drop(in_writer);
            connection.enqueue(&payload, None);
            proptest::prop_assert_eq!(*closed.borrow(), None);
            connection.enqueue(&payload, None);
            proptest::prop_assert_eq!(*closed.borrow(), Some(POLICY_CLOSE));
            drop(outgoing);
            proptest::prop_assert_eq!(connection.byte_budget.available_permits(), budget);
        }
    }
}
