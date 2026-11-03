import { useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  KeyRound,
  Loader2,
  Lock,
  Pencil,
  Plug,
  Plus,
  Server,
  Trash2,
  X,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useApp } from "../context/AppContext";
import { getErrorMessage } from "../lib/error";
import {
  deleteRemoteEnv,
  formatEnvAddress,
  installRemoteCli,
  LOCAL_ENV_ID,
  saveRemoteEnv,
  testRemoteEnv,
  type RemoteAuthMethod,
  type RemoteEnv,
  type RemoteEnvProbe,
} from "../lib/remoteApi";
import { cn } from "../utils";
import { ConfirmDialog } from "./ConfirmDialog";

interface Props {
  open: boolean;
  onClose: () => void;
}

interface FormState {
  id: string;
  name: string;
  host: string;
  port: string;
  user: string;
  identityFile: string;
  cliPath: string;
  auth: RemoteAuthMethod;
  password: string;
  /** The saved environment already has a password in the keychain. */
  hasSavedPassword: boolean;
}

const EMPTY_FORM: FormState = {
  id: "",
  name: "",
  host: "",
  port: "",
  user: "",
  identityFile: "",
  cliPath: "",
  auth: "key",
  password: "",
  hasSavedPassword: false,
};

function toForm(env: RemoteEnv): FormState {
  const auth = env.auth ?? "key";
  return {
    id: env.id,
    name: env.name,
    host: env.host,
    port: env.port ? String(env.port) : "",
    user: env.user ?? "",
    identityFile: env.identity_file ?? "",
    cliPath: env.cli_path ?? "",
    auth,
    password: "",
    hasSavedPassword: auth === "password",
  };
}

function toEnv(form: FormState): RemoteEnv {
  const port = Number.parseInt(form.port, 10);
  const isPassword = form.auth === "password";
  return {
    id: form.id,
    name: form.name.trim(),
    host: form.host.trim(),
    port: Number.isFinite(port) && port > 0 ? port : null,
    user: form.user.trim() || null,
    identity_file: isPassword ? null : form.identityFile.trim() || null,
    cli_path: form.cliPath.trim() || null,
    auth: form.auth,
    password: isPassword && form.password !== "" ? form.password : null,
  };
}

export function RemoteEnvDialog({ open, onClose }: Props) {
  const { t } = useTranslation();
  const { envs, activeEnvId, refreshEnvs, switchEnv } = useApp();
  const [form, setForm] = useState<FormState | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [probe, setProbe] = useState<RemoteEnvProbe | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<RemoteEnv | null>(null);

  if (!open) return null;

  const busy = saving || testing || installing;

  const handleClose = () => {
    if (busy) return;
    setForm(null);
    setProbe(null);
    onClose();
  };
  const canSubmit =
    !!form &&
    form.name.trim() !== "" &&
    form.host.trim() !== "" &&
    (form.auth === "key" ||
      (form.user.trim() !== "" && (form.password !== "" || form.hasSavedPassword)));

  const startEdit = (next: FormState) => {
    setForm(next);
    setProbe(null);
  };

  const update = (patch: Partial<FormState>) => {
    setForm((prev) => (prev ? { ...prev, ...patch } : prev));
  };

  const handleTest = async () => {
    if (!form) return;
    setTesting(true);
    setProbe(null);
    try {
      setProbe(await testRemoteEnv(toEnv(form)));
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setTesting(false);
    }
  };

  const handleInstall = async () => {
    if (!form) return;
    setInstalling(true);
    const toastId = toast.loading(t("remote.installing"));
    try {
      const next = await installRemoteCli(toEnv(form));
      toast.success(t("remote.installed", { version: next.cli_version ?? "?" }), { id: toastId });
      setProbe(next);
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")), { id: toastId });
    } finally {
      setInstalling(false);
    }
  };

  const handleSave = async () => {
    if (!form || !canSubmit) return;
    setSaving(true);
    try {
      const saved = await saveRemoteEnv(toEnv(form));
      await refreshEnvs();
      toast.success(t("remote.saved", { name: saved.name }));
      setForm(null);
      setProbe(null);
      // A changed host must take effect for the session already using it.
      if (saved.id === activeEnvId) await switchEnv(saved.id);
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (!deleteTarget) return;
    try {
      const wasActive = deleteTarget.id === activeEnvId;
      await deleteRemoteEnv(deleteTarget.id);
      await refreshEnvs();
      if (wasActive) await switchEnv(LOCAL_ENV_ID);
      if (form?.id === deleteTarget.id) setForm(null);
      toast.success(t("remote.deleted", { name: deleteTarget.name }));
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    }
  };

  const inputClass = "app-input h-9 w-full bg-background";
  const labelClass = "mb-1 block text-[13px] font-medium text-tertiary";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={handleClose} />
      <div className="relative flex max-h-[85vh] w-full max-w-[560px] flex-col rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-1 flex items-center justify-between">
          <h2 className="text-[14px] font-semibold text-primary">{t("remote.manageTitle")}</h2>
          <button
            onClick={handleClose}
            disabled={busy}
            className="rounded-md p-1 text-muted outline-none transition-colors hover:text-secondary disabled:opacity-50"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <p className="mb-4 text-[12px] text-muted">{t("remote.manageDescription")}</p>

        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto pr-1">
          {/* Saved environments */}
          <div className="space-y-1.5">
            {envs.length === 0 && !form && (
              <p className="rounded-lg border border-dashed border-border-subtle px-3 py-4 text-center text-[13px] text-muted">
                {t("remote.empty")}
              </p>
            )}
            {envs.map((env) => (
              <div
                key={env.id}
                className="flex items-center gap-2.5 rounded-lg border border-border-subtle bg-background px-3 py-2"
              >
                <Server className="h-4 w-4 shrink-0 text-muted" />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-[13px] font-medium text-secondary">
                    {env.name}
                    {env.id === activeEnvId && (
                      <span className="ml-2 rounded-full bg-accent-bg px-1.5 py-px text-[11px] font-medium text-accent-light">
                        {t("remote.active")}
                      </span>
                    )}
                  </p>
                  <p className="truncate text-[12px] text-muted">{formatEnvAddress(env)}</p>
                </div>
                <button
                  onClick={() => startEdit(toForm(env))}
                  disabled={busy}
                  className="rounded-md p-1 text-faint transition hover:text-secondary disabled:opacity-50"
                  title={t("common.edit")}
                >
                  <Pencil className="h-3.5 w-3.5" />
                </button>
                <button
                  onClick={() => setDeleteTarget(env)}
                  disabled={busy}
                  className="rounded-md p-1 text-faint transition hover:text-red-600 disabled:opacity-50 dark:hover:text-red-400"
                  title={t("common.delete")}
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </button>
              </div>
            ))}
            {!form && (
              <button
                onClick={() => startEdit(EMPTY_FORM)}
                className="flex w-full items-center gap-2 rounded-lg px-3 py-2 text-[13px] text-muted outline-none transition-colors hover:bg-surface-hover hover:text-secondary"
              >
                <Plus className="h-3.5 w-3.5" />
                {t("remote.add")}
              </button>
            )}
          </div>

          {/* Edit form */}
          {form && (
            <div className="space-y-3 rounded-xl border border-border-subtle bg-bg-secondary p-4">
              <h3 className="text-[13px] font-semibold text-primary">
                {form.id ? t("remote.editTitle") : t("remote.addTitle")}
              </h3>
              <div className="grid grid-cols-2 gap-3">
                <div className="col-span-2">
                  <label className={labelClass}>{t("remote.name")}</label>
                  <input
                    value={form.name}
                    onChange={(e) => update({ name: e.target.value })}
                    placeholder={t("remote.namePlaceholder")}
                    className={inputClass}
                    autoFocus
                  />
                </div>
                <div>
                  <label className={labelClass}>{t("remote.host")}</label>
                  <input
                    value={form.host}
                    onChange={(e) => update({ host: e.target.value })}
                    placeholder={t("remote.hostPlaceholder")}
                    className={inputClass}
                  />
                </div>
                <div>
                  <label className={labelClass}>{t("remote.port")}</label>
                  <input
                    value={form.port}
                    onChange={(e) => update({ port: e.target.value.replace(/\D/g, "").slice(0, 5) })}
                    placeholder="22"
                    inputMode="numeric"
                    className={inputClass}
                  />
                </div>
                <div className="col-span-2">
                  <label className={labelClass}>{t("remote.authMethod")}</label>
                  <div className="app-segmented bg-background">
                    {(
                      [
                        ["key", KeyRound, t("remote.authKey")],
                        ["password", Lock, t("remote.authPassword")],
                      ] as const
                    ).map(([method, Icon, label]) => (
                      <button
                        key={method}
                        type="button"
                        onClick={() => update({ auth: method })}
                        className={cn(
                          "app-segmented-button flex items-center gap-1.5",
                          form.auth === method
                            ? "bg-surface-active text-secondary"
                            : "text-muted hover:text-tertiary"
                        )}
                      >
                        <Icon className="h-3 w-3" /> {label}
                      </button>
                    ))}
                  </div>
                </div>
                <div>
                  <label className={labelClass}>{t("remote.user")}</label>
                  <input
                    value={form.user}
                    onChange={(e) => update({ user: e.target.value })}
                    placeholder={form.auth === "password" ? t("remote.required") : t("remote.optional")}
                    className={inputClass}
                  />
                </div>
                {form.auth === "key" ? (
                  <div>
                    <label className={labelClass}>{t("remote.identityFile")}</label>
                    <input
                      value={form.identityFile}
                      onChange={(e) => update({ identityFile: e.target.value })}
                      placeholder="~/.ssh/id_ed25519"
                      className={inputClass}
                    />
                  </div>
                ) : (
                  <div>
                    <label className={labelClass}>{t("remote.password")}</label>
                    <input
                      type="password"
                      autoComplete="new-password"
                      value={form.password}
                      onChange={(e) => update({ password: e.target.value })}
                      placeholder={form.hasSavedPassword ? t("remote.passwordKeep") : ""}
                      className={inputClass}
                    />
                  </div>
                )}
                {form.auth === "password" && (
                  <p className="col-span-2 -mt-1 text-[12px] text-muted">{t("remote.passwordHint")}</p>
                )}
                <div className="col-span-2">
                  <label className={labelClass}>{t("remote.cliPath")}</label>
                  <input
                    value={form.cliPath}
                    onChange={(e) => update({ cliPath: e.target.value })}
                    placeholder="~/.skills-manager/bin/skills-manager-cli"
                    className={inputClass}
                  />
                  <p className="mt-1 text-[12px] text-muted">{t("remote.cliPathHint")}</p>
                </div>
              </div>

              {probe && <ProbeResult probe={probe} />}

              <div className="flex flex-wrap items-center justify-end gap-2 pt-1">
                <button
                  onClick={handleTest}
                  disabled={!canSubmit || busy}
                  className="app-button-secondary h-[34px] px-3 py-0"
                >
                  {testing ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Plug className="h-3.5 w-3.5" />}
                  {t("remote.test")}
                </button>
                <button
                  onClick={handleInstall}
                  disabled={!canSubmit || busy}
                  className="app-button-secondary h-[34px] px-3 py-0"
                  title={t("remote.installHint")}
                >
                  {installing ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Download className="h-3.5 w-3.5" />}
                  {probe?.cli_version ? t("remote.upgradeCli") : t("remote.installCli")}
                </button>
                <div className="flex-1" />
                <button
                  onClick={() => {
                    setForm(null);
                    setProbe(null);
                  }}
                  disabled={busy}
                  className="rounded-lg px-3 py-1.5 text-[13px] font-medium text-tertiary outline-none transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={handleSave}
                  disabled={!canSubmit || busy}
                  className="app-button-primary h-[34px] px-3 py-0"
                >
                  {saving ? t("common.loading") : t("common.save")}
                </button>
              </div>
            </div>
          )}
        </div>
      </div>

      <ConfirmDialog
        open={deleteTarget !== null}
        message={t("remote.deleteConfirm", { name: deleteTarget?.name ?? "" })}
        onClose={() => setDeleteTarget(null)}
        onConfirm={handleDelete}
      />
    </div>
  );
}

function ProbeResult({ probe }: { probe: RemoteEnvProbe }) {
  const { t } = useTranslation();
  const rows: { ok: boolean; warn?: boolean; text: string }[] = [
    { ok: true, text: t("remote.probe.connected", { platform: probe.platform ?? "?" }) },
  ];
  if (!probe.cli_asset) {
    rows.push({ ok: false, text: t("remote.probe.platformUnsupported") });
  }
  if (probe.glibc_supported === false) {
    rows.push({
      ok: false,
      text: t("remote.probe.glibcTooOld", {
        version: probe.glibc_version,
        required: probe.glibc_required,
      }),
    });
  } else if (probe.glibc_version) {
    rows.push({ ok: true, text: t("remote.probe.glibc", { version: probe.glibc_version }) });
  }
  if (!probe.cli_version) {
    rows.push({ ok: false, warn: true, text: t("remote.probe.cliMissing") });
  } else if (probe.cli_outdated) {
    rows.push({
      ok: false,
      warn: true,
      text: t("remote.cliOutdated", { version: probe.cli_version, appVersion: probe.app_version }),
    });
  } else {
    rows.push({
      ok: true,
      text: t("remote.probe.cliReady", { version: probe.cli_version, path: probe.cli_path ?? "" }),
    });
  }

  return (
    <div className="space-y-1 rounded-lg border border-border-subtle bg-surface px-3 py-2">
      {rows.map((row, index) => (
        <p
          key={index}
          className={
            row.ok
              ? "flex items-start gap-1.5 text-[12px] text-emerald-600 dark:text-emerald-400"
              : row.warn
                ? "flex items-start gap-1.5 text-[12px] text-amber-600 dark:text-amber-400"
                : "flex items-start gap-1.5 text-[12px] text-red-600 dark:text-red-400"
          }
        >
          {row.ok ? (
            <CheckCircle2 className="mt-px h-3.5 w-3.5 shrink-0" />
          ) : (
            <AlertTriangle className="mt-px h-3.5 w-3.5 shrink-0" />
          )}
          <span className="break-all">{row.text}</span>
        </p>
      ))}
    </div>
  );
}
