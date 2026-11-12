use super::McpClientManager;
use crate::permission::runtime::PermissionRuntime;
use crate::tools::canonical_tool_name;
use rig::tool::{DynamicTool, ToolExecutionError, ToolOutput};
use std::fmt;

#[derive(Debug)]
pub struct McpToolError(pub String);

impl fmt::Display for McpToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for McpToolError {}

pub struct McpRegisteredTool {
    pub manager: McpClientManager,
    pub permission: Option<PermissionRuntime>,
    pub server_name: String,
    pub tool_name: String,
    pub registered_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl McpRegisteredTool {
    pub fn into_dynamic_tool(self) -> DynamicTool {
        let name = canonical_tool_name(&self.registered_name);
        let description = self.description;
        let parameters = self.input_schema;
        let manager = self.manager;
        let permission = self.permission;
        let server = self.server_name;
        let tool = self.tool_name;
        DynamicTool::new(name, description, parameters, move |context, args| {
            let manager = manager.clone();
            let permission = permission.clone();
            let server = server.clone();
            let tool = tool.clone();
            Box::pin(async move {
                authorize_mcp(&permission, &mcp_target_input(&server, &tool))
                    .await
                    .map_err(tool_error)?;
                let result = manager
                    .call_tool(&server, &tool, args.as_object().cloned())
                    .await
                    .map_err(tool_error)?;
                context.insert_result(crate::tools::ScriptToolResult(
                    serde_json::to_value(&result).map_err(|error| tool_error(error.into()))?,
                ));
                render_call_result(result).map(ToolOutput::text)
            })
        })
    }
}

fn mcp_target_input(server: &str, tool: &str) -> String {
    format!(
        "call {}/{}",
        permission_identifier(server),
        permission_identifier(tool)
    )
}

fn permission_identifier(value: &str) -> String {
    value.replace('%', "%25").replace('/', "%2F")
}

async fn authorize_mcp(permission: &Option<PermissionRuntime>, input: &str) -> anyhow::Result<()> {
    permission
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("MCP invocation requires an explicit permission policy"))?
        .authorize_tool("mcp", input, None, Some("extension:mcp".into()))
        .await
}

const MAX_RESULT_BYTES: usize = 256 * 1024;

fn render_call_result(result: rmcp::model::CallToolResult) -> Result<String, ToolExecutionError> {
    let output = project_call_result(&result);
    if result.is_error.unwrap_or(false) {
        return Err(tool_error(anyhow::anyhow!(if output == "(no output)" {
            "MCP tool returned an error".to_string()
        } else {
            output
        })));
    }
    Ok(output)
}

fn project_call_result(result: &rmcp::model::CallToolResult) -> String {
    let mut output = BoundedOutput::new(MAX_RESULT_BYTES);
    let mut has_text = false;
    for item in &result.content {
        match item {
            rmcp::model::ContentBlock::Text(text) => {
                has_text |= !text.text.trim().is_empty();
                if !text.text.is_empty() {
                    output.push(&text.text);
                }
            }
            rmcp::model::ContentBlock::Image(image) => output.push(&format!(
                "[image: mime={}, base64-bytes={}]",
                image.mime_type,
                image.data.len()
            )),
            rmcp::model::ContentBlock::Audio(audio) => output.push(&format!(
                "[audio: mime={}, base64-bytes={}]",
                audio.mime_type,
                audio.data.len()
            )),
            rmcp::model::ContentBlock::Resource(resource) => {
                project_embedded_resource(&mut output, &resource.resource)
            }
            rmcp::model::ContentBlock::ResourceLink(resource) => output.push(&format!(
                "[resource link: name={}, uri={}, mime={}, size={}]",
                resource.name,
                resource.uri,
                resource.mime_type.as_deref().unwrap_or("unknown"),
                resource
                    .size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )),
            _ => output.push("[unsupported MCP content block]"),
        }
    }
    if (!has_text || result.is_error.unwrap_or(false))
        && let Some(structured) = &result.structured_content
    {
        let structured =
            serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
        output.push(&format!("[structured content]\n{structured}"));
    }
    output.finish()
}

fn project_embedded_resource(output: &mut BoundedOutput, resource: &rmcp::model::ResourceContents) {
    match resource {
        rmcp::model::ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => {
            output.push(&format!(
                "[resource: uri={}, mime={}]",
                uri,
                mime_type.as_deref().unwrap_or("text/plain")
            ));
            output.push(text);
        }
        rmcp::model::ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => output.push(&format!(
            "[binary resource: uri={}, mime={}, base64-bytes={}]",
            uri,
            mime_type.as_deref().unwrap_or("application/octet-stream"),
            blob.len()
        )),
        _ => output.push("[unsupported embedded MCP resource]"),
    }
}

struct BoundedOutput {
    value: String,
    remaining: usize,
    truncated: bool,
}

impl BoundedOutput {
    fn new(limit: usize) -> Self {
        Self {
            value: String::new(),
            remaining: limit,
            truncated: false,
        }
    }

    fn push(&mut self, value: &str) {
        if self.remaining == 0 {
            self.truncated = true;
            return;
        }
        let separator = usize::from(!self.value.is_empty());
        if separator > self.remaining {
            self.truncated = true;
            return;
        }
        if separator == 1 {
            self.value.push('\n');
            self.remaining -= 1;
        }
        let end = value
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= self.remaining)
            .last()
            .unwrap_or_default();
        let end = if value.len() <= self.remaining {
            value.len()
        } else {
            end
        };
        self.value.push_str(&value[..end]);
        self.remaining -= end;
        self.truncated |= end < value.len();
    }

    fn finish(mut self) -> String {
        if self.value.is_empty() {
            return "(no output)".to_string();
        }
        if self.truncated {
            self.value.push_str("\n[output truncated]");
        }
        self.value
    }
}

fn tool_error(error: anyhow::Error) -> ToolExecutionError {
    ToolExecutionError::other(McpToolError(error.to_string()).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use rig::tool::{ToolContext, ToolSet};
    use rmcp::model::{CallToolResult, ContentBlock, Resource};

    use super::super::config::McpServerConfig;
    use crate::permission::PermissionConfig;
    use crate::permission::checker::PermissionChecker;

    #[test]
    fn projects_structured_and_non_text_results_safely() {
        let mut structured = CallToolResult::default();
        structured.structured_content = Some(serde_json::json!({ "count": 2 }));
        let output = render_call_result(structured).unwrap();
        assert!(output.contains("[structured content]"));
        assert!(output.contains("\"count\": 2"));

        let resource = Resource::new("https://example.com/file", "file")
            .with_mime_type("application/pdf")
            .with_size(123);
        let result = CallToolResult::success(vec![
            ContentBlock::image("large-image-data", "image/png"),
            ContentBlock::audio("large-audio-data", "audio/wav"),
            ContentBlock::ResourceLink(resource),
        ]);

        let output = render_call_result(result).unwrap();
        assert!(output.contains("[image: mime=image/png"));
        assert!(output.contains("[audio: mime=audio/wav"));
        assert!(output.contains("uri=https://example.com/file"));
        assert!(!output.contains("large-image-data"));
        assert!(!output.contains("large-audio-data"));
    }

    #[test]
    fn bounds_large_results() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("x".repeat(MAX_RESULT_BYTES * 2)),
            ContentBlock::resource(rmcp::model::ResourceContents::BlobResourceContents {
                uri: "file:///large.bin".to_string(),
                mime_type: Some("application/octet-stream".to_string()),
                blob: "z".repeat(MAX_RESULT_BYTES),
                meta: None,
            }),
        ]);

        let output = render_call_result(result).unwrap();
        assert!(output.len() <= MAX_RESULT_BYTES + "\n[output truncated]".len());
        assert!(output.ends_with("[output truncated]"));
        assert!(!output.contains("file:///large.bin"));
    }

    const SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}, 'resources': {}}, 'serverInfo': {'name': 'permission-fixture', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'probe', 'description': 'probe', 'inputSchema': {'type': 'object'}}]}
    elif method == 'resources/list':
        result = {'resources': [{'uri': 'fixture://document', 'name': 'document'}]}
    elif method == 'prompts/list':
        result = {'prompts': []}
    elif method in ('tools/call', 'resources/read'):
        with open(sys.argv[1], 'a') as effects:
            effects.write(method + '\n')
        if method == 'tools/call':
            if request.get('params', {}).get('arguments', {}).get('elicit'):
                print(json.dumps({'jsonrpc': '2.0', 'id': 'elicitation', 'method': 'elicitation/create', 'params': {'mode': 'form', 'message': 'Confirm this call', 'requestedSchema': {'type': 'object', 'properties': {}}}}), flush=True)
                answer = json.loads(sys.stdin.readline())
                assert answer['id'] == 'elicitation' and answer['result']['action'] == 'accept'
            if request.get('params', {}).get('arguments', {}).get('structured_error'):
                result = {'content': [{'type': 'text', 'text': 'conflict'}], 'isError': True, 'structuredContent': {'kind': 'conflict', 'retry': False}}
            else:
                result = {'content': [{'type': 'text', 'text': 'called'}]}
        else:
            result = {'contents': [{'uri': 'fixture://document', 'text': 'read'}]}
    else:
        print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32601, 'message': 'method not found'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
    "#;

    struct Fixture {
        root: PathBuf,
        manager: McpClientManager,
    }

    impl Fixture {
        async fn new() -> Self {
            Self::with_hidden_tools(&[]).await
        }

        async fn with_hidden_tools(hidden: &[&str]) -> Self {
            let root =
                std::env::temp_dir().join(format!("phi-mcp-permission-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("server.py"), SERVER).unwrap();
            let configs: HashMap<String, McpServerConfig> = serde_json::from_value(serde_json::json!({
                "fixture": {"command":"python3", "args":[root.join("server.py"), root.join("effects")], "exposure":"direct", "exposeResources":true, "toolExposure": hidden.iter().map(|name| (*name, crate::tools::ToolExposure::Hidden)).collect::<std::collections::HashMap<_, _>>()}
            })).unwrap();
            let manager = McpClientManager::load_with_settings(&configs, Default::default());
            manager.connect("fixture").await.unwrap();
            Self { root, manager }
        }

        async fn tools(&self, config: serde_json::Value) -> ToolSet {
            let config = PermissionConfig::try_from_value(config).unwrap();
            let checker = PermissionChecker::new(&config, Some(self.root.clone()));
            let permission = PermissionRuntime::new(Arc::new(Mutex::new(checker)));
            self.tools_with_permission(permission).await
        }

        async fn tools_with_permission(&self, permission: PermissionRuntime) -> ToolSet {
            ToolSet::from_dynamic_tools(
                self.manager
                    .tool_registrations(Some(permission))
                    .await
                    .into_iter()
                    .map(|registration| registration.tool)
                    .collect(),
            )
        }

        fn effects(&self) -> usize {
            std::fs::read_to_string(self.root.join("effects"))
                .unwrap_or_default()
                .lines()
                .count()
        }

        async fn close(self) {
            self.manager.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[tokio::test]
    async fn codemode_mcp_errors_preserve_structured_data_without_hiding_failure() {
        let fixture = Fixture::new().await;
        let registrations = fixture.manager.tool_registrations(None).await;
        assert_eq!(
            registrations
                .iter()
                .find(|entry| entry.tool.name() == "fixture_probe")
                .unwrap()
                .namespace
                .as_deref(),
            Some("fixture")
        );
        assert!(registrations.iter().all(|entry| entry.tool.name() != "MCP"));
        let tools = fixture.tools(serde_json::json!({"mode":"allow"})).await;
        for (name, args) in [("fixture_probe", r#"{"structured_error":true}"#)] {
            let mut context = ToolContext::new();
            let result = tools.execute(name, args, &mut context).await;
            assert!(result.error().is_some(), "{name}: {result:?}");
            assert!(result.output().render().contains("conflict"));
            let data = &context
                .result::<crate::tools::ScriptToolResult>()
                .unwrap()
                .0;
            assert_eq!(data["isError"], true);
            assert_eq!(
                data["structuredContent"],
                serde_json::json!({"kind":"conflict","retry":false})
            );
            assert_eq!(data["content"][0]["text"], "conflict");
        }
        assert_eq!(fixture.effects(), 1);
        fixture.close().await;
    }

    #[tokio::test]
    async fn registered_calls_enforce_mode_and_specific_rules_before_effects() {
        let fixture = Fixture::new().await;
        for (config, allowed) in [
            (serde_json::json!({"mode":"allow"}), true),
            (serde_json::json!({"mode":"ask"}), false),
            (serde_json::json!({"mode":"deny"}), false),
            (serde_json::json!({"mode":"yolo", "mcp":"deny"}), true),
            (
                serde_json::json!({"mode":"allow", "mcp":{"call fixture/**":"deny"}}),
                false,
            ),
            (
                serde_json::json!({"mode":"deny", "mcp":{"call fixture/**":"allow"}}),
                true,
            ),
        ] {
            let before = fixture.effects();
            let tools = fixture.tools(config).await;
            for (name, args) in [("fixture_probe", "{}"), ("fixture_get_document", "{}")] {
                let result = tools.execute(name, args, &mut ToolContext::new()).await;
                assert_eq!(result.error().is_none(), allowed, "{name}: {result:?}");
            }
            assert_eq!(fixture.effects() - before, if allowed { 2 } else { 0 });
        }
        fixture.close().await;
    }

    #[tokio::test]
    async fn registered_calls_prompt_once_with_semantic_identity() {
        use crate::core::interaction::PermissionDecision;
        let fixture = Fixture::new().await;
        let checker =
            PermissionChecker::new(&PermissionConfig::default(), Some(fixture.root.clone()));
        let permission = PermissionRuntime::new(Arc::new(Mutex::new(checker))).interactive(true);
        let (sender, mut requests) = tokio::sync::mpsc::channel(4);
        let permission = permission.with_interaction_sender(Some(sender));
        let (elicitation_sender, mut elicitations) = tokio::sync::mpsc::channel(4);
        let manager = fixture
            .manager
            .clone()
            .with_interaction_sender(Some(elicitation_sender));
        let tools = Arc::new(ToolSet::from_dynamic_tools(
            manager
                .tool_registrations(Some(permission))
                .await
                .into_iter()
                .map(|registration| registration.tool)
                .collect(),
        ));
        for (name, args) in [
            ("fixture_probe", r#"{"elicit":true}"#),
            ("fixture_probe", r#"{"elicit":true,"query":"different"}"#),
        ] {
            let tools = tools.clone();
            let call =
                tokio::spawn(
                    async move { tools.execute(name, args, &mut ToolContext::new()).await },
                );
            let request = tokio::time::timeout(std::time::Duration::from_secs(2), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request.permission.tool, "mcp");
            assert_eq!(request.permission.input, "call fixture/probe");
            request.reply.send(PermissionDecision::AllowOnce).unwrap();
            let elicitation =
                tokio::time::timeout(std::time::Duration::from_secs(2), elicitations.recv())
                    .await
                    .unwrap()
                    .unwrap();
            elicitation
                .reply
                .send(crate::core::interaction::McpElicitationResponse::Accept {
                    content: Some(serde_json::json!({})),
                })
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), call)
                    .await
                    .unwrap()
                    .unwrap()
                    .error()
                    .is_none()
            );
            assert!(requests.try_recv().is_err());
        }
        assert_eq!(fixture.effects(), 2);
        fixture.close().await;
    }

    #[tokio::test]
    async fn hidden_exposure_blocks_stale_registrations_even_in_yolo() {
        let fixture = Fixture::with_hidden_tools(&["probe"]).await;
        let permission = PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfig::try_from_value(serde_json::json!({"mode":"yolo"})).unwrap(),
            None,
        ))));
        let stale = McpRegisteredTool {
            manager: fixture.manager.clone(),
            permission: Some(permission.clone()),
            server_name: "fixture".into(),
            tool_name: "probe".into(),
            registered_name: "fixture_probe".into(),
            description: "stale direct definition".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let tools = ToolSet::from_dynamic_tools(
            fixture
                .manager
                .tool_registrations(Some(permission))
                .await
                .into_iter()
                .map(|registration| registration.tool)
                .chain([stale.into_dynamic_tool()])
                .collect(),
        );
        for (name, args) in [("fixture_probe", "{}")] {
            let result = tools.execute(name, args, &mut ToolContext::new()).await;
            assert!(
                result.error().unwrap().to_string().contains("disabled"),
                "{result:?}"
            );
        }
        assert_eq!(fixture.effects(), 0);
        fixture.close().await;
    }

    #[tokio::test]
    async fn pooled_mcp_calls_keep_permission_routing_and_grants_instance_local() {
        use crate::core::interaction::PermissionDecision;
        let fixture = Fixture::new().await;
        let mut instances = Vec::new();
        for _ in 0..2 {
            let permission = PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(
                &PermissionConfig::default(),
                Some(fixture.root.clone()),
            ))))
            .interactive(true);
            let (sender, receiver) = tokio::sync::mpsc::channel(4);
            let permission = permission.with_interaction_sender(Some(sender));
            instances.push((
                Arc::new(fixture.tools_with_permission(permission).await),
                receiver,
            ));
        }
        for attempt in 0..2 {
            let (name, args) = ("fixture_probe", "{}");
            for (index, (tools, requests)) in instances.iter_mut().enumerate() {
                let calling = tools.clone();
                let call = tokio::spawn(async move {
                    calling.execute(name, args, &mut ToolContext::new()).await
                });
                if attempt == 0 || index == 1 {
                    let request =
                        tokio::time::timeout(std::time::Duration::from_secs(2), requests.recv())
                            .await
                            .unwrap()
                            .unwrap();
                    assert_eq!(request.permission.tool, "mcp");
                    assert_eq!(request.permission.input, "call fixture/probe");
                    let decision = if index == 0 {
                        PermissionDecision::AllowAlways("call fixture/probe".into())
                    } else {
                        PermissionDecision::Deny
                    };
                    request.reply.send(decision).unwrap();
                }
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), call)
                        .await
                        .unwrap()
                        .unwrap()
                        .error()
                        .is_none(),
                    index == 0
                );
                assert!(requests.try_recv().is_err());
            }
        }
        assert_eq!(fixture.effects(), 2);
        fixture.close().await;
    }

    #[tokio::test]
    async fn restored_mcp_grants_resolve_ask_but_never_specific_deny() {
        let fixture = Fixture::new().await;
        for (config, allowed) in [
            (serde_json::json!({"mode":"ask"}), true),
            (
                serde_json::json!({"mode":"allow", "mcp":{"call fixture/probe":"deny"}}),
                false,
            ),
        ] {
            let config = PermissionConfig::try_from_value(config).unwrap();
            let checker = PermissionChecker::new(&config, Some(fixture.root.clone()));
            let permission = PermissionRuntime::new(Arc::new(Mutex::new(checker)));
            permission.load_session_allowlist(&[("mcp".into(), "call fixture/probe".into())]);
            let tools = fixture.tools_with_permission(permission).await;
            let before = fixture.effects();
            for (name, args) in [("fixture_probe", "{}")] {
                assert_eq!(
                    tools
                        .execute(name, args, &mut ToolContext::new())
                        .await
                        .error()
                        .is_none(),
                    allowed
                );
            }
            assert_eq!(fixture.effects() - before, if allowed { 1 } else { 0 });
        }
        fixture.close().await;
    }
}
