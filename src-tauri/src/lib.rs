mod coordinator;
mod credentials;
mod diagnostics;
mod engine;
mod settings;
mod snapshot;
mod ssh;
use coordinator::{Coordinator, RemoteEndpoint};
use engine::{CompareOptions, CompareResult, SyncResult};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use sync_core::endpoint::Stamp;
use tauri::{AppHandle, Manager, State};
#[derive(Default)]
struct TaskState {
    compare: Mutex<Option<Arc<AtomicBool>>>,
    sync: Mutex<Option<Arc<AtomicBool>>>,
}
fn begin(slot: &Mutex<Option<Arc<AtomicBool>>>) -> Arc<AtomicBool> {
    let mut s = slot.lock().unwrap();
    if let Some(old) = s.take() {
        old.store(true, Ordering::Relaxed)
    }
    let c = Arc::new(AtomicBool::new(false));
    *s = Some(c.clone());
    c
}
fn remote(path: String, endpoint: Option<RemoteEndpoint>) -> RemoteEndpoint {
    endpoint.unwrap_or(RemoteEndpoint::Filesystem { path })
}
#[tauri::command]
async fn compare_dirs(
    app: AppHandle,
    tasks: State<'_, TaskState>,
    local: String,
    remote: String,
    remote_endpoint: Option<RemoteEndpoint>,
    options: Option<CompareOptions>,
) -> Result<CompareResult, String> {
    let c = begin(&tasks.compare);
    let e = self::remote(remote, remote_endpoint);
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = coordinator::compare(
            &app,
            &app.state::<Coordinator>(),
            local,
            e,
            options.unwrap_or_default(),
            &c,
        );
        outcome
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn sync_entries(
    app: AppHandle,
    tasks: State<'_, TaskState>,
    comparison_id: String,
    selected_paths: Vec<String>,
    conflict_policy: String,
    concurrency: Option<usize>,
) -> Result<SyncResult, String> {
    let c = begin(&tasks.sync);
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = coordinator::sync(
            &app,
            &app.state::<Coordinator>(),
            &comparison_id,
            selected_paths,
            conflict_policy,
            concurrency.unwrap_or(4).clamp(1, 32),
            &c,
        );
        outcome
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn cancel_compare(tasks: State<'_, TaskState>) {
    if let Some(c) = tasks.compare.lock().unwrap().as_ref() {
        c.store(true, Ordering::Relaxed)
    }
}
#[tauri::command]
fn cancel_sync(tasks: State<'_, TaskState>) {
    if let Some(c) = tasks.sync.lock().unwrap().as_ref() {
        c.store(true, Ordering::Relaxed)
    }
}
#[tauri::command]
fn load_settings(app: AppHandle) -> settings::AppSettings {
    settings::load(&app)
}
#[tauri::command]
fn save_settings(app: AppHandle, settings: settings::AppSettings) -> Result<(), String> {
    settings::save(&app, &settings)
}
#[tauri::command]
async fn read_file_pair(
    app: AppHandle,
    local: String,
    remote: String,
    remote_endpoint: Option<RemoteEndpoint>,
    rel_path: String,
) -> Result<coordinator::TextPair, String> {
    let e = self::remote(remote, remote_endpoint);
    tauri::async_runtime::spawn_blocking(move || {
        coordinator::read_pair(&app, &local, &e, &rel_path)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn write_file_text(
    app: AppHandle,
    local: String,
    remote: String,
    remote_endpoint: Option<RemoteEndpoint>,
    rel_path: String,
    side: String,
    content: String,
    expected: Option<Stamp>,
) -> Result<Stamp, String> {
    let e = self::remote(remote, remote_endpoint);
    tauri::async_runtime::spawn_blocking(move || {
        coordinator::write(
            &app,
            &app.state::<Coordinator>(),
            &local,
            &e,
            &rel_path,
            &side,
            &content,
            &expected,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn compare_one_entry(
    app: AppHandle,
    local: String,
    remote: String,
    remote_endpoint: Option<RemoteEndpoint>,
    rel_path: String,
    options: Option<CompareOptions>,
) -> Result<Option<engine::DiffEntry>, String> {
    let e = self::remote(remote, remote_endpoint);
    tauri::async_runtime::spawn_blocking(move || {
        coordinator::compare_one(&app, &local, &e, &rel_path, &options.unwrap_or_default())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn profiles_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())?
        .join("ssh_profiles.json"))
}
#[tauri::command]
fn load_ssh_profiles(app: AppHandle) -> Result<Vec<ssh::SshProfile>, String> {
    let p = profiles_path(&app)?;
    if !p.exists() {
        return Ok(vec![]);
    }
    serde_json::from_slice(&std::fs::read(p).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
#[tauri::command]
fn save_ssh_profiles(app: AppHandle, profiles: Vec<ssh::SshProfile>) -> Result<(), String> {
    let p = profiles_path(&app)?;
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(
        p,
        serde_json::to_vec_pretty(&profiles).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
#[tauri::command]
async fn connect_ssh(
    app: AppHandle,
    profile: ssh::SshProfile,
    password: Option<String>,
) -> Result<ssh::ConnectionInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<Coordinator>();
        let _guard = state
            .lock
            .try_lock()
            .map_err(|_| "请先结束正在执行的目录任务")?;
        state.sessions.lock().unwrap().clear();
        let c = ssh::connect(&app, &app.state::<ssh::SshState>(), profile, password)?;
        let i = c.info.lock().unwrap().clone();
        Ok(i)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn ssh_auth_reply(
    state: State<'_, ssh::SshState>,
    id: String,
    answer: Option<String>,
) -> Result<(), String> {
    state
        .pending
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or("认证请求已过期")?
        .send(answer)
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn disconnect_ssh(app: AppHandle, id: String) -> Result<(), String> {
    let state = app.state::<Coordinator>();
    let _guard = state
        .lock
        .try_lock()
        .map_err(|_| "请先取消正在执行的任务")?;
    state.sessions.lock().unwrap().clear();
    app.state::<ssh::SshState>()
        .connections
        .lock()
        .unwrap()
        .remove(&id);
    Ok(())
}
#[tauri::command]
async fn deploy_ssh_agent(app: AppHandle, id: String) -> Result<ssh::ConnectionInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<Coordinator>();
        let _guard = state.lock.try_lock().map_err(|_| "请等待目录任务结束")?;
        let c = app
            .state::<ssh::SshState>()
            .connections
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or("请先连接 SSH")?;
        ssh::deploy(&app, &c)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn forget_ssh_password(profile: ssh::SshProfile) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        credentials::access(&ssh::credential_key(&profile), None, true).map(|_| ())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn browse_ssh(
    app: AppHandle,
    id: String,
    path: String,
) -> Result<sync_core::endpoint::Directory, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let c = app
            .state::<ssh::SshState>()
            .connections
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or("请先连接 SSH")?;
        let root = if path.is_empty() {
            c.info.lock().unwrap().home.clone()
        } else {
            path
        };
        serde_json::from_value(c.request(
            &root,
            sync_core::protocol::Operation::Browse,
            &AtomicBool::new(false),
            &mut |_| {},
        )?)
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[cfg(target_os = "linux")]
fn apply_webkit_workarounds() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
    }
    if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1")
    }
}
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    apply_webkit_workarounds();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(TaskState::default())
        .manage(Coordinator::default())
        .manage(ssh::SshState::default())
        .invoke_handler(tauri::generate_handler![
            compare_dirs,
            sync_entries,
            cancel_compare,
            cancel_sync,
            load_settings,
            save_settings,
            read_file_pair,
            write_file_text,
            compare_one_entry,
            load_ssh_profiles,
            save_ssh_profiles,
            connect_ssh,
            disconnect_ssh,
            ssh_auth_reply,
            deploy_ssh_agent,
            forget_ssh_password,
            browse_ssh
        ])
        .build(tauri::generate_context!())
        .expect("error building app")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                let tasks = app.state::<TaskState>();
                cancel_compare(tasks.clone());
                cancel_sync(tasks);
                app.state::<ssh::SshState>()
                    .connections
                    .lock()
                    .unwrap()
                    .clear();
            }
        });
}

#[cfg(feature = "integration-test")]
pub mod integration;
