use super::AgenticLoop;
use super::response::tool_item_id;
use super::runner::RoundControl;
use crate::core::event::{AgentEvent, ToolResultDetails};
use crate::core::event::{NestedCallStatus, NestedToolCall};
use crate::core::lifecycle::{PermissionDecision, ToolFailureEvent, ToolResultEvent, ToolUseEvent};
use crate::extension::codemode::{CodemodeHost, NestedCallObserver};
use crate::hooks::{emit_post_tool_use, emit_post_tool_use_failure, emit_pre_tool_use};
use crate::tools::{
    ToolExecutionMode, ToolRegistry, ToolResultFinalizeContext, canonical_tool_name,
};
use crate::transcript::Transcript;
use compact_str::CompactString;
use futures::StreamExt;
use rig::completion::CompletionModel;
use rig::message::ToolCall;
use rig::tool::ToolOutput;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};

const TOOL_EXECUTION_CONCURRENCY: usize = 8;

enum PreparedToolCall {
    Immediate(ToolBatchItem),
    Ready {
        call: ToolCall,
        input: serde_json::Value,
        original_bash_command: Option<String>,
    },
}

pub(super) struct ToolBatchItem {
    call: ToolCall,
    entry_id: CompactString,
    input: serde_json::Value,
    output: ToolOutput,
    is_error: bool,
    terminate: bool,
    details: Option<ToolResultDetails>,
    script_value: Option<serde_json::Value>,
}

impl ToolBatchItem {
    pub(super) fn lifecycle_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": tool_item_id(&self.call),
            "entry_id": self.entry_id,
            "provider_call_id": self.call.provider.as_ref().map(|provider| provider.call_id.as_str()),
            "provider_item_id": self.call.provider.as_ref().and_then(|provider| provider.item_id.as_deref()),
            "tool_name": self.call.function.name,
            "input": self.input,
            "output": self.output.render(),
            "is_error": self.is_error,
            "details": self.details,
        })
    }

    pub(super) fn event(&self) -> AgentEvent {
        AgentEvent::ToolResult {
            id: CompactString::new(tool_item_id(&self.call)),
            entry_id: self.entry_id.clone(),
            output: CompactString::from(self.output.render()),
            is_error: self.is_error,
            details: self.details.clone(),
        }
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.is_error.then(|| self.output.render())
    }

    pub(super) fn terminates(&self) -> bool {
        self.terminate && !self.is_error
    }
}

impl<M> AgenticLoop<M>
where
    M: CompletionModel + Clone + 'static,
{
    pub(super) async fn execute_tool_batch(
        &self,
        tool_calls: &[ToolCall],
        events: &mpsc::Sender<AgentEvent>,
        control: &RoundControl,
    ) -> Vec<ToolBatchItem> {
        if self.runtime.is_directed()
            && (tool_calls.len() != 1 || !tool_calls[0].function.name.eq_ignore_ascii_case("Agent"))
        {
            let error =
                "directed delegation requires exactly one Agent call; no other tools were executed";
            return tool_calls
                .iter()
                .map(|call| {
                    tool_batch_item(
                        call,
                        Transcript::new_entry_id(),
                        &call.function.arguments,
                        ToolOutput::text(error),
                        true,
                        None,
                    )
                })
                .collect();
        }
        ToolExecutor {
            tools: self.runtime.tools().clone(),
            hooks: self.hooks.clone(),
            interaction_scope: self.interaction_scope.clone(),
            events: events.clone(),
            control: control.clone(),
            nested_gate: Arc::new(RwLock::new(())),
        }
        .execute_batch(tool_calls)
        .await
    }
}

#[derive(Clone)]
struct ToolExecutor {
    tools: ToolRegistry,
    hooks: crate::hooks::HookRegistry,
    interaction_scope: crate::core::event::CoreEventScope,
    events: mpsc::Sender<AgentEvent>,
    control: RoundControl,
    nested_gate: Arc<RwLock<()>>,
}

impl ToolExecutor {
    fn execute_nested(
        self,
        call: NestedToolCall,
        arguments: serde_json::Value,
        observer: NestedCallObserver,
        policy: Option<crate::permission::execution_scope::OuterTurnPolicy>,
    ) -> futures::future::BoxFuture<'static, Result<Option<serde_json::Value>, String>> {
        Box::pin(async move {
            let permit = self
                .events
                .clone()
                .reserve_owned()
                .await
                .map_err(|_| "tool event owner is unavailable".to_string())?;
            let mut guard = NestedCallGuard {
                permit: Some(permit),
                entry_id: Transcript::new_entry_id(),
                call,
                observer,
            };
            let _ = self
                .events
                .send(AgentEvent::NestedToolCall {
                    entry_id: None,
                    call: guard.call.clone(),
                })
                .await;
            let started = Instant::now();
            let mut scope = self.interaction_scope.clone();
            scope.parent_tool_call_id = Some(guard.call.parent_tool_call_id.clone().into());
            let item = crate::core::interaction::with_interaction_scope(
                scope,
                crate::permission::execution_scope::OuterTurnPolicy::with_captured_async(
                    policy,
                    async {
                        let _gate = if self.tools.execution_mode(&guard.call.name)
                            == ToolExecutionMode::Sequential
                        {
                            futures::future::Either::Left(self.nested_gate.write().await)
                        } else {
                            futures::future::Either::Right(self.nested_gate.read().await)
                        };
                        let tool_call = ToolCall::new(
                            rig::message::ToolCallId::new_or_mint(guard.call.id.clone()),
                            rig::message::ToolFunction::new(guard.call.name.clone(), arguments),
                        );
                        self.execute_prepared(
                            self.prepare_tool_call(&tool_call).await,
                            Some(&mut guard),
                        )
                        .await
                    },
                ),
            )
            .await;
            if guard.permit.is_some() {
                guard.record_result(&item, started);
            }
            if let Some(value) = item.script_value {
                Ok(Some(value))
            } else if !item.is_error
                && canonical_tool_name(&guard.call.name) == "Bash"
                && let Some(output) = item
                    .details
                    .as_ref()
                    .and_then(|details| details.output.as_ref())
            {
                Ok(Some(
                    bash_script_value(&item.output.render(), output, started.elapsed()).await,
                ))
            } else if item.is_error {
                Err(item.output.render())
            } else {
                Ok(Some(tool_output_value(&item.output)))
            }
        })
    }

    async fn execute_batch(&self, tool_calls: &[ToolCall]) -> Vec<ToolBatchItem> {
        let has_sequential_call = tool_calls.iter().any(|call| {
            self.tools.execution_mode(&call.function.name) == ToolExecutionMode::Sequential
        });

        if has_sequential_call {
            return self.execute_tool_batch_sequential(tool_calls).await;
        }

        self.execute_tool_batch_parallel(tool_calls).await
    }

    async fn execute_tool_batch_sequential(&self, tool_calls: &[ToolCall]) -> Vec<ToolBatchItem> {
        let mut results = Vec::with_capacity(tool_calls.len());
        for call in tool_calls {
            results.push(
                self.execute_prepared(self.prepare_tool_call(call).await, None)
                    .await,
            );
        }
        results
    }

    async fn execute_tool_batch_parallel(&self, tool_calls: &[ToolCall]) -> Vec<ToolBatchItem> {
        let mut prepared = Vec::with_capacity(tool_calls.len());
        for call in tool_calls {
            prepared.push(self.prepare_tool_call(call).await);
        }

        futures::stream::iter(prepared)
            .map(|call| self.execute_prepared(call, None))
            .buffered(TOOL_EXECUTION_CONCURRENCY)
            .collect()
            .await
    }

    async fn execute_prepared(
        &self,
        prepared: PreparedToolCall,
        nested: Option<&mut NestedCallGuard>,
    ) -> ToolBatchItem {
        match prepared {
            PreparedToolCall::Immediate(mut item) => {
                if let Some(nested) = nested {
                    item.entry_id = nested.entry_id.clone();
                }
                item
            }
            PreparedToolCall::Ready {
                call,
                input,
                original_bash_command,
            } => {
                let entry_id = nested
                    .as_ref()
                    .map(|nested| nested.entry_id.clone())
                    .unwrap_or_else(Transcript::new_entry_id);
                self.execute_prepared_tool_call(
                    call,
                    input,
                    original_bash_command,
                    entry_id,
                    nested,
                )
                .await
            }
        }
    }

    async fn prepare_tool_call(&self, call: &ToolCall) -> PreparedToolCall {
        let tool_name = call.function.name.clone();
        let tool_use_id = tool_item_id(call).to_string();
        let mut tool_input = call.function.arguments.clone();
        let original_bash_command = if tool_name.eq_ignore_ascii_case("bash") {
            tool_input
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        } else {
            None
        };

        let pre = emit_pre_tool_use(
            &self.hooks,
            ToolUseEvent {
                tool_name,
                tool_use_id,
                tool_input: tool_input.clone(),
            },
        )
        .await;

        if matches!(pre.permission_decision, Some(PermissionDecision::Deny)) {
            return PreparedToolCall::Immediate(tool_batch_item(
                call,
                Transcript::new_entry_id(),
                &tool_input,
                ToolOutput::text(
                    pre.reason
                        .unwrap_or_else(|| "Tool execution denied by hook".to_string()),
                ),
                true,
                None,
            ));
        }

        if let Some(updated_input) = pre.updated_input {
            tool_input = updated_input;
        }

        PreparedToolCall::Ready {
            call: call.clone(),
            input: tool_input,
            original_bash_command,
        }
    }

    async fn execute_prepared_tool_call(
        &self,
        call: ToolCall,
        tool_input: serde_json::Value,
        original_bash_command: Option<String>,
        entry_id: CompactString,
        nested: Option<&mut NestedCallGuard>,
    ) -> ToolBatchItem {
        let tool_name = call.function.name.clone();
        let started = Instant::now();
        let args = serde_json::to_string(&tool_input).unwrap_or_else(|_| tool_input.to_string());
        let mut context = rig::tool::ToolContext::new();
        match canonical_tool_name(&tool_name).as_str() {
            "ToolSearch" => {
                context.insert(super::search::ToolSearchHost {
                    tools: self.tools.clone(),
                    control: self.control.clone(),
                });
            }
            "Codemode" => {
                let executor = self.clone();
                let policy = crate::permission::execution_scope::OuterTurnPolicy::capture();
                let read_control = self.control.clone();
                let commit_control = self.control.clone();
                context.insert(CodemodeHost::new(
                    self.tools.clone(),
                    tool_item_id(&call).to_string(),
                    Arc::new(move |tools, call, arguments, observer| {
                        let mut executor = executor.clone();
                        executor.tools = tools;
                        executor.execute_nested(call, arguments, observer, policy.clone())
                    }),
                    Arc::new(move || {
                        let control = read_control.clone();
                        Box::pin(async move { control.codemode_store().await })
                    }),
                    Arc::new(move |writes| {
                        let control = commit_control.clone();
                        Box::pin(async move { control.commit_codemode_store(writes).await })
                    }),
                ));
            }
            _ => {}
        }
        let tool_result = if nested.is_some() && !self.tools.is_script_callable(&tool_name) {
            Err(format!("Tool {tool_name} is not callable from Codemode"))
        } else if nested.is_none()
            && self.tools.is_available(&tool_name)
            && !self.tools.is_declared(&tool_name)
        {
            Err(format!(
                "Tool {tool_name} is deferred; load it with ToolSearch or call it through Codemode"
            ))
        } else {
            crate::core::interaction::with_tool_call_scope(
                tool_item_id(&call).to_string(),
                crate::core::interaction::with_original_bash_command(
                    original_bash_command,
                    self.tools.call(&tool_name, &args, context),
                ),
            )
            .await
        };
        let mut item = match tool_result {
            Ok(result) => {
                let finalized = if nested.is_none() && result.result.is_success() {
                    self.tools.finalize_result(ToolResultFinalizeContext {
                        tool_name: &tool_name,
                        session_id: self.interaction_scope.session_id.as_ref(),
                        session_entry_id: &entry_id,
                    })
                } else {
                    Ok(())
                };
                match finalized {
                    Ok(()) => {
                        let mut item = tool_batch_item(
                            &call,
                            entry_id,
                            &tool_input,
                            result.result.output().clone(),
                            !result.result.is_success(),
                            result.details,
                        );
                        item.script_value = result.script_value;
                        item.terminate = !item.is_error && result.terminate_on_success;
                        item
                    }
                    Err(error) => {
                        tracing::warn!(tool = tool_name, "{error}");
                        tool_batch_item(
                            &call,
                            entry_id,
                            &tool_input,
                            ToolOutput::text(error),
                            true,
                            None,
                        )
                    }
                }
            }
            Err(error) => tool_batch_item(
                &call,
                entry_id,
                &tool_input,
                ToolOutput::text(error),
                true,
                None,
            ),
        };
        if let Some(nested) = nested {
            nested.record_result(&item, started);
        }
        let original_output = tool_output_value(&item.output);
        item.output = if item.is_error {
            self.emit_tool_failure(
                &tool_name,
                tool_item_id(&call),
                &tool_input,
                item.output,
                started,
            )
            .await
        } else {
            self.emit_tool_success(
                &tool_name,
                tool_item_id(&call),
                &tool_input,
                item.output,
                started,
            )
            .await
        };
        if tool_output_value(&item.output) != original_output {
            item.script_value = None;
            if let Some(details) = &mut item.details {
                details.output = None;
                details.images.clear();
                details.image_offsets.clear();
            }
        }
        item
    }

    async fn emit_tool_failure(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        tool_input: &serde_json::Value,
        error: ToolOutput,
        started: Instant,
    ) -> ToolOutput {
        let result = emit_post_tool_use_failure(
            &self.hooks,
            ToolFailureEvent {
                tool_name: tool_name.to_string(),
                tool_use_id: tool_use_id.to_string(),
                tool_input: tool_input.clone(),
                error: error.render(),
                is_interrupt: false,
                duration_ms: Some(started.elapsed().as_millis() as u64),
            },
        )
        .await;
        result
            .updated_output
            .map(tool_output_from_value)
            .unwrap_or(error)
    }

    async fn emit_tool_success(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        tool_input: &serde_json::Value,
        output: ToolOutput,
        started: Instant,
    ) -> ToolOutput {
        let result = emit_post_tool_use(
            &self.hooks,
            ToolResultEvent {
                tool_name: tool_name.to_string(),
                tool_use_id: tool_use_id.to_string(),
                tool_input: tool_input.clone(),
                tool_output: tool_output_value(&output),
                duration_ms: Some(started.elapsed().as_millis() as u64),
            },
        )
        .await;

        result
            .updated_output
            .map(tool_output_from_value)
            .unwrap_or(output)
    }
}

async fn bash_script_value(
    text: &str,
    details: &crate::core::event::ToolOutputDetails,
    duration: std::time::Duration,
) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    const MAX_SCRIPT_OUTPUT_BYTES: usize = 1024 * 1024;
    let captured = async {
        let path = details
            .full_output_path
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no output file"))?;
        let mut file = tokio::fs::File::open(path).await?;
        let length = file.metadata().await?.len();
        let mut bytes = Vec::new();
        if length <= MAX_SCRIPT_OUTPUT_BYTES as u64 {
            file.take(MAX_SCRIPT_OUTPUT_BYTES as u64)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>((String::from_utf8_lossy(&bytes).into_owned(), false))
        } else {
            let half = MAX_SCRIPT_OUTPUT_BYTES / 2;
            (&mut file)
                .take(half as u64)
                .read_to_end(&mut bytes)
                .await?;
            let head = String::from_utf8_lossy(&bytes).into_owned();
            bytes.clear();
            file.seek(std::io::SeekFrom::End(-(half as i64))).await?;
            file.take(half as u64).read_to_end(&mut bytes).await?;
            Ok((
                format!(
                    "{head}\n… output omitted …\n{}",
                    String::from_utf8_lossy(&bytes)
                ),
                true,
            ))
        }
    }
    .await;
    let (output, truncated) = captured.unwrap_or_else(|_| {
        let suffix = format!("\nExit code: {}", details.exit_code);
        let output = if details.total_bytes == 0 {
            ""
        } else if details.exit_code != 0 {
            text.strip_suffix(&suffix).unwrap_or(text)
        } else {
            text
        };
        (output.to_string(), details.truncated)
    });
    serde_json::json!({ "output": output, "truncated": truncated, "full_output_path": details.full_output_path, "exit_code": details.exit_code, "wall_time_seconds": duration.as_secs_f64() })
}

struct NestedCallGuard {
    permit: Option<mpsc::OwnedPermit<AgentEvent>>,
    entry_id: CompactString,
    call: NestedToolCall,
    observer: NestedCallObserver,
}

impl NestedCallGuard {
    fn record_result(&mut self, item: &ToolBatchItem, started: Instant) {
        self.call.duration_ms = started.elapsed().as_millis() as u64;
        self.call.status = if item.is_error {
            NestedCallStatus::Error
        } else {
            NestedCallStatus::Ok
        };
        self.call.error = item
            .is_error
            .then(|| item.output.render().chars().take(500).collect());
        self.call.details = None;
        self.finish();
    }

    fn finish(&mut self) {
        (self.observer)(&self.call);
        if let Some(permit) = self.permit.take() {
            permit.send(AgentEvent::NestedToolCall {
                entry_id: Some(self.entry_id.clone()),
                call: self.call.clone(),
            });
        }
    }
}

impl Drop for NestedCallGuard {
    fn drop(&mut self) {
        if self.permit.is_some() {
            self.call.status = NestedCallStatus::Cancelled;
            self.finish();
        }
    }
}

fn tool_batch_item(
    call: &ToolCall,
    entry_id: CompactString,
    input: &serde_json::Value,
    output: ToolOutput,
    is_error: bool,
    details: Option<ToolResultDetails>,
) -> ToolBatchItem {
    ToolBatchItem {
        call: call.clone(),
        entry_id,
        input: input.clone(),
        output,
        is_error,
        terminate: false,
        details,
        script_value: None,
    }
}

fn tool_output_value(output: &ToolOutput) -> serde_json::Value {
    if let Some(value) = output.as_json() {
        value.clone()
    } else if let Some(text) = output.as_text() {
        serde_json::Value::String(text.to_owned())
    } else {
        serde_json::to_value(output.as_content()).unwrap_or_else(|_| output.render().into())
    }
}

fn tool_output_from_value(value: serde_json::Value) -> ToolOutput {
    match value {
        serde_json::Value::String(text) => ToolOutput::text(text),
        value => ToolOutput::json(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::CoreEventScope;
    use crate::core::lifecycle::PostToolUseOutcome;
    use crate::hooks::{Hook, HookRegistry};
    use crate::tools::{
        ToolAuthorization, ToolCatalogBuilder, ToolMetadata, ToolOwner, ToolPromptMetadata,
        ToolRegistration,
    };
    use crate::transcript::TranscriptEntryKind;
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::{Barrier, Notify, oneshot};

    fn effect_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
        let effects = Arc::new(AtomicUsize::new(0));
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        catalog.register(ToolRegistration::phi_dynamic(
            rig::tool::DynamicTool::new(
                "effect",
                "Record an external effect",
                json!({"type":"object"}),
                {
                    let effects = effects.clone();
                    move |_context, args| {
                        effects.fetch_add(1, Ordering::SeqCst);
                        Box::pin(async move { Ok(ToolOutput::json(args)) })
                    }
                },
            ),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default()
                .with_exposure(crate::tools::ToolExposure::Deferred)
                .with_authorization(ToolAuthorization::Invocation),
        ));
        (catalog.build().unwrap(), effects)
    }

    fn script_pipeline(
        tools: ToolRegistry,
        hooks: HookRegistry,
        code: &str,
    ) -> (
        super::super::EventPipeline,
        oneshot::Receiver<ToolBatchItem>,
    ) {
        let (events, event_rx) = mpsc::channel(256);
        let (control, requests) = RoundControl::channel();
        let executor = ToolExecutor {
            tools,
            hooks,
            events,
            control,
            interaction_scope: CoreEventScope::default(),
            nested_gate: Arc::new(RwLock::new(())),
        };
        let call = ToolCall::new(
            rig::message::ToolCallId::new_or_mint("script"),
            rig::message::ToolFunction::new("Codemode".into(), json!({"code":code})),
        );
        let (reply, result) = oneshot::channel();
        let task = tokio::spawn(async move {
            let item = executor.execute_batch(&[call]).await.remove(0);
            let _ = reply.send(item);
        });
        let runner = super::super::AgentRunner {
            task,
            event_rx,
            round_requests: Some(requests),
            tool_policy: None,
            steering: Arc::new(super::super::runner::AgentSteerMailbox::new(
                crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
            )),
        };
        (
            super::super::EventPipeline::new(runner, CoreEventScope::default()),
            result,
        )
    }

    async fn finish_script(
        pipeline: &mut super::super::EventPipeline,
        result: &mut oneshot::Receiver<ToolBatchItem>,
        transcript: &mut Transcript,
    ) -> ToolBatchItem {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                pipeline.progress_round(transcript, &crate::hooks::HookRegistry::new(), |_| {
                    Some(Box::pin(async {
                        Ok(super::super::CompletionDecision::Finish)
                    }))
                });
                pipeline.drain(transcript).await;
                if let Ok(item) = result.try_recv() {
                    pipeline.drain(transcript).await;
                    return item;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn terminating_tool_only_skips_follow_up_for_successful_batches() {
        let mut catalog = ToolCatalogBuilder::new(false);
        for name in ["launch", "ordinary"] {
            let tool = crate::tools::ToolDeclaration::dynamic(rig::tool::PortableDynamicTool::new(
                name,
                "Test result",
                json!({"type": "object"}),
                |args| {
                    Box::pin(async move {
                        if args["fail"] == true {
                            return Err(rig::tool::ToolExecutionError::provider("launch failed"));
                        }
                        Ok(ToolOutput::text("accepted"))
                    })
                },
            ));
            catalog.register(if name == "launch" {
                tool.terminate_on_success().registration()
            } else {
                tool.registration()
            });
        }
        catalog.register(ToolRegistration::phi_dynamic(
            rig::tool::DynamicTool::new(
                "conditional",
                "Conditional turn end",
                json!({"type":"object"}),
                |context, arguments| {
                    Box::pin(async move {
                        if arguments["terminate"] == true {
                            context.insert_result(crate::tools::ToolTurnEnd);
                        }
                        if arguments["fail"] == true {
                            return Err(rig::tool::ToolExecutionError::provider("launch failed"));
                        }
                        Ok(ToolOutput::text("accepted"))
                    })
                },
            ),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        let (events, _event_rx) = mpsc::channel(16);
        let executor = ToolExecutor {
            tools: catalog.build().unwrap(),
            hooks: HookRegistry::default(),
            events,
            control: RoundControl::channel().0,
            interaction_scope: CoreEventScope::default(),
            nested_gate: Arc::new(RwLock::new(())),
        };
        let call = |name: &str, arguments| {
            ToolCall::new(
                rig::message::ToolCallId::new_or_mint(uuid::Uuid::new_v4().to_string()),
                rig::message::ToolFunction::new(name.into(), arguments),
            )
        };
        for name in ["launch", "conditional"] {
            let arguments = json!({"terminate":true});
            let launch = executor
                .execute_batch(&[call(name, arguments.clone())])
                .await;
            assert!(launch[0].terminates());
            assert!(!launch[0].is_error);
            let failed = executor
                .execute_batch(&[call(name, json!({"terminate":true, "fail":true}))])
                .await;
            assert!(failed[0].is_error);
            assert!(!failed[0].terminates());
            for peer in ["ordinary", "conditional", "missing"] {
                let mixed = executor
                    .execute_batch(&[call(name, arguments.clone()), call(peer, json!({}))])
                    .await;
                assert_eq!(mixed.len(), 2);
                assert!(!mixed.iter().all(ToolBatchItem::terminates));
                assert_eq!(mixed[1].is_error, peer == "missing");
            }
        }
    }

    #[tokio::test]
    async fn codemode_deferred_tools_require_nested_entry_and_remain_discoverable() {
        let (tools, effects) = effect_registry();
        let declared = tools.provider_declarations();
        assert_eq!(declared.len(), 1);
        assert!(!declared[0].description.contains("effect(args:"));
        let (mut pipeline, mut result) = script_pipeline(
            tools.clone(),
            HookRegistry::default(),
            "const found = await searchTools('external effect'); const known = await describeTool('effect'); if (!ALL_TOOLS.some(tool => tool.name === 'effect') || !known.includes('effect(args:')) throw new Error('missing deferred tool'); return await tools[found[0].name]({evidence:'discovered'});",
        );
        let mut transcript = Transcript::new();
        let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
        assert!(!item.is_error, "{}", item.output.render());
        assert!(item.output.render().contains("discovered"));
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        let (events, _event_rx) = mpsc::channel(16);
        let executor = ToolExecutor {
            tools: tools.clone(),
            hooks: HookRegistry::default(),
            events,
            control: RoundControl::channel().0,
            interaction_scope: CoreEventScope::default(),
            nested_gate: Arc::new(RwLock::new(())),
        };
        let direct = ToolCall::new(
            rig::message::ToolCallId::new_or_mint("direct"),
            rig::message::ToolFunction::new("effect".into(), json!({})),
        );
        let denied = executor.execute_batch(&[direct]).await.remove(0);
        assert!(denied.is_error);
        assert!(denied.output.render().contains("deferred"));
        let (mut pipeline, mut result) = script_pipeline(
            tools.with_policy(None, vec!["effect".into()]),
            HookRegistry::default(),
            "if ((await searchTools('external effect')).length || (await describeTool('effect')) || ALL_TOOLS.some(tool => tool.name === 'effect')) throw new Error('removed tool leaked'); try { await tools.effect({}); } catch { return 'unavailable'; } throw new Error('removed tool ran');",
        );
        let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
        assert!(!item.is_error, "{}", item.output.render());
        assert!(item.output.render().contains("unavailable"));
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn codemode_discovers_cold_deferred_mcp_without_changing_provider_declarations() {
        use crate::extension::mcp::McpClientManager;
        let root =
            std::path::Path::new("/tmp").join(format!("phi-deferred-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let server = root.join("server.py");
        let effects = root.join("effects");
        std::fs::write(&server, r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request.get('method')
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':name,'description':'deferred fixture operation','inputSchema':{'type':'object'}} for name in ['probe','blocked']]}
    elif method in ['resources/list','prompts/list']:
        result = {method.split('/')[0]:[]}
    elif method == 'tools/call':
        with open(sys.argv[1], 'a') as target:
            target.write(request['params']['name'] + '\n')
        result = {'content':[{'type':'text','text':'deferred-result'}]}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'method not found'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#).unwrap();
        let configs = serde_json::from_value(json!({"fixture":{
            "command":"python3", "args":[server, effects], "exposure":"deferred",
            "toolExposure":{"blocked":"hidden"}
        }}))
        .unwrap();
        let manager = McpClientManager::load_with_settings(&configs, Default::default());
        let (snapshot, _) = manager.snapshot_tool_contracts().await;
        assert!(manager.tool_refresh_candidates().await.is_empty());
        assert!(!manager.server_snapshots().await[0].status.connected);
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        for registration in snapshot.tool_registrations(None).await {
            catalog.register(registration.into());
        }
        let registry = catalog.build().unwrap().with_mcp_manager(Some(snapshot));
        let declarations = serde_json::to_value(registry.provider_declarations()).unwrap();
        assert!(!registry.is_script_callable("fixture_probe"));
        let (mut pipeline, mut result) = script_pipeline(
            registry.clone(),
            HookRegistry::default(),
            "if (ALL_TOOLS.some(tool => tool.name === 'fixture_blocked') || await describeTool('fixture_blocked')) throw new Error('hidden target exposed'); const found = (await searchTools('deferred fixture')).find(tool => tool.name === 'fixture_probe'); if (!found) throw new Error('cold target missing'); return await tools[found.name]({});",
        );
        let item = finish_script(&mut pipeline, &mut result, &mut Transcript::new()).await;
        assert!(!item.is_error, "{}", item.output.render());
        assert!(item.output.render().contains("deferred-result"));
        assert_eq!(std::fs::read_to_string(&effects).unwrap(), "probe\n");
        assert_eq!(
            serde_json::to_value(registry.provider_declarations()).unwrap(),
            declarations
        );
        assert!(!registry.is_script_callable("fixture_probe"));
        assert!(manager.server_snapshots().await[0].status.connected);
        registry
            .outer_turn_policy(crate::core::turn::TurnToolPolicy {
                removals: vec!["fixture_probe".into()],
                ..Default::default()
            })
            .run(async {
                let scoped = registry.discovery_registry().await.unwrap();
                assert!(!scoped.is_script_callable("fixture_probe"));
                assert!(
                    scoped
                        .call("fixture_probe", "{}", rig::tool::ToolContext::new())
                        .await
                        .is_err()
                );
            })
            .await;
        assert_eq!(std::fs::read_to_string(&effects).unwrap(), "probe\n");
        manager.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn codemode_scheduling_overlaps_parallel_and_excludes_sequential() {
        let active = Arc::new(Mutex::new((0, 0)));
        let peak = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        for mode in [ToolExecutionMode::Parallel, ToolExecutionMode::Sequential] {
            let active = active.clone();
            let peak = peak.clone();
            let barrier = barrier.clone();
            let name = match mode {
                ToolExecutionMode::Parallel => "parallel",
                ToolExecutionMode::Sequential => "sequential",
            };
            let metadata = match mode {
                ToolExecutionMode::Parallel => ToolMetadata::default(),
                ToolExecutionMode::Sequential => {
                    ToolMetadata::sequential(ToolPromptMetadata::new("exclusive execution"))
                }
            };
            catalog.register(ToolRegistration::phi_dynamic(
                rig::tool::DynamicTool::new(
                    name,
                    "Check execution admission",
                    json!({"type":"object","properties":{"pair":{"type":"boolean"}}}),
                    move |_context, args| {
                        let active = active.clone();
                        let peak = peak.clone();
                        let barrier = barrier.clone();
                        Box::pin(async move {
                            {
                                let mut active = active.lock().unwrap();
                                assert_eq!(active.1, 0, "sequential call overlapped another call");
                                match mode {
                                    ToolExecutionMode::Parallel => active.0 += 1,
                                    ToolExecutionMode::Sequential => {
                                        assert_eq!(
                                            active.0, 0,
                                            "parallel call overlapped sequential"
                                        );
                                        active.1 += 1;
                                    }
                                }
                                peak.fetch_max(active.0, Ordering::SeqCst);
                            }
                            if args["pair"] == true {
                                barrier.wait().await;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                            let mut active = active.lock().unwrap();
                            match mode {
                                ToolExecutionMode::Parallel => active.0 -= 1,
                                ToolExecutionMode::Sequential => active.1 -= 1,
                            }
                            Ok(ToolOutput::text("done"))
                        })
                    },
                ),
                "test",
                ToolOwner::Phi,
                metadata.with_authorization(ToolAuthorization::Invocation),
            ));
        }
        let tools = catalog.build().unwrap();
        let mut scripts = vec![
            "await Promise.all([tools.parallel({pair:true}), tools.parallel({pair:true})]);"
                .to_string(),
        ];
        for schedule in 0..8 {
            let calls = (0..3)
                .map(|index| {
                    let name = if schedule & (1 << index) == 0 {
                        "parallel"
                    } else {
                        "sequential"
                    };
                    format!("tools.{name}({{}})")
                })
                .collect::<Vec<_>>()
                .join(",");
            scripts.push(format!("await Promise.all([{calls}]);"));
        }
        for code in scripts {
            let (mut pipeline, mut result) =
                script_pipeline(tools.clone(), HookRegistry::default(), &code);
            let mut transcript = Transcript::new();
            let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
            assert!(!item.is_error, "{code}: {}", item.output.render());
            assert_eq!(*active.lock().unwrap(), (0, 0));
        }
        assert!(peak.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn codemode_unawaited_calls_stop_before_store_commit_and_parent_result() {
        struct ActiveCall(Arc<AtomicUsize>);
        impl Drop for ActiveCall {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        catalog.register(ToolRegistration::phi_dynamic(
            rig::tool::DynamicTool::new(
                "wait",
                "Wait until cancelled",
                json!({"type":"object"}),
                {
                    let active = active.clone();
                    let entered = entered.clone();
                    move |_context, _args| {
                        let active = active.clone();
                        let entered = entered.clone();
                        Box::pin(async move {
                            active.fetch_add(1, Ordering::SeqCst);
                            let _active = ActiveCall(active);
                            entered.notify_one();
                            std::future::pending().await
                        })
                    }
                },
            ),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        catalog.register(ToolRegistration::phi_dynamic(
            rig::tool::DynamicTool::new(
                "started",
                "Wait for tool admission",
                json!({"type":"object"}),
                move |_context, _args| {
                    let entered = entered.clone();
                    Box::pin(async move {
                        entered.notified().await;
                        Ok(ToolOutput::text("started"))
                    })
                },
            ),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        let (mut pipeline, mut result) = script_pipeline(
            catalog.build().unwrap(),
            HookRegistry::default(),
            "tools.wait({}); await tools.started({}); store('done', 1); return 'finished';",
        );
        let mut transcript = Transcript::new();
        let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
        assert!(!item.is_error, "{}", item.output.render());
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            crate::extension::codemode::read_store(&transcript)["done"],
            1
        );
        let calls = item.details.unwrap().nested_calls.unwrap();
        assert!(calls.complete);
        assert_eq!(calls.calls.len(), 2);
        assert!(matches!(
            calls
                .calls
                .iter()
                .find(|call| call.name == "wait")
                .unwrap()
                .status,
            NestedCallStatus::Cancelled
        ));
        let records: Vec<_> = transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                TranscriptEntryKind::Event { name, data }
                    if name == "nested_tool_call" && data["name"] == "wait" =>
                {
                    Some(data)
                }
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["status"], "cancelled");
    }

    #[tokio::test]
    async fn codemode_excludes_tape_mutations_and_durable_tools() {
        struct DurableFinalizer;
        impl crate::tools::ToolResultFinalizer for DurableFinalizer {
            fn finalize(
                &self,
                _: crate::tools::ToolResultFinalizeContext<'_>,
            ) -> Result<(), String> {
                Ok(())
            }
        }
        let forbidden = [
            "tape_anchor",
            "tape_delete",
            "tape_reset",
            "renamed_mutation",
            "durable",
        ];
        let readable = ["memory_search", "memory_check", "tape_info", "tape_read"];
        let effects = Arc::new(AtomicUsize::new(0));
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        for name in forbidden.into_iter().chain(readable) {
            let authorization = if forbidden.contains(&name) && name != "durable" {
                ToolAuthorization::TapeMutation
            } else {
                ToolAuthorization::Invocation
            };
            let metadata = ToolMetadata::default().with_authorization(authorization);
            let effects = effects.clone();
            let mut registration = ToolRegistration::phi_dynamic(
                rig::tool::DynamicTool::new(
                    name,
                    "Probe script eligibility",
                    json!({"type":"object"}),
                    move |_context, _args| {
                        let effects = effects.clone();
                        Box::pin(async move {
                            effects.fetch_add(1, Ordering::SeqCst);
                            Ok(ToolOutput::json(json!({"found":true})))
                        })
                    },
                ),
                "test",
                ToolOwner::Phi,
                metadata,
            );
            if name == "durable" {
                registration = registration.with_finalizer(Arc::new(DurableFinalizer));
            }
            catalog.register(registration);
        }
        let tools = catalog.build().unwrap();
        let provider = tools.provider_declarations();
        assert!(
            forbidden
                .iter()
                .all(|name| provider.iter().any(|tool| tool.name == *name))
        );
        let code = format!(
            "const forbidden = {}; const readable = {};\n\
             for (const name of forbidden) {{\n\
               if (name in tools || ALL_TOOLS.some(tool => tool.name === name) || await describeTool(name) !== undefined) throw new Error('mutation exposed: ' + name);\n\
               if ((await searchTools(name, {{limit:100}})).some(tool => tool.name === name)) throw new Error('mutation discoverable: ' + name);\n\
             }}\n\
             for (const name of readable) {{\n\
               if (!ALL_TOOLS.some(tool => tool.name === name) || !await describeTool(name)) throw new Error('read omitted: ' + name);\n\
               if (!(await tools[name]({{}})).found) throw new Error('read failed: ' + name);\n\
             }}\n\
             return 'read-only memory';",
            json!(forbidden),
            json!(readable),
        );
        let (mut pipeline, mut result) =
            script_pipeline(tools.clone(), HookRegistry::default(), &code);
        let mut transcript = Transcript::new();
        let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
        assert!(!item.is_error, "{}", item.output.render());
        assert_eq!(effects.load(Ordering::SeqCst), readable.len());

        let (events, _event_rx) = mpsc::channel(256);
        let executor = ToolExecutor {
            tools,
            hooks: HookRegistry::default(),
            interaction_scope: CoreEventScope::default(),
            events,
            control: RoundControl::channel().0,
            nested_gate: Arc::new(RwLock::new(())),
        };
        for name in forbidden {
            let call = NestedToolCall {
                id: format!("script/{name}"),
                parent_tool_call_id: "script".into(),
                name: name.into(),
                status: NestedCallStatus::Running,
                arguments: Some(json!({})),
                duration_ms: 0,
                error: None,
                details: None,
            };
            let error = executor
                .clone()
                .execute_nested(call, json!({}), Arc::new(|_| {}), None)
                .await
                .unwrap_err();
            assert!(error.contains("not callable from Codemode"), "{error}");
        }
        assert_eq!(
            effects.load(Ordering::SeqCst),
            readable.len(),
            "nested admission must reject before effects"
        );
        let direct: Vec<_> = forbidden
            .into_iter()
            .map(|name| {
                ToolCall::new(
                    rig::message::ToolCallId::new_or_mint(name),
                    rig::message::ToolFunction::new(name.into(), json!({})),
                )
            })
            .collect();
        assert!(
            executor
                .execute_batch(&direct)
                .await
                .iter()
                .all(|item| !item.is_error)
        );
        assert_eq!(
            effects.load(Ordering::SeqCst),
            readable.len() + forbidden.len()
        );
    }

    #[test]
    fn nested_audit_omits_full_result_details() {
        let observed = Arc::new(Mutex::new(None));
        let (events, mut receiver) = mpsc::channel(1);
        let mut guard = NestedCallGuard {
            permit: Some(events.try_reserve_owned().unwrap()),
            entry_id: Transcript::new_entry_id(),
            observer: Arc::new({
                let observed = observed.clone();
                move |call| *observed.lock().unwrap() = Some(call.clone())
            }),
            call: NestedToolCall {
                id: "parent/1".into(),
                parent_tool_call_id: "parent".into(),
                name: "Write".into(),
                status: NestedCallStatus::Running,
                arguments: None,
                duration_ms: 0,
                error: None,
                details: None,
            },
        };
        let call = ToolCall::new(
            rig::message::ToolCallId::new_or_mint("parent/1"),
            rig::message::ToolFunction::new("Write".into(), json!({})),
        );
        let item = tool_batch_item(
            &call,
            Transcript::new_entry_id(),
            &json!({}),
            ToolOutput::text("done"),
            false,
            Some(ToolResultDetails {
                diff: Some(crate::core::event::ToolDiffDetails {
                    hunks: vec![],
                    patch: Some("private diff".repeat(60_000)),
                    first_changed_line: Some(1),
                }),
                ..Default::default()
            }),
        );
        guard.record_result(&item, Instant::now());
        let AgentEvent::NestedToolCall { call, .. } = receiver.try_recv().unwrap() else {
            panic!("missing audit");
        };
        assert!(call.details.is_none());
        assert!(serde_json::to_vec(&call).unwrap().len() < 1024);
        assert_eq!(
            serde_json::to_value(observed.lock().unwrap().as_ref().unwrap()).unwrap(),
            serde_json::to_value(&call).unwrap()
        );
    }

    #[tokio::test]
    async fn codemode_post_hooks_invalidate_raw_results() {
        let paths = [7, 60_000].map(|length| {
            let path = std::env::temp_dir().join(format!("phi-hook-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, "X".repeat(length)).unwrap();
            path
        });
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        for name in ["Bash", "protocol", "plain"] {
            catalog.register(ToolRegistration::phi_dynamic(
                rig::tool::DynamicTool::new(name, "Raw result", json!({"type":"object"}), {
                    let paths = paths.clone();
                    move |context, arguments| {
                        let path = paths[usize::from(arguments["large"] == true)].clone();
                        Box::pin(async move {
                            if name == "Bash" {
                                context.insert_result(ToolResultDetails {
                                    output: Some(crate::core::event::ToolOutputDetails {
                                        total_bytes: std::fs::metadata(&path).unwrap().len(),
                                        total_lines: 1,
                                        truncated: arguments["large"] == true,
                                        full_output_path: Some(path),
                                        exit_code: 0,
                                    }),
                                    ..Default::default()
                                });
                            } else if name == "protocol" {
                                context.insert_result(crate::tools::ScriptToolResult(
                                    json!({"structuredContent":"PRIVATE"}),
                                ));
                            }
                            Ok(ToolOutput::text("PRIVATE"))
                        })
                    }
                }),
                "test",
                ToolOwner::Phi,
                ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
            ));
        }
        let tools = catalog.build().unwrap();
        let mut hooks = HookRegistry::default();
        hooks.register_hook(Hook::post_tool_use(|event| async move {
            Ok(if event.tool_input["rewrite"] == true {
                PostToolUseOutcome::Rewrite {
                    tool_output: if event.tool_name == "plain" {
                        json!({"hooked":true})
                    } else {
                        json!("REDACTED")
                    },
                }
            } else {
                PostToolUseOutcome::Continue
            })
        }));
        let (mut pipeline, mut result) = script_pipeline(
            tools,
            hooks,
            r#"
for (const large of [false, true]) {
    for (const name of ['Bash', 'protocol', 'plain']) {
        for (const rewrite of [false, true]) {
            const value = await tools[name]({large, rewrite});
            if (rewrite) {
                if (name === 'plain') {
                    if (!value.hooked) throw new Error('plain rewrite lost');
                } else if (value !== 'REDACTED') {
                    throw new Error(name + ': stale raw output');
                }
            } else if (name === 'Bash') {
                if (value.output.length !== (large ? 60000 : 7)) throw new Error('bash output lost');
            } else if (name === 'protocol') {
                if (value.structuredContent !== 'PRIVATE') throw new Error('protocol output lost');
            } else if (value !== 'PRIVATE') {
                throw new Error('plain output lost');
            }
        }
    }
}
return 'safe';
"#,
        );
        let item = finish_script(&mut pipeline, &mut result, &mut Transcript::new()).await;
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
        assert!(!item.is_error, "{}", item.output.render());
        let calls = item.details.unwrap().nested_calls.unwrap();
        assert!(calls.complete);
        assert_eq!(calls.calls.len(), 12);
    }

    #[tokio::test]
    async fn codemode_preserves_output_and_reuses_images_through_history() {
        let (tools, _) = effect_registry();
        for max_output_tokens in [60, 10_000] {
            for ending in ["return 'done';", "throw new Error('failed');"] {
                let code = format!(
                    "// @options: {{\"max_output_tokens\":{max_output_tokens}}}\ntext('before界'); image('data:image/png;base64,iVBORw0KGgo='); text('after'); image('data:image/png;base64,iVBORw0KGgo='); text('中'.repeat(1000)); console.log('console-evidence'); {ending}"
                );
                let (mut pipeline, mut result) =
                    script_pipeline(tools.clone(), HookRegistry::default(), &code);
                let mut transcript = Transcript::new();
                let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
                assert_eq!(item.is_error, ending.starts_with("throw"));
                let output = item.output.render();
                let truncated = max_output_tokens == 60;
                assert_eq!(output.contains("output truncated"), truncated);
                assert!(output.contains(if item.is_error {
                    "Tool effects are not undone."
                } else {
                    "console-evidence"
                }));
                let output_path = output.lines().find_map(|line| {
                    line.strip_prefix("[Full output: ")
                        .and_then(|line| line.strip_suffix(']'))
                });
                assert_eq!(output_path.is_some(), truncated);
                let image_paths = output
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("[Image saved to ")
                            .and_then(|line| line.strip_suffix(']'))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(image_paths.len(), 2);
                assert_eq!(image_paths[0], image_paths[1]);
                let image_path = image_paths[0];
                let full = output_path
                    .map(|path| std::fs::read_to_string(path).unwrap())
                    .unwrap_or_else(|| output.clone());
                assert!(full.contains(&"中".repeat(1000)));
                assert!(full.contains("console-evidence"));
                assert_eq!(std::fs::read(image_path).unwrap(), b"\x89PNG\r\n\x1a\n");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    for path in output_path.into_iter().chain(std::iter::once(image_path)) {
                        assert_eq!(
                            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                            0o600
                        );
                    }
                }
                crate::core::reducer::reduce_agent_event(
                    &mut transcript,
                    &mut crate::core::reducer::CoreRunState::default(),
                    item.event(),
                );
                let restored: Transcript =
                    serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
                let content = restored
                    .entries
                    .iter()
                    .find_map(|entry| match &entry.kind {
                        TranscriptEntryKind::Message {
                            message: crate::transcript::AgentMessage::ToolResult { content, .. },
                        } => Some(content),
                        _ => None,
                    })
                    .unwrap();
                let image_positions = content
                    .iter()
                    .enumerate()
                    .filter_map(|(index, block)| {
                        matches!(block, crate::transcript::ContentBlock::Image { .. })
                            .then_some(index)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(image_positions.len(), 2);
                if !truncated {
                    let first_image = image_positions[0];
                    let second_image = image_positions[1];
                    assert!(content[..first_image].iter().any(|block| matches!(block,
                    crate::transcript::ContentBlock::Text { text } if text.contains("before界"))));
                    assert!(
                        content[first_image + 1..second_image]
                            .iter()
                            .any(|block| matches!(block,
                    crate::transcript::ContentBlock::Text { text } if text.contains("after")))
                    );
                }
                let history =
                    serde_json::to_string(&crate::context::convert_history(&restored).unwrap())
                        .unwrap();
                assert_eq!(history.matches("iVBORw0KGgo=").count(), 2);
                assert!(history.contains(r#""media_type":"png""#));
                assert_eq!(history.contains("Full output:"), truncated);
                if let Some(path) = output_path {
                    std::fs::remove_file(path).unwrap();
                }
                std::fs::remove_file(image_path).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn codemode_structured_errors_stay_script_data_and_audited_failures() {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(crate::extension::codemode::registration());
        catalog.register(ToolRegistration::phi_dynamic(
            rig::tool::DynamicTool::new(
                "structured",
                "Return a protocol-level structured error",
                json!({"type":"object"}),
                |context, _args| Box::pin(async move {
                    context.insert_result(crate::tools::ScriptToolResult(json!({
                        "isError":true,"structuredContent":{"kind":"conflict","private":"raw-protocol-data"}
                    })));
                    Err(rig::tool::ToolExecutionError::other("conflict"))
                }),
            ),
            "test",
            ToolOwner::Phi,
            ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
        ));
        let mut hooks = HookRegistry::default();
        hooks.register_hook(Hook::post_tool_failure(|event| async move {
            Ok(if event.tool_input["rewrite"] == true {
                crate::core::lifecycle::PostToolFailureOutcome::Rewrite {
                    error: "REDACTED".into(),
                }
            } else {
                crate::core::lifecycle::PostToolFailureOutcome::Continue
            })
        }));
        let (mut pipeline, mut result) = script_pipeline(
            catalog.build().unwrap(),
            hooks,
            "const result = await tools.structured({}); if (!result.isError || result.structuredContent.kind !== 'conflict') throw new Error('lost protocol result'); let rejected = false; try { await tools.structured({rewrite:true}); } catch (error) { rejected = error.message.includes('REDACTED'); } if (!rejected) throw new Error('stale error result'); return 'handled';",
        );
        let mut transcript = Transcript::new();
        let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
        assert!(!item.is_error, "{}", item.output.render());
        assert!(item.output.render().contains("handled"));
        assert!(!item.output.render().contains("raw-protocol-data"));
        let calls = item.details.unwrap().nested_calls.unwrap();
        assert!(calls.complete);
        assert!(matches!(calls.calls[0].status, NestedCallStatus::Error));
        let records: Vec<_> = transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                TranscriptEntryKind::Event { name, data } if name == "nested_tool_call" => {
                    Some(data)
                }
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record["status"] == "error"));
        assert!(
            !serde_json::to_string(&transcript)
                .unwrap()
                .contains("raw-protocol-data")
        );
    }

    #[tokio::test]
    async fn codemode_failed_scripts_keep_effects_without_committing_store() {
        let (tools, effects) = effect_registry();
        let mut transcript = Transcript::new();
        for (code, success) in [
            ("store('value', 1); return load('value');", true),
            (
                "store('value', 2); text('partial'); await tools.effect({value:7}); throw new Error('failed');",
                false,
            ),
            (
                "// @options: {\"timeout_ms\":500}\nstore('value', 3); text('partial'); await tools.effect({value:7}); while(true) {}",
                false,
            ),
        ] {
            let (mut pipeline, mut result) =
                script_pipeline(tools.clone(), HookRegistry::default(), code);
            let item = finish_script(&mut pipeline, &mut result, &mut transcript).await;
            assert_eq!(!item.is_error, success, "{}", item.output.render());
            assert_eq!(
                crate::extension::codemode::read_store(&transcript).get("value"),
                Some(&json!(1))
            );
            if !success {
                assert!(item.output.render().contains("partial"));
                let records: Vec<_> = transcript
                    .entries
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        TranscriptEntryKind::Event { name, data }
                            if name == "nested_tool_call" && data["name"] == "effect" =>
                        {
                            Some(data)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(records.len(), effects.load(Ordering::SeqCst));
                assert!(records.iter().all(|data| data["status"] == "ok"));
            }
        }
        assert_eq!(effects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn codemode_cancel_during_post_hook_keeps_completed_call_audit() {
        let (tools, effects) = effect_registry();
        let ready = Arc::new(Notify::new());
        let mut hooks = HookRegistry::default();
        hooks.register_hook(Hook::post_tool_use({
            let ready = ready.clone();
            move |event| {
                let ready = ready.clone();
                async move {
                    if event.tool_name == "effect" {
                        ready.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok(PostToolUseOutcome::Continue)
                }
            }
        }));
        let (mut pipeline, _result) =
            script_pipeline(tools, hooks, "await tools.effect({value:7});");
        let mut transcript = Transcript::new();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                pipeline.progress_round(&mut transcript, &crate::hooks::HookRegistry::new(), |_| Some(Box::pin(async { Ok(super::super::CompletionDecision::Finish) })));
                pipeline.drain(&mut transcript).await;
                tokio::select! { _ = ready.notified() => break, _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {} }
            }
        }).await.unwrap();
        pipeline.abort(&mut transcript, "test", "test");
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        let entries: Vec<_> = transcript
            .entries
            .iter()
            .filter(|entry| {
                matches!(&entry.kind,
                TranscriptEntryKind::Event { name, data }
                    if name == "nested_tool_call" && data["name"] == "effect")
            })
            .collect();
        assert_eq!(entries.len(), 1, "completed call needs one audit entry");
        let TranscriptEntryKind::Event { name, data } = &entries[0].kind else {
            panic!("expected audit event");
        };
        assert_eq!(name, "nested_tool_call");
        assert_eq!(data["status"], "ok");
    }
}
