use std::sync::Arc;

use nanocodex_core::{
    AgentEventKind, EventSink, MODEL, ModelConfig, ResponseItem, Thinking,
    responses::ResponseHistory,
};
use nanocodex_service::{
    ResponsesAttempt, ResponsesAttemptFactory, ResponsesClient, ResponsesOutput,
    ResponsesServiceResponse, TurnResult,
};
use tower::Service;
use tracing::{Instrument, info_span};
use web_time::Instant;

#[cfg(not(target_family = "wasm"))]
use super::ModelRouteChanged;
use super::{
    AgentSend, ModelCallCompleted, ModelCallFailed, ModelCallStarted, RunStats, elapsed_ns,
    record_indexed_span_content, record_span_content, trace_content_enabled, trace_model_input,
};
#[cfg(not(target_family = "wasm"))]
use crate::{
    KimiRefusalFallback,
    kimi::{KimiClient, KimiTranscript},
};
use crate::{NanocodexError, Result};

pub(super) trait ModelCallContext {
    fn previous_response_id(&self) -> Option<&str>;
    fn prompt_history(&self) -> ResponseHistory;
    fn shared_history(&self) -> ResponseHistory;
    fn delta_start(&self) -> usize;

    #[cfg(not(target_family = "wasm"))]
    fn flattened_history(&self) -> Vec<ResponseItem>;

    #[cfg(not(target_family = "wasm"))]
    fn history_revision(&self) -> u64;
}

/// Configuration carried by the agent tree for the model-call middleware.
///
/// Keeping optional provider policy behind this value prevents agent
/// lifecycle, spawning, and checkpoint code from depending on its mechanics.
#[derive(Clone, Default)]
pub(crate) struct ModelCallMiddlewareConfig {
    #[cfg(not(target_family = "wasm"))]
    kimi_refusal: Option<Arc<KimiRefusalFallback>>,
}

impl ModelCallMiddlewareConfig {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn with_kimi_refusal(mut self, fallback: KimiRefusalFallback) -> Self {
        self.kimi_refusal = Some(Arc::new(fallback));
        self
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(fallback) = &self.kimi_refusal {
            fallback
                .validate()
                .map_err(NanocodexError::InvalidRequest)?;
        }
        Ok(())
    }
}

/// Owns model selection around the primary Responses client.
///
/// The agent loop submits one logical generation here. This middleware alone
/// decides whether a structured primary refusal should be retried through
/// Kimi, maintains the exponential lease, and records provider-level events.
pub(super) struct ModelCallMiddleware<S> {
    events: EventSink,
    config: Arc<ModelConfig>,
    primary: ResponsesClient<S>,
    server_reasoning_included: bool,
    #[cfg(not(target_family = "wasm"))]
    kimi: Option<KimiClient>,
}

impl<S> ModelCallMiddleware<S> {
    pub(super) fn new(
        events: EventSink,
        config: Arc<ModelConfig>,
        primary: ResponsesClient<S>,
        middleware: ModelCallMiddlewareConfig,
    ) -> Self {
        #[cfg(target_family = "wasm")]
        let _ = middleware;
        Self {
            events,
            config,
            primary,
            server_reasoning_included: false,
            #[cfg(not(target_family = "wasm"))]
            kimi: middleware.kimi_refusal.map(KimiClient::new),
        }
    }

    pub(super) fn begin_turn(&self) -> ModelCallTurn {
        ModelCallTurn {
            #[cfg(not(target_family = "wasm"))]
            lease: self
                .kimi
                .as_ref()
                .map(|client| FallbackLease::new(client.max_lease_generations())),
            #[cfg(not(target_family = "wasm"))]
            transcript: None,
        }
    }

    pub(super) const fn server_reasoning_included(&self) -> bool {
        self.server_reasoning_included
    }

    pub(super) fn include_server_reasoning(&mut self, included: bool) {
        self.server_reasoning_included |= included;
    }
}

impl<S> ModelCallMiddleware<S>
where
    S: Service<ResponsesAttempt, Response = ResponsesServiceResponse> + AgentSend + 'static,
    S::Error: Into<NanocodexError>,
    S::Future: AgentSend,
{
    pub(super) async fn execute_primary(
        &mut self,
        request: ResponsesAttempt,
    ) -> std::result::Result<ResponsesServiceResponse, S::Error> {
        self.primary.execute(request).await
    }

    pub(super) async fn generate<C>(
        &mut self,
        conversation: &C,
        factory: &ResponsesAttemptFactory,
        thinking: Thinking,
        fast_mode: bool,
        stats: &mut RunStats,
        turn: &mut ModelCallTurn,
    ) -> Result<ModelCallOutput>
    where
        C: ModelCallContext,
    {
        loop {
            let call_index = stats.model_calls + 1;
            #[cfg(not(target_family = "wasm"))]
            if turn.uses_fallback() {
                let response = self
                    .perform_kimi_call(call_index, conversation, factory, stats, turn)
                    .await?;
                if turn.record_fallback_generation() {
                    self.emit_route_changed(
                        call_index,
                        KimiClient::model(),
                        MODEL,
                        "lease_expired",
                        0,
                    )?;
                }
                return Ok(ModelCallOutput {
                    call_index,
                    response,
                    primary_checkpoint: false,
                });
            }

            match self
                .perform_primary_call(
                    call_index,
                    conversation,
                    factory,
                    thinking,
                    fast_mode,
                    stats,
                )
                .await
            {
                Ok(response) => {
                    return Ok(ModelCallOutput {
                        call_index,
                        response,
                        primary_checkpoint: true,
                    });
                }
                Err(error) => {
                    #[cfg(not(target_family = "wasm"))]
                    if self.kimi.is_some()
                        && error
                            .responses_error()
                            .is_some_and(nanocodex_service::ResponsesError::is_cyber_policy)
                    {
                        let lease_generations = turn.grant_after_refusal().ok_or(
                            NanocodexError::InvalidAttemptState {
                                detail: "refusal fallback did not have lease state",
                            },
                        )?;
                        stats.safety_refusals += 1;
                        self.emit_route_changed(
                            call_index,
                            MODEL,
                            KimiClient::model(),
                            "cyber_policy",
                            lease_generations,
                        )?;
                        continue;
                    }
                    #[cfg(target_family = "wasm")]
                    let _ = turn;
                    return Err(error);
                }
            }
        }
    }

    /// Completes route bookkeeping at a final-answer boundary.
    ///
    /// Returns whether the response lacked a primary provider checkpoint and
    /// therefore requires complete typed-history replay on the next turn.
    pub(super) fn finish_turn(
        &self,
        call_index: u32,
        has_primary_checkpoint: bool,
        turn: &ModelCallTurn,
    ) -> Result<bool> {
        if has_primary_checkpoint {
            return Ok(false);
        }
        #[cfg(target_family = "wasm")]
        let _ = (call_index, turn);
        #[cfg(not(target_family = "wasm"))]
        if turn.uses_fallback() {
            self.emit_route_changed(call_index, KimiClient::model(), MODEL, "turn_completed", 0)?;
        }
        Ok(true)
    }

    async fn perform_primary_call<C>(
        &mut self,
        call_index: u32,
        conversation: &C,
        factory: &ResponsesAttemptFactory,
        thinking: Thinking,
        fast_mode: bool,
        stats: &mut RunStats,
    ) -> Result<TurnResult>
    where
        C: ModelCallContext,
    {
        let previous_response_id = conversation.previous_response_id();
        let started_at = Instant::now();
        stats.model_calls += 1;
        self.events.emit(
            AgentEventKind::ModelCallStarted,
            ModelCallStarted {
                call_index,
                model: MODEL,
                reasoning_mode: self.config.reasoning_mode.as_str(),
                effort: thinking.as_str(),
                previous_response_id,
            },
        )?;
        let request = factory.generation(
            call_index,
            conversation.prompt_history(),
            conversation.shared_history(),
            conversation.delta_start(),
            previous_response_id,
            thinking,
            fast_mode,
        );
        let (input_item_count, input_bytes, input_content) = trace_model_input(&request);
        let span = model_call_span(
            MODEL,
            call_index,
            self.config.reasoning_mode.as_str(),
            thinking.as_str(),
            previous_response_id.is_some(),
            input_item_count,
            input_bytes,
        );
        if let Some(input_content) = &input_content {
            record_span_content(&span, "model.input", input_content);
        }
        let success = match self.primary.execute(request).instrument(span.clone()).await {
            Ok(success) => success,
            Err(error) => {
                span.record("status", "failed");
                span.record("otel.status_code", "ERROR");
                span.record("duration_ns", elapsed_ns(started_at));
                return self.model_call_failed(MODEL, call_index, started_at, error.into(), stats);
            }
        };
        let attempt = success.attempt();
        let connection_generation = success.connection_generation();
        self.include_server_reasoning(success.server_reasoning_included());
        let ResponsesOutput::Generation(response) = success.into_output() else {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            return Err(NanocodexError::InvalidAttemptState {
                detail: "generation returned a non-generation response",
            });
        };
        let duration_ns = elapsed_ns(started_at);
        record_model_response(&span, &response);
        span.record("status", "completed");
        span.record("otel.status_code", "OK");
        span.record("duration_ns", duration_ns);
        if let Some(usage) = &response.usage {
            span.record("input_tokens", usage.input_tokens);
            span.record(
                "cached_input_tokens",
                usage
                    .input_tokens_details
                    .as_ref()
                    .map_or(0, |details| details.cached_tokens),
            );
            span.record("output_tokens", usage.output_tokens);
            stats.usage.add(usage);
        }
        stats.model_duration_ns += duration_ns;
        stats.last_response_id = Some(response.id.clone());
        self.events.emit(
            AgentEventKind::ModelCallCompleted,
            ModelCallCompleted {
                call_index,
                model: MODEL,
                response_id: &response.id,
                attempt,
                connection_generation,
                status: &response.status,
                duration_ns,
                time_to_first_event_ns: response.time_to_first_event_ns,
                time_to_first_output_ns: response.time_to_first_output_ns,
                tool_calls: response.code_calls.len(),
                usage: response.usage.as_ref(),
            },
        )?;
        Ok(response)
    }

    #[cfg(not(target_family = "wasm"))]
    async fn perform_kimi_call<C>(
        &mut self,
        call_index: u32,
        conversation: &C,
        factory: &ResponsesAttemptFactory,
        stats: &mut RunStats,
        turn: &mut ModelCallTurn,
    ) -> Result<TurnResult>
    where
        C: ModelCallContext,
    {
        let kimi = self
            .kimi
            .as_ref()
            .ok_or(NanocodexError::InvalidAttemptState {
                detail: "fallback route selected without configured middleware",
            })?;
        let model = KimiClient::model();
        let reasoning_effort = kimi.reasoning_effort();
        let request_prefix = factory.profile().prefix();
        let history = conversation.flattened_history();
        let input_item_count = request_prefix.len().saturating_add(history.len());
        let input_content = trace_content_enabled()
            .then(|| serde_json::to_string(&(request_prefix, &history)).ok())
            .flatten();
        let input_bytes = input_content.as_ref().map_or(0, String::len);
        let started_at = Instant::now();
        stats.model_calls += 1;
        stats.fallback_model_calls += 1;
        self.events.emit(
            AgentEventKind::ModelCallStarted,
            ModelCallStarted {
                call_index,
                model,
                reasoning_mode: "chat_completions",
                effort: reasoning_effort,
                previous_response_id: None,
            },
        )?;
        let span = model_call_span(
            model,
            call_index,
            "chat_completions",
            reasoning_effort,
            false,
            input_item_count,
            input_bytes,
        );
        if let Some(input_content) = &input_content {
            record_span_content(&span, "model.input", input_content);
        }
        let outcome = kimi
            .generate(
                &self.events,
                call_index,
                request_prefix,
                &history,
                conversation.history_revision(),
                factory.profile().prompt_cache_key(),
                &mut turn.transcript,
            )
            .instrument(span.clone())
            .await;
        let response = match outcome {
            Ok(response) => response,
            Err(error) => {
                span.record("status", "failed");
                span.record("otel.status_code", "ERROR");
                span.record("duration_ns", elapsed_ns(started_at));
                return self.model_call_failed(model, call_index, started_at, error.into(), stats);
            }
        };
        let duration_ns = elapsed_ns(started_at);
        record_model_response(&span, &response);
        span.record("status", "completed");
        span.record("otel.status_code", "OK");
        span.record("duration_ns", duration_ns);
        if let Some(usage) = &response.usage {
            span.record("input_tokens", usage.input_tokens);
            span.record(
                "cached_input_tokens",
                usage
                    .input_tokens_details
                    .as_ref()
                    .map_or(0, |details| details.cached_tokens),
            );
            span.record("output_tokens", usage.output_tokens);
            stats.usage.add(usage);
            stats.fallback_usage.add(usage);
        }
        stats.model_duration_ns += duration_ns;
        stats.last_fallback_response_id = Some(response.id.clone());
        self.events.emit(
            AgentEventKind::ModelCallCompleted,
            ModelCallCompleted {
                call_index,
                model,
                response_id: &response.id,
                attempt: 1,
                connection_generation: 0,
                status: &response.status,
                duration_ns,
                time_to_first_event_ns: response.time_to_first_event_ns,
                time_to_first_output_ns: response.time_to_first_output_ns,
                tool_calls: response.code_calls.len(),
                usage: response.usage.as_ref(),
            },
        )?;
        Ok(response)
    }

    fn model_call_failed<T>(
        &self,
        model: &str,
        call_index: u32,
        started_at: Instant,
        error: NanocodexError,
        stats: &mut RunStats,
    ) -> Result<T> {
        let duration_ns = elapsed_ns(started_at);
        stats.model_duration_ns += duration_ns;
        let message = error.to_string();
        self.events.emit(
            AgentEventKind::ModelCallFailed,
            ModelCallFailed {
                call_index,
                model,
                duration_ns,
                error: &message,
            },
        )?;
        Err(error)
    }

    #[cfg(not(target_family = "wasm"))]
    fn emit_route_changed(
        &self,
        after_model_call_index: u32,
        from_model: &str,
        to_model: &str,
        reason: &'static str,
        lease_generations: u32,
    ) -> Result<()> {
        self.events.emit(
            AgentEventKind::ModelRouteChanged,
            ModelRouteChanged {
                after_model_call_index,
                from_model,
                to_model,
                reason,
                lease_generations,
            },
        )?;
        Ok(())
    }
}

pub(super) struct ModelCallOutput {
    pub(super) call_index: u32,
    pub(super) response: TurnResult,
    primary_checkpoint: bool,
}

impl ModelCallOutput {
    pub(super) const fn has_primary_checkpoint(&self) -> bool {
        self.primary_checkpoint
    }
}

pub(super) struct ModelCallTurn {
    #[cfg(not(target_family = "wasm"))]
    lease: Option<FallbackLease>,
    #[cfg(not(target_family = "wasm"))]
    transcript: Option<KimiTranscript>,
}

#[cfg(not(target_family = "wasm"))]
impl ModelCallTurn {
    fn uses_fallback(&self) -> bool {
        self.lease
            .as_ref()
            .is_some_and(FallbackLease::uses_fallback)
    }

    fn grant_after_refusal(&mut self) -> Option<u32> {
        self.lease.as_mut().map(FallbackLease::grant_after_refusal)
    }

    fn record_fallback_generation(&mut self) -> bool {
        self.lease
            .as_mut()
            .is_some_and(FallbackLease::record_generation)
    }
}

#[cfg(not(target_family = "wasm"))]
struct FallbackLease {
    using_fallback: bool,
    remaining: u32,
    next_grant: u32,
    maximum: u32,
}

#[cfg(not(target_family = "wasm"))]
impl FallbackLease {
    const fn new(maximum: u32) -> Self {
        Self {
            using_fallback: false,
            remaining: 0,
            next_grant: 1,
            maximum,
        }
    }

    const fn uses_fallback(&self) -> bool {
        self.using_fallback
    }

    fn grant_after_refusal(&mut self) -> u32 {
        let grant = self.next_grant.min(self.maximum);
        self.using_fallback = true;
        self.remaining = grant;
        self.next_grant = grant.saturating_mul(2).min(self.maximum);
        grant
    }

    /// Returns true when the completed generation expires the lease.
    fn record_generation(&mut self) -> bool {
        if !self.using_fallback {
            return false;
        }
        self.remaining = self.remaining.saturating_sub(1);
        if self.remaining == 0 {
            self.using_fallback = false;
            return true;
        }
        false
    }
}

fn model_call_span(
    model: &str,
    call_index: u32,
    reasoning_mode: &str,
    reasoning_effort: &str,
    previous_response: bool,
    input_item_count: usize,
    input_bytes: usize,
) -> tracing::Span {
    info_span!(
        target: "nanocodex",
        "model.call",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        model,
        reasoning.mode = reasoning_mode,
        reasoning.effort = reasoning_effort,
        model.call_index = call_index,
        previous_response,
        model.input.item_count = input_item_count,
        model.input.bytes = input_bytes,
        model.response.id = tracing::field::Empty,
        model.response.status = tracing::field::Empty,
        model.response.end_turn = tracing::field::Empty,
        model.output.item_count = tracing::field::Empty,
        model.output.bytes = tracing::field::Empty,
        model.tool_call_count = tracing::field::Empty,
        assistant.output.bytes = tracing::field::Empty,
        status = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
        input_tokens = tracing::field::Empty,
        cached_input_tokens = tracing::field::Empty,
        output_tokens = tracing::field::Empty,
        reasoning.summary_count = tracing::field::Empty,
        time_to_first_event_ns = tracing::field::Empty,
        time_to_first_output_ns = tracing::field::Empty,
        stream.display_delta.count = tracing::field::Empty,
        stream.display_delta.bytes = tracing::field::Empty,
        stream.inter_delta_gap.max_ns = tracing::field::Empty,
        stream.inter_delta_stall_100ms.count = tracing::field::Empty,
    )
}

fn record_model_response(span: &tracing::Span, response: &TurnResult) {
    span.record("model.response.id", response.id.as_str());
    span.record("model.response.status", response.status.as_str());
    if let Some(end_turn) = response.end_turn {
        span.record("model.response.end_turn", end_turn);
    }
    span.record("model.output.item_count", response.output_items.len());
    span.record("model.tool_call_count", response.code_calls.len());
    let trace_content = trace_content_enabled();
    let mut output_bytes = usize::from(trace_content).saturating_mul(2);
    let mut serialized_items = 0_usize;
    let mut summary_count = 0_usize;
    for (index, item) in response.output_items.iter().enumerate() {
        let kind = if let ResponseItem::Reasoning { summary, .. } = item {
            summary_count = summary_count.saturating_add(summary.len());
            "reasoning"
        } else {
            "model.output_item"
        };
        if trace_content && let Ok(content) = serde_json::to_string(item) {
            output_bytes = output_bytes
                .saturating_add(usize::from(serialized_items != 0))
                .saturating_add(content.len());
            serialized_items = serialized_items.saturating_add(1);
            record_indexed_span_content(span, kind, index, &content);
        }
    }
    span.record("model.output.bytes", output_bytes);
    if let Some(message) = &response.final_message {
        span.record("assistant.output.bytes", message.len());
    }
    span.record("reasoning.summary_count", summary_count);
    span.record("time_to_first_event_ns", response.time_to_first_event_ns);
    if let Some(time_to_first_output_ns) = response.time_to_first_output_ns {
        span.record("time_to_first_output_ns", time_to_first_output_ns);
    }
    span.record(
        "stream.display_delta.count",
        response.pipeline_stats.display_delta_count,
    );
    span.record(
        "stream.display_delta.bytes",
        response.pipeline_stats.display_delta_bytes,
    );
    span.record(
        "stream.inter_delta_gap.max_ns",
        response.pipeline_stats.inter_delta_gap_max_ns,
    );
    span.record(
        "stream.inter_delta_stall_100ms.count",
        response.pipeline_stats.inter_delta_stall_100ms_count,
    );
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::FallbackLease;

    #[test]
    fn refusal_leases_back_off_exponentially_and_cap() {
        let mut lease = FallbackLease::new(4);

        assert_eq!(lease.grant_after_refusal(), 1);
        assert!(lease.uses_fallback());
        assert!(lease.record_generation());
        assert!(!lease.uses_fallback());

        assert_eq!(lease.grant_after_refusal(), 2);
        assert!(!lease.record_generation());
        assert!(lease.record_generation());

        assert_eq!(lease.grant_after_refusal(), 4);
        for remaining in (1..=4).rev() {
            assert_eq!(lease.record_generation(), remaining == 1);
        }
        assert_eq!(lease.grant_after_refusal(), 4);
    }
}
