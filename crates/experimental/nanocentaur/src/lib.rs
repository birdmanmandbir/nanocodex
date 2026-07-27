//! Durable, policy-aware managed agents built from Nanocodex components.
//!
//! Nanocentaur is the service layer above the headless
//! [`nanocodex_agent`] SDK. It owns tenant authentication, capability and
//! secret policy, `SQLite` command/event durability, actor wake-up, idempotency,
//! forking, cancellation, and REST/SSE projection. Model execution, tools,
//! context, VMs, and egress remain reusable lower-layer components.
//!
//! # Smallest complete server
//!
//! The mock factory exercises the same manager, persistence, policy, and HTTP
//! boundaries without making a model request:
//!
//! ```no_run
//! use std::{sync::Arc, time::Duration};
//!
//! use nanocentaur::{
//!     AdminAuthorizer, AgentManager, ApiState, FreePaymentGate,
//!     ManagedAgentFactory, MockAgentFactory, PolicyStore, SecretGateway,
//! };
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let state_directory = tempfile::tempdir()?;
//! let policy = Arc::new(PolicyStore::in_memory()?);
//! policy.bootstrap(
//!     "local-client",
//!     "Local development client",
//!     "dev-api-key",
//!     "local-principal",
//!     [],
//! )?;
//! let factory: Arc<dyn ManagedAgentFactory> =
//!     Arc::new(MockAgentFactory::new(Duration::from_millis(10)));
//! let manager = Arc::new(AgentManager::new(factory, state_directory.path())?);
//! let router = ApiState::new(
//!     manager,
//!     policy,
//!     Arc::new(AdminAuthorizer::new("dev-admin-token")?),
//!     Arc::new(FreePaymentGate),
//!     Arc::new(SecretGateway::new("http://127.0.0.1:3000")?),
//! )
//! .router();
//!
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//! axum::serve(listener, router).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Ownership
//!
//! [`AgentManager`] is the embeddable durable API. It owns lightweight
//! per-agent actors and one `SQLite` session store. [`ManagedAgentFactory`] is
//! the deliberate backend seam: [`NanocodexAgentFactory`] creates real
//! VM-backed agents, while [`MockAgentFactory`] is deterministic for tests and
//! harness benchmarks. [`ApiState`] adds transport; it does not become a
//! second lifecycle owner.
//!
//! Each completed turn retains typed output, authoritative aggregate token
//! usage, optional versioned USD cost, a session snapshot, and the exact
//! ordered event stream. Restart and fork read those same durable records.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod admin;
mod agent;
mod api;
mod auth;
mod capabilities;
mod egress;
mod manager;
mod mock;
mod payment;
mod policy;
mod secrets;
mod session;

pub use agent::{
    AgentError, AgentRunResult, AgentSpec, ManagedAgent, ManagedAgentFactory, ManagedTurn,
    ManagedTurnControl, NanocodexAgentFactory, RuntimeEvent, SpawnedAgent, run_guest_command,
    run_vmm, run_vmm_command,
};
pub use api::{ApiState, app};
pub use auth::{AdminAuthorizer, AuthorizationError};
pub use capabilities::{AgentCapabilities, CapabilityName, CapabilityNameError};
pub use egress::{
    CapabilityEgress, EgressContext, EgressError, EgressLease, EgressProvider, ManagedEgress,
    ProxyProfile,
};
pub use manager::{
    AgentEvent, AgentEventPayload, AgentManager, AgentStatus, AgentView, ContentBlock, CreateAgent,
    CreateAgentResponse, CreateTurn, EventCursor, ForkResponse, ForkSource, ManagerError,
    TurnAction, TurnActionResponse, TurnDelivery, TurnFailure, TurnStatus, TurnView,
};
pub use mock::MockAgentFactory;
pub use payment::{
    FreePaymentGate, PaymentError, PaymentGate, PaymentManagementResponse, PaymentManagementStatus,
    PaymentOutcome, PaymentReceipt,
};
pub use policy::{
    AgentConfig, AgentIdentity, ApiClientView, ApiKeyView, AuthenticatedClient, ContextBindingView,
    CreateApiClient, CreateApiKey, CreateContextBinding, CreatePermission, CreatePrincipal,
    CreateRole, EffectivePrincipal, PatchApiClient, PatchContextBinding, PatchPrincipal, PatchRole,
    PermissionView, PolicyError, PolicyStore, PrincipalMetadata, PrincipalView, ReasoningEffort,
    ResolveContext, ResolvedContextView, RoleView, require,
};
pub use secrets::{
    CompositeSecretManager, CreateSecret, EnvironmentSecretManager, FileSecretManager,
    MAX_SECRET_GATEWAY_REQUEST_BYTES, MAX_SECRET_GATEWAY_RESPONSE_BYTES, PatchSecret,
    SecretConfigError, SecretDelivery, SecretError, SecretGateway, SecretGatewayError,
    SecretGuestConfig, SecretHttpMethod, SecretManager, SecretRef, SecretRequestRule, SecretSpec,
    SecretView,
};
#[cfg(feature = "onepassword-sdk")]
pub use secrets::{
    ONEPASSWORD_CORE_SHA256, ONEPASSWORD_CORE_URL, ONEPASSWORD_CORE_VERSION,
    OnePasswordSdkConfigError, OnePasswordSdkSecretManager,
};
#[cfg(feature = "onepassword-connect")]
pub use secrets::{OnePasswordConnectConfigError, OnePasswordConnectSecretManager};
