# VM-backed tools and egress

Nanocodex keeps the agent lifecycle independent from where workspace tools
execute. The default `Tools` selection runs `exec_command`, `write_stdin`,
`apply_patch`, and `view_image` in the embedding process. Applications can
replace those handlers with one persistent libkrun VM without changing their
model-visible names or schemas.

Four packages own the boundary:

- `nanovm` owns typed libkrun configuration, the small audited FFI boundary,
  private VMM process configuration, gvproxy lifecycle, and provider-neutral
  egress leases.
- `nanovm-image` owns content-addressed OCI resolution, the supported
  Dockerfile subset, immutable ext4 preparation, cache locking, and disposable
  attempt reflinks.
- `nanocodex-vm` owns the typed host/guest tool protocol, retained guest shell
  sessions, bounded VMM process ownership, and adapters for
  Nanocodex's standard workspace tools.
- `nanocodex-vm-egress` owns the standalone host proxy that composes MPP
  payment with policy-scoped secret injection and projects it as one neutral
  `EgressLease`.

Applications still choose agent identity, route policy, payment limits, secret
providers, and whether to enable VM tools.

## Preparing immutable images

`VmImageBuilder` turns a directory containing a concrete `Dockerfile` into one
validated immutable disk. The cache key includes the Dockerfile, deterministic
context archive, target architecture, base manifest digests, and disk size.
Every mutable VM gets a reflink or sparse copy:

```rust,no_run
use nanocodex_vm::GuestRuntimeDisk;
use nanovm_image::{CachePolicy, VmImageBuilder};

# async fn prepare() -> Result<(), Box<dyn std::error::Error>> {
let runtime = GuestRuntimeDisk::prepare(
    "target/aarch64-unknown-linux-musl/release/nanocodex-vm-guest",
    ".cache/vm",
)?;
let images = VmImageBuilder::new(
    "target/debug/vm-tools",
    runtime.path(),
)
.firmware_directory(".cache/libkrunfw/libkrunfw")
.vmm_arg("--vmm");
let image = images
    .prepare(
        "evals/history-derived/embedded-prompt/environment",
        10 * 1024 * 1024 * 1024,
        ".cache/vm",
        CachePolicy::Reuse,
    )
    .await?;
std::fs::create_dir_all(".nanocodex/attempts/018f")?;
image.reflink_to(".nanocodex/attempts/018f/rootfs.ext4")?;
# Ok(())
# }
```

`GuestRuntimeDisk::prepare` hashes the exact guest ELF, validates a compatible
Nanoeval `v2` cache entry when present, and otherwise formats and atomically
publishes one read-only 128 MiB ext4 runtime disk. Same-key processes
single-flight on a filesystem lock. The returned path remains in the
caller-selected cache after the value is dropped.

Dockerfile build VMs default to 2 vCPUs, 4096 MiB, ordinary internet egress, a
30-minute `RUN` timeout, and a 10-minute mount/`COPY` timeout.
`VmImageBuilder::cpus`, `memory_mib`, `egress`, `run_timeout`, and
`copy_timeout` make each policy explicit when those defaults are unsuitable.

OCI references and layers resolve concurrently, with at most eight operations
at either boundary. Same-key work single-flights across tasks and processes,
while unrelated images remain parallel. Cache records and disks publish
atomically; a valid warm disk hit never launches a VM or decodes layer
contents.

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

Host-owned tools compose on top of that boundary. For example, one `Browser`
can be cloned into the same factory while every workspace tool continues to
target the shared VM:

```rust,no_run
# use nanocodex::{Nanocodex, OpenAiAuth};
# use nanocodex_browser::{Browser, BrowserTool};
# use nanocodex_vm::VmToolSession;
# fn build(auth: OpenAiAuth, session: VmToolSession, browser: Browser) -> nanocodex::Result<()> {
let vm = session.tools();
let (agent, events) = Nanocodex::builder(auth)
    .tools_factory(move |_agent| {
        vm.tools_builder()
            .working_directory("/workspace")
            .tool(BrowserTool::from_browser(browser.clone()))
            .build()
    })
    .build()?;
# drop((agent, events));
# Ok(())
# }
```

This keeps browser lifecycle, authentication state, and CDP policy on the host
while `exec_command`, `write_stdin`, `apply_patch`, and `view_image` share one
guest runtime across the root agent and its subagents.

## Configurable egress

`EgressLease` is a provider-neutral VM capability. It can carry:

- a network mode;
- guest environment such as an authenticated `HTTP_PROXY`/`HTTPS_PROXY`;
- read-only mounts and public configuration such as a proxy CA; and
- lifecycle guards that keep revocable host services alive for the VM.

Compatible leases compose transactionally and conflicting environment, mount,
file, or network claims fail closed. Application code normally receives a
complete lease from `nanocodex-vm-egress` rather than assembling proxy values
itself.

### One host proxy for MPP and secrets

`VmEgress` owns one authenticated HTTP(S) front proxy. Requests with no secret
route use its ordinary MPP path: a `402` is paid and the exact bounded request
is replayed by the host. A configured route first authorizes identity, origin,
method, and path, then resolves and injects its secret on the host. The guest
receives the route's public base URL, proxy capability, and public CA. The
egress layer never puts the resolved value or payment provider in that
configuration.

```rust,no_run
use std::sync::Arc;

use mpp::client::MultiProvider;
use nanocodex_vm_egress::{
    CompositeSecretManager, EgressContext, EnvironmentSecretManager,
    SecretDelivery, SecretGuestConfig, SecretHttpMethod, SecretRef,
    SecretRequestRule, SecretSpec, StaticSecretPolicy, VmEgress,
};

# async fn configure() -> Result<(), Box<dyn std::error::Error>> {
// The host variable is NANOCODEX_SECRET_OPENAI_API_KEY. It is read again for
// every authorized request, so rotation needs no guest restart.
let manager = CompositeSecretManager::new().provider(
    "environment",
    Arc::new(EnvironmentSecretManager::new("NANOCODEX_SECRET_")),
);
let openai = SecretSpec::builder(
    "openai-responses",
    SecretRef::new("environment", "OPENAI_API_KEY"),
    "https://api.openai.com",
    SecretDelivery::inject_header("authorization", "Bearer "),
    SecretGuestConfig::new("OPENAI_BASE_URL"),
)
.rule(
    SecretRequestRule::new()
        .method(SecretHttpMethod::Post)
        .path_prefix("/v1/responses"),
)
.build()?;

// The empty provider rejects 402 challenges. Add a concrete provider with
// `MultiProvider::with` when this VM may spend through MPP.
let egress = VmEgress::builder(MultiProvider::new())
    .secrets(
        EgressContext::new(
            "agent-019c-0000-7000-8000-000000000001",
            "service:nanocodex-local",
        ),
        Arc::new(StaticSecretPolicy::new([openai])),
        Arc::new(manager),
    )
    .spawn()
    .await?;
let lease = egress.lease();

assert_eq!(
    lease.guest_environment().get("OPENAI_BASE_URL"),
    Some(&"https://api.openai.com".to_owned()),
);
assert!(
    !lease
        .guest_environment()
        .contains_key("NANOCODEX_SECRET_OPENAI_API_KEY"),
);

drop(lease);
egress.shutdown().await?;
# Ok(())
# }
```

`VmToolSession::spawn_configured` consumes the complete lease and applies it to
both launch configuration and retained session state. This selects the network,
attaches provider directories read-only, mounts them before the guest runtime
starts, installs public files, and retains every provider guard:

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

`StaticSecretPolicy` is the standalone immutable implementation. Managed
applications implement `SecretPolicy`; it is queried for every request, so
route revocation is immediate. `SecretManager` has file, environment,
heterogeneous composite, 1Password Connect, and optional 1Password SDK
implementations. Header injection and placeholder replacement are the only
credential-delivery modes. Redirects are disabled, ambiguous routes and
transport-owned headers are rejected, and configured-origin method or path
misses fail even when unrelated MPP traffic is allowed.

Existing direct MPP consumers remain supported: `MppEgress::start` and
`mpp_egress_layer(Arc<MppEgress>)` still produce the same lease without secret
policy. New code that needs both capabilities uses `VmEgress` so the guest has
one unambiguous `HTTPS_PROXY`.

`BrowserVm` consumes the same lease. It rewrites the host-loopback proxy
through gvproxy, installs the public CA into Chromium's system and NSS trust,
and uses a VM-owned extension that answers only proxy authentication
challenges. Origin authentication prompts never receive the proxy capability.

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
- The guest-only build selects the dependency-light OAI/tool contract and local
  workspace runtime. It does not link the OpenAI client, TLS, MCP, remote
  tools, or Code Mode. Normal native `nanocodex-tools` builds still include MCP
  and the complete agreed tool surface by default.

See
`cargo run -p nanocodex-examples --bin vm-tools -- ROOTFS GUEST_RUNTIME_BINARY`
for the end-to-end tool protocol example. Build the lean guest artifact with
`just build-vm-guest`; the example stages that ELF through
`GuestRuntimeDisk::prepare` and mounts the resulting disk read-only.
If the runtime argument is omitted, the rootfs must already contain
`/usr/local/bin/nanocodex-vm-guest`.

The retained baseline and regression budgets are recorded in
[`benchmarks/refactor_vm_baseline_2026-07-26.md`](../benchmarks/refactor_vm_baseline_2026-07-26.md).
