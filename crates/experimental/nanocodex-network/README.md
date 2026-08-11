# nanocodex-network

`nanocodex-network` is an unpublished standalone connectivity experiment. It
owns durable node identities, Iroh endpoint lifecycle, bootstrap admission,
live topology, and identity-bound direct paths. It deliberately supports one
transport: Iroh. It is not a provider abstraction.

The crate has no dependency on Nanocodex agents, evaluations, VMs, SQLite,
models, tools, payments, or TEEs. Applications choose identity paths and run
their own protocols over the resulting topology.

The initial topology uses one durable `Hub` as rendezvous and admission
authority. A durable `Node` joins that network and remains useful without
publishing a local service. Applications register a bounded `ProtocolId` with
`Node::listen` and open an opaque `PeerStream` with `Node::connect`. The hub
issues a short-lived, single-use grant bound to the requester, provider, and
protocol. Application bytes then travel directly between the authenticated
Iroh endpoint identities rather than through the hub.

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

`nanocodex-eval` is not part of this crate's runtime graph. The Nanocodex CLI is
the current composition root. `TcpBridge` is an optional adapter that publishes
one fixed loopback service from a provider node and exposes it on another joined
node. It is implemented over the same public protocol listener and peer stream
contract. Evaluation task claiming is merely the first application protocol
carried over that adapter.

The crate does not define task matching, service discovery, worker supervision,
TEE claims, or application framing. Those contracts stay in consuming
applications.

The CLI keeps the same boundary:

```sh
nanocodex network publish --target 127.0.0.1:8789
nanocodex network connect 'nanocodex-tcp:...' --port 8789
```
