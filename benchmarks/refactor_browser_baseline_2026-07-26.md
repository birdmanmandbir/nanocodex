# Browser and browser-VM baseline — 2026-07-26

This is the first reproducible baseline for the refactor's browser slice. It
separates the dependency-light typed protocol from real CDP work and from a
cached-image libkrun lifecycle. OCI resolution and image construction happen
before every timed browser-VM sample.

## Environment

- MacBookPro18,2, Apple M1 Max, 32 GiB
- macOS 26.3.1 (25D771280a)
- rustc 1.97.1 (`8bab26f4f`, LLVM 22.1.6)
- Cargo `bench` profile, optimized, debug information disabled
- Criterion 0.7, 20 samples for deterministic operations and 10 for live VM
- Alpine 3.22 browser image on a content-addressed 2 GiB ext4 disk
- headed Alpine Chromium under Xvfb, reached through private gvproxy/CDP
- libkrun firmware 5 and the current ad-hoc-signed release `vm-tools` VMM

The source worktree was based on `refactor/07-browser@309343c`; the
measurements include the uncommitted browser/controller composition recorded
by this document.

## Results

Criterion's reported interval is shown verbatim. It is a confidence interval
for the estimate, not a percentile claim. The broad live DOM intervals reflect
macOS VM scheduling across ten deliberately bounded samples.

| Operation | Fixture | Estimate |
| --- | --- | --- |
| typed action serialize + deserialize | representative semantic snapshot action | 215.49–217.27 ns |
| recording tool call | typed open action appended to an owned recording | 706.63–711.05 ns |
| retained recording | 100 heterogeneous typed actions | 59.368–60.048 µs total |
| warm text read | retained headed VM, local data page, `#status` | 3.3387–16.617 ms |
| warm semantic snapshot | retained headed VM, compact interactive tree | 8.8886–27.683 ms |
| warm screenshot | retained headed VM, 1920×1080 viewport | 41.889–46.917 ms |
| cached-image boot + first navigation + shutdown | private disk reflink, gvproxy, Xvfb, headed Chromium, typed CDP controller | 1.4502–1.4918 s |

The retained 100-action case sustains 1.6653–1.6844 million typed actions per
second, or about 0.60 µs per retained action.

Reproduce the deterministic measurements with:

```sh
just bench-browser
```

Prepare the exact content-addressed browser image, then run the live suite:

```sh
just prepare-browser-vm-image .cache/libkrunfw/libkrunfw
just bench-browser-live \
  .cache/browser-vm/builds/3f3f66dc5c70b2da77323f1ee1f0789b2bd61213c0d7eace6ef6bb2197af1f2d.ext4 \
  .cache/gvproxy/v0.8.9/gvproxy \
  .cache/libkrunfw/libkrunfw
```

The image hash changes whenever the Dockerfile, build context, base OCI
manifest, guest runtime, or image-builder identity changes. Use the path
printed by `prepare-browser-vm-image`; the hash above records this run rather
than naming a mutable "latest" image.

## Regression gates

Machine-local numeric budgets apply to this M1 Max baseline. Live browser and
hypervisor comparisons run on a pinned host; deterministic protocol checks can
run in ordinary CI.

| Operation | Budget |
| --- | --- |
| typed action serialize + deserialize | ≤ 350 ns |
| recording tool call | ≤ 1.2 µs |
| retained recording, per action | ≤ 1.0 µs |
| warm text read | ≤ 30 ms |
| warm semantic snapshot | ≤ 45 ms |
| warm screenshot | ≤ 75 ms |
| cached-image boot + first navigation + shutdown | ≤ 2.25 s |

Hard structural gates:

- browser actions and results remain typed; JSON is a wire/tool projection;
- one cloneable `Browser` owns one serialized action sequence and dedicated
  Chrome session;
- independent sessions never multiplex through one Chrome process or CDP
  endpoint;
- the browser VM attaches no empty egress share and never rebuilds its
  unchanged image;
- every VM receives a disposable disk, private gvproxy network, random
  host-loopback CDP endpoint, and unprivileged headed Chromium process;
- canonical Unix-socket paths are validated before libkrun can enter its VMM
  loop;
- startup failures retain bounded VMM, Chromium, and gvproxy diagnostics;
- controller close and timeout/drop cleanup terminate Chromium, gvproxy, the
  VMM, and the private disk together;
- screenshots and other produced artifacts remain bounded; and
- full-fidelity tracing observes the normal path and records complete ordered
  configuration, raw CDP events, actions, and results only when enabled.

## Live proof and feature parity

The live smoke prepared the image through public `GuestRuntimeDisk` and
`VmImageBuilder` APIs, booted the ad-hoc-signed release VMM, opened a data page,
returned the exact compact semantic snapshot and interactive reference,
captured a PNG, and shut down cleanly.

The first live attempt exposed two real integration bugs which are now
regression-protected: libkrun's command transport cannot represent literal
double quotes, so the browser bootstrap is an image-owned executable; and a
97-byte gvproxy path became 105 bytes when `/var` canonicalized to
`/private/var`, so the browser owns a short socket directory and `nanovm`
rejects overlong canonical paths before VMM entry.

Master's direct browser, remote-CDP, Brave-cookie handoff, storage-state,
passkey, React diagnostics, egress, canary, inspection, replay, audit,
profiling, screenshot, and CLI paths remain present. The VM composition adds a
hosting boundary around the same `Browser` and `BrowserTool`; it does not
replace or narrow the controller contract. The existing mixed `vm-tools`
consumer again proves that retained VM shell tools and the ordinary host
browser tool can execute concurrently through Code Mode.
