# Experimental crates

This directory contains complete Nanocodex components whose APIs are still
being exercised and revised:

- `nanovm` and `nanovm-image`: embedded VM lifecycle and image preparation; and
- `nanocodex-vm`: agent tools backed by retained VM sessions.

Experimental means API stability, not reduced engineering standards. These
packages remain workspace members and must pass the normal formatting, Clippy,
documentation, test, cancellation, tracing, and benchmark gates. They are not
published as part of the stable crates.io release.

Stable crates may not depend on experimental crates. Executables, examples, and
evaluation adapters may consume them so that the APIs can mature against real
workloads before promotion into `crates/`.
