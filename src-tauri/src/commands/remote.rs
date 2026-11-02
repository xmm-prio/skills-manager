use std::sync::Arc;
use std::time::Instant;

use tauri::State;

use crate::core::{
    error::AppError,
    remote_env::{self, RemoteEnv, RemoteEnvProbe},
    skill_store::SkillStore,
};

#[tauri::command]
pub async fn list_remote_envs(
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<RemoteEnv>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || remote_env::list_envs(&store)).await?
}

#[tauri::command]
pub async fn save_remote_env(
    env: RemoteEnv,
    store: State<'_, Arc<SkillStore>>,
) -> Result<RemoteEnv, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let saved = remote_env::save_env(&store, env)?;
        log::info!("remote env saved: {} ({})", saved.name, saved.id);
        Ok(saved)
    })
    .await?
}

#[tauri::command]
pub async fn delete_remote_env(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        remote_env::delete_env(&store, &id)?;
        log::info!("remote env deleted: {id}");
        Ok(())
    })
    .await?
}

#[tauri::command]
pub async fn get_active_env(store: State<'_, Arc<SkillStore>>) -> Result<String, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || remote_env::get_active_env_id(&store)).await?
}

#[tauri::command]
pub async fn set_active_env(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        remote_env::set_active_env_id(&store, &id)?;
        log::info!("active env switched to {id}");
        Ok(())
    })
    .await?
}

/// Takes the unsaved form values so a connection can be tested before saving.
#[tauri::command]
pub async fn test_remote_env(env: RemoteEnv) -> Result<RemoteEnvProbe, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let env = remote_env::normalize_env(env)?;
        let started = Instant::now();
        let probe = remote_env::probe(&env)?;
        log::info!(
            "remote env probe {}: platform={:?} cli={:?} in {} ms",
            env.host,
            probe.platform,
            probe.cli_version,
            started.elapsed().as_millis()
        );
        Ok(probe)
    })
    .await?
}

#[tauri::command]
pub async fn install_remote_cli(env: RemoteEnv) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let env = remote_env::normalize_env(env)?;
        let version = remote_env::install_cli(&env)?;
        log::info!("remote CLI {version} installed on {}", env.host);
        Ok(version)
    })
    .await?
}

/// Runs CLI invocations on a saved environment in one ssh session. Each
/// result is `{"ok":true,"data":…}` or `{"ok":false,"error":{…}}`.
#[tauri::command]
pub async fn remote_cli_exec(
    env_id: String,
    commands: Vec<Vec<String>>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<serde_json::Value>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let env = remote_env::find_env(&store, &env_id)?;
        let started = Instant::now();
        let labels: Vec<String> = commands
            .iter()
            .map(|args| args.iter().take(2).cloned().collect::<Vec<_>>().join(" "))
            .collect();
        let result = remote_env::exec(&env, &commands);
        let elapsed_ms = started.elapsed().as_millis();
        match &result {
            Ok(_) => log::info!(
                "remote exec on {}: [{}] in {elapsed_ms} ms",
                env.host,
                labels.join(", ")
            ),
            Err(err) => log::warn!(
                "remote exec on {} failed after {elapsed_ms} ms: [{}]: {err}",
                env.host,
                labels.join(", ")
            ),
        }
        result
    })
    .await?
}
