use super::catalog::{
    ToolAuthorization, ToolExecutionMode, ToolMetadata, ToolOwner, ToolPromptMetadata,
    ToolRegistration, ToolRuntimeMetadata,
};
use super::policy::ToolExposure;
use rig::tool::{DynamicTool, PortableDynamicTool, PortableTool};
use std::fmt;
use std::sync::Arc;

/// Scheduling requirement for a declared tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolDeclarationExecution {
    #[default]
    Parallel,
    Sequential,
}

/// Agent-facing prompt and execution metadata for a declared tool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolDeclarationMetadata {
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
    pub execution: ToolDeclarationExecution,
    pub exposure: ToolExposure,
    pub terminate_on_success: bool,
}

type RegistrationFactory = dyn Fn(ToolMetadata) -> ToolRegistration + Send + Sync;

/// A repeatable Rust tool declaration resolved into each client runtime.
#[derive(Clone)]
pub struct ToolDeclaration {
    name: String,
    metadata: ToolDeclarationMetadata,
    registration: Arc<RegistrationFactory>,
}

impl ToolDeclaration {
    pub fn new<T>(tool: T) -> Self
    where
        T: PortableTool + Clone + 'static,
    {
        Self {
            name: T::NAME.to_string(),
            metadata: ToolDeclarationMetadata::default(),
            registration: Arc::new(move |metadata| {
                ToolRegistration::phi(
                    tool.clone(),
                    format!("rust-sdk:{}", T::NAME),
                    ToolOwner::Phi,
                    metadata,
                )
            }),
        }
    }

    pub fn dynamic(tool: PortableDynamicTool) -> Self {
        let name = tool.name().to_string();
        let source_name = name.clone();
        Self {
            name,
            metadata: ToolDeclarationMetadata::default(),
            registration: Arc::new(move |metadata| {
                ToolRegistration::phi_dynamic(
                    DynamicTool::from_portable(tool.clone()),
                    format!("rust-sdk:{source_name}"),
                    ToolOwner::Phi,
                    metadata,
                )
            }),
        }
    }

    pub fn with_metadata(mut self, metadata: ToolDeclarationMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn prompt_snippet(mut self, snippet: impl Into<String>) -> Self {
        self.metadata.prompt_snippet = Some(snippet.into());
        self
    }

    pub fn prompt_guidelines(
        mut self,
        guidelines: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.metadata.prompt_guidelines = guidelines.into_iter().map(Into::into).collect();
        self
    }

    pub fn exposure(mut self, exposure: ToolExposure) -> Self {
        self.metadata.exposure = exposure;
        self
    }

    pub fn sequential(mut self) -> Self {
        self.metadata.execution = ToolDeclarationExecution::Sequential;
        self
    }

    pub fn terminate_on_success(mut self) -> Self {
        self.metadata.terminate_on_success = true;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn metadata(&self) -> &ToolDeclarationMetadata {
        &self.metadata
    }

    pub(crate) fn registration(&self) -> ToolRegistration {
        (self.registration)(self.metadata.clone().into())
    }
}

impl fmt::Debug for ToolDeclaration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDeclaration")
            .field("name", &self.name)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl From<ToolDeclarationMetadata> for ToolMetadata {
    fn from(metadata: ToolDeclarationMetadata) -> Self {
        Self {
            prompt: ToolPromptMetadata {
                snippet: metadata.prompt_snippet,
                guidelines: metadata.prompt_guidelines,
            },
            runtime: ToolRuntimeMetadata {
                execution_mode: match metadata.execution {
                    ToolDeclarationExecution::Parallel => ToolExecutionMode::Parallel,
                    ToolDeclarationExecution::Sequential => ToolExecutionMode::Sequential,
                },
                exposure: metadata.exposure,
                durable_finalization: false,
                terminate_on_success: metadata.terminate_on_success,
                authorization: Some(ToolAuthorization::Invocation),
            },
        }
    }
}

#[cfg(test)]
mod declaration_tests {
    use super::*;
    use crate::tools::ToolCatalogBuilder;
    use rig::tool::ToolContext;
    use serde::Deserialize;

    #[derive(Clone)]
    struct EchoTool;

    #[derive(Deserialize)]
    struct EchoArgs {
        text: String,
    }

    impl PortableTool for EchoTool {
        const NAME: &'static str = "echo";
        type Args = EchoArgs;
        type Output = String;
        type Error = std::convert::Infallible;

        fn description(&self) -> String {
            "Echo text".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
            Ok(args.text)
        }
    }

    proptest::proptest! {
        #[test]
        fn exposure_and_loading_preserve_capability_boundaries(
            exposure in proptest::sample::select(vec![ToolExposure::Direct, ToolExposure::Deferred, ToolExposure::Hidden]),
            globally_disabled in proptest::bool::ANY,
            excluded in proptest::bool::ANY,
            selected in proptest::bool::ANY,
        ) {
            let mut catalog = ToolCatalogBuilder::new(globally_disabled);
            catalog.register(ToolDeclaration::new(EchoTool)
                .prompt_snippet("Echo snippet")
                .prompt_guidelines(["Echo guideline"])
                .exposure(exposure)
                .registration());
            let registry = catalog.build().unwrap().with_policy(
                Some(if selected { vec!["echo".into()] } else { vec![] }),
                if excluded { vec!["echo".into()] } else { vec![] },
            );
            let loaded = futures::executor::block_on(registry.loaded_registry(
                std::collections::BTreeSet::from(["echo".into()]),
            )).unwrap();
            let available = !globally_disabled && !excluded && selected && exposure != ToolExposure::Hidden;
            proptest::prop_assert_eq!(registry.is_declared("echo"), available && exposure == ToolExposure::Direct);
            proptest::prop_assert_eq!(registry.deferred_declarations().len(), usize::from(available && exposure == ToolExposure::Deferred));
            for projection in [&registry, &loaded] {
                let declared = projection.is_declared("echo");
                proptest::prop_assert_eq!(projection.is_available("echo"), available);
                proptest::prop_assert_eq!(projection.is_script_callable("echo"), available);
                proptest::prop_assert_eq!(projection.script_declarations().len(), usize::from(available));
                proptest::prop_assert_eq!(projection.provider_declarations().len(), usize::from(declared));
                proptest::prop_assert_eq!(projection.prompt_summary().contains("Echo snippet"), declared);
                proptest::prop_assert_eq!(projection.prompt_guidelines().contains("Echo guideline"), declared);
                proptest::prop_assert_eq!(futures::executor::block_on(projection.call(
                    "echo", r#"{"text":"probe"}"#, ToolContext::new(),
                )).is_ok(), available);
            }
            proptest::prop_assert_eq!(loaded.is_declared("echo"), available);
            proptest::prop_assert!(loaded.deferred_declarations().is_empty());
        }
    }

    #[tokio::test]
    async fn codemode_deferred_exposure_changes_presentation_not_authority() {
        let declaration = ToolDeclaration::new(EchoTool)
            .prompt_snippet("Deferred echo snippet")
            .prompt_guidelines(["Deferred echo guideline"])
            .exposure(ToolExposure::Deferred);
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(declaration.registration());
        catalog.register(crate::extension::codemode::registration());
        let registry = catalog.build().unwrap();
        assert!(registry.is_available("echo"));
        assert_eq!(registry.script_declarations().len(), 1);
        let declared = registry.provider_declarations();
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].name, "Codemode");
        assert!(!declared[0].description.contains("Deferred echo"));
        assert!(!registry.prompt_summary().contains("Deferred echo"));
        assert!(!registry.prompt_guidelines().contains("Deferred echo"));
        assert!(
            registry.script_declarations()[0]
                .description
                .contains("Deferred echo guideline")
        );
        assert!(
            registry
                .call("echo", r#"{"text":"hello"}"#, ToolContext::new())
                .await
                .is_ok()
        );
        let restricted = registry.with_policy(None, vec!["echo".into()]);
        assert!(restricted.script_declarations().is_empty());
        assert!(
            restricted
                .call("echo", r#"{"text":"hello"}"#, ToolContext::new())
                .await
                .is_err()
        );
    }
}
