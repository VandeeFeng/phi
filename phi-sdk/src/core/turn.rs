pub use crate::context::InjectedContext;
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::transcript::UserContentBlock;
use base64::Engine;
use std::path::Path;

/// Controls how accepted submissions are drained from a Steer or Follow-up queue.
///
/// This mode does not classify prompts, choose a dispatch mode, or apply to
/// immediate execution. Prompt preparation still runs once per submission;
/// this mode only determines how many pending submissions the runtime consumes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueuedSubmissionDrainMode {
    /// Consume only the oldest pending submission and leave the rest for later
    /// dispatch points, allowing the agent to respond between queued submissions.
    #[default]
    OneAtATime,
    /// Consume every submission pending at this dispatch point together; submissions
    /// accepted afterward remain queued for the next dispatch point.
    AllPending,
}

/// Correlates an accepted submission across receipts, execution events, and recovery.
pub type SubmissionId = String;

/// Why an accepted submission ended without a canonical prompt commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionDiscardReason {
    #[default]
    DispatchFailed,
    Cancelled,
}

/// Prompt content carried by a submission: text, images, and injected context.
/// It may express user intent, an Agent message, or task instructions.
/// Immediate and Follow-up start a new turn; Steer joins the active turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnPrompt {
    pub prompt: String,
    pub source: UserPromptSubmitSource,
    pub injected_context: Vec<InjectedContext>,
    pub images: Vec<ImageInput>,
    #[serde(default)]
    pub tool_policy: TurnToolPolicy,
}

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImageInputError {
    #[error("failed to read image {}: {source}", path.display())]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("image {} exceeds the {max_bytes} byte limit", path.display())]
    FileTooLarge {
        path: std::path::PathBuf,
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("image exceeds the {max_bytes} byte limit")]
    TooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("unsupported image format: {}", path.display())]
    UnsupportedFormat { path: std::path::PathBuf },
    #[error("image URL is empty")]
    EmptyUrl,
    #[error("unsupported image MIME type: {mime_type}")]
    UnsupportedMimeType { mime_type: String },
    #[error("invalid base64 image data: {source}")]
    InvalidBase64 {
        #[source]
        source: base64::DecodeError,
    },
    #[error("image data does not match MIME type {declared}")]
    MimeTypeMismatch {
        declared: String,
        detected: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageInput {
    Url {
        url: String,
        mime_type: Option<String>,
    },
    Base64 {
        data: String,
        mime_type: String,
    },
}

impl ImageInput {
    pub fn url(url: impl Into<String>) -> Self {
        Self::Url {
            url: url.into(),
            mime_type: None,
        }
    }

    pub fn url_with_mime_type(url: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Url {
            url: url.into(),
            mime_type: Some(mime_type.into()),
        }
    }

    pub fn base64(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Base64 {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    pub fn local_path(path: impl AsRef<Path>) -> Result<Self, ImageInputError> {
        let path = path.as_ref();
        let data = std::fs::read(path).map_err(|source| ImageInputError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if data.len() > MAX_IMAGE_BYTES {
            return Err(ImageInputError::FileTooLarge {
                path: path.to_path_buf(),
                actual_bytes: data.len(),
                max_bytes: MAX_IMAGE_BYTES,
            });
        }
        let mime_type =
            detect_image_mime_type(&data).ok_or_else(|| ImageInputError::UnsupportedFormat {
                path: path.to_path_buf(),
            })?;
        Ok(Self::base64(
            base64::engine::general_purpose::STANDARD.encode(data),
            mime_type,
        ))
    }

    pub(crate) fn validate(&self) -> Result<(), ImageInputError> {
        match self {
            Self::Url { url, .. } if url.trim().is_empty() => Err(ImageInputError::EmptyUrl),
            Self::Url {
                mime_type: Some(mime_type),
                ..
            } if !is_supported_image_mime_type(mime_type) => {
                Err(ImageInputError::UnsupportedMimeType {
                    mime_type: mime_type.clone(),
                })
            }
            Self::Url { .. } => Ok(()),
            Self::Base64 { data, mime_type } => validate_base64_image(data, mime_type),
        }
    }
}

/// Hook-processed prompt content awaiting its resolved dispatch path.
pub(crate) struct PreparedTurnPrompt {
    pub(crate) hook_notices: Vec<crate::hooks::HookNotice>,
    pub(crate) submission_id: Option<SubmissionId>,
    pub(crate) prompt: String,
    pub(crate) source: UserPromptSubmitSource,
    pub(crate) contexts: Vec<InjectedContext>,
    pub(crate) user_content: Vec<UserContentBlock>,
    pub(crate) tool_policy: TurnToolPolicy,
}

pub(crate) enum CoreTurnPreparation {
    Ready(PreparedTurnPrompt),
    Handled { reason: Option<String> },
}

fn validate_base64_image(data: &str, mime_type: &str) -> Result<(), ImageInputError> {
    if !is_supported_image_mime_type(mime_type) {
        return Err(ImageInputError::UnsupportedMimeType {
            mime_type: mime_type.to_string(),
        });
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|source| ImageInputError::InvalidBase64 { source })?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ImageInputError::TooLarge {
            actual_bytes: bytes.len(),
            max_bytes: MAX_IMAGE_BYTES,
        });
    }
    let detected = detect_image_mime_type(&bytes);
    if detected != Some(mime_type) {
        return Err(ImageInputError::MimeTypeMismatch {
            declared: mime_type.to_string(),
            detected: detected.map(ToString::to_string),
        });
    }
    Ok(())
}

fn is_supported_image_mime_type(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}

fn detect_image_mime_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_image_is_snapshotted_as_valid_base64() {
        let path = format!("/tmp/phi-image-{}.png", uuid::Uuid::new_v4());
        let bytes = b"\x89PNG\r\n\x1a\nimage";
        std::fs::write(&path, bytes).unwrap();

        let image = ImageInput::local_path(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        image.validate().unwrap();
        assert!(matches!(
            image,
            ImageInput::Base64 { data, mime_type }
                if mime_type == "image/png"
                    && base64::engine::general_purpose::STANDARD.decode(&data).unwrap() == bytes
        ));
    }

    #[test]
    fn base64_validation_rejects_mime_mismatch() {
        let data = base64::engine::general_purpose::STANDARD.encode(b"GIF89aimage");
        let image = ImageInput::base64(data, "image/png");

        assert!(matches!(
            image.validate().unwrap_err(),
            ImageInputError::MimeTypeMismatch { declared, detected }
                if declared == "image/png" && detected.as_deref() == Some("image/gif")
        ));
    }
}

/// Format-independent pre-approval and whole-tool removal intent for one outer execution.
/// Execution owns activation, Steer merging and retirement; parsing activates nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TurnToolPolicy {
    /// Outer-turn preapproval intent; the field name is retained for SDK and wire compatibility.
    pub allowances: Vec<(String, String)>,
    pub removals: Vec<String>,
}
