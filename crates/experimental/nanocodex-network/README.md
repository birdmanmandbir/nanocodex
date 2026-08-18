# nanocodex-network

`nanocodex-network` is an unpublished standalone connectivity experiment. It
owns durable node identities, Iroh endpoint lifecycle, bootstrap admission,
live topology, and identity-bound direct paths. It deliberately supports one
transport: Iroh. It is not a provider abstraction.

The crate has no dependency on Nanocodex agents, evaluations, VMs, SQLite,
models, tools, payments, or TEEs. Applications choose identity paths and run
their own protocols over the resulting topology.

The normal `Hub::bind` and `Node::join` path is deliberately LAN-only. It uses
direct IP transports, publishes no DNS address records, and configures no Iroh
relay, internet probes, or gateway port mapping. Join tickets carry the hub's
current direct addresses, so every member must have a directly reachable path
to the host. Model inference is outside this crate; a consuming Nanocodex agent
may still call OpenAI over WAN.

The initial topology uses one durable `Hub` as rendezvous, admission authority,
gossip bootnode, and late-join anti-entropy cache. Every durable `Node` joins a
private Iroh gossip topic derived from the hub identity and the shared join
capability. Nodes spread small, signed capability records through that overlay
and materialize their own local cluster views. The hub is not the source of
those records and does not fan them out over its control channel.

Applications register a bounded `ProtocolId` with `Node::listen` and open an
opaque `PeerStream` with `Node::connect`. The hub issues a short-lived,
single-use grant bound to the requester, provider, and protocol. Application
bytes then travel over a separate direct Iroh connection between the
authenticated LAN-reachable endpoint identities rather than through the hub.

```rust,no_run
# async fn example(node: &nanocodex_network::Node, peer: iroh::EndpointId) -> Result<(), nanocodex_network::NetworkError> {
use nanocodex_network::ProtocolId;

let protocol = ProtocolId::new("nanocodex/example/1")?;
let mut listener = node.listen(protocol.clone()).await?;
let outgoing = node.connect(peer, &protocol).await?;
let incoming = listener.accept().await;
# let _ = (outgoing, incoming);
# Ok(())
# }
```

Nodes may also publish signed, expiring capability records. Services are typed
`ProtocolId`s. Other capability facts are application-owned string, unsigned
integer, boolean, or string-set attributes, so the networking crate can filter
`cpu.arch`, `worker.free_slots`, TEE kinds, and artifact hashes without owning a
worker model or scheduler.

```rust,no_run
# async fn example(node: &nanocodex_network::Node) -> Result<(), nanocodex_network::NetworkError> {
use nanocodex_network::{NodeAdvertisement, ProtocolId, Query};

let worker = ProtocolId::new("nanocodex.worker/1")?;
let lease = node
    .advertise(
        NodeAdvertisement::new(1)
            .with_service(worker.clone())
            .with_attribute("cpu.arch", "aarch64")
            .with_attribute("worker.free_slots", 2_u64),
    )
    .await?;

let query = Query::service(worker)
    .attribute_eq("cpu.arch", "aarch64")?
    .attribute_at_least("worker.free_slots", 1)?;
let mut peers = node.watch(query).await;
while let Some(change) = peers.next().await {
    println!("{change:?}");
}
# drop(lease);
# Ok(())
# }
```

The returned advertisement lease renews in the background. Dropping it stops
renewal; observers distinguish capability updates, query mismatches, and signed
lease expiration. Gossip-neighbor changes do not produce worker lifecycle
events because a node may remain reachable through another overlay branch.
Records are signed directly by the same durable Ed25519 identity authenticated
by Iroh. Replays and stale revisions are ignored, while conflicting content at
one identity and revision is rejected.

Each watcher coalesces pending changes by durable identity. Slow consumers see
the latest query-relative state for every changed peer instead of accumulating
an update-frequency-dependent queue, and initial snapshots are not capped at a
fixed fleet size. Advertisement renewal times are deterministically staggered
by identity within a safe pre-expiry window to avoid synchronized renewal
bursts after a fleet starts together.

The executable example runs a hub, an advertising worker, and a late-joining
client. The client discovers the worker through gossip and then exchanges
`ping`/`pong` over a direct peer stream:

```sh
cargo run -p nanocodex-network --example gossip_cluster -- hub ./hub.identity
cargo run -p nanocodex-network --example gossip_cluster -- \
  serve "$JOIN_TICKET" ./worker.identity aarch64
cargo run -p nanocodex-network --example gossip_cluster -- \
  dial "$JOIN_TICKET" ./client.identity aarch64
```

The public examples package composes the independent network and agent crates
into a real multiplayer consumer. One laptop hosts an authoritative round;
other laptops advertise retained Nanocodex agents, and every typed prompt and
streamed response crosses a direct authenticated peer session. Party traffic
stays on the LAN while each participating agent performs OpenAI inference over
its own WAN connection:

```sh
cargo run -p nanocodex-examples --bin lan-party -- host ./.party-host
cargo run -p nanocodex-examples --bin lan-party -- \
  join "$PARTY_TICKET" ./.party-alice alice
```

Each joining machine prefers `OPENAI_API_KEY` when set and otherwise uses its
existing Codex login. Only inference uses WAN; discovery and peer streams do
not. The host labels outbound prompts and inbound streams, and every agent
terminal shows its received prompt and streamed reply.

`nanocodex-eval` is not part of this crate's runtime graph. The Nanocodex CLI is
the current composition root. `TcpBridge` is an optional adapter that publishes
one fixed loopback service from a provider node and exposes it on another joined
node. It is implemented over the same public protocol listener and peer stream
contract. Evaluation task claiming is merely the first application protocol
carried over that adapter.

The crate does not rank candidates, schedule tasks, define resource semantics,
supervise workers, validate TEE claims, or own application framing. Those
contracts stay in consuming applications. Discovery queries only filter the
node's local authenticated cluster view. The current shared join capability is
also the gossip-topic capability: it is intentionally simple bearer admission,
not individually revocable worker credentials.

The CLI keeps the same boundary:

```sh
nanocodex network publish --target 127.0.0.1:8789
nanocodex network connect 'nanocodex-tcp:...' --port 8789
```
