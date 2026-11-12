use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rmcp::model::{
    ElicitRequestParams, ElicitResult, ElicitationAction, ErrorData as McpError, RequestId,
};
use tokio::sync::{oneshot, watch};

use crate::core::event::CoreEventScope;

use crate::core::interaction::{
    McpElicitationInteraction, McpElicitationRequest, McpElicitationResponse, McpElicitationSender,
};

#[derive(Clone, Default)]
pub(super) struct McpElicitationRouter {
    inner: Arc<RouterInner>,
}

#[derive(Default)]
struct RouterInner {
    active: Mutex<HashMap<String, ActiveCall>>,
    call_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    next_generation: AtomicU64,
}

#[derive(Clone)]
struct ActiveCall {
    generation: u64,
    sender: Option<McpElicitationSender>,
    scope: CoreEventScope,
    parent_tool_call_id: Option<String>,
    cancelled: watch::Receiver<bool>,
}

pub(super) struct McpCallGuard {
    router: McpElicitationRouter,
    server: String,
    generation: u64,
    cancel: watch::Sender<bool>,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl McpElicitationRouter {
    pub(super) async fn begin_call(
        &self,
        server: &str,
        sender: Option<McpElicitationSender>,
        scope: CoreEventScope,
        parent_tool_call_id: Option<String>,
    ) -> McpCallGuard {
        let call_lock = {
            let mut locks = self.inner.call_locks.lock().await;
            locks
                .entry(server.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let lock = call_lock.lock_owned().await;
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancelled) = watch::channel(false);
        self.inner.active.lock().unwrap().insert(
            server.to_string(),
            ActiveCall {
                generation,
                sender,
                scope,
                parent_tool_call_id,
                cancelled,
            },
        );
        McpCallGuard {
            router: self.clone(),
            server: server.to_string(),
            generation,
            cancel,
            _lock: lock,
        }
    }

    pub(super) async fn elicit(
        &self,
        server: &str,
        request_id: RequestId,
        request: ElicitRequestParams,
    ) -> Result<ElicitResult, McpError> {
        let Some(active) = self.inner.active.lock().unwrap().get(server).cloned() else {
            return Ok(elicitation_result(McpElicitationResponse::Decline));
        };
        let Some(sender) = active.sender else {
            return Ok(elicitation_result(McpElicitationResponse::Decline));
        };
        let Some(mode) = request_mode(&request) else {
            return Ok(elicitation_result(McpElicitationResponse::Decline));
        };
        let request = project_request(server, request_id, active.parent_tool_call_id, request)?;
        let (reply, response) = oneshot::channel();
        if sender
            .send(McpElicitationInteraction {
                scope: active.scope,
                request,
                reply,
            })
            .await
            .is_err()
        {
            return Ok(elicitation_result(McpElicitationResponse::Decline));
        }
        let mut cancelled = active.cancelled;
        let response = tokio::select! {
            response = response => response.unwrap_or(McpElicitationResponse::Cancel),
            _ = wait_cancelled(&mut cancelled) => McpElicitationResponse::Cancel,
        };
        validate_response(mode, &response)?;
        Ok(elicitation_result(response))
    }
}

impl Drop for McpCallGuard {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        let mut active = self.router.inner.active.lock().unwrap();
        if active
            .get(&self.server)
            .is_some_and(|call| call.generation == self.generation)
        {
            active.remove(&self.server);
        }
    }
}

#[derive(Clone, Copy)]
enum ElicitationMode {
    Form,
    Url,
}

fn request_mode(request: &ElicitRequestParams) -> Option<ElicitationMode> {
    match request {
        ElicitRequestParams::FormElicitationParams { .. } => Some(ElicitationMode::Form),
        ElicitRequestParams::UrlElicitationParams { .. } => Some(ElicitationMode::Url),
        _ => None,
    }
}

fn project_request(
    server: &str,
    request_id: RequestId,
    parent_tool_call_id: Option<String>,
    request: ElicitRequestParams,
) -> Result<McpElicitationRequest, McpError> {
    let protocol_request_id = request_id.into_json_value();
    match request {
        ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => Ok(McpElicitationRequest::Form {
            server: server.to_string(),
            protocol_request_id,
            parent_tool_call_id,
            message,
            requested_schema: serde_json::to_value(requested_schema).map_err(|error| {
                McpError::invalid_params(format!("invalid elicitation schema: {error}"), None)
            })?,
        }),
        ElicitRequestParams::UrlElicitationParams {
            message,
            url,
            elicitation_id,
            ..
        } => Ok(McpElicitationRequest::Url {
            server: server.to_string(),
            protocol_request_id,
            parent_tool_call_id,
            message,
            url,
            elicitation_id,
        }),
        _ => Err(McpError::invalid_params(
            "unsupported elicitation mode",
            None,
        )),
    }
}

fn validate_response(
    mode: ElicitationMode,
    response: &McpElicitationResponse,
) -> Result<(), McpError> {
    let McpElicitationResponse::Accept { content } = response else {
        return Ok(());
    };
    match (mode, content) {
        (ElicitationMode::Form, Some(serde_json::Value::Object(_)))
        | (ElicitationMode::Url, None) => Ok(()),
        (ElicitationMode::Form, _) => Err(McpError::invalid_params(
            "accepted form elicitation requires object content",
            None,
        )),
        (ElicitationMode::Url, Some(_)) => Err(McpError::invalid_params(
            "accepted URL elicitation must not include content",
            None,
        )),
    }
}

fn elicitation_result(response: McpElicitationResponse) -> ElicitResult {
    match response {
        McpElicitationResponse::Accept { content } => match content {
            Some(content) => ElicitResult::new(ElicitationAction::Accept).with_content(content),
            None => ElicitResult::new(ElicitationAction::Accept),
        },
        McpElicitationResponse::Decline => ElicitResult::new(ElicitationAction::Decline),
        McpElicitationResponse::Cancel => ElicitResult::new(ElicitationAction::Cancel),
    }
}

async fn wait_cancelled(cancelled: &mut watch::Receiver<bool>) {
    if *cancelled.borrow() {
        return;
    }
    while cancelled.changed().await.is_ok() {
        if *cancelled.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::interaction::McpElicitationReceiver;
    use rmcp::model::{ElicitationSchema, NumberOrString};
    use tokio::sync::mpsc;

    fn form_request() -> ElicitRequestParams {
        ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Choose a value".to_string(),
            requested_schema: ElicitationSchema::builder().build().unwrap(),
        }
    }

    async fn active_router() -> (McpElicitationRouter, McpCallGuard, McpElicitationReceiver) {
        let router = McpElicitationRouter::default();
        let (sender, receiver) = mpsc::channel(4);
        let scope = CoreEventScope {
            session_id: Some("session".into()),
            turn_id: Some("turn".into()),
            ..CoreEventScope::default()
        };
        let guard = router
            .begin_call("server", Some(sender), scope, Some("tool-call".to_string()))
            .await;
        (router, guard, receiver)
    }

    #[tokio::test]
    async fn routes_form_accept_with_correlation() {
        let (router, _guard, mut receiver) = active_router().await;
        let task = tokio::spawn(async move {
            router
                .elicit("server", NumberOrString::Number(7), form_request())
                .await
        });

        let interaction = receiver.recv().await.unwrap();
        assert_eq!(interaction.scope.session_id(), Some("session"));
        assert_eq!(interaction.scope.turn_id(), Some("turn"));
        let McpElicitationRequest::Form {
            server,
            protocol_request_id,
            parent_tool_call_id,
            ..
        } = interaction.request
        else {
            panic!("expected form elicitation");
        };
        assert_eq!(server, "server");
        assert_eq!(protocol_request_id, serde_json::json!(7));
        assert_eq!(parent_tool_call_id.as_deref(), Some("tool-call"));
        interaction
            .reply
            .send(McpElicitationResponse::Accept {
                content: Some(serde_json::json!({ "value": "yes" })),
            })
            .unwrap();

        let result = task.await.unwrap().unwrap();
        assert_eq!(result.action, ElicitationAction::Accept);
        assert_eq!(result.content, Some(serde_json::json!({ "value": "yes" })));
    }

    #[tokio::test]
    async fn declines_unavailable_and_rejects_malformed_responses() {
        let router = McpElicitationRouter::default();
        let _guard = router
            .begin_call("server", None, CoreEventScope::default(), None)
            .await;
        let result = router
            .elicit("server", NumberOrString::Number(1), form_request())
            .await
            .unwrap();
        assert_eq!(result.action, ElicitationAction::Decline);
        drop(_guard);

        let (router, _guard, mut receiver) = active_router().await;
        let task = tokio::spawn(async move {
            router
                .elicit("server", NumberOrString::Number(1), form_request())
                .await
        });
        receiver
            .recv()
            .await
            .unwrap()
            .reply
            .send(McpElicitationResponse::Accept {
                content: Some(serde_json::json!("not an object")),
            })
            .unwrap();
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .message
                .contains("requires object content")
        );
    }

    #[tokio::test]
    async fn ending_a_call_cancels_its_pending_request() {
        let (router, guard, mut receiver) = active_router().await;
        let task = tokio::spawn(async move {
            router
                .elicit("server", NumberOrString::Number(1), form_request())
                .await
        });
        let _interaction = receiver.recv().await.unwrap();
        drop(guard);
        assert_eq!(
            task.await.unwrap().unwrap().action,
            ElicitationAction::Cancel
        );
    }

    #[tokio::test]
    async fn serializes_same_server_calls() {
        let router = McpElicitationRouter::default();
        let first = router
            .begin_call("server", None, CoreEventScope::default(), None)
            .await;
        let next_router = router.clone();
        let second = tokio::spawn(async move {
            next_router
                .begin_call("server", None, CoreEventScope::default(), None)
                .await
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        drop(first);
        let second = second.await.unwrap();
        drop(second);
    }
}
