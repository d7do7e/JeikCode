//! Autonomous `/goal` verification harness:
//! - Change-driven workspace git-tree fingerprinting (`HEAD` + staged/unstaged diff + untracked state)
//! - Non-destructive, worktree- & Windows-safe milestone git checkpoints (`refs/jeikcode/goal-checkpoints/...`)
//! - Identical-state & oscillation (`A -> B -> A`) no-progress circuit breaker with opt-in rollback
//! - `/goal` flag parsing (`--until-stopped` / `--continuous`, `--checkpoint`, `--verify`,
//!   `--no-verify`, `--fresh-epoch`, `--no-fresh-epoch`, `--rollback-on-stall`,
//!   `--no-rollback-on-stall`, `--max-rounds <N>`, `--max-tokens <N>`, `shift` preset)

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use jeikcode_capabilities::process_utils::suppress_console_window_sync;

/// Default consecutive identical-state rounds before the circuit breaker trips.
pub(crate) const DEFAULT_GOAL_STALL_LIMIT: u32 = 3;

/// Default round batch size per continuous rollover checkpoint when `--until-stopped`
/// is used and the user's configured `goal_max_rounds` is `0` or smaller.
pub(crate) const DEFAULT_CONTINUOUS_CHECKPOINT_INTERVAL: u32 = 25;

/// Maximum single untracked file size included in a milestone checkpoint commit (1 MiB).
const MAX_CHECKPOINT_UNTRACKED_BYTES: u64 = 1024 * 1024;
/// Maximum number of milestone checkpoint refs retained per goal (oldest pruned).
const MAX_CHECKPOINTS_PER_GOAL: usize = 20;
/// Maximum size of bounded `git diff --stat HEAD` embedded in blind-verifier prompts.
const MAX_DIFF_STAT_CHARS: usize = 4_000;
const MAX_DIFF_STAT_LINES: usize = 60;
/// Ring-buffer depth for oscillation (`A -> B -> A`) detection.
const STALL_FINGERPRINT_RING_CAP: usize = 8;

/// Configurable `/goal` defaults supplied by `[coding]` / `[goal]` in `config.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoalLaunchDefaults {
    pub rollback_on_stall: bool,
    pub fresh_epoch: Option<bool>,
}

impl Default for GoalLaunchDefaults {
    fn default() -> Self {
        Self {
            rollback_on_stall: true,
            fresh_epoch: None,
        }
    }
}

/// Parsed `/goal` launch options and stripped condition text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoalOptions {
    pub continuous: bool,
    pub checkpoint_milestones: bool,
    pub force_verifier: bool,
    pub skip_verifier: bool,
    pub fresh_epoch: bool,
    pub rollback_on_stall: bool,
    pub max_tokens: Option<u64>,
    pub max_rounds: Option<u32>,
    /// Tracks explicit `--fresh-epoch` / `--no-fresh-epoch` user override.
    pub fresh_epoch_override: Option<bool>,
    /// Tracks explicit `--rollback-on-stall` / `--no-rollback-on-stall` user override.
    pub rollback_on_stall_override: Option<bool>,
}

impl GoalOptions {
    /// Apply smart defaults (`rollback_on_stall = true` by default; `fresh_epoch = true`
    /// automatically when `--until-stopped` / `shift` is active unless disabled).
    pub fn resolve_defaults(&mut self, defaults: GoalLaunchDefaults) {
        self.fresh_epoch = self
            .fresh_epoch_override
            .unwrap_or_else(|| defaults.fresh_epoch.unwrap_or(self.continuous));
        self.rollback_on_stall = self
            .rollback_on_stall_override
            .unwrap_or(defaults.rollback_on_stall);
        if self.rollback_on_stall {
            self.checkpoint_milestones = true;
        }
    }
}

/// Parse optional leading flags (or `shift` preset) from `/goal <flags> <condition>`
/// using default launch settings.
pub fn parse_goal_launch(raw: &str) -> (String, GoalOptions) {
    parse_goal_launch_with_defaults(raw, GoalLaunchDefaults::default())
}

/// Parse optional leading flags (or `shift` preset) from `/goal <flags> <condition>`
/// and resolve unset flags against `defaults`:
/// - `shift <prompt>` — preset for `--until-stopped --fresh-epoch --rollback-on-stall`
/// - `--until-stopped` / `--continuous` — roll over round budgets continuously with checkpoints
/// - `--checkpoint` / `--checkpoints` — record non-destructive git checkpoint refs at milestones
/// - `--verify` / `--no-verify` — force or skip the blind `verifier` subagent
/// - `--fresh-epoch` / `--no-fresh-epoch` — reset context to frozen prefix on rollovers
/// - `--rollback-on-stall` / `--no-rollback-on-stall` — restore milestone checkpoint on stall
/// - `--max-rounds <N>` / `--max-rounds=<N>` — override the round budget (`0` = unbounded)
/// - `--max-tokens <N>` / `--max-tokens=<N>` — stop once cumulative prompt+completion tokens reach `N`
pub fn parse_goal_launch_with_defaults(
    raw: &str,
    defaults: GoalLaunchDefaults,
) -> (String, GoalOptions) {
    let mut opts = GoalOptions::default();
    let mut rest = raw.trim();

    if let Some((first, after)) = split_first_token(rest) {
        if first.eq_ignore_ascii_case("shift") {
            opts.continuous = true;
            opts.checkpoint_milestones = true;
            opts.fresh_epoch_override = Some(true);
            opts.rollback_on_stall_override = Some(true);
            rest = after;
        }
    }

    loop {
        let trimmed = rest.trim_start();
        let Some((token, after)) = split_first_token(trimmed) else {
            break;
        };
        match token {
            "--until-stopped" | "--continuous" => {
                opts.continuous = true;
                opts.checkpoint_milestones = true;
                rest = after;
            }
            "--checkpoint" | "--checkpoints" => {
                opts.checkpoint_milestones = true;
                rest = after;
            }
            "--verify" => {
                opts.force_verifier = true;
                opts.skip_verifier = false;
                rest = after;
            }
            "--no-verify" => {
                opts.skip_verifier = true;
                opts.force_verifier = false;
                rest = after;
            }
            "--fresh-epoch" => {
                opts.fresh_epoch = true;
                opts.fresh_epoch_override = Some(true);
                rest = after;
            }
            "--no-fresh-epoch" => {
                opts.fresh_epoch = false;
                opts.fresh_epoch_override = Some(false);
                rest = after;
            }
            "--rollback-on-stall" => {
                opts.rollback_on_stall = true;
                opts.rollback_on_stall_override = Some(true);
                opts.checkpoint_milestones = true;
                rest = after;
            }
            "--no-rollback-on-stall" => {
                opts.rollback_on_stall = false;
                opts.rollback_on_stall_override = Some(false);
                rest = after;
            }
            "--max-rounds" => {
                if let Some((num_tok, after_num)) = split_first_token(after) {
                    if let Ok(rounds) = num_tok.replace(['_', ','], "").parse::<u32>() {
                        opts.max_rounds = Some(rounds);
                        rest = after_num;
                        continue;
                    }
                }
                break;
            }
            other if other.starts_with("--max-rounds=") => {
                if let Ok(rounds) = other["--max-rounds=".len()..]
                    .replace(['_', ','], "")
                    .parse::<u32>()
                {
                    opts.max_rounds = Some(rounds);
                    rest = after;
                } else {
                    break;
                }
            }
            "--max-tokens" => {
                if let Some((num_tok, after_num)) = split_first_token(after) {
                    if let Ok(limit) = num_tok.replace(['_', ','], "").parse::<u64>() {
                        if limit > 0 {
                            opts.max_tokens = Some(limit);
                        }
                        rest = after_num;
                        continue;
                    }
                }
                break;
            }
            other if other.starts_with("--max-tokens=") => {
                if let Ok(limit) = other["--max-tokens=".len()..]
                    .replace(['_', ','], "")
                    .parse::<u64>()
                {
                    if limit > 0 {
                        opts.max_tokens = Some(limit);
                    }
                    rest = after;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    opts.resolve_defaults(defaults);
    (rest.trim().to_string(), opts)
}

fn split_first_token(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.find(char::is_whitespace) {
        Some(idx) => Some((&trimmed[..idx], trimmed[idx..].trim_start())),
        None => Some((trimmed, "")),
    }
}

/// Resolve the no-progress stall circuit breaker limit (`JEIKCODE_GOAL_STALL_LIMIT`,
/// `0` disables the breaker; default `3`).
pub(crate) fn goal_stall_limit_from_env() -> u32 {
    std::env::var("JEIKCODE_GOAL_STALL_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_GOAL_STALL_LIMIT)
}

/// Deterministic workspace state fingerprint used to detect real progress and
/// invalidate stale verification verdicts whenever the working tree changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceFingerprint {
    pub is_git_repo: bool,
    pub head_sha: Option<String>,
    pub hash: String,
    pub changed_files: Vec<String>,
    pub untracked_files: Vec<String>,
}

/// Capture a bounded `git diff --stat HEAD` summary asynchronously for blind-verifier
/// prompts (`#50`) — called on demand only when the blind verifier actually runs.
pub(crate) async fn compute_git_diff_stat_async(working_dir: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        if !is_git_work_tree(&working_dir) {
            return None;
        }
        let raw = git_stdout(&working_dir, &["diff", "--stat", "HEAD"])
            .or_else(|_| git_stdout(&working_dir, &["diff", "--stat"]))
            .ok()?;
        bound_diff_stat(&raw)
    })
    .await
    .ok()
    .flatten()
}

impl WorkspaceFingerprint {
    pub(crate) fn has_workspace_changes(&self) -> bool {
        !self.changed_files.is_empty()
    }
}

/// Capture a deterministic workspace fingerprint synchronously (safe to run inside
/// `tokio::task::spawn_blocking` or unit tests).
pub(crate) fn compute_workspace_fingerprint(working_dir: &Path) -> WorkspaceFingerprint {
    if !working_dir.exists() {
        return WorkspaceFingerprint {
            is_git_repo: false,
            head_sha: None,
            hash: "missing-dir".to_string(),
            changed_files: Vec::new(),
            untracked_files: Vec::new(),
        };
    }

    if let Some((is_wt, head_sha)) = git_work_tree_and_head(working_dir) {
        if !is_wt {
            // Fall through to non-git fallback below.
        } else {
            let status_raw =
                git_stdout_bytes_readonly(working_dir, &["status", "--porcelain=v1", "-z"])
                    .unwrap_or_default();
            let (changed_files, untracked_files, has_staged, has_unstaged) =
                parse_porcelain_z_detailed(&status_raw);
            let diff_work = if has_unstaged {
                git_stdout_bytes_readonly(working_dir, &["diff", "--no-ext-diff", "--binary"])
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let diff_cached = if has_staged {
                git_stdout_bytes_readonly(
                    working_dir,
                    &["diff", "--cached", "--no-ext-diff", "--binary"],
                )
                .unwrap_or_default()
            } else {
                Vec::new()
            };

            let mut hasher = Fnv1a128::new();
            hasher.update(b"git-v1\0");
            if let Some(head) = &head_sha {
                hasher.update(head.as_bytes());
            } else {
                hasher.update(b"UNBORN");
            }
            hasher.update(b"\0status:");
            hasher.update(&status_raw);
            hasher.update(b"\0diff:");
            hasher.update(&diff_work);
            hasher.update(b"\0cached:");
            hasher.update(&diff_cached);

            for rel in &untracked_files {
                hasher.update(b"\0untracked:");
                hasher.update(rel.as_bytes());
                let full = working_dir.join(rel);
                if let Ok(meta) = std::fs::metadata(&full) {
                    hasher.update(&meta.len().to_le_bytes());
                    if let Ok(modified) = meta.modified() {
                        if let Ok(dur) = modified.duration_since(UNIX_EPOCH) {
                            hasher.update(&dur.as_nanos().to_le_bytes());
                        }
                    }
                    if meta.is_file() && meta.len() <= 64 * 1024 {
                        if let Ok(bytes) = std::fs::read(&full) {
                            hasher.update(&bytes);
                        }
                    }
                }
            }

            return WorkspaceFingerprint {
                is_git_repo: true,
                head_sha,
                hash: hasher.finish_hex(),
                changed_files,
                untracked_files,
            };
        }
    }

    // Non-git fallback: hash top-level entries + lengths + mtimes so tests or non-git
    // directories still detect local file edits without recursive walks.
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(working_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            entries.push((name, len, mtime));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Fnv1a128::new();
    hasher.update(b"nongit-v1\0");
    for (name, len, mtime) in entries {
        hasher.update(name.as_bytes());
        hasher.update(&len.to_le_bytes());
        hasher.update(&mtime.to_le_bytes());
    }

    WorkspaceFingerprint {
        is_git_repo: false,
        head_sha: None,
        hash: hasher.finish_hex(),
        changed_files: Vec::new(),
        untracked_files: Vec::new(),
    }
}

fn bound_diff_stat(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let mut out = if lines.len() > MAX_DIFF_STAT_LINES {
        let mut s = lines[..MAX_DIFF_STAT_LINES - 1].join("\n");
        s.push_str(&format!(
            "\n... ({} more lines)\n{}",
            lines.len() - MAX_DIFF_STAT_LINES,
            lines[lines.len() - 1]
        ));
        s
    } else {
        trimmed.to_string()
    };
    if out.chars().count() > MAX_DIFF_STAT_CHARS {
        out = out.chars().take(MAX_DIFF_STAT_CHARS).collect::<String>();
        out.push_str("\n[truncated]");
    }
    Some(out)
}

pub(crate) async fn compute_workspace_fingerprint_async(
    working_dir: std::path::PathBuf,
) -> WorkspaceFingerprint {
    tokio::task::spawn_blocking(move || compute_workspace_fingerprint(&working_dir))
        .await
        .unwrap_or_else(|_| WorkspaceFingerprint {
            is_git_repo: false,
            head_sha: None,
            hash: "fingerprint-error".to_string(),
            changed_files: Vec::new(),
            untracked_files: Vec::new(),
        })
}

/// Create a non-destructive, worktree- & Windows-safe git checkpoint ref under
/// `refs/jeikcode/goal-checkpoints/` capturing tracked changes plus bounded,
/// non-locked untracked files (`<= 1 MiB`) in a `git stash apply`-compatible
/// 3-parent stash commit without altering `HEAD`, the live index, the working tree,
/// or `refs/stash`. Automatically prunes old refs beyond `MAX_CHECKPOINTS_PER_GOAL`.
pub(crate) fn create_milestone_checkpoint(
    working_dir: &Path,
    goal_id: u64,
    round: u32,
) -> Result<Option<String>, String> {
    if !is_git_work_tree(working_dir) {
        return Ok(None);
    }
    let head = match git_stdout(working_dir, &["rev-parse", "--verify", "HEAD"]) {
        Ok(h) => h.trim().to_string(),
        Err(_) => return Ok(None),
    };
    if head.is_empty() {
        return Ok(None);
    }

    let msg = format!("jeikcode goal {goal_id} milestone checkpoint round {round}");
    let target_sha = build_stash_compatible_checkpoint_commit(working_dir, &head, &msg)
        .unwrap_or_else(|| {
            let stash_out = git_stdout(working_dir, &["stash", "create", &msg])
                .unwrap_or_default()
                .trim()
                .to_string();
            if stash_out.is_empty() {
                head.clone()
            } else {
                stash_out
            }
        });

    let ref_name = format!("refs/jeikcode/goal-checkpoints/goal-{goal_id}-r{round}");
    let out = git_command(working_dir)
        .args(["update-ref", &ref_name, &target_sha])
        .output()
        .map_err(|e| format!("git update-ref spawn failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("git update-ref failed: {err}"));
    }
    prune_old_goal_checkpoints(working_dir, goal_id, MAX_CHECKPOINTS_PER_GOAL);
    Ok(Some(ref_name))
}

struct TempCheckpointDir(PathBuf);

impl TempCheckpointDir {
    fn create() -> Option<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "jeikcode-goal-cp-{}-{seq}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok()?;
        Some(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempCheckpointDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a `git stash apply`-compatible 2- or 3-parent checkpoint commit using
/// temporary `GIT_INDEX_FILE`s resolved via `git rev-parse --git-path index`
/// (worktree-safe). Bounded untracked files (`<= 1 MiB`) that are readable on
/// Windows are staged individually into parent 3 so a single locked file never
/// aborts the checkpoint.
fn build_stash_compatible_checkpoint_commit(
    working_dir: &Path,
    head: &str,
    msg: &str,
) -> Option<String> {
    // 1. Capture untracked files (excluding .gitignore) via `-z` porcelain.
    let untracked_raw = git_stdout_bytes(
        working_dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .ok()?;
    let untracked: Vec<String> = untracked_raw
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .map(|e| String::from_utf8_lossy(e).into_owned())
        .collect();

    // If there are no untracked files, standard `git stash create` already gives
    // a fast, native 2-parent stash commit (or empty string when completely clean).
    if untracked.is_empty() {
        return None;
    }

    let tmp_dir = TempCheckpointDir::create()?;
    let live_index_rel = git_stdout(working_dir, &["rev-parse", "--git-path", "index"]).ok()?;
    let live_index = resolve_git_path(working_dir, live_index_rel.trim());

    // 2. Snapshot current index tree (parent 2).
    let index_tree = git_stdout(working_dir, &["write-tree"])
        .ok()?
        .trim()
        .to_string();
    if index_tree.is_empty() {
        return None;
    }
    let index_commit = git_commit_tree(
        working_dir,
        &index_tree,
        &[head],
        &format!("index on {msg}"),
    )?;

    // 3. Snapshot tracked working-tree changes into a copied temp index.
    let wt_index = tmp_dir.path().join("wt.index");
    if live_index.is_file() {
        std::fs::copy(&live_index, &wt_index).ok()?;
    }
    let mut add_u = git_command(working_dir);
    add_u.env("GIT_INDEX_FILE", &wt_index).args(["add", "-u"]);
    if !add_u.output().ok()?.status.success() {
        return None;
    }
    let wt_tree = git_stdout_with_index(working_dir, &wt_index, &["write-tree"])
        .ok()?
        .trim()
        .to_string();
    if wt_tree.is_empty() {
        return None;
    }

    // 4. Stage readable, bounded (<= 1 MiB) untracked files into an empty temp index
    //    via `hash-object -w` + `update-index --add --cacheinfo` so a single locked
    //    file on Windows is skipped rather than failing the whole checkpoint.
    let untracked_index = tmp_dir.path().join("untracked.index");
    let mut staged_untracked = 0usize;
    for rel in &untracked {
        let full = working_dir.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_CHECKPOINT_UNTRACKED_BYTES {
            continue;
        }
        // Quick readability check (skips exclusively-locked Windows DB/log files).
        if std::fs::File::open(&full).is_err() {
            continue;
        }
        let Ok(hash_out) = git_command(working_dir)
            .args(["hash-object", "-w", "--"])
            .arg(&full)
            .output()
        else {
            continue;
        };
        if !hash_out.status.success() {
            continue;
        }
        let blob = String::from_utf8_lossy(&hash_out.stdout).trim().to_string();
        if blob.is_empty() {
            continue;
        }
        let cacheinfo = format!("100644,{blob},{rel}");
        if let Ok(upd) = git_command(working_dir)
            .env("GIT_INDEX_FILE", &untracked_index)
            .args(["update-index", "--add", "--cacheinfo", &cacheinfo])
            .output()
        {
            if upd.status.success() {
                staged_untracked += 1;
            }
        }
    }

    let mut parents: Vec<&str> = vec![head, index_commit.as_str()];
    let untracked_commit_owned;
    if staged_untracked > 0 {
        if let Ok(u_tree) = git_stdout_with_index(working_dir, &untracked_index, &["write-tree"]) {
            let u_tree = u_tree.trim();
            if !u_tree.is_empty() {
                if let Some(uc) = git_commit_tree(
                    working_dir,
                    u_tree,
                    &[],
                    &format!("untracked files on {msg}"),
                ) {
                    untracked_commit_owned = uc;
                    parents.push(untracked_commit_owned.as_str());
                }
            }
        }
    }

    git_commit_tree(working_dir, &wt_tree, &parents, msg)
}

fn resolve_git_path(working_dir: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        working_dir.join(p)
    }
}

fn git_commit_tree(
    working_dir: &Path,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> Option<String> {
    let mut cmd = git_command(working_dir);
    cmd.args(["commit-tree", tree]);
    for p in parents {
        cmd.args(["-p", p]);
    }
    cmd.args(["-m", message]);
    // Ensure commit-tree works even in repos without global git user config.
    cmd.env("GIT_AUTHOR_NAME", "JeikCode")
        .env("GIT_AUTHOR_EMAIL", "jeikcode@local")
        .env("GIT_COMMITTER_NAME", "JeikCode")
        .env("GIT_COMMITTER_EMAIL", "jeikcode@local");
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

fn prune_old_goal_checkpoints(working_dir: &Path, goal_id: u64, keep: usize) {
    let prefix = format!("refs/jeikcode/goal-checkpoints/goal-{goal_id}-r");
    let Ok(out) = git_stdout(
        working_dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/jeikcode/goal-checkpoints/goal-{goal_id}-r*"),
        ],
    ) else {
        return;
    };
    let mut refs: Vec<(u32, String)> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|ref_name| {
            let round = ref_name
                .strip_prefix(&prefix)
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            (round, ref_name.to_string())
        })
        .collect();
    if refs.len() <= keep {
        return;
    }
    refs.sort_by_key(|(round, _)| *round);
    let drop_count = refs.len() - keep;
    for (_, stale_ref) in refs.into_iter().take(drop_count) {
        let _ = git_command(working_dir)
            .args(["update-ref", "-d", &stale_ref])
            .output();
    }
}

/// Restore the working tree back to a recorded milestone checkpoint ref without
/// moving `HEAD` or deleting pre-existing user untracked files (`baseline_untracked`).
pub(crate) fn restore_milestone_checkpoint(
    working_dir: &Path,
    ref_name: &str,
    baseline_untracked: &[String],
) -> Result<String, String> {
    if !is_git_work_tree(working_dir) {
        return Err("workspace is not a git work tree".to_string());
    }
    git_stdout(working_dir, &["rev-parse", "--verify", ref_name])
        .map_err(|e| format!("checkpoint ref `{ref_name}` not found: {e}"))?;

    // Reset staged index back to HEAD first so working-tree checkout is clean.
    let reset = git_command(working_dir)
        .args(["reset", "--mixed", "-q", "HEAD"])
        .output()
        .map_err(|e| format!("git reset failed: {e}"))?;
    if !reset.status.success() {
        return Err(String::from_utf8_lossy(&reset.stderr).trim().to_string());
    }

    // Restore tracked files from the checkpoint worktree commit.
    let checkout = git_command(working_dir)
        .args(["checkout", ref_name, "--", "."])
        .output()
        .map_err(|e| format!("git checkout failed: {e}"))?;
    if !checkout.status.success() {
        return Err(String::from_utf8_lossy(&checkout.stderr).trim().to_string());
    }

    // Re-unstage tracked files checked out from the checkpoint so git index matches HEAD.
    let _ = git_command(working_dir)
        .args(["reset", "--mixed", "-q", "HEAD"])
        .output();

    // Restore checkpointed untracked files (parent 3, if present) and record them as allowed.
    let mut allowed_untracked: HashSet<String> = baseline_untracked.iter().cloned().collect();
    let parent3 = format!("{ref_name}^3");
    if git_stdout(working_dir, &["rev-parse", "--verify", &parent3]).is_ok() {
        if let Ok(raw_list) = git_stdout_bytes(
            working_dir,
            &["ls-tree", "-r", "--name-only", "-z", &parent3],
        ) {
            for entry in raw_list.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                allowed_untracked.insert(String::from_utf8_lossy(entry).into_owned());
            }
        }
        let _ = git_command(working_dir)
            .args(["checkout", &parent3, "--", "."])
            .output();
        let _ = git_command(working_dir)
            .args(["reset", "--mixed", "-q", "HEAD"])
            .output();
    }

    // Remove only agent-created untracked files introduced AFTER the checkpoint.
    if let Ok(cur_untracked) = git_stdout_bytes(
        working_dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ) {
        for entry in cur_untracked.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let rel = String::from_utf8_lossy(entry).into_owned();
            if !allowed_untracked.contains(&rel) {
                let target = working_dir.join(&rel);
                if target.is_file() {
                    let _ = std::fs::remove_file(&target);
                }
            }
        }
    }

    Ok(format!(
        "restored workspace to milestone checkpoint `{ref_name}`"
    ))
}

pub(crate) async fn restore_milestone_checkpoint_async(
    working_dir: PathBuf,
    ref_name: String,
    baseline_untracked: Vec<String>,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        restore_milestone_checkpoint(&working_dir, &ref_name, &baseline_untracked)
    })
    .await
    .map_err(|e| format!("rollback task failed: {e}"))?
}

pub(crate) async fn create_milestone_checkpoint_async(
    working_dir: std::path::PathBuf,
    goal_id: u64,
    round: u32,
) -> Result<Option<String>, String> {
    tokio::task::spawn_blocking(move || create_milestone_checkpoint(&working_dir, goal_id, round))
        .await
        .map_err(|e| format!("checkpoint task failed: {e}"))?
}

/// Tracks identical-state rounds AND oscillating edit loops (`A -> B -> A`) to
/// detect stuck autonomous `/goal` runs.
#[derive(Clone, Debug, Default)]
pub(crate) struct StallTracker {
    pub baseline_fingerprint: Option<String>,
    pub baseline_untracked: Vec<String>,
    pub last_fingerprint: Option<String>,
    pub last_verified_fingerprint: Option<String>,
    pub last_reason_sig: Option<String>,
    pub consecutive_stalls: u32,
    pub recent_fingerprints: VecDeque<String>,
    pub oscillation_hits: u32,
    pub rounds_since_verified_progress: u32,
}

impl StallTracker {
    /// Record an evaluation round (`NotMet`). Returns `true` if `stall_limit > 0`
    /// and either:
    /// - consecutive identical unproductive rounds reached `stall_limit`, OR
    /// - an oscillating fingerprint cycle (`A -> B -> A`) repeated, OR
    /// - `rounds_since_verified_progress` reached `2 * stall_limit` without advancing.
    pub(crate) fn record_not_met(
        &mut self,
        fingerprint: &WorkspaceFingerprint,
        reason: &str,
        had_snapshot_mutations: bool,
        stall_limit: u32,
    ) -> bool {
        let sig = normalize_reason_signature(reason);
        let same_fp = self
            .last_fingerprint
            .as_deref()
            .is_some_and(|prev| prev == fingerprint.hash);
        let same_sig = self
            .last_reason_sig
            .as_deref()
            .is_some_and(|prev| prev == sig);

        if !had_snapshot_mutations && same_fp && same_sig {
            self.consecutive_stalls = self.consecutive_stalls.saturating_add(1);
        } else {
            self.consecutive_stalls = 1;
        }

        // Detect oscillating edit loops (revisiting an earlier fingerprint distinct
        // from the immediate previous round, e.g. A -> B -> A).
        let revisited_earlier = !same_fp
            && self
                .recent_fingerprints
                .iter()
                .any(|seen| seen == &fingerprint.hash);
        if revisited_earlier {
            self.oscillation_hits = self.oscillation_hits.saturating_add(1);
        }
        if !same_fp {
            self.recent_fingerprints.push_back(fingerprint.hash.clone());
            while self.recent_fingerprints.len() > STALL_FINGERPRINT_RING_CAP {
                self.recent_fingerprints.pop_front();
            }
        }
        self.rounds_since_verified_progress = self.rounds_since_verified_progress.saturating_add(1);

        self.last_fingerprint = Some(fingerprint.hash.clone());
        self.last_reason_sig = Some(sig);

        if stall_limit == 0 {
            return false;
        }
        let oscillation_tripped =
            revisited_earlier && self.oscillation_hits >= stall_limit.saturating_sub(1).max(1);
        let futile_budget_tripped =
            self.rounds_since_verified_progress >= stall_limit.saturating_mul(2).max(4);
        self.consecutive_stalls >= stall_limit || oscillation_tripped || futile_budget_tripped
    }

    pub(crate) fn reset_stalls(&mut self) {
        self.consecutive_stalls = 0;
        self.oscillation_hits = 0;
        self.rounds_since_verified_progress = 0;
        self.recent_fingerprints.clear();
    }

    pub(crate) fn mark_verified(&mut self, fingerprint_hash: String) {
        self.last_verified_fingerprint = Some(fingerprint_hash);
        self.reset_stalls();
    }
}

fn normalize_reason_signature(reason: &str) -> String {
    reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn git_command(working_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(working_dir);
    suppress_console_window_sync(&mut cmd);
    cmd
}

/// Read-only git command that passes `--no-optional-locks` so concurrent status/diff
/// probes across mesh sessions or tests never contend on `.git/index.lock`.
fn git_command_readonly(working_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks").arg("-C").arg(working_dir);
    suppress_console_window_sync(&mut cmd);
    cmd
}

fn git_work_tree_and_head(working_dir: &Path) -> Option<(bool, Option<String>)> {
    let out = git_command_readonly(working_dir)
        .args(["rev-parse", "--is-inside-work-tree", "HEAD"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines().map(str::trim);
    let is_wt = lines.next()? == "true";
    if !is_wt {
        return Some((false, None));
    }
    let head = if out.status.success() {
        lines
            .next()
            .map(str::to_string)
            .filter(|s| !s.is_empty() && s != "HEAD")
    } else {
        None
    };
    Some((true, head))
}

fn is_git_work_tree(working_dir: &Path) -> bool {
    git_stdout_readonly(working_dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

fn git_stdout_readonly(working_dir: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = git_stdout_bytes_readonly(working_dir, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn git_stdout_bytes_readonly(working_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = git_command_readonly(working_dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn git_stdout(working_dir: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = git_stdout_bytes(working_dir, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn git_stdout_with_index(
    working_dir: &Path,
    index_path: &Path,
    args: &[&str],
) -> Result<String, String> {
    let output = git_command(working_dir)
        .env("GIT_INDEX_FILE", index_path)
        .args(args)
        .output()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_stdout_bytes(working_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = git_command(working_dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn parse_porcelain_z_detailed(raw: &[u8]) -> (Vec<String>, Vec<String>, bool, bool) {
    let mut changed = Vec::new();
    let mut untracked = Vec::new();
    let mut has_staged = false;
    let mut has_unstaged = false;
    let mut parts = raw.split(|b| *b == 0);
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        let path = String::from_utf8_lossy(&entry[3..]).to_string();
        if !path.is_empty() {
            if code == b"??" {
                untracked.push(path.clone());
            } else {
                if code[0] != b' ' && code[0] != b'?' {
                    has_staged = true;
                }
                if code[1] != b' ' && code[1] != b'?' {
                    has_unstaged = true;
                }
            }
            changed.push(path);
        }
        // Renames/copies in `-z` porcelain emit an extra NUL-terminated source path.
        if code[0] == b'R' || code[0] == b'C' || code[1] == b'R' || code[1] == b'C' {
            let _ = parts.next();
        }
    }
    (changed, untracked, has_staged, has_unstaged)
}

struct Fnv1a128(u128);

impl Fnv1a128 {
    const OFFSET: u128 = 0x6c62272e07bb0142_62b821756295c58d;
    const PRIME: u128 = 0x0000000001000000_000000000000013b;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u128;
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish_hex(self) -> String {
        format!("{:032x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git_repo(dir: &Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Tester"],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("git init");
            assert!(status.success());
        }
        std::fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        let add = Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(add.success());
        let commit = Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(commit.success());
    }

    #[test]
    fn parse_goal_launch_handles_flags_and_preserves_plain_conditions() {
        let (cond, opts) = parse_goal_launch("make all tests pass");
        assert_eq!(cond, "make all tests pass");
        assert!(!opts.continuous);
        assert!(opts.rollback_on_stall);
        assert!(opts.checkpoint_milestones);
        assert!(!opts.fresh_epoch);
        assert_eq!(opts.max_rounds, None);

        let (cond2, opts2) = parse_goal_launch("--until-stopped fix remaining bugs");
        assert_eq!(cond2, "fix remaining bugs");
        assert!(opts2.continuous);
        assert!(opts2.checkpoint_milestones);
        assert!(opts2.fresh_epoch);
        assert!(opts2.rollback_on_stall);

        let (cond3, opts3) = parse_goal_launch("--verify --checkpoint check auth flow");
        assert_eq!(cond3, "check auth flow");
        assert!(opts3.force_verifier);
        assert!(opts3.checkpoint_milestones);

        let (cond4, opts4) = parse_goal_launch("shift --max-rounds 20 harden persistence");
        assert_eq!(cond4, "harden persistence");
        assert!(opts4.continuous);
        assert!(opts4.fresh_epoch);
        assert!(opts4.rollback_on_stall);
        assert_eq!(opts4.max_rounds, Some(20));

        let (cond5, opts5) = parse_goal_launch(
            "--until-stopped --no-fresh-epoch --no-rollback-on-stall --max-rounds=15 keep context",
        );
        assert_eq!(cond5, "keep context");
        assert!(opts5.continuous);
        assert!(!opts5.fresh_epoch);
        assert!(!opts5.rollback_on_stall);
        assert_eq!(opts5.max_rounds, Some(15));

        let (cond6, opts6) = parse_goal_launch_with_defaults(
            "audit codebase",
            GoalLaunchDefaults {
                rollback_on_stall: false,
                fresh_epoch: Some(true),
            },
        );
        assert_eq!(cond6, "audit codebase");
        assert!(!opts6.rollback_on_stall);
        assert!(opts6.fresh_epoch);

        // Flag-only inputs without a trailing prompt must consume all valid flags
        // and leave condition empty so callers reject them with GoalPromptRequired.
        let (empty_mr, opts_mr) = parse_goal_launch("--max-rounds 20");
        assert!(empty_mr.is_empty());
        assert_eq!(opts_mr.max_rounds, Some(20));

        let (empty_mt, opts_mt) = parse_goal_launch("--max-tokens 5000");
        assert!(empty_mt.is_empty());
        assert_eq!(opts_mt.max_tokens, Some(5000));

        let (empty_shift, opts_shift) = parse_goal_launch("shift --max-rounds 12");
        assert!(empty_shift.is_empty());
        assert!(opts_shift.continuous);
        assert_eq!(opts_shift.max_rounds, Some(12));
    }

    #[test]
    fn workspace_fingerprint_tracks_dirty_and_untracked_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());

        let fp_clean = compute_workspace_fingerprint(dir.path());
        assert!(fp_clean.is_git_repo);
        assert!(fp_clean.head_sha.is_some());
        assert!(fp_clean.changed_files.is_empty());

        std::fs::write(dir.path().join("lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        let fp_dirty = compute_workspace_fingerprint(dir.path());
        assert_ne!(fp_clean.hash, fp_dirty.hash);
        assert!(fp_dirty.changed_files.iter().any(|p| p == "lib.rs"));

        std::fs::write(dir.path().join("extra.txt"), "hello").unwrap();
        let fp_untracked = compute_workspace_fingerprint(dir.path());
        assert_ne!(fp_dirty.hash, fp_untracked.hash);
        assert!(fp_untracked.changed_files.iter().any(|p| p == "extra.txt"));
    }

    #[test]
    fn milestone_checkpoint_creates_ref_without_modifying_head_or_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        let head_before = git_stdout(dir.path(), &["rev-parse", "HEAD"]).unwrap();

        std::fs::write(dir.path().join("lib.rs"), "pub fn v() -> i32 { 99 }\n").unwrap();
        let ref_name = create_milestone_checkpoint(dir.path(), 7, 3)
            .expect("checkpoint should succeed")
            .expect("git repo should return ref");
        assert_eq!(ref_name, "refs/jeikcode/goal-checkpoints/goal-7-r3");

        let head_after = git_stdout(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head_before, head_after);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
            "pub fn v() -> i32 { 99 }\n"
        );
        let ref_sha = git_stdout(dir.path(), &["rev-parse", "--verify", &ref_name]).unwrap();
        assert_ne!(ref_sha.trim(), head_before.trim());
    }

    #[test]
    fn stall_tracker_trips_only_on_consecutive_identical_unproductive_rounds() {
        let fp = WorkspaceFingerprint {
            is_git_repo: true,
            head_sha: Some("abc".into()),
            hash: "fp-1".into(),
            changed_files: vec!["a.rs".into()],
            untracked_files: vec![],
        };
        let mut tracker = StallTracker::default();
        assert!(!tracker.record_not_met(&fp, "2 tests still fail", false, 3));
        assert!(!tracker.record_not_met(&fp, "2 tests still fail", false, 3));
        assert!(tracker.record_not_met(&fp, "2 tests still fail", false, 3));

        // A workspace change resets the stall counter back to 1.
        let fp2 = WorkspaceFingerprint {
            hash: "fp-2".into(),
            ..fp.clone()
        };
        assert!(!tracker.record_not_met(&fp2, "2 tests still fail", false, 3));
        assert_eq!(tracker.consecutive_stalls, 1);

        // Oscillating back to fp-1 -> fp-2 -> fp-1 trips the oscillation breaker.
        let mut osc = StallTracker::default();
        assert!(!osc.record_not_met(&fp, "broken", true, 3));
        assert!(!osc.record_not_met(&fp2, "broken", true, 3));
        assert!(!osc.record_not_met(&fp, "broken", true, 3));
        assert!(osc.record_not_met(&fp2, "broken", true, 3));
    }

    #[test]
    fn milestone_checkpoint_captures_untracked_files_and_restores_safely() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        // Pre-existing user untracked file must never be deleted on rollback.
        std::fs::write(dir.path().join("user_notes.txt"), "keep me").unwrap();
        let baseline = compute_workspace_fingerprint(dir.path());

        std::fs::write(dir.path().join("lib.rs"), "pub fn v() -> i32 { 42 }\n").unwrap();
        std::fs::write(
            dir.path().join("new_mod.rs"),
            "pub fn good() -> bool { true }\n",
        )
        .unwrap();

        let cp = create_milestone_checkpoint(dir.path(), 9, 1)
            .expect("checkpoint should succeed")
            .expect("ref expected");

        // Now corrupt lib.rs, break new_mod.rs, and create a junk agent file.
        std::fs::write(dir.path().join("lib.rs"), "BROKEN\n").unwrap();
        std::fs::write(dir.path().join("new_mod.rs"), "BROKEN\n").unwrap();
        std::fs::write(dir.path().join("junk_agent.rs"), "junk").unwrap();

        restore_milestone_checkpoint(dir.path(), &cp, &baseline.untracked_files)
            .expect("rollback must succeed");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib.rs"))
                .unwrap()
                .replace("\r\n", "\n"),
            "pub fn v() -> i32 { 42 }\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new_mod.rs"))
                .unwrap()
                .replace("\r\n", "\n"),
            "pub fn good() -> bool { true }\n"
        );
        assert!(
            dir.path().join("user_notes.txt").is_file(),
            "pre-existing user untracked file must be preserved"
        );
        assert!(
            !dir.path().join("junk_agent.rs").exists(),
            "post-checkpoint agent untracked file must be cleaned on rollback"
        );
    }
}
