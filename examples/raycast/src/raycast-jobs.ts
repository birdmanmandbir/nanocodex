import { environment, launchCommand, LaunchType } from "@raycast/api";
import { join } from "node:path";

import { type BackgroundJobSubmission, BackgroundJobStore } from "./jobs";

const BACKGROUND_COMMAND = "run-nanocodex-job";

export function raycastJobStore(): BackgroundJobStore {
  return new BackgroundJobStore(
    join(environment.supportPath, "nanocodex-background-jobs"),
  );
}

export async function launchBackgroundWorker(
  jobId: string,
  submission?: BackgroundJobSubmission,
): Promise<void> {
  await launchCommand({
    name: BACKGROUND_COMMAND,
    type: LaunchType.Background,
    context: { jobId, ...(submission ? { submission } : {}) },
  });
}
