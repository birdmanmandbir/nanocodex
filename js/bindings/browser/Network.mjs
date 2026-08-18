const BRIDGE_STATE = Symbol.for("nanocodex.browser.network.host.v1");
const policyFinalizer = typeof FinalizationRegistry === "undefined"
  ? undefined
  : new FinalizationRegistry((releases) => {
      for (const release of releases) release();
    });
let initialized;
let runtime;

export function prewarm(options = {}) {
  return initialized ||= loadRuntime()
    .then(({ default: init }) => options.module === undefined
      ? init()
      : init({ module_or_path: options.module }))
    .then(() => undefined)
    .catch((error) => {
      initialized = undefined;
      throw error;
    });
}

/** Starts the authority, rendezvous, and gossip bootstrap node in this browser. */
export async function host(options = {}) {
  const callbacks = bridgeState();
  const [authorization] = registerPolicies([
    [callbacks.authorizers, options.authorizeSession],
  ]);
  try {
    await prewarm({ module: options.module });
    const { NetworkHub } = await loadRuntime();
    const raw = await NetworkHub.bind(JSON.stringify(compact({
      authority: encodeBytes(options.authority, "authority"),
      relayUrl: options.relayUrl,
      sessionAuthorizerId: authorization?.id,
    })));
    return new Hub(raw, authorization?.release);
  } catch (error) {
    authorization?.release();
    throw error;
  }
}

/** Joins a browser node to an existing Nanocodex network. */
export async function join(options) {
  if (!options || typeof options.ticket !== "string" || !options.ticket) {
    throw new TypeError("ticket must be a non-empty network join ticket");
  }
  if (options.attest !== undefined && options.peerAttestation !== undefined) {
    throw new TypeError("attest and peerAttestation cannot both be configured");
  }
  const callbacks = bridgeState();
  const [incoming, verifier, attestor] = registerPolicies([
    [callbacks.authorizers, options.authorizeIncoming],
    [callbacks.authorizers, options.verifyPeer],
    [callbacks.attestors, options.attest],
  ]);
  const releases = [incoming, verifier, attestor].filter(Boolean).map((entry) => entry.release);
  try {
    await prewarm({ module: options.module });
    const { NetworkNode } = await loadRuntime();
    const raw = await NetworkNode.join(JSON.stringify(compact({
      ticket: options.ticket,
      identity: encodeBytes(options.identity, "identity"),
      relayUrl: options.relayUrl,
      incomingAuthorizerId: incoming?.id,
      peerVerifierId: verifier?.id,
      peerAttestorId: attestor?.id,
      peerAttestation: encodeBytes(options.peerAttestation, "peerAttestation"),
    })));
    return new Node(raw, releases);
  } catch (error) {
    for (const release of releases) release();
    throw error;
  }
}

class Hub {
  #raw;
  #release;

  constructor(raw, release) {
    this.#raw = raw;
    this.#release = release;
    if (release) policyFinalizer?.register(this, [release], this);
  }

  get endpointId() {
    return this.#raw.endpointId;
  }

  get ticket() {
    return this.#raw.ticket;
  }

  /** Returns 64 secret bytes suitable for caller-owned encrypted persistence. */
  exportAuthority() {
    return decodeBytes(this.#hub().exportAuthority());
  }

  async watch(query = {}) {
    return new Watcher(await this.#hub().watch(JSON.stringify(query)));
  }

  async snapshot(query = {}) {
    return JSON.parse(await this.#hub().snapshot(JSON.stringify(query)));
  }

  async ingestAdvertisement(record) {
    return this.#hub().ingestAdvertisement(JSON.stringify(record));
  }

  async shutdown() {
    const raw = this.#take();
    try {
      await raw.shutdown();
    } finally {
      raw.free();
      policyFinalizer?.unregister(this);
      this.#release?.();
      this.#release = undefined;
    }
  }

  #take() {
    const raw = this.#hub();
    this.#raw = undefined;
    return raw;
  }

  #hub() {
    if (!this.#raw) throw new Error("network hub is shut down");
    return this.#raw;
  }
}

class Node {
  #raw;
  #releases;

  constructor(raw, releases) {
    this.#raw = raw;
    this.#releases = releases;
    if (releases.length > 0) policyFinalizer?.register(this, releases, this);
  }

  get endpointId() {
    return this.#node().endpointId;
  }

  /** Returns 32 secret bytes suitable for caller-owned encrypted persistence. */
  exportIdentity() {
    return decodeBytes(this.#node().exportIdentity());
  }

  async advertise(options) {
    const raw = await this.#node().advertise(JSON.stringify(options));
    return new AdvertisementLease(raw);
  }

  async watch(query = {}) {
    return new Watcher(await this.#node().watch(JSON.stringify(query)));
  }

  async snapshot(query = {}) {
    return JSON.parse(await this.#node().snapshot(JSON.stringify(query)));
  }

  async ingestAdvertisement(record) {
    return this.#node().ingestAdvertisement(JSON.stringify(record));
  }

  async listen(protocol) {
    return new Listener(await this.#node().listen(protocol));
  }

  async connect(peerId, protocol, credentials = {}) {
    const raw = await this.#node().connect(peerId, protocol, JSON.stringify(compact({
      authority: encodeBytes(credentials.authority, "credentials.authority"),
      peer: encodeBytes(credentials.peer, "credentials.peer"),
    })));
    return new PeerStream(raw);
  }

  async shutdown() {
    const raw = this.#take();
    try {
      await raw.shutdown();
    } finally {
      raw.free();
      policyFinalizer?.unregister(this);
      for (const release of this.#releases) release();
      this.#releases = [];
    }
  }

  #node() {
    if (!this.#raw) throw new Error("network node is shut down");
    return this.#raw;
  }

  #take() {
    const raw = this.#node();
    this.#raw = undefined;
    return raw;
  }
}

class AdvertisementLease {
  #raw;

  constructor(raw) {
    this.#raw = raw;
  }

  latest() {
    return JSON.parse(this.#lease().latest());
  }

  async next() {
    const record = await this.#lease().next();
    return record === undefined ? undefined : JSON.parse(record);
  }

  close() {
    this.#take().free();
  }

  [Symbol.asyncIterator]() {
    return {
      next: async () => {
        const value = await this.next();
        return value === undefined ? { done: true, value: undefined } : { done: false, value };
      },
    };
  }

  #lease() {
    if (!this.#raw) throw new Error("network advertisement lease is closed");
    return this.#raw;
  }

  #take() {
    const raw = this.#lease();
    this.#raw = undefined;
    return raw;
  }
}

class Watcher {
  #raw;

  constructor(raw) {
    this.#raw = raw;
  }

  async next() {
    const change = await this.#watcher().next();
    return change === undefined ? undefined : JSON.parse(change);
  }

  close() {
    this.#take().free();
  }

  [Symbol.asyncIterator]() {
    return {
      next: async () => {
        const value = await this.next();
        return value === undefined ? { done: true, value: undefined } : { done: false, value };
      },
    };
  }

  #watcher() {
    if (!this.#raw) throw new Error("network watcher is closed");
    return this.#raw;
  }

  #take() {
    const raw = this.#watcher();
    this.#raw = undefined;
    return raw;
  }
}

class Listener {
  #raw;

  constructor(raw) {
    this.#raw = raw;
  }

  async accept() {
    const stream = await this.#listener().accept();
    return stream === undefined ? undefined : new PeerStream(stream);
  }

  close() {
    this.#take().free();
  }

  #listener() {
    if (!this.#raw) throw new Error("network listener is closed");
    return this.#raw;
  }

  #take() {
    const raw = this.#listener();
    this.#raw = undefined;
    return raw;
  }
}

class PeerStream {
  #raw;

  constructor(raw) {
    this.#raw = raw;
  }

  get peerId() {
    return this.#stream().peerId;
  }

  get protocol() {
    return this.#stream().protocol;
  }

  write(bytes) {
    return this.#stream().write(asUint8Array(bytes, "bytes"));
  }

  read(maxBytes = 64 * 1024) {
    return this.#stream().read(maxBytes);
  }

  finish() {
    return this.#stream().finish();
  }

  close() {
    this.#take().free();
  }

  #stream() {
    if (!this.#raw) throw new Error("network peer stream is closed");
    return this.#raw;
  }

  #take() {
    const raw = this.#stream();
    this.#raw = undefined;
    return raw;
  }
}

function createBridgeState() {
  const authorizers = new Map();
  const attestors = new Map();
  let nextId = 1;
  const previous = globalThis.nanocodexNetworkHost;
  const bridge = {
    async authorize(id, encoded) {
      const authorize = authorizers.get(id);
      if (authorize) return (await authorize(parseAuthorization(encoded))) === true;
      return (await previous?.authorize?.(id, encoded)) === true;
    },
    async attest(id, encoded) {
      const attest = attestors.get(id);
      if (!attest) return (await previous?.attest?.(id, encoded)) ?? null;
      const credential = await attest(parseAuthorization(encoded));
      return credential == null ? null : encodeBytes(credential, "attestation");
    },
  };
  globalThis.nanocodexNetworkHost = bridge;
  return {
    authorizers,
    attestors,
    allocateId() {
      if (nextId > 0xffff_ffff) throw new RangeError("network callback registry is exhausted");
      return nextId++;
    },
  };
}

function register(registry, callback) {
  if (callback === undefined) return undefined;
  if (typeof callback !== "function") throw new TypeError("network policy must be a function");
  const id = bridgeState().allocateId();
  registry.set(id, callback);
  let active = true;
  return {
    id,
    release() {
      if (!active) return;
      active = false;
      registry.delete(id);
    },
  };
}

function registerPolicies(policies) {
  const registrations = [];
  try {
    for (const [registry, callback] of policies) {
      registrations.push(register(registry, callback));
    }
    return registrations;
  } catch (error) {
    for (const registration of registrations) registration?.release();
    throw error;
  }
}

function bridgeState() {
  return globalThis[BRIDGE_STATE] ||= createBridgeState();
}

function loadRuntime() {
  return runtime ||= import("../pkg-network/nanocodex_network.js");
}

function parseAuthorization(encoded) {
  const request = JSON.parse(encoded);
  return Object.freeze({
    requesterId: request.requesterId,
    providerId: request.providerId,
    protocol: request.protocol,
    credential: decodeBytes(request.credential),
  });
}

function compact(value) {
  return Object.fromEntries(Object.entries(value).filter(([, item]) => item !== undefined));
}

function encodeBytes(value, name) {
  if (value === undefined) return undefined;
  if (typeof value === "string") return value;
  const bytes = asUint8Array(value, name);
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function decodeBytes(value) {
  const encoded = value.replaceAll("-", "+").replaceAll("_", "/");
  const binary = atob(encoded.padEnd(Math.ceil(encoded.length / 4) * 4, "="));
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function asUint8Array(value, name) {
  if (value instanceof Uint8Array) return value;
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  throw new TypeError(`${name} must be a Uint8Array, ArrayBuffer, or base64url string`);
}
