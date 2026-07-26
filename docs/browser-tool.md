# Browser library and tool

## Status

`nanocodex-browser` is an in-process Rust library over Chromium's typed
DevTools protocol. It does not invoke `agent-browser`, run a daemon, or
communicate through a CLI socket. The library launches Chrome directly, owns
the DevTools connection and active page, and keeps both warm across actions.

The public `Browser` handle is useful without Nanocodex:

```rust,ignore
use nanocodex_browser::{Browser, BrowserAction};

let browser = Browser::new()?;
browser.execute(BrowserAction::Open {
    url: "https://example.com".to_owned(),
}).await?;

let snapshot = browser.execute(BrowserAction::Snapshot {
    interactive: true,
    compact: true,
    depth: None,
    selector: None,
    include_urls: false,
}).await?;

browser.close().await?;
```

Explicit launch policy uses the builder:

```rust,ignore
let browser = Browser::builder()
    .executable("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    .build()?;
```

Deterministic page policy is harness configuration, not a model action:

```rust,ignore
use nanocodex_browser::{
    Browser, BrowserColorScheme, BrowserContext, BrowserReducedMotion,
    BrowserViewport,
};

let context = BrowserContext::default()
    .viewport(BrowserViewport::desktop(1440, 900))
    .locale("en-US")
    .timezone("America/Los_Angeles")
    .color_scheme(BrowserColorScheme::Dark)
    .reduced_motion(BrowserReducedMotion::Reduce)
    .init_script("globalThis.__testRun = 'stable';");
let browser = Browser::builder().context(context).build()?;
```

The same policy can fix user agent/platform, headers, HTTP credentials,
geolocation and permissions, media, CPU throttling, and network conditions. It
is installed before the first navigation of every tab. Credentials, header
values, and init scripts never become model-callable inputs.

Chrome can instead run behind a virtual display in a Linux VM or managed
browser service while the Rust harness stays in-process:

```rust,ignore
let browser = Browser::builder()
    .cdp_endpoint(Url::parse("http://browser-vm.internal:9222")?)
    .build()?;
```

This still uses the same typed `Browser` and `BrowserTool` APIs. CDP is the
browser-driver boundary, not a model-visible tool boundary. The endpoint must
be dedicated to this browser session: `Browser::close` closes that Chrome
instance. Keep it on loopback, a private VM network, or an authenticated
`wss://` endpoint rather than exposing DevTools publicly.

Unattended passkey flows are explicit browser policy:

```rust,ignore
use nanocodex_browser::{Browser, VirtualAuthenticator};

let browser = Browser::builder()
    .virtual_authenticator(VirtualAuthenticator::platform_passkey())
    .build()?;
```

The driver enables Chrome's WebAuthn domain after navigation and installs a
CTAP2 platform authenticator with resident keys, automatic presence, and user
verification. It applies the same policy to cross-origin iframe targets. This
keeps passkey prompts inside the isolated browser session instead of invoking
the host's passkey UI. `browser.virtual_credentials().await?` returns typed
public metadata for registered credentials; private keys and user handles never
cross the library boundary.

Authenticated Brave cookies can seed a separate headless session without
attaching the agent to the user's visible browser:

```rust,ignore
use nanocodex_browser::{BraveSession, Browser};
use url::Url;

let session = BraveSession::standard()?
    .profile_directory("Default")
    .allow_origin(Url::parse("https://console.cloud.google.com")?)
    .allow_origin(Url::parse("https://company.okta.com")?);
let browser = Browser::builder()
    .brave_session(session)
    .build()?;
```

The source profile may remain open. The library snapshots its cookie database
with SQLite's online backup API, copies Brave's encryption state, filters the
copy to cookies applicable to the explicit origins, and launches a new headless
Brave process against that private temporary profile. Headless writes never
reach the ordinary Brave profile. `brave_session` and `executable` are mutually
exclusive because the session must use the matching Brave binary to decrypt
its copied cookies. Session-cookie rows are made persistent for at most 24
hours inside the ephemeral copy so Chromium does not discard them as remnants
of another browser process; the temporary profile normally disappears much
earlier when `Browser::close` completes.

The same cookie policy composes with a dedicated remote browser:

```rust,ignore
let browser = Browser::builder()
    .cdp_endpoint(Url::parse("ws://127.0.0.1:9222/devtools/browser/...")?)
    .brave_session(session)
    .build()?;
```

In this mode a short-lived invisible Brave broker opens the filtered private
copy on the host, lets Brave and the operating-system keychain decrypt the
cookies, and exports typed cookie records into the dedicated CDP browser. The
broker then closes and its temporary profile is deleted. Cookie values are
never returned by a browser action or included in tracing. The Brave profile,
`Local State`, keychain, and encrypted cookie database never enter the VM.
The remote browser's existing cookies are replaced because the CDP endpoint is
required to be dedicated to this one `Browser` session.

This first authenticated-state slice deliberately imports cookies, including
HTTP-only cookies while Brave remains open. Add `.include_site_data()` to also
clone `localStorage`, `IndexedDB`, and storage-bucket metadata. That stronger
mode requires Brave to be closed at the instant the lazy browser first starts:
those databases use `LevelDB` and do not have SQLite's online backup guarantee.
On APFS their files are copy-on-write clones, so a large profile does not become
a second eagerly allocated copy. Service workers, client certificates, device
posture, extension state, and OS-integrated enterprise SSO are not imported.
Site-data cloning is local-only: combining `.include_site_data()` with
`.cdp_endpoint(...)` is rejected at build time because those stores are not
portable through the current typed remote boundary. Cookie-only Brave sessions
are supported across remote CDP.

Callers can also capture and restore portable typed cookies plus
`localStorage`/`sessionStorage` without involving Brave:

```rust,ignore
let state = browser.storage_state().await?;

let next = Browser::builder()
    .storage_state(state.clone())
    .build()?;

// Or replace state on an already-owned browser.
next.restore_storage_state(state).await?;
```

This is deliberately a direct `Browser` API, not a `BrowserAction`: the model
cannot read cookie values, inject a state file, or widen its authenticated
origins. `Debug` output redacts values, but the serialized
`BrowserStorageState` is credential-bearing data and must be protected like a
cookie jar.

When a real passkey, device posture check, or other human authentication gate
is required, the harness can perform an explicit handoff without giving the
model control of the user's browser:

```rust,ignore
let opened = browser
    .auth_handoff(Url::parse("https://console.cloud.google.com")?)?
    .open()?;

// The embedding application waits for its own user confirmation here.
opened.resume().await?;
```

`auth_handoff` accepts only an exact origin already configured on the
`BraveSession`. `open` launches that protected URL in the selected ordinary
Brave profile, where the user performs the passkey ceremony. `resume` takes a
fresh allowlisted cookie snapshot and reopens the protected URL. A local
headless browser is replaced with a fresh private process and profile. A remote
browser stays alive while its cookie set is refreshed in place, so the VM and
its display do not restart. The credential never crosses into Nanocodex. The
handoff is deliberately absent from `BrowserAction`, so a model cannot invoke
it or choose a different URL.

The separate `BrowserTool` adapter exposes that same browser through
Nanocodex's ordinary `Tool` boundary:

```rust,ignore
let browser = Browser::new()?;
let tools = Tools::builder()
    .without_defaults()
    .tool(BrowserTool::from_browser(browser.clone()))
    .build()?;
```

This separation lets an application use the typed library directly, share a
browser with another application component, or give one owned session to an
agent. `BrowserTool::new()?` is shorthand when the caller does not need the
direct handle.

The main CLI registers the adapter on demand:

```sh
cargo run -- run \
  "Use Code Mode to open https://example.com with the browser tool." \
  --browser
```

`--browser-cdp` enables the same tool against a dedicated browser elsewhere;
`--browser` is not also required:

```sh
cargo run -- run \
  "Open the requested page, detect any gate, and summarize it." \
  --browser-cdp http://browser-vm.internal:9222
```

Seed that remote browser with allowlisted Brave cookies by combining the
options. This is the normal invisible authenticated-VM configuration:

```sh
cargo run -- run \
  "Open Cloud Console and summarize the failed deployment." \
  --browser-cdp ws://127.0.0.1:9222/devtools/browser/SESSION \
  --browser-brave https://console.cloud.google.com \
  --browser-brave https://company.okta.com
```

Only the filtered cookie records cross the CDP boundary. Do not add
`--browser-brave-site-data` to a remote browser configuration.

Use `--browser-passkeys` instead when the site needs unattended passkey
registration or authentication. It enables the browser tool and the virtual
authenticator together:

```sh
cargo run -- run \
  "Open the Tempo send-payment demo, create an account, and report what happened." \
  --browser-passkeys
```

Use the same private Brave snapshot through the normal agent CLI by repeating
`--browser-brave` for every application or identity-provider origin:

```sh
cargo run -- run \
  "Open Cloud Console and summarize the failed deployment." \
  --browser-brave https://console.cloud.google.com \
  --browser-brave https://company.okta.com
```

Add `--browser-brave-site-data` when the workflow also needs `localStorage` or
`IndexedDB`; close Brave before the agent makes its first browser call.
`--browser-passkeys` may be combined with a Brave-backed session.
The `browser-brave-session` example adds `--auth-handoff` to demonstrate the
visible-user-authentication/fresh-headless-resume lifecycle and accepts
`--cdp-endpoint` for the VM path.

Only a local Chrome or Chromium installation is required. Chromium starts
lazily on the first action. `Browser::start().await` starts the same session
without navigating, which lets an embedding application overlap cold browser
startup with unrelated work. The opt-in Nanocodex CLI uses that path in the
background while its first model request is running.

## Code Mode

Code Mode receives one normal child tool rather than injected browser globals
or a second JavaScript runtime:

```javascript
const opened = await tools.browser({
  action: "open",
  url: "http://app:3000",
});

const page = await tools.browser({
  action: "snapshot",
});

const save = Object.entries(page.refs)
  .find(([, element]) =>
    element.role === "button" && element.name === "Save"
  )?.[0];

if (save === undefined) {
  throw new Error("Save button not found");
}

await tools.browser({
  action: "click",
  target: { by: "ref", reference: `@${save}` },
});

const screenshot = await tools.browser({ action: "screenshot" });
image(screenshot.image.modelImage);

const [textResult, vitals, accessibility] = await Promise.all([
  tools.browser({
    action: "get_text",
    target: { by: "css", selector: "main" },
  }),
  tools.browser({ action: "web_vitals" }),
  tools.browser({ action: "accessibility_audit" }),
]);

// Do not copy the base64-bearing modelImage back into text output.
text({ text: textResult.text, vitals: vitals.vitals, accessibility });
```

Compose an entire investigation in one Code Mode cell instead of returning to
the model after each action. The browser orders actions inside its session, so
use ordinary sequential `await` where one action changes what the next action
sees. `Promise.all` remains useful for independent reads and other independent
tools. There is intentionally no second browser batch language.

Direct Rust consumers receive file-backed image artifacts. The `BrowserTool`
adapter additionally puts a `modelImage` object on screenshot, baseline, diff,
and visual-anomaly results. Passing that object to Code Mode's `image(...)`
helper makes the pixels part of the outer model-visible result. The compact
typed record and event stream keep only artifact metadata and paths, not a
second base64 copy.

## Session traces and replay

An explicit session trace flushes each typed action and result to JSONL as it
finishes. Optional PNG and flattened-DOM captures are stored beside the event
stream; final network and console/error indexes are written on stop:

```rust,ignore
browser.execute(BrowserAction::SessionTraceStart {
    screenshots: true,
    dom_snapshots: true,
    max_actions: Some(500),
}).await?;

browser.execute(BrowserAction::Open {
    url: "https://example.com".to_owned(),
}).await?;

let stopped = browser.execute(BrowserAction::SessionTraceStop).await?;
let BrowserActionResult::SessionTrace { trace, .. } = stopped else {
    unreachable!();
};

let retained = trace.persist("./artifacts/repro-1").await?;
let replay_results = another_browser.replay(&retained).await?;
```

Trace control calls are skipped during replay; every other action is attempted
in original order, including actions that originally failed. Session-private
artifacts disappear when the owning browser closes, so call `persist` first
when evidence must outlive that session. `persist` rewrites artifact paths
inside the typed event stream and refuses to overwrite an existing directory.

## Protocol

The model-facing protocol uses a compact semantic snapshot and stable `eN`
references:

- `snapshot` returns compact accessibility-oriented text, the page origin, and
  an `eN` reference map;
- snapshots include same-origin and cross-origin child frames, and references
  retain their containing frame so ordinary `click`, `fill`, and DOM reads can
  act inside embedded applications;
- interaction actions accept a typed target: an `@eN` reference, CSS, role and
  accessible name, rendered text, label, placeholder, alt text, title, or test
  ID. `first`, `last`, and zero-based `nth` selection are explicit rather than
  hidden in a selector dialect;
- targeted reads expose text, inner HTML, values, attributes, element counts,
  bounding boxes, and computed styles;
- `evaluate` is the sole dynamic escape hatch for data not represented by a
  typed operation.

This keeps full HTML out of model context unless the model asks for it. The
reference convention is intentionally familiar to models trained on browser
agents, but its implementation and lifecycle belong to this Rust crate.

`BrowserAction` is a strict tagged enum:

- `open { url }`
- `reload`
- `detect_gate`
- `snapshot { interactive, compact, depth, selector, include_urls }`
- `snapshot_find { query, max_results }`
- `dom_snapshot { computed_styles, include_dom_rects, include_paint_order }`
- `click { target, options }`
- `fill { target, text }`
- `press { target, key, modifiers }`
- `hover { target }`
- `mouse_move`, `mouse_down`, `mouse_up`, and `mouse_wheel`
- `touch_tap` and `touch_swipe`
- `keyboard_down`, `keyboard_up`, and `insert_text`
- `scroll { target, x, y }`
- `select_option { target, values }`
- `set_checked { target, checked }`
- `drag { source, destination }`
- `upload_files { target, paths }`
- `set_viewport { width, height }`
- `go_back`
- `go_forward`
- `wait_for_selector { target, state }`
- `wait_for_text { text, target, hidden }`
- `wait_for_url { url_contains }`
- `wait_for_load_state { state }`
- `wait_for_function { expression }`
- `wait_for_timeout { milliseconds }`
- `screenshot { full_page, annotate }`
- `pdf { landscape, print_background, prefer_css_page_size, tagged, document_outline }`
- `visual_baseline { full_page }`
- `visual_diff { baseline_id, threshold, full_page }`
- `visual_trace_start { frames_per_second, max_frames }`
- `visual_trace_stop`
- `session_trace_start { screenshots, dom_snapshots, max_actions }`
- `session_trace_stop`
- `get_text { target }`
- `get_html { target }`
- `get_value { target }`
- `get_attribute { target, name }`
- `get_title`
- `get_url`
- `get_count { target }`
- `get_box { target }`
- `get_styles { target }`
- `matched_styles { target }`
- `force_pseudo_state { target, pseudo_classes }`
- `event_listeners { target, depth, pierce }`
- `debugger_set_pause_on_exceptions`
- `debugger_set_breakpoint` and `debugger_remove_breakpoint`
- `debugger_paused`, `debugger_resume`, and debugger step actions
- `storage_inspect`
- `console { limit }`
- `errors { limit }`
- `network_requests { filter, after, limit }`
- `network_body { request_id, kind }`
- `web_socket_messages { request_id, after, limit }`
- `react_events { after, limit }`
- `element_context { target }`
- `web_vitals`
- `performance_trace_start`
- `performance_trace_stop`
- `cpu_profile_start`
- `cpu_profile_stop`
- `coverage_start`
- `coverage_stop`
- `heap_snapshot { collect_garbage }`
- `heap_compare { before_id, after_id }`
- `heap_retainers { artifact_id, node_id, max_depth, max_nodes }`
- `heap_inspect { artifact_id, class_name, minimum_retained_size, max_nodes, include_duplicate_strings }`
- `video_start { frames_per_second, quality }`
- `video_stop`
- `list_frames`
- `evaluate_frame { frame_id, expression }`
- `new_tab { url }`
- `list_tabs`
- `select_tab { tab_id }`
- `close_tab { tab_id }`
- `dialog`
- `handle_dialog { accept, prompt_text }`
- `network_route { route_id, url_contains, response }`
- `remove_network_route { route_id }`
- `clear_network_routes`
- `set_offline { offline }`
- `export_har { include_bodies }`
- `accessibility_audit`
- `axe_audit`
- `lighthouse_audit { categories, form_factor }`
- `crux { scope, form_factor }`
- `downloads`
- `evaluate { expression }`

Results are also a strict tagged enum. Every result contains a monotonically
increasing session sequence and an `executed` flag. Mutations, snapshots,
targeted reads, screenshots, diagnostics, and evaluation each have their own
typed result variant; only arbitrary JavaScript evaluation contains a dynamic
JSON value.

Real mutating actions also return a typed outcome containing the final page
state, action-caused network counts and settling status, new console entries
and page errors, a pending dialog, and new downloads. The builder's
`.after_action(BrowserAfterAction::Snapshot)` policy additionally returns a
compact semantic snapshot with fresh references. The default summary avoids
paying for a snapshot when a direct library consumer does not need one.

Navigation tolerates an immediate redirect replacing Chromium's execution
context, and targeted DOM reads evaluate against the current document instead
of retaining stale node handles. `detect_gate` classifies visible CAPTCHA,
JavaScript-forwarding, and access-denial states. A forwarding page can be
transient, so Code Mode should wait briefly and check again before concluding
that a workflow is blocked.

Semantic snapshots, references, targeted reads, waits, counts, and raw CSS
selectors traverse open shadow roots. This keeps web-component-heavy
development tools such as file trees and code viewers visible without
flattening their DOM or requiring JavaScript evaluation.

Pointer and form actions enforce one strict match. They wait for the element
to be attached, rendered, stable across animation frames, enabled or editable
as appropriate, and unobscured at the native pointer hit target. Pointer,
keyboard, double-click, modifier, drag, and child-frame coordinate input is
sent through Chromium rather than by invoking an element's JavaScript methods.
Action completion observes only requests created after that action and returns
after relevant document, stylesheet, script, XHR, and fetch work settles or a
bounded timeout is reported in the outcome.

`snapshot_find` searches the complete semantic snapshot and returns bounded
matching windows, structural ancestors, and only the references visible in
those windows. It is the cheap discovery path for long pages.

`dom_snapshot` is the complete debugging path. Chromium captures one atomic
flattened snapshot containing light DOM, open, closed, and user-agent shadow
trees, template contents, child documents, layout rectangles, paint order, and
explicitly requested computed-style properties. Its result is fully typed but
can be large, so Code Mode should filter it before returning model-visible
text. Closed shadow content is inspectable through this read-only snapshot; it
does not become addressable by ordinary CSS-selector actions.

Console entries, page errors, and network requests use bounded recent-history
buffers. Reads return the newest 200 matching records by default, accept an
explicit limit up to 1,000, and report both the retained total and the number of
older records dropped. Failed requests include DevTools failure text rather
than remaining indistinguishable from requests that are merely pending.
Network records cover the page and recursively attached child targets,
including dedicated and module workers. Child targets are paused long enough
to enable capture before their first script runs, then resumed. Request and
response bodies are fetched only when requested rather than retained in every
summary. Network and WebSocket reads use monotonic cursors so Code Mode can
drain a busy session without silently losing records.
Console and page-error stack frames preserve one-based generated locations.
Inline and same-origin source maps are loaded through Chromium with the active
session's credentials and add original authored locations without discarding
the generated evidence.

`browser-inspect` exercises this complete path through the real Code Mode
nested-tool binding:

```sh
cargo run -p nanocodex-examples --bin browser-inspect -- \
  https://127.0.0.1:5173/ \
  --activate Code
```

It compares the typed `dom_snapshot` against a JavaScript composed-tree walk,
compares captured requests against `PerformanceResourceTiming`, enumerates
custom elements and shadow-tree state, drains WebSocket messages, and reads a
worker response body through its child DevTools session.

## Visual, performance, and accessibility diagnostics

`visual_baseline` retains a private PNG and returns an opaque artifact ID.
`visual_diff` captures the current page, performs a deterministic pixel
comparison, and returns change ratios plus a magenta diff image. Neither action
requires an application dependency.

`visual_trace_start` runs a bounded screenshot sampler in the owned browser
session. `visual_trace_stop` compares consecutive frames, incorporates the
page's cumulative layout shift, and returns typed `flash`, `blank_frame`, and
`large_visual_change` findings. Sampling is capped at 30 frames per second and
600 frames. Only up to three anomaly images are forwarded to Code Mode; all
artifacts remain available to the direct Rust caller.

`web_vitals` is the cheap read path. Instrumentation is installed before
application code and reports FCP, LCP, CLS, INP when observed, TTFB, document
milestones, long tasks, resource counts, and transferred bytes.
`performance_trace_start` and `performance_trace_stop` own a bounded Chromium
trace and return its private artifact path plus scripting, rendering, painting,
and long-task summaries. Typed findings cover long tasks, forced reflows, slow
selectors, layout shifts, LCP candidates, render-blocking resources, request
dependency chains, repeated JavaScript resources, cache opportunities, DOM
size, and third-party transfer cost. Trace collection drains data emitted
before Chromium's completion marker and fails explicitly rather than silently
returning a partial result when Chromium reports data loss.

`cpu_profile_start`/`cpu_profile_stop` retain the complete V8 profile and rank
the highest-sampled functions. `coverage_start`/`coverage_stop` retain precise
V8 coverage and report nested-range-correct used and unused bytes; start it
before loading the application when initial unused modules matter. A
`heap_snapshot` retains the complete V8 artifact and a bounded class summary
using self size plus the largest dominator-based retained size and representative
node ID for that class. Two retained snapshots can be compared with
`heap_compare`. `heap_retainers` then walks strong incoming references from a
representative node toward GC roots. Its depth and node bounds make the
reverse-reference graph safe to consume directly in Code Mode.
`heap_inspect` adds dominator-ranked retained objects, detached-node state, and
aggregated duplicate strings without returning the complete heap graph.

`matched_styles` returns authored declarations and their stylesheet URLs and
source ranges, including inheritance and pseudo-elements.
`force_pseudo_state` makes `:hover`, `:focus`, and related states
deterministic. `event_listeners` reports native listener provenance. Typed
debugger actions configure exception pausing and URL breakpoints, retain the
latest source-mapped pause stack and lexical-scope metadata, and expose
resume/step operations. `storage_inspect` lists service workers, Cache Storage
requests, IndexedDB databases, and local/session-storage keys without exposing
their values.

`video_start`/`video_stop` streams Chromium screencast frames directly to an
explicit `ffmpeg` child and returns a file-backed WebM artifact. Video is
optional and is the only diagnostic here with a non-Chromium runtime
dependency; callers may select its executable with `.ffmpeg_executable(...)`.

`accessibility_audit` is a fast, embedded, deterministic audit over the main
document, current child frames, and open shadow roots. It checks image
alternatives, interactive names, form labels, positive tab order,
`aria-hidden` focus targets, document language/title, duplicate IDs, and
heading order. Findings contain a typed impact, selector, frame ID, and frame
URL. It is useful in every development loop but is deliberately not presented
as a complete WCAG conformance determination.

`axe_audit` runs pinned axe-core 4.12.1 from an embedded, auditable bundle; the
inspected application does not install or fetch it. Findings retain rule help,
impact, tags, frame provenance, affected HTML, selector, and failure summary.
Its reproducible source and notices live under
`crates/nanocodex-browser/assets-src` and
[`AXE_NOTICE.md`](../crates/nanocodex-browser/assets/AXE_NOTICE.md).
`pdf` uses Chromium's print protocol and returns a private file-backed
artifact.

Exact Lighthouse remains deliberately opt-in because Lighthouse itself is a
Node application rather than a Chromium protocol primitive:

```rust,ignore
let browser = Browser::builder()
    .lighthouse_executable("./node_modules/.bin/lighthouse")
    .build()?;
```

`lighthouse_audit` attaches that executable to the already-owned Chrome
debugging port, disables storage reset, retains the complete JSON report, and
returns typed category scores plus bounded failing-audit summaries. Supplying a
different executable named `lighthouse` (for example, an unrelated blockchain
client) fails normally rather than being auto-discovered.

CrUX is a separate field-data path and requires explicit harness credentials:

```rust,ignore
use nanocodex_browser::BrowserCruxClient;

let browser = Browser::builder()
    .crux_client(BrowserCruxClient::new(std::env::var("CRUX_API_KEY")?))
    .build()?;
```

`crux` queries either the current normalized URL or its origin, with an
optional desktop/phone/tablet filter, and returns typed p75 values, histograms,
categorical fractions, and collection dates. The API key is absent from tool
schemas and redacted from `Debug`; HTTP failures discard their request URL
before becoming browser errors so the query-string credential is not exposed.

## Frames, tabs, inputs, and downloads

Snapshot references retain their frame ID, so ordinary clicks, fills, input
state changes, uploads, and targeted reads work in child frames without a
special selector dialect. `list_frames` and `evaluate_frame` cover explicit
frame diagnostics. Tabs use Chromium target IDs and remain within the same
owned browser session. Switching tabs reinstalls the page diagnostics and
network controls before returning.

The strict input surface includes scrolling, native select values, checked
state, drag/drop, and file uploads. Uploads may read only canonical paths below
the harness-configured `.file_root(...)`; the root is not model-callable.
Remote-CDP builds reject a host file root because the two processes do not share
a filesystem.

Downloads are forced into the session-private download directory.
`downloads` reports progress, final path, byte counts, and failure state. The
model cannot choose an arbitrary host destination.

## Network control and HAR

Network routes, offline emulation, and HAR export are ordinary typed actions.
A route has a stable ID, an explicit URL substring, and a deterministic typed
response. Routes are resolved before egress policy, which lets tests serve
fixtures for otherwise nonexistent domains without opening network access.
HAR export uses retained request ordering and redacted headers; response bodies
are fetched only when explicitly requested and available.

Egress policy is harness policy and therefore lives only on `BrowserBuilder`:

```rust,ignore
use nanocodex_browser::{Browser, BrowserEgressPolicy};
use url::Url;

let policy = BrowserEgressPolicy::deny_by_default()
    .allow_origin(Url::parse("https://app.example.com")?)
    .allow_domain("static.example.com")
    .allow_loopback(true);

let browser = Browser::builder()
    .egress_policy(policy)
    .build()?;
```

When configured, interception is enabled before the first real navigation for
the page and recursively attached frame and worker targets. `about:`, `data:`,
and `blob:` documents remain available. Restricted sessions bypass service
workers, disable non-proxied WebRTC UDP in Chrome, and replace the page's WebRTC
constructor before application scripts. Blocked requests fail closed and are
also retained as typed page errors. A Brave-backed session defaults to the
exact allowlisted cookie origins unless the caller supplies a different policy.

The policy contains browser HTTP, WebSocket, frame, and worker traffic at the
DevTools boundary. It does not turn a remote CDP endpoint into a trusted
transport: keep that endpoint private or authenticated, and treat the browser
process and its host as part of the embedding application's security boundary.

## React diagnostics

React diagnostics require no application dependency or source edit. Enable
the policy on the browser:

```rust,ignore
use nanocodex_browser::{Browser, ReactDiagnostics};

let browser = Browser::builder()
    .react_diagnostics(ReactDiagnostics::default())
    .build()?;
```

`nanocodex-browser` installs one pinned diagnostics bootstrap containing
[`react-scan/lite`](https://github.com/aidenybai/react-scan/tree/759b6dabbe179c8901e415d7df8b9393812b34ca/packages/scan/src/lite)
with Bippy 0.5.43 plus a small
[`react-grab`](https://github.com/aidenybai/react-grab)-derived element-context
primitive in the document's main world before application scripts. React Grab
itself is not a runtime dependency, so there is only one DevTools hook and one
Bippy instance. The artifact is local and auditable; the browser never fetches
code from a CDN. Its reproducible source, lockfile, versions, digest, and MIT
notices are recorded under `crates/nanocodex-browser/assets-src` and in
[`REACT_DIAGNOSTICS_NOTICE.md`](../crates/nanocodex-browser/assets/REACT_DIAGNOSTICS_NOTICE.md).
No toolbar is embedded.

The same bootstrap maps a rendered element back to its component and source.
Snapshot references retain their frame and shadow-root context; the action
accepts any ordinary typed target:

```javascript
const selected = await tools.browser({
  action: "element_context",
  target: { by: "ref", reference: "@e7" },
});

text({
  component: selected.context.componentName,
  source: selected.context.source,
  owners: selected.context.ownerStack,
  selector: selected.context.selector,
  snippet: selected.context.snippet,
  styles: selected.context.styles,
});
```

The DOM element and Fiber never cross the tool boundary. The Rust result is a
strict `BrowserElementContext`: component/source metadata, a symbolicated owner
stack, stable selector, compact markup, and scoped CSS. Strings and stack depth
are bounded in the document before serialization.

Code Mode reads the bounded typed stream with the same cursor pattern as
network diagnostics:

```javascript
let cursor = 0;
const events = [];
for (;;) {
  const page = await tools.browser({
    action: "react_events",
    after: cursor,
    limit: 1000,
  });
  events.push(...page.events);
  if (!page.has_more || page.last_sequence === null) break;
  cursor = page.last_sequence;
}

const hottest = events
  .filter((event) => event.kind === "commit")
  .flatMap((event) => event.tree)
  .sort((left, right) => right.actualDurationMs - left.actualDurationMs)
  .slice(0, 20);
```

Each event and Fiber is a strict Rust type. Commit trees contain stable Fiber
identities, source and owner metadata, durations, and typed explanations of
prop, state, context, hook, or parent changes. Renderer capability status is
returned with every read. Retention is bounded to 512 events per document;
reads return at most 1,000 and report dropped events. A full navigation creates
a fresh document and cursor sequence, so begin again at zero after `open` or
`reload`.

The CLI enables this policy automatically whenever its browser tool is
enabled. `--browser-react=false` is the explicit escape hatch. Direct library
consumers remain opt-in because profiling adds page work on every React commit
and renderer profiling hooks can conflict with the React DevTools Timeline
channel.

React 19.2 and later production or non-profiling builds do not expose
`injectProfilingHooks`, so per-component render/effect events cannot be
promised. Commit events and Fiber-tree/change/source/identity inspection remain
the reliable baseline. `status.renderers` and `profiling-hooks-status` events
report that limitation explicitly rather than silently returning partial data.
`element_context` does not require those profiling hooks and remains useful in
production builds when React exposes host-instance Fiber metadata.

Static source analysis is intentionally not injected into the page.
`nanocodex-react` is an independent Rust implementation built directly on Oxc:
it scans a bounded workspace, retains exact typed source spans, and exposes the
ordinary `react_doctor` tool. It does not invoke or link the React Doctor
package. The reusable library API is:

```rust,ignore
use nanocodex_react::{ReactDoctor, ReactDoctorTool};
use nanocodex_tools::Tools;

let doctor = ReactDoctor::builder("./web")
    .max_files(20_000)
    .max_file_bytes(2 * 1024 * 1024)
    .max_diagnostics(10_000)
    .build()?;

let direct_report = doctor.analyze_path("src")?;
let tools = Tools::builder()
    .tool(ReactDoctorTool::new(doctor))
    .build()?;
```

Runtime browser evidence stays in this crate and can be correlated with source
findings in Code Mode:

```javascript
const runtime = await tools.browser({
  action: "element_context",
  target: { by: "ref", reference: "@e7" },
});
const source = await tools.react_doctor({ path: "web/src" });

text({
  component: runtime.context.componentName,
  source: runtime.context.source,
  matchingFindings: source.diagnostics.filter(
    (diagnostic) =>
      runtime.context.source?.url.endsWith(diagnostic.path),
  ),
});
```

`browser-inspect` enables the policy and performs the aggregation in Code Mode.
Against this repository's React 19.2.6 `./web` application it reports commit
counts, profiling capability, hot Fibers, `web/src` creation sites, component
activity from the locally linked `js` packages, render counts, durations, and
change causes without changing the application.

The bounded `browser-canary` example repeatedly verifies expected rendered
content, the active origin, `navigator.webdriver`, and gate state while writing
a final or first-failure screenshot. It is a browser-engine correctness test,
not a promise that headed Chromium is undetectable:

```sh
cargo run -p nanocodex-examples --bin browser-canary -- \
  https://example.com \
  --cdp-endpoint ws://127.0.0.1:9222/devtools/browser/SESSION \
  --attempts 3 \
  --expect-text "Example Domain" \
  --evidence-directory /tmp/nanocodex-browser-canary
```

`browser-bench` reports typed JSON for lazy startup plus navigation, snapshot,
warm DOM-read percentiles, screenshot, cleanup, and total concurrent wall time:

```sh
cargo run -p nanocodex-examples --bin browser-bench -- \
  --sessions 4 \
  --warm-reads 25
```

For remote browsers, repeat `--cdp-endpoint` once per dedicated VM. The
benchmark never multiplexes logically independent sessions through one browser
process.

`browser-debug-bench` measures the representative local-development loop
against a real application rather than a synthetic page. For this repository,
start `web/` and inspect its Code surface:

```sh
cargo run -p nanocodex-examples --bin browser-debug-bench -- \
  https://127.0.0.1:5173/ \
  --activate Code \
  --settle-selector .code-workspace \
  --probe-selector "[role='treeitem']" \
  --cycles 50 \
  --snapshot-output /tmp/nanocodex-web-snapshot.txt
```

The typed report separates cold browser startup from navigation, records the
light and open-shadow DOM sizes, and reports p50/p95/max latency for snapshots,
text, computed styles, geometry, console, page errors, and network reads. Add
`--console-events 10000 --network-events 500` to stress long-running diagnostic
capture without allowing retained memory or result size to grow without bound.

## Lifecycle and isolation

`Browser` is a cheap cloneable handle to one logical session. Its clones share
one active page, reference map, diagnostics history, and action sequence.
Actions are serialized so a snapshot's references cannot race another action.
Independent `Browser::new()` calls receive independent private Chrome profiles
and temporary output directories.

Virtual passkeys live only for that browser session. They are neither imported
from the host nor persisted after the session closes.

`close().await` gracefully shuts down Chromium and permanently closes every
clone of that handle. Dropping the final handle also terminates its owned Chrome
process.

The library never attaches to a running personal browser. A local
`BraveSession` copies only allowlisted state into a private headless profile. A
remote `BraveSession` sends only decrypted, allowlisted cookie records to its
dedicated CDP browser. A normal `Browser` starts empty. Screenshots remain
inside the session's private temporary directory, and navigation accepts only
`http`, `https`, `data`, and `about` URLs. A Brave-backed session additionally
rejects explicit `open` actions outside its configured origins and defaults its
request policy to those exact origins. This blocks direct host-file navigation
and model-selected screenshot or download paths. A normal browser without
`.egress_policy(...)` deliberately retains unrestricted network access; choose
an explicit deny-by-default policy for untrusted pages or agents.
