//! SSH remote development environments.
//!
//! A remote environment keeps its own library on the remote host and is
//! driven through that host's `skills-manager-cli --json`. Nothing here reads
//! or writes remote files directly: every operation is a CLI invocation over
//! ssh. Key-based environments use the system `ssh`, so `~/.ssh/config`,
//! ssh-agent and keys work unchanged; password environments go through the
//! bundled libssh2 because the system client cannot take a password from a
//! GUI process on every platform.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::AppError;
use super::skill_store::SkillStore;

pub const LOCAL_ENV_ID: &str = "local";
const ENVS_SETTING_KEY: &str = "remote_envs";
const ACTIVE_SETTING_KEY: &str = "active_env_id";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a password session waits on one blocking read before giving up,
/// standing in for the system client's ServerAlive checks.
const SESSION_TIMEOUT_MS: u32 = 10 * 60 * 1000;

const RELEASE_BASE_URL: &str = "https://github.com/xmm-prio/skills-manager/releases";
const DEFAULT_REMOTE_CLI_PATH: &str = "~/.skills-manager/bin/skills-manager-cli";

const CLI_MISSING_MARKER: &str = "__SM_CLI_MISSING__";
const EXIT_MARKER: &str = "__SM_EXIT__";
const UNAME_MARKER: &str = "__SM_UNAME__";
const LDD_MARKER: &str = "__SM_LDD__";
const CLI_PATH_MARKER: &str = "__SM_CLI_PATH__";
const CLI_VERSION_MARKER: &str = "__SM_CLI_VERSION__";
const UPLOAD_SIZE_MARKER: &str = "__SM_UPLOAD_SIZE__";

/// Remote hosts often reach GitHub only through a proxy set in a login shell
/// profile, which a non-interactive `ssh host cmd` never loads. The CLI is
/// therefore fetched here, through the app's own proxy setting, and uploaded.
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Top-level CLI command groups the app is allowed to drive remotely.
const ALLOWED_COMMAND_GROUPS: &[&str] = &["repo", "agents", "tools", "skills", "presets", "projects"];

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// System `ssh`: keys, ssh-agent and `~/.ssh/config`.
    #[default]
    Key,
    /// Bundled libssh2 with a password kept in the OS keychain.
    Password,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEnv {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Host name, IP, or (key auth only) a `Host` alias from `~/.ssh/config`.
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
    #[serde(default)]
    pub auth: AuthMethod,
    /// Proxy for the remote CLI's own network access (Git and skills.sh
    /// installs, update checks). A non-interactive ssh session does not load
    /// the proxy variables a login shell sets, so it is passed explicitly.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Only ever travels from the UI to the backend: never written to the
    /// settings table and never sent back. Empty on save keeps the stored one.
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
}

impl std::fmt::Debug for RemoteEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEnv")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("identity_file", &self.identity_file)
            .field("cli_path", &self.cli_path)
            .field("auth", &self.auth)
            .field("proxy_url", &self.proxy_url.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
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
///
/// The password goes to the OS keychain before the settings row is written,
/// so a keychain failure never leaves a password environment without one.
pub fn save_env(store: &SkillStore, env: RemoteEnv) -> Result<RemoteEnv, AppError> {
    let mut env = normalize_env(env)?;
    let password = env.password.take();
    let mut envs = list_envs(store)?;

    let previous_auth = if env.id.is_empty() {
        None
    } else {
        Some(
            envs.iter()
                .find(|e| e.id == env.id)
                .map(|e| e.auth)
                .ok_or_else(|| {
                    AppError::not_found(format!("Remote environment not found: {}", env.id))
                })?,
        )
    };

    if env.auth == AuthMethod::Password && password.is_none() {
        let has_stored = match previous_auth {
            Some(AuthMethod::Password) => load_password(&env.id)?.is_some(),
            _ => false,
        };
        if !has_stored {
            return Err(AppError::invalid_input("Password is required for password login"));
        }
    }

    if env.id.is_empty() {
        env.id = uuid::Uuid::new_v4().to_string();
        envs.push(env.clone());
    } else if let Some(existing) = envs.iter_mut().find(|e| e.id == env.id) {
        *existing = env.clone();
    }

    match (env.auth, password) {
        (AuthMethod::Password, Some(password)) => store_password(&env.id, &password)?,
        (AuthMethod::Key, _) if previous_auth == Some(AuthMethod::Password) => {
            delete_password(&env.id)?
        }
        _ => {}
    }
    write_envs(store, &envs)?;
    Ok(env)
}

/// Delete an environment; switches back to local if it was active.
pub fn delete_env(store: &SkillStore, id: &str) -> Result<(), AppError> {
    let mut envs = list_envs(store)?;
    let Some(index) = envs.iter().position(|env| env.id == id) else {
        return Err(AppError::not_found(format!(
            "Remote environment not found: {id}"
        )));
    };
    let removed = envs.remove(index);
    write_envs(store, &envs)?;
    if removed.auth == AuthMethod::Password {
        if let Err(err) = delete_password(id) {
            log::warn!("remote env {id}: failed to remove keychain password: {err}");
        }
    }
    if get_active_env_id(store)? == id {
        set_active_env_id(store, LOCAL_ENV_ID)?;
    }
    Ok(())
}

// ── Keychain ───────────────────────────────────────────────────────────────

#[cfg(not(test))]
mod keychain {
    use super::AppError;

    const KEYRING_SERVICE: &str = "skills-manager-remote-ssh";

    fn entry(env_id: &str) -> Result<keyring::Entry, AppError> {
        keyring::Entry::new(KEYRING_SERVICE, env_id)
            .map_err(|e| AppError::internal(format!("Failed to open keychain entry: {e}")))
    }

    pub fn set(env_id: &str, password: &str) -> Result<(), AppError> {
        entry(env_id)?.set_password(password).map_err(|e| {
            AppError::internal(format!("Failed to store SSH password in OS keychain: {e}"))
        })
    }

    pub fn get(env_id: &str) -> Result<Option<String>, AppError> {
        match entry(env_id)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AppError::internal(format!(
                "Failed to read SSH password from OS keychain: {e}"
            ))),
        }
    }

    pub fn delete(env_id: &str) -> Result<(), AppError> {
        match entry(env_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AppError::internal(format!(
                "Failed to remove SSH password from OS keychain: {e}"
            ))),
        }
    }
}

/// keyring's mock store does not share credentials between `Entry` values,
/// so tests use a process-wide map instead of the real OS keychain.
#[cfg(test)]
mod keychain {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use super::AppError;

    fn map() -> &'static Mutex<HashMap<String, String>> {
        static MAP: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        MAP.get_or_init(Default::default)
    }

    pub fn set(env_id: &str, password: &str) -> Result<(), AppError> {
        map().lock().unwrap().insert(env_id.into(), password.into());
        Ok(())
    }

    pub fn get(env_id: &str) -> Result<Option<String>, AppError> {
        Ok(map().lock().unwrap().get(env_id).cloned())
    }

    pub fn delete(env_id: &str) -> Result<(), AppError> {
        map().lock().unwrap().remove(env_id);
        Ok(())
    }
}

fn store_password(env_id: &str, password: &str) -> Result<(), AppError> {
    keychain::set(env_id, password)
}

fn load_password(env_id: &str) -> Result<Option<String>, AppError> {
    keychain::get(env_id)
}

fn delete_password(env_id: &str) -> Result<(), AppError> {
    keychain::delete(env_id)
}

/// The password typed into the form wins; otherwise the saved environment's.
fn resolve_password(env: &RemoteEnv) -> Result<String, AppError> {
    if let Some(password) = &env.password {
        return Ok(password.clone());
    }
    let stored = if env.id.is_empty() {
        None
    } else {
        load_password(&env.id)?
    };
    stored.ok_or_else(|| {
        AppError::invalid_input("No password is saved for this environment; edit it and enter one")
    })
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
    if env.auth == AuthMethod::Password && user.is_none() {
        return Err(AppError::invalid_input("User is required for password login"));
    }
    let proxy_url = trimmed(env.proxy_url);
    if let Some(proxy) = &proxy_url {
        validate_proxy_url(proxy)?;
    }

    // Passwords are kept verbatim: surrounding spaces may be part of them.
    let password = match env.auth {
        AuthMethod::Password => env.password.filter(|p| !p.is_empty()),
        AuthMethod::Key => None,
    };

    Ok(RemoteEnv {
        id: env.id.trim().to_string(),
        name,
        host,
        port: env.port,
        user,
        identity_file,
        cli_path,
        auth: env.auth,
        proxy_url,
        password,
    })
}

fn validate_proxy_url(proxy: &str) -> Result<(), AppError> {
    let invalid = || {
        AppError::invalid_input(format!(
            "Invalid proxy URL: {proxy} (expected e.g. http://127.0.0.1:7890 or socks5://host:1080)"
        ))
    };
    if proxy.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(proxy).map_err(|_| invalid())?;
    let scheme_ok = matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h");
    if !scheme_ok || url.host_str().is_none() {
        return Err(invalid());
    }
    Ok(())
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

/// Exports the proxy for every tool the CLI may reach the network through:
/// `SKILLS_MANAGER_PROXY` for the CLI itself (it also covers libgit2, which
/// ignores the standard variables), the rest for git and HTTP clients.
fn proxy_exports(proxy_url: Option<&str>) -> String {
    let Some(proxy) = proxy_url else {
        return String::new();
    };
    let value = shell_quote(proxy);
    let assignments: Vec<String> = [
        "SKILLS_MANAGER_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "https_proxy",
        "http_proxy",
        "all_proxy",
    ]
    .iter()
    .map(|name| format!("{name}={value}"))
    .collect();
    format!("export {}\n", assignments.join(" "))
}

/// Runs each CLI invocation in turn, merging its stdout and stderr and
/// closing each with an exit-code marker line.
pub fn build_exec_script(
    cli_path: Option<&str>,
    proxy_url: Option<&str>,
    commands: &[Vec<String>],
) -> String {
    let mut script = proxy_exports(proxy_url);
    script.push_str(&resolve_cli_script(cli_path));
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

/// Writes the CLI arriving on stdin into place, then probes again in the same
/// session so the caller needs no third connection to show the result.
fn build_install_script(cli_path: Option<&str>, expected_size: usize) -> String {
    let dest = remote_path_expr(cli_path.unwrap_or(DEFAULT_REMOTE_CLI_PATH));
    format!(
        r#"set -e
DEST={dest}
mkdir -p "$(dirname "$DEST")"
TMP="$DEST.upload.$$"
trap 'rm -f "$TMP"' EXIT
cat > "$TMP"
SIZE=$(wc -c < "$TMP" | tr -d ' ')
if [ "$SIZE" != "{expected_size}" ]; then echo "{UPLOAD_SIZE_MARKER}$SIZE"; exit 4; fi
chmod +x "$TMP"
"$TMP" --version >/dev/null
mv -f "$TMP" "$DEST"
set +e
{probe}"#,
        probe = build_probe_script(cli_path),
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
    run_ssh_with_input(env, script, None)
}

/// `input`, when given, is streamed to the remote script's stdin and then
/// closed, so the script can `cat` it into a file.
fn run_ssh_with_input(
    env: &RemoteEnv,
    script: &str,
    input: Option<&[u8]>,
) -> Result<SshOutput, AppError> {
    match env.auth {
        AuthMethod::Key => run_system_ssh(env, script, input),
        AuthMethod::Password => run_password_ssh(env, script, input),
    }
}

fn run_system_ssh(
    env: &RemoteEnv,
    script: &str,
    input: Option<&[u8]>,
) -> Result<SshOutput, AppError> {
    let mut cmd = Command::new(ssh_program());
    cmd.args(ssh_args(env, script))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::not_found("ssh was not found on this computer; install an OpenSSH client")
        } else {
            AppError::io(format!("Failed to start ssh: {e}"))
        }
    })?;
    // Written from a thread so a full stdout pipe cannot deadlock the upload.
    let writer = match (input, child.stdin.take()) {
        (Some(data), Some(mut stdin)) => {
            let data = data.to_vec();
            Some(std::thread::spawn(move || stdin.write_all(&data)))
        }
        _ => None,
    };
    let output = child
        .wait_with_output()
        .map_err(|e| AppError::io(format!("Failed to run ssh: {e}")))?;
    let write_result = writer.map(|handle| handle.join());
    let result = SshOutput {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    };
    // ssh itself exits 255 on connection, auth and host key failures.
    if result.status == Some(255) {
        return Err(connection_error(&result.stderr));
    }
    match write_result {
        Some(Ok(Err(e))) => Err(AppError::network(format!(
            "SSH session closed while uploading: {e}{}",
            stderr_suffix(&result.stderr)
        ))),
        Some(Err(_)) => Err(AppError::internal("ssh upload thread panicked")),
        _ => Ok(result),
    }
}

fn stderr_suffix(stderr: &str) -> String {
    let detail = stderr.trim();
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
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
        " (check the user, identity file or ssh-agent, or switch this environment to password login)"
    } else {
        ""
    };
    AppError::network(format!("SSH connection failed: {detail}{hint}"))
}

// ── Password login (libssh2) ───────────────────────────────────────────────

fn ssh2_error(stage: &str, err: ssh2::Error) -> AppError {
    AppError::network(format!("SSH connection failed: {stage}: {err}"))
}

fn run_password_ssh(
    env: &RemoteEnv,
    script: &str,
    input: Option<&[u8]>,
) -> Result<SshOutput, AppError> {
    let user = env
        .user
        .as_deref()
        .ok_or_else(|| AppError::invalid_input("User is required for password login"))?;
    let password = resolve_password(env)?;
    let port = env.port.unwrap_or(22);
    let known_hosts = known_hosts_path()?;
    let entries = read_known_hosts(&known_hosts);

    let mut session = open_session(&env.host, port, None)?;
    if let HostKeyCheck::RetryWith(types) =
        verify_host_key(&session, &env.host, port, &entries, &known_hosts)?
    {
        // The host is on record under another key type: negotiate that one
        // rather than trusting whatever new key this connection offered.
        let prefs = types
            .iter()
            .flat_map(|t| host_key_algorithms(t))
            .copied()
            .collect::<Vec<_>>()
            .join(",");
        session = open_session(&env.host, port, Some(&prefs)).map_err(|err| {
            AppError::network(format!(
                "{}; known_hosts lists {} only with key types this client cannot use ({})",
                err.message,
                env.host,
                types.join(", ")
            ))
        })?;
        if let HostKeyCheck::RetryWith(_) =
            verify_host_key(&session, &env.host, port, &entries, &known_hosts)?
        {
            return Err(AppError::network(format!(
                "SSH connection failed: {} did not offer a host key type recorded in known_hosts",
                env.host
            )));
        }
    }
    authenticate(&session, user, &password)?;

    session.set_timeout(SESSION_TIMEOUT_MS);
    let mut channel = session
        .channel_session()
        .map_err(|e| ssh2_error("open channel", e))?;
    channel
        .exec(&format!("sh -c {}", shell_quote(script)))
        .map_err(|e| ssh2_error("exec", e))?;
    if let Some(data) = input {
        channel
            .write_all(data)
            .map_err(|e| AppError::network(format!("SSH session closed while uploading: {e}")))?;
    }
    channel.send_eof().map_err(|e| ssh2_error("send eof", e))?;
    let mut stdout = Vec::new();
    channel
        .read_to_end(&mut stdout)
        .map_err(|e| AppError::network(format!("SSH session failed while reading output: {e}")))?;
    let mut stderr = Vec::new();
    channel
        .stderr()
        .read_to_end(&mut stderr)
        .map_err(|e| AppError::network(format!("SSH session failed while reading output: {e}")))?;
    channel.wait_close().map_err(|e| ssh2_error("close", e))?;
    let status = channel.exit_status().map_err(|e| ssh2_error("exit status", e))?;

    Ok(SshOutput {
        status: Some(status),
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    })
}

/// Plain `password` first; servers that route passwords through PAM often
/// offer only `keyboard-interactive`, which gets the same password per prompt.
fn authenticate(session: &ssh2::Session, user: &str, password: &str) -> Result<(), AppError> {
    let methods = session
        .auth_methods(user)
        .map_err(|e| ssh2_error("authentication", e))?
        .to_string();
    if session.authenticated() {
        return Ok(());
    }
    let offered = |name: &str| methods.split(',').any(|m| m.trim() == name);

    if offered("password") && session.userauth_password(user, password).is_ok() {
        return Ok(());
    }
    if offered("keyboard-interactive") {
        struct Prompter<'a>(&'a str);
        impl ssh2::KeyboardInteractivePrompt for Prompter<'_> {
            fn prompt<'b>(
                &mut self,
                _username: &str,
                _instructions: &str,
                prompts: &[ssh2::Prompt<'b>],
            ) -> Vec<String> {
                prompts.iter().map(|_| self.0.to_string()).collect()
            }
        }
        if session
            .userauth_keyboard_interactive(user, &mut Prompter(password))
            .is_ok()
            && session.authenticated()
        {
            return Ok(());
        }
    }
    if !offered("password") && !offered("keyboard-interactive") {
        return Err(AppError::network(format!(
            "SSH connection failed: the server does not accept password login (offers: {methods})"
        )));
    }
    Err(AppError::network(
        "SSH connection failed: Permission denied (wrong user or password)",
    ))
}

fn known_hosts_path() -> Result<PathBuf, AppError> {
    dirs::home_dir()
        .map(|home| home.join(".ssh").join("known_hosts"))
        .ok_or_else(|| AppError::internal("Cannot locate the home directory for known_hosts"))
}

/// `host` on port 22, `[host]:port` otherwise — the OpenSSH known_hosts form.
fn known_host_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// TCP connect plus SSH handshake, optionally pinning the host key algorithms.
fn open_session(
    host: &str,
    port: u16,
    host_key_prefs: Option<&str>,
) -> Result<ssh2::Session, AppError> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| AppError::network(format!("SSH connection failed: cannot resolve {host}: {e}")))?
        .collect();
    let mut last_err = None;
    let mut tcp = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        AppError::network(format!(
            "SSH connection failed: cannot connect to {host}:{port}: {}",
            last_err.map_or_else(|| "no address".to_string(), |e| e.to_string())
        ))
    })?;

    let mut session = ssh2::Session::new().map_err(|e| ssh2_error("session", e))?;
    session.set_tcp_stream(tcp);
    session.set_timeout(CONNECT_TIMEOUT.as_millis() as u32);
    if let Some(prefs) = host_key_prefs {
        session
            .method_pref(ssh2::MethodType::HostKey, prefs)
            .map_err(|e| ssh2_error("host key algorithms", e))?;
    }
    session.handshake().map_err(|e| ssh2_error("handshake", e))?;
    Ok(session)
}

/// Key type names as they appear in the second known_hosts column.
const KNOWN_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
    "ssh-dss",
];

fn host_key_type_name(kind: ssh2::HostKeyType) -> Option<&'static str> {
    match kind {
        ssh2::HostKeyType::Rsa => Some("ssh-rsa"),
        ssh2::HostKeyType::Dss => Some("ssh-dss"),
        ssh2::HostKeyType::Ecdsa256 => Some("ecdsa-sha2-nistp256"),
        ssh2::HostKeyType::Ecdsa384 => Some("ecdsa-sha2-nistp384"),
        ssh2::HostKeyType::Ecdsa521 => Some("ecdsa-sha2-nistp521"),
        ssh2::HostKeyType::Ed25519 => Some("ssh-ed25519"),
        _ => None,
    }
}

/// Negotiation names for a known_hosts key type; RSA keys sign with SHA-2.
fn host_key_algorithms(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "ssh-rsa" => &["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"],
        "ssh-ed25519" => &["ssh-ed25519"],
        "ecdsa-sha2-nistp256" => &["ecdsa-sha2-nistp256"],
        "ecdsa-sha2-nistp384" => &["ecdsa-sha2-nistp384"],
        "ecdsa-sha2-nistp521" => &["ecdsa-sha2-nistp521"],
        "ssh-dss" => &["ssh-dss"],
        _ => &[],
    }
}

/// `(key type, line)` for each plain known_hosts entry. `@cert-authority` and
/// `@revoked` lines are skipped: libssh2 cannot evaluate them.
fn read_known_hosts(path: &Path) -> Vec<(String, String)> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_known_hosts(&content)
}

fn parse_known_hosts(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('@'))
        .filter_map(|line| {
            let key_type = line.split_whitespace().nth(1)?;
            Some((key_type.to_string(), line.to_string()))
        })
        .collect()
}

/// libssh2 compares a host's first entry regardless of key type, so each
/// check only sees entries of one type to avoid a false "key changed".
fn check_known_host(
    session: &ssh2::Session,
    entries: &[(String, String)],
    key_type: &str,
    host: &str,
    port: u16,
    key: &[u8],
) -> Result<ssh2::CheckResult, AppError> {
    let mut known = session
        .known_hosts()
        .map_err(|e| ssh2_error("known_hosts", e))?;
    for (_, line) in entries.iter().filter(|(t, _)| t == key_type) {
        let _ = known.read_str(line, ssh2::KnownHostFileKind::OpenSSH);
    }
    Ok(known.check_port(host, port, key))
}

enum HostKeyCheck {
    Trusted,
    /// Unknown under the negotiated type but recorded under these types.
    RetryWith(Vec<&'static str>),
}

/// Trust on first use: a known key must match, an unknown host is appended
/// to `known_hosts` so the system `ssh` trusts it too.
fn verify_host_key(
    session: &ssh2::Session,
    host: &str,
    port: u16,
    entries: &[(String, String)],
    known_hosts: &Path,
) -> Result<HostKeyCheck, AppError> {
    let (key, kind) = session
        .host_key()
        .ok_or_else(|| AppError::network("SSH connection failed: server sent no host key"))?;
    let type_name = host_key_type_name(kind)
        .ok_or_else(|| AppError::network("SSH connection failed: unsupported host key type"))?;
    let fingerprint = session
        .host_key_hash(ssh2::HashType::Sha256)
        .map(|hash| {
            format!(
                "SHA256:{}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash)
            )
        })
        .unwrap_or_else(|| "unknown".to_string());

    match check_known_host(session, entries, type_name, host, port, key)? {
        ssh2::CheckResult::Match => Ok(HostKeyCheck::Trusted),
        ssh2::CheckResult::Mismatch => Err(AppError::network(format!(
            "SSH connection failed: the host key for {host} has changed (now {fingerprint}). \
             If the server was reinstalled, remove its old entry from {}; otherwise someone may be intercepting the connection",
            known_hosts.display()
        ))),
        ssh2::CheckResult::NotFound => {
            // Our key cannot equal a key of another type, so a mismatch there
            // just means the host is recorded under that type.
            let mut recorded = Vec::new();
            for other in KNOWN_KEY_TYPES.iter().filter(|t| **t != type_name) {
                if matches!(
                    check_known_host(session, entries, other, host, port, key)?,
                    ssh2::CheckResult::Mismatch
                ) {
                    recorded.push(*other);
                }
            }
            if !recorded.is_empty() {
                return Ok(HostKeyCheck::RetryWith(recorded));
            }
            let line = format!(
                "{} {type_name} {}\n",
                known_host_name(host, port),
                base64::engine::general_purpose::STANDARD.encode(key)
            );
            append_known_host(known_hosts, &line)?;
            log::info!("trusted new host key for {host}:{port} ({fingerprint})");
            Ok(HostKeyCheck::Trusted)
        }
        ssh2::CheckResult::Failure => Err(AppError::network(
            "SSH connection failed: could not check the host key against known_hosts",
        )),
    }
}

fn append_known_host(path: &Path, line: &str) -> Result<(), AppError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::io(format!("Failed to create {}: {e}", dir.display())))?;
    }
    let needs_newline = std::fs::read(path)
        .map(|bytes| bytes.last().is_some_and(|b| *b != b'\n'))
        .unwrap_or(false);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| AppError::io(format!("Failed to open {}: {e}", path.display())))?;
    if needs_newline {
        file.write_all(b"\n")
            .map_err(|e| AppError::io(format!("Failed to update {}: {e}", path.display())))?;
    }
    file.write_all(line.as_bytes())
        .map_err(|e| AppError::io(format!("Failed to update {}: {e}", path.display())))
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

    let script = build_exec_script(env.cli_path.as_deref(), env.proxy_url.as_deref(), commands);
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

/// Executable header check, so an HTML error page is never installed.
fn looks_like_executable(bytes: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"\x7fELF",
        &[0xcf, 0xfa, 0xed, 0xfe],
        &[0xfe, 0xed, 0xfa, 0xcf],
        &[0xca, 0xfe, 0xba, 0xbe],
    ];
    MAGICS.iter().any(|magic| bytes.starts_with(magic))
}

/// The release CLI for `asset`, preferring this app's version and falling
/// back to the latest release. Pinned downloads are cached under `cache_dir`
/// so installing on several hosts downloads each build once.
pub fn fetch_release_cli(
    asset: &str,
    version: &str,
    proxy_url: Option<&str>,
    cache_dir: &Path,
) -> Result<Vec<u8>, AppError> {
    let cached = cache_dir.join(format!("v{version}")).join(asset);
    if let Ok(bytes) = std::fs::read(&cached) {
        if looks_like_executable(&bytes) {
            return Ok(bytes);
        }
    }

    let mut builder = reqwest::blocking::Client::builder()
        .user_agent("skills-manager")
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT);
    if let Some(proxy) = proxy_url.filter(|p| !p.is_empty()) {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| AppError::invalid_input(format!("Invalid proxy URL: {e}")))?;
        builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(AppError::internal)?;

    let download = |url: &str| -> Result<Option<Vec<u8>>, AppError> {
        let resp = client.get(url).send().map_err(|e| download_error(asset, &e))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status().map_err(|e| download_error(asset, &e))?;
        let bytes = resp.bytes().map_err(|e| download_error(asset, &e))?;
        Ok(Some(bytes.to_vec()))
    };

    let pinned_url = format!("{RELEASE_BASE_URL}/download/v{version}/{asset}");
    let (bytes, pinned) = match download(&pinned_url)? {
        Some(bytes) => (bytes, true),
        None => {
            let latest_url = format!("{RELEASE_BASE_URL}/latest/download/{asset}");
            let bytes = download(&latest_url)?.ok_or_else(|| {
                AppError::not_found(format!("{asset} is not published in {RELEASE_BASE_URL}"))
            })?;
            (bytes, false)
        }
    };
    if !looks_like_executable(&bytes) {
        return Err(AppError::network(format!(
            "Downloaded {asset} is not an executable; a proxy or captive portal may have replaced it"
        )));
    }

    if pinned {
        if let Some(dir) = cached.parent() {
            let tmp = cached.with_extension("part");
            let _ = std::fs::remove_file(&cached);
            let saved = std::fs::create_dir_all(dir)
                .and_then(|_| std::fs::write(&tmp, &bytes))
                .and_then(|_| std::fs::rename(&tmp, &cached));
            if let Err(err) = saved {
                log::warn!("remote CLI cache: failed to save {}: {err}", cached.display());
            }
        }
    }
    Ok(bytes)
}

fn download_error(asset: &str, err: &reqwest::Error) -> AppError {
    let hint = if err.is_timeout() || err.is_connect() {
        ". If this computer reaches GitHub through a proxy, set it under Settings → Network proxy"
    } else {
        ""
    };
    AppError::network(format!("Failed to download {asset}: {err}{hint}"))
}

/// Install the release CLI matching the remote platform and this app's
/// version: download it here, upload it over ssh. Returns a fresh probe.
pub fn install_cli(
    env: &RemoteEnv,
    proxy_url: Option<&str>,
    cache_dir: &Path,
) -> Result<RemoteEnvProbe, AppError> {
    let before = probe(env)?;
    let platform = before.platform.clone().unwrap_or_default();
    let asset = before.cli_asset.clone().ok_or_else(|| {
        AppError::invalid_input(format!(
            "No skills-manager-cli build is available for remote platform '{platform}'"
        ))
    })?;

    let bytes = fetch_release_cli(&asset, app_version(), proxy_url, cache_dir)?;
    let script = build_install_script(env.cli_path.as_deref(), bytes.len());
    let output = run_ssh_with_input(env, &script, Some(&bytes))?;
    if output.status != Some(0) {
        let mut message = format!("Failed to install skills-manager-cli ({asset})");
        if let Some(size) = marker_value(&output.stdout, UPLOAD_SIZE_MARKER) {
            message.push_str(&format!(
                ": upload incomplete ({size} of {} bytes arrived)",
                bytes.len()
            ));
        } else if before.glibc_supported == Some(false) {
            message.push_str(&format!(
                ": remote glibc {} is older than the required {}",
                before.glibc_version.as_deref().unwrap_or("?"),
                before.glibc_required.as_deref().unwrap_or("?"),
            ));
        }
        message.push_str(&stderr_suffix(&output.stderr));
        return Err(AppError::network(message));
    }
    let after = parse_probe(&output.stdout, app_version());
    if after.cli_version.is_none() {
        return Err(AppError::internal("Installed CLI did not report a version"));
    }
    Ok(after)
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
            auth: AuthMethod::Key,
            proxy_url: None,
            password: None,
        }
    }

    fn password_env() -> RemoteEnv {
        RemoteEnv {
            id: String::new(),
            identity_file: None,
            auth: AuthMethod::Password,
            password: Some(" s3cret ".into()),
            ..env()
        }
    }

    fn test_store() -> (tempfile::TempDir, SkillStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();
        (tmp, store)
    }

    #[test]
    fn password_never_reaches_serialized_settings_or_debug() {
        let env = password_env();
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("s3cret"));
        assert!(json.contains("\"auth\":\"password\""));
        assert!(!format!("{env:?}").contains("s3cret"));

        let legacy: RemoteEnv =
            serde_json::from_str(r#"{"id":"e1","name":"dev","host":"h"}"#).unwrap();
        assert_eq!(legacy.auth, AuthMethod::Key);
    }

    #[test]
    fn normalize_handles_password_fields() {
        let normalized = normalize_env(password_env()).unwrap();
        assert_eq!(normalized.password.as_deref(), Some(" s3cret "));

        let mut blank = password_env();
        blank.password = Some(String::new());
        assert_eq!(normalize_env(blank).unwrap().password, None);

        let mut no_user = password_env();
        no_user.user = None;
        assert!(normalize_env(no_user).is_err());

        let mut key = env();
        key.password = Some("leftover".into());
        assert_eq!(normalize_env(key).unwrap().password, None);
    }

    #[test]
    fn save_env_keeps_password_in_keychain_only() {
        let (_tmp, store) = test_store();
        let saved = save_env(&store, password_env()).unwrap();
        assert!(saved.password.is_none());
        assert_eq!(load_password(&saved.id).unwrap().as_deref(), Some(" s3cret "));
        let raw = store.get_setting(ENVS_SETTING_KEY).unwrap().unwrap();
        assert!(!raw.contains("s3cret"));

        // A blank password on edit keeps the stored one.
        let mut edit = saved.clone();
        edit.name = "renamed".into();
        save_env(&store, edit).unwrap();
        assert_eq!(load_password(&saved.id).unwrap().as_deref(), Some(" s3cret "));
        assert_eq!(resolve_password(&find_env(&store, &saved.id).unwrap()).unwrap(), " s3cret ");

        // Switching to key auth drops the stored password.
        let mut to_key = find_env(&store, &saved.id).unwrap();
        to_key.auth = AuthMethod::Key;
        save_env(&store, to_key).unwrap();
        assert_eq!(load_password(&saved.id).unwrap(), None);
    }

    #[test]
    fn save_env_requires_a_password_for_password_login() {
        let (_tmp, store) = test_store();
        let mut missing = password_env();
        missing.password = None;
        assert!(save_env(&store, missing).is_err());

        let key_env = save_env(&store, RemoteEnv { id: String::new(), ..env() }).unwrap();
        let mut switched = key_env.clone();
        switched.auth = AuthMethod::Password;
        assert!(save_env(&store, switched).is_err());
    }

    #[test]
    fn delete_env_removes_keychain_password() {
        let (_tmp, store) = test_store();
        let saved = save_env(&store, password_env()).unwrap();
        delete_env(&store, &saved.id).unwrap();
        assert_eq!(load_password(&saved.id).unwrap(), None);
    }

    #[test]
    fn known_host_names_follow_openssh_format() {
        assert_eq!(known_host_name("dev.example.com", 22), "dev.example.com");
        assert_eq!(known_host_name("10.0.0.5", 2222), "[10.0.0.5]:2222");
    }

    #[test]
    fn known_hosts_entries_are_grouped_by_key_type() {
        let content = "# comment\n\
            dev.example.com ssh-ed25519 AAAAC3\n\
            |1|c2FsdA==|aGFzaA== ecdsa-sha2-nistp256 AAAAE2\n\
            @cert-authority *.example.com ssh-rsa AAAAB3\n\
            \n\
            [10.0.0.5]:2222 ssh-rsa AAAAB3 comment\n";
        let entries = parse_known_hosts(content);
        let types: Vec<&str> = entries.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(types, ["ssh-ed25519", "ecdsa-sha2-nistp256", "ssh-rsa"]);
        assert_eq!(entries[2].1, "[10.0.0.5]:2222 ssh-rsa AAAAB3 comment");
        assert!(KNOWN_KEY_TYPES.iter().all(|t| !host_key_algorithms(t).is_empty()));
    }

    #[test]
    fn append_known_host_keeps_existing_lines_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".ssh").join("known_hosts");
        append_known_host(&path, "a ssh-ed25519 AAAA\n").unwrap();
        std::fs::write(&path, "a ssh-ed25519 AAAA").unwrap();
        append_known_host(&path, "b ssh-ed25519 BBBB\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a ssh-ed25519 AAAA\nb ssh-ed25519 BBBB\n"
        );
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
            None,
            &[vec!["skills".into(), "install".into(), "it's; rm -rf ~".into()]],
        );
        assert!(script.contains("\"$CLI\" --json 'skills' 'install' 'it'\\''s; rm -rf ~'"));
        assert!(script.contains(CLI_MISSING_MARKER));
        assert_eq!(script.matches(EXIT_MARKER).count(), 1);
        assert!(!script.contains("PROXY"));
    }

    #[test]
    fn exec_script_exports_the_environment_proxy_before_the_cli_runs() {
        let script = build_exec_script(
            None,
            Some("http://u:p'w@10.0.0.1:3128"),
            &[vec!["skills".into(), "check".into()]],
        );
        let export_line = script.lines().next().unwrap();
        assert!(export_line.starts_with("export "));
        for name in ["SKILLS_MANAGER_PROXY", "HTTPS_PROXY", "http_proxy", "all_proxy"] {
            assert!(export_line.contains(&format!("{name}='http://u:p'\\''w@10.0.0.1:3128'")));
        }
        assert!(script.find("export ").unwrap() < script.find("\"$CLI\" --json").unwrap());
    }

    #[test]
    fn proxy_urls_are_validated_and_trimmed() {
        let with_proxy = |proxy: &str| RemoteEnv {
            proxy_url: Some(proxy.into()),
            ..env()
        };
        let saved = normalize_env(with_proxy("  socks5h://proxy.lan:1080 ")).unwrap();
        assert_eq!(saved.proxy_url.as_deref(), Some("socks5h://proxy.lan:1080"));
        assert_eq!(normalize_env(with_proxy("   ")).unwrap().proxy_url, None);
        for bad in ["127.0.0.1:7890", "ftp://proxy:21", "http://", "http://a b:1"] {
            assert!(normalize_env(with_proxy(bad)).is_err(), "{bad} should be rejected");
        }
        assert!(!format!("{:?}", with_proxy("http://u:secret@p:1")).contains("secret"));
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
        assert!(err.message.contains("password login"));
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
    fn install_script_takes_the_cli_from_stdin_and_reprobes() {
        let script = build_install_script(None, 1234);
        assert!(script.contains("DEST=\"$HOME\"/'.skills-manager/bin/skills-manager-cli'"));
        assert!(script.contains("cat > \"$TMP\""));
        assert!(script.contains("\"$SIZE\" != \"1234\""));
        assert!(!script.contains("curl") && !script.contains("wget"));
        // The probe runs after `set +e`, in the same session as the upload.
        let reprobe = script.find("set +e").unwrap();
        assert!(script[reprobe..].contains(UNAME_MARKER));
        assert!(script[reprobe..].contains(CLI_VERSION_MARKER));
    }

    #[test]
    fn install_output_parses_as_a_fresh_probe() {
        let stdout = "__SM_UNAME__Linux x86_64\n__SM_LDD__ldd (GNU libc) 2.35\n__SM_CLI_PATH__/home/a/.skills-manager/bin/skills-manager-cli\n__SM_CLI_VERSION__skills-manager-cli 1.43.5\n";
        let probe = parse_probe(stdout, "1.43.5");
        assert_eq!(probe.cli_version.as_deref(), Some("1.43.5"));
        assert!(!probe.cli_outdated);
    }

    #[test]
    fn cached_cli_is_used_without_downloading() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = tmp.path().join("v9.9.9").join("skills-manager-cli-Linux-x64");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"\x7fELF-cached").unwrap();
        // An unroutable proxy proves no request is made.
        let bytes = fetch_release_cli(
            "skills-manager-cli-Linux-x64",
            "9.9.9",
            Some("http://127.0.0.1:9"),
            tmp.path(),
        )
        .unwrap();
        assert_eq!(bytes, b"\x7fELF-cached");
    }

    #[test]
    fn only_executables_pass_the_header_check() {
        assert!(looks_like_executable(b"\x7fELF\x02\x01"));
        assert!(looks_like_executable(&[0xcf, 0xfa, 0xed, 0xfe, 7]));
        assert!(!looks_like_executable(b"<!DOCTYPE html>"));
        assert!(!looks_like_executable(b""));
    }

    #[test]
    fn exec_rejects_unknown_command_groups() {
        let err = exec(&env(), &[vec!["git".into(), "push".into()]]).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
    }
}
