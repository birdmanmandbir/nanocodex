"use client";

import {
  ArrowUpRight,
  Check,
  ChevronRight,
  Copy,
  GitBranch,
  GitPullRequest,
  Moon,
  Search,
  Sun,
  X,
} from "lucide-react";
import {
  Suspense,
  lazy,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useLocation, useNavigate } from "react-router";
import type { CodeBrowserHandle } from "./CodeBrowser";
import type { CommitCodeStreamHandle } from "./CommitCodeStream";
import { fuzzyScore } from "./fuzzy";
import { pathForSurface, surfaceFromUrl, type Surface } from "./navigation";

const Evals = lazy(() =>
  import("./Evals").then((module) => ({ default: module.Evals }))
);
const AgentTerminal = lazy(() =>
  import("./AgentTerminal").then((module) => ({
    default: module.AgentTerminal,
  }))
);
const loadPierreWorkerProvider = () => import("./PierreWorkerProvider");
const loadCodeBrowser = () => import("./CodeBrowser");
const loadCommitCodeStream = () => import("./CommitCodeStream");
const loadVirtualCommitList = () => import("./VirtualCommitList");
const PierreWorkerProvider = lazy(() =>
  loadPierreWorkerProvider().then((module) => ({
    default: module.PierreWorkerProvider,
  }))
);
const CodeBrowser = lazy(() =>
  loadCodeBrowser().then((module) => ({ default: module.CodeBrowser }))
);
const CommitCodeStream = lazy(() =>
  loadCommitCodeStream().then((module) => ({
    default: module.CommitCodeStream,
  }))
);
const VirtualCommitList = lazy(() =>
  loadVirtualCommitList().then((module) => ({
    default: module.VirtualCommitList,
  })),
);

export type Theme = "light" | "dark";
type Scope = "all" | "eval" | "fix" | "docs" | "perf";
type ProposalState = "ready" | "submitting" | "payment-required";
type RepositoryFile = {
  path: string;
  mode: string;
  objectId: string;
  size: number | null;
  contentUrl: string | null;
};

export type ChangedFile = {
  path: string;
  previousPath: string | null;
  status: string;
  additions: number | null;
  deletions: number | null;
};

export type HarnessCommit = {
  hash: string;
  shortHash: string;
  parents: string[];
  author: string;
  authoredAt: string;
  refs: string[];
  subject: string;
  body: string;
  files: ChangedFile[];
  stats: {
    files: number;
    additions: number;
    deletions: number;
  };
};

type RepositorySnapshot = {
  repository: {
    fullName: string;
    branch: string;
    head: string;
    totalCommits: number;
    indexedCommits?: number;
    commitPageSize?: number;
    dirty: boolean;
    dirtyCount: number;
  };
  generatedAt: string;
  tree: RepositoryFile[];
};

const emptyCommits: HarnessCommit[] = [];
let repositorySnapshotPromise: Promise<RepositorySnapshot> | undefined;
let repositorySnapshotLoadedAt = 0;
const repositoryCommitPagePromises = new Map<string, Promise<HarnessCommit[]>>();
const REPOSITORY_SNAPSHOT_TTL_MS = 60_000;

function loadRepositorySnapshot(refresh = false): Promise<RepositorySnapshot> {
  if (
    repositorySnapshotPromise &&
    (repositorySnapshotLoadedAt === 0 || !refresh || Date.now() - repositorySnapshotLoadedAt < REPOSITORY_SNAPSHOT_TTL_MS)
  ) return repositorySnapshotPromise;
  const url = import.meta.env.DEV
    ? "/__nanocodex/repository/snapshot"
    : "/api/repository/snapshot";
  const request = fetch(url, {
    cache: import.meta.env.DEV ? "no-store" : "default",
  }).then(async (response) => {
    if (!response.ok) {
      throw new Error(`Repository request failed (${response.status})`);
    }
    const snapshot = await response.json() as RepositorySnapshot;
    const generation = response.headers.get("x-repository-generation");
    if (generation && generation !== snapshot.repository.head) {
      throw new Error("Repository snapshot generation mismatch");
    }
    repositorySnapshotLoadedAt = Date.now();
    return snapshot;
  }).catch((error) => {
    if (repositorySnapshotPromise === request) repositorySnapshotPromise = undefined;
    throw error;
  });
  repositorySnapshotPromise = request;
  return request;
}

function loadRepositoryCommitPage(
  generation: string,
  page: number,
): Promise<HarnessCommit[]> {
  const cacheKey = `${generation}:${page}`;
  const existing = repositoryCommitPagePromises.get(cacheKey);
  if (existing) return existing;
  const base = import.meta.env.DEV
    ? "/__nanocodex/repository/commits"
    : "/api/repository/commits";
  const url = `${base}?page=${page}&generation=${generation}`;
  const request = fetch(url, {
    cache: import.meta.env.DEV ? "no-store" : "default",
  }).then((response) => {
    if (!response.ok) {
      throw new Error(`Commit page request failed (${response.status})`);
    }
    const responseGeneration = response.headers.get("x-repository-generation");
    if (!import.meta.env.DEV && responseGeneration !== generation) {
      throw new Error("Commit page generation mismatch");
    }
    return response.json() as Promise<HarnessCommit[]>;
  }).catch((error) => {
    repositoryCommitPagePromises.delete(cacheKey);
    throw error;
  });
  repositoryCommitPagePromises.set(cacheKey, request);
  return request;
}

function preloadCodeSurface() {
  void loadRepositorySnapshot();
  void loadPierreWorkerProvider();
  void loadCodeBrowser();
}

function preloadCommitSurface() {
  void loadRepositorySnapshot().then((snapshot) =>
    loadRepositoryCommitPage(snapshot.repository.head, 0)
  );
  void loadPierreWorkerProvider();
  void loadCommitCodeStream();
  void loadVirtualCommitList();
}

const scopes: Array<{ id: Scope; label: string }> = [
  { id: "all", label: "All commits" },
  { id: "eval", label: "Eval" },
  { id: "fix", label: "Fix" },
  { id: "docs", label: "Docs" },
  { id: "perf", label: "Perf" },
];

function subjectScope(subject: string) {
  const prefix = subject.split(":", 1)[0].toLowerCase();
  return scopes.some(({ id }) => id === prefix) ? (prefix as Scope) : "other";
}

function commitSearchScore(commit: HarnessCommit, tokens: readonly string[]) {
  if (!tokens.length) return 0;
  const fields = [
    { value: commit.hash, weight: 160 },
    { value: commit.subject, weight: 120 },
    { value: commit.author, weight: 60 },
    { value: commit.body, weight: 30 },
    ...commit.files.map((file) => ({ value: file.path, weight: 90 })),
  ];

  let total = 0;
  for (const token of tokens) {
    const best = fields.reduce<number | null>((current, field) => {
      const score = fuzzyScore(field.value, token);
      if (score === null) return current;
      const weighted = score + field.weight;
      return current === null || weighted > current ? weighted : current;
    }, null);
    if (best === null) return null;
    total += best;
  }
  return total;
}

const installCommand =
  "curl -fsSL https://nanocodex.paradigm.xyz | bash";

function RepositorySurfaceError() {
  return (
    <section className="requests-empty page-grid" role="alert">
      <GitBranch aria-hidden="true" />
      <p className="eyebrow">Repository</p>
      <h1>Repository data unavailable.</h1>
    </section>
  );
}

export function NanocodexApp() {
  return <NanocodexShell />;
}

function NanocodexShell() {
  const location = useLocation();
  const navigate = useNavigate();
  const [theme, setTheme] = useState<Theme>(() => {
    const initialTheme = document.documentElement.dataset.theme;
    if (initialTheme === "dark" || initialTheme === "light")
      return initialTheme;
    const stored =
      localStorage.getItem("nanocodex-theme");
    return stored === "dark" ? "dark" : "light";
  });
  const surface = surfaceFromUrl({
    pathname: location.pathname,
    searchParams: new URLSearchParams(location.search),
  });
  const [snapshot, setSnapshot] = useState<RepositorySnapshot>();
  const [commits, setCommits] = useState<HarnessCommit[]>(emptyCommits);
  const [commitsLoaded, setCommitsLoaded] = useState(false);
  const [repositoryLoadError, setRepositoryLoadError] = useState(false);
  const [scope, setScope] = useState<Scope>("all");
  const [query, setQuery] = useState("");
  const [searchOpen, setSearchOpen] = useState(false);
  const [selectedHash, setSelectedHash] = useState<string>();
  const [proposalOpen, setProposalOpen] = useState(false);
  const [proposalState, setProposalState] = useState<ProposalState>("ready");
  const [proposalTitle, setProposalTitle] = useState("");
  const [commitRailOpen, setCommitRailOpen] = useState(false);
  const [installCopied, setInstallCopied] = useState(false);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const headerCenterRef = useRef<HTMLDivElement>(null);
  const codeBrowserRef = useRef<CodeBrowserHandle>(null);
  const commitStreamRef = useRef<CommitCodeStreamHandle>(null);
  const nextCommitPageRef = useRef(0);
  const commitPageLoadRef = useRef<Promise<boolean> | null>(null);
  const repositoryGenerationRef = useRef<string | undefined>(undefined);
  const indexedCommitCount = snapshot?.repository.indexedCommits ??
    snapshot?.repository.totalCommits ?? commits.length;

  const selected = useMemo(
    () =>
      commits.find((commit) => commit.hash === selectedHash) ??
      commits[0] ??
      null,
    [commits, selectedHash],
  );
  const scopeCounts = useMemo(
    () =>
      commits.reduce<Record<Scope, number>>(
        (counts, commit) => {
          const commitScope = subjectScope(commit.subject);
          if (commitScope !== "other") counts[commitScope] += 1;
          return counts;
        },
        {
          all: commits.length,
          eval: 0,
          fix: 0,
          docs: 0,
          perf: 0,
        },
      ),
    [commits],
  );
  const queryTokens = useMemo(
    () => query.trim().toLowerCase().split(/\s+/).filter(Boolean),
    [query],
  );

  const filteredCommits = useMemo(() => {
    const scoped = commits.filter(
      (commit) => scope === "all" || subjectScope(commit.subject) === scope,
    );
    if (!queryTokens.length) return scoped;
    return scoped
      .map((commit) => ({
        commit,
        score: commitSearchScore(commit, queryTokens),
      }))
      .filter(
        (match): match is { commit: HarnessCommit; score: number } =>
          match.score !== null,
      )
      .sort((left, right) => right.score - left.score)
      .map((match) => match.commit);
  }, [commits, queryTokens, scope]);

  const searchResults = useMemo(
    () => {
      if (!searchOpen) return [];
      return commits
        .map((commit) => ({
          commit,
          score: commitSearchScore(commit, queryTokens),
        }))
        .filter(
          (match): match is { commit: HarnessCommit; score: number } =>
            match.score !== null,
        )
        .sort((left, right) => right.score - left.score)
        .slice(0, 12)
        .map((match) => match.commit);
    },
    [commits, queryTokens, searchOpen],
  );

  useEffect(() => {
    const needsRepository =
      surface === "code" || surface === "commits" || proposalOpen;
    if (!needsRepository || snapshot) return;
    let active = true;
    setRepositoryLoadError(false);
    void loadRepositorySnapshot().then(
      (loaded) => {
        if (!active) return;
        repositoryGenerationRef.current = loaded.repository.head;
        setSnapshot(loaded);
        setSelectedHash((current) => current ?? loaded.repository.head);
      },
      () => {
        if (active) setRepositoryLoadError(true);
      },
    );
    return () => {
      active = false;
    };
  }, [proposalOpen, snapshot, surface]);

  useEffect(() => {
    if (surface === "code") preloadCodeSurface();
    if (surface === "commits") preloadCommitSurface();
  }, [surface]);

  useEffect(() => {
    if (surface !== "home") return;
    const preload = () => {
      preloadCodeSurface();
      preloadCommitSurface();
    };
    if ("requestIdleCallback" in window) {
      const id = window.requestIdleCallback(preload, { timeout: 1_500 });
      return () => window.cancelIdleCallback(id);
    }
    const id = setTimeout(preload, 1_000);
    return () => clearTimeout(id);
  }, [surface]);

  useEffect(() => {
    const needsCommits = surface === "commits" || proposalOpen;
    if (!needsCommits || commitsLoaded) return;
    let active = true;
    setRepositoryLoadError(false);
    if (!snapshot) return;
    repositoryGenerationRef.current = snapshot.repository.head;
    void loadRepositoryCommitPage(snapshot.repository.head, 0).then(
      (loaded) => {
        if (!active) return;
        setCommits(loaded);
        nextCommitPageRef.current = 1;
        setCommitsLoaded(true);
      },
      () => {
        if (active) setRepositoryLoadError(true);
      },
    );
    return () => {
      active = false;
    };
  }, [commitsLoaded, proposalOpen, snapshot, surface]);

  const loadNextCommitPage = useCallback((): Promise<boolean> => {
    if (
      snapshot == null ||
      commits.length >= indexedCommitCount
    ) {
      return Promise.resolve(false);
    }
    if (commitPageLoadRef.current != null) return commitPageLoadRef.current;
    const page = nextCommitPageRef.current;
    const generation = snapshot.repository.head;
    const request = loadRepositoryCommitPage(generation, page)
      .then((loaded) => {
        if (repositoryGenerationRef.current !== generation) return false;
        if (loaded.length === 0) return false;
        setCommits((current) => {
          const known = new Set(current.map((commit) => commit.hash));
          return [...current, ...loaded.filter((commit) => !known.has(commit.hash))];
        });
        nextCommitPageRef.current = page + 1;
        return true;
      })
      .finally(() => {
        commitPageLoadRef.current = null;
      });
    commitPageLoadRef.current = request;
    return request;
  }, [commits.length, indexedCommitCount, snapshot]);

  useEffect(() => {
    const needsRepository = surface === "code" || surface === "commits" || proposalOpen;
    if (!needsRepository || !snapshot) return;
    let active = true;
    const refresh = () => {
      void loadRepositorySnapshot(true).then(async (loaded) => {
        if (!active || loaded.repository.head === repositoryGenerationRef.current) return;
        const firstPage = commitsLoaded || surface === "commits" || proposalOpen
          ? await loadRepositoryCommitPage(loaded.repository.head, 0)
          : undefined;
        if (!active) return;
        repositoryGenerationRef.current = loaded.repository.head;
        commitPageLoadRef.current = null;
        nextCommitPageRef.current = firstPage ? 1 : 0;
        setSnapshot(loaded);
        if (firstPage) {
          setCommits(firstPage);
          setCommitsLoaded(true);
        }
      }).catch((error) => {
        console.warn("Failed to refresh repository publication", error);
      });
    };
    const interval = window.setInterval(refresh, REPOSITORY_SNAPSHOT_TTL_MS);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [commitsLoaded, proposalOpen, snapshot, surface]);

  useEffect(() => {
    const filtering = scope !== "all" || queryTokens.length > 0;
    if (!filtering || commits.length >= indexedCommitCount) return;
    void loadNextCommitPage().catch((error) => {
      console.warn("Failed to complete commit metadata for filtering", error);
    });
  }, [commits.length, indexedCommitCount, loadNextCommitPage, queryTokens.length, scope]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document
      .querySelector('meta[name="theme-color"]')
      ?.setAttribute("content", theme === "dark" ? "#161616" : "#ffffff");
    localStorage.setItem("nanocodex-theme", theme);
  }, [theme]);

  const navigateToSurface = useCallback((nextSurface: Surface) => {
    navigate(pathForSurface(nextSurface));
  }, [navigate]);

  useLayoutEffect(() => {
    const headerCenter = headerCenterRef.current;
    const activeButton =
      headerCenter?.querySelector<HTMLButtonElement>(".is-active");
    if (
      !headerCenter ||
      !activeButton ||
      headerCenter.scrollWidth <= headerCenter.clientWidth
    )
      return;
    headerCenter.scrollLeft =
      activeButton.offsetLeft -
      (headerCenter.clientWidth - activeButton.offsetWidth) / 2;
  }, [surface]);

  useEffect(() => {
    if (searchOpen)
      requestAnimationFrame(() => searchInputRef.current?.focus());
  }, [searchOpen]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const originalTarget = event.composedPath()[0];
      const target =
        originalTarget instanceof HTMLElement
          ? originalTarget
          : (event.target as HTMLElement | null);
      const isTyping = target?.matches(
        "input, textarea, [contenteditable='true']"
      );
      const primaryModifier = event.ctrlKey || event.metaKey;
      const key = event.key.toLowerCase();

      if (
        surface === "code" &&
        primaryModifier &&
        !event.altKey &&
        key === "p"
      ) {
        event.preventDefault();
        event.stopPropagation();
        codeBrowserRef.current?.openTreeSearch();
        return;
      }
      if (
        surface === "code" &&
        primaryModifier &&
        !event.altKey &&
        key === "f"
      ) {
        event.preventDefault();
        event.stopPropagation();
        codeBrowserRef.current?.openFileSearch();
        return;
      }

      if (event.key === "Escape") {
        setSearchOpen(false);
        setProposalOpen(false);
        setCommitRailOpen(false);
        codeBrowserRef.current?.closeSearches();
        return;
      }
      if (isTyping || primaryModifier || event.altKey) return;
      if (key === "f") {
        if (surface !== "commits") return;
        event.preventDefault();
        event.stopPropagation();
        setSearchOpen(true);
        return;
      }
      if (key === "m") {
        event.preventDefault();
        event.stopPropagation();
        setTheme((current) => (current === "light" ? "dark" : "light"));
        return;
      }
      if (key === "p") {
        event.preventDefault();
        event.stopPropagation();
        setProposalState("ready");
        setProposalOpen(true);
        return;
      }
      const nextSurface =
        key === "h"
          ? "home"
          : key === "t"
          ? "code"
          : key === "c"
          ? "commits"
          : key === "r"
          ? "requests"
          : key === "e"
          ? "evals"
          : null;
      if (nextSurface) {
        event.preventDefault();
        event.stopPropagation();
        target?.blur();
        navigateToSurface(nextSurface);
      }
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    return () =>
      window.removeEventListener("keydown", onKeyDown, { capture: true });
  }, [navigateToSurface, surface]);

  const selectCommit = (commit: HarnessCommit) => {
    setSelectedHash(commit.hash);
    const index = commits.findIndex((candidate) => candidate.hash === commit.hash);
    if (index >= 0) commitStreamRef.current?.scrollToCommit(index);
    setSearchOpen(false);
    setCommitRailOpen(false);
    setQuery("");
  };

  const submitProposal = async () => {
    if (!snapshot || !selected) return;
    setProposalState("submitting");
    try {
      await fetch("/api/proposals", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          repository: snapshot.repository.fullName,
          base: selected.hash,
          title: proposalTitle || "Untitled proposal",
        }),
      });
    } finally {
      setProposalState("payment-required");
    }
  };

  return (
    <div className={`site-shell surface-${surface}`}>
        <header className="site-header">
          <a
            className="wordmark"
            href="/"
            aria-label="nanocodex home"
            onClick={(event) => {
              event.preventDefault();
              navigateToSurface("home");
            }}
          >
            nanocodex <span>[H]</span>
          </a>
          <div className="header-center" ref={headerCenterRef}>
            <nav className="surface-switch" aria-label="Repository surfaces">
              <a
                className={surface === "code" ? "is-active" : ""}
                href={pathForSurface("code")}
                onPointerEnter={preloadCodeSurface}
                onFocus={preloadCodeSurface}
                onClick={(event) => {
                  event.preventDefault();
                  navigateToSurface("code");
                }}
              >
                Code <span>[T]</span>
              </a>
              <a
                className={surface === "commits" ? "is-active" : ""}
                href={pathForSurface("commits")}
                onPointerEnter={preloadCommitSurface}
                onFocus={preloadCommitSurface}
                onClick={(event) => {
                  event.preventDefault();
                  navigateToSurface("commits");
                }}
              >
                Commits <span>[C]</span>
              </a>
              <a
                className={surface === "requests" ? "is-active" : ""}
                href={pathForSurface("requests")}
                onClick={(event) => {
                  event.preventDefault();
                  navigateToSurface("requests");
                }}
              >
                Requests <span>[R]</span>
              </a>
              <a
                className={surface === "evals" ? "is-active" : ""}
                href={pathForSurface("evals")}
                onClick={(event) => {
                  event.preventDefault();
                  navigateToSurface("evals");
                }}
              >
                Evals <span>[E]</span>
              </a>
            </nav>
          </div>
          <nav className="header-actions" aria-label="Site actions">
            <button
              className="text-action"
              type="button"
              onClick={() =>
                setTheme((current) => (current === "light" ? "dark" : "light"))
              }
              aria-label={`Use ${theme === "light" ? "dark" : "light"} theme`}
            >
              {theme === "light" ? (
                <Moon aria-hidden="true" />
              ) : (
                <Sun aria-hidden="true" />
              )}
              <span>Theme</span>
              <span className="keycap">[M]</span>
            </button>
            <button
              className="button button--medium header-propose"
              type="button"
              onClick={() => {
                setProposalState("ready");
                setProposalOpen(true);
              }}
            >
              Propose <span>[P]</span>
            </button>
          </nav>
        </header>

        <main id="top">
          {surface === "home" ? (
            <section className="home-page" aria-labelledby="home-title">
                <article className="home-article">
                  <header className="home-intro">
                    <h1 id="home-title">
                      Nanocodex: a headless Codex runtime you can embed.
                    </h1>
                    <p>
                      Nanocodex packages Codex lifecycle and context management
                      behind an ergonomic Rust API, so you can bring the agent
                      loop into your own applications without bringing along an
                      entire product.
                    </p>
                  </header>

                  <div className="agent-demo-slot">
                    <Suspense fallback={null}>
                      <AgentTerminal />
                    </Suspense>
                  </div>

                  <section
                    className="home-release-section"
                    aria-labelledby="home-release-title"
                  >
                    <div className="home-release-heading">
                      <div>
                        <p className="eyebrow">Install the CLI</p>
                        <h2 id="home-release-title">One binary. Kept current.</h2>
                      </div>
                      <a
                        href="https://github.com/gakonst/nanocodex/releases/latest"
                        target="_blank"
                        rel="noreferrer"
                      >
                        Latest release <ArrowUpRight aria-hidden="true" />
                      </a>
                    </div>
                    <div className="home-install-command">
                      <code>{installCommand}</code>
                      <button
                        type="button"
                        aria-label="Copy install command"
                        onClick={() => {
                          void navigator.clipboard
                            .writeText(installCommand)
                            .then(() => {
                              setInstallCopied(true);
                              window.setTimeout(
                                () => setInstallCopied(false),
                                1_500
                              );
                            });
                        }}
                      >
                        {installCopied ? (
                          <Check aria-hidden="true" />
                        ) : (
                          <Copy aria-hidden="true" />
                        )}
                        {installCopied ? "Copied" : "Copy"}
                      </button>
                    </div>
                    <div className="home-release-grid">
                      <article>
                        <span>Update</span>
                        <code>nanocodex update</code>
                        <p>
                          Downloads the host binary, verifies its SHA-256, and
                          replaces the current executable.
                        </p>
                      </article>
                      <article>
                        <span>Embed</span>
                        <code>cargo add nanocodex</code>
                        <p>
                          All seven public Rust crates ship together under one
                          version in dependency order.
                        </p>
                      </article>
                      <article>
                        <span>Inspect</span>
                        <a href="https://github.com/gakonst/nanocodex/blob/master/CHANGELOG.md">
                          Release changelog <ArrowUpRight aria-hidden="true" />
                        </a>
                        <p>
                          Conventional commits are grouped in full; GitHub
                          release notes credit every pull request contributor.
                        </p>
                      </article>
                    </div>
                  </section>

                  <div className="home-copy">
                    <section>
                      <h2>A library, not a product</h2>
                      <ul>
                        <li>
                          Codex is a complete coding agent. Nanocodex isolates
                          the reusable engine: model lifecycle, conversation
                          context &amp; the exact tool boundary.
                        </li>
                        <li>
                          The core intentionally does not ship subagents, skills
                          or on-disk history. Your application owns orchestration,
                          persistence &amp; product policy.
                        </li>
                        <li>
                          The library-first architecture is designed for native
                          Rust applications, WebAssembly in the browser &amp;
                          Python bindings through PyO3.
                        </li>
                      </ul>
                    </section>

                    <section>
                      <h2>What we optimize for</h2>
                      <ul>
                        <li>
                          Production-grade Rust patterns. Tower middleware keeps
                          cross-cutting behavior composable &amp; application-specific
                          extensions straightforward.
                        </li>
                        <li>
                          Conversation forks as a first-class primitive, so
                          branching from shared context is fast &amp; efficient.
                        </li>
                        <li>
                          Eval-driven development. Every vertical slice runs on
                          real tasks, with failures traced through the verifier
                          &amp; complete tool trajectory.
                        </li>
                      </ul>
                    </section>
                  </div>

                </article>
            </section>
          ) : surface === "code" ? snapshot ? (
            <Suspense fallback={null}>
              <PierreWorkerProvider>
                <CodeBrowser
                  ref={codeBrowserRef}
                  files={snapshot.tree}
                  branch={snapshot.repository.branch}
                  head={snapshot.repository.head}
                  theme={theme}
                />
              </PierreWorkerProvider>
            </Suspense>
          ) : (
            repositoryLoadError ? <RepositorySurfaceError /> : null
          ) : surface === "commits" ? snapshot && commitsLoaded ? (
            <Suspense fallback={null}>
              <PierreWorkerProvider>
                <section
                  className="commits-workspace"
                  aria-label="Repository commits"
                >
                <button
                  className={
                    commitRailOpen
                      ? "workspace-backdrop is-visible"
                      : "workspace-backdrop"
                  }
                  type="button"
                  aria-label="Close commit list"
                  onClick={() => setCommitRailOpen(false)}
                />
                <aside
                  className={
                    commitRailOpen
                      ? "commit-sidebar is-mobile-open"
                      : "commit-sidebar"
                  }
                  aria-labelledby="history-title"
                >
                  <header className="commit-sidebar-header">
                    <div>
                      <strong id="history-title">Jump to commit</strong>
                      <span>
                        <GitBranch aria-hidden="true" />{" "}
                        {snapshot.repository.branch} · {indexedCommitCount}
                      </span>
                    </div>
                    <nav
                      className="commit-sidebar-actions"
                      aria-label="Commit index actions"
                    >
                      <button
                        className="icon-button"
                        type="button"
                        onClick={() => setSearchOpen(true)}
                      >
                        <Search aria-hidden="true" />
                        <span className="sr-only">Find commits</span>
                        <kbd>F</kbd>
                      </button>
                      <button
                        className="mobile-drawer-close"
                        type="button"
                        onClick={() => setCommitRailOpen(false)}
                        aria-label="Close commit index"
                      >
                        <X aria-hidden="true" />
                      </button>
                    </nav>
                  </header>

                  <nav
                    className="commit-scope-tabs"
                    aria-label="Quick jump scopes"
                  >
                    {scopes.map((item) => (
                      <button
                        className={scope === item.id ? "is-active" : ""}
                        type="button"
                        key={item.id}
                        onClick={() => setScope(item.id)}
                      >
                      {item.label}{" "}
                      <span>
                        {item.id === "all"
                          ? indexedCommitCount
                          : scopeCounts[item.id]}
                      </span>
                      </button>
                    ))}
                  </nav>

                  {query ? (
                    <div className="commit-query">
                      <span>
                        {filteredCommits.length} matches for “{query}”
                      </span>
                      <button
                        type="button"
                        onClick={() => setQuery("")}
                        aria-label="Clear commit search"
                      >
                        <X aria-hidden="true" />
                      </button>
                    </div>
                  ) : null}

                  <Suspense fallback={null}>
                    <VirtualCommitList
                      commits={filteredCommits}
                      hasMore={
                        queryTokens.length === 0 &&
                        scope === "all" &&
                        commits.length < indexedCommitCount
                      }
                      selectedHash={selected?.hash}
                      onClearSearch={() => setQuery("")}
                      onLoadMore={loadNextCommitPage}
                      onSelectCommit={selectCommit}
                    />
                  </Suspense>
                </aside>
                {commits.length > 0 ? (
                <Suspense fallback={null}>
                  <CommitCodeStream
                    ref={commitStreamRef}
                    commits={commits}
                    hasMoreCommits={commits.length < indexedCommitCount}
                    onOpenCommitRail={() => setCommitRailOpen(true)}
                    onLoadMoreCommits={loadNextCommitPage}
                    theme={theme}
                    totalCommitCount={indexedCommitCount}
                  />
                </Suspense>
                ) : null}
                </section>
              </PierreWorkerProvider>
            </Suspense>
          ) : (
            repositoryLoadError ? <RepositorySurfaceError /> : null
          ) : surface === "requests" ? (
            <section
              className="requests-empty page-grid"
              aria-labelledby="requests-title"
            >
              <GitPullRequest aria-hidden="true" />
              <p className="eyebrow">Requests</p>
              <h1 id="requests-title">No requests yet.</h1>
              <p>
                This view is reserved for proposed changes. We’ll leave it quiet
                for now.
              </p>
            </section>
          ) : (
            <Suspense fallback={null}>
              <Evals />
            </Suspense>
          )}
        </main>

        {searchOpen && surface === "commits" ? (
          <div
            className="overlay"
            role="presentation"
            onMouseDown={() => setSearchOpen(false)}
          >
            <section
              className="search-dialog"
              role="dialog"
              aria-modal="true"
              aria-label="Find commits"
              onMouseDown={(event) => event.stopPropagation()}
            >
              <div className="search-field">
                <Search aria-hidden="true" />
                <input
                  ref={searchInputRef}
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  placeholder="Search hashes, messages, authors, and paths"
                />
                <button
                  type="button"
                  onClick={() => setSearchOpen(false)}
                  aria-label="Close search"
                >
                  <X aria-hidden="true" />
                </button>
              </div>
              <div className="search-results">
                {searchResults.length ? (
                  searchResults.map((commit, index) => (
                    <button
                      className={
                        index === 0 ? "search-result is-first" : "search-result"
                      }
                      type="button"
                      key={commit.hash}
                      onClick={() => selectCommit(commit)}
                    >
                      <span>{commit.shortHash}</span>
                      <strong>{commit.subject}</strong>
                      <small>{commit.author}</small>
                      <ChevronRight aria-hidden="true" />
                    </button>
                  ))
                ) : (
                  <p className="search-empty">No commits found.</p>
                )}
              </div>
              <footer className="search-footer">
                <span>{searchResults.length} results</span>
                <span>Esc to close</span>
              </footer>
            </section>
          </div>
        ) : null}

        {proposalOpen ? (
          <div
            className="overlay"
            role="presentation"
            onMouseDown={() => setProposalOpen(false)}
          >
            <section
              className="proposal-dialog"
              role="dialog"
              aria-modal="true"
              aria-labelledby="proposal-title"
              onMouseDown={(event) => event.stopPropagation()}
            >
              <button
                className="dialog-close"
                type="button"
                onClick={() => setProposalOpen(false)}
              >
                <X aria-hidden="true" /> <span className="sr-only">Close</span>
              </button>
              <p className="eyebrow">MPP proposal gate · testnet preview</p>
              <h2 id="proposal-title">Propose a change</h2>
              {!snapshot || !selected ? (
                repositoryLoadError ? (
                  <p className="proposal-intro" role="alert">
                    Repository data is unavailable.
                  </p>
                ) : null
              ) : proposalState === "payment-required" ? (
                <div className="payment-required">
                  <div className="payment-mark">402</div>
                  <h3>Payment challenge ready</h3>
                  <p>
                    The Worker returned the preview MPP challenge. No funds
                    moved; a live recipient and settlement policy still need to
                    be configured.
                  </p>
                  <button
                    className="button button--high"
                    type="button"
                    onClick={() => setProposalOpen(false)}
                  >
                    Done
                  </button>
                </div>
              ) : (
                <>
                  <p className="proposal-intro">
                    Submit a patch against <strong>{selected.shortHash}</strong>
                    . The $0.20 proposal fee is a rate limit, not access to the
                    repository.
                  </p>
                  <label>
                    Proposal title
                    <input
                      value={proposalTitle}
                      onChange={(event) => setProposalTitle(event.target.value)}
                      placeholder="What should change?"
                    />
                  </label>
                  <div className="proposal-summary">
                    <div>
                      <span>Repository</span>
                      <strong>nanocodex</strong>
                    </div>
                    <div>
                      <span>Base</span>
                      <strong>{selected.shortHash}</strong>
                    </div>
                    <div>
                      <span>Preview fee</span>
                      <strong>$0.20</strong>
                    </div>
                  </div>
                  <button
                    className="button button--high proposal-submit"
                    type="button"
                    disabled={proposalState === "submitting"}
                    onClick={submitProposal}
                  >
                    Continue to payment
                    <ArrowUpRight aria-hidden="true" />
                  </button>
                </>
              )}
            </section>
          </div>
        ) : null}
    </div>
  );
}
