//! HTTP endpoints for managing local scheduled tasks (`~/.jeikcode/schedules/`)
//! and inspecting their headless execution history (`SessionOrigin::Scheduled`).

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Mutex as StdMutex, OnceLock};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use jeikcode_capabilities::session::{
    CatalogEntry, SessionManager as NativeSessionManager, SessionOrigin,
};
use jeikcode_config::schedule::{
    self, build_task, next_run, valid_id, validate_task_fields, Schedule, ScheduleTask,
};
use jeikcode_config::schedule_os::{self, InstallState};
use serde::{Deserialize, Serialize};

use crate::{
    catalog_entry_with_project, catalog_scan_in_root, delete_session_checked_in_root,
    publish_global, run_session_catalog_io, AppState, GlobalWireEvent, SessionMetaWithProject,
    SessionMode,
};

static IN_FLIGHT_RUNS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

fn in_flight_runs() -> &'static StdMutex<HashSet<String>> {
    IN_FLIGHT_RUNS.get_or_init(|| StdMutex::new(HashSet::new()))
}

fn is_task_running(id: &str) -> bool {
    in_flight_runs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(id)
        || schedule::is_run_locked(id)
}

fn try_mark_running(id: &str) -> bool {
    in_flight_runs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string())
}

fn clear_running(id: &str) {
    in_flight_runs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
}

struct RunningGuard {
    id: String,
    active: bool,
}

impl RunningGuard {
    fn acquire(id: &str) -> Option<Self> {
        if try_mark_running(id) {
            Some(Self {
                id: id.to_string(),
                active: true,
            })
        } else {
            None
        }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if self.active {
            clear_running(&self.id);
        }
    }
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn current_platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

fn max_interval_minutes_for_platform() -> Option<u32> {
    if cfg!(target_os = "windows") {
        Some(1439)
    } else {
        None
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ScheduleTaskWire {
    #[serde(flatten)]
    pub task: ScheduleTask,
    pub next_run_at: Option<i64>,
    pub os_status: &'static str,
    pub is_running: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SchedulesListResponse {
    pub tasks: Vec<ScheduleTaskWire>,
    pub os_platform: &'static str,
    pub os_supported: bool,
    pub supports_cron: bool,
    pub max_interval_minutes: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateSchedulePayload {
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub cwd: Option<String>,
    pub schedule: Schedule,
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    #[serde(default = "default_notify")]
    pub notify: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateSchedulePayload {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub notify: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToggleSchedulePayload {
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn default_permission_mode() -> String {
    "plan".to_string()
}

fn default_notify() -> String {
    "important".to_string()
}

fn default_true() -> bool {
    true
}

fn err_response(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "success": false,
            "error": msg.into(),
        })),
    )
        .into_response()
}

fn reconcile_os_for_task(task: &ScheduleTask) -> Option<String> {
    let os = match schedule_os::current() {
        Ok(os) => os,
        Err(e) => return Some(format!("OS scheduler unavailable: {e}")),
    };
    if task.enabled {
        if let Err(e) = os.install(task) {
            return Some(format!("OS scheduler registration failed: {e}"));
        }
    } else if let Err(e) = os.uninstall(&task.id) {
        return Some(format!("OS scheduler unregistration failed: {e}"));
    }
    None
}

fn enrich_task_wire(
    task: ScheduleTask,
    now: i64,
    os: Option<&(dyn schedule_os::OsScheduler + Send + Sync)>,
) -> ScheduleTaskWire {
    let next_run_at = next_run(&task.schedule, now);
    let os_status = match os.map(|s| s.status(&task.id)) {
        Some(InstallState::Installed) => "registered",
        Some(InstallState::Missing) => "missing",
        None => "unknown",
    };
    let is_running = is_task_running(&task.id);
    ScheduleTaskWire {
        task,
        next_run_at,
        os_status,
        is_running,
    }
}

/// GET /schedules — List all local scheduled tasks with next-run & OS registration state.
pub(crate) async fn list_schedules() -> impl IntoResponse {
    let res = tokio::task::spawn_blocking(|| {
        let tasks = schedule::list();
        let os = schedule_os::current().ok();
        let now = now_epoch_secs();
        let os_supported = os.is_some();
        let wires: Vec<ScheduleTaskWire> = tasks
            .into_iter()
            .map(|t| enrich_task_wire(t, now, os.as_deref()))
            .collect();
        SchedulesListResponse {
            tasks: wires,
            os_platform: current_platform_name(),
            os_supported,
            supports_cron: false,
            max_interval_minutes: max_interval_minutes_for_platform(),
        }
    })
    .await;

    match res {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to list schedules: {e}"),
        ),
    }
}

/// POST /schedules — Create a new scheduled task and register it with the OS scheduler.
pub(crate) async fn create_schedule(
    State(state): State<AppState>,
    Json(payload): Json<CreateSchedulePayload>,
) -> impl IntoResponse {
    let default_cwd = {
        let proj = state.project.read().await;
        proj.working_dir.to_string_lossy().into_owned()
    };
    let cwd = payload
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(default_cwd);

    if let Err(e) = validate_task_fields(
        &payload.title,
        &payload.prompt,
        &cwd,
        &payload.schedule,
        &payload.permission_mode,
        &payload.notify,
    ) {
        return err_response(StatusCode::BAD_REQUEST, e.to_string());
    }

    let mut task = build_task(
        &payload.title,
        &payload.prompt,
        &cwd,
        payload.schedule,
        &payload.permission_mode,
        &payload.notify,
    );
    task.enabled = payload.enabled;

    let res = tokio::task::spawn_blocking(
        move || -> anyhow::Result<(ScheduleTaskWire, Option<String>)> {
            schedule::save(&task)?;
            let os_warning = if task.enabled {
                reconcile_os_for_task(&task)
            } else {
                None
            };
            let os = schedule_os::current().ok();
            let wire = enrich_task_wire(task, now_epoch_secs(), os.as_deref());
            Ok((wire, os_warning))
        },
    )
    .await;

    match res {
        Ok(Ok((wire, os_warning))) => {
            publish_global(GlobalWireEvent::SchedulesUpdated);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "success": true,
                    "task": wire,
                    "os_warning": os_warning,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save schedule: {e}"),
        ),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Schedule task panicked: {e}"),
        ),
    }
}

/// PUT /schedules/:id — Update an existing scheduled task and reconcile OS registration.
pub(crate) async fn update_schedule(
    Path(id): Path<String>,
    Json(payload): Json<UpdateSchedulePayload>,
) -> impl IntoResponse {
    if !valid_id(&id) {
        return err_response(StatusCode::BAD_REQUEST, format!("invalid task id {id:?}"));
    }

    let res = tokio::task::spawn_blocking(
        move || -> Result<(ScheduleTaskWire, Option<String>), (StatusCode, String)> {
            #[derive(Debug)]
            enum UpdateErr {
                Io(std::io::Error),
                Validation(String),
            }
            impl From<std::io::Error> for UpdateErr {
                fn from(e: std::io::Error) -> Self {
                    Self::Io(e)
                }
            }
            let task = schedule::update_task(&id, |task| -> Result<(), UpdateErr> {
                if let Some(title) = payload.title {
                    task.title = title.trim().to_string();
                }
                if let Some(prompt) = payload.prompt {
                    task.prompt = prompt;
                }
                if let Some(cwd) = payload.cwd {
                    task.cwd = cwd.trim().to_string();
                }
                if let Some(sched) = payload.schedule {
                    task.schedule = sched;
                }
                if let Some(mode) = payload.permission_mode {
                    task.permission_mode = mode;
                }
                if let Some(notify) = payload.notify {
                    task.notify = notify;
                }
                if let Some(enabled) = payload.enabled {
                    task.enabled = enabled;
                }

                validate_task_fields(
                    &task.title,
                    &task.prompt,
                    &task.cwd,
                    &task.schedule,
                    &task.permission_mode,
                    &task.notify,
                )
                .map_err(|e| UpdateErr::Validation(e.to_string()))?;
                Ok(())
            })
            .map_err(|err| match err {
                UpdateErr::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (StatusCode::NOT_FOUND, format!("task {id:?} not found"))
                }
                UpdateErr::Io(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to update task: {e}"),
                ),
                UpdateErr::Validation(msg) => (StatusCode::BAD_REQUEST, msg),
            })?;

            let os_warning = reconcile_os_for_task(&task);
            let os = schedule_os::current().ok();
            let wire = enrich_task_wire(task, now_epoch_secs(), os.as_deref());
            Ok((wire, os_warning))
        },
    )
    .await;

    match res {
        Ok(Ok((wire, os_warning))) => {
            publish_global(GlobalWireEvent::SchedulesUpdated);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "task": wire,
                    "os_warning": os_warning,
                })),
            )
                .into_response()
        }
        Ok(Err((status, msg))) => err_response(status, msg),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Schedule update failed: {e}"),
        ),
    }
}

/// POST /schedules/:id/toggle — Enable or disable a scheduled task.
pub(crate) async fn toggle_schedule(
    Path(id): Path<String>,
    body: Option<Json<ToggleSchedulePayload>>,
) -> impl IntoResponse {
    if !valid_id(&id) {
        return err_response(StatusCode::BAD_REQUEST, format!("invalid task id {id:?}"));
    }
    let desired_enabled = body.and_then(|Json(p)| p.enabled);

    let res = tokio::task::spawn_blocking(
        move || -> Result<(ScheduleTaskWire, Option<String>), (StatusCode, String)> {
            let task = schedule::update_task(&id, |task| -> std::io::Result<()> {
                task.enabled = desired_enabled.unwrap_or(!task.enabled);
                Ok(())
            })
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    (StatusCode::NOT_FOUND, format!("task {id:?} not found"))
                } else {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to toggle task: {e}"),
                    )
                }
            })?;
            let os_warning = reconcile_os_for_task(&task);
            let os = schedule_os::current().ok();
            let wire = enrich_task_wire(task, now_epoch_secs(), os.as_deref());
            Ok((wire, os_warning))
        },
    )
    .await;

    match res {
        Ok(Ok((wire, os_warning))) => {
            publish_global(GlobalWireEvent::SchedulesUpdated);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "task": wire,
                    "os_warning": os_warning,
                })),
            )
                .into_response()
        }
        Ok(Err((status, msg))) => err_response(status, msg),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Schedule toggle failed: {e}"),
        ),
    }
}

async fn delete_schedule_in_root(
    schedules_root: std::path::PathBuf,
    id: String,
    uninstall_os: bool,
) -> axum::response::Response {
    if !valid_id(&id) {
        return err_response(StatusCode::BAD_REQUEST, format!("invalid task id {id:?}"));
    }

    let Some(_guard) = RunningGuard::acquire(&id) else {
        return err_response(
            StatusCode::CONFLICT,
            format!("task {id:?} is currently running; stop it before deleting"),
        );
    };

    let id_for_task = id.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<(), (StatusCode, String)> {
        if schedule::is_run_locked_in(&schedules_root, &id_for_task) {
            return Err((
                StatusCode::CONFLICT,
                format!("task {id_for_task:?} is currently running; stop it before deleting"),
            ));
        }
        match schedule::load_in(&schedules_root, &id_for_task) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("task {id_for_task:?} not found"),
                ));
            }
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to inspect task {id_for_task:?}: {e}"),
                ));
            }
        }
        if uninstall_os {
            if let Ok(os) = schedule_os::current() {
                let _ = os.uninstall(&id_for_task);
            }
        }
        schedule::remove_in(&schedules_root, &id_for_task).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to remove task {id_for_task:?}: {e}"),
            )
        })?;
        Ok(())
    })
    .await;

    match res {
        Ok(Ok(())) => {
            publish_global(GlobalWireEvent::SchedulesUpdated);
            (StatusCode::OK, Json(serde_json::json!({ "success": true }))).into_response()
        }
        Ok(Err((status, msg))) => err_response(status, msg),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Schedule delete failed: {e}"),
        ),
    }
}

/// DELETE /schedules/:id — Unregister from OS scheduler and delete task definition.
pub(crate) async fn delete_schedule(Path(id): Path<String>) -> impl IntoResponse {
    delete_schedule_in_root(schedule::schedules_root(), id, true).await
}

/// POST /schedules/sync — Reconcile all stored tasks with the OS scheduler.
pub(crate) async fn sync_schedules() -> impl IntoResponse {
    let res = tokio::task::spawn_blocking(|| -> Result<serde_json::Value, String> {
        let os = schedule_os::current().map_err(|e| format!("OS scheduler unavailable: {e}"))?;
        let tasks = schedule::list();
        let mut installed = 0usize;
        let mut uninstalled = 0usize;
        let mut errors = Vec::<String>::new();

        for task in &tasks {
            if task.enabled {
                match os.install(task) {
                    Ok(()) => installed += 1,
                    Err(e) => errors.push(format!("{}: {e}", task.id)),
                }
            } else {
                match os.uninstall(&task.id) {
                    Ok(()) => uninstalled += 1,
                    Err(e) => errors.push(format!("{}: {e}", task.id)),
                }
            }
        }
        Ok(serde_json::json!({
            "success": errors.is_empty(),
            "installed": installed,
            "uninstalled": uninstalled,
            "error_count": errors.len(),
            "errors": errors,
        }))
    })
    .await;

    match res {
        Ok(Ok(val)) => {
            publish_global(GlobalWireEvent::SchedulesUpdated);
            (StatusCode::OK, Json(val)).into_response()
        }
        Ok(Err(msg)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, msg),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Schedule sync failed: {e}"),
        ),
    }
}

/// POST /schedules/:id/run — Trigger an enabled scheduled task immediately in the background.
pub(crate) async fn run_schedule_now(Path(id): Path<String>) -> impl IntoResponse {
    if !valid_id(&id) {
        return err_response(StatusCode::BAD_REQUEST, format!("invalid task id {id:?}"));
    }

    let Some(guard) = RunningGuard::acquire(&id) else {
        return err_response(
            StatusCode::CONFLICT,
            format!("task {id:?} is already running"),
        );
    };

    let id_for_load = id.clone();
    let loaded = tokio::task::spawn_blocking(move || schedule::load(&id_for_load)).await;
    let task = match loaded {
        Ok(Ok(t)) => t,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return err_response(StatusCode::NOT_FOUND, format!("task {id:?} not found"));
        }
        Ok(Err(e)) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load task: {e}"),
            );
        }
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load task: {e}"),
            );
        }
    };

    if !task.enabled {
        return err_response(
            StatusCode::BAD_REQUEST,
            "cannot run a disabled scheduled task; enable it first",
        );
    }

    let id_for_lock_check = id.clone();
    let already_locked =
        tokio::task::spawn_blocking(move || schedule::is_run_locked(&id_for_lock_check))
            .await
            .unwrap_or(false);
    if already_locked {
        return err_response(
            StatusCode::CONFLICT,
            format!("task {id:?} is already running"),
        );
    }

    let exe = match schedule_os::resolve_schedule_runner_exe() {
        Ok(p) => p,
        Err(e) => {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    };

    let mut cmd = tokio::process::Command::new(&exe);
    cmd.args(["schedule", "run", &id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to spawn scheduled task runner: {e}"),
            );
        }
    };

    publish_global(GlobalWireEvent::SchedulesUpdated);

    tokio::spawn(async move {
        let _guard = guard;
        let _ = child.wait().await;
        publish_global(GlobalWireEvent::SchedulesUpdated);
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "success": true,
            "started": true,
            "id": id,
        })),
    )
        .into_response()
}

pub(crate) fn list_scheduled_runs_in_root(
    sessions_root: &std::path::Path,
    schedule_id: &str,
) -> std::io::Result<Vec<SessionMetaWithProject>> {
    let mut entries: Vec<CatalogEntry> = catalog_scan_in_root(sessions_root)?
        .entries
        .into_iter()
        .filter(|entry| {
            entry.origin == SessionOrigin::Scheduled
                && entry.schedule_id.as_deref() == Some(schedule_id)
                && entry.message_count > 0
        })
        .collect();
    entries.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    entries.truncate(50);
    Ok(entries.iter().map(catalog_entry_with_project).collect())
}

/// Collect ALL `(project_bucket, session_id)` targets belonging to `schedule_id`
/// without the 50-item display cap and including any empty (`message_count == 0`)
/// leftover scheduled sessions.
pub(crate) fn list_all_scheduled_run_targets_in_root(
    sessions_root: &std::path::Path,
    schedule_id: &str,
) -> std::io::Result<Vec<(String, String)>> {
    let targets = catalog_scan_in_root(sessions_root)?
        .entries
        .into_iter()
        .filter(|entry| {
            entry.origin == SessionOrigin::Scheduled
                && entry.schedule_id.as_deref() == Some(schedule_id)
        })
        .map(|entry| (entry.project_bucket, entry.id))
        .collect();
    Ok(targets)
}

/// GET /schedules/:id/runs — Return historical `SessionOrigin::Scheduled` sessions for a task.
pub(crate) async fn get_schedule_runs(Path(id): Path<String>) -> impl IntoResponse {
    if !valid_id(&id) {
        return err_response(StatusCode::BAD_REQUEST, format!("invalid task id {id:?}"));
    }
    let res = run_session_catalog_io(move || {
        list_scheduled_runs_in_root(&NativeSessionManager::sessions_root(), &id)
    })
    .await;

    match res {
        Ok(runs) => (StatusCode::OK, Json(runs)).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to load scheduled runs: {e}"),
        ),
    }
}

pub(crate) async fn clear_schedule_runs_in_root(
    state: AppState,
    client_mode: SessionMode,
    sessions_root: std::path::PathBuf,
    id: String,
) -> axum::response::Response {
    if !valid_id(&id) {
        return err_response(StatusCode::BAD_REQUEST, format!("invalid task id {id:?}"))
            .into_response();
    }
    let id_for_scan = id.clone();
    let root_for_scan = sessions_root.clone();
    let targets_res = run_session_catalog_io(move || {
        list_all_scheduled_run_targets_in_root(&root_for_scan, &id_for_scan)
    })
    .await;

    let targets = match targets_res {
        Ok(list) => list,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to scan scheduled runs: {e}"),
            )
            .into_response();
        }
    };

    let mut deleted = 0usize;
    let mut skipped_active = 0usize;
    let mut failed = 0usize;

    for (project_hash, session_id) in targets {
        match delete_session_checked_in_root(
            &state,
            client_mode,
            sessions_root.clone(),
            &project_hash,
            &session_id,
        )
        .await
        {
            Ok(()) => deleted += 1,
            Err((StatusCode::CONFLICT, _)) => skipped_active += 1,
            Err((StatusCode::NOT_FOUND, _)) => {}
            Err(_) => failed += 1,
        }
    }

    if deleted > 0 {
        publish_global(GlobalWireEvent::SchedulesUpdated);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": failed == 0,
            "deleted": deleted,
            "skipped_active": skipped_active,
            "failed": failed,
        })),
    )
        .into_response()
}

/// DELETE /schedules/:id/runs — Delete all historical `SessionOrigin::Scheduled` sessions for a task.
/// Active turns stay protected (skipped as `skipped_active` instead of failing the batch).
pub(crate) async fn clear_schedule_runs(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<SessionMode>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    clear_schedule_runs_in_root(
        state,
        client_mode,
        NativeSessionManager::sessions_root(),
        id,
    )
    .await
}

/// On Windows startup, silently re-register any enabled scheduled task whose
/// existing `schtasks` action does not yet invoke the GUI-subsystem `--schedule-run`
/// wrapper, so legacy tasks created before the silent launcher stop popping consoles.
pub(crate) fn spawn_windows_schedule_reconcile() {
    #[cfg(target_os = "windows")]
    {
        tokio::spawn(async {
            let updated = tokio::task::spawn_blocking(|| -> usize {
                let Ok(os) = schedule_os::current() else {
                    return 0;
                };
                let mut count = 0usize;
                for task in schedule::list() {
                    if task.enabled
                        && schedule_os::windows_task_needs_reconcile(&task.id)
                        && os.install(&task).is_ok()
                    {
                        count += 1;
                    }
                }
                count
            })
            .await
            .unwrap_or(0);
            if updated > 0 {
                publish_global(GlobalWireEvent::SchedulesUpdated);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeikcode_capabilities::session::{CatalogPresence, SessionMeta, StorageOwner};

    #[test]
    fn list_scheduled_runs_filters_by_origin_and_schedule_id() {
        let tmp = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let mgr = NativeSessionManager::with_root(tmp.path().join(bucket));

        let mut manual = SessionMeta::new("m1", "/proj", 10);
        manual.owner = StorageOwner::Native;
        manual.message_count = 2;
        mgr.write_meta(&manual).unwrap();

        let mut run_a = SessionMeta::new("r1", "/proj", 20);
        run_a.owner = StorageOwner::Native;
        run_a.origin = SessionOrigin::Scheduled;
        run_a.schedule_id = Some("task-a".into());
        run_a.message_count = 4;
        mgr.write_meta(&run_a).unwrap();

        let mut run_b = SessionMeta::new("r2", "/proj", 30);
        run_b.owner = StorageOwner::Native;
        run_b.origin = SessionOrigin::Scheduled;
        run_b.schedule_id = Some("task-b".into());
        run_b.message_count = 3;
        mgr.write_meta(&run_b).unwrap();

        let runs = list_scheduled_runs_in_root(tmp.path(), "task-a").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].meta.id, "r1");
        assert_eq!(runs[0].meta.origin, SessionOrigin::Scheduled);

        let _ = CatalogPresence::NativeOnly;
    }

    #[tokio::test]
    async fn schedules_http_routes_reject_invalid_ids_and_invalid_payloads() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::{get, post};
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route("/schedules", get(list_schedules))
            .route(
                "/schedules/:id",
                axum::routing::put(update_schedule).delete(delete_schedule),
            )
            .route("/schedules/:id/toggle", post(toggle_schedule))
            .route("/schedules/:id/run", post(run_schedule_now))
            .route("/schedules/:id/runs", get(get_schedule_runs));

        // Path traversal / invalid id must fail closed with 400 on every :id endpoint.
        for (method, uri) in [
            ("DELETE", "/schedules/bad%2Fid"),
            ("POST", "/schedules/bad..id/toggle"),
            ("POST", "/schedules/bad..id/run"),
            ("GET", "/schedules/bad..id/runs"),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
        }

        // Missing task returns 404 on toggle / run / update.
        let toggle_missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/schedules/nonexistent-task-id/toggle")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(toggle_missing.status(), StatusCode::NOT_FOUND);

        // Disabled task returns 400 and in-flight run returns 409.
        let id_disabled = format!("http-disabled-{}", uuid::Uuid::new_v4().simple());
        let disabled_task = ScheduleTask {
            id: id_disabled.clone(),
            title: "Disabled".into(),
            prompt: "noop".into(),
            cwd: "/tmp".into(),
            schedule: Schedule::Hourly,
            permission_mode: "plan".into(),
            notify: "important".into(),
            enabled: false,
            created_at: 1,
            last_run_at: None,
            last_status: None,
        };
        schedule::save(&disabled_task).unwrap();
        let disabled_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/schedules/{id_disabled}/run"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(disabled_resp.status(), StatusCode::BAD_REQUEST);
        let _ = schedule::remove(&id_disabled);

        let id_busy = format!("http-busy-{}", uuid::Uuid::new_v4().simple());
        let mut busy_task = disabled_task;
        busy_task.id = id_busy.clone();
        busy_task.enabled = true;
        schedule::save(&busy_task).unwrap();
        assert!(try_mark_running(&id_busy));
        let conflict_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/schedules/{id_busy}/run"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        clear_running(&id_busy);
        let _ = schedule::remove(&id_busy);
        assert_eq!(conflict_resp.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn list_all_scheduled_run_targets_includes_all_and_zero_message_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let bucket = "0123456789abcdef";
        let mgr = NativeSessionManager::with_root(tmp.path().join(bucket));

        // 55 scheduled runs for task-a (including one 0-message leftover), exceeding the 50-row display cap.
        for i in 0..55 {
            let mut m = SessionMeta::new(&format!("run-a-{i}"), "/proj", 100 + i as i64);
            m.owner = StorageOwner::Native;
            m.origin = SessionOrigin::Scheduled;
            m.schedule_id = Some("task-a".into());
            m.message_count = if i == 0 { 0 } else { 2 };
            mgr.write_meta(&m).unwrap();
        }
        // Unrelated manual session and another task's scheduled run must not be collected.
        let mut manual = SessionMeta::new("manual-1", "/proj", 200);
        manual.owner = StorageOwner::Native;
        manual.message_count = 3;
        mgr.write_meta(&manual).unwrap();

        let mut other = SessionMeta::new("run-b-0", "/proj", 201);
        other.owner = StorageOwner::Native;
        other.origin = SessionOrigin::Scheduled;
        other.schedule_id = Some("task-b".into());
        other.message_count = 2;
        mgr.write_meta(&other).unwrap();

        let targets = list_all_scheduled_run_targets_in_root(tmp.path(), "task-a").unwrap();
        assert_eq!(targets.len(), 55);
        assert!(targets.iter().all(|(b, _)| b == bucket));
        assert!(targets.iter().any(|(_, id)| id == "run-a-0"));
        assert!(!targets
            .iter()
            .any(|(_, id)| id == "manual-1" || id == "run-b-0"));
    }

    #[tokio::test]
    async fn clear_schedule_runs_rejects_invalid_id_with_bad_request() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::delete;
        use tower::ServiceExt;

        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        let app = axum::Router::new()
            .route("/schedules/:id/runs", delete(clear_schedule_runs))
            .layer(axum::Extension(SessionMode::Webui))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/schedules/bad..id/runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn clear_schedule_runs_deletes_matching_runs_skips_active_and_emits_events() {
        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        let sessions_root = NativeSessionManager::sessions_root();
        let bucket = "0123456789abcdef";
        let mgr = NativeSessionManager::with_root(sessions_root.join(bucket));

        // 2 completed scheduled runs for task-a (including one 0-message leftover),
        // 1 active leased run for task-a (must be skipped as skipped_active),
        // 1 manual session, and 1 scheduled run for task-b (both must be preserved).
        for (sid, msg_count) in [("run-done-1", 3u32), ("run-empty-2", 0u32)] {
            let mut m = SessionMeta::new(sid, "/proj", 100);
            m.owner = StorageOwner::Native;
            m.origin = SessionOrigin::Scheduled;
            m.schedule_id = Some("task-a".into());
            m.message_count = msg_count;
            mgr.write_meta(&m).unwrap();
        }
        let mut active_m = SessionMeta::new("run-active-3", "/proj", 105);
        active_m.owner = StorageOwner::Native;
        active_m.origin = SessionOrigin::Scheduled;
        active_m.schedule_id = Some("task-a".into());
        active_m.message_count = 1;
        mgr.write_meta(&active_m).unwrap();
        let _active_lease = mgr.acquire_lease("run-active-3").unwrap();

        let mut manual_m = SessionMeta::new("manual-keep", "/proj", 110);
        manual_m.owner = StorageOwner::Native;
        manual_m.message_count = 4;
        mgr.write_meta(&manual_m).unwrap();

        let mut other_m = SessionMeta::new("other-task-keep", "/proj", 115);
        other_m.owner = StorageOwner::Native;
        other_m.origin = SessionOrigin::Scheduled;
        other_m.schedule_id = Some("task-b".into());
        other_m.message_count = 2;
        mgr.write_meta(&other_m).unwrap();

        let mut rx = crate::global_events().subscribe();
        let resp = clear_schedule_runs_in_root(
            state,
            SessionMode::Webui,
            sessions_root.clone(),
            "task-a".to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["deleted"], 2);
        assert_eq!(body["skipped_active"], 1);
        assert_eq!(body["failed"], 0);

        // Deleted runs are gone; active run, manual session, and other task's run survive.
        assert!(mgr.read_meta("run-done-1").is_err());
        assert!(mgr.read_meta("run-empty-2").is_err());
        assert!(mgr.read_meta("run-active-3").is_ok());
        assert!(mgr.read_meta("manual-keep").is_ok());
        assert!(mgr.read_meta("other-task-keep").is_ok());

        // Verify SessionDeleted + SchedulesUpdated events were broadcast.
        let mut deleted_ids = Vec::new();
        let mut saw_schedules_updated = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                GlobalWireEvent::SessionDeleted { session_id, .. } => deleted_ids.push(session_id),
                GlobalWireEvent::SchedulesUpdated => saw_schedules_updated = true,
                _ => {}
            }
        }
        assert!(deleted_ids.contains(&"run-done-1".to_string()));
        assert!(deleted_ids.contains(&"run-empty-2".to_string()));
        assert!(saw_schedules_updated);
    }

    #[tokio::test]
    async fn delete_schedule_guards_running_and_missing_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // Missing task returns 404.
        let missing = delete_schedule_in_root(root.clone(), "missing-task".into(), false).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let task = ScheduleTask {
            id: "sched-guard-1".into(),
            title: "Guard test".into(),
            prompt: "ping".into(),
            cwd: "/tmp".into(),
            schedule: Schedule::Hourly,
            permission_mode: "plan".into(),
            notify: "important".into(),
            enabled: true,
            created_at: 1,
            last_run_at: None,
            last_status: None,
        };
        schedule::save_in(&root, &task).unwrap();

        // In-flight memory guard returns 409 and preserves task file.
        {
            let _guard = RunningGuard::acquire(&task.id).expect("in-flight lock");
            let conflict_mem = delete_schedule_in_root(root.clone(), task.id.clone(), false).await;
            assert_eq!(conflict_mem.status(), StatusCode::CONFLICT);
            assert!(schedule::load_in(&root, &task.id).is_ok());
        }

        // Disk run lock returns 409 and preserves task file.
        {
            let _disk_lock = schedule::try_acquire_run_lock_in(&root, &task.id)
                .unwrap()
                .expect("disk run lock");
            let conflict_disk = delete_schedule_in_root(root.clone(), task.id.clone(), false).await;
            assert_eq!(conflict_disk.status(), StatusCode::CONFLICT);
            assert!(schedule::load_in(&root, &task.id).is_ok());
        }

        // Once locks release, delete succeeds with 200 and removes the task.
        let ok = delete_schedule_in_root(root.clone(), task.id.clone(), false).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(schedule::load_in(&root, &task.id).is_err());
    }
}
