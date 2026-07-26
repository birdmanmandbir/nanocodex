import { homedir } from "node:os";
import { join } from "node:path";

export function defaultWorkspace(): string {
  return (
    process.env.NANOCODEX_CWD ||
    join(homedir(), "github", "gakonst", "nanocodex")
  );
}
