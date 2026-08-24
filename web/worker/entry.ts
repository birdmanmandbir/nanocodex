import { WorkerEntrypoint } from "cloudflare:workers";

import worker from "./index.ts";
import {
  handleAppGitRequest,
  type AppGitServiceProps,
  type ThreadGitStorageEnv,
} from "./threadRoutes.ts";

export { ChatGptEgress } from "./chatGptEgress.ts";
export {
  ByokSession,
  ChatGptSession,
  EvalCoordinator,
  GitRepository,
  ThreadGitRepository,
} from "./index.ts";

export class AppGitService extends WorkerEntrypoint<ThreadGitStorageEnv, AppGitServiceProps> {
  request(repositoryName: string, request: Request): Promise<Response> {
    return handleAppGitRequest(repositoryName, request, this.env, this.ctx.props, this.ctx);
  }
}

export default worker;
