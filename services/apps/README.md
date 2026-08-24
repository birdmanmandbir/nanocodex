# Nanocodex dynamic apps

This service turns an authenticated request such as “build me a private notes
app” into a versioned Cloudflare Dynamic Worker. The control console lives at
`/apps` on the normal Nanocodex website; it has no separate password or account
system.

## Trust and tenancy

The website and managed Worker authenticate the existing persistent passkey
account. The managed Worker passes the verified user ID to the private
`AppPlatform` service binding and removes authorization and caller-supplied
identity headers. API keys are intentionally not accepted for the browser app
console.

Every registry is sharded by an opaque tenant ID. Personal tenants use
`user:<uuid>`. The registry also accepts `team:<id>`, but team selection is not
enabled until Nanocodex has an authoritative membership service. App metadata,
build jobs, revisions, state, Git repositories, launch tickets, and mounted
agents are resolved inside that tenant boundary.

Launching an app mints a signed, one-use, 60-second ticket. The public runtime
redeems it over the private `RuntimePlatform` binding and sets a host-only,
HTTP-only cookie scoped to exactly one tenant and app. The account cookie and
provider credentials never reach generated code.

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
have no public `/git/app-*` route. A revision records both its SHA-256 executable
artifact ID and its SHA-1 source commit. Updates append commits. Rollback only
moves the active revision pointer; it never rewrites Git history. One build per
app may be active at a time.

## Generated-app bindings

Dynamic Workers receive only `NANOCODEX`; ambient outbound networking is
disabled. The binding exposes app-scoped state and text generation methods, and
also implements `fetch()` for durable Nanocodex agents:

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
least 32 UTF-8 bytes.

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
