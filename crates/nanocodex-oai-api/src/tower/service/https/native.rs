use web_time::Instant;

use crate::{
    AgentEventKind, EncodedRequest, OpenAiAuthMode, ResponsesError, ResponsesTransport,
    http::{HttpMetadata, ResponsesHttpStream},
    telemetry::{ApiEvent, elapsed_ns},
    tower::{
        ResponsesAttempt, ResponsesAttemptKind, ResponsesOutput, ResponsesServiceError,
        ResponsesServiceResponse,
        service_error::FailurePhase,
        stream::{self, ResponseEventSource},
    },
};

use super::super::{
    ResponsesAttemptGuard, ResponsesService, record_pipeline_stats, required_call_index,
};

pub(crate) async fn run(
    service: &ResponsesService,
    attempt: &mut ResponsesAttemptGuard<'_>,
    request: &ResponsesAttempt,
    started_at: Instant,
) -> Result<ResponsesServiceResponse, ResponsesServiceError> {
    if matches!(request.kind, ResponsesAttemptKind::Warmup) {
        return Err(ResponsesServiceError::invalid_attempt_state(
            "HTTPS Responses transport does not perform a warmup request",
            FailurePhase::Protocol,
            0,
        ));
    }
    let encode_started_at = Instant::now();
    let encoded = service.encode_request(attempt, request, ResponsesTransport::Https)?;
    let encode_duration_ns = elapsed_ns(encode_started_at);
    let request_bytes = encoded.raw().get().len();
    let transport = ResponsesTransport::Https.as_str();
    let span = tracing::Span::current();
    span.record("request.bytes", request_bytes);
    span.record("request.encode.duration_ns", encode_duration_ns);
    tracing::trace!(
        target: "nanocodex_oai_api",
        direction = "outbound",
        transport,
        phase = request.kind.phase(),
        model.call_index = request.call_index,
        api.request = %encoded.raw().get(),
        "OpenAI Responses API request"
    );
    request.observer.emit(
        AgentEventKind::ApiEvent,
        ApiEvent {
            direction: "outbound",
            transport,
            phase: request.kind.phase(),
            model_call_index: request.call_index,
            event: encoded.raw(),
        },
    )?;
    let send_started_at = Instant::now();
    let turn_state = attempt.turn_state.clone();
    let (mut response, metadata) = send_with_auth_recovery(
        service,
        request.profile.session_id(),
        turn_state.as_deref(),
        &encoded,
        attempt.progress_mut(),
    )
    .await
    .map_err(|error| ResponsesServiceError::responses(error, FailurePhase::Send, 0))?;
    attempt.progress_mut().mark_provider_accepted();
    attempt.observe_turn_state(metadata.turn_state.as_deref());
    let send_duration_ns = elapsed_ns(send_started_at);
    span.record("request.send.duration_ns", send_duration_ns);
    let output = match request.kind {
        ResponsesAttemptKind::Generation => ResponsesOutput::Generation(
            stream::receive(
                &mut response,
                transport,
                &request.observer,
                required_call_index(request)?,
                started_at,
                attempt.progress_mut(),
            )
            .await?,
        ),
        ResponsesAttemptKind::Compaction => ResponsesOutput::Compaction(
            stream::receive_compaction(
                &mut response,
                transport,
                &request.observer,
                required_call_index(request)?,
                started_at,
                attempt.progress_mut(),
            )
            .await?,
        ),
        ResponsesAttemptKind::Warmup => unreachable!("warmup rejected above"),
    };
    let pipeline_stats = match &output {
        ResponsesOutput::Generation(result) => result.pipeline_stats,
        ResponsesOutput::Compaction(result) => result.pipeline_stats,
        ResponsesOutput::Warmup(_) => unreachable!("warmup rejected above"),
    };
    record_pipeline_stats(
        &span,
        request_bytes,
        encode_duration_ns,
        send_duration_ns,
        pipeline_stats,
    );
    Ok(ResponsesServiceResponse {
        output,
        attempt: request.attempt,
        connection_generation: 0,
        server_reasoning_included: metadata.reasoning_included,
    })
}

impl ResponseEventSource for ResponsesHttpStream {
    async fn next_text_or_idle_timeout(
        &mut self,
    ) -> Result<crate::socket::ReceivedText, ResponsesError> {
        Self::next_text_or_idle_timeout(self).await
    }
}

async fn send_with_auth_recovery(
    service: &ResponsesService,
    session_id: &str,
    turn_state: Option<&str>,
    request: &EncodedRequest,
    progress: &mut super::super::AttemptProgress,
) -> Result<(ResponsesHttpStream, HttpMetadata), ResponsesError> {
    let auth = service.auth_snapshot().await?;
    progress.mark_request_send_started();
    let first = service
        .platform
        .http()
        .send(
            &service.config.api_base_url,
            &auth,
            session_id,
            turn_state,
            request,
        )
        .await;
    if matches!(first, Err(ResponsesError::HttpRejected { .. })) {
        progress.mark_provider_rejected();
    }
    match first {
        Err(ResponsesError::HttpRejected { status: 401, .. })
            if auth.mode() == OpenAiAuthMode::ChatGpt =>
        {
            service
                .config
                .auth
                .recover_unauthorized(&auth)
                .await
                .map_err(|error| ResponsesError::Authorization {
                    detail: error.to_string(),
                })?;
            let refreshed = service.auth_snapshot().await?;
            progress.mark_request_send_started();
            let second = service
                .platform
                .http()
                .send(
                    &service.config.api_base_url,
                    &refreshed,
                    session_id,
                    turn_state,
                    request,
                )
                .await;
            if matches!(second, Err(ResponsesError::HttpRejected { .. })) {
                progress.mark_provider_rejected();
            }
            second
        }
        result => result,
    }
}
