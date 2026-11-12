use compact_str::CompactString;

use super::event::CoreEvent;
use crate::core::event::AgentEvent;
use crate::transcript::{ContentBlock, SwitchSnapshot, Transcript, Usage};

#[derive(Debug, Default)]
pub struct CoreRunState {
    switch: Option<SwitchSnapshot>,
    assistant_id: Option<CompactString>,
    assistant_original: String,
    assistant_display: String,
    reasoning: String,
    reasoning_recorded: bool,
    assistant_blocks: Vec<ContentBlock>,
    recorded_text_len: usize,
    last_response: CompactString,
    last_display: CompactString,
    total_usage: Usage,
}

impl CoreRunState {
    pub(crate) fn new(config: &crate::config::RuntimeConfig) -> Self {
        Self {
            switch: config.switch.as_deref().map(|name| SwitchSnapshot {
                name: name.into(),
                provider: CompactString::default(),
                model: CompactString::default(),
                thinking_level: config.default_thinking_level.as_deref().map(Into::into),
                mode: config.agent_mode.as_str().into(),
                prompt: config
                    .agent_mode
                    .prompt(config.mode_prompt_override.as_deref())
                    .map(Into::into),
            }),
            ..Default::default()
        }
    }

    pub fn reset(&mut self) {
        self.assistant_id = None;
        self.assistant_original.clear();
        self.assistant_display.clear();
        self.reasoning.clear();
        self.reasoning_recorded = false;
        self.assistant_blocks.clear();
        self.recorded_text_len = 0;
        self.last_response = CompactString::new("");
        self.last_display = CompactString::new("");
        self.total_usage = Usage::default();
    }

    pub(crate) fn unfinished_assistant_event(
        &self,
        turn_id: CompactString,
        provider: &str,
        api: Option<crate::provider::ProviderApi>,
        model: &str,
        stop_reason: &str,
    ) -> Option<AgentEvent> {
        let id = self.assistant_id.clone()?;
        if self.assistant_original.trim().is_empty()
            && self.reasoning.trim().is_empty()
            && self.assistant_blocks.is_empty()
        {
            return None;
        }
        Some(AgentEvent::AssistantFinished {
            id,
            provider_message_id: None,
            turn_id,
            provider: provider.into(),
            api,
            model: model.into(),
            content: Vec::new(),
            response: "".into(),
            display: "".into(),
            usage: Usage::default(),
            stop_reason: stop_reason.into(),
            has_tool_calls: self
                .assistant_blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall { .. })),
        })
    }
}

pub fn reduce_agent_event(
    session: &mut Transcript,
    state: &mut CoreRunState,
    event: AgentEvent,
) -> Vec<CoreEvent> {
    let mut events = Vec::new();
    match event {
        AgentEvent::NestedToolCall { entry_id, call } => {
            if let Some(entry_id) = entry_id {
                session.append_nested_tool_call(entry_id, &call);
            }
            events.push(CoreEvent::NestedToolCall { call });
        }
        AgentEvent::ProviderRecovery { recovery } => {
            events.push(CoreEvent::ProviderRecovery { recovery })
        }
        AgentEvent::AssistantInterrupted {
            turn_id,
            provider,
            api,
            model,
        } => {
            if let Some(partial) =
                state.unfinished_assistant_event(turn_id, &provider, api, &model, "error")
            {
                events.extend(reduce_agent_event(session, state, partial));
            }
        }
        AgentEvent::SubmissionDiscarded { submission_id } => {
            events.push(CoreEvent::SubmissionDiscarded {
                submission_id,
                reason: crate::core::turn::SubmissionDiscardReason::DispatchFailed,
            });
        }
        AgentEvent::UserInputInjected {
            submission_id,
            prompt,
            source,
            contexts,
            content,
        } => {
            session.append_user_content(content);
            for context in contexts {
                session.append_context_message(
                    context.source,
                    crate::transcript::ContextScope::Turn,
                    &context.content,
                );
            }
            events.push(CoreEvent::UserInputInjected {
                submission_id,
                prompt,
                source,
            });
        }
        AgentEvent::AssistantStarted { id } => {
            state.assistant_id = Some(id.clone());
            state.assistant_original.clear();
            state.assistant_display.clear();
            state.reasoning.clear();
            state.reasoning_recorded = false;
            state.assistant_blocks.clear();
            state.recorded_text_len = 0;
            events.push(CoreEvent::AssistantStarted { id });
        }
        AgentEvent::AssistantDelta { original, display } => {
            state.assistant_original.push_str(&original);
            state.assistant_display.push_str(&display);
            events.push(CoreEvent::AssistantDelta { original, display });
        }
        AgentEvent::AssistantImage { source } => {
            finalize_reasoning(state, &mut events);
            record_assistant_text(state, None);
            state.assistant_blocks.push(ContentBlock::Image {
                source: source.clone(),
            });
            events.push(CoreEvent::AssistantImage { source });
        }
        AgentEvent::Reasoning(text) => {
            state.reasoning = text.to_string();
            events.push(CoreEvent::ReasoningDelta { text });
        }
        AgentEvent::ReasoningDone => finalize_reasoning(state, &mut events),
        AgentEvent::ToolCall {
            id,
            provider_call_id,
            provider_item_id,
            name,
            args,
        } => {
            finalize_reasoning(state, &mut events);
            record_assistant_text(state, None);
            state.assistant_blocks.push(ContentBlock::ToolCall {
                id: id.clone(),
                provider_call_id: provider_call_id.clone(),
                provider_item_id: provider_item_id.clone(),
                name: name.clone(),
                args: args.clone(),
            });
            events.push(CoreEvent::ToolCallStarted {
                id,
                provider_call_id,
                provider_item_id,
                name,
                args,
            });
        }
        AgentEvent::ToolResult {
            id,
            entry_id,
            output,
            is_error,
            details,
        } => {
            let (provider_call_id, provider_item_id, tool_name) =
                pending_tool_call_metadata(state, &id);
            session.append_tool_result_with_entry_id(
                entry_id,
                &id,
                provider_call_id,
                provider_item_id,
                tool_name,
                &output,
                is_error,
                details.clone(),
            );
            events.push(CoreEvent::ToolCallFinished {
                id,
                output,
                is_error,
                details,
            });
        }
        AgentEvent::AssistantFinished {
            id,
            provider_message_id,
            turn_id,
            provider,
            api,
            model,
            content,
            response,
            display,
            usage,
            stop_reason,
            has_tool_calls,
        } => {
            finalize_reasoning(state, &mut events);
            let response = final_response(&response, state);
            let display = if display.is_empty() {
                CompactString::new(&state.assistant_display)
            } else {
                display
            };
            record_assistant_text(state, Some(&response));
            if !content.is_empty() {
                state.assistant_blocks = content;
            }
            let content = std::mem::take(&mut state.assistant_blocks);
            let switch = state.switch.clone().map(|mut snapshot| {
                snapshot.provider = provider.clone();
                snapshot.model = model.clone();
                snapshot
            });
            session.append_assistant_with_switch(
                provider_message_id,
                Some(turn_id),
                content.clone(),
                provider,
                api,
                model,
                switch,
                usage.clone(),
                stop_reason,
            );
            state.assistant_id = None;
            state.last_response = response.clone();
            state.last_display = display.clone();
            state.total_usage = add_usage(&state.total_usage, &usage);
            events.push(CoreEvent::AssistantFinished {
                id,
                content,
                response,
                display,
                usage,
                has_tool_calls,
            });
            state.assistant_original.clear();
            state.assistant_display.clear();
            state.recorded_text_len = 0;
        }
        AgentEvent::RunFinished { response, usage } => {
            let response = if response.is_empty() {
                state.last_response.clone()
            } else {
                response
            };
            let display = state.last_display.clone();
            events.push(CoreEvent::TurnFinished {
                response,
                display,
                usage,
                compacted: false,
            });
            state.reset();
        }
        AgentEvent::Error(failure) => {
            finalize_reasoning(state, &mut events);
            events.push(CoreEvent::TurnFailed { failure });
            state.reset();
        }
    }
    events
}

fn finalize_reasoning(state: &mut CoreRunState, events: &mut Vec<CoreEvent>) {
    if state.reasoning_recorded || state.reasoning.trim().is_empty() {
        return;
    }
    state.assistant_blocks.push(ContentBlock::Thinking {
        text: CompactString::new(&state.reasoning).into(),
        metadata: None,
    });
    events.push(CoreEvent::ReasoningFinished {
        text: CompactString::new(&state.reasoning),
    });
    state.reasoning_recorded = true;
}

fn pending_tool_call_metadata(
    state: &CoreRunState,
    tool_call_id: &str,
) -> (
    Option<CompactString>,
    Option<CompactString>,
    Option<CompactString>,
) {
    state
        .assistant_blocks
        .iter()
        .rev()
        .find_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                provider_call_id,
                provider_item_id,
                name,
                ..
            } if id == tool_call_id => Some((
                provider_call_id.clone(),
                provider_item_id.clone(),
                Some(name.clone()),
            )),
            _ => None,
        })
        .unwrap_or((None, None, None))
}

fn record_assistant_text(state: &mut CoreRunState, response: Option<&CompactString>) {
    let text = response.map_or(state.assistant_original.as_str(), |text| text.as_str());
    if state.recorded_text_len >= text.len() {
        return;
    }
    let unrecorded = &text[state.recorded_text_len..];
    if !unrecorded.trim().is_empty() {
        state.assistant_blocks.push(ContentBlock::Text {
            text: CompactString::new(unrecorded).into(),
        });
    }
    state.recorded_text_len = text.len();
}

fn final_response(response: &CompactString, state: &CoreRunState) -> CompactString {
    if !response.is_empty() {
        return response.clone();
    }
    CompactString::new(&state.assistant_original)
}

fn add_usage(left: &Usage, right: &Usage) -> Usage {
    Usage {
        input: left.input.saturating_add(right.input),
        output: left.output.saturating_add(right.output),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
        cost: left.cost + right.cost,
        cache_read: left.cache_read.saturating_add(right.cache_read),
        cache_write: left.cache_write.saturating_add(right.cache_write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{ImageSource, UserContentBlock};

    fn assistant_finished(response: &str, content: Vec<ContentBlock>) -> AgentEvent {
        AgentEvent::AssistantFinished {
            id: CompactString::new("assistant-1"),
            provider_message_id: None,
            turn_id: CompactString::new("turn-1"),
            provider: CompactString::new("test"),
            api: None,
            model: CompactString::new("test"),
            content,
            response: CompactString::new(response),
            display: CompactString::new(""),
            usage: Usage::default(),
            stop_reason: CompactString::new("stop"),
            has_tool_calls: false,
        }
    }

    fn new_transcript() -> Transcript {
        Transcript::new()
    }

    proptest::proptest! {
        #[test]
        fn assistant_switch_is_execution_attribution(
            switch in proptest::option::of("[a-z][a-z0-9-]{0,20}"),
            provider_message_id in proptest::option::of("[a-z][a-z0-9_-]{0,20}"),
        ) {
            let mut transcript = new_transcript();
            let config = crate::config::RuntimeConfig {
                switch: switch.clone(),
                default_thinking_level: Some("high".into()),
                agent_mode: crate::config::AgentMode::Plan,
                mode_prompt_override: Some("captured prompt".into()),
                ..Default::default()
            };
            let mut state = CoreRunState::new(&config);
            for interrupted in [false, true] {
                state.reset();
                let mut published = Vec::new();
                for event in [
                    AgentEvent::AssistantStarted { id: "assistant-1".into() },
                    AgentEvent::AssistantDelta {
                        original: "response".into(), display: "response".into(),
                    },
                    if interrupted {
                        AgentEvent::AssistantInterrupted {
                            turn_id: "turn-1".into(), provider: "test".into(),
                            api: Some(crate::provider::ProviderApi::OpenAiResponses), model: "test".into(),
                        }
                    } else {
                        AgentEvent::AssistantFinished {
                            id: "assistant-1".into(),
                            provider_message_id: provider_message_id.as_deref().map(Into::into),
                            turn_id: "turn-1".into(),
                            provider: "test".into(),
                            api: Some(crate::provider::ProviderApi::OpenAiResponses),
                            model: "test".into(),
                            content: Vec::new(),
                            response: "response".into(),
                            display: "response".into(),
                            usage: Usage::default(),
                            stop_reason: "stop".into(),
                            has_tool_calls: false,
                        }
                    },
                ] {
                    published.extend(reduce_agent_event(&mut transcript, &mut state, event));
                }
                proptest::prop_assert!(matches!(published.last(), Some(CoreEvent::AssistantFinished { id, .. }) if id == "assistant-1"), "completion must keep the display ID");
                let crate::transcript::TranscriptEntryKind::Message { message } =
                    &transcript.entries.last().unwrap().kind
                else {
                    unreachable!("assistant response must be a message");
                };
                let value = serde_json::to_value(message).unwrap();
                let expected_id = switch.as_ref().map(|name| format!("{name}-1"));
                proptest::prop_assert_eq!(value["switch"].as_str(), expected_id.as_deref());
                proptest::prop_assert_eq!(value["id"].as_str(), provider_message_id.as_deref().filter(|_| !interrupted));
                proptest::prop_assert_eq!(value["api"].as_str(), Some("openai-responses"));
                let history = serde_json::to_value(&transcript).unwrap();
                let snapshots = history["switch_snapshots"].as_array().unwrap();
                proptest::prop_assert_eq!(snapshots.len(), usize::from(switch.is_some()));
                if switch.is_some() {
                    proptest::prop_assert_eq!(snapshots[0]["id"].as_str(), expected_id.as_deref());
                    let snapshot = &snapshots[0]["switch"];
                    proptest::prop_assert_eq!(snapshot["name"].as_str(), switch.as_deref());
                    proptest::prop_assert_eq!(snapshot["provider"].as_str(), Some("test"));
                    proptest::prop_assert_eq!(snapshot["model"].as_str(), Some("test"));
                    proptest::prop_assert_eq!(snapshot["thinking_level"].as_str(), Some("high"));
                    proptest::prop_assert_eq!(snapshot["mode"].as_str(), Some("plan"));
                    proptest::prop_assert_eq!(snapshot["prompt"].as_str(), Some("captured prompt"));
                }
                proptest::prop_assert!(value.get("thinking_level").is_none());
                proptest::prop_assert!(value.get("mode").is_none());
                let decoded: crate::transcript::AgentMessage = serde_json::from_value(value.clone()).unwrap();
                proptest::prop_assert_eq!(serde_json::to_value(decoded).unwrap(), value.clone());
            }
            proptest::prop_assert_eq!(transcript.entries.len(), 2);
        }
    }

    #[test]
    fn injected_user_input_is_committed_to_transcript() {
        let mut transcript = new_transcript();
        let mut state = CoreRunState::default();

        let events = reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::UserInputInjected {
                submission_id: None,
                prompt: CompactString::new("change direction"),
                source: crate::core::lifecycle::UserPromptSubmitSource::Interactive,
                contexts: Vec::new(),
                content: vec![
                    UserContentBlock::Text {
                        text: "change direction".into(),
                    },
                    UserContentBlock::Image {
                        source: ImageSource::Url {
                            url: "https://example.com/image.png".into(),
                            mime_type: None,
                        },
                    },
                ],
            },
        );

        assert!(transcript.has_user_message());
        assert!(matches!(
            &transcript.current_branch()[0].kind,
            crate::transcript::TranscriptEntryKind::Message {
                message: crate::transcript::AgentMessage::User { content }
            } if matches!(content.get(1), Some(UserContentBlock::Image { .. }))
        ));
        assert!(matches!(
            events.as_slice(),
            [CoreEvent::UserInputInjected { prompt, .. }] if prompt == "change direction"
        ));
    }

    #[test]
    fn assistant_image_is_streamed_and_committed_as_canonical_content() {
        let mut transcript = new_transcript();
        let mut state = CoreRunState::default();
        let source = ImageSource::Url {
            url: "https://example.com/generated.png".into(),
            mime_type: Some("image/png".into()),
        };

        let streamed = reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::AssistantImage {
                source: source.clone(),
            },
        );
        let finished = reduce_agent_event(
            &mut transcript,
            &mut state,
            assistant_finished("", vec![ContentBlock::Image { source }]),
        );

        assert!(matches!(
            streamed.as_slice(),
            [CoreEvent::AssistantImage { .. }]
        ));
        assert!(matches!(
            finished.as_slice(),
            [CoreEvent::AssistantFinished { content, .. }]
                if matches!(content.as_slice(), [ContentBlock::Image { .. }])
        ));
        assert!(matches!(
            &transcript.current_branch()[0].kind,
            crate::transcript::TranscriptEntryKind::Message {
                message: crate::transcript::AgentMessage::Assistant { content, .. }
            } if matches!(content.as_slice(), [ContentBlock::Image { .. }])
        ));
    }

    #[test]
    fn reasoning_only_response_does_not_become_assistant_text() {
        let mut transcript = new_transcript();
        let mut state = CoreRunState::default();

        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::AssistantStarted {
                id: CompactString::new("assistant-1"),
            },
        );
        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::Reasoning(CompactString::new("internal thoughts")),
        );
        let events = reduce_agent_event(
            &mut transcript,
            &mut state,
            assistant_finished("", Vec::new()),
        );

        assert!(
            events
                .iter()
                .any(|event| matches!(event, CoreEvent::ReasoningFinished { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            CoreEvent::AssistantFinished { response, display, .. }
                if response.is_empty() && display.is_empty()
        )));
    }

    #[test]
    fn canonical_content_replaces_provisional_reasoning() {
        let mut transcript = new_transcript();
        let mut state = CoreRunState::default();
        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::Reasoning(CompactString::new("provisional")),
        );
        let canonical = vec![ContentBlock::Thinking {
            text: CompactString::new("summary").into(),
            metadata: Some(crate::transcript::ReasoningMetadata {
                id: Some(CompactString::new("rs_1")),
                content: Vec::new(),
            }),
        }];

        reduce_agent_event(
            &mut transcript,
            &mut state,
            assistant_finished("", canonical),
        );

        assert!(matches!(
            &transcript.entries.last().unwrap().kind,
            crate::transcript::TranscriptEntryKind::Message {
                message: crate::transcript::AgentMessage::Assistant { content, .. }
            } if matches!(
                content.as_slice(),
                [ContentBlock::Thinking {
                    metadata: Some(crate::transcript::ReasoningMetadata { id: Some(id), .. }),
                    ..
                }] if id == "rs_1"
            )
        ));
    }

    #[test]
    fn tool_result_uses_pending_tool_call_metadata() {
        let mut transcript = new_transcript();
        let mut state = CoreRunState::default();

        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::ToolCall {
                id: CompactString::new("tool-1"),
                provider_call_id: Some(CompactString::new("call-1")),
                provider_item_id: Some(CompactString::new("tool-1")),
                name: CompactString::new("bash"),
                args: serde_json::json!({"command":"true"}),
            },
        );
        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::ToolResult {
                id: CompactString::new("tool-1"),
                entry_id: CompactString::new("entry-1"),
                output: CompactString::new("ok"),
                is_error: false,
                details: None,
            },
        );

        assert_last_tool_result_metadata(&transcript, Some("call-1"), "bash");
        assert_eq!(transcript.entries.last().unwrap().id, "entry-1");
    }

    #[test]
    fn tool_result_falls_back_to_committed_tool_call_metadata() {
        let mut transcript = new_transcript();
        let mut state = CoreRunState::default();

        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::ToolCall {
                id: CompactString::new("tool-1"),
                provider_call_id: Some(CompactString::new("call-1")),
                provider_item_id: Some(CompactString::new("tool-1")),
                name: CompactString::new("bash"),
                args: serde_json::json!({"command":"true"}),
            },
        );
        reduce_agent_event(
            &mut transcript,
            &mut state,
            assistant_finished("", Vec::new()),
        );
        reduce_agent_event(
            &mut transcript,
            &mut state,
            AgentEvent::ToolResult {
                id: CompactString::new("tool-1"),
                entry_id: CompactString::new("entry-1"),
                output: CompactString::new("ok"),
                is_error: false,
                details: None,
            },
        );

        assert_last_tool_result_metadata(&transcript, Some("call-1"), "bash");
    }

    fn assert_last_tool_result_metadata(
        transcript: &Transcript,
        expected_provider_call_id: Option<&str>,
        expected_tool_name: &str,
    ) {
        let Some(crate::transcript::TranscriptEntryKind::Message {
            message:
                crate::transcript::AgentMessage::ToolResult {
                    provider_call_id,
                    provider_item_id,
                    tool_name,
                    ..
                },
        }) = transcript.entries.last().map(|entry| &entry.kind)
        else {
            panic!("expected tool result entry");
        };
        assert_eq!(provider_call_id.as_deref(), expected_provider_call_id);
        assert_eq!(provider_item_id.as_deref(), Some("tool-1"));
        assert_eq!(tool_name.as_str(), expected_tool_name);
    }
}
