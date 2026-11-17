//! Session-owned mesh nickname sanitization, soft live-PID uniqueness, default labels,
//! verbatim Windows path cleanup, target resolution (by `name` or `session_id`), and banner formatting.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jeikcode_capabilities::pathnorm::strip_verbatim;
use jeikcode_capabilities::process_utils::is_pid_alive;
use serde::{Deserialize, Serialize};

/// Validate a session ID before turning it into a filename under `live/`.
pub(crate) fn validate_live_session_id(id: &str) -> Result<&str, String> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err("session_id must be 1..=128 characters".to_string());
    }
    if trimmed.starts_with('.')
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("invalid session_id `{trimmed}`"));
    }
    Ok(trimmed)
}

/// Descriptor persisted at `~/.jeikcode/live/<session_id>.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSessionDescriptor {
    pub session_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub pid: u32,
    pub endpoint: String,
    pub auth_token: String,
    pub working_dir: PathBuf,
    pub repo_root: PathBuf,
    pub model: String,
    pub started_at_ms: i64,
}

/// Live session summary returned by [`discover_live_sessions`](crate::discover_live_sessions).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSessionView {
    pub session_id: String,
    pub name: Option<String>,
    pub display_name: String,
    pub pid: u32,
    pub working_dir: String,
    pub repo_root: String,
    pub same_repo: bool,
    pub repo_scope: String,
    pub is_self: bool,
    pub model: String,
    pub phase: String,
    pub busy: bool,
    pub message_count: usize,
    pub started_at_ms: i64,
}

pub(crate) fn write_descriptor(
    live_dir: &Path,
    desc: &LiveSessionDescriptor,
) -> io::Result<PathBuf> {
    fs::create_dir_all(live_dir)?;
    let target = live_dir.join(format!("{}.json", desc.session_id));
    let tmp = live_dir.join(format!(".{}.{}.tmp", desc.session_id, uuid::Uuid::new_v4()));
    let json = serde_json::to_vec_pretty(desc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &target)?;
    Ok(target)
}

/// Maximum character length for a session-owned mesh nickname/label.
pub const MAX_MESH_NAME_CHARS: usize = 48;

pub(crate) fn max_mesh_hops_from_mesh() -> u32 {
    crate::session_mesh::max_mesh_hops()
}

pub(crate) fn same_canonical_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> String {
        let s = fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/");
        let stripped = s.strip_prefix("//?/").unwrap_or(&s);
        if cfg!(windows) {
            stripped.to_ascii_lowercase()
        } else {
            stripped.to_string()
        }
    };
    norm(a) == norm(b)
}

/// Format a path cleanly for display in tool output, banners, and `<peer_message>` envelopes
/// by stripping Windows `\\?\` / `//?/` verbatim prefixes and normalizing separators.
pub fn display_clean_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = strip_verbatim(&raw);
    if cfg!(windows) {
        stripped.replace('\\', "/")
    } else {
        stripped.into_owned()
    }
}

/// Sanitize and validate a session-owned mesh name/nickname (non-empty, no control chars, <= 48 chars).
pub fn sanitize_mesh_name(raw: &str) -> Result<String, String> {
    if raw.chars().any(char::is_control) {
        return Err("session name cannot contain control characters".to_string());
    }
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err("session name cannot be empty".to_string());
    }
    let char_count = collapsed.chars().count();
    if char_count > MAX_MESH_NAME_CHARS {
        return Err(format!(
            "session name exceeds {MAX_MESH_NAME_CHARS} characters (got {char_count})"
        ));
    }
    Ok(collapsed)
}

/// Clean default display label when a session has no explicit nickname: `<dir_or_repo>#<short_id>`.
pub fn default_display_name(working_dir: &Path, repo_root: &Path, session_id: &str) -> String {
    let base_from = |p: &Path| -> Option<String> {
        let clean = display_clean_path(p);
        Path::new(&clean)
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != ".")
            .map(str::to_string)
    };
    let label = base_from(repo_root)
        .or_else(|| base_from(working_dir))
        .unwrap_or_else(|| "session".to_string());
    let short_id: String = session_id.chars().take(8).collect();
    format!("{label}#{short_id}")
}

/// Format an incoming peer message for visible display in TUI / WebUI timelines.
#[allow(clippy::too_many_arguments)]
pub fn format_peer_message_banner(
    from_session_id: &str,
    from_session_name: Option<&str>,
    from_working_dir: &Path,
    from_repo_root: &Path,
    same_repo: bool,
    hop_depth: u32,
    steered: bool,
    text: &str,
) -> String {
    let short_id: String = from_session_id.chars().take(8).collect();
    let sender_label = match from_session_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => format!("{name} ({short_id})"),
        None => short_id,
    };
    let clean_cwd = display_clean_path(from_working_dir);
    let clean_repo = display_clean_path(from_repo_root);
    let scope_badge = if same_repo {
        "same-repo".to_string()
    } else {
        format!("EXTERNAL REPO: {clean_repo}")
    };
    let mode_tag = if steered { ", steered" } else { "" };
    format!(
        "[peer {sender_label} @ {clean_cwd} | {scope_badge} | hop {hop_depth}{mode_tag}]\n{}",
        text.trim()
    )
}

pub(crate) fn read_live_descriptors_reaping_stale(
    live_dir: &Path,
) -> Vec<(PathBuf, LiveSessionDescriptor)> {
    let Ok(entries) = fs::read_dir(live_dir) else {
        return Vec::new();
    };
    let mut descriptors = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(desc) = serde_json::from_str::<LiveSessionDescriptor>(&raw) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if validate_live_session_id(&desc.session_id).is_err() || !is_pid_alive(desc.pid) {
            let _ = fs::remove_file(&path);
            continue;
        }
        descriptors.push((path, desc));
    }
    descriptors
}

/// Enforce soft uniqueness among currently live PIDs (`auto_suffix_on_collision` adds `-2`, `-3`, ...).
pub(crate) fn resolve_unique_live_name(
    live_dir: &Path,
    self_session_id: &str,
    desired: &str,
    auto_suffix_on_collision: bool,
) -> Result<String, String> {
    let mut active_names: Vec<(String, String, u32)> = Vec::new();
    for (_, desc) in read_live_descriptors_reaping_stale(live_dir) {
        if desc.session_id == self_session_id {
            continue;
        }
        if let Some(existing_name) = desc.name {
            active_names.push((existing_name, desc.session_id, desc.pid));
        }
    }

    let conflict = active_names
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(desired));
    let Some((_, owner_sid, owner_pid)) = conflict else {
        return Ok(desired.to_string());
    };
    if !auto_suffix_on_collision {
        let short_id: String = owner_sid.chars().take(8).collect();
        return Err(format!(
            "live session name `{desired}` is already in use by session `{short_id}` (pid {owner_pid})"
        ));
    }

    for suffix in 2..=999u32 {
        let tail = format!("-{suffix}");
        let max_base_chars = MAX_MESH_NAME_CHARS.saturating_sub(tail.len());
        let base: String = desired.chars().take(max_base_chars).collect();
        let candidate = format!("{base}{tail}");
        if !active_names
            .iter()
            .any(|(n, _, _)| n.eq_ignore_ascii_case(&candidate))
        {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not allocate unique suffix for session name `{desired}`"
    ))
}

/// Resolve a live session descriptor by exact `session_id`, case-insensitive `name`,
/// `display_name`, or unique `session_id` prefix (>= 4 chars), reaping dead PIDs along the way.
pub(crate) fn resolve_live_target(
    live_dir: &Path,
    target: &str,
) -> Result<LiveSessionDescriptor, String> {
    let query = target.trim();
    if query.is_empty() {
        return Err("target session name or session_id cannot be empty".to_string());
    }

    if let Ok(valid_id) = validate_live_session_id(query) {
        let exact_path = live_dir.join(format!("{valid_id}.json"));
        if exact_path.is_file() {
            let raw = fs::read_to_string(&exact_path)
                .map_err(|_| format!("live session `{valid_id}` not found in mesh registry"))?;
            let desc: LiveSessionDescriptor = serde_json::from_str(&raw)
                .map_err(|e| format!("corrupt descriptor for `{valid_id}`: {e}"))?;
            if !is_pid_alive(desc.pid) {
                let _ = fs::remove_file(&exact_path);
                return Err(format!(
                    "live session `{valid_id}` (pid {}) is no longer alive (reaped stale entry)",
                    desc.pid
                ));
            }
            return Ok(desc);
        }
    }

    let descriptors = read_live_descriptors_reaping_stale(live_dir);
    let name_matches: Vec<LiveSessionDescriptor> = descriptors
        .iter()
        .filter(|(_, d)| {
            d.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(query))
        })
        .map(|(_, d)| d.clone())
        .collect();
    if name_matches.len() == 1 {
        return Ok(name_matches.into_iter().next().unwrap());
    }
    if name_matches.len() > 1 {
        return Err(format_ambiguous_target_error(query, &name_matches));
    }

    let query_lower = query.to_ascii_lowercase();
    let fallback_matches: Vec<LiveSessionDescriptor> = descriptors
        .into_iter()
        .map(|(_, d)| d)
        .filter(|d| {
            let disp = d.name.clone().unwrap_or_else(|| {
                default_display_name(&d.working_dir, &d.repo_root, &d.session_id)
            });
            disp.eq_ignore_ascii_case(query)
                || (query_lower.len() >= 4
                    && d.session_id.to_ascii_lowercase().starts_with(&query_lower))
        })
        .collect();
    match fallback_matches.len() {
        1 => Ok(fallback_matches.into_iter().next().unwrap()),
        0 => Err(format!(
            "live session `{query}` not found in mesh registry (pass a live `name` or `session_id` from `list_live_sessions`)"
        )),
        _ => Err(format_ambiguous_target_error(query, &fallback_matches)),
    }
}

fn format_ambiguous_target_error(query: &str, matches: &[LiveSessionDescriptor]) -> String {
    let options: Vec<String> = matches
        .iter()
        .map(|d| {
            let disp = d.name.clone().unwrap_or_else(|| {
                default_display_name(&d.working_dir, &d.repo_root, &d.session_id)
            });
            format!(
                "`{disp}` (session_id={}, cwd={})",
                d.session_id,
                display_clean_path(&d.working_dir)
            )
        })
        .collect();
    format!(
        "ambiguous target `{query}` matched {} live sessions: {}; pass the exact `session_id`",
        matches.len(),
        options.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_live_session_id_rejects_traversal() {
        assert!(validate_live_session_id("abc-123_XYZ").is_ok());
        assert!(validate_live_session_id("../evil").is_err());
        assert!(validate_live_session_id(".hidden").is_err());
        assert!(validate_live_session_id("a/b").is_err());
        assert!(validate_live_session_id("").is_err());
    }

    #[test]
    fn sanitize_mesh_name_and_display_clean_path_and_default_label() {
        assert_eq!(
            sanitize_mesh_name("  backend   api ").unwrap(),
            "backend api"
        );
        assert!(sanitize_mesh_name("   ").is_err());
        assert!(sanitize_mesh_name("bad\nname").is_err());
        assert!(sanitize_mesh_name(&"a".repeat(MAX_MESH_NAME_CHARS + 1)).is_err());

        let cleaned = display_clean_path(Path::new(r"\\?\C:\Users\dev\Repo"));
        assert!(!cleaned.contains(r"\\?\"));
        assert!(cleaned.ends_with("Users/dev/Repo"));

        let label = default_display_name(
            Path::new(r"\\?\C:\Users\dev\Repo"),
            Path::new(r"\\?\C:\Users\dev\Repo"),
            "e5da3378-1122-3344",
        );
        assert_eq!(label, "Repo#e5da3378");
    }

    #[test]
    fn resolve_unique_live_name_handles_collisions_and_dead_pids() {
        let dir = tempfile::tempdir().unwrap();
        let live_pid = std::process::id();
        write_descriptor(
            dir.path(),
            &LiveSessionDescriptor {
                session_id: "sess-1111".into(),
                name: Some("Backend".into()),
                pid: live_pid,
                endpoint: "127.0.0.1:1".into(),
                auth_token: "t".into(),
                working_dir: dir.path().to_path_buf(),
                repo_root: dir.path().to_path_buf(),
                model: "m".into(),
                started_at_ms: 1,
            },
        )
        .unwrap();
        write_descriptor(
            dir.path(),
            &LiveSessionDescriptor {
                session_id: "sess-dead".into(),
                name: Some("Frontend".into()),
                pid: 0,
                endpoint: "127.0.0.1:2".into(),
                auth_token: "t".into(),
                working_dir: dir.path().to_path_buf(),
                repo_root: dir.path().to_path_buf(),
                model: "m".into(),
                started_at_ms: 1,
            },
        )
        .unwrap();

        assert_eq!(
            resolve_unique_live_name(dir.path(), "sess-2222", "frontend", false).unwrap(),
            "frontend"
        );
        assert!(resolve_unique_live_name(dir.path(), "sess-2222", "backend", false).is_err());
        assert_eq!(
            resolve_unique_live_name(dir.path(), "sess-2222", "backend", true).unwrap(),
            "backend-2"
        );
        assert_eq!(
            resolve_unique_live_name(dir.path(), "sess-1111", "backend", false).unwrap(),
            "backend"
        );
    }

    #[test]
    fn resolve_live_target_supports_name_id_and_disambiguation() {
        let dir = tempfile::tempdir().unwrap();
        let live_pid = std::process::id();
        write_descriptor(
            dir.path(),
            &LiveSessionDescriptor {
                session_id: "aaa11111-0000".into(),
                name: Some("Backend".into()),
                pid: live_pid,
                endpoint: "127.0.0.1:1".into(),
                auth_token: "t".into(),
                working_dir: dir.path().to_path_buf(),
                repo_root: dir.path().to_path_buf(),
                model: "m".into(),
                started_at_ms: 1,
            },
        )
        .unwrap();
        write_descriptor(
            dir.path(),
            &LiveSessionDescriptor {
                session_id: "bbb22222-0000".into(),
                name: Some("Frontend".into()),
                pid: live_pid,
                endpoint: "127.0.0.1:2".into(),
                auth_token: "t".into(),
                working_dir: dir.path().to_path_buf(),
                repo_root: dir.path().to_path_buf(),
                model: "m".into(),
                started_at_ms: 2,
            },
        )
        .unwrap();

        assert_eq!(
            resolve_live_target(dir.path(), "backend")
                .unwrap()
                .session_id,
            "aaa11111-0000"
        );
        assert_eq!(
            resolve_live_target(dir.path(), "bbb22222-0000")
                .unwrap()
                .name
                .as_deref(),
            Some("Frontend")
        );
        assert_eq!(
            resolve_live_target(dir.path(), "aaa1").unwrap().session_id,
            "aaa11111-0000"
        );

        write_descriptor(
            dir.path(),
            &LiveSessionDescriptor {
                session_id: "ccc33333-0000".into(),
                name: Some("Backend".into()),
                pid: live_pid,
                endpoint: "127.0.0.1:3".into(),
                auth_token: "t".into(),
                working_dir: dir.path().to_path_buf(),
                repo_root: dir.path().to_path_buf(),
                model: "m".into(),
                started_at_ms: 3,
            },
        )
        .unwrap();
        let err = resolve_live_target(dir.path(), "backend").unwrap_err();
        assert!(err.contains("ambiguous target"));
        assert!(err.contains("aaa11111-0000"));
        assert!(err.contains("ccc33333-0000"));
    }
}
