use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use chromiumoxide::{
    Connection, Method,
    cdp::{
        browser_protocol::{
            fetch::{ContinueRequestParams, EnableParams as FetchEnableParams, FailRequestParams},
            network::{
                EnableParams as NetworkEnableParams, ErrorReason, GetRequestPostDataParams,
                GetRequestPostDataReturns, GetResponseBodyParams, GetResponseBodyReturns,
            },
            target::{
                FilterEntry, SessionId, SetAutoAttachParams, SetAutoAttachReturns, TargetFilter,
                TargetId,
            },
        },
        events::{CdpEvent, CdpEventMessage},
        js_protocol::runtime::RunIfWaitingForDebuggerParams,
    },
    types::{CallId, Message},
};
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::warn;

use super::{
    BrowserError, BrowserNetworkBodyKind, BrowserNetworkContext, BrowserNetworkRequest,
    BrowserWebSocketDirection, Diagnostics, NetworkSource, apply_response, finish_request,
    network_control::{NetworkControls, RequestDecision, fulfill_params},
    network_headers, network_initiator, seconds_to_milliseconds,
};

const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_CAPACITY: usize = 32;

pub(super) struct NetworkObserver {
    commands: mpsc::Sender<ObserverCommand>,
}

pub(super) struct NetworkBody {
    pub(super) body: String,
    pub(super) base64_encoded: bool,
}

enum ObserverCommand {
    Body {
        session_id: String,
        request_id: String,
        kind: BrowserNetworkBodyKind,
        response: oneshot::Sender<Result<NetworkBody, String>>,
    },
}

struct PendingBody {
    kind: BrowserNetworkBodyKind,
    response: oneshot::Sender<Result<NetworkBody, String>>,
}

impl NetworkObserver {
    pub(super) async fn body(
        &self,
        session_id: String,
        request_id: String,
        kind: BrowserNetworkBodyKind,
    ) -> Result<NetworkBody, BrowserError> {
        let (response, body) = oneshot::channel();
        self.commands
            .send(ObserverCommand::Body {
                session_id,
                request_id,
                kind,
                response,
            })
            .await
            .map_err(|_| BrowserError::NetworkObserver {
                message: "the observer task stopped".to_owned(),
            })?;
        body.await
            .map_err(|_| BrowserError::NetworkObserver {
                message: "the observer dropped a body response".to_owned(),
            })?
            .map_err(|message| BrowserError::NetworkObserver { message })
    }
}

pub(super) async fn start(
    websocket_address: &str,
    target_id: TargetId,
    diagnostics: Arc<StdMutex<Diagnostics>>,
    network_controls: NetworkControls,
) -> Result<(NetworkObserver, JoinHandle<()>), BrowserError> {
    let mut connection = Connection::<CdpEventMessage>::connect(websocket_address).await?;
    let filter = TargetFilter::new(vec![
        FilterEntry::builder().r#type("page").exclude(false).build(),
        FilterEntry::builder().exclude(true).build(),
    ]);
    let auto_attach = SetAutoAttachParams::builder()
        .auto_attach(true)
        .wait_for_debugger_on_start(true)
        .flatten(true)
        .filter(filter)
        .build()
        .map_err(|message| BrowserError::NetworkObserver { message })?;
    let attach_call = submit(&mut connection, None, auto_attach)?;
    let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (ready, ready_rx) = oneshot::channel();
    let task = tokio::spawn(run(
        connection,
        attach_call,
        target_id,
        command_rx,
        ready,
        diagnostics,
        network_controls,
    ));
    match timeout(INITIALIZATION_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok((NetworkObserver { commands }, task)),
        Ok(Ok(Err(message))) => {
            task.abort();
            Err(BrowserError::NetworkObserver { message })
        }
        Ok(Err(_)) => {
            task.abort();
            Err(BrowserError::NetworkObserver {
                message: "the observer stopped during initialization".to_owned(),
            })
        }
        Err(_) => {
            task.abort();
            Err(BrowserError::NetworkObserver {
                message: "initialization timed out".to_owned(),
            })
        }
    }
}

async fn run(
    mut connection: Connection<CdpEventMessage>,
    attach_call: CallId,
    root_target_id: TargetId,
    mut commands: mpsc::Receiver<ObserverCommand>,
    ready: oneshot::Sender<Result<(), String>>,
    diagnostics: Arc<StdMutex<Diagnostics>>,
    network_controls: NetworkControls,
) {
    let mut ready = Some(ready);
    let mut pending_bodies = HashMap::<CallId, PendingBody>::new();
    let mut root_session = None;
    let mut child_sessions = HashSet::new();

    loop {
        tokio::select! {
            message = connection.next() => {
                let Some(message) = message else {
                    fail_ready(&mut ready, "the DevTools connection closed");
                    break;
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        fail_ready(&mut ready, &error.to_string());
                        warn!(target: "nanocodex_browser", %error, "network observer stopped");
                        break;
                    }
                };
                match message {
                    Message::Response(response) => {
                        if response.id == attach_call {
                            if let Err(message) = response_result::<SetAutoAttachReturns>(response) {
                                fail_ready(&mut ready, &message);
                                break;
                            }
                            if let Some(ready) = ready.take() {
                                let _ = ready.send(Ok(()));
                            }
                        } else if let Some(pending) = pending_bodies.remove(&response.id) {
                            let result = decode_body_response(response, pending.kind);
                            let _ = pending.response.send(result);
                        } else if let Some(error) = response.error {
                            warn!(
                                target: "nanocodex_browser",
                                %error,
                                "child-target DevTools setup command failed"
                            );
                        }
                    }
                    Message::Event(event) => {
                        handle_event(
                            event,
                            &root_target_id,
                            &mut root_session,
                            &mut child_sessions,
                            &mut connection,
                            &diagnostics,
                            &network_controls,
                        );
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                let ObserverCommand::Body {
                    session_id,
                    request_id,
                    kind,
                    response,
                } = command;
                match submit_body(
                    &mut connection,
                    SessionId::new(session_id),
                    request_id,
                    kind,
                ) {
                    Ok(call_id) => {
                        pending_bodies.insert(call_id, PendingBody { kind, response });
                    }
                    Err(message) => {
                        let _ = response.send(Err(message));
                    }
                }
            }
        }
    }

    for (_, pending) in pending_bodies {
        let _ = pending
            .response
            .send(Err("the observer task stopped".to_owned()));
    }
}

fn configure_child(
    connection: &mut Connection<CdpEventMessage>,
    session_id: SessionId,
) -> Result<(), String> {
    submit(
        connection,
        Some(session_id.clone()),
        NetworkEnableParams::default(),
    )
    .map_err(|error| error.to_string())?;
    submit(
        connection,
        Some(session_id.clone()),
        FetchEnableParams::default(),
    )
    .map_err(|error| error.to_string())?;
    enable_child_auto_attach(connection, session_id.clone())?;
    submit(
        connection,
        Some(session_id),
        RunIfWaitingForDebuggerParams::default(),
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn configure_root(
    connection: &mut Connection<CdpEventMessage>,
    session_id: SessionId,
) -> Result<(), String> {
    enable_child_auto_attach(connection, session_id.clone())?;
    resume_target(connection, session_id)
}

fn enable_child_auto_attach(
    connection: &mut Connection<CdpEventMessage>,
    session_id: SessionId,
) -> Result<(), String> {
    let params = SetAutoAttachParams::builder()
        .auto_attach(true)
        .wait_for_debugger_on_start(true)
        .flatten(true)
        .build()?;
    submit(connection, Some(session_id), params)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn resume_target(
    connection: &mut Connection<CdpEventMessage>,
    session_id: SessionId,
) -> Result<(), String> {
    submit(
        connection,
        Some(session_id),
        RunIfWaitingForDebuggerParams::default(),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn submit<T: serde::Serialize + Method>(
    connection: &mut Connection<CdpEventMessage>,
    session_id: Option<SessionId>,
    command: T,
) -> Result<CallId, serde_json::Error> {
    connection.submit_command(
        command.identifier(),
        session_id,
        serde_json::to_value(command)?,
    )
}

fn submit_body(
    connection: &mut Connection<CdpEventMessage>,
    session_id: SessionId,
    request_id: String,
    kind: BrowserNetworkBodyKind,
) -> Result<CallId, String> {
    match kind {
        BrowserNetworkBodyKind::Request => submit(
            connection,
            Some(session_id),
            GetRequestPostDataParams::new(request_id),
        ),
        BrowserNetworkBodyKind::Response => submit(
            connection,
            Some(session_id),
            GetResponseBodyParams::new(request_id),
        ),
    }
    .map_err(|error| error.to_string())
}

fn response_result<T: serde::de::DeserializeOwned>(
    response: chromiumoxide::types::Response,
) -> Result<T, String> {
    if let Some(error) = response.error {
        return Err(error.to_string());
    }
    serde_json::from_value(
        response
            .result
            .ok_or_else(|| "DevTools returned no command result".to_owned())?,
    )
    .map_err(|error| error.to_string())
}

fn decode_body_response(
    response: chromiumoxide::types::Response,
    kind: BrowserNetworkBodyKind,
) -> Result<NetworkBody, String> {
    match kind {
        BrowserNetworkBodyKind::Request => {
            let response = response_result::<GetRequestPostDataReturns>(response)?;
            Ok(NetworkBody {
                body: response.post_data,
                base64_encoded: false,
            })
        }
        BrowserNetworkBodyKind::Response => {
            let response = response_result::<GetResponseBodyReturns>(response)?;
            Ok(NetworkBody {
                body: response.body,
                base64_encoded: response.base64_encoded,
            })
        }
    }
}

fn fail_ready(ready: &mut Option<oneshot::Sender<Result<(), String>>>, message: &str) {
    if let Some(ready) = ready.take() {
        let _ = ready.send(Err(message.to_owned()));
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive typed dispatch preserves child-target network event ordering"
)]
fn handle_event(
    event: CdpEventMessage,
    root_target_id: &TargetId,
    root_session: &mut Option<String>,
    child_sessions: &mut HashSet<String>,
    connection: &mut Connection<CdpEventMessage>,
    diagnostics: &Arc<StdMutex<Diagnostics>>,
    network_controls: &NetworkControls,
) {
    let parent_session = event.session_id.clone();
    if let CdpEvent::TargetAttachedToTarget(attached) = &event.params {
        let is_root = attached.target_info.target_id == *root_target_id;
        let is_child = parent_session.as_ref().is_some_and(|parent| {
            root_session.as_ref() == Some(parent) || child_sessions.contains(parent)
        });
        let setup = if is_root {
            root_session.replace(attached.session_id.as_ref().to_owned());
            configure_root(connection, attached.session_id.clone())
        } else if is_child {
            child_sessions.insert(attached.session_id.as_ref().to_owned());
            configure_child(connection, attached.session_id.clone())
        } else {
            resume_target(connection, attached.session_id.clone())
        };
        if let Err(message) = setup {
            warn!(
                target: "nanocodex_browser",
                target_type = %attached.target_info.r#type,
                %message,
                "failed to configure child target"
            );
        }
        return;
    }

    let Some(session_id) = event.session_id else {
        return;
    };
    if !child_sessions.contains(&session_id) {
        return;
    }
    if let CdpEvent::FetchRequestPaused(event) = &event.params {
        let command = match network_controls.decide(&event.request.url) {
            RequestDecision::Continue => submit(
                connection,
                Some(SessionId::new(session_id.clone())),
                ContinueRequestParams::new(event.request_id.clone()),
            ),
            RequestDecision::Block => submit(
                connection,
                Some(SessionId::new(session_id.clone())),
                FailRequestParams::new(event.request_id.clone(), ErrorReason::BlockedByClient),
            ),
            RequestDecision::Fulfill(response) => submit(
                connection,
                Some(SessionId::new(session_id.clone())),
                fulfill_params(event, response),
            ),
        };
        if let Err(error) = command {
            warn!(
                target: "nanocodex_browser",
                %error,
                "failed to resolve a child-target intercepted request"
            );
        }
        return;
    }
    let request_key = |request_id: &str| child_request_key(&session_id, request_id);

    let Ok(mut diagnostics) = diagnostics.lock() else {
        return;
    };
    match event.params {
        CdpEvent::NetworkRequestWillBeSent(event) => {
            let id = request_key(event.request_id.as_ref());
            let timestamp = *event.timestamp.inner();
            if let Some(redirect) = &event.redirect_response
                && let Some(entry) = diagnostics.request_entry_mut(&id)
            {
                apply_response(&mut entry.request, redirect);
                finish_request(entry, timestamp, redirect.encoded_data_length);
            }
            diagnostics.push_request(
                &id,
                NetworkSource::ChildTarget {
                    session_id,
                    request_id: event.request_id.as_ref().to_owned(),
                },
                timestamp,
                BrowserNetworkRequest {
                    sequence: 0,
                    request_id: String::new(),
                    context: BrowserNetworkContext::ChildTarget,
                    body_available: true,
                    url: event.request.url.clone(),
                    method: event.request.method.clone(),
                    document_url: event.document_url.clone(),
                    resource_type: event
                        .r#type
                        .as_ref()
                        .map_or_else(|| "Other".to_owned(), |kind| kind.as_ref().to_owned()),
                    started_at_epoch_ms: seconds_to_milliseconds(*event.wall_time.inner()),
                    duration_ms: None,
                    initiator: Some(network_initiator(&event.initiator)),
                    request_headers: network_headers(&event.request.headers),
                    has_post_data: event.request.has_post_data.unwrap_or(false),
                    status: None,
                    status_text: None,
                    response_headers: Vec::new(),
                    mime_type: None,
                    charset: None,
                    protocol: None,
                    remote_ip_address: None,
                    remote_port: None,
                    from_disk_cache: false,
                    from_service_worker: false,
                    encoded_data_length: None,
                    timing: None,
                    completed: false,
                    failure: None,
                },
            );
        }
        CdpEvent::NetworkResponseReceived(event) => {
            let id = request_key(event.request_id.as_ref());
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                apply_response(&mut entry.request, &event.response);
                event
                    .r#type
                    .as_ref()
                    .clone_into(&mut entry.request.resource_type);
            }
        }
        CdpEvent::NetworkLoadingFinished(event) => {
            let id = request_key(event.request_id.as_ref());
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                finish_request(entry, *event.timestamp.inner(), event.encoded_data_length);
            }
        }
        CdpEvent::NetworkLoadingFailed(event) => {
            let id = request_key(event.request_id.as_ref());
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                entry.request.failure = Some(event.error_text.clone());
                event
                    .r#type
                    .as_ref()
                    .clone_into(&mut entry.request.resource_type);
                finish_request(entry, *event.timestamp.inner(), 0.0);
            }
        }
        CdpEvent::NetworkWebSocketCreated(event) => {
            let id = request_key(event.request_id.as_ref());
            if diagnostics.request_entry_mut(&id).is_none() {
                diagnostics.push_request(
                    &id,
                    NetworkSource::ChildTarget {
                        session_id,
                        request_id: event.request_id.as_ref().to_owned(),
                    },
                    0.0,
                    BrowserNetworkRequest {
                        sequence: 0,
                        request_id: String::new(),
                        context: BrowserNetworkContext::ChildTarget,
                        body_available: false,
                        url: event.url.clone(),
                        method: "GET".to_owned(),
                        document_url: String::new(),
                        resource_type: "WebSocket".to_owned(),
                        started_at_epoch_ms: 0,
                        duration_ms: None,
                        initiator: event.initiator.as_ref().map(network_initiator),
                        request_headers: Vec::new(),
                        has_post_data: false,
                        status: None,
                        status_text: None,
                        response_headers: Vec::new(),
                        mime_type: None,
                        charset: None,
                        protocol: Some("websocket".to_owned()),
                        remote_ip_address: None,
                        remote_port: None,
                        from_disk_cache: false,
                        from_service_worker: false,
                        encoded_data_length: None,
                        timing: None,
                        completed: false,
                        failure: None,
                    },
                );
            }
        }
        CdpEvent::NetworkWebSocketWillSendHandshakeRequest(event) => {
            let id = request_key(event.request_id.as_ref());
            let timestamp = *event.timestamp.inner();
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                entry.started_at_monotonic_seconds = timestamp;
                entry.request.started_at_epoch_ms =
                    seconds_to_milliseconds(*event.wall_time.inner());
                entry.request.request_headers = network_headers(&event.request.headers);
            }
        }
        CdpEvent::NetworkWebSocketHandshakeResponseReceived(event) => {
            let id = request_key(event.request_id.as_ref());
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                entry.request.status = Some(event.response.status);
                entry.request.status_text = Some(event.response.status_text.clone());
                entry.request.response_headers = network_headers(&event.response.headers);
            }
        }
        CdpEvent::NetworkWebSocketFrameSent(event) => {
            diagnostics.push_web_socket_message(
                request_key(event.request_id.as_ref()),
                BrowserWebSocketDirection::Sent,
                *event.timestamp.inner(),
                &event.response,
            );
        }
        CdpEvent::NetworkWebSocketFrameReceived(event) => {
            diagnostics.push_web_socket_message(
                request_key(event.request_id.as_ref()),
                BrowserWebSocketDirection::Received,
                *event.timestamp.inner(),
                &event.response,
            );
        }
        CdpEvent::NetworkWebSocketFrameError(event) => {
            let id = request_key(event.request_id.as_ref());
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                entry.request.failure = Some(event.error_message.clone());
            }
        }
        CdpEvent::NetworkWebSocketClosed(event) => {
            let id = request_key(event.request_id.as_ref());
            if let Some(entry) = diagnostics.request_entry_mut(&id) {
                finish_request(entry, *event.timestamp.inner(), 0.0);
            }
        }
        _ => {}
    }
}

fn child_request_key(session_id: &str, request_id: &str) -> String {
    format!("child:{session_id}:{request_id}")
}
