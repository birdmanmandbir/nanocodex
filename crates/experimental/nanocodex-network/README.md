# nanocodex-network

`nanocodex-network` is an unpublished standalone connectivity experiment. It
owns durable node identities, Iroh endpoint lifecycle, bootstrap admission,
and identity-bound bilateral sessions. It deliberately supports one transport:
Iroh. It is not a provider abstraction.

The crate has no dependency on Nanocodex agents, evaluations, VMs, SQLite,
models, tools, payments, or TEEs. Applications choose identity paths and attach
their own protocols or bounded local service bridges.

The initial topology uses one durable hub as rendezvous and admission authority.
Once two nodes are registered, the hub can issue a single-use grant bound to
both endpoint identities and arrange a direct Iroh session between them. The
direct session does not traverse the hub.

`nanocodex-eval` is not part of this crate's runtime graph. The Nanocodex CLI is
the current composition root: it starts an eval HTTP service, asks this crate to
publish that loopback service, and gives each connector a durable node identity.
Task claiming remains ordinary application traffic over that bridge.

The current bilateral operation is intentionally only a direct-path proof. It
does not define task matching, arbitrary application protocols, worker
supervision, TEE claims, or a final node-session API; those contracts should be
introduced only with their first real consumer.
