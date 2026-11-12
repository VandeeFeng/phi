use http::StatusCode;
use rig::completion::CompletionError;

const RIG_TRANSPORT_ERROR_PREFIX: &str = "Http client error: ";
const RIG_STREAM_ENDED: &str = "Stream ended";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderErrorKind {
    Authentication,
    AccessDenied,
    RateLimited,
    Unavailable,
    Rejected,
    InvalidResponse,
    InvalidRequest,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderErrorFacts {
    pub(crate) kind: ProviderErrorKind,
    pub(crate) status: Option<u16>,
    pub(crate) context_overflow: bool,
}

pub(crate) fn error_facts(error: &CompletionError) -> ProviderErrorFacts {
    let status = error.provider_response_status();
    let kind = if super::auth::is_authorization_error(error) {
        ProviderErrorKind::Authentication
    } else {
        match status {
            Some(StatusCode::UNAUTHORIZED) => ProviderErrorKind::Authentication,
            Some(StatusCode::FORBIDDEN) => ProviderErrorKind::AccessDenied,
            Some(StatusCode::TOO_MANY_REQUESTS) => ProviderErrorKind::RateLimited,
            Some(StatusCode::REQUEST_TIMEOUT) => ProviderErrorKind::Unavailable,
            Some(status) if status.is_server_error() => ProviderErrorKind::Unavailable,
            Some(status) if status.is_client_error() => ProviderErrorKind::Rejected,
            _ => match error {
                CompletionError::HttpError(_) => ProviderErrorKind::Unavailable,
                // Rig flattens SSE transport failures into provider strings.
                CompletionError::ProviderError(message)
                    if message.starts_with(RIG_TRANSPORT_ERROR_PREFIX)
                        || message == RIG_STREAM_ENDED =>
                {
                    ProviderErrorKind::Unavailable
                }
                CompletionError::ProviderError(_) => ProviderErrorKind::Unknown,
                CompletionError::JsonError(_) | CompletionError::ResponseError(_) => {
                    ProviderErrorKind::InvalidResponse
                }
                CompletionError::ProviderResponse(_) => ProviderErrorKind::Rejected,
                CompletionError::UrlError(_) | CompletionError::RequestError(_) => {
                    ProviderErrorKind::InvalidRequest
                }
            },
        }
    };
    ProviderErrorFacts {
        kind,
        status: status.map(|status| status.as_u16()),
        context_overflow: is_context_overflow(error),
    }
}

fn is_context_overflow(error: &CompletionError) -> bool {
    if error.provider_response_status().is_some_and(|status| {
        !status.is_success()
            && !matches!(
                status,
                StatusCode::BAD_REQUEST
                    | StatusCode::PAYLOAD_TOO_LARGE
                    | StatusCode::UNPROCESSABLE_ENTITY
            )
    }) {
        return false;
    }
    let Some(body) = error.provider_response_body() else {
        return false;
    };
    let message = body.to_lowercase();
    if ["rate limit", "too many requests", "throttling"]
        .iter()
        .any(|text| message.contains(text))
    {
        return false;
    }
    [
        "context_length_exceeded",
        "model_context_window_exceeded",
        "prompt is too long",
        "prompt too long",
        "exceeds the context window",
        "maximum context length",
        "exceeds the available context size",
    ]
    .iter()
    .any(|text| message.contains(text))
        || (message.contains("input token count") && message.contains("exceeds the maximum"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: None,
            ..Default::default()
        })]
        #[test]
        fn server_failures_never_trigger_context_compaction(
            status in 500u16..600,
            detail in ".{0,128}",
        ) {
            let status = StatusCode::from_u16(status).unwrap();
            let body = format!("context_length_exceeded: {detail}");
            let error = CompletionError::from_http_response(status, body);
            let facts = error_facts(&error);
            prop_assert_eq!(facts.kind, ProviderErrorKind::Unavailable);
            prop_assert_eq!(facts.status, Some(status.as_u16()));
            prop_assert!(!facts.context_overflow);
        }
    }

    #[test]
    fn status_classification_preserves_provider_evidence() {
        for (status, expected) in [
            (StatusCode::UNAUTHORIZED, ProviderErrorKind::Authentication),
            (StatusCode::FORBIDDEN, ProviderErrorKind::AccessDenied),
            (
                StatusCode::TOO_MANY_REQUESTS,
                ProviderErrorKind::RateLimited,
            ),
            (StatusCode::REQUEST_TIMEOUT, ProviderErrorKind::Unavailable),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ProviderErrorKind::Unavailable,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ProviderErrorKind::Unavailable,
            ),
            (StatusCode::BAD_REQUEST, ProviderErrorKind::Rejected),
        ] {
            let error = CompletionError::from_http_response(status, "provider failure");
            let facts = error_facts(&error);
            assert_eq!(facts.kind, expected);
            assert_eq!(facts.status, Some(status.as_u16()));
            assert!(!facts.context_overflow);
            assert_eq!(error.provider_response_body(), Some("provider failure"));
        }
    }

    #[test]
    fn ambiguous_errors_do_not_imply_transport_failure() {
        for (error, expected) in [
            (
                CompletionError::ProviderError("provider error".into()),
                ProviderErrorKind::Unknown,
            ),
            (
                CompletionError::ProviderError(
                    "Http client error: error decoding response body".into(),
                ),
                ProviderErrorKind::Unavailable,
            ),
            (
                CompletionError::ProviderError("Stream ended".into()),
                ProviderErrorKind::Unavailable,
            ),
            (
                CompletionError::ResponseError("invalid response".into()),
                ProviderErrorKind::InvalidResponse,
            ),
            (
                CompletionError::RequestError(anyhow::anyhow!("invalid request").into()),
                ProviderErrorKind::InvalidRequest,
            ),
        ] {
            let facts = error_facts(&error);
            assert_eq!(facts.kind, expected);
            assert_eq!(facts.status, None);
            assert!(!facts.context_overflow);
        }
    }

    #[test]
    fn overflow_detection_requires_explicit_provider_evidence() {
        for (status, body, expected) in [
            (StatusCode::OK, "context_length_exceeded", true),
            (StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", false),
            (StatusCode::BAD_REQUEST, "request_too_large", false),
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "image exceeds byte limit",
                false,
            ),
            (StatusCode::PAYLOAD_TOO_LARGE, "prompt too long", true),
            (StatusCode::BAD_REQUEST, "context_length_exceeded", true),
            (StatusCode::BAD_REQUEST, "prompt is too long", true),
            (
                StatusCode::BAD_REQUEST,
                "input token count exceeds the maximum",
                true,
            ),
            (StatusCode::BAD_REQUEST, "invalid input token count", false),
            (StatusCode::UNAUTHORIZED, "prompt is too long", false),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "context_length_exceeded",
                false,
            ),
            (
                StatusCode::BAD_REQUEST,
                "rate limit: context_length_exceeded",
                false,
            ),
        ] {
            let error = CompletionError::from_http_response(status, body);
            assert_eq!(
                error_facts(&error).context_overflow,
                expected,
                "{status}: {body}"
            );
        }
        assert!(
            !error_facts(&CompletionError::ResponseError(
                "context_length_exceeded".into()
            ))
            .context_overflow
        );
    }
}
