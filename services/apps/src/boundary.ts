const PRIVATE_REQUEST_HEADERS = Object.freeze([
  "authorization",
  "cf-access-jwt-assertion",
  "cookie",
  "proxy-authorization",
]);

const PRIVATE_RESPONSE_HEADERS = Object.freeze([
  "authentication-info",
  "clear-site-data",
  "proxy-authentication-info",
  "proxy-authenticate",
  "report-to",
  "refresh",
  "set-cookie",
  "set-cookie2",
  "www-authenticate",
]);

const APP_CONTROLLED_SECURITY_HEADERS = Object.freeze([
  "content-security-policy-report-only",
  "cross-origin-embedder-policy",
  "cross-origin-embedder-policy-report-only",
  "cross-origin-opener-policy",
  "cross-origin-opener-policy-report-only",
  "cross-origin-resource-policy",
  "document-policy",
  "document-policy-report-only",
  "nel",
  "origin-agent-cluster",
  "permissions-policy",
  "permissions-policy-report-only",
  "service-worker-allowed",
  "timing-allow-origin",
]);

const APP_CONTENT_SECURITY_POLICY = [
  "default-src 'none'",
  "script-src 'unsafe-inline'",
  "script-src-attr 'none'",
  "style-src 'unsafe-inline'",
  "img-src data:",
  "worker-src 'none'",
  "connect-src 'none'",
  "form-action 'none'",
  "frame-src 'none'",
  "object-src 'none'",
  "base-uri 'none'",
  "frame-ancestors 'none'",
].join("; ");

export function appRequest(request: Request): Request {
  const headers = new Headers(request.headers);
  for (const name of PRIVATE_REQUEST_HEADERS) headers.delete(name);
  for (const name of [...headers.keys()]) {
    if (name.startsWith("x-nanocodex-")) headers.delete(name);
  }
  return new Request(request, { headers });
}

export function appResponse(response: Response): Response {
  const headers = new Headers(response.headers);
  for (const name of PRIVATE_RESPONSE_HEADERS) headers.delete(name);
  for (const name of APP_CONTROLLED_SECURITY_HEADERS) headers.delete(name);
  for (const name of [...headers.keys()]) {
    if (name.startsWith("access-control-")) headers.delete(name);
    if (name.startsWith("x-nanocodex-")) headers.delete(name);
  }
  headers.set("cache-control", "no-store");
  headers.set("content-security-policy", APP_CONTENT_SECURITY_POLICY);
  headers.set("referrer-policy", "no-referrer");
  headers.set("x-content-type-options", "nosniff");
  headers.set("x-frame-options", "DENY");
  return new Response(response.body, {
    headers,
    status: response.status,
    statusText: response.statusText,
  });
}
