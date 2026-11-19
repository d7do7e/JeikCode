// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 薄桌面壳：拉起同一个 `jeikcode webui`，窗口只打开它。
//! 直接就地（In-Place）执行安装目录下的 `jeikcode`，单一来源运行，消除运行期文件释放与杀毒误报。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jeikcode_config::desktop_shared::{
    cleanup_legacy_dropper_files, desktop_settings_path, is_loopback_host, load_close_action,
    load_webui_listen, migrate_hkcu_path, save_close_action_at, save_webui_listen_pref,
    strip_windows_verbatim_path, DESKTOP_CLI_BIN_ENV, DESKTOP_ENV, DESKTOP_MANAGED_ENV,
    DESKTOP_PID_ENV, DESKTOP_SHELL_BIN_ENV, RESTART_EXACT_PORT_ENV, RESTART_LISTEN_PORT_ENV,
    RESTART_REQUESTED_EXIT_CODE, SERVER_TOKEN_ENV,
};
use jeikcode_config::i18n::resolve_locale_from_disk;
use jeikcode_config::locale::Locale;
use jeikcode_config::schedule::valid_id;
use jeikcode_config::schedule_os::resolve_schedule_runner_exe;
use jeikcode_config::util::real_home_dir;
use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

static DESKTOP_SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// 这次桌面壳拉起的 WebUI。端口可能因占用从 13457 顺延。
#[derive(Clone, Debug, PartialEq, Eq)]
struct WebuiOrigin {
    host: String,
    port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkAction {
    StayInWebview,
    OpenExternal,
}

/// Tauri 自己的页面。交给系统浏览器会去连一个不存在的 `tauri.localhost`。
fn is_shell_internal_url(url: &tauri::Url) -> bool {
    let scheme = url.scheme();
    if matches!(
        scheme,
        "tauri" | "about" | "data" | "ipc" | "asset" | "blob"
    ) {
        return true;
    }
    if scheme == "http" || scheme == "https" {
        if let Some(host) = url.host_str() {
            return host.eq_ignore_ascii_case("tauri.localhost");
        }
    }
    false
}

fn is_launched_webui(url: &tauri::Url, origin: &WebuiOrigin) -> bool {
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let Some(port) = url.port() else {
        return false;
    };
    if port != origin.port {
        return false;
    }
    host.eq_ignore_ascii_case(&origin.host)
        || (is_loopback_host(host) && is_loopback_host(&origin.host))
}

/// 顶层导航：只有壳页面和这次的 WebUI 留在窗口里。
/// `http://localhost:3000` 这类本机开发服务改由系统浏览器打开，避免盖掉当前界面。
fn navigation_action(url: &tauri::Url, origin: Option<&WebuiOrigin>) -> LinkAction {
    if is_shell_internal_url(url) {
        return LinkAction::StayInWebview;
    }
    // 仅允许当前已拉起的 WebUI 端口，或在重启交接时允许配置文件中的目标回环端口；
    // 不再凭 `desktop=` 查询参数放行任意本地端口。
    if origin.is_some_and(|origin| is_launched_webui(url, origin)) {
        return LinkAction::StayInWebview;
    }
    if origin.is_some() {
        if let Some(host) = url.host_str() {
            if is_loopback_host(host) {
                let (_h, saved_port, _t) = load_webui_listen();
                if url.port() == Some(saved_port) {
                    return LinkAction::StayInWebview;
                }
            }
        }
    }
    LinkAction::OpenExternal
}

/// Markdown 链接带 `target="_blank"`，走新窗口而不是顶层导航。
/// 除 Tauri 虚拟地址外一律交给系统浏览器，包括本机其它端口。
fn new_window_action(url: &tauri::Url) -> LinkAction {
    if is_shell_internal_url(url) {
        LinkAction::StayInWebview
    } else {
        LinkAction::OpenExternal
    }
}

fn remember_webui_origin(slot: &Mutex<Option<WebuiOrigin>>, url: &str) {
    let Ok(parsed) = url.parse::<tauri::Url>() else {
        return;
    };
    let (Some(host), Some(port)) = (parsed.host_str(), parsed.port()) else {
        return;
    };
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(WebuiOrigin {
        host: host.to_string(),
        port,
    });
}

/// Quiet only while the native window is in front. Minimized stays away even
/// if the webview still reports itself focused.
fn host_window_away(minimized: bool, focused: bool) -> bool {
    minimized || !focused
}

fn read_host_away(window: &tauri::WebviewWindow) -> Option<bool> {
    let minimized = window.is_minimized().ok()?;
    let focused = window.is_focused().ok()?;
    Some(host_window_away(minimized, focused))
}

fn publish_host_away(window: &tauri::WebviewWindow, away: bool) {
    let _ = window.eval(format!("window.__jeikcodeHostAway={away};"));
}

fn show_main_window(window: &tauri::WebviewWindow) {
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
    #[cfg(target_os = "windows")]
    if let Ok(hwnd) = window.hwnd() {
        #[link(name = "user32")]
        extern "system" {
            fn ShowWindow(hWnd: *mut std::ffi::c_void, nCmdShow: i32) -> i32;
            fn SetForegroundWindow(hWnd: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            // SW_RESTORE (9) 确保从隐藏/最小化状态恢复到正常窗口大小并展示，避免 SW_SHOW 与 SW_RESTORE 重复调用导致状态失步
            ShowWindow(hwnd.0 as *mut std::ffi::c_void, 9);
            SetForegroundWindow(hwnd.0 as *mut std::ffi::c_void);
        }
    }
    publish_host_away(window, false);
}

fn hide_window_to_tray(window: &tauri::WebviewWindow) {
    let _ = window.hide();
    // 兜底直接调用 Win32 ShowWindow(SW_HIDE)：防止外部快捷方式曾直接操作 HWND 导致
    // tao 内部 WindowFlags::VISIBLE 标志位与真实窗口显隐状态失步而忽略 hide()。
    #[cfg(target_os = "windows")]
    if let Ok(hwnd) = window.hwnd() {
        #[link(name = "user32")]
        extern "system" {
            fn ShowWindow(hWnd: *mut std::ffi::c_void, nCmdShow: i32) -> i32;
        }
        unsafe {
            ShowWindow(hwnd.0 as *mut std::ffi::c_void, 0);
        }
    }
    publish_host_away(window, true);
}

fn send_desktop_shutdown(port: u16, token: Option<&str>) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    let auth_header = match token {
        Some(t) if !t.is_empty() => format!("Authorization: Bearer {t}\r\n"),
        _ => String::new(),
    };
    let req = format!(
        "POST /shutdown HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth_header}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(req.as_bytes());
}

#[cfg(target_os = "windows")]
fn cleanup_desktop_owned_children_windows() {
    #[allow(non_snake_case)]
    #[repr(C)]
    struct ProcessEntry32W {
        dwSize: u32,
        cntUsage: u32,
        th32ProcessID: u32,
        th32DefaultHeapID: usize,
        th32ModuleID: u32,
        cntThreads: u32,
        th32ParentProcessID: u32,
        pcPriClassBase: i32,
        dwFlags: u32,
        szExeFile: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> *mut std::ffi::c_void;
        fn Process32FirstW(hSnapshot: *mut std::ffi::c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(hSnapshot: *mut std::ffi::c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(
            dwDesiredAccess: u32,
            bInheritHandle: i32,
            dwProcessId: u32,
        ) -> *mut std::ffi::c_void;
        fn TerminateProcess(hProcess: *mut std::ffi::c_void, uExitCode: u32) -> i32;
        fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
    }

    let my_pid = std::process::id();
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_TERMINATE: u32 = 0x0001;

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == -1isize as *mut std::ffi::c_void {
        return;
    }

    let mut entry = ProcessEntry32W {
        dwSize: std::mem::size_of::<ProcessEntry32W>() as u32,
        cntUsage: 0,
        th32ProcessID: 0,
        th32DefaultHeapID: 0,
        th32ModuleID: 0,
        cntThreads: 0,
        th32ParentProcessID: 0,
        pcPriClassBase: 0,
        dwFlags: 0,
        szExeFile: [0; 260],
    };

    if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            if entry.th32ParentProcessID == my_pid && entry.th32ProcessID != my_pid {
                let name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_matches('\0')
                    .to_ascii_lowercase();
                if name.starts_with("jeikcode") || name.starts_with("cli-bin") {
                    let h_proc = unsafe { OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID) };
                    if !h_proc.is_null() {
                        unsafe {
                            TerminateProcess(h_proc, 1);
                            CloseHandle(h_proc);
                        }
                    }
                }
            }
            if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe {
        CloseHandle(snapshot);
    }
}

fn terminate_managed_child(app: &tauri::AppHandle) {
    DESKTOP_SHUTTING_DOWN.store(true, Ordering::SeqCst);
    if let Some(slot) = app.try_state::<ChildSlot>() {
        let taken = slot.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut child) = taken {
            #[cfg(unix)]
            {
                let pid = child.id() as i32;
                if pid > 0 {
                    extern "C" {
                        fn kill(pid: i32, sig: i32) -> i32;
                    }
                    const SIGTERM: i32 = 15;
                    unsafe {
                        let _ = kill(-pid, SIGTERM);
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    // 兜底 1：读取当前配置端口，向 /shutdown 发送优雅关停请求（即使子进程句柄丢失也能通知关停）
    let (_host, port, token) = load_webui_listen();
    if port > 0 {
        send_desktop_shutdown(port, token.as_deref());
    }
    // 兜底 2：在 Windows 上，强力清理属于本桌面壳 PID 的后台子进程，彻底消除改参数重启后脱离的孤儿子进程
    #[cfg(target_os = "windows")]
    cleanup_desktop_owned_children_windows();
}

fn store_child_unless_shutting_down(
    app: &tauri::AppHandle,
    mut child: std::process::Child,
) -> bool {
    if let Some(slot) = app.try_state::<ChildSlot>() {
        let mut guard = slot.0.lock().unwrap_or_else(|e| e.into_inner());
        if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
            drop(guard);
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        *guard = Some(child);
        return true;
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

fn tray_locale_from_query(raw: &str) -> Option<Locale> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Locale::from_str(trimmed).ok()
}

fn update_tray_texts_for_app(app: &tauri::AppHandle, locale: Locale) {
    let texts = tray_texts_for_locale(locale);
    if let Some(state) = app.try_state::<TrayUiState>() {
        if let Some(handles) = state.0.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = handles.tray.set_tooltip(Some(texts.tooltip));
            let _ = handles.show_item.set_text(texts.show_window);
            let _ = handles.quit_item.set_text(texts.quit);
        }
    }
}

fn is_authorized_shell_action_url(url: &tauri::Url, origin: Option<&WebuiOrigin>) -> bool {
    is_shell_internal_url(url) || origin.is_some_and(|o| is_launched_webui(url, o))
}

fn handle_desktop_shell_action(
    url: &tauri::Url,
    app: &tauri::AppHandle,
    origin: Option<&WebuiOrigin>,
) -> bool {
    if !url.path().starts_with("/__desktop_action/") {
        return false;
    }
    if !is_authorized_shell_action_url(url, origin) {
        return false;
    }
    match url.path() {
        "/__desktop_action/hide_to_tray" => {
            if let Some(window) = app.get_webview_window("main") {
                hide_window_to_tray(&window);
            }
            true
        }
        "/__desktop_action/quit_app" => {
            terminate_managed_child(app);
            app.exit(0);
            true
        }
        "/__desktop_action/set_close_action" => {
            if let Some(action) = url
                .query_pairs()
                .find(|(k, _)| k == "action")
                .map(|(_, v)| v.trim().to_string())
            {
                let _ = save_close_action_at(&desktop_settings_path(), &action);
            }
            true
        }
        "/__desktop_action/set_language" => {
            let locale = url
                .query_pairs()
                .find(|(k, _)| k == "lang")
                .and_then(|(_, v)| tray_locale_from_query(&v))
                .unwrap_or_else(|| resolve_locale_from_disk(None));
            update_tray_texts_for_app(app, locale);
            true
        }
        _ => false,
    }
}

/// The page's document.hidden / hasFocus miss a minimized WebView. Push the
/// native state so a selected session still notifies after minimize.
fn watch_host_presence(window: tauri::WebviewWindow) {
    let listener = window.clone();
    window.on_window_event(move |event| {
        if !matches!(event, tauri::WindowEvent::Focused(_)) {
            return;
        }
        if let Some(away) = read_host_away(&listener) {
            publish_host_away(&listener, away);
        }
    });

    std::thread::spawn(move || {
        let mut last = None;
        loop {
            let Some(away) = read_host_away(&window) else {
                break;
            };
            if last != Some(away) {
                publish_host_away(&window, away);
                last = Some(away);
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    });
}

/// `0.0.0.0` / `::` 是监听地址，浏览器打不开。打开前改成回环。
fn external_browser_url(url: &tauri::Url) -> String {
    let bare = url
        .host_str()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']');
    if bare == "0.0.0.0" || bare == "::" {
        let mut rewritten = url.clone();
        if rewritten.set_host(Some("127.0.0.1")).is_ok() {
            return rewritten.to_string();
        }
    }
    url.to_string()
}

fn open_in_external_browser(url: &str) {
    if url.contains("tauri.localhost")
        || url.starts_with("tauri://")
        || url.starts_with("about:")
        || url.starts_with("data:")
    {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                hwnd: *mut std::ffi::c_void,
                lpOperation: *const u16,
                lpFile: *const u16,
                lpParameters: *const u16,
                lpDirectory: *const u16,
                nShowCmd: i32,
            ) -> isize;
        }

        fn wide(s: &str) -> Vec<u16> {
            OsStr::new(s)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        }

        let verb = wide("open");
        let file = wide(url);
        // SW_SHOWNORMAL = 1
        let rc = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if rc > 32 {
            return;
        }
        // Fallback
        let _ = Command::new("explorer").arg(url).spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
    }
}

#[cfg(target_os = "windows")]
fn ensure_single_instance_windows() -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            lpMutexAttributes: *mut std::ffi::c_void,
            bInitialOwner: i32,
            lpName: *const u16,
        ) -> *mut std::ffi::c_void;
        fn CreateEventW(
            lpEventAttributes: *mut std::ffi::c_void,
            bManualReset: i32,
            bInitialState: i32,
            lpName: *const u16,
        ) -> *mut std::ffi::c_void;
        fn SetEvent(hEvent: *mut std::ffi::c_void) -> i32;
        fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn AllowSetForegroundWindow(dwProcessId: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let mutex_name = wide("Local\\JeikCode_Desktop_SingleInstance_Mutex");
    let event_name = wide("Local\\JeikCode_Desktop_Activate_Event");
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const ASFW_ANY: u32 = 0xFFFF_FFFF;

    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 1, mutex_name.as_ptr()) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
            // 触发激活命名事件，由主实例主线程规范执行窗口还原与激活，
            // 绝不在外部进程跨进程直接调用 ShowWindow 篡改 HWND 导致 tao 内部 WindowFlags 失步。
            let ev = CreateEventW(std::ptr::null_mut(), 0, 0, event_name.as_ptr());
            if !ev.is_null() {
                SetEvent(ev);
                CloseHandle(ev);
            }
        }
        return true;
    }
    let _ = handle;
    false
}

#[cfg(target_os = "windows")]
fn spawn_single_instance_activation_listener(window: tauri::WebviewWindow) {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(
            lpEventAttributes: *mut std::ffi::c_void,
            bManualReset: i32,
            bInitialState: i32,
            lpName: *const u16,
        ) -> *mut std::ffi::c_void;
        fn WaitForSingleObject(hHandle: *mut std::ffi::c_void, dwMilliseconds: u32) -> u32;
        fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
    }

    let event_name: Vec<u16> = std::ffi::OsStr::new("Local\\JeikCode_Desktop_Activate_Event")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let ev = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, event_name.as_ptr()) };
    if ev.is_null() {
        return;
    }
    let ev_addr = ev as usize;
    let window_for_thread = window.clone();
    std::thread::spawn(move || {
        let handle = ev_addr as *mut std::ffi::c_void;
        const WAIT_OBJECT_0: u32 = 0;
        loop {
            if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
                break;
            }
            let rc = unsafe { WaitForSingleObject(handle, 400) };
            if rc == WAIT_OBJECT_0 {
                // 必须调度到主事件循环线程执行还原与聚焦，确保 tao 内部 WindowFlags::VISIBLE 100% 同步恢复
                let win = window_for_thread.clone();
                let _ = window_for_thread.run_on_main_thread(move || {
                    show_main_window(&win);
                });
            }
        }
        unsafe {
            CloseHandle(handle);
        }
    });
}

/// Fast-path for `--schedule-run <id>` when invoked by Windows Task Scheduler.
/// Runs `jeikcode.exe schedule run <id>` with `CREATE_NO_WINDOW` and null stdio so
/// scheduled background tasks never flash or open a terminal window.
fn maybe_run_scheduled_task_cli() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--schedule-run" {
            let id = match args.next() {
                Some(id) if valid_id(&id) => id,
                _ => return Some(2),
            };
            let exe = match resolve_schedule_runner_exe() {
                Ok(p) => p,
                Err(_) => return Some(1),
            };
            let mut cmd = Command::new(exe);
            cmd.args(["schedule", "run", &id])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            suppress_console(&mut cmd);
            let code = cmd.status().map(|s| s.code().unwrap_or(1)).unwrap_or(1);
            return Some(code);
        }
    }
    None
}

fn main() {
    if let Some(code) = maybe_run_scheduled_task_cli() {
        std::process::exit(code);
    }

    #[cfg(target_os = "windows")]
    if ensure_single_instance_windows() {
        return;
    }

    let child = Mutex::new(None);
    tauri::Builder::default()
        .manage(ChildSlot(child))
        .manage(TrayUiState(Mutex::new(None)))
        .setup(|app| {
            let webui_origin = Arc::new(Mutex::new(None));
            let nav_origin = Arc::clone(&webui_origin);
            let new_win_origin = Arc::clone(&webui_origin);
            let nav_app_handle = app.handle().clone();
            let new_win_app_handle = app.handle().clone();
            let window_builder = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("JeikCode Desktop")
            .inner_size(1280.0, 800.0)
            .resizable(true)
            .on_page_load(|window, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    if let Some(away) = read_host_away(&window) {
                        publish_host_away(&window, away);
                    }
                }
            })
            .on_navigation(move |url| {
                let current_origin = nav_origin.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if handle_desktop_shell_action(url, &nav_app_handle, current_origin.as_ref()) {
                    return false;
                }
                let action = navigation_action(url, current_origin.as_ref());
                match action {
                    LinkAction::StayInWebview => {
                        if !is_shell_internal_url(url) {
                            remember_webui_origin(&nav_origin, url.as_str());
                        }
                        true
                    }
                    LinkAction::OpenExternal => {
                        open_in_external_browser(&external_browser_url(url));
                        false
                    }
                }
            })
            .on_new_window(move |url, _features| {
                let current_origin = new_win_origin
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if handle_desktop_shell_action(&url, &new_win_app_handle, current_origin.as_ref())
                {
                    return tauri::webview::NewWindowResponse::Deny;
                }
                if new_window_action(&url) == LinkAction::OpenExternal {
                    open_in_external_browser(&external_browser_url(&url));
                }
                tauri::webview::NewWindowResponse::Deny
            });
            // Windows 上有两套原生放置目标，都会把「不是系统文件」的拖放写成 DROPEFFECT_NONE，
            // 光标就是禁止。disable_drag_drop_handler / tauri.conf.json 的 dragDropEnabled
            // 只卸 WebView2 那一套；窗口自己的 FileDropHandler 还在，应用内拖文件树、标签照样被禁。
            // 两套都关掉，拖放才进页面里的 HTML5。
            #[cfg(target_os = "windows")]
            let window_builder = window_builder.disable_drag_drop_handler().drag_and_drop(false);
            let window = window_builder.build()?;

            #[cfg(target_os = "windows")]
            spawn_single_instance_activation_listener(window.clone());

            watch_host_presence(window.clone());
            #[cfg(not(target_os = "windows"))]
            let drop_window = window.clone();
            let close_window = window.clone();
            let app_handle_for_close = app.handle().clone();

            let last_close_time = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
            let last_close_for_event = Arc::clone(&last_close_time);

            // 窗口关闭拦截与 HTML5 拖放事件处理
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let action = load_close_action();
                    match action.as_str() {
                        "tray" => {
                            api.prevent_close();
                            hide_window_to_tray(&close_window);
                        }
                        "quit" => {
                            terminate_managed_child(&app_handle_for_close);
                        }
                        _ => {
                            api.prevent_close();
                            // 连点容错兜底：若 1.2 秒内再次点击 X，说明弹窗未能正常捕获或用户强行关闭，立即最小化到托盘
                            let mut last = last_close_for_event
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let now = Instant::now();
                            if now.duration_since(*last) < Duration::from_millis(1200) {
                                *last = now - Duration::from_secs(10);
                                hide_window_to_tray(&close_window);
                            } else {
                                *last = now;
                                let _ = close_window.eval(
                                    "window.dispatchEvent(new CustomEvent('jeikcode:desktop:close_prompt'));",
                                );
                            }
                        }
                    }
                }
                // Windows disables native FileDropHandler so HTML5 in-app drag-and-drop works without
                // DROPEFFECT_NONE; OS file drops on Windows enter WebUI via DataTransfer.files.
                #[cfg(not(target_os = "windows"))]
                if let tauri::WindowEvent::DragDrop(drop_event) = event {
                    if let tauri::DragDropEvent::Drop { paths, .. } = drop_event {
                        for path in paths {
                            let p_str = path.to_string_lossy().replace('\\', "/");
                            let json_path = serde_json::to_string(&p_str).unwrap_or_default();
                            let script = format!(
                                "window.dispatchEvent(new CustomEvent('jeikcode:switch_right_tab', {{ detail: 'preview' }}));\
                                 window.dispatchEvent(new CustomEvent('jeikcode:preview:open_file', {{ detail: {{ filePath: {json_path} }} }}));\
                                 window.dispatchEvent(new CustomEvent('jeikcode:open_file', {{ detail: {{ path: {json_path} }} }}));"
                            );
                            let _ = drop_window.eval(&script);
                        }
                    }
                }
            });
            setup_system_tray(app).expect("初始化系统托盘图标失败");

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match start_webui(&handle) {
                    Ok((url, child, eof_rx, launch)) => {
                        if !store_child_unless_shutting_down(&handle, child) {
                            return;
                        }
                        // 必须先记下地址再导航，否则这次 WebUI 会被当成外链弹出浏览器。
                        remember_webui_origin(&webui_origin, &url);
                        let _ = window.navigate(
                            url.parse()
                                .unwrap_or_else(|_| "about:blank".parse().expect("about:blank")),
                        );
                        supervise_webui(
                            handle.clone(),
                            window.clone(),
                            webui_origin.clone(),
                            eof_rx,
                            launch,
                        );
                    }
                    Err(err) => {
                        let text =
                            serde_json::to_string(&err).unwrap_or_else(|_| "\"启动失败\"".into());
                        let _ = window.eval(&format!(
                            "document.getElementById('status').textContent = {text}"
                        ));
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动 JeikCode Desktop 失败")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                terminate_managed_child(app);
            }
        });
}

struct TrayTexts {
    tooltip: &'static str,
    show_window: &'static str,
    quit: &'static str,
}

fn tray_texts_for_locale(locale: Locale) -> TrayTexts {
    match locale {
        Locale::ZhCn => TrayTexts {
            tooltip: "JeikCode Desktop - AI 编程助手",
            show_window: "显示主界面",
            quit: "退出",
        },
        Locale::ZhTw => TrayTexts {
            tooltip: "JeikCode Desktop - AI 程式設計助手",
            show_window: "顯示主視窗",
            quit: "結束",
        },
        Locale::Vi => TrayTexts {
            tooltip: "JeikCode Desktop - Trợ lý lập trình AI",
            show_window: "Hiển thị giao diện",
            quit: "Thoát",
        },
        Locale::Ja => TrayTexts {
            tooltip: "JeikCode Desktop - AI コーディングアシスタント",
            show_window: "ウィンドウを表示",
            quit: "終了",
        },
        Locale::Ko => TrayTexts {
            tooltip: "JeikCode Desktop - AI 코딩 어시스턴트",
            show_window: "창 표시",
            quit: "종료",
        },
        Locale::Ru => TrayTexts {
            tooltip: "JeikCode Desktop - AI-помощник программиста",
            show_window: "Показать окно",
            quit: "Выход",
        },
        Locale::Fr => TrayTexts {
            tooltip: "JeikCode Desktop - Assistant de programmation IA",
            show_window: "Afficher la fenêtre",
            quit: "Quitter",
        },
        Locale::Es => TrayTexts {
            tooltip: "JeikCode Desktop - Asistente de programación IA",
            show_window: "Mostrar ventana",
            quit: "Salir",
        },
        Locale::De => TrayTexts {
            tooltip: "JeikCode Desktop - KI-Programmierassistent",
            show_window: "Fenster anzeigen",
            quit: "Beenden",
        },
        Locale::Ar => TrayTexts {
            tooltip: "JeikCode Desktop - مساعد البرمجة بالذكاء الاصطناعي",
            show_window: "إظهار النافذة",
            quit: "إنهاء",
        },
        Locale::En => TrayTexts {
            tooltip: "JeikCode Desktop - AI Coding Agent",
            show_window: "Show Window",
            quit: "Quit",
        },
    }
}

fn resolve_tray_texts() -> TrayTexts {
    tray_texts_for_locale(resolve_locale_from_disk(None))
}

struct TrayUiHandles {
    tray: TrayIcon,
    show_item: MenuItem<tauri::Wry>,
    quit_item: MenuItem<tauri::Wry>,
}

struct TrayUiState(Mutex<Option<TrayUiHandles>>);

fn setup_system_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let texts = resolve_tray_texts();
    let show_item = MenuItemBuilder::with_id("show", texts.show_window).build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", texts.quit).build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&show_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let icon = app
        .default_window_icon()
        .cloned()
        .expect("未在 tauri.conf.json 中找到应用默认图标");

    let tray = TrayIconBuilder::with_id("main-tray")
        .tooltip(texts.tooltip)
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    show_main_window(&window);
                }
            }
            "quit" => {
                terminate_managed_child(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let is_visible = window.is_visible().unwrap_or(false);
                    let is_minimized = window.is_minimized().unwrap_or(false);
                    let is_focused = window.is_focused().unwrap_or(false);
                    if is_visible && !is_minimized && is_focused {
                        hide_window_to_tray(&window);
                    } else {
                        show_main_window(&window);
                    }
                }
            }
        })
        .build(app)?;

    if let Some(state) = app.try_state::<TrayUiState>() {
        *state.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(TrayUiHandles {
            tray,
            show_item,
            quit_item,
        });
    }

    Ok(())
}

struct ChildSlot(Mutex<Option<std::process::Child>>);

struct WebuiLaunch {
    bin: PathBuf,
    host: String,
    port: u16,
    token: String,
    home: PathBuf,
    bundled: Option<PathBuf>,
}

fn start_webui(
    app: &tauri::AppHandle,
) -> Result<
    (
        String,
        std::process::Child,
        std::sync::mpsc::Receiver<()>,
        WebuiLaunch,
    ),
    String,
> {
    let bundled = bundled_cli(app);
    let bin = resolve_desktop_cli(bundled.as_deref());
    let home = real_home_dir().unwrap_or_else(|| PathBuf::from("."));
    let (host, raw_port, saved_token) = load_webui_listen();
    let port = prepare_desktop_port_for_launch(raw_port, &bin, saved_token.as_deref());
    let token = saved_token.unwrap_or_else(|| mint_desktop_token(&host, port));
    let mut launch = WebuiLaunch {
        bin,
        host,
        port,
        token,
        home,
        bundled,
    };
    let (url, child, eof_rx) = spawn_desktop_server(&launch, false, Duration::from_secs(90))?;
    let probe_port = url
        .parse::<tauri::Url>()
        .ok()
        .and_then(|parsed| parsed.port())
        .unwrap_or(launch.port);
    if probe_port != launch.port {
        launch.port = probe_port;
        let _ = save_webui_listen_pref(Some(&launch.host), probe_port, Some(&launch.token));
    }
    Ok((url, child, eof_rx, launch))
}

#[cfg(windows)]
fn assign_child_to_desktop_job(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(
            lpJobAttributes: *mut std::ffi::c_void,
            lpName: *const u16,
        ) -> *mut std::ffi::c_void;
        fn SetInformationJobObject(
            hJob: *mut std::ffi::c_void,
            JobObjectInfoClass: u32,
            lpJobObjectInfo: *const std::ffi::c_void,
            cbJobObjectInfoLength: u32,
        ) -> i32;
        fn AssignProcessToJobObject(
            hJob: *mut std::ffi::c_void,
            hProcess: *mut std::ffi::c_void,
        ) -> i32;
    }

    static JOB_HANDLE: OnceLock<usize> = OnceLock::new();
    let job_ptr = *JOB_HANDLE.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
        const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
        const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x0000_0800;

        let mut info = JobObjectExtendedLimitInformation::default();
        info.basic_limit_information.limit_flags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let _ = SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
        );
        job as usize
    });

    if job_ptr != 0 {
        unsafe {
            let _ = AssignProcessToJobObject(
                job_ptr as *mut std::ffi::c_void,
                child.as_raw_handle() as *mut std::ffi::c_void,
            );
        }
    }
}

#[cfg(not(windows))]
fn assign_child_to_desktop_job(_child: &std::process::Child) {}

fn spawn_desktop_server(
    launch: &WebuiLaunch,
    exact_port: bool,
    url_timeout: Duration,
) -> Result<(String, std::process::Child, std::sync::mpsc::Receiver<()>), String> {
    let mut cmd = Command::new(&launch.bin);
    cmd.arg("webui")
        .arg("--host")
        .arg(&launch.host)
        .arg("--port")
        .arg(launch.port.to_string())
        .arg("--no-open");
    if !launch.token.trim().is_empty() {
        cmd.env(SERVER_TOKEN_ENV, &launch.token);
    }
    cmd.env(DESKTOP_ENV, "1");
    cmd.env(DESKTOP_MANAGED_ENV, "1");
    cmd.env(DESKTOP_PID_ENV, std::process::id().to_string());
    if exact_port {
        // 与 jeikcode-updater 的同名变量一致：只绑定这个端口，不顺延，也不关闭别的进程。
        cmd.env(RESTART_EXACT_PORT_ENV, "1");
        cmd.env(RESTART_LISTEN_PORT_ENV, launch.port.to_string());
    }
    if let Some(ref bundled) = launch.bundled {
        cmd.env(DESKTOP_CLI_BIN_ENV, strip_windows_verbatim_path(bundled));
    }
    if let Ok(self_exe) = std::env::current_exe() {
        cmd.env(
            DESKTOP_SHELL_BIN_ENV,
            strip_windows_verbatim_path(&self_exe),
        );
    }
    // 开启 stdin 管道作为跨平台生命线（ChildSlot 持有 ChildStdin 写端；一旦桌面壳退出，
    // OS 内核立即关闭管道，子进程阻塞 read 瞬间收到 EOF 退出，0 轮询）。
    cmd.current_dir(&launch.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    suppress_console(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 {}：{e}", launch.bin.display()))?;
    assign_child_to_desktop_job(&child);
    let stderr = child.stderr.take();
    let (url_result, eof_rx) = match stderr {
        Some(pipe) => read_url(pipe, url_timeout),
        None => {
            let (_tx, rx) = std::sync::mpsc::channel();
            (Ok(None), rx)
        }
    };
    let mut url = match url_result {
        Ok(Some(url)) => url,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("jeikcode webui 没有打印出地址".to_string());
        }
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    if !url.contains("desktop=") {
        let separator = if url.contains('?') { "&" } else { "?" };
        url.push_str(&format!("{separator}desktop=1"));
    }
    // The process listens on 0.0.0.0. This window is on this computer, so it
    // opens 127.0.0.1. Other devices use the LAN addresses in the panel.
    url = window_url(&url);
    let probe_port = url
        .parse::<tauri::Url>()
        .ok()
        .and_then(|parsed| parsed.port())
        .unwrap_or(launch.port);
    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < ready_deadline && !health_ready(probe_port) {
        if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("desktop shutting down".to_string());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Ok((url, child, eof_rx))
}

/// 纯事件驱动的子进程监管（0 轮询、0 死循环）：
/// 线程平时完全阻塞在子进程 `stderr` 管道的内核 EOF 信号（`eof_rx.recv()`）上，0% CPU 占用；
/// 仅当子进程因「自更新升级」或「小地球修改端口/地址立即重启」以显式退出码 `75` 退出（或首次闪退自愈）时，
/// 才由桌面壳读取最新配置重新拉起并写回 `ChildSlot`，彻底消除状态失步与无限重启死循环。
fn supervise_webui(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    origin: Arc<Mutex<Option<WebuiOrigin>>>,
    mut eof_rx: std::sync::mpsc::Receiver<()>,
    mut launch: WebuiLaunch,
) {
    std::thread::spawn(move || {
        let mut crash_recovered = false;
        loop {
            // 阻塞等待当前子进程退出导致的管道 EOF 事件（0 轮询）
            let _ = eof_rx.recv();
            if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
                return;
            }
            let Some(exit_code) = reap_exited_child(&app) else {
                return;
            };
            if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
                return;
            }

            let intentional_restart = exit_code == Some(RESTART_REQUESTED_EXIT_CODE);
            if !intentional_restart {
                // 非主动请求重启的异常退出：仅允许最多一次自愈拉起，绝不陷入死循环
                if crash_recovered {
                    return;
                }
                crash_recovered = true;
            } else {
                // 主动重启成功后重置自愈计数
                crash_recovered = false;
            }

            // 动态读取最新配置与二进制路径，由桌面壳直接重新拉起并存入 ChildSlot
            let (latest_host, latest_port, latest_token) = load_webui_listen();
            let effective_port = if latest_port != 0 {
                latest_port
            } else {
                launch.port
            };
            let effective_token = latest_token.unwrap_or_else(|| launch.token.clone());
            launch.bin = resolve_desktop_cli(launch.bundled.as_deref());
            launch.host = latest_host;
            launch.port = effective_port;
            launch.token = effective_token;

            let spawned = if intentional_restart {
                // 原地重启交接：锁定原端口并支持退避重试，绝不随意误判漂移端口导致异地失联
                let mut res = spawn_desktop_server(&launch, true, Duration::from_secs(30));
                for _ in 0..3 {
                    if res.is_ok() || DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(400));
                    if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
                        break;
                    }
                    res = spawn_desktop_server(&launch, true, Duration::from_secs(30));
                }
                res
            } else {
                if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
                    return;
                }
                spawn_desktop_server(&launch, true, Duration::from_secs(30))
                    .or_else(|_| spawn_desktop_server(&launch, false, Duration::from_secs(30)))
            };

            match spawned {
                Ok((next_url, child, next_eof_rx)) => {
                    if !store_child_unless_shutting_down(&app, child) {
                        return;
                    }
                    let probe_port = next_url
                        .parse::<tauri::Url>()
                        .ok()
                        .and_then(|parsed| parsed.port())
                        .unwrap_or(launch.port);
                    if probe_port != launch.port {
                        launch.port = probe_port;
                        let _ = save_webui_listen_pref(
                            Some(&launch.host),
                            probe_port,
                            Some(&launch.token),
                        );
                    }
                    eof_rx = next_eof_rx;
                    open_webui_in_shell(&window, &origin, &next_url);
                }
                Err(err) => {
                    let text =
                        serde_json::to_string(&err).unwrap_or_else(|_| "\"重启失败\"".into());
                    let _ = window.eval(&format!(
                        "var status = document.getElementById('status'); if (status) status.textContent = {text};"
                    ));
                    return;
                }
            }
        }
    });
}

fn reap_exited_child(app: &tauri::AppHandle) -> Option<Option<i32>> {
    let slot = app.try_state::<ChildSlot>()?;
    let mut child = slot
        .0
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take()?;
    match child.wait() {
        Ok(status) => Some(status.code()),
        Err(_) => Some(None),
    }
}

#[allow(dead_code)]
fn wait_until_health(port: u16, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if DESKTOP_SHUTTING_DOWN.load(Ordering::SeqCst) {
            return false;
        }
        if health_ready(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn open_webui_in_shell(
    window: &tauri::WebviewWindow,
    origin: &Mutex<Option<WebuiOrigin>>,
    url: &str,
) {
    remember_webui_origin(origin, url);
    if let Ok(parsed) = url.parse() {
        let _ = window.navigate(parsed);
    }
}

/// Keep the printed launch address, but open this computer through loopback.
fn window_url(url: &str) -> String {
    let Ok(mut parsed) = url.parse::<tauri::Url>() else {
        return url.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return url.to_string();
    };
    if is_loopback_host(host) {
        return parsed.to_string();
    }
    if parsed.set_host(Some("127.0.0.1")).is_ok() {
        parsed.to_string()
    } else {
        url.to_string()
    }
}

fn bundled_cli(app: &tauri::AppHandle) -> Option<PathBuf> {
    let exe_name = cli_file_name();
    // 1. app.path().resource_dir() 及其 resources 子目录
    if let Ok(dir) = app.path().resource_dir() {
        for candidate in [
            dir.join(exe_name),
            dir.join("resources").join(exe_name),
            dir.join("cli-bin"),
            dir.join("resources").join("cli-bin"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // 2. 当前运行 exe (jeikcode-desktop.exe) 自身所在目录及其 resources 子目录
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            for candidate in [
                parent.join(exe_name),
                parent.join("resources").join(exe_name),
                parent.join("cli-bin"),
                parent.join("resources").join("cli-bin"),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn read_url(
    pipe: impl std::io::Read + Send + 'static,
    timeout: Duration,
) -> (
    Result<Option<String>, String>,
    std::sync::mpsc::Receiver<()>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (eof_tx, eof_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut pending = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    pending.push_str(&line);
                    if let Some(url) = extract_webui_url(&pending) {
                        let _ = tx.send(url);
                        // Dropping stderr here closes the pipe. A later write
                        // then panics twice and aborts the child, so the
                        // window sees connection refused. Drain until exit.
                        let mut extra = String::new();
                        loop {
                            extra.clear();
                            match reader.read_line(&mut extra) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                        break;
                    }
                }
            }
        }
        // 子进程已退出，stderr 管道关闭（EOF），立即事件通知 supervise_webui
        let _ = eof_tx.send(());
    });
    let res = match rx.recv_timeout(timeout) {
        Ok(url) => Ok(Some(url)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("等待 WebUI 启动超时".into()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(None),
    };
    (res, eof_rx)
}

pub fn extract_webui_url(text: &str) -> Option<String> {
    let start = text.find("http://").or_else(|| text.find("https://"))?;
    let rest = &text[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(|c: char| matches!(c, ')' | '。' | '，' | '"' | '\''));
    url.contains("token=").then(|| url.to_string())
}

fn health_ready(port: u16) -> bool {
    http_loopback_request(port, "GET", "/health", None).is_some_and(|(status, _)| status == 200)
}

#[derive(Debug, serde::Deserialize)]
struct DaemonHealthInfo {
    #[serde(default)]
    version: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    binary_hash: String,
}

fn decode_http_response(raw: &[u8]) -> Option<(u16, String)> {
    let (head_bytes, body_bytes) = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| (&raw[..pos], &raw[pos + 4..]))
        .or_else(|| {
            raw.windows(2)
                .position(|w| w == b"\n\n")
                .map(|pos| (&raw[..pos], &raw[pos + 2..]))
        })?;
    let head = String::from_utf8_lossy(head_bytes);
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    let is_chunked = head.lines().any(|line| {
        line.to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
    });
    let decoded_body = if is_chunked {
        let mut out_bytes = Vec::new();
        let mut rest = body_bytes;
        while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
            let size_line = String::from_utf8_lossy(&rest[..pos]);
            let tail = &rest[pos + 2..];
            let size_hex = size_line.split(';').next().unwrap_or("").trim();
            let Ok(chunk_len) = usize::from_str_radix(size_hex, 16) else {
                break;
            };
            if chunk_len == 0 {
                break;
            }
            if tail.len() < chunk_len {
                out_bytes.extend_from_slice(tail);
                break;
            }
            out_bytes.extend_from_slice(&tail[..chunk_len]);
            let after = &tail[chunk_len..];
            rest = after.strip_prefix(b"\r\n").unwrap_or(after);
        }
        String::from_utf8_lossy(&out_bytes).into_owned()
    } else {
        String::from_utf8_lossy(body_bytes).into_owned()
    };
    Some((status, decoded_body))
}

fn http_loopback_request(
    port: u16,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> Option<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(250)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(600)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(token) = bearer.map(str::trim).filter(|t| !t.is_empty()) {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if method == "POST" {
        req.push_str("Content-Length: 0\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    decode_http_response(&raw)
}

fn local_binary_sha256(bin: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let bytes = std::fs::read(bin).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

fn daemon_health_is_stale(
    health: &DaemonHealthInfo,
    expected_version: &str,
    expected_binary_hash: Option<&str>,
) -> bool {
    if health.service != "jeikcode-daemon" {
        return false;
    }
    if health.version != expected_version {
        return true;
    }
    expected_binary_hash.is_some_and(|local| {
        !health.binary_hash.is_empty() && !local.is_empty() && health.binary_hash != local
    })
}

fn parse_active_sessions_idle(status: u16, body: &str) -> bool {
    status == 200
        && serde_json::from_str::<Vec<String>>(body.trim()).is_ok_and(|list| list.is_empty())
}

fn pick_free_loopback_port(starting_after: u16) -> Option<u16> {
    let start = starting_after.saturating_add(1).max(1024);
    for offset in 0..100u16 {
        let candidate = start.saturating_add(offset);
        if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return Some(candidate);
        }
    }
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok().map(|a| a.port()))
}

/// Before spawning a desktop-managed daemon, inspect any daemon already listening
/// on `port`. If its `/health` version or `binary_hash` differs from our CLI
/// binary, cleanly shut it down when `/chat/active` is idle; if busy or
/// unauthorized, pick a free port instead of killing active sessions or serving
/// an outdated daemon.
fn prepare_desktop_port_for_launch(port: u16, bin: &Path, saved_token: Option<&str>) -> u16 {
    if port == 0 {
        return 0;
    }
    let Some((status, body)) = http_loopback_request(port, "GET", "/health", None) else {
        return port;
    };
    if status != 200 {
        return pick_free_loopback_port(port).unwrap_or(port);
    }
    let Ok(health) = serde_json::from_str::<DaemonHealthInfo>(body.trim()) else {
        return pick_free_loopback_port(port).unwrap_or(port);
    };
    if health.service != "jeikcode-daemon" {
        return pick_free_loopback_port(port).unwrap_or(port);
    }
    let local_hash = local_binary_sha256(bin);
    if !daemon_health_is_stale(&health, env!("CARGO_PKG_VERSION"), local_hash.as_deref()) {
        return port;
    }

    let idle = http_loopback_request(port, "GET", "/chat/active", saved_token)
        .is_some_and(|(s, b)| parse_active_sessions_idle(s, &b));
    if !idle {
        return pick_free_loopback_port(port).unwrap_or(port);
    }

    let shutdown_ok = http_loopback_request(port, "POST", "/shutdown", saved_token)
        .is_some_and(|(s, _)| (200..300).contains(&s));
    if !shutdown_ok {
        return pick_free_loopback_port(port).unwrap_or(port);
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if http_loopback_request(port, "GET", "/health", None).is_none() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    pick_free_loopback_port(port).unwrap_or(port)
}

fn fill_os_random_bytes(buf: &mut [u8]) -> bool {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "bcrypt")]
        extern "system" {
            fn BCryptGenRandom(
                hAlgorithm: *mut std::ffi::c_void,
                pbBuffer: *mut u8,
                cbBuffer: u32,
                dwFlags: u32,
            ) -> i32;
        }
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        status == 0
    }
    #[cfg(unix)]
    {
        let Ok(mut file) = std::fs::File::open("/dev/urandom") else {
            return false;
        };
        file.read_exact(buf).is_ok()
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        let _ = buf;
        false
    }
}

fn generate_csprng_token() -> String {
    let mut bytes = [0u8; 16];
    if !fill_os_random_bytes(&mut bytes) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let mixed = nanos ^ (pid << 64) ^ (&bytes as *const _ as usize as u128);
        bytes = mixed.to_le_bytes();
    }
    let mut token = String::with_capacity(3 + bytes.len() * 2);
    token.push_str("sk-");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{b:02x}");
    }
    token
}

fn mint_desktop_token(host: &str, port: u16) -> String {
    let token = generate_csprng_token();
    let _ = save_webui_listen_pref(Some(host), port, Some(&token));
    token
}

fn resolve_desktop_cli(bundled: Option<&Path>) -> PathBuf {
    // 同步执行老版本残留迁移清理（立即清理 ~/.local/bin 并注册当前安装目录到 PATH）
    cleanup_legacy_dropper_install();

    if let Some(src) = bundled {
        set_executable(src);
        return src.to_path_buf();
    }

    // 开发环境或 fallback：尝试同级可执行文件、resources 子目录或 PATH
    let name = cli_file_name();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            for candidate in [
                parent.join(name),
                parent.join("resources").join(name),
                parent.join("cli-bin"),
                parent.join("resources").join("cli-bin"),
            ] {
                if candidate.is_file() {
                    set_executable(&candidate);
                    return candidate;
                }
            }
        }
    }
    PathBuf::from(name)
}

fn cleanup_legacy_dropper_install() {
    let current_exe = std::env::current_exe().ok();
    let _ = cleanup_legacy_dropper_files(current_exe.as_deref());
    #[cfg(windows)]
    if let Some(dir) = current_exe.as_ref().and_then(|p| p.parent()) {
        let _ = migrate_hkcu_path(&[dir.to_path_buf(), dir.join("resources")]);
    }
}

fn cli_file_name() -> &'static str {
    if cfg!(windows) {
        "jeikcode.exe"
    } else {
        "jeikcode"
    }
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    let _ = path;
}

#[cfg(windows)]
fn suppress_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn suppress_console(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use jeikcode_config::desktop_shared::{
        is_valid_close_action, parse_webui_listen, save_close_action_at,
    };

    use super::{
        daemon_health_is_stale, decode_http_response, external_browser_url, extract_webui_url,
        generate_csprng_token, host_window_away, is_authorized_shell_action_url, navigation_action,
        new_window_action, parse_active_sessions_idle, remember_webui_origin, resolve_desktop_cli,
        tray_locale_from_query, tray_texts_for_locale, window_url, DaemonHealthInfo, LinkAction,
        Locale, WebuiOrigin,
    };

    fn parse_url(raw: &str) -> tauri::Url {
        raw.parse().unwrap()
    }

    #[test]
    fn resolve_desktop_cli_prefers_bundled() {
        let dummy = std::path::Path::new("dummy-bundled-path");
        let resolved = resolve_desktop_cli(Some(dummy));
        assert_eq!(resolved, dummy);
    }

    fn origin(host: &str, port: u16) -> WebuiOrigin {
        WebuiOrigin {
            host: host.to_string(),
            port,
        }
    }

    #[test]
    fn minimized_window_is_away_even_if_the_page_keeps_focus() {
        assert!(!host_window_away(false, true));
        assert!(host_window_away(true, true));
        assert!(host_window_away(true, false));
        assert!(host_window_away(false, false));
    }

    #[test]
    fn window_opens_loopback_when_the_launch_url_uses_a_lan_address() {
        assert_eq!(
            window_url("http://192.168.123.20:13457/?token=abc&desktop=1"),
            "http://127.0.0.1:13457/?token=abc&desktop=1"
        );
        assert_eq!(
            window_url("http://127.0.0.1:13457/?token=abc"),
            "http://127.0.0.1:13457/?token=abc"
        );
    }

    #[test]
    fn saved_listen_uses_the_port_and_token_and_ignores_a_blank_token() {
        assert_eq!(
            parse_webui_listen(r#"{"host":"0.0.0.0","port":4096,"token":"desk-token"}"#),
            ("0.0.0.0".to_string(), 4096, Some("desk-token".to_string()))
        );
        assert_eq!(
            parse_webui_listen(r#"{"port":0,"token":"  "}"#),
            ("127.0.0.1".to_string(), 13457, None)
        );
        assert_eq!(
            parse_webui_listen("not json"),
            ("127.0.0.1".to_string(), 13457, None)
        );
    }

    #[test]
    fn reads_token_url_from_webui_line() {
        let line = "webui 已启动：http://127.0.0.1:13457/?token=abc&sync=0\n";
        assert_eq!(
            extract_webui_url(line).as_deref(),
            Some("http://127.0.0.1:13457/?token=abc&sync=0")
        );
    }

    #[test]
    fn ignores_text_without_token() {
        assert!(extract_webui_url("listen http://127.0.0.1:13457/").is_none());
    }

    #[test]
    fn shell_pages_stay_in_the_window() {
        for raw in [
            "http://tauri.localhost/",
            "http://tauri.localhost/index.html",
            "https://tauri.localhost/index.html",
            "tauri://localhost",
            "about:blank",
        ] {
            let parsed = parse_url(raw);
            assert_eq!(
                navigation_action(&parsed, None),
                LinkAction::StayInWebview,
                "{raw}"
            );
            assert_eq!(
                new_window_action(&parsed),
                LinkAction::StayInWebview,
                "{raw}"
            );
        }
    }

    #[test]
    fn launched_webui_stays_in_the_window() {
        let launched = origin("127.0.0.1", 13457);
        assert_eq!(
            navigation_action(
                &parse_url("http://127.0.0.1:13457/?token=abc"),
                Some(&launched)
            ),
            LinkAction::StayInWebview
        );
        assert_eq!(
            navigation_action(
                &parse_url("http://localhost:13457/?token=abc&desktop=1"),
                Some(&launched)
            ),
            LinkAction::StayInWebview
        );
        let shifted = origin("127.0.0.1", 13458);
        assert_eq!(
            navigation_action(
                &parse_url("http://127.0.0.1:13458/?token=abc"),
                Some(&shifted)
            ),
            LinkAction::StayInWebview
        );
        // 地址还没记下来时不能把 WebUI 留在窗口里，调用方必须先 remember 再导航。
        assert_eq!(
            navigation_action(&parse_url("http://127.0.0.1:13457/?token=abc"), None),
            LinkAction::OpenExternal
        );
    }

    #[test]
    fn localhost_dev_servers_open_in_the_browser() {
        let launched = origin("127.0.0.1", 13457);
        for raw in [
            "http://localhost:3000/",
            "http://127.0.0.1:5173/",
            "http://[::1]:8080/",
            "http://0.0.0.0:3000/",
            "http://app.localhost:3000/",
            "https://github.com/jeikl/JeikCode",
            "https://google.com",
        ] {
            let parsed = parse_url(raw);
            assert_eq!(
                navigation_action(&parsed, Some(&launched)),
                LinkAction::OpenExternal,
                "{raw}"
            );
            assert_eq!(
                new_window_action(&parsed),
                LinkAction::OpenExternal,
                "{raw}"
            );
        }
        let webui = parse_url("http://127.0.0.1:13457/?token=abc");
        assert_eq!(new_window_action(&webui), LinkAction::OpenExternal);
    }

    #[test]
    fn wildcard_bind_urls_open_on_loopback() {
        assert_eq!(
            external_browser_url(&parse_url("http://0.0.0.0:3000/")),
            "http://127.0.0.1:3000/"
        );
        assert_eq!(
            external_browser_url(&parse_url("http://[::]:8080/docs")),
            "http://127.0.0.1:8080/docs"
        );
        assert_eq!(
            external_browser_url(&parse_url("http://localhost:5173/")),
            "http://localhost:5173/"
        );
    }

    #[test]
    fn remembers_launched_origin() {
        let slot = Mutex::new(None);
        remember_webui_origin(&slot, "http://127.0.0.1:13480/?token=abc&desktop=1");
        let stored = slot.lock().unwrap().clone().unwrap();
        assert_eq!(stored, origin("127.0.0.1", 13480));
    }

    #[test]
    fn desktop_query_param_does_not_bypass_navigation_port_check() {
        let launched = origin("127.0.0.1", 13457);
        assert_eq!(
            navigation_action(
                &parse_url("http://127.0.0.1:3000/?desktop=1"),
                Some(&launched)
            ),
            LinkAction::OpenExternal
        );
    }

    #[test]
    fn blob_urls_stay_in_webview() {
        let blob = parse_url("blob:http://127.0.0.1:13457/1234-5678");
        assert_eq!(navigation_action(&blob, None), LinkAction::StayInWebview);
    }

    #[test]
    fn shell_action_gating_and_atomic_bak_settings_write() {
        let launched = origin("127.0.0.1", 13457);
        assert!(is_authorized_shell_action_url(
            &parse_url("http://tauri.localhost/__desktop_action/hide_to_tray"),
            Some(&launched)
        ));
        assert!(is_authorized_shell_action_url(
            &parse_url("http://127.0.0.1:13457/__desktop_action/quit_app"),
            Some(&launched)
        ));
        assert!(!is_authorized_shell_action_url(
            &parse_url("https://evil.example/__desktop_action/quit_app"),
            Some(&launched)
        ));
        assert!(is_valid_close_action("tray"));
        assert!(!is_valid_close_action("tray\",\"injected\":\"1"));

        let tmp_dir = std::env::temp_dir().join(format!(
            "jeikcode-desktop-settings-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let settings = tmp_dir.join("desktop-settings.json");

        save_close_action_at(&settings, "tray").unwrap();
        assert!(settings.read_to_string_lossy_test().contains("\"tray\""));

        save_close_action_at(&settings, "quit").unwrap();
        let bak = tmp_dir.join("desktop-settings.json.bak");
        assert!(bak.is_file());
        assert!(std::fs::read_to_string(&bak).unwrap().contains("\"tray\""));
        assert!(std::fs::read_to_string(&settings)
            .unwrap()
            .contains("\"quit\""));
        assert!(save_close_action_at(&settings, "bogus").is_err());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn tray_texts_cover_all_eleven_locales() {
        for &loc in Locale::ALL {
            let texts = tray_texts_for_locale(loc);
            assert!(!texts.tooltip.is_empty());
            assert!(!texts.show_window.is_empty());
            assert!(!texts.quit.is_empty());
        }
    }

    #[test]
    fn tray_locale_query_parses_supported_locales_and_rejects_empty() {
        assert_eq!(tray_locale_from_query("zh"), Some(Locale::ZhCn));
        assert_eq!(tray_locale_from_query("zh-TW"), Some(Locale::ZhTw));
        assert_eq!(tray_locale_from_query("vi"), Some(Locale::Vi));
        assert_eq!(tray_locale_from_query("ja"), Some(Locale::Ja));
        assert_eq!(tray_locale_from_query(""), None);
        assert_eq!(tray_locale_from_query("   "), None);
    }

    trait ReadToStringTest {
        fn read_to_string_lossy_test(&self) -> String;
    }
    impl ReadToStringTest for std::path::PathBuf {
        fn read_to_string_lossy_test(&self) -> String {
            std::fs::read_to_string(self).unwrap()
        }
    }

    #[test]
    fn csprng_desktop_tokens_are_distinct_and_random_length() {
        let a = generate_csprng_token();
        let b = generate_csprng_token();
        assert!(a.starts_with("sk-"));
        assert_eq!(a.len(), 35);
        assert_ne!(a, b);
    }

    #[test]
    fn stale_daemon_detection_checks_version_binary_hash_and_active_idle() {
        let current = DaemonHealthInfo {
            version: "7.5.1".to_string(),
            service: "jeikcode-daemon".to_string(),
            binary_hash: "abc123".to_string(),
        };
        assert!(!daemon_health_is_stale(&current, "7.5.1", Some("abc123")));
        assert!(daemon_health_is_stale(&current, "7.5.2", Some("abc123")));
        assert!(daemon_health_is_stale(&current, "7.5.1", Some("def456")));

        let other = DaemonHealthInfo {
            version: "1.0.0".to_string(),
            service: "other-service".to_string(),
            binary_hash: "xyz".to_string(),
        };
        assert!(!daemon_health_is_stale(&other, "7.5.1", Some("abc123")));

        assert!(parse_active_sessions_idle(200, "[]"));
        assert!(!parse_active_sessions_idle(200, "[\"sess-1\"]"));
        assert!(!parse_active_sessions_idle(401, "[]"));

        let decoded = decode_http_response(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"service\":\"jeikcode-daemon\"}",
        )
        .unwrap();
        assert_eq!(decoded.0, 200);
        assert_eq!(decoded.1, "{\"service\":\"jeikcode-daemon\"}");
    }
}
