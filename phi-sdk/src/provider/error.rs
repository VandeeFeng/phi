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
    pub(crate) retry_after: Option<std::time::Duration>,
}

pub(crate) fn error_facts(error: &CompletionError) -> ProviderErrorFacts {
    let status = error.provider_response_status();
    let payload = error
        .provider_response_body()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok());
    let detail = payload
        .as_ref()
        .map(|value| value.get("error").unwrap_or(value));
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
                CompletionError::ProviderResponse(_) => detail
                    .and_then(stream_error_kind)
                    .unwrap_or(ProviderErrorKind::Rejected),
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
        retry_after: retry_after(error, detail),
    }
}

fn stream_error_kind(detail: &serde_json::Value) -> Option<ProviderErrorKind> {
    for identity in ["code", "type"]
        .iter()
        .filter_map(|key| detail.get(key)?.as_str())
    {
        match identity {
            "server_is_overloaded"
            | "overloaded_error"
            | "service_unavailable_error"
            | "server_error"
            | "internal_server_error" => return Some(ProviderErrorKind::Unavailable),
            "rate_limit_exceeded" | "rate_limit_error" => {
                return Some(ProviderErrorKind::RateLimited);
            }
            _ => {}
        }
    }
    None
}

fn retry_after(
    error: &CompletionError,
    detail: Option<&serde_json::Value>,
) -> Option<std::time::Duration> {
    let header = |name: &str| {
        error
            .provider_response_headers()
            .and_then(|headers| headers.get(name)?.to_str().ok())
            .or_else(|| detail?.get("headers")?.get(name)?.as_str())
    };
    for (name, units_per_second) in [("retry-after-ms", 1000.0), ("retry-after", 1.0)] {
        if let Some(delay) = header(name)
            .and_then(|value| value.parse::<f64>().ok())
            .and_then(|value| std::time::Duration::try_from_secs_f64(value / units_per_second).ok())
        {
            return Some(delay);
        }
    }
    let date = chrono::DateTime::parse_from_rfc2822(header("retry-after")?).ok()?;
    (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
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
    fn stream_overload_is_retryable_without_http_status() {
        let body = r#"{"type":"error","sequence_number":3,"error":{"message":"Our servers are currently overloaded. Please try again later.","type":"service_unavailable_error","param":null,"code":"server_is_overloaded","headers":{"retry-after":"309","retry-after-ms":"309000"}}}"#;
        for error in [
            CompletionError::from_http_response(StatusCode::OK, body),
            CompletionError::from_provider_body(body),
        ] {
            let facts = error_facts(&error);
            assert_eq!(facts.kind, ProviderErrorKind::Unavailable);
            assert_eq!(facts.retry_after, Some(std::time::Duration::from_secs(309)));
            assert!(!facts.context_overflow);
        }
    }

    #[test]
    fn structured_stream_errors_preserve_terminal_failures() {
        for (identity, expected) in [
            ("overloaded_error", ProviderErrorKind::Unavailable),
            ("server_error", ProviderErrorKind::Unavailable),
            ("rate_limit_error", ProviderErrorKind::RateLimited),
            ("insufficient_quota", ProviderErrorKind::Rejected),
            ("invalid_api_key", ProviderErrorKind::Rejected),
            ("unknown_error", ProviderErrorKind::Rejected),
        ] {
            let error = CompletionError::from_provider_body(
                serde_json::json!({"error":{"type":identity}}).to_string(),
            );
            assert_eq!(error_facts(&error).kind, expected);
        }
        let error = CompletionError::from_http_response(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"type":"server_error"}}"#,
        );
        assert_eq!(error_facts(&error).kind, ProviderErrorKind::Rejected);
    }

    #[test]
    fn retry_headers_use_milliseconds_then_seconds_then_date() {
        for (millis, seconds, expected) in [
            (Some("1500"), "309", Some(1500)),
            (Some("invalid"), "2.5", Some(2500)),
            (Some("-1"), "2", Some(2000)),
            (Some("NaN"), "2", Some(2000)),
            (Some("inf"), "2", Some(2000)),
            (None, "0", Some(0)),
            (None, "invalid", None),
            (None, "-1", None),
            (None, "Wed, 21 Oct 2015 07:28:00 GMT", None),
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert("retry-after", seconds.parse().unwrap());
            if let Some(millis) = millis {
                headers.insert("retry-after-ms", millis.parse().unwrap());
            }
            let error =
                CompletionError::from_http_response(StatusCode::SERVICE_UNAVAILABLE, "busy")
                    .with_response_headers(Some(Box::new(headers)));
            assert_eq!(
                error_facts(&error).retry_after,
                expected.map(std::time::Duration::from_millis)
            );
        }
        let date = chrono::Utc::now() + chrono::Duration::seconds(60);
        let error = CompletionError::from_provider_body(
            serde_json::json!({"error":{"headers":{"retry-after":date.to_rfc2822()}}}).to_string(),
        );
        let delay = error_facts(&error).retry_after.unwrap();
        assert!(delay > std::time::Duration::from_secs(58));
        assert!(delay <= std::time::Duration::from_secs(60));
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
