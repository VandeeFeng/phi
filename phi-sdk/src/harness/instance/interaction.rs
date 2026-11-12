use crate::core::interaction::{
    InteractionAnswer, InteractionEnvelope, InteractionRequest, InteractionResponse,
    McpElicitationInteraction, McpElicitationReceiver, McpElicitationResponse, PermissionDecision,
    PermissionReceiver, PermissionRequest,
};
use crate::hooks::{HookRegistry, emit_permission_denied, emit_permission_request};
use std::collections::HashMap;

pub(crate) struct InteractionBroker {
    permission_rx: Option<PermissionReceiver>,
    elicitation_rx: Option<McpElicitationReceiver>,
    pending: HashMap<String, PendingInteraction>,
}

#[derive(Debug)]
enum PendingInteraction {
    Permission(PermissionRequest),
    McpElicitation(McpElicitationInteraction),
}

impl InteractionBroker {
    pub(crate) fn new(
        permission_rx: Option<PermissionReceiver>,
        elicitation_rx: Option<McpElicitationReceiver>,
    ) -> Self {
        Self {
            permission_rx,
            elicitation_rx,
            pending: HashMap::new(),
        }
    }

    pub(crate) async fn poll(&mut self, hooks: &HookRegistry) -> Option<InteractionEnvelope> {
        self.prune_closed();
        if let Some(request) = self
            .permission_rx
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            hooks.schedule_notification(crate::core::lifecycle::NotificationEvent {
                kind: crate::core::lifecycle::NotificationKind::PermissionPrompt,
                level: "info".into(),
                message: "Permission approval required".into(),
                tool_name: Some(request.permission.tool.clone()),
            });
            emit_permission_request(hooks, &request.permission.tool, &request.permission.input)
                .await;
            return Some(self.insert_pending(PendingInteraction::Permission(request)));
        }
        if let Some(request) = self
            .elicitation_rx
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            return Some(self.insert_pending(PendingInteraction::McpElicitation(request)));
        }
        None
    }

    pub(crate) async fn respond(
        &mut self,
        request_id: &str,
        response: InteractionResponse,
        hooks: &HookRegistry,
    ) -> anyhow::Result<InteractionAnswer> {
        let request = self
            .pending
            .remove(request_id)
            .ok_or_else(|| anyhow::anyhow!("unknown interaction request `{request_id}`"))?;
        match (request, response) {
            (
                PendingInteraction::Permission(request),
                InteractionResponse::Permission(decision),
            ) => answer_permission_request(request, decision, hooks).await,
            (
                PendingInteraction::McpElicitation(request),
                InteractionResponse::McpElicitation(response),
            ) => {
                let status = match response {
                    McpElicitationResponse::Accept { .. } => "MCP elicitation accepted",
                    McpElicitationResponse::Decline => "MCP elicitation declined",
                    McpElicitationResponse::Cancel => "MCP elicitation cancelled",
                };
                let _ = request.reply.send(response);
                Ok(InteractionAnswer {
                    status: status.to_string(),
                    allow_always: None,
                })
            }
            (request, _) => {
                self.pending.insert(request_id.to_string(), request);
                anyhow::bail!("interaction response type does not match request `{request_id}`")
            }
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.elicitation_rx
            .as_ref()
            .is_some_and(|receiver| !receiver.is_empty())
            || self
                .permission_rx
                .as_ref()
                .is_some_and(|receiver| !receiver.is_empty())
            || self.pending.values().any(|request| match request {
                PendingInteraction::Permission(request) => !request.reply.is_closed(),
                PendingInteraction::McpElicitation(request) => !request.reply.is_closed(),
            })
    }

    pub(crate) fn is_pending(&mut self, request_id: &str) -> bool {
        self.prune_closed();
        self.pending.contains_key(request_id)
    }

    pub(crate) fn deny_all(&mut self) {
        for (_, request) in self.pending.drain() {
            match request {
                PendingInteraction::Permission(request) => {
                    let _ = request.reply.send(PermissionDecision::Deny);
                }
                PendingInteraction::McpElicitation(request) => {
                    let _ = request.reply.send(McpElicitationResponse::Cancel);
                }
            }
        }
    }

    fn prune_closed(&mut self) {
        self.pending.retain(|_, request| match request {
            PendingInteraction::Permission(request) => !request.reply.is_closed(),
            PendingInteraction::McpElicitation(request) => !request.reply.is_closed(),
        });
    }

    fn insert_pending(&mut self, request: PendingInteraction) -> InteractionEnvelope {
        let request_id = uuid::Uuid::new_v4().to_string();
        let envelope = interaction_envelope(&request_id, &request);
        self.pending.insert(request_id, request);
        envelope
    }
}

impl Drop for InteractionBroker {
    fn drop(&mut self) {
        self.deny_all();
    }
}

fn interaction_envelope(request_id: &str, request: &PendingInteraction) -> InteractionEnvelope {
    let (scope, request) = match request {
        PendingInteraction::Permission(request) => (
            request.scope.clone(),
            InteractionRequest::Permission(request.permission.clone()),
        ),
        PendingInteraction::McpElicitation(request) => (
            request.scope.clone(),
            InteractionRequest::McpElicitation(request.request.clone()),
        ),
    };
    InteractionEnvelope {
        request_id: request_id.to_string(),
        emitted_at: chrono::Utc::now(),
        scope,
        request,
    }
}

async fn answer_permission_request(
    request: PermissionRequest,
    decision: PermissionDecision,
    hooks: &HookRegistry,
) -> anyhow::Result<InteractionAnswer> {
    let mut allow_always = None;
    let status = match &decision {
        PermissionDecision::AllowOnce => "permission allowed once",
        PermissionDecision::AllowAlways(pattern) => {
            allow_always = Some((request.permission.tool.clone(), pattern.clone()));
            "permission allowed always"
        }
        PermissionDecision::Deny => "permission denied",
    };
    if matches!(decision, PermissionDecision::Deny) {
        emit_permission_denied(hooks, &request.permission.tool, &request.permission.input).await;
    }
    request
        .reply
        .send(decision)
        .map_err(|_| anyhow::anyhow!("permission request is no longer active"))?;
    request
        .applied
        .await
        .map_err(|_| anyhow::anyhow!("permission decision was not applied"))?;
    Ok(InteractionAnswer {
        status: status.to_string(),
        allow_always,
    })
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use crate::core::event::CoreEventScope;
    use crate::core::interaction::PermissionInteraction;
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn permission_answer_waits_for_application_confirmation() {
        #[derive(Clone, Copy)]
        enum Application {
            Confirmed,
            Lost,
        }
        for decision in [
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowAlways("printf".into()),
            PermissionDecision::Deny,
        ] {
            for application in [Application::Confirmed, Application::Lost] {
                let (sender, receiver) = mpsc::channel(1);
                let (reply, response) = oneshot::channel();
                let (applied, confirmation) = oneshot::channel();
                sender
                    .send(PermissionRequest {
                        scope: CoreEventScope::default(),
                        permission: PermissionInteraction {
                            tool: "bash".into(),
                            input: "printf".into(),
                            preview: None,
                            origin: None,
                        },
                        reply,
                        applied: confirmation,
                    })
                    .await
                    .unwrap();
                let mut broker = InteractionBroker::new(Some(receiver), None);
                assert!(broker.has_pending());
                let hooks = HookRegistry::new();
                let request = broker.poll(&hooks).await.unwrap();
                assert!(broker.has_pending());
                let answer = broker.respond(
                    &request.request_id,
                    InteractionResponse::Permission(decision.clone()),
                    &hooks,
                );
                tokio::pin!(answer);
                assert!(futures::poll!(&mut answer).is_pending());
                let received = response.await.unwrap();
                assert_eq!(
                    std::mem::discriminant(&received),
                    std::mem::discriminant(&decision)
                );
                if let PermissionDecision::AllowAlways(pattern) = received {
                    assert_eq!(pattern, "printf");
                }
                match application {
                    Application::Confirmed => {
                        applied.send(()).unwrap();
                        let answer = answer.await.unwrap();
                        let (status, allowance) = match &decision {
                            PermissionDecision::AllowOnce => ("permission allowed once", None),
                            PermissionDecision::AllowAlways(pattern) => (
                                "permission allowed always",
                                Some(("bash".into(), pattern.clone())),
                            ),
                            PermissionDecision::Deny => ("permission denied", None),
                        };
                        assert_eq!(answer.status, status);
                        assert_eq!(answer.allow_always, allowance);
                    }
                    Application::Lost => {
                        drop(applied);
                        assert_eq!(
                            answer.await.err().unwrap().to_string(),
                            "permission decision was not applied"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn closed_permission_reply_cannot_report_an_applied_grant() {
        let (sender, receiver) = mpsc::channel(1);
        let (reply, response) = oneshot::channel();
        let (_applied, applied) = oneshot::channel();
        sender
            .send(PermissionRequest {
                scope: CoreEventScope::default(),
                permission: PermissionInteraction {
                    tool: "bash".into(),
                    input: "printf".into(),
                    preview: None,
                    origin: None,
                },
                reply,
                applied,
            })
            .await
            .unwrap();
        let mut broker = InteractionBroker::new(Some(receiver), None);
        let hooks = HookRegistry::new();
        let request = broker.poll(&hooks).await.unwrap();
        drop(response);
        assert!(
            broker
                .respond(
                    &request.request_id,
                    InteractionResponse::Permission(PermissionDecision::AllowAlways(
                        "printf".into()
                    )),
                    &hooks,
                )
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::CoreEventScope;
    use crate::core::interaction::McpElicitationRequest;
    use crate::core::interaction::PermissionInteraction;
    use tokio::sync::{mpsc, oneshot};

    fn elicitation() -> McpElicitationRequest {
        McpElicitationRequest::Form {
            server: "server".to_string(),
            protocol_request_id: serde_json::json!(1),
            parent_tool_call_id: Some("tool-call".to_string()),
            message: "input".to_string(),
            requested_schema: serde_json::json!({ "type": "object" }),
        }
    }

    #[tokio::test]
    async fn mismatched_replies_preserve_pending_requests() {
        const RESPONSE_RETRIES: usize = 3;
        for response in [
            InteractionResponse::Permission(PermissionDecision::AllowOnce),
            InteractionResponse::Permission(PermissionDecision::AllowAlways("printf".into())),
            InteractionResponse::Permission(PermissionDecision::Deny),
            InteractionResponse::McpElicitation(McpElicitationResponse::Accept { content: None }),
            InteractionResponse::McpElicitation(McpElicitationResponse::Accept {
                content: Some(serde_json::json!({ "value": "yes" })),
            }),
            InteractionResponse::McpElicitation(McpElicitationResponse::Decline),
            InteractionResponse::McpElicitation(McpElicitationResponse::Cancel),
        ] {
            let (permission_sender, permission_receiver) = mpsc::channel(1);
            let (permission_reply, mut permission_response) = oneshot::channel();
            let (_applied, confirmation) = oneshot::channel();
            permission_sender
                .send(PermissionRequest {
                    scope: CoreEventScope::default(),
                    permission: PermissionInteraction {
                        tool: "bash".into(),
                        input: "printf".into(),
                        preview: None,
                        origin: None,
                    },
                    reply: permission_reply,
                    applied: confirmation,
                })
                .await
                .unwrap();
            let (mcp_sender, mcp_receiver) = mpsc::channel(1);
            let (mcp_reply, mut mcp_response) = oneshot::channel();
            mcp_sender
                .send(McpElicitationInteraction {
                    scope: CoreEventScope::default(),
                    request: elicitation(),
                    reply: mcp_reply,
                })
                .await
                .unwrap();
            let mut broker = InteractionBroker::new(Some(permission_receiver), Some(mcp_receiver));
            assert!(broker.has_pending());
            let hooks = HookRegistry::new();
            let permission = broker.poll(&hooks).await.unwrap();
            let mcp = broker.poll(&hooks).await.unwrap();
            assert!(matches!(
                permission.request,
                InteractionRequest::Permission(_)
            ));
            assert!(matches!(mcp.request, InteractionRequest::McpElicitation(_)));
            let request_id = match &response {
                InteractionResponse::Permission(_) => &mcp.request_id,
                InteractionResponse::McpElicitation(_) => &permission.request_id,
            };
            for _ in 0..RESPONSE_RETRIES {
                assert!(
                    broker
                        .respond(request_id, response.clone(), &hooks)
                        .await
                        .is_err()
                );
                assert!(broker.is_pending(&permission.request_id));
                assert!(broker.is_pending(&mcp.request_id));
                assert!(matches!(
                    permission_response.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
                assert!(matches!(
                    mcp_response.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
            }
            drop(broker);
            assert!(matches!(
                permission_response.await.unwrap(),
                PermissionDecision::Deny
            ));
            assert_eq!(mcp_response.await.unwrap(), McpElicitationResponse::Cancel);
        }
    }

    #[tokio::test]
    async fn routes_and_cancels_mcp_without_permission_state() {
        let (sender, receiver) = mpsc::channel(1);
        let (reply, response) = oneshot::channel();
        sender
            .send(McpElicitationInteraction {
                scope: CoreEventScope::default(),
                request: elicitation(),
                reply,
            })
            .await
            .unwrap();
        let mut broker = InteractionBroker::new(None, Some(receiver));
        let envelope = broker.poll(&HookRegistry::new()).await.unwrap();
        assert!(matches!(
            envelope.request,
            InteractionRequest::McpElicitation(_)
        ));

        let answer = broker
            .respond(
                &envelope.request_id,
                InteractionResponse::McpElicitation(McpElicitationResponse::Decline),
                &HookRegistry::new(),
            )
            .await
            .unwrap();

        assert_eq!(response.await.unwrap(), McpElicitationResponse::Decline);
        assert!(answer.allow_always.is_none());

        let (sender, receiver) = mpsc::channel(1);
        let (reply, response) = oneshot::channel();
        sender
            .send(McpElicitationInteraction {
                scope: CoreEventScope::default(),
                request: elicitation(),
                reply,
            })
            .await
            .unwrap();
        let mut broker = InteractionBroker::new(None, Some(receiver));
        broker.poll(&HookRegistry::new()).await.unwrap();
        drop(broker);
        assert_eq!(response.await.unwrap(), McpElicitationResponse::Cancel);

        let (sender, receiver) = mpsc::channel(1);
        let (reply, response) = oneshot::channel();
        sender
            .send(McpElicitationInteraction {
                scope: CoreEventScope::default(),
                request: elicitation(),
                reply,
            })
            .await
            .unwrap();
        let mut broker = InteractionBroker::new(None, Some(receiver));
        let envelope = broker.poll(&HookRegistry::new()).await.unwrap();
        drop(response);
        assert!(!broker.is_pending(&envelope.request_id));
    }
}
