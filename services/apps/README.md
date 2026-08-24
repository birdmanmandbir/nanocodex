# Nanocodex dynamic apps

This service turns an authenticated request such as “build me a private notes
app” into a versioned Cloudflare Dynamic Worker. The control console lives at
`/apps` on the normal Nanocodex website; it has no separate password or account
system.

## Trust and tenancy

The website and managed Worker authenticate the existing persistent passkey
account. The managed Worker derives a typed personal or team access record and
passes it to the private `AppPlatform` service binding after removing
authorization and caller-supplied identity headers. API keys are intentionally
not accepted for the browser app console.

Every registry is sharded by an opaque tenant ID. Personal tenants use
`user:<uuid>` and teams use `team:<opaque Durable Object ID>`. Only namespace-
issued team IDs are accepted, so forged selectors cannot allocate Organization
objects. Organization membership is authoritative; UserAccount team references
are discovery hints that are revalidated before use. Members may read and run
team apps, while owners may also build, update, activate, and roll back them.
App metadata, build jobs, revisions, state, Git repositories, and launch tickets
are resolved inside that tenant boundary. Shared app state uses the team tenant,
while mounted agents and provider-backed actions remain owned by the acting
member.

Launching an app first creates a signed account intent. The runtime establishes
a high-entropy, HTTP-only browser transaction and returns to the authenticated
account before the control plane mints a signed, one-use, 60-second ticket. A
copied ticket cannot be redeemed from a different browser. The runtime session
cookie is path-scoped to the app's immutable ID, so multiple private apps can
remain open without sharing ambient authority.

Team authority is checked when the launch completes, when its ticket is
redeemed, and on every app invocation. Removing a member therefore revokes an
already-open team app on its next request; membership-service failures fail
closed without adding that dependency to personal app invocations.

The authenticated runtime page is host-owned. It places generated UI in a
sandboxed iframe without `allow-same-origin`, using a separate signed URL bound
to one actor, tenant, app, and expiry. The host overwrites generated security
headers and CSP: service workers and external browser connections are denied,
while API calls back to the same app frame are permitted. Account cookies,
runtime cookies, API keys, and provider credentials never reach generated code.
Generated source is nevertheless builder-approved code: it can deliberately
navigate its own sandboxed frame and encode app-visible data in that navigation.
The boundary isolates Nanocodex credentials, the host page, and other apps; it
does not claim to make source approved by the builder non-exfiltrating.

## Build and version-control pipeline

Each Cloudflare Workflow run:

1. loads the active project for an update;
2. asks Workers AI for a bounded React/Worker project;
3. bundles it with Cloudflare's Worker bundler;
4. stores the content-addressed artifact in R2;
5. commits the complete source tree to `app-<app uuid>` through the website's
   private `AppGitService`; and
6. atomically publishes and activates the immutable revision.

The Git repository uses the existing Smart HTTP, Durable Object, pack, and R2
implementation, but app repositories occupy a separate private namespace and
have no public `/git/app-*` route. Before moving a ref, the receiver validates
the checksums and identities of every retained Git object, requires a direct
fast-forward commit, and proves its complete reachable tree contains only
supported source objects. After each push, the builder fetches into a fresh
filesystem, proves the advertised commit is the expected direct fast-forward,
and compares every tracked file to the generated project before publication. A
revision records both its SHA-256 executable
artifact ID and its SHA-1 source commit. Updates append commits. Rollback only
moves the active revision pointer; it never rewrites Git history. One build per
app may be active at a time.

## Generated-app bindings

Builders explicitly approve a fixed capability manifest before creating an
app. Dynamic Workers receive only `NANOCODEX`; ambient outbound networking is
disabled. The binding exposes profile read, app-scoped state, text generation,
and durable Nanocodex agents:

```js
const created = await env.NANOCODEX.fetch("https://agents.internal/v1/agents", {
  method: "POST",
});
const { agent_id } = await created.json();

await env.NANOCODEX.fetch(`https://agents.internal/v1/agents/${agent_id}/turns`, {
  method: "POST",
  headers: { "content-type": "application/json", "idempotency-key": "turn-1" },
  body: JSON.stringify({ id: "turn-1", input: "Summarize my project" }),
});

const status = await env.NANOCODEX.fetch(
  `https://agents.internal/v1/agents/${agent_id}/turns/turn-1`,
);
```

The returned agent ID is an app-local opaque handle. Trusted host code maps it
to a managed agent owned by the actor's Nanocodex account. Generated apps must
not send an `Authorization` or `Cookie` header to this binding.

## Development and deployment

```sh
npm ci
npm run check
```

The control Worker requires `LAUNCH_TICKET_SECRET`; the runtime Worker requires
the independently generated `RUNTIME_SESSION_SECRET`. Each must contain at
least 32 UTF-8 bytes. `RUNTIME_ORIGIN` on control and `MANAGED_ORIGIN` on runtime
are fixed HTTPS origins, never caller-provided redirect targets.

```sh
openssl rand -base64 48 | npx wrangler secret put LAUNCH_TICKET_SECRET
openssl rand -base64 48 | npx wrangler secret put RUNTIME_SESSION_SECRET \
  --config wrangler.runtime.jsonc
npm run deploy
```

The final deployment has these private named bindings:

- managed Worker → `AppPlatform`, with client ID `nanocodex-managed`;
- app control → `ManagedAgentEntrypoint` and `AppGitService`, with client ID
  `nanocodex-apps`; and
- public app runtime → `RuntimePlatform`, with client ID
  `nanocodex-app-runtime`.

On the first rollout, deploy the named entrypoint providers before their
consumers. The app control Worker itself has `workers_dev` disabled and is only
reachable through the authenticated managed gateway.
