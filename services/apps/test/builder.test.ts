import { describe, expect, it, vi } from "vitest";

import {
  APP_COMPATIBILITY_DATE,
  APP_GENERATION_MODEL,
  APP_POLICY_VERSION,
  PROJECT_SYSTEM_PROMPT,
  buildProject,
  canonicalJson,
  extractGeneratedProject,
  generateProject,
  parseArtifact,
  serializeArtifact,
  validateProject,
  type WorkerBundler,
} from "../src/builder";

const project = {
  name: "Tiny app",
  slug: "tiny-app",
  entryPoint: "src/index.ts",
  files: [
    { path: "src/message.ts", content: "export const message = 'hello';" },
    {
      path: "src/index.ts",
      content: "import { message } from './message'; export default { fetch(){ return new Response(message) } };",
    },
  ],
};

function fakeBundler(code = "export default {fetch(){return new Response('ok')}}"): WorkerBundler {
  return vi.fn(async () => ({ mainModule: "bundle.js", modules: { "bundle.js": code } }));
}

describe("generated project validation", () => {
  it("accepts JSON or objects and canonicalizes file ordering", () => {
    const shuffled = { ...project, files: [...project.files].reverse() };
    expect(validateProject(JSON.stringify(shuffled))).toEqual(validateProject(project));
    expect(validateProject(shuffled).files.map((file) => file.path)).toEqual([
      "src/index.ts",
      "src/message.ts",
    ]);
  });

  it.each([
    ["unknown key", { ...project, extra: true }],
    ["traversal", { ...project, files: [{ path: "../index.ts", content: "" }] }],
    ["absolute", { ...project, entryPoint: "/src/index.ts" }],
    ["node_modules", { ...project, files: [{ path: "src/node_modules/x.ts", content: "" }] }],
    ["package config", { ...project, files: [...project.files, { path: "package.json", content: "{}" }] }],
    ["wrangler config", { ...project, files: [...project.files, { path: "wrangler.jsonc", content: "{}" }] }],
    ["unsupported extension", { ...project, files: [{ path: "src/index.wasm", content: "" }] }],
    ["entry mismatch", { ...project, entryPoint: "src/missing.ts" }],
  ])("rejects %s", (_name, invalid) => {
    expect(() => validateProject(invalid)).toThrow();
  });

  it.each(["react", "node:fs", "npm:thing", "https://example.com/x.js", "data:text/javascript,x"])(
    "rejects forbidden import %s",
    (specifier) => {
      const invalid = {
        ...project,
        files: [{ path: "src/index.ts", content: `import value from ${JSON.stringify(specifier)}; export default value;` }],
      };
      expect(() => validateProject(invalid)).toThrow(/forbidden module/);
    },
  );

  it("allows cloudflare and in-project relative imports but rejects missing and non-literal imports", () => {
    const cloudflare = {
      ...project,
      files: project.files.map((file) => file.path === "src/index.ts"
        ? { ...file, content: `import "cloudflare:workers"; ${file.content}` }
        : file),
    };
    expect(validateProject(cloudflare)).toBeTruthy();
    expect(() => validateProject({
      ...project,
      files: [{ path: "src/index.ts", content: "import './missing';" }],
    })).toThrow(/missing local module/);
    expect(() => validateProject({
      ...project,
      files: [{ path: "src/index.ts", content: "const x = './message'; import(x);" }, project.files[0]],
    })).toThrow(/non-literal/);
  });

  it("enforces file count and UTF-8 byte limits", () => {
    const tooMany = Array.from({ length: 25 }, (_, index) => ({ path: `src/${index}.txt`, content: "x" }));
    expect(() => validateProject({ ...project, files: tooMany })).toThrow(/between 1 and 24/);
    expect(() => validateProject({
      ...project,
      files: [{ path: "src/index.ts", content: "😀".repeat(12 * 1024 + 1) }],
    })).toThrow(/49152 UTF-8 bytes/);
  });
});

describe("runtime bundle artifact", () => {
  it("passes only host-fixed bundler policy and emits a stable R2 artifact", async () => {
    const bundler = fakeBundler();
    const artifact = await buildProject(project, bundler);
    expect(bundler).toHaveBeenCalledWith({
      files: {
        "src/index.ts": project.files[1]!.content,
        "src/message.ts": project.files[0]!.content,
      },
      entryPoint: "src/index.ts",
      bundle: true,
      target: "es2022",
      minify: true,
      sourcemap: false,
      conditions: ["workerd", "worker", "browser"],
    });
    expect(artifact).toMatchObject({
      compatibilityDate: APP_COMPATIBILITY_DATE,
      policyVersion: APP_POLICY_VERSION,
      mainModule: "bundle.js",
      modules: { "bundle.js": { js: expect.any(String) } },
      revision: expect.stringMatching(/^[0-9a-f]{64}$/),
    });
    const serialized = serializeArtifact(artifact);
    expect(serialized).toBe(canonicalJson(artifact));
    expect(await parseArtifact(serialized)).toEqual(artifact);
    expect((await buildProject({ ...project, files: [...project.files].reverse() }, fakeBundler())).revision)
      .toBe(artifact.revision);
  });

  it("hashes source, compiled output, and fixed policy and detects tampering", async () => {
    const first = await buildProject(project, fakeBundler("one"));
    const sourceChanged = await buildProject({
      ...project,
      files: project.files.map((file) => file.path === "src/message.ts" ? { ...file, content: `${file.content}\n` } : file),
    }, fakeBundler("one"));
    const bundleChanged = await buildProject(project, fakeBundler("two"));
    expect(new Set([first.revision, sourceChanged.revision, bundleChanged.revision]).size).toBe(3);
    const tampered = JSON.parse(serializeArtifact(first)) as Record<string, unknown>;
    tampered.mainModule = "other.js";
    await expect(parseArtifact(JSON.stringify(tampered))).rejects.toThrow();
  });

  it("fails closed on warnings and non-text modules", async () => {
    await expect(buildProject(project, async () => ({
      mainModule: "bundle.js",
      modules: { "bundle.js": "code" },
      warnings: ["suspicious resolution"],
    }))).rejects.toThrow(/emitted warnings/);
    await expect(buildProject(project, async () => ({
      mainModule: "bundle.js",
      modules: { "bundle.js": { data: new ArrayBuffer(1) } },
    }))).rejects.toThrow(/text-only/);
  });
});

describe("Workers AI output", () => {
  it("extracts legacy, chat-completion, and content-block responses", () => {
    const json = JSON.stringify(project);
    expect(extractGeneratedProject({ response: json })).toBe(json);
    expect(extractGeneratedProject({ choices: [{ message: { content: json } }] })).toBe(json);
    expect(extractGeneratedProject({ result: { content: [{ type: "text", text: json }] } })).toBe(json);
    expect(extractGeneratedProject(project)).toBe(project);
  });

  it("uses the fixed model, strict capability prompt, and validates output", async () => {
    const run = vi.fn(async () => ({ response: JSON.stringify(project) }));
    await expect(generateProject({ run }, "Build a tiny app")).resolves.toEqual(validateProject(project));
    expect(run).toHaveBeenCalledWith(APP_GENERATION_MODEL, {
      max_completion_tokens: 20_000,
      messages: [
        { role: "system", content: PROJECT_SYSTEM_PROMPT },
        { role: "user", content: "Build a tiny app" },
      ],
      response_format: { type: "json_object" },
      temperature: 0.2,
    });
    for (const method of ["context", "get", "put", "delete", "list", "counter", "incrementCounter", "generateText"]) {
      expect(PROJECT_SYSTEM_PROMPT).toContain(`${method}(`);
    }
    expect(PROJECT_SYSTEM_PROMPT).toContain("There is no global fetch");
    expect(PROJECT_SYSTEM_PROMPT).toContain("at least 16px");
    expect(PROJECT_SYSTEM_PROMPT).toContain("at least 44 by 44 CSS pixels");
    expect(PROJECT_SYSTEM_PROMPT).toContain("zero horizontal overflow");
  });
});
