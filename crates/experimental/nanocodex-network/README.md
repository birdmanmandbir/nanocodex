# nanocodex-network

`nanocodex-network` is an unpublished standalone connectivity experiment. It
owns durable node identities, Iroh endpoint lifecycle, bootstrap admission,
live topology, and identity-bound direct paths. It deliberately supports one
transport: Iroh. It is not a provider abstraction.

The crate has no dependency on Nanocodex agents, evaluations, VMs, SQLite,
models, tools, payments, or TEEs. Applications choose identity paths and run
their own protocols over the resulting topology.

The convenience `Hub::bind` and `Node::join` path uses Iroh's internet-capable
`N0` preset. Reachability policy is otherwise caller-owned. Durable identities
produce ordinary Iroh endpoint builders, so an application can compose LAN
addresses, self-hosted relays, DNS, mDNS, DHT-backed address lookup, endpoint
hooks, or custom transports before passing the bound endpoint to
`Hub::from_endpoint` or `Node::from_endpoint`. The library verifies that a
supplied endpoint uses the requested durable identity.

For example, an application can deliberately create a direct LAN endpoint
without changing the network or protocol APIs:

```rust,no_run
use iroh::{RelayMode, endpoint::presets};
use nanocodex_network::{Hub, JoinAuthority};

# async fn example(authority: &JoinAuthority) -> Result<(), Box<dyn std::error::Error>> {
let endpoint = authority
    .endpoint_builder(presets::Minimal)
    .relay_mode(RelayMode::Disabled)
    .clear_address_lookup()
    .bind()
    .await?;
let (_hub, _ticket) = Hub::from_endpoint(authority, endpoint).await?;
# Ok(())
# }
```

Model inference and application egress remain outside this crate. A LAN-only
peer topology may still call OpenAI directly or route application traffic
through a separately authorized gateway peer.

The identity, admission, gossip, signed catalog, authorization, and peer-stream
core also compiles for `wasm32-unknown-unknown`. Browser Iroh cannot open UDP
sockets or use direct address discovery, so browser endpoints are relay-only.
For a network that must stay offline and on one LAN, run an Iroh relay on that
LAN and give every browser node its trusted HTTPS/WSS relay URL. The
relay-disabled native example above is intentionally not a browser topology.
Using Iroh's default relay network makes the same API internet-capable instead.
See Iroh's [browser documentation](https://docs.iroh.computer/languages/wasm-browser)
for the underlying platform boundary.

Filesystem identity persistence and the loopback `TcpBridge` remain native-only.
WASM callers generate or import explicit secret bytes and persist them in an
application-owned encrypted store. Browsers expose raw authenticated byte
streams; an application can implement agent, MCP, control-plane, or egress
protocols without pretending the browser can bind a local TCP port.

The initial topology uses one durable `Hub` as rendezvous, admission authority,
gossip bootnode, and late-join anti-entropy cache. Every durable `Node` joins a
private Iroh gossip topic derived from the hub identity and the shared join
capability. Nodes spread small, signed capability records through that overlay
and merge them into their local peer catalogs. The hub is not the source of
those records and does not fan them out over its control channel.

Applications register a bounded `ProtocolId` with `Node::listen` and open an
opaque `PeerStream` with `Node::connect`. The hub issues a short-lived,
single-use grant bound to the requester, provider, and protocol. Application
bytes then travel over a separate Iroh connection between the authenticated
endpoint identities rather than through the hub. Iroh and the
application-configured endpoint decide whether that path is direct, relayed,
or carried by a custom transport.

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

`Node::catalog` and `Hub::catalog` return a cloneable `PeerCatalog` over the
same authenticated view fed by gossip. Applications can ingest a
`SignedAdvertisement` obtained from Kademlia, a control plane, retained cache,
or another discovery mechanism. Every source shares the same signature,
expiry, monotonic-revision, replay, and equivocation checks; callers may also
take deterministic filtered snapshots. Address lookup remains an independent
Iroh endpoint concern, so discovering a service and finding candidate network
paths do not collapse into one trust boundary.

The publication side is composable too. `AdvertisementLease::latest` returns
the current signed record and `AdvertisementLease::subscribe` yields that
record followed by freshly signed renewals. An application can mirror those
opaque records into Kademlia, an operational control plane, or a retained cache
without receiving the node's private key. All consumers later feed the same
record shape back through `PeerCatalog::ingest`:

```rust,no_run
# async fn mirror(
#     lease: &nanocodex_network::AdvertisementLease,
#     catalog: &nanocodex_network::PeerCatalog,
# ) -> Result<(), nanocodex_network::NetworkError> {
let mut signed_records = lease.subscribe();
while let Some(record) = signed_records.next().await {
    // A real adapter would put `record` in a DHT or control plane here.
    catalog.ingest(record).await?;
}
# Ok(())
# }
```

Protocol access has three independent application-policy seams. `Hub::builder`
can authorize routing, a provider's `NodeBuilder` can independently authorize
the incoming peer, and a caller's `NodeBuilder` can verify the provider's
attestation before receiving a usable stream. Every decision receives
authenticated requester and provider identities, the bounded `ProtocolId`, and
boundary-specific bounded opaque evidence. Policies default to allow-all for
compatibility, are time-bounded, and do not leak engine details in denials.

`Node::connect_with_credential` preserves the authority-only path: its evidence
is encrypted to the hub and never forwarded to the provider. Bilateral callers
use `Node::connect_with_credentials` and a `SessionCredentials` value containing
separate authority and end-to-end peer evidence. After provider-local policy
allows the request, the provider returns evidence from its `peer_attestor`; the
caller's `peer_verifier` must allow that evidence before application bytes can
flow through the returned `PeerStream`. The network never logs, interprets, or
persists any of these opaque values.

```rust,no_run
use nanocodex_network::{
    Hub, JoinAuthority, SessionCredential, SessionCredentials, SessionDecision,
};

# async fn example(
#     authority: &JoinAuthority,
#     endpoint: iroh::Endpoint,
#     node: &nanocodex_network::Node,
#     gateway: iroh::EndpointId,
#     egress: nanocodex_network::ProtocolId,
# ) -> Result<(), nanocodex_network::NetworkError> {
let (_hub, _ticket) = Hub::builder(authority, endpoint)?
    .session_authorizer_fn(|request| async move {
        if request.protocol().as_str() == "nanocodex/egress/1"
            && request.credential().as_bytes() == b"example-offline-biscuit"
        {
            SessionDecision::Allow
        } else {
            SessionDecision::Deny
        }
    })
    .spawn()
    .await?;

let biscuit = SessionCredential::new(b"example-offline-biscuit".to_vec())?;
let credentials = SessionCredentials::shared(biscuit);
let stream = node
    .connect_with_credentials(gateway, &egress, credentials)
    .await?;
# drop(stream);
# Ok(())
# }
```

The provider configures its local side with
`NodeBuilder::incoming_session_authorizer` and either a fixed
`peer_attestation` or asynchronous `peer_attestor`; the caller uses
`NodeBuilder::peer_verifier`. This is the intended OIDC/Biscuit split: an
application performs OIDC enrollment while it has an identity-provider path,
issues short-lived endpoint- and protocol-scoped Biscuits, and evaluates them
at all required boundaries. The async attestor can read refreshed or revoked
credential state for every session without restarting the node. A provider can
enforce local sovereignty after hub policy allows a route, while a caller can
verify control-plane-attested provider claims before sending an MCP request,
prompt, or egress payload. Verification can remain fully offline on the LAN.
The same seams can authorize
`nanocodex/mcp/1`, `nanocodex/egress/1`, worker, or control protocols without
making Biscuit, OIDC, MCP, HTTP, or proxy semantics a network dependency.

For peer-provided internet access, an online peer advertises an application
egress protocol. An offline peer discovers it from any merged source, presents
its scoped credential, and opens an authenticated `PeerStream`; the consuming
application terminates that stream into `nanocodex-egress` or another bounded
gateway. The network library supplies identity, discovery, authorization, and
the byte stream—not a transparent system-wide proxy or an ambient right to use
another peer's WAN connection.

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

The party ticket also carries a bounded application credential. The hub allows
only host-originated agent sessions, every agent independently verifies the host
identity and credential, and the host verifies each agent's attestation before
sending a prompt. Each joining machine prefers `OPENAI_API_KEY` when set and
otherwise uses its existing Codex login. Only inference uses WAN; discovery and
peer streams do not. The host labels outbound prompts and inbound streams, and
every agent terminal shows its received prompt and streamed reply.

`nanocodex-eval` is not part of this crate's runtime graph. The Nanocodex CLI is
the current composition root. `TcpBridge` is an optional adapter that publishes
one fixed loopback service from a provider node and exposes it on another joined
node. It is implemented over the same public protocol listener and peer stream
contract. Evaluation task claiming is merely the first application protocol
carried over that adapter.

The crate does not rank candidates, schedule tasks, define resource semantics,
supervise workers, validate TEE claims, or own application framing. Those
contracts stay in consuming applications. Discovery queries only filter the
node's merged local authenticated catalog. The current shared join capability
is also the gossip-topic capability: it remains simple bootstrap admission and
is not individually revocable. Per-session authorization protects application
protocols independently; rotating or replacing cluster bootstrap admission is
a separate future boundary. OIDC enrollment, Biscuit verification, MCP
aggregation, operational policy, and peer-provided egress stay in consumers.

The CLI keeps the same boundary:

```sh
nanocodex network publish --target 127.0.0.1:8789
nanocodex network connect 'nanocodex-tcp:...' --port 8789
```
