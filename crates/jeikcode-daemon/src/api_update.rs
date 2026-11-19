use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::AppState;
use jeikcode_coding::config_sync::{
    apply_selected_diffs_checked, get_last_seen_version, scan_jeikcode_config_diffs,
    set_last_seen_version, ConfigDiffItem,
};
use jeikcode_config::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Beta,
}

impl Default for UpdateChannel {
    fn default() -> Self {
        if env!("CARGO_PKG_VERSION").contains('-') {
            UpdateChannel::Beta
        } else {
            UpdateChannel::Stable
        }
    }
}

impl UpdateChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpdateChannel::Stable => "stable",
            UpdateChannel::Beta => "beta",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct UpdateCheckQuery {
    #[serde(default)]
    pub channel: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ExecuteUpdateRequest {
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub download_url: Option<String>,
    /// Reinstall even when current already matches latest (stability testing).
    #[serde(default)]
    pub force: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateVersionEntry {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateCheckResponse {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub is_desktop: bool,
    pub release_notes: Option<String>,
    pub download_url: Option<String>,
    pub released_at: Option<String>,
    pub channel: String,
    /// Channel history, newest first. `versions[0]` is `latest_version`.
    #[serde(default)]
    pub versions: Vec<UpdateVersionEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// "idle" | "downloading" | "ready" | "installing" | "done" | "error"
    pub status: String,
    pub progress: u32,
    pub bytes: u64,
    pub total: u64,
    pub error: Option<String>,
    /// Stable `UpgradeFailureKind` code. The WebUI localizes
    /// `update.error.<code>` and keeps `error` as the raw diagnostic detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

static CURRENT_UPDATE_STATUS: Mutex<Option<UpdateStatus>> = Mutex::new(None);
static IS_UPDATING: AtomicBool = AtomicBool::new(false);

fn get_update_status() -> UpdateStatus {
    CURRENT_UPDATE_STATUS
        .lock()
        .unwrap()
        .clone()
        .unwrap_or(UpdateStatus {
            status: "idle".to_string(),
            progress: 0,
            bytes: 0,
            total: 0,
            error: None,
            error_code: None,
        })
}

fn set_update_status(status: UpdateStatus) {
    *CURRENT_UPDATE_STATUS.lock().unwrap() = Some(status);
}

/// 语义化版本号比较器（共享 `jeikcode_updater::version_is_newer` 实现）
pub fn compare_versions(latest: &str, current: &str) -> bool {
    jeikcode_updater::version_is_newer(latest, current)
}

/// 兼容老接口
pub fn is_newer_version(latest: &str, current: &str) -> bool {
    compare_versions(latest, current)
}

/// 探测当前是否在桌面端环境
pub fn is_desktop_environment() -> bool {
    jeikcode_updater::is_desktop_managed()
        || std::env::var_os(jeikcode_config::desktop_shared::DESKTOP_ENV).is_some()
}

const GITHUB_LATEST_API_URL: &str = "https://api.github.com/repos/jeikl/JeikCode/releases/latest";
const DEFAULT_RELEASE_DOWNLOAD_BASE: &str = "https://github.com/jeikl/JeikCode/releases/download";

#[derive(Debug, Deserialize, Clone)]
struct GitHubRelease {
    tag_name: String,
    #[allow(dead_code)]
    html_url: String,
    body: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Deserialize, Clone)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
}

fn create_update_client() -> Option<reqwest::Client> {
    jeikcode_updater::apply_proxy_policy(reqwest::Client::builder())
        .user_agent(concat!("jeikcode/", env!("CARGO_PKG_VERSION")))
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
            );
            headers
        })
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .ok()
}

fn normalized_tag(version: &str) -> String {
    let trimmed = version.trim();
    if trimmed.starts_with('v') || trimmed.starts_with('V') {
        format!(
            "v{}",
            trimmed.trim_start_matches('v').trim_start_matches('V')
        )
    } else {
        format!("v{trimmed}")
    }
}

fn effective_download_base() -> String {
    jeikcode_updater::download_base()
        .trim_end_matches('/')
        .to_string()
}

fn resolve_release_asset_url(base: &str, tag: &str, asset_name: &str, default_url: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(DEFAULT_RELEASE_DOWNLOAD_BASE) {
        let norm_tag = normalized_tag(tag);
        format!("{trimmed}/{norm_tag}/{asset_name}")
    } else {
        default_url.to_string()
    }
}

fn desktop_installer_filename(version: &str) -> Option<String> {
    let tag = normalized_tag(version);
    let clean_ver = tag.trim_start_matches('v').trim_start_matches('V');
    #[cfg(target_os = "windows")]
    {
        Some(format!("JeikCode.Desktop_{clean_ver}_x64-setup.exe"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(format!("JeikCode.Desktop_{clean_ver}_aarch64.dmg"))
    }
    #[cfg(target_os = "linux")]
    {
        Some(format!("JeikCode.Desktop_{clean_ver}_amd64.AppImage"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = clean_ver;
        None
    }
}

/// 根据指定的更新通道，在 Release 列表中寻找语义化版本号最大的目标 Release，支持跨版本直升最高版
#[cfg(test)]
fn find_highest_release(
    releases: &[GitHubRelease],
    channel: UpdateChannel,
) -> Option<GitHubRelease> {
    let mut highest: Option<&GitHubRelease> = None;

    for r in releases {
        let is_pre = r.prerelease || r.tag_name.contains('-');
        match channel {
            UpdateChannel::Stable if is_pre => continue,
            UpdateChannel::Beta if !is_pre => continue,
            _ => {}
        }

        match highest {
            None => highest = Some(r),
            Some(curr) => {
                // 如果当前 release 的版本号高于已记录的最大版本，则更新为当前 release
                if compare_versions(&r.tag_name, &curr.tag_name) {
                    highest = Some(r);
                }
            }
        }
    }

    highest.cloned()
}

fn find_desktop_installer_url_in_assets(
    assets: &[GitHubReleaseAsset],
    tag: &str,
    download_base: &str,
) -> Option<String> {
    #[cfg(target_os = "windows")]
    let predicate = |name: &str| -> bool {
        let lower = name.to_lowercase();
        lower.ends_with(".exe") && (lower.contains("setup") || lower.contains("desktop"))
    };
    #[cfg(target_os = "macos")]
    let predicate = |name: &str| -> bool { name.to_lowercase().ends_with(".dmg") };
    #[cfg(target_os = "linux")]
    let predicate = |name: &str| -> bool {
        let lower = name.to_lowercase();
        lower.ends_with(".appimage") || lower.ends_with(".deb")
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let predicate = |_: &str| false;

    assets
        .iter()
        .find(|a| predicate(&a.name))
        .map(|a| resolve_release_asset_url(download_base, tag, &a.name, &a.browser_download_url))
}

fn find_cli_asset_url(
    assets: &[GitHubReleaseAsset],
    tag: &str,
    download_base: &str,
) -> Option<String> {
    if let Some(target) = jeikcode_updater::detect_target() {
        let expected_name = format!("jeikcode-{tag}-{target}");
        let expected_exe = format!("{expected_name}.exe");
        if let Some(a) = assets
            .iter()
            .find(|a| a.name == expected_name || a.name == expected_exe)
        {
            return Some(resolve_release_asset_url(
                download_base,
                tag,
                &a.name,
                &a.browser_download_url,
            ));
        }
    }
    None
}

fn resolve_desktop_installer_url_fallback_with_base(
    version: &str,
    download_base: &str,
) -> Option<String> {
    let tag = normalized_tag(version);
    let filename = desktop_installer_filename(&tag)?;
    let base = download_base.trim_end_matches('/');
    let effective = if base.is_empty() {
        DEFAULT_RELEASE_DOWNLOAD_BASE
    } else {
        base
    };
    Some(format!("{effective}/{tag}/{filename}"))
}

fn resolve_desktop_installer_url_fallback(version: &str) -> Option<String> {
    resolve_desktop_installer_url_fallback_with_base(version, &effective_download_base())
}

/// 解析桌面安装包候选下载地址（优先走用户配置的更新镜像，再退回 GitHub Release API）
async fn resolve_desktop_installer_url(version: &str) -> Option<String> {
    let tag = normalized_tag(version);
    let base = effective_download_base();
    let client = create_update_client();

    if !base.is_empty() && !base.eq_ignore_ascii_case(DEFAULT_RELEASE_DOWNLOAD_BASE) {
        if let (Some(ref c), Some(mirror_url)) = (
            &client,
            resolve_desktop_installer_url_fallback_with_base(&tag, &base),
        ) {
            if let Ok(resp) = c.head(&mirror_url).send().await {
                if resp.status().is_success() {
                    return Some(mirror_url);
                }
            }
        }
    }

    if let Some(ref c) = client {
        let api_url = format!("https://api.github.com/repos/jeikl/JeikCode/releases/tags/{tag}");
        if let Ok(resp) = c.get(&api_url).send().await {
            if resp.status().is_success() {
                if let Ok(rel) = resp.json::<GitHubRelease>().await {
                    if let Some(url) =
                        find_desktop_installer_url_in_assets(&rel.assets, &tag, &base)
                    {
                        return Some(url);
                    }
                }
            }
        }
    }

    resolve_desktop_installer_url_fallback_with_base(&tag, &base)
}

/// GET /api/update/check
pub async fn check_update(Query(query): Query<UpdateCheckQuery>) -> impl IntoResponse {
    let current_version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let is_desktop = is_desktop_environment();
    let channel = match query.channel.as_deref() {
        Some("beta") => UpdateChannel::Beta,
        Some("stable") => UpdateChannel::Stable,
        _ => UpdateChannel::default(),
    };

    // 离线/内网隔离模式下，直接返回当前版本且无新版，绝不产生外网探测请求
    if jeikcode_config::config::offline::is_offline_active()
        || jeikcode_config::config::runtime_state::is_dead()
    {
        return Json(UpdateCheckResponse {
            current_version: current_version.clone(),
            latest_version: current_version,
            has_update: false,
            is_desktop,
            release_notes: None,
            download_url: None,
            released_at: None,
            channel: channel.as_str().to_string(),
            versions: Vec::new(),
        });
    }

    let client = create_update_client();

    let mut latest_version = String::new();
    let mut release_notes = None;
    let mut released_at = None;
    let mut download_url = None;
    let mut versions: Vec<UpdateVersionEntry> = Vec::new();

    let updater_channel = match channel {
        UpdateChannel::Beta => jeikcode_updater::UpdateChannel::Beta,
        UpdateChannel::Stable => jeikcode_updater::UpdateChannel::Stable,
    };

    // 边缘缓存与 GitHub 分页合并后，按通道倒序给出全部历史版本。
    if let Ok(releases) = jeikcode_updater::list_releases().await {
        let matched = jeikcode_updater::filter_channel_releases(&releases, updater_channel);
        versions = matched
            .iter()
            .map(|r| UpdateVersionEntry {
                version: r.tag_name.clone(),
                notes: r.body.clone().filter(|s| !s.trim().is_empty()),
                released_at: r.published_at.clone(),
            })
            .collect();
        if let Some(rel) = matched.first() {
            latest_version = rel.tag_name.clone();
            release_notes = rel.body.clone();
            released_at = rel.published_at.clone();
            let assets: Vec<GitHubReleaseAsset> = rel
                .assets
                .iter()
                .map(|a| GitHubReleaseAsset {
                    name: a.name.clone(),
                    browser_download_url: a.browser_download_url.clone(),
                })
                .collect();
            let dl_base = effective_download_base();
            if is_desktop {
                download_url =
                    find_desktop_installer_url_in_assets(&assets, &latest_version, &dl_base)
                        .or_else(|| resolve_desktop_installer_url_fallback(&latest_version));
            } else {
                download_url =
                    find_cli_asset_url(&assets, &latest_version, &dl_base).or_else(|| {
                        jeikcode_updater::detect_target()
                            .map(|target| jeikcode_updater::binary_url(&latest_version, target))
                    });
            }
        }
    }

    if latest_version.is_empty() {
        // 后备方案（发布列表不可用时退回从官方 latest.json 探测）
        if let Ok(m) = jeikcode_updater::fetch_manifest().await {
            latest_version = m.version;
            released_at = m.released_at;
            if is_desktop {
                download_url = resolve_desktop_installer_url(&latest_version).await;
            } else if let Some(target) = jeikcode_updater::detect_target() {
                download_url = Some(jeikcode_updater::binary_url(&latest_version, target));
            }
        } else if let Some(ref c) = client {
            if let Ok(resp) = c.get(GITHUB_LATEST_API_URL).send().await {
                if resp.status().is_success() {
                    if let Ok(rel) = resp.json::<GitHubRelease>().await {
                        latest_version = rel.tag_name.clone();
                        release_notes = rel.body;
                        released_at = rel.published_at;
                        let dl_base = effective_download_base();
                        if is_desktop {
                            download_url = find_desktop_installer_url_in_assets(
                                &rel.assets,
                                &latest_version,
                                &dl_base,
                            )
                            .or_else(|| resolve_desktop_installer_url_fallback(&latest_version));
                        } else {
                            download_url =
                                find_cli_asset_url(&rel.assets, &latest_version, &dl_base).or_else(
                                    || {
                                        jeikcode_updater::detect_target().map(|target| {
                                            jeikcode_updater::binary_url(&latest_version, target)
                                        })
                                    },
                                );
                        }
                    }
                }
            }
        }
        if !latest_version.is_empty() {
            versions = vec![UpdateVersionEntry {
                version: latest_version.clone(),
                notes: release_notes.clone().filter(|s| !s.trim().is_empty()),
                released_at: released_at.clone(),
            }];
        }
    }

    let has_update =
        !latest_version.is_empty() && compare_versions(&latest_version, &current_version);

    Json(UpdateCheckResponse {
        current_version,
        latest_version,
        has_update,
        is_desktop,
        release_notes,
        download_url,
        released_at,
        channel: channel.as_str().to_string(),
        versions,
    })
}

/// GET /api/update/status
pub async fn get_status() -> impl IntoResponse {
    Json(get_update_status())
}

/// POST /api/update/execute
pub async fn execute_update(
    State(state): State<AppState>,
    payload: Option<Json<ExecuteUpdateRequest>>,
) -> impl IntoResponse {
    if jeikcode_config::config::runtime_state::is_dead() {
        return Json(serde_json::json!({
            "success": false,
            "message": "updates are disabled"
        }));
    }
    if IS_UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Json(serde_json::json!({
            "success": false,
            "message": "更新任务正在执行中，请勿重复发起"
        }));
    }

    let req = payload.map(|Json(p)| p).unwrap_or_default();
    let listen_host = state.bind_host.clone();
    let listen_port = state.bind_port;
    let restart_token = state.webui_tokens.display();
    let shutdown_tx = Some(state.shutdown_tx.clone());
    let no_token = state
        .token_optional
        .load(std::sync::atomic::Ordering::Relaxed)
        || !state
            .enforce_token
            .load(std::sync::atomic::Ordering::Relaxed);

    tokio::spawn(async move {
        set_update_status(UpdateStatus {
            status: "downloading".to_string(),
            progress: 0,
            bytes: 0,
            total: 0,
            error: None,
            error_code: None,
        });

        // WebUI is embedded in this binary. Remote browsers and the desktop
        // shell all hit this process — never open a host-side installer wizard.
        // Swap the running jeikcode in place, then restart so the new UI is served.
        if let Err(e) = run_cli_update(
            req.version,
            req.channel,
            req.download_url,
            req.force.unwrap_or(false),
            Some(listen_host),
            listen_port,
            restart_token,
            no_token,
            shutdown_tx,
        )
        .await
        {
            set_update_status(UpdateStatus {
                status: "error".to_string(),
                progress: 0,
                bytes: 0,
                total: 0,
                error: Some(format!("更新二进制失败: {e}")),
                error_code: Some(
                    jeikcode_updater::UpgradeFailureKind::Other
                        .as_str()
                        .to_string(),
                ),
            });
            IS_UPDATING.store(false, Ordering::SeqCst);
        }
    });

    Json(serde_json::json!({
        "success": true,
        "message": "已开始更新流程"
    }))
}

async fn run_cli_update(
    version_opt: Option<String>,
    channel_opt: Option<String>,
    _direct_url: Option<String>,
    force: bool,
    listen_host: Option<String>,
    listen_port: u16,
    restart_token: Option<String>,
    no_token: bool,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
) -> Result<()> {
    let current_version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<jeikcode_updater::UpgradeEvent>();
    let channel = match channel_opt.as_deref() {
        Some("beta") => jeikcode_updater::UpdateChannel::Beta,
        _ => jeikcode_updater::UpdateChannel::Stable,
    };

    let driver = if let Some(ver) = version_opt.filter(|v| !v.trim().is_empty()) {
        let tag = if ver.starts_with('v') || ver.starts_with('V') {
            ver
        } else {
            format!("v{ver}")
        };
        // 走 updater 的 tag 清单解析，避免把下载源硬编码成 github.com：
        // 用户配置的 JEIKCODE_UPDATE_DOWNLOAD_BASE / [config] update_download_base
        // 镜像地址必须在这条路径上同样生效。
        match jeikcode_updater::fetch_manifest_for_tag(&tag).await {
            Ok(manifest) => tokio::spawn(jeikcode_updater::run_upgrade_with_manifest(
                manifest,
                current_version,
                force,
                tx,
            )),
            Err(_) => tokio::spawn(jeikcode_updater::run_upgrade_for_channel(
                channel,
                current_version,
                force,
                tx,
            )),
        }
    } else {
        tokio::spawn(jeikcode_updater::run_upgrade_for_channel(
            channel,
            current_version,
            force,
            tx,
        ))
    };

    while let Some(ev) = rx.recv().await {
        match ev {
            jeikcode_updater::UpgradeEvent::Downloading { bytes, total } => {
                let pct = if total > 0 {
                    ((bytes * 100) / total) as u32
                } else {
                    0
                };
                set_update_status(UpdateStatus {
                    status: "downloading".to_string(),
                    progress: pct.min(100),
                    bytes,
                    total,
                    error: None,
                    error_code: None,
                });
            }
            jeikcode_updater::UpgradeEvent::Verifying
            | jeikcode_updater::UpgradeEvent::Replacing => {
                set_update_status(UpdateStatus {
                    status: "installing".to_string(),
                    progress: 99,
                    bytes: 0,
                    total: 0,
                    error: None,
                    error_code: None,
                });
            }
            jeikcode_updater::UpgradeEvent::Done { exe, .. } => {
                set_update_status(UpdateStatus {
                    status: "done".to_string(),
                    progress: 100,
                    bytes: 0,
                    total: 0,
                    error: None,
                    error_code: None,
                });
                // File is already swapped in place. Restart so remote WebUI /
                // serve picks up the new image (Unix exec; Windows delayed spawn).
                let restart_token = restart_token.clone();
                let restart_host = listen_host.clone();
                if listen_port > 0 {
                    let _ = crate::api_config::save_webui_listen_pref(
                        restart_host.clone(),
                        listen_port,
                        restart_token.clone(),
                    );
                }
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    // 在调用 restart 前，主动触发优雅停机释放当前的监听套接字（Listen Socket）
                    if let Some(tx) = shutdown_tx {
                        let _ = tx.send(true);
                    }
                    crate::stop_server();
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                    let Err(err) = jeikcode_updater::restart_process_with_config(
                        &exe,
                        listen_port,
                        restart_host.as_deref(),
                        listen_port,
                        restart_token.as_deref(),
                        Some(no_token),
                    );
                    tracing::error!("in-place upgrade restart failed: {err}");
                    set_update_status(UpdateStatus {
                        status: "error".to_string(),
                        progress: 0,
                        bytes: 0,
                        total: 0,
                        error: Some(format!("重启失败: {err}")),
                        error_code: Some(
                            jeikcode_updater::UpgradeFailureKind::Restart
                                .as_str()
                                .to_string(),
                        ),
                    });
                    IS_UPDATING.store(false, Ordering::SeqCst);
                });
            }
            jeikcode_updater::UpgradeEvent::Failed(err) => {
                set_update_status(UpdateStatus {
                    status: "error".to_string(),
                    progress: 0,
                    bytes: 0,
                    total: 0,
                    error: Some(err),
                    error_code: None,
                });
            }
            _ => {}
        }
    }

    // 进度事件只描述 “正在做什么”；真正的失败通过 JoinHandle 的 Result 返回。
    // 这里必须把它落成 error 终态：若丢弃 Result，清单拉取或下载失败后
    // WebUI 轮询到的状态会永远停在 “downloading 0%”，既看不到原因也无法重试。
    let outcome = match driver.await {
        Ok(result) => result,
        Err(join_err) => Err(jeikcode_updater::UpgradeFailure::new(
            jeikcode_updater::UpgradeFailureKind::Other,
            anyhow::anyhow!("update task terminated before reporting a result: {join_err}"),
        )),
    };
    IS_UPDATING.store(false, Ordering::SeqCst);
    if let Err(failure) = outcome {
        set_update_status(UpdateStatus {
            status: "error".to_string(),
            progress: 0,
            bytes: 0,
            total: 0,
            error: Some(format!("{:#}", failure.error)),
            error_code: Some(failure.kind.as_str().to_string()),
        });
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 配置更新多选覆盖 API
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct UpgradeDiffsQuery {
    pub first_launch: Option<bool>,
    pub force: Option<bool>,
}

#[derive(Serialize)]
pub struct UpgradeDiffsResponse {
    pub should_prompt: bool,
    pub current_version: String,
    pub last_seen_version: Option<String>,
    pub diffs: Vec<ConfigDiffItem>,
}

/// GET /api/config/upgrade-diffs
pub async fn get_upgrade_diffs(
    Query(query): Query<UpgradeDiffsQuery>,
) -> Json<UpgradeDiffsResponse> {
    let home = Config::config_dir();
    let current_ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let last_seen = get_last_seen_version(&home);

    if jeikcode_config::config::runtime_state::is_dead() {
        return Json(UpgradeDiffsResponse {
            should_prompt: false,
            current_version: current_ver,
            last_seen_version: last_seen,
            diffs: Vec::new(),
        });
    }

    let diffs = scan_jeikcode_config_diffs(&home);

    if diffs.is_empty() {
        // 无差异，自动对齐版本标记
        let _ = set_last_seen_version(&home, &current_ver);
        return Json(UpgradeDiffsResponse {
            should_prompt: false,
            current_version: current_ver,
            last_seen_version: last_seen,
            diffs: Vec::new(),
        });
    }

    // Never auto-prompt an overwrite modal on first browser/WebUI load;
    // only return `should_prompt: true` when explicitly requested via `force=true`.
    let _ = query.first_launch;
    let should_prompt = query.force.unwrap_or(false) && !diffs.is_empty();

    Json(UpgradeDiffsResponse {
        should_prompt,
        current_version: current_ver,
        last_seen_version: last_seen,
        diffs,
    })
}

#[derive(Debug, Deserialize)]
pub struct ApplyDiffsRequest {
    pub selected_paths: Vec<String>,
}

pub(crate) fn apply_diffs_at(
    home: &Path,
    current_ver: &str,
    selected_paths: &[String],
) -> anyhow::Result<usize> {
    let diffs = scan_jeikcode_config_diffs(home);
    let selected_items: Vec<ConfigDiffItem> = diffs
        .into_iter()
        .map(|mut it| {
            it.selected = selected_paths.contains(&it.relative_path);
            it
        })
        .collect();

    let applied_count = apply_selected_diffs_checked(selected_items)?;
    let _ = set_last_seen_version(home, current_ver);
    Ok(applied_count)
}

/// POST /api/config/apply-diffs
pub async fn apply_diffs(Json(req): Json<ApplyDiffsRequest>) -> impl IntoResponse {
    let home = Config::config_dir();
    let current_ver = format!("v{}", env!("CARGO_PKG_VERSION"));

    if jeikcode_config::config::runtime_state::is_dead() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "applied_count": 0,
            })),
        )
            .into_response();
    }

    match apply_diffs_at(&home, &current_ver, &req.selected_paths) {
        Ok(applied_count) => {
            if applied_count > 0 {
                crate::publish_global(crate::GlobalWireEvent::ConfigReloaded);
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "applied_count": applied_count,
                })),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": err.to_string(),
            })),
        )
            .into_response(),
    }
}

/// POST /api/config/dismiss-diffs
pub async fn dismiss_diffs() -> impl IntoResponse {
    let home = Config::config_dir();
    let current_ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let _ = set_last_seen_version(&home, &current_ver);

    Json(serde_json::json!({
        "success": true
    }))
}

/// 注册更新与配置同步路由
pub fn register_update_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/update/check", get(check_update))
        .route("/api/update/status", get(get_status))
        .route("/api/update/execute", post(execute_update))
        .route("/api/config/upgrade-diffs", get(get_upgrade_diffs))
        .route("/api/config/apply-diffs", post(apply_diffs))
        .route("/api/config/dismiss-diffs", post(dismiss_diffs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_versions() {
        assert!(compare_versions("v7.1.30", "v7.1.7"));
        assert!(compare_versions("7.2.0", "7.1.99"));
        assert!(compare_versions("v8.0.0", "v7.9.9"));
        assert!(!compare_versions("v7.1.7", "v7.1.7"));
        assert!(!compare_versions("v7.1.6", "v7.1.7"));
        assert!(!compare_versions("7.1.7", "7.1.7"));
        assert!(compare_versions("7.1.7-beta.2", "7.1.6"));

        // Pre-release 语义比较测试（与 Antigravity-Manager 完全对齐）
        assert!(compare_versions("7.1.50-beta.2", "7.1.50-beta.1"));
        assert!(!compare_versions("7.1.50-beta.1", "7.1.50-beta.2"));
        assert!(compare_versions("7.1.50", "7.1.50-beta.2")); // 正式版 > 预发布版
        assert!(!compare_versions("7.1.50-beta.2", "7.1.50"));
        assert!(compare_versions("7.1.51-beta.1", "7.1.50")); // 更高主版本的预发布 > 低版本正式版
        assert!(!compare_versions("7.1.50-beta.1", "7.1.50-beta.1"));
    }

    #[test]
    fn test_find_highest_release_cross_version() {
        let releases = vec![
            GitHubRelease {
                tag_name: "v7.1.53-beta.1".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: true,
                assets: vec![],
            },
            GitHubRelease {
                tag_name: "v7.1.53-beta.3".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: true,
                assets: vec![],
            },
            GitHubRelease {
                tag_name: "v7.1.53-beta.2".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: true,
                assets: vec![],
            },
            GitHubRelease {
                tag_name: "v7.1.52".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: false,
                assets: vec![],
            },
        ];

        // 1. Beta 通道：应当在所有候选版本中直接跨版本挑出最高的 v7.1.53-beta.3
        let highest_beta = find_highest_release(&releases, UpdateChannel::Beta);
        assert_eq!(highest_beta.unwrap().tag_name, "v7.1.53-beta.3");

        // 2. Stable 通道：应当排除预发布版，准确挑出最高的正式版 v7.1.52
        let highest_stable = find_highest_release(&releases, UpdateChannel::Stable);
        assert_eq!(highest_stable.unwrap().tag_name, "v7.1.52");
    }

    #[test]
    fn test_find_highest_release_beta_ignores_newer_stable() {
        let releases = vec![
            GitHubRelease {
                tag_name: "v7.2.4".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: false,
                assets: vec![],
            },
            GitHubRelease {
                tag_name: "v7.2.4-beta.5".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: true,
                assets: vec![],
            },
            GitHubRelease {
                tag_name: "v7.2.4-beta.4".to_string(),
                html_url: "".to_string(),
                body: None,
                published_at: None,
                prerelease: true,
                assets: vec![],
            },
        ];
        let highest_beta = find_highest_release(&releases, UpdateChannel::Beta);
        assert_eq!(highest_beta.unwrap().tag_name, "v7.2.4-beta.5");
        let highest_stable = find_highest_release(&releases, UpdateChannel::Stable);
        assert_eq!(highest_stable.unwrap().tag_name, "v7.2.4");
    }

    #[tokio::test]
    async fn upgrade_diffs_never_auto_prompts_without_force() {
        let resp_default = get_upgrade_diffs(Query(UpgradeDiffsQuery::default()))
            .await
            .0;
        assert!(!resp_default.should_prompt);

        let resp_first = get_upgrade_diffs(Query(UpgradeDiffsQuery {
            first_launch: Some(true),
            force: Some(false),
        }))
        .await
        .0;
        assert!(!resp_first.should_prompt);
    }

    #[test]
    fn mirror_download_base_rewrites_release_asset_and_fallback_urls() {
        let custom = "https://mirror.example.com/jeikcode/releases/download/";
        assert_eq!(
            resolve_release_asset_url(
                custom,
                "v7.5.1",
                "JeikCode.Desktop_7.5.1_x64-setup.exe",
                "https://github.com/jeikl/JeikCode/releases/download/v7.5.1/JeikCode.Desktop_7.5.1_x64-setup.exe",
            ),
            "https://mirror.example.com/jeikcode/releases/download/v7.5.1/JeikCode.Desktop_7.5.1_x64-setup.exe"
        );
        assert_eq!(
            resolve_release_asset_url(
                DEFAULT_RELEASE_DOWNLOAD_BASE,
                "v7.5.1",
                "JeikCode.Desktop_7.5.1_x64-setup.exe",
                "https://github.com/jeikl/JeikCode/releases/download/v7.5.1/JeikCode.Desktop_7.5.1_x64-setup.exe",
            ),
            "https://github.com/jeikl/JeikCode/releases/download/v7.5.1/JeikCode.Desktop_7.5.1_x64-setup.exe"
        );
        if let Some(url) = resolve_desktop_installer_url_fallback_with_base("7.5.1", custom) {
            assert!(
                url.starts_with("https://mirror.example.com/jeikcode/releases/download/v7.5.1/JeikCode.Desktop_7.5.1_")
            );
        }
    }

    #[test]
    fn apply_diffs_at_creates_bak_and_preserves_coding_and_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let orig_toml = "default_model = \"gpt-5.1\"\n[coding]\ncompact_threshold = 0.35\n";
        let orig_mcp = r#"{"mcpServers":{"context7":{"command":"npx","args":[]}}}"#;
        std::fs::write(home.join("config.toml"), orig_toml).unwrap();
        std::fs::write(home.join("mcp.json"), orig_mcp).unwrap();

        let count = apply_diffs_at(
            home,
            "v7.5.1",
            &["config.toml".to_string(), "mcp.json".to_string()],
        )
        .unwrap();
        assert!(count >= 1);
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml.bak")).unwrap(),
            orig_toml
        );
        let new_toml = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed_toml = Config::load(&home.join("config.toml")).unwrap();
        assert!(new_toml.contains("compact_threshold = 0.35"));
        assert_eq!(parsed_toml.coding.compact_threshold, 0.35);
        let parsed_mcp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("mcp.json")).unwrap()).unwrap();
        assert!(parsed_mcp["mcpServers"].get("context7").is_some());
    }
}
