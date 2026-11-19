use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Daily { time: String },               // "HH:MM"
    Weekly { weekday: u8, time: String }, // weekday 1..=7 (1=Mon)
    Hourly,
    Interval { every_minutes: u32 },
    Cron { expr: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub cwd: String,
    pub schedule: Schedule,
    #[serde(default = "default_mode")]
    pub permission_mode: String, // "plan" | "accept_edits" | "auto"
    #[serde(default = "default_notify")]
    pub notify: String, // "off" | "important" | "all"
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub last_status: Option<String>,
}

fn default_mode() -> String {
    "plan".into()
}
fn default_notify() -> String {
    "important".into()
}
fn default_true() -> bool {
    true
}

pub fn schedules_root() -> PathBuf {
    crate::config::Config::config_dir().join("schedules")
}

/// A task id is used verbatim to build its on-disk `<id>.json` path, so it must
/// not be able to escape the schedules directory. Ids minted by `build_task`
/// (slug + uuid) are always in-shape, but `load`/`remove`/`enable`/`disable`/`run`
/// take an id straight from an untrusted CLI or HTTP argument. Accept only a
/// conservative filename-safe alphabet and explicitly reject path separators and `..`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != ".."
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Parse and strictly validate `"HH:MM"` (00..=23, 00..=59).
pub fn parse_hhmm(s: &str) -> anyhow::Result<(u8, u8)> {
    let (h_str, m_str) = s
        .trim()
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("time must be in HH:MM format, got {s:?}"))?;
    if h_str.len() != 2 && h_str.len() != 1 || m_str.len() != 2 {
        anyhow::bail!("time must be in HH:MM format, got {s:?}");
    }
    let h: u8 = h_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid hour in time {s:?}"))?;
    let m: u8 = m_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid minute in time {s:?}"))?;
    if h > 23 || m > 59 {
        anyhow::bail!("time out of range (expected 00:00..23:59), got {s:?}");
    }
    Ok((h, m))
}

/// Validate a [`Schedule`] definition (including platform bounds such as Windows
/// `schtasks /SC MINUTE /MO` 1..=1439 minute cap).
pub fn validate_schedule(schedule: &Schedule) -> anyhow::Result<()> {
    match schedule {
        Schedule::Daily { time } => {
            parse_hhmm(time)?;
        }
        Schedule::Weekly { weekday, time } => {
            if !(1..=7).contains(weekday) {
                anyhow::bail!("weekly weekday must be 1..=7 (1=Mon), got {weekday}");
            }
            parse_hhmm(time)?;
        }
        Schedule::Hourly => {}
        Schedule::Interval { every_minutes } => {
            if *every_minutes == 0 {
                anyhow::bail!("interval every_minutes must be > 0");
            }
            #[cfg(target_os = "windows")]
            if *every_minutes > 1439 {
                anyhow::bail!(
                    "Windows Task Scheduler supports intervals up to 1439 minutes (got {every_minutes}); use daily or weekly instead"
                );
            }
        }
        Schedule::Cron { expr } => {
            if expr.trim().is_empty() {
                anyhow::bail!("cron expression cannot be empty");
            }
        }
    }
    Ok(())
}

/// Validate user-supplied task fields before saving or registering.
pub fn validate_task_fields(
    title: &str,
    prompt: &str,
    cwd: &str,
    schedule: &Schedule,
    permission_mode: &str,
    notify: &str,
) -> anyhow::Result<()> {
    if title.trim().is_empty() {
        anyhow::bail!("task title cannot be empty");
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("task prompt cannot be empty");
    }
    if cwd.trim().is_empty() {
        anyhow::bail!("working directory cannot be empty");
    }
    if !matches!(permission_mode, "plan" | "accept_edits" | "auto") {
        anyhow::bail!(
            "invalid permission_mode {permission_mode:?}; expected plan, accept_edits, or auto"
        );
    }
    if !matches!(notify, "off" | "important" | "all") {
        anyhow::bail!("invalid notify {notify:?}; expected off, important, or all");
    }
    validate_schedule(schedule)
}

/// Convert a title string to an ASCII-only slug usable as part of a task id.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "task".to_string()
    } else {
        out.chars().take(32).collect()
    }
}

/// Build a [`ScheduleTask`] with a fresh slug+uuid id and current timestamp.
pub fn build_task(
    title: &str,
    prompt: &str,
    cwd: &str,
    schedule: Schedule,
    permission_mode: &str,
    notify: &str,
) -> ScheduleTask {
    let slug = slug(title);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("{}-{}", slug, &suffix[..6]);
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    ScheduleTask {
        id,
        title: title.trim().to_string(),
        prompt: prompt.to_string(),
        cwd: cwd.trim().to_string(),
        schedule,
        permission_mode: permission_mode.to_string(),
        notify: notify.to_string(),
        enabled: true,
        created_at,
        last_run_at: None,
        last_status: None,
    }
}

fn task_path_in(root: &std::path::Path, id: &str) -> std::io::Result<PathBuf> {
    if !valid_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid scheduled task id {id:?}"),
        ));
    }
    Ok(root.join(format!("{id}.json")))
}

fn meta_lock_path_in(root: &std::path::Path, id: &str) -> std::io::Result<PathBuf> {
    if !valid_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid scheduled task id {id:?}"),
        ));
    }
    Ok(root.join(format!("{id}.meta.lock")))
}

fn run_lock_path_in(root: &std::path::Path, id: &str) -> std::io::Result<PathBuf> {
    if !valid_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid scheduled task id {id:?}"),
        ));
    }
    Ok(root.join(format!("{id}.run.lock")))
}

fn with_meta_lock_in<T>(
    root: &std::path::Path,
    id: &str,
    op: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    use fs2::FileExt;
    std::fs::create_dir_all(root)?;
    let lock_path = meta_lock_path_in(root, id)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock_file.lock_exclusive()?;
    let res = op();
    let _ = lock_file.unlock();
    res
}

fn save_atomic_in(root: &std::path::Path, task: &ScheduleTask) -> std::io::Result<()> {
    let path = task_path_in(root, &task.id)?;
    std::fs::create_dir_all(root)?;
    let bytes = serde_json::to_vec_pretty(task)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{}.", task.id))
        .suffix(".tmp")
        .tempfile_in(root)?;
    std::io::Write::write_all(&mut tmp, &bytes)?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(())
}

pub fn save_in(root: &std::path::Path, task: &ScheduleTask) -> std::io::Result<()> {
    with_meta_lock_in(root, &task.id, || save_atomic_in(root, task))
}

fn update_task_in<F, E>(root: &std::path::Path, id: &str, f: F) -> Result<ScheduleTask, E>
where
    F: FnOnce(&mut ScheduleTask) -> Result<(), E>,
    E: From<std::io::Error>,
{
    let mut result: Option<Result<ScheduleTask, E>> = None;
    let io_res = with_meta_lock_in(root, id, || {
        let mut task = load_in(root, id)?;
        if let Err(err) = f(&mut task) {
            result = Some(Err(err));
            return Ok(());
        }
        save_atomic_in(root, &task)?;
        result = Some(Ok(task));
        Ok(())
    });
    match io_res {
        Ok(()) => result.expect("with_meta_lock_in closure must run"),
        Err(e) => Err(E::from(e)),
    }
}

/// Exclusive per-task execution guard shared between CLI (`jeikcode schedule run`)
/// and daemon (`POST /schedules/:id/run`). Dropped automatically on completion or exit.
pub struct ScheduleRunLock {
    file: std::fs::File,
}

impl Drop for ScheduleRunLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn try_acquire_run_lock_in(
    root: &std::path::Path,
    id: &str,
) -> std::io::Result<Option<ScheduleRunLock>> {
    use fs2::FileExt;
    std::fs::create_dir_all(root)?;
    let lock_path = run_lock_path_in(root, id)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(ScheduleRunLock { file })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => {
            #[cfg(windows)]
            if e.raw_os_error() == Some(33) {
                return Ok(None);
            }
            Err(e)
        }
    }
}

pub fn is_run_locked_in(root: &std::path::Path, id: &str) -> bool {
    match try_acquire_run_lock_in(root, id) {
        Ok(Some(_guard)) => false,
        Ok(None) => true,
        Err(_) => false,
    }
}

pub fn try_acquire_run_lock(id: &str) -> std::io::Result<Option<ScheduleRunLock>> {
    try_acquire_run_lock_in(&schedules_root(), id)
}

pub fn is_run_locked(id: &str) -> bool {
    is_run_locked_in(&schedules_root(), id)
}

pub fn update_task<F, E>(id: &str, f: F) -> Result<ScheduleTask, E>
where
    F: FnOnce(&mut ScheduleTask) -> Result<(), E>,
    E: From<std::io::Error>,
{
    update_task_in(&schedules_root(), id, f)
}

pub fn update_run_status_in(
    root: &std::path::Path,
    id: &str,
    last_run_at: i64,
    last_status: &str,
) -> std::io::Result<ScheduleTask> {
    update_task_in(root, id, |task| {
        task.last_run_at = Some(last_run_at);
        task.last_status = Some(last_status.to_string());
        Ok(())
    })
}

pub fn update_run_status(
    id: &str,
    last_run_at: i64,
    last_status: &str,
) -> std::io::Result<ScheduleTask> {
    update_run_status_in(&schedules_root(), id, last_run_at, last_status)
}

pub fn load_in(root: &std::path::Path, id: &str) -> std::io::Result<ScheduleTask> {
    let bytes = std::fs::read(task_path_in(root, id)?)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn list_in(root: &std::path::Path) -> Vec<ScheduleTask> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(t) = serde_json::from_slice::<ScheduleTask>(&bytes) {
                out.push(t); // corrupt files are skipped
            }
        }
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

pub fn remove_in(root: &std::path::Path, id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(task_path_in(root, id)?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn save(task: &ScheduleTask) -> std::io::Result<()> {
    save_in(&schedules_root(), task)
}
pub fn load(id: &str) -> std::io::Result<ScheduleTask> {
    load_in(&schedules_root(), id)
}
pub fn list() -> Vec<ScheduleTask> {
    list_in(&schedules_root())
}
pub fn remove(id: &str) -> std::io::Result<()> {
    remove_in(&schedules_root(), id)
}

/// Next fire time (epoch secs, UTC) for simple frequencies. `Cron` returns None
/// because OS cron translation is not handled here.
pub fn next_run(schedule: &Schedule, now_epoch_secs: i64) -> Option<i64> {
    match schedule {
        Schedule::Interval { every_minutes } => {
            (*every_minutes > 0).then(|| now_epoch_secs + (*every_minutes as i64) * 60)
        }
        Schedule::Hourly => {
            let secs_into_hour = now_epoch_secs.rem_euclid(3600);
            Some(now_epoch_secs + (3600 - secs_into_hour))
        }
        Schedule::Daily { time } => {
            let (h, m) = parse_hhmm(time).ok()?;
            let day = now_epoch_secs.div_euclid(86400) * 86400;
            let target = day + (h as i64) * 3600 + (m as i64) * 60;
            Some(if target > now_epoch_secs {
                target
            } else {
                target + 86400
            })
        }
        Schedule::Weekly { weekday, time } => {
            if !(1..=7).contains(weekday) {
                return None;
            }
            let (h, m) = parse_hhmm(time).ok()?;
            let days_since_epoch = now_epoch_secs.div_euclid(86400);
            // 1970-01-01 was Thursday (ISO weekday 4).
            let current_weekday = (days_since_epoch + 3).rem_euclid(7) + 1;
            let target_weekday = *weekday as i64;
            let mut delta_days = (target_weekday - current_weekday).rem_euclid(7);
            let mut target =
                (days_since_epoch + delta_days) * 86400 + (h as i64) * 3600 + (m as i64) * 60;
            if target <= now_epoch_secs {
                delta_days += 7;
                target =
                    (days_since_epoch + delta_days) * 86400 + (h as i64) * 3600 + (m as i64) * 60;
            }
            Some(target)
        }
        Schedule::Cron { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ScheduleTask {
        ScheduleTask {
            id: "t1".into(),
            title: "Daily brief".into(),
            prompt: "summarize".into(),
            cwd: "/tmp/proj".into(),
            schedule: Schedule::Daily {
                time: "09:00".into(),
            },
            permission_mode: "plan".into(),
            notify: "important".into(),
            enabled: true,
            created_at: 0,
            last_run_at: None,
            last_status: None,
        }
    }

    #[test]
    fn task_json_roundtrips() {
        let t = sample();
        let json = serde_json::to_string(&t).unwrap();
        let back: ScheduleTask = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "t1");
        assert!(matches!(back.schedule, Schedule::Daily { .. }));
        assert_eq!(back.permission_mode, "plan");
    }

    #[test]
    fn store_save_load_list_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_in(root, &sample()).unwrap();
        assert_eq!(load_in(root, "t1").unwrap().title, "Daily brief");
        assert_eq!(list_in(root).len(), 1);
        remove_in(root, "t1").unwrap();
        assert!(list_in(root).is_empty());
    }

    #[test]
    fn store_rejects_ids_that_escape_the_schedules_dir() {
        use std::io::ErrorKind;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for bad in ["../evil", "a/b", "a\\b", "foo/../bar", "..", ""] {
            // remove/enable/disable/run and save all flow through these helpers,
            // so a crafted id must be rejected before it can touch the filesystem.
            assert_eq!(
                remove_in(root, bad).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "remove should reject id {bad:?}"
            );
            assert_eq!(
                load_in(root, bad).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "load should reject id {bad:?}"
            );
            let mut task = sample();
            task.id = bad.to_string();
            assert_eq!(
                save_in(root, &task).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "save should reject id {bad:?}"
            );
        }
        // A traversal id must not have written anything outside the store.
        assert!(!tmp.path().parent().unwrap().join("evil.json").exists());
    }

    #[test]
    fn store_accepts_slug_and_uuid_shaped_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut task = sample();
        task.id = "daily-brief_2026.01-9f8e7d6c".into();
        save_in(root, &task).unwrap();
        assert_eq!(load_in(root, &task.id).unwrap().id, task.id);
        remove_in(root, &task.id).unwrap();
    }

    #[test]
    fn next_run_daily_is_today_or_tomorrow_at_time() {
        // 2026-07-31 08:00:00 UTC = 1785657600 ; daily 09:00 → same day 09:00
        let now = 1785657600;
        let nr = next_run(
            &Schedule::Daily {
                time: "09:00".into(),
            },
            now,
        )
        .unwrap();
        assert!(nr > now && nr - now <= 24 * 3600);
    }

    #[test]
    fn next_run_cron_is_none_in_phase1() {
        assert_eq!(
            next_run(
                &Schedule::Cron {
                    expr: "0 9 * * *".into()
                },
                0
            ),
            None
        );
    }

    #[test]
    fn next_run_weekly_respects_target_weekday() {
        // 1970-01-01 08:00 UTC (Thursday, weekday=4) -> target Monday (weekday=1) at 09:00 is +4 days.
        let now = 8 * 3600;
        let nr = next_run(
            &Schedule::Weekly {
                weekday: 1,
                time: "09:00".into(),
            },
            now,
        )
        .unwrap();
        assert_eq!(nr, 4 * 86400 + 9 * 3600);
    }

    #[test]
    fn validate_task_fields_rejects_bad_times_and_modes() {
        assert!(parse_hhmm("25:00").is_err());
        assert!(parse_hhmm("09:99").is_err());
        assert!(parse_hhmm("09:30").is_ok());
        let ok_sched = Schedule::Daily {
            time: "09:30".into(),
        };
        assert!(validate_task_fields("", "p", "/tmp", &ok_sched, "plan", "important").is_err());
        assert!(validate_task_fields("t", "p", "/tmp", &ok_sched, "bogus", "important").is_err());
        assert!(validate_task_fields("t", "p", "/tmp", &ok_sched, "plan", "bogus").is_err());
        assert!(validate_task_fields("t", "p", "/tmp", &ok_sched, "plan", "important").is_ok());
    }

    #[test]
    fn run_lock_is_mutually_exclusive_and_status_update_preserves_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut task = build_task(
            "Lock Test",
            "prompt",
            "/tmp",
            Schedule::Hourly,
            "plan",
            "important",
        );
        save_in(dir.path(), &task).unwrap();

        assert!(!is_run_locked_in(dir.path(), &task.id));
        let lock = try_acquire_run_lock_in(dir.path(), &task.id)
            .unwrap()
            .expect("first lock succeeds");
        assert!(is_run_locked_in(dir.path(), &task.id));
        assert!(try_acquire_run_lock_in(dir.path(), &task.id)
            .unwrap()
            .is_none());

        // Simulate a concurrent user edit (disabling task + changing title) while task runs.
        task.enabled = false;
        task.title = "Edited Title".into();
        save_in(dir.path(), &task).unwrap();

        let updated = update_run_status_in(dir.path(), &task.id, 12345, "ok").unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.title, "Edited Title");
        assert_eq!(updated.last_run_at, Some(12345));
        assert_eq!(updated.last_status.as_deref(), Some("ok"));

        drop(lock);
        assert!(!is_run_locked_in(dir.path(), &task.id));
    }
}
