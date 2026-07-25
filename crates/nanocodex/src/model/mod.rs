pub(crate) mod agent;
#[cfg(not(target_family = "wasm"))]
mod agents_md;
#[cfg(target_family = "wasm")]
#[path = "agents_md_wasm.rs"]
mod agents_md;
mod call_middleware;
mod compaction;
mod context_manager;
mod input;
mod telemetry;

#[cfg(not(target_family = "wasm"))]
pub(crate) use agents_md::load_global_instructions;
pub(crate) use call_middleware::ModelCallMiddlewareConfig;
#[cfg(not(target_family = "wasm"))]
use telemetry::ModelRouteChanged;
pub(crate) use telemetry::resolve_workspace;
use telemetry::{
    CompactionCompleted, CompactionFailed, CompactionStarted, ModelCallCompleted, ModelCallFailed,
    ModelCallStarted, RunError, RunStarted, RunStats, RunSteered, ToolCallArguments, ToolCallEvent,
    ToolResultEvent, WarmupCompleted, WarmupFailed, WarmupStarted, display_endpoint, elapsed_ns,
    record_indexed_span_content, record_span_content, serialize_trace_content, terminal_payload,
    trace_content_enabled, trace_model_input,
};

#[cfg(not(target_family = "wasm"))]
pub(crate) trait AgentSend: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send> AgentSend for T {}

#[cfg(target_family = "wasm")]
pub(crate) trait AgentSend {}
#[cfg(target_family = "wasm")]
impl<T> AgentSend for T {}
