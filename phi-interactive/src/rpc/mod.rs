mod controls;
mod io;
mod messages;
mod observations;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use phi_sdk::agent_instance::{AgentInstanceEvent, AgentInstanceId};
use phi_sdk::interaction::{InteractionEnvelope, InteractionRequest};
use phi_sdk::turn::{SubmissionDispatch, SubmissionDisposition};
use phi_sdk::{PhiEvent, PhiFailureKind};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::runtime::preparation::ModelChoices;
use crate::runtime::session::{AgentSession, SessionInteractionAnswer};
use crate::runtime::{RuntimePreparation, SessionAssociations};

use io::{Connection, Frame};
use messages::{CloseMode, Code, Control, Dispatch, Fault, Incoming, Outgoing, VERSION};
use observations::Observation;

const MAX_CONTROLS: usize = 64;
const MAX_QUESTIONS: usize = 64;

struct Question {
    agent: Arc<AgentSession>,
    envelope: InteractionEnvelope,
    answering: bool,
}

enum Completion {
    Initialized(String, Result<Arc<AgentSession>, Fault>),
    Admission(
        String,
        Result<crate::runtime::session::SessionSubmission, anyhow::Error>,
    ),
    Control(String, Result<Value, Fault>),
    Answer(String, Result<SessionInteractionAnswer, Fault>),
    Quiescent(anyhow::Result<()>),
    Barrier {
        id: String,
        agent: Arc<AgentSession>,
        result: Result<(), phi_sdk::agent_instance::AgentInstanceError>,
    },
}

pub(crate) struct RpcSession {
    connection: Connection,
    sessions: Arc<SessionAssociations>,
    preparation: Arc<RuntimePreparation>,
    models: ModelChoices,
    root: Option<Arc<AgentSession>>,
    initializing: bool,
    input_id: Option<String>,
    controls: HashSet<String>,
    interrupt_id: Option<String>,
    workers: JoinSet<Completion>,
    boundaries: JoinSet<Completion>,
    streams: JoinSet<Result<(), Fault>>,
    observations: mpsc::Receiver<Observation>,
    observed: mpsc::Sender<Observation>,
    bindings: HashMap<AgentInstanceId, Arc<AgentSession>>,
    task_ids: HashMap<AgentInstanceId, String>,
    stream_stops: HashMap<AgentInstanceId, oneshot::Sender<()>>,
    questions: HashMap<String, Question>,
    last_terminal: HashMap<AgentInstanceId, String>,
    close: Option<CloseMode>,
    stdin_available: bool,
    explicit_close: bool,
    quiescence_started: bool,
    quiescence_complete: bool,
    root_boundary: Option<String>,
    barriers: HashSet<String>,
    flushing: bool,
    execution_failed: bool,
    failure: Option<Fault>,
    output_failed: bool,
}

impl RpcSession {
    pub(crate) fn open(
        sessions: Arc<SessionAssociations>,
        preparation: Arc<RuntimePreparation>,
        models: ModelChoices,
    ) -> anyhow::Result<Self> {
        Self::new(Connection::open()?, sessions, preparation, models)
    }

    fn new(
        connection: Connection,
        sessions: Arc<SessionAssociations>,
        preparation: Arc<RuntimePreparation>,
        models: ModelChoices,
    ) -> anyhow::Result<Self> {
        let (observed, observations) = mpsc::channel(io::CAPACITY);
        Ok(Self {
            connection,
            sessions,
            preparation,
            models,
            root: None,
            initializing: false,
            input_id: None,
            controls: HashSet::new(),
            interrupt_id: None,
            workers: JoinSet::new(),
            boundaries: JoinSet::new(),
            streams: JoinSet::new(),
            observations,
            observed,
            bindings: HashMap::new(),
            task_ids: HashMap::new(),
            stream_stops: HashMap::new(),
            questions: HashMap::new(),
            last_terminal: HashMap::new(),
            close: None,
            stdin_available: true,
            explicit_close: false,
            quiescence_started: false,
            quiescence_complete: false,
            root_boundary: None,
            barriers: HashSet::new(),
            flushing: false,
            execution_failed: false,
            failure: None,
            output_failed: false,
        })
    }

    pub(crate) async fn run(&mut self) {
        if let Err(error) = self.control_loop().await {
            self.fail(error);
        }
    }

    async fn control_loop(&mut self) -> Result<(), Fault> {
        loop {
            if self.close == Some(CloseMode::Cancel) {
                return Ok(());
            }
            self.check_input_reachability()?;
            self.begin_drain();
            if self.flushing
                && self.workers.is_empty()
                && self.boundaries.is_empty()
                && self.barriers.is_empty()
                && self.observations.is_empty()
            {
                return Ok(());
            }
            self.step().await?;
        }
    }

    fn report_notices(&mut self) -> Result<(), Fault> {
        for notices in self.sessions.take_notices().map_err(Fault::operation)? {
            for notice in notices.hook_notices {
                self.emit(Outgoing::Notice {
                    code: Code::HookNotice,
                    message: serde_json::to_string(&notice).map_err(Fault::operation)?,
                    agent_id: Some(notices.agent_id.to_string()),
                })?;
            }
            for message in notices.extension_notifications {
                self.emit(Outgoing::Notice {
                    code: Code::ExtensionNotification,
                    message,
                    agent_id: Some(notices.agent_id.to_string()),
                })?;
            }
        }
        Ok(())
    }

    fn pending_application_work(&self) -> bool {
        !self.workers.is_empty()
    }

    fn begin_drain(&mut self) {
        if self.close != Some(CloseMode::Drain) || self.pending_application_work() {
            return;
        }
        if !self.quiescence_started {
            self.quiescence_started = true;
            if let Some(root) = &self.root {
                let root = root.clone();
                let boundary = uuid::Uuid::new_v4().to_string();
                self.root_boundary = Some(boundary.clone());
                let sessions = self.sessions.clone();
                self.boundaries.spawn(async move {
                    Completion::Quiescent(sessions.wait_for_quiescence(&root, Some(boundary)).await)
                });
            } else {
                self.quiescence_complete = true;
            }
        }
        if self.quiescence_complete && self.root_boundary.is_none() && !self.flushing {
            self.flushing = true;
            for binding in self.bindings.values().cloned().collect::<Vec<_>>() {
                self.barrier(binding);
            }
        }
    }

    fn barrier(&mut self, binding: Arc<AgentSession>) {
        let id = uuid::Uuid::new_v4().to_string();
        self.barriers.insert(id.clone());
        self.boundaries.spawn(async move {
            let result = binding
                .instance()
                .view_with_event_boundary(id.clone())
                .await
                .map(|_| ());
            Completion::Barrier {
                id,
                agent: binding,
                result,
            }
        });
    }

    async fn step(&mut self) -> Result<(), Fault> {
        self.report_notices()?;
        tokio::select! {
            frame = self.connection.receive(), if !self.output_failed => self.frame(frame?).await,
            completion = self.workers.join_next(), if !self.workers.is_empty() => {
                self.completed(completion.expect("worker exists").map_err(|error| Fault::new(Code::OperationFailed, error))?).await
            }
            completion = self.boundaries.join_next(), if !self.boundaries.is_empty() => {
                self.completed(completion.expect("boundary exists").map_err(|error| Fault::new(Code::ObservationLost, error))?).await
            }
            observation = self.observations.recv() => {
                self.observe(observation.ok_or_else(|| Fault::new(Code::ObservationLost, "observation channel closed"))?)
            }
            completion = self.streams.join_next(), if !self.streams.is_empty() => {
                completion.expect("observer exists").map_err(|error| Fault::new(Code::ObservationLost, error))?
            }
        }
    }

    async fn frame(&mut self, frame: Option<Frame>) -> Result<(), Fault> {
        match frame {
            None => {
                self.stdin_available = false;
                if self.close.is_none() {
                    self.close = Some(CloseMode::Drain);
                }
                Ok(())
            }
            Some(Frame::Failed(error)) => Err(Fault::new(Code::InputFailed, error)),
            Some(Frame::Invalid(error)) => {
                self.emit(Outgoing::notice(Fault::new(Code::InvalidMessage, error)))
            }
            Some(Frame::Message(message)) => self.message(message).await,
        }
    }

    async fn message(&mut self, message: Incoming) -> Result<(), Fault> {
        match message {
            Incoming::Input {
                input_id,
                prompt,
                dispatch,
                directed,
            } => {
                let error = if self.close.is_some() {
                    Some(Fault::new(Code::Closing, "input admission is closed"))
                } else if self.root.is_none() {
                    Some(Fault::new(Code::NotReady, "initialize first"))
                } else if self.input_id.as_ref() == Some(&input_id) {
                    Some(Fault::new(Code::DuplicateId, "input ID is pending"))
                } else if self.input_id.is_some() {
                    Some(Fault::new(Code::Busy, "another input is preparing"))
                } else if directed.is_some() && dispatch != Dispatch::Immediate {
                    Some(Fault::new(
                        Code::InvalidMessage,
                        "directed input requires immediate dispatch",
                    ))
                } else {
                    None
                };
                if let Some(error) = error {
                    return self.emit(Outgoing::Admission {
                        input_id,
                        disposition: json!({"kind":"rejected","error":error}),
                    });
                }
                let root = self.root.as_ref().expect("ready root").clone();
                self.input_id = Some(input_id.clone());
                self.workers.spawn(async move {
                    let directed = directed.map(|directed| phi_sdk::turn::DirectedDelegation {
                        agent: directed.agent,
                        mode: None,
                    });
                    let dispatch = match dispatch {
                        Dispatch::Immediate => None,
                        Dispatch::Steer => Some(SubmissionDispatch::Steer),
                        Dispatch::FollowUp => Some(SubmissionDispatch::FollowUp),
                    };
                    Completion::Admission(
                        input_id,
                        root.submit(prompt.into(), directed, dispatch).await,
                    )
                });
                Ok(())
            }
            Incoming::Control {
                control_id,
                request,
            } => self.control(control_id, request).await,
            Incoming::Answer {
                question_id,
                response,
            } => {
                let error = if let Some(question) = self.questions.get(&question_id) {
                    if question.answering {
                        Some(Fault::new(Code::Busy, "answer is pending"))
                    } else {
                        None
                    }
                } else {
                    Some(Fault::new(
                        Code::InteractionClosed,
                        "question is unknown or retired",
                    ))
                };
                if let Some(error) = error {
                    return self.reject_answer(question_id, error);
                }
                let response = match response.into_response() {
                    Ok(response) => response,
                    Err(error) => return self.reject_answer(question_id, error),
                };
                let question = self
                    .questions
                    .get_mut(&question_id)
                    .expect("pending question");
                question.answering = true;
                let agent = question.agent.clone();
                let request_id = question.envelope.request_id.clone();
                self.workers.spawn(async move {
                    Completion::Answer(
                        question_id,
                        agent
                            .respond_interaction(request_id, response)
                            .await
                            .map_err(Fault::operation),
                    )
                });
                Ok(())
            }
        }
    }

    async fn control(&mut self, control_id: String, request: Control) -> Result<(), Fault> {
        if self.controls.contains(&control_id) || self.interrupt_id.as_ref() == Some(&control_id) {
            return self.emit(Outgoing::control(
                control_id,
                Err(Fault::new(Code::DuplicateId, "control ID is pending")),
            ));
        }
        if let Control::Close { mode } = request {
            self.explicit_close = true;
            if self.close != Some(CloseMode::Cancel) {
                self.close = Some(mode);
            }
            return self.emit(Outgoing::control(
                control_id,
                Ok(json!({"mode":self.close})),
            ));
        }
        if self.close.is_some() && !request.during_drain() {
            return self.emit(Outgoing::control(
                control_id,
                Err(Fault::new(
                    Code::Closing,
                    "ordinary control admission is closed",
                )),
            ));
        }
        if let Control::Initialize { version, startup } = request {
            let error = if version != VERSION {
                Some(Fault::new(
                    Code::UnsupportedVersion,
                    "expected protocol version 2",
                ))
            } else if self.root.is_some() {
                Some(Fault::new(
                    Code::InvalidMessage,
                    "root is already initialized",
                ))
            } else if self.initializing {
                Some(Fault::new(Code::Busy, "initialization is pending"))
            } else {
                None
            };
            if let Some(error) = error {
                return self.emit(Outgoing::control(control_id, Err(error)));
            }
            self.initializing = true;
            self.controls.insert(control_id.clone());
            let sessions = self.sessions.clone();
            let preparation = self.preparation.clone();
            self.workers.spawn(async move {
                Completion::Initialized(
                    control_id,
                    controls::initialize(sessions, preparation, startup).await,
                )
            });
            return Ok(());
        }
        let Some(root) = self.root.clone() else {
            return self.emit(Outgoing::control(
                control_id,
                Err(Fault::new(Code::NotReady, "initialize first")),
            ));
        };
        if matches!(request, Control::Interrupt) {
            if self.interrupt_id.is_some() {
                return self.emit(Outgoing::control(
                    control_id,
                    Err(Fault::new(Code::Busy, "interrupt is pending")),
                ));
            }
            self.interrupt_id = Some(control_id.clone());
        } else {
            if self.controls.len() >= MAX_CONTROLS {
                return self.emit(Outgoing::control(
                    control_id,
                    Err(Fault::new(Code::Busy, "control capacity reached")),
                ));
            }
            self.controls.insert(control_id.clone());
        }
        let sessions = self.sessions.clone();
        let preparation = self.preparation.clone();
        let models = self.models.clone();
        self.workers.spawn(async move {
            Completion::Control(
                control_id,
                controls::execute(root, sessions, preparation, models, request).await,
            )
        });
        Ok(())
    }

    async fn completed(&mut self, completion: Completion) -> Result<(), Fault> {
        match completion {
            Completion::Initialized(id, result) => {
                self.initializing = false;
                self.controls.remove(&id);
                match result {
                    Ok(root) => {
                        self.attach(root.clone())?;
                        let view = root.current_runtime_view().await;
                        self.root = Some(root.clone());
                        self.emit(Outgoing::control(id, Ok(json!({"version":VERSION,"session_id":root.instance().id(),"runtime":controls::identity(&view)}))))
                    }
                    Err(error) => {
                        self.emit(Outgoing::control(id, Err(error.clone())))?;
                        if self.sessions.has_associations().map_err(Fault::operation)? {
                            return Err(error);
                        }
                        Ok(())
                    }
                }
            }
            Completion::Admission(id, result) => {
                self.input_id = None;
                let disposition = match result {
                    Ok(submission) => {
                        for notice in submission.notices {
                            self.emit(Outgoing::Notice {
                                code: Code::HookNotice,
                                message: serde_json::to_string(&notice)
                                    .map_err(Fault::operation)?,
                                agent_id: self
                                    .root
                                    .as_ref()
                                    .map(|root| root.instance().id().to_string()),
                            })?;
                        }
                        match submission.disposition {
                            SubmissionDisposition::Started { turn_id, .. } => {
                                json!({"kind":"started","turn_id":turn_id})
                            }
                            SubmissionDisposition::Queued {
                                submission_id,
                                dispatch,
                                ..
                            } => {
                                json!({"kind":"queued","submission_id":submission_id,"dispatch":dispatch.as_str()})
                            }
                            SubmissionDisposition::Handled { reason } => {
                                json!({"kind":"handled","reason":reason})
                            }
                        }
                    }
                    Err(error) => {
                        if error.is::<tokio::task::JoinError>()
                            || error
                                .downcast_ref::<phi_sdk::agent_instance::AgentInstanceError>()
                                .is_some_and(|error| {
                                    matches!(
                                        error,
                                        phi_sdk::agent_instance::AgentInstanceError::Unavailable
                                    )
                                })
                        {
                            return Err(Fault::new(Code::ExecutionFailed, error));
                        }
                        let code = if error
                            .downcast_ref::<phi_sdk::agent_instance::AgentInstanceError>()
                            .is_some_and(|error| {
                                matches!(
                                    error,
                                    phi_sdk::agent_instance::AgentInstanceError::Busy
                                        | phi_sdk::agent_instance::AgentInstanceError::Turn(
                                            phi_sdk::turn::PhiTurnError::Busy
                                        )
                                )
                            }) {
                            Code::Busy
                        } else {
                            Code::OperationFailed
                        };
                        json!({"kind":"rejected","error":Fault::new(code,error)})
                    }
                };
                self.emit(Outgoing::Admission {
                    input_id: id,
                    disposition,
                })
            }
            Completion::Control(id, result) => {
                self.controls.remove(&id);
                if self.interrupt_id.as_ref() == Some(&id) {
                    self.interrupt_id = None;
                }
                self.emit(Outgoing::control(id, result))
            }
            Completion::Answer(id, result) => {
                self.questions.remove(&id);
                match result {
                    Ok(answer) => self.emit(Outgoing::QuestionClosed {
                        question_id: id,
                        status: "answered",
                        error: None,
                        persistence_warning: answer.persistence_error,
                    }),
                    Err(error) => self.emit(Outgoing::QuestionClosed {
                        question_id: id,
                        status: "expired",
                        error: Some(error),
                        persistence_warning: None,
                    }),
                }
            }
            Completion::Quiescent(result) => {
                // Cancel supersedes drain; main-owned finalization now certifies cleanup.
                if self.close != Some(CloseMode::Cancel) {
                    result.map_err(|error| Fault::new(Code::ObservationLost, error))?;
                }
                self.quiescence_complete = true;
                Ok(())
            }
            Completion::Barrier { id, agent, result } => {
                if matches!(
                    result,
                    Err(phi_sdk::agent_instance::AgentInstanceError::Stopped)
                ) && (self.close == Some(CloseMode::Cancel) || !self.is_root(&agent))
                {
                    self.barriers.remove(&id);
                    Ok(())
                } else {
                    result.map_err(|error| Fault::new(Code::ObservationLost, error))
                }
            }
        }
    }

    fn attach(&mut self, agent: Arc<AgentSession>) -> Result<(), Fault> {
        self.attach_visible(agent, observations::Visibility::Execution)
    }

    fn attach_visible(
        &mut self,
        agent: Arc<AgentSession>,
        visibility: observations::Visibility,
    ) -> Result<(), Fault> {
        let id = agent.instance().id().clone();
        if self
            .bindings
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &agent))
        {
            return Ok(());
        }
        let (stop, stopping) = oneshot::channel();
        self.task_ids.remove(&id);
        self.stream_stops.insert(id.clone(), stop);
        self.bindings.insert(id, agent.clone());
        self.streams.spawn(observations::subscribe(
            agent.clone(),
            self.sessions.clone(),
            self.observed.clone(),
            stopping,
            visibility,
        ));
        if self.flushing {
            self.barrier(agent);
        }
        Ok(())
    }

    fn observe(&mut self, observation: Observation) -> Result<(), Fault> {
        match observation {
            Observation::Children(children) => {
                for (child, task_id, visibility) in children {
                    let id = child.instance().id().clone();
                    self.attach_visible(child, visibility)?;
                    if let Some(task_id) = task_id {
                        self.task_ids.insert(id, task_id);
                    }
                }
                Ok(())
            }
            Observation::Workflow(agent, payload) => self.emit(Outgoing::Event {
                agent_id: agent.instance().id().to_string(),
                task_id: None,
                payload,
            }),
            Observation::Event(agent, event) => match event {
                AgentInstanceEvent::Phi(envelope) => match &envelope.event {
                    PhiEvent::TurnFinished {
                        response,
                        display,
                        usage,
                        compacted,
                    } => {
                        let turn_id = envelope.turn_id.clone().ok_or_else(|| {
                            Fault::new(
                                Code::ObservationLost,
                                "terminal result has no turn identity",
                            )
                        })?;
                        self.outcome(&agent,turn_id,json!({"status":"completed","response":response,"display":display,"usage":usage,"compacted":compacted}))
                    }
                    PhiEvent::TurnFailed { failure } => {
                        let cancelled = failure.kind == PhiFailureKind::Cancelled;
                        if !cancelled && self.is_root(&agent) {
                            self.execution_failed = true;
                        }
                        let turn_id = envelope.turn_id.clone().ok_or_else(|| {
                            Fault::new(Code::ObservationLost, "failure has no turn identity")
                        })?;
                        self.outcome(
                            &agent,
                            turn_id,
                            if cancelled {
                                json!({"status":"cancelled","cancellation_reason":failure.cancellation_reason,"failure":failure})
                            } else {
                                json!({"status":"failed","failure":failure})
                            },
                        )
                    }
                    event => {
                        if matches!(
                            event,
                            PhiEvent::SubmissionDiscarded {
                                reason: phi_sdk::turn::SubmissionDiscardReason::DispatchFailed,
                                ..
                            }
                        ) && self.is_root(&agent)
                        {
                            self.execution_failed = true;
                        }
                        self.emit(Outgoing::Event {
                            agent_id: agent.instance().id().to_string(),
                            task_id: self.task_id(&agent, envelope.turn_id.as_deref()),
                            payload: json!({"kind":"execution","envelope":envelope}),
                        })
                    }
                },
                AgentInstanceEvent::Progress(progress) => self.emit(Outgoing::Event {
                    agent_id: agent.instance().id().to_string(),
                    task_id: self.task_id(&agent, Some(&progress.turn_id)),
                    payload: json!({"kind":"progress","progress":progress}),
                }),
                AgentInstanceEvent::Subagent(activity) => self.emit(Outgoing::Event {
                    agent_id: agent.instance().id().to_string(),
                    task_id: None,
                    payload: json!({"kind":"task","activity":activity}),
                }),
                AgentInstanceEvent::RunCancelled {
                    turn_id: Some(turn_id),
                    reason,
                } => self.outcome(
                    &agent,
                    turn_id,
                    json!({"status":"cancelled", "cancellation_reason":reason}),
                ),
                AgentInstanceEvent::RunCancelled { turn_id: None, .. } => {
                    self.emit(Outgoing::Notice {
                        code: Code::OperationFailed,
                        message: "idle execution interrupted".into(),
                        agent_id: Some(agent.instance().id().to_string()),
                    })
                }
                AgentInstanceEvent::InteractionRequested(envelope) => {
                    self.question(agent, envelope)
                }
                AgentInstanceEvent::InteractionClosed { request_id } => {
                    let id = self
                        .questions
                        .iter()
                        .find(|(_, question)| {
                            Arc::ptr_eq(&question.agent, &agent)
                                && question.envelope.request_id == request_id
                                && !question.answering
                        })
                        .map(|(id, _)| id.clone());
                    if let Some(id) = id {
                        self.questions.remove(&id);
                        self.emit(Outgoing::QuestionClosed {
                            question_id: id,
                            status: "cancelled",
                            error: None,
                            persistence_warning: None,
                        })?;
                    }
                    Ok(())
                }
                AgentInstanceEvent::ViewBoundary { id } => {
                    if self.root_boundary.as_ref() == Some(&id) {
                        self.root_boundary = None;
                    }
                    self.barriers.remove(&id);
                    Ok(())
                }
                AgentInstanceEvent::PumpFailed { message } => {
                    Err(Fault::new(Code::ObservationLost, message))
                }
                AgentInstanceEvent::Stopped => {
                    self.bindings.remove(agent.instance().id());
                    self.task_ids.remove(agent.instance().id());
                    self.stream_stops.remove(agent.instance().id());
                    self.last_terminal.remove(agent.instance().id());
                    Ok(())
                }
                _ => Ok(()),
            },
        }
    }

    fn task_id(&self, agent: &Arc<AgentSession>, turn_id: Option<&str>) -> Option<String> {
        self.bindings
            .get(agent.instance().id())
            .filter(|current| Arc::ptr_eq(current, agent))
            .and_then(|_| self.task_ids.get(agent.instance().id()))
            .filter(|id| Some(id.as_str()) == turn_id)
            .cloned()
    }

    fn is_root(&self, agent: &Arc<AgentSession>) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| Arc::ptr_eq(root, agent))
    }

    fn outcome(
        &mut self,
        agent: &Arc<AgentSession>,
        turn_id: String,
        result: Value,
    ) -> Result<(), Fault> {
        if self.last_terminal.get(agent.instance().id()) == Some(&turn_id) {
            return Ok(());
        }
        self.last_terminal
            .insert(agent.instance().id().clone(), turn_id.clone());
        self.emit(Outgoing::Outcome {
            agent_id: agent.instance().id().to_string(),
            task_id: self.task_id(agent, Some(&turn_id)),
            turn_id,
            result,
        })
    }

    fn question(
        &mut self,
        agent: Arc<AgentSession>,
        envelope: InteractionEnvelope,
    ) -> Result<(), Fault> {
        if self.questions.values().any(|question| {
            Arc::ptr_eq(&question.agent, &agent)
                && question.envelope.request_id == envelope.request_id
        }) {
            return Ok(());
        }
        if self.questions.len() >= MAX_QUESTIONS {
            return Err(Fault::new(
                Code::ObservationLost,
                "pending question capacity reached",
            ));
        }
        let question_id = uuid::Uuid::new_v4().to_string();
        let InteractionRequest::Permission(permission) = &envelope.request else {
            return Err(Fault::new(
                Code::OperationFailed,
                "interaction is unsupported in RPC mode",
            ));
        };
        let request = json!({"kind":"permission","tool":permission.tool,"input":permission.input,"preview":permission.preview,"origin":permission.origin});
        self.emit(Outgoing::Question {
            question_id: question_id.clone(),
            agent_id: agent.instance().id().to_string(),
            task_id: self.task_id(&agent, envelope.scope.turn_id.as_deref()),
            turn_id: envelope.scope.turn_id.as_ref().map(ToString::to_string),
            request,
        })?;
        self.questions.insert(
            question_id,
            Question {
                agent,
                envelope,
                answering: false,
            },
        );
        Ok(())
    }

    fn reject_answer(&mut self, id: String, error: Fault) -> Result<(), Fault> {
        self.emit(Outgoing::QuestionClosed {
            question_id: id,
            status: "rejected",
            error: Some(error),
            persistence_warning: None,
        })
    }

    fn check_input_reachability(&self) -> Result<(), Fault> {
        if self.stdin_available || self.close == Some(CloseMode::Cancel) {
            return Ok(());
        }
        for binding in self.bindings.values() {
            for pending in binding.instance().snapshot().pending_interactions {
                if !self.questions.values().any(|question| {
                    Arc::ptr_eq(&question.agent, binding)
                        && question.envelope.request_id == pending.request_id
                        && question.answering
                }) {
                    return Err(Fault::new(
                        Code::InputUnavailable,
                        "execution requires an answer after stdin EOF",
                    ));
                }
            }
        }
        Ok(())
    }

    fn emit(&mut self, message: Outgoing) -> Result<(), Fault> {
        if self.output_failed {
            return Ok(());
        }
        self.connection.send(message)
    }

    fn fail(&mut self, error: Fault) {
        if error.code == Code::OutputUnavailable {
            self.output_failed = true;
        }
        if self.failure.is_none() || error.code == Code::OutputUnavailable {
            self.failure = Some(error);
        }
        self.close = Some(CloseMode::Cancel);
    }

    pub(crate) async fn finish(
        mut self,
        finalization: impl std::future::Future<Output = anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        // Startup/admission commits must settle before shutdown snapshots the associations.
        while self.pending_application_work() {
            if let Err(error) = self.step().await {
                self.fail(error);
            }
        }
        let mut finalization = std::pin::pin!(finalization);
        let cleanup = loop {
            tokio::select! {
                result = &mut finalization => break result,
                result = self.step() => { if let Err(error)=result { self.fail(error); } }
            }
        };
        if let Err(error) = cleanup {
            let previous = self
                .failure
                .take()
                .map(|failure| format!("; prior failure: {failure}"))
                .unwrap_or_default();
            self.failure = Some(Fault::new(
                Code::FinalizationFailed,
                format!("{error:#}{previous}"),
            ));
        }
        // Keep accepted accounting and ordered terminal events alive through main-owned cleanup.
        while !self.workers.is_empty()
            || !self.boundaries.is_empty()
            || !self.streams.is_empty()
            || !self.observations.is_empty()
        {
            if let Err(error) = self.step().await {
                self.fail(error);
            }
        }
        if let Err(error) = self.report_notices() {
            self.fail(error);
        }
        if self.failure.is_none() && self.execution_failed {
            self.failure = Some(Fault::new(Code::ExecutionFailed, "a root execution failed"));
        }
        let status = if self.failure.is_some() {
            "failed"
        } else if self.close == Some(CloseMode::Cancel) {
            "cancelled"
        } else {
            "completed"
        };
        let reason = self
            .failure
            .as_ref()
            .map(|error| serde_json::to_value(error.code).expect("symbolic code"))
            .unwrap_or_else(|| {
                json!(if self.explicit_close {
                    if self.close == Some(CloseMode::Cancel) {
                        "close_cancel"
                    } else {
                        "close_drain"
                    }
                } else {
                    "eof"
                })
            });
        if !self.output_failed {
            let closed = Outgoing::Closed {
                status,
                reason,
                error: self.failure.clone(),
            };
            if let Err(error) = self.connection.send(closed) {
                self.fail(error);
            }
        }
        if !self.output_failed {
            if let Err(error) = self.connection.flush().await {
                return Err(error.into());
            }
        }
        match self.failure {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::hooks::Hook;
    use phi_sdk::lifecycle::UserPromptSubmitOutcome;
    use tokio::sync::Notify;

    fn fixture(
        hooks: Vec<Hook>,
    ) -> (
        std::path::PathBuf,
        Arc<crate::runtime::TestRuntime>,
        RpcSession,
        mpsc::Sender<Frame>,
        std::sync::mpsc::Receiver<Value>,
    ) {
        let (directory, policy, runtime) =
            crate::runtime::preparation::tests::session_test_runtime_with_hooks(hooks);
        policy.install(&runtime.preparation);
        let (connection, input, output) = Connection::testing();
        let rpc = RpcSession::new(
            connection,
            runtime.sessions.clone(),
            runtime.preparation.clone(),
            policy.model_choices_owner(),
        )
        .unwrap();
        (directory, runtime, rpc, input, output)
    }

    fn request(value: Value) -> Frame {
        Frame::Message(serde_json::from_value(value).unwrap())
    }

    async fn ready(rpc: &mut RpcSession) {
        rpc.frame(Some(request(json!({"type":"control","control_id":"init","request":{"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"ephemeral"}}})))).await.unwrap();
        while rpc.root.is_none() {
            rpc.step().await.unwrap();
        }
    }

    #[tokio::test]
    async fn drain_waits_for_workflow_without_live_workers() {
        use phi_sdk::workflow::{WorkflowLimits, WorkflowRequest, WorkflowStatus};
        use std::time::Duration;

        use clap::Parser;
        for policy in [None, Some(CloseMode::Drain), Some(CloseMode::Cancel)] {
            let (directory, runtime, mut rpc, input, output) = fixture(Vec::new());
            let models = directory.join("workflow-models.json");
            std::fs::write(&models, json!({"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}).to_string()).unwrap();
            let catalog =
                phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                    store_path: directory.join("workflow-store.json"),
                    user_paths: vec![models],
                    offline: true,
                })
                .unwrap();
            let settings = crate::config::Settings::default();
            let cli = crate::cli::Cli::parse_from(["phi", "--rpc"]);
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = directory.to_string_lossy().into();
            let root = runtime
                .sessions
                .create(
                    &runtime.preparation,
                    session.clone(),
                    crate::runtime::RuntimeRebuildOptions {
                        phi: crate::runtime::PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: settings.runtime_config(&catalog),
                            context: phi_sdk::context::load(true),
                            agent_options: Default::default(),
                            hooks: Vec::new(),
                        },
                        instance: crate::runtime::AgentInstanceBuildOptions::from_session(&session),
                    },
                    phi_sdk::lifecycle::SessionStartReason::Startup,
                    false,
                )
                .await
                .unwrap();
            rpc.attach(root.clone()).unwrap();
            rpc.root = Some(root.clone());
            let workflow_request = WorkflowRequest::parse(
                "export const meta = {name: 'drain', description: 'whole-run lifetime'}; while (true) {}",
                None,
            ).await.unwrap();
            let awake = if policy.is_none() {
                rpc.barrier(root.clone());
                while !rpc.barriers.is_empty() || !rpc.boundaries.is_empty() {
                    rpc.step().await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
                tokio::time::pause();
                Some(tokio::spawn(async {
                    loop {
                        tokio::task::yield_now().await;
                    }
                }))
            } else {
                None
            };
            let run = root
                .launch_workflow(
                    runtime.workflows.clone(),
                    workflow_request,
                    None,
                    WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(),
                    None,
                )
                .await
                .unwrap();
            let mut observed_without_clock = None;
            if let Some(awake) = awake {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let frozen = tokio::time::Instant::now();
                let mut observed = false;
                while std::time::Instant::now() < deadline && !observed {
                    while let Ok(observation) = rpc.observations.try_recv() {
                        if let Observation::Workflow(_, payload) = &observation {
                            observed |= payload["run_id"] == run.id().to_string()
                                && payload["status"] == "running";
                        }
                        rpc.observe(observation).unwrap();
                    }
                    tokio::task::yield_now().await;
                }
                assert_eq!(tokio::time::Instant::now(), frozen);
                tokio::time::resume();
                awake.abort();
                observed_without_clock = Some(observed);
            }
            root.instance().wait_for_quiescence(None).await.unwrap();
            let input = if policy.is_none() {
                drop(input);
                None
            } else {
                input.send(request(json!({"type":"control","control_id":"drain","request":{"kind":"close","mode":"drain"}}))).await.unwrap();
                Some(input)
            };
            let mut running = tokio::spawn(async move {
                rpc.run().await;
                rpc
            });
            let premature = tokio::time::timeout(Duration::from_millis(100), &mut running)
                .await
                .ok();
            if policy == Some(CloseMode::Cancel) {
                input.as_ref().unwrap().send(request(json!({"type":"control","control_id":"cancel","request":{"kind":"close","mode":"cancel"}}))).await.unwrap();
            } else {
                run.stop().await.unwrap();
            }
            let returned_early = premature.is_some();
            let rpc = match premature {
                Some(result) => result.unwrap(),
                None => tokio::time::timeout(Duration::from_secs(5), running)
                    .await
                    .unwrap()
                    .unwrap(),
            };
            rpc.finish(async {
                runtime
                    .sessions
                    .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                    .await
                    .map(|_| ())
            })
            .await
            .unwrap();
            assert_eq!(run.snapshot().status(), WorkflowStatus::Stopped);
            assert_eq!(
                output.try_iter().last().unwrap()["status"],
                if policy == Some(CloseMode::Cancel) {
                    "cancelled"
                } else {
                    "completed"
                }
            );
            std::fs::remove_dir_all(directory).unwrap();
            assert!(
                !returned_early,
                "RPC drain ignored the live Workflow script: {policy:?}"
            );
            assert_ne!(
                observed_without_clock,
                Some(false),
                "Workflow state observation must not require clock advancement"
            );
        }
    }

    #[tokio::test]
    async fn workflow_permissions_remain_scoped_through_drain() {
        const HTTP_STATUS_OK: &str = "200 OK";

        use clap::Parser;
        use phi_sdk::workflow::{WorkflowLimits, WorkflowRequest};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            for policy in ["answer", "eof", "stop"] {
                let directory = std::path::PathBuf::from("/tmp")
                    .join(format!("phi-rpc-workflow-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(directory.join(".phi/agents")).unwrap();
                std::fs::write(directory.join(".phi/settings.json"), json!({
                    "defaultProvider":"local", "defaultModel":"worker",
                    "permission":{"mode":"allow", "bash":"ask"},
                    "extensions":["!*.lua"], "memory":{"enabled":false}
                }).to_string()).unwrap();
                for (role, model) in [("probe", "worker"), ("nested", "nested")] {
                    std::fs::write(directory.join(format!(".phi/agents/{role}.md")),
                        format!("---\nname: {role}\ndescription: inspect\nmodel: {model}\n---\nPerform the delegated task.")).unwrap();
                }
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let command = format!("touch {}", directory.join("permitted-effect").display());
                let provider = tokio::spawn(async move {
                    let mut connections = JoinSet::new();
                    loop {
                        let (mut connection, _) = listener.accept().await.unwrap();
                        let command = command.clone();
                        connections.spawn(async move {
                            let mut bytes = Vec::new();
                            let request: Value = loop {
                                let mut buffer = [0; 4096];
                                let count = connection.read(&mut buffer).await.unwrap();
                                assert_ne!(count, 0);
                                bytes.extend_from_slice(&buffer[..count]);
                                if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                    let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                                    let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                                    if bytes.len() >= start + 4 + length {
                                        break serde_json::from_slice(&bytes[start + 4..start + 4 + length]).unwrap();
                                    }
                                }
                            };
                            let tool_result = request["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool");
                            let delta = match request["model"].as_str().unwrap() {
                                "worker" if !tool_result => json!({"tool_calls":[{"index":0,"id":"nested-call","type":"function",
                                    "function":{"name":"Agent","arguments":json!({"action":"create","role":"nested","prompt":"private-nested-input"}).to_string()}}]}),
                                "nested" if !tool_result => json!({"tool_calls":[{"index":0,"id":"permission-call","type":"function",
                                    "function":{"name":"Bash","arguments":json!({"command":command}).to_string()}}]}),
                                "worker" | "nested" => json!({"content":"private-worker-output"}),
                                _ => json!({"content":"creator-report"}),
                            };
                            let chunk = json!({"id":"completion","model":request["model"],"choices":[{
                                "finish_reason":if delta.get("tool_calls").is_some() {"tool_calls"} else {"stop"}, "delta":delta
                            }]});
                            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                            let response = format!("HTTP/1.1 {HTTP_STATUS_OK}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            let _ = connection.write_all(response.as_bytes()).await;
                        });
                    }
                });
                let models = directory.join("models.json");
                std::fs::write(&models, json!({"providers":{"local":{"api":"openai-completions","baseUrl":format!("http://{address}/v1"),
                    "models":{"test":{"contextWindow":128000},"worker":{"contextWindow":128000},"nested":{"contextWindow":128000}}}}}).to_string()).unwrap();
                let catalog = Arc::new(phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                    store_path: directory.join("store.json"), user_paths:vec![models], offline:true,
                }).unwrap());
                let resources = crate::runtime::RuntimeResources::new_for(
                    phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()).permission_policy(
                        phi_sdk::permission::PermissionConfig {mode:phi_sdk::permission::PermissionMode::Allow, ..Default::default()}, Some(directory.clone())),
                    Arc::new(phi_sdk::extension::ExtensionRuntime::load_with_paths(Vec::new())), catalog.clone(),
                    phi_sdk::sandbox::Sandbox::new(false, directory.clone()).unwrap(), &directory,
                );
                let runtime = crate::runtime::TestRuntime::new(crate::runtime::AgentRuntimeFactory::new(resources));
                let cli = crate::cli::Cli::parse_from(["phi", "--rpc"]);
                runtime.preparation.enable_subagents(&runtime.sessions);
                let settings_manager = crate::config::SettingsManager::load_for(&directory).unwrap();
                let settings = settings_manager.settings();
                let mut session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
                session.working_dir = directory.to_string_lossy().into();
                let models = runtime.preparation.initialize(&directory, Arc::new(cli.clone()), settings, settings_manager, Vec::new(), catalog);
                let workflows = crate::runtime::preparation::SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
                let root = workflows.start_session(session, true).await.unwrap();
                let unrelated = if policy == "stop" {
                    let mut session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
                    session.working_dir = directory.to_string_lossy().into();
                    let other = workflows.start_session(session, false).await.unwrap();
                    Some(other.launch_workflow(runtime.workflows.clone(), WorkflowRequest::parse(
                        "export const meta = {name:'other', description:'outside controlled root'}; while (true) {}", None
                    ).await.unwrap(), None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None).await.unwrap())
                } else { None };
                let (connection, input, output) = Connection::testing();
                let mut rpc = RpcSession::new(connection, runtime.sessions.clone(), runtime.preparation.clone(), models).unwrap();
                rpc.attach(root.clone()).unwrap();
                rpc.root = Some(root.clone());
                let run = root.launch_workflow(runtime.workflows.clone(), WorkflowRequest::parse(
                    "export const meta = {name:'rpc', description:'scoped permissions'}; await agent('private-worker-input', {role:'probe'}); return 'public-summary';", None
                ).await.unwrap(), None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None).await.unwrap();
                while rpc.questions.is_empty() { rpc.step().await.unwrap(); }
                let question_id = rpc.questions.keys().next().unwrap().clone();
                let worker_turn = rpc.questions[&question_id].envelope.scope.turn_id.as_ref().unwrap().to_string();
                rpc.control("worker-stop".into(), Control::TaskStop { task_id: worker_turn }).await.unwrap();
                rpc.control("interrupt".into(), Control::Interrupt).await.unwrap();
                while !rpc.controls.is_empty() || rpc.interrupt_id.is_some() { rpc.step().await.unwrap(); }
                assert_eq!(run.snapshot().status(), phi_sdk::workflow::WorkflowStatus::Running);
                let outside_run = unrelated.as_ref().map(|run| run.id().to_string()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                rpc.control("unknown-run".into(), serde_json::from_value(json!({"kind":"workflow_stop","run_id":outside_run})).unwrap()).await.unwrap();
                rpc.control("drain".into(), Control::Close {mode:CloseMode::Drain}).await.unwrap();
                match policy {
                    "answer" => input.send(request(json!({"type":"answer","question_id":question_id,"response":{"kind":"permission","decision":"allow_once"}}))).await.unwrap(),
                    "stop" => input.send(request(json!({"type":"control","control_id":"stop-run","request":{"kind":"workflow_stop","run_id":run.id()}}))).await.unwrap(),
                    _ => {}
                }
                if policy == "eof" { drop(input); }
                rpc.run().await;
                if let Some(unrelated) = &unrelated {
                    assert_eq!(unrelated.snapshot().status(), phi_sdk::workflow::WorkflowStatus::Running);
                }
                let result = rpc.finish(async {
                    runtime.sessions.shutdown(phi_sdk::lifecycle::SessionEndReason::Quit).await.map(|_| ())
                }).await;
                assert_eq!(result.is_ok(), policy != "eof", "{policy}: {result:?}");
                let frames = output.try_iter().collect::<Vec<_>>();
                assert!(frames.iter().any(|frame| frame["payload"]["kind"] == "workflow" && frame["payload"]["run_id"] == run.id().to_string()));
                assert!(frames.iter().any(|frame| frame["control_id"] == "unknown-run" && frame["result"]["error"]["code"] == "invalid_target"));
                assert!(frames.iter().any(|frame| frame["control_id"] == "worker-stop" && frame["result"]["error"]["code"] == "invalid_target"));
                assert!(frames.iter().filter(|frame| frame["type"] == "event" && frame["payload"]["kind"] != "workflow").all(|frame| frame["agent_id"] == root.instance().id().as_str()));
                assert!(!serde_json::to_string(&frames).unwrap().contains("private-worker-output"));
                assert!(!serde_json::to_string(&frames).unwrap().contains("private-nested-input"));
                assert!(frames.iter().filter(|frame| frame["type"] == "outcome").all(|frame| frame["agent_id"] == root.instance().id().as_str()));
                assert_eq!(directory.join("permitted-effect").exists(), policy == "answer");
                if policy == "answer" {
                    assert!(frames.iter().any(|frame| frame["payload"]["value"] == "public-summary" && frame["payload"]["status"] == "completed"));
                } else {
                    assert_eq!(run.snapshot().status(), phi_sdk::workflow::WorkflowStatus::Stopped);
                    if policy == "stop" {
                        assert!(frames.iter().any(|frame| frame["control_id"] == "stop-run" && frame["result"]["value"]["status"] == "stopped"));
                        assert!(frames.iter().any(|frame| frame["payload"]["run_id"] == run.id().to_string() && frame["payload"]["status"] == "stopped"));
                    } else {
                        assert_eq!(frames.last().unwrap()["reason"], "input_unavailable");
                    }
                }
                assert!(!runtime.sessions.has_associations().unwrap());
                provider.abort();
                std::fs::remove_dir_all(directory).unwrap();
            }
        }).await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_cannot_finalize_before_pending_initialization() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let hook = Hook::session_start({
            let entered = entered.clone();
            let release = release.clone();
            move |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(Vec::new())
                }
            }
        });
        let (directory, runtime, mut rpc, _input, output) = fixture(vec![hook]);
        rpc.frame(Some(request(json!({"type":"control","control_id":"init","request":{"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"ephemeral"}}})))).await.unwrap();
        entered.notified().await;
        rpc.close = Some(CloseMode::Cancel);
        rpc.run().await;
        let finalized = runtime.sessions.clone();
        let task = tokio::spawn(async move {
            rpc.finish(async move {
                assert!(
                    finalized.has_associations().unwrap(),
                    "cleanup overtook startup"
                );
                finalized
                    .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                    .await
                    .map(|_| ())
            })
            .await
        });
        release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(!runtime.sessions.has_associations().unwrap());
        let frames: Vec<_> = output.try_iter().collect();
        assert_eq!(frames.last().unwrap()["status"], "cancelled");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn eof_settles_pending_admission_without_fabricating_a_turn() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let hook = Hook::user_prompt_submit({
            let entered = entered.clone();
            let release = release.clone();
            move |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(UserPromptSubmitOutcome::Handled {
                        reason: Some("handled".into()),
                    })
                }
            }
        });
        let (directory, runtime, mut rpc, input, output) = fixture(vec![hook]);
        ready(&mut rpc).await;
        input.send(request(json!({"type":"input","input_id":"held","dispatch":"immediate","prompt":{"text":"held"}}))).await.unwrap();
        let task = tokio::spawn(async move {
            rpc.run().await;
            rpc
        });
        entered.notified().await;
        drop(input);
        assert!(!task.is_finished());
        release.notify_one();
        let rpc = task.await.unwrap();
        let finalized = runtime.sessions.clone();
        rpc.finish(async move {
            finalized
                .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                .await
                .map(|_| ())
        })
        .await
        .unwrap();
        let frames: Vec<_> = output.try_iter().collect();
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "admission"
                    && frame["disposition"]["kind"] == "handled")
        );
        assert!(!frames.iter().any(|frame| frame["type"] == "outcome"));
        assert_eq!(frames.last().unwrap()["status"], "completed");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn input_and_cleanup_failures_both_survive_finalization() {
        let (directory, runtime, mut rpc, input, output) = fixture(Vec::new());
        ready(&mut rpc).await;
        input
            .send(Frame::Failed(std::io::Error::other("read-fault")))
            .await
            .unwrap();
        rpc.run().await;
        let finalized = runtime.sessions.clone();
        let result = rpc
            .finish(async move {
                finalized
                    .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                    .await?;
                anyhow::bail!("cleanup-fault")
            })
            .await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("read-fault") && error.contains("cleanup-fault"),
            "{error}"
        );
        assert!(!runtime.sessions.has_associations().unwrap());
        let frames: Vec<_> = output.try_iter().collect();
        assert_eq!(frames.last().unwrap()["reason"], "finalization_failed");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn unsupported_interaction_fails_without_emitting_a_question() {
        let (directory, runtime, mut rpc, _input, output) = fixture(Vec::new());
        ready(&mut rpc).await;
        let root = rpc.root.clone().unwrap();
        let error = rpc
            .question(
                root,
                InteractionEnvelope {
                    request_id: "unsupported".into(),
                    emitted_at: chrono::Utc::now(),
                    scope: Default::default(),
                    request: InteractionRequest::McpElicitation(
                        phi_sdk::extension::mcp::McpElicitationRequest::Form {
                            server: "test".into(),
                            protocol_request_id: json!(1),
                            parent_tool_call_id: None,
                            message: "unsupported".into(),
                            requested_schema: json!({}),
                        },
                    ),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, Code::OperationFailed);
        rpc.fail(error);
        let finalized = runtime.sessions.clone();
        assert!(
            rpc.finish(async move {
                finalized
                    .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                    .await
                    .map(|_| ())
            })
            .await
            .is_err()
        );
        assert!(!runtime.sessions.has_associations().unwrap());
        let frames: Vec<_> = output.try_iter().collect();
        assert!(!frames.iter().any(|frame| frame["type"] == "question"));
        assert_eq!(frames.last().unwrap()["reason"], "operation_failed");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn late_subscription_recovers_terminal_progress_at_its_boundary() {
        let hook = Hook::provider_context(|_| async { Err("stop before provider request".into()) });
        let (directory, runtime, mut rpc, _input, _output) = fixture(vec![hook]);
        ready(&mut rpc).await;
        let root = rpc.root.clone().unwrap();
        root.instance()
            .submit(phi_sdk::turn::TurnPrompt {
                prompt: "fail deterministically".into(),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        let view = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            root.instance().wait_for_quiescence(None),
        )
        .await
        .unwrap()
        .unwrap();
        let expected = view.snapshot.progress.unwrap();
        assert_eq!(
            expected.status,
            phi_sdk::agent_instance::ExecutionStatus::Failed
        );
        let (sender, mut received) = mpsc::channel(io::CAPACITY);
        let (stop, stopping) = oneshot::channel();
        let observer = tokio::spawn(observations::subscribe(
            root,
            runtime.sessions.clone(),
            sender,
            stopping,
            observations::Visibility::Execution,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut recovered = None;
            loop {
                match received.recv().await.unwrap() {
                    Observation::Event(_, AgentInstanceEvent::Progress(progress)) => {
                        recovered = Some(progress)
                    }
                    Observation::Event(_, AgentInstanceEvent::ViewBoundary { .. }) => {
                        assert_eq!(recovered.as_ref(), Some(&expected));
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        observer.await.unwrap().unwrap();
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        drop(rpc);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn observation_loss_never_reports_successful_close() {
        let (directory, runtime, mut rpc, _input, output) = fixture(Vec::new());
        ready(&mut rpc).await;
        let root = rpc.root.clone().unwrap();
        let (sender, _observations) = mpsc::channel(io::CAPACITY);
        let (_stop, stopping) = oneshot::channel();
        let observer = observations::subscribe(
            root.clone(),
            runtime.sessions.clone(),
            sender,
            stopping,
            observations::Visibility::Execution,
        );
        const OBSERVATION_BURST: usize = 1024;
        for _ in 0..OBSERVATION_BURST {
            root.instance().abort().await.unwrap();
        }
        let error = observer.await.unwrap_err();
        assert_eq!(error.code, Code::ObservationLost);
        rpc.fail(error);
        let finalized = runtime.sessions.clone();
        let result = rpc
            .finish(async move {
                finalized
                    .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                    .await
                    .map(|_| ())
            })
            .await;
        assert!(result.is_err());
        let frames: Vec<_> = output.try_iter().collect();
        assert_eq!(frames.last().unwrap()["reason"], "observation_lost");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn failed_worker_does_not_leave_a_phantom_pending_control() {
        let (directory, runtime, mut rpc, _input, output) = fixture(Vec::new());
        ready(&mut rpc).await;
        rpc.controls.insert("failed".into());
        rpc.workers.spawn(async {
            panic!("worker-fault");
        });
        rpc.run().await;
        let finalized = runtime.sessions.clone();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rpc.finish(async move {
                finalized
                    .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                    .await
                    .map(|_| ())
            }),
        )
        .await
        .expect("failed worker must not pin cleanup");
        assert!(result.is_err());
        assert!(!runtime.sessions.has_associations().unwrap());
        let frames: Vec<_> = output.try_iter().collect();
        assert_eq!(frames.last().unwrap()["status"], "failed");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn control_capacity_never_consumes_interrupt_or_close_slots() {
        let (directory, runtime, mut rpc, _input, output) = fixture(Vec::new());
        ready(&mut rpc).await;
        rpc.controls
            .extend((0..MAX_CONTROLS).map(|index| format!("pending-{index}")));
        rpc.control("overflow".into(), Control::RuntimeInfo)
            .await
            .unwrap();
        rpc.control("interrupt".into(), Control::Interrupt)
            .await
            .unwrap();
        rpc.control("interrupt".into(), Control::Interrupt)
            .await
            .unwrap();
        rpc.control(
            "close".into(),
            Control::Close {
                mode: CloseMode::Cancel,
            },
        )
        .await
        .unwrap();
        let finalized = runtime.sessions.clone();
        rpc.finish(async move {
            finalized
                .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                .await
                .map(|_| ())
        })
        .await
        .unwrap();
        let frames: Vec<_> = output.try_iter().collect();
        assert!(
            frames.iter().any(|frame| frame["control_id"] == "overflow"
                && frame["result"]["error"]["code"] == "busy")
        );
        assert!(frames.iter().any(|frame| frame["control_id"] == "interrupt"
            && frame["result"]["error"]["code"] == "duplicate_id"));
        assert!(
            frames.iter().any(
                |frame| frame["control_id"] == "interrupt" && frame["result"]["status"] == "ok"
            )
        );
        assert_eq!(frames.last().unwrap()["status"], "cancelled");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
