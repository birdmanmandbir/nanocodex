import type { CreateWorkerOptions, CreateWorkerResult } from "@cloudflare/worker-bundler";

export const APP_COMPATIBILITY_DATE = "2026-08-24";
export const APP_POLICY_VERSION = 1;
export const APP_ARTIFACT_SCHEMA_VERSION = 1;
export const APP_GENERATION_MODEL = "@cf/zai-org/glm-5.2";

const MAX_FILES = 24;
const MAX_FILE_BYTES = 48 * 1024;
const MAX_PROJECT_BYTES = 96 * 1024;
const ALLOWED_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx", ".json", ".css", ".txt"]);
const SOURCE_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx"]);
const PROJECT_KEYS = ["entryPoint", "files", "name", "slug"] as const;
const FILE_KEYS = ["content", "path"] as const;
const ARTIFACT_KEYS = [
  "compatibilityDate",
  "mainModule",
  "modules",
  "policyVersion",
  "project",
  "revision",
  "schemaVersion",
] as const;

export type ProjectFile = Readonly<{ path: string; content: string }>;

export type GeneratedProject = Readonly<{
  name: string;
  slug: string;
  entryPoint: string;
  files: readonly ProjectFile[];
}>;

export type TextWorkerModule =
  | Readonly<{ js: string }>
  | Readonly<{ cjs: string }>
  | Readonly<{ text: string }>;

export type BuildArtifact = Readonly<{
  schemaVersion: 1;
  policyVersion: 1;
  compatibilityDate: "2026-08-24";
  project: GeneratedProject;
  mainModule: string;
  modules: Readonly<Record<string, TextWorkerModule>>;
  revision: string;
}>;

export type WorkerBundler = (options: CreateWorkerOptions) => Promise<CreateWorkerResult>;

type AiRunner = Readonly<{
  run(model: string, input: Record<string, unknown>): Promise<unknown>;
}>;

export const PROJECT_SYSTEM_PROMPT = `You generate one small Cloudflare Worker project.
Return only strict JSON with exactly this shape:
{"name":"Human name","slug":"lowercase-slug","entryPoint":"src/index.ts","files":[{"path":"src/index.ts","content":"..."}]}

Rules:
- Use at most 24 UTF-8 text files and only .ts, .tsx, .js, .jsx, .json, .css, or .txt files.
- Every path is a normalized relative path. The entry point is an exact file under src/.
- Imports may be relative local imports or cloudflare:* imports only. Do not use npm, bare, node:, URL, data:, package, or Wrangler configuration imports/files.
- Export a default Worker fetch handler from the entry point.
- There is no global fetch. All host functionality is available only through env.NANOCODEX:
  context(): Promise<object>
  get(key: string): Promise<unknown | null>
  put(key: string, value: unknown): Promise<void>
  delete(key: string): Promise<boolean>
  list(prefix?: string): Promise<Array<{key:string,value:unknown}>>
  counter(): Promise<number>
  incrementCounter(): Promise<number>
  generateText(prompt: string): Promise<string>
- For a durable Nanocodex coding agent, use the same REST shapes as the external API through the binding's native fetch method:
  await env.NANOCODEX.fetch("https://agents.internal/v1/agents", {method:"POST"})
  await env.NANOCODEX.fetch("https://agents.internal/v1/agents/<agent-id>/turns", {method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({id:"stable-turn-id",input:"..."})})
  await env.NANOCODEX.fetch("https://agents.internal/v1/agents/<agent-id>/turns/<turn-id>")
- Agent IDs returned by this binding are private app-scoped handles. Poll the turn resource to a terminal state. The host supplies user authorization and credentials; never add Authorization headers.
- Never reference credentials, account identity, hidden bindings, or APIs not listed above.
- Serve a polished, responsive, accessible browser interface from GET /. Browser links and fetch URLs must be relative (for example "api/items", never "/api/items") because the host mounts the app below a private path prefix.
- Mobile layouts must have zero horizontal overflow. Editable controls must use a computed font size of at least 16px, and every interactive touch target must be at least 44 by 44 CSS pixels.
- Do not render spinners, skeletons, transient loading copy, or blank placeholders. Preserve complete content and show actionable errors only after failures.
- Encode all source code as JSON strings. Do not emit Markdown fences, commentary, or keys outside the schema.`;

export class ProjectValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProjectValidationError";
  }
}

export class ProjectBuildError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProjectBuildError";
  }
}

export function validateProject(input: unknown): GeneratedProject {
  const value = typeof input === "string" ? parseJson(input, "project") : input;
  assertRecord(value, "project");
  assertExactKeys(value, PROJECT_KEYS, "project");

  const name = requiredString(value.name, "project.name");
  if (name.length > 120) throw new ProjectValidationError("project.name exceeds 120 characters");
  const slug = requiredString(value.slug, "project.slug");
  if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(slug) || slug.length > 63) {
    throw new ProjectValidationError("project.slug must be a lowercase DNS-style slug up to 63 characters");
  }
  const entryPoint = validatePath(requiredString(value.entryPoint, "project.entryPoint"), "entry point");
  if (!entryPoint.startsWith("src/") || !SOURCE_EXTENSIONS.has(extension(entryPoint))) {
    throw new ProjectValidationError("project.entryPoint must be a JavaScript or TypeScript file under src/");
  }
  if (!Array.isArray(value.files) || value.files.length === 0 || value.files.length > MAX_FILES) {
    throw new ProjectValidationError(`project.files must contain between 1 and ${MAX_FILES} files`);
  }

  const paths = new Set<string>();
  const files: ProjectFile[] = [];
  let totalBytes = 0;
  for (let index = 0; index < value.files.length; index += 1) {
    const file = value.files[index];
    assertRecord(file, `project.files[${index}]`);
    assertExactKeys(file, FILE_KEYS, `project.files[${index}]`);
    const path = validatePath(requiredString(file.path, `project.files[${index}].path`), "file path");
    if (paths.has(path)) throw new ProjectValidationError(`duplicate file path: ${path}`);
    paths.add(path);
    const content = stringValue(file.content, `project.files[${index}].content`);
    assertWellFormedUnicode(content, `content for ${path}`);
    const bytes = new TextEncoder().encode(content).byteLength;
    if (bytes > MAX_FILE_BYTES) {
      throw new ProjectValidationError(`${path} exceeds ${MAX_FILE_BYTES} UTF-8 bytes`);
    }
    totalBytes += bytes;
    if (totalBytes > MAX_PROJECT_BYTES) {
      throw new ProjectValidationError(`project exceeds ${MAX_PROJECT_BYTES} UTF-8 bytes`);
    }
    files.push({ path, content });
  }
  if (!paths.has(entryPoint)) {
    throw new ProjectValidationError("project.entryPoint must exactly match a project file path");
  }

  const normalizedFiles = files.toSorted((left, right) => compareText(left.path, right.path));
  for (const file of normalizedFiles) validateImports(file, paths);
  return { name, slug, entryPoint, files: normalizedFiles };
}

export async function buildProject(
  input: unknown,
  bundler: WorkerBundler = defaultBundler,
): Promise<BuildArtifact> {
  const project = validateProject(input);
  const files = Object.fromEntries(project.files.map((file) => [file.path, file.content]));
  let result: CreateWorkerResult;
  try {
    result = await bundler({
      files,
      entryPoint: project.entryPoint,
      bundle: true,
      target: "es2022",
      minify: true,
      sourcemap: false,
      conditions: ["workerd", "worker", "browser"],
    });
  } catch (error) {
    throw new ProjectBuildError(`project bundle failed: ${errorMessage(error)}`);
  }
  if (result.warnings && result.warnings.length > 0) {
    throw new ProjectBuildError(`project bundle emitted warnings: ${result.warnings.join("; ")}`);
  }

  const mainModule = validatePath(requiredString(result.mainModule, "bundle mainModule"), "bundle module path");
  const modules = normalizeModules(result.modules);
  if (!(mainModule in modules)) throw new ProjectBuildError("bundle mainModule is missing from modules");
  const unsigned = {
    schemaVersion: APP_ARTIFACT_SCHEMA_VERSION,
    policyVersion: APP_POLICY_VERSION,
    compatibilityDate: APP_COMPATIBILITY_DATE,
    project,
    mainModule,
    modules,
  } as const;
  const revision = await sha256(canonicalJson(unsigned));
  return { ...unsigned, revision };
}

export function serializeArtifact(artifact: BuildArtifact): string {
  return canonicalJson(artifact);
}

export async function parseArtifact(serialized: string): Promise<BuildArtifact> {
  const value = parseJson(serialized, "artifact");
  assertRecord(value, "artifact");
  assertExactKeys(value, ARTIFACT_KEYS, "artifact");
  if (value.schemaVersion !== APP_ARTIFACT_SCHEMA_VERSION) {
    throw new ProjectValidationError("unsupported artifact schemaVersion");
  }
  if (value.policyVersion !== APP_POLICY_VERSION) {
    throw new ProjectValidationError("unsupported artifact policyVersion");
  }
  if (value.compatibilityDate !== APP_COMPATIBILITY_DATE) {
    throw new ProjectValidationError("unsupported artifact compatibilityDate");
  }
  const project = validateProject(value.project);
  const mainModule = validatePath(requiredString(value.mainModule, "artifact.mainModule"), "bundle module path");
  const modules = normalizeModules(value.modules);
  if (!(mainModule in modules)) throw new ProjectValidationError("artifact mainModule is missing from modules");
  const revision = requiredString(value.revision, "artifact.revision");
  if (!/^[0-9a-f]{64}$/.test(revision)) throw new ProjectValidationError("artifact.revision is not a SHA-256 digest");
  const unsigned = {
    schemaVersion: APP_ARTIFACT_SCHEMA_VERSION,
    policyVersion: APP_POLICY_VERSION,
    compatibilityDate: APP_COMPATIBILITY_DATE,
    project,
    mainModule,
    modules,
  } as const;
  const expected = await sha256(canonicalJson(unsigned));
  if (revision !== expected) throw new ProjectValidationError("artifact revision digest does not match its contents");
  return { ...unsigned, revision };
}

export async function generateProject(ai: unknown, prompt: string, base?: unknown): Promise<GeneratedProject> {
  if (typeof prompt !== "string" || prompt.trim().length === 0) {
    throw new ProjectValidationError("generation prompt must not be empty");
  }
  const runner = ai as Partial<AiRunner>;
  if (typeof runner?.run !== "function") throw new ProjectValidationError("AI binding does not provide run()");
  const baseProject = base === undefined ? undefined : validateProject(base);
  const userPrompt = baseProject === undefined
    ? prompt
    : `${prompt}\n\nRevise this existing project while returning the complete replacement project:\n${canonicalJson(baseProject)}`;
  const output = await runner.run(APP_GENERATION_MODEL, {
    max_completion_tokens: 20_000,
    messages: [
      { role: "system", content: PROJECT_SYSTEM_PROMPT },
      { role: "user", content: userPrompt },
    ],
    response_format: { type: "json_object" },
    temperature: 0.2,
  });
  return validateProject(extractGeneratedProject(output));
}

export function extractGeneratedProject(output: unknown): unknown {
  if (typeof output === "string") return output;
  if (looksLikeProject(output)) return output;

  const candidates: unknown[] = [];
  if (isRecord(output)) {
    candidates.push(output.response, output.output_text, output.content, output.message, output.result);
    if (Array.isArray(output.choices)) {
      for (const choice of output.choices) {
        if (isRecord(choice)) candidates.push(choice.message, choice.text, choice.content);
      }
    }
  }
  for (const candidate of candidates) {
    const extracted = textFromAiValue(candidate);
    if (extracted !== undefined) return extracted;
    if (looksLikeProject(candidate)) return candidate;
    if (isRecord(candidate)) {
      for (const nested of [candidate.response, candidate.output_text, candidate.content, candidate.text]) {
        const nestedText = textFromAiValue(nested);
        if (nestedText !== undefined) return nestedText;
        if (looksLikeProject(nested)) return nested;
      }
    }
  }
  throw new ProjectValidationError("AI response did not contain generated project JSON");
}

export function canonicalJson(value: unknown): string {
  if (value === null) return "null";
  if (typeof value === "string" || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new ProjectValidationError("canonical JSON cannot encode a non-finite number");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (isRecord(value)) {
    return `{${Object.keys(value).toSorted(compareText).map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
  }
  throw new ProjectValidationError(`canonical JSON cannot encode ${typeof value}`);
}

async function defaultBundler(options: CreateWorkerOptions): Promise<CreateWorkerResult> {
  const { createWorker } = await import("@cloudflare/worker-bundler");
  return createWorker(options);
}

function normalizeModules(value: unknown): Readonly<Record<string, TextWorkerModule>> {
  assertRecord(value, "bundle modules");
  const entries: Array<[string, TextWorkerModule]> = [];
  for (const rawName of Object.keys(value).toSorted(compareText)) {
    const name = validatePath(rawName, "bundle module path");
    const rawModule = value[rawName];
    if (typeof rawModule === "string") {
      assertWellFormedUnicode(rawModule, `bundle module ${name}`);
      entries.push([name, { js: rawModule }]);
      continue;
    }
    assertRecord(rawModule, `bundle module ${name}`);
    const keys = Object.keys(rawModule);
    if (keys.length !== 1 || !["js", "cjs", "text"].includes(keys[0]!)) {
      throw new ProjectBuildError(`bundle module ${name} is not a text-only WorkerLoader module`);
    }
    const kind = keys[0] as "js" | "cjs" | "text";
    const content = stringValue(rawModule[kind], `bundle module ${name}.${kind}`);
    assertWellFormedUnicode(content, `bundle module ${name}`);
    entries.push([name, { [kind]: content } as TextWorkerModule]);
  }
  if (entries.length === 0) throw new ProjectBuildError("bundle produced no modules");
  return Object.fromEntries(entries);
}

function validatePath(path: string, label: string): string {
  assertWellFormedUnicode(path, label);
  if (path !== path.normalize("NFC")) throw new ProjectValidationError(`${label} must use NFC Unicode normalization`);
  if (path.includes("\\") || path.startsWith("/") || /^[A-Za-z]:/.test(path)) {
    throw new ProjectValidationError(`${label} must be relative and use forward slashes`);
  }
  const parts = path.split("/");
  if (parts.length === 0 || parts.some((part) => part.length === 0 || part === "." || part === "..")) {
    throw new ProjectValidationError(`${label} is not normalized`);
  }
  if (parts.some((part) => part.toLowerCase() === "node_modules")) {
    throw new ProjectValidationError(`${label} may not contain node_modules`);
  }
  if (parts.some((part) => /[\u0000-\u001f\u007f]/.test(part))) {
    throw new ProjectValidationError(`${label} contains control characters`);
  }
  const basename = parts.at(-1)!.toLowerCase();
  if (basename === "package.json" || basename === "wrangler.toml" || basename === "wrangler.json" || basename === "wrangler.jsonc") {
    throw new ProjectValidationError(`${label} may not contain package or Wrangler configuration`);
  }
  if (!ALLOWED_EXTENSIONS.has(extension(path))) {
    throw new ProjectValidationError(`${label} has an unsupported extension`);
  }
  return path;
}

function validateImports(file: ProjectFile, projectPaths: ReadonlySet<string>): void {
  if (!SOURCE_EXTENSIONS.has(extension(file.path)) && extension(file.path) !== ".css") return;
  const source = stripComments(file.content);
  const specifiers: string[] = [];
  const patterns = [
    /\b(?:import|export)\s+(?:type\s+)?(?:[\s\S]*?\s+from\s*)?["']([^"'\n\r]+)["']/g,
    /\b(?:import|require)\s*\(\s*["']([^"'\n\r]+)["']\s*\)/g,
    /@import\s+(?:url\(\s*)?["']([^"'\n\r]+)["']/g,
  ];
  for (const pattern of patterns) {
    for (const match of source.matchAll(pattern)) specifiers.push(match[1]!);
  }
  const dynamicCalls = source.matchAll(/\b(?:import|require)\s*\(([^)]*)\)/g);
  for (const match of dynamicCalls) {
    if (!/^\s*["'][^"'\n\r]+["']\s*$/.test(match[1]!)) {
      throw new ProjectValidationError(`${file.path} contains a non-literal dynamic import`);
    }
  }
  for (const specifier of specifiers) {
    if (specifier.includes("\\")) throw new ProjectValidationError(`${file.path} contains an escaped import specifier`);
    if (/^cloudflare:[A-Za-z0-9_./-]+$/.test(specifier)) continue;
    if (!specifier.startsWith("./") && !specifier.startsWith("../")) {
      throw new ProjectValidationError(`${file.path} imports forbidden module ${specifier}`);
    }
    const resolved = resolveRelative(file.path, specifier);
    if (resolved === undefined) {
      throw new ProjectValidationError(`${file.path} imports outside the project: ${specifier}`);
    }
    if (!localImportExists(resolved, projectPaths)) {
      throw new ProjectValidationError(`${file.path} imports missing local module ${specifier}`);
    }
  }
}

function resolveRelative(importer: string, specifier: string): string | undefined {
  const parts = importer.split("/").slice(0, -1);
  for (const part of specifier.split("/")) {
    if (part === "." || part === "") continue;
    if (part === "..") {
      if (parts.length === 0) return undefined;
      parts.pop();
    } else {
      parts.push(part);
    }
  }
  return parts.join("/");
}

function localImportExists(resolved: string, paths: ReadonlySet<string>): boolean {
  if (paths.has(resolved)) return true;
  for (const suffix of ALLOWED_EXTENSIONS) if (paths.has(`${resolved}${suffix}`)) return true;
  for (const suffix of ALLOWED_EXTENSIONS) if (paths.has(`${resolved}/index${suffix}`)) return true;
  return false;
}

function stripComments(source: string): string {
  let result = "";
  let quote = "";
  for (let index = 0; index < source.length; index += 1) {
    const char = source[index]!;
    const next = source[index + 1];
    if (quote) {
      result += char;
      if (char === "\\") {
        result += next ?? "";
        index += 1;
      } else if (char === quote) {
        quote = "";
      }
      continue;
    }
    if (char === '"' || char === "'" || char === "`") {
      quote = char;
      result += char;
    } else if (char === "/" && next === "/") {
      while (index < source.length && source[index] !== "\n") index += 1;
      result += "\n";
    } else if (char === "/" && next === "*") {
      index += 2;
      while (index < source.length && !(source[index] === "*" && source[index + 1] === "/")) {
        result += source[index] === "\n" ? "\n" : " ";
        index += 1;
      }
      index += 1;
    } else {
      result += char;
    }
  }
  return result;
}

function textFromAiValue(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return undefined;
  const pieces: string[] = [];
  for (const part of value) {
    if (typeof part === "string") pieces.push(part);
    else if (isRecord(part) && typeof part.text === "string") pieces.push(part.text);
  }
  return pieces.length > 0 ? pieces.join("") : undefined;
}

function looksLikeProject(value: unknown): boolean {
  return isRecord(value) && "name" in value && "slug" in value && "entryPoint" in value && "files" in value;
}

function parseJson(value: string, label: string): unknown {
  try {
    return JSON.parse(value) as unknown;
  } catch (error) {
    throw new ProjectValidationError(`${label} is not valid JSON: ${errorMessage(error)}`);
  }
}

function assertRecord(value: unknown, label: string): asserts value is Record<string, unknown> {
  if (!isRecord(value)) throw new ProjectValidationError(`${label} must be an object`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function assertExactKeys(value: Record<string, unknown>, expected: readonly string[], label: string): void {
  const actual = Object.keys(value).toSorted(compareText);
  const wanted = [...expected].toSorted(compareText);
  if (actual.length !== wanted.length || actual.some((key, index) => key !== wanted[index])) {
    throw new ProjectValidationError(`${label} must contain exactly: ${wanted.join(", ")}`);
  }
}

function requiredString(value: unknown, label: string): string {
  const result = stringValue(value, label);
  if (result.length === 0) throw new ProjectValidationError(`${label} must not be empty`);
  assertWellFormedUnicode(result, label);
  return result;
}

function stringValue(value: unknown, label: string): string {
  if (typeof value !== "string") throw new ProjectValidationError(`${label} must be a string`);
  return value;
}

function assertWellFormedUnicode(value: string, label: string): void {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) throw new ProjectValidationError(`${label} is not well-formed Unicode`);
      index += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      throw new ProjectValidationError(`${label} is not well-formed Unicode`);
    }
  }
}

function extension(path: string): string {
  const index = path.lastIndexOf(".");
  return index < 0 ? "" : path.slice(index).toLowerCase();
}

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function sha256(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
