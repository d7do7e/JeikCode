//! Git repository inspection and branch management API for WebUI.
//!
//! Provides endpoints to:
//! - List local and remote branches + detect active branch (`GET /git/branches`)
//! - Query commit history graph with DAG parents and ref badges (`GET /git/graph`)
//! - Switch / checkout branches safely (`POST /git/checkout`)

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{json_error, normalize_dir_arg, AppState};

#[derive(Clone)]
struct ReposCacheEntry {
    repos: Vec<GitRepoInfo>,
    timestamp: Instant,
}

#[derive(Clone)]
struct GraphCacheEntry {
    head_commit: String,
    fetch_head_commit: Option<String>,
    current_branch: Option<String>,
    branch_param: Option<String>,
    limit: usize,
    response: GitGraphResponse,
}

#[derive(Clone)]
struct RepoMetaCacheEntry {
    repo_root: Option<String>,
    remote_url: Option<String>,
    timestamp: Instant,
}

static REPOS_CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, ReposCacheEntry>>> =
    OnceLock::new();
static GRAPH_CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, GraphCacheEntry>>> =
    OnceLock::new();
static REPO_META_CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, RepoMetaCacheEntry>>> =
    OnceLock::new();

fn get_repos_cache() -> &'static Mutex<std::collections::HashMap<PathBuf, ReposCacheEntry>> {
    REPOS_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn get_graph_cache() -> &'static Mutex<std::collections::HashMap<PathBuf, GraphCacheEntry>> {
    GRAPH_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn get_repo_meta_cache() -> &'static Mutex<std::collections::HashMap<PathBuf, RepoMetaCacheEntry>> {
    REPO_META_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

pub fn invalidate_git_cache(dir: &Path) {
    if let Ok(mut g) = get_graph_cache().lock() {
        g.remove(dir);
    }
    if let Ok(mut r) = get_repos_cache().lock() {
        r.remove(dir);
    }
    if let Ok(mut m) = get_repo_meta_cache().lock() {
        m.remove(dir);
    }
}

/// Query parameters for Git endpoints.
#[derive(Debug, Deserialize)]
pub struct GitQuery {
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GitRepoInfo {
    pub root: String,
    pub name: String,
    pub relative_path: String,
    pub current_branch: Option<String>,
    pub is_root: bool,
}

#[derive(Debug, Serialize)]
pub struct GitReposResponse {
    pub repos: Vec<GitRepoInfo>,
}

#[derive(Debug, Serialize)]
pub struct GitBranchesResponse {
    pub is_repo: bool,
    pub repo_root: Option<String>,
    pub remote_url: Option<String>,
    pub current: Option<String>,
    pub local: Vec<String>,
    pub remote: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GitCommitItem {
    pub hash: String,
    pub short_hash: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: i64,
    pub message: String,
    pub refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_additions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_deletions: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GitGraphResponse {
    pub is_repo: bool,
    pub current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    pub commits: Vec<GitCommitItem>,
}

#[derive(Debug, Deserialize)]
pub struct GitActionReq {
    pub action: String, // "create_branch", "create_tag", "checkout", "cherry_pick", "revert"
    pub target: String,
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GitActionResp {
    pub success: bool,
    pub message: String,
}

/// POST /git/action
pub async fn git_action(
    State(_state): State<AppState>,
    Json(req): Json<GitActionReq>,
) -> impl IntoResponse {
    let target = match validate_git_ref_or_remote(&req.target) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Invalid target").into_response(),
    };
    let name = match req.action.as_str() {
        "create_branch" => match validate_git_ref_or_remote(req.name.as_deref().unwrap_or("")) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid branch name").into_response();
            }
        },
        "create_tag" => match validate_git_ref_or_remote(req.name.as_deref().unwrap_or("")) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid tag name").into_response();
            }
        },
        "rename_branch" => match validate_git_ref_or_remote(req.name.as_deref().unwrap_or("")) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid new branch name")
                    .into_response();
            }
        },
        _ => None,
    };
    let remote_branch_to_delete = if req.action == "delete_remote_branch" {
        let stripped = target.strip_prefix("origin/").unwrap_or(&target);
        match validate_git_ref_or_remote(stripped) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid target").into_response();
            }
        }
    } else {
        None
    };

    let cwd = req.cwd.clone();
    let dir = match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        Ok(dir)
    })
    .await
    {
        Ok(Ok(dir)) => dir,
        Ok(Err(resp)) | Err(resp) => return resp,
    };

    let output = match req.action.as_str() {
        "delete_remote_branch" => {
            let branch_name = remote_branch_to_delete.expect("validated above");
            match run_git_remote_cmd(
                dir.clone(),
                vec![
                    "push".into(),
                    "origin".into(),
                    "--delete".into(),
                    branch_name,
                ],
                GIT_REMOTE_TIMEOUT_SECS,
                "Git delete remote branch",
            )
            .await
            {
                Ok(out) => Ok(out),
                Err(resp) => return resp,
            }
        }
        "push_branch" => {
            let target_owned = target.clone();
            match run_git_remote_cmd(
                dir.clone(),
                vec!["push".into(), "-u".into(), "origin".into(), target_owned],
                GIT_REMOTE_TIMEOUT_SECS,
                "Git push branch",
            )
            .await
            {
                Ok(out) => Ok(out),
                Err(resp) => return resp,
            }
        }
        "checkout" | "create_branch" | "create_tag" | "cherry_pick" | "revert"
        | "delete_branch" | "rename_branch" | "merge_branch" => {
            let action = req.action.clone();
            let dir_for_cmd = dir.clone();
            let target_for_cmd = target.clone();
            let name_for_cmd = name.clone();
            match run_git_blocking(move || match action.as_str() {
                "checkout" => Ok(git_cmd(&dir_for_cmd)
                    .args(["checkout", &target_for_cmd])
                    .output()),
                "create_branch" => {
                    let branch_name = name_for_cmd.as_deref().unwrap_or("");
                    Ok(git_cmd(&dir_for_cmd)
                        .args(["branch", branch_name, &target_for_cmd])
                        .output())
                }
                "create_tag" => {
                    let tag_name = name_for_cmd.as_deref().unwrap_or("");
                    Ok(git_cmd(&dir_for_cmd)
                        .args(["tag", tag_name, &target_for_cmd])
                        .output())
                }
                "cherry_pick" => Ok(git_cmd(&dir_for_cmd)
                    .args(["cherry-pick", &target_for_cmd])
                    .output()),
                "revert" => Ok(git_cmd(&dir_for_cmd)
                    .args(["revert", "--no-edit", &target_for_cmd])
                    .output()),
                "delete_branch" => {
                    let head_out = git_cmd(&dir_for_cmd)
                        .args(["rev-parse", "--abbrev-ref", "HEAD"])
                        .output();
                    if let Ok(ref ho) = head_out {
                        if String::from_utf8_lossy(&ho.stdout).trim() == target_for_cmd {
                            return Err(json_error(
                                StatusCode::BAD_REQUEST,
                                "Cannot delete the currently active branch",
                            )
                            .into_response());
                        }
                    }
                    Ok(git_cmd(&dir_for_cmd)
                        .args(["branch", "-D", &target_for_cmd])
                        .output())
                }
                "rename_branch" => {
                    let new_name = name_for_cmd.as_deref().unwrap_or("");
                    Ok(git_cmd(&dir_for_cmd)
                        .args(["branch", "-m", &target_for_cmd, new_name])
                        .output())
                }
                "merge_branch" => Ok(git_cmd(&dir_for_cmd)
                    .args(["merge", &target_for_cmd])
                    .output()),
                _ => unreachable!(),
            })
            .await
            {
                Ok(Ok(out_res)) => out_res,
                Ok(Err(resp)) | Err(resp) => return resp,
            }
        }
        _ => return json_error(StatusCode::BAD_REQUEST, "Unknown action").into_response(),
    };

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let combined = if stdout.is_empty() { stderr } else { stdout };
            if out.status.success() {
                invalidate_git_cache(&dir);
                crate::publish_global(crate::GlobalWireEvent::GitUpdated {
                    cwd: dir.to_string_lossy().to_string(),
                    fetch: false,
                });
                Json(GitActionResp {
                    success: true,
                    message: combined.trim().to_string(),
                })
                .into_response()
            } else {
                json_error(StatusCode::BAD_REQUEST, combined.trim().to_string()).into_response()
            }
        }
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute action: {e}"),
        )
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct GitCheckoutReq {
    pub branch: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GitCheckoutResp {
    pub success: bool,
    pub branch: String,
    pub message: String,
}

const GIT_REMOTE_TIMEOUT_SECS: u64 = 20;

fn validate_git_ref_or_remote(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ';' | '&' | '|'))
    {
        return Err("Invalid git ref or remote");
    }
    Ok(trimmed.to_string())
}

async fn run_git_blocking<F, T>(f: F) -> Result<T, axum::response::Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Git task failed: {e}"),
        )
        .into_response()
    })
}

fn tokio_git_cmd(working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(working_dir);
    cmd.args(["-c", "core.quotepath=false"]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd.stdin(Stdio::null());
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

async fn run_git_remote_cmd(
    dir: PathBuf,
    args: Vec<String>,
    timeout_secs: u64,
    op_label: &'static str,
) -> Result<std::process::Output, axum::response::Response> {
    let fut = tokio_git_cmd(&dir).args(&args).output();
    match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute {op_label}: {e}"),
        )
        .into_response()),
        Err(_) => Err(json_error(
            StatusCode::GATEWAY_TIMEOUT,
            format!("{op_label} timed out after {timeout_secs} seconds"),
        )
        .into_response()),
    }
}

/// Helper to create a git Command with creation flags to avoid console popup on Windows.
fn git_cmd(working_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(working_dir);
    cmd.args(["-c", "core.quotepath=false"]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Decode Git quoted / octal-escaped paths into a normal UTF-8 string.
/// For example: `"\346\226\260\345\273\272 \346\226\207\346\234\254\346\226\207\346\241\243.txt"` -> `"新建 文本文档.txt"`
fn unquote_git_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut bytes = Vec::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next) = chars.peek() {
                    if ('0'..='7').contains(&next) {
                        let mut octal_val = 0u8;
                        let mut count = 0;
                        while count < 3 {
                            if let Some(&digit) = chars.peek() {
                                if ('0'..='7').contains(&digit) {
                                    octal_val = (octal_val << 3) + (digit as u8 - b'0');
                                    chars.next();
                                    count += 1;
                                    continue;
                                }
                            }
                            break;
                        }
                        bytes.push(octal_val);
                        continue;
                    } else if next == '\\' {
                        chars.next();
                        bytes.push(b'\\');
                        continue;
                    } else if next == '"' {
                        chars.next();
                        bytes.push(b'"');
                        continue;
                    } else if next == 't' {
                        chars.next();
                        bytes.push(b'\t');
                        continue;
                    } else if next == 'n' {
                        chars.next();
                        bytes.push(b'\n');
                        continue;
                    } else if next == 'r' {
                        chars.next();
                        bytes.push(b'\r');
                        continue;
                    }
                }
            }
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
        }
        String::from_utf8(bytes).unwrap_or_else(|_| inner.to_string())
    } else {
        trimmed.to_string()
    }
}

/// Git still interprets pathspec magic after `--`. Destructive single-path API calls
/// must opt into literal pathspecs so a client spelling like `*` or `:(glob)**` cannot
/// broaden one requested file into a repository-wide operation.
fn git_cmd_literal_paths(working_dir: &Path) -> Command {
    let mut cmd = git_cmd(working_dir);
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    cmd
}

/// Resolve the target directory from the optional cwd parameter.
fn resolve_target_dir(cwd: Option<&str>) -> PathBuf {
    let dir = match cwd {
        Some(path_str) if !path_str.trim().is_empty() => {
            let expanded = normalize_dir_arg(path_str);
            expanded.canonicalize().unwrap_or(expanded)
        }
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    jeikcode_capabilities::pathnorm::strip_verbatim_path(&dir)
}

fn scan_for_git_repos(
    current_dir: &Path,
    root_base: &Path,
    depth: usize,
    max_depth: usize,
    found: &mut Vec<GitRepoInfo>,
    visited_roots: &mut std::collections::HashSet<PathBuf>,
) {
    if depth > max_depth {
        return;
    }

    let read_res = match std::fs::read_dir(current_dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    for entry in read_res.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        if name_str.starts_with('.')
            || name_str == "node_modules"
            || name_str == "target"
            || name_str == "dist"
            || name_str == "build"
            || name_str == "vendor"
            || name_str == ".cargo"
        {
            continue;
        }

        let git_marker = path.join(".git");
        if git_marker.exists() {
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            let canon = jeikcode_capabilities::pathnorm::strip_verbatim_path(&canon);
            if visited_roots.insert(canon.clone()) {
                let current_branch = git_cmd(&path)
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

                let rel = path
                    .strip_prefix(root_base)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| name_str.to_string());

                found.push(GitRepoInfo {
                    root: canon.to_string_lossy().to_string(),
                    name: name_str.to_string(),
                    relative_path: rel,
                    current_branch,
                    is_root: false,
                });
            }
        } else {
            scan_for_git_repos(&path, root_base, depth + 1, max_depth, found, visited_roots);
        }
    }
}

/// GET /git/repos
pub async fn get_git_repos(
    State(_state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> impl IntoResponse {
    let cwd = q.cwd.clone();
    match run_git_blocking(move || get_git_repos_sync(cwd.as_deref())).await {
        Ok(resp) => Json(resp).into_response(),
        Err(resp) => resp,
    }
}

fn get_git_repos_sync(cwd: Option<&str>) -> GitReposResponse {
    let base_dir = resolve_target_dir(cwd);

    // 内存缓存加速：5分钟内不重复全盘扫描磁盘
    if let Ok(cache) = get_repos_cache().lock() {
        if let Some(entry) = cache.get(&base_dir) {
            if entry.timestamp.elapsed() < Duration::from_secs(300) {
                return GitReposResponse {
                    repos: entry.repos.clone(),
                };
            }
        }
    }

    let mut repos = Vec::new();
    let mut visited_roots = std::collections::HashSet::new();

    // 1. Check if base_dir is inside a git repo
    let root_out = git_cmd(&base_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output();

    let root_repo_path = if let Ok(ref out) = root_out {
        if out.status.success() {
            let p_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let p = PathBuf::from(p_str);
            let canon = p.canonicalize().unwrap_or(p);
            let canon = jeikcode_capabilities::pathnorm::strip_verbatim_path(&canon);
            Some(canon)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(ref root_p) = root_repo_path {
        visited_roots.insert(root_p.clone());
        let current_branch = git_cmd(root_p)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });

        let name = root_p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());

        repos.push(GitRepoInfo {
            root: root_p.to_string_lossy().to_string(),
            name,
            relative_path: ".".to_string(),
            current_branch,
            is_root: true,
        });

        // 2. Check .gitmodules in root repo
        let gitmodules_file = root_p.join(".gitmodules");
        if gitmodules_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&gitmodules_file) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if let Some(sub_path_str) = trimmed.strip_prefix("path = ") {
                        let sub_path_clean = sub_path_str.trim();
                        let sub_full = root_p.join(sub_path_clean);
                        let sub_canon = sub_full.canonicalize().unwrap_or(sub_full);
                        let sub_canon =
                            jeikcode_capabilities::pathnorm::strip_verbatim_path(&sub_canon);
                        if visited_roots.insert(sub_canon.clone()) {
                            let current_branch = git_cmd(&sub_canon)
                                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                                .output()
                                .ok()
                                .and_then(|o| {
                                    if o.status.success() {
                                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                                    } else {
                                        None
                                    }
                                });

                            let name = sub_canon
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| sub_path_clean.to_string());

                            repos.push(GitRepoInfo {
                                root: sub_canon.to_string_lossy().to_string(),
                                name,
                                relative_path: sub_path_clean.to_string(),
                                current_branch,
                                is_root: false,
                            });
                        }
                    }
                }
            }
        }

        // 3. Scan subdirectories up to depth 3 for nested git repos
        scan_for_git_repos(root_p, root_p, 1, 3, &mut repos, &mut visited_roots);
    } else {
        // Base directory is not a git repo itself, but may contain nested git repos (e.g. monorepo / multi-root workspace)
        scan_for_git_repos(&base_dir, &base_dir, 1, 3, &mut repos, &mut visited_roots);
    }

    if let Ok(mut cache) = get_repos_cache().lock() {
        cache.insert(
            base_dir.clone(),
            ReposCacheEntry {
                repos: repos.clone(),
                timestamp: Instant::now(),
            },
        );
    }

    GitReposResponse { repos }
}

/// Check if the target directory is inside a Git working tree.
fn is_git_repo(dir: &Path) -> bool {
    let output = git_cmd(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    match output {
        Ok(out) => out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true",
        Err(_) => false,
    }
}

/// GET /git/branches
pub async fn get_git_branches(
    State(_state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> impl IntoResponse {
    let cwd = q.cwd.clone();
    match run_git_blocking(move || get_git_branches_sync(cwd.as_deref())).await {
        Ok(resp) => Json(resp).into_response(),
        Err(resp) => resp,
    }
}

fn get_git_branches_sync(cwd: Option<&str>) -> GitBranchesResponse {
    let dir = resolve_target_dir(cwd);
    if !is_git_repo(&dir) {
        return GitBranchesResponse {
            is_repo: false,
            repo_root: None,
            remote_url: None,
            current: None,
            local: Vec::new(),
            remote: Vec::new(),
        };
    }

    // Get repo root
    let repo_root = git_cmd(&dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });

    // Get remote origin url
    let remote_url = git_cmd(&dir)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !url.is_empty() {
                    Some(url)
                } else {
                    None
                }
            } else {
                None
            }
        });

    // List branches
    let branch_output = git_cmd(&dir)
        .args([
            "branch",
            "-a",
            "--format=%(HEAD)|%(refname)|%(refname:short)",
        ])
        .output();

    let mut current_branch = None;
    let mut local_branches = Vec::new();
    let mut remote_branches = Vec::new();

    if let Ok(out) = branch_output {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = trimmed.split('|').collect();
                if parts.len() < 3 {
                    continue;
                }
                let is_head = parts[0].trim() == "*";
                let full_ref = parts[1].trim();
                let short_ref = parts[2].trim();

                // Skip symbolic HEADs like origin/HEAD
                if full_ref.ends_with("/HEAD") || short_ref.ends_with("/HEAD") {
                    continue;
                }

                if full_ref.starts_with("refs/heads/") {
                    let name = short_ref.to_string();
                    if is_head {
                        current_branch = Some(name.clone());
                    }
                    if !local_branches.contains(&name) {
                        local_branches.push(name);
                    }
                } else if full_ref.starts_with("refs/remotes/") {
                    let name = short_ref.to_string();
                    if !remote_branches.contains(&name) {
                        remote_branches.push(name);
                    }
                }
            }
        }
    }

    if current_branch.is_none() {
        current_branch = git_cmd(&dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if !name.is_empty() {
                        if name == "HEAD" {
                            git_cmd(&dir)
                                .args(["rev-parse", "--short", "HEAD"])
                                .output()
                                .ok()
                                .map(|ho| String::from_utf8_lossy(&ho.stdout).trim().to_string())
                        } else {
                            Some(name)
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            });
    }

    // If detached HEAD, get short sha
    if current_branch.is_none() {
        if let Ok(out) = git_cmd(&dir)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
        {
            if out.status.success() {
                let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !sha.is_empty() {
                    current_branch = Some(format!("(HEAD detached at {sha})"));
                }
            }
        }
    }

    GitBranchesResponse {
        is_repo: true,
        repo_root,
        remote_url,
        current: current_branch,
        local: local_branches,
        remote: remote_branches,
    }
}

/// Helper to parse git shortstat output: " 5 files changed, 201 insertions(+), 163 deletions(-)"
fn parse_shortstat(text: &str) -> (Option<usize>, Option<usize>, Option<usize>) {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("changed") {
            continue;
        }
        let mut files = None;
        let mut additions = None;
        let mut deletions = None;
        for part in trimmed.split(',') {
            let p = part.trim();
            if p.contains("file") {
                if let Some(num_str) = p.split_whitespace().next() {
                    files = num_str.parse::<usize>().ok();
                }
            } else if p.contains("insertion") {
                if let Some(num_str) = p.split_whitespace().next() {
                    additions = num_str.parse::<usize>().ok();
                }
            } else if p.contains("deletion") {
                if let Some(num_str) = p.split_whitespace().next() {
                    deletions = num_str.parse::<usize>().ok();
                }
            }
        }
        if files.is_some() {
            return (files, additions.or(Some(0)), deletions.or(Some(0)));
        }
    }
    (None, None, None)
}

/// GET /git/graph
pub async fn get_git_graph(
    State(_state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> impl IntoResponse {
    let branch_filter = match q.branch.as_deref() {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed == "all" {
                None
            } else {
                match validate_git_ref_or_remote(trimmed) {
                    Ok(valid) => Some(valid),
                    Err(_) => {
                        return json_error(StatusCode::BAD_REQUEST, "Invalid branch name")
                            .into_response();
                    }
                }
            }
        }
        None => None,
    };

    let cwd = q.cwd.clone();
    let limit = q.limit.unwrap_or(60).clamp(1, 300);
    let raw_branch_param = q.branch.clone();
    match run_git_blocking(move || {
        get_git_graph_sync(cwd.as_deref(), branch_filter, raw_branch_param, limit)
    })
    .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(resp) => resp,
    }
}

fn get_git_graph_sync(
    cwd: Option<&str>,
    branch_filter: Option<String>,
    raw_branch_param: Option<String>,
    limit: usize,
) -> GitGraphResponse {
    let dir = resolve_target_dir(cwd);
    if !is_git_repo(&dir) {
        return GitGraphResponse {
            is_repo: false,
            current_branch: None,
            remote_url: None,
            commits: Vec::new(),
        };
    }

    // Get current branch
    let current_branch = git_cmd(&dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if name == "HEAD" {
                    None
                } else {
                    Some(name)
                }
            } else {
                None
            }
        });

    // 极速 HEAD 与 FETCH_HEAD 联合验证：若本地 commit、远端 FETCH_HEAD 与当前分支均未改变，直接 0ms 返回内存缓存
    let head_commit = git_cmd(&dir)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });

    let fetch_head_commit = git_cmd(&dir)
        .args(["rev-parse", "-q", "--verify", "FETCH_HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            } else {
                None
            }
        });

    if let Some(ref head) = head_commit {
        if let Ok(cache) = get_graph_cache().lock() {
            if let Some(entry) = cache.get(&dir) {
                if entry.head_commit == *head
                    && entry.fetch_head_commit == fetch_head_commit
                    && entry.current_branch == current_branch
                    && entry.branch_param == raw_branch_param
                    && entry.limit == limit
                {
                    return entry.response.clone();
                }
            }
        }
    }

    let remote_url = git_cmd(&dir)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let u = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if u.is_empty() {
                    None
                } else {
                    Some(u)
                }
            } else {
                None
            }
        });

    let mut args = vec![
        "-n".to_string(),
        limit.to_string(),
        "--date=iso-strict".to_string(),
        "--shortstat".to_string(),
        "--pretty=format:__COMMIT_START__\x1f%H\x1f%h\x1f%P\x1f%an\x1f%ae\x1f%at\x1f%D\x1f%B\x1f__COMMIT_BODY_END__".to_string(),
    ];

    if let Some(branch) = branch_filter {
        args.insert(0, branch);
    } else {
        // Default to all branches to show complete DAG graph
        args.insert(0, "--all".to_string());
    }

    let output = git_cmd(&dir).arg("log").args(&args).output();

    let mut commits = Vec::new();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let chunks = stdout.split("__COMMIT_START__\x1f");
            for chunk in chunks {
                if chunk.trim().is_empty() {
                    continue;
                }
                let (meta_part, stat_part) = match chunk.split_once("__COMMIT_BODY_END__") {
                    Some((m, s)) => (m, s),
                    None => (chunk, ""),
                };
                let fields: Vec<&str> = meta_part.split('\x1f').collect();
                if fields.len() < 7 {
                    continue;
                }
                let hash = fields[0].trim().to_string();
                let short_hash = fields[1].trim().to_string();
                let parents: Vec<String> = fields[2]
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                let author_name = fields[3].trim().to_string();
                let author_email = fields[4].trim().to_string();
                let timestamp = fields[5].trim().parse::<i64>().unwrap_or(0);
                let refs: Vec<String> = if !fields[6].trim().is_empty() {
                    fields[6]
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                } else {
                    Vec::new()
                };

                // fields[7] contains raw %B message (complete subject, body, trailers)
                let message = if fields.len() >= 8 {
                    fields[7]
                        .trim_matches(|c| c == '\r' || c == '\n')
                        .to_string()
                } else {
                    String::new()
                };

                let (total_files, total_additions, total_deletions) = parse_shortstat(stat_part);

                commits.push(GitCommitItem {
                    hash,
                    short_hash,
                    parents,
                    author_name,
                    author_email,
                    timestamp,
                    message,
                    refs,
                    total_files,
                    total_additions,
                    total_deletions,
                });
            }
        }
    }

    let response = GitGraphResponse {
        is_repo: true,
        current_branch: current_branch.clone(),
        remote_url,
        commits,
    };

    if let Some(head) = head_commit {
        if let Ok(mut cache) = get_graph_cache().lock() {
            cache.insert(
                dir.clone(),
                GraphCacheEntry {
                    head_commit: head,
                    fetch_head_commit,
                    current_branch: current_branch.clone(),
                    branch_param: raw_branch_param,
                    limit,
                    response: response.clone(),
                },
            );
        }
    }

    response
}

/// POST /git/checkout
pub async fn git_checkout(
    State(_state): State<AppState>,
    Json(req): Json<GitCheckoutReq>,
) -> impl IntoResponse {
    if req.branch.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Branch name cannot be empty").into_response();
    }

    let branch = match validate_git_ref_or_remote(&req.branch) {
        Ok(b) => b,
        Err(_) => {
            return json_error(StatusCode::BAD_REQUEST, "Invalid branch name").into_response()
        }
    };

    // If branch is remote like origin/feature-a, strip remote prefix if checking out new local branch or let git track
    let target_branch = match branch.strip_prefix("origin/") {
        Some(stripped) => match validate_git_ref_or_remote(stripped) {
            Ok(valid) => valid,
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid branch name").into_response();
            }
        },
        None => branch,
    };

    let cwd = req.cwd.clone();
    let target_for_cmd = target_branch.clone();
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        let output = git_cmd(&dir).args(["checkout", &target_for_cmd]).output();
        Ok((dir, output))
    })
    .await
    {
        Ok(Ok((dir, Ok(out)))) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let combined = if stdout.is_empty() { stderr } else { stdout };

            if out.status.success() {
                invalidate_git_cache(&dir);
                crate::publish_global(crate::GlobalWireEvent::GitUpdated {
                    cwd: dir.to_string_lossy().to_string(),
                    fetch: false,
                });
                Json(GitCheckoutResp {
                    success: true,
                    branch: target_branch,
                    message: combined.trim().to_string(),
                })
                .into_response()
            } else {
                json_error(StatusCode::BAD_REQUEST, combined.trim().to_string()).into_response()
            }
        }
        Ok(Ok((_, Err(e)))) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute git checkout: {e}"),
        )
        .into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
pub struct GitCommitDetailQuery {
    pub hash: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitFileDiffQuery {
    pub hash: String,
    pub path: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GitCommitFile {
    pub path: String,
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Serialize)]
pub struct GitCommitDetailResp {
    pub hash: String,
    pub files: Vec<GitCommitFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_additions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_deletions: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct GitFileDiffResp {
    pub hash: String,
    pub path: String,
    pub diff: String,
}

/// GET /git/commit-detail
pub async fn get_git_commit_detail(
    State(_state): State<AppState>,
    Query(q): Query<GitCommitDetailQuery>,
) -> impl IntoResponse {
    let hash = match validate_git_ref_or_remote(&q.hash) {
        Ok(h) => h,
        Err(_) => {
            return json_error(StatusCode::BAD_REQUEST, "Invalid commit hash").into_response()
        }
    };

    let cwd = q.cwd.clone();
    match run_git_blocking(move || get_git_commit_detail_sync(cwd.as_deref(), hash)).await {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

fn get_git_commit_detail_sync(
    cwd: Option<&str>,
    hash: String,
) -> Result<GitCommitDetailResp, axum::response::Response> {
    let dir = resolve_target_dir(cwd);
    if !is_git_repo(&dir) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "Target directory is not a Git repository",
        )
        .into_response());
    }

    // 1. Get name-status
    let status_out = git_cmd(&dir)
        .args(["diff-tree", "--no-commit-id", "--name-status", "-r", &hash])
        .output();

    // 2. Get numstat
    let numstat_out = git_cmd(&dir)
        .args(["show", "--numstat", "--format=", &hash])
        .output();

    let mut numstat_map: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    if let Ok(ref out) = numstat_out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    let adds = parts[0].parse::<usize>().unwrap_or(0);
                    let dels = parts[1].parse::<usize>().unwrap_or(0);
                    let file_path = unquote_git_path(parts[2].trim());
                    numstat_map.insert(file_path, (adds, dels));
                }
            }
        }
    }

    let mut files = Vec::new();
    if let Ok(out) = status_out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = trimmed.split('\t').collect();
                if parts.len() >= 2 {
                    let status_char = parts[0].chars().next().unwrap_or('M').to_string();
                    let file_path = unquote_git_path(parts[1].trim());
                    let (adds, dels) = numstat_map.get(&file_path).copied().unwrap_or((0, 0));
                    files.push(GitCommitFile {
                        path: file_path,
                        status: status_char,
                        additions: adds,
                        deletions: dels,
                    });
                }
            }
        }
    }

    let total_files = files.len();
    let total_additions = files.iter().map(|f| f.additions).sum();
    let total_deletions = files.iter().map(|f| f.deletions).sum();

    Ok(GitCommitDetailResp {
        hash,
        files,
        total_files: Some(total_files),
        total_additions: Some(total_additions),
        total_deletions: Some(total_deletions),
    })
}

/// GET /git/file-diff
pub async fn get_git_file_diff(
    State(_state): State<AppState>,
    Query(q): Query<GitFileDiffQuery>,
) -> impl IntoResponse {
    let hash = match validate_git_ref_or_remote(&q.hash) {
        Ok(h) => h,
        Err(_) => {
            return json_error(StatusCode::BAD_REQUEST, "Invalid commit hash").into_response()
        }
    };
    let file_path = q.path.trim().to_string();
    if file_path.is_empty() || file_path.starts_with('-') {
        return json_error(StatusCode::BAD_REQUEST, "Invalid file path").into_response();
    }

    let cwd = q.cwd.clone();
    let file_path_for_cmd = file_path.clone();
    let hash_for_cmd = hash.clone();
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        Ok(git_cmd(&dir)
            .args(["show", "--format=", &hash_for_cmd, "--", &file_path_for_cmd])
            .output())
    })
    .await
    {
        Ok(Ok(Ok(out))) => {
            let diff = String::from_utf8_lossy(&out.stdout).to_string();
            Json(GitFileDiffResp {
                hash,
                path: file_path,
                diff,
            })
            .into_response()
        }
        Ok(Ok(Err(e))) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to get diff: {e}"),
        )
        .into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

// ============================================================================
// Source Control: Status, Stage, Commit, Push, Pull, Discard
// ============================================================================

#[derive(Debug, Serialize, Clone)]
pub struct GitStatusItem {
    pub path: String,
    pub status: String,
    pub staged: bool,
}

#[derive(Debug, Serialize)]
pub struct GitStatusResponse {
    pub is_repo: bool,
    pub current_branch: Option<String>,
    pub tracking_branch: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub staged: Vec<GitStatusItem>,
    pub unstaged: Vec<GitStatusItem>,
    pub untracked: Vec<GitStatusItem>,
}

#[derive(Debug, Deserialize)]
pub struct GitStageReq {
    pub path: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitUnstageReq {
    pub path: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitDiscardReq {
    pub path: String,
    #[serde(default)]
    pub is_untracked: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitCommitReq {
    pub message: String,
    #[serde(default)]
    pub amend: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitPushReq {
    pub branch: Option<String>,
    pub remote: Option<String>,
    #[serde(default)]
    pub set_upstream: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitPullReq {
    pub remote: Option<String>,
    pub branch: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitFetchReq {
    pub remote: Option<String>,
    #[serde(default)]
    pub prune: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitWorkingDiffQuery {
    pub path: String,
    pub staged: Option<bool>,
    pub cwd: Option<String>,
}

/// GET /git/status
pub async fn get_git_status(
    State(_state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> impl IntoResponse {
    let cwd = q.cwd.clone();
    match run_git_blocking(move || get_git_status_sync(cwd.as_deref())).await {
        Ok(resp) => Json(resp).into_response(),
        Err(resp) => resp,
    }
}

fn get_git_status_sync(cwd: Option<&str>) -> GitStatusResponse {
    let dir = resolve_target_dir(cwd);
    if !is_git_repo(&dir) {
        return GitStatusResponse {
            is_repo: false,
            current_branch: None,
            tracking_branch: None,
            ahead: 0,
            behind: 0,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
        };
    }

    let output = git_cmd(&dir)
        .args(["status", "--porcelain=v1", "-u", "--branch"])
        .output();

    let mut current_branch = None;
    let mut tracking_branch = None;
    let mut ahead = 0;
    let mut behind = 0;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    if let Ok(out) = output {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let trimmed = line.trim_end_matches('\r');
                if trimmed.starts_with("##") {
                    // Branch info line: ## main...origin/main [ahead 1, behind 2]
                    let header = trimmed.trim_start_matches('#').trim();
                    let (branch_part, track_info) = if let Some(bracket_idx) = header.find('[') {
                        (header[..bracket_idx].trim(), Some(&header[bracket_idx..]))
                    } else {
                        (header, None)
                    };

                    if let Some(dots_idx) = branch_part.find("...") {
                        current_branch = Some(branch_part[..dots_idx].trim().to_string());
                        tracking_branch = Some(branch_part[dots_idx + 3..].trim().to_string());
                    } else {
                        current_branch = Some(branch_part.to_string());
                    }

                    if let Some(info) = track_info {
                        if let Some(idx) = info.find("ahead ") {
                            let rest = &info[idx + 6..];
                            let num: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            ahead = num.parse().unwrap_or(0);
                        }
                        if let Some(idx) = info.find("behind ") {
                            let rest = &info[idx + 7..];
                            let num: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            behind = num.parse().unwrap_or(0);
                        }
                    }
                    continue;
                }

                if trimmed.len() < 4 {
                    continue;
                }

                let x = trimmed.as_bytes()[0] as char;
                let y = trimmed.as_bytes()[1] as char;
                let file_path = unquote_git_path(trimmed[3..].trim());

                if x == '?' && y == '?' {
                    untracked.push(GitStatusItem {
                        path: file_path,
                        status: "?".to_string(),
                        staged: false,
                    });
                } else {
                    if x != ' ' {
                        staged.push(GitStatusItem {
                            path: file_path.clone(),
                            status: x.to_string(),
                            staged: true,
                        });
                    }
                    if y != ' ' {
                        unstaged.push(GitStatusItem {
                            path: file_path,
                            status: y.to_string(),
                            staged: false,
                        });
                    }
                }
            }
        }
    }

    GitStatusResponse {
        is_repo: true,
        current_branch,
        tracking_branch,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
    }
}

/// POST /git/stage
pub async fn git_stage(
    State(_state): State<AppState>,
    Json(req): Json<GitStageReq>,
) -> impl IntoResponse {
    if !req.all && req.path.is_none() {
        return json_error(StatusCode::BAD_REQUEST, "Must specify path or all=true")
            .into_response();
    }
    let all = req.all;
    let path = req.path.clone();
    if !all && path.as_deref().is_none_or(|p| p.trim().is_empty()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Must specify non-empty path or all=true",
        )
        .into_response();
    }
    let cwd = req.cwd.clone();
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        let output = if all {
            git_cmd(&dir).args(["add", "-A"]).output()
        } else {
            let p = path.as_deref().unwrap_or("");
            git_cmd_literal_paths(&dir).args(["add", "--", p]).output()
        };
        Ok((dir, output))
    })
    .await
    {
        Ok(Ok((dir, Ok(out)))) if out.status.success() => {
            invalidate_git_cache(&dir);
            crate::publish_global(crate::GlobalWireEvent::GitUpdated {
                cwd: dir.to_string_lossy().to_string(),
                fetch: false,
            });
            Json(serde_json::json!({ "success": true })).into_response()
        }
        Ok(Ok((_, Ok(out)))) => json_error(
            StatusCode::BAD_REQUEST,
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
        .into_response(),
        Ok(Ok((_, Err(e)))) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to stage: {e}"),
        )
        .into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

/// POST /git/unstage
pub async fn git_unstage(
    State(_state): State<AppState>,
    Json(req): Json<GitUnstageReq>,
) -> impl IntoResponse {
    if !req.all && req.path.is_none() {
        return json_error(StatusCode::BAD_REQUEST, "Must specify path or all=true")
            .into_response();
    }
    let all = req.all;
    let path = req.path.clone();
    if !all && path.as_deref().is_none_or(|p| p.trim().is_empty()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Must specify non-empty path or all=true",
        )
        .into_response();
    }
    let cwd = req.cwd.clone();
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        let output = if all {
            git_cmd(&dir).args(["restore", "--staged", "."]).output()
        } else {
            let p = path.as_deref().unwrap_or("");
            git_cmd_literal_paths(&dir)
                .args(["restore", "--staged", "--", p])
                .output()
        };
        Ok((dir, output))
    })
    .await
    {
        Ok(Ok((dir, Ok(out)))) if out.status.success() => {
            invalidate_git_cache(&dir);
            crate::publish_global(crate::GlobalWireEvent::GitUpdated {
                cwd: dir.to_string_lossy().to_string(),
                fetch: false,
            });
            Json(serde_json::json!({ "success": true })).into_response()
        }
        Ok(Ok((_, Ok(out)))) => json_error(
            StatusCode::BAD_REQUEST,
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
        .into_response(),
        Ok(Ok((_, Err(e)))) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to unstage: {e}"),
        )
        .into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

fn validated_repo_relative_path(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('-') {
        return Err("Invalid file path".to_string());
    }

    let path = Path::new(raw);
    if path.is_absolute() {
        return Err("File path must be relative to the Git repository".to_string());
    }

    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("File path must stay within the Git repository".to_string());
            }
        }
    }

    if !has_normal_component {
        return Err("Invalid file path".to_string());
    }

    Ok(path.to_path_buf())
}

fn discard_untracked_path(dir: &Path, file_path: &Path) -> Result<(), String> {
    // `git clean <dir>` is recursive over untracked descendants. If the requested
    // directory itself contains tracked entries, a forged `is_untracked=true` flag
    // must not turn that directory into authority to erase every scratch file below it.
    // Literal pathspec mode also prevents metacharacters from broadening this probe.
    let tracked = git_cmd_literal_paths(dir)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(file_path)
        .output()
        .map_err(|error| format!("Failed to inspect tracked path: {error}"))?;
    if tracked.status.success() {
        return Err("Path is tracked or contains tracked files".to_string());
    }
    if tracked.status.code() != Some(1) {
        return Err(String::from_utf8_lossy(&tracked.stderr).trim().to_string());
    }

    let preview = git_cmd_literal_paths(dir)
        .args(["clean", "-nd", "--"])
        .arg(file_path)
        .output()
        .map_err(|error| format!("Failed to inspect untracked path: {error}"))?;
    if !preview.status.success() {
        return Err(String::from_utf8_lossy(&preview.stderr).trim().to_string());
    }
    if preview.stdout.is_empty() {
        return Err("Path is not an untracked file or directory".to_string());
    }

    let clean = git_cmd_literal_paths(dir)
        .args(["clean", "-fd", "--"])
        .arg(file_path)
        .output()
        .map_err(|error| format!("Failed to discard untracked path: {error}"))?;
    if clean.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&clean.stderr).trim().to_string())
    }
}

/// POST /git/discard
pub async fn git_discard(
    State(_state): State<AppState>,
    Json(req): Json<GitDiscardReq>,
) -> impl IntoResponse {
    let file_path = match validated_repo_relative_path(&req.path) {
        Ok(path) => path,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
    };

    let cwd = req.cwd.clone();
    let is_untracked = req.is_untracked;
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }

        let discard_result = if is_untracked {
            match discard_untracked_path(&dir, &file_path) {
                Ok(()) => Ok(()),
                Err(_) => {
                    // 如果 clean 失败，可能文件其实已被 index 跟踪，回退尝试 restore 回退到 HEAD
                    let output = git_cmd_literal_paths(&dir)
                        .args(["restore", "--staged", "--worktree", "--"])
                        .arg(&file_path)
                        .output();
                    match output {
                        Ok(out) if out.status.success() => Ok(()),
                        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).to_string()),
                        Err(e) => Err(format!("Failed to discard changes: {e}")),
                    }
                }
            }
        } else {
            // 先尝试恢复暂存区和工作区（无论是否被 git add，彻底回滚到 HEAD）
            let output = git_cmd_literal_paths(&dir)
                .args(["restore", "--staged", "--worktree", "--"])
                .arg(&file_path)
                .output();
            match output {
                Ok(out) if out.status.success() => Ok(()),
                _ => {
                    // 如果 restore 失败（例如是未跟踪文件但客户端没标 untracked），回退尝试 clean
                    discard_untracked_path(&dir, &file_path)
                }
            }
        };

        Ok((dir, discard_result))
    })
    .await
    {
        Ok(Ok((dir, Ok(())))) => {
            invalidate_git_cache(&dir);
            crate::publish_global(crate::GlobalWireEvent::GitUpdated {
                cwd: dir.to_string_lossy().to_string(),
                fetch: false,
            });
            Json(serde_json::json!({ "success": true })).into_response()
        }
        Ok(Ok((_, Err(error)))) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

/// POST /git/commit
pub async fn git_commit(
    State(_state): State<AppState>,
    Json(req): Json<GitCommitReq>,
) -> impl IntoResponse {
    let message = req.message.trim().to_string();
    if message.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Commit message cannot be empty")
            .into_response();
    }

    let cwd = req.cwd.clone();
    let amend = req.amend;
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }

        let mut args = vec!["commit".to_string()];
        if amend {
            args.push("--amend".to_string());
        }
        args.push("-m".to_string());
        args.push(message);

        let output = git_cmd(&dir).args(&args).output();
        Ok((dir, output))
    })
    .await
    {
        Ok(Ok((dir, Ok(out)))) if out.status.success() => {
            invalidate_git_cache(&dir);
            crate::publish_global(crate::GlobalWireEvent::GitUpdated {
                cwd: dir.to_string_lossy().to_string(),
                fetch: false,
            });
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            Json(serde_json::json!({
                "success": true,
                "message": stdout.trim().to_string(),
            }))
            .into_response()
        }
        Ok(Ok((_, Ok(out)))) => json_error(
            StatusCode::BAD_REQUEST,
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
        .into_response(),
        Ok(Ok((_, Err(e)))) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to commit: {e}"),
        )
        .into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

/// POST /git/push
pub async fn git_push(
    State(_state): State<AppState>,
    Json(req): Json<GitPushReq>,
) -> impl IntoResponse {
    let remote = match req.remote.as_deref() {
        Some(r) => match validate_git_ref_or_remote(r) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid remote name").into_response();
            }
        },
        None => None,
    };
    let branch = match req.branch.as_deref() {
        Some(b) => match validate_git_ref_or_remote(b) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid branch name").into_response();
            }
        },
        None => None,
    };
    if !req.set_upstream && remote.is_none() && branch.is_some() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Remote is required when branch is specified",
        )
        .into_response();
    }

    let cwd = req.cwd.clone();
    let dir = match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        Ok(dir)
    })
    .await
    {
        Ok(Ok(dir)) => dir,
        Ok(Err(resp)) | Err(resp) => return resp,
    };

    let mut args = vec!["push".to_string()];
    if req.set_upstream {
        args.push("-u".to_string());
        args.push(remote.unwrap_or_else(|| "origin".to_string()));
        args.push(branch.unwrap_or_else(|| "HEAD".to_string()));
    } else if let Some(r) = remote {
        args.push(r);
        if let Some(b) = branch {
            args.push(b);
        }
    }

    let out = match run_git_remote_cmd(dir.clone(), args, GIT_REMOTE_TIMEOUT_SECS, "Git push").await
    {
        Ok(out) => out,
        Err(resp) => return resp,
    };

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = if stdout.is_empty() { stderr } else { stdout };

    if out.status.success() {
        invalidate_git_cache(&dir);
        crate::publish_global(crate::GlobalWireEvent::GitUpdated {
            cwd: dir.to_string_lossy().to_string(),
            fetch: false,
        });
        Json(serde_json::json!({
            "success": true,
            "message": combined.trim().to_string(),
        }))
        .into_response()
    } else {
        json_error(StatusCode::BAD_REQUEST, combined.trim().to_string()).into_response()
    }
}

/// POST /git/pull
pub async fn git_pull(
    State(_state): State<AppState>,
    Json(req): Json<GitPullReq>,
) -> impl IntoResponse {
    let remote = match req.remote.as_deref() {
        Some(r) => match validate_git_ref_or_remote(r) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid remote name").into_response();
            }
        },
        None => None,
    };
    let branch = match req.branch.as_deref() {
        Some(b) => match validate_git_ref_or_remote(b) {
            Ok(v) => Some(v),
            Err(_) => {
                return json_error(StatusCode::BAD_REQUEST, "Invalid branch name").into_response();
            }
        },
        None => None,
    };
    if remote.is_none() && branch.is_some() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Remote is required when branch is specified",
        )
        .into_response();
    }

    let cwd = req.cwd.clone();
    let dir = match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        Ok(dir)
    })
    .await
    {
        Ok(Ok(dir)) => dir,
        Ok(Err(resp)) | Err(resp) => return resp,
    };

    let mut args = vec!["pull".to_string()];
    if let Some(r) = remote {
        args.push(r);
        if let Some(b) = branch {
            args.push(b);
        }
    }

    let out = match run_git_remote_cmd(dir.clone(), args, GIT_REMOTE_TIMEOUT_SECS, "Git pull").await
    {
        Ok(out) => out,
        Err(resp) => return resp,
    };

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = if stdout.is_empty() { stderr } else { stdout };

    if out.status.success() {
        invalidate_git_cache(&dir);
        crate::publish_global(crate::GlobalWireEvent::GitUpdated {
            cwd: dir.to_string_lossy().to_string(),
            fetch: false,
        });
        Json(serde_json::json!({
            "success": true,
            "message": combined.trim().to_string(),
        }))
        .into_response()
    } else {
        json_error(StatusCode::BAD_REQUEST, combined.trim().to_string()).into_response()
    }
}

/// POST /git/fetch
pub async fn git_fetch(
    State(_state): State<AppState>,
    Json(req): Json<GitFetchReq>,
) -> impl IntoResponse {
    let validated_remote = match req.remote.as_deref() {
        Some(r) => {
            if r.trim().is_empty() {
                Some("origin".to_string())
            } else {
                match validate_git_ref_or_remote(r) {
                    Ok(v) => Some(v),
                    Err(_) => {
                        return json_error(StatusCode::BAD_REQUEST, "Invalid remote name")
                            .into_response();
                    }
                }
            }
        }
        None => None,
    };

    let cwd = req.cwd.clone();
    let prune = req.prune.unwrap_or(true);
    let prep = match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }

        // 检查是否有配置 remote。若无 remote（如纯本地 repo），直接返回 success，避免报错
        let remote_check = git_cmd(&dir).args(["remote"]).output();
        let has_remote = match remote_check {
            Ok(ref out) => {
                out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
            }
            Err(_) => false,
        };
        if !has_remote {
            return Ok((dir, None));
        }

        let mut args = vec!["fetch".to_string()];
        if let Some(r) = validated_remote {
            args.push(r);
        } else {
            // 优先检查当前分支绑定的 remote，若无则默认 origin，若无 origin 再回退 --all，杜绝陈旧失效 remote 拖垮
            let current_branch_name = git_cmd(&dir)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        if s.is_empty() || s == "HEAD" {
                            None
                        } else {
                            Some(s)
                        }
                    } else {
                        None
                    }
                });

            let tracking_remote = current_branch_name.as_ref().and_then(|b| {
                git_cmd(&dir)
                    .args(["config", "--get", &format!("branch.{b}.remote")])
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            let r = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            if r.is_empty() {
                                None
                            } else {
                                Some(r)
                            }
                        } else {
                            None
                        }
                    })
            });

            if let Some(r) = tracking_remote {
                args.push(r);
            } else {
                let has_origin = git_cmd(&dir)
                    .args(["remote", "get-url", "origin"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if has_origin {
                    args.push("origin".to_string());
                } else {
                    args.push("--all".to_string());
                }
            }
        }

        if prune {
            args.push("--prune".to_string());
        }
        args.push("--tags".to_string());
        Ok((dir, Some(args)))
    })
    .await
    {
        Ok(Ok(res)) => res,
        Ok(Err(resp)) | Err(resp) => return resp,
    };

    let (dir, Some(args)) = prep else {
        return Json(serde_json::json!({
            "success": true,
            "message": "No remotes configured",
            "has_remote": false,
        }))
        .into_response();
    };

    let out =
        match run_git_remote_cmd(dir.clone(), args, GIT_REMOTE_TIMEOUT_SECS, "Git fetch").await {
            Ok(out) => out,
            Err(resp) => return resp,
        };

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = if stdout.is_empty() { stderr } else { stdout };

    if out.status.success() {
        // Fetch 成功后，主动失效后端的 Graph 缓存和 Meta 缓存，确保后续请求能拿到最新上游提交
        invalidate_git_cache(&dir);
        crate::publish_global(crate::GlobalWireEvent::GitUpdated {
            cwd: dir.to_string_lossy().to_string(),
            fetch: false,
        });
        Json(serde_json::json!({
            "success": true,
            "message": combined.trim().to_string(),
            "has_remote": true,
        }))
        .into_response()
    } else {
        json_error(StatusCode::BAD_REQUEST, combined.trim().to_string()).into_response()
    }
}

/// GET /git/working-diff
pub async fn get_git_working_diff(
    State(_state): State<AppState>,
    Query(q): Query<GitWorkingDiffQuery>,
) -> impl IntoResponse {
    let file_path = q.path.trim().to_string();
    if file_path.is_empty() || file_path.starts_with('-') {
        return json_error(StatusCode::BAD_REQUEST, "Invalid file path").into_response();
    }

    let cwd = q.cwd.clone();
    let staged = q.staged == Some(true);
    let file_path_for_cmd = file_path.clone();
    match run_git_blocking(move || {
        let dir = resolve_target_dir(cwd.as_deref());
        if !is_git_repo(&dir) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "Target directory is not a Git repository",
            )
            .into_response());
        }
        let output = if staged {
            git_cmd(&dir)
                .args(["diff", "--cached", "--", &file_path_for_cmd])
                .output()
        } else {
            // Try git diff first
            let diff_out = git_cmd(&dir)
                .args(["diff", "--", &file_path_for_cmd])
                .output();

            if let Ok(ref out) = diff_out {
                if out.status.success() && !out.stdout.is_empty() {
                    diff_out
                } else {
                    // If diff is empty, file might be untracked! Try diff against /dev/null
                    git_cmd(&dir)
                        .args(["diff", "--no-index", "/dev/null", &file_path_for_cmd])
                        .output()
                }
            } else {
                diff_out
            }
        };
        Ok(output)
    })
    .await
    {
        Ok(Ok(Ok(out))) => {
            let diff = String::from_utf8_lossy(&out.stdout).to_string();
            Json(GitFileDiffResp {
                hash: "working-tree".to_string(),
                path: file_path,
                diff,
            })
            .into_response()
        }
        Ok(Ok(Err(e))) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to get working diff: {e}"),
        )
        .into_response(),
        Ok(Err(resp)) | Err(resp) => resp,
    }
}

#[cfg(test)]
mod discard_tests {
    use super::*;
    use std::fs;

    fn temp_repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("temp parent");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("create repo dir");
        let status = git_cmd(&repo)
            .args(["init", "--quiet"])
            .status()
            .expect("git init");
        assert!(status.success(), "git init must succeed");
        (temp, repo)
    }

    #[test]
    fn discard_untracked_rejects_parent_traversal_and_preserves_sibling() {
        let (temp, repo) = temp_repo();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "sentinel").unwrap();

        let result = validated_repo_relative_path("../outside.txt")
            .and_then(|path| discard_untracked_path(&repo, &path));
        assert!(result.is_err());
        assert!(outside.exists());
        assert!(repo.exists());
    }

    #[test]
    fn discard_untracked_rejects_absolute_path_and_preserves_target() {
        let (temp, repo) = temp_repo();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "sentinel").unwrap();

        let result = validated_repo_relative_path(outside.to_string_lossy().as_ref())
            .and_then(|path| discard_untracked_path(&repo, &path));
        assert!(result.is_err());
        assert!(outside.exists());
    }

    #[test]
    fn discard_untracked_removes_nested_relative_file() {
        let (_temp, repo) = temp_repo();
        let nested = repo.join("nested");
        fs::create_dir(&nested).unwrap();
        let file = nested.join("note.txt");
        fs::write(&file, "untracked").unwrap();

        let path = validated_repo_relative_path("nested/note.txt").unwrap();
        discard_untracked_path(&repo, &path).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn discard_untracked_removes_untracked_directory() {
        let (_temp, repo) = temp_repo();
        let directory = repo.join("scratch");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("nested.txt"), "untracked").unwrap();

        let path = validated_repo_relative_path("scratch").unwrap();
        discard_untracked_path(&repo, &path).unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn discard_untracked_refuses_git_known_file_even_if_client_claims_untracked() {
        let (_temp, repo) = temp_repo();
        let file = repo.join("tracked.txt");
        fs::write(&file, "tracked").unwrap();
        let status = git_cmd(&repo)
            .args(["add", "--", "tracked.txt"])
            .status()
            .expect("git add");
        assert!(status.success());

        let path = validated_repo_relative_path("tracked.txt").unwrap();
        let error = discard_untracked_path(&repo, &path).unwrap_err();
        assert!(error.contains("tracked"), "{error}");
        assert!(file.exists());
    }

    #[test]
    fn discard_untracked_refuses_tracked_directory_with_untracked_descendants() {
        let (_temp, repo) = temp_repo();
        let src = repo.join("src");
        fs::create_dir(&src).unwrap();
        let tracked = src.join("tracked.rs");
        let scratch = src.join("scratch.tmp");
        fs::write(&tracked, "tracked").unwrap();
        fs::write(&scratch, "scratch").unwrap();
        assert!(git_cmd(&repo)
            .args(["add", "--", "src/tracked.rs"])
            .status()
            .unwrap()
            .success());

        let path = validated_repo_relative_path("src").unwrap();
        let error = discard_untracked_path(&repo, &path).unwrap_err();
        assert!(error.contains("tracked"), "{error}");
        assert!(tracked.exists());
        assert!(
            scratch.exists(),
            "tracked parent must not authorize cleaning descendants"
        );
    }

    #[test]
    fn discard_untracked_treats_git_pathspec_magic_as_a_literal_path() {
        let (_temp, repo) = temp_repo();
        let first = repo.join("first.txt");
        let second = repo.join("second.txt");
        fs::write(&first, "one").unwrap();
        fs::write(&second, "two").unwrap();

        let magic = validated_repo_relative_path(":(glob)**").unwrap();
        assert!(discard_untracked_path(&repo, &magic).is_err());
        assert!(first.exists(), "literal pathspec must not clean first.txt");
        assert!(
            second.exists(),
            "literal pathspec must not clean second.txt"
        );
    }

    #[test]
    fn tracked_restore_treats_wildcards_as_literal_paths() {
        let (_temp, repo) = temp_repo();
        fs::write(repo.join("first.txt"), "base-one").unwrap();
        fs::write(repo.join("second.txt"), "base-two").unwrap();
        assert!(git_cmd(&repo)
            .args(["add", "--", "first.txt", "second.txt"])
            .status()
            .unwrap()
            .success());
        fs::write(repo.join("first.txt"), "changed-one").unwrap();
        fs::write(repo.join("second.txt"), "changed-two").unwrap();

        let output = git_cmd_literal_paths(&repo)
            .args(["restore", "--"])
            .arg("*")
            .output()
            .unwrap();
        assert!(!output.status.success(), "no literal '*' path should match");
        assert_eq!(
            fs::read_to_string(repo.join("first.txt")).unwrap(),
            "changed-one"
        );
        assert_eq!(
            fs::read_to_string(repo.join("second.txt")).unwrap(),
            "changed-two"
        );
    }

    #[test]
    fn discard_path_validator_accepts_nested_paths_and_rejects_dot() {
        assert_eq!(
            validated_repo_relative_path("./nested/file.txt").unwrap(),
            PathBuf::from("./nested/file.txt")
        );
        assert!(validated_repo_relative_path(".").is_err());
    }

    #[test]
    fn unquote_git_path_decodes_chinese_octal_escapes() {
        let raw =
            r#""\346\226\260\345\273\272 \346\226\207\346\234\254\346\226\207\346\241\243.txt""#;
        assert_eq!(unquote_git_path(raw), "新建 文本文档.txt");
        assert_eq!(unquote_git_path("normal.txt"), "normal.txt");
        assert_eq!(
            unquote_git_path(r#""file with spaces.txt""#),
            "file with spaces.txt"
        );
    }

    #[test]
    fn validate_git_ref_or_remote_accepts_valid_and_rejects_flags() {
        assert_eq!(validate_git_ref_or_remote("  main  ").unwrap(), "main");
        assert_eq!(
            validate_git_ref_or_remote("origin/feat-1").unwrap(),
            "origin/feat-1"
        );
        assert_eq!(validate_git_ref_or_remote("HEAD~1").unwrap(), "HEAD~1");
        assert!(validate_git_ref_or_remote("").is_err());
        assert!(validate_git_ref_or_remote("--upload-pack=id").is_err());
        assert!(validate_git_ref_or_remote("-f").is_err());
        assert!(validate_git_ref_or_remote("main\t--force").is_err());
        assert!(validate_git_ref_or_remote("main;id").is_err());
        assert!(validate_git_ref_or_remote("main\n--all").is_err());
    }

    #[tokio::test]
    async fn api_git_routes_reject_flag_injection_with_bad_request() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::{get, post};
        use tower::ServiceExt;

        let home = crate::tests::ScopedChatHome::new();
        let state = crate::tests::chat_test_state(&home);
        let (_temp, repo) = temp_repo();
        let cwd_str = repo.to_string_lossy().to_string();

        let app = axum::Router::new()
            .route("/git/graph", get(get_git_graph))
            .route("/git/push", post(git_push))
            .route("/git/pull", post(git_pull))
            .route("/git/fetch", post(git_fetch))
            .route("/git/checkout", post(git_checkout))
            .route("/git/action", post(git_action))
            .route("/git/commit-detail", get(get_git_commit_detail))
            .route("/git/file-diff", get(get_git_file_diff))
            .route("/git/stage", post(git_stage))
            .route("/git/unstage", post(git_unstage))
            .with_state(state);

        // /git/graph flag injection -> 400
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/git/graph?branch=--output%3Dpwn")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // /git/push flag injection -> 400
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/git/push")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "remote": "--receive-pack=id",
                            "cwd": cwd_str,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // /git/pull flag injection -> 400
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/git/pull")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "remote": "--upload-pack=id",
                            "cwd": cwd_str,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // /git/fetch flag injection even when repo has no remotes -> 400 (not 200 OK)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/git/fetch")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "remote": "--upload-pack=id",
                            "cwd": cwd_str,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // /git/checkout post-origin-strip flag injection -> 400
        for bad_branch in ["origin/-f", "origin/", "feat\tbar"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/git/checkout")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "branch": bad_branch,
                                "cwd": cwd_str,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "branch={bad_branch}"
            );
        }

        // /git/action delete_remote_branch post-origin-strip flag injection -> 400
        for bad_target in ["origin/-f", "origin/"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/git/action")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "action": "delete_remote_branch",
                                "target": bad_target,
                                "cwd": cwd_str,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "target={bad_target}"
            );
        }

        // /git/commit-detail & /git/file-diff flag injection -> 400
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/git/commit-detail?hash=--stat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/git/file-diff?hash=-p&path=a.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Literal pathspec guard: wildcard "*" must not stage unrelated files
        std::fs::write(repo.join("other.txt"), "other").unwrap();
        let stage_glob = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/git/stage")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "cwd": cwd_str,
                            "path": "*"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stage_glob.status(), StatusCode::BAD_REQUEST);

        let empty_unstage = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/git/unstage")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "cwd": cwd_str,
                            "path": "   "
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty_unstage.status(), StatusCode::BAD_REQUEST);
    }
}
