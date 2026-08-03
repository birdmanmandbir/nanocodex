# nanocodex-computer

`nanocodex-computer` is Nanocodex's owned native computer-use runtime. It is
not a Cua client, wrapper, or protocol adapter. The current backend directly
uses macOS Accessibility, CoreGraphics, and ScreenCaptureKit.

The public boundary is deliberately split into three planes:

```text
agent / direct caller  -- ComputerAction --> serial native actor --> target PID
preview / observer    <-- events + frames -- observation actor <-- AX + capture
human                 -- pause/takeover --> ComputerControl  ----> action gate
```

- `Computer` is a cheap cloneable action handle. One private actor owns target
  selection, accessibility generations, input ordering, settling, and capture.
- `ComputerEvents` is an ordered bounded lifecycle stream with explicit lag
  markers. `ComputerFrames` is a coalescing latest-frame stream, so a slow
  visual consumer never blocks input.
- `ComputerControl` pauses, resumes, records takeover, or stops the session
  without going through the model-facing action queue.
- `ComputerPreview` serves the current target window on an unguessable
  loopback-only URL with Pause, Resume, and Take over controls. It polls the
  coalescing frame stream and never enters the action path.
- `ComputerTool` exposes one deferred Code Mode tool, `tools.computer`, without
  adding another always-visible model schema.

## Native behavior

The backend discovers graphical apps and layer-zero windows with CoreGraphics
and captures their isolated contents with ScreenCaptureKit (with a legacy
CoreGraphics capture fallback). `attach` fixes an exact PID/window pair. `observe` returns a
fresh PNG plus a bounded accessibility tree containing roles, labels, values,
actions, bounds, and generation-bound references. A reference includes a
fingerprint and is rejected if the tree changed or another observation made it
stale.

Semantic actions use `AXPress`, settable `AXValue`, or another advertised AX
action first. Coordinate clicks, drags, scrolling, keys, and Unicode text use
public `CGEventPostToPid`, so they target the attached process without stealing
clipboard contents. Generated events carry a private marker. A listen-only
event tap ignores those events and pauses the actor when it observes physical
clicks, scrolling, or keyboard input from the human.

After a mutation the backend repeatedly captures semantic and visual state. It
returns after two equal samples or the configured timeout, and reports whether
the final state settled. Screenshots are file-backed internally and become
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
```

On first use, Nanocodex invokes the public macOS prompts for Screen & System
Audio Recording and Accessibility. Enable the executable macOS identifies,
then fully quit and relaunch it when the system prompt requests that. Automatic
takeover detection separately requires Input Monitoring. Permission denial is
typed and does not trigger a bypass. The loopback preview remains usable for
manual pause/takeover if Input Monitoring is unavailable.

Cargo examples and the full CLI are distinct executables, so macOS may request
permission separately for `target/debug/examples/observe` and
`target/debug/nanocodex`. Rebuilding an ad-hoc development executable can also
require granting it again; a distributed application should use a stably signed
app or helper identity.

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
