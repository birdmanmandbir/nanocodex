import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildUploadPlan,
  isRetriableUploadStatus,
} from "./publish-repository.mjs";

test("repository publication uploads only content absent from the prior inventory", () => {
  assert.deepEqual(
    buildUploadPlan(
      { blobs: ["a", "b"], patches: ["1", "2"] },
      { blobs: ["a"], patches: ["1"] },
    ),
    { blobs: ["b"], patches: ["2"] },
  );
});

test("repository uploads retry only transient and secret-propagation responses", () => {
  for (const status of [401, 408, 425, 429, 500, 503]) {
    assert.equal(isRetriableUploadStatus(status), true, `${status} should retry`);
  }
  for (const status of [400, 403, 404, 409, 422]) {
    assert.equal(isRetriableUploadStatus(status), false, `${status} should fail`);
  }
});
