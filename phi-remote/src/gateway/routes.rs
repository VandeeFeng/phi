use std::net::SocketAddr;
use std::sync::{Arc, atomic::Ordering};

use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as RoutePath, Query, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::FutureExt;
use phi_sdk::agent_instance::AgentInstanceId;
use phi_sdk::extension::mcp::McpElicitationResponse;
use phi_sdk::interaction::{
    InteractionEnvelope, InteractionRequest, InteractionResponse, PermissionDecision,
};
use phi_sdk::turn::{SubmissionDispatch, SubmissionDisposition, TurnPrompt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use super::projection::{clipped, permission_fits_display, read_view};
use super::{
    GatewayState, HTTP_TIMEOUT, MAX_BODY_BYTES, RemoteHost, RemoteInteraction, RemoteRuntimeUpdate,
    RemoteSubmission, connection, session,
};
use crate::protocol::{EmptyParams, ErrorCode};
use crate::session_segment;

const MAX_MESSAGE_BYTES: usize = 12 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn router<H: RemoteHost>(
    state: Arc<GatewayState<H>>,
    connections: Option<&Arc<connection::ConnectionServer>>,
    plugin_routes: Router,
) -> Router {
    let router = Router::new()
        .route(
            "/api/sessions",
            get(list_sessions::<H>).post(create_session::<H>),
        )
        .route("/api/gateway/stop", post(stop_gateway::<H>))
        .route("/api/gateway/devices", get(list_devices::<H>))
        .route(
            "/api/gateway/devices/{identity}/{action}",
            post(device_action::<H>),
        )
        .route("/api/sessions/saved", get(saved_sessions::<H>))
        .route("/api/sessions/resume", post(resume_session::<H>))
        .route("/api/sessions/{session}/snapshot", get(snapshot::<H>))
        .route("/api/sessions/{session}/configure", post(configure::<H>))
        .route("/api/sessions/{session}/reload", post(reload::<H>))
        .route("/api/sessions/{session}/catalog", get(catalog::<H>))
        .route("/api/sessions/{session}/new", post(new_session::<H>))
        .route("/api/sessions/{session}/close", post(close_session::<H>))
        .route("/api/sessions/{session}/view", get(observe::<H>))
        .route("/api/sessions/{session}/message", post(send_message::<H>))
        .route("/api/sessions/{session}/submit", post(submit_prompt::<H>))
        .route(
            "/api/sessions/{session}/permission",
            post(answer_permission::<H>),
        )
        .route("/api/sessions/{session}/cancel", post(cancel::<H>))
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .with_state(state.clone())
        .merge(plugin_routes)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(state, protect::<H>));
    match connections {
        Some(connections) => router.merge(connections.router()),
        None => router,
    }
}

async fn protect<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let mut response = match protected_response(state, request, next).await {
        Ok(response) => response,
        Err(status) => (
            status,
            Json(json!({"error": match status {
                StatusCode::UNAUTHORIZED => "Gateway authentication required",
                StatusCode::FORBIDDEN => "Gateway request forbidden",
                StatusCode::GATEWAY_TIMEOUT => "request timed out; outcome may be uncertain; do not automatically resend",
                StatusCode::PAYLOAD_TOO_LARGE => "request body exceeds Gateway limit",
                _ => "Gateway request unavailable",
            }})),
        ).into_response(),
    };
    let headers = response.headers_mut();
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    response
}

#[derive(Clone)]
enum HttpAuthorization {
    Local,
    Device(crate::pairing::DeviceAuthorization),
}

async fn revalidate_http<H: RemoteHost>(
    state: Arc<GatewayState<H>>,
    authorization: HttpAuthorization,
) -> Result<(), StatusCode> {
    tokio::task::spawn_blocking(move || match authorization {
        HttpAuthorization::Local => Ok(()),
        HttpAuthorization::Device(authorization) => state
            .authentication
            .as_ref()
            .ok_or(StatusCode::UNAUTHORIZED)?
            .store
            .authorize(&authorization)
            .map_err(|_| StatusCode::UNAUTHORIZED),
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
}

async fn protected_response<H: RemoteHost>(
    state: Arc<GatewayState<H>>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    use crate::network::{IngressKind, admit_ingress};
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .ok_or(StatusCode::FORBIDDEN)?
        .0
        .ip();
    let proxies = state
        .authentication
        .as_ref()
        .map_or(&[][..], |access| access.trusted_proxies.as_slice());
    let ingress = admit_ingress(&state.origins, proxies, peer, request.headers())
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let headers = request.headers();
    if headers
        .get_all("sec-fetch-site")
        .iter()
        .any(|value| value == "cross-site")
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let path = request.uri().path();
    if !path.starts_with("/api/") {
        return Ok(next.run(request).await);
    }
    if *state.shutdown.borrow() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let mutation = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    if mutation
        && (headers.get_all(header::CONTENT_TYPE).iter().count() != 1
            || headers
                .get(header::CONTENT_TYPE)
                .is_none_or(|value| value != "application/json")
            || !headers.contains_key(header::ORIGIN))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let control = path == "/api/gateway/stop"
        || (path.starts_with("/api/sessions/")
            && (path.ends_with("/cancel") || path.ends_with("/permission")));
    let permit = if control {
        &state.controls
    } else {
        &state.requests
    }
    .clone()
    .try_acquire_owned()
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut credentials = headers.get_all(header::AUTHORIZATION).iter();
    let credential = credentials
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|_| credentials.next().is_none())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_owned();
    let local = ingress.kind == IngressKind::Local
        && bool::from(credential.as_bytes().ct_eq(state.token.as_bytes()));
    let (authorization, permit) = if local {
        (Ok(HttpAuthorization::Local), permit)
    } else {
        let access = state
            .authentication
            .clone()
            .ok_or(StatusCode::UNAUTHORIZED)?;
        state
            .tasks
            .spawn_blocking(move || {
                let authorization = access
                    .store
                    .authenticate(&credential)
                    .map(HttpAuthorization::Device)
                    .map_err(|_| StatusCode::UNAUTHORIZED);
                (authorization, permit)
            })
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    };
    let authorization = authorization?;
    if path.starts_with("/api/gateway/") && !matches!(authorization, HttpAuthorization::Local) {
        return Err(StatusCode::FORBIDDEN);
    }
    let (parts, body) = request.into_parts();
    let body = tokio::time::timeout(HTTP_TIMEOUT, axum::body::to_bytes(body, MAX_BODY_BYTES))
        .await
        .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let mut request = axum::extract::Request::from_parts(parts, Body::from(body));
    request.extensions_mut().insert(authorization.clone());
    let owner = state.clone();
    // HTTP waiter loss must not drop an application operation after invocation.
    let task = state.tasks.spawn(async move {
        let _permit = permit;
        revalidate_http(owner.clone(), authorization.clone()).await?;
        if *owner.shutdown.borrow() {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        match std::panic::AssertUnwindSafe(next.run(request))
            .catch_unwind()
            .await
        {
            Ok(response) => {
                revalidate_http(owner, authorization).await?;
                Ok(response)
            }
            Err(_) => {
                owner.failed.store(true, Ordering::Release);
                let _ = owner.shutdown.send(true);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    });
    tokio::time::timeout(HTTP_TIMEOUT, task)
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum DeviceAction {
    Approve,
    Reject,
    Revoke,
}

fn pairing_clock() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

async fn list_devices<H: RemoteHost>(State(state): State<Arc<GatewayState<H>>>) -> ApiResult {
    let store = state
        .authentication
        .as_ref()
        .context(ErrorCode::Unavailable)?
        .store
        .clone();
    let devices = tokio::task::spawn_blocking(move || store.devices(pairing_clock()?)).await??;
    Ok(Json(serde_json::to_value(devices)?))
}

async fn device_action<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath((identity, action)): RoutePath<(String, DeviceAction)>,
    Json(_): Json<EmptyParams>,
) -> ApiResult {
    if !crate::pairing::is_identity(&identity) {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let store = state
        .authentication
        .as_ref()
        .context(ErrorCode::Unavailable)?
        .store
        .clone();
    tokio::task::spawn_blocking(move || match action {
        DeviceAction::Approve => store.approve(&identity, pairing_clock()?),
        DeviceAction::Reject => store.reject(&identity, pairing_clock()?),
        DeviceAction::Revoke => store.revoke(&identity),
    })
    .await??;
    Ok(Json(json!({})))
}

type ApiResult = Result<Json<Value>, ApiError>;
struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.downcast_ref::<ErrorCode>() {
            Some(ErrorCode::Unauthenticated) => StatusCode::UNAUTHORIZED,
            Some(ErrorCode::InvalidRequest) => StatusCode::BAD_REQUEST,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(json!({"error":self.0.to_string()}))).into_response()
    }
}

async fn list_sessions<H: RemoteHost>(State(state): State<Arc<GatewayState<H>>>) -> ApiResult {
    Ok(Json(json!(state.host.sessions().await?)))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateSessionRequest {
    working_dir: std::path::PathBuf,
}

async fn create_session<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult {
    Ok(Json(json!(state.host.create(request.working_dir).await?)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSessionsQuery {
    #[serde(default)]
    query: String,
}

async fn saved_sessions<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    Query(query): Query<SavedSessionsQuery>,
) -> ApiResult {
    Ok(Json(json!(state.host.saved(query.query).await?)))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotQuery {
    event_boundary: Option<String>,
}

async fn snapshot<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<SnapshotQuery>,
) -> ApiResult {
    Ok(Json(
        read_snapshot_at_boundary(&state, &id, query.event_boundary).await?,
    ))
}

async fn read_snapshot_at_boundary<H: RemoteHost>(
    state: &GatewayState<H>,
    id: &str,
    boundary: Option<String>,
) -> anyhow::Result<Value> {
    ensure!(
        boundary.as_deref().is_none_or(crate::pairing::is_identity),
        ErrorCode::InvalidRequest
    );
    session(state, id).await?;
    let mut recovered = match boundary.as_ref() {
        Some(boundary) => {
            state
                .host
                .recovery_at_boundary(AgentInstanceId::new(id), boundary.clone())
                .await?
        }
        None => state.host.recovery(AgentInstanceId::new(id)).await?,
    };
    if recovered.session.id != id
        || recovered.view.snapshot.id.as_str() != id
        || recovered.view.transcript.id != id
    {
        anyhow::bail!("recovery returned another Session");
    }
    // Root questions must share the root execution/history boundary.
    recovered
        .interactions
        .retain(|interaction| interaction.agent_id == id);
    recovered.interactions.extend(
        state
            .host
            .interactions(AgentInstanceId::new(id))
            .await?
            .into_iter()
            .filter(|(agent, _)| agent.as_str() != id)
            .flat_map(|(agent, pending)| {
                pending.into_iter().map(move |request| RemoteInteraction {
                    agent_id: agent.to_string(),
                    request,
                })
            }),
    );
    let mut value = serde_json::to_value(crate::snapshot::Snapshot::from(recovered))?;
    if let Some(boundary) = boundary {
        value["eventBoundary"] = json!(boundary);
    }
    ensure_snapshot_size(&value)?;
    Ok(value)
}

fn runtime_response<Application: Serialize>(
    id: &str,
    recovered: RemoteRuntimeUpdate<Application>,
) -> ApiResult {
    if recovered.session.id != id
        || recovered.snapshot.id.as_str() != id
        || recovered.transcript.id != id
    {
        return Err(anyhow::anyhow!("runtime operation returned another Session").into());
    }
    let value = serde_json::to_value(crate::snapshot::RuntimeUpdate::from(recovered))?;
    ensure_snapshot_size(&value)?;
    Ok(Json(value))
}

async fn configure<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(selection): Json<crate::RemoteSelection>,
) -> ApiResult {
    session(&state, &id).await?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    let accepted = state
        .host
        .configure(AgentInstanceId::new(&id), selection)
        .await?;
    runtime_response(&id, accepted)
}

async fn reload<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(_): Json<EmptyParams>,
) -> ApiResult {
    session(&state, &id).await?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    let accepted = state.host.reload(AgentInstanceId::new(&id)).await?;
    runtime_response(&id, accepted)
}

async fn catalog<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
) -> ApiResult {
    session(&state, &id).await?;
    Ok(Json(serde_json::to_value(
        state.host.catalog(AgentInstanceId::new(id)).await?,
    )?))
}

async fn new_session<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(_): Json<EmptyParams>,
) -> ApiResult {
    session(&state, &id).await?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    Ok(Json(serde_json::to_value(
        state.host.new_session(AgentInstanceId::new(id)).await?,
    )?))
}

async fn submit_prompt<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(request): Json<crate::wire::SubmitInput>,
) -> ApiResult {
    session(&state, &id).await?;
    let (prompt, dispatch) = request.into_sdk();
    validate_message(&prompt.prompt)?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    let submission = state
        .host
        .submit(AgentInstanceId::new(id), prompt, dispatch)
        .await?;
    Ok(Json(serde_json::to_value(submission)?))
}

fn ensure_snapshot_size(value: &Value) -> anyhow::Result<()> {
    if serde_json::to_vec(value)?.len() > MAX_SNAPSHOT_BYTES {
        return Err(anyhow::Error::new(ErrorCode::LimitExceeded)
            .context("Session exceeds the terminal snapshot limit"));
    }
    Ok(())
}

async fn stop_gateway<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    Json(_): Json<EmptyParams>,
) -> ApiResult {
    let _ = state.shutdown.send(true);
    Ok(Json(json!({"status":"stopping"})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeRequest {
    id: String,
}

async fn resume_session<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    Json(request): Json<ResumeRequest>,
) -> ApiResult {
    session_segment(&request.id)?;
    Ok(Json(json!(state.host.resume(request.id).await?)))
}

async fn close_session<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(_): Json<EmptyParams>,
) -> ApiResult {
    session(&state, &id).await?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    state.host.close(AgentInstanceId::new(id)).await?;
    Ok(Json(json!({"status":"closed"})))
}

async fn observe<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
) -> ApiResult {
    Ok(Json(read_view(&state, &id).await?))
}

async fn send_message<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(request): Json<MessageRequest>,
) -> ApiResult {
    session(&state, &id).await?;
    validate_message(&request.message)?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    if let Some(target) = request.agent_id.as_deref()
        && target != id
    {
        let receipt = state
            .host
            .message(
                AgentInstanceId::new(&id),
                AgentInstanceId::new(target),
                request.message,
            )
            .await?;
        return Ok(Json(message_receipt(receipt)));
    }
    let submission = state
        .host
        .submit(
            AgentInstanceId::new(id),
            message_prompt(request.message),
            SubmissionDispatch::FollowUp,
        )
        .await?;
    Ok(Json(session_receipt(submission)))
}

async fn answer_permission<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(request): Json<PermissionRequest>,
) -> ApiResult {
    session(&state, &id).await?;
    let session = AgentInstanceId::new(id);
    let target = AgentInstanceId::new(&request.agent_id);
    let pending = state
        .host
        .interactions(session.clone())
        .await?
        .into_iter()
        .find(|(agent, _)| agent == &target)
        .ok_or(ErrorCode::Forbidden)?
        .1;
    let (request, response) = permission_response(request, &pending)?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    let warning = state
        .host
        .respond(session, target, request, response)
        .await?;
    Ok(Json(
        json!({"status":"answered", "persistenceWarning":warning}),
    ))
}

async fn cancel<H: RemoteHost>(
    State(state): State<Arc<GatewayState<H>>>,
    RoutePath(id): RoutePath<String>,
    axum::Extension(authorization): axum::Extension<HttpAuthorization>,
    Json(_): Json<EmptyParams>,
) -> ApiResult {
    session(&state, &id).await?;
    revalidate_http(state.clone(), authorization)
        .await
        .map_err(|_| ErrorCode::Unauthenticated)?;
    state.manager.interrupt(&AgentInstanceId::new(id)).await?;
    Ok(Json(json!({"status":"interrupted"})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MessageRequest {
    agent_id: Option<String>,
    message: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PermissionRequest {
    agent_id: String,
    request_id: String,
    decision: PermissionAnswer,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PermissionAnswer {
    AllowOnce,
    AllowAlways,
    Deny,
}

fn validate_message(message: &str) -> anyhow::Result<()> {
    ensure!(
        !message.trim().is_empty() && message.len() <= MAX_MESSAGE_BYTES,
        "message must contain 1 to {MAX_MESSAGE_BYTES} bytes"
    );
    Ok(())
}

fn message_prompt(message: String) -> TurnPrompt {
    TurnPrompt {
        prompt: message,
        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
        injected_context: Vec::new(),
        images: Vec::new(),
        tool_policy: Default::default(),
    }
}

fn session_receipt(submission: RemoteSubmission) -> Value {
    let mut receipt = match submission.disposition {
        SubmissionDisposition::Started { turn_id, .. } => {
            json!({"status":"started", "turnId":turn_id})
        }
        SubmissionDisposition::Queued { submission_id, .. } => {
            json!({"status":"queued", "submissionId":submission_id})
        }
        SubmissionDisposition::Handled { reason } => json!({"status":"handled", "reason":reason}),
    };
    receipt["kind"] = json!("submission");
    let notices: Vec<_> = submission
        .notices
        .into_iter()
        .filter_map(|notice| match notice {
            phi_sdk::hooks::HookNotice::Info { message }
            | phi_sdk::hooks::HookNotice::Warning { message } => Some(clipped(&message)),
            phi_sdk::hooks::HookNotice::Cancelled { event } => {
                Some(clipped(&format!("{event} hook cancelled")))
            }
            phi_sdk::hooks::HookNotice::ContextInjected { .. } => None,
        })
        .collect();
    receipt["notices"] = json!(notices);
    receipt
}

fn permission_response(
    request: PermissionRequest,
    pending: &[InteractionEnvelope],
) -> anyhow::Result<(String, InteractionResponse)> {
    let id = request.request_id;
    ensure!(id.len() <= 256, "invalid requestId");
    let envelope = pending
        .iter()
        .find(|envelope| envelope.request_id == id)
        .context("permission is no longer pending")?;
    let decision = request.decision;
    let response = match (&envelope.request, &decision) {
        (
            InteractionRequest::Permission(permission),
            PermissionAnswer::AllowOnce | PermissionAnswer::AllowAlways,
        ) => {
            ensure!(
                permission_fits_display(permission),
                "permission details exceed the display limit; approval is unavailable"
            );
            InteractionResponse::Permission(match decision {
                PermissionAnswer::AllowAlways => {
                    PermissionDecision::AllowAlways(permission.input.clone())
                }
                _ => PermissionDecision::AllowOnce,
            })
        }
        (InteractionRequest::Permission(_), PermissionAnswer::Deny) => {
            InteractionResponse::Permission(PermissionDecision::Deny)
        }
        (InteractionRequest::McpElicitation(_), PermissionAnswer::Deny) => {
            InteractionResponse::McpElicitation(McpElicitationResponse::Cancel)
        }
        _ => anyhow::bail!("this interaction cannot be approved in the browser"),
    };
    Ok((id, response))
}

fn message_receipt(receipt: phi_sdk::agent_manager::AgentMessageReceipt) -> Value {
    json!({
        "kind": "agent_message", "messageId": receipt.message().id.as_str(),
        "sender": receipt.message().sender,
        "delivery": receipt.delivery(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::operation_error;
    use super::*;
    const APPROVAL_DISPLAY_BYTES: usize = 8 * 1024;

    #[test]
    fn snapshot_limit_preserves_a_typed_wire_error() {
        let at_limit = Value::String("x".repeat(MAX_SNAPSHOT_BYTES - 2));
        assert!(ensure_snapshot_size(&at_limit).is_ok());
        let oversized = Value::String("x".repeat(MAX_SNAPSHOT_BYTES - 1));
        assert_eq!(
            operation_error(ensure_snapshot_size(&oversized).unwrap_err()),
            ErrorCode::LimitExceeded
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]

        #[test]
        fn session_inputs_accept_only_declared_fields(
            extra in proptest::option::of((
                "unexpected_[a-z]{1,24}|provider|source|tool_policy|injected_context",
                proptest::prelude::any::<u64>(),
            )),
        ) {
            let mut fixtures: Value =
                serde_json::from_str(include_str!("../../fixtures/session-v1.json")).unwrap();
            if let Some((field, value)) = &extra {
                for params in fixtures.as_object_mut().unwrap().values_mut() {
                    params[field] = json!(value);
                }
            }
            let accepted = [
                serde_json::from_value::<CreateSessionRequest>(fixtures["create"].clone()).is_ok(),
                serde_json::from_value::<crate::protocol::SessionParams>(fixtures["session"].clone()).is_ok(),
                serde_json::from_value::<SavedSessionsQuery>(fixtures["saved"].clone()).is_ok(),
                serde_json::from_value::<ResumeRequest>(fixtures["resume"].clone()).is_ok(),
                serde_json::from_value::<crate::RemoteSelection>(fixtures["configure"].clone()).is_ok(),
                serde_json::from_value::<MessageRequest>(fixtures["message"].clone()).is_ok(),
                serde_json::from_value::<PermissionRequest>(fixtures["permission"].clone()).is_ok(),
                serde_json::from_value::<crate::wire::SubmitInput>(fixtures["submit"].clone()).is_ok(),
            ];
            proptest::prop_assert!(accepted.iter().all(|accepted| *accepted == extra.is_none()), "{accepted:?}");
        }
    }

    #[test]
    fn permission_answers_require_a_current_fully_visible_request() {
        let request = || {
            serde_json::from_value::<PermissionRequest>(json!({
                "agentId":"root", "requestId":"question", "decision":"allow_once"
            }))
            .unwrap()
        };
        assert!(permission_response(request(), &[]).is_err());
        let mut pending = vec![InteractionEnvelope {
            request_id: "question".into(),
            emitted_at: chrono::Utc::now(),
            scope: Default::default(),
            request: InteractionRequest::Permission(phi_sdk::interaction::PermissionInteraction {
                tool: "bash".into(),
                input: "touch /tmp/permission-test".into(),
                preview: None,
                origin: None,
            }),
        }];
        assert!(permission_response(request(), &pending).is_ok());
        let always = || {
            serde_json::from_value::<PermissionRequest>(
                json!({"agentId":"root","requestId":"question","decision":"allow_always"}),
            )
            .unwrap()
        };
        let (_, response) = permission_response(always(), &pending).unwrap();
        assert!(matches!(
            response,
            InteractionResponse::Permission(PermissionDecision::AllowAlways(input))
                if input == "touch /tmp/permission-test"
        ));
        assert!(permission_response(always(), &[]).is_err());
        for length in [
            APPROVAL_DISPLAY_BYTES - 1,
            APPROVAL_DISPLAY_BYTES,
            APPROVAL_DISPLAY_BYTES + 1,
        ] {
            for field in ["input", "preview"] {
                if let InteractionRequest::Permission(permission) = &mut pending[0].request {
                    permission.input = "safe".into();
                    permission.preview = None;
                    match field {
                        "input" => permission.input = "x".repeat(length),
                        _ => permission.preview = Some("x".repeat(length)),
                    }
                    assert_eq!(
                        permission_fits_display(permission),
                        length <= APPROVAL_DISPLAY_BYTES
                    );
                }
                assert_eq!(
                    permission_response(request(), &pending).is_ok(),
                    length <= APPROVAL_DISPLAY_BYTES
                );
                assert_eq!(
                    permission_response(always(), &pending).is_ok(),
                    length <= APPROVAL_DISPLAY_BYTES
                );
                let deny = serde_json::from_value(json!({
                    "agentId":"root", "requestId":"question", "decision":"deny"
                }))
                .unwrap();
                assert!(permission_response(deny, &pending).is_ok());
            }
        }
        if let InteractionRequest::Permission(permission) = &mut pending[0].request {
            permission.input = "界".repeat(APPROVAL_DISPLAY_BYTES);
        }
        assert!(permission_response(request(), &pending).is_err());
    }
}
