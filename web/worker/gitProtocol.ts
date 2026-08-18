import type {
  RepositoryPublication,
  RepositoryRef,
} from "./gitRepository.ts";

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const maxSidebandDataBytes = 65_515;

export type PacketLine =
  | { kind: "data"; data: Uint8Array }
  | { kind: "flush" }
  | { kind: "delimiter" };

export function encodePacketLine(value: string | Uint8Array): Uint8Array {
  const payload = typeof value === "string" ? encoder.encode(value) : value;
  const length = payload.byteLength + 4;
  if (length > 0xffff) throw new Error("packet line exceeds 65535 bytes");
  const prefix = encoder.encode(length.toString(16).padStart(4, "0"));
  const packet = new Uint8Array(length);
  packet.set(prefix);
  packet.set(payload, 4);
  return packet;
}

export function parsePacketLines(value: Uint8Array): PacketLine[] {
  const packets: PacketLine[] = [];
  let offset = 0;
  while (offset < value.byteLength) {
    if (offset + 4 > value.byteLength) throw new Error("truncated packet prefix");
    const rawLength = decoder.decode(value.subarray(offset, offset + 4));
    if (!/^[0-9a-f]{4}$/.test(rawLength)) throw new Error("invalid packet prefix");
    const length = Number.parseInt(rawLength, 16);
    offset += 4;
    if (length === 0) {
      packets.push({ kind: "flush" });
      continue;
    }
    if (length === 1) {
      packets.push({ kind: "delimiter" });
      continue;
    }
    if (length < 4 || offset + length - 4 > value.byteLength) {
      throw new Error("truncated packet payload");
    }
    packets.push({
      kind: "data",
      data: value.slice(offset, offset + length - 4),
    });
    offset += length - 4;
  }
  return packets;
}

export function repositoryAdvertisement(): Uint8Array {
  return concatenate([
    encodePacketLine("version 2\n"),
    encodePacketLine("agent=nanocodex-cloudflare/1\n"),
    encodePacketLine("ls-refs=unborn\n"),
    encodePacketLine("fetch\n"),
    encodePacketLine("object-format=sha1\n"),
    flushPacket,
  ]);
}

export function parseV2Command(body: Uint8Array): {
  command: string | null;
  arguments: string[];
} {
  const packets = parsePacketLines(body);
  let command: string | null = null;
  let inArguments = false;
  const arguments_: string[] = [];
  for (const packet of packets) {
    if (packet.kind === "delimiter") {
      inArguments = true;
      continue;
    }
    if (packet.kind !== "data") continue;
    const line = decoder.decode(packet.data).replace(/\n$/, "");
    if (!inArguments && line.startsWith("command=")) {
      command = line.slice("command=".length);
    } else if (inArguments) {
      arguments_.push(line);
    }
  }
  return { command, arguments: arguments_ };
}

export function buildLsRefsResponse(
  publication: RepositoryPublication,
  arguments_: readonly string[],
): Uint8Array {
  const prefixes = arguments_
    .filter((argument) => argument.startsWith("ref-prefix "))
    .map((argument) => argument.slice("ref-prefix ".length));
  const refs = publication.refs.filter(
    (ref) => prefixes.length === 0 || prefixes.some((prefix) => ref.name.startsWith(prefix)),
  );
  const headRef = `refs/heads/${publication.branch}`;
  const head = publication.refs.find((ref) => ref.name === headRef) ?? {
    name: headRef,
    oid: publication.head,
  };
  return concatenate([
    encodePacketLine(`${head.oid} HEAD symref-target:${headRef}\n`),
    ...refs.map(formatRefPacket),
    flushPacket,
  ]);
}

function formatRefPacket(ref: RepositoryRef): Uint8Array {
  return encodePacketLine(`${ref.oid} ${ref.name}\n`);
}

export function buildNegotiationResponse(): Uint8Array {
  return concatenate([
    encodePacketLine("acknowledgments\n"),
    encodePacketLine("NAK\n"),
    flushPacket,
  ]);
}

export function buildFullPackResponse(
  pack: ReadableStream<Uint8Array>,
): ReadableStream<Uint8Array> {
  const reader = pack.getReader();
  let chunk: Uint8Array<ArrayBufferLike> = new Uint8Array(0);
  let chunkOffset = 0;
  let finished = false;
  return new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encodePacketLine("packfile\n"));
    },
    async pull(controller) {
      if (finished) return;
      if (chunkOffset >= chunk.byteLength) {
        const next = await reader.read();
        if (next.done) {
          finished = true;
          controller.enqueue(flushPacket);
          controller.close();
          return;
        }
        chunk = next.value;
        chunkOffset = 0;
      }
      const end = Math.min(chunkOffset + maxSidebandDataBytes, chunk.byteLength);
      const payload = new Uint8Array(1 + end - chunkOffset);
      payload[0] = 1;
      payload.set(chunk.subarray(chunkOffset, end), 1);
      chunkOffset = end;
      controller.enqueue(encodePacketLine(payload));
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
}

export const flushPacket = encoder.encode("0000");

function concatenate(chunks: readonly Uint8Array[]): Uint8Array {
  const length = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}
