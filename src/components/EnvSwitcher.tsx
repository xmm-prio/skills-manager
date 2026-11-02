import { useEffect, useState } from "react";
import { AlertTriangle, ChevronDown, Loader2, Monitor, Server, Settings2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useApp } from "../context/AppContext";
import { getErrorMessage } from "../lib/error";
import { formatEnvAddress, LOCAL_ENV_ID, testRemoteEnv, type RemoteEnvProbe } from "../lib/remoteApi";
import { RemoteEnvDialog } from "./RemoteEnvDialog";

export function EnvSwitcher() {
  const { t } = useTranslation();
  const { envs, activeEnvId, activeEnv, isRemote, envError, loading, switchEnv } = useApp();
  const [switching, setSwitching] = useState(false);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [probe, setProbe] = useState<{ envId: string; result: RemoteEnvProbe } | null>(null);

  // One probe per switch, for the CLI version line and the outdated warning.
  useEffect(() => {
    if (!activeEnv) return;
    let cancelled = false;
    testRemoteEnv(activeEnv)
      .then((result) => {
        if (!cancelled) setProbe({ envId: activeEnv.id, result });
      })
      .catch(() => {
        // The data load reports connection failures through envError.
      });
    return () => {
      cancelled = true;
    };
  }, [activeEnv]);

  const activeProbe = probe && probe.envId === activeEnvId ? probe.result : null;

  const handleSwitch = async (id: string) => {
    if (id === activeEnvId) return;
    setSwitching(true);
    try {
      await switchEnv(id);
      const name = id === LOCAL_ENV_ID ? t("remote.local") : envs.find((env) => env.id === id)?.name ?? id;
      toast.success(t("remote.switched", { name }));
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setSwitching(false);
    }
  };

  const busy = switching || loading;
  const Icon = isRemote ? Server : Monitor;

  let status: { text: string; className: string };
  if (!isRemote) {
    status = { text: t("remote.localDescription"), className: "text-muted" };
  } else if (busy) {
    status = { text: t("remote.connecting"), className: "text-muted" };
  } else if (envError) {
    status = { text: envError, className: "text-red-600 dark:text-red-400" };
  } else {
    const version = activeProbe?.cli_version
      ? t("remote.cliVersion", { version: activeProbe.cli_version })
      : "";
    status = {
      text: [t("remote.connected"), version].filter(Boolean).join(" · "),
      className: "text-emerald-600 dark:text-emerald-400",
    };
  }

  return (
    <div className="app-panel flex flex-wrap items-center gap-3 px-4 py-3">
      <div
        className={
          isRemote
            ? "rounded-md border border-border-subtle bg-sky-500/[0.08] p-2 text-sky-600 dark:text-sky-400"
            : "rounded-md border border-border-subtle bg-accent-bg p-2 text-accent-light"
        }
      >
        {busy && isRemote ? <Loader2 className="h-4 w-4 animate-spin" /> : <Icon className="h-4 w-4" />}
      </div>

      <div className="min-w-0 flex-1">
        <p className="app-section-title mb-1">{t("remote.currentEnv")}</p>
        <p className={`truncate text-[12px] ${status.className}`} title={status.text}>
          {isRemote && activeEnv ? `${formatEnvAddress(activeEnv)} · ` : ""}
          {status.text}
        </p>
        {isRemote && activeProbe?.cli_outdated && (
          <p className="mt-1 flex items-center gap-1 text-[12px] text-amber-600 dark:text-amber-400">
            <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
            {t("remote.cliOutdated", {
              version: activeProbe.cli_version,
              appVersion: activeProbe.app_version,
            })}
          </p>
        )}
      </div>

      <div className="relative">
        <select
          value={activeEnvId}
          disabled={switching}
          onChange={(e) => handleSwitch(e.target.value)}
          aria-label={t("remote.currentEnv")}
          className="app-input h-[34px] min-w-[180px] cursor-pointer appearance-none bg-background pr-8 disabled:cursor-not-allowed disabled:opacity-50 dark:[color-scheme:dark]"
        >
          <option value={LOCAL_ENV_ID}>{t("remote.local")}</option>
          {envs.map((env) => (
            <option key={env.id} value={env.id}>
              {env.name}
            </option>
          ))}
        </select>
        <ChevronDown className="pointer-events-none absolute right-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
      </div>

      <button
        onClick={() => setDialogOpen(true)}
        className="app-button-secondary h-[34px] px-3 py-0"
      >
        <Settings2 className="h-3.5 w-3.5 text-tertiary" />
        {t("remote.manage")}
      </button>

      <RemoteEnvDialog open={dialogOpen} onClose={() => setDialogOpen(false)} />
    </div>
  );
}
