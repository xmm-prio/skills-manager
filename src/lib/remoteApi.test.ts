import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("i18next", () => ({ default: { t: (key: string) => key } }));

import {
  LOCAL_ENV_ID,
  REMOTE_UNSUPPORTED,
  routedInvoke,
  setRoutingEnv,
  toManagedSkill,
  type CliSkillSummary,
} from "./remoteApi";

const summary = (overrides: Partial<CliSkillSummary> = {}): CliSkillSummary => ({
  id: "s1",
  name: "db",
  description: "Database helper",
  path: "/home/a/.skills-manager/skills/db",
  enabled: true,
  tags: ["backend"],
  source_type: "git",
  source_ref: "https://github.com/o/r",
  preset_ids: ["p1"],
  presets: ["Backend"],
  deployed_to: ["claude_code", "codex"],
  ...overrides,
});

/** Answers each remote_cli_exec batch with one result per command. */
function answerRemote(handler: (args: string[]) => { ok: true; data: unknown } | { ok: false; error: unknown }) {
  invoke.mockImplementation(async (command: string, payload: { commands: string[][] }) => {
    if (command !== "remote_cli_exec") throw new Error(`unexpected local call: ${command}`);
    return payload.commands.map(handler);
  });
}

beforeEach(() => {
  invoke.mockReset();
  setRoutingEnv("env-1");
});

afterEach(() => {
  setRoutingEnv(LOCAL_ENV_ID);
});

describe("routedInvoke", () => {
  it("passes every command through to Tauri on the local environment", async () => {
    setRoutingEnv(LOCAL_ENV_ID);
    invoke.mockResolvedValue([]);
    await routedInvoke("get_managed_skills");
    await routedInvoke("git_backup_status");
    expect(invoke.mock.calls.map(([command]) => command)).toEqual([
      "get_managed_skills",
      "git_backup_status",
    ]);
  });

  it("keeps app preferences local during a remote session", async () => {
    invoke.mockResolvedValue("dark");
    await expect(routedInvoke("get_settings", { key: "theme" })).resolves.toBe("dark");
    expect(invoke).toHaveBeenCalledWith("get_settings", { key: "theme" });
  });

  it("rejects local-only features remotely without touching the local library", async () => {
    await expect(routedInvoke("git_backup_status")).rejects.toMatchObject({
      code: REMOTE_UNSUPPORTED,
      kind: "invalid_input",
    });
    expect(invoke).not.toHaveBeenCalled();
  });

  it("sends calls made in the same tick as one ssh batch", async () => {
    answerRemote((args) => {
      if (args[0] === "agents") return { ok: true, data: [{ key: "codex" }] };
      if (args[0] === "presets") return { ok: true, data: null };
      return { ok: true, data: [summary()] };
    });
    const [tools, active, skills] = await Promise.all([
      routedInvoke("get_tool_status"),
      routedInvoke("get_active_preset"),
      routedInvoke("get_managed_skills"),
    ]);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("remote_cli_exec", {
      envId: "env-1",
      commands: [
        ["agents", "list"],
        ["presets", "current"],
        ["skills", "list"],
      ],
    });
    expect(tools).toEqual([{ key: "codex" }]);
    expect(active).toBeNull();
    expect((skills as { id: string }[])[0].id).toBe("s1");
  });

  it("rejects only the failed command of a batch, with the remote error", async () => {
    const error = { kind: "internal", message: "skill not found: nope" };
    answerRemote((args) =>
      args[1] === "deploy" ? { ok: false, error } : { ok: true, data: [] }
    );
    const [deploy, tags] = await Promise.allSettled([
      routedInvoke("sync_skill_to_tool", { skillId: "nope", tool: "codex" }),
      routedInvoke("get_all_tags"),
    ]);
    expect(deploy).toEqual({ status: "rejected", reason: error });
    expect(tags).toEqual({ status: "fulfilled", value: [] });
  });

  it("maps app arguments onto CLI arguments", async () => {
    answerRemote(() => ({ ok: true, data: {} }));
    await routedInvoke("sync_skill_to_tool", { skillId: "s1", tool: "codex" });
    await routedInvoke("set_skill_tags", { skillId: "s1", tags: [] });
    await routedInvoke("install_from_skillssh", { source: "o/r", skillId: "db" });
    await routedInvoke("install_from_skillssh", { source: "example.com", skillId: "db" });
    expect(invoke.mock.calls.map(([, payload]) => payload.commands[0])).toEqual([
      ["skills", "deploy", "s1", "--agent", "codex"],
      ["skills", "tag", "set", "s1"],
      ["skills", "install", "o/r@db", "--skillssh"],
      ["skills", "install", "https://skills.sh/site/example.com/db"],
    ]);
  });

  it("maps project workspace commands onto the CLI projects group", async () => {
    answerRemote(() => ({ ok: true, data: null }));
    const skill = { projectId: "p1", skillRelativePath: "team/db", agent: "claude_code" };
    await routedInvoke("add_linked_workspace", { name: "Hermes", path: "~/.hermes/skills", disabledPath: null });
    await routedInvoke("toggle_project_skill", { ...skill, enabled: false });
    await routedInvoke("delete_project_skill", skill);
    await routedInvoke("update_project_skill_to_center", skill);
    await routedInvoke("export_skill_to_project", { skillId: "s1", projectId: "p1", agents: ["codex", "cursor"] });
    await routedInvoke("export_skill_to_project", { skillId: "s1", projectId: "p1", agents: null });
    expect(invoke.mock.calls.map(([, payload]) => payload.commands[0])).toEqual([
      ["projects", "add-linked", "~/.hermes/skills", "--name", "Hermes"],
      ["projects", "disable", "p1", "team/db", "--agent", "claude_code"],
      ["projects", "delete-skill", "p1", "team/db", "--agent", "claude_code", "--yes"],
      ["projects", "import", "p1", "team/db", "--agent", "claude_code"],
      ["projects", "export", "p1", "s1", "--agent", "codex", "--agent", "cursor"],
      ["projects", "export", "p1", "s1"],
    ]);
  });

  it("treats a remote CLI without the projects group as having no workspaces", async () => {
    const error = { kind: "invalid_input", message: "error: unrecognized subcommand 'projects'" };
    answerRemote(() => ({ ok: false, error }));
    await expect(routedInvoke("get_projects")).resolves.toEqual([]);
    await expect(routedInvoke("add_project", { path: "/srv/app" })).rejects.toMatchObject({
      code: REMOTE_UNSUPPORTED,
      message: "remote.projectsNeedNewerCli",
    });
  });

  it("carries the last check result into later skill lists", async () => {
    answerRemote((args) =>
      args[1] === "check"
        ? {
            ok: true,
            data: [{ skill_id: "s1", update_status: "update_available", last_check_error: null, skipped: false }],
          }
        : { ok: true, data: [summary()] }
    );
    await routedInvoke("check_all_skill_updates", { force: false });
    const [skill] = (await routedInvoke("get_managed_skills")) as { update_status: string }[];
    expect(skill.update_status).toBe("update_available");

    setRoutingEnv("env-2");
    const [fresh] = (await routedInvoke("get_managed_skills")) as { update_status: string }[];
    expect(fresh.update_status).toBe("unknown");
  });
});

describe("toManagedSkill", () => {
  it("turns deployments into per-agent targets", () => {
    const skill = toManagedSkill(summary());
    expect(skill.central_path).toBe("/home/a/.skills-manager/skills/db");
    expect(skill.targets.map((target) => target.tool)).toEqual(["claude_code", "codex"]);
    expect(skill.targets.every((target) => target.skill_id === "s1" && target.status === "ok")).toBe(true);
    expect(skill.preset_ids).toEqual(["p1"]);
  });

  it("marks local skills as having no upstream to check", () => {
    expect(toManagedSkill(summary({ id: "local-1", source_type: "local" })).update_status).toBe("local_only");
  });
});
