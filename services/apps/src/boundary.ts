const PRIVATE_REQUEST_HEADERS = Object.freeze([
  "authorization",
  "cf-access-jwt-assertion",
  "cookie",
  "proxy-authorization",
]);

const PRIVATE_RESPONSE_HEADERS = Object.freeze([
  "authentication-info",
  "proxy-authenticate",
  "set-cookie",
  "set-cookie2",
]);

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
  headers.set("referrer-policy", "same-origin");
  headers.set("x-content-type-options", "nosniff");
  return new Response(response.body, {
    headers,
    status: response.status,
    statusText: response.statusText,
  });
}
