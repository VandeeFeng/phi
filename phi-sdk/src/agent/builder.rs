use crate::agent::options::AgentOptions;
use crate::agent::tools;
use crate::config::RuntimeConfig;
use crate::context::ContextFiles;
use crate::extension::mcp::McpClientManager;
use crate::harness::agentic_loop::AgentRuntime;
use crate::permission::runtime::PermissionRuntime;
use crate::provider::ProviderApi;
use crate::sandbox::Sandbox;
use crate::tools::{
    ToolAuthorization, ToolCatalogBuilder, ToolMetadata, ToolOwner, ToolPromptMetadata,
    ToolRegistration, ToolRegistry, canonical_tool_name,
};
use rig::completion::CompletionModel;
use rig::tool::Tool;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct RuntimeBuildContext<'a> {
    pub(crate) descriptor: crate::models::ModelDescriptor,
    pub(crate) options: &'a AgentOptions,
    pub(crate) cfg: &'a RuntimeConfig,
    pub(crate) context: &'a ContextFiles,
    pub(crate) working_dir: &'a std::path::Path,
    pub(crate) permission: Option<PermissionRuntime>,
    pub(crate) sandbox: Sandbox,
    pub(crate) extensions: Option<&'a Arc<crate::extension::ExtensionRuntime>>,
    pub(crate) tool_declarations: &'a [crate::tools::ToolDeclaration],
    pub(crate) mcp_manager: Option<&'a McpClientManager>,
}

struct ToolBuildContext<'a> {
    working_dir: &'a std::path::Path,
    options: &'a AgentOptions,
    agents: &'a crate::agent::custom::AgentCatalog,
    permission: Option<PermissionRuntime>,
    sandbox: Sandbox,
    cfg: &'a RuntimeConfig,
    extensions: Option<&'a Arc<crate::extension::ExtensionRuntime>>,
    tool_declarations: &'a [crate::tools::ToolDeclaration],
    mcp_manager: Option<&'a McpClientManager>,
}

pub(crate) async fn build_runtime_inner<M: CompletionModel + Clone + 'static>(
    model: M,
    provider_api: ProviderApi,
    build: RuntimeBuildContext<'_>,
) -> anyhow::Result<AgentRuntime<M>> {
    let RuntimeBuildContext {
        descriptor,
        options,
        cfg,
        context,
        working_dir,
        permission,
        sandbox,
        extensions,
        tool_declarations,
        mcp_manager,
    } = build;
    let max_tokens = resolve_max_tokens(options.max_tokens, descriptor.max_output_tokens);
    let temperature = options.temperature;
    let additional_params = model_thinking_params(provider_api, &descriptor, options, cfg);
    let tools = build_tool_registry(ToolBuildContext {
        working_dir,
        options,
        agents: &context.agent_catalog,
        permission,
        sandbox,
        cfg,
        extensions,
        tool_declarations,
        mcp_manager,
    })
    .await?;
    let preamble = crate::context::build_system_prompt(
        context,
        cfg.agent_mode,
        cfg.mode_prompt_override.as_deref(),
        &tools.prompt_summary(),
        &tools.prompt_guidelines(),
        working_dir,
    );

    Ok(AgentRuntime::new(model, Some(preamble), tools)
        .usage_model(&descriptor.provider, &descriptor.id)
        .history_policy(
            descriptor.api,
            crate::provider::ProviderHistoryPolicy::for_model(descriptor.api, &descriptor.input),
        )
        .model_cost(descriptor.cost)
        .max_tokens(max_tokens)
        .temperature(temperature)
        .static_context(Vec::new())
        .additional_params(additional_params)
        .tool_choice(None))
}

fn resolve_max_tokens(explicit: Option<u64>, descriptor: u64) -> Option<u64> {
    explicit.or(Some(descriptor))
}

pub(crate) fn model_thinking_params(
    api: ProviderApi,
    descriptor: &crate::models::ModelDescriptor,
    options: &AgentOptions,
    cfg: &RuntimeConfig,
) -> Option<serde_json::Value> {
    crate::provider::generation::model_thinking_params(
        api,
        descriptor,
        options
            .thinking_level
            .as_deref()
            .or_else(|| cfg.default_thinking_level()),
        cfg.openai_reasoning.as_ref(),
    )
}

async fn build_tool_registry(build: ToolBuildContext<'_>) -> anyhow::Result<ToolRegistry> {
    let no_tools = build.options.no_tools;
    let mode = build.cfg.agent_mode;
    let codemode = build.cfg.extensions.codemode.clone();
    let permission = build.permission.clone();
    let mcp_manager = build.mcp_manager.cloned();
    let tools = build_tools(build).await;

    let mut catalog = ToolCatalogBuilder::new(no_tools);
    catalog.permission(permission);
    for registration in tools {
        catalog.register(registration);
    }
    Ok(catalog
        .build()?
        .with_mode(mode)
        .with_codemode_config(codemode)
        .with_mcp_manager(mcp_manager))
}

fn builtin_tool_metadata() -> HashMap<String, ToolMetadata> {
    HashMap::from([
        (
            "Read".to_string(),
            ToolMetadata::prompt(
                ToolPromptMetadata::new("Read file contents")
                    .with_guidelines(["Use Read to examine files before editing."]),
            ),
        ),
        (
            "Write".to_string(),
            ToolMetadata::prompt(
                ToolPromptMetadata::new("Create or overwrite files")
                    .with_guidelines(["Use Write only for new files or complete rewrites."]),
            ),
        ),
        (
            "Edit".to_string(),
            ToolMetadata::prompt(
                ToolPromptMetadata::new("Make precise file edits with exact text replacement")
                    .with_guidelines(["Use Edit for precise changes with exact text replacement."]),
            ),
        ),
        (
            "Bash".to_string(),
            ToolMetadata::prompt(
                ToolPromptMetadata::new("Execute bash commands")
                    .with_guidelines(["Use Bash for running commands, tests, and git operations."]),
            ),
        ),
        (
            "Grep".to_string(),
            ToolMetadata::prompt(
                ToolPromptMetadata::new("Search file contents with regex")
                    .with_guidelines(["Use Grep to search file contents."]),
            ),
        ),
        (
            "Glob".to_string(),
            ToolMetadata::prompt(
                ToolPromptMetadata::new("Find files by glob pattern")
                    .with_guidelines(["Use Glob to find files by name or path pattern."]),
            ),
        ),
        (
            "Agent".to_string(),
            ToolMetadata::sequential(ToolPromptMetadata::new(
                "Create independent Agents, discover members and exchange messages",
            )),
        ),
        (
            tools::task::TaskTool::NAME.to_string(),
            ToolMetadata::sequential(ToolPromptMetadata::new("Create, get, list or update recorded work using action")
                .with_guidelines([
                    "Use Task action=update to set in_progress when starting and completed before reporting completion. Replies and active_form never change status.",
                ])),
        ),
    ])
}

async fn build_tools(build: ToolBuildContext<'_>) -> Vec<ToolRegistration> {
    let ToolBuildContext {
        working_dir,
        options,
        agents,
        permission,
        sandbox,
        cfg,
        extensions,
        tool_declarations,
        mcp_manager,
    } = build;
    let metadata = builtin_tool_metadata();
    let mut registrations = if options.no_tools {
        Vec::new()
    } else {
        base_tool_registrations(
            options,
            agents,
            permission.clone(),
            sandbox,
            working_dir,
            &metadata,
        )
    };
    if !options.no_tools {
        registrations.push(crate::harness::agentic_loop::search_registration());
    }
    registrations.extend(
        tool_declarations
            .iter()
            .map(crate::tools::ToolDeclaration::registration),
    );

    registrations.extend(
        crate::extension::collect_tool_registrations(
            extensions,
            crate::extension::ExtensionToolContext {
                config: cfg,
                options,
                permission,
                mcp: mcp_manager,
            },
        )
        .await,
    );
    registrations
}

fn builtin_registration<T>(
    tool: T,
    metadata: &HashMap<String, ToolMetadata>,
    authorization: ToolAuthorization,
) -> ToolRegistration
where
    T: Tool + 'static,
{
    let tool_metadata = metadata
        .get(&canonical_tool_name(T::NAME))
        .cloned()
        .unwrap_or_default();
    ToolRegistration::phi(
        tool,
        "builtin",
        ToolOwner::Phi,
        tool_metadata.with_authorization(authorization),
    )
}

fn base_tool_registrations(
    options: &AgentOptions,
    agents: &crate::agent::custom::AgentCatalog,
    permission: Option<PermissionRuntime>,
    sandbox: Sandbox,
    working_dir: &std::path::Path,
    metadata: &HashMap<String, ToolMetadata>,
) -> Vec<ToolRegistration> {
    let mut registrations = Vec::new();
    if let Some(sender) = &options.agent_sender {
        registrations.push(builtin_registration(
            tools::AgentTool::new(sender.clone(), agents, permission.clone()),
            metadata,
            ToolAuthorization::AgentOperation,
        ));
    }
    if let Some(sender) = &options.task_sender {
        use tools::task::{TaskDispatcher, TaskTool};
        registrations.push(builtin_registration(
            TaskTool(TaskDispatcher::new(sender.clone())),
            metadata,
            ToolAuthorization::TaskOperation,
        ));
    }
    registrations.push(builtin_registration(
        tools::ReadTool::new(permission.clone(), working_dir.to_path_buf()),
        metadata,
        ToolAuthorization::ResolvedPath,
    ));
    let file_access = Arc::new(sandbox.clone());
    registrations.push(builtin_registration(
        tools::WriteTool::new(
            permission.clone(),
            working_dir.to_path_buf(),
            file_access.clone(),
        ),
        metadata,
        ToolAuthorization::ResolvedPath,
    ));
    registrations.push(builtin_registration(
        tools::EditTool::new(permission.clone(), working_dir.to_path_buf(), file_access),
        metadata,
        ToolAuthorization::ResolvedPath,
    ));
    registrations.push(builtin_registration(
        tools::BashTool::new(permission.clone(), sandbox)
            .with_dispatcher(options.bash_sender.clone()),
        metadata,
        ToolAuthorization::CommandEvidence,
    ));
    registrations.push(builtin_registration(
        tools::GrepTool::new(permission.clone(), working_dir.to_path_buf()),
        metadata,
        ToolAuthorization::ResolvedPath,
    ));
    registrations.push(builtin_registration(
        tools::GlobTool::new(permission, working_dir.to_path_buf()),
        metadata,
        ToolAuthorization::ResolvedPath,
    ));
    registrations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_agent_thinking_precedence() {
        let descriptor = crate::models::ModelDescriptor {
            provider: "openai".into(),
            id: "reasoning-model".into(),
            name: "Reasoning model".into(),
            api: ProviderApi::OpenAiResponses,
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::models::ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 16_384,
            cost: Default::default(),
        };
        let config = RuntimeConfig {
            default_thinking_level: Some("high".into()),
            ..Default::default()
        };
        let mut options = AgentOptions::default();
        assert_eq!(
            model_thinking_params(descriptor.api, &descriptor, &options, &config),
            Some(serde_json::json!({ "reasoning": { "effort": "high" } }))
        );
        for level in crate::models::ThinkingLevel::ALL {
            options.thinking_level = Some(level.as_str().into());
            let resolved = descriptor.resolve_thinking_level(level.as_str());
            let effort = if resolved == "off" { "none" } else { &resolved };
            assert_eq!(
                model_thinking_params(descriptor.api, &descriptor, &options, &config),
                Some(serde_json::json!({ "reasoning": { "effort": effort } }))
            );
        }
    }
}
