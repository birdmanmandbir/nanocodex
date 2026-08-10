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
publishing a local service. Once two nodes are registered, the hub can issue a
single-use grant bound to both endpoint identities and prove a direct Iroh path
between them. Direct traffic does not traverse the hub.

`nanocodex-eval` is not part of this crate's runtime graph. The Nanocodex CLI is
the current composition root. `TcpBridge` is an optional adapter that publishes
one fixed loopback service through the hub and exposes it on a joined node.
Evaluation task claiming is merely the first application protocol carried over
that adapter.

The current bilateral operation is intentionally only a direct-path proof. It
does not define task matching, arbitrary application protocols, worker
supervision, TEE claims, or a final node-session API; those contracts should be
introduced only with their first real consumer.

The CLI keeps the same boundary:

```sh
nanocodex network publish --target 127.0.0.1:8789
nanocodex network connect 'nanocodex-net:...' --port 8789
```
