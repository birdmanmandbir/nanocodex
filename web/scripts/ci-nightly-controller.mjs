#!/usr/bin/env node

import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const sha1Pattern = /^[a-f0-9]{40}$/;
const uuidV4Pattern =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const maximumResponseBytes = 16 * 1024;
const maximumErrorBytes = 4 * 1024;
const maximumTerminalErrorBytes = 2 * 1024;
const defaultRequestTimeoutMs = 10_000;
const defaultMaximumAttempts = 4;
const defaultRetryBaseMs = 250;
const defaultMaximumRetryDelayMs = 2_000;

class NightlyOperatorError extends Error {
  constructor(message, { cause, head = null, workflowId = null } = {}) {
    super(message, { cause });
    this.name = "NightlyOperatorError";
    this.head = head;
    this.workflowId = workflowId;
  }
}

class NightlyProtocolError extends NightlyOperatorError {
  constructor(message, options) {
    super(message, options);
    this.name = "NightlyProtocolError";
  }
}

export function parseNightlyArguments(args) {
  if (!Array.isArray(args)) {
    throw new TypeError("nightly arguments must be an array");
  }
  if (args.length === 0) return Object.freeze({ head: null });
  if (
    args.length === 2 &&
    args[0] === "--head" &&
    typeof args[1] === "string" &&
    sha1Pattern.test(args[1])
  ) {
    return Object.freeze({ head: args[1] });
  }
  throw new Error(
    "usage: ci-nightly-controller.mjs [--head <40-lowercase-hex>]",
  );
}

export function parseCiPublicOrigin(value) {
  if (
    typeof value !== "string" ||
    value === "" ||
    value !== value.trim() ||
    /[\u0000-\u0020\u007f]/u.test(value)
  ) {
    throw new Error("CI_PUBLIC_ORIGIN must be an absolute CI origin");
  }
  const shape = /^(https?):\/\/([^/?#]+)\/?$/i.exec(value);
  if (!shape || shape[2].includes("@") || value.includes("\\")) {
    throw new Error("CI_PUBLIC_ORIGIN must be an absolute CI origin");
  }
  let url;
  try {
    url = new URL(value);
  } catch (cause) {
    throw new Error("CI_PUBLIC_ORIGIN must be an absolute CI origin", { cause });
  }
  const loopback =
    url.hostname === "localhost" ||
    url.hostname.endsWith(".localhost") ||
    url.hostname === "127.0.0.1" ||
    url.hostname === "[::1]";
  if (
    (url.protocol !== "https:" && !(url.protocol === "http:" && loopback)) ||
    url.username !== "" ||
    url.password !== "" ||
    url.pathname !== "/" ||
    url.search !== "" ||
    url.hash !== "" ||
    url.origin === "null"
  ) {
    throw new Error(
      "CI_PUBLIC_ORIGIN must be an HTTPS origin without credentials, path, query, or fragment " +
        "(HTTP is allowed only for loopback)",
    );
  }
  return url.origin;
}

export function nightlyConfiguration(env = process.env) {
  if (env == null || typeof env !== "object") {
    throw new TypeError("nightly environment must be an object");
  }
  const releaseToken = env.CI_RELEASE_TOKEN;
  if (
    typeof releaseToken !== "string" ||
    releaseToken.length === 0 ||
    releaseToken.length > 8 * 1024 ||
    /\s|[\u0000-\u001f\u007f]/u.test(releaseToken)
  ) {
    throw new Error("CI_RELEASE_TOKEN must be a non-empty bearer token");
  }
  const origin = parseCiPublicOrigin(env.CI_PUBLIC_ORIGIN);
  return Object.freeze({
    origin,
    endpoint: new URL("/api/ci/releases/nightly", origin).href,
    releaseToken,
  });
}

export function parseNightlyResponse(value, requestedHead = null) {
  if (!isRecord(value)) {
    throw new NightlyProtocolError("nightly endpoint returned an invalid response schema");
  }
  const { status, head, workflowId, requestId } = value;
  if (
    !["current", "accepted", "restarted"].includes(status) ||
    typeof head !== "string" ||
    !sha1Pattern.test(head) ||
    (requestedHead != null && head !== requestedHead)
  ) {
    throw new NightlyProtocolError("nightly endpoint returned an invalid response identity");
  }
  const keys = Object.keys(value).sort();
  if (status === "current") {
    if (keys.length !== 2 || keys[0] !== "head" || keys[1] !== "status") {
      throw new NightlyProtocolError("nightly endpoint returned an invalid current response");
    }
    return Object.freeze({ status, head, workflowId: null });
  }
  if (
    keys.length !== 4 ||
    keys[0] !== "head" ||
    keys[1] !== "requestId" ||
    keys[2] !== "status" ||
    keys[3] !== "workflowId" ||
    workflowId !== `nightly-${head}` ||
    typeof requestId !== "string" ||
    !uuidV4Pattern.test(requestId)
  ) {
    throw new NightlyProtocolError("nightly endpoint returned an invalid attempt identity");
  }
  return Object.freeze({ status, head, workflowId, requestId });
}

/**
 * Dispatch one nightly operation. Retries preserve the exact authenticated POST
 * bytes; the endpoint reconciles a replay as current, accepted, or restarted.
 */
export async function dispatchNightlyRelease(
  config,
  options,
  runtime = {},
) {
  assertConfiguration(config);
  const head = options?.head ?? null;
  if (head != null && (typeof head !== "string" || !sha1Pattern.test(head))) {
    throw new TypeError("nightly head must be exactly 40 lowercase hexadecimal characters");
  }
  const settings = runtimeSettings(runtime);
  const body = head == null ? undefined : JSON.stringify({ head });
  const workflowId = head == null ? null : `nightly-${head}`;
  const headers = Object.freeze({
    accept: "application/json",
    authorization: `Bearer ${config.releaseToken}`,
    ...(body == null ? {} : { "content-type": "application/json" }),
  });
  const request = Object.freeze({
    method: "POST",
    headers,
    redirect: "error",
    ...(body == null ? {} : { body }),
  });

  let lastTransportFailure;
  for (let attempt = 1; attempt <= settings.maximumAttempts; attempt += 1) {
    const controller = new AbortController();
    let timedOut = false;
    let timeout;
    const deadline = new Promise((_, reject) => {
      timeout = setTimeout(() => {
        timedOut = true;
        const cause = new Error(
          `request timed out after ${settings.requestTimeoutMs}ms`,
        );
        controller.abort(cause);
        reject(cause);
      }, settings.requestTimeoutMs);
    });
    let response;
    try {
      try {
        response = await Promise.race([
          settings.fetchImpl(config.endpoint, {
            ...request,
            signal: controller.signal,
          }),
          deadline,
        ]);
      } catch (cause) {
        lastTransportFailure = timedOut
          ? new Error(`request timed out after ${settings.requestTimeoutMs}ms`)
          : cause;
      }

      if (response != null) {
        if (retryableStatus(response.status)) {
          if (attempt === settings.maximumAttempts) {
            throw await responseFailureBeforeDeadline(
              response,
              config.releaseToken,
              { head, workflowId },
              deadline,
            );
          }
          const delay = retryDelayMs(
            response,
            attempt,
            settings.retryBaseMs,
            settings.maximumRetryDelayMs,
            settings.now,
          );
          await Promise.race([
            response.body?.cancel().catch(() => undefined) ?? Promise.resolve(),
            deadline,
          ]).catch(() => undefined);
          clearTimeout(timeout);
          await settings.sleep(delay);
          continue;
        }
        if (!response.ok) {
          throw await responseFailureBeforeDeadline(
            response,
            config.releaseToken,
            { head, workflowId },
            deadline,
          );
        }
        try {
          const result = await Promise.race([
            readNightlyResponse(response, head),
            deadline,
          ]);
          if (
            (result.status === "current" && response.status !== 200) ||
            (result.status !== "current" && response.status !== 202)
          ) {
            throw new NightlyProtocolError(
              "nightly endpoint returned an invalid success status",
              { head, workflowId },
            );
          }
          return result;
        } catch (cause) {
          if (cause instanceof NightlyProtocolError) throw cause;
          lastTransportFailure = cause;
        }
      }
    } finally {
      clearTimeout(timeout);
    }

    if (attempt < settings.maximumAttempts) {
      await settings.sleep(
        exponentialDelay(
          attempt,
          settings.retryBaseMs,
          settings.maximumRetryDelayMs,
        ),
      );
      continue;
    }
  }

  const detail = redactNightlySecrets(
    errorMessage(lastTransportFailure) || "transport failure",
    [config.releaseToken],
  );
  throw new NightlyOperatorError(
    `nightly POST failed after ${settings.maximumAttempts} attempts: ${detail}`,
    { head, workflowId },
  );
}

export function nightlyFailureRecord(cause, env = process.env, args = []) {
  const argumentHead =
    Array.isArray(args) && args.length === 2 && args[0] === "--head" &&
      typeof args[1] === "string" && sha1Pattern.test(args[1])
      ? args[1]
      : null;
  const head = typeof cause?.head === "string" && sha1Pattern.test(cause.head)
    ? cause.head
    : argumentHead;
  const workflowId =
    typeof cause?.workflowId === "string" && cause.workflowId === `nightly-${head}`
      ? cause.workflowId
      : head == null
      ? null
      : `nightly-${head}`;
  return {
    status: "error",
    head,
    workflowId,
    error: redactNightlySecrets(errorMessage(cause) || "nightly request failed", [
      ...environmentSecrets(env),
      env?.CI_RELEASE_TOKEN,
    ]).slice(0, maximumTerminalErrorBytes),
  };
}

export function redactNightlySecrets(value, secrets = []) {
  let redacted = String(value)
    .replace(/\bBearer\s+[^\s,;"']+/giu, "Bearer [redacted]");
  const ordered = [...new Set(secrets)]
    .filter((secret) => typeof secret === "string" && secret !== "")
    .sort((left, right) => right.length - left.length);
  for (const secret of ordered) {
    redacted = redacted.split(secret).join("[redacted]");
  }
  return redacted;
}

export async function main(
  args = process.argv.slice(2),
  env = process.env,
  runtime = {},
) {
  const options = parseNightlyArguments(args);
  const config = nightlyConfiguration(env);
  const { stdout = process.stdout, ...requestRuntime } = runtime;
  const result = await dispatchNightlyRelease(config, options, requestRuntime);
  stdout.write(`${JSON.stringify(result)}\n`);
  return result;
}

async function readNightlyResponse(response, requestedHead) {
  const contentType = response.headers.get("content-type")?.split(";", 1)[0]?.trim()
    .toLowerCase();
  if (contentType !== "application/json") {
    cancelBody(response.body);
    throw new NightlyProtocolError(
      "nightly endpoint did not return application/json",
      { head: requestedHead },
    );
  }
  const bytes = await boundedResponseBytes(
    response,
    maximumResponseBytes,
    "nightly response",
  );
  let value;
  try {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    value = JSON.parse(text);
  } catch (cause) {
    throw new NightlyProtocolError("nightly endpoint returned invalid JSON", {
      cause,
      head: requestedHead,
    });
  }
  return parseNightlyResponse(value, requestedHead);
}

async function responseFailure(response, releaseToken, identity) {
  let detail = "";
  try {
    const bytes = await boundedResponseBytes(
      response,
      maximumErrorBytes,
      "nightly error response",
    );
    detail = new TextDecoder("utf-8").decode(bytes).trim();
  } catch {
    cancelBody(response.body);
  }
  const sanitized = redactNightlySecrets(detail, [releaseToken]);
  return new NightlyOperatorError(
    `nightly POST failed with HTTP ${response.status}${sanitized ? `: ${sanitized}` : ""}`,
    identity,
  );
}

async function responseFailureBeforeDeadline(
  response,
  releaseToken,
  identity,
  deadline,
) {
  try {
    return await Promise.race([
      responseFailure(response, releaseToken, identity),
      deadline,
    ]);
  } catch {
    cancelBody(response.body);
    return new NightlyOperatorError(
      `nightly POST failed with HTTP ${response.status}`,
      identity,
    );
  }
}

async function boundedResponseBytes(response, maximumBytes, operation) {
  const declared = response.headers.get("content-length");
  if (declared != null) {
    if (!/^(?:0|[1-9][0-9]*)$/.test(declared) || Number(declared) > maximumBytes) {
      cancelBody(response.body);
      throw new NightlyProtocolError(`${operation} exceeded ${maximumBytes} bytes`);
    }
  }
  if (response.body == null) {
    throw new NightlyProtocolError(`${operation} returned no body`);
  }
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > maximumBytes) {
        cancelBody(reader);
        throw new NightlyProtocolError(`${operation} exceeded ${maximumBytes} bytes`);
      }
      chunks.push(Buffer.from(value));
    }
  } finally {
    reader.releaseLock();
  }
  return Buffer.concat(chunks, size);
}

function runtimeSettings(runtime) {
  if (runtime == null || typeof runtime !== "object") {
    throw new TypeError("nightly runtime settings must be an object");
  }
  const settings = {
    fetchImpl: runtime.fetchImpl ?? globalThis.fetch,
    sleep: runtime.sleep ?? sleep,
    now: runtime.now ?? Date.now,
    requestTimeoutMs: runtime.requestTimeoutMs ?? defaultRequestTimeoutMs,
    maximumAttempts: runtime.maximumAttempts ?? defaultMaximumAttempts,
    retryBaseMs: runtime.retryBaseMs ?? defaultRetryBaseMs,
    maximumRetryDelayMs:
      runtime.maximumRetryDelayMs ?? defaultMaximumRetryDelayMs,
  };
  if (typeof settings.fetchImpl !== "function" || typeof settings.sleep !== "function") {
    throw new TypeError("nightly fetch and sleep implementations must be functions");
  }
  if (typeof settings.now !== "function") {
    throw new TypeError("nightly clock must be a function");
  }
  if (
    !Number.isSafeInteger(settings.requestTimeoutMs) ||
    settings.requestTimeoutMs <= 0 ||
    settings.requestTimeoutMs > 60_000 ||
    !Number.isSafeInteger(settings.maximumAttempts) ||
    settings.maximumAttempts <= 0 ||
    settings.maximumAttempts > 10 ||
    !Number.isSafeInteger(settings.retryBaseMs) ||
    settings.retryBaseMs < 0 ||
    !Number.isSafeInteger(settings.maximumRetryDelayMs) ||
    settings.maximumRetryDelayMs < settings.retryBaseMs ||
    settings.maximumRetryDelayMs > 60_000
  ) {
    throw new TypeError("nightly retry settings are invalid");
  }
  return settings;
}

function retryableStatus(status) {
  return status === 408 || status === 425 || status === 429 ||
    (status >= 500 && status <= 599);
}

function retryDelayMs(response, attempt, baseMs, maximumMs, now) {
  const header = response.headers.get("retry-after");
  if (header != null) {
    if (/^(?:0|[1-9][0-9]*)$/.test(header)) {
      const milliseconds = Number(header) * 1_000;
      if (Number.isSafeInteger(milliseconds)) return Math.min(milliseconds, maximumMs);
    }
    const date = Date.parse(header);
    if (Number.isFinite(date)) {
      return Math.min(Math.max(0, date - now()), maximumMs);
    }
  }
  return exponentialDelay(attempt, baseMs, maximumMs);
}

function exponentialDelay(attempt, baseMs, maximumMs) {
  return Math.min(baseMs * (2 ** Math.max(0, attempt - 1)), maximumMs);
}

function assertConfiguration(config) {
  if (
    !isRecord(config) ||
    typeof config.origin !== "string" ||
    parseCiPublicOrigin(config.origin) !== config.origin ||
    config.endpoint !== new URL("/api/ci/releases/nightly", config.origin).href ||
    typeof config.releaseToken !== "string" ||
    config.releaseToken.length === 0 ||
    config.releaseToken.length > 8 * 1024 ||
    /\s|[\u0000-\u001f\u007f]/u.test(config.releaseToken)
  ) {
    throw new TypeError("nightly configuration is invalid");
  }
}

function environmentSecrets(env) {
  if (env == null || typeof env !== "object") return [];
  return Object.entries(env)
    .filter(([name, value]) =>
      /(?:TOKEN|SECRET|PASSWORD|API_KEY|PRIVATE_KEY)/i.test(name) &&
      typeof value === "string" && value !== ""
    )
    .map(([, value]) => value);
}

function isRecord(value) {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(value) {
  return value instanceof Error ? value.message : String(value ?? "");
}

function sleep(milliseconds) {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, milliseconds));
}

function cancelBody(bodyOrReader) {
  try {
    bodyOrReader?.cancel().catch(() => undefined);
  } catch {
    // Cancellation is best effort; validation and the hard deadline own control flow.
  }
}

if (resolve(process.argv[1] ?? "") === scriptPath) {
  main().catch((cause) => {
    process.stderr.write(
      `${JSON.stringify(nightlyFailureRecord(cause, process.env, process.argv.slice(2)))}\n`,
    );
    process.exitCode = 1;
  });
}
