import {
  Action,
  ActionPanel,
  Icon,
  Keyboard,
  type LaunchProps,
  List,
  Toast,
  showToast,
  useNavigation,
} from "@raycast/api";
import type { TerminalEntry } from "nanocodex-tui";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { defaultWorkspace } from "./config";
import {
  type BackgroundConversation,
  type BackgroundJob,
  draftBackgroundJob,
  isTerminalJob,
  newBackgroundJobSubmission,
  serializeConversation,
} from "./jobs";
import { launchBackgroundWorker, raycastJobStore } from "./raycast-jobs";
import {
  listSavedConversations,
  loadSavedTranscript,
  workspaceName,
  type SavedConversation,
} from "./sessions";

type Arguments = {
  prompt?: string;
};

const CHAT_ITEM_ID = "conversation";
const JOB_POLL_MS = 250;
const BROWSER_POLL_MS = 750;
const STALE_JOB_MS = 30_000;
const RECOVERY_LAUNCH_COOLDOWN_MS = 10_000;
const MAX_RENDERED_TRANSCRIPT_CHARACTERS = 120_000;
const CANCEL_SHORTCUT = {
  modifiers: ["cmd"],
  key: ".",
} as const satisfies Keyboard.Shortcut;

export default function Command(props: LaunchProps<{ arguments: Arguments }>) {
  const initialPrompt = (
    props.fallbackText ||
    props.arguments.prompt ||
    ""
  ).trim();
  return initialPrompt ? (
    <Conversation initialPrompt={initialPrompt} />
  ) : (
    <ConversationBrowser />
  );
}

function ConversationBrowser() {
  const { push } = useNavigation();
  const store = useMemo(() => raycastJobStore(), []);
  const [conversations, setConversations] = useState<SavedConversation[]>([]);
  const [jobs, setJobs] = useState<BackgroundJob[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();
  const lastRecoveryLaunch = useRef(0);
  const observedJobs = useRef(false);
  const terminalJobs = useRef(new Set<string>());

  const refreshConversations = useCallback(async () => {
    setConversations(await listSavedConversations());
  }, []);

  const refreshJobs = useCallback(async () => {
    const next = await store.list(100);
    const completed = new Set(
      next.filter((job) => job.status === "completed").map((job) => job.id),
    );
    const gainedCompletion =
      observedJobs.current &&
      [...completed].some((id) => !terminalJobs.current.has(id));
    observedJobs.current = true;
    terminalJobs.current = completed;
    setJobs(next);
    if (gainedCompletion) await refreshConversations();

    const now = Date.now();
    const recoverable = next
      .slice()
      .reverse()
      .find(
        (job) =>
          job.status === "queued" ||
          (!isTerminalJob(job) &&
            now - Date.parse(job.updatedAt) > STALE_JOB_MS),
      );
    if (
      recoverable &&
      now - lastRecoveryLaunch.current > RECOVERY_LAUNCH_COOLDOWN_MS
    ) {
      lastRecoveryLaunch.current = now;
      void launchBackgroundWorker(recoverable.id).catch(() => undefined);
    }
  }, [refreshConversations, store]);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(undefined);
    try {
      await Promise.all([refreshConversations(), refreshJobs()]);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setLoading(false);
    }
  }, [refreshConversations, refreshJobs]);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => {
      void refreshJobs().catch((cause: unknown) => {
        setError(errorMessage(cause));
      });
    }, BROWSER_POLL_MS);
    return () => clearInterval(timer);
  }, [refresh, refreshJobs]);

  const active = conversations.filter((conversation) => !conversation.archived);
  const archived = conversations.filter(
    (conversation) => conversation.archived,
  );
  const visibleJobs = jobs
    .filter((job) => job.status !== "completed")
    .slice(0, 20);
  const openConversation = (conversation?: SavedConversation) =>
    push(<Conversation saved={conversation} />);
  const openJob = (job: BackgroundJob) =>
    push(<Conversation initialJobId={job.id} />);

  return (
    <List
      isLoading={loading}
      navigationTitle="Nanocodex Conversations"
      searchBarPlaceholder="Search jobs and ~/.codex/sessions..."
    >
      {error ? (
        <List.EmptyView
          icon={Icon.ExclamationMark}
          title="Could not load Nanocodex"
          description={error}
          actions={
            <ActionPanel>
              <Action
                icon={Icon.ArrowClockwise}
                title="Retry"
                onAction={refresh}
              />
            </ActionPanel>
          }
        />
      ) : null}

      {visibleJobs.length ? (
        <List.Section title="Background Jobs">
          {visibleJobs.map((job) => (
            <BackgroundJobItem
              key={job.id}
              job={job}
              onOpen={() => openJob(job)}
              onRefresh={refresh}
            />
          ))}
        </List.Section>
      ) : null}

      {active.length ? (
        <List.Section title="Recent Conversations">
          {active.map((conversation) => (
            <ConversationItem
              key={conversation.id}
              conversation={conversation}
              onOpen={() => openConversation(conversation)}
              onNew={() => openConversation()}
              onRefresh={refresh}
            />
          ))}
        </List.Section>
      ) : null}

      <List.Section title="New">
        <List.Item
          id="new-conversation"
          icon={Icon.PlusCircle}
          title="New Nanocodex Conversation"
          subtitle="Background JS/WASM · Tempo MPP · ~/.codex/sessions"
          actions={
            <ActionPanel>
              <Action
                icon={Icon.Plus}
                title="Start New Conversation"
                onAction={() => openConversation()}
              />
              <Action
                icon={Icon.ArrowClockwise}
                title="Refresh Conversations"
                shortcut={Keyboard.Shortcut.Common.Refresh}
                onAction={refresh}
              />
            </ActionPanel>
          }
        />
      </List.Section>

      {archived.length ? (
        <List.Section title="Archived">
          {archived.map((conversation) => (
            <ConversationItem
              key={conversation.id}
              conversation={conversation}
              onOpen={() => openConversation(conversation)}
              onNew={() => openConversation()}
              onRefresh={refresh}
            />
          ))}
        </List.Section>
      ) : null}
    </List>
  );
}

function BackgroundJobItem({
  job,
  onOpen,
  onRefresh,
}: {
  job: BackgroundJob;
  onOpen(): void;
  onRefresh(): void;
}) {
  return (
    <List.Item
      id={`job-${job.id}`}
      icon={
        job.status === "failed"
          ? Icon.ExclamationMark
          : job.status === "cancelled"
            ? Icon.Stop
            : Icon.Clock
      }
      title={preview(job.prompt)}
      subtitle={job.statusDetail}
      accessories={[{ tag: job.status }, { date: new Date(job.updatedAt) }]}
      actions={
        <ActionPanel>
          <Action
            icon={Icon.Bubble}
            title="Open Background Job"
            onAction={onOpen}
          />
          <Action.CopyToClipboard
            content={job.id}
            title="Copy Background Job ID"
          />
          <Action
            icon={Icon.ArrowClockwise}
            title="Refresh"
            shortcut={Keyboard.Shortcut.Common.Refresh}
            onAction={onRefresh}
          />
        </ActionPanel>
      }
    />
  );
}

function ConversationItem({
  conversation,
  onOpen,
  onNew,
  onRefresh,
}: {
  conversation: SavedConversation;
  onOpen(): void;
  onNew(): void;
  onRefresh(): void;
}) {
  return (
    <List.Item
      id={conversation.id}
      icon={conversation.archived ? Icon.Box : Icon.Bubble}
      title={conversation.title}
      subtitle={conversation.cwd}
      accessories={[
        { tag: workspaceName(conversation) },
        { date: conversation.updatedAt },
      ]}
      actions={
        <ActionPanel>
          <Action
            icon={Icon.Bubble}
            title="Open Conversation"
            onAction={onOpen}
          />
          <Action
            icon={Icon.Plus}
            title="New Conversation"
            shortcut={Keyboard.Shortcut.Common.New}
            onAction={onNew}
          />
          <Action.CopyToClipboard
            content={conversation.id}
            title="Copy Thread ID"
          />
          <Action
            icon={Icon.ArrowClockwise}
            title="Refresh Conversations"
            shortcut={Keyboard.Shortcut.Common.Refresh}
            onAction={onRefresh}
          />
        </ActionPanel>
      }
    />
  );
}

function Conversation({
  saved,
  initialPrompt = "",
  initialJobId,
}: {
  saved?: SavedConversation;
  initialPrompt?: string;
  initialJobId?: string;
}) {
  const store = useMemo(() => raycastJobStore(), []);
  const requestedWorkspace = useMemo(() => defaultWorkspace(), []);
  const [job, setJob] = useState<BackgroundJob>();
  const [parentJob, setParentJob] = useState<BackgroundJob>();
  const [jobId, setJobId] = useState(initialJobId);
  const [conversation, setConversation] = useState<
    BackgroundConversation | undefined
  >(saved ? serializeConversation(saved) : undefined);
  const [history, setHistory] = useState<TerminalEntry[]>([]);
  const [historyTruncated, setHistoryTruncated] = useState(false);
  const [historyLoading, setHistoryLoading] = useState(Boolean(saved));
  const [loadedHistoryKey, setLoadedHistoryKey] = useState("");
  const [draft, setDraft] = useState("");
  const [viewError, setViewError] = useState<string>();
  const initialDispatched = useRef(false);
  const pendingHandoffs = useRef(new Map<string, number>());

  useEffect(() => {
    if (!jobId) return;
    let active = true;
    const poll = async () => {
      const next = await store.read(jobId);
      if (!active) return;
      if (!next) {
        const handoffStarted = pendingHandoffs.current.get(jobId);
        if (
          handoffStarted !== undefined &&
          Date.now() - handoffStarted < 5_000
        ) {
          return;
        }
        setViewError(`Background job ${jobId} no longer exists`);
        return;
      }
      pendingHandoffs.current.delete(jobId);
      setViewError(undefined);
      setJob((current) =>
        current?.revision === next.revision ? current : next,
      );
      if (next.conversation) setConversation(next.conversation);
      if (next.parentJobId && !isTerminalJob(next)) {
        const parent = await store.read(next.parentJobId);
        if (!active) return;
        setParentJob(parent);
        if (parent?.conversation) setConversation(parent.conversation);
      } else {
        setParentJob(undefined);
      }
    };
    void poll().catch((cause: unknown) => setViewError(errorMessage(cause)));
    const timer = setInterval(() => {
      void poll().catch((cause: unknown) => setViewError(errorMessage(cause)));
    }, JOB_POLL_MS);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, [jobId, store]);

  const historyPhase =
    job?.status === "completed"
      ? "completed"
      : job?.status === "starting" || job?.status === "running"
        ? "running"
        : "base";
  const historyKey = conversation
    ? `${conversation.id}:${conversation.path}:${conversation.updatedAt}:${historyPhase}`
    : "";

  useEffect(() => {
    if (!conversation || historyKey === loadedHistoryKey) return;
    let active = true;
    setHistoryLoading(true);
    void loadSavedTranscript(asSavedConversation(conversation))
      .then((transcript) => {
        if (!active) return;
        setHistory(transcript.entries);
        setHistoryTruncated(transcript.truncated);
        setLoadedHistoryKey(historyKey);
      })
      .catch((cause: unknown) => {
        if (active) setViewError(errorMessage(cause));
      })
      .finally(() => {
        if (active) setHistoryLoading(false);
      });
    return () => {
      active = false;
    };
  }, [conversation, historyKey, loadedHistoryKey]);

  const dispatchPrompt = useCallback(
    async (rawPrompt: string) => {
      const prompt = rawPrompt.trim();
      if (!prompt) return;
      if (job?.status === "queued") {
        await showToast({
          style: Toast.Style.Failure,
          title: "A follow-up is already queued",
          message: "Open another conversation to run an independent job.",
        });
        return;
      }
      const parentJobId = job && !isTerminalJob(job) ? job.id : undefined;
      const submission = newBackgroundJobSubmission({
        prompt,
        workspace: conversation?.cwd ?? requestedWorkspace,
        ...(conversation ? { conversation } : {}),
        ...(parentJobId ? { parentJobId } : {}),
      });
      const created = draftBackgroundJob(submission);
      setDraft("");
      setViewError(undefined);
      setParentJob(parentJobId ? job : undefined);
      setJob(created);
      setJobId(created.id);
      pendingHandoffs.current.set(created.id, Date.now());
      const launch = launchBackgroundWorker(created.id, submission);
      try {
        await launch;
      } catch (cause) {
        const message = errorMessage(cause);
        const saved = await store.enqueue(submission);
        setJob(saved);
        setViewError(
          `The prompt is saved but Raycast could not launch its worker: ${message}`,
        );
        await showToast({
          style: Toast.Style.Failure,
          title: "Background worker did not launch",
          message,
        });
      }
    },
    [conversation, job, requestedWorkspace, store],
  );

  useEffect(() => {
    if (!initialPrompt || initialDispatched.current) return;
    initialDispatched.current = true;
    void dispatchPrompt(initialPrompt).catch((cause: unknown) => {
      const message = errorMessage(cause);
      setViewError(message);
      void showToast({
        style: Toast.Style.Failure,
        title: "Could not queue Nanocodex",
        message,
      });
    });
  }, [dispatchPrompt, initialPrompt]);

  const submit = useCallback(() => {
    void dispatchPrompt(draft).catch((cause: unknown) => {
      const message = errorMessage(cause);
      setViewError(message);
      void showToast({
        style: Toast.Style.Failure,
        title: "Could not queue Nanocodex",
        message,
      });
    });
  }, [dispatchPrompt, draft]);

  const cancel = useCallback(() => {
    if (!job || isTerminalJob(job)) return;
    void store
      .requestCancellation(job.id)
      .then(() => {
        setJob((current) =>
          current
            ? {
                ...current,
                statusDetail: "Cancellation requested",
                terminal: {
                  ...current.terminal,
                  status: "Cancellation requested",
                },
              }
            : current,
        );
      })
      .catch((cause: unknown) => {
        setViewError(errorMessage(cause));
      });
  }, [job, store]);

  const parentActive = parentJob && !isTerminalJob(parentJob);
  const observedJob = parentActive ? parentJob : job;
  const completedHistoryLoaded =
    job?.status === "completed" && loadedHistoryKey === historyKey;
  const liveEntries = [
    ...(parentActive ? parentJob.terminal.entries : []),
    ...(job && !completedHistoryLoaded ? job.terminal.entries : []),
  ];
  const displayEntries = [...history, ...liveEntries];
  const status = parentActive
    ? `${job?.statusDetail ?? "Follow-up queued"} · ${parentJob.statusDetail}`
    : (job?.statusDetail ??
      (historyLoading ? "Loading conversation..." : "Ready"));
  const markdown = useMemo(
    () =>
      transcriptMarkdown(
        viewError
          ? [
              ...displayEntries,
              {
                id: "view-error",
                kind: "error",
                text: viewError,
              } satisfies TerminalEntry,
            ]
          : displayEntries,
        status,
        historyTruncated || job?.truncatedHistory === true,
      ),
    [
      displayEntries,
      historyTruncated,
      job?.truncatedHistory,
      status,
      viewError,
    ],
  );
  const lastAnswer = [...displayEntries]
    .reverse()
    .find((entry) => entry.kind === "assistant");
  const normalizedDraft = draft.trim();
  const running = Boolean(job && !isTerminalJob(job));
  const threadId = conversation?.id;
  const payment = observedJob?.payment ?? job?.payment;
  const title =
    saved?.title ??
    conversation?.title ??
    preview(job?.prompt || initialPrompt || "New Nanocodex Conversation");

  return (
    <List
      filtering={false}
      isLoading={historyLoading && displayEntries.length === 0}
      isShowingDetail
      navigationTitle="Ask Nanocodex"
      onSearchTextChange={setDraft}
      searchBarPlaceholder={
        running ? "Queue one follow-up..." : "Ask a follow-up..."
      }
      searchText={draft}
      selectedItemId={CHAT_ITEM_ID}
      throttle={false}
    >
      <List.Item
        id={CHAT_ITEM_ID}
        icon={Icon.Bubble}
        title={title}
        subtitle="Background JS/WASM · Codex session · Tempo MPP"
        detail={
          <List.Item.Detail
            markdown={markdown}
            metadata={
              <List.Item.Detail.Metadata>
                <List.Item.Detail.Metadata.Label title="Status" text={status} />
                <List.Item.Detail.Metadata.Label
                  title="Workspace"
                  text={conversation?.cwd ?? requestedWorkspace}
                />
                {threadId ? (
                  <List.Item.Detail.Metadata.Label
                    title="Thread"
                    text={threadId}
                  />
                ) : null}
                {job ? (
                  <List.Item.Detail.Metadata.Label
                    title="Background Job"
                    text={job.id}
                  />
                ) : null}
                {payment ? (
                  <List.Item.Detail.Metadata.Label
                    title="MPP Paid"
                    text={`${payment.cumulativePayment} pathUSD`}
                  />
                ) : null}
                {payment?.channelId ? (
                  <List.Item.Detail.Metadata.Label
                    title="MPP Channel"
                    text={payment.channelId}
                  />
                ) : null}
              </List.Item.Detail.Metadata>
            }
          />
        }
        actions={
          <ActionPanel>
            {normalizedDraft ? (
              <Action
                icon={Icon.ArrowRightCircle}
                title={running ? "Queue Follow-Up" : "Ask Nanocodex"}
                onAction={submit}
              />
            ) : lastAnswer ? (
              <Action.CopyToClipboard content={lastAnswer.text} />
            ) : null}
            {lastAnswer ? (
              <Action.Paste
                content={lastAnswer.text}
                title="Paste Last Answer"
              />
            ) : null}
            {job && !isTerminalJob(job) ? (
              <Action
                icon={Icon.Stop}
                title="Cancel Background Job"
                shortcut={CANCEL_SHORTCUT}
                onAction={cancel}
              />
            ) : null}
            {threadId ? (
              <Action.CopyToClipboard
                content={threadId}
                title="Copy Thread ID"
              />
            ) : null}
            {job ? (
              <Action.CopyToClipboard
                content={job.id}
                title="Copy Background Job ID"
              />
            ) : null}
          </ActionPanel>
        }
      />
    </List>
  );
}

function transcriptMarkdown(
  entries: readonly TerminalEntry[],
  status: string,
  truncatedHistory: boolean,
): string {
  if (!entries.length) return `# Nanocodex\n\n_${status}_`;
  const rendered: string[] = [];
  let characters = 0;
  let omitted = truncatedHistory;
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const entry = entries[index];
    if (!entry) continue;
    const markdown = entryMarkdown(entry);
    if (
      rendered.length &&
      characters + markdown.length > MAX_RENDERED_TRANSCRIPT_CHARACTERS
    ) {
      omitted = true;
      break;
    }
    rendered.push(markdown);
    characters += markdown.length;
  }
  rendered.reverse();
  return [
    omitted ? "_Earlier history omitted for a fast preview._" : undefined,
    ...rendered,
    status === "Ready" ? undefined : `---\n\n_${status}_`,
  ]
    .filter(Boolean)
    .join("\n\n");
}

function entryMarkdown(entry: TerminalEntry): string {
  switch (entry.kind) {
    case "user":
      return `## You\n\n${entry.text}`;
    case "reasoning":
      return `### Thinking${entry.streaming ? "..." : ""}\n\n${blockquote(entry.text)}`;
    case "assistant":
      return `## Nanocodex\n\n${entry.text || "_Writing..._"}`;
    case "tool": {
      const details = [
        entry.tool.arguments
          ? `\`\`\`text\n${safeFence(entry.tool.arguments)}\n\`\`\``
          : undefined,
        ...entry.tool.children.map(
          (child) =>
            `- **${child.name}** · ${child.status}${child.arguments ? ` · ${child.arguments}` : ""}`,
        ),
        entry.tool.result,
      ]
        .filter(Boolean)
        .join("\n\n");
      return `#### ${entry.tool.name === "exec" ? "Code Mode" : entry.tool.name} · ${entry.tool.status}${details ? `\n\n${details}` : ""}`;
    }
    case "plan":
      return [
        "### Plan",
        entry.update.explanation,
        entry.update.plan
          .map(
            ({ status, step }) =>
              `- [${status === "completed" ? "x" : " "}] ${status === "in_progress" ? "◐ " : ""}${step}`,
          )
          .join("\n"),
      ]
        .filter(Boolean)
        .join("\n\n");
    case "error":
      return `## Error\n\n${entry.text}`;
  }
}

function asSavedConversation(
  conversation: BackgroundConversation,
): SavedConversation {
  return {
    ...conversation,
    createdAt: new Date(conversation.createdAt),
    updatedAt: new Date(conversation.updatedAt),
  };
}

function blockquote(value: string): string {
  return value
    .split("\n")
    .map((line) => `> ${line}`)
    .join("\n");
}

function safeFence(value: string): string {
  return value.replaceAll("```", "``\\`");
}

function preview(value: string, limit = 88): string {
  const normalized = value.split(/\s+/).filter(Boolean).join(" ");
  const characters = [...normalized];
  return characters.length <= limit
    ? normalized
    : `${characters.slice(0, limit).join("")}...`;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
