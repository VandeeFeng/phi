//! HTTP contracts and adapters for supported client operations.

use anyhow::ensure;
use phi_sdk::transcript::{ContextSource, ImageSource};
use phi_sdk::turn::{ImageInput, SubmissionDispatch, TurnPrompt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum RemoteSelection {
    Model {
        selection: String,
    },
    Switch {
        name: String,
    },
    ThinkingLevel {
        level: phi_sdk::models::ThinkingLevel,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCatalog {
    pub models: Vec<RemoteModel>,
    pub switches: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RemoteModel {
    pub provider: String,
    pub model: String,
    pub complete: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SubmitInput {
    text: String,
    delivery: SubmissionDispatch,
    #[serde(default)]
    images: Vec<ImageSource>,
    #[serde(default)]
    file_context: Vec<String>,
}

impl SubmitInput {
    pub(crate) fn from_sdk(
        prompt: TurnPrompt,
        dispatch: SubmissionDispatch,
    ) -> anyhow::Result<Self> {
        let mut file_context = Vec::new();
        for context in prompt.injected_context {
            ensure!(
                context.source == ContextSource::FileMention,
                "Gateway accepts only file-reference context from clients"
            );
            file_context.push(context.content);
        }
        Ok(Self {
            text: prompt.prompt,
            delivery: dispatch,
            images: prompt
                .images
                .into_iter()
                .map(|image| match image {
                    ImageInput::Url { url, mime_type } => ImageSource::Url {
                        url: url.into(),
                        mime_type: mime_type.map(Into::into),
                    },
                    ImageInput::Base64 { data, mime_type } => ImageSource::Base64 {
                        data: data.into(),
                        mime_type: mime_type.into(),
                    },
                })
                .collect(),
            file_context,
        })
    }

    pub(crate) fn into_sdk(self) -> (TurnPrompt, SubmissionDispatch) {
        let prompt = TurnPrompt {
            prompt: self.text,
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
            images: self
                .images
                .into_iter()
                .map(|image| match image {
                    ImageSource::Url { url, mime_type } => ImageInput::Url {
                        url: url.to_string(),
                        mime_type: mime_type.map(|value| value.to_string()),
                    },
                    ImageSource::Base64 { data, mime_type } => ImageInput::Base64 {
                        data: data.to_string(),
                        mime_type: mime_type.to_string(),
                    },
                })
                .collect(),
            injected_context: self
                .file_context
                .into_iter()
                .map(|content| phi_sdk::turn::InjectedContext {
                    source: ContextSource::FileMention,
                    content,
                })
                .collect(),
            tool_policy: Default::default(),
        };
        (prompt, self.delivery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn submission_accepts_only_client_fields() {
        for field in [
            "source",
            "tool_policy",
            "injected_context",
            "prompt",
            "dispatch",
        ] {
            let mut value = json!({"text":"inspect", "delivery":"steer"});
            value[field] = json!(null);
            assert!(
                serde_json::from_value::<SubmitInput>(value).is_err(),
                "{field}"
            );
        }
        let input: SubmitInput = serde_json::from_value(json!({
            "text":"inspect", "delivery":"follow_up", "fileContext":["reference contents"],
            "images":[{"type":"url", "url":"https://example.test/image.png"}]
        }))
        .unwrap();
        let (prompt, delivery) = input.into_sdk();
        assert_eq!(delivery, SubmissionDispatch::FollowUp);
        assert!(matches!(
            prompt.source,
            phi_sdk::lifecycle::UserPromptSubmitSource::Interactive
        ));
        assert_eq!(
            prompt.injected_context[0].source,
            ContextSource::FileMention
        );
        assert_eq!(prompt.injected_context[0].content, "reference contents");
        assert!(
            matches!(prompt.images.as_slice(), [ImageInput::Url { url, mime_type }]
                if url == "https://example.test/image.png" && mime_type.is_none()
            )
        );
        assert_eq!(prompt.tool_policy, phi_sdk::turn::TurnToolPolicy::default());
    }

    #[test]
    fn client_rejects_internal_context_provenance() {
        let prompt = TurnPrompt {
            prompt: "inspect".into(),
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
            injected_context: vec![phi_sdk::turn::InjectedContext {
                source: ContextSource::Hook,
                content: "not a file reference".into(),
            }],
            images: Vec::new(),
            tool_policy: Default::default(),
        };
        assert!(SubmitInput::from_sdk(prompt, SubmissionDispatch::Steer).is_err());
    }
}
