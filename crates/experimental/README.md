# Experimental crates

This directory contains complete Nanocodex components whose APIs are still
being exercised and revised:

- `nanovm` and `nanovm-image`: embedded VM lifecycle and image preparation;
- `nanocodex-vm` and `nanocodex-vm-egress`: agent tools and capability-scoped
  VM egress;
- `nanocodex-browser` and `nanocodex-browser-vm`: deterministic browser control
  locally and inside a headed VM; and
- `nanocentaur` and `nanocentaur-server`: durable managed agents composed from
  those components.

Experimental means API stability, not reduced engineering standards. These
packages remain workspace members and must pass the normal formatting, Clippy,
documentation, test, cancellation, tracing, and benchmark gates. They are not
published as part of the stable crates.io release.

Stable crates may not depend on experimental crates. Executables, examples, and
evaluation adapters may consume them so that the APIs can mature against real
workloads before promotion into `crates/`.
