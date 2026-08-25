/// <reference lib="DOM" />
/// <reference lib="DOM.Iterable" />

import {
  type FormEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

type JobStatus = "building" | "completed" | "failed";
export type TeamRole = "owner" | "member";
export type Workspace = "personal" | `team:${string}`;

const APP_GRANTS = Object.freeze([
  "profile:read",
  "state:read",
  "state:write",
  "ai:generate",
  "agents:run",
]);

export type BuildJob = Readonly<{
  id: string;
  app_id: string;
  status: JobStatus;
  revision: string | null;
  error: string | null;
  created_at: string;
  completed_at: string | null;
}>;

type SourceFile = Readonly<{ path: string; bytes?: number }>;

export type AppRevision = Readonly<{
  id: string;
  source_commit: string;
  created_at: string;
  artifact_bytes: number;
  generation_model: string;
  source_summary: Readonly<{
    entryPoint: string;
    files: readonly (SourceFile | string)[];
  }>;
}>;

export type GeneratedApp = Readonly<{
  id: string;
  display_name: string;
  slug: string;
  live_slug: string;
  active_revision: string;
  created_at: string;
  grants: readonly string[];
  updated_at: string;
  revisions: readonly AppRevision[];
}>;

type Team = Readonly<{
  id: string;
  name: string;
  role: TeamRole;
  created_at: number;
}>;

type TeamMember = Readonly<{
  user_id: string;
  role: TeamRole;
  joined_at: number;
}>;

type MeResponse = Readonly<{
  user: Readonly<{ id: string; persistent: boolean }>;
  teams: readonly Team[];
}>;

type AppsResponse = Readonly<{
  apps: readonly GeneratedApp[];
  tenant: Readonly<{ id: string; kind: "personal" | "team"; role: TeamRole }>;
}>;

type JobResponse = Readonly<{ job: BuildJob }>;
type ActivateResponse = Readonly<{ app: GeneratedApp }>;
type LaunchResponse = Readonly<{ launch_url: string }>;
type TeamResponse = Readonly<{ team: Team; replayed?: boolean }>;
type MembersResponse = Readonly<{ data: readonly TeamMember[] }>;
type InvitationResponse = Readonly<{
  invitation: string;
  expires_at: number;
  role: TeamRole;
}>;

type TrackedJob = BuildJob & Readonly<{
  requestedPrompt: string;
  updateAppId?: string;
}>;

type Notice = Readonly<{ kind: "error" | "success"; message: string }>;
type FreshInvitation = Readonly<{
  teamId: string;
  token: string;
  role: TeamRole;
  expiresAt: number;
}>;

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string | undefined,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export function workspacePath(path: string, workspace: Workspace): string {
  if (new URL(path, "https://apps.invalid").searchParams.has("workspace")) {
    throw new Error("workspace selector is already present");
  }
  const separator = path.includes("?") ? "&" : "?";
  return `${path}${separator}${new URLSearchParams({ workspace }).toString()}`;
}

export function appLaunchPath(appId: string, workspace: Workspace): string {
  return workspacePath(`/apps/api/apps/${encodeURIComponent(appId)}/launch`, workspace);
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "Unknown size";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"] as const;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`;
}

export function hasCompletedBuild(responses: readonly JobResponse[]): boolean {
  return responses.some(({ job }) => job.status === "completed");
}

export function AppsConsole() {
  const [teams, setTeams] = useState<readonly Team[]>([]);
  const [accountUserId, setAccountUserId] = useState<string>();
  const [accountFailure, setAccountFailure] = useState<string>();
  const [selectedWorkspace, setSelectedWorkspace] = useState<Workspace>("personal");
  const selectedWorkspaceRef = useRef<Workspace>("personal");
  const [appSnapshots, setAppSnapshots] = useState<Partial<Record<Workspace, readonly GeneratedApp[]>>>({});
  const [appsFailure, setAppsFailure] = useState<string>();
  const [notice, setNotice] = useState<Notice>();
  const [teamNotice, setTeamNotice] = useState<Notice>();
  const [jobs, setJobs] = useState<readonly TrackedJob[]>([]);
  const [createPrompt, setCreatePrompt] = useState("");
  const [createApproved, setCreateApproved] = useState(false);
  const [updatePrompts, setUpdatePrompts] = useState<Record<string, string>>({});
  const [creating, setCreating] = useState(false);
  const [updatingAppIds, setUpdatingAppIds] = useState<ReadonlySet<string>>(new Set());
  const [rollbackKeys, setRollbackKeys] = useState<ReadonlySet<string>>(new Set());
  const [teamName, setTeamName] = useState("");
  const [creatingTeam, setCreatingTeam] = useState(false);
  const [inviteRole, setInviteRole] = useState<TeamRole>("member");
  const [creatingInvitation, setCreatingInvitation] = useState(false);
  const [freshInvitation, setFreshInvitation] = useState<FreshInvitation>();
  const [invitationInput, setInvitationInput] = useState("");
  const [acceptingInvitation, setAcceptingInvitation] = useState(false);
  const [membersByTeam, setMembersByTeam] = useState<Record<string, readonly TeamMember[]>>({});
  const [membersFailure, setMembersFailure] = useState<string>();
  const [removingMembers, setRemovingMembers] = useState<ReadonlySet<string>>(new Set());

  const selectedTeam = selectedWorkspace === "personal"
    ? undefined
    : teams.find((team) => `team:${team.id}` === selectedWorkspace);
  const canManage = selectedWorkspace === "personal" || selectedTeam?.role === "owner";
  const apps = appSnapshots[selectedWorkspace];

  const refreshAccount = useCallback(async () => {
    try {
      const response = await requestJson<MeResponse>("/v1/me");
      if (!response.user?.persistent || !Array.isArray(response.teams)) {
        throw new Error("A persistent Nanocodex account is required to use the app studio.");
      }
      setAccountUserId(response.user.id);
      setTeams(response.teams);
      setAccountFailure(undefined);
      const current = selectedWorkspaceRef.current;
      if (current !== "personal" && !response.teams.some((team) => `team:${team.id}` === current)) {
        selectWorkspace("personal");
      }
    } catch (error) {
      setAccountFailure(actionableError(error, "Your teams could not be refreshed."));
    }
  }, []);

  const handleWorkspaceAuthorityLoss = useCallback(async (error: unknown, workspace: Workspace) => {
    if (workspace === "personal" || !(error instanceof ApiError)
      || (error.status !== 401 && error.status !== 403 && error.status !== 404)) return false;
    const teamId = workspace.slice("team:".length);
    setAppSnapshots((current) => {
      const next = { ...current };
      delete next[workspace];
      return next;
    });
    setMembersByTeam((current) => {
      const next = { ...current };
      delete next[teamId];
      return next;
    });
    if (selectedWorkspaceRef.current === workspace) selectWorkspace("personal");
    await refreshAccount();
    setTeamNotice({
      kind: "error",
      message: "Your access to that team changed. Its cached app data was cleared and the workspace list was refreshed.",
    });
    return true;
  }, [refreshAccount]);

  const refreshApps = useCallback(async (workspace: Workspace) => {
    try {
      const response = await requestJson<AppsResponse>(workspacePath("/apps/api/apps", workspace));
      const expectedKind = workspace === "personal" ? "personal" : "team";
      const expectedTeamTenant = workspace === "personal" || response.tenant?.id === workspace;
      if (!Array.isArray(response.apps) || response.tenant?.kind !== expectedKind || !expectedTeamTenant) {
        throw new Error("The app platform returned an unexpected workspace.");
      }
      setAppSnapshots((current) => ({ ...current, [workspace]: response.apps }));
      if (selectedWorkspaceRef.current === workspace) setAppsFailure(undefined);
      return true;
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, workspace)) return false;
      if (selectedWorkspaceRef.current === workspace) {
        setAppsFailure(actionableError(error, "The app list could not be refreshed."));
      }
      return false;
    }
  }, [handleWorkspaceAuthorityLoss]);

  const refreshMembers = useCallback(async (teamId: string) => {
    try {
      const response = await requestJson<MembersResponse>(`/v1/teams/${encodeURIComponent(teamId)}/members`);
      if (!Array.isArray(response.data)) throw new Error("The team returned an unexpected member list.");
      setMembersByTeam((current) => ({ ...current, [teamId]: response.data }));
      if (selectedWorkspaceRef.current === `team:${teamId}`) setMembersFailure(undefined);
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, `team:${teamId}`)) return;
      if (selectedWorkspaceRef.current === `team:${teamId}`) {
        setMembersFailure(actionableError(error, "Team members could not be refreshed."));
      }
    }
  }, [handleWorkspaceAuthorityLoss]);

  useEffect(() => { void refreshAccount(); }, [refreshAccount]);
  useEffect(() => { void refreshApps(selectedWorkspace); }, [refreshApps, selectedWorkspace]);
  useEffect(() => {
    if (selectedTeam?.role === "owner") void refreshMembers(selectedTeam.id);
  }, [refreshMembers, selectedTeam?.id, selectedTeam?.role]);

  const activeJobIds = useMemo(
    () => jobs.filter((job) => job.status === "building").map((job) => job.id),
    [jobs],
  );
  const activeJobKey = activeJobIds.join(",");

  useEffect(() => {
    if (!activeJobKey) return undefined;
    const workspace = selectedWorkspace;
    const ids = activeJobKey.split(",");
    let cancelled = false;
    let timer: number | undefined;
    const poll = async () => {
      try {
        const responses = await Promise.all(ids.map((id) => requestJson<JobResponse>(
          workspacePath(`/apps/api/builds/${encodeURIComponent(id)}`, workspace),
        )));
        if (cancelled || selectedWorkspaceRef.current !== workspace) return;
        const published = hasCompletedBuild(responses);
        setJobs((current) => current.map((tracked) => {
          const response = responses.find((candidate) => candidate.job.id === tracked.id);
          return response ? {
            ...response.job,
            requestedPrompt: tracked.requestedPrompt,
            ...(tracked.updateAppId ? { updateAppId: tracked.updateAppId } : {}),
          } : tracked;
        }));
        if (published) await refreshApps(workspace);
        setNotice((current) => current?.message === "Build status could not be checked. Retry the page to reconnect."
          ? undefined
          : current);
      } catch (error) {
        if (await handleWorkspaceAuthorityLoss(error, workspace)) return;
        if (!cancelled && selectedWorkspaceRef.current === workspace) {
          setNotice({ kind: "error", message: "Build status could not be checked. Retry the page to reconnect." });
        }
      }
      if (!cancelled) timer = window.setTimeout(() => void poll(), 2_000);
    };
    timer = window.setTimeout(() => void poll(), 2_000);
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [activeJobKey, handleWorkspaceAuthorityLoss, refreshApps, selectedWorkspace]);

  function selectWorkspace(workspace: Workspace) {
    selectedWorkspaceRef.current = workspace;
    setSelectedWorkspace(workspace);
    setAppsFailure(undefined);
    setNotice(undefined);
    setTeamNotice(undefined);
    setJobs([]);
    setCreatePrompt("");
    setCreateApproved(false);
    setUpdatePrompts({});
    setCreating(false);
    setUpdatingAppIds(new Set());
    setRollbackKeys(new Set());
    setMembersFailure(undefined);
    setFreshInvitation(undefined);
    setInvitationInput("");
    setCreatingInvitation(false);
  }

  const createApp = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const workspace = selectedWorkspace;
    const prompt = createPrompt.trim();
    if (!canManage || !prompt || !createApproved || creating) return;
    setCreating(true);
    setNotice(undefined);
    try {
      const response = await startBuild(prompt, workspace);
      if (selectedWorkspaceRef.current !== workspace) return;
      setJobs((current) => [trackJob(response.job, prompt), ...current]);
      setCreatePrompt("");
      setCreateApproved(false);
      setNotice({ kind: "success", message: "Build accepted. Its status will stay here while it runs." });
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, workspace)) return;
      if (selectedWorkspaceRef.current === workspace) {
        setNotice({ kind: "error", message: actionableError(error, "The build could not be started.") });
      }
    } finally {
      if (selectedWorkspaceRef.current === workspace) setCreating(false);
    }
  };

  const updateApp = async (event: FormEvent<HTMLFormElement>, appId: string) => {
    event.preventDefault();
    const workspace = selectedWorkspace;
    const prompt = (updatePrompts[appId] ?? "").trim();
    if (!canManage || !prompt || updatingAppIds.has(appId)) return;
    setUpdatingAppIds((current) => withSetValue(current, appId, true));
    setNotice(undefined);
    try {
      const response = await startBuild(prompt, workspace, appId);
      if (selectedWorkspaceRef.current !== workspace) return;
      setJobs((current) => [trackJob(response.job, prompt, appId), ...current]);
      setUpdatePrompts((current) => ({ ...current, [appId]: "" }));
      setNotice({ kind: "success", message: "Update accepted. The current app remains live during the build." });
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, workspace)) return;
      if (selectedWorkspaceRef.current === workspace) {
        setNotice({ kind: "error", message: actionableError(error, "The update could not be started.") });
      }
    } finally {
      if (selectedWorkspaceRef.current === workspace) {
        setUpdatingAppIds((current) => withSetValue(current, appId, false));
      }
    }
  };

  const rollback = async (app: GeneratedApp, revision: AppRevision) => {
    const workspace = selectedWorkspace;
    const key = `${app.id}:${revision.id}`;
    if (!canManage || rollbackKeys.has(key)) return;
    setRollbackKeys((current) => withSetValue(current, key, true));
    setNotice(undefined);
    try {
      const response = await requestJson<ActivateResponse>(
        workspacePath(`/apps/api/apps/${encodeURIComponent(app.id)}/activate`, workspace),
        jsonMutation("POST", {
          revision: revision.id,
          expected_revision: app.active_revision,
          reason: "rollback",
        }),
      );
      if (selectedWorkspaceRef.current !== workspace) return;
      setAppSnapshots((current) => ({
        ...current,
        [workspace]: (current[workspace] ?? []).map((candidate) => candidate.id === response.app.id
          ? response.app
          : candidate),
      }));
      setNotice({ kind: "success", message: `${response.app.display_name} now serves the selected revision.` });
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, workspace)) return;
      if (selectedWorkspaceRef.current !== workspace) return;
      if (error instanceof ApiError && error.code === "stale_active") {
        await refreshApps(workspace);
        setNotice({
          kind: "error",
          message: "The active revision changed before rollback. The app card was refreshed; review it and try again.",
        });
      } else {
        setNotice({ kind: "error", message: actionableError(error, "The revision could not be restored.") });
      }
    } finally {
      if (selectedWorkspaceRef.current === workspace) {
        setRollbackKeys((current) => withSetValue(current, key, false));
      }
    }
  };

  const createTeam = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const name = teamName.trim();
    if (!name || creatingTeam) return;
    setCreatingTeam(true);
    setTeamNotice(undefined);
    try {
      const response = await requestJson<TeamResponse>("/v1/teams", jsonMutation("POST", { name }));
      setTeams((current) => mergeTeam(current, response.team));
      setTeamName("");
      selectWorkspace(`team:${response.team.id}`);
      setTeamNotice({ kind: "success", message: `${response.team.name} is ready.` });
    } catch (error) {
      setTeamNotice({ kind: "error", message: actionableError(error, "The team could not be created.") });
    } finally {
      setCreatingTeam(false);
    }
  };

  const createInvitation = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!selectedTeam || selectedTeam.role !== "owner" || creatingInvitation) return;
    const workspace = selectedWorkspace;
    setCreatingInvitation(true);
    setTeamNotice(undefined);
    setFreshInvitation(undefined);
    try {
      const response = await requestJson<InvitationResponse>(
        `/v1/teams/${encodeURIComponent(selectedTeam.id)}/invitations`,
        jsonMutation("POST", { role: inviteRole }),
      );
      if (selectedWorkspaceRef.current !== workspace) return;
      setFreshInvitation({
        teamId: selectedTeam.id,
        token: response.invitation,
        role: response.role,
        expiresAt: response.expires_at,
      });
      setTeamNotice({ kind: "success", message: "Invitation created. It is shown only in this page session." });
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, workspace)) return;
      if (selectedWorkspaceRef.current === workspace) {
        setTeamNotice({ kind: "error", message: actionableError(error, "The invitation could not be created.") });
      }
    } finally {
      if (selectedWorkspaceRef.current === workspace) setCreatingInvitation(false);
    }
  };

  const copyInvitation = async () => {
    if (!freshInvitation) return;
    try {
      await navigator.clipboard.writeText(freshInvitation.token);
      setTeamNotice({ kind: "success", message: "Invitation copied." });
    } catch {
      setTeamNotice({ kind: "error", message: "The invitation could not be copied. Select it and copy it manually." });
    }
  };

  const acceptInvitation = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const invitation = invitationInput.trim();
    if (!invitation || acceptingInvitation) return;
    setAcceptingInvitation(true);
    setTeamNotice(undefined);
    try {
      const response = await requestJson<TeamResponse>(
        "/v1/team-invitations/accept",
        jsonMutation("POST", { invitation }),
      );
      setInvitationInput("");
      setTeams((current) => mergeTeam(current, response.team));
      selectWorkspace(`team:${response.team.id}`);
      setTeamNotice({ kind: "success", message: `You joined ${response.team.name}.` });
    } catch (error) {
      setTeamNotice({ kind: "error", message: actionableError(error, "The invitation could not be accepted.") });
    } finally {
      setAcceptingInvitation(false);
    }
  };

  const removeMember = async (member: TeamMember) => {
    if (!selectedTeam || selectedTeam.role !== "owner" || removingMembers.has(member.user_id)) return;
    if (!window.confirm(`Remove ${member.user_id} from ${selectedTeam.name}?`)) return;
    const team = selectedTeam;
    const workspace = selectedWorkspace;
    setRemovingMembers((current) => withSetValue(current, member.user_id, true));
    setTeamNotice(undefined);
    try {
      await requestEmpty(`/v1/teams/${encodeURIComponent(team.id)}/members/${encodeURIComponent(member.user_id)}`, {
        method: "DELETE",
        credentials: "same-origin",
      });
      if (member.user_id === accountUserId) {
        setTeams((current) => current.filter((candidate) => candidate.id !== team.id));
        selectWorkspace("personal");
        setTeamNotice({ kind: "success", message: `You left ${team.name}.` });
      } else if (selectedWorkspaceRef.current === workspace) {
        setMembersByTeam((current) => ({
          ...current,
          [team.id]: (current[team.id] ?? []).filter((candidate) => candidate.user_id !== member.user_id),
        }));
        setTeamNotice({ kind: "success", message: "Team member removed." });
      }
    } catch (error) {
      if (await handleWorkspaceAuthorityLoss(error, workspace)) return;
      if (selectedWorkspaceRef.current === workspace) {
        setTeamNotice({ kind: "error", message: actionableError(error, "The team member could not be removed.") });
      }
    } finally {
      setRemovingMembers((current) => withSetValue(current, member.user_id, false));
    }
  };

  const restoreFailedPrompt = (job: TrackedJob) => {
    if (!canManage) return;
    if (job.updateAppId) setUpdatePrompts((current) => ({ ...current, [job.updateAppId!]: job.requestedPrompt }));
    else setCreatePrompt(job.requestedPrompt);
    setNotice({ kind: "success", message: "The prompt is ready to edit and submit again." });
  };

  return (
    <div className="console-shell">
      <header className="site-header">
        <a className="brand" href="/" aria-label="Nanocodex Apps home">
          <span className="brand-mark" aria-hidden="true">N</span>
          <span>Nanocodex Apps</span>
        </a>
        <a className="quiet-button" href="/agent">Back to Nanocodex</a>
      </header>

      <main>
        <section className="workspace-panel" aria-labelledby="workspace-title">
          <div className="workspace-selection">
            <label htmlFor="workspace-select" id="workspace-title">Workspace</label>
            <select
              id="workspace-select"
              value={selectedWorkspace}
              onChange={(event) => selectWorkspace(event.currentTarget.value as Workspace)}
            >
              <option value="personal">Personal</option>
              {teams.map((team) => <option key={team.id} value={`team:${team.id}`}>{team.name}</option>)}
            </select>
            <div className="workspace-identity">
              <strong>{selectedTeam?.name ?? "Personal"}</strong>
              <span className="role-badge">{selectedTeam?.role ?? "owner"}</span>
              {selectedTeam?.role === "member" ? <small>View and open access</small> : <small>Build and manage access</small>}
            </div>
          </div>
          <details className="team-tools">
            <summary>Team controls</summary>
            <div className="team-tool-grid">
              <form className="compact-form" onSubmit={createTeam}>
                <label htmlFor="team-name">Create a team</label>
                <div className="inline-fields">
                  <input
                    id="team-name"
                    value={teamName}
                    onChange={(event) => setTeamName(event.currentTarget.value)}
                    placeholder="Team name"
                    maxLength={64}
                    required
                  />
                  <button type="submit" disabled={creatingTeam || !teamName.trim()}>Create</button>
                </div>
              </form>
              <form className="compact-form" onSubmit={acceptInvitation}>
                <label htmlFor="invitation-token">Accept an invitation</label>
                <div className="inline-fields">
                  <input
                    id="invitation-token"
                    name="invitation"
                    type="password"
                    autoComplete="off"
                    value={invitationInput}
                    onChange={(event) => setInvitationInput(event.currentTarget.value)}
                    placeholder="Paste invitation"
                    required
                  />
                  <button type="submit" disabled={acceptingInvitation || !invitationInput.trim()}>Join</button>
                </div>
              </form>
              {selectedTeam?.role === "owner" ? (
                <form className="compact-form" onSubmit={createInvitation}>
                  <label htmlFor="invite-role">Invite to {selectedTeam.name}</label>
                  <div className="inline-fields">
                    <select id="invite-role" value={inviteRole} onChange={(event) => setInviteRole(event.currentTarget.value as TeamRole)}>
                      <option value="member">Member</option>
                      <option value="owner">Owner</option>
                    </select>
                    <button type="submit" disabled={creatingInvitation}>Create invite</button>
                  </div>
                </form>
              ) : null}
            </div>
            {freshInvitation && freshInvitation.teamId === selectedTeam?.id ? (
              <div className="fresh-invitation" role="status">
                <div>
                  <strong>Fresh {freshInvitation.role} invitation</strong>
                  <small>Expires {formatDate(freshInvitation.expiresAt)}. It disappears when you switch workspaces or leave this page.</small>
                </div>
                <input readOnly aria-label="Fresh invitation" value={freshInvitation.token} onFocus={(event) => event.currentTarget.select()} />
                <button type="button" onClick={() => void copyInvitation()}>Copy</button>
              </div>
            ) : null}
            {selectedTeam?.role === "owner" && membersByTeam[selectedTeam.id] ? (
              <div className="member-list">
                <div className="member-list-heading">
                  <strong>Members</strong>
                  <span>{membersByTeam[selectedTeam.id]!.length}</span>
                </div>
                <ul>
                  {membersByTeam[selectedTeam.id]!.map((member) => (
                    <li key={member.user_id}>
                      <div>
                        <code>{member.user_id === accountUserId ? "You" : member.user_id}</code>
                        <span className="role-badge">{member.role}</span>
                      </div>
                      <button
                        type="button"
                        className="danger-button"
                        disabled={removingMembers.has(member.user_id)}
                        onClick={() => void removeMember(member)}
                      >Remove</button>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
            {membersFailure ? <div className="compact-error" role="alert">{membersFailure}</div> : null}
          </details>
        </section>

        {accountFailure ? <div className="notice notice-error" role="alert"><span>{accountFailure}</span><button type="button" onClick={() => void refreshAccount()}>Try again</button></div> : null}
        {teamNotice ? <div className={`notice notice-${teamNotice.kind}`} role={teamNotice.kind === "error" ? "alert" : "status"}><span>{teamNotice.message}</span><button type="button" aria-label="Dismiss team message" onClick={() => setTeamNotice(undefined)}>×</button></div> : null}

        <section className="hero" aria-labelledby="console-title">
          <p className="eyebrow">{selectedTeam ? `${selectedTeam.name} app studio` : "Private app studio"}</p>
          <h1 id="console-title">Describe it. Ship it.</h1>
          <p className="hero-copy">
            {canManage
              ? "Turn an idea into a private, live Dynamic Worker. Nanocodex builds the first version and keeps every revision ready to restore."
              : `Open ${selectedTeam?.name ?? "team"} apps and inspect their immutable revision history.`}
          </p>
          {canManage ? (
            <form className="prompt-composer" onSubmit={createApp}>
              <label htmlFor="create-prompt">What should we build?</label>
              <textarea id="create-prompt" name="prompt" value={createPrompt} onChange={(event) => setCreatePrompt(event.currentTarget.value)} placeholder="Build me a lightweight project tracker with a calm dashboard and a JSON API…" rows={6} maxLength={24_576} required />
              <label className="capability-approval">
                <input type="checkbox" checked={createApproved} onChange={(event) => setCreateApproved(event.currentTarget.checked)} required />
                <span><strong>Grant this private app access</strong><small>Profile, app state, Workers AI, and Nanocodex agents. Credentials remain host-managed.</small></span>
              </label>
              <div className="composer-footer">
                <span>Include the audience, workflow, and visual direction.</span>
                <button className="primary-button" type="submit" disabled={creating || !createApproved || !createPrompt.trim()}>Build app</button>
              </div>
            </form>
          ) : null}
        </section>

        {notice ? <div className={`notice notice-${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}><span>{notice.message}</span><button type="button" aria-label="Dismiss message" onClick={() => setNotice(undefined)}>×</button></div> : null}

        {jobs.length ? (
          <section className="jobs-section" aria-labelledby="jobs-title">
            <div className="section-heading"><div><p className="eyebrow">Build activity</p><h2 id="jobs-title">Durable jobs</h2></div><span className="section-count">{jobs.length}</span></div>
            <div className="job-grid">{jobs.map((job) => <JobCard key={job.id} job={job} apps={apps ?? []} workspace={selectedWorkspace} onRestore={restoreFailedPrompt} />)}</div>
          </section>
        ) : null}

        {appsFailure ? <div className="notice notice-error" role="alert"><span>{appsFailure}</span><button type="button" onClick={() => void refreshApps(selectedWorkspace)}>Try again</button></div> : null}

        {apps ? (
          <section className="apps-section" aria-labelledby="apps-title">
            <div className="section-heading"><div><p className="eyebrow">{selectedTeam ? `${selectedTeam.name} workspace` : "Personal workspace"}</p><h2 id="apps-title">Live apps</h2></div><span className="section-count">{apps.length}</span></div>
            {apps.length ? (
              <div className="apps-grid">{apps.map((app) => {
                const appBuilding = jobs.some((job) => job.status === "building" && job.app_id === app.id);
                return <AppCard key={app.id} app={app} workspace={selectedWorkspace} canManage={canManage} prompt={updatePrompts[app.id] ?? ""} building={appBuilding} submitting={updatingAppIds.has(app.id)} rollbackKeys={rollbackKeys} onPromptChange={(prompt) => setUpdatePrompts((current) => ({ ...current, [app.id]: prompt }))} onSubmit={(event) => void updateApp(event, app.id)} onRollback={(revision) => void rollback(app, revision)} />;
              })}</div>
            ) : <div className="empty-state"><h3>{canManage ? "Your first app starts with a sentence." : "No team apps yet."}</h3><p>{canManage ? "Describe the result above. The app and its revision history will appear here." : "A team owner can build the first app for this workspace."}</p></div>}
          </section>
        ) : null}
      </main>

      <footer><span>Generated apps stay inside the selected workspace.</span><span>Every source change is a Git commit; every deploy is immutable.</span></footer>
    </div>
  );
}

function JobCard({ job, apps, workspace, onRestore }: Readonly<{
  job: TrackedJob;
  apps: readonly GeneratedApp[];
  workspace: Workspace;
  onRestore: (job: TrackedJob) => void;
}>) {
  const app = apps.find((candidate) => candidate.id === job.app_id);
  const title = app?.display_name ?? (job.updateAppId ? "App update" : "New app");
  return (
    <article className={`job-card job-${job.status}`}>
      <div className="job-card-head"><span className="status-dot" aria-hidden="true" /><div><h3>{title}</h3><p>{job.status === "building" ? "Build in progress" : job.status === "completed" ? "Build completed" : "Build failed"}</p></div><time dateTime={job.created_at}>{formatDate(job.created_at)}</time></div>
      <p className="job-prompt">{job.requestedPrompt}</p>
      {job.status === "completed" && app ? <LaunchApp app={app} workspace={workspace} className="inline-action" label="Open live app" /> : null}
      {job.status === "failed" ? <div className="job-failure" role="alert"><p>{job.error || "The builder did not complete this request. Adjust the prompt and try again."}</p><button type="button" onClick={() => onRestore(job)}>Edit prompt and try again</button></div> : null}
    </article>
  );
}

function AppCard({ app, workspace, canManage, prompt, building, submitting, rollbackKeys, onPromptChange, onSubmit, onRollback }: Readonly<{
  app: GeneratedApp;
  workspace: Workspace;
  canManage: boolean;
  prompt: string;
  building: boolean;
  submitting: boolean;
  rollbackKeys: ReadonlySet<string>;
  onPromptChange: (prompt: string) => void;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
  onRollback: (revision: AppRevision) => void;
}>) {
  const active = app.revisions.find((revision) => revision.id === app.active_revision);
  const rollbackActive = [...rollbackKeys].some((key) => key.startsWith(`${app.id}:`));
  return (
    <article className="app-card">
      <div className="app-card-header"><div className="app-identity"><span className="app-monogram" aria-hidden="true">{app.display_name.slice(0, 1).toUpperCase()}</span><div><h3>{app.display_name}</h3><p>/{app.live_slug}</p></div></div><LaunchApp app={app} workspace={workspace} className="launch-button" label="Open app" /></div>
      <dl className="app-meta"><div><dt>Active revision</dt><dd><code title={app.active_revision}>{shortRevision(app.active_revision)}</code></dd></div><div><dt>Published</dt><dd><time dateTime={app.updated_at}>{formatDate(app.updated_at)}</time></dd></div><div><dt>Artifact</dt><dd>{active ? formatBytes(active.artifact_bytes) : "Unavailable"}</dd></div></dl>
      <p className="capability-summary"><strong>Granted:</strong> profile, private state, Workers AI, and Nanocodex agents</p>
      {canManage ? (
        <form className="update-form" id={`update-${app.id}`} onSubmit={onSubmit}>
          <label htmlFor={`update-prompt-${app.id}`}>Describe an update</label>
          <textarea id={`update-prompt-${app.id}`} name="prompt" value={prompt} onChange={(event) => onPromptChange(event.currentTarget.value)} placeholder="Add filtering, simplify the navigation, change the color palette…" rows={3} maxLength={24_576} required />
          <div className="update-actions">{building ? <span className="building-note">An update is building</span> : <span />}<button type="submit" disabled={submitting || building || rollbackActive || !prompt.trim()}>Build update</button></div>
        </form>
      ) : null}
      <details className="revision-history"><summary><span>Revision history</span><span>{app.revisions.length}</span></summary><ol>{app.revisions.map((revision) => {
        const isActive = revision.id === app.active_revision;
        const rollbackKey = `${app.id}:${revision.id}`;
        const restoring = rollbackKeys.has(rollbackKey);
        return <li key={revision.id}><div className="revision-main"><div className="revision-title"><code title={revision.id}>{shortRevision(revision.id)}</code>{isActive ? <span className="active-badge">Live</span> : null}</div><p><time dateTime={revision.created_at}>{formatDate(revision.created_at)}</time><span aria-hidden="true"> · </span>{formatBytes(revision.artifact_bytes)}<span aria-hidden="true"> · </span>{revision.generation_model}</p><p className="source-summary">Git {shortCommit(revision.source_commit)} · {revision.source_summary.entryPoint} · {revision.source_summary.files.length} {revision.source_summary.files.length === 1 ? "file" : "files"}</p></div>{canManage && !isActive ? <button type="button" disabled={restoring || rollbackActive || building || submitting} onClick={() => onRollback(revision)}>Rollback</button> : null}</li>;
      })}</ol></details>
    </article>
  );
}

async function startBuild(prompt: string, workspace: Workspace, appId?: string): Promise<JobResponse> {
  return requestJson<JobResponse>(workspacePath("/apps/api/builds", workspace), jsonMutation("POST", {
    grants: APP_GRANTS,
    prompt,
    ...(appId ? { app_id: appId } : {}),
  }));
}

function LaunchApp({ app, workspace, className, label }: Readonly<{
  app: GeneratedApp;
  workspace: Workspace;
  className: string;
  label: string;
}>) {
  const [opening, setOpening] = useState(false);
  const [failure, setFailure] = useState<string>();
  const launch = async () => {
    if (opening) return;
    const popup = window.open("about:blank", "_blank");
    if (!popup) {
      setFailure("Your browser blocked the app window. Allow popups for Nanocodex and try again.");
      return;
    }
    popup.opener = null;
    setOpening(true);
    setFailure(undefined);
    try {
      const response = await requestJson<LaunchResponse>(
        appLaunchPath(app.id, workspace),
        jsonMutation("POST", {}),
      );
      const target = new URL(response.launch_url);
      if (target.protocol !== "https:") throw new Error("The app runtime returned an unsafe launch URL.");
      popup.location.replace(target.href);
    } catch (error) {
      popup.close();
      setFailure(actionableError(error, "The app could not be opened."));
    } finally {
      setOpening(false);
    }
  };
  return <div className="launch-action"><button className={className} type="button" disabled={opening} onClick={() => void launch()}>{label} <span aria-hidden="true">↗</span></button>{failure ? <p role="alert">{failure}</p> : null}</div>;
}

function mergeTeam(teams: readonly Team[], team: Team): readonly Team[] {
  return [...teams.filter((candidate) => candidate.id !== team.id), team]
    .sort((left, right) => left.name.localeCompare(right.name));
}

function trackJob(job: BuildJob, requestedPrompt: string, updateAppId?: string): TrackedJob {
  return { ...job, requestedPrompt, ...(updateAppId ? { updateAppId } : {}) };
}

function jsonMutation(method: "POST" | "DELETE", body: Record<string, unknown>): RequestInit {
  return { method, headers: { "content-type": "application/json" }, body: JSON.stringify(body) };
}

async function requestJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await request(path, init);
  try {
    return await response.json() as T;
  } catch {
    throw new ApiError(response.status, undefined, "The platform returned an unreadable response.");
  }
}

async function requestEmpty(path: string, init?: RequestInit): Promise<void> {
  const response = await request(path, init);
  await response.body?.cancel();
}

async function request(path: string, init?: RequestInit): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: { accept: "application/json", ...headersObject(init?.headers) },
    });
  } catch {
    throw new Error("The platform could not be reached. Check your connection and try again.");
  }
  if (!response.ok) {
    let payload: unknown;
    try { payload = await response.json(); } catch { payload = undefined; }
    const record = isRecord(payload) ? payload : {};
    const code = typeof record.error === "string" ? record.error : undefined;
    const detail = typeof record.message === "string" ? record.message : code;
    throw new ApiError(response.status, code, detail || `Request failed with status ${response.status}.`);
  }
  return response;
}

function headersObject(headers: HeadersInit | undefined): Record<string, string> {
  return Object.fromEntries(new Headers(headers).entries());
}

function actionableError(error: unknown, fallback: string): string {
  if (error instanceof ApiError) {
    if (error.status === 401) return "Your account session expired. Log in again, then retry this action.";
    if (error.status === 403 || error.status === 404) return "This workspace no longer grants access to that action. Refresh your teams and try again.";
    if (error.status === 409) return `${error.message} Refresh the current state and try again.`;
    return `${fallback} ${error.message}`;
  }
  return error instanceof Error && error.message ? error.message : fallback;
}

function withSetValue(current: ReadonlySet<string>, value: string, present: boolean): ReadonlySet<string> {
  const next = new Set(current);
  if (present) next.add(value); else next.delete(value);
  return next;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function shortRevision(revision: string): string { return revision ? revision.slice(0, 10) : "none"; }
function shortCommit(commit: string): string { return commit ? commit.slice(0, 8) : "unavailable"; }

function formatDate(value: string | number): string {
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return "Unknown time";
  return new Intl.DateTimeFormat(undefined, { day: "numeric", hour: "numeric", minute: "2-digit", month: "short", year: "numeric" }).format(date);
}
