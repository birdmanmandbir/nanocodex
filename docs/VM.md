# VM-backed tools and egress

Nanocodex keeps the agent lifecycle independent from where workspace tools
execute. The default `Tools` selection runs `exec_command`, `write_stdin`,
`apply_patch`, and `view_image` in the embedding process. Applications can
replace those handlers with one persistent libkrun VM without changing their
model-visible names or schemas.

Two packages own the boundary:

- `nanovm` owns typed libkrun configuration, the small audited FFI boundary,
  private VMM process configuration, gvproxy lifecycle, and provider-neutral
  egress leases.
- `nanocodex-vm` owns the typed host/guest tool protocol, retained guest shell
  sessions, bounded VMM process ownership, and adapters for
  Nanocodex's standard workspace tools.

Neither package owns application policy, agent identity, payment limits,
secret resolution, rootfs preparation, or the choice to enable VM tools.

## Selecting host or VM tools

Host execution remains the default:

```rust,no_run
# use nanocodex::{Nanocodex, OpenAiAuth};
# fn build(auth: OpenAiAuth) -> nanocodex::Result<()> {
let (agent, events) = Nanocodex::builder(auth).build()?;
# drop((agent, events));
# Ok(())
# }
```

A caller that has started one `VmToolSession` opts into VM workspace effects
through the normal tool-selection API:

```rust,no_run
# use nanocodex::{Nanocodex, OpenAiAuth};
# use nanocodex_vm::{VmToolSession, VmTools};
# fn build(auth: OpenAiAuth, session: VmToolSession) -> nanocodex::Result<()> {
let vm = VmTools::new(session);
let tools = vm
    .tools_builder()
    .working_directory("/workspace")
    .default_shell("sh")
    .build()?;
let (agent, events) = Nanocodex::builder(auth)
    .workspace("/workspace")
    .tools(tools)
    .build()?;
# drop((agent, events));
# Ok(())
# }
```

`VmTools::tools_builder` replaces only workspace-effecting tools. Web search,
image generation, and `update_plan` retain their existing host-side behavior.
Callers can disable or replace those independently.

Use `NanocodexBuilder::tools_factory` when an agent can spawn or fork. The
factory must create a fresh root disk, VMM process, and `VmToolSession` for each
driver; a child must not share its parent's mutable guest or shell sessions.

## Configurable egress

`EgressLease` is the VM-facing output of application policy. A layer may
contribute:

- a network mode;
- guest environment such as an authenticated `HTTP_PROXY`/`HTTPS_PROXY`;
- read-only CA or configuration mounts; and
- lifecycle guards that keep revocable host services alive for the VM.

Independent provider layers compose transactionally:

```rust,no_run
use nanovm::{EgressLease, EgressMount};
use std::{path::PathBuf, sync::Arc};

# fn configure() -> Result<EgressLease, nanovm::EgressError> {
let mut mpp = EgressLease::internet();
mpp.insert_environment(
    "HTTPS_PROXY",
    "http://mpp-lease:credential@host.internal:8080",
)?;

let mut secrets = EgressLease::internet();
secrets.insert_environment(
    "NANOCENTAUR_SECRET_BASE_URL",
    "https://secret-gateway.internal/v1",
)?;
secrets.insert_mount(EgressMount {
    tag: "secret-ca".to_owned(),
    host_path: PathBuf::from("/host/secret-ca"),
    guest_path: PathBuf::from("/run/egress/secret-ca"),
})?;
secrets.retain(Arc::new(())); // the real layer retains its proxy lease

EgressLease::internet()
    .with_layer(mpp)?
    .with_layer(secrets)
# }
```

Applying the result to a `VmConfig` and `GuestCommand` selects the network,
attaches provider directories read-only, mounts them before the guest runtime
starts, and injects only the resolved guest environment:

```rust,no_run
# use nanovm::{EgressLease, GuestCommand, VmConfig};
# fn configure(egress: EgressLease) {
let guest = GuestCommand::new("/usr/local/bin/nanocodex-vm-guest").arg("/workspace");
let (vm, command) = egress.configure(
    VmConfig::ext4("rootfs.ext4"),
    &guest,
);
# drop((vm, command));
# }
```

The complete configuration can be serialized to a mode-`0600` temporary file
with `VmProcessConfig::write_private`. This keeps bearer proxy URLs and
secret-route placeholders out of the VMM command line.

### MPP and secret proxy layers

The MPP provider remains a host-owned HTTP(S) proxy. Its layer points the guest
at that proxy, mounts its public interception CA, and retains the wallet/proxy
guard. Ordinary guest commands such as `curl` therefore receive a `402` through
the proxy; the host pays and replays the exact bounded request without exposing
the wallet to the VM. Enable `nanocodex-vm`'s `mpp` feature and pass an
`Arc<MppEgress>` to `mpp_egress_layer` to produce that configuration.

NanoCentaur's Iron/secret egress follows the same contract: its layer carries
the scoped proxy or gateway route, public CA mount, placeholders, and revocable
lease guard. Resolved secrets remain host-side.

A guest process can have only one value for `HTTPS_PROXY`. If two independently
started providers both claim the front-proxy variables, lease composition
fails closed. An application that needs both proxies on one request path must
chain or route them host-side and expose one front proxy to the guest. The VM
package deliberately does not guess proxy order or silently overwrite one
provider's credentials.

## Lifecycle and security

- One VM tool session retains interactive guest processes across sequential
  turns.
- Dropping the session kills its VMM child. Explicit shutdown asks the guest to
  stop and bounds the wait for process exit.
- Failed partial protocol responses are not converted into successful tool
  results.
- Egress values are omitted from `Debug`; only environment names, mount
  metadata, and guard counts are shown.
- Read-only provider mounts and environment conflicts are explicit.
- The libkrun unsafe surface stays inside `nanovm`; the rest of Nanocodex
  remains safe Rust.

See `cargo run -p nanocodex-examples --bin vm-tools -- ROOTFS` for the
end-to-end tool protocol example. The rootfs must contain
`/usr/local/bin/nanocodex-vm-guest`.
