import assert from "node:assert/strict";
import test from "node:test";
import { putMultipartStream } from "../node_modules/@cloudflare/ci/src/ci/runners/multipart-stream.mjs";

const mebibyte = 1024 * 1024;

test("large CI streams are uploaded as exact R2 multipart parts", async () => {
  const bucket = fakeBucket();
  const chunks = [
    new Uint8Array(3 * mebibyte).fill(1),
    new Uint8Array(4 * mebibyte).fill(2),
    new Uint8Array(5 * mebibyte + 17).fill(3),
  ];
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);

  const object = await putMultipartStream(
    bucket.binding,
    "runs/head/steps/gate/attempts/1/stderr.log",
    byteStream(chunks),
    size,
    { httpMetadata: { contentType: "text/plain" } },
  );

  assert.equal(object.size, size);
  assert.deepEqual(bucket.partSizes, [5 * mebibyte, 5 * mebibyte, 2 * mebibyte + 17]);
  assert.equal(bucket.completed, true);
  assert.equal(bucket.aborted, false);
});

test("CI multipart upload aborts an overlong stream", async () => {
  const bucket = fakeBucket();
  await assert.rejects(
    putMultipartStream(
      bucket.binding,
      "too-long.log",
      byteStream([new Uint8Array(9), new Uint8Array(2)]),
      10,
      {},
    ),
    /expected 10, observed at least 11/,
  );
  assert.equal(bucket.completed, false);
  assert.equal(bucket.aborted, true);
});

test("CI multipart upload aborts a short stream", async () => {
  const bucket = fakeBucket();
  await assert.rejects(
    putMultipartStream(
      bucket.binding,
      "too-short.log",
      byteStream([new Uint8Array(9)]),
      10,
      {},
    ),
    /expected 10, observed 9/,
  );
  assert.equal(bucket.completed, false);
  assert.equal(bucket.aborted, true);
});

function byteStream(chunks: Uint8Array[]) {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk);
      controller.close();
    },
  });
}

function fakeBucket() {
  const partSizes: number[] = [];
  let completed = false;
  let aborted = false;
  let key = "";
  const binding = {
    async createMultipartUpload(nextKey: string) {
      key = nextKey;
      return {
        key,
        uploadId: "upload",
        async uploadPart(partNumber: number, value: ArrayBufferView) {
          partSizes.push(value.byteLength);
          return { partNumber, etag: `part-${partNumber}` };
        },
        async complete() {
          completed = true;
          return object(key, partSizes.reduce((total, size) => total + size, 0));
        },
        async abort() {
          aborted = true;
        },
      };
    },
    async delete() {},
  } as unknown as R2Bucket;
  return {
    binding,
    partSizes,
    get completed() { return completed; },
    get aborted() { return aborted; },
  };
}

function object(key: string, size: number) {
  return { key, size } as R2Object;
}
