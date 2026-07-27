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
# use nanocodex_vm::VmToolSession;
# fn build(auth: OpenAiAuth, session: VmToolSession) -> nanocodex::Result<()> {
let vm = session.tools();
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

Use `NanocodexBuilder::tools_factory` when an agent can spawn or fork. Start one
`VmToolSession` for the root agent tree and capture its clone-cheap `VmTools` in
the factory. Nanocodex invokes the factory once per driver, so agent-relative
tools are freshly bound to that driver while every driver deliberately shares
the same VM, filesystem, and retained guest shell sessions:

```rust,no_run
# use nanocodex::{Nanocodex, OpenAiAuth};
# use nanocodex_vm::VmToolSession;
# fn build(auth: OpenAiAuth, session: VmToolSession) -> nanocodex::Result<()> {
let vm = session.tools();
let (agent, events) = Nanocodex::builder(auth)
    .workspace("/workspace")
    .tools_factory(move |_agent| {
        vm.tools_builder()
            .working_directory("/workspace")
            .default_shell("sh")
            .build()
    })
    .build()?;
# drop((agent, events));
# Ok(())
# }
```

The `VmToolSession` is the non-cloneable graceful-shutdown capability.
`VmTools` and `VmToolSessionHandle` are cloneable capabilities. Every one of
them keeps the VMM, private launch configuration, and egress guards alive, so
capturing `VmTools` in a driver factory is sufficient for the complete agent
tree. Graceful shutdown fails while sibling capabilities remain; drop the
agents, tool registries, and cloned handles before calling it.

## Configurable egress

`EgressLease` is the VM-facing output of application policy. A layer may
contribute:

- a network mode;
- guest environment such as an authenticated `HTTP_PROXY`/`HTTPS_PROXY`;
- read-only provider directories and public guest configuration files; and
- lifecycle guards that keep revocable host services alive for the VM.

Independent provider layers compose transactionally:

```rust,no_run
use nanovm::{EgressFile, EgressLease, EgressMount};
use std::{path::PathBuf, sync::Arc};

# fn configure() -> Result<EgressLease, nanovm::EgressError> {
let mut mpp = EgressLease::internet();
mpp.insert_environment(
    "HTTPS_PROXY",
    "http://mpp-lease:credential@host.internal:8080",
)?;
mpp.insert_file(EgressFile::new(
    "/tmp/nanocodex/egress/mpp/ca.pem",
    b"public CA bytes".to_vec(),
    0o444,
))?;

let mut secrets = EgressLease::internet();
secrets.insert_environment(
    "NANOCENTAUR_SECRET_BASE_URL",
    "https://secret-gateway.internal/v1",
)?;
secrets.insert_mount(EgressMount {
    tag: "secret-ca".to_owned(),
    host_path: PathBuf::from("/host/secret-ca"),
    guest_path: PathBuf::from("/tmp/nanocodex/egress/secrets/ca"),
})?;
secrets.retain(Arc::new(())); // the real layer retains its proxy lease

EgressLease::internet()
    .with_layer(mpp)?
    .with_layer(secrets)
# }
```

`VmToolSession::spawn_configured` consumes the complete lease and applies it to
both launch configuration and retained session state. This selects the network,
attaches provider directories read-only, mounts them before the guest runtime
starts, injects only the resolved guest environment, provisions public files,
and retains every provider guard:

```rust,no_run
# use nanocodex_vm::VmToolSession;
# use nanovm::{EgressLease, GuestCommand, VmConfig};
# use tokio::process::Command;
# async fn launch(egress: EgressLease) -> Result<VmToolSession, Box<dyn std::error::Error>> {
let guest = GuestCommand::new("/usr/local/bin/nanocodex-vm-guest").arg("/workspace");
let mut vmm = Command::new("dedicated-vmm-process");
vmm.arg("--run-config");
let session = VmToolSession::spawn_configured(
    vmm,
    VmConfig::ext4("private-session-rootfs.ext4"),
    guest,
    egress,
).await?;
# Ok(session)
# }
```

The method serializes complete launch configuration to a mode-`0600` temporary
file and retains it until the last VM capability is dropped. This keeps bearer
proxy URLs and secret-route placeholders out of the VMM command line and avoids
a process-start race. Lower-level `configure`, `write_private`, `spawn`, and
`provision_egress` operations remain available for specialized launchers, but
the application must then preserve the same ownership ordering itself.

### MPP and secret proxy layers

The MPP provider remains a host-owned HTTP(S) proxy. Its layer points the guest
at that proxy, provisions its public interception CA, and retains the
wallet/proxy guard. Ordinary guest commands such as `curl` therefore receive a
`402` through the proxy; the host pays and replays the exact bounded request
without exposing the wallet to the VM. Enable `nanocodex-vm`'s `mpp` feature
and pass an `Arc<MppEgress>` to `mpp_egress_layer` to produce that
configuration.

NanoCentaur's Iron/secret egress follows the same contract: its layer carries
the scoped proxy or gateway route, public CA/configuration files, placeholders,
and revocable lease guard. Resolved secrets remain host-side and must never be
placed in an `EgressFile`.

A guest process can have only one value for `HTTPS_PROXY`. If two independently
started providers both claim the front-proxy variables, lease composition
fails closed. An application that needs both proxies on one request path must
chain or route them host-side and expose one front proxy to the guest. The VM
package deliberately does not guess proxy order or silently overwrite one
provider's credentials.

## Lifecycle and security

- One VM tool session is shared by the complete root-agent tree and retains
  interactive guest processes across sequential turns and subagent calls.
- Concurrent drivers are multiplexed by request ID; one slow guest command
  does not block unrelated subagent calls. Dropping an individual host request
  sends a targeted cancellation frame, and the guest aborts that request's
  process group without disturbing sibling work.
- The last session/tool capability kills its VMM child and releases egress.
  Explicit shutdown first rejects live sibling capabilities, asks the guest to
  cancel in-flight work and sync filesystems, then bounds the wait for exit.
- Protocol frames are limited to 64 MiB and carry binary fields as base64
  strings rather than allocation-heavy JSON byte arrays. Harness file reads
  accept only regular files and are limited to 32 MiB; trusted command output
  defaults to 8 MiB. Command timeouts, request cancellation, output-limit
  cancellation, guest shutdown, and capability drop terminate process groups,
  including descendants.
- Egress files are limited to 4 MiB. Mounts and files must be non-overlapping
  descendants of `/tmp/nanocodex/egress`; mount tags and modes are validated
  before launch.
- A writable ext4 root is session-private. Reflink or sparse-copy an immutable
  base image for each VM rather than attaching a shared benchmark image
  directly.
- On macOS, the dedicated VMM executable must carry the
  `com.apple.security.hypervisor` entitlement. `just build-vm-example` builds
  and ad-hoc signs the public proof binary with `nanovm.entitlements`.
- Failed partial protocol responses are not converted into successful tool
  results.
- Egress values are omitted from `Debug`; only environment names, mount
  metadata, and guard counts are shown.
- Read-only provider mounts and environment conflicts are explicit.
- The libkrun unsafe surface stays inside `nanovm`; the rest of Nanocodex
  remains safe Rust.

See `cargo run -p nanocodex-examples --bin vm-tools -- ROOTFS` for the
end-to-end tool protocol example. The rootfs must contain
`/usr/local/bin/nanocodex-vm-guest`. Build the lean guest artifact with
`just build-vm-guest`; its guest-only feature excludes `nanovm` and libkrun.
