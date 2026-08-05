# nanocodex-computer

`nanocodex-computer` is Nanocodex's owned native computer-use runtime. It is
not a Cua client, wrapper, or protocol adapter. The current backend directly
uses macOS Accessibility, CoreGraphics, and the system screenshot service.

The public boundary is deliberately split into three planes:

```text
agent / direct caller  -- ComputerAction --> serial native actor --> target PID
preview / observer    <-- events + frames -- observation actor <-- AX + capture
human                 -- pause/takeover --> ComputerControl  ----> action gate
```

- `Computer` is a cheap cloneable action handle. One private actor owns target
  selection, accessibility generations, input ordering, settling, and capture.
- `ComputerEvents` is an ordered bounded lifecycle stream with explicit lag
  markers. Additional consumers can subscribe through `Computer::events()`.
  `ComputerFrames` is a coalescing latest-frame stream, so a slow visual
  consumer never blocks input.
- `ComputerControl` pauses, resumes, records takeover, or stops the session
  without going through the model-facing action queue.
- `ComputerPreview` serves the current target window on an unguessable
  loopback-only URL with Pause, Resume, and Take over controls. It polls the
  coalescing frame stream and never enters the action path.
- `ComputerPip` renders the same frame stream in a non-activating native
  floating panel. The CLI hosts it in-process on its main-thread `LocalSet`;
  no helper application or service is involved.
- `ComputerTool` registers one always-available typed tool. With the default
  exposure it appears only inside Code Mode as `tools.computer`; callers that
  explicitly select mixed direct exposure also make it a direct Responses tool.

## Native behavior

The backend discovers graphical apps and layer-zero windows with CoreGraphics
and captures their isolated contents through macOS's bounded
`/usr/sbin/screencapture` service, with an in-process CoreGraphics fallback.
The service process has a three-second hard deadline and is terminated on a
stall. `attach` fixes an exact PID/window pair. `observe` returns a fresh PNG
plus a bounded accessibility tree containing roles, labels, values, actions,
bounds, and generation-bound references. Capture and accessibility collection
run concurrently. A reference includes its raw accessibility-tree index and a
fingerprint; resolving an action rebuilds metadata only for that candidate and
relocates it only when the fingerprint remains unique in the same generation.

The runtime enables Electron's public manual/enhanced Accessibility attributes
before traversal, matching the behavior needed for background control of apps
whose renderer tree is otherwise dormant. If a renderer remains dormant, an
idempotent `open_application` briefly activates it and restores the prior
frontmost application before returning. Semantic actions use `AXPress`,
settable `AXValue`, or another advertised AX action first. Coordinate clicks,
drags, scrolling, keys, and Unicode text use public `CGEventPostToPid`, so they
target the attached process without stealing clipboard contents. Generated
events carry a private marker. A listen-only event tap ignores those events and
pauses the actor only when physical input is directed at the attached
application; input in other applications can continue alongside background
work.

After a mutation the backend repeatedly captures visual samples and publishes
them as shared in-memory `ComputerFrame`s for human observers. It returns after
two equal samples or the configured timeout, then captures one bounded
accessibility tree for the final model-facing `ComputerObservation`. Visual
settling therefore does not rebuild the accessibility tree or advance semantic
element-reference generations. The final PNG is persisted once and becomes
high-detail image input only at the Nanocodex tool boundary. Capture memory is
pixel-bounded, and a session retains only its 16 newest screenshot artifacts.

## Example

```rust,no_run
use nanocodex_computer::{
    ApplicationSelector, Computer, ComputerAction, ComputerPreview,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (computer, mut events) = Computer::new()?;
    let preview = ComputerPreview::spawn_and_open(&computer).await?;

    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            eprintln!("{event:?}");
        }
    });

    computer
        .execute(ComputerAction::Attach {
            application: ApplicationSelector::BundleId("com.apple.TextEdit".into()),
            window_id: None,
        })
        .await?;
    computer
        .execute(ComputerAction::TypeText {
            text: "Hello from Nanocodex".into(),
        })
        .await?;

    println!("preview: {}", preview.url());
    computer.stop();
    Ok(())
}
```

The CLI integration is opt-in:

```sh
nanocodex --computer
nanocodex run "Open TextEdit and draft a note" --computer
nanocodex run "Inspect the current app" --computer --computer-preview=false
nanocodex run "Draft in TextEdit" --computer \
  --computer-allow-app com.apple.TextEdit
nanocodex run "Inspect example.com" --computer \
  --computer-allow-app com.apple.Safari \
  --computer-allow-url https://example.com
```

For a source-tree acceptance run, exercise that same production path rather
than the backend examples:

```sh
cargo run -p nanocodex-bin -- run \
  "Open TextEdit, type Hello from Nanocodex, and verify the text." \
  --computer --computer-preview=false
```

With preview enabled, the CLI opens a non-activating native floating PIP and
keeps the loopback controls available without foregrounding a browser. The PIP
shows the live system cursor, remains draggable and edge-resizable, and preserves
the user's placement and scale when the source window changes shape. In the
Ratatui consumer, the first frame also opens an adaptive live computer pane.
Kitty-capable terminals render the captured pixels; other terminals retain a
compact target/status fallback and can use the loopback preview. Human control
never passes through the model:

```text
/computer show
/computer hide
/computer pause
/computer resume
/computer takeover
/computer open
```

On first use, Nanocodex invokes the public macOS prompts for Screen & System
Audio Recording and Accessibility. Enable the executable macOS identifies,
then fully quit and relaunch it when the system prompt requests that. Automatic
takeover detection separately requires Input Monitoring. Permission denial is
typed and does not trigger a bypass. The loopback preview remains usable for
manual pause/takeover if Input Monitoring is unavailable.

The host can put a session in allowlist mode with repeated
`--computer-allow-app <bundle-id>` arguments (or `ComputerBuilder::allow_bundle_id`).
Discovery hides other applications and launch or attachment fails closed.
Computer actions also fail with a typed error while `loginwindow` owns the
foreground; Nanocodex never attempts to synthesize an unlock.
Repeated `--computer-allow-url <origin>` arguments enable URL allowlist mode.
Semantic links are checked before activation, and a browser `AXWebArea` outside
the allowed origins stops the session before any further computer action.

Cargo examples and the full CLI are distinct executables, so macOS may request
permission separately for `target/debug/examples/observe` and
`target/debug/nanocodex`. Rebuilding an ad-hoc development executable can also
require granting it again; a distributed application should use a stably signed
app or helper identity.

The lower-level example is only a backend diagnostic. To inspect the native API
directly, including a fresh Accessibility check of the typed value:

```sh
cargo build -p nanocodex-computer --example observe
target/debug/examples/observe com.apple.TextEdit \
  --type-text "Hello from Nanocodex" --preview
```

## Scope and security

- Native computer use is currently implemented only on macOS 14 or newer.
- The target must be explicitly attached; actions do not implicitly follow the
  frontmost app.
- The implementation uses public macOS APIs. It does not load SkyLight private
  SPI, inject into apps, scrape another product's credentials, or modify system
  privacy databases.
- Preview endpoints bind only `127.0.0.1` and are scoped by a random path
  capability. Images and state use `no-store` responses.
- Accessibility values and screenshot artifacts may contain sensitive user
  data. The caller controls their trace, artifact, and telemetry retention.
