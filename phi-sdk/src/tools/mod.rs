mod catalog;
mod declaration;
mod display;
mod policy;

pub(crate) use catalog::{
    DEFAULT_TOOL_SEARCH_LIMIT, ScriptToolResult, ToolAuthority, ToolAuthorization,
    ToolCatalogBuilder, ToolExecutionMode, ToolMetadata, ToolOwner, ToolPromptMetadata,
    ToolRegistration, ToolRegistry, ToolResultFinalizeContext, ToolResultFinalizer, ToolTurnEnd,
};
pub(crate) use policy::canonical_tool_name;

pub use crate::core::event::ToolDiffChange;
pub use crate::core::event::ToolDiffChangeKind;
pub use crate::core::event::ToolDiffDetails;
pub use crate::core::event::ToolDiffHunk;
pub use crate::core::event::ToolDisplay;
pub use crate::core::event::{
    NestedCallStatus, NestedCalls, NestedToolCall, ToolOutputDetails, ToolResultDetails,
};
pub use declaration::ToolDeclaration;
pub use declaration::ToolDeclarationExecution;
pub use declaration::ToolDeclarationMetadata;
pub use display::ToolDisplayFormatter;
pub use display::ToolDisplayRegistry;
pub use policy::ToolExposure;
pub use policy::ToolPolicy;
pub use policy::{ToolRule, ToolRuleError, ToolRules};
pub use rig::tool::{
    IntoToolOutput, PortableDynamicTool, PortableTool, ToolExecutionError, ToolOutput, ToolResult,
};
