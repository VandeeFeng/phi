use super::runtime::{
    MAX_OBSERVATIONS, ScriptOutcome, WorkflowAgentOptions, WorkflowError, WorkflowObservation,
    WorkflowRun, WorkflowRuntime, WorkflowScriptHost,
};
use phi_codemode::{
    CodemodeSandbox, CodemodeTool, ErrorKind, ExecuteOptions, ExecutionOutcome, ExecutionTimeout,
    SandboxLimits, SandboxOptions, SandboxSurface, ToolDefinition,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tree_sitter::Node;

const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_ENGINE_SOURCE_BYTES: usize = 8 * MAX_SOURCE_BYTES;
const COMPOSITION: &str = include_str!("script.js");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowMetadata {
    name: String,
    description: String,
    phases: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct WorkflowDefinition {
    metadata: WorkflowMetadata,
    body: String,
}

/// Validated source and captured JSON input; parsing executes no user script effects.
#[derive(Debug, Clone)]
pub struct WorkflowRequest {
    source: String,
    definition: WorkflowDefinition,
    args: Option<Value>,
}

impl WorkflowRequest {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn name(&self) -> &str {
        &self.definition.metadata.name
    }

    pub fn description(&self) -> &str {
        &self.definition.metadata.description
    }

    pub fn phases(&self) -> &[String] {
        &self.definition.metadata.phases
    }

    pub fn args(&self) -> Option<&Value> {
        self.args.as_ref()
    }

    pub async fn parse(source: &str, args: Option<Value>) -> Result<Self, WorkflowError> {
        if args
            .as_ref()
            .is_some_and(|value| value.to_string().len() > MAX_ARGUMENT_BYTES)
        {
            return Err(invalid("argument byte limit reached"));
        }
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            surface: SandboxSurface::HostOnly,
            limits: SandboxLimits {
                max_source_bytes: MAX_ENGINE_SOURCE_BYTES,
                ..Default::default()
            },
            ..Default::default()
        })
        .await?;
        let definition = WorkflowDefinition::parse(source, &sandbox).await;
        sandbox.close().await;
        Ok(Self {
            source: source.to_owned(),
            definition: definition?,
            args,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum HostRequest {
    Agent {
        prompt: String,
        #[serde(default)]
        options: WorkflowAgentOptions,
        phase: Option<String>,
    },
    Phase {
        title: String,
    },
    Log {
        message: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostResponse {
    Success { value: Option<String> },
    Operational { message: String },
    Fatal { message: String },
}

async fn host_request(host: &WorkflowScriptHost, args: Option<Value>) -> HostResponse {
    let request = serde_json::from_value::<HostRequest>(args.unwrap_or(Value::Null));
    let result = match request {
        Ok(HostRequest::Agent {
            prompt,
            options,
            phase,
        }) => host.request_agent(prompt, options, phase).await.map(Some),
        Ok(HostRequest::Phase { title }) if !title.trim().is_empty() => host
            .observe(WorkflowObservation::Phase(title))
            .await
            .map(|()| None),
        Ok(HostRequest::Log { message }) => host
            .observe(WorkflowObservation::Log(message))
            .await
            .map(|()| None),
        Ok(HostRequest::Phase { .. }) => Err(Arc::new(invalid("phase title is empty"))),
        Err(error) => Err(Arc::new(invalid(error.to_string()))),
    };
    match result {
        Ok(value) => HostResponse::Success { value },
        Err(error) if matches!(error.as_ref(), WorkflowError::Worker(_)) => {
            HostResponse::Operational {
                message: error.to_string(),
            }
        }
        Err(error) => HostResponse::Fatal {
            message: error.to_string(),
        },
    }
}

/// Prepared engine and creator scope. No run or script effects exist until start.
/// Dropping this value releases preparation without launching work.
pub struct WorkflowLaunch {
    id: super::runtime::WorkflowRunId,
    request: WorkflowRequest,
    scope: crate::harness::instance::execution::WorkflowScope,
    sandbox: CodemodeSandbox,
    binding: Arc<OnceLock<WorkflowScriptHost>>,
    code: String,
}

impl WorkflowLaunch {
    pub fn id(&self) -> super::runtime::WorkflowRunId {
        self.id
    }

    pub fn request(&self) -> &WorkflowRequest {
        &self.request
    }

    /// Install delivery ownership synchronously before the first script effect.
    pub fn start(
        self,
        runtime: &WorkflowRuntime,
        install: impl FnOnce(WorkflowRun),
    ) -> Result<WorkflowRun, Arc<WorkflowError>> {
        let (run, host, finished) = runtime.start_run(self.scope, self.request, self.id)?;
        let cancellation = host.cancellation();
        let _ = self.binding.set(host);
        install(run.clone());
        let sandbox = self.sandbox;
        let code = self.code;
        tokio::spawn(async move {
            let result = sandbox
                .execute(
                    &code,
                    ExecuteOptions {
                        cancellation,
                        timeout: ExecutionTimeout::After(WORKFLOW_TIMEOUT),
                        ..Default::default()
                    },
                )
                .await;
            sandbox.close().await;
            let outcome = match result.outcome {
                ExecutionOutcome::Success { value, .. } => ScriptOutcome::Returned(value),
                ExecutionOutcome::Failure(error) if error.kind == ErrorKind::Aborted => {
                    ScriptOutcome::Cancelled
                }
                ExecutionOutcome::Failure(error) => {
                    ScriptOutcome::Failed(Arc::new(WorkflowError::Script(error)))
                }
            };
            let _ = finished.send(outcome);
        });
        Ok(run)
    }
}

impl WorkflowRuntime {
    /// Launch with creator-bound authority. Dropping the returned handle does not stop the run.
    /// The caller must finalize this runtime before finalizing creator Sessions.
    pub async fn launch(
        &self,
        request: WorkflowRequest,
        creator: &crate::agent_instance::AgentInstance,
        default_role: Option<String>,
        limits: super::runtime::WorkflowLimits,
    ) -> Result<WorkflowRun, Arc<WorkflowError>> {
        self.prepare(request, creator, default_role, limits)
            .await?
            .start(self, |_| {})
    }

    /// Prepare without script effects, allowing durable input capture before start.
    pub async fn prepare(
        &self,
        request: WorkflowRequest,
        creator: &crate::agent_instance::AgentInstance,
        default_role: Option<String>,
        limits: super::runtime::WorkflowLimits,
    ) -> Result<WorkflowLaunch, Arc<WorkflowError>> {
        let scope = creator
            .capture_workflow_scope(default_role, limits)
            .await
            .map_err(Arc::new)?;
        let limits = scope.limits();
        let script_limits = json!({
            "concurrency": limits.concurrency(),
            "items": limits.total_invocations(),
            "outputBytes": limits.output_bytes(),
            "observations": MAX_OBSERVATIONS,
        });
        let composition = COMPOSITION.replace("WORKFLOW_LIMITS", &script_limits.to_string());
        let args = request.args.as_ref().map_or_else(
            || "undefined".to_owned(),
            |value| format!("JSON.parse({})", json!(value.to_string())),
        );
        let code = format!(
            "{composition}\nconst args = {args};\nlet result; try {{ result = await (async function(tools, console) {{ {}\n}})(undefined, undefined); }} finally {{ await flush(); }} return result;",
            request.definition.body
        );
        let binding = Arc::new(OnceLock::<WorkflowScriptHost>::new());
        let bound = binding.clone();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            surface: SandboxSurface::HostOnly,
            globals: vec![CodemodeTool::new(
                ToolDefinition {
                    name: "workflowHost".into(),
                    ..Default::default()
                },
                move |args, _| {
                    let bound = bound.clone();
                    async move {
                        let host = bound
                            .get()
                            .ok_or_else(|| "workflow host is not installed".to_owned())?;
                        serde_json::to_value(host_request(host, args).await)
                            .map(Some)
                            .map_err(|error| error.to_string())
                    }
                },
            )],
            limits: SandboxLimits {
                max_calls: limits.total_invocations() + MAX_OBSERVATIONS,
                max_pending_calls: limits.total_invocations() + MAX_OBSERVATIONS,
                max_source_bytes: MAX_ENGINE_SOURCE_BYTES,
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .map_err(|error| Arc::new(WorkflowError::Engine(error)))?;
        Ok(WorkflowLaunch {
            id: super::runtime::WorkflowRunId::new(),
            request,
            scope,
            sandbox,
            binding,
            code,
        })
    }
}

impl WorkflowDefinition {
    pub(super) async fn parse(
        source: &str,
        sandbox: &CodemodeSandbox,
    ) -> Result<Self, WorkflowError> {
        if source.len() > MAX_SOURCE_BYTES {
            return Err(invalid("source byte limit reached"));
        }
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .map_err(|error| invalid(error.to_string()))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| invalid("metadata parsing failed"))?;
        let root = tree.root_node();
        let mut cursor = root.walk();
        loop {
            if cursor.node().kind() == "import" {
                return Err(invalid("dynamic imports are not allowed"));
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node() == root {
                break;
            }
        }
        let export = children(root)
            .into_iter()
            .next()
            .ok_or_else(|| invalid("missing metadata declaration"))?;
        if export.kind() != "export_statement" || export.has_error() {
            return Err(invalid(
                "first statement must be export const meta = { ... }",
            ));
        }
        let declaration = export
            .child_by_field_name("declaration")
            .ok_or_else(|| invalid("missing metadata declaration"))?;
        if declaration.kind() != "lexical_declaration"
            || declaration
                .child_by_field_name("kind")
                .map(|node| text(node, source))
                != Some("const")
        {
            return Err(invalid("metadata must be a const declaration"));
        }
        let declarations = children(declaration);
        if declarations.len() != 1 {
            return Err(invalid("metadata must be the only exported declaration"));
        }
        let binding = declarations[0];
        let name = binding.child_by_field_name("name");
        if name.map(|node| (node.kind(), text(node, source))) != Some(("identifier", "meta")) {
            return Err(invalid("metadata binding must be named meta"));
        }
        let object = binding
            .child_by_field_name("value")
            .filter(|node| node.kind() == "object" && !node.has_error())
            .ok_or_else(|| invalid("metadata must be an object literal"))?;
        let mut name = None;
        let mut description = None;
        let mut phases = None;
        for property in children(object) {
            if property.kind() != "pair" {
                return Err(invalid("metadata permits only literal properties"));
            }
            let key = property.child_by_field_name("key").unwrap();
            let key = match key.kind() {
                "property_identifier" => text(key, source).to_owned(),
                "string" => literal_string(key, source)?,
                _ => return Err(invalid("metadata keys must be literal names")),
            };
            let value = property.child_by_field_name("value").unwrap();
            match key.as_str() {
                "name" if name.is_none() => name = Some(nonempty_string(value, source)?),
                "description" if description.is_none() => {
                    description = Some(nonempty_string(value, source)?);
                }
                "phases" if phases.is_none() && value.kind() == "array" => {
                    // Unnamed commas must not introduce array holes.
                    let mut cursor = value.walk();
                    let mut expecting_value = true;
                    let mut titles = Vec::new();
                    for child in value.children(&mut cursor) {
                        match child.kind() {
                            "[" | "]" | "comment" => (),
                            "," if !expecting_value => expecting_value = true,
                            "string" if expecting_value => {
                                titles.push(nonempty_string(child, source)?);
                                expecting_value = false;
                            }
                            _ => return Err(invalid("phases must be a literal string array")),
                        }
                    }
                    phases = Some(titles);
                }
                _ => {
                    return Err(invalid(
                        "unknown, duplicate or nonliteral metadata property",
                    ));
                }
            }
        }
        let metadata = WorkflowMetadata {
            name: name.ok_or_else(|| invalid("metadata name is required"))?,
            description: description.ok_or_else(|| invalid("metadata description is required"))?,
            phases: phases.unwrap_or_default(),
        };
        // Preserve source line numbers without treating metadata as executable code.
        let mut body = String::with_capacity(source.len());
        for character in source[..export.end_byte()].chars() {
            body.push(
                if matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                    character
                } else {
                    ' '
                },
            );
        }
        body.push_str(&source[export.end_byte()..]);
        // Constructing, never calling, an async function parses the exact async-body grammar.
        // JSON encoding prevents a body from escaping into this trusted preflight wrapper.
        let preflight = format!(
            "const AsyncFunction = Object.getPrototypeOf(async function() {{}}).constructor; new AsyncFunction('tools', 'console', {});",
            serde_json::to_string(&body).map_err(|error| invalid(error.to_string()))?
        );
        match sandbox
            .execute(&preflight, ExecuteOptions::default())
            .await
            .outcome
        {
            ExecutionOutcome::Success { .. } => Ok(Self { metadata, body }),
            ExecutionOutcome::Failure(error) => Err(WorkflowError::Script(error)),
        }
    }
}

fn invalid(message: impl Into<String>) -> WorkflowError {
    WorkflowError::InvalidRequest(message.into())
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    &source[node.byte_range()]
}

fn nonempty_string(node: Node<'_>, source: &str) -> Result<String, WorkflowError> {
    let value = literal_string(node, source)?;
    if value.trim().is_empty() {
        return Err(invalid("metadata strings must not be empty"));
    }
    Ok(value)
}

fn literal_string(node: Node<'_>, source: &str) -> Result<String, WorkflowError> {
    if node.kind() != "string" || node.has_error() {
        return Err(invalid("metadata values must be string literals"));
    }
    let literal = text(node, source);
    let mut characters = literal[1..literal.len() - 1].chars().peekable();
    let mut units = Vec::new();
    while let Some(character) = characters.next() {
        let character = if character == '\\' {
            match characters
                .next()
                .ok_or_else(|| invalid("incomplete string escape"))?
            {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'b' => '\u{0008}',
                'f' => '\u{000c}',
                'v' => '\u{000b}',
                '0' if characters.peek().is_none_or(|next| !next.is_ascii_digit()) => '\0',
                '\n' | '\u{2028}' | '\u{2029}' => continue,
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    continue;
                }
                escape @ ('x' | 'u') => {
                    let braced = escape == 'u' && characters.peek() == Some(&'{');
                    if braced {
                        characters.next();
                    }
                    let count = if escape == 'x' { 2 } else { 4 };
                    let mut digits = String::new();
                    if braced {
                        loop {
                            match characters.next() {
                                Some('}') if !digits.is_empty() => break,
                                Some(digit) if digit.is_ascii_hexdigit() && digits.len() < 6 => {
                                    digits.push(digit)
                                }
                                _ => return Err(invalid("invalid Unicode escape")),
                            }
                        }
                    } else {
                        for _ in 0..count {
                            digits.push(
                                characters
                                    .next()
                                    .filter(char::is_ascii_hexdigit)
                                    .ok_or_else(|| invalid("invalid string escape"))?,
                            );
                        }
                    }
                    let value = u32::from_str_radix(&digits, 16)
                        .map_err(|error| invalid(error.to_string()))?;
                    if !braced {
                        units.push(value as u16);
                        continue;
                    }
                    char::from_u32(value).ok_or_else(|| invalid("invalid Unicode code point"))?
                }
                digit if digit.is_ascii_digit() => {
                    return Err(invalid("legacy numeric escapes are unsupported"));
                }
                escaped => escaped,
            }
        } else {
            character
        };
        units.extend(character.encode_utf16(&mut [0; 2]).iter().copied());
    }
    String::from_utf16(&units).map_err(|_| invalid("unpaired Unicode surrogate in metadata"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_codemode::{CodemodeTool, SandboxOptions, ToolDefinition};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn preflight_parses_literals_without_running_effects() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let sandbox = CodemodeSandbox::new(SandboxOptions {
            globals: vec![CodemodeTool::new(
                ToolDefinition {
                    name: "effect".into(),
                    ..Default::default()
                },
                move |_, _| {
                    let calls = observed.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(None)
                    }
                },
            )],
            ..Default::default()
        })
        .await
        .unwrap();
        let definition = WorkflowDefinition::parse(r#"// comment
export const meta = {
    name: 'audit-\u0072outes', description: "check \uD83D\uDE80", phases: ['One', /* title */ 'Two',],
};
await effect(); return args;
"#, &sandbox).await.unwrap();
        assert_eq!(definition.metadata.name, "audit-routes");
        assert_eq!(definition.metadata.description, "check \u{1f680}");
        assert_eq!(definition.metadata.phases, ["One", "Two"]);
        assert!(definition.body.ends_with("await effect(); return args;\n"));
        assert_eq!(definition.body.lines().count(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for source in [
            "export const meta = { name: effect(), description: 'd' };",
            "export const meta = { name: 'n', get description() { effect(); } };",
            "export const meta = { name: 'n', description: 'd', ...effect() };",
            "export const meta = { name: 'n', description: 'd', phases: [, 'a'] };",
            "export const meta = { name: 'n', name: 'm', description: 'd' };",
            "export let meta = { name: 'n', description: 'd' };",
            "export const meta = { name: 'n', description: 'd' }, extra = effect();",
            "effect(); export const meta = { name: 'n', description: 'd' };",
            "export const meta = { name: 'n', description: 'd' }; await effect(); const = ;",
            "export const meta = { name: 'n', description: 'd' }; const tools = {};",
            "export const meta = { name: 'n', description: 'd' }; const console = {};",
            "export const meta = { name: 'n', description: 'd' }; break;",
            "export const meta = { name: 'n', description: 'd' }; await effect(); return import('module');",
            "export const meta = { name: 'n', description: 'd' }; if (false) { import('module'); }",
            "export const meta = { name: 'n', description: 'd' }; const load = () => import /* comment */ ('module');",
            "export const meta = { name: 'n', description: 'd' }; return `${await import('module')}`;",
            "export const meta = { name: 'n', description: 'd' }; return (import('module')).then(() => effect());",
            "export const meta = { name: 'n', description: 'd' }; export const other = 1;",
            "export const meta = { name: 'n', description: 'd' }; }); await effect(); (async () => {",
        ] {
            assert!(
                WorkflowDefinition::parse(source, &sandbox).await.is_err(),
                "{source}"
            );
        }
        for body in [
            "return \"import('module')\";",
            "// import('module')\nreturn 1;",
            "return /import\\('module'\\)/;",
            "return ({ import() { return 1; } }).import();",
            "return new.target;",
        ] {
            let source = format!("export const meta = {{ name: 'n', description: 'd' }}; {body}");
            WorkflowDefinition::parse(&source, &sandbox).await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        sandbox.close().await;
    }

    #[test]
    fn literal_metadata_round_trips_generated_strings() {
        use proptest::prelude::*;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let sandbox = runtime
            .block_on(CodemodeSandbox::new(SandboxOptions::default()))
            .unwrap();
        let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
            cases: 24,
            failure_persistence: None,
            ..Default::default()
        });
        runner.run(&proptest::collection::vec(any::<char>(), 0..64), |characters| {
            let expected = format!("name-{}", characters.into_iter().collect::<String>());
            let encoded = serde_json::to_string(&expected).unwrap();
            let source = format!(
                "export const meta = {{name: {encoded}, description: {encoded}, phases: [{encoded}]}}; return 1;"
            );
            let definition = runtime.block_on(WorkflowDefinition::parse(&source, &sandbox)).unwrap();
            prop_assert_eq!(&definition.metadata.name, &expected);
            prop_assert_eq!(&definition.metadata.description, &expected);
            prop_assert_eq!(&definition.metadata.phases, &vec![expected]);
            Ok(())
        }).unwrap();
        runtime.block_on(sandbox.close());
    }

    #[tokio::test]
    async fn malformed_metadata_and_source_limits_fail_closed() {
        let sandbox = CodemodeSandbox::new(SandboxOptions::default())
            .await
            .unwrap();
        for metadata in [
            "{}",
            "{name: '', description: 'd'}",
            "{name: 'n'}",
            "{name: 'n', description: 'd', extra: 1}",
            "{name: `n`, description: 'd'}",
            "{['name']: 'n', description: 'd'}",
            "{name: 'n', description: 'd', phases: ['one',,]}",
            "{name: 'n', description: 'd', phases: ['one', ...[]]}",
            "{name: 'n', description: 'd', phases: ['one'], phases: []}",
            r"{name: '\uD800', description: 'd'}",
            r"{name: '\08', description: 'd'}",
        ] {
            let source = format!("export const meta = {metadata}; return 1;");
            assert!(
                matches!(
                    WorkflowDefinition::parse(&source, &sandbox).await,
                    Err(WorkflowError::InvalidRequest(_))
                ),
                "{source}"
            );
        }
        let oversized = " ".repeat(MAX_SOURCE_BYTES + 1);
        assert!(matches!(
            WorkflowDefinition::parse(&oversized, &sandbox).await,
            Err(WorkflowError::InvalidRequest(_))
        ));
        sandbox.close().await;
        assert!(matches!(
            WorkflowDefinition::parse(
                "export const meta = {name: 'n', description: 'd'}; return 1;",
                &sandbox
            )
            .await,
            Err(WorkflowError::Script(_))
        ));
    }
}
