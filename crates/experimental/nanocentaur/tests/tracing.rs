use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use nanocentaur::{
    AgentCapabilities, AgentConfig, AgentIdentity, AgentManager, ContentBlock, CreateTurn,
    EffectivePrincipal, MockAgentFactory, TurnDelivery,
};
use tracing::{Id, Instrument, Subscriber, info_span, span::Attributes};
use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan};

#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<HashMap<u64, CapturedSpan>>>);

#[derive(Clone, Debug)]
struct CapturedSpan {
    name: &'static str,
    parent: Option<u64>,
}

impl<S> Layer<S> for TraceCapture
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
        let parent = attributes
            .parent()
            .map(|parent| parent.clone().into_u64())
            .or_else(|| {
                attributes
                    .is_contextual()
                    .then(|| context.current_span().id().map(Id::into_u64))
                    .flatten()
            });
        self.0.lock().unwrap().insert(
            id.clone().into_u64(),
            CapturedSpan {
                name: attributes.metadata().name(),
                parent,
            },
        );
    }
}

fn identity() -> AgentIdentity {
    AgentIdentity {
        id: "019c-0000-7000-8000-000000000001".to_owned(),
        owner_client_id: "test-client".to_owned(),
        context_key: Some("trace-test".to_owned()),
        principal: EffectivePrincipal {
            id: "test-principal".to_owned(),
            agent_config: AgentConfig::default(),
            permissions: AgentCapabilities::default(),
            secret_revision: 0,
        },
        created_at: Utc::now(),
    }
}

#[test]
fn channel_work_keeps_explicit_bounded_parentage() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let dispatch = tracing::Dispatch::new(subscriber);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    tracing::dispatcher::with_default(&dispatch, || {
        runtime.block_on(
            async {
                let directory = tempfile::tempdir().unwrap();
                let manager = AgentManager::new(
                    Arc::new(MockAgentFactory::new(Duration::from_millis(1))),
                    directory.path(),
                )
                .unwrap();
                let identity = identity();
                manager.register(identity.clone()).await.unwrap();
                let accepted = manager
                    .create_turn(
                        identity.clone(),
                        CreateTurn {
                            delivery: TurnDelivery::Steer,
                            content: vec![ContentBlock::Text {
                                text: "Trace the complete managed operation.".to_owned(),
                            }],
                        },
                        Some("trace-request-1".to_owned()),
                    )
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if manager
                            .get_turn(identity.clone(), &accepted.turn_id)
                            .await
                            .unwrap()
                            .state
                            .is_terminal()
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                manager.shutdown().await.unwrap();
            }
            .instrument(info_span!("test.managed_request")),
        );
    });

    let spans = capture.0.lock().unwrap();
    let root = spans
        .iter()
        .find_map(|(id, span)| (span.name == "test.managed_request").then_some(*id))
        .unwrap();
    let agent_commands = spans
        .iter()
        .filter(|(_, span)| span.name == "nanocentaur.agent.command")
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    assert!(
        agent_commands
            .iter()
            .any(|id| spans.get(id).and_then(|span| span.parent) == Some(root)),
        "captured spans: {spans:?}",
    );
    assert!(
        spans.values().any(|span| {
            span.name == "nanocentaur.sqlite.command"
                && span
                    .parent
                    .is_some_and(|parent| agent_commands.contains(&parent))
        }),
        "captured spans: {spans:?}"
    );
    assert!(
        spans
            .values()
            .all(|span| !matches!(span.name, "nanocentaur.driver" | "nanocentaur.session"))
    );
}
