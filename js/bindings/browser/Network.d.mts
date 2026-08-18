export type Bytes = Uint8Array | ArrayBuffer | ArrayBufferView;
export type Secret = Bytes | string;
export type Credential = Secret;

export interface SessionAuthorization {
  readonly requesterId: string;
  readonly providerId: string;
  readonly protocol: string;
  readonly credential: Uint8Array;
}

export type Authorizer = (
  request: SessionAuthorization,
) => boolean | Promise<boolean>;

export type Attestor = (
  request: SessionAuthorization,
) => Credential | null | undefined | Promise<Credential | null | undefined>;

export type CapabilityValue = string | boolean | number | readonly string[];

export interface Advertisement {
  revision: number;
  services?: readonly string[] | undefined;
  attributes?: Readonly<Record<string, CapabilityValue>> | undefined;
  leaseMillis?: number | undefined;
}

export interface Query {
  services?: readonly string[] | undefined;
  equals?: Readonly<Record<string, CapabilityValue>> | undefined;
  minimums?: Readonly<Record<string, number>> | undefined;
  contains?: Readonly<Record<string, string>> | undefined;
}

export type SignedCapability =
  | { type: "string"; value: string }
  | { type: "unsigned"; value: number }
  | { type: "boolean"; value: boolean }
  | { type: "string_set"; value: readonly string[] };

export interface SignedAdvertisement {
  version: number;
  node_id: string;
  issued_at_unix_millis: number;
  expires_at_unix_millis: number;
  advertisement: {
    revision: number;
    services: readonly string[];
    attributes: Readonly<Record<string, SignedCapability>>;
  };
  signature: string;
}

export interface PeerChange {
  type: "joined" | "updated" | "expired" | "unmatched";
  record: SignedAdvertisement;
}

export interface Hub {
  readonly endpointId: string;
  readonly ticket: string;
  exportAuthority(): Uint8Array;
  watch(query?: Query): Promise<Watcher>;
  snapshot(query?: Query): Promise<readonly SignedAdvertisement[]>;
  ingestAdvertisement(record: SignedAdvertisement): Promise<"applied" | "replay" | "stale">;
  shutdown(): Promise<void>;
}

export interface Node {
  readonly endpointId: string;
  exportIdentity(): Uint8Array;
  advertise(options: Advertisement): Promise<AdvertisementLease>;
  watch(query?: Query): Promise<Watcher>;
  snapshot(query?: Query): Promise<readonly SignedAdvertisement[]>;
  ingestAdvertisement(record: SignedAdvertisement): Promise<"applied" | "replay" | "stale">;
  listen(protocol: string): Promise<Listener>;
  connect(
    peerId: string,
    protocol: string,
    credentials?: { authority?: Credential | undefined; peer?: Credential | undefined },
  ): Promise<PeerStream>;
  shutdown(): Promise<void>;
}

export interface AdvertisementLease extends AsyncIterable<SignedAdvertisement> {
  latest(): SignedAdvertisement;
  next(): Promise<SignedAdvertisement | undefined>;
  close(): void;
}

export interface Watcher extends AsyncIterable<PeerChange> {
  next(): Promise<PeerChange | undefined>;
  close(): void;
}

export interface Listener {
  accept(): Promise<PeerStream | undefined>;
  close(): void;
}

export interface PeerStream {
  readonly peerId: string;
  readonly protocol: string;
  write(bytes: Bytes): Promise<void>;
  read(maxBytes?: number): Promise<Uint8Array>;
  finish(): Promise<void>;
  close(): void;
}

/** Downloads and compiles the browser runtime without opening a network endpoint. */
export function prewarm(options?: { module?: unknown }): Promise<void>;

/** Starts a relay-backed authority, rendezvous, and gossip bootstrap node. */
export function host(options?: host.Options): Promise<host.ReturnType>;
export declare namespace host {
  interface Options {
    /** A trusted HTTPS/WSS Iroh relay URL. Omit to use Iroh's public relay network. */
    relayUrl?: string | undefined;
    /** Previously exported 64-byte authority secret. Generated when omitted. */
    authority?: Secret | undefined;
    authorizeSession?: Authorizer | undefined;
    module?: unknown;
  }
  type ReturnType = Hub;
}

/** Joins a relay-backed browser node using an authority-issued admission ticket. */
export function join(options: join.Options): Promise<join.ReturnType>;
export declare namespace join {
  interface Options {
    ticket: string;
    /** A trusted HTTPS/WSS Iroh relay URL. Omit to use Iroh's public relay network. */
    relayUrl?: string | undefined;
    /** Previously exported 32-byte node identity. Generated when omitted. */
    identity?: Secret | undefined;
    authorizeIncoming?: Authorizer | undefined;
    verifyPeer?: Authorizer | undefined;
    attest?: Attestor | undefined;
    peerAttestation?: Credential | undefined;
    module?: unknown;
  }
  type ReturnType = Node;
}
