import { parsePatchFiles, type CodeViewItem } from "@pierre/diffs";
import { type CodeViewHandle, useStableCallback } from "@pierre/diffs/react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type RefObject,
} from "react";
import {
  appendCommitItemToCommitData,
  appendFileDiffToCommitData,
  createCommitDataAccumulator,
  takePendingCommitItems,
  type CommitDataAccumulator,
  type CommitStreamItem,
} from "./commitDataAccumulator";
import type { HarnessCommit } from "./NanocodexApp";

const COMMIT_BATCH_SIZE = 4;

export type CommitStreamLoadState = "fetching" | "ready" | "error";

interface UseCommitStreamLoaderOptions {
  collapseMode: "expanded" | "collapsed";
  commits: HarnessCommit[];
  startCommitIndex: number;
  viewerRef: RefObject<CodeViewHandle<undefined> | null>;
}

interface LoadContext {
  accumulator: CommitDataAccumulator;
  controller: AbortController;
  id: number;
  loadedCommitCount: number;
  requestedCommitCount: number;
  running: Promise<void> | null;
}

function commitItemId(commit: HarnessCommit): string {
  return `commit:${commit.hash}`;
}

function createCommitItem(commit: HarnessCommit): CommitStreamItem {
  return {
    id: commitItemId(commit),
    type: "file",
    collapsed: true,
    file: {
      name: commit.subject,
      contents: "",
      lang: "markdown",
      cacheKey: `${commit.hash}:message`,
    },
  };
}

function patchUrl(commit: HarnessCommit): string {
  return import.meta.env.DEV
    ? `/__nanocodex/repository/commits.diff?hash=${commit.hash}`
    : `/api/repository/commit/${commit.hash}.patch`;
}

async function fetchCommitPatch(
  commit: HarnessCommit,
  signal: AbortSignal,
): Promise<string> {
  const url = patchUrl(commit);
  const response = await fetch(url, {
    cache: import.meta.env.DEV ? "no-store" : "default",
    signal,
  });
  if (!response.ok) {
    throw new Error(`Patch request failed (${response.status}).`);
  }
  return response.text();
}

export function useCommitStreamLoader({
  collapseMode,
  commits,
  startCommitIndex,
  viewerRef,
}: UseCommitStreamLoaderOptions) {
  const [initialItems, setInitialItems] = useState<CodeViewItem<undefined>[]>(
    [],
  );
  const [loadState, setLoadState] =
    useState<CommitStreamLoadState>("fetching");
  const [loadedCommitCount, setLoadedCommitCount] = useState(0);
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [viewerKey, setViewerKey] = useState(0);
  const requestIdRef = useRef(0);
  const contextRef = useRef<LoadContext | null>(null);
  const loadedItemIdsRef = useRef<Set<string>>(new Set());
  const commitsRef = useRef(commits);
  const collapseModeRef = useRef(collapseMode);
  commitsRef.current = commits;
  collapseModeRef.current = collapseMode;

  const prepareItemsForViewer = useStableCallback(
    (items: readonly CodeViewItem<undefined>[]) => {
      const targetCollapsed = collapseModeRef.current === "collapsed";
      for (const item of items) {
        loadedItemIdsRef.current.add(item.id);
        if (item.type === "diff") item.collapsed = targetCollapsed;
      }
    },
  );

  const applyCollapseModeToLoaded = useStableCallback(
    (mode: "expanded" | "collapsed") => {
      const targetCollapsed = mode === "collapsed";
      const viewer = viewerRef.current;
      if (viewer == null) {
        setInitialItems((previous) => {
          let changed = false;
          const next = previous.map((item) => {
            if (
              item.type !== "diff" ||
              (item.collapsed === true) === targetCollapsed
            ) {
              return item;
            }
            changed = true;
            return { ...item, collapsed: targetCollapsed };
          });
          return changed ? next : previous;
        });
        return;
      }

      for (const itemId of loadedItemIdsRef.current) {
        const item = viewer.getItem(itemId);
        if (item == null || item.type !== "diff") continue;
        if ((item.collapsed === true) === targetCollapsed) continue;
        item.collapsed = targetCollapsed;
        item.version = getNextItemVersion(item);
        viewer.updateItem(item);
      }
    },
  );

  const drainLoadQueue = useStableCallback(async (context: LoadContext) => {
    try {
      while (
        context === contextRef.current &&
        !context.controller.signal.aborted &&
        context.loadedCommitCount < context.requestedCommitCount
      ) {
        const start = context.loadedCommitCount;
        const end = Math.min(
          start + COMMIT_BATCH_SIZE,
          context.requestedCommitCount,
          commitsRef.current.length,
        );
        const batch = commitsRef.current.slice(start, end);
        const patches = batch.map(async (commit) => {
          try {
            return { patch: await fetchCommitPatch(commit, context.controller.signal) };
          } catch (error) {
            return { error };
          }
        });

        for (const [index, commit] of batch.entries()) {
          const result = await patches[index];
          if (context !== contextRef.current) return;
          if ("error" in result) throw result.error;
          appendCommitItemToCommitData(
            context.accumulator,
            createCommitItem(commit),
          );
          const parsed = parsePatchFiles(
            result.patch,
            encodeURIComponent(patchUrl(commit)),
          );
          for (const patch of parsed) {
            for (const fileDiff of patch.files) {
              appendFileDiffToCommitData(
                context.accumulator,
                fileDiff,
                commit.hash,
              );
            }
          }
          const pendingItems = takePendingCommitItems(context.accumulator);
          prepareItemsForViewer(pendingItems);
          const viewer = viewerRef.current;
          if (viewer == null) {
            setInitialItems((previous) => [...previous, ...pendingItems]);
          } else {
            viewer.addItems(pendingItems);
          }
          context.loadedCommitCount = start + index + 1;
          setLoadedCommitCount(context.loadedCommitCount);
          setLoadState("ready");
          await yieldToBrowser();
          if (context !== contextRef.current) return;
        }
      }
    } catch (error) {
      if (
        context !== contextRef.current ||
        context.controller.signal.aborted
      ) {
        return;
      }
      console.warn("Failed to load commit diff", error);
      setLoadState("error");
    } finally {
      if (context === contextRef.current) context.running = null;
    }
  });

  const loadThrough = useStableCallback((commitCount: number) => {
    const context = contextRef.current;
    if (context == null) return;
    context.requestedCommitCount = Math.max(
      context.requestedCommitCount,
      Math.min(commitCount, commitsRef.current.length),
    );
    if (context.loadedCommitCount >= context.requestedCommitCount) return;
    if (context.running == null) {
      setLoadState("fetching");
      context.running = drainLoadQueue(context);
    }
  });

  const loadMore = useStableCallback(() => {
    const context = contextRef.current;
    if (context == null) return;
    loadThrough(context.loadedCommitCount + COMMIT_BATCH_SIZE);
  });

  useEffect(() => {
    const controller = new AbortController();
    const id = ++requestIdRef.current;
    const context: LoadContext = {
      accumulator: createCommitDataAccumulator(),
      controller,
      id,
      loadedCommitCount: startCommitIndex,
      requestedCommitCount: startCommitIndex,
      running: null,
    };
    contextRef.current = context;
    loadedItemIdsRef.current = new Set();
    setViewerKey(id);
    setInitialItems([]);
    setLoadedCommitCount(startCommitIndex);
    setLoadState("fetching");
    loadThrough(startCommitIndex + COMMIT_BATCH_SIZE);

    return () => {
      controller.abort();
      if (contextRef.current === context) contextRef.current = null;
    };
  }, [loadAttempt, loadThrough, startCommitIndex]);

  const retryLoad = useCallback(() => {
    const context = contextRef.current;
    if (context != null && context.loadedCommitCount > 0) {
      setLoadState("fetching");
      context.requestedCommitCount = Math.max(
        context.requestedCommitCount,
        context.loadedCommitCount + COMMIT_BATCH_SIZE,
      );
      if (context.running == null) context.running = drainLoadQueue(context);
      return;
    }
    setLoadAttempt((attempt) => attempt + 1);
  }, [drainLoadQueue]);

  return {
    applyCollapseModeToLoaded,
    hasMore: loadedCommitCount < commits.length,
    firstLoadedCommitIndex: startCommitIndex,
    initialItems,
    loadedCommitCount,
    loadMore,
    loadState,
    loadThrough,
    retryLoad,
    viewerKey,
  };
}

function getNextItemVersion(item: { version?: string | number }): number {
  return typeof item.version === "number" ? item.version + 1 : 1;
}

function yieldToBrowser(): Promise<void> {
  return new Promise((resolve) => {
    window.requestAnimationFrame(() => resolve());
  });
}
