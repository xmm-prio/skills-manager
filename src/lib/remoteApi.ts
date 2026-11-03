import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import i18next from "i18next";
import type { AppError } from "./error";
import type {
  BatchDeleteSkillsResult,
  BatchUpdateSkillsResult,
  ManagedSkill,
  Preset,
  Project,
  ProjectAgentTarget,
  ProjectSkill,
  ProjectSkillDocument,
  SkillDocument,
  SkillToolToggle,
  ToolInfo,
  UpdateSkillResult,
} from "./tauri";

// ── Environments ──

export const LOCAL_ENV_ID = "local";

export interface RemoteEnv {
  id: string;
  name: string;
  host: string;
  port: number | null;
  user: string | null;
  identity_file: string | null;
  cli_path: string | null;
  /** Missing on environments saved before password login existed. */
  auth?: RemoteAuthMethod;
  /** Write-only: sent on save/test, never returned. Empty keeps the stored one. */
  password?: string | null;
}

export type RemoteAuthMethod = "key" | "password";

export interface RemoteEnvProbe {
  platform: string | null;
  cli_asset: string | null;
  glibc_version: string | null;
  glibc_required: string | null;
  glibc_supported: boolean | null;
  cli_path: string | null;
  cli_version: string | null;
  app_version: string;
  cli_outdated: boolean;
}

/** `user@host:port`, leaving out the parts ssh fills in from its config. */
export function formatEnvAddress(env: RemoteEnv) {
  const user = env.user ? `${env.user}@` : "";
  const port = env.port ? `:${env.port}` : "";
  return `${user}${env.host}${port}`;
}

export const listRemoteEnvs = () => tauriInvoke<RemoteEnv[]>("list_remote_envs");

export const saveRemoteEnv = (env: RemoteEnv) =>
  tauriInvoke<RemoteEnv>("save_remote_env", { env });

export const deleteRemoteEnv = (id: string) =>
  tauriInvoke<void>("delete_remote_env", { id });

export const getActiveEnv = () => tauriInvoke<string>("get_active_env");

export const setActiveEnv = (id: string) =>
  tauriInvoke<void>("set_active_env", { id });

export const testRemoteEnv = (env: RemoteEnv) =>
  tauriInvoke<RemoteEnvProbe>("test_remote_env", { env });

/** Resolves to the probe taken right after installing. */
export const installRemoteCli = (env: RemoteEnv) =>
  tauriInvoke<RemoteEnvProbe>("install_remote_cli", { env });

let activeEnvId = LOCAL_ENV_ID;

/** Point every routed API call at `id`. Persisting the choice is separate. */
export function setRoutingEnv(id: string) {
  if (id !== activeEnvId) updateStatusCache.clear();
  activeEnvId = id;
}

export function isRemoteRouting() {
  return activeEnvId !== LOCAL_ENV_ID;
}

// ── Remote CLI transport ──

type RemoteResult =
  | { ok: true; data: unknown }
  | { ok: false; error: AppError };

interface QueuedCall {
  envId: string;
  args: string[];
  resolve: (value: unknown) => void;
  reject: (reason: unknown) => void;
}

let queue: QueuedCall[] = [];
let flushScheduled = false;

/**
 * Calls made in the same tick share one ssh session: a page load fires
 * several reads at once, and each separate connection costs a handshake.
 */
export function remoteCli<T>(args: string[]): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    queue.push({
      envId: activeEnvId,
      args,
      resolve: resolve as (value: unknown) => void,
      reject,
    });
    if (!flushScheduled) {
      flushScheduled = true;
      setTimeout(flushQueue, 0);
    }
  });
}

async function flushQueue() {
  flushScheduled = false;
  const pending = queue;
  queue = [];
  const byEnv = new Map<string, QueuedCall[]>();
  for (const call of pending) {
    const list = byEnv.get(call.envId) ?? [];
    list.push(call);
    byEnv.set(call.envId, list);
  }
  await Promise.all(
    [...byEnv.entries()].map(async ([envId, calls]) => {
      try {
        const results = await tauriInvoke<RemoteResult[]>("remote_cli_exec", {
          envId,
          commands: calls.map((call) => call.args),
        });
        calls.forEach((call, index) => {
          const result = results[index];
          if (!result) {
            call.reject({ kind: "internal", message: "Missing remote result" });
          } else if (result.ok) {
            call.resolve(result.data);
          } else {
            call.reject(result.error);
          }
        });
      } catch (error) {
        calls.forEach((call) => call.reject(error));
      }
    })
  );
}

export const REMOTE_UNSUPPORTED = "REMOTE_UNSUPPORTED";

export function remoteUnsupportedError(): AppError & { code: string } {
  return {
    kind: "invalid_input",
    code: REMOTE_UNSUPPORTED,
    message: i18next.t("remote.unsupported"),
  };
}

// ── CLI output shapes ──

export interface CliSkillSummary {
  id: string;
  name: string;
  description: string | null;
  path: string;
  enabled: boolean;
  tags: string[];
  source_type: string;
  source_ref: string | null;
  preset_ids: string[];
  presets: string[];
  deployed_to: string[];
}

interface CliSkillDetail extends CliSkillSummary {
  skill_file: string;
  files: string[];
  markdown: string;
}

export interface CliPresetInfo {
  id: string;
  name: string;
  description: string | null;
  icon: string | null;
  sort_order: number;
  skill_count: number;
  active: boolean;
}

interface CliSkillStatus extends CliSkillSummary {
  agents: {
    key: string;
    display_name: string;
    installed: boolean;
    globally_enabled: boolean;
    deployed: boolean;
    target_path: string | null;
  }[];
}

interface CliCheckReport {
  skill_id: string;
  update_status: string;
  last_check_error: string | null;
  skipped: boolean;
}

interface CliUpdateReport {
  skill_id: string;
  name: string;
  refreshed: boolean;
  error: string | null;
  held_back_removals?: string[];
}

interface CliRemoveReport {
  deleted: number;
  failed: string[];
}

// ── Mapping ──

interface UpdateStatusEntry {
  update_status: string;
  last_check_error: string | null;
  last_checked_at: number;
}

/** The CLI list has no update status, so the last check result is kept here. */
const updateStatusCache = new Map<string, UpdateStatusEntry>();

function rememberChecks(reports: CliCheckReport[]) {
  const now = Math.floor(Date.now() / 1000);
  for (const report of reports) {
    if (report.skipped) continue;
    updateStatusCache.set(report.skill_id, {
      update_status: report.update_status,
      last_check_error: report.last_check_error,
      last_checked_at: now,
    });
  }
}

export function toManagedSkill(skill: CliSkillSummary): ManagedSkill {
  const status = updateStatusCache.get(skill.id);
  return {
    id: skill.id,
    name: skill.name,
    description: skill.description,
    source_type: skill.source_type,
    source_ref: skill.source_ref,
    source_ref_resolved: null,
    source_subpath: null,
    source_branch: null,
    source_revision: null,
    remote_revision: null,
    update_status:
      status?.update_status ?? (skill.source_type === "local" ? "local_only" : "unknown"),
    last_checked_at: status?.last_checked_at ?? null,
    last_check_error: status?.last_check_error ?? null,
    central_path: skill.path,
    enabled: skill.enabled,
    created_at: 0,
    updated_at: 0,
    status: "ok",
    targets: skill.deployed_to.map((tool) => ({
      id: `${skill.id}:${tool}`,
      skill_id: skill.id,
      tool,
      target_path: "",
      mode: "remote",
      status: "ok",
      synced_at: null,
    })),
    preset_ids: skill.preset_ids,
    tags: skill.tags,
  };
}

export function toPreset(preset: CliPresetInfo): Preset {
  return {
    id: preset.id,
    name: preset.name,
    description: preset.description,
    icon: preset.icon,
    sort_order: preset.sort_order,
    skill_count: preset.skill_count,
    created_at: 0,
    updated_at: 0,
  };
}

function baseName(path: string) {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

export function toSkillDocument(detail: CliSkillDetail): SkillDocument {
  return {
    skill_id: detail.id,
    filename: baseName(detail.skill_file),
    content: detail.markdown,
    central_path: detail.path,
  };
}

// ── Handlers, keyed by Tauri command name with the same argument object ──

type Args = Record<string, unknown>;
type Handler = (args: Args) => Promise<unknown>;

const str = (value: unknown) => String(value ?? "");
const optional = (flag: string, value: unknown): string[] =>
  typeof value === "string" && value.trim() !== "" ? [flag, value] : [];

const listSkills = async (extra: string[] = []) =>
  (await remoteCli<CliSkillSummary[]>(["skills", "list", ...extra])).map(toManagedSkill);

async function findSkill(skillId: string): Promise<ManagedSkill> {
  const skill = (await listSkills()).find((item) => item.id === skillId);
  if (!skill) throw { kind: "not_found", message: `Skill not found: ${skillId}` };
  return skill;
}

async function updateOne(skillId: string): Promise<CliUpdateReport> {
  const reports = await remoteCli<CliUpdateReport[]>(["skills", "update", skillId]);
  const report = reports[0];
  if (!report) throw { kind: "internal", message: "Remote update returned no report" };
  if (!report.error && !(report.held_back_removals?.length)) {
    updateStatusCache.set(skillId, {
      update_status: "up_to_date",
      last_check_error: null,
      last_checked_at: Math.floor(Date.now() / 1000),
    });
  }
  return report;
}

const deploy = (skillId: unknown, tool: unknown, enabled: boolean) =>
  remoteCli(["skills", enabled ? "deploy" : "undeploy", str(skillId), "--agent", str(tool)]).then(
    () => undefined
  );

function isOldCliError(error: unknown) {
  const message = (error as AppError | undefined)?.message;
  return typeof message === "string" && message.includes("unrecognized subcommand 'projects'");
}

/** CLIs before 1.44 have no `projects` group; say so instead of echoing clap. */
const projectsCli = <T = unknown>(args: string[]) =>
  remoteCli<T>(["projects", ...args]).catch((error: unknown) => {
    if (isOldCliError(error)) {
      throw { kind: "invalid_input", code: REMOTE_UNSUPPORTED, message: i18next.t("remote.projectsNeedNewerCli") };
    }
    throw error;
  });

const projectSkillArgs = ({ projectId, skillRelativePath, agent }: Args) => [
  str(projectId),
  str(skillRelativePath),
  "--agent",
  str(agent),
];

const projectSkillCommand = (subcommand: string, extra: string[] = []) => (args: Args) =>
  projectsCli([subcommand, ...projectSkillArgs(args), ...extra]).then(() => undefined);

const remoteHandlers: Record<string, Handler> = {
  // Agents
  get_tool_status: () => remoteCli<ToolInfo[]>(["agents", "list"]),
  set_tool_enabled: ({ key, enabled }) =>
    remoteCli(["agents", enabled ? "enable" : "disable", str(key)]).then(() => undefined),
  set_all_tools_enabled: async ({ enabled }) => {
    const tools = await remoteCli<ToolInfo[]>(["agents", "list"]);
    const keys = tools.filter((tool) => tool.enabled !== enabled).map((tool) => tool.key);
    if (keys.length > 0) await remoteCli(["agents", enabled ? "enable" : "disable", ...keys]);
  },
  set_tool_order_cmd: async () => undefined,
  add_custom_tool: ({ key, displayName, skillsDir, projectRelativeSkillsDir }) =>
    remoteCli([
      "agents",
      "add-custom",
      str(key),
      "--path",
      str(skillsDir),
      ...optional("--name", displayName),
      ...optional("--project-path", projectRelativeSkillsDir),
    ]).then(() => undefined),

  // Skills
  get_managed_skills: () => listSkills(),
  get_skills_for_preset: ({ presetId }) => listSkills(["--preset", str(presetId)]),
  get_skill_document: async ({ skillId }) =>
    toSkillDocument(await remoteCli<CliSkillDetail>(["skills", "show", str(skillId)])),
  delete_managed_skill: ({ skillId }) =>
    remoteCli(["skills", "remove", str(skillId), "--yes"]).then(() => undefined),
  delete_managed_skills: async ({ skillIds }): Promise<BatchDeleteSkillsResult> => {
    const ids = (skillIds as string[]) ?? [];
    if (ids.length === 0) return { deleted: 0, failed: [] };
    const report = await remoteCli<CliRemoveReport>(["skills", "remove", ...ids, "--yes"]);
    return { deleted: report.deleted, failed: report.failed };
  },
  install_git: ({ repoUrl, name }) =>
    remoteCli(["skills", "install", str(repoUrl), "--git", ...optional("--name", name)]).then(
      () => undefined
    ),
  // A source without `/` is a website that publishes its own skills.
  install_from_skillssh: ({ source, skillId }) =>
    remoteCli(
      str(source).includes("/")
        ? ["skills", "install", `${str(source)}@${str(skillId)}`, "--skillssh"]
        : ["skills", "install", `https://skills.sh/site/${str(source)}/${str(skillId)}`]
    ).then(() => undefined),
  check_skill_update: async ({ skillId, force }) => {
    rememberChecks(
      await remoteCli<CliCheckReport[]>([
        "skills",
        "check",
        str(skillId),
        ...(force ? ["--force"] : []),
      ])
    );
    return findSkill(str(skillId));
  },
  check_all_skill_updates: async ({ force }) => {
    rememberChecks(
      await remoteCli<CliCheckReport[]>(["skills", "check", "--all", ...(force ? ["--force"] : [])])
    );
  },
  update_skill: async ({ skillId }): Promise<UpdateSkillResult> => {
    const report = await updateOne(str(skillId));
    if (report.error) throw { kind: "internal", message: report.error };
    if (report.held_back_removals?.length) {
      throw {
        kind: "invalid_input",
        message: i18next.t("remote.updateHeldBack", {
          paths: report.held_back_removals.slice(0, 5).join(", "),
        }),
      };
    }
    return {
      skill: await findSkill(str(skillId)),
      content_changed: report.refreshed,
      pending_removals: [],
      removal_approval: null,
    };
  },
  batch_update_skills: async ({ skillIds }): Promise<BatchUpdateSkillsResult> => {
    const ids = (skillIds as string[]) ?? [];
    const settled = await Promise.allSettled(ids.map((id) => updateOne(id)));
    const result: BatchUpdateSkillsResult = { refreshed: 0, unchanged: 0, held_back: [], failed: [] };
    settled.forEach((outcome, index) => {
      if (outcome.status === "rejected") {
        result.failed.push(ids[index]);
      } else if (outcome.value.error) {
        result.failed.push(outcome.value.name);
      } else if (outcome.value.held_back_removals?.length) {
        result.held_back.push(outcome.value.name);
      } else if (outcome.value.refreshed) {
        result.refreshed += 1;
      } else {
        result.unchanged += 1;
      }
    });
    return result;
  },
  get_all_tags: () => remoteCli<string[]>(["skills", "tag", "list"]),
  set_skill_tags: ({ skillId, tags }) =>
    remoteCli(["skills", "tag", "set", str(skillId), ...((tags as string[]) ?? [])]).then(
      () => undefined
    ),
  rename_tag: ({ oldName, newName }) =>
    remoteCli(["skills", "tag", "rename", str(oldName), str(newName)]).then(() => undefined),
  delete_tag: ({ name }) =>
    remoteCli(["skills", "tag", "delete", str(name), "--yes"]).then(() => undefined),

  // Deployment
  sync_skill_to_tool: ({ skillId, tool }) => deploy(skillId, tool, true),
  unsync_skill_from_tool: ({ skillId, tool }) => deploy(skillId, tool, false),
  get_skill_tool_toggles: async ({ skillId }): Promise<SkillToolToggle[]> => {
    const status = await remoteCli<CliSkillStatus>(["skills", "status", str(skillId)]);
    return status.agents.map((agent) => ({
      tool: agent.key,
      display_name: agent.display_name,
      installed: agent.installed,
      globally_enabled: agent.globally_enabled,
      enabled: agent.deployed,
    }));
  },
  set_skill_tool_toggle: ({ skillId, tool, enabled }) => deploy(skillId, tool, Boolean(enabled)),

  // Presets
  get_presets: async () => (await remoteCli<CliPresetInfo[]>(["presets", "list"])).map(toPreset),
  get_active_preset: async () => {
    const current = await remoteCli<CliPresetInfo | null>(["presets", "current"]);
    return current ? toPreset(current) : null;
  },
  create_preset: async ({ name, description, icon }) =>
    toPreset(
      await remoteCli<CliPresetInfo>([
        "presets",
        "create",
        str(name),
        ...optional("--description", description),
        ...optional("--icon", icon),
      ])
    ),
  update_preset: ({ id, name, description, icon }) =>
    remoteCli([
      "presets",
      "update",
      str(id),
      "--name",
      str(name),
      "--description",
      str(description),
      "--icon",
      str(icon),
    ]).then(() => undefined),
  delete_preset: ({ id }) =>
    remoteCli(["presets", "delete", str(id), "--yes"]).then(() => undefined),
  switch_preset: ({ id }) => remoteCli(["presets", "apply", str(id)]).then(() => undefined),
  apply_preset_to_default: ({ id }) =>
    remoteCli(["presets", "apply", str(id)]).then(() => undefined),
  add_skill_to_preset: ({ skillId, presetId }) =>
    remoteCli(["presets", "add-skill", str(presetId), str(skillId)]).then(() => undefined),
  remove_skill_from_preset: ({ skillId, presetId }) =>
    remoteCli(["presets", "remove-skill", str(presetId), str(skillId)]).then(() => undefined),
  get_preset_skill_order: async () => [],

  // Project workspaces: folders on the remote host
  get_projects: () =>
    projectsCli<Project[]>(["list"]).catch((error: unknown) => {
      // The sidebar loads this on every refresh; an old CLI just has none.
      if ((error as { code?: string })?.code === REMOTE_UNSUPPORTED) return [];
      throw error;
    }),
  add_project: ({ path }) => projectsCli<Project>(["add", str(path)]),
  add_linked_workspace: ({ name, path, disabledPath }) =>
    projectsCli<Project>([
      "add-linked",
      str(path),
      "--name",
      str(name),
      ...optional("--disabled-path", disabledPath),
    ]),
  remove_project: ({ id }) => projectsCli(["remove", str(id)]).then(() => undefined),
  reorder_projects: ({ ids }) =>
    projectsCli(["reorder", ...((ids as string[]) ?? [])]).then(() => undefined),
  scan_projects: ({ root }) => projectsCli<string[]>(["scan", str(root)]),
  get_project_agent_targets: ({ projectId }) =>
    projectsCli<ProjectAgentTarget[]>(["agents", str(projectId)]),
  get_project_skills: ({ projectId }) => projectsCli<ProjectSkill[]>(["skills", str(projectId)]),
  get_project_skill_document: (args) =>
    projectsCli<ProjectSkillDocument>(["show-skill", ...projectSkillArgs(args)]),
  import_project_skill_to_center: projectSkillCommand("import"),
  update_project_skill_to_center: projectSkillCommand("import"),
  update_project_skill_from_center: projectSkillCommand("pull"),
  toggle_project_skill: (args) => projectSkillCommand(args.enabled ? "enable" : "disable")(args),
  delete_project_skill: projectSkillCommand("delete-skill", ["--yes"]),
  export_skill_to_project: ({ skillId, projectId, agents }) =>
    projectsCli([
      "export",
      str(projectId),
      str(skillId),
      ...((agents as string[] | null) ?? []).flatMap((agent) => ["--agent", agent]),
    ]).then(() => undefined),

  // Local-machine features with no remote counterpart yet
  get_global_local_skills: async () => [],
};

/**
 * Commands that always run on this machine: app chrome, UI preferences,
 * marketplace browsing, and environment management itself.
 */
const LOCAL_COMMANDS = new Set([
  "get_settings",
  "set_settings",
  "log_startup_event",
  "fetch_leaderboard",
  "search_skillssh",
  "slugify_skill_names",
  "cancel_install",
  "check_app_update",
  "update_install_blocker",
  "restart_app",
  "app_exit",
  "hide_to_tray",
  "get_diagnostic_info",
  "get_recent_log_excerpt",
  "export_logs_zip",
  "check_last_panic",
  "clear_last_panic",
  "get_central_repo_path",
  "get_central_repo_path_override",
  "get_central_repo_pending_path",
  "get_central_repo_warnings",
  "set_central_repo_path",
  "open_central_repo_folder",
]);

/** `invoke` for the app API: runs locally, or on the active remote environment. */
export function routedInvoke<T>(command: string, args?: Args): Promise<T> {
  if (!isRemoteRouting() || LOCAL_COMMANDS.has(command)) {
    return tauriInvoke<T>(command, args);
  }
  const handler = remoteHandlers[command];
  if (!handler) return Promise.reject(remoteUnsupportedError());
  return handler(args ?? {}) as Promise<T>;
}
