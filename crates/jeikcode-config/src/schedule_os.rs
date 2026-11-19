use crate::desktop_shared::{
    strip_windows_verbatim_path, DESKTOP_CLI_BIN_ENV, DESKTOP_SHELL_BIN_ENV,
};
use crate::schedule::{parse_hhmm, Schedule, ScheduleTask};
use std::path::PathBuf;
use std::sync::Arc;

// ---- InstallState ----

#[derive(Debug, PartialEq, Eq)]
pub enum InstallState {
    Installed,
    Missing,
}

// ---- Runner Exe Resolution ----

fn is_cli_runner_stem(path: &std::path::Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("jeikcode") || s.eq_ignore_ascii_case("cli-bin"))
        .unwrap_or(false)
}

fn find_cli_in_dir(dir: &std::path::Path) -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &["jeikcode.exe", "cli-bin.exe", "jeikcode", "cli-bin"]
    } else {
        &["jeikcode", "cli-bin"]
    };
    for name in candidates {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    let res_dir = dir.join("resources");
    if res_dir.is_dir() {
        for name in candidates {
            let p = res_dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Resolve the `jeikcode` CLI executable that OS schedulers and `/schedules/:id/run`
/// invoke (`<jeikcode> schedule run <id>`).
///
/// Standalone `jeikcode-daemon` cannot use raw `std::env::current_exe()` because that
/// points to `jeikcode-daemon`, which does not implement the `schedule run` subcommand.
pub fn resolve_schedule_runner_exe_with(
    env_override: Option<&str>,
    current_exe: Option<&std::path::Path>,
    path_var: Option<&std::ffi::OsStr>,
) -> anyhow::Result<PathBuf> {
    if let Some(raw) = env_override.map(str::trim).filter(|s| !s.is_empty()) {
        let p = strip_windows_verbatim_path(std::path::Path::new(raw));
        if p.is_file() || cfg!(test) {
            return Ok(p);
        }
    }
    if let Some(cur) = current_exe {
        let clean_cur = strip_windows_verbatim_path(cur);
        if is_cli_runner_stem(&clean_cur) {
            return Ok(clean_cur);
        }
        if let Some(parent) = clean_cur.parent() {
            if let Some(sibling) = find_cli_in_dir(parent) {
                return Ok(strip_windows_verbatim_path(&sibling));
            }
        }
    }
    if let Some(paths) = path_var {
        for dir in std::env::split_paths(paths) {
            if let Some(found) = find_cli_in_dir(&dir) {
                return Ok(strip_windows_verbatim_path(&found));
            }
        }
    }
    // Under `cargo test`, `current_exe()` is `target/debug/deps/<crate>-<hash>`,
    // which is not named `jeikcode`. Allow fake-runner unit tests to proceed
    // when no real `jeikcode` binary is installed on PATH.
    #[cfg(test)]
    if let Some(cur) = current_exe {
        let is_daemon = cur
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("jeikcode-daemon"));
        if !is_daemon {
            return Ok(strip_windows_verbatim_path(cur));
        }
    }
    anyhow::bail!(
        "cannot locate the jeikcode CLI executable for scheduled task execution; ensure `jeikcode` is next to the daemon or on PATH"
    )
}

pub fn resolve_schedule_runner_exe() -> anyhow::Result<PathBuf> {
    let env_override = std::env::var(DESKTOP_CLI_BIN_ENV).ok();
    let current_exe = std::env::current_exe().ok();
    let path_var = std::env::var_os("PATH");
    resolve_schedule_runner_exe_with(
        env_override.as_deref(),
        current_exe.as_deref(),
        path_var.as_deref(),
    )
}

/// On Windows, find a GUI-subsystem (`windows_subsystem = "windows"`) wrapper binary
/// (`jeikcode-desktop.exe` or `jeikcode-daemon.exe`) so Task Scheduler does not pop
/// up a visible Windows Terminal / ConHost window for console-subsystem `jeikcode.exe`.
#[cfg(any(test, target_os = "windows"))]
pub fn resolve_windows_silent_launcher_with(
    desktop_env: Option<&str>,
    runner_exe: &std::path::Path,
) -> Option<PathBuf> {
    if let Some(raw) = desktop_env.map(str::trim).filter(|s| !s.is_empty()) {
        let p = strip_windows_verbatim_path(std::path::Path::new(raw));
        if p.is_file() || cfg!(test) {
            return Some(p);
        }
    }
    let clean_runner = strip_windows_verbatim_path(runner_exe);
    let mut search_dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = clean_runner.parent() {
        search_dirs.push(parent.to_path_buf());
        if parent
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("resources"))
        {
            if let Some(grandparent) = parent.parent() {
                search_dirs.push(grandparent.to_path_buf());
            }
        }
    }
    for dir in search_dirs {
        for name in ["jeikcode-desktop.exe", "jeikcode-daemon.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(strip_windows_verbatim_path(&candidate));
            }
        }
    }
    None
}

#[cfg(any(test, target_os = "windows"))]
pub fn windows_task_action(
    desktop_env: Option<&str>,
    runner_exe: &std::path::Path,
    task_id: &str,
) -> String {
    let clean_runner = strip_windows_verbatim_path(runner_exe);
    if let Some(launcher) = resolve_windows_silent_launcher_with(desktop_env, &clean_runner) {
        format!("\"{}\" --schedule-run {}", launcher.display(), task_id)
    } else {
        format!("\"{}\" schedule run {}", clean_runner.display(), task_id)
    }
}

/// Check whether a Windows scheduled task already uses the silent GUI wrapper when one is available.
#[cfg(target_os = "windows")]
pub fn windows_task_needs_reconcile(id: &str) -> bool {
    let runner = match resolve_schedule_runner_exe() {
        Ok(r) => r,
        Err(_) => return false,
    };
    let desktop_env = std::env::var(DESKTOP_SHELL_BIN_ENV).ok();
    if resolve_windows_silent_launcher_with(desktop_env.as_deref(), &runner).is_none() {
        return false;
    }
    let tn = TaskSched::task_name(id);
    let out = RealCommandRunner.run(
        "schtasks",
        &[
            "/Query".to_string(),
            "/TN".to_string(),
            tn,
            "/FO".to_string(),
            "LIST".to_string(),
            "/V".to_string(),
        ],
    );
    match out {
        Ok(o) if o.status.success() => {
            let txt = String::from_utf8_lossy(&o.stdout);
            !txt.contains("--schedule-run")
        }
        _ => false,
    }
}

// ---- CommandRunner ----

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output>;
}

pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        // Prevent flashing console windows when Desktop / daemon queries or registers
        // scheduled tasks via `schtasks.exe` on Windows.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.output()
    }
}

/// Run a command and bail if it exits non-zero.
fn run_checked(runner: &dyn CommandRunner, prog: &str, args: &[String]) -> anyhow::Result<()> {
    let out = runner.run(prog, args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{prog} failed: {stderr}");
    }
    Ok(())
}

// ---- OsScheduler ----

pub trait OsScheduler {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()>;
    fn uninstall(&self, id: &str) -> anyhow::Result<()>;
    fn status(&self, id: &str) -> InstallState;
}

// ---- Launchd (macOS) ----

#[cfg(any(test, target_os = "macos"))]
pub struct Launchd {
    pub root: PathBuf,
    pub runner: Arc<dyn CommandRunner + Send + Sync>,
}

#[cfg(any(test, target_os = "macos"))]
impl Launchd {
    fn label(id: &str) -> String {
        format!("com.jeikcode.schedule.{id}")
    }

    fn plist_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.plist", Self::label(id)))
    }

    fn render_plist(id: &str, exe: &str, trigger: &LaunchdTrigger) -> String {
        let label = xml_escape(&Self::label(id));
        let exe = xml_escape(exe);
        let id_esc = xml_escape(id);
        let trigger_xml = match trigger {
            LaunchdTrigger::Calendar {
                hour,
                minute,
                weekday,
            } => {
                let mut dict = String::from("\t\t<dict>\n");
                if let Some(h) = hour {
                    dict.push_str(&format!(
                        "\t\t\t<key>Hour</key>\n\t\t\t<integer>{h}</integer>\n"
                    ));
                }
                if let Some(m) = minute {
                    dict.push_str(&format!(
                        "\t\t\t<key>Minute</key>\n\t\t\t<integer>{m}</integer>\n"
                    ));
                }
                if let Some(wd) = weekday {
                    dict.push_str(&format!(
                        "\t\t\t<key>Weekday</key>\n\t\t\t<integer>{wd}</integer>\n"
                    ));
                }
                dict.push_str("\t\t</dict>");
                format!("\t<key>StartCalendarInterval</key>\n{dict}")
            }
            LaunchdTrigger::Interval(secs) => {
                format!("\t<key>StartInterval</key>\n\t<integer>{secs}</integer>")
            }
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>schedule</string>
		<string>run</string>
		<string>{id_esc}</string>
	</array>
{trigger_xml}
	<key>RunAtLoad</key>
	<false/>
</dict>
</plist>
"#
        )
    }
}

#[cfg(any(test, target_os = "macos"))]
impl OsScheduler for Launchd {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
        #[cfg(all(not(test), not(unix)))]
        {
            anyhow::bail!("launchd is only available on macOS/Unix");
        }
        let trigger = launchd_calendar(&task.schedule)?;
        let exe = resolve_schedule_runner_exe()?;
        let exe_str = exe.to_string_lossy();
        let plist_content = Self::render_plist(&task.id, &exe_str, &trigger);
        std::fs::create_dir_all(&self.root)?;
        let path = self.plist_path(&task.id);
        std::fs::write(&path, plist_content)?;
        let uid = get_uid();
        let label = Self::label(&task.id);
        let _ = self.runner.run(
            "launchctl",
            &["bootout".to_string(), format!("gui/{uid}/{label}")],
        );
        let domain = format!("gui/{uid}");
        run_checked(
            self.runner.as_ref(),
            "launchctl",
            &[
                "bootstrap".to_string(),
                domain,
                path.to_string_lossy().to_string(),
            ],
        )?;
        Ok(())
    }

    fn uninstall(&self, id: &str) -> anyhow::Result<()> {
        let uid = get_uid();
        let domain_target = format!("gui/{uid}/{}", Self::label(id));
        let _ = self
            .runner
            .run("launchctl", &["bootout".to_string(), domain_target]);
        let path = self.plist_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    fn status(&self, id: &str) -> InstallState {
        if self.plist_path(id).exists() {
            InstallState::Installed
        } else {
            InstallState::Missing
        }
    }
}

// ---- SystemdTimer (Linux) ----

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
pub struct SystemdTimer {
    pub root: PathBuf,
    pub runner: Arc<dyn CommandRunner + Send + Sync>,
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
impl SystemdTimer {
    fn service_name(id: &str) -> String {
        format!("jeikcode-schedule-{id}.service")
    }

    fn timer_name(id: &str) -> String {
        format!("jeikcode-schedule-{id}.timer")
    }

    fn service_path(&self, id: &str) -> PathBuf {
        self.root.join(Self::service_name(id))
    }

    fn timer_path(&self, id: &str) -> PathBuf {
        self.root.join(Self::timer_name(id))
    }

    fn render_service(id: &str, exe: &str) -> String {
        format!(
            "[Unit]\nDescription=JeikCode scheduled task: {id}\n\n\
             [Service]\nType=oneshot\nExecStart=\"{exe}\" schedule run {id}\n"
        )
    }

    fn render_timer(id: &str, cal: &OnCalendar) -> String {
        let on_cal = match cal {
            OnCalendar::Calendar(s) => format!("OnCalendar={s}"),
            // Include OnActiveSec so the first run fires after the interval even
            // when the oneshot service has never been active yet.
            OnCalendar::Interval(s) => format!("OnActiveSec={s}\nOnUnitActiveSec={s}"),
        };
        format!(
            "[Unit]\nDescription=JeikCode scheduler timer: {id}\n\n\
             [Timer]\n{on_cal}\nPersistent=true\n\n\
             [Install]\nWantedBy=timers.target\n"
        )
    }
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
impl OsScheduler for SystemdTimer {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
        let cal = systemd_calendar(&task.schedule)?;
        let exe = resolve_schedule_runner_exe()?;
        let exe_str = exe.to_string_lossy();
        std::fs::create_dir_all(&self.root)?;
        std::fs::write(
            self.service_path(&task.id),
            Self::render_service(&task.id, &exe_str),
        )?;
        std::fs::write(
            self.timer_path(&task.id),
            Self::render_timer(&task.id, &cal),
        )?;
        run_checked(
            self.runner.as_ref(),
            "systemctl",
            &["--user".to_string(), "daemon-reload".to_string()],
        )?;
        run_checked(
            self.runner.as_ref(),
            "systemctl",
            &[
                "--user".to_string(),
                "enable".to_string(),
                "--now".to_string(),
                Self::timer_name(&task.id),
            ],
        )?;
        Ok(())
    }

    fn uninstall(&self, id: &str) -> anyhow::Result<()> {
        let _ = self.runner.run(
            "systemctl",
            &[
                "--user".to_string(),
                "disable".to_string(),
                "--now".to_string(),
                Self::timer_name(id),
            ],
        );
        for path in [self.timer_path(id), self.service_path(id)] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let _ = self.runner.run(
            "systemctl",
            &["--user".to_string(), "daemon-reload".to_string()],
        );
        Ok(())
    }

    fn status(&self, id: &str) -> InstallState {
        if !self.timer_path(id).exists() {
            return InstallState::Missing;
        }
        let ok = self
            .runner
            .run(
                "systemctl",
                &[
                    "--user".to_string(),
                    "is-enabled".to_string(),
                    Self::timer_name(id),
                ],
            )
            .is_ok_and(|out| out.status.success());
        if ok {
            InstallState::Installed
        } else {
            InstallState::Missing
        }
    }
}

// ---- TaskSched (Windows) ----

#[cfg(any(test, target_os = "windows"))]
pub struct TaskSched {
    pub runner: Arc<dyn CommandRunner + Send + Sync>,
}

#[cfg(any(test, target_os = "windows"))]
impl TaskSched {
    fn task_name(id: &str) -> String {
        format!("jeikcode\\schedule\\{id}")
    }
}

#[cfg(any(test, target_os = "windows"))]
impl OsScheduler for TaskSched {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
        let exe = resolve_schedule_runner_exe()?;
        let desktop_env = std::env::var(DESKTOP_SHELL_BIN_ENV).ok();
        let tr = windows_task_action(desktop_env.as_deref(), &exe, &task.id);
        let tn = Self::task_name(&task.id);
        let sched_args = schtasks_args(&task.schedule)?;
        let mut args = vec![
            "/Create".to_string(),
            "/F".to_string(),
            "/TN".to_string(),
            tn,
            "/TR".to_string(),
            tr,
        ];
        args.extend(sched_args);
        run_checked(self.runner.as_ref(), "schtasks", &args)?;
        Ok(())
    }

    fn uninstall(&self, id: &str) -> anyhow::Result<()> {
        let tn = Self::task_name(id);
        let _ = self.runner.run(
            "schtasks",
            &[
                "/Delete".to_string(),
                "/F".to_string(),
                "/TN".to_string(),
                tn,
            ],
        );
        Ok(())
    }

    fn status(&self, id: &str) -> InstallState {
        let tn = Self::task_name(id);
        let result = self
            .runner
            .run("schtasks", &["/Query".to_string(), "/TN".to_string(), tn]);
        match result {
            Ok(out) if out.status.success() => InstallState::Installed,
            _ => InstallState::Missing,
        }
    }
}

// ---- Platform UID helper ----

#[cfg(any(test, target_os = "macos"))]
fn get_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid() is always safe to call
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ---- current() — platform selector ----

#[cfg(target_os = "macos")]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join("Library/LaunchAgents");
    Ok(Box::new(Launchd {
        root,
        runner: Arc::new(RealCommandRunner),
    }))
}

#[cfg(target_os = "linux")]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".config/systemd/user");
    Ok(Box::new(SystemdTimer {
        root,
        runner: Arc::new(RealCommandRunner),
    }))
}

#[cfg(target_os = "windows")]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    Ok(Box::new(TaskSched {
        runner: Arc::new(RealCommandRunner),
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".config/systemd/user");
    Ok(Box::new(SystemdTimer {
        root,
        runner: Arc::new(RealCommandRunner),
    }))
}

// ---- Shared helpers ----

#[cfg(any(test, target_os = "macos"))]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

fn hhmm(s: &str) -> anyhow::Result<(u8, u8)> {
    parse_hhmm(s)
}

// ---- systemd ----

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
#[derive(Debug, PartialEq, Eq)]
pub enum OnCalendar {
    Calendar(String),
    Interval(String),
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
pub fn systemd_calendar(s: &Schedule) -> anyhow::Result<OnCalendar> {
    Ok(match s {
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            OnCalendar::Calendar(format!("*-*-* {h:02}:{m:02}:00"))
        }
        Schedule::Weekly { weekday, time } => {
            let (h, m) = hhmm(time)?;
            OnCalendar::Calendar(format!("{} *-*-* {h:02}:{m:02}:00", dow_abbr(*weekday)?))
        }
        Schedule::Hourly => OnCalendar::Calendar("hourly".into()),
        Schedule::Interval { every_minutes } => {
            if *every_minutes == 0 {
                anyhow::bail!("interval every_minutes must be > 0");
            }
            OnCalendar::Interval(format!("{every_minutes}min"))
        }
        Schedule::Cron { .. } => {
            anyhow::bail!(
                "cron schedules are not supported on systemd timers; use a simple frequency"
            )
        }
    })
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
fn dow_abbr(wd: u8) -> anyhow::Result<&'static str> {
    if wd == 0 || wd > 7 {
        anyhow::bail!("bad weekday {wd}");
    }
    Ok(["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][(wd - 1) as usize])
}

// ---- launchd ----

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchdTrigger {
    Calendar {
        hour: Option<u8>,
        minute: Option<u8>,
        weekday: Option<u8>,
    },
    Interval(u64),
}

#[cfg(any(test, target_os = "macos"))]
pub fn launchd_calendar(s: &Schedule) -> anyhow::Result<LaunchdTrigger> {
    Ok(match s {
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            LaunchdTrigger::Calendar {
                hour: Some(h),
                minute: Some(m),
                weekday: None,
            }
        }
        Schedule::Weekly { weekday, time } => {
            if *weekday == 0 || *weekday > 7 {
                anyhow::bail!("bad weekday {weekday}");
            }
            let (h, m) = hhmm(time)?;
            LaunchdTrigger::Calendar {
                hour: Some(h),
                minute: Some(m),
                weekday: Some(*weekday % 7),
            }
        }
        Schedule::Hourly => LaunchdTrigger::Calendar {
            hour: None,
            minute: Some(0),
            weekday: None,
        },
        Schedule::Interval { every_minutes } => {
            if *every_minutes == 0 {
                anyhow::bail!("interval every_minutes must be > 0");
            }
            LaunchdTrigger::Interval(*every_minutes as u64 * 60)
        }
        Schedule::Cron { .. } => {
            anyhow::bail!(
                "cron schedules are not supported on macOS launchd; use a simple frequency"
            )
        }
    })
}

// ---- schtasks ----

#[cfg(any(test, target_os = "windows"))]
pub fn schtasks_args(s: &Schedule) -> anyhow::Result<Vec<String>> {
    let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(match s {
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            v(&["/SC", "DAILY", "/ST"])
                .into_iter()
                .chain([format!("{h:02}:{m:02}")])
                .collect()
        }
        Schedule::Weekly { weekday, time } => {
            let (h, m) = hhmm(time)?;
            v(&["/SC", "WEEKLY", "/D"])
                .into_iter()
                .chain([
                    schtasks_dow(*weekday)?.into(),
                    "/ST".into(),
                    format!("{h:02}:{m:02}"),
                ])
                .collect()
        }
        Schedule::Hourly => v(&["/SC", "HOURLY"]),
        Schedule::Interval { every_minutes } => {
            if *every_minutes == 0 || *every_minutes > 1439 {
                anyhow::bail!(
                    "Windows Task Scheduler minute intervals must be 1..=1439 (got {every_minutes})"
                );
            }
            v(&["/SC", "MINUTE", "/MO"])
                .into_iter()
                .chain([every_minutes.to_string()])
                .collect()
        }
        Schedule::Cron { .. } => {
            anyhow::bail!(
                "cron schedules are not supported on Windows Task Scheduler; use a simple frequency"
            )
        }
    })
}

#[cfg(any(test, target_os = "windows"))]
fn schtasks_dow(wd: u8) -> anyhow::Result<&'static str> {
    if wd == 0 || wd > 7 {
        anyhow::bail!("bad weekday {wd}");
    }
    Ok(["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"][(wd - 1) as usize])
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Schedule;

    #[test]
    fn resolve_runner_rejects_daemon_binary_without_cli() {
        let err = resolve_schedule_runner_exe_with(
            None,
            Some(std::path::Path::new("/nonexistent/jeikcode-daemon")),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("jeikcode"));
    }

    #[test]
    fn resolve_runner_honors_env_override() {
        let p = resolve_schedule_runner_exe_with(
            Some("/custom/bin/jeikcode"),
            Some(std::path::Path::new("/other/jeikcode-daemon")),
            None,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/custom/bin/jeikcode"));
    }

    #[test]
    fn resolve_runner_finds_sibling_and_path_cli_binary() {
        let sibling_dir = tempfile::tempdir().unwrap();
        let cli_name = if cfg!(windows) {
            "jeikcode.exe"
        } else {
            "jeikcode"
        };
        let cli_path = sibling_dir.path().join(cli_name);
        std::fs::write(&cli_path, b"").unwrap();
        let daemon_path = sibling_dir.path().join("jeikcode-daemon");

        // Sibling next to jeikcode-daemon wins.
        let found_sibling =
            resolve_schedule_runner_exe_with(None, Some(&daemon_path), None).unwrap();
        assert_eq!(found_sibling, cli_path);

        // When current_exe has no sibling, PATH lookup finds it.
        let other_dir = tempfile::tempdir().unwrap();
        let other_daemon = other_dir.path().join("jeikcode-daemon");
        let found_path = resolve_schedule_runner_exe_with(
            None,
            Some(&other_daemon),
            Some(sibling_dir.path().as_os_str()),
        )
        .unwrap();
        assert_eq!(found_path, cli_path);
    }

    #[test]
    fn windows_task_action_prefers_silent_desktop_wrapper_and_strips_verbatim() {
        let runner = std::path::Path::new(r"\\?\C:\Program Files\JeikCode\jeikcode.exe");
        let cmd = windows_task_action(
            Some(r"\\?\C:\Program Files\JeikCode\jeikcode-desktop.exe"),
            runner,
            "daily-brief-abc123",
        );
        assert_eq!(
            cmd,
            r#""C:\Program Files\JeikCode\jeikcode-desktop.exe" --schedule-run daily-brief-abc123"#
        );
    }

    #[test]
    fn windows_task_action_finds_sibling_desktop_exe_in_parent_of_resources() {
        let root = tempfile::tempdir().unwrap();
        let desktop_exe = root.path().join("jeikcode-desktop.exe");
        std::fs::write(&desktop_exe, b"").unwrap();
        let res_dir = root.path().join("resources");
        std::fs::create_dir_all(&res_dir).unwrap();
        let cli_exe = res_dir.join("jeikcode.exe");
        std::fs::write(&cli_exe, b"").unwrap();

        let cmd = windows_task_action(None, &cli_exe, "t1");
        assert!(
            cmd.contains("jeikcode-desktop.exe") && cmd.contains("--schedule-run t1"),
            "got: {cmd}"
        );
    }

    #[test]
    fn systemd_oncalendar_translation() {
        assert_eq!(
            systemd_calendar(&Schedule::Daily {
                time: "09:30".into()
            })
            .unwrap(),
            OnCalendar::Calendar("*-*-* 09:30:00".into())
        );
        assert_eq!(
            systemd_calendar(&Schedule::Hourly).unwrap(),
            OnCalendar::Calendar("hourly".into())
        );
        assert_eq!(
            systemd_calendar(&Schedule::Interval { every_minutes: 30 }).unwrap(),
            OnCalendar::Interval("30min".into())
        );
        assert_eq!(
            systemd_calendar(&Schedule::Weekly {
                weekday: 1,
                time: "16:00".into()
            })
            .unwrap(),
            OnCalendar::Calendar("Mon *-*-* 16:00:00".into())
        );
    }

    #[test]
    fn launchd_calendar_translation() {
        let d = launchd_calendar(&Schedule::Daily {
            time: "09:30".into(),
        })
        .unwrap();
        assert_eq!(
            d,
            LaunchdTrigger::Calendar {
                hour: Some(9),
                minute: Some(30),
                weekday: None
            }
        );
        assert_eq!(
            launchd_calendar(&Schedule::Interval { every_minutes: 30 }).unwrap(),
            LaunchdTrigger::Interval(1800)
        );
    }

    #[test]
    fn schtasks_args_translation_and_bounds() {
        let a = schtasks_args(&Schedule::Daily {
            time: "09:30".into(),
        })
        .unwrap();
        assert!(
            a.contains(&"/SC".to_string())
                && a.contains(&"DAILY".to_string())
                && a.windows(2).any(|w| w[0] == "/ST" && w[1] == "09:30")
        );
        assert!(schtasks_args(&Schedule::Interval {
            every_minutes: 1440
        })
        .is_err());
        assert!(schtasks_args(&Schedule::Daily {
            time: "25:00".into()
        })
        .is_err());
    }

    #[test]
    fn cron_kind_rejected_on_all_platforms() {
        assert!(launchd_calendar(&Schedule::Cron {
            expr: "0 9 * * *".into()
        })
        .is_err());
        assert!(schtasks_args(&Schedule::Cron {
            expr: "0 9 * * *".into()
        })
        .is_err());
        assert!(systemd_calendar(&Schedule::Cron {
            expr: "0 9 * * *".into()
        })
        .is_err());
    }

    #[test]
    fn launchd_weekly_sunday_conversion() {
        assert_eq!(
            launchd_calendar(&Schedule::Weekly {
                weekday: 7,
                time: "08:00".into()
            })
            .unwrap(),
            LaunchdTrigger::Calendar {
                hour: Some(8),
                minute: Some(0),
                weekday: Some(0)
            }
        );
    }

    #[test]
    fn invalid_weekday_zero_rejected() {
        assert!(dow_abbr(0).is_err());
        assert!(launchd_calendar(&Schedule::Weekly {
            weekday: 0,
            time: "08:00".into()
        })
        .is_err());
        assert!(schtasks_args(&Schedule::Weekly {
            weekday: 0,
            time: "08:00".into()
        })
        .is_err());
    }

    struct FakeRunner {
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
        fail_if: Option<(String, String)>,
    }

    impl FakeRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Default::default(),
                fail_if: None,
            })
        }

        fn failing(prog_substr: &str, arg_substr: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: Default::default(),
                fail_if: Some((prog_substr.into(), arg_substr.into())),
            })
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, p: &str, a: &[String]) -> std::io::Result<std::process::Output> {
            self.calls.lock().unwrap().push((p.into(), a.to_vec()));
            if let Some((prog_sub, arg_sub)) = &self.fail_if {
                if p.contains(prog_sub.as_str()) && a.iter().any(|x| x.contains(arg_sub.as_str())) {
                    return Ok(std::process::Output {
                        status: {
                            #[cfg(unix)]
                            {
                                std::os::unix::process::ExitStatusExt::from_raw(1)
                            }
                            #[cfg(windows)]
                            {
                                std::os::windows::process::ExitStatusExt::from_raw(1)
                            }
                            #[cfg(not(any(unix, windows)))]
                            {
                                Default::default()
                            }
                        },
                        stdout: vec![],
                        stderr: b"simulated failure".to_vec(),
                    });
                }
            }
            Ok(std::process::Output {
                status: Default::default(),
                stdout: vec![],
                stderr: vec![],
            })
        }
    }

    fn sample_task(id: &str) -> ScheduleTask {
        ScheduleTask {
            id: id.into(),
            title: "Test task".into(),
            prompt: "do something".into(),
            cwd: "/tmp".into(),
            schedule: Schedule::Daily {
                time: "09:30".into(),
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
    fn systemd_install_writes_units_and_enables() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = SystemdTimer {
            root: tmp.path().to_path_buf(),
            runner: runner.clone(),
        };
        let task = sample_task("t1");
        sched.install(&task).unwrap();
        assert!(tmp.path().join("jeikcode-schedule-t1.service").exists());
        let timer_content =
            std::fs::read_to_string(tmp.path().join("jeikcode-schedule-t1.timer")).unwrap();
        assert!(timer_content.contains("OnCalendar=*-*-* 09:30:00"));
        assert!(timer_content.contains("Persistent=true"));
        assert_eq!(sched.status("t1"), InstallState::Installed);
        sched.uninstall("t1").unwrap();
        assert!(!tmp.path().join("jeikcode-schedule-t1.timer").exists());
        assert_eq!(sched.status("t1"), InstallState::Missing);
    }

    #[test]
    fn systemd_interval_includes_on_active_sec_and_on_unit_active_sec() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = SystemdTimer {
            root: tmp.path().to_path_buf(),
            runner,
        };
        let mut task = sample_task("t2");
        task.schedule = Schedule::Interval { every_minutes: 15 };
        sched.install(&task).unwrap();
        let timer_content =
            std::fs::read_to_string(tmp.path().join("jeikcode-schedule-t2.timer")).unwrap();
        assert!(timer_content.contains("OnActiveSec=15min"));
        assert!(timer_content.contains("OnUnitActiveSec=15min"));
        assert!(timer_content.contains("Persistent=true"));
    }

    #[test]
    fn systemd_status_reports_missing_when_is_enabled_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::failing("systemctl", "is-enabled");
        let sched = SystemdTimer {
            root: tmp.path().to_path_buf(),
            runner,
        };
        let task = sample_task("t_status");
        sched.install(&task).unwrap();
        assert_eq!(sched.status("t_status"), InstallState::Missing);
    }

    #[test]
    fn launchd_install_writes_plist_and_bootstraps() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = Launchd {
            root: tmp.path().to_path_buf(),
            runner: runner.clone(),
        };
        let task = sample_task("lt1");
        sched.install(&task).unwrap();
        let plist_path = tmp.path().join("com.jeikcode.schedule.lt1.plist");
        assert!(plist_path.exists());
        assert_eq!(sched.status("lt1"), InstallState::Installed);
        sched.uninstall("lt1").unwrap();
        assert!(!plist_path.exists());
        assert_eq!(sched.status("lt1"), InstallState::Missing);
    }

    #[test]
    fn tasksched_install_calls_schtasks_create() {
        let runner = FakeRunner::new();
        let sched = TaskSched {
            runner: runner.clone(),
        };
        let task = sample_task("ws1");
        sched.install(&task).unwrap();
        let calls = runner.calls();
        assert!(calls
            .iter()
            .any(|(p, a)| p == "schtasks" && a.iter().any(|x| x == "/Create")));
        sched.uninstall("ws1").unwrap();
        assert!(runner
            .calls()
            .iter()
            .any(|(p, a)| p == "schtasks" && a.iter().any(|x| x == "/Delete")));
    }

    #[test]
    fn xml_escape_works() {
        assert_eq!(xml_escape("a&<b>\"c\""), "a&amp;&lt;b&gt;&quot;c&quot;");
    }
}
