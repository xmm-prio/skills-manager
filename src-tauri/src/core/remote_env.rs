//! SSH remote development environments.
//!
//! A remote environment keeps its own library on the remote host and is
//! driven through that host's `skills-manager-cli --json`. Nothing here reads
//! or writes remote files directly: every operation is a CLI invocation over
//! the system `ssh`, so `~/.ssh/config`, ssh-agent and keys work unchanged.

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::AppError;
use super::skill_store::SkillStore;

pub const LOCAL_ENV_ID: &str = "local";
const ENVS_SETTING_KEY: &str = "remote_envs";
const ACTIVE_SETTING_KEY: &str = "active_env_id";

const RELEASE_BASE_URL: &str = "https://github.com/xingkongliang/skills-manager/releases";
const DEFAULT_REMOTE_CLI_PATH: &str = "~/.skills-manager/bin/skills-manager-cli";

const CLI_MISSING_MARKER: &str = "__SM_CLI_MISSING__";
const EXIT_MARKER: &str = "__SM_EXIT__";
const UNAME_MARKER: &str = "__SM_UNAME__";
const LDD_MARKER: &str = "__SM_LDD__";
const CLI_PATH_MARKER: &str = "__SM_CLI_PATH__";
const CLI_VERSION_MARKER: &str = "__SM_CLI_VERSION__";
const NO_DOWNLOADER_MARKER: &str = "__SM_NO_DOWNLOADER__";

/// Top-level CLI command groups the app is allowed to drive remotely.
const ALLOWED_COMMAND_GROUPS: &[&str] = &["repo", "agents", "tools", "skills", "presets"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEnv {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Host name, IP, or a `Host` alias from `~/.ssh/config`.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    /// Remote CLI location. Empty means: look in the usual install places.
    #[serde(default)]
    pub cli_path: Option<String>,
}

/// What a connection test found on the remote host.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteEnvProbe {
    /// `uname -sm`, e.g. `Linux x86_64`.
    pub platform: Option<String>,
    /// Release asset matching the platform; `None` when no build exists for it.
    pub cli_asset: Option<String>,
    pub glibc_version: Option<String>,
    /// Minimum glibc the matching Linux build needs; `None` off Linux.
    pub glibc_required: Option<String>,
    /// `Some(false)` when the remote glibc is older than the build needs.
    pub glibc_supported: Option<bool>,
    pub cli_path: Option<String>,
    pub cli_version: Option<String>,
    pub app_version: String,
    /// The remote CLI is older than this app.
    pub cli_outdated: bool,
}

// ── Storage ────────────────────────────────────────────────────────────────

pub fn list_envs(store: &SkillStore) -> Result<Vec<RemoteEnv>, AppError> {
    let raw = store.get_setting(ENVS_SETTING_KEY).map_err(AppError::db)?;
    match raw {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw)
            .map_err(|e| AppError::internal(format!("Invalid remote environment settings: {e}"))),
        _ => Ok(Vec::new()),
    }
}

fn write_envs(store: &SkillStore, envs: &[RemoteEnv]) -> Result<(), AppError> {
    let raw = serde_json::to_string(envs).map_err(AppError::internal)?;
    store.set_setting(ENVS_SETTING_KEY, &raw).map_err(AppError::db)
}

pub fn find_env(store: &SkillStore, id: &str) -> Result<RemoteEnv, AppError> {
    list_envs(store)?
        .into_iter()
        .find(|env| env.id == id)
        .ok_or_else(|| AppError::not_found(format!("Remote environment not found: {id}")))
}

/// Insert or update an environment. A blank id creates a new one.
pub fn save_env(store: &SkillStore, env: RemoteEnv) -> Result<RemoteEnv, AppError> {
    let mut env = normalize_env(env)?;
    let mut envs = list_envs(store)?;
    if env.id.is_empty() {
        env.id = uuid::Uuid::new_v4().to_string();
        envs.push(env.clone());
    } else if let Some(existing) = envs.iter_mut().find(|e| e.id == env.id) {
        *existing = env.clone();
    } else {
        return Err(AppError::not_found(format!(
            "Remote environment not found: {}",
            env.id
        )));
    }
    write_envs(store, &envs)?;
    Ok(env)
}

/// Delete an environment; switches back to local if it was active.
pub fn delete_env(store: &SkillStore, id: &str) -> Result<(), AppError> {
    let mut envs = list_envs(store)?;
    let before = envs.len();
    envs.retain(|env| env.id != id);
    if envs.len() == before {
        return Err(AppError::not_found(format!(
            "Remote environment not found: {id}"
        )));
    }
    write_envs(store, &envs)?;
    if get_active_env_id(store)? == id {
        set_active_env_id(store, LOCAL_ENV_ID)?;
    }
    Ok(())
}

/// The active environment id, falling back to local when the stored id no
/// longer names a saved environment.
pub fn get_active_env_id(store: &SkillStore) -> Result<String, AppError> {
    let stored = store
        .get_setting(ACTIVE_SETTING_KEY)
        .map_err(AppError::db)?
        .unwrap_or_default();
    if stored.is_empty() || stored == LOCAL_ENV_ID {
        return Ok(LOCAL_ENV_ID.to_string());
    }
    if list_envs(store)?.iter().any(|env| env.id == stored) {
        Ok(stored)
    } else {
        Ok(LOCAL_ENV_ID.to_string())
    }
}

pub fn set_active_env_id(store: &SkillStore, id: &str) -> Result<(), AppError> {
    if id != LOCAL_ENV_ID {
        find_env(store, id)?;
    }
    store
        .set_setting(ACTIVE_SETTING_KEY, id)
        .map_err(AppError::db)
}

fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Trim fields and reject values that could be read as ssh options.
pub fn normalize_env(env: RemoteEnv) -> Result<RemoteEnv, AppError> {
    let name = env.name.trim().to_string();
    let host = env.host.trim().to_string();
    let user = trimmed(env.user);
    let identity_file = trimmed(env.identity_file);
    let cli_path = trimmed(env.cli_path);

    if name.is_empty() {
        return Err(AppError::invalid_input("Environment name is required"));
    }
    if host.is_empty() {
        return Err(AppError::invalid_input("Host is required"));
    }
    if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AppError::invalid_input(format!("Invalid host: {host}")));
    }
    if let Some(user) = &user {
        if user.starts_with('-')
            || user.contains('@')
            || user.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(AppError::invalid_input(format!("Invalid user: {user}")));
        }
    }
    if let Some(identity) = &identity_file {
        if identity.starts_with('-') || identity.chars().any(char::is_control) {
            return Err(AppError::invalid_input(format!(
                "Invalid identity file: {identity}"
            )));
        }
    }
    if let Some(path) = &cli_path {
        if path.chars().any(char::is_control) {
            return Err(AppError::invalid_input(format!("Invalid CLI path: {path}")));
        }
    }
    if env.port == Some(0) {
        return Err(AppError::invalid_input("Port must be between 1 and 65535"));
    }

    Ok(RemoteEnv {
        id: env.id.trim().to_string(),
        name,
        host,
        port: env.port,
        user,
        identity_file,
        cli_path,
    })
}

// ── Shell script building ──────────────────────────────────────────────────

/// Quote a value for a POSIX shell. The `'\''` dance also holds in fish.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A remote path as a shell expression, expanding a leading `~/` to `$HOME`.
fn remote_path_expr(path: &str) -> String {
    if path == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("\"$HOME\"/{}", shell_quote(rest))
    } else {
        shell_quote(path)
    }
}

/// Sets `CLI` to the remote CLI path, or to an empty string when not found.
fn resolve_cli_script(cli_path: Option<&str>) -> String {
    match cli_path {
        Some(path) => format!("CLI={}\n[ -x \"$CLI\" ] || CLI=\n", remote_path_expr(path)),
        None => [
            "CLI=\"$HOME/.skills-manager/bin/skills-manager-cli\"",
            "[ -x \"$CLI\" ] || CLI=\"$HOME/.local/bin/skills-manager-cli\"",
            "[ -x \"$CLI\" ] || CLI=\"$HOME/.cargo/bin/skills-manager-cli\"",
            "[ -x \"$CLI\" ] || CLI=\"$(command -v skills-manager-cli 2>/dev/null)\"",
            "[ -n \"$CLI\" ] && [ -x \"$CLI\" ] || CLI=",
            "",
        ]
        .join("\n"),
    }
}

/// Runs each CLI invocation in turn, merging its stdout and stderr and
/// closing each with an exit-code marker line.
pub fn build_exec_script(cli_path: Option<&str>, commands: &[Vec<String>]) -> String {
    let mut script = resolve_cli_script(cli_path);
    script.push_str(&format!(
        "if [ -z \"$CLI\" ]; then echo {CLI_MISSING_MARKER}; exit 127; fi\n"
    ));
    for args in commands {
        let quoted: Vec<String> = args.iter().map(|arg| shell_quote(arg)).collect();
        script.push_str(&format!(
            "\"$CLI\" --json {} 2>&1 </dev/null\ncode=$?\nprintf '\\n{EXIT_MARKER}%s\\n' \"$code\"\n",
            quoted.join(" ")
        ));
    }
    script
}

fn build_probe_script(cli_path: Option<&str>) -> String {
    let mut script = String::new();
    script.push_str(&format!("echo \"{UNAME_MARKER}$(uname -sm 2>/dev/null)\"\n"));
    script.push_str(&format!(
        "echo \"{LDD_MARKER}$(ldd --version 2>&1 | head -n 1)\"\n"
    ));
    script.push_str(&resolve_cli_script(cli_path));
    script.push_str(&format!(
        "if [ -n \"$CLI\" ]; then echo \"{CLI_PATH_MARKER}$CLI\"; \
         echo \"{CLI_VERSION_MARKER}$(\"$CLI\" --version 2>/dev/null | head -n 1)\"; fi\n"
    ));
    script
}

fn build_install_script(cli_path: Option<&str>, asset: &str, app_version: &str) -> String {
    let dest = remote_path_expr(cli_path.unwrap_or(DEFAULT_REMOTE_CLI_PATH));
    let pinned_url = format!("{RELEASE_BASE_URL}/download/v{app_version}/{asset}");
    let latest_url = format!("{RELEASE_BASE_URL}/latest/download/{asset}");
    format!(
        r#"set -e
DEST={dest}
mkdir -p "$(dirname "$DEST")"
TMP="$DEST.download.$$"
trap 'rm -f "$TMP"' EXIT
fetch() {{
  if command -v curl >/dev/null 2>&1; then curl -fsSL --retry 2 -o "$TMP" "$1"
  elif command -v wget >/dev/null 2>&1; then wget -q -O "$TMP" "$1"
  else echo {NO_DOWNLOADER_MARKER}; exit 3
  fi
}}
fetch {pinned} || fetch {latest}
chmod +x "$TMP"
"$TMP" --version >/dev/null
mv -f "$TMP" "$DEST"
echo "{CLI_VERSION_MARKER}$("$DEST" --version | head -n 1)"
"#,
        pinned = shell_quote(&pinned_url),
        latest = shell_quote(&latest_url),
    )
}

// ── ssh invocation ─────────────────────────────────────────────────────────

fn ssh_program() -> String {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("SystemRoot") {
            let bundled = std::path::Path::new(&root).join("System32\\OpenSSH\\ssh.exe");
            if bundled.is_file() {
                return bundled.to_string_lossy().to_string();
            }
        }
    }
    "ssh".to_string()
}

/// Arguments for `ssh`, ending with the remote command that runs `script`
/// under `sh` regardless of the remote login shell.
pub fn ssh_args(env: &RemoteEnv, script: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // Windows OpenSSH has no ControlMaster support.
    #[cfg(not(target_os = "windows"))]
    if let Some(home) = dirs::home_dir() {
        if home.join(".ssh").is_dir() {
            args.extend([
                "-o".to_string(),
                "ControlMaster=auto".to_string(),
                "-o".to_string(),
                "ControlPath=~/.ssh/skills-manager-%C".to_string(),
                "-o".to_string(),
                "ControlPersist=120".to_string(),
            ]);
        }
    }

    if let Some(port) = env.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(identity) = &env.identity_file {
        args.push("-i".to_string());
        args.push(identity.clone());
    }
    if let Some(user) = &env.user {
        args.push("-l".to_string());
        args.push(user.clone());
    }
    args.push("--".to_string());
    args.push(env.host.clone());
    args.push(format!("sh -c {}", shell_quote(script)));
    args
}

struct SshOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_ssh(env: &RemoteEnv, script: &str) -> Result<SshOutput, AppError> {
    let mut cmd = Command::new(ssh_program());
    cmd.args(ssh_args(env, script))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("APPDIR").is_some() {
        cmd.env_remove("LD_LIBRARY_PATH");
    }

    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::not_found("ssh was not found on this computer; install an OpenSSH client")
        } else {
            AppError::io(format!("Failed to start ssh: {e}"))
        }
    })?;
    let result = SshOutput {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    };
    // ssh itself exits 255 on connection, auth and host key failures.
    if result.status == Some(255) {
        return Err(connection_error(&result.stderr));
    }
    Ok(result)
}

fn connection_error(stderr: &str) -> AppError {
    let detail = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .unwrap_or("connection failed");
    let lower = stderr.to_ascii_lowercase();
    let hint = if lower.contains("host key verification failed") {
        " (connect once with ssh in a terminal to trust the host key)"
    } else if lower.contains("permission denied") {
        " (key-based login is required; check the user, identity file or ssh-agent)"
    } else {
        ""
    };
    AppError::network(format!("SSH connection failed: {detail}{hint}"))
}

fn cli_missing_error() -> AppError {
    AppError::not_found("skills-manager-cli was not found on the remote host")
}

// ── Output parsing ─────────────────────────────────────────────────────────

/// Split exec output into per-command `(exit_code, lines)` segments.
fn split_segments(stdout: &str) -> Vec<(i32, Vec<String>)> {
    let mut segments = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if let Some(code) = line.trim().strip_prefix(EXIT_MARKER) {
            let code = code.trim().parse::<i32>().unwrap_or(1);
            segments.push((code, std::mem::take(&mut current)));
        } else {
            current.push(line.to_string());
        }
    }
    segments
}

fn error_kind_from_envelope(envelope: &Value) -> &'static str {
    if let Some(kind) = envelope.get("kind").and_then(Value::as_str) {
        let known = [
            "database",
            "io",
            "network",
            "git",
            "not_found",
            "invalid_input",
            "cancelled",
            "internal",
            "target_conflict",
        ];
        if let Some(found) = known.iter().find(|k| **k == kind) {
            return *found;
        }
    }
    match envelope.get("code").and_then(Value::as_str) {
        Some("INVALID_ARGUMENT") => "invalid_input",
        Some("TARGET_CONFLICT") => "target_conflict",
        _ => "internal",
    }
}

/// One command's outcome as `{"ok":true,"data":…}` or
/// `{"ok":false,"error":{"kind","message","details"?}}`.
pub fn parse_segment(code: i32, lines: &[String]) -> Value {
    let parsed: Vec<Value> = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();

    if code == 0 {
        if let Some(data) = parsed.into_iter().next() {
            return serde_json::json!({ "ok": true, "data": data });
        }
        return error_value("internal", "Remote CLI returned no JSON output", None);
    }

    let envelope = parsed
        .iter()
        .rev()
        .find(|value| value.get("ok") == Some(&Value::Bool(false)));
    match envelope {
        Some(envelope) => {
            let message = envelope
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Remote command failed")
                .to_string();
            error_value(
                error_kind_from_envelope(envelope),
                &message,
                envelope.get("details").cloned(),
            )
        }
        None => {
            let text = lines
                .iter()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            let message = if text.is_empty() {
                format!("Remote command exited with status {code}")
            } else {
                text
            };
            error_value("internal", &message, None)
        }
    }
}

fn error_value(kind: &str, message: &str, details: Option<Value>) -> Value {
    let mut error = serde_json::json!({ "kind": kind, "message": message });
    if let Some(details) = details {
        error["details"] = details;
    }
    serde_json::json!({ "ok": false, "error": error })
}

fn marker_value(stdout: &str, marker: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(marker))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Release asset name for a `uname -sm` platform string.
pub fn release_asset_for(platform: &str) -> Option<&'static str> {
    let mut parts = platform.split_whitespace();
    let os = parts.next()?;
    let arch = parts.next()?;
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Some("skills-manager-cli-Linux-x64"),
        ("Linux", "aarch64" | "arm64") => Some("skills-manager-cli-Linux-arm64"),
        ("Darwin", "arm64" | "aarch64") => Some("skills-manager-cli-macOS-arm64"),
        ("Darwin", "x86_64") => Some("skills-manager-cli-macOS-x64"),
        _ => None,
    }
}

/// Minimum glibc of each Linux release build (the glibc of its build runner).
fn required_glibc_for(asset: &str) -> Option<&'static str> {
    match asset {
        "skills-manager-cli-Linux-x64" => Some("2.35"),
        "skills-manager-cli-Linux-arm64" => Some("2.39"),
        _ => None,
    }
}

/// Extract the version from the first line of `ldd --version`.
pub fn parse_glibc_version(ldd_line: &str) -> Option<String> {
    let lower = ldd_line.to_ascii_lowercase();
    if lower.contains("musl") || !(lower.contains("glibc") || lower.contains("gnu libc")) {
        return None;
    }
    ldd_line
        .split_whitespace()
        .rev()
        .find(|token| {
            let mut pieces = token.split('.');
            pieces.next().is_some_and(|p| p.parse::<u32>().is_ok())
                && pieces.next().is_some_and(|p| p.parse::<u32>().is_ok())
        })
        .map(str::to_string)
}

fn version_tuple(version: &str) -> (u32, u32) {
    let mut pieces = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (pieces.next().unwrap_or(0), pieces.next().unwrap_or(0))
}

/// `skills-manager-cli 1.43.2` → `1.43.2`.
fn parse_cli_version(line: &str) -> Option<String> {
    line.split_whitespace()
        .last()
        .map(|v| v.trim_start_matches('v').to_string())
        .filter(|v| semver::Version::parse(v).is_ok())
}

fn is_outdated(cli_version: &str, app_version: &str) -> bool {
    match (
        semver::Version::parse(cli_version),
        semver::Version::parse(app_version),
    ) {
        (Ok(cli), Ok(app)) => cli < app,
        _ => false,
    }
}

pub fn parse_probe(stdout: &str, app_version: &str) -> RemoteEnvProbe {
    let platform = marker_value(stdout, UNAME_MARKER);
    let cli_asset = platform
        .as_deref()
        .and_then(release_asset_for)
        .map(str::to_string);
    let glibc_version = marker_value(stdout, LDD_MARKER)
        .as_deref()
        .and_then(parse_glibc_version);
    let glibc_required = cli_asset
        .as_deref()
        .and_then(required_glibc_for)
        .map(str::to_string);
    let glibc_supported = match (&glibc_version, &glibc_required) {
        (Some(have), Some(need)) => Some(version_tuple(have) >= version_tuple(need)),
        _ => None,
    };
    let cli_path = marker_value(stdout, CLI_PATH_MARKER);
    let cli_version = marker_value(stdout, CLI_VERSION_MARKER)
        .as_deref()
        .and_then(parse_cli_version);
    let cli_outdated = cli_version
        .as_deref()
        .is_some_and(|v| is_outdated(v, app_version));

    RemoteEnvProbe {
        platform,
        cli_asset,
        glibc_version,
        glibc_required,
        glibc_supported,
        cli_path,
        cli_version,
        app_version: app_version.to_string(),
        cli_outdated,
    }
}

// ── Operations ─────────────────────────────────────────────────────────────

pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Run CLI invocations on the remote host in one ssh session.
pub fn exec(env: &RemoteEnv, commands: &[Vec<String>]) -> Result<Vec<Value>, AppError> {
    if commands.is_empty() {
        return Ok(Vec::new());
    }
    for args in commands {
        let group = args.first().map(String::as_str).unwrap_or_default();
        if !ALLOWED_COMMAND_GROUPS.contains(&group) {
            return Err(AppError::invalid_input(format!(
                "Unsupported remote command group: {group}"
            )));
        }
    }

    let script = build_exec_script(env.cli_path.as_deref(), commands);
    let output = run_ssh(env, &script)?;
    if output.stdout.lines().any(|line| line.trim() == CLI_MISSING_MARKER) {
        return Err(cli_missing_error());
    }

    let segments = split_segments(&output.stdout);
    if segments.len() != commands.len() {
        let detail = output.stderr.trim();
        return Err(AppError::network(format!(
            "Remote session ended early ({} of {} commands finished){}",
            segments.len(),
            commands.len(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )));
    }
    Ok(segments
        .iter()
        .map(|(code, lines)| parse_segment(*code, lines))
        .collect())
}

pub fn probe(env: &RemoteEnv) -> Result<RemoteEnvProbe, AppError> {
    let output = run_ssh(env, &build_probe_script(env.cli_path.as_deref()))?;
    Ok(parse_probe(&output.stdout, app_version()))
}

/// Download the release CLI matching the remote platform and this app's
/// version. Returns the installed version.
pub fn install_cli(env: &RemoteEnv) -> Result<String, AppError> {
    let probe = probe(env)?;
    let platform = probe.platform.clone().unwrap_or_default();
    let asset = probe.cli_asset.clone().ok_or_else(|| {
        AppError::invalid_input(format!(
            "No skills-manager-cli build is available for remote platform '{platform}'"
        ))
    })?;

    let script = build_install_script(env.cli_path.as_deref(), &asset, app_version());
    let output = run_ssh(env, &script)?;
    if output.stdout.contains(NO_DOWNLOADER_MARKER) {
        return Err(AppError::not_found(
            "Neither curl nor wget is available on the remote host",
        ));
    }
    if output.status != Some(0) {
        let mut message = format!("Failed to install skills-manager-cli ({asset})");
        if probe.glibc_supported == Some(false) {
            message.push_str(&format!(
                ": remote glibc {} is older than the required {}",
                probe.glibc_version.as_deref().unwrap_or("?"),
                probe.glibc_required.as_deref().unwrap_or("?"),
            ));
        }
        let detail = output.stderr.trim();
        if !detail.is_empty() {
            message.push_str(&format!(": {detail}"));
        }
        return Err(AppError::network(message));
    }
    marker_value(&output.stdout, CLI_VERSION_MARKER)
        .as_deref()
        .and_then(parse_cli_version)
        .ok_or_else(|| AppError::internal("Installed CLI did not report a version"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::ErrorKind;

    fn env() -> RemoteEnv {
        RemoteEnv {
            id: "e1".into(),
            name: "dev".into(),
            host: "dev.example.com".into(),
            port: Some(2222),
            user: Some("alice".into()),
            identity_file: Some("~/.ssh/id_ed25519".into()),
            cli_path: None,
        }
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME; rm -rf /"), "'$HOME; rm -rf /'");
    }

    #[test]
    fn remote_path_expands_home_prefix_only() {
        assert_eq!(remote_path_expr("~/bin/cli"), "\"$HOME\"/'bin/cli'");
        assert_eq!(remote_path_expr("/opt/cli"), "'/opt/cli'");
        assert_eq!(remote_path_expr("~"), "\"$HOME\"");
    }

    #[test]
    fn ssh_args_put_options_before_host_and_command_last() {
        let args = ssh_args(&env(), "echo hi");
        let host_index = args.iter().position(|a| a == "dev.example.com").unwrap();
        assert_eq!(args[host_index - 1], "--");
        assert!(args.contains(&"BatchMode=yes".to_string()));
        let port_index = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[port_index + 1], "2222");
        let user_index = args.iter().position(|a| a == "-l").unwrap();
        assert_eq!(args[user_index + 1], "alice");
        assert!(port_index < host_index && user_index < host_index);
        assert_eq!(args.last().unwrap(), "sh -c 'echo hi'");
    }

    #[test]
    fn exec_script_quotes_every_argument() {
        let script = build_exec_script(
            None,
            &[vec!["skills".into(), "install".into(), "it's; rm -rf ~".into()]],
        );
        assert!(script.contains("\"$CLI\" --json 'skills' 'install' 'it'\\''s; rm -rf ~'"));
        assert!(script.contains(CLI_MISSING_MARKER));
        assert_eq!(script.matches(EXIT_MARKER).count(), 1);
    }

    #[test]
    fn normalize_rejects_option_like_values() {
        let mut bad = env();
        bad.host = "-oProxyCommand=evil".into();
        assert!(normalize_env(bad).is_err());

        let mut bad = env();
        bad.user = Some("-oProxyCommand=evil".into());
        assert!(normalize_env(bad).is_err());

        let mut bad = env();
        bad.host = "two words".into();
        assert!(normalize_env(bad).is_err());

        let mut blank = env();
        blank.user = Some("  ".into());
        blank.cli_path = Some("".into());
        let normalized = normalize_env(blank).unwrap();
        assert_eq!(normalized.user, None);
        assert_eq!(normalized.cli_path, None);
    }

    #[test]
    fn split_segments_pairs_output_with_exit_codes() {
        let stdout = "[{\"key\":\"codex\"}]\n\n__SM_EXIT__0\n{\"ok\":false,\"code\":\"COMMAND_FAILED\",\"message\":\"boom\"}\n\n__SM_EXIT__1\n";
        let segments = split_segments(stdout);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].0, 0);
        assert_eq!(segments[1].0, 1);
    }

    #[test]
    fn parse_segment_success_skips_warning_lines() {
        let lines = vec!["warning: something".to_string(), "[1,2]".to_string()];
        let value = parse_segment(0, &lines);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"], serde_json::json!([1, 2]));
    }

    #[test]
    fn parse_segment_null_output_is_success() {
        let value = parse_segment(0, &["null".to_string()]);
        assert_eq!(value["ok"], true);
        assert!(value["data"].is_null());
    }

    #[test]
    fn parse_segment_maps_error_envelopes() {
        let failed = parse_segment(
            1,
            &[r#"{"ok":false,"code":"COMMAND_FAILED","message":"skill not found: x","error":"skill not found: x"}"#.to_string()],
        );
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["error"]["kind"], "internal");
        assert_eq!(failed["error"]["message"], "skill not found: x");

        let invalid = parse_segment(
            2,
            &[r#"{"ok":false,"code":"INVALID_ARGUMENT","message":"bad flag"}"#.to_string()],
        );
        assert_eq!(invalid["error"]["kind"], "invalid_input");

        let conflict = parse_segment(
            1,
            &[r#"{"ok":false,"code":"TARGET_CONFLICT","kind":"target_conflict","message":"in the way","details":{"conflicts":[{"path":"/home/a/.claude/skills/db","reason":"foreign"}]}}"#.to_string()],
        );
        assert_eq!(conflict["error"]["kind"], "target_conflict");
        assert_eq!(
            conflict["error"]["details"]["conflicts"][0]["path"],
            "/home/a/.claude/skills/db"
        );
    }

    #[test]
    fn parse_segment_without_envelope_keeps_raw_text() {
        let value = parse_segment(126, &["sh: permission denied".to_string()]);
        assert_eq!(value["error"]["message"], "sh: permission denied");
    }

    #[test]
    fn connection_errors_get_actionable_hints() {
        let err = connection_error("Host key verification failed.\r\n");
        assert!(matches!(err.kind, ErrorKind::Network));
        assert!(err.message.contains("trust the host key"));
        let err = connection_error("alice@dev: Permission denied (publickey).");
        assert!(err.message.contains("key-based login"));
    }

    #[test]
    fn glibc_versions_are_parsed_and_compared() {
        assert_eq!(
            parse_glibc_version("ldd (Ubuntu GLIBC 2.35-0ubuntu3.8) 2.35").as_deref(),
            Some("2.35")
        );
        assert_eq!(
            parse_glibc_version("ldd (GNU libc) 2.17").as_deref(),
            Some("2.17")
        );
        assert_eq!(parse_glibc_version("musl libc (x86_64)"), None);
        assert!(version_tuple("2.39") >= version_tuple("2.35"));
        assert!(version_tuple("2.31") < version_tuple("2.35"));
    }

    #[test]
    fn probe_output_is_interpreted() {
        let stdout = "__SM_UNAME__Linux x86_64\n__SM_LDD__ldd (GNU libc) 2.31\n__SM_CLI_PATH__/home/a/.skills-manager/bin/skills-manager-cli\n__SM_CLI_VERSION__skills-manager-cli 1.40.0\n";
        let probe = parse_probe(stdout, "1.43.2");
        assert_eq!(probe.platform.as_deref(), Some("Linux x86_64"));
        assert_eq!(probe.cli_asset.as_deref(), Some("skills-manager-cli-Linux-x64"));
        assert_eq!(probe.glibc_supported, Some(false));
        assert_eq!(probe.cli_version.as_deref(), Some("1.40.0"));
        assert!(probe.cli_outdated);

        let missing = parse_probe("__SM_UNAME__Linux aarch64\n__SM_LDD__\n", "1.43.2");
        assert_eq!(missing.cli_asset.as_deref(), Some("skills-manager-cli-Linux-arm64"));
        assert_eq!(missing.cli_version, None);
        assert_eq!(missing.glibc_supported, None);
        assert!(!missing.cli_outdated);
    }

    #[test]
    fn release_assets_cover_published_platforms() {
        assert_eq!(release_asset_for("Darwin arm64"), Some("skills-manager-cli-macOS-arm64"));
        assert_eq!(release_asset_for("FreeBSD amd64"), None);
        assert_eq!(release_asset_for(""), None);
    }

    #[test]
    fn install_script_pins_app_version_with_latest_fallback() {
        let script = build_install_script(None, "skills-manager-cli-Linux-x64", "1.43.2");
        assert!(script.contains("/download/v1.43.2/skills-manager-cli-Linux-x64"));
        assert!(script.contains("/latest/download/skills-manager-cli-Linux-x64"));
        assert!(script.contains("DEST=\"$HOME\"/'.skills-manager/bin/skills-manager-cli'"));
    }

    #[test]
    fn exec_rejects_unknown_command_groups() {
        let err = exec(&env(), &[vec!["git".into(), "push".into()]]).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
    }
}
