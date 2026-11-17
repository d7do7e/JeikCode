//! Autonomous `/goal` verification harness:
//! - Change-driven workspace git-tree fingerprinting (`HEAD` + staged/unstaged diff + untracked state)
//! - Non-destructive milestone git checkpoints (`refs/jeikcode/goal-checkpoints/...`)
//! - Identical-state no-progress circuit breaker
//! - `/goal` flag parsing (`--until-stopped` / `--continuous`, `--checkpoint`, `--verify`, `--no-verify`)

use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Default consecutive identical-state rounds before the circuit breaker trips.
pub(crate) const DEFAULT_GOAL_STALL_LIMIT: u32 = 3;

/// Default round batch size per continuous rollover checkpoint when `--until-stopped`
/// is used and the user's configured `goal_max_rounds` is `0` or smaller.
pub(crate) const DEFAULT_CONTINUOUS_CHECKPOINT_INTERVAL: u32 = 25;

/// Parsed `/goal` launch options and stripped condition text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoalOptions {
    pub continuous: bool,
    pub checkpoint_milestones: bool,
    pub force_verifier: bool,
    pub skip_verifier: bool,
}

/// Parse optional leading flags from `/goal <flags> <condition>`.
///
/// Supported flags:
/// - `--until-stopped` / `--continuous` — roll over round budgets continuously with checkpoints
/// - `--checkpoint` / `--checkpoints` — record non-destructive git checkpoint refs at milestones
/// - `--verify` — require the blind `verifier` subagent even when no file edits are detected
/// - `--no-verify` — skip the blind `verifier` subagent (text-only evaluator)
pub fn parse_goal_launch(raw: &str) -> (String, GoalOptions) {
    let mut opts = GoalOptions::default();
    let mut rest = raw.trim();

    loop {
        let trimmed = rest.trim_start();
        let Some((token, after)) = split_first_token(trimmed) else {
            break;
        };
        if after.trim().is_empty() {
            // Do not consume the final word as a flag if it would leave an empty condition.
            break;
        }
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
            _ => break,
        }
    }

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
        };
    }

    if is_git_work_tree(working_dir) {
        let head_sha = git_stdout(working_dir, &["rev-parse", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let status_raw =
            git_stdout_bytes(working_dir, &["status", "--porcelain=v1", "-z"]).unwrap_or_default();
        let diff_work = git_stdout_bytes(working_dir, &["diff", "--no-ext-diff", "--binary"])
            .unwrap_or_default();
        let diff_cached = git_stdout_bytes(
            working_dir,
            &["diff", "--cached", "--no-ext-diff", "--binary"],
        )
        .unwrap_or_default();

        let (changed_files, untracked_files) = parse_porcelain_z(&status_raw);
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
        };
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
    }
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
        })
}

/// Create a non-destructive git checkpoint ref under `refs/jeikcode/goal-checkpoints/`
/// capturing the current working tree (dirty state via `git stash create`, or `HEAD`
/// when clean) without altering `HEAD`, the index, the working tree, or `refs/stash`.
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
    let stash_out = git_stdout(working_dir, &["stash", "create", &msg])
        .unwrap_or_default()
        .trim()
        .to_string();
    let target_sha = if stash_out.is_empty() {
        head
    } else {
        stash_out
    };
    let ref_name = format!("refs/jeikcode/goal-checkpoints/goal-{goal_id}-r{round}");
    let out = Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(["update-ref", &ref_name, &target_sha])
        .output()
        .map_err(|e| format!("git update-ref spawn failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("git update-ref failed: {err}"));
    }
    Ok(Some(ref_name))
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

/// Tracks identical-state rounds to detect stuck autonomous `/goal` loops.
#[derive(Clone, Debug, Default)]
pub(crate) struct StallTracker {
    pub baseline_fingerprint: Option<String>,
    pub last_fingerprint: Option<String>,
    pub last_verified_fingerprint: Option<String>,
    pub last_reason_sig: Option<String>,
    pub consecutive_stalls: u32,
}

impl StallTracker {
    /// Record an evaluation round (`NotMet`). Returns `true` if `stall_limit > 0`
    /// and the consecutive identical-state stall counter reached `stall_limit`.
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
        self.last_fingerprint = Some(fingerprint.hash.clone());
        self.last_reason_sig = Some(sig);

        stall_limit > 0 && self.consecutive_stalls >= stall_limit
    }

    pub(crate) fn reset_stalls(&mut self) {
        self.consecutive_stalls = 0;
    }

    pub(crate) fn mark_verified(&mut self, fingerprint_hash: String) {
        self.last_verified_fingerprint = Some(fingerprint_hash);
    }
}

fn normalize_reason_signature(reason: &str) -> String {
    reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn is_git_work_tree(working_dir: &Path) -> bool {
    git_stdout(working_dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

fn git_stdout(working_dir: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = git_stdout_bytes(working_dir, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn git_stdout_bytes(working_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn parse_porcelain_z(raw: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut changed = Vec::new();
    let mut untracked = Vec::new();
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
            }
            changed.push(path);
        }
        // Renames/copies in `-z` porcelain emit an extra NUL-terminated source path.
        if code[0] == b'R' || code[0] == b'C' || code[1] == b'R' || code[1] == b'C' {
            let _ = parts.next();
        }
    }
    (changed, untracked)
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
        assert!(!opts.checkpoint_milestones);

        let (cond2, opts2) = parse_goal_launch("--until-stopped fix remaining bugs");
        assert_eq!(cond2, "fix remaining bugs");
        assert!(opts2.continuous);
        assert!(opts2.checkpoint_milestones);

        let (cond3, opts3) = parse_goal_launch("--verify --checkpoint check auth flow");
        assert_eq!(cond3, "check auth flow");
        assert!(opts3.force_verifier);
        assert!(opts3.checkpoint_milestones);
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
        };
        let mut tracker = StallTracker::default();
        assert!(!tracker.record_not_met(&fp, "2 tests still fail", false, 3));
        assert!(!tracker.record_not_met(&fp, "2 tests still fail", false, 3));
        assert!(tracker.record_not_met(&fp, "2 tests still fail", false, 3));

        // A workspace change resets the stall counter back to 1.
        let fp2 = WorkspaceFingerprint {
            hash: "fp-2".into(),
            ..fp
        };
        assert!(!tracker.record_not_met(&fp2, "2 tests still fail", false, 3));
        assert_eq!(tracker.consecutive_stalls, 1);
    }
}
