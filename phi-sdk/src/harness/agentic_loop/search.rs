use super::runner::RoundControl;
use crate::tools::{
    DEFAULT_TOOL_SEARCH_LIMIT, ToolAuthorization, ToolMetadata, ToolOwner, ToolPromptMetadata,
    ToolRegistration, ToolRegistry,
};
use crate::transcript::{Transcript, TranscriptEntryKind};
use rig::tool::{DynamicTool, ToolExecutionError, ToolOutput};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeSet;

pub(super) const LOAD_EVENT: &str = "tools_loaded";

#[derive(Clone)]
pub(super) struct ToolSearchHost {
    pub(super) tools: ToolRegistry,
    pub(super) control: RoundControl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    limit: Option<usize>,
    namespace: Option<String>,
}

pub(crate) fn registration() -> ToolRegistration {
    let tool = DynamicTool::new(
        "ToolSearch",
        "Search deferred tool metadata with BM25 and load matching tools for the next model request. Use this to discover MCP tools. Loaded tools remain declared on this history branch; loading grants no invocation permission.",
        json!({
            "type":"object",
            "properties":{
                "query":{"type":"string","description":"Capabilities to search for"},
                "limit":{"type":"integer","minimum":1,"description":"Maximum matches; defaults to 8"},
                "namespace":{"type":"string","description":"Optional namespace, such as an MCP server"}
            },
            "required":["query"],
            "additionalProperties":false
        }),
        |context, input| {
            Box::pin(async move {
                let host = context.require::<ToolSearchHost>()?.clone();
                let input: SearchInput = serde_json::from_value(input)
                    .map_err(|error| ToolExecutionError::other(error.to_string()))?;
                if input.query.trim().is_empty() {
                    return Err(ToolExecutionError::other("query must not be empty"));
                }
                let limit = input.limit.unwrap_or(DEFAULT_TOOL_SEARCH_LIMIT);
                if limit == 0 {
                    return Err(ToolExecutionError::other(
                        "limit must be a positive integer",
                    ));
                }
                let tools = host
                    .tools
                    .discovery_registry()
                    .await
                    .map_err(|error| ToolExecutionError::other(error.to_string()))?;
                let matches = tools.search_tools(
                    &tools.deferred_declarations(),
                    &input.query,
                    input.namespace.as_deref(),
                    limit,
                );
                if matches.is_empty() {
                    return Ok(ToolOutput::text("No matching deferred tools found."));
                }
                host.control
                    .load_tools(matches.iter().map(|tool| tool.name.clone()).collect())
                    .await
                    .map_err(ToolExecutionError::other)?;
                Ok(ToolOutput::text(format!(
                    "Loaded {} tools. Available from the next model request:\n{}",
                    matches.len(),
                    matches
                        .iter()
                        .map(|tool| format!(
                            "- {}: {}",
                            tool.name,
                            tool.description.lines().next().unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )))
            })
        },
    );
    ToolRegistration::phi_dynamic(
        tool,
        "builtin",
        ToolOwner::Phi,
        ToolMetadata::sequential(
            ToolPromptMetadata::new("Search and load deferred tools").with_guidelines([
                "Use ToolSearch to discover and load deferred tools before calling them directly.",
            ]),
        )
        .with_authorization(ToolAuthorization::Invocation),
    )
}

pub(super) fn read_loaded_tools(transcript: &Transcript) -> BTreeSet<String> {
    transcript
        .current_branch()
        .into_iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Event { name, data } if name == LOAD_EVENT => {
                serde_json::from_value::<Vec<String>>(data["tools"].clone()).ok()
            }
            _ => None,
        })
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCatalogBuilder;

    #[tokio::test]
    async fn invalid_searches_and_empty_catalog_do_not_request_loading() {
        let mut catalog = ToolCatalogBuilder::new(false);
        catalog.register(registration());
        let tools = catalog.build().unwrap();
        let (control, mut requests) = RoundControl::channel();
        for input in [
            json!({"query":" "}),
            json!({"query":"probe","limit":0}),
            json!({"query":"probe","limit":-1}),
            json!({"query":"probe","limit":1.5}),
            json!({"query":"probe","namespace":1}),
            json!({"query":"probe","unknown":true}),
        ] {
            let mut context = rig::tool::ToolContext::new();
            context.insert(ToolSearchHost {
                tools: tools.clone(),
                control: control.clone(),
            });
            let result = tools.call("ToolSearch", &input.to_string(), context).await;
            assert!(result.is_ok_and(|output| !output.result.is_success()));
        }
        let mut context = rig::tool::ToolContext::new();
        context.insert(ToolSearchHost {
            tools: tools.clone(),
            control,
        });
        let result = tools
            .call("ToolSearch", r#"{"query":"missing"}"#, context)
            .await
            .unwrap();
        assert!(result.result.is_success());
        assert!(requests.try_recv().is_err());
        assert!(!tools.is_script_callable("ToolSearch"));
    }

    #[test]
    fn loading_survives_compaction_without_crossing_branch_boundaries() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("root");
        let root = transcript.leaf_id.clone().unwrap();
        transcript.append_entry(TranscriptEntryKind::Event {
            name: LOAD_EVENT.into(),
            data: json!({"tools":["first","first"]}),
        });
        transcript.append_entry(TranscriptEntryKind::Event {
            name: LOAD_EVENT.into(),
            data: json!({"tools":false}),
        });
        transcript.append_user_message("retained");
        let retained = transcript.leaf_id.clone().unwrap();
        transcript.compress("summary".into(), &retained).unwrap();
        let compacted = transcript.leaf_id.clone().unwrap();
        assert_eq!(
            read_loaded_tools(&transcript),
            BTreeSet::from(["first".into()])
        );
        transcript.select_branch(Some(&root)).unwrap();
        assert!(read_loaded_tools(&transcript).is_empty());
        transcript.append_entry(TranscriptEntryKind::Event {
            name: LOAD_EVENT.into(),
            data: json!({"tools":["second"]}),
        });
        assert_eq!(
            read_loaded_tools(&transcript),
            BTreeSet::from(["second".into()])
        );
        let mut restored: Transcript =
            serde_json::from_value(serde_json::to_value(transcript).unwrap()).unwrap();
        restored.select_branch(Some(&compacted)).unwrap();
        assert_eq!(
            read_loaded_tools(&restored),
            BTreeSet::from(["first".into()])
        );
    }
}
