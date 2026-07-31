# nanocentaur

Experimental host-managed Nanocodex sessions with durable SQLite state,
tenant policy, REST/SSE projection, VM-backed workspace tools, and host-owned
secret egress.

The normal embedding boundary is `AgentManager` plus a
`ManagedAgentFactory`. `MockAgentFactory` provides a deterministic backend;
`NanocodexAgentFactory` provisions the current `nanocodex-vm` workspace and
resumes completed `SessionSnapshot` values. `ApiState` adds the Axum
transport without becoming a second lifecycle owner.

See the [managed-agent guide](../../../docs/nanocentaur.md) for API, durability,
policy, egress, and server examples.
