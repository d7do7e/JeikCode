use axum::extract::State;
use axum::{response::IntoResponse, Json};
use jeikcode_config::config::provider::ProviderConfig;
use jeikcode_config::config::Config;
use jeikcode_config::ConfigStore;

use crate::{json_error, AppState, ConfigResponse, ProviderInfo};

/// Load config from disk.
pub(crate) fn load_config() -> Result<Config, String> {
    let path = Config::default_path();
    match Config::load(&path) {
        Ok(config) => Ok(config),
        Err(e) => {
            let is_missing = e.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            });
            if is_missing {
                Ok(empty_config())
            } else {
                Err(format!("Failed to load config: {:#}", e))
            }
        }
    }
}

fn empty_config() -> Config {
    Config::default()
}

/// Build a sanitized ConfigResponse from a loaded Config.
///
/// Lists the unified model catalog (`logical_models`) so new-schema and folded
/// CodingPlan models — which no longer live in `config.providers` — are still
/// selectable in the webui. Each selection id is reconstructed into a
/// `ProviderConfig` view via the resolution boundary.
pub(crate) fn config_response(config: &Config) -> ConfigResponse {
    let default_selection = config.effective_model_selection().unwrap_or_default();
    let logical_models = config.logical_models();
    let mut ids: Vec<String> = logical_models.keys().cloned().collect();
    ids.sort();
    let providers = ids
        .iter()
        .filter_map(|id| {
            config.provider_config_for_selection(id).map(|p| {
                let mut info = provider_info(id, &p, &default_selection);
                if let Some(m) = logical_models.get(id) {
                    info.account = Some(m.account.clone());
                }
                info
            })
        })
        .collect();

    let logical_accounts = config.logical_accounts();
    let mut account_ids: Vec<String> = logical_accounts.keys().cloned().collect();
    account_ids.sort();
    let accounts = account_ids
        .into_iter()
        .map(|id| {
            let a = &logical_accounts[&id];
            crate::AccountInfo {
                id: id.clone(),
                provider_type: a.provider.clone(),
                base_url: a.base_url.clone(),
                has_api_key: a.api_key.as_ref().is_some_and(|k| !k.is_empty()),
                skip_tls_verify: a.skip_tls_verify,
            }
        })
        .collect();

    let language = config
        .language
        .unwrap_or_default()
        .language_key()
        .to_string();
    let update_dead = jeikcode_config::config::runtime_state::is_dead();
    ConfigResponse {
        path: Config::default_path(),
        default_provider: default_selection,
        default_workdir: config.default_workdir.clone(),
        providers,
        accounts,
        language,
        github_logo: config.github_logo && !update_dead,
        update_dead,
        auto_git_sync: config.auto_git_sync,
    }
}

/// Build a sanitized ProviderInfo from a name + ProviderConfig.
pub(crate) fn provider_info(
    name: &str,
    p: &ProviderConfig,
    default_provider: &str,
) -> ProviderInfo {
    ProviderInfo {
        name: name.to_string(),
        provider_type: p.provider_type.clone(),
        model: p.model.clone(),
        base_url: p.base_url.clone(),
        has_api_key: p.resolved_api_key().is_some(),
        requires_login: p
            .base_url
            .as_deref()
            .is_some_and(jeikcode_auth::gateway_crypto::is_jeikcode_gateway),
        is_default: name == default_provider,
        context_window: p.context_window,
        max_tokens: p.max_tokens,
        thinking_enabled: p.thinking_enabled,
        thinking_budget: p.thinking_budget,
        thinking_type: p.thinking_type.clone(),
        thinking_keep: p.thinking_keep.clone(),
        reasoning_history: p.reasoning_history.clone(),
        reasoning_effort: p.reasoning_effort.clone(),
        skip_tls_verify: p.skip_tls_verify,
        ephemeral: p.ephemeral,
        pricing: p.pricing,
        supports_vision: p.supports_vision,
        reasoning_model: p.reasoning_model,
        account: None,
    }
}

/// Validate a provider name. Returns the trimmed name on success.
pub(crate) fn validate_provider_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Provider name cannot be empty".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Provider name cannot be '.' or '..'".into());
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.contains('\t')
    {
        return Err(
            "Provider name cannot contain /, \\, NUL, newline, carriage return, or tab".into(),
        );
    }
    Ok(trimmed.to_string())
}

/// Apply one config delta to the latest on-disk snapshot. Provider/config API
/// handlers should use this instead of load-mutate-save to avoid lost updates
/// when an IDE and TUI write concurrently.
///
/// 落盘成功后广播 `config_reloaded`：语言/主题/模型/提供商/思考参数等**任何**
/// 界面设置都是同一份 config，别的实例必须立即重读，而不是等手动 reload 或冷启动。
pub(crate) fn update_config(
    mutate: impl FnOnce(&mut Config) -> anyhow::Result<()>,
) -> Result<Config, String> {
    let result = ConfigStore::default_store()
        .update(mutate)
        .map(|commit| commit.snapshot.config)
        .map_err(|e| format!("Failed to update config: {e:#}"));
    if result.is_ok() {
        crate::publish_global(crate::GlobalWireEvent::ConfigReloaded);
    }
    result
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /config - Returns sanitized config state.
pub(crate) async fn get_config() -> impl IntoResponse {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            return json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    };
    Json(config_response(&config)).into_response()
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct LanguageBody {
    language: String,
}

fn parse_language(input: &str) -> Result<jeikcode_config::locale::Locale, String> {
    input.parse().map_err(|_| {
        "Supported languages: en, vi-VN, zh-CN / Ngôn ngữ hỗ trợ: en, vi-VN, zh-CN / 支持的语言：en, vi-VN, zh-CN".to_string()
    })
}

fn persist_language(
    locale: jeikcode_config::locale::Locale,
    store: &ConfigStore,
) -> Result<Config, String> {
    let config = store
        .update(|cfg| {
            cfg.language = Some(locale);
            Ok(())
        })
        .map_err(|err| format!("Failed to update config: {err:#}"))?
        .snapshot
        .config;
    jeikcode_config::i18n::set_locale(locale);
    Ok(config)
}

/// POST /config/language — persist the global UI language and apply it in this process.
pub(crate) async fn set_language(Json(body): Json<LanguageBody>) -> impl IntoResponse {
    let locale = match parse_language(&body.language) {
        Ok(locale) => locale,
        Err(err) => return json_error(axum::http::StatusCode::BAD_REQUEST, err).into_response(),
    };
    let config = match persist_language(locale, &ConfigStore::default_store()) {
        Ok(config) => config,
        Err(err) => {
            return json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
        }
    };
    // 语言（小地球）也是全局设置：别的实例/标签页必须立即跟随，冷启动再读同一个 config。
    crate::publish_global(crate::GlobalWireEvent::ConfigReloaded);
    Json(config_response(&config)).into_response()
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct AutoGitSyncBody {
    enabled: bool,
}

/// POST /config/auto-git-sync — 持久化「Agent 执行 git 变更命令后自动刷新/同步」开关。
pub(crate) async fn set_auto_git_sync(Json(body): Json<AutoGitSyncBody>) -> impl IntoResponse {
    let config = match update_config(|cfg| {
        cfg.auto_git_sync = body.enabled;
        Ok(())
    }) {
        Ok(config) => config,
        Err(err) => {
            return json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
        }
    };
    // update_config 已统一广播 config_reloaded（语言/主题/模型/提供商同一份 config）。
    Json(config_response(&config)).into_response()
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct RemoteAccessBody {
    host: String,
    port: u16,
    #[serde(default)]
    token: String,
    #[serde(default)]
    no_token: bool,
    #[serde(default)]
    stop: bool,
    /// Save the port and token for the next desktop launch (`0.0.0.0`).
    /// The same port updates the live token. A different port waits until
    /// the next start. `no_token` is never written to disk.
    #[serde(default)]
    apply_launch: bool,
    /// Immediately restart the daemon to apply changed host or port.
    #[serde(default)]
    restart: bool,
}

#[derive(Debug, serde::Serialize)]
struct RemoteAccessStatus {
    host: String,
    port: u16,
    no_token: bool,
    active: bool,
    token: Option<String>,
    url: Option<String>,
    /// Every address another device can open. `url` is the first of these.
    /// Empty when the bind is up but this machine reported no shareable IPv4.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    urls: Vec<String>,
    /// `allowed` when this exe may receive inbound connections, `prompt` when
    /// Windows still needs an Allow click. Absent on loopback and non-Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    firewall: Option<String>,
    /// Port saved for the next launch when it differs from the socket that
    /// is listening now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_port: Option<u16>,
    /// Host saved for the next launch when it differs from the socket that
    /// is listening now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_host: Option<String>,
    /// How this process was started: `desktop` / `systemd` / `procd` /
    /// `launchd` / `windows-logon` / `foreground`. Restarts follow this mode.
    launch_mode: &'static str,
}

#[cfg_attr(not(test), allow(dead_code))]
fn remote_status(state: &crate::AppState, token: Option<String>) -> RemoteAccessStatus {
    remote_status_with_pref(state, token, &webui_listen_pref_path())
}

fn remote_status_with_pref(
    state: &crate::AppState,
    token: Option<String>,
    pref_path: &std::path::Path,
) -> RemoteAccessStatus {
    // Copy the bind out and drop the mutex before any address lookup.
    // `extra_remote` is a std mutex: calling back into this function while
    // the guard is still held deadlocks the worker, and the WebUI Apply
    // button then stays disabled forever.
    let (host, port, active, active_token) = {
        let extra = state.extra_remote.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bind) = extra.as_ref() {
            (bind.host.clone(), bind.port, true, bind.token.clone())
        } else {
            let host = state.bind_host.clone();
            // The desktop starts on 0.0.0.0. That socket is the remote
            // listener; there is no second port to open.
            let sharing = !is_loopback_host(&host);
            (host, state.bind_port, sharing, state.webui_tokens.display())
        }
    };
    let token = token.or(active_token);
    let no_token = state
        .token_optional
        .load(std::sync::atomic::Ordering::Relaxed)
        || !state
            .enforce_token
            .load(std::sync::atomic::Ordering::Relaxed);
    let urls = if active {
        access_urls(&host, port, token.as_deref(), no_token)
    } else {
        Vec::new()
    };
    let url = urls.first().cloned();
    let next_port = load_webui_listen_pref_at(pref_path)
        .map(|pref| pref.port)
        .filter(|saved| *saved != 0 && *saved != port);
    RemoteAccessStatus {
        host,
        port,
        no_token,
        active,
        token,
        url,
        urls,
        firewall: None,
        next_port,
        next_host: None,
        launch_mode: jeikcode_updater::detect_launch_mode().as_str(),
    }
}

fn format_access_url(host: &str, port: u16, token: Option<&str>, no_token: bool) -> String {
    let display = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if !no_token {
        if let Some(tok) = token.filter(|item| !item.is_empty()) {
            return format!("http://{display}:{port}/?token={tok}");
        }
    }
    format!("http://{display}:{port}/")
}

/// Addresses that belong on a link sent to another device.
///
/// A `0.0.0.0` / `::` listener is reachable on every interface, but the URL
/// must not be `127.0.0.1`: that address only opens on this computer.
fn access_urls(host: &str, port: u16, token: Option<&str>, no_token: bool) -> Vec<String> {
    let hosts = if is_unspecified_host(host) {
        shareable_network_addrs()
    } else if is_loopback_host(host) {
        vec!["127.0.0.1".to_string()]
    } else {
        vec![host.to_string()]
    };
    hosts
        .into_iter()
        .map(|item| format_access_url(&item, port, token, no_token))
        .collect()
}

fn is_unspecified_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]")
}

fn is_loopback_host(host: &str) -> bool {
    jeikcode_config::desktop_shared::is_loopback_host(host)
}

pub(crate) fn shareable_network_addrs() -> Vec<String> {
    let blob = interface_network_blob();
    let mut found = Vec::new();

    for ip4 in collect_ipv4_literals(&blob) {
        if !found.contains(&ip4) {
            found.push(ip4);
        }
    }
    if let Some(primary) = crate::primary_lan_ipv4() {
        if let Ok(ip) = primary.parse::<std::net::Ipv4Addr>() {
            if ipv4_is_shareable(&ip) && !found.contains(&primary) {
                found.push(primary);
            }
        }
    }

    for ip6 in collect_ipv6_literals(&blob) {
        if !found.contains(&ip6) {
            found.push(ip6);
        }
    }
    if let Some(primary6) = crate::primary_lan_ipv6() {
        if let Ok(ip) = primary6.parse::<std::net::Ipv6Addr>() {
            if ipv6_is_shareable(&ip) && !found.contains(&primary6) {
                found.push(primary6);
            }
        }
    }

    found.sort_by(|left, right| {
        network_ip_rank(left)
            .cmp(&network_ip_rank(right))
            .then(left.cmp(right))
    });
    if let Some(primary6) = crate::primary_lan_ipv6() {
        if let Some(pos) = found.iter().position(|item| item == &primary6) {
            let ip = found.remove(pos);
            let insert_pos = found
                .iter()
                .position(|item| network_ip_rank(item) >= 1)
                .unwrap_or(found.len());
            found.insert(insert_pos, ip);
        }
    }
    if let Some(primary) = crate::primary_lan_ipv4() {
        if let Some(pos) = found.iter().position(|item| item == &primary) {
            let ip = found.remove(pos);
            found.insert(0, ip);
        }
    }
    found
}

pub(crate) fn ipv4_is_shareable(ip: &std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    if octets[0] == 0 || octets[0] == 127 || octets[0] >= 224 {
        return false;
    }
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_broadcast() && !ip.is_multicast()
}

pub(crate) fn ipv6_is_shareable(ip: &std::net::Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let segments = ip.segments();
    // Exclude link-local fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // Exclude IPv4-mapped ::ffff:0:0/96
    if segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0xffff
    {
        return false;
    }
    // Exclude documentation and benchmarking prefixes
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }
    if segments[0] == 0x0100 && segments[1] == 0 {
        return false;
    }
    // Allow Global Unicast (2000::/3, e.g. 2409:: / 240e:: / 2600::) and Unique Local Addresses (fc00::/7, e.g. fd00::)
    let first = segments[0];
    let is_gua = (first & 0xe000) == 0x2000;
    let is_ula = (first & 0xfe00) == 0xfc00;
    is_gua || is_ula
}

fn network_ip_rank(text: &str) -> u8 {
    if let Ok(ip4) = text.parse::<std::net::Ipv4Addr>() {
        if !ipv4_is_shareable(&ip4) {
            9
        } else if ip4.is_private() {
            0
        } else if ip4.is_link_local() {
            3
        } else {
            2
        }
    } else if let Ok(ip6) = text.parse::<std::net::Ipv6Addr>() {
        if !ipv6_is_shareable(&ip6) {
            9
        } else {
            1
        }
    } else {
        9
    }
}

pub(crate) fn collect_ipv4_literals(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        let mut dots = 0;
        let mut octet_len = 0;
        let mut ok = true;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte.is_ascii_digit() {
                octet_len += 1;
                if octet_len > 3 {
                    ok = false;
                }
                index += 1;
            } else if byte == b'.' && dots < 3 && octet_len > 0 {
                dots += 1;
                octet_len = 0;
                index += 1;
            } else {
                break;
            }
        }
        if ok && dots == 3 && octet_len > 0 {
            let bounded = (start == 0 || !bytes[start - 1].is_ascii_digit())
                && (index >= bytes.len() || !bytes[index].is_ascii_digit());
            if bounded {
                if let Ok(text) = std::str::from_utf8(&bytes[start..index]) {
                    if let Ok(ip) = text.parse::<std::net::Ipv4Addr>() {
                        if ipv4_is_shareable(&ip) {
                            let rendered = ip.to_string();
                            if !out.contains(&rendered) {
                                out.push(rendered);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn collect_ipv6_literals(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(bytes);
    for token in text.split_whitespace() {
        let trimmed = token
            .trim_matches(|c: char| !c.is_ascii_hexdigit() && c != ':' && c != '%' && c != '/')
            .trim();
        let candidate = if let Some((head, _)) = trimmed.split_once('%') {
            head
        } else {
            trimmed
        };
        let candidate = if let Some((head, _)) = candidate.split_once('/') {
            head
        } else {
            candidate
        };
        if candidate.contains(':') {
            if let Ok(ip) = candidate.parse::<std::net::Ipv6Addr>() {
                if ipv6_is_shareable(&ip) {
                    let rendered = ip.to_string();
                    if !out.contains(&rendered) {
                        out.push(rendered);
                    }
                }
            }
        }
    }
    out
}

fn interface_network_blob() -> Vec<u8> {
    #[cfg(windows)]
    {
        windows_ipconfig_bytes()
    }
    #[cfg(target_os = "macos")]
    {
        mac_ifconfig_bytes()
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        linux_ip_addr_bytes()
    }
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandDrainError {
    SpawnFailed,
    TimedOut,
}

const MAX_CAPTURED_PIPE_BYTES: usize = 4 * 1024 * 1024;

fn spawn_capped_pipe_reader<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < MAX_CAPTURED_PIPE_BYTES {
                        let remaining = MAX_CAPTURED_PIPE_BYTES - buf.len();
                        buf.extend_from_slice(&chunk[..n.min(remaining)]);
                    }
                    // Continue draining until EOF so the child never blocks on a full pipe.
                }
            }
        }
        buf
    })
}

fn run_command_with_timeout_drain(
    mut cmd: std::process::Command,
    limit: std::time::Duration,
) -> Result<CommandOutput, CommandDrainError> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|_| CommandDrainError::SpawnFailed)?;
    let stdout_handle = child.stdout.take().map(spawn_capped_pipe_reader);
    let stderr_handle = child.stderr.take().map(spawn_capped_pipe_reader);
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = stdout_handle {
                    let _ = h.join();
                }
                if let Some(h) = stderr_handle {
                    let _ = h.join();
                }
                return Err(CommandDrainError::TimedOut);
            }
        }
    };
    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    Ok(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn mac_ifconfig_bytes() -> Vec<u8> {
    let cmd = std::process::Command::new("ifconfig");
    run_command_with_timeout_drain(cmd, std::time::Duration::from_millis(1500))
        .map(|out| out.stdout)
        .unwrap_or_default()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn linux_ip_addr_bytes() -> Vec<u8> {
    let mut ip_cmd = std::process::Command::new("ip");
    ip_cmd.args(["-o", "addr"]);
    match run_command_with_timeout_drain(ip_cmd, std::time::Duration::from_millis(1500)) {
        Ok(out) => out.stdout,
        Err(CommandDrainError::SpawnFailed) => {
            let ifcfg = std::process::Command::new("ifconfig");
            run_command_with_timeout_drain(ifcfg, std::time::Duration::from_millis(1500))
                .map(|out| out.stdout)
                .unwrap_or_default()
        }
        Err(CommandDrainError::TimedOut) => Vec::new(),
    }
}

#[cfg(windows)]
fn windows_ipconfig_bytes() -> Vec<u8> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new("ipconfig");
    cmd.creation_flags(CREATE_NO_WINDOW);
    run_command_with_timeout_drain(cmd, std::time::Duration::from_millis(1500))
        .map(|out| out.stdout)
        .unwrap_or_default()
}

fn apply_remote_auth(state: &crate::AppState, no_token: bool) {
    if no_token {
        state
            .token_optional
            .store(true, std::sync::atomic::Ordering::Relaxed);
        state
            .enforce_token
            .store(false, std::sync::atomic::Ordering::Relaxed);
    } else {
        state
            .token_optional
            .store(false, std::sync::atomic::Ordering::Relaxed);
        state
            .enforce_token
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn restore_remote_auth(state: &crate::AppState) {
    state
        .token_optional
        .store(false, std::sync::atomic::Ordering::Relaxed);
    state.enforce_token.store(
        state.initial_enforce_token,
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) fn bind_listener(
    addr: std::net::SocketAddr,
    only_v6: bool,
) -> std::io::Result<tokio::net::TcpListener> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    // 允许地址复用（消除 TIME_WAIT 状态导致原地重启交接时的端口冲突）
    let _ = socket.set_reuse_address(true);
    #[cfg(all(unix, not(target_os = "solaris")))]
    let _ = socket.set_reuse_port(true);
    // Windows defaults IPV6_V6ONLY to 0, so a `[::]` socket also claims
    // `0.0.0.0` and can leave the IPv4 listener accepting only this PC.
    // Keep the two stacks on separate sockets.
    if only_v6 {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(addr))?;
    socket.listen(1024)?;
    let listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(listener)
}

async fn open_listeners(
    host: &str,
    port: u16,
) -> Result<(tokio::net::TcpListener, Option<tokio::net::TcpListener>), String> {
    if host == "0.0.0.0" {
        let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
        let primary = bind_listener(v4, false)
            .map_err(|err| format!("failed to listen on {host}:{port}: {err}"))?;
        let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        let secondary = match bind_listener(v6, true) {
            Ok(listener) => {
                tracing::info!(%v6, "temporary remote access: IPv6 listener active");
                Some(listener)
            }
            Err(err) => {
                tracing::warn!(%v6, %err, "temporary remote access: IPv6 bind failed (IPv4 only)");
                None
            }
        };
        ensure_unspecified(&primary, host)?;
        return Ok((primary, secondary));
    }
    if host == "::" || host == "[::]" {
        let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        let primary = bind_listener(v6, true)
            .map_err(|err| format!("failed to listen on {host}:{port}: {err}"))?;
        let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
        let secondary = match bind_listener(v4, false) {
            Ok(listener) => {
                tracing::info!(%v4, "temporary remote access: IPv4 listener active");
                Some(listener)
            }
            Err(err) => {
                tracing::warn!(%v4, %err, "temporary remote access: IPv4 bind failed");
                None
            }
        };
        ensure_unspecified(&primary, host)?;
        return Ok((primary, secondary));
    }
    if let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() {
        let primary = bind_listener(addr, addr.is_ipv6())
            .map_err(|err| format!("failed to listen on {host}:{port}: {err}"))?;
        return Ok((primary, None));
    }
    match tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
        Ok(listener) => Ok((listener, None)),
        Err(err) => Err(format!("failed to listen on {host}:{port}: {err}")),
    }
}

fn ensure_unspecified(listener: &tokio::net::TcpListener, host: &str) -> Result<(), String> {
    let bound = listener
        .local_addr()
        .map_err(|err| format!("failed to read listener address: {err}"))?;
    if bound.ip().is_loopback() {
        return Err(format!(
            "listener for {host} came up on {bound}, which other devices cannot open"
        ));
    }
    tracing::info!(%bound, "temporary remote access listening");
    Ok(())
}

fn take_extra_remote(state: &crate::AppState) -> Option<crate::ExtraRemoteBind> {
    state
        .extra_remote
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take()
}

async fn with_firewall(mut status: RemoteAccessStatus) -> RemoteAccessStatus {
    if status.active && !is_loopback_host(&status.host) {
        status.firewall = inbound_firewall_status().await;
    }
    status
}

async fn inbound_firewall_status() -> Option<String> {
    #[cfg(all(windows, not(test)))]
    {
        if FIREWALL_ALLOWED.load(std::sync::atomic::Ordering::Relaxed) {
            return Some("allowed".to_string());
        }
        let status = tokio::task::spawn_blocking(windows_firewall_status)
            .await
            .ok()
            .flatten();
        if status.as_deref() == Some("allowed") {
            FIREWALL_ALLOWED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        return status;
    }
    #[cfg(not(all(windows, not(test))))]
    {
        None
    }
}

#[cfg(all(windows, not(test)))]
static FIREWALL_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(windows, not(test)))]
static FIREWALL_PROMPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(windows, not(test)))]
fn windows_firewall_status() -> Option<String> {
    let exe = current_exe_path()?;
    if firewall_rule_matches(&exe) {
        return Some("allowed".to_string());
    }
    if try_add_firewall_rule(&exe) && firewall_rule_matches(&exe) {
        return Some("allowed".to_string());
    }
    if !FIREWALL_PROMPTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let _ = spawn_elevated_firewall_rule(&exe);
    }
    Some("prompt".to_string())
}

#[cfg(all(windows, not(test)))]
fn current_exe_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let text = exe.display().to_string();
    let text = text.trim_start_matches(r"\\?\").trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(all(windows, not(test)))]
fn firewall_rule_matches(exe: &str) -> bool {
    let Some(text) = netsh_text(&[
        "advfirewall",
        "firewall",
        "show",
        "rule",
        "name=JeikCode",
        "verbose",
    ]) else {
        return false;
    };
    let folded = text.to_ascii_lowercase();
    folded.contains(&exe.to_ascii_lowercase())
        && (folded.contains("allow") || folded.contains("允许"))
}

#[cfg(all(windows, not(test)))]
fn try_add_firewall_rule(exe: &str) -> bool {
    netsh_status(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        "name=JeikCode",
        "dir=in",
        "action=allow",
        &format!("program={exe}"),
        "enable=yes",
        "profile=any",
    ])
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn build_firewall_elevated_script(exe: &str) -> String {
    let escaped = exe.replace('\'', "''");
    format!(
        "netsh advfirewall firewall delete rule name=JeikCode | Out-Null\r\nnetsh advfirewall firewall add rule name=JeikCode dir=in action=allow program='{escaped}' enable=yes profile=any\r\n"
    )
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn encode_powershell_utf16_command(script: &str) -> String {
    use base64::Engine as _;
    let utf16_bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16_bytes)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn build_firewall_elevated_launch_command(exe: &str) -> String {
    let script = build_firewall_elevated_script(exe);
    let encoded = encode_powershell_utf16_command(&script);
    format!(
        "Start-Process powershell -Verb RunAs -WindowStyle Hidden -ArgumentList '-NoProfile','-EncodedCommand','{encoded}'"
    )
}

#[cfg(all(windows, not(test)))]
fn spawn_elevated_firewall_rule(exe: &str) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let launch = build_firewall_elevated_launch_command(exe);
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &launch])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .is_ok()
}

#[cfg(all(windows, not(test)))]
fn netsh_text(args: &[&str]) -> Option<String> {
    let output =
        run_command_with_timeout_drain(netsh_command(args), std::time::Duration::from_millis(1500))
            .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

#[cfg(all(windows, not(test)))]
fn netsh_status(args: &[&str]) -> bool {
    run_command_with_timeout_drain(netsh_command(args), std::time::Duration::from_millis(1500))
        .is_ok_and(|output| output.success)
}

#[cfg(all(windows, not(test)))]
fn netsh_command(args: &[&str]) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new("netsh");
    command.args(args).creation_flags(CREATE_NO_WINDOW);
    command
}

use jeikcode_config::desktop_shared::{
    desktop_settings_path as shared_desktop_settings_path, load_close_action_at,
    load_webui_listen_pref_at, save_close_action_at, save_webui_listen_pref_at, webui_listen_path,
    DesktopSettings as DesktopSettingsPayload, DesktopSettingsError, WebuiListenPref,
};

fn webui_listen_pref_path() -> std::path::PathBuf {
    webui_listen_path()
}

fn load_webui_listen_pref() -> Option<WebuiListenPref> {
    load_webui_listen_pref_at(&webui_listen_pref_path())
}

pub(crate) fn saved_listen_port() -> Option<u16> {
    load_webui_listen_pref()
        .map(|pref| pref.port)
        .filter(|port| *port != 0)
}

pub(crate) fn saved_listen_token() -> Option<String> {
    load_webui_listen_pref().and_then(|pref| pref.token)
}

/// Write the next desktop launch. `token: None` keeps the token already on disk.
pub(crate) fn save_webui_listen_pref(
    host: Option<String>,
    port: u16,
    token: Option<String>,
) -> Result<(), String> {
    save_webui_listen_pref_at(
        &webui_listen_pref_path(),
        host.as_deref(),
        port,
        token.as_deref(),
    )
}

async fn remote_status_async(
    state: &crate::AppState,
    token: Option<String>,
) -> Result<RemoteAccessStatus, axum::http::StatusCode> {
    let state = state.clone();
    #[cfg(test)]
    let pref_path = webui_listen_pref_path();
    tokio::task::spawn_blocking(move || {
        let mut status = remote_status_with_pref(&state, token, {
            #[cfg(test)]
            {
                &pref_path
            }
            #[cfg(not(test))]
            {
                &webui_listen_pref_path()
            }
        });
        let _ = &mut status;
        status
    })
    .await
    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

/// The running socket stays as it is. A matching port registers the token now.
/// A different port is only remembered for the next `0.0.0.0` start.
/// Checking "no token" changes this process and is not written down.
fn apply_launch_settings_blocking(
    state: &crate::AppState,
    body: &RemoteAccessBody,
    pref_path: &std::path::Path,
) -> Result<RemoteAccessStatus, String> {
    if body.port == 0 {
        return Err("port must be 1-65535".to_string());
    }
    let target_host = body.host.trim();
    let host_opt = if !target_host.is_empty() {
        Some(target_host.to_string())
    } else {
        None
    };
    let port_changed = body.port != state.bind_port;
    let host_changed =
        !target_host.is_empty() && !target_host.eq_ignore_ascii_case(&state.bind_host);

    if body.no_token {
        if !port_changed && !host_changed {
            // Only disable auth live when no non-loopback listener is active.
            let extra_non_loopback = state
                .extra_remote
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .as_ref()
                .is_some_and(|b| !crate::auth_token::is_loopback_bind_host(&b.host));
            if crate::auth_token::is_loopback_bind_host(&state.bind_host) && !extra_non_loopback {
                apply_remote_auth(state, true);
            }
        } else {
            save_webui_listen_pref_at(pref_path, host_opt.as_deref(), body.port, None)?;
        }
    } else {
        let token = body.token.trim();
        let token = if token.is_empty() {
            state
                .webui_tokens
                .display()
                .unwrap_or_else(|| state.webui_tokens.mint())
        } else {
            if !state.webui_tokens.register(token) {
                return Err("token is empty".to_string());
            }
            token.to_string()
        };
        if !port_changed && !host_changed {
            state.webui_tokens.set_display(&token);
            apply_remote_auth(state, false);
        }
        save_webui_listen_pref_at(pref_path, host_opt.as_deref(), body.port, Some(&token))?;
    }
    let mut status = remote_status_with_pref(state, None, pref_path);
    if port_changed || host_changed {
        if port_changed {
            status.next_port = Some(body.port);
        }
        if host_changed {
            status.next_host = Some(body.host.clone());
        }
        if !body.no_token {
            if let Some(pref) = load_webui_listen_pref_at(pref_path) {
                status.token = pref.token;
            }
        }
    }
    Ok(status)
}

async fn apply_launch_settings(
    state: &crate::AppState,
    body: RemoteAccessBody,
) -> Result<RemoteAccessStatus, (axum::http::StatusCode, String)> {
    let state = state.clone();
    let pref_path = webui_listen_pref_path();
    tokio::task::spawn_blocking(move || apply_launch_settings_blocking(&state, &body, &pref_path))
        .await
        .map_err(|err| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("remote access task panicked: {err}"),
            )
        })?
        .map_err(|err| (axum::http::StatusCode::BAD_REQUEST, err))
}

/// GET /api/remote-access — current temporary listener, if one is open.
pub(crate) async fn get_remote_access(State(state): State<AppState>) -> impl IntoResponse {
    let extra_open = state
        .extra_remote
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .is_some();
    let status = match remote_status_async(&state, None).await {
        Ok(status) => status,
        Err(code) => return json_error(code, "failed to inspect remote status").into_response(),
    };
    if extra_open {
        Json(with_firewall(status).await).into_response()
    } else {
        Json(status).into_response()
    }
}

fn validate_desktop_no_token(
    no_token: bool,
    desktop_managed: bool,
) -> Result<(), (axum::http::StatusCode, &'static str)> {
    if no_token && desktop_managed {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "desktop mode requires an access token",
        ));
    }
    Ok(())
}

/// POST /api/remote-access — open or close an extra listen address.
///
/// The desktop and local WebUI stay on their original port. This binds a
/// second listener (default `0.0.0.0:4096`) so a phone on the LAN can connect
/// without restarting the process.
pub(crate) async fn post_remote_access(
    State(state): State<AppState>,
    Json(body): Json<RemoteAccessBody>,
) -> impl IntoResponse {
    // Serialize remote-access mutations across stop/apply/rebind awaits so concurrent
    // calls cannot leak an extra listener or expose a wildcard socket without auth.
    let _remote_guard = state.remote_access_lock.lock().await;

    if body.stop {
        if let Some(prev) = take_extra_remote(&state) {
            prev.abort_all();
        }
        restore_remote_auth(&state);
        return match remote_status_async(&state, None).await {
            Ok(status) => Json(status).into_response(),
            Err(code) => json_error(code, "failed to inspect remote status").into_response(),
        };
    }

    if let Err((status, msg)) =
        validate_desktop_no_token(body.no_token, jeikcode_updater::is_desktop_managed())
    {
        return json_error(status, msg).into_response();
    }

    // Remote-access is a privilege expansion: it must never turn a loopback/no-token
    // daemon into a network-reachable/no-token daemon, nor strip auth from any active
    // non-loopback listener via a loopback-targeted `apply_launch` payload.
    if body.no_token {
        let already_unauthenticated_remote = !state.initial_enforce_token
            && !crate::auth_token::is_loopback_bind_host(&state.bind_host);
        if !already_unauthenticated_remote {
            let target = body.host.trim();
            let pref_path = webui_listen_pref_path();
            let saved_host = tokio::task::spawn_blocking(move || {
                load_webui_listen_pref_at(&pref_path).and_then(|p| p.host)
            })
            .await
            .ok()
            .flatten();
            let effective_target = if !target.is_empty() {
                target
            } else if body.apply_launch {
                saved_host.as_deref().unwrap_or(state.bind_host.as_str())
            } else {
                state.bind_host.as_str()
            };
            let extra_non_loopback = state
                .extra_remote
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .as_ref()
                .is_some_and(|b| !crate::auth_token::is_loopback_bind_host(&b.host));
            let active_non_loopback =
                !crate::auth_token::is_loopback_bind_host(&state.bind_host) || extra_non_loopback;
            let is_restart_to_loopback = body.apply_launch
                && body.restart
                && crate::auth_token::is_loopback_bind_host(effective_target)
                && !extra_non_loopback;
            if !crate::auth_token::is_loopback_bind_host(effective_target)
                || (active_non_loopback && !is_restart_to_loopback)
            {
                return json_error(
                    axum::http::StatusCode::BAD_REQUEST,
                    "non-loopback remote access requires an access token",
                )
                .into_response();
            }
        }
    }
    if body.apply_launch {
        let should_restart = body.restart;
        let old_port = state.bind_port;
        let restart_host = body.host.trim().to_string();
        let restart_port = body.port;
        let no_token = body.no_token;
        let restart_token = body.token.clone();
        return match apply_launch_settings(&state, body).await {
            Ok(status) => {
                if should_restart {
                    let effective_token = status.token.clone().or_else(|| {
                        let t = restart_token.trim();
                        (!t.is_empty()).then(|| t.to_string())
                    });
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Ok(exe) = jeikcode_updater::current_exe_path() {
                                let _ = jeikcode_updater::restart_process_with_config(
                                    &exe,
                                    old_port,
                                    if restart_host.is_empty() {
                                        None
                                    } else {
                                        Some(&restart_host)
                                    },
                                    restart_port,
                                    effective_token.as_deref(),
                                    Some(no_token),
                                );
                            }
                        })
                        .await;
                    });
                }
                Json(status).into_response()
            }
            Err((code, err)) => json_error(code, err).into_response(),
        };
    }

    let host = body.host.trim().to_string();
    if host.is_empty()
        || host.len() > 255
        || host
            .chars()
            .any(|ch| ch.is_whitespace() || ch == '/' || ch == '\\')
    {
        return json_error(
            axum::http::StatusCode::BAD_REQUEST,
            "listen address is empty or invalid",
        )
        .into_response();
    }
    if body.port == 0 {
        return json_error(axum::http::StatusCode::BAD_REQUEST, "port must be 1-65535")
            .into_response();
    }

    let minted = if body.no_token {
        None
    } else {
        let token = body.token.trim();
        if token.is_empty() {
            Some(state.webui_tokens.mint())
        } else if state.webui_tokens.register(token) {
            Some(token.to_string())
        } else {
            return json_error(axum::http::StatusCode::BAD_REQUEST, "token is empty")
                .into_response();
        }
    };

    let same_as_primary =
        host.eq_ignore_ascii_case(&state.bind_host) && body.port == state.bind_port;
    if same_as_primary {
        apply_remote_auth(&state, body.no_token);
        return match remote_status_async(&state, minted).await {
            Ok(status) => Json(with_firewall(status).await).into_response(),
            Err(code) => json_error(code, "failed to inspect remote status").into_response(),
        };
    }

    // Same host and port: update the token only. Drop the mutex before
    // `remote_status` — it locks `extra_remote` again, and a std mutex does
    // not allow that on the same thread.
    let reused = {
        let mut guard = state
            .extra_remote
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(bind) = guard.as_mut() {
            if bind.host == host && bind.port == body.port {
                bind.token = minted.clone();
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if reused {
        apply_remote_auth(&state, body.no_token);
        return match remote_status_async(&state, minted).await {
            Ok(status) => Json(with_firewall(status).await).into_response(),
            Err(code) => json_error(code, "failed to inspect remote status").into_response(),
        };
    }

    let Some(router) = state.http_router.get().cloned() else {
        return json_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "server is still starting",
        )
        .into_response();
    };

    // A different port is bound first, so a failed Apply keeps the listener
    // that is already open. The same port has to be released before the new
    // socket can take it; Windows often needs a moment after the abort.
    let previous_port = {
        let guard = state
            .extra_remote
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        guard.as_ref().map(|bind| bind.port)
    };
    let same_port = previous_port == Some(body.port);
    if same_port {
        if let Some(prev) = take_extra_remote(&state) {
            prev.abort_all();
            tokio::task::yield_now().await;
        }
    }

    let opened = match open_listeners(&host, body.port).await {
        Ok(listeners) => Ok(listeners),
        Err(err) if same_port => {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match open_listeners(&host, body.port).await {
                Ok(listeners) => Ok(listeners),
                Err(_) => Err(err),
            }
        }
        Err(err) => Err(err),
    };
    let (primary_listener, secondary_listener) = match opened {
        Ok(listeners) => listeners,
        Err(err) => {
            if same_port {
                restore_remote_auth(&state);
            }
            return json_error(axum::http::StatusCode::CONFLICT, err).into_response();
        }
    };

    if !same_port {
        if let Some(prev) = take_extra_remote(&state) {
            prev.abort_all();
        }
    }

    // Apply auth enforcement BEFORE spawning the listener tasks so an incoming
    // wildcard connection can never race ahead of `enforce_token` / `token_optional`.
    apply_remote_auth(&state, body.no_token);

    let mut aborts = Vec::new();
    let router_primary = router.clone();
    let task_primary = tokio::spawn(async move {
        if let Err(err) = axum::serve(primary_listener, router_primary).await {
            tracing::error!(?err, "temporary remote listener stopped");
        }
    });
    aborts.push(task_primary.abort_handle());

    if let Some(secondary) = secondary_listener {
        let router_secondary = router.clone();
        let task_secondary = tokio::spawn(async move {
            if let Err(err) = axum::serve(secondary, router_secondary).await {
                tracing::error!(?err, "temporary remote secondary listener stopped");
            }
        });
        aborts.push(task_secondary.abort_handle());
    }

    *state
        .extra_remote
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(crate::ExtraRemoteBind {
        host,
        port: body.port,
        aborts,
        token: minted.clone(),
    });

    match remote_status_async(&state, minted).await {
        Ok(status) => Json(with_firewall(status).await).into_response(),
        Err(code) => json_error(code, "failed to inspect remote status").into_response(),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ZombieProcessInfo {
    pub pid: u32,
    pub name: String,
    pub path: Option<String>,
    pub reason: String,
}

#[derive(Debug, serde::Serialize)]
pub struct LegacyStatusResponse {
    pub has_legacy_files: bool,
    pub legacy_files: Vec<String>,
    pub has_dirty_path: bool,
    pub zombie_count: usize,
    pub zombies: Vec<ZombieProcessInfo>,
    pub needs_migration: bool,
}

#[derive(Debug, serde::Deserialize)]
pub struct MigrateLegacyRequest {
    #[serde(default)]
    pub kill_zombies: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct MigrateLegacyResponse {
    pub success: bool,
    pub cleaned_files_count: usize,
    pub killed_zombies_count: usize,
}

fn scan_zombie_processes_in(sys: &sysinfo::System) -> Vec<ZombieProcessInfo> {
    let my_pid = sysinfo::get_current_pid().ok().map(|p| p.as_u32());
    let mut out = Vec::new();

    // 收集所有当前活跃的桌面壳 PID (jeikcode-desktop)
    let mut active_desktop_pids = std::collections::HashSet::new();
    for (pid, proc_) in sys.processes() {
        let name = proc_.name().to_string_lossy().to_ascii_lowercase();
        if name.contains("desktop") || name.contains("tauri") {
            active_desktop_pids.insert(pid.as_u32());
        }
    }

    for (pid, proc_) in sys.processes() {
        let pid_u32 = pid.as_u32();
        if Some(pid_u32) == my_pid {
            continue;
        }
        let name = proc_.name().to_string_lossy().to_ascii_lowercase();
        if name.contains("desktop") || name.contains("tauri") {
            continue;
        }

        let is_jeik_family = name.starts_with("jeikcode") || name.starts_with("atomcode");
        if !is_jeik_family {
            continue;
        }

        let exe_path_str = proc_
            .exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        // 1. 判定是否为老旧 Dropper 目录残存
        let is_legacy_path =
            jeikcode_config::desktop_shared::is_dirty_legacy_path_entry(&exe_path_str);

        // 2. 检查环境变量是否包含 JEIKCODE_DESKTOP=1
        let is_desktop_env = proc_.environ().iter().any(|env_str| {
            let s = env_str.to_string_lossy();
            s.starts_with("JEIKCODE_DESKTOP=")
                || s.starts_with("JEIKCODE_DESKTOP_CLI_BIN=")
                || s.starts_with("JEIKCODE_DESKTOP_PID=")
        });

        let declared_desktop_pid = proc_.environ().iter().find_map(|env_str| {
            let s = env_str.to_string_lossy();
            s.strip_prefix("JEIKCODE_DESKTOP_PID=")
                .and_then(|val| val.trim().parse::<u32>().ok())
                .filter(|pid| *pid > 0)
        });

        // 3. 检查父进程与绑定的桌面壳状态
        let parent_pid = proc_.parent().map(|p| p.as_u32());
        let desktop_owner_alive = parent_pid
            .is_some_and(|ppid| active_desktop_pids.contains(&ppid))
            || declared_desktop_pid.is_some_and(|dpid| active_desktop_pids.contains(&dpid));

        // 判定准则（严格保护普通终端/开发/TUI 实例）：
        // - 如果是运行在旧目录 .local\bin 下的实例 -> 100% 历史遗留僵尸
        // - 如果原本是桌面端子进程 (is_desktop_env)，但绑定的父级桌面壳已不再存活 -> 100% 孤儿僵尸
        let mut zombie_reason = None;
        if is_legacy_path {
            zombie_reason = Some("旧版 ~/.local/bin 遗留运行实例".to_string());
        } else if is_desktop_env && !desktop_owner_alive {
            zombie_reason = Some("父级桌面壳已退出，脱离托管的孤儿子进程".to_string());
        }

        if let Some(reason) = zombie_reason {
            out.push(ZombieProcessInfo {
                pid: pid_u32,
                name: name.clone(),
                path: if exe_path_str.is_empty() {
                    None
                } else {
                    Some(exe_path_str)
                },
                reason,
            });
        }
    }
    out
}

pub fn scan_zombie_processes() -> Vec<ZombieProcessInfo> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    scan_zombie_processes_in(&sys)
}

pub fn kill_zombie_processes_all() -> usize {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let zombies = scan_zombie_processes_in(&sys);
    let mut killed = 0;
    for z in zombies {
        if let Some(proc_) = sys.process(Pid::from_u32(z.pid)) {
            if proc_.kill() {
                killed += 1;
            }
        }
    }
    killed
}

pub(crate) async fn get_legacy_status() -> impl IntoResponse {
    match tokio::task::spawn_blocking(|| {
        let current_exe = std::env::current_exe().ok();
        let legacy_files: Vec<String> =
            jeikcode_config::desktop_shared::scan_legacy_dropper_files(current_exe.as_deref())
                .into_iter()
                .map(|p| p.display().to_string())
                .collect();
        let has_dirty_path = jeikcode_config::desktop_shared::has_dirty_hkcu_path();

        let zombies = scan_zombie_processes();
        let zombie_count = zombies.len();
        let has_legacy_files = !legacy_files.is_empty();
        let needs_migration = has_legacy_files || has_dirty_path || zombie_count > 0;

        LegacyStatusResponse {
            has_legacy_files,
            legacy_files,
            has_dirty_path,
            zombie_count,
            zombies,
            needs_migration,
        }
    })
    .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(_) => json_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "legacy status scan failed",
        )
        .into_response(),
    }
}

pub(crate) async fn post_migrate_legacy(
    Json(payload): Json<MigrateLegacyRequest>,
) -> impl IntoResponse {
    match tokio::task::spawn_blocking(move || {
        let current_exe = std::env::current_exe().ok();
        let cleaned_files_count =
            jeikcode_config::desktop_shared::cleanup_legacy_dropper_files(current_exe.as_deref());

        jeikcode_updater::migrate_legacy_environment();

        let mut killed_zombies_count = 0;
        if payload.kill_zombies {
            killed_zombies_count = kill_zombie_processes_all();
        }

        MigrateLegacyResponse {
            success: true,
            cleaned_files_count,
            killed_zombies_count,
        }
    })
    .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(_) => json_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "legacy migration failed",
        )
        .into_response(),
    }
}

fn desktop_settings_file_path() -> std::path::PathBuf {
    shared_desktop_settings_path()
}

pub(crate) async fn get_desktop_settings() -> impl IntoResponse {
    let path = desktop_settings_file_path();
    Json(DesktopSettingsPayload {
        close_action: load_close_action_at(&path),
    })
}

fn write_desktop_settings_at(
    path: &std::path::Path,
    payload: &DesktopSettingsPayload,
) -> Result<(), (axum::http::StatusCode, String)> {
    save_close_action_at(path, &payload.close_action).map_err(|err| match err {
        DesktopSettingsError::InvalidAction => {
            (axum::http::StatusCode::BAD_REQUEST, err.to_string())
        }
        DesktopSettingsError::Io(msg) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg),
    })
}

pub(crate) async fn post_desktop_settings(
    Json(payload): Json<DesktopSettingsPayload>,
) -> impl IntoResponse {
    let path = desktop_settings_file_path();
    match write_desktop_settings_at(&path, &payload) {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err((status, msg)) => json_error(status, msg).into_response(),
    }
}

/// POST /config/reload - Reloads config.toml from disk, remounts MCP/skills on
/// the live runtime, and returns the sanitized config view.
pub(crate) async fn reload_config(State(state): State<AppState>) -> impl IntoResponse {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            return json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    };
    let _ = crate::reload_mcp_and_live_runtime(&state, true, None).await;
    let reloaded_registry =
        invalidate_and_reload_idle_registry_handles(Some(&state.telemetry), &config).await;
    crate::publish_global(crate::GlobalWireEvent::ConfigReloaded);
    let _ = reloaded_registry;
    Json(config_response(&config)).into_response()
}

/// Refresh idle `SessionRuntimeRegistry` handles (and clear cached provider
/// fingerprints for busy handles so their next idle boundary or `/live/message`
/// reassembles against the fresh `config.toml`).
async fn invalidate_and_reload_idle_registry_handles(
    telemetry: Option<&std::sync::Arc<jeikcode_telemetry::Telemetry>>,
    config: &Config,
) -> usize {
    let registry = jeikcode_coding::session_runtime_registry::SessionRuntimeRegistry::global();
    let mut reloaded = 0usize;
    for entry in registry.list_all() {
        let sid = &entry.session_id;
        // Clear the cached fingerprint first so even a busy session (or one
        // whose config reload is skipped here) re-evaluates on next message.
        registry.set_provider_fingerprint(sid, None);
        let Some(handle) = registry.handle(sid) else {
            continue;
        };
        if !matches!(
            handle.status().phase,
            jeikcode_coding::RuntimePhase::Ready | jeikcode_coding::RuntimePhase::AwaitingProvider
        ) {
            continue;
        }
        let resolved_provider = {
            let mgr =
                jeikcode_capabilities::session::SessionManager::for_project(&entry.working_dir);
            mgr.load_native_session(sid)
                .ok()
                .and_then(|loaded| loaded.meta.preferred_model)
                .filter(|name| !name.is_empty() && config.selection_exists(name))
        };
        let Some(next_cfg) = crate::native_live::build_reload_coding_config_from(
            config,
            &entry.working_dir,
            resolved_provider.as_deref(),
            Some(sid),
            telemetry.cloned(),
        ) else {
            continue;
        };
        let target_provider = next_cfg.provider_name.clone();
        let plugin_dirs = crate::gather_plugin_skill_dirs_for(&entry.working_dir);
        if handle
            .reload_capabilities_with_config(Some(plugin_dirs), next_cfg)
            .await
            .is_ok()
        {
            reloaded += 1;
            if let Ok(fp) = crate::native_live::provider_fingerprint(config, &target_provider) {
                registry.set_provider_fingerprint(sid, Some(fp));
            }
        }
    }
    reloaded
}

#[cfg(test)]
mod tests {
    #[test]
    fn language_endpoint_canonical_choices_persist_before_atomic_update() {
        use jeikcode_config::{i18n, locale::Locale, ConfigStore};
        let _guard = i18n::test_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ConfigStore::new(tmp.path().join("config.toml"));
        for (key, locale) in [
            ("en", Locale::En),
            ("vi-VN", Locale::Vi),
            ("zh-CN", Locale::ZhCn),
        ] {
            let parsed = super::parse_language(key).unwrap();
            assert_eq!(parsed, locale);
            let config = super::persist_language(parsed, &store).unwrap();
            assert_eq!(super::config_response(&config).language, key);
            assert_eq!(store.read().unwrap().config.language, Some(locale));
            assert_eq!(i18n::current_locale(), locale);
        }
        let before = std::fs::read(store.path()).unwrap();
        for invalid in ["invalid_lang", "vi-VN-extra", "", "vi_US"] {
            assert!(super::parse_language(invalid).is_err());
            assert_eq!(std::fs::read(store.path()).unwrap(), before);
            assert_eq!(i18n::current_locale(), Locale::ZhCn);
        }
        let bad_store = ConfigStore::new(tmp.path());
        assert!(super::persist_language(Locale::Vi, &bad_store).is_err());
        assert_eq!(i18n::current_locale(), Locale::ZhCn);
    }

    use super::*;

    fn provider(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_type: "openai".into(),
            api_key: None,
            model: "model".into(),
            base_url: Some(base_url.into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            reasoning_levels: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
            pricing: None,
            supports_vision: None,
            reasoning_model: None,
        }
    }

    #[test]
    fn config_response_lists_new_schema_and_folded_codingplan_models() {
        // A config where the selectable models live ONLY in the new schema
        // (provider_accounts + models) — none in [providers.*].
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "JeikCode-GLM-5.2",
            "provider_accounts": { "JeikCode": { "provider": "openai", "base_url": "" } },
            "models": {
                "JeikCode-GLM-5.2": { "account": "JeikCode", "model": "GLM-5.2", "context_window": 128000 },
                "JeikCode-Qwen": { "account": "JeikCode", "model": "Qwen", "context_window": 128000 }
            }
        }))
        .unwrap();
        let resp = config_response(&config);
        let names: Vec<&str> = resp.providers.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"JeikCode-GLM-5.2"),
            "new-schema model listed"
        );
        assert!(names.contains(&"JeikCode-Qwen"));
        assert_eq!(resp.default_provider, "JeikCode-GLM-5.2");
        let glm = resp
            .providers
            .iter()
            .find(|p| p.name == "JeikCode-GLM-5.2")
            .unwrap();
        assert!(glm.is_default);
        assert!(!glm.requires_login, "gateway signing is retired");
        assert_eq!(glm.model, "GLM-5.2");
    }

    #[test]
    fn provider_info_reports_login_dependency_from_gateway() {
        assert!(!provider_info("renamed", &provider(""), "renamed").requires_login);
        assert!(
            !provider_info(
                "JeikCode-looking-custom",
                &provider("https://example.test/v1"),
                "JeikCode-looking-custom"
            )
            .requires_login
        );
    }

    #[test]
    fn provider_info_exposes_pricing_without_credentials() {
        let mut configured = provider("https://example.test/v1");
        configured.pricing = Some(jeikcode_config::config::provider::ProviderPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: 0.25,
        });
        let info = provider_info("custom", &configured, "custom");
        assert_eq!(info.pricing, configured.pricing);
        assert!(!info.has_api_key);
    }

    #[tokio::test]
    async fn remote_status_and_enforce_token_lifecycle() {
        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);

        // 1. 初始状态：未激活临时访问，enforce_token = false
        assert!(!state.is_token_enforced());
        let status = remote_status(&state, None);
        assert!(!status.active);
        assert!(status.url.is_none());

        // 2. 模拟配置带 token 的监听
        let _ = state.webui_tokens.register("test-token");
        state
            .enforce_token
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.extra_remote.lock().unwrap() = Some(crate::ExtraRemoteBind {
            host: "0.0.0.0".into(),
            port: 4096,
            aborts: Vec::new(),
            token: Some("test-token".into()),
        });

        // 验证状态回显与 token 保留。通配监听不能把 127.0.0.1 当成分享地址。
        assert!(state.is_token_enforced());
        let active_status = remote_status(&state, None);
        assert!(active_status.active);
        assert_eq!(active_status.token.as_deref(), Some("test-token"));
        for url in active_status.url.iter().chain(active_status.urls.iter()) {
            assert!(url.contains(":4096/?token=test-token"), "{url}");
            assert!(!url.contains("127.0.0.1"), "{url}");
            assert!(!url.contains("[::1]"), "{url}");
        }

        // 3. 停止临时访问后，恢复到初始 enforce_token (false)
        state.extra_remote.lock().unwrap().take();
        state.enforce_token.store(
            state.initial_enforce_token,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert!(!state.is_token_enforced());
        let stopped_status = remote_status(&state, None);
        assert!(!stopped_status.active);
    }

    #[test]
    fn ipv4_literals_skip_loopback_and_keep_lan_addresses() {
        let blob = b"IPv4 Address. . . . . . . . . . . : 192.168.1.20\r\n\
                     Subnet Mask . . . . . . . . . . . : 255.255.255.0\r\n\
                     Autoconfiguration IPv4 Address. . : 169.254.8.9\r\n\
                     Subnet Mask . . . . . . . . . . . : 255.255.240.0\r\n\
                     IPv4 Address. . . . . . . . . . . : 127.0.0.1\r\n\
                     IPv4 Address. . . . . . . . . . . : 10.0.0.8\r\n\
                     IPv4 Address. . . . . . . . . . . : 0.0.0.0\r\n";
        let found = collect_ipv4_literals(blob);
        assert_eq!(
            found,
            vec![
                "192.168.1.20".to_string(),
                "169.254.8.9".to_string(),
                "10.0.0.8".to_string(),
            ]
        );
        let urls = access_urls("127.0.0.1", 4096, Some("tok"), false);
        assert_eq!(urls, vec!["http://127.0.0.1:4096/?token=tok".to_string()]);
    }

    #[test]
    fn ipv6_literals_extract_gua_and_ula_skip_link_local() {
        let blob = b"IPv6 Address. . . . . . . . . . . : 2409:8a00:1234:5678::1\r\n\
                     Temporary IPv6 Address. . . . . . : 2409:8a00:1234:5678:abcd:ef01:2345:6789\r\n\
                     Link-local IPv6 Address . . . . . : fe80::51b2:2cb4:9999:8888%12\r\n\
                     IPv6 Address. . . . . . . . . . . : fd12:3456:789a::1\r\n\
                     IPv6 Address. . . . . . . . . . . : ::1\r\n\
                     IPv6 Address. . . . . . . . . . . : ::\r\n";
        let found = collect_ipv6_literals(blob);
        assert_eq!(
            found,
            vec![
                "2409:8a00:1234:5678::1".to_string(),
                "2409:8a00:1234:5678:abcd:ef01:2345:6789".to_string(),
                "fd12:3456:789a::1".to_string(),
            ]
        );
    }

    #[test]
    fn ipv6_literals_strip_cidr_and_supports_linux_ip_addr_output() {
        let blob = b"2: eth0    inet6 2409:8a00:1234:5678::100/64 scope global dynamic \r\n\
                     3: wlan0   inet6 fe80::1234/64 scope link \r\n\
                     4: docker0 inet6 fd00:dead:beef::1/48 scope global \r\n\
                     1: lo      inet6 ::1/128 scope host \r\n";
        let found = collect_ipv6_literals(blob);
        assert_eq!(
            found,
            vec![
                "2409:8a00:1234:5678::100".to_string(),
                "fd00:dead:beef::1".to_string(),
            ]
        );
    }

    #[test]
    fn access_urls_formats_ipv6_with_brackets() {
        let url = format_access_url("2409:8a00:1::1", 13457, Some("anhuang"), false);
        assert_eq!(url, "http://[2409:8a00:1::1]:13457/?token=anhuang");
    }

    #[tokio::test]
    async fn wildcard_bind_accepts_loopback_on_an_unspecified_socket() {
        let listener = bind_listener(
            std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0)),
            false,
        )
        .expect("bind 0.0.0.0");
        let bound = listener.local_addr().unwrap();
        assert!(bound.ip().is_unspecified(), "{bound}");
        let port = bound.port();
        let probe = tokio::net::TcpStream::connect(("127.0.0.1", port)).await;
        assert!(probe.is_ok(), "this PC should reach a 0.0.0.0 listener");
    }

    #[tokio::test]
    async fn reapply_same_remote_bind_returns_and_stop_clears_it() {
        use axum::extract::State;
        use axum::Json;

        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        *state.extra_remote.lock().unwrap() = Some(crate::ExtraRemoteBind {
            host: "0.0.0.0".into(),
            port: 4096,
            aborts: Vec::new(),
            token: Some("old-token".into()),
        });

        let apply = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 4096,
                token: "next-token".into(),
                no_token: false,
                stop: false,
                apply_launch: false,
                restart: false,
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), apply)
            .await
            .expect("reapply deadlocked while the Apply button would stay disabled");
        assert_eq!(
            state
                .extra_remote
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|bind| bind.token.clone())
                .as_deref(),
            Some("next-token")
        );

        let stop = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 4096,
                token: String::new(),
                no_token: false,
                stop: true,
                apply_launch: false,
                restart: false,
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), stop)
            .await
            .expect("stop deadlocked");
        assert!(state.extra_remote.lock().unwrap().is_none());
        assert!(!state.is_token_enforced());
    }

    #[tokio::test]
    async fn loopback_no_token_daemon_cannot_expand_to_unauthenticated_wildcard_listener() {
        use axum::extract::State;
        use axum::response::IntoResponse;
        use axum::Json;

        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        assert!(
            !state.is_token_enforced(),
            "test starts in legacy loopback no-token mode"
        );

        let response = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 4096,
                token: String::new(),
                no_token: true,
                stop: false,
                apply_launch: false,
                restart: false,
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(state.extra_remote.lock().unwrap().is_none());
        assert!(!state.is_token_enforced());
    }

    #[tokio::test]
    async fn failed_rebind_keeps_the_open_listener_until_the_same_port_is_released() {
        use axum::extract::State;
        use axum::Json;

        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        state
            .http_router
            .set(axum::Router::new())
            .expect("router slot empty");
        state
            .enforce_token
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.extra_remote.lock().unwrap() = Some(crate::ExtraRemoteBind {
            host: "0.0.0.0".into(),
            port: 4096,
            aborts: Vec::new(),
            token: Some("old-token".into()),
        });

        // 192.0.2.1 is documentation space and is not assigned to this PC,
        // so the bind fails before the existing listener is touched.
        let different_port = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "192.0.2.1".into(),
                port: 4097,
                token: "next-token".into(),
                no_token: false,
                stop: false,
                apply_launch: false,
                restart: false,
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), different_port)
            .await
            .expect("different-port failure returned");
        {
            let guard = state.extra_remote.lock().unwrap();
            let kept = guard.as_ref().expect("previous listener stays open");
            assert_eq!(kept.port, 4096);
            assert_eq!(kept.token.as_deref(), Some("old-token"));
        }
        assert!(state.is_token_enforced());

        let same_port = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "192.0.2.1".into(),
                port: 4096,
                token: "next-token".into(),
                no_token: false,
                stop: false,
                apply_launch: false,
                restart: false,
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), same_port)
            .await
            .expect("same-port failure returned");
        assert!(state.extra_remote.lock().unwrap().is_none());
        assert!(!state.is_token_enforced());
    }

    #[tokio::test]
    async fn launch_pref_applies_token_now_and_keeps_a_new_port_for_the_next_start() {
        use axum::extract::State;
        use axum::Json;

        let home = crate::tests::ScopedChatHome::new();
        let mut state = crate::tests::chat_test_state(&home);
        state.bind_host = "0.0.0.0".into();
        state.bind_port = 13457;
        state.initial_enforce_token = true;
        state.webui_tokens.register("old-token");
        state.webui_tokens.set_display("old-token");
        state
            .enforce_token
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let same_port = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 13457,
                token: "user-token".into(),
                no_token: false,
                stop: false,
                apply_launch: true,
                restart: false,
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), same_port)
            .await
            .expect("same-port token update returned");
        assert_eq!(state.webui_tokens.display().as_deref(), Some("user-token"));
        assert!(state.webui_tokens.is_valid("old-token"));
        assert!(state.webui_tokens.is_valid("user-token"));
        assert!(state.is_token_enforced());
        let saved = std::fs::read_to_string(webui_listen_pref_path()).unwrap();
        assert!(saved.contains("\"port\": 13457"), "{saved}");
        assert!(saved.contains("user-token"), "{saved}");
        assert!(!saved.contains("no_token"), "{saved}");

        let next_port = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 4096,
                token: "later-token".into(),
                no_token: false,
                stop: false,
                apply_launch: true,
                restart: false,
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), next_port)
            .await
            .expect("port change returned");
        assert_eq!(state.bind_port, 13457);
        assert_eq!(state.webui_tokens.display().as_deref(), Some("user-token"));
        let saved = std::fs::read_to_string(webui_listen_pref_path()).unwrap();
        assert!(saved.contains("\"port\": 4096"), "{saved}");
        assert!(saved.contains("later-token"), "{saved}");

        let open_now = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 13457,
                token: String::new(),
                no_token: true,
                stop: false,
                apply_launch: true,
                restart: false,
            }),
        );
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), open_now)
            .await
            .expect("no-token update returned")
            .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(state.is_token_enforced());
        let saved = std::fs::read_to_string(webui_listen_pref_path()).unwrap();
        assert!(saved.contains("\"port\": 4096"), "{saved}");
        assert!(saved.contains("later-token"), "{saved}");
        assert!(!saved.contains("no_token"), "{saved}");
    }

    #[tokio::test]
    async fn loopback_no_token_payload_cannot_strip_auth_off_active_non_loopback_listener() {
        use axum::extract::State;
        use axum::Json;

        let home = crate::tests::ScopedChatHome::new();
        let mut state = crate::tests::chat_test_state(&home);
        state.bind_host = "127.0.0.1".into();
        state.bind_port = 13457;
        state.initial_enforce_token = false;
        state
            .enforce_token
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.extra_remote.lock().unwrap() = Some(crate::ExtraRemoteBind {
            host: "0.0.0.0".into(),
            port: 4096,
            aborts: Vec::new(),
            token: Some("lan-token".into()),
        });

        // Targeting 127.0.0.1 with no_token=true while 0.0.0.0 extra_remote is active must fail 400.
        let resp = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "127.0.0.1".into(),
                port: 13457,
                token: String::new(),
                no_token: true,
                stop: false,
                apply_launch: true,
                restart: false,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(state.is_token_enforced());

        // `stop: true` must still succeed even if the client sends `no_token: true`.
        let stop_resp = post_remote_access(
            State(state.clone()),
            Json(RemoteAccessBody {
                host: "0.0.0.0".into(),
                port: 4096,
                token: String::new(),
                no_token: true,
                stop: true,
                apply_launch: false,
                restart: false,
            }),
        )
        .await
        .into_response();
        assert_eq!(stop_resp.status(), axum::http::StatusCode::OK);
        assert!(state.extra_remote.lock().unwrap().is_none());
        assert!(!state.is_token_enforced());
    }

    #[test]
    fn desktop_settings_write_validates_action_and_creates_bak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-settings.json");

        let bad = write_desktop_settings_at(
            &path,
            &DesktopSettingsPayload {
                close_action: "invalid".into(),
            },
        );
        assert_eq!(bad.unwrap_err().0, axum::http::StatusCode::BAD_REQUEST);

        write_desktop_settings_at(
            &path,
            &DesktopSettingsPayload {
                close_action: "tray".into(),
            },
        )
        .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"tray\""));

        write_desktop_settings_at(
            &path,
            &DesktopSettingsPayload {
                close_action: "quit".into(),
            },
        )
        .unwrap();
        let bak = dir.path().join("desktop-settings.json.bak");
        assert!(bak.is_file());
        assert!(std::fs::read_to_string(&bak).unwrap().contains("\"tray\""));
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"quit\""));
        assert!(write_desktop_settings_at(
            &path,
            &DesktopSettingsPayload {
                close_action: "bogus".into(),
            },
        )
        .is_err());
    }

    #[test]
    fn desktop_managed_mode_rejects_no_token_for_all_non_stop_payloads() {
        assert_eq!(
            validate_desktop_no_token(true, true).unwrap_err().0,
            axum::http::StatusCode::BAD_REQUEST
        );
        assert!(validate_desktop_no_token(false, true).is_ok());
        assert!(validate_desktop_no_token(true, false).is_ok());
    }

    #[tokio::test]
    async fn reload_config_clears_cached_registry_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let reg = jeikcode_coding::session_runtime_registry::SessionRuntimeRegistry::global();
        let sid = format!("test-reload-fp-{}", uuid::Uuid::new_v4().simple());
        let _ = reg.open_or_attach(sid.clone(), dir.path().to_path_buf());
        reg.set_provider_fingerprint(&sid, Some("stale-fp".into()));
        assert_eq!(reg.provider_fingerprint(&sid).as_deref(), Some("stale-fp"));

        let mut cfg = Config::default();
        cfg.default_model = Some("m".into());
        cfg.models.insert(
            "m".into(),
            jeikcode_config::config::provider::ModelProfileConfig {
                account: "acc".into(),
                model: "m".into(),
                display_name: None,
                system_prompt: None,
                context_window: 1_000_000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                reasoning_levels: None,
                thinking_enabled: None,
                thinking_budget: None,
                capable_model: None,
                pricing: None,
                supports_vision: None,
                reasoning_model: None,
            },
        );
        cfg.provider_accounts.insert(
            "acc".into(),
            jeikcode_config::config::provider::ProviderAccountConfig {
                provider: "openai".into(),
                display_name: None,
                api_key: Some("k".into()),
                base_url: Some("https://example.invalid/v1".into()),
                user_agent: None,
                skip_tls_verify: false,
                enterprise_url: None,
                ephemeral: false,
            },
        );
        cfg.coding.compact_threshold = 0.5;
        let mut agent_cfg = crate::kernel_runtime::coding_config_from_runtime(
            &jeikcode_coding::CodingRuntimeConfig::from_config(
                &cfg,
                dir.path(),
                None,
                None,
                true,
                false,
            ),
        );
        agent_cfg.compact_threshold = 0.35;
        let prepare = jeikcode_coding::PrepareOptions {
            request_user_input: false,
            session: jeikcode_coding::SessionMode::Disabled,
            skill_dirs: None,
            plugin_skill_dirs: Vec::new(),
            mcp: false,
            shared_mcp_registry: None,
            memory: false,
            web: false,
            review: false,
            rate_limit_source: None,
        };
        let start = jeikcode_coding::CodingRuntimeStart {
            agent: agent_cfg,
            prepare,
            provider_factory: crate::runtime_host::coding_provider_factory(),
            plugin_hooks: crate::runtime_host::installed_plugin_hook_source(),
            image_preprocessor: None,
        };
        let runtime = jeikcode_coding::CodingRuntime::start_with_bootstrap(
            start,
            jeikcode_coding::ProviderBootstrap::Required,
        )
        .await
        .expect("start test runtime");
        assert!(reg.bind_handle(&sid, runtime.handle.clone(), None));

        let reloaded = super::invalidate_and_reload_idle_registry_handles(None, &cfg).await;
        assert_eq!(reloaded, 1);
        let expected_fp = crate::native_live::provider_fingerprint(&cfg, "m").unwrap();
        assert_eq!(
            reg.provider_fingerprint(&sid).as_deref(),
            Some(expected_fp.as_str())
        );

        let _ = runtime.handle.shutdown().await;
        let _ = reg.force_remove(&sid);
    }

    #[test]
    fn firewall_launch_uses_encoded_command_without_temp_ps1() {
        let launch =
            build_firewall_elevated_launch_command(r"C:\Program Files\Jeik's\jeikcode.exe");
        assert!(launch.contains("-EncodedCommand"));
        assert!(!launch.contains(".ps1"));
        assert!(!launch.contains("ExecutionPolicy"));

        let b64 = launch
            .split("'-EncodedCommand','")
            .nth(1)
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("encoded command argument");
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        assert_eq!(raw.len() % 2, 0);
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let decoded = String::from_utf16(&units).expect("valid utf-16le");
        assert!(decoded.contains("program='C:\\Program Files\\Jeik''s\\jeikcode.exe'"));
        assert!(decoded.contains("netsh advfirewall firewall delete rule name=JeikCode"));
    }

    #[test]
    fn command_drain_handles_large_stdout_and_stderr_without_pipe_deadlock() {
        #[cfg(windows)]
        let cmd = {
            let mut c = std::process::Command::new("cmd");
            c.args([
                "/C",
                "for /L %i in (1,1,2000) do @(echo 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 & echo 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 1>&2)",
            ]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new("sh");
            c.args([
                "-c",
                "i=0; while [ $i -lt 2000 ]; do echo 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789; echo 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 >&2; i=$((i+1)); done",
            ]);
            c
        };
        let out = run_command_with_timeout_drain(cmd, std::time::Duration::from_secs(5))
            .expect("large dual-pipe child must not deadlock");
        assert!(out.success);
        assert!(
            out.stdout.len() > 100_000,
            "stdout len={}",
            out.stdout.len()
        );
        assert!(
            out.stderr.len() > 100_000,
            "stderr len={}",
            out.stderr.len()
        );
    }

    #[tokio::test]
    async fn remote_status_async_matches_sync_snapshot() {
        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        let sync_status = remote_status(&state, Some("tok-1".into()));
        let async_status = remote_status_async(&state, Some("tok-1".into()))
            .await
            .expect("remote_status_async");
        assert_eq!(async_status.host, sync_status.host);
        assert_eq!(async_status.port, sync_status.port);
        assert_eq!(async_status.active, sync_status.active);
        assert_eq!(async_status.token, sync_status.token);
    }

    #[tokio::test]
    async fn concurrent_post_remote_access_serializes_stop_and_bind() {
        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        let _ = state.http_router.set(axum::Router::new());

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let enable = RemoteAccessBody {
            host: "127.0.0.1".into(),
            port,
            token: "tok-concurrent".into(),
            no_token: false,
            stop: false,
            apply_launch: false,
            restart: false,
        };
        let stop = RemoteAccessBody {
            host: String::new(),
            port: 0,
            token: String::new(),
            no_token: false,
            stop: true,
            apply_launch: false,
            restart: false,
        };

        let (r1, r2) = tokio::join!(
            post_remote_access(axum::extract::State(state.clone()), axum::Json(enable)),
            post_remote_access(axum::extract::State(state.clone()), axum::Json(stop)),
        );
        assert_eq!(r1.into_response().status(), axum::http::StatusCode::OK);
        assert_eq!(r2.into_response().status(), axum::http::StatusCode::OK);
        if let Some(prev) = take_extra_remote(&state) {
            prev.abort_all();
        }
    }
}
