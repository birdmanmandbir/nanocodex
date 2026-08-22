#!/usr/bin/env node

import { createHash } from "node:crypto";
import { chmod, lstat, readFile, realpath, rename, unlink, writeFile } from "node:fs/promises";

const target = "/container-server/dist/index.js";
const expectedInputSha256 = "80f83fb4f8ad2a3ecc75f1964f48d8e7d50d921e52fe4840cb3d45cb020227a6";
const temporary = `${target}.nanocodex-auth`;

const identity = await lstat(target);
if (!identity.isFile() || identity.isSymbolicLink() || identity.uid !== 0 || identity.gid !== 0) {
  throw new Error("pinned Sandbox control server is not one root-owned regular file");
}
const canonical = await realpath(target);
if (canonical !== target) throw new Error("pinned Sandbox control server path is not canonical");

const input = await readFile(target, "utf8");
const inputSha256 = createHash("sha256").update(input).digest("hex");
if (inputSha256 !== expectedInputSha256) {
  throw new Error(`unexpected Sandbox control server bytes: ${inputSha256}`);
}

const functionNeedle = "async function E1(){";
const fetchNeedle = 'fetch:async(Q,X)=>{if(Q.headers.get("Upgrade")?.toLowerCase()==="websocket")';
if (count(input, functionNeedle) !== 1 || count(input, fetchNeedle) !== 1) {
  throw new Error("Sandbox control server patch boundary is ambiguous");
}

const authorization = String.raw`function nanocodexControlAuthorized($){let J=new URL($.url).pathname;if($.method==="GET"&&(J==="/"||J==="/api/health"||J==="/api/ping"||J==="/api/version"))return true;let Z=process.env.NANOCODEX_SANDBOX_CONTROL_TOKEN,Y=$.headers.get("X-Nanocodex-Sandbox-Control");if(typeof Z!=="string"||Z.length<80||typeof Y!=="string"||Y.length!==Z.length)return false;let Q=0;for(let X=0;X<Z.length;X++)Q|=Z.charCodeAt(X)^Y.charCodeAt(X);return Q===0}async function E1(){`;
const guardedFetch = String.raw`fetch:async(Q,X)=>{if(!nanocodexControlAuthorized(Q))return new Response(JSON.stringify({error:"unauthorized"}),{status:401,headers:{"Cache-Control":"no-store","Content-Type":"application/json","X-Content-Type-Options":"nosniff"}});if(Q.headers.get("Upgrade")?.toLowerCase()==="websocket")`;
const output = input
  .replace(functionNeedle, authorization)
  .replace(fetchNeedle, guardedFetch);

if (
  count(output, "nanocodexControlAuthorized") !== 2 ||
  count(output, "X-Nanocodex-Sandbox-Control") !== 1 ||
  count(output, fetchNeedle) !== 0
) {
  throw new Error("Sandbox control server patch did not produce one authenticated boundary");
}

await unlink(temporary).catch((error) => {
  if (error?.code !== "ENOENT") throw error;
});
try {
  await writeFile(temporary, output, { encoding: "utf8", flag: "wx", mode: 0o644 });
  await chmod(temporary, 0o644);
  await rename(temporary, target);
} catch (error) {
  await unlink(temporary).catch(() => undefined);
  throw error;
}

function count(value, needle) {
  return value.split(needle).length - 1;
}
