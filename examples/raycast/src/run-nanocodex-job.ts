import type { LaunchProps } from "@raycast/api";

import { runBackgroundJobs } from "./background-worker";
import type { BackgroundJobSubmission } from "./jobs";
import { raycastJobStore } from "./raycast-jobs";

type LaunchContext = {
  jobId?: string;
  submission?: BackgroundJobSubmission;
};

export default async function Command(
  props: LaunchProps<{ launchContext: LaunchContext }>,
): Promise<void> {
  const store = raycastJobStore();
  const submission = props.launchContext?.submission;
  if (submission) {
    if (
      props.launchContext?.jobId &&
      submission.id !== props.launchContext.jobId
    ) {
      throw new Error("background job launch IDs do not match");
    }
    await store.enqueue(submission);
  }
  await runBackgroundJobs(store, props.launchContext?.jobId ?? submission?.id);
}
