import { useVirtualizer } from "@tanstack/react-virtual";
import { ChevronRight } from "lucide-react";
import { memo, useCallback, useEffect, useRef, useState } from "react";
import type { HarnessCommit } from "./NanocodexApp";

type VirtualCommitListProps = {
  commits: HarnessCommit[];
  hasMore: boolean;
  selectedHash?: string;
  onClearSearch(): void;
  onLoadMore(): Promise<boolean>;
  onSelectCommit(commit: HarnessCommit): void;
};

const dateFormatter = new Intl.DateTimeFormat("en", {
  month: "short",
  day: "numeric",
  year: "numeric",
  hour: "numeric",
  minute: "2-digit",
});

const relativeFormatter = new Intl.RelativeTimeFormat("en", {
  numeric: "auto",
});

function relativeDate(value: string) {
  const milliseconds = new Date(value).getTime() - Date.now();
  const hours = Math.round(milliseconds / 3_600_000);
  if (Math.abs(hours) < 24) return relativeFormatter.format(hours, "hour");
  const days = Math.round(milliseconds / 86_400_000);
  if (Math.abs(days) < 30) return relativeFormatter.format(days, "day");
  return dateFormatter.format(new Date(value));
}

export const VirtualCommitList = memo(function VirtualCommitList({
  commits,
  hasMore,
  selectedHash,
  onClearSearch,
  onLoadMore,
  onSelectCommit,
}: VirtualCommitListProps) {
  const listRef = useRef<HTMLDivElement>(null);
  const [loadError, setLoadError] = useState(false);
  const virtualizer = useVirtualizer({
    count: commits.length,
    getScrollElement: () => listRef.current,
    estimateSize: () => 108,
    getItemKey: (index) => commits[index]?.hash ?? index,
    overscan: 8,
  });
  const virtualItems = virtualizer.getVirtualItems();

  const loadMore = useCallback(() => {
    setLoadError(false);
    void onLoadMore().catch((error) => {
      console.warn("Failed to load commit metadata", error);
      setLoadError(true);
    });
  }, [onLoadMore]);

  useEffect(() => {
    const last = virtualItems.at(-1);
    if (hasMore && !loadError && last != null && last.index >= commits.length - 8) {
      loadMore();
    }
  }, [commits.length, hasMore, loadError, loadMore, virtualItems]);

  return (
    <div className="commit-list" ref={listRef}>
      {commits.length ? (
        <div
          style={{
            position: "relative",
            height: `${virtualizer.getTotalSize()}px`,
          }}
        >
          {virtualItems.map((virtualRow) => {
            const commit = commits[virtualRow.index];
            if (!commit) return null;
            const isSelected = commit.hash === selectedHash;
            return (
              <button
                className={isSelected ? "commit-row is-selected" : "commit-row"}
                type="button"
                key={commit.hash}
                aria-current={isSelected ? "location" : undefined}
                style={{
                  position: "absolute",
                  top: 0,
                  left: 0,
                  transform: `translateY(${virtualRow.start}px)`,
                }}
                onClick={() => onSelectCommit(commit)}
              >
                <span className="commit-meta">
                  <span>{commit.shortHash}</span>
                  <span>{relativeDate(commit.authoredAt)}</span>
                </span>
                <strong>{commit.subject}</strong>
                <span className="commit-byline">
                  {commit.author} · {commit.stats.files} file
                  {commit.stats.files === 1 ? "" : "s"}
                </span>
                <ChevronRight className="commit-chevron" aria-hidden="true" />
              </button>
            );
          })}
        </div>
      ) : (
        <div className="empty-state">
          <p>No commits match this filter.</p>
          <button type="button" onClick={onClearSearch}>
            Clear search
          </button>
        </div>
      )}
      {loadError ? (
        <div className="commit-list-tail-error" role="alert">
          <span>Couldn’t load more commits.</span>
          <button type="button" onClick={loadMore}>Try again</button>
        </div>
      ) : null}
    </div>
  );
});
