use super::{AgentRunner, AgenticLoop};
use crate::harness::manager::preparation::DelegationMode;
use crate::hooks::{HookRegistry, SystemPromptPreparation};
use crate::provider::AnyRuntime;
use crate::provider::generation::{PreparedGeneration, request_builder};

use crate::tools::ToolRegistry;
use rig::completion::{CompletionModel, Document, Message};
use rig::message::ToolChoice;
use std::sync::{Arc, OnceLock};

#[derive(Clone)]
pub struct AgentRuntime<M>
where
    M: CompletionModel,
{
    model: M,
    provider_id: String,
    model_id: String,
    api: Option<crate::provider::ProviderApi>,
    model_cost: crate::models::ModelCost,
    history_policy: crate::provider::ProviderHistoryPolicy,
    system_prompt: Option<String>,
    prompt_preparation: Option<Arc<SystemPromptPreparation>>,
    turn_instruction: Option<String>,
    tools: ToolRegistry,
    static_context: Vec<Document>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    additional_params: Option<serde_json::Value>,
    tool_choice: Option<ToolChoice>,
    directed_delegation: Option<Arc<OnceLock<DelegationMode>>>,
}

impl<M> AgentRuntime<M>
where
    M: CompletionModel + Clone,
{
    pub fn new(model: M, system_prompt: Option<String>, tools: ToolRegistry) -> Self {
        Self {
            model,
            provider_id: String::new(),
            model_id: String::new(),
            api: None,
            model_cost: crate::models::ModelCost::default(),
            history_policy: crate::provider::ProviderHistoryPolicy::default(),
            system_prompt,
            prompt_preparation: None,
            turn_instruction: None,
            tools,
            static_context: Vec::new(),
            temperature: None,
            max_tokens: None,
            additional_params: None,
            tool_choice: None,
            directed_delegation: None,
        }
    }

    fn defer_system_prompt(mut self, hooks: &HookRegistry, working_dir: &std::path::Path) -> Self {
        let preparation = Arc::new(SystemPromptPreparation::new(
            crate::core::lifecycle::BuildSystemPromptEvent {
                system_prompt: self.system_prompt.clone().unwrap_or_default(),
                cwd: Some(working_dir.display().to_string()),
            },
        ));
        hooks.defer_system_prompt(&preparation);
        self.prompt_preparation = Some(preparation);
        self
    }

    pub(super) async fn prepare_system_prompt(&mut self, hooks: &HookRegistry) {
        if let Some(preparation) = self.prompt_preparation.take() {
            self.system_prompt = Some(preparation.finish(hooks).await);
        }
    }

    pub(crate) fn validate_turn_policy(
        &self,
        policy: &crate::core::turn::TurnToolPolicy,
    ) -> Result<(), crate::tools::ToolRuleError> {
        self.tools.validate_turn_policy(policy)
    }

    pub(crate) fn is_tool_available(&self, name: &str) -> bool {
        self.tools.is_available(name)
    }

    pub fn usage_model(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.provider_id = provider.into();
        self.model_id = model.into();
        self
    }

    pub fn model_cost(mut self, cost: crate::models::ModelCost) -> Self {
        self.model_cost = cost;
        self
    }

    pub(crate) fn history_policy(
        mut self,
        api: crate::provider::ProviderApi,
        policy: crate::provider::ProviderHistoryPolicy,
    ) -> Self {
        self.api = Some(api);
        self.history_policy = policy;
        self
    }

    pub fn temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn max_tokens(mut self, max_tokens: Option<u64>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn static_context(mut self, context: Vec<Document>) -> Self {
        self.static_context = context;
        self
    }

    pub fn additional_params(mut self, params: Option<serde_json::Value>) -> Self {
        self.additional_params = params;
        self
    }

    pub fn tool_choice(mut self, choice: Option<ToolChoice>) -> Self {
        self.tool_choice = choice;
        self
    }

    pub fn system_prompt(mut self, prompt: Option<String>) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn append_system_prompt(mut self, prompt: impl AsRef<str>) -> Self {
        match &mut self.system_prompt {
            Some(system_prompt) => {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(prompt.as_ref());
            }
            None => self.system_prompt = Some(prompt.as_ref().to_string()),
        }
        self
    }

    pub(crate) fn directed_delegation(mut self, admitted: Arc<OnceLock<DelegationMode>>) -> Self {
        self.directed_delegation = Some(admitted);
        self
    }

    fn turn_instruction(mut self, instruction: String) -> Self {
        self.turn_instruction = Some(instruction);
        self
    }

    pub fn model(mut self, model: M) -> Self {
        self.model = model;
        self
    }

    pub fn tool_policy(mut self, selected: Option<Vec<String>>, excluded: Vec<String>) -> Self {
        self.tools = self.tools.with_policy(selected, excluded);
        self
    }

    fn request_system_prompt(&self, tools: Option<&ToolRegistry>) -> Option<String> {
        let mut prompt = self.system_prompt.clone()?;
        crate::context::replace_prompt_section(
            &mut prompt,
            "<available_tools>",
            "</available_tools>",
            &tools.map_or_else(|| "(none)".to_string(), ToolRegistry::prompt_summary),
        );
        crate::context::replace_prompt_section(
            &mut prompt,
            "<tool_guidelines>",
            "</tool_guidelines>",
            &tools
                .map(ToolRegistry::prompt_guidelines)
                .unwrap_or_default(),
        );
        if let Some(instruction) = &self.turn_instruction {
            prompt.push_str("\n\n");
            prompt.push_str(instruction);
        }
        Some(prompt)
    }

    pub(super) fn request(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<rig::completion::CompletionRequest, rig::completion::CompletionError> {
        let mut messages = history;
        messages.push(prompt);
        let tools =
            (self.directed_mode() != Some(DelegationMode::Foreground)).then_some(&self.tools);
        let declarations = tools
            .map(ToolRegistry::provider_declarations)
            .unwrap_or_default();
        let mut request = request_builder(&self.model, messages, self.history_policy)?
            .temperature_opt(self.temperature)
            .max_tokens_opt(self.max_tokens)
            .additional_params_opt(self.additional_params.clone())
            .documents(self.static_context.clone())
            .tools(declarations);

        if let Some(system_prompt) = self.request_system_prompt(tools) {
            request = request.preamble(system_prompt);
        }
        if tools.is_none() {
            request = request.tool_choice(ToolChoice::None);
        } else if let Some(tool_choice) = &self.tool_choice {
            request = request.tool_choice(tool_choice.clone());
        }

        Ok(request.build())
    }

    pub(super) fn prepare_generation(
        &self,
        request: rig::completion::CompletionRequest,
    ) -> Result<PreparedGeneration<M>, rig::completion::CompletionError> {
        PreparedGeneration::new(self.model.clone(), request)
    }
}

impl<M: CompletionModel> AgentRuntime<M> {
    pub(super) fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub(super) fn provider_api(&self) -> Option<crate::provider::ProviderApi> {
        self.api
    }

    pub(super) fn model_id(&self) -> &str {
        &self.model_id
    }

    pub(super) fn cost(&self) -> &crate::models::ModelCost {
        &self.model_cost
    }

    pub(super) fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub(super) async fn restore_loaded_tools(
        &mut self,
        names: std::collections::BTreeSet<String>,
    ) -> anyhow::Result<()> {
        self.tools = self.tools.loaded_registry(names).await?;
        Ok(())
    }

    pub(super) fn is_directed(&self) -> bool {
        self.directed_delegation.is_some()
    }

    pub(super) fn directed_mode(&self) -> Option<DelegationMode> {
        self.directed_delegation.as_ref()?.get().copied()
    }
}

macro_rules! map_runtime {
    ($runtime:expr, |$inner:ident| $body:expr) => {
        match $runtime {
            AnyRuntime::OpenRouter($inner) => AnyRuntime::OpenRouter($body),
            AnyRuntime::OpenAI($inner) => AnyRuntime::OpenAI($body),
            AnyRuntime::OpenAICompletions($inner) => AnyRuntime::OpenAICompletions($body),
            AnyRuntime::ChatGPT($inner) => AnyRuntime::ChatGPT($body),
            AnyRuntime::Anthropic($inner) => AnyRuntime::Anthropic($body),
            AnyRuntime::Gemini($inner) => AnyRuntime::Gemini($body),
            AnyRuntime::DeepSeek($inner) => AnyRuntime::DeepSeek($body),
            AnyRuntime::Ollama($inner) => AnyRuntime::Ollama($body),
        }
    };
}

macro_rules! dispatch_runtime {
    ($runtime:expr, |$inner:ident| $body:expr) => {
        match $runtime {
            AnyRuntime::OpenRouter($inner) => $body,
            AnyRuntime::OpenAI($inner) => $body,
            AnyRuntime::OpenAICompletions($inner) => $body,
            AnyRuntime::ChatGPT($inner) => $body,
            AnyRuntime::Anthropic($inner) => $body,
            AnyRuntime::Gemini($inner) => $body,
            AnyRuntime::DeepSeek($inner) => $body,
            AnyRuntime::Ollama($inner) => $body,
        }
    };
}

pub(in crate::harness) use dispatch_runtime;

impl AnyRuntime {
    pub(crate) fn defer_system_prompt(
        self,
        hooks: &HookRegistry,
        working_dir: &std::path::Path,
    ) -> Self {
        map_runtime!(self, |runtime| runtime
            .defer_system_prompt(hooks, working_dir))
    }

    pub(crate) fn is_tool_available(&self, name: &str) -> bool {
        dispatch_runtime!(self, |runtime| runtime.is_tool_available(name))
    }

    pub fn append_system_prompt(self, prompt: impl AsRef<str>) -> Self {
        map_runtime!(self, |runtime| runtime.append_system_prompt(prompt))
    }

    pub(crate) fn turn_instruction(self, instruction: String) -> Self {
        map_runtime!(self, |runtime| runtime.turn_instruction(instruction))
    }

    pub(crate) fn directed_delegation(self, admitted: Arc<OnceLock<DelegationMode>>) -> Self {
        map_runtime!(self, |runtime| runtime.directed_delegation(admitted))
    }

    pub fn tool_policy(self, selected: Option<Vec<String>>, excluded: Vec<String>) -> Self {
        map_runtime!(self, |runtime| runtime.tool_policy(selected, excluded))
    }

    pub(crate) fn validate_turn_policy(
        &self,
        policy: &crate::core::turn::TurnToolPolicy,
    ) -> Result<(), crate::tools::ToolRuleError> {
        dispatch_runtime!(self, |runtime| runtime.validate_turn_policy(policy))
    }

    pub(crate) fn spawn_runner(
        self,
        hooks: HookRegistry,
        scope: crate::core::event::CoreEventScope,
        tool_policy: crate::core::turn::TurnToolPolicy,
    ) -> AgentRunner {
        dispatch_runtime!(self, |runtime| AgenticLoop::new(runtime, &hooks)
            .interaction_scope(scope)
            .tool_policy(tool_policy)
            .spawn_events())
    }
}
