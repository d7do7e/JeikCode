//! Shared desktop/daemon/CLI contracts, listen preferences, and settings I/O.
//!
//! Consolidates constants, loopback detection, `webui-listen.json`, and
//! `desktop-settings.json` persistence in the leaf `jeikcode-config` crate so
//! `desktop/src-tauri`, `jeikcode-daemon`, `jeikcode-cli`, and
//! `jeikcode-updater` stay locked onto a single source of truth.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::store::{atomic_replace_with_mode, backup_sibling_bak, ensure_parent};
use crate::util::real_home_dir;

pub const DEFAULT_LISTEN_HOST: &str = "127.0.0.1";
pub const WEBUI_DEFAULT_PORT: u16 = 13457;

/// Dedicated exit code used by a desktop-managed child to signal an intentional
/// restart (e.g. after self-upgrade or remote-access port/host/token change).
pub const RESTART_REQUESTED_EXIT_CODE: i32 = 75;

pub const RESTART_LISTEN_PORT_ENV: &str = "JEIKCODE_RESTART_LISTEN_PORT";
pub const RESTART_LISTEN_HOST_ENV: &str = "JEIKCODE_RESTART_LISTEN_HOST";
pub const RESTART_EXACT_PORT_ENV: &str = "JEIKCODE_RESTART_EXACT_PORT";
pub const RESTART_TOKEN_ENV: &str = "JEIKCODE_RESTART_TOKEN";
pub const RESTART_NO_TOKEN_ENV: &str = "JEIKCODE_RESTART_NO_TOKEN";
pub const RESTARTED_ENV: &str = "JEIKCODE_RESTARTED";

pub const DESKTOP_ENV: &str = "JEIKCODE_DESKTOP";
pub const DESKTOP_MANAGED_ENV: &str = "JEIKCODE_DESKTOP_MANAGED";
pub const DESKTOP_PID_ENV: &str = "JEIKCODE_DESKTOP_PID";
pub const DESKTOP_CLI_BIN_ENV: &str = "JEIKCODE_DESKTOP_CLI_BIN";
pub const SERVER_TOKEN_ENV: &str = "JEIKCODE_SERVER_TOKEN";

/// Returns `true` for `"localhost"` or any IPv4/IPv6 loopback address
/// (including `127.0.0.0/8` and `[::1]` bracketed literals).
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let literal = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    literal
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn normalize_opt_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebuiListenPref {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

pub fn webui_listen_path() -> PathBuf {
    Config::config_dir().join("webui-listen.json")
}

pub fn parse_webui_listen(text: &str) -> (String, u16, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return (DEFAULT_LISTEN_HOST.to_string(), WEBUI_DEFAULT_PORT, None);
    };
    let host = normalize_opt_str(value.get("host").and_then(|item| item.as_str()))
        .unwrap_or_else(|| DEFAULT_LISTEN_HOST.to_string());
    let port = value
        .get("port")
        .and_then(|item| item.as_u64())
        .and_then(|item| u16::try_from(item).ok())
        .filter(|item| *item != 0)
        .unwrap_or(WEBUI_DEFAULT_PORT);
    let token = normalize_opt_str(value.get("token").and_then(|item| item.as_str()));
    (host, port, token)
}

pub fn load_webui_listen_at(path: &Path) -> (String, u16, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (DEFAULT_LISTEN_HOST.to_string(), WEBUI_DEFAULT_PORT, None);
    };
    parse_webui_listen(&text)
}

pub fn load_webui_listen() -> (String, u16, Option<String>) {
    load_webui_listen_at(&webui_listen_path())
}

pub fn load_webui_listen_pref_at(path: &Path) -> Option<WebuiListenPref> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut pref: WebuiListenPref = serde_json::from_str(&text).ok()?;
    pref.host = normalize_opt_str(pref.host.as_deref());
    pref.token = normalize_opt_str(pref.token.as_deref());
    Some(pref)
}

/// Write `webui-listen.json` atomically with `0o600` permissions on Unix.
/// Never writes a `.bak` copy so bearer tokens are not duplicated on disk.
pub fn save_webui_listen_pref_at(
    path: &Path,
    host: Option<&str>,
    port: u16,
    token: Option<&str>,
) -> Result<(), String> {
    if port == 0 {
        return Err("port must be 1-65535".to_string());
    }
    ensure_parent(path).map_err(|err| err.to_string())?;
    let mut pref = load_webui_listen_pref_at(path).unwrap_or(WebuiListenPref {
        host: None,
        port,
        token: None,
    });
    pref.port = port;
    if let Some(h) = normalize_opt_str(host) {
        pref.host = Some(h);
    }
    if let Some(raw_token) = token {
        pref.token = normalize_opt_str(Some(raw_token));
    }
    let body = serde_json::to_string_pretty(&pref).map_err(|err| err.to_string())? + "\n";
    atomic_replace_with_mode(path, body.as_bytes(), true).map_err(|err| err.to_string())
}

pub fn save_webui_listen_pref(
    host: Option<&str>,
    port: u16,
    token: Option<&str>,
) -> Result<(), String> {
    save_webui_listen_pref_at(&webui_listen_path(), host, port, token)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopSettings {
    #[serde(default)]
    pub close_action: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopSettingsError {
    InvalidAction,
    Io(String),
}

impl std::fmt::Display for DesktopSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAction => f.write_str("close_action must be 'ask', 'tray', or 'quit'"),
            Self::Io(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for DesktopSettingsError {}

pub fn is_valid_close_action(action: &str) -> bool {
    matches!(action, "ask" | "tray" | "quit")
}

pub fn desktop_settings_path() -> PathBuf {
    Config::config_dir().join("desktop-settings.json")
}

pub fn load_close_action_at(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "ask".to_string();
    };
    serde_json::from_str::<DesktopSettings>(&text)
        .ok()
        .map(|s| s.close_action.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ask".to_string())
}

pub fn load_close_action() -> String {
    load_close_action_at(&desktop_settings_path())
}

pub fn save_close_action_at(path: &Path, action: &str) -> Result<(), DesktopSettingsError> {
    let trimmed = action.trim();
    if !is_valid_close_action(trimmed) {
        return Err(DesktopSettingsError::InvalidAction);
    }
    ensure_parent(path).map_err(|e| DesktopSettingsError::Io(e.to_string()))?;
    backup_sibling_bak(path).map_err(|e| DesktopSettingsError::Io(e.to_string()))?;
    let payload = DesktopSettings {
        close_action: trimmed.to_string(),
    };
    let body = serde_json::to_string_pretty(&payload)
        .map_err(|e| DesktopSettingsError::Io(e.to_string()))?
        + "\n";
    atomic_replace_with_mode(path, body.as_bytes(), false)
        .map_err(|e| DesktopSettingsError::Io(e.to_string()))
}

pub fn save_close_action(action: &str) -> Result<(), DesktopSettingsError> {
    save_close_action_at(&desktop_settings_path(), action)
}

fn same_exe_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

pub fn is_legacy_dropper_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("jeikcode") || lower.starts_with("atomcode")
}

pub fn is_dirty_legacy_path_entry(entry: &str) -> bool {
    let lower = entry.trim().to_ascii_lowercase();
    lower.contains(".local\\bin") || lower.contains(".local/bin") || lower.contains("atomcode")
}

pub fn legacy_local_bin_dir() -> Option<PathBuf> {
    real_home_dir().map(|h| h.join(".local").join("bin"))
}

pub fn scan_legacy_dropper_files_in(local_bin: &Path, current_exe: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(local_bin) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_current = current_exe.is_some_and(|c| same_exe_path(&path, c));
        if is_legacy_dropper_filename(name) && !is_current {
            found.push(path);
        }
    }
    found
}

pub fn scan_legacy_dropper_files(current_exe: Option<&Path>) -> Vec<PathBuf> {
    let Some(local_bin) = legacy_local_bin_dir() else {
        return Vec::new();
    };
    scan_legacy_dropper_files_in(&local_bin, current_exe)
}

pub fn cleanup_legacy_dropper_files_in(local_bin: &Path, skip_exe: Option<&Path>) -> usize {
    let mut removed = 0;
    for path in scan_legacy_dropper_files_in(local_bin, skip_exe) {
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    let _ = std::fs::remove_dir(local_bin);
    removed
}

pub fn cleanup_legacy_dropper_files(skip_exe: Option<&Path>) -> usize {
    let Some(local_bin) = legacy_local_bin_dir() else {
        return 0;
    };
    cleanup_legacy_dropper_files_in(&local_bin, skip_exe)
}

#[cfg(windows)]
pub fn has_dirty_hkcu_path() -> bool {
    use winreg::enums::*;
    use winreg::RegKey;
    let Ok(env) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment") else {
        return false;
    };
    let path_val: String = env.get_value("Path").unwrap_or_default();
    path_val.split(';').any(is_dirty_legacy_path_entry)
}

#[cfg(not(windows))]
pub fn has_dirty_hkcu_path() -> bool {
    false
}

#[cfg(windows)]
pub fn migrate_hkcu_path(extra_dirs: &[PathBuf]) -> bool {
    use winreg::enums::*;
    use winreg::RegKey;
    let Ok(env) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
    else {
        return false;
    };
    let current: String = env.get_value("Path").unwrap_or_default();
    let mut parts: Vec<String> = current
        .split(';')
        .filter(|p| {
            let trimmed = p.trim();
            !trimmed.is_empty() && !is_dirty_legacy_path_entry(trimmed)
        })
        .map(str::to_string)
        .collect();

    for dir in extra_dirs.iter().rev() {
        if !dir.is_dir() {
            continue;
        }
        let dir_str = dir.display().to_string();
        let lower = dir_str.to_ascii_lowercase();
        if lower.contains("temp")
            || lower.contains("staged")
            || lower.contains("target\\")
            || lower.contains("target/")
        {
            continue;
        }
        if !parts
            .iter()
            .any(|p| p.trim().eq_ignore_ascii_case(&dir_str))
        {
            parts.insert(0, dir_str);
        }
    }

    let new_path = parts.join(";");
    if new_path != current && env.set_value("Path", &new_path).is_ok() {
        broadcast_env_setting_change();
        return true;
    }
    false
}

#[cfg(not(windows))]
pub fn migrate_hkcu_path(_extra_dirs: &[PathBuf]) -> bool {
    false
}

#[cfg(windows)]
fn broadcast_env_setting_change() {
    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            hwnd: *mut std::ffi::c_void,
            msg: u32,
            wparam: usize,
            lparam: *const u16,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }
    use std::os::windows::ffi::OsStrExt;
    let env_wide: Vec<u16> = std::ffi::OsStr::new("Environment")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let hwnd_broadcast = 0xffff as *mut std::ffi::c_void;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    let mut res = 0;
    unsafe {
        SendMessageTimeoutW(
            hwnd_broadcast,
            WM_SETTINGCHANGE,
            0,
            env_wide.as_ptr(),
            SMTO_ABORTIFHUNG,
            5000,
            &mut res,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_host_handles_aliases_and_brackets() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host(" LOCALHOST "));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn webui_listen_normalizes_blank_fields_and_skips_bak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webui-listen.json");

        assert_eq!(
            parse_webui_listen(r#"{"host":"0.0.0.0","port":4096,"token":"desk-token"}"#),
            ("0.0.0.0".to_string(), 4096, Some("desk-token".to_string()))
        );
        assert_eq!(
            parse_webui_listen(r#"{"host":"  ","port":0,"token":"  "}"#),
            ("127.0.0.1".to_string(), WEBUI_DEFAULT_PORT, None)
        );

        save_webui_listen_pref_at(&path, Some(" 127.0.0.1 "), 13457, Some(" tok-1 ")).unwrap();
        save_webui_listen_pref_at(&path, Some("   "), 13458, Some("   ")).unwrap();

        let pref = load_webui_listen_pref_at(&path).unwrap();
        assert_eq!(pref.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(pref.port, 13458);
        assert_eq!(pref.token, None);
        assert!(!dir.path().join("webui-listen.json.bak").exists());
        assert!(save_webui_listen_pref_at(&path, None, 0, None).is_err());
    }

    #[test]
    fn desktop_settings_validates_action_creates_bak_and_atomic_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-settings.json");

        assert_eq!(
            save_close_action_at(&path, "bogus").unwrap_err(),
            DesktopSettingsError::InvalidAction
        );
        save_close_action_at(&path, "tray").unwrap();
        assert_eq!(load_close_action_at(&path), "tray");

        save_close_action_at(&path, "quit").unwrap();
        assert_eq!(load_close_action_at(&path), "quit");
        let bak = dir.path().join("desktop-settings.json.bak");
        assert!(bak.is_file());
        assert_eq!(load_close_action_at(&bak), "tray");
    }

    #[test]
    fn legacy_dropper_helpers_fold_case_preserve_skip_exe_and_detect_dirty_paths() {
        assert!(is_legacy_dropper_filename("JeikCode.exe"));
        assert!(is_legacy_dropper_filename("atomcode"));
        assert!(!is_legacy_dropper_filename("other-tool.exe"));

        assert!(is_dirty_legacy_path_entry("C:\\Users\\a\\.local\\bin"));
        assert!(is_dirty_legacy_path_entry("/home/a/.local/bin"));
        assert!(is_dirty_legacy_path_entry("C:\\Apps\\AtomCode"));
        assert!(!is_dirty_legacy_path_entry("C:\\Program Files\\JeikCode"));

        let dir = tempfile::tempdir().unwrap();
        let local_bin = dir.path().join(".local").join("bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let keep = local_bin.join("JeikCode.exe");
        let stale = local_bin.join("AtomCode.exe");
        let unrelated = local_bin.join("other.exe");
        std::fs::write(&keep, b"live").unwrap();
        std::fs::write(&stale, b"old").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let scanned = scan_legacy_dropper_files_in(&local_bin, Some(&keep));
        assert_eq!(scanned, vec![stale.clone()]);

        let removed = cleanup_legacy_dropper_files_in(&local_bin, Some(&keep));
        assert_eq!(removed, 1);
        assert!(keep.is_file());
        assert!(!stale.exists());
        assert!(unrelated.is_file());
    }
}
