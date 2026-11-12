use std::collections::HashMap;
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use rmcp::ClientHandler;
use rmcp::model::{
    CacheScope, ClientCapabilities, ClientInfo, ElicitRequestParams, ElicitResult,
    ElicitationCapability, FormElicitationCapability, Implementation, PaginatedRequestParams,
    ProtocolVersion, UrlElicitationCapability,
};
use rmcp::service::{
    ClientLifecycleMode, NotificationContext, RequestContext, RoleClient, RunningService,
    serve_client_with_lifecycle,
};
use rmcp::transport::child_process::TokioChildProcess;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio_util::task::AbortOnDropHandle;
use tracing::instrument::WithSubscriber;

use super::cache::{McpCachePolicy, McpProtocolHint};
use super::config::{McpAuth, McpServerConfig};
use super::elicitation::McpElicitationRouter;

const TOOLS_CHANGED: u8 = 1;
const RESOURCES_CHANGED: u8 = 1 << 1;
const PROMPTS_CHANGED: u8 = 1 << 2;
const LEGACY_CACHE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Clone)]
pub(super) struct McpClientHandler {
    info: ClientInfo,
    server: String,
    elicitation: McpElicitationRouter,
    metadata_changes: MetadataChangeSender,
}

#[derive(Clone)]
pub(super) struct MetadataChangeSender {
    changes: Arc<MetadataChanges>,
}

pub(super) struct MetadataChangeListener {
    changes: Arc<MetadataChanges>,
}

pub(super) struct MetadataChangeSet {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
}

struct MetadataChanges {
    pending: AtomicU8,
    notify: Notify,
}

impl McpClientHandler {
    pub(super) fn new(
        server: String,
        elicitation: McpElicitationRouter,
        metadata_changes: MetadataChangeSender,
        protocol_hint: Option<McpProtocolHint>,
    ) -> Self {
        let mut info = client_info();
        if protocol_hint == Some(McpProtocolHint::Legacy) {
            info.protocol_version = ProtocolVersion::V_2025_11_25;
        }
        Self {
            info,
            server,
            elicitation,
            metadata_changes,
        }
    }
}

pub(super) fn metadata_change_channel() -> (MetadataChangeSender, MetadataChangeListener) {
    let changes = Arc::new(MetadataChanges {
        pending: AtomicU8::new(0),
        notify: Notify::new(),
    });
    (
        MetadataChangeSender {
            changes: changes.clone(),
        },
        MetadataChangeListener { changes },
    )
}

impl MetadataChangeSender {
    fn signal(&self, change: u8) {
        if self.changes.pending.fetch_or(change, Ordering::AcqRel) & change == 0 {
            self.changes.notify.notify_one();
        }
    }

    pub(super) fn signal_notification(&self, notification: &rmcp::model::ServerNotification) {
        match notification {
            rmcp::model::ServerNotification::ToolListChangedNotification(_) => {
                self.signal(TOOLS_CHANGED)
            }
            rmcp::model::ServerNotification::ResourceListChangedNotification(_) => {
                self.signal(RESOURCES_CHANGED)
            }
            rmcp::model::ServerNotification::PromptListChangedNotification(_) => {
                self.signal(PROMPTS_CHANGED)
            }
            _ => {}
        }
    }
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::model::ErrorData> {
        tokio::select! {
            result = self.elicitation.elicit(&self.server, context.id, request) => result,
            _ = context.ct.cancelled() => Ok(ElicitResult::new(rmcp::model::ElicitationAction::Cancel)),
        }
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.signal_legacy_change(&context, TOOLS_CHANGED);
    }

    async fn on_resource_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.signal_legacy_change(&context, RESOURCES_CHANGED);
    }

    async fn on_prompt_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.signal_legacy_change(&context, PROMPTS_CHANGED);
    }
}

impl McpClientHandler {
    fn signal_legacy_change(&self, context: &NotificationContext<RoleClient>, change: u8) {
        if !is_modern_peer(&context.peer) {
            self.metadata_changes.signal(change);
        }
    }
}

impl MetadataChangeListener {
    pub(super) async fn changed(&self) -> MetadataChangeSet {
        self.changes.notify.notified().await;
        let pending = self.changes.pending.swap(0, Ordering::AcqRel);
        MetadataChangeSet {
            tools: pending & TOOLS_CHANGED != 0,
            resources: pending & RESOURCES_CHANGED != 0,
            prompts: pending & PROMPTS_CHANGED != 0,
        }
    }
}

pub(super) struct McpClientHandle {
    running_service: RunningService<RoleClient, McpClientHandler>,
    _stderr_task: Option<AbortOnDropHandle<()>>,
}

pub(super) struct McpListed<T> {
    pub items: Vec<T>,
    pub policy: McpCachePolicy,
}

pub(super) struct McpListedMetadata {
    pub tools: McpListed<rmcp::model::Tool>,
    pub resources: McpListed<rmcp::model::Resource>,
    pub prompts: McpListed<rmcp::model::Prompt>,
}

impl McpClientHandle {
    pub(super) async fn connect(
        server_name: String,
        config: &McpServerConfig,
        handler: McpClientHandler,
        protocol_hint: Option<McpProtocolHint>,
        sandbox: Option<&crate::sandbox::Sandbox>,
    ) -> anyhow::Result<Self> {
        match config {
            McpServerConfig::Command {
                command,
                args,
                env,
                cwd,
                ..
            } => {
                let mut cmd = match sandbox {
                    Some(sandbox) => match cwd {
                        Some(cwd) => {
                            sandbox.wrap_program_in(command, args, std::path::Path::new(cwd))?
                        }
                        None => sandbox.wrap_program(command, args),
                    },
                    None => {
                        let mut cmd = Command::new(command);
                        cmd.args(args);
                        if let Some(cwd) = cwd {
                            cmd.current_dir(cwd);
                        }
                        cmd
                    }
                };
                for (k, v) in env {
                    cmd.env(k, interpolate_env(v));
                }
                let (transport, stderr_task) = stdio_transport(cmd, server_name.clone())?;
                let running_service = connect_transport(handler, transport, protocol_hint)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("MCP connection failed for '{server_name}': {error}")
                    })?;
                Ok(Self {
                    running_service,
                    _stderr_task: Some(stderr_task),
                })
            }
            McpServerConfig::Url {
                url,
                headers,
                auth,
                bearer_token,
                bearer_token_env,
                ..
            } => {
                let custom_headers =
                    resolve_headers(&server_name, headers, *auth, bearer_token, bearer_token_env)?;
                let cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom_headers);
                let running_service = Box::pin(connect_http(handler, cfg, protocol_hint))
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {error}")
                    })?;
                Ok(Self {
                    running_service,
                    _stderr_task: None,
                })
            }
        }
    }

    pub(super) fn peer(&self) -> rmcp::service::Peer<RoleClient> {
        self.running_service.peer().clone()
    }
}

async fn connect_http(
    handler: McpClientHandler,
    config: rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig,
    protocol_hint: Option<McpProtocolHint>,
) -> Result<RunningService<RoleClient, McpClientHandler>, rmcp::service::ClientInitializeError> {
    const RETRY_DELAYS: [u64; 2] = [500, 1_000];
    type HttpClient = rmcp::transport::StreamableHttpClientTransport<reqwest::Client>;
    for delay in RETRY_DELAYS {
        let transport = HttpClient::from_config(config.clone());
        match connect_transport(handler.clone(), transport, protocol_hint).await {
            Ok(service) => return Ok(service),
            Err(error) if retryable_http_initialization(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            Err(error) => return Err(error),
        }
    }
    connect_transport(handler, HttpClient::from_config(config), protocol_hint).await
}

fn retryable_http_initialization(error: &rmcp::service::ClientInitializeError) -> bool {
    use rmcp::service::ClientInitializeError;
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    match error {
        ClientInitializeError::ConnectionClosed(_) => true,
        ClientInitializeError::TransportError { error, .. } => {
            match error
                .error
                .downcast_ref::<StreamableHttpError<reqwest::Error>>()
            {
                Some(StreamableHttpError::Client(error)) => {
                    error.is_connect() || error.is_timeout() || error.is_body()
                }
                Some(
                    StreamableHttpError::UnexpectedEndOfStream
                    | StreamableHttpError::TransportChannelClosed,
                ) => true,
                _ => false,
            }
        }
        _ => false,
    }
}

fn empty_list<T>(peer: &rmcp::service::Peer<RoleClient>) -> McpListed<T> {
    McpListed {
        items: Vec::new(),
        policy: metadata_fallback_policy(peer),
    }
}

pub(super) async fn list_metadata(
    peer: &rmcp::service::Peer<RoleClient>,
    server_name: &str,
    expose_resources: bool,
) -> anyhow::Result<McpListedMetadata> {
    let capabilities = peer
        .peer_info()
        .map(|info| info.capabilities.clone())
        .ok_or_else(|| anyhow::anyhow!("MCP handshake returned no server capabilities"))?;
    let tools = if capabilities.tools.is_some() {
        list_tools(peer).await?
    } else {
        empty_list(peer)
    };
    let resources = if expose_resources && capabilities.resources.is_some() {
        optional_list_or_empty(list_resources(peer).await, peer, server_name, "resource")
    } else {
        empty_list(peer)
    };
    let prompts = if capabilities.prompts.is_some() {
        optional_list_or_empty(list_prompts(peer).await, peer, server_name, "prompt")
    } else {
        empty_list(peer)
    };
    Ok(McpListedMetadata {
        tools,
        resources,
        prompts,
    })
}

pub(super) async fn list_tools(
    peer: &rmcp::service::Peer<RoleClient>,
) -> Result<McpListed<rmcp::model::Tool>, rmcp::ServiceError> {
    list_pages(peer, |params| async move {
        let page = peer.list_tools(params).await?;
        Ok(ListedPage {
            items: page.tools,
            next_cursor: page.next_cursor,
            ttl_ms: page.ttl_ms,
            cache_scope: page.cache_scope,
        })
    })
    .await
}

pub(super) async fn list_resources(
    peer: &rmcp::service::Peer<RoleClient>,
) -> Result<McpListed<rmcp::model::Resource>, rmcp::ServiceError> {
    list_pages(peer, |params| async move {
        let page = peer.list_resources(params).await?;
        Ok(ListedPage {
            items: page.resources,
            next_cursor: page.next_cursor,
            ttl_ms: page.ttl_ms,
            cache_scope: page.cache_scope,
        })
    })
    .await
}

pub(super) async fn list_prompts(
    peer: &rmcp::service::Peer<RoleClient>,
) -> Result<McpListed<rmcp::model::Prompt>, rmcp::ServiceError> {
    list_pages(peer, |params| async move {
        let page = peer.list_prompts(params).await?;
        Ok(ListedPage {
            items: page.prompts,
            next_cursor: page.next_cursor,
            ttl_ms: page.ttl_ms,
            cache_scope: page.cache_scope,
        })
    })
    .await
}

fn optional_list_or_empty<T>(
    result: Result<McpListed<T>, rmcp::ServiceError>,
    peer: &rmcp::service::Peer<RoleClient>,
    server_name: &str,
    kind: &str,
) -> McpListed<T> {
    normalize_optional_list(result, peer, server_name, kind).unwrap_or_else(|error| {
        tracing::warn!(target: "mcp", "Failed to read optional {kind} metadata from '{server_name}': {error}");
        empty_list(peer)
    })
}

pub(super) fn normalize_optional_list<T>(
    result: Result<McpListed<T>, rmcp::ServiceError>,
    peer: &rmcp::service::Peer<RoleClient>,
    server_name: &str,
    kind: &str,
) -> Result<McpListed<T>, rmcp::ServiceError> {
    match result {
        Err(rmcp::ServiceError::UnexpectedResponse) => {
            tracing::debug!(target: "mcp", "Ignoring invalid optional {kind} metadata from '{server_name}'");
            Ok(empty_list(peer))
        }
        result => result,
    }
}

struct ListedPage<T> {
    items: Vec<T>,
    next_cursor: Option<String>,
    ttl_ms: Option<u64>,
    cache_scope: Option<CacheScope>,
}

async fn list_pages<T, F, Fut>(
    peer: &rmcp::service::Peer<RoleClient>,
    mut fetch: F,
) -> Result<McpListed<T>, rmcp::ServiceError>
where
    F: FnMut(Option<PaginatedRequestParams>) -> Fut,
    Fut: Future<Output = Result<ListedPage<T>, rmcp::ServiceError>>,
{
    let mut listed = ListedPages::new(metadata_fallback_policy(peer));
    loop {
        let page = fetch(listed.params()).await?;
        listed.policy.observe(page.ttl_ms, page.cache_scope);
        listed.items.extend(page.items);
        if !listed.advance(page.next_cursor) {
            return Ok(listed.finish());
        }
    }
}

struct ListedPages<T> {
    items: Vec<T>,
    cursor: Option<String>,
    policy: CachePolicyAccumulator,
}

impl<T> ListedPages<T> {
    fn new(fallback: McpCachePolicy) -> Self {
        Self {
            items: Vec::new(),
            cursor: None,
            policy: CachePolicyAccumulator::new(fallback),
        }
    }

    fn params(&self) -> Option<PaginatedRequestParams> {
        Some(PaginatedRequestParams::default().with_cursor(self.cursor.clone()))
    }

    fn advance(&mut self, cursor: Option<String>) -> bool {
        self.cursor = cursor;
        self.cursor.is_some()
    }

    fn finish(self) -> McpListed<T> {
        McpListed {
            items: self.items,
            policy: self.policy.finish(),
        }
    }
}

struct CachePolicyAccumulator {
    fallback: McpCachePolicy,
    ttl_ms: Option<u64>,
    public: bool,
    saw_page: bool,
}

impl CachePolicyAccumulator {
    fn new(fallback: McpCachePolicy) -> Self {
        Self {
            fallback,
            ttl_ms: None,
            public: false,
            saw_page: false,
        }
    }

    fn observe(&mut self, ttl_ms: Option<u64>, scope: Option<CacheScope>) {
        let ttl_ms = ttl_ms.unwrap_or(self.fallback.ttl_ms);
        let scope = scope.unwrap_or(self.fallback.scope);
        self.ttl_ms = Some(self.ttl_ms.map_or(ttl_ms, |current| current.min(ttl_ms)));
        self.public = (!self.saw_page || self.public) && scope == CacheScope::Public;
        self.saw_page = true;
    }

    fn finish(self) -> McpCachePolicy {
        let scope = if self.public {
            CacheScope::Public
        } else {
            CacheScope::Private
        };
        McpCachePolicy::new(self.ttl_ms.unwrap_or_default(), scope)
    }
}

async fn connect_transport<T, E, A>(
    handler: McpClientHandler,
    transport: T,
    protocol_hint: Option<McpProtocolHint>,
) -> Result<RunningService<RoleClient, McpClientHandler>, rmcp::service::ClientInitializeError>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    serve_client_with_lifecycle(handler, transport, lifecycle(protocol_hint)).await
}

fn client_info() -> ClientInfo {
    let elicitation = ElicitationCapability::new()
        .with_form(FormElicitationCapability::new())
        .with_url(UrlElicitationCapability::new());
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation = Some(elicitation);
    ClientInfo::new(
        capabilities,
        Implementation::new("phi", env!("CARGO_PKG_VERSION")),
    )
}

fn lifecycle(protocol_hint: Option<McpProtocolHint>) -> ClientLifecycleMode {
    match protocol_hint {
        Some(McpProtocolHint::Legacy) => ClientLifecycleMode::Initialize,
        None => ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        },
    }
}

pub(super) fn protocol_hint(peer: &rmcp::service::Peer<RoleClient>) -> Option<McpProtocolHint> {
    peer.peer_info()
        .filter(|info| info.protocol_version != ProtocolVersion::V_2026_07_28)
        .map(|_| McpProtocolHint::Legacy)
}

pub(super) fn is_modern_peer(peer: &rmcp::service::Peer<RoleClient>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.protocol_version == ProtocolVersion::V_2026_07_28)
}

fn metadata_fallback_policy(peer: &rmcp::service::Peer<RoleClient>) -> McpCachePolicy {
    if is_modern_peer(peer) {
        McpCachePolicy::transient()
    } else {
        McpCachePolicy::new(LEGACY_CACHE_TTL_MS, CacheScope::Public)
    }
}

fn resolve_headers(
    server_name: &str,
    headers: &HashMap<String, String>,
    auth: Option<McpAuth>,
    bearer_token: &Option<String>,
    bearer_token_env: &Option<String>,
) -> anyhow::Result<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut headers = headers
        .iter()
        .map(|(name, value)| (name.clone(), interpolate_env(value)))
        .collect::<HashMap<_, _>>();
    if !headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        let token = bearer_token
            .clone()
            .or_else(|| bearer_token_from_env(bearer_token_env));
        if let Some(token) = token {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        } else if matches!(auth, Some(McpAuth::Oauth)) {
            anyhow::bail!(
                "MCP OAuth is not implemented for '{server_name}'; configure explicit authorization headers or a bearer token"
            );
        }
    }
    parse_headers(&headers)
}

fn bearer_token_from_env(env_name: &Option<String>) -> Option<String> {
    env_name
        .as_ref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}

fn interpolate_env(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '$' && chars.get(index + 1) == Some(&'{') {
            if let Some(end) = chars[index + 2..].iter().position(|ch| *ch == '}') {
                let name = chars[index + 2..index + 2 + end].iter().collect::<String>();
                output.push_str(&std::env::var(name).unwrap_or_default());
                index += end + 3;
                continue;
            }
        }
        if chars[index] == '$' && chars.get(index + 1) == Some(&'e') {
            if let Some(rest) = value.get(index..) {
                if let Some(name) = rest.strip_prefix("$env:") {
                    let name = name
                        .chars()
                        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                        .collect::<String>();
                    if !name.is_empty() {
                        output.push_str(&std::env::var(&name).unwrap_or_default());
                        index += "$env:".len() + name.len();
                        continue;
                    }
                }
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

fn parse_headers(
    headers: &HashMap<String, String>,
) -> anyhow::Result<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut result = HashMap::new();
    for (name, value) in headers {
        let h_name: http::HeaderName = name
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header name '{name}': {e}"))?;
        let h_value: http::HeaderValue = value
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header value for '{name}': {e}"))?;
        result.insert(h_name, h_value);
    }
    Ok(result)
}

fn stdio_transport(
    command: Command,
    server_name: String,
) -> std::io::Result<(TokioChildProcess, AbortOnDropHandle<()>)> {
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = stderr.expect("MCP stderr was configured as a pipe");
    let task = tokio::spawn(
        async move {
            // Split oversized lines so a noisy server cannot grow the buffer without bound.
            const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::with_capacity(MAX_DIAGNOSTIC_BYTES);
            loop {
                buffer.clear();
                match (&mut reader)
                    .take(MAX_DIAGNOSTIC_BYTES as u64)
                    .read_until(b'\n', &mut buffer)
                    .await
                {
                    Ok(0) => break,
                    Ok(_) => {
                        let message = String::from_utf8_lossy(&buffer);
                        tracing::warn!(server = %server_name, source = "mcp.stderr", "{}", message.trim_end_matches(['\r', '\n']));
                    }
                    Err(error) => {
                        tracing::warn!(server = %server_name, %error, "failed to read MCP stderr");
                        break;
                    }
                }
            }
        }
        .with_current_subscriber(),
    );
    Ok((transport, AbortOnDropHandle::new(task)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_stderr_uses_diagnostic_logging() {
        let path = std::env::temp_dir().join(format!("phi-mcp-log-{}", uuid::Uuid::new_v4()));
        let output = std::fs::File::create(&path).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(output))
            .finish();
        async {
            let mut command = Command::new("sh");
            command
                .args([
                    "-c",
                    "printf '%s\\n' \"$1\" >&2; printf '\\377final diagnostic' >&2",
                    "sh",
                ])
                .arg(format!("server diagnostic {}", "z".repeat(20_000)));
            let (mut transport, task) = stdio_transport(command, "test-server".into()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            transport.graceful_shutdown().await.unwrap();
        }
        .with_subscriber(subscriber)
        .await;
        let output = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(output.contains("server diagnostic"));
        assert!(output.contains("test-server"));
        assert!(output.contains("mcp.stderr"));
        assert!(output.contains("final diagnostic"));
        assert_eq!(output.matches('z').count(), 20_000);
    }

    #[test]
    fn mcp_oauth_without_explicit_credentials_is_unsupported() {
        let error = resolve_headers(
            "test-server",
            &HashMap::new(),
            Some(McpAuth::Oauth),
            &None,
            &None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("MCP OAuth is not implemented"));
    }

    #[test]
    fn mcp_oauth_preserves_explicit_credentials() {
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), "Bearer header-token".into());
        let resolved = resolve_headers(
            "test-server",
            &headers,
            Some(McpAuth::Oauth),
            &Some("token".into()),
            &None,
        )
        .unwrap();
        assert_eq!(
            resolved[&http::header::AUTHORIZATION],
            "Bearer header-token"
        );
        let resolved = resolve_headers(
            "test-server",
            &HashMap::new(),
            Some(McpAuth::Oauth),
            &Some("token".into()),
            &None,
        )
        .unwrap();
        assert_eq!(resolved[&http::header::AUTHORIZATION], "Bearer token");
    }

    #[test]
    fn mcp_oauth_status_uses_explicit_configuration() {
        for (credentials, needs_token) in [
            (serde_json::json!({}), true),
            (serde_json::json!({"bearerToken": "token"}), false),
            (
                serde_json::json!({"bearerTokenEnv": "PHI_MCP_TEST_TOKEN"}),
                false,
            ),
            (
                serde_json::json!({"headers": {"Authorization": "Bearer token"}}),
                false,
            ),
        ] {
            let mut config = serde_json::json!({"url": "https://example.invalid", "auth": "oauth"});
            config
                .as_object_mut()
                .unwrap()
                .extend(credentials.as_object().unwrap().clone());
            let config: McpServerConfig = serde_json::from_value(config).unwrap();
            assert_eq!(config.needs_oauth_token(), needs_token);
        }
    }

    #[test]
    fn retries_only_transient_http_initialization_errors() {
        use rmcp::service::ClientInitializeError;
        use rmcp::transport::streamable_http_client::StreamableHttpError;
        type HttpError = StreamableHttpError<reqwest::Error>;
        let transport_error = |error: HttpError| ClientInitializeError::TransportError {
            error: rmcp::transport::DynamicTransportError::from_parts(
                "http",
                std::any::TypeId::of::<HttpError>(),
                Box::new(error),
            ),
            context: "send discover request".into(),
        };
        assert!(retryable_http_initialization(
            &ClientInitializeError::ConnectionClosed("discover".into())
        ));
        assert!(retryable_http_initialization(&transport_error(
            HttpError::TransportChannelClosed
        )));
        assert!(!retryable_http_initialization(&transport_error(
            HttpError::UnexpectedContentType(None)
        )));
        assert!(!retryable_http_initialization(
            &ClientInitializeError::Cancelled
        ));
    }

    #[tokio::test]
    async fn retries_connection_errors_but_not_invalid_requests() {
        use rmcp::transport::streamable_http_client::StreamableHttpError;
        type HttpError = StreamableHttpError<reqwest::Error>;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (url, retryable) in [(url.as_str(), true), ("not a url", false)] {
            let error = client.get(url).send().await.unwrap_err();
            let error = rmcp::service::ClientInitializeError::TransportError {
                error: rmcp::transport::DynamicTransportError::from_parts(
                    "http",
                    std::any::TypeId::of::<HttpError>(),
                    Box::new(HttpError::Client(error)),
                ),
                context: "send discover request".into(),
            };
            assert_eq!(retryable_http_initialization(&error), retryable);
        }
    }

    #[tokio::test]
    async fn falls_back_to_legacy_initialization() {
        assert!(matches!(
            lifecycle(Some(McpProtocolHint::Legacy)),
            ClientLifecycleMode::Initialize
        ));
        use rmcp::model::{
            ClientJsonRpcMessage, ClientRequest, ErrorCode, ErrorData, InitializeResult,
            ListToolsResult, ServerCapabilities, ServerJsonRpcMessage, ServerResult,
        };
        use rmcp::transport::{IntoTransport, Transport};

        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let mut server =
                IntoTransport::<rmcp::RoleServer, _, _>::into_transport(server_transport);
            let ClientJsonRpcMessage::Request(discover) = server.receive().await.unwrap() else {
                panic!("expected discover request");
            };
            assert!(matches!(
                discover.request,
                ClientRequest::DiscoverRequest(_)
            ));
            server
                .send(ServerJsonRpcMessage::error(
                    ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", None),
                    Some(discover.id),
                ))
                .await
                .unwrap();
            let ClientJsonRpcMessage::Request(initialize) = server.receive().await.unwrap() else {
                panic!("expected initialize request");
            };
            assert!(matches!(
                initialize.request,
                ClientRequest::InitializeRequest(_)
            ));
            server
                .send(ServerJsonRpcMessage::response(
                    ServerResult::InitializeResult(InitializeResult::new(
                        ServerCapabilities::default(),
                    )),
                    initialize.id,
                ))
                .await
                .unwrap();
            assert!(matches!(
                server.receive().await,
                Some(ClientJsonRpcMessage::Notification(_))
            ));
            let ClientJsonRpcMessage::Request(list_tools) = server.receive().await.unwrap() else {
                panic!("expected tools/list request");
            };
            assert!(matches!(
                list_tools.request,
                ClientRequest::ListToolsRequest(_)
            ));
            server
                .send(ServerJsonRpcMessage::response(
                    ServerResult::ListToolsResult(ListToolsResult::default()),
                    list_tools.id,
                ))
                .await
                .unwrap();
            for expected in ["resources/list", "prompts/list"] {
                let ClientJsonRpcMessage::Request(request) = server.receive().await.unwrap() else {
                    panic!("expected {expected} request");
                };
                assert_eq!(request.request.method(), expected);
                server
                    .send(ServerJsonRpcMessage::error(
                        ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "not supported", None),
                        Some(request.id),
                    ))
                    .await
                    .unwrap();
            }
        });
        let (changes, _) = metadata_change_channel();
        let handler = McpClientHandler::new(
            String::from("legacy"),
            McpElicitationRouter::default(),
            changes,
            None,
        );
        let running = connect_transport(handler, client_transport, None)
            .await
            .unwrap();

        assert_eq!(
            running.peer().peer_info().unwrap().protocol_version,
            ProtocolVersion::V_2025_11_25
        );
        let listed = list_tools(running.peer()).await.unwrap();
        assert_eq!(listed.policy.ttl_ms, LEGACY_CACHE_TTL_MS);
        assert_eq!(listed.policy.scope, CacheScope::Public);
        let resources = optional_list_or_empty(
            list_resources(running.peer()).await,
            running.peer(),
            "legacy",
            "resource",
        );
        assert!(resources.items.is_empty());
        assert_eq!(resources.policy.scope, CacheScope::Public);
        let prompts = optional_list_or_empty(
            list_prompts(running.peer()).await,
            running.peer(),
            "legacy",
            "prompt",
        );
        assert!(prompts.items.is_empty());
        assert_eq!(prompts.policy.scope, CacheScope::Public);
        let _ = running.cancel().await;
        server_task.await.unwrap();
    }

    #[test]
    fn aggregates_cache_policy_conservatively() {
        let mut paginated = CachePolicyAccumulator::new(McpCachePolicy::transient());
        paginated.observe(Some(30_000), Some(CacheScope::Public));
        paginated.observe(Some(5_000), Some(CacheScope::Private));
        let paginated = paginated.finish();
        assert_eq!(paginated.ttl_ms, 5_000);
        assert_eq!(paginated.scope, CacheScope::Private);

        let mut missing = CachePolicyAccumulator::new(McpCachePolicy::transient());
        missing.observe(None, None);
        let missing = missing.finish();
        assert_eq!(missing.ttl_ms, 0);
        assert_eq!(missing.scope, CacheScope::Private);

        let fallback = McpCachePolicy::new(LEGACY_CACHE_TTL_MS, CacheScope::Public);
        let mut legacy = CachePolicyAccumulator::new(fallback);
        legacy.observe(None, None);
        let legacy = legacy.finish();
        assert_eq!(legacy.ttl_ms, LEGACY_CACHE_TTL_MS);
        assert_eq!(legacy.scope, CacheScope::Public);
    }
}
