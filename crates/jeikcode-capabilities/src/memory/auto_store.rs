//! Two-tier git-root-keyed auto-memory store (`MEMORY.md` compact index + `topics/*.md`).
//!
//! Tier 1 (`MEMORY.md`): strictly bounded to `<= 150` lines and `<= 8 KiB`, loaded into
//! the frozen `<memory>` block at session start so prompt overhead stays tiny.
//! Tier 2 (`topics/<slug>.md`): detailed durable knowledge fetched on demand via the
//! `memory` tool (`action:"recall"`), shared across git worktrees via `--git-common-dir`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Hard line ceiling for Tier-1 `MEMORY.md` index.
pub const MEMORY_INDEX_MAX_LINES: usize = 150;
/// Hard byte ceiling for Tier-1 `MEMORY.md` index (8 KiB).
pub const MEMORY_INDEX_MAX_BYTES: usize = 8 * 1024;
/// Hard byte ceiling per Tier-2 topic markdown file (16 KiB).
pub const MEMORY_TOPIC_MAX_BYTES: usize = 16 * 1024;
/// Maximum character length of a single Tier-1 `MEMORY.md` bullet entry.
pub const MEMORY_INDEX_ENTRY_MAX_CHARS: usize = 240;

static CANONICAL_REPO_ROOT_CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

/// Resolve the canonical project root for auto-memory keying.
///
/// Inside a git worktree, `git rev-parse --git-common-dir` returns the shared `.git`
/// directory across all worktrees; its parent is used as the canonical repository root.
/// Outside git repos, falls back to canonicalized `working_dir`.
pub fn resolve_canonical_repo_root(working_dir: &Path) -> PathBuf {
    let key = fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
    let cache = CANONICAL_REPO_ROOT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }

    let resolved = if working_dir.exists() {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(working_dir).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]);
        crate::process_utils::suppress_console_window_sync(&mut cmd);
        if let Ok(output) = cmd.output() {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !raw.is_empty() {
                    let common = PathBuf::from(&raw);
                    let abs_common = if common.is_absolute() {
                        common
                    } else {
                        working_dir.join(common)
                    };
                    let parent = abs_common
                        .file_name()
                        .and_then(|n| n.to_str())
                        .filter(|&n| n == ".git")
                        .and_then(|_| abs_common.parent())
                        .unwrap_or(&abs_common);
                    let canon = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
                    if let Ok(mut guard) = cache.lock() {
                        guard.insert(key, canon.clone());
                    }
                    return canon;
                }
            }
        }
        key.clone()
    } else {
        key.clone()
    };

    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, resolved.clone());
    }
    resolved
}

/// Deterministic 16-hex-char bucket hash for a canonical project root path.
pub fn project_memory_bucket(working_dir: &Path) -> String {
    let root = resolve_canonical_repo_root(working_dir);
    bucket_for_canonical_root(&root)
}

fn bucket_for_canonical_root(canonical_root: &Path) -> String {
    let normalized = normalize_path_for_hash(canonical_root);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn normalize_path_for_hash(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_end_matches('/');
    let clean = if trimmed.is_empty() { "/" } else { trimmed };
    PathBuf::from(clean)
}

/// Validate a topic slug (`[a-zA-Z0-9_-]{1,64}`), rejecting traversal, slashes, or dotfiles.
pub fn validate_topic_slug(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let slug = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    if slug.is_empty() || slug.len() > 64 {
        return Err("topic slug must be 1..=64 characters".to_string());
    }
    if slug.starts_with('.') {
        return Err("topic slug cannot start with '.'".to_string());
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("topic slug may contain only ASCII letters, digits, '-' and '_'".to_string());
    }
    Ok(slug.to_string())
}

/// Two-tier auto-memory directory manager (`MEMORY.md` + `topics/*.md`).
#[derive(Clone, Debug)]
pub struct AutoMemoryStore {
    root_dir: PathBuf,
    project_root_marker: Option<(PathBuf, String)>,
}

impl AutoMemoryStore {
    /// Standard project store keyed by canonical git-common-dir under `$JEIKCODE_HOME/projects/<bucket>/memory`
    /// (or `<working_dir>/<JEIKCODE_PROJECT_MEMORY_DIR>/auto` if an explicit project dir override is set).
    pub fn for_project(working_dir: &Path) -> Self {
        let override_dir = std::env::var("JEIKCODE_PROJECT_MEMORY_DIR").ok();
        Self::for_project_with_override(working_dir, override_dir.as_deref())
    }

    fn for_project_with_override(working_dir: &Path, override_dir: Option<&str>) -> Self {
        if let Some(trimmed) = override_dir.map(str::trim).filter(|s| !s.is_empty()) {
            return Self {
                root_dir: working_dir.join(trimmed).join("auto"),
                project_root_marker: None,
            };
        }
        let canonical_root = resolve_canonical_repo_root(working_dir);
        let bucket = bucket_for_canonical_root(&canonical_root);
        let bucket_dir = super::config_dir().join("projects").join(bucket);
        let root_dir = bucket_dir.join("memory");
        let normalized_root = crate::pathnorm::to_display(&canonical_root);
        Self {
            root_dir,
            project_root_marker: Some((bucket_dir.join(".project_root"), normalized_root)),
        }
    }

    /// Explicit directory root (primarily for isolated tests).
    pub fn with_root(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            project_root_marker: None,
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.root_dir.join("MEMORY.md")
    }

    pub fn topics_dir(&self) -> PathBuf {
        self.root_dir.join("topics")
    }

    pub fn topic_path(&self, slug: &str) -> Result<PathBuf, String> {
        let valid = validate_topic_slug(slug)?;
        Ok(self.topics_dir().join(format!("{valid}.md")))
    }

    /// Load raw lines from `MEMORY.md`, returning empty if absent.
    pub fn load_index_raw(&self) -> String {
        fs::read_to_string(self.index_path()).unwrap_or_default()
    }

    /// Load non-empty bullet entries from `MEMORY.md` (`- ...`).
    pub fn load_index_entries(&self) -> Vec<String> {
        self.load_index_raw()
            .lines()
            .filter_map(|line| line.trim().strip_prefix("- ").map(str::to_string))
            .filter(|s| !s.trim().is_empty())
            .collect()
    }

    /// Sorted list of available topic slugs in `topics/*.md`.
    pub fn list_topics(&self) -> Vec<String> {
        let dir = self.topics_dir();
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut slugs: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if !path.is_file() {
                    return None;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    return None;
                }
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| validate_topic_slug(s).is_ok())
                    .map(str::to_string)
            })
            .collect();
        slugs.sort();
        slugs
    }

    /// Append a concise bullet entry to `MEMORY.md`, enforcing deduplication and the
    /// `150` lines / `8 KiB` index budget. Returns `Ok(true)` if written, `Ok(false)` if duplicate.
    pub fn remember_index_entry(&self, entry: &str) -> Result<bool, String> {
        let clean = normalize_single_line(entry)?;
        if clean.chars().count() > MEMORY_INDEX_ENTRY_MAX_CHARS {
            return Err(format!(
                "index entry exceeds {MEMORY_INDEX_ENTRY_MAX_CHARS} chars; store details in a topic file (`topic:\"slug\"`) and index a short summary"
            ));
        }

        let existing = self.load_index_raw();
        let bullet = format!("- {clean}");
        if existing
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- "))
            .any(|e| e.trim().eq_ignore_ascii_case(&clean))
        {
            return Ok(false);
        }

        let candidate = append_line_text(&existing, &bullet);
        enforce_index_budget(&candidate)?;
        self.write_index(&candidate).map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Replace an existing `MEMORY.md` bullet containing `target` (or exact match) with `replacement`.
    pub fn update_index_entry(&self, target: &str, replacement: &str) -> Result<bool, String> {
        let target_clean = target.trim();
        if target_clean.is_empty() {
            return Err("target query cannot be empty".to_string());
        }
        let replacement_clean = normalize_single_line(replacement)?;
        if replacement_clean.chars().count() > MEMORY_INDEX_ENTRY_MAX_CHARS {
            return Err(format!(
                "replacement exceeds {MEMORY_INDEX_ENTRY_MAX_CHARS} chars"
            ));
        }

        let existing = self.load_index_raw();
        if existing.is_empty() {
            return Ok(false);
        }
        let target_lower = target_clean.to_ascii_lowercase();
        let mut updated = false;
        let mut out_lines = Vec::new();
        for line in existing.lines() {
            let trimmed = line.trim();
            if !updated {
                if let Some(body) = trimmed.strip_prefix("- ") {
                    if body.to_ascii_lowercase().contains(&target_lower) {
                        out_lines.push(format!("- {replacement_clean}"));
                        updated = true;
                        continue;
                    }
                }
            }
            out_lines.push(line.to_string());
        }
        if !updated {
            return Ok(false);
        }
        let mut next = out_lines.join("\n");
        if !next.is_empty() {
            next.push('\n');
        }
        enforce_index_budget(&next)?;
        self.write_index(&next).map_err(|e| e.to_string())?;
        Ok(true)
    }

    /// Remove `MEMORY.md` bullets matching `keyword` (case-insensitive).
    pub fn prune_index(&self, keyword: &str) -> io::Result<Vec<String>> {
        let existing = self.load_index_raw();
        if existing.is_empty() {
            return Ok(Vec::new());
        }
        let kw_lower = keyword.trim().to_ascii_lowercase();
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for line in existing.lines() {
            let trimmed = line.trim();
            if let Some(body) = trimmed.strip_prefix("- ") {
                if body.to_ascii_lowercase().contains(&kw_lower) {
                    removed.push(body.to_string());
                    continue;
                }
            }
            kept.push(line.to_string());
        }
        if !removed.is_empty() {
            let mut next = kept.join("\n");
            if !next.is_empty() && !next.ends_with('\n') {
                next.push('\n');
            }
            self.write_index(&next)?;
        }
        Ok(removed)
    }

    /// Read a Tier-2 topic markdown file.
    pub fn read_topic(&self, slug: &str) -> Result<String, String> {
        let path = self.topic_path(slug)?;
        fs::read_to_string(&path).map_err(|e| format!("topic `{slug}` not found: {e}"))
    }

    /// Write or append to a Tier-2 topic file (`topics/<slug>.md`) and ensure `MEMORY.md`
    /// contains a compact pointer to that topic.
    pub fn write_topic(
        &self,
        slug: &str,
        content: &str,
        summary: Option<&str>,
        append: bool,
    ) -> Result<PathBuf, String> {
        let valid_slug = validate_topic_slug(slug)?;
        let clean = content.trim();
        if clean.is_empty() {
            return Err("topic content cannot be empty".to_string());
        }

        let path = self.topics_dir().join(format!("{valid_slug}.md"));
        let next_body = if append && path.exists() {
            let prev = fs::read_to_string(&path).unwrap_or_default();
            if prev.trim().is_empty() {
                format!("# {valid_slug}\n\n{clean}\n")
            } else if prev.ends_with('\n') {
                format!("{prev}{clean}\n")
            } else {
                format!("{prev}\n{clean}\n")
            }
        } else if clean.starts_with('#') {
            format!("{clean}\n")
        } else {
            format!("# {valid_slug}\n\n{clean}\n")
        };

        if next_body.len() > MEMORY_TOPIC_MAX_BYTES {
            return Err(format!(
                "topic `{valid_slug}` would exceed {MEMORY_TOPIC_MAX_BYTES} bytes ({} bytes); prune or replace instead of appending",
                next_body.len()
            ));
        }

        // Compute and validate the index pointer update BEFORE touching disk so budget
        // violations never leave an orphaned unindexed topic.
        let pointer_prefix = format!("[topics/{valid_slug}.md]");
        let pointer_head = format!("- {pointer_prefix}(topics/{valid_slug}.md) — ");
        let max_summary_chars = MEMORY_INDEX_ENTRY_MAX_CHARS
            .saturating_add(2)
            .saturating_sub(pointer_head.chars().count())
            .max(16);
        let raw_summary = summary
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.lines().next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| first_meaningful_line(clean, max_summary_chars.min(120)));
        let short_summary = truncate_at_word_boundary(&raw_summary, max_summary_chars);
        let pointer_line = format!("{pointer_head}{short_summary}");

        let existing_index = self.load_index_raw();
        let next_index = upsert_topic_pointer(&existing_index, &pointer_prefix, &pointer_line);
        enforce_index_budget(&next_index)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        atomic_write_str(&path, &next_body).map_err(|e| e.to_string())?;
        self.write_index(&next_index).map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// Replace the first non-heading bullet/line matching `target` (case-insensitive) inside
    /// `topics/<slug>.md`, preserving `#` headings and enforcing `<= 16 KiB`.
    pub fn update_topic_entry(
        &self,
        slug: &str,
        target: &str,
        replacement: &str,
    ) -> Result<PathBuf, String> {
        let valid_slug = validate_topic_slug(slug)?;
        let target_clean = target.trim();
        if target_clean.is_empty() {
            return Err("target query cannot be empty".to_string());
        }
        let repl_trimmed = replacement.trim();
        if repl_trimmed.is_empty() {
            return Err("replacement content cannot be empty".to_string());
        }

        let path = self.topics_dir().join(format!("{valid_slug}.md"));
        let existing = fs::read_to_string(&path)
            .map_err(|e| format!("topic `{valid_slug}` not found: {e}"))?;
        let target_lower = target_clean.to_lowercase();
        let mut updated = false;
        let mut out_lines = Vec::new();

        for line in existing.lines() {
            let trimmed_start = line.trim_start();
            let indent = &line[..line.len() - trimmed_start.len()];
            if !updated
                && !trimmed_start.is_empty()
                && !trimmed_start.starts_with('#')
                && line.to_lowercase().contains(&target_lower)
            {
                if trimmed_start.starts_with("- ") {
                    if !repl_trimmed.contains('\n') && !repl_trimmed.contains('\r') {
                        let body = repl_trimmed
                            .strip_prefix("- ")
                            .unwrap_or(repl_trimmed)
                            .trim();
                        out_lines.push(format!("{indent}- {body}"));
                    } else {
                        out_lines.push(repl_trimmed.to_string());
                    }
                } else if indent.is_empty() {
                    out_lines.push(repl_trimmed.to_string());
                } else {
                    out_lines.push(format!("{indent}{repl_trimmed}"));
                }
                updated = true;
                continue;
            }
            out_lines.push(line.to_string());
        }

        if !updated {
            return Err(format!(
                "no entry in topic `{valid_slug}` matched `{target_clean}`"
            ));
        }

        let mut next_body = out_lines.join("\n");
        if !next_body.is_empty() {
            next_body.push('\n');
        }
        if next_body.len() > MEMORY_TOPIC_MAX_BYTES {
            return Err(format!(
                "topic `{valid_slug}` would exceed {MEMORY_TOPIC_MAX_BYTES} bytes ({} bytes); prune or shorten entry",
                next_body.len()
            ));
        }

        atomic_write_str(&path, &next_body).map_err(|e| e.to_string())?;
        self.ensure_project_root_marker()
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// Remove matching non-heading bullet/line(s) inside `topics/<slug>.md` without deleting the
    /// topic file itself. Preserves `#` headings and remaining entries.
    pub fn prune_topic_entries(&self, slug: &str, keyword: &str) -> Result<Vec<String>, String> {
        let valid_slug = validate_topic_slug(slug)?;
        let kw_clean = keyword.trim();
        if kw_clean.is_empty() {
            return Err("prune query cannot be empty".to_string());
        }

        let path = self.topics_dir().join(format!("{valid_slug}.md"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        let existing = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let kw_lower = kw_clean.to_lowercase();
        let mut removed = Vec::new();
        let mut kept = Vec::new();

        for line in existing.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && !trimmed.starts_with('#')
                && trimmed.to_lowercase().contains(&kw_lower)
            {
                let entry = trimmed
                    .strip_prefix("- ")
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string();
                removed.push(entry);
                continue;
            }
            kept.push(line.to_string());
        }

        if !removed.is_empty() {
            while kept.last().is_some_and(|l| l.trim().is_empty()) {
                kept.pop();
            }
            let mut next_body = kept.join("\n");
            if !next_body.is_empty() {
                next_body.push('\n');
            }
            atomic_write_str(&path, &next_body).map_err(|e| e.to_string())?;
            self.ensure_project_root_marker()
                .map_err(|e| e.to_string())?;
        }

        Ok(removed)
    }

    /// Delete a Tier-2 topic file and prune its pointer from `MEMORY.md`.
    pub fn delete_topic(&self, slug: &str) -> Result<bool, String> {
        let valid_slug = validate_topic_slug(slug)?;
        let path = self.topics_dir().join(format!("{valid_slug}.md"));
        let existed = path.exists();
        if existed {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        let pointer_token = format!("topics/{valid_slug}.md");
        let _ = self.prune_index(&pointer_token);
        Ok(existed)
    }

    /// Recall by exact topic slug or keyword search across `MEMORY.md` and `topics/*.md`.
    pub fn recall(&self, query: Option<&str>, topic: Option<&str>) -> Result<String, String> {
        if let Some(slug) = topic.map(str::trim).filter(|s| !s.is_empty()) {
            return self.read_topic(slug);
        }

        let topics = self.list_topics();
        let index_entries = self.load_index_entries();
        let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) else {
            let mut out = String::new();
            if index_entries.is_empty() && topics.is_empty() {
                return Ok("(auto-memory is empty)".to_string());
            }
            if !index_entries.is_empty() {
                out.push_str("Index (MEMORY.md):\n");
                for entry in &index_entries {
                    out.push_str(&format!("- {entry}\n"));
                }
            }
            if !topics.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("Topics: {}\n", topics.join(", ")));
            }
            return Ok(out);
        };

        let kw = q.to_ascii_lowercase();
        let mut hits = Vec::new();
        for entry in &index_entries {
            if entry.to_ascii_lowercase().contains(&kw) {
                hits.push(format!("[MEMORY.md] - {entry}"));
            }
        }
        for slug in &topics {
            if let Ok(body) = self.read_topic(slug) {
                let matching_lines: Vec<&str> = body
                    .lines()
                    .filter(|l| l.to_ascii_lowercase().contains(&kw))
                    .take(12)
                    .collect();
                if slug.to_ascii_lowercase().contains(&kw) || !matching_lines.is_empty() {
                    hits.push(format!(
                        "[topics/{slug}.md]\n{}",
                        if matching_lines.is_empty() {
                            truncate_utf8(&body, 600)
                        } else {
                            matching_lines.join("\n")
                        }
                    ));
                }
            }
        }
        if hits.is_empty() {
            Ok(format!("No auto-memory entries matched '{q}'."))
        } else {
            Ok(hits.join("\n\n"))
        }
    }

    /// Render Tier-1 `MEMORY.md` safely capped at `MEMORY_INDEX_MAX_LINES` and `MEMORY_INDEX_MAX_BYTES`
    /// for `MemoryHook` injection.
    pub fn render_prompt_index(&self) -> String {
        let raw = self.load_index_raw();
        let topics = self.list_topics();
        if raw.trim().is_empty() && topics.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();
        let mut total_bytes = 0usize;
        let mut truncated = false;
        for line in raw.lines() {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            if lines.len() >= MEMORY_INDEX_MAX_LINES
                || total_bytes + trimmed.len() + 1 > MEMORY_INDEX_MAX_BYTES
            {
                truncated = true;
                break;
            }
            total_bytes += trimmed.len() + 1;
            lines.push(trimmed.to_string());
        }

        let mut out = lines.join("\n");
        if truncated {
            out.push_str("\n[...MEMORY.md index capped; use memory(action=\"recall\") for topics]");
        }
        if !topics.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("Available topic files: {}", topics.join(", ")));
        }
        out
    }

    fn ensure_project_root_marker(&self) -> io::Result<()> {
        if let Some((marker_path, root_text)) = &self.project_root_marker {
            let up_to_date = fs::read_to_string(marker_path)
                .map(|existing| existing.trim() == root_text)
                .unwrap_or(false);
            if !up_to_date {
                atomic_write_str(marker_path, &format!("{root_text}\n"))?;
            }
        }
        Ok(())
    }

    fn write_index(&self, content: &str) -> io::Result<()> {
        fs::create_dir_all(&self.root_dir)?;
        atomic_write_str(&self.index_path(), content)?;
        self.ensure_project_root_marker()
    }
}

pub(crate) fn atomic_write_str(target: &Path, content: &str) -> io::Result<()> {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("memory");
    let tmp = parent.join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        let _ = f.sync_all();
    }
    match fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(_) => {
            let res = fs::write(target, content.as_bytes());
            let _ = fs::remove_file(&tmp);
            res
        }
    }
}

fn normalize_single_line(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let clean = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim();
    if clean.is_empty() {
        return Err("memory entry cannot be empty".to_string());
    }
    if clean.contains('\n') || clean.contains('\r') {
        return Err(
            "index entry must be a single concise line (use a `topic` file for multi-line details)"
                .to_string(),
        );
    }
    Ok(clean.to_string())
}

fn append_line_text(existing: &str, line: &str) -> String {
    if existing.trim().is_empty() {
        format!("{line}\n")
    } else if existing.ends_with('\n') {
        format!("{existing}{line}\n")
    } else {
        format!("{existing}\n{line}\n")
    }
}

fn upsert_topic_pointer(existing: &str, pointer_prefix: &str, pointer_line: &str) -> String {
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in existing.lines() {
        if line.contains(pointer_prefix) {
            if !replaced {
                lines.push(pointer_line.to_string());
                replaced = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(pointer_line.to_string());
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn enforce_index_budget(content: &str) -> Result<(), String> {
    let line_count = content.lines().filter(|l| !l.trim().is_empty()).count();
    if line_count > MEMORY_INDEX_MAX_LINES {
        return Err(format!(
            "MEMORY.md index exceeded {MEMORY_INDEX_MAX_LINES} lines ({line_count}); prune stale entries or consolidate into a topic file"
        ));
    }
    if content.len() > MEMORY_INDEX_MAX_BYTES {
        return Err(format!(
            "MEMORY.md index exceeded {MEMORY_INDEX_MAX_BYTES} bytes ({}); prune or move details to topic files",
            content.len()
        ));
    }
    Ok(())
}

fn first_meaningful_line(body: &str, max_chars: usize) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| body.trim());
    let stripped = line.strip_prefix("- ").unwrap_or(line).trim();
    truncate_at_word_boundary(stripped, max_chars)
}

fn is_trailing_open_or_sep_punct(c: char) -> bool {
    matches!(
        c,
        '(' | '[' | '{' | '`' | '"' | '\'' | '—' | '-' | ',' | ';' | ':'
    )
}

fn is_separator_punct(c: char) -> bool {
    matches!(c, ',' | ';' | ':' | '.' | '/' | '\\' | '-' | '—')
}

/// Truncate `text` cleanly at a word/punctuation boundary `<= max_chars` (UTF-8/char-safe),
/// appending `...` only when truncation actually occurred.
pub(crate) fn truncate_at_word_boundary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max_chars {
        return trimmed.to_string();
    }
    if max_chars <= 3 {
        return chars.into_iter().take(max_chars).collect();
    }
    let budget = max_chars - 3;

    // Prefer a clean whitespace boundary when the cut lands inside a word or right at whitespace.
    let mut cut_end = if chars[budget].is_whitespace() || is_separator_punct(chars[budget]) {
        Some(budget)
    } else {
        (1..budget)
            .rev()
            .find(|&i| chars[i].is_whitespace())
            .or_else(|| (1..budget).rev().find(|&i| is_separator_punct(chars[i])))
    };

    if let Some(mut end) = cut_end {
        while end > 0
            && (chars[end - 1].is_whitespace() || is_trailing_open_or_sep_punct(chars[end - 1]))
        {
            end -= 1;
        }
        cut_end = (end > 0).then_some(end);
    }

    let final_end = cut_end.unwrap_or(budget);
    let prefix: String = chars[..final_end].iter().collect();
    format!("{prefix}...")
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[...truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktrees_share_same_canonical_repo_root_and_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let main_repo = dir.path().join("repo");
        fs::create_dir_all(&main_repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "Tester"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .current_dir(&main_repo)
                .status()
                .unwrap()
                .success());
        }
        fs::write(main_repo.join("README.md"), "hi\n").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&main_repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&main_repo)
            .status()
            .unwrap()
            .success());

        let wt_dir = dir.path().join("repo-wt");
        assert!(Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "wt-branch",
                wt_dir.to_str().unwrap()
            ])
            .current_dir(&main_repo)
            .status()
            .unwrap()
            .success());

        assert_eq!(
            project_memory_bucket(&main_repo),
            project_memory_bucket(&wt_dir)
        );
    }

    #[test]
    fn index_enforces_150_line_ceiling_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutoMemoryStore::with_root(dir.path().to_path_buf());
        assert_eq!(store.remember_index_entry("Use cargo fmt").unwrap(), true);
        assert_eq!(store.remember_index_entry("use cargo fmt").unwrap(), false);

        for i in 1..MEMORY_INDEX_MAX_LINES {
            store
                .remember_index_entry(&format!("fact number {i}"))
                .unwrap();
        }
        let err = store.remember_index_entry("fact over limit").unwrap_err();
        assert!(err.contains("150 lines"), "unexpected err: {err}");
    }

    #[test]
    fn topic_crud_updates_pointer_and_validates_slug() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutoMemoryStore::with_root(dir.path().to_path_buf());

        assert!(validate_topic_slug("../evil").is_err());
        assert!(validate_topic_slug("a/b").is_err());
        assert!(validate_topic_slug(".hidden").is_err());

        store
            .write_topic(
                "build_rules",
                "- Run cargo check --workspace --locked\n- Avoid combined mcp test flags",
                Some("Workspace build and test flags"),
                false,
            )
            .unwrap();
        assert_eq!(store.list_topics(), vec!["build_rules"]);
        assert!(store
            .load_index_raw()
            .contains("[topics/build_rules.md](topics/build_rules.md)"));
        assert!(store
            .recall(None, Some("build_rules"))
            .unwrap()
            .contains("Avoid combined mcp test flags"));

        assert!(store.delete_topic("build_rules").unwrap());
        assert!(store.list_topics().is_empty());
        assert!(!store.load_index_raw().contains("build_rules.md"));
    }

    #[test]
    fn project_root_marker_written_idempotently_and_skipped_for_overrides() {
        let _home = jeikcode_config::test_util::isolated_home();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("my_repo");
        fs::create_dir_all(&repo).unwrap();

        // with_root override never writes .project_root
        let custom_root = dir.path().join("custom_auto");
        let custom_store = AutoMemoryStore::with_root(custom_root.clone());
        custom_store.remember_index_entry("custom fact").unwrap();
        assert!(!dir.path().join(".project_root").exists());
        assert!(!custom_root.join(".project_root").exists());

        // JEIKCODE_PROJECT_MEMORY_DIR override never writes .project_root
        let override_store = AutoMemoryStore::for_project_with_override(&repo, Some(".custom_mem"));
        override_store
            .remember_index_entry("override fact")
            .unwrap();
        assert!(!repo.join(".custom_mem/.project_root").exists());

        // Standard for_project writes .project_root under $JEIKCODE_HOME/projects/<bucket>/
        let store = AutoMemoryStore::for_project_with_override(&repo, None);
        let marker = store.root_dir().parent().unwrap().join(".project_root");
        assert!(
            !marker.exists(),
            "read/construct must not eagerly create .project_root"
        );

        store.remember_index_entry("project fact").unwrap();
        assert!(marker.is_file());
        let expected_root = crate::pathnorm::to_display(&resolve_canonical_repo_root(&repo));
        assert_eq!(fs::read_to_string(&marker).unwrap().trim(), expected_root);

        // If .project_root drifted, next topic/index write repairs it idempotently
        fs::write(&marker, "/stale/wrong/path\n").unwrap();
        store
            .write_topic("setup", "- Use uv venv", Some("Setup"), false)
            .unwrap();
        assert_eq!(fs::read_to_string(&marker).unwrap().trim(), expected_root);
    }

    #[test]
    fn topic_bullet_update_and_prune_preserve_headings_and_enforce_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutoMemoryStore::with_root(dir.path().to_path_buf());
        store
            .write_topic(
                "build_rules",
                "# Build Rules\n\n- Old rule A\n- Keep rule B\n- Old rule C",
                Some("Build rules"),
                false,
            )
            .unwrap();

        // Headings must never be matched or replaced
        let heading_err = store
            .update_topic_entry("build_rules", "Build Rules", "Tampered")
            .unwrap_err();
        assert!(heading_err.contains("no entry in topic"));

        // Bullet update preserves bullet prefix and other entries
        store
            .update_topic_entry("build_rules", "rule A", "New rule A")
            .unwrap();
        let body = store.read_topic("build_rules").unwrap();
        assert!(body.contains("# Build Rules"));
        assert!(body.contains("- New rule A"));
        assert!(body.contains("- Keep rule B"));
        assert!(!body.contains("Old rule A"));

        // Oversized update is rejected without corrupting file
        let huge = "z".repeat(MEMORY_TOPIC_MAX_BYTES + 64);
        let size_err = store
            .update_topic_entry("build_rules", "Keep rule B", &huge)
            .unwrap_err();
        assert!(size_err.contains("16384"));
        assert!(store
            .read_topic("build_rules")
            .unwrap()
            .contains("- Keep rule B"));

        // Bullet prune removes only matching non-heading lines and keeps topic file
        let pruned_heading = store
            .prune_topic_entries("build_rules", "Build Rules")
            .unwrap();
        assert!(pruned_heading.is_empty());

        let pruned = store
            .prune_topic_entries("build_rules", "Old rule")
            .unwrap();
        assert_eq!(pruned, vec!["Old rule C"]);
        let after = store.read_topic("build_rules").unwrap();
        assert!(after.contains("# Build Rules"));
        assert!(after.contains("- New rule A"));
        assert!(after.contains("- Keep rule B"));
        assert!(!after.contains("Old rule C"));
    }

    #[test]
    fn word_boundary_summary_truncation_handles_utf8_and_backticks() {
        assert_eq!(truncate_at_word_boundary("short text", 20), "short text");

        let sample = "Always pass explicit `path` to MCP tools when running multi-workspace";
        // Cut inside `path` ("Always pass explicit `p" = 23 + 3 = 26)
        let truncated = truncate_at_word_boundary(sample, 26);
        assert!(truncated.ends_with("..."));
        assert!(truncated.chars().count() <= 26);
        assert!(
            !truncated.contains("`p"),
            "must not cut mid-word/backtick: {truncated}"
        );
        assert_eq!(truncated, "Always pass explicit...");

        // Multibyte UTF-8 safety
        let utf8 = "中文说明 — pass explicit `workspace_path` argument";
        let utf8_trunc = truncate_at_word_boundary(utf8, 28);
        assert!(utf8_trunc.chars().count() <= 28);
        assert!(utf8_trunc.ends_with("..."));

        // Auto-generated pointer summary in write_topic stays bounded & word-safe
        let dir = tempfile::tempdir().unwrap();
        let store = AutoMemoryStore::with_root(dir.path().to_path_buf());
        let long_first_line =
            "- Always pass explicit `workspace_path` parameter on every DatumRef MCP invocation across concurrent multi-repo sessions to avoid routing mistakes";
        store
            .write_topic("datumref_rules", long_first_line, None, false)
            .unwrap();
        let index_entry = store.load_index_entries().pop().unwrap();
        assert!(index_entry.chars().count() <= MEMORY_INDEX_ENTRY_MAX_CHARS);
        assert!(index_entry.ends_with("..."));
        assert!(!index_entry.ends_with("`..."));
    }
}
