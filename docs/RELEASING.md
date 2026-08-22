# Releasing Nanocodex

Nanocodex releases eight crates.io packages in lockstep with the CLI binaries,
Linux VM guest, and npm package. GitHub remains the authoritative Git and tag
host, but GitHub Actions is not part of the release path. Cloudflare Workflows
build the artifacts, R2 stores them, and the Worker serves immutable manifests,
rolling channels, and downloads.

## Initial Cloudflare cutover

Do not point an installer or updater exclusively at an empty release ledger.
The one permitted bootstrap is the existing annotated `v0.5.0` release at
commit `e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1`, represented by annotated tag
object `9d8e097cd3eeb87e50809e14d54c46978f72a229`. No other tag or commit may use
the import namespace or become the first stable release.

Deploy the release ledger and public routes first, while both `latest` and
`nightly` are still absent. From a Nanocodex checkout with the public tag
available, run this one command boundary:

```sh
git fetch origin tag v0.5.0
test "$(git rev-parse refs/tags/v0.5.0)" = 9d8e097cd3eeb87e50809e14d54c46978f72a229
test "$(git rev-parse 'refs/tags/v0.5.0^{}')" = e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1
cd web
export CI_PUBLIC_ORIGIN=https://nanocodex.me-7fb.workers.dev
export CI_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-ci-release -w)"
npm run ci:import-v0.5-release
unset CI_RELEASE_TOKEN
```

The importer accepts no arguments or source overrides. Before its first
authenticated mutation it independently checks the local and public annotated
tag, fetches the three exact unauthenticated GitHub release downloads, verifies
their pinned sizes and hashes, derives the pinned `gzip -n -9` Linux asset, and
constructs the byte-pinned migration provenance and reduced five-asset draft.
It uploads only to `release-import/stable/v0.5.0/`, holds the global stable
publication lease while finalizing, and treats exact lost-acknowledgement
replays as idempotent. A different, partial, or already-advanced ledger fails
closed.

Success prints exactly:

```text
Cloudflare latest serves v0.5.0 at e4eea49fc6fab06a98ff01ec8c3da8d9a729eee1; all public bytes verified
```

That line is emitted only after the tool refetches the immutable manifest and
rolling `latest`, recomputes the canonical public-manifest digest, and streams
every immutable and rolling asset through bounded size, header, and SHA-256
verification. Afterward, smoke the shell installer and `nanocodex update` on
Linux and Apple Silicon before merging or deploying Cloudflare-only consumers.
`v0.5.0` is the sole Linux stable allowed to omit the VM guest; every later
Linux release installs the CLI and guest as one coherent version directory.
The `release-import/`, `distribution/stable/`, and `distribution/commit/`
namespaces must never receive an R2 expiration rule.

## Nightly releases

The production Worker runs the nightly dispatcher at 05:00 UTC. It proceeds
only when the current published `master` commit has a successful retained CI
result. A commit is built at most once: the Workflow ID is
`nightly-<full-commit>`, and an already-current nightly channel is left alone.

The distribution contains the x86_64 Linux CLI, static Linux VM guest, Apple
Silicon macOS CLI, exact tested npm tarball, `SHA256SUMS`, and
`PROVENANCE.json`. Successful output is finalized as both an immutable commit
manifest and the rolling `nightly` channel. `nanocodex update --nightly`
verifies the rolling pointer, refetches the immutable manifest, and validates
every selected asset's declared size and SHA-256 before installation.

The cron and the operator command share one idempotent dispatch boundary. To
retry the current authoritative green head, or to prove that an expected head
has not moved, run:

```sh
cd web
export CI_PUBLIC_ORIGIN=https://nanocodex.me-7fb.workers.dev
export CI_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-ci-release -w)"
npm run ci:nightly
npm run ci:nightly -- --head <full-40-hex-master-sha>
```

The pinned form can only create, resume, or report the deterministic
`nightly-<sha>` Workflow. Authentication, conflicts, and malformed evidence are
not retried; lost acknowledgements and bounded transient failures replay the
same request.

Old nightly clients still discover the rolling `nightly` release on GitHub.
Before disabling `nightly.yml`, use an old nightly installation on Linux and
Apple Silicon to install and run a current Cloudflare-aware updater through that
surface, then prove its next `nanocodex update --nightly` resolves and installs
the exact Cloudflare nightly. After the Cloudflare manifest is final, disable
`nightly.yml`, wait for every run to become terminal, and mirror only those
already-finalized bytes:

```sh
export NANOCODEX_GITHUB_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-github-release -w)"
export CI_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-ci-release -w)"
npm run ci:mirror-github-release -- --dry-run nightly <full-40-hex-commit>
npm run ci:mirror-github-release -- nightly <full-40-hex-commit>
unset NANOCODEX_GITHUB_RELEASE_TOKEN CI_RELEASE_TOKEN
```

The bridge requires the existing rolling release, refuses an unowned or
ambiguous draft, and verifies the public GitHub downloads used by old clients.
A published bridge marker is not authority by itself: before replacing those
assets, the bridge fetches the marker's referenced immutable Cloudflare commit
release and requires its manifest digest and all three compatibility bytes to
match the GitHub release. Keep the bridge available until the crossing test has
covered the supported platforms; it never builds or publishes a registry
package. Its GitHub token needs `Administration: read`, `Actions: read`, and
`Contents: write`; it does not need `Workflows: write`.

## Pull-request previews

Every pull request runs the same tested-merge CI as `master`. A successful run
publishes the native Linux/macOS binaries and a separate npm tarball with exact
version `0.0.0-preview-<tested-merge-sha>` to R2. The release-grade package is
sealed first and keeps the repository version; preview mutation can never enter
stable or nightly staging. Resolve the current open pull request at:

```text
GET /api/ci/pull-requests/<number>
```

The response contains the npm `artifact.downloadPath` and a SHA-256-bound
native manifest. The npm URL can be passed directly to `npm install`.
`nanocodex update --pr <number>` resolves the current GitHub PR head, refetches
the immutable Cloudflare manifest, verifies the platform binary, and rechecks
the current PR pointer after download. This replaces the former pkg.pr.new and
GitHub Actions artifact path; no GitHub App or workflow dispatch is required.

## Changelogs

The root `CHANGELOG.md` is generated by `git-cliff` from every commit. Each of
the eight published crates carries its own path-filtered
`crates/*/CHANGELOG.md`. Conventional prefixes group changes into Features,
Bug Fixes, Documentation, Dependencies, Performance, Refactor, Styling,
Testing, Miscellaneous Tasks, and Other.

Use conventional commit subjects (`feat:`, `fix:`, `docs:`, `perf:`,
`refactor:`, `test:`, `chore:`, `ci:`, or `build:`). GitHub labels may still be
used for review and project organization, but the public release record is the
committed changelog plus the Worker-owned immutable manifest; release
correctness does not depend on GitHub-generated notes.

## Prepare a release

Install the release tools once:

```sh
cargo install git-cliff --locked
cargo install cargo-semver-checks --locked
```

Then prepare a release pull request from the latest `master`:

1. Choose a new semantic version. Update `workspace.package.version` and every
   `nanocodex*` entry in `workspace.dependencies` in `Cargo.toml`. Keep
   `js/bindings/package.json` and
   `py/bindings/python/nanocodex/__init__.py` on the same version. Never reuse a
   version already present on crates.io or npm.
2. Run `cargo check --workspace` to refresh `Cargo.lock`.
3. Run `just changelog x.y.z`, then review the root and eight crate changelogs.
   Fix misleading commit subjects and regenerate instead of maintaining a
   second grouping scheme by hand.
4. Starting with the second published version, run
   `cargo +stable semver-checks` and resolve every API break.
5. Run `just release-check x.y.z` and the normal `just check` release gate.
6. Merge through normal review and wait for the trusted master controller to
   report the exact commit green and production-verified through GitHub status
   context `ci success`.

## Stage and publish a stable release

The checked-in legacy `release.yml` has a broad tag trigger that independently
builds and publishes crates, npm, and GitHub assets. Disable that workflow and
verify it has no queued or running job before creating or pushing any stable
tag. Never push a tag while the broad publisher is active: irreversible
registry bytes could race the reviewed Cloudflare plan. The exact-byte bridge
below independently rechecks this precondition.

Create and push one annotated tag matching the workspace version:

```sh
git tag -a vx.y.z -m "Nanocodex x.y.z"
git push origin vx.y.z
```

Use a clean, detached checkout of that exact tag for the release controller.
The controller independently compares the local tag object, remote tag object,
resolved commit, workspace versions, and retained green master evidence. It
holds a repository-scoped kernel lock throughout each command.

Configure the non-registry release authority without putting values in argv or
the repository, then stage:

```sh
cd web
export NANOCODEX_CI_ORIGIN=https://nanocodex.me-7fb.workers.dev
export NANOCODEX_RELEASE_ORIGIN=https://nanocodex.me-7fb.workers.dev
export CI_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-ci-release -w)"
npm run ci:release-controller -- stage vx.y.z
```

`stage` performs no registry or public-channel mutation. It packages all eight
crates in a fresh isolated Cargo home, checks any already-published crates.io
checksums, starts or reuses the exact stable distribution Workflow, validates
all eight release assets, and downloads the exact tested npm tarball. The
command prints the path of a private JSON report, normally under
`.git/nanocodex-release-reports/`.

Review that report and `SHA256SUMS`. To smoke a staged binary before
publication, download its reported R2 key with an operator-scoped Wrangler/R2
credential and verify the reported SHA-256. Do not publish if the tag, draft,
or expected rolling-channel predecessor changes.

After review, load registry tokens into the trusted operator process and run:

```sh
export CARGO_REGISTRY_TOKEN="$(security find-generic-password -s nanocodex-crates-io -w)"
export NPM_TOKEN="$(security find-generic-password -s nanocodex-npm -w)"
npm run ci:release-controller -- publish vx.y.z
unset CARGO_REGISTRY_TOKEN NPM_TOKEN
```

`publish` redoes the local package proof, reconciles every existing crates.io
checksum before the first upload, publishes only missing crates in dependency
order, verifies registry API checksums and downloaded crate bytes, publishes
the exact green-CI npm tarball, and verifies npm integrity. Only then does it
finalize the immutable stable manifest and advance `latest`. A failed registry
or verification step never advances the public channel.

Registry preflight can verify the account identity and package ownership, but
the registries do not expose enough information to prove a token's exact
package scope, automation/2FA eligibility, or remaining authorization. Obtain
that proof out of band before the first irreversible publication and retain the
exact-byte post-publication verification as the authoritative check.

Smoke these public surfaces after publication:

```text
GET /api/releases/channels/latest
GET /api/releases/releases/stable/vx.y.z
GET /api/releases/releases/stable/vx.y.z/assets/<name>
```

Then run the public `install` script and `nanocodex update` on each supported
platform.

The first stable release whose updater knows the Cloudflare release API has one
additional, temporary migration duty. Nanocodex v0.5.0 and older discover
stable updates through GitHub Releases, so mirror that first release there as a
matching non-prerelease with the exact R2-tested binary bytes and checksums.
Run the bridge only after Cloudflare publication and only while the broad
workflow remains disabled and idle:

```sh
export NANOCODEX_GITHUB_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-github-release -w)"
export CI_RELEASE_TOKEN="$(security find-generic-password -s nanocodex-ci-release -w)"
npm run ci:mirror-github-release -- --dry-run stable vx.y.z
npm run ci:mirror-github-release -- stable vx.y.z
unset NANOCODEX_GITHUB_RELEASE_TOKEN CI_RELEASE_TOKEN
```

Before this bridge window, install an active GitHub ruleset for `v*` tags that
denies the compatibility credential permission to create, update, or delete a
matching ref, and do not use an administrator bypass. The GitHub create-release
API has no "tag must already exist" precondition: `target_commitish` only pins
where GitHub would create a missing tag. The bridge therefore supplies only the
full peeled commit, pins the exact raw annotated-tag object and its direct
commit target, and rechecks that identity before and after release creation,
publication, `make_latest`, and final byte verification. The ruleset is still
required to close deletion/recreation races that can occur entirely between
two GitHub reads. The compatibility bridge has no stable-tag ref mutation
endpoint of its own.

Keep the compatibility credential and every administrator or ruleset-bypass
actor exclusive for the entire bridge invocation. The Worker publication lease
serializes bridge invocations, and the bridge repeatedly proves that
`release.yml` is disabled and idle, but neither mechanism fences a separate
GitHub release writer. Immediately before and after `make_latest`, the bridge
enumerates the complete published canonical `vMAJOR.MINOR.PATCH` release set
and requires the target to be its highest semantic version; it also rechecks
the final authenticated and public `latest` views. GitHub exposes no conditional
release update or `make_latest` compare-and-swap, so these checks are detection,
not a CAS guarantee.

The bridge token needs `Actions: read` and `Contents: write`; rolling-nightly
policy inspection additionally needs `Administration: read`. It never needs
`Workflows: write`. Authenticated API reads prove the exact release and asset
metadata set, while unauthenticated downloads with an asset-ID cache buster
prove the exact bytes served to old updater clients.

Verify an installed v0.5.0 can cross that bridge before removing the legacy
GitHub release workflow. Cloudflare remains the canonical manifest and asset
store; later releases do not need the compatibility mirror. Do not choose the
bridging version until the release scope in `PLAN.md` is complete.

## npm provenance limitation

The release controller uses a narrowly scoped npm token. npm does not recognize
Cloudflare Workflows or this self-hosted controller as an OIDC trusted
publisher, so registry-recognized npm provenance is unavailable. The release
includes `PROVENANCE.json` with Cloudflare builder and source metadata, but that
file is not an npm registry attestation. Keep this distinction visible in the
release report and do not invoke `npm publish --provenance` from this path. See
npm's current lists of [trusted publishers](https://docs.npmjs.com/trusted-publishers/)
and [provenance-capable hosted CI](https://docs.npmjs.com/generating-provenance-statements/)
before changing this policy.

## Recovery

For the same unpublished or current release, rerun `stage` after an
infrastructure failure and rerun `publish` after registry propagation recovers.
The controller verifies and skips matching crate/npm versions, resumes only
missing mutations, and revalidates the remote tag and reviewed draft before
finalization. Do not describe a superseded stable publication as replayable:
verify its immutable manifest directly, report that it is no longer `latest`,
and release a new patch instead of trying to move the channel backward.

Published registry versions and stable manifests are immutable. A checksum
mismatch stops the release; it is never papered over. Fix a bad public release
with a new patch version, and yank only when continuing to resolve the old
version would harm users.
