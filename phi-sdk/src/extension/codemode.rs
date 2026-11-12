use crate::core::event::{NestedCallStatus, NestedCalls, NestedToolCall, ToolResultDetails};
use crate::tools::{
    DEFAULT_TOOL_SEARCH_LIMIT, ToolAuthorization, ToolMetadata, ToolOwner, ToolPromptMetadata,
    ToolRegistration, ToolRegistry,
};
use crate::transcript::{ImageSource, Transcript, TranscriptEntryKind};
use futures::future::BoxFuture;
use phi_codemode::{
    ArgumentMode, CodemodeError, CodemodeSandbox, CodemodeTool, ErrorKind, ExecuteOptions,
    ExecutionOutcome, ExecutionResult, ExecutionTimeout, OutputItem, SandboxOptions,
    ToolDefinition, parse_codemode_source, render_tool_signature, to_codemode_identifier,
};
use rig::tool::{DynamicTool, ToolExecutionError, ToolOutput};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const STORE_EVENT: &str = "codemode_store";
const MAX_RECORDED_CALLS: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 8 * 1024;
const MAX_TOTAL_ARGUMENT_BYTES: usize = 32 * 1024;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const DEFAULT_OUTPUT_TOKENS: usize = 10_000;
const CHARS_PER_TOKEN: usize = 4;
const DESCRIPTION: &str = "Run JavaScript as an async function body in an isolated QuickJS sandbox. Use top-level await and return. No filesystem, network, timers or Node APIs. Call tools.Name(args); calls retain normal permissions and tool restrictions. Tools return text or structured JSON; failures reject. Use Promise.allSettled for independent calls. Reads, including the same file, and edits/writes to different files can overlap; edits/writes to the same file use a mutation queue. Explicitly await an edit/write before reading its result. Codemode scripts run serially to preserve store updates. Only text(), image(), console output and return reach the model. Failed scripts retain partial output; tool effects are not rolled back. Unfinished calls are cancelled. Codemode cannot call itself, mutate Tape anchors or threads, or call tools requiring durable finalization. Optional first line: // @options: {\"max_output_tokens\":10000,\"timeout_ms\":60000}. No default deadline. store(key,value)/load(key) keep small JSON values on the current history branch; only successful scripts commit writes. Discover tools with ALL_TOOLS, searchTools(query,{limit?,namespace?}), describeTool(name), describeNamespace(name).";

pub(crate) fn registration() -> ToolRegistration {
    let tool = DynamicTool::new(
        "Codemode",
        description(&[], 0),
        json!({
            "type":"object", "properties":{"code":{"type":"string","description":"JavaScript async function body"}}, "required":["code"], "additionalProperties":false
        }),
        |context, input| {
            Box::pin(async move {
                let host = context.require::<CodemodeHost>()?.clone();
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Input {
                    code: String,
                }
                let input: Input = serde_json::from_value(input)
                    .map_err(|error| ToolExecutionError::other(error.to_string()))?;
                let (text, details, success) = host
                    .execute(&input.code)
                    .await
                    .map_err(ToolExecutionError::other)?;
                context.insert_result(details);
                if success {
                    Ok(ToolOutput::text(text))
                } else {
                    Err(ToolExecutionError::other(text))
                }
            })
        },
    );
    ToolRegistration::phi_dynamic(tool, "builtin", ToolOwner::Phi,
        ToolMetadata::sequential(ToolPromptMetadata::new("Run JavaScript to compose governed tool calls")
            .with_guidelines(["Use Codemode to batch independent calls, chain tools and filter large results before returning output."]))
            .with_authorization(ToolAuthorization::Invocation))
}

pub(crate) fn description(tools: &[ToolDefinition], inline_budget: usize) -> String {
    let mut text = DESCRIPTION.to_string();
    let mut remaining = inline_budget.saturating_mul(CHARS_PER_TOKEN);
    for tool in tools {
        let section = tool_description(tool);
        let length = section.chars().count();
        if length <= remaining {
            remaining -= length;
            text.push_str("\n\n");
            text.push_str(&section);
        }
    }
    text
}

fn tool_description(tool: &ToolDefinition) -> String {
    format!("{}\n{}", tool.description, render_tool_signature(tool))
}

#[derive(Default)]
struct CallRecorder {
    calls: Vec<NestedToolCall>,
    argument_bytes: usize,
    next_id: usize,
    incomplete: bool,
}

impl CallRecorder {
    fn start(&mut self, parent: &str, name: &str, args: &Value) -> NestedToolCall {
        self.next_id += 1;
        let bytes = serde_json::to_vec(args)
            .expect("tool arguments are JSON")
            .len();
        let arguments = if bytes <= MAX_ARGUMENT_BYTES
            && self.argument_bytes + bytes <= MAX_TOTAL_ARGUMENT_BYTES
        {
            self.argument_bytes += bytes;
            Some(args.clone())
        } else {
            self.incomplete = true;
            None
        };
        let call = NestedToolCall {
            id: format!("{parent}/{}", self.next_id),
            parent_tool_call_id: parent.into(),
            name: name.into(),
            arguments,
            status: NestedCallStatus::Running,
            duration_ms: 0,
            error: None,
            details: None,
        };
        if self.calls.len() < MAX_RECORDED_CALLS {
            self.calls.push(call.clone());
        } else {
            self.incomplete = true;
        }
        call
    }

    fn finish(&mut self, call: &NestedToolCall) {
        if let Some(record) = self.calls.iter_mut().find(|record| record.id == call.id) {
            *record = call.clone();
        }
    }

    fn snapshot(&self) -> Option<NestedCalls> {
        (!self.calls.is_empty() || self.incomplete).then(|| {
            let mut summary = NestedCalls {
                calls: self.calls.clone(),
                complete: !self.incomplete
                    && self
                        .calls
                        .iter()
                        .all(|call| !matches!(call.status, NestedCallStatus::Running)),
            };
            while serde_json::to_vec(&summary)
                .expect("nested calls are JSON")
                .len()
                > MAX_SUMMARY_BYTES
            {
                summary.calls.pop();
                summary.complete = false;
            }
            summary
        })
    }
}

pub(crate) type NestedCallObserver = Arc<dyn Fn(&NestedToolCall) + Send + Sync>;
type InvokeTool = dyn Fn(
        ToolRegistry,
        NestedToolCall,
        Value,
        NestedCallObserver,
    ) -> BoxFuture<'static, Result<Option<Value>, String>>
    + Send
    + Sync;
type ReadStore =
    dyn Fn() -> BoxFuture<'static, Result<BTreeMap<String, Value>, String>> + Send + Sync;
type CommitStore =
    dyn Fn(phi_codemode::StoreWrites) -> BoxFuture<'static, Result<(), String>> + Send + Sync;

#[derive(Clone)]
pub(crate) struct CodemodeHost {
    tools: ToolRegistry,
    parent: String,
    invoke: Arc<InvokeTool>,
    read_store: Arc<ReadStore>,
    commit_store: Arc<CommitStore>,
    recorder: Arc<Mutex<CallRecorder>>,
}

impl CodemodeHost {
    pub(crate) fn new(
        tools: ToolRegistry,
        parent: String,
        invoke: Arc<InvokeTool>,
        read_store: Arc<ReadStore>,
        commit_store: Arc<CommitStore>,
    ) -> Self {
        Self {
            tools,
            parent,
            invoke,
            read_store,
            commit_store,
            recorder: Arc::new(Mutex::new(CallRecorder::default())),
        }
    }

    async fn execute(&self, source: &str) -> Result<(String, ToolResultDetails, bool), String> {
        let started = Instant::now();
        let source = parse_codemode_source(source).map_err(|error| error.to_string())?;
        let deadline = source
            .options
            .timeout_ms
            .map(|value| {
                started
                    .checked_add(Duration::from_millis(value))
                    .ok_or_else(|| "timeout is too large".to_string())
            })
            .transpose()?;
        let prepare = async {
            let mut host = self.clone();
            host.tools = self
                .tools
                .discovery_registry()
                .await
                .map_err(|error| error.to_string())?;
            let declarations = host.tools.script_declarations();
            let tools = declarations
                .iter()
                .cloned()
                .map(|definition| {
                    let host = host.clone();
                    let name = definition.name.clone();
                    CodemodeTool::new(definition, move |arguments, _context| {
                        let host = host.clone();
                        let name = name.clone();
                        async move {
                            let arguments = arguments.unwrap_or_else(|| json!({}));
                            let call = host.recorder.lock().unwrap().start(
                                &host.parent,
                                &name,
                                &arguments,
                            );
                            let recorder = host.recorder.clone();
                            (host.invoke)(
                                host.tools.clone(),
                                call,
                                arguments,
                                Arc::new(move |call| recorder.lock().unwrap().finish(call)),
                            )
                            .await
                        }
                    })
                })
                .collect();
            let store = (self.read_store)().await?;
            let sandbox = CodemodeSandbox::new(SandboxOptions {
                cache_dir: self.tools.codemode_cache_dir(),
                tools,
                globals: discovery_globals(declarations, host.tools.clone()),
                timeout: None,
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
            Ok::<_, String>((sandbox, store))
        };
        let prepared = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), prepare)
                .await
                .map_err(|_| "execution timed out during preparation".to_string())
                .and_then(|result| result),
            None => prepare.await,
        };
        let timeout_error = || CodemodeError {
            kind: ErrorKind::Timeout,
            name: None,
            message: "execution timed out".into(),
            stack: None,
        };
        let timeout_result = || ExecutionResult {
            outcome: ExecutionOutcome::Failure(timeout_error()),
            output: Vec::new(),
            calls: Vec::new(),
        };
        let mut result = match prepared {
            Ok((sandbox, store)) => {
                let remaining =
                    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                let result = if remaining == Some(Duration::ZERO) {
                    timeout_result()
                } else {
                    sandbox
                        .execute(
                            &source.code,
                            ExecuteOptions {
                                store,
                                timeout: remaining
                                    .map(ExecutionTimeout::After)
                                    .unwrap_or(ExecutionTimeout::Unlimited),
                                ..Default::default()
                            },
                        )
                        .await
                };
                sandbox.close().await;
                result
            }
            Err(_) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                timeout_result()
            }
            Err(error) => return Err(error),
        };
        if matches!(result.outcome, ExecutionOutcome::Success { .. })
            && deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            result.outcome = ExecutionOutcome::Failure(timeout_error());
        }
        let success = matches!(result.outcome, ExecutionOutcome::Success { .. });
        let failure = match result.outcome {
            ExecutionOutcome::Success {
                value,
                store_writes,
            } => {
                (self.commit_store)(store_writes).await?;
                if let Some(value) = value {
                    result.output.push(OutputItem::Text {
                        text: value_text(&value),
                        console: false,
                    });
                }
                None
            }
            ExecutionOutcome::Failure(error) => Some(format!(
                "Script error: {:?}: {}\nTool effects are not undone.",
                error.kind,
                error.stack.as_deref().unwrap_or(&error.message)
            )),
        };
        let (output, images, mut image_offsets) = format_output(
            result.output,
            source
                .options
                .max_output_tokens
                .unwrap_or(DEFAULT_OUTPUT_TOKENS as u64) as usize,
            failure,
        )
        .await;
        let header = if success {
            "Script completed"
        } else {
            "Script failed"
        };
        let prefix = format!(
            "{header}\nWall time {:.1} seconds\nOutput:\n",
            started.elapsed().as_secs_f64()
        );
        for offset in &mut image_offsets {
            *offset += prefix.len();
        }
        Ok((
            format!("{prefix}{output}"),
            ToolResultDetails {
                nested_calls: self.recorder.lock().unwrap().snapshot(),
                images,
                image_offsets,
                ..Default::default()
            },
            success,
        ))
    }
}

pub(crate) fn read_store(transcript: &Transcript) -> BTreeMap<String, Value> {
    let mut store = BTreeMap::new();
    for entry in transcript.current_branch() {
        if let TranscriptEntryKind::Event { name, data } = &entry.kind
            && name == STORE_EVENT
        {
            if let Some(keys) = data["delete"].as_array() {
                for key in keys.iter().filter_map(Value::as_str) {
                    store.remove(key);
                }
            }
            if let Some(values) = data["set"].as_object() {
                store.extend(
                    values
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
            }
        }
    }
    store
}

fn discovery_globals(
    tools: Vec<ToolDefinition>,
    registry: crate::tools::ToolRegistry,
) -> Vec<CodemodeTool> {
    let tools = Arc::new(tools);
    ["searchTools", "describeTool", "describeNamespace"].into_iter().map(|name| {
        let tools = tools.clone();
        let registry = registry.clone();
        CodemodeTool::new(ToolDefinition { name: name.into(), ..Default::default() }, move |args, _| {
            let tools = tools.clone();
            let registry = registry.clone();
            async move {
                let args = args.as_ref().and_then(Value::as_array).ok_or("expected arguments")?;
                let query = args.first().and_then(Value::as_str).ok_or("expected a string")?;
                match name {
                    "describeTool" => Ok(tools.iter().find(|tool| tool.name == query || to_codemode_identifier(&tool.name) == query)
                        .map(|tool| Value::String(tool_description(tool)))),
                    "describeNamespace" => {
                        let names = tools.iter().filter(|tool| registry.tool_namespace(&tool.name).is_some_and(|name| name == query || to_codemode_identifier(name) == query))
                            .map(|tool| to_codemode_identifier(&tool.name)).collect::<Vec<_>>();
                        Ok((!names.is_empty()).then(|| json!({"name":query,"tools":names})))
                    }
                    _ => {
                        let options = args.get(1).filter(|value| !value.is_null());
                        if options.is_some_and(|value| !value.is_object()) { return Err("search options must be an object".into()); }
                        let limit = match options.and_then(|value| value.get("limit")) {
                            Some(value) => value.as_u64().filter(|value| *value > 0).ok_or("limit must be a positive integer")? as usize,
                            None => DEFAULT_TOOL_SEARCH_LIMIT,
                        };
                        let filter = options.and_then(|value| value.get("namespace"));
                        if filter.is_some_and(|value| !value.is_string()) { return Err("namespace must be a string".into()); }
                        let entries = registry.search_tools(&tools, query, filter.and_then(Value::as_str), limit).iter()
                            .map(|tool| json!({"name":to_codemode_identifier(&tool.name),"description":tool_description(tool)})).collect::<Vec<_>>();
                        Ok(Some(json!(entries)))
                    }
                }
            }
        }).with_arguments(ArgumentMode::Spread)
    }).collect()
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

async fn save_output(bytes: &[u8], extension: &str) -> Result<std::path::PathBuf, std::io::Error> {
    use tokio::io::AsyncWriteExt;
    let path = std::env::temp_dir().join(format!(
        "phi-codemode-{}.{}",
        uuid::Uuid::new_v4(),
        extension
    ));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(path)
}

async fn format_output(
    items: Vec<OutputItem>,
    max_tokens: usize,
    failure: Option<String>,
) -> (String, Vec<ImageSource>, Vec<usize>) {
    use base64::Engine;
    let text_count = items
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { console: false, .. }))
        .count();
    let mut index = 0;
    let mut console = Vec::new();
    let mut ordered = Vec::new();
    for item in items {
        match item {
            OutputItem::Text {
                text,
                console: true,
            } => console.push(text),
            OutputItem::Text {
                text,
                console: false,
            } => {
                index += 1;
                let text = if text_count > 1 {
                    format!("==> text {index}/{text_count} <==\n{text}")
                } else {
                    text
                };
                ordered.push(OutputItem::Text {
                    text,
                    console: false,
                });
            }
            item @ OutputItem::Image { .. } => ordered.push(item),
        }
    }
    if !console.is_empty() {
        let text = format!(
            "<console_output>\n{}\n</console_output>",
            console.join("\n")
        );
        ordered.push(OutputItem::Text {
            text,
            console: false,
        });
    }
    if let Some(text) = failure {
        ordered.push(OutputItem::Text {
            text: format!("<script_error>\n{text}\n</script_error>"),
            console: false,
        });
    }
    let combined = ordered
        .iter()
        .filter_map(|item| match item {
            OutputItem::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let budget = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    if combined.chars().count() > budget {
        let spilled = save_output(combined.as_bytes(), "txt").await;
        let head = combined.chars().take(budget / 2).collect::<String>();
        let tail = combined
            .chars()
            .rev()
            .take(budget - budget / 2)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let mut text = format!("{head}\n… output truncated …\n{tail}");
        match spilled {
            Ok(path) => text.push_str(&format!("\n[Full output: {}]", path.display())),
            Err(error) => text.push_str(&format!("\n[Could not save full output: {error}]")),
        }
        ordered.retain(|item| matches!(item, OutputItem::Image { .. }));
        ordered.insert(
            0,
            OutputItem::Text {
                text,
                console: false,
            },
        );
    }
    let mut output = String::new();
    let mut images = Vec::new();
    let mut image_offsets = Vec::new();
    let mut image_labels = HashMap::new();
    for item in ordered {
        if !output.is_empty() {
            output.push('\n');
        }
        match item {
            OutputItem::Text { text, .. } => output.push_str(&text),
            OutputItem::Image { data, mime_type } => {
                let extension = match mime_type.as_str() {
                    "image/png" => "png",
                    "image/jpeg" => "jpg",
                    "image/gif" => "gif",
                    "image/webp" => "webp",
                    _ => unreachable!("sandbox validates image MIME"),
                };
                let label = match image_labels.entry(data.clone()) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let label = match base64::engine::general_purpose::STANDARD.decode(&data) {
                            Ok(bytes) => match save_output(&bytes, extension).await {
                                Ok(path) => format!("[Image saved to {}]", path.display()),
                                Err(error) => format!("[Image could not be saved: {error}]"),
                            },
                            Err(error) => format!("[Image could not be saved: {error}]"),
                        };
                        entry.insert(label)
                    }
                };
                output.push_str(label);
                image_offsets.push(output.len());
                images.push(ImageSource::Base64 {
                    data: data.into(),
                    mime_type: mime_type.into(),
                });
            }
        }
    }
    (output, images, image_offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCatalogBuilder, ToolRegistry};

    #[tokio::test]
    async fn codemode_execution_uses_configured_cache_policy() {
        let root = std::path::PathBuf::from("/tmp").join(format!(
            "phi-codemode-cache-policy-{}",
            uuid::Uuid::new_v4()
        ));
        let directory = root.join("custom");
        for cache in [json!(false), json!(directory)] {
            let config = serde_json::from_value(json!({"cache": cache})).unwrap();
            let registry = discovery_registry().with_codemode_config(config);
            let expected = if cache == false {
                None
            } else {
                Some(directory.clone())
            };
            assert_eq!(registry.codemode_cache_dir(), expected);
            let host = CodemodeHost::new(
                registry,
                "parent".into(),
                Arc::new(|_, _, _, _| Box::pin(async { panic!("no tools expected") })),
                Arc::new(|| Box::pin(async { Ok(BTreeMap::new()) })),
                Arc::new(|_| Box::pin(async { Ok(()) })),
            );
            let (output, _, success) = host.execute("return 2;").await.unwrap();
            assert!(success, "{output}");
            assert_eq!(directory.join("modules").is_dir(), expected.is_some());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn codemode_deadline_covers_store_preparation() {
        let sandbox = CodemodeSandbox::new(SandboxOptions::default())
            .await
            .unwrap();
        sandbox.close().await;
        for (delay, code) in [
            (350, "return 1;"),
            (60, "return await tools.renamed_probe({});"),
        ] {
            let host = CodemodeHost::new(
                discovery_registry(),
                "parent".into(),
                Arc::new(|_, _, _, _| {
                    Box::pin(async {
                        tokio::time::sleep(Duration::from_millis(75)).await;
                        Ok(Some(json!(1)))
                    })
                }),
                Arc::new(move || {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        Ok(BTreeMap::new())
                    })
                }),
                Arc::new(|_| Box::pin(async { panic!("timed out scripts must not commit") })),
            );
            let (text, _, success) = host
                .execute(&format!("// @options: {{\"timeout_ms\":100}}\n{code}"))
                .await
                .unwrap();
            assert!(!success, "{text}");
            assert!(text.contains("Timeout"), "{text}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codemode_deadline_covers_cold_mcp_discovery() {
        use crate::extension::mcp::{
            McpClientManager,
            config::{McpServerConfig, McpSettings},
        };
        let config: McpServerConfig = serde_json::from_value(json!({
            "command": "sh", "args": ["-c", "exec sleep 0.35"], "exposure": "deferred"
        }))
        .unwrap();
        let manager = McpClientManager::load_with_settings(
            &HashMap::from([("deadline-probe".into(), config)]),
            McpSettings::default(),
        );
        let host = CodemodeHost::new(
            discovery_registry().with_mcp_manager(Some(manager.clone())),
            "parent".into(),
            Arc::new(|_, _, _, _| Box::pin(async { panic!("no tools expected") })),
            Arc::new(|| Box::pin(async { panic!("discovery exhausted the deadline") })),
            Arc::new(|_| Box::pin(async { panic!("timed out scripts must not commit") })),
        );
        let result = host
            .execute("// @options: {\"timeout_ms\":100}\nreturn 1;")
            .await;
        manager.shutdown().await;
        let (text, _, success) = result.unwrap();
        assert!(!success, "{text}");
        assert!(text.contains("Timeout"), "{text}");
    }

    #[tokio::test]
    async fn failure_diagnostics_follow_console_and_survive_truncation() {
        for max_tokens in [120, DEFAULT_OUTPUT_TOKENS] {
            let (output, images, offsets) = format_output(
                vec![
                    OutputItem::Text {
                        text: "x".repeat(2000),
                        console: false,
                    },
                    OutputItem::Text {
                        text: "y".repeat(2000),
                        console: true,
                    },
                ],
                max_tokens,
                Some(
                    "Script error: Script: diagnostic-marker\nTool effects are not undone.".into(),
                ),
            )
            .await;
            assert!(images.is_empty());
            assert!(offsets.is_empty());
            let truncated = max_tokens == 120;
            assert_eq!(output.contains("output truncated"), truncated);
            assert!(output.contains("Script error:"), "{output}");
            assert!(output.contains("diagnostic-marker"), "{output}");
            assert!(output.contains("<script_error>\nScript error:"), "{output}");
            assert!(
                output.contains("Tool effects are not undone.\n</script_error>"),
                "{output}"
            );
            let path = output.lines().find_map(|line| {
                line.strip_prefix("[Full output: ")
                    .and_then(|line| line.strip_suffix(']'))
            });
            assert_eq!(path.is_some(), truncated);
            let full = path
                .map(|path| std::fs::read_to_string(path).unwrap())
                .unwrap_or_else(|| output.clone());
            assert!(full.find("</console_output>").unwrap() < full.find("<script_error>").unwrap());
            assert!(full.ends_with("</script_error>"));
            assert!(!full.contains("==> text"));
            if let Some(path) = path {
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn nested_summary_has_a_serialized_byte_limit() {
        for marker in ["a", "界", "\\\"\n"] {
            let mut recorder = CallRecorder::default();
            for _ in 0..MAX_RECORDED_CALLS {
                let mut call = recorder.start("parent", "tool", &json!({}));
                call.status = NestedCallStatus::Error;
                call.error = Some(marker.repeat(500));
                recorder.finish(&call);
            }
            let summary = recorder.snapshot().unwrap();
            assert!(serde_json::to_vec(&summary).unwrap().len() <= MAX_SUMMARY_BYTES);
            assert!(!summary.complete);
            assert!(summary.calls.iter().all(|call| call.details.is_none()));
        }
    }

    fn discovery_registry() -> ToolRegistry {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(registration());
        for (name, namespace) in [("other_probe", "other"), ("hidden_probe", "fixture")] {
            catalog.register(
                ToolRegistration::phi_dynamic(
                    DynamicTool::new(
                        name,
                        "Search fixture documents",
                        json!({"type":"object"}),
                        |_context, _args| Box::pin(async { Ok(ToolOutput::text("result")) }),
                    ),
                    "test",
                    ToolOwner::Phi,
                    ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
                )
                .with_namespace(namespace),
            );
        }
        catalog.register(
            ToolRegistration::phi_dynamic(
                DynamicTool::new(
                    "renamed_probe",
                    "Search fixture documents",
                    json!({"type":"object","properties":{"query":{"type":"string"}}}),
                    |_context, _args| Box::pin(async { Ok(ToolOutput::text("result")) }),
                ),
                "test",
                ToolOwner::Phi,
                ToolMetadata::default()
                    .with_exposure(crate::tools::ToolExposure::Deferred)
                    .with_authorization(ToolAuthorization::Invocation),
            )
            .with_namespace("fixture"),
        );
        catalog
            .build()
            .unwrap()
            .with_policy(None, vec!["hidden_probe".into()])
            .with_codemode_config(crate::config::CodemodeConfig {
                cache: crate::config::CodemodeCache::Enabled(false),
                ..Default::default()
            })
    }

    #[test]
    fn codemode_inline_budget_limits_descriptions_not_callable_tools() {
        let registry = discovery_registry();
        let tools = registry.script_declarations();
        assert_eq!(tools.len(), 2);
        let base = description(&[], 0);
        assert_eq!(description(&tools, 0), base);
        for budget in [1, 16, 64, 256] {
            let rendered = description(&tools, budget);
            assert!(
                rendered.chars().count() - base.chars().count()
                    <= budget * CHARS_PER_TOKEN + tools.len() * 2
            );
            assert!(!rendered.contains("hidden_probe"));
        }
        let declarations = registry
            .with_codemode_config(crate::config::CodemodeConfig {
                inline_budget: 0,
                ..Default::default()
            })
            .provider_declarations();
        let codemode = declarations
            .iter()
            .find(|tool| tool.name == "Codemode")
            .unwrap();
        assert_eq!(codemode.description, base);
        assert!(!declarations.iter().any(|tool| tool.name == "renamed_probe"));
        assert!(!codemode.description.contains("renamed_probe"));
        let mut unicode = tools[0].clone();
        unicode.description = "中".repeat(100);
        assert_eq!(description(&[unicode], 1), base);
    }

    #[tokio::test]
    async fn codemode_namespace_discovery_does_not_infer_server_from_tool_name() {
        let registry = discovery_registry();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            globals: discovery_globals(registry.script_declarations(), registry.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
        let result = sandbox
            .execute(
                "const namespace = await describeNamespace('fixture'); const matches = await searchTools('documents', {namespace:'fixture',limit:1}); const hidden = await describeTool('hidden_probe'); const known = await describeTool('renamed_probe'); let rejected = 0; for (const options of [{limit:0},{limit:-1},{limit:1.5},{namespace:1},[]]) { try { await searchTools('documents', options); } catch { rejected++; } } return {namespace, matches:matches.map(tool => tool.name), hidden:hidden === undefined, known:known.includes('query'), rejected};",
                ExecuteOptions::default(),
            )
            .await;
        let ExecutionOutcome::Success { value, .. } = result.outcome else {
            panic!("discovery script failed");
        };
        assert_eq!(
            value,
            Some(
                json!({"namespace":{"name":"fixture","tools":["renamed_probe"]},"matches":["renamed_probe"],"hidden":true,"known":true,"rejected":5})
            )
        );
        sandbox.close().await;
    }
}
