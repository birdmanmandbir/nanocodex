# Nanocodex examples

All language consumers live at this repository boundary:

- Rust: `minimal.rs`, `follow_on.rs`, `lifecycle.rs`, `custom_tool.rs`,
  `vm_tools.rs`,
  `browser_tool.rs`, `browser_bench.rs`, `browser_debug_bench.rs`,
  `browser_inspect.rs`, `browser_element_context.rs`,
  `react_doctor.rs`,
  `subagents.rs`, `resume.rs`, `fork_conversations.rs`,
  `fork_checkpoint_bench.rs`, and `mcp.rs` are binaries in the
  `nanocodex-examples` package.
- Python: `python/` uses the native PyO3 binding.
- Node.js: `node/` uses the shared Rust/WASM package with a Node WebSocket host.
- Browser: `react-vite/` runs that WASM agent in a module Worker and renders its
  ordered events in React.
- Browser CDN: `browser-cdn/` is one static HTML file that imports the published
  package directly, with no install or build step.

From the repository root:

```sh
cargo run -p nanocodex-examples --bin minimal
cargo run -p nanocodex-examples --bin lifecycle
cargo run -p nanocodex-examples --bin fork-conversations
cargo run -p nanocodex-examples --bin subagents
cargo run -p nanocodex-examples --bin subagents -- \
  "Review the retry policy using whatever clean or context-bearing workers you need"
NANOCODEX_SUBAGENT_JSONL=1 cargo run -p nanocodex-examples --bin subagents
cargo run -p nanocodex-examples --bin mcp
just build-vm-example
target/debug/vm-tools ROOTFS [GUEST_RUNTIME_BINARY_OR_EXT4] [--prove-mpp] [--prove-browser]
cargo run -p nanocodex-examples --bin browser-tool
cargo run -p nanocodex-examples --bin browser-tool -- \
  --cdp-endpoint ws://127.0.0.1:9222/devtools/browser/SESSION
cargo run -p nanocodex-examples --bin browser-bench -- \
  --sessions 4 --warm-reads 25
cargo run -p nanocodex-examples --bin browser-debug-bench -- \
  https://127.0.0.1:5173/ \
  --react \
  --activate Code \
  --settle-selector .code-workspace \
  --probe-selector "[role='treeitem']" \
  --cycles 50
cargo run -p nanocodex-examples --bin browser-inspect -- \
  https://127.0.0.1:5173/ \
  --element-context main \
  --activate Code
cargo run -p nanocodex-examples --bin browser-element-context -- \
  https://127.0.0.1:5173/ main
cargo run -p nanocodex-examples --bin react-doctor -- ./web --path src
cargo run -p nanocodex-examples --bin browser-screenshot -- \
  https://tempo.xyz /tmp/tempo.png
cargo run -p nanocodex-examples --bin browser-screenshot -- \
  https://example.com /tmp/example.png --expect-text "Example Domain"
cargo run -p nanocodex-examples --bin browser-screenshot -- \
  https://example.com /tmp/example-vm.png \
  --cdp-endpoint http://browser-vm.internal:9222 \
  --expect-text "Example Domain"
cargo run -p nanocodex-examples --bin browser-passkey -- \
  https://tempo.xyz/developers/docs/guide/payments/send-a-payment
cargo run -p nanocodex-examples --bin browser-brave-session -- \
  https://console.cloud.google.com \
  --allow-origin https://company.okta.com
cargo run -p nanocodex-examples --bin browser-brave-session -- \
  https://console.cloud.google.com \
  --cdp-endpoint ws://127.0.0.1:9222/devtools/browser/SESSION \
  --allow-origin https://company.okta.com
cargo run -p nanocodex-examples --bin browser-brave-session -- \
  https://console.cloud.google.com \
  --allow-origin https://company.okta.com \
  --auth-handoff
just smoke-python
just smoke-wasm-node
just build-react-example
```

The live programs require `OPENAI_API_KEY`. The browser example instead asks
the embedding application for an already-authorized Responses WebSocket URL;
standard browser WebSockets cannot attach the upgrade authorization header.

`vm-tools` does not call the model. It proves all VM-backed standard workspace
tools against one retained guest and accepts either a directory root containing
`/usr/local/bin/nanocodex-vm-guest` or an ext4 root plus a guest-runtime ELF or
read-only runtime image. A runtime ELF is packed into a temporary ext4 image,
and a supplied ext4 root is reflinked or sparse-copied into a private per-run
disk before boot. On macOS, `just build-vm-example` also applies the required
Hypervisor entitlement.
Pass `--prove-mpp` to additionally run a real guest `curl` through the
host-owned MPP proxy and verify one payment and one exact replay.
Pass `--prove-browser` to additionally compose the cloneable VM tools and one
host-owned browser in the same `Tools` value, then exercise both concurrently
from one Code Mode cell.

The Rust `browser-tool` example does not call the model and needs no API key. It
exercises the real browser through the same Code Mode nested-tool path used by
an agent. Its repeated `get_value` calls report warm Code Mode-to-browser p50
and p95 latency. By default it launches local Chrome or Chromium;
`--cdp-endpoint` drives a dedicated headed browser in a VM through the
identical tool path. It also emits its screenshot through Code Mode's
`image(...)` helper, proving that the outer model result receives pixels while
the direct Rust artifact and retained JSON stay file-backed and compact. It
also exercises `snapshot_find`, native click actionability, explicit text
waiting, and the builder-owned automatic post-action snapshot through that
same Code Mode boundary.
`browser-bench` measures lazy startup plus navigation, semantic snapshots, warm
DOM reads, screenshots, cleanup, and concurrent independent sessions. Repeat
`--cdp-endpoint` once per dedicated remote browser to benchmark VM sessions;
without endpoints, `--sessions` launches isolated local headless browsers.
`browser-debug-bench` measures a full real-frontend loop: explicit cold
startup, navigation, an optional UI transition, open-shadow-root document
shape, repeated semantic snapshots, text/style/geometry reads, browser
diagnostics, screenshot, and cleanup. `--console-events` and
`--network-events` stress the bounded diagnostic buffers; `--snapshot-output`
retains the semantic view for correctness inspection. `--react` installs the
pinned React diagnostics bootstrap before application code and reports
renderer, commit, Fiber, and typed-read measurements.
`browser-inspect` validates the full debugging surface through the same Code
Mode nested-tool binding used by an agent. It captures the typed flattened DOM
including shadow trees and layout, drains page and worker network records,
compares them with the page's Performance Resource Timing entries, reads an
on-demand child-target response body, reports WebSocket traffic, and aggregates
React commits into hot Fibers with source locations and render causes. React
instrumentation is supplied by the browser; the inspected app is unchanged.
`--element-context` additionally proves the typed rendered-element-to-component
path, including its symbolicated owner stack, source file, stable selector,
markup preview, and scoped styles.
`browser-element-context` is the focused Code Mode consumer for that path; it
avoids collecting unrelated full-page diagnostics on very large applications.
`react-doctor` is the direct library consumer for the independent Rust-native
source analyzer. The corresponding `ReactDoctorTool` is an ordinary typed
Code Mode tool; the Nanocodex CLI installs it with `--react-doctor`, and any
browser configuration installs it automatically so runtime evidence and source
findings can be correlated without injecting a source linter into the page.
`browser-screenshot` is a smaller direct-library consumer that copies a
viewport capture to a caller-selected host path; pass `--full-page` when the
site does not defer rendering until scroll. Repeatable `--expect-text` values
make it a deterministic browser smoke test. `--cdp-endpoint` drives a dedicated
Chrome instance in a VM, virtual display, or managed browser without changing
the library action API.
`browser-passkey` enables a virtual platform authenticator and exercises
registration, optional embedded-wallet consent, disconnect, and authentication
against the supplied page. It requires no model or API key.
`browser-brave-session` takes a consistency-safe, origin-filtered cookie
snapshot of an ordinary Brave profile and opens the requested page in a
separate headless Brave process. It does not attach to or mutate the visible
browser. With `--cdp-endpoint`, a short-lived invisible host broker decrypts
the filtered cookies and synchronizes them into a dedicated remote browser;
profile files and keychain state never enter the remote environment.
`--include-site-data` remains local-only. With `--auth-handoff`, the example
opens only the protected allowlisted URL in ordinary Brave, waits for the user
to complete a passkey or other auth gate, then takes a fresh cookie snapshot
and resumes. A remote browser is refreshed without being restarted.

`subagents` exposes generic `spawn_agent`, `fork_agent`, and `prompt_agent` Code
Mode tools; its Rust host contains no worker graph. The parent model decides the
orchestration topology and follow-ups from the goal. Initial workers return an
`agent_id` with their attributed report; `prompt_agent` sends later turns
through that child's retained session. `tools_factory` reinstantiates
agent-relative handlers with a weak `AgentHandle` for every driver. Its
`spawn()` method reuses private builder configuration without inheriting
conversation history, while `fork()` targets the agent that actually invoked
the tool.
The example prints only the final root answer by default. Set
`NANOCODEX_SUBAGENT_JSONL=1` to emit each child's lifecycle JSONL to stderr;
the records retain their native request IDs and sequence numbers without a
custom merged-event protocol.

The MCP example defaults to the public OpenAI documentation MCP. Override
`NANOCODEX_MCP_URL` for another Streamable HTTP server and set
`NANOCODEX_MCP_BEARER_TOKEN` when it requires bearer authentication.
