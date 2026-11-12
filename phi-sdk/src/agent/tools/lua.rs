use crate::extension::{
    ExtensionId, ExtensionRuntime, ExtensionToolRegistration, ExtensionToolSpec,
};
use crate::tools::{ToolAuthorization, ToolMetadata, canonical_tool_name};
use rig::tool::{DynamicTool, ToolExecutionError, ToolOutput};
use std::sync::Arc;

fn lua_agent_tool(spec: ExtensionToolSpec, runtime: Arc<ExtensionRuntime>) -> DynamicTool {
    let runtime_name = spec.name.clone();
    DynamicTool::new(
        canonical_tool_name(&spec.name),
        spec.description,
        spec.parameters,
        move |_context, args| {
            let name = runtime_name.clone();
            let runtime = runtime.clone();
            Box::pin(async move {
                let args = serde_json::to_string(&args).map_err(ToolExecutionError::from_error)?;
                let scope = crate::permission::execution_scope::OuterTurnPolicy::capture();
                let output = tokio::task::spawn_blocking(move || {
                    crate::permission::execution_scope::OuterTurnPolicy::with_captured(
                        scope,
                        || runtime.call_tool(&name, args),
                    )
                })
                .await
                .map_err(|error| tool_error(format!("lua tool task failed: {error}")))?
                .map_err(|error| tool_error(error.to_string()))?;
                Ok(ToolOutput::text(output))
            })
        },
    )
}

pub(crate) fn agent_tools(runtime: Arc<ExtensionRuntime>) -> Vec<ExtensionToolRegistration> {
    runtime
        .tool_specs()
        .into_iter()
        .map(|spec| {
            let owner = ExtensionId::lua(&spec.extension);
            let capability = format!("{}\0{}", spec.extension, spec.name);
            let override_existing = spec.override_existing;
            let tool = lua_agent_tool(spec, runtime.clone());
            let mut registration =
                ExtensionToolRegistration::delegated(owner, tool, capability, runtime.clone())
                    .with_metadata(
                        ToolMetadata::default().with_authorization(ToolAuthorization::Invocation),
                    );
            registration.override_existing = override_existing;
            registration
        })
        .collect()
}

fn tool_error(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::other(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_tool_delegates_to_loaded_extension_runtime() {
        let path = std::env::temp_dir().join(format!(
            "phi-unified-extension-tool-{}.lua",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"
local count = 0
register_tool("counter", {
  description = "counter",
  handler = function()
    count = count + 1
    return { count = count }
  end,
})
"#,
        )
        .unwrap();
        let runtime = Arc::new(ExtensionRuntime::try_load_with_paths(vec![path.clone()]).unwrap());
        let tools = agent_tools(runtime);

        assert!(tools[0].owner.as_ref().starts_with("lua:"));
        let toolset = rig::tool::ToolSet::from_dynamic_tools(
            tools
                .into_iter()
                .map(|registration| registration.tool)
                .collect(),
        );
        let mut context = rig::tool::ToolContext::new();
        let first = toolset.execute("counter", "{}", &mut context).await;
        let second = toolset.execute("counter", "{}", &mut context).await;

        let _ = std::fs::remove_file(path);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&first.output().render()).unwrap()["count"],
            1
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&second.output().render()).unwrap()["count"],
            2
        );
    }
}
