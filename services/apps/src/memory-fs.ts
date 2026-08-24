import type { PromiseFsClient } from "isomorphic-git";

type Entry =
  | Readonly<{ kind: "directory"; modifiedAt: number }>
  | Readonly<{ kind: "file"; bytes: Uint8Array; modifiedAt: number }>;

type StatLike = Readonly<{
  size: number;
  mode: number;
  mtimeMs: number;
  ctimeMs: number;
  isFile(): boolean;
  isDirectory(): boolean;
  isSymbolicLink(): boolean;
}>;

export type MemoryGitFs = PromiseFsClient & Readonly<{
  promises: {
    readFile(path: string, options?: string | { encoding?: string }): Promise<Uint8Array | string>;
    writeFile(path: string, value: string | Uint8Array | ArrayBuffer): Promise<void>;
    unlink(path: string): Promise<void>;
    readdir(path: string): Promise<string[]>;
    mkdir(path: string, options?: unknown): Promise<void>;
    rmdir(path: string): Promise<void>;
    stat(path: string): Promise<StatLike>;
    lstat(path: string): Promise<StatLike>;
    readlink(): Promise<never>;
    symlink(): Promise<never>;
    chmod(): Promise<void>;
  };
}>;

/** A bounded, invocation-local filesystem for isomorphic-git. */
export function createMemoryGitFs(): MemoryGitFs {
  const entries = new Map<string, Entry>([["/", { kind: "directory", modifiedAt: Date.now() }]]);

  const promises = {
    async readFile(path: string, options?: string | { encoding?: string }): Promise<Uint8Array | string> {
      const entry = requireEntry(entries, path);
      if (entry.kind !== "file") throw fsError("EISDIR", `cannot read directory ${path}`);
      const bytes = entry.bytes.slice();
      const encoding = typeof options === "string" ? options : options?.encoding;
      return encoding ? new TextDecoder(encoding).decode(bytes) : bytes;
    },

    async writeFile(path: string, value: string | Uint8Array | ArrayBuffer): Promise<void> {
      const normalized = normalize(path);
      requireDirectory(entries, parent(normalized));
      entries.set(normalized, {
        kind: "file",
        bytes: toBytes(value),
        modifiedAt: Date.now(),
      });
    },

    async unlink(path: string): Promise<void> {
      const normalized = normalize(path);
      const entry = requireEntry(entries, normalized);
      if (entry.kind !== "file") throw fsError("EISDIR", `cannot unlink directory ${path}`);
      entries.delete(normalized);
    },

    async readdir(path: string): Promise<string[]> {
      const normalized = normalize(path);
      requireDirectory(entries, normalized);
      const prefix = normalized === "/" ? "/" : `${normalized}/`;
      const names = new Set<string>();
      for (const key of entries.keys()) {
        if (!key.startsWith(prefix) || key === normalized) continue;
        const relative = key.slice(prefix.length);
        if (relative && !relative.includes("/")) names.add(relative);
      }
      return [...names].sort();
    },

    async mkdir(path: string, _options?: unknown): Promise<void> {
      const normalized = normalize(path);
      let current = "";
      for (const segment of normalized.split("/").filter(Boolean)) {
        current += `/${segment}`;
        const existing = entries.get(current);
        if (existing?.kind === "file") throw fsError("ENOTDIR", `${current} is a file`);
        if (!existing) entries.set(current, { kind: "directory", modifiedAt: Date.now() });
      }
    },

    async rmdir(path: string): Promise<void> {
      const normalized = normalize(path);
      requireDirectory(entries, normalized);
      const prefix = `${normalized}/`;
      if ([...entries.keys()].some((key) => key.startsWith(prefix))) {
        throw fsError("ENOTEMPTY", `directory is not empty: ${path}`);
      }
      if (normalized === "/") throw fsError("EPERM", "cannot remove filesystem root");
      entries.delete(normalized);
    },

    async stat(path: string): Promise<StatLike> {
      return statFor(requireEntry(entries, path));
    },

    async lstat(path: string): Promise<StatLike> {
      return statFor(requireEntry(entries, path));
    },

    async readlink(): Promise<never> {
      throw fsError("EINVAL", "symbolic links are unsupported");
    },

    async symlink(): Promise<never> {
      throw fsError("EPERM", "symbolic links are unsupported");
    },

    async chmod(): Promise<void> {
      // Generated app repositories contain regular non-executable source files.
    },
  };

  return { promises };
}

function normalize(path: string): string {
  if (typeof path !== "string" || path.includes("\0")) throw fsError("EINVAL", "invalid path");
  const segments: string[] = [];
  for (const segment of path.replaceAll("\\", "/").split("/")) {
    if (!segment || segment === ".") continue;
    if (segment === "..") {
      if (segments.length === 0) throw fsError("EPERM", "path escapes filesystem root");
      segments.pop();
    } else {
      segments.push(segment);
    }
  }
  return `/${segments.join("/")}`;
}

function parent(path: string): string {
  const separator = path.lastIndexOf("/");
  return separator <= 0 ? "/" : path.slice(0, separator);
}

function requireEntry(entries: ReadonlyMap<string, Entry>, path: string): Entry {
  const normalized = normalize(path);
  const entry = entries.get(normalized);
  if (!entry) throw fsError("ENOENT", `path does not exist: ${normalized}`);
  return entry;
}

function requireDirectory(entries: ReadonlyMap<string, Entry>, path: string): void {
  const entry = requireEntry(entries, path);
  if (entry.kind !== "directory") throw fsError("ENOTDIR", `${path} is not a directory`);
}

function statFor(entry: Entry): StatLike {
  const directory = entry.kind === "directory";
  return {
    size: directory ? 0 : entry.bytes.byteLength,
    mode: directory ? 0o040755 : 0o100644,
    mtimeMs: entry.modifiedAt,
    ctimeMs: entry.modifiedAt,
    isFile: () => !directory,
    isDirectory: () => directory,
    isSymbolicLink: () => false,
  };
}

function toBytes(value: string | Uint8Array | ArrayBuffer): Uint8Array {
  if (typeof value === "string") return new TextEncoder().encode(value);
  if (value instanceof Uint8Array) return value.slice();
  if (value instanceof ArrayBuffer) return new Uint8Array(value.slice(0));
  throw fsError("EINVAL", "file contents must be bytes or text");
}

function fsError(code: string, message: string): Error & { code: string } {
  return Object.assign(new Error(message), { code });
}
