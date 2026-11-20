//! `POST /command`: 无状态斜杠命令执行器（对已持久化会话/记忆施加一次性变更）。
use axum::{extract::State, response::IntoResponse, Json};
use std::path::Path;
use std::sync::Arc;

use crate::AppState;
#[cfg(test)]
use jeikcode_capabilities::session::SessionMeta as NativeSessionMeta;
use jeikcode_capabilities::session::{
    LoadedSession, SessionLease as NativeSessionLease, SessionManager as NativeSessionManager,
    SessionStoreError,
};
use jeikcode_config::config::memory::MemoryStore;

#[derive(serde::Serialize)]
pub(crate) struct CostModelResult {
    provider: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    estimated_cost_usd: Option<f64>,
    free: bool,
}

#[derive(serde::Deserialize)]
pub(crate) struct CommandReq {
    pub command: String,
    #[serde(default)]
    pub arg: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub project_hash: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommandResult {
    Undo {
        undone: usize,
    },
    Remember {
        scope: String,
    },
    Forget {
        removed: Vec<String>,
    },
    Memory {
        global: Vec<String>,
        project: Vec<String>,
    },
    Context {
        used_tokens: usize,
        total_messages: usize,
        ctx_window: usize,
        utilization: f32,
        ctx_name: String,
    },
    Compact {
        applied: bool,
        removed_messages: usize,
        before_tokens: usize,
        after_tokens: usize,
    },
    Status {
        logged_in: bool,
        username: Option<String>,
        provider: String,
        model: String,
        working_dir: String,
        config_path: String,
        text: String,
    },
    Config {
        path: String,
        provider: String,
    },
    Diff {
        stat: String,
    },
    Cost {
        total_tokens: usize,
        turn_count: usize,
        models: Vec<CostModelResult>,
        unattributed_tokens: u64,
        estimated_cost_usd: Option<f64>,
    },
    Todo {
        items: Vec<TodoItemJson>,
    },
    Goal {
        action: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<crate::GoalProgressWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Error {
        message: String,
    },
}

#[derive(serde::Serialize)]
pub(crate) struct TodoItemJson {
    pub status: String,
    pub content: String,
}

struct NativeCommandSession {
    manager: NativeSessionManager,
    lease: NativeSessionLease,
    loaded: LoadedSession,
}

fn command_project_bucket(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
) -> anyhow::Result<String> {
    let bucket = project_hash
        .map(str::to_owned)
        .unwrap_or_else(|| NativeSessionManager::project_hash(working_dir));
    if bucket.len() != 16 || !bucket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid project session bucket")
    }
    Ok(bucket)
}

fn load_native_command_session_in_root(
    sessions_root: &Path,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    id: &str,
) -> anyhow::Result<Option<NativeCommandSession>> {
    let bucket = command_project_bucket(working_dir, project_hash)?;
    let manager = NativeSessionManager::with_root(sessions_root.join(bucket));
    let has_existing = [
        manager.meta_path(id)?,
        manager.snapshot_path(id)?,
        manager.legacy_path(id)?,
    ]
    .iter()
    .any(|path| path.exists());
    if !has_existing {
        return Ok(None);
    }
    let lease = manager.acquire_lease(id)?;
    crate::legacy_convert::converge_session(&manager, &lease)?;
    let loaded = manager.load_native_session(id)?;
    Ok(Some(NativeCommandSession {
        manager,
        lease,
        loaded,
    }))
}

fn load_command_session_view_in_root(
    sessions_root: &Path,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    id: &str,
) -> anyhow::Result<crate::legacy_convert::CatalogSessionView> {
    let bucket = command_project_bucket(working_dir, project_hash)?;
    crate::legacy_convert::load_catalog_session_view_in_project_root(sessions_root, &bucket, id)?
        .ok_or_else(|| anyhow::anyhow!("session {id:?} not found"))
}

fn exec_native_undo(session: NativeCommandSession, arg: &str) -> anyhow::Result<CommandResult> {
    let expected_snapshot = session.loaded.snapshot;
    let available = expected_snapshot
        .messages
        .iter()
        .filter(|message| {
            message.role == jeikcode_kernel::message::Role::User && !message.synthetic
        })
        .count();
    if available == 0 {
        return Ok(CommandResult::Undo { undone: 0 });
    }
    let target = arg.trim().parse::<usize>().ok();
    let undo = jeikcode_coding::runtime::undo_snapshot_to_prompt(&expected_snapshot, target)?;
    let message_count = undo.snapshot.messages.len();
    let persisted_message_count = u32::try_from(message_count)?;
    session.manager.commit_native_runtime_mutation(
        &session.lease,
        &undo.snapshot,
        move |current_snapshot, meta, presentation| {
            if current_snapshot != &expected_snapshot {
                return Err(SessionStoreError::Corrupt {
                    kind: "session mutation",
                    message: "session snapshot changed while preparing undo".into(),
                });
            }
            meta.turn_stats
                .retain(|stat| !stat.position_valid || stat.after_message <= message_count);
            let surviving_turn_ids: std::collections::BTreeSet<_> = meta
                .turn_stats
                .iter()
                .filter_map(|stat| {
                    (stat.position_valid && stat.turn_id != 0).then_some(stat.turn_id)
                })
                .collect();
            presentation.retain_turns(&surviving_turn_ids);
            meta.message_count = persisted_message_count;
            // 回溯改写了上下文：压缩估算的占用不再成立。
            meta.compacted_context_tokens = None;
            meta.turn_count =
                u32::try_from(meta.turn_stats.len()).map_err(|_| SessionStoreError::TooLarge {
                    kind: "session turn stats",
                    limit: u32::MAX as usize,
                    actual: meta.turn_stats.len(),
                })?;
            meta.updated_at = jeikcode_capabilities::session::now_ms();
            Ok(())
        },
    )?;
    // /undo is an explicit truncate: rewrite the v4 log in place when the last
    // surviving message is addressable. Only a legacy/empty target falls back to
    // invalidate + rebuild.
    let target_id = undo.snapshot.messages.iter().rev().find_map(|message| {
        let addressable = matches!(
            &message.role,
            jeikcode_kernel::message::Role::User
                | jeikcode_kernel::message::Role::Assistant
                | jeikcode_kernel::message::Role::Tool
        );
        (addressable && !message.synthetic)
            .then(|| message.id.clone())
            .flatten()
    });
    let meta = session.manager.read_meta(&session.lease.id()).ok();
    let rewritten = match (target_id.as_deref(), meta) {
        (Some(target_id), Some(meta)) => {
            let surviving_last_turn = meta
                .turn_stats
                .iter()
                .filter(|stat| stat.position_valid)
                .map(|stat| stat.turn_id)
                .max()
                .unwrap_or(0);
            let completed_turns: Vec<u64> = meta
                .turn_stats
                .iter()
                .filter(|stat| stat.position_valid && stat.turn_id != 0)
                .map(|stat| stat.turn_id)
                .collect();
            crate::rewrite_display_for_mutation(
                &session.manager,
                &session.lease.id(),
                crate::DisplayMutation::Truncate {
                    target_id: Some(target_id),
                    inclusive: false,
                },
                surviving_last_turn,
                &completed_turns,
            )
        }
        _ => false,
    };
    if !rewritten {
        let _ = session.manager.invalidate_display(&session.lease.id());
    }
    // Recall reads the raw transcript; an undo must remove the rewound turns
    // there too, otherwise a later recall can resurrect history the user
    // explicitly rewound past.
    if let Ok(meta) = session.manager.read_meta(&session.lease.id()) {
        let surviving: std::collections::BTreeSet<u64> = meta
            .turn_stats
            .iter()
            .filter(|stat| stat.position_valid && stat.turn_id != 0)
            .map(|stat| stat.turn_id)
            .collect();
        let _ = session
            .manager
            .retain_transcript_turns(&session.lease.id(), &surviving);
    }
    Ok(CommandResult::Undo {
        undone: undo
            .prompts_before
            .saturating_sub(undo.target_n.saturating_sub(1)),
    })
}

fn commit_native_compaction(
    session: NativeCommandSession,
    messages: Vec<jeikcode_kernel::message::Message>,
    mutation: jeikcode_coding::runtime::SnapshotCompactionMutation,
    after_tokens: u64,
) -> anyhow::Result<()> {
    use jeikcode_coding::runtime::SnapshotCompactionMutation;

    let expected_snapshot = session.loaded.snapshot;
    let mut snapshot = expected_snapshot.clone();
    snapshot.messages = messages;
    // 一次已提交的压缩恰好推进一次 cache_epoch（与内核 `prepare_plan` 的契约一致）。
    // `compact_snapshot` 在 epoch=0 的临时会话上运行，若这里沿用旧 epoch，
    // `session_mutated.revision` 就与各端已知版本相同，被当成“已应用”丢弃。
    snapshot.cache_epoch = expected_snapshot.cache_epoch.saturating_add(1);
    let message_count = snapshot.messages.len();
    let persisted_message_count = u32::try_from(message_count)?;
    session.manager.commit_native_runtime_mutation(
        &session.lease,
        &snapshot,
        move |current_snapshot, meta, presentation| {
            if current_snapshot != &expected_snapshot {
                return Err(SessionStoreError::Corrupt {
                    kind: "session mutation",
                    message: "session snapshot changed while preparing compaction".into(),
                });
            }
            if let SnapshotCompactionMutation::Replace {
                old_start,
                old_end,
                new_end,
            } = mutation
            {
                let _ = meta.archive_turn_stats_where(|stat| {
                    stat.position_valid
                        && stat.after_message > old_start
                        && stat.after_message < old_end
                });
                for stat in &mut meta.turn_stats {
                    if !stat.position_valid {
                        continue;
                    }
                    if stat.after_message >= old_end {
                        stat.after_message = new_end + stat.after_message.saturating_sub(old_end);
                    }
                }
            }
            let surviving_turn_ids: std::collections::BTreeSet<_> = meta
                .turn_stats
                .iter()
                .filter_map(|stat| {
                    (stat.position_valid && stat.turn_id != 0).then_some(stat.turn_id)
                })
                .collect();
            presentation.retain_turns(&surviving_turn_ids);
            meta.message_count = persisted_message_count;
            // 压缩后的占用是当前上下文事实：落盘后任何客户端冷启动都读它，
            // 直到下一次真实 usage 写入（record_turn 清空）。
            meta.compacted_context_tokens = Some(after_tokens);
            meta.turn_count =
                u32::try_from(meta.turn_stats.len()).map_err(|_| SessionStoreError::TooLarge {
                    kind: "session turn stats",
                    limit: u32::MAX as usize,
                    actual: meta.turn_stats.len(),
                })?;
            meta.updated_at = jeikcode_capabilities::session::now_ms();
            Ok(())
        },
    )?;
    Ok(())
}

/// 手动压缩的提交结果：命令回执 + 已落盘的压缩边界记录（权威 `seq`/`id`）
/// + 提交后的会话版本。
struct NativeCompactOutcome {
    result: CommandResult,
    notice: Option<jeikcode_capabilities::session::DisplayRecord>,
    /// Human-readable boundary text even when the display append failed; the
    /// caller can still broadcast a transient Warning instead of silently
    /// losing the completion marker.
    notice_text: Option<String>,
    revision: Option<u64>,
}

async fn exec_native_compact(
    provider_name: Option<&str>,
    arg: &str,
    session: NativeCommandSession,
    working_dir: &std::path::Path,
    config_path: &Path,
    telemetry: Arc<jeikcode_telemetry::Telemetry>,
    cancellation: tokio_util::sync::CancellationToken,
) -> anyhow::Result<NativeCompactOutcome> {
    let config_path_owned = config_path.to_path_buf();
    let config = tokio::task::spawn_blocking(move || {
        jeikcode_config::config::Config::load(&config_path_owned)
    })
    .await
    .map_err(|e| anyhow::anyhow!("config load task failed: {e}"))??;
    let resolved = crate::live_api::resolve_provider_name(&config, provider_name);
    // Validate the provider up front for a clean error (the old core path did
    // `providers.get(...).ok_or("Provider not found")`); without this a missing
    // key surfaces as a murkier build/network failure deeper in.
    if !config.selection_exists(&resolved) {
        anyhow::bail!("Provider '{resolved}' not found");
    }

    // Build the summarizing provider via the SAME native chain `/chat` uses
    // (chat_runtime_config → coding_config_from_runtime → coding_provider_factory().build),
    // yielding a kernel-native `LlmProvider` directly — no core provider, no adapter.
    // `build` may do blocking auth I/O (gateway token), so run it off the async runtime.
    let coding_cfg = crate::kernel_runtime::coding_config_from_runtime(
        &crate::live_api::chat_runtime_config(&config, &resolved, working_dir, telemetry),
    );
    let factory = crate::runtime_host::coding_provider_factory();
    let provider = tokio::task::spawn_blocking(move || factory.build(&coding_cfg, None))
        .await
        .map_err(|e| anyhow::anyhow!("provider build task panicked: {e}"))?
        .map_err(|e| anyhow::anyhow!("provider construction failed: {e}"))?;

    // 阶段登记与广播由 `exec_compact` 在受理那一刻统一负责；这里只做压缩与提交。
    let sid_for_notice = session.lease.id().to_string();

    let compact_fut = jeikcode_coding::runtime::compact_snapshot_with_epoch(
        session.loaded.snapshot.messages.clone(),
        provider,
        (!arg.trim().is_empty()).then(|| arg.trim().to_string()),
        session.loaded.snapshot.cache_epoch,
    );
    let compacted = tokio::select! {
        _ = cancellation.cancelled() => {
            anyhow::bail!("compaction was cancelled by user");
        }
        res = compact_fut => res,
    };
    let mut notice = None;
    let mut notice_text = None;
    let mut revision = None;
    if compacted.outcome.committed {
        let mark = jeikcode_config::i18n::format_compaction_mark(
            compacted.outcome.removed_messages,
            compacted.outcome.estimated_tokens_before,
            compacted.outcome.estimated_tokens_after,
        );
        notice_text = Some(mark.clone());
        let after_tokens = compacted.outcome.estimated_tokens_after as u64;
        let compacted_messages = compacted.messages;
        let compaction_mutation = compacted.mutation;
        let (committed_notice, committed_rev) = tokio::task::spawn_blocking(move || {
            let mgr_root = session.manager.root().to_path_buf();
            // 先用压缩前的完整会话把 v4 显示日志补齐，再改写 snapshot 并追加压缩边界。
            // 否则一旦此前 `/undo` 或编辑使 display.jsonl 失效，`append_display_notice`
            // 会返回 `None`，压缩成功标记就退化成仅发起端可见的本地瞬态提示。
            crate::ensure_v4_display_for_session(&session.manager, &session.loaded);
            commit_native_compaction(
                session,
                compacted_messages,
                compaction_mutation,
                after_tokens,
            )?;
            let mgr = NativeSessionManager::with_root(mgr_root);
            // 压缩边界是转录里的正式记录：保留显示日志分配的权威 `seq`/`id`，
            // 交给调用方原样广播，所有实例按同一序号摆放。
            let notice = mgr
                .append_display_notice(&sid_for_notice, mark, None)
                .ok()
                .flatten();
            let revision = mgr
                .load_snapshot(&sid_for_notice)
                .ok()
                .map(|snap| snap.cache_epoch);
            anyhow::Ok((notice, revision))
        })
        .await
        .map_err(|e| anyhow::anyhow!("compaction commit task failed: {e}"))??;
        notice = committed_notice;
        revision = committed_rev;
    }
    Ok(NativeCompactOutcome {
        result: CommandResult::Compact {
            applied: compacted.outcome.committed,
            removed_messages: compacted.outcome.removed_messages,
            before_tokens: compacted.outcome.estimated_tokens_before,
            after_tokens: compacted.outcome.estimated_tokens_after,
        },
        notice,
        notice_text,
        revision,
    })
}

fn exec_undo_in_root(
    sessions_root: &Path,
    working_dir: &Path,
    session_id: Option<&str>,
    arg: &str,
    project_hash: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for undo"))?;
    let native =
        load_native_command_session_in_root(sessions_root, working_dir, project_hash, sid)?
            .ok_or_else(|| anyhow::anyhow!("session {sid:?} not found"))?;
    exec_native_undo(native, arg)
}

/// The session's current context size for `/context`: the prompt tokens the
/// provider reported on the most recent assistant turn (`meta.used_tokens`), or
/// 0 before any assistant turn. Mirrors kernel `Conversation::last_pressure`'s
/// used-tokens read, but works directly off a persisted snapshot so `/context`
/// reflects what the live (native) turn actually sent — no parallel tool assembly.
pub(crate) fn snapshot_used_tokens(messages: &[jeikcode_kernel::message::Message]) -> u32 {
    use jeikcode_kernel::message::Role;
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant && !m.is_display_only())
        .find_map(|m| m.meta.as_ref())
        .map(|meta| meta.used_tokens)
        .unwrap_or(0)
}

async fn exec_context(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
    provider: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id
        .ok_or_else(|| anyhow::anyhow!("session_id required for context"))?
        .to_string();
    let sessions_root = NativeSessionManager::sessions_root();
    let config_path = jeikcode_config::config::Config::default_path();
    let wd = working_dir.to_path_buf();
    let ph = project_hash.map(str::to_string);
    let prov = provider.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let session = load_command_session_view_in_root(&sessions_root, &wd, ph.as_deref(), &sid)?;
        let config = jeikcode_config::config::Config::load(&config_path)?;
        let resolved = crate::live_api::resolve_provider_name(&config, prov.as_deref());
        let provider_config = config
            .provider_config_for_selection(&resolved)
            .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", resolved))?;
        let ctx_window = provider_config.context_window as u32;
        let used_tokens = snapshot_used_tokens(&session.snapshot.messages);
        let utilization = if ctx_window > 0 {
            used_tokens as f32 / ctx_window as f32
        } else {
            0.0
        };
        Ok(CommandResult::Context {
            used_tokens: used_tokens as usize,
            total_messages: session.snapshot.messages.len(),
            ctx_window: ctx_window as usize,
            utilization,
            ctx_name: provider_config.model.clone(),
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("context task failed: {e}"))?
}

/// 解析 `/remember` 参数：可选前缀 `--global`。返回 (是否全局, 去掉前缀并 trim 后的内容)。
pub(crate) fn parse_remember_arg(arg: &str) -> (bool, &str) {
    let arg = arg.trim();
    if let Some(rest) = arg.strip_prefix("--global") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return (true, rest.trim());
        }
    }
    (false, arg)
}

#[cfg(test)]
fn exec_remember(working_dir: &Path, arg: &str) -> anyhow::Result<CommandResult> {
    exec_remember_with_store(MemoryStore::global(), working_dir, arg)
}

#[cfg(test)]
fn exec_forget(working_dir: &Path, arg: &str) -> anyhow::Result<CommandResult> {
    exec_forget_with_store(MemoryStore::global(), working_dir, arg)
}

fn exec_remember_with_store(
    global_store: MemoryStore,
    working_dir: &Path,
    arg: &str,
) -> anyhow::Result<CommandResult> {
    let (global, content) = parse_remember_arg(arg);
    if content.is_empty() {
        anyhow::bail!("remember needs content");
    }
    let store = if global {
        global_store
    } else {
        MemoryStore::project(working_dir)
    };
    store.append(content)?;
    Ok(CommandResult::Remember {
        scope: if global { "global" } else { "project" }.to_string(),
    })
}

fn exec_forget_with_store(
    global_store: MemoryStore,
    working_dir: &Path,
    arg: &str,
) -> anyhow::Result<CommandResult> {
    let keyword = arg.trim();
    if keyword.is_empty() {
        anyhow::bail!("forget needs a keyword");
    }
    let mut removed = global_store.remove_matching(keyword)?;
    removed.extend(MemoryStore::project(working_dir).remove_matching(keyword)?);
    Ok(CommandResult::Forget { removed })
}

fn exec_memory_with_store(
    global_store: MemoryStore,
    working_dir: &Path,
) -> anyhow::Result<CommandResult> {
    Ok(CommandResult::Memory {
        global: global_store.load(),
        project: MemoryStore::project(working_dir).load(),
    })
}

async fn exec_compact(
    working_dir: std::path::PathBuf,
    project_hash: Option<String>,
    session_id: Option<String>,
    provider: Option<String>,
    arg: String,
    telemetry: Arc<jeikcode_telemetry::Telemetry>,
    active_chats: crate::ActiveChatRegistry,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for compact"))?;
    let sid = sid.as_str();
    // 同一会话只能有一个执行 owner：live runtime 正在跑 turn 时，禁止旁路
    // 读盘压缩，否则会和 live runtime 的快照/display 写入互相覆盖。
    if crate::native_live::live_turn_running(sid) {
        anyhow::bail!("session {sid:?} is running a live turn; stop it before compacting");
    }
    let admission = match active_chats.admit(Some(sid), None).await {
        Ok(adm) => adm,
        Err(_) => anyhow::bail!("session {sid:?} is currently busy"),
    };

    let compact_owner = crate::manual_compaction_owner(&admission.operation_id);

    struct ActiveChatGuard {
        active_chats: crate::ActiveChatRegistry,
        operation_id: String,
        session_id: String,
        compact_owner: String,
    }
    impl Drop for ActiveChatGuard {
        fn drop(&mut self) {
            // 任何退出路径（含 panic / 提前返回）都不能把压缩阶段留在登记表里。
            // 只结束本次 operation 自己的阶段，绝不误清 live/turn owner。
            crate::finish_compaction_phase(&self.session_id, &self.compact_owner, false);
            let chats = self.active_chats.clone();
            let op_id = self.operation_id.clone();
            tokio::spawn(async move {
                chats.complete(&op_id).await;
            });
        }
    }
    let _active_guard = ActiveChatGuard {
        active_chats: active_chats.clone(),
        operation_id: admission.operation_id.clone(),
        session_id: sid.to_string(),
        compact_owner: compact_owner.clone(),
    };

    // 从受理那一刻起就是压缩阶段：登记权威起点并立即广播，不等 provider 构建。
    // 否则异地实例只会先收到 admit 的无阶段 activity，把计时器错锁到上一条提问。
    crate::note_compaction_started(sid, &compact_owner);
    active_chats
        .set_operation_status(&admission.operation_id, Some("compacting".to_string()))
        .await;
    active_chats
        .broadcast_to_session(
            sid,
            crate::ChatEvent::SessionStatus {
                session_id: sid.to_string(),
                status: "compacting".to_string(),
            },
        )
        .await;

    let operation_id = admission.operation_id.clone();
    let sessions_root = NativeSessionManager::sessions_root();
    let config_path = jeikcode_config::config::Config::default_path();
    let wd_for_load = working_dir.clone();
    let ph_for_load = project_hash.clone();
    let sid_for_load = sid.to_string();
    let outcome = match tokio::task::spawn_blocking(move || {
        load_native_command_session_in_root(
            &sessions_root,
            &wd_for_load,
            ph_for_load.as_deref(),
            &sid_for_load,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("session load task failed: {e}"))?
    {
        Ok(Some(native)) => {
            exec_native_compact(
                provider.as_deref(),
                &arg,
                native,
                &working_dir,
                &config_path,
                telemetry,
                admission.cancellation,
            )
            .await
        }
        Ok(None) => Err(anyhow::anyhow!("session {sid:?} not found")),
        Err(error) => Err(error),
    };

    // 压缩 pass 到此结束（提交 / 空操作 / 失败 / 取消）：先收掉阶段，再发结果帧。
    crate::finish_compaction_phase(sid, &compact_owner, false);
    active_chats.set_operation_status(&operation_id, None).await;
    if let Ok(NativeCompactOutcome {
        result:
            CommandResult::Compact {
                applied: true,
                after_tokens,
                ..
            },
        notice,
        notice_text,
        revision,
    }) = &outcome
    {
        if let Some(record) = notice {
            // 带显示日志权威 `seq`/`id` 的边界记录：观察者按序号把它摆成一条
            // 独立 system 行，而不是塞进“最后一条 assistant”的 warning 片段。
            active_chats
                .broadcast_to_session(
                    sid,
                    crate::ChatEvent::DisplayNotice {
                        session_id: sid.to_string(),
                        id: record.id.clone().unwrap_or_default(),
                        display_seq: record.seq,
                        text: record.content.clone(),
                        created_at: record.created_at,
                    },
                )
                .await;
        } else if let Some(mark) = notice_text {
            // 边界行写盘失败时至少让当前观察者看到完成标记；下次冷启动
            // 仍可由 ensure_v4_display/backfill 修复。
            active_chats
                .broadcast_to_session(
                    sid,
                    crate::ChatEvent::Warning {
                        message: mark.clone(),
                        message_id: None,
                    },
                )
                .await;
        }
        active_chats
            .broadcast_to_session(
                sid,
                crate::ChatEvent::ContextCompacted {
                    session_id: sid.to_string(),
                    tokens: *after_tokens as u32,
                },
            )
            .await;
        if let Some(revision) = revision {
            // 全局总线：没打开该会话的标签页也要按新版本作废缓存。
            crate::publish_global(crate::GlobalWireEvent::SessionMutated {
                session_id: sid.to_string(),
                revision: *revision,
            });
        }
    }
    // 无论结果如何都显式结束阶段：空操作/失败也不能让观察者挂在 Compacting…。
    active_chats
        .broadcast_to_session(
            sid,
            crate::ChatEvent::SessionStatus {
                session_id: sid.to_string(),
                status: "idle".to_string(),
            },
        )
        .await;
    outcome.map(|outcome| outcome.result)
}

fn exec_config_at(path: &Path) -> anyhow::Result<CommandResult> {
    let provider = jeikcode_config::config::Config::load(path)
        .map(|c| c.default_provider)
        .unwrap_or_default();
    Ok(CommandResult::Config {
        path: path.display().to_string(),
        provider,
    })
}

fn exec_diff(working_dir: &std::path::Path) -> anyhow::Result<CommandResult> {
    // No console-window flash for git when spawned from the console-less daemon
    // on Windows (no-op elsewhere). Mirrors capabilities' own tool spawns.
    let mut cmd = std::process::Command::new("git");
    cmd.args(["diff", "--stat"]).current_dir(working_dir);
    jeikcode_capabilities::process_utils::suppress_console_window_sync(&mut cmd);
    let out = cmd.output()?;
    let stat = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    } else {
        String::from_utf8_lossy(&out.stderr).trim_end().to_string()
    };
    Ok(CommandResult::Diff { stat })
}

fn render_context_file_status_block_with(
    config_dir: &Path,
    global_memory: MemoryStore,
    working_dir: &std::path::Path,
) -> String {
    use jeikcode_config::config::instructions::{InstructionLevel, LayeredInstructions};
    use jeikcode_config::i18n::{t, Msg};
    let instructions = LayeredInstructions::load_with_config_dir(config_dir, working_dir);
    let mut out = t(Msg::StatusInstructionFilesHeader).into_owned();
    for line in instructions.status_lines_with_config_dir(config_dir, working_dir) {
        let scope = t(match line.level {
            InstructionLevel::Global => Msg::StatusInstructionScopeGlobal,
            InstructionLevel::Project => Msg::StatusInstructionScopeProject,
        });
        let path = line.path.display().to_string();
        if line.found {
            out.push_str(&t(Msg::StatusInstructionPresent {
                path: &path,
                label: line.level.label(),
                scope: &scope,
            }));
        } else {
            out.push_str(&t(Msg::StatusInstructionMissing {
                path: &path,
                label: line.level.label(),
                scope: &scope,
            }));
        }
    }
    out.push('\n');
    out.push_str(&t(Msg::StatusMemoryFilesHeader));
    for (scope_msg, store) in [
        (Msg::StatusMemoryScopeGlobal, global_memory),
        (
            Msg::StatusMemoryScopeProject,
            jeikcode_config::config::memory::MemoryStore::project(working_dir),
        ),
    ] {
        let scope = t(scope_msg);
        let path = store.path().display().to_string();
        if store.path().is_file() {
            out.push_str(&t(Msg::StatusMemoryPresent {
                path: &path,
                scope: &scope,
            }));
        } else {
            out.push_str(&t(Msg::StatusMemoryMissing {
                path: &path,
                scope: &scope,
            }));
        }
    }
    out
}

#[cfg(test)]
fn render_context_file_status_block(working_dir: &std::path::Path) -> String {
    render_context_file_status_block_with(
        &jeikcode_config::config::Config::config_dir(),
        MemoryStore::global(),
        working_dir,
    )
}

fn assemble_status(body: &str, proxy: &str, instructions: &str) -> String {
    let mut txt = String::with_capacity(body.len() + proxy.len() + instructions.len() + 16);
    txt.push_str(body);
    txt.push_str(proxy);
    txt.push('\n');
    txt.push_str(instructions);
    txt
}

fn exec_status_with(
    config_dir: &Path,
    config_path: &Path,
    global_memory: MemoryStore,
    working_dir: &std::path::Path,
    provider: Option<&str>,
) -> anyhow::Result<CommandResult> {
    use jeikcode_config::i18n::{t, Msg};
    let config = jeikcode_config::config::Config::load(config_path).ok();
    let provider_name = provider
        .map(|s| s.to_string())
        .or_else(|| config.as_ref().map(|c| c.default_provider.clone()))
        .unwrap_or_default();
    let model = config
        .as_ref()
        .and_then(|c| c.provider_config_for_selection(&provider_name))
        .map(|p| p.model)
        .unwrap_or_default();
    let body = t(Msg::StatusBody {
        model: &model,
        dir: &working_dir.display().to_string(),
        config: &config_path.display().to_string(),
    })
    .into_owned();
    let proxy_summary = config
        .as_ref()
        .map(|c| c.network.proxy.summary())
        .unwrap_or_else(|| "follow_system".to_string());
    let proxy_line = format!("  Proxy:  {}\n", proxy_summary);

    let text = assemble_status(
        &body,
        &proxy_line,
        &render_context_file_status_block_with(config_dir, global_memory, working_dir),
    );

    Ok(CommandResult::Status {
        logged_in: false,
        username: None,
        provider: provider_name,
        model,
        working_dir: working_dir.display().to_string(),
        config_path: config_path.display().to_string(),
        text,
    })
}

fn exec_cost_in_root(
    sessions_root: &Path,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for cost"))?;
    let session = load_command_session_view_in_root(sessions_root, working_dir, project_hash, sid)?;
    let report = jeikcode_capabilities::session::aggregate_session_cost(&session.meta);
    Ok(CommandResult::Cost {
        total_tokens: report.total_tokens as usize,
        turn_count: session.meta.turn_stats.len(),
        models: report
            .models
            .into_iter()
            .map(|model| CostModelResult {
                provider: model.provider_id,
                model: model.model_id,
                input_tokens: model.tokens.input,
                output_tokens: model.tokens.output,
                cached_tokens: model.tokens.cached_input,
                estimated_cost_usd: model.estimated_cost_usd,
                free: model.explicitly_free,
            })
            .collect(),
        unattributed_tokens: report.unattributed_tokens,
        estimated_cost_usd: report.estimated_cost_usd,
    })
}

fn exec_todo_in_root(
    sessions_root: &Path,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for todo"))?;
    let session = load_command_session_view_in_root(sessions_root, working_dir, project_hash, sid)?;

    let _ = session;
    Ok(CommandResult::Todo {
        items: todo_items_from_board(sid),
    })
}

fn todo_items_from_board(session_id: &str) -> Vec<TodoItemJson> {
    use jeikcode_capabilities::tools::todo::TodoStatus;
    let todos = jeikcode_capabilities::tools::read_todo_board(session_id)
        .map(|board| board.items)
        .unwrap_or_default();

    todos
        .into_iter()
        .map(|t| TodoItemJson {
            status: match t.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
            }
            .to_string(),
            content: t.content,
        })
        .collect()
}

fn goal_launch_missing_prompt(condition: &str) -> bool {
    let trimmed = condition.trim();
    trimmed.is_empty() || (trimmed.starts_with("--") && !trimmed.contains(char::is_whitespace))
}

fn parse_goal_rounds_arg(raw: &str) -> Option<jeikcode_coding::GoalRoundAdjustment> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(delta_str) = trimmed.strip_prefix('+') {
        let delta = delta_str
            .trim()
            .replace(['_', ','], "")
            .parse::<u32>()
            .ok()?;
        (delta > 0).then_some(jeikcode_coding::GoalRoundAdjustment::Add(delta))
    } else {
        let total = trimmed.replace(['_', ','], "").parse::<u32>().ok()?;
        Some(jeikcode_coding::GoalRoundAdjustment::Set(total))
    }
}

fn format_goal_status_message(progress: &crate::GoalProgressWire) -> String {
    let round = progress.round + 1;
    let mins = progress.elapsed_secs / 60;
    let secs = progress.elapsed_secs % 60;
    let cap_label = progress
        .max_rounds
        .map(|n| n.to_string())
        .unwrap_or_else(|| "∞".to_string());
    let reason = progress
        .last_reason
        .as_deref()
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    format!(
        "Goal [{phase}] round {round}/{cap_label} · {mins}m {secs:02}s · {tokens} tok{reason}\n{cond}",
        phase = progress.phase,
        tokens = progress.tokens_used,
        cond = progress.condition,
    )
}

pub(crate) async fn exec_goal(
    state: &AppState,
    working_dir: &Path,
    session_id: Option<&str>,
    provider: Option<&str>,
    arg: &str,
) -> anyhow::Result<CommandResult> {
    let trimmed = arg.trim();
    let (head, rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(h, r)| (h, r.trim()))
        .unwrap_or((trimmed, ""));
    let head_lower = head.to_ascii_lowercase();

    if matches!(head_lower.as_str(), "help" | "?" | "-h" | "--help") {
        let message = [
            "Usage: /goal <condition>",
            "  /goal status              Show current goal progress",
            "  /goal pause               Pause active goal after current round",
            "  /goal resume              Resume paused goal",
            "  /goal stop | clear        Stop and clear active goal",
            "  /goal rounds <+N|N>       Add N rounds or set max rounds (0 = unbounded)",
            "  /goal shift <prompt>      Shift goal direction immediately",
            "Flags: --continuous  --max-rounds N  --max-tokens N  --rollback-on-stall  --fresh-epoch",
        ]
        .join("\n");
        return Ok(CommandResult::Goal {
            action: "help".into(),
            message,
            progress: session_id.and_then(crate::goal_controller_snapshot),
            session_id: session_id.map(str::to_string),
        });
    }

    if head_lower == "shift" && rest.is_empty() {
        return Ok(CommandResult::Goal {
            action: "help".into(),
            message: "Usage: /goal shift <new goal condition>".into(),
            progress: session_id.and_then(crate::goal_controller_snapshot),
            session_id: session_id.map(str::to_string),
        });
    }

    match head_lower.as_str() {
        "" | "status" => {
            let sid = session_id.map(str::trim).filter(|s| !s.is_empty());
            let progress = sid.and_then(crate::goal_controller_snapshot);
            let message = match &progress {
                Some(p) => format_goal_status_message(p),
                None => "No active goal in this session. Use /goal <condition> to start.".into(),
            };
            Ok(CommandResult::Goal {
                action: "status".into(),
                message,
                progress,
                session_id: sid.map(str::to_string),
            })
        }
        "pause" => {
            let sid = session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("session_id required for /goal pause"))?;
            let Some(handle) = crate::native_live::existing_runner_handle(sid) else {
                return Ok(CommandResult::Goal {
                    action: "pause".into(),
                    message: "No active goal runtime in this session.".into(),
                    progress: None,
                    session_id: Some(sid.to_string()),
                });
            };
            handle
                .pause_goal()
                .await
                .map_err(|e| anyhow::anyhow!("failed to pause goal: {e}"))?;
            Ok(CommandResult::Goal {
                action: "pause".into(),
                message: "Goal paused.".into(),
                progress: crate::goal_controller_snapshot(sid),
                session_id: Some(sid.to_string()),
            })
        }
        "resume" => {
            let sid = session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("session_id required for /goal resume"))?;
            let current = crate::goal_controller_snapshot(sid);
            match current.as_ref().map(|p| p.phase.as_str()) {
                Some("pursuing") => {
                    return Ok(CommandResult::Goal {
                        action: "resume".into(),
                        message: "Goal is already running.".into(),
                        progress: current,
                        session_id: Some(sid.to_string()),
                    });
                }
                Some("paused") | Some("paused_at_cap") => {}
                _ => {
                    return Ok(CommandResult::Goal {
                        action: "resume".into(),
                        message: "No paused goal to resume.".into(),
                        progress: current,
                        session_id: Some(sid.to_string()),
                    });
                }
            }
            let progress = crate::launch_or_resume_goal_bridge(
                state,
                sid.to_string(),
                working_dir.to_path_buf(),
                provider.map(str::to_string),
                crate::GoalBridgeCommand::Resume,
            )
            .await?;
            Ok(CommandResult::Goal {
                action: "resume".into(),
                message: "Goal resumed.".into(),
                progress,
                session_id: Some(sid.to_string()),
            })
        }
        "rounds" => {
            let sid = session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("session_id required for /goal rounds"))?;
            let Some(adj) = parse_goal_rounds_arg(rest) else {
                return Ok(CommandResult::Goal {
                    action: "rounds".into(),
                    message: "Usage: /goal rounds <+N|N> (0 = unbounded)".into(),
                    progress: crate::goal_controller_snapshot(sid),
                    session_id: Some(sid.to_string()),
                });
            };
            let Some(handle) = crate::native_live::existing_runner_handle(sid) else {
                return Ok(CommandResult::Goal {
                    action: "rounds".into(),
                    message: "No active goal runtime in this session.".into(),
                    progress: None,
                    session_id: Some(sid.to_string()),
                });
            };
            let updated_cap = handle
                .adjust_goal_rounds(adj)
                .await
                .map_err(|e| anyhow::anyhow!("failed to update goal rounds: {e}"))?;
            let mut progress = crate::goal_controller_snapshot(sid);
            if let Some(p) = progress.as_mut() {
                p.max_rounds = updated_cap;
            }
            let cap_label = updated_cap
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unbounded".into());
            Ok(CommandResult::Goal {
                action: "rounds".into(),
                message: format!("Goal max rounds updated to {cap_label}."),
                progress,
                session_id: Some(sid.to_string()),
            })
        }
        "clear" | "stop" | "off" | "reset" | "none" | "cancel" => {
            let sid = session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("session_id required for /goal stop"))?;
            if let Some(handle) = crate::native_live::existing_runner_handle(sid) {
                let _ = handle.stop_goal().await;
            }
            if state
                .active_chats
                .operation_for_session(sid)
                .await
                .is_some()
            {
                let _ = state
                    .active_chats
                    .stop_and_wait(sid.to_string(), Some(std::time::Duration::from_secs(2)))
                    .await;
            }
            crate::clear_goal_controller_state(sid);
            Ok(CommandResult::Goal {
                action: "stop".into(),
                message: "Goal stopped and cleared.".into(),
                progress: None,
                session_id: Some(sid.to_string()),
            })
        }
        _ => {
            let config = jeikcode_config::config::Config::load(
                &jeikcode_config::config::Config::default_path(),
            )
            .unwrap_or_default();
            let defaults = jeikcode_coding::GoalLaunchDefaults {
                rollback_on_stall: config.coding.goal_rollback_on_stall,
                fresh_epoch: config.coding.goal_fresh_epoch,
            };
            let (condition, options) =
                jeikcode_coding::parse_goal_launch_with_defaults(trimmed, defaults);
            if goal_launch_missing_prompt(&condition) {
                return Ok(CommandResult::Goal {
                    action: "help".into(),
                    message: "Goal prompt is required. Usage: /goal <condition>".into(),
                    progress: session_id.and_then(crate::goal_controller_snapshot),
                    session_id: session_id.map(str::to_string),
                });
            }
            let sid = session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let progress = crate::launch_or_resume_goal_bridge(
                state,
                sid.clone(),
                working_dir.to_path_buf(),
                provider.map(str::to_string),
                crate::GoalBridgeCommand::Launch {
                    condition: condition.clone(),
                    options: options.clone(),
                },
            )
            .await?;
            let max_rounds_label = options
                .max_rounds
                .or(config.coding.goal_max_rounds)
                .or_else(jeikcode_coding::config::goal_max_rounds_env)
                .map(|n| {
                    if n == 0 {
                        "∞".to_string()
                    } else {
                        n.to_string()
                    }
                })
                .unwrap_or_else(|| "300".to_string());
            Ok(CommandResult::Goal {
                action: "started".into(),
                message: format!(
                    "Goal started (max rounds: {max_rounds_label}, rollback on stall: {}, fresh epoch: {}): {condition}",
                    if options.rollback_on_stall { "on" } else { "off" },
                    if options.fresh_epoch { "on" } else { "off" },
                ),
                progress,
                session_id: Some(sid),
            })
        }
    }
}

pub(crate) async fn run_command(
    State(state): State<AppState>,
    Json(req): Json<CommandReq>,
) -> impl IntoResponse {
    let working_dir = match req.working_dir.as_deref() {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => {
            return Json(CommandResult::Error {
                message: "working_dir required".into(),
            });
        }
    };
    let result = match req.command.as_str() {
        "undo" => {
            let sessions_root = NativeSessionManager::sessions_root();
            let wd = working_dir.clone();
            let sid = req.session_id.clone();
            let arg = req.arg.clone();
            let ph = req.project_hash.clone();
            tokio::task::spawn_blocking(move || {
                exec_undo_in_root(&sessions_root, &wd, sid.as_deref(), &arg, ph.as_deref())
            })
            .await
            .map_err(|e| anyhow::anyhow!("undo task failed: {e}"))
            .and_then(|res| res)
        }
        "remember" => {
            let global_store = MemoryStore::global();
            let wd = working_dir.clone();
            let arg = req.arg.clone();
            tokio::task::spawn_blocking(move || exec_remember_with_store(global_store, &wd, &arg))
                .await
                .map_err(|e| anyhow::anyhow!("remember task failed: {e}"))
                .and_then(|res| res)
        }
        "forget" => {
            let global_store = MemoryStore::global();
            let wd = working_dir.clone();
            let arg = req.arg.clone();
            tokio::task::spawn_blocking(move || exec_forget_with_store(global_store, &wd, &arg))
                .await
                .map_err(|e| anyhow::anyhow!("forget task failed: {e}"))
                .and_then(|res| res)
        }
        "memory" => {
            let global_store = MemoryStore::global();
            let wd = working_dir.clone();
            tokio::task::spawn_blocking(move || exec_memory_with_store(global_store, &wd))
                .await
                .map_err(|e| anyhow::anyhow!("memory task failed: {e}"))
                .and_then(|res| res)
        }
        "context" => {
            exec_context(
                &working_dir,
                req.project_hash.as_deref(),
                req.session_id.as_deref(),
                req.provider.as_deref(),
            )
            .await
        }
        "compact" => {
            // 压缩是登记在 `ActiveChatRegistry` 里的服务端操作（可由 stop 取消），其生命周期
            // 不能绑在这条 HTTP 请求上：刷新/关页会丢弃 handler future，连带取消压缩
            // 并清掉阶段登记。放进独立任务里跑，这里只等它的回执。
            let task = tokio::spawn(exec_compact(
                working_dir.clone(),
                req.project_hash.clone(),
                req.session_id.clone(),
                req.provider.clone(),
                req.arg.clone(),
                state.telemetry.clone(),
                state.active_chats.clone(),
            ));
            match task.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("compaction task failed: {error}")),
            }
        }
        "config" => {
            let path = jeikcode_config::config::Config::default_path();
            tokio::task::spawn_blocking(move || exec_config_at(&path))
                .await
                .map_err(|e| anyhow::anyhow!("config task failed: {e}"))
                .and_then(|res| res)
        }
        "diff" => {
            let wd = working_dir.clone();
            tokio::task::spawn_blocking(move || exec_diff(&wd))
                .await
                .map_err(|e| anyhow::anyhow!("diff task failed: {e}"))
                .and_then(|res| res)
        }
        "status" => {
            let config_dir = jeikcode_config::config::Config::config_dir();
            let config_path = jeikcode_config::config::Config::default_path();
            let global_memory = MemoryStore::global();
            let wd = working_dir.clone();
            let prov = req.provider.clone();
            tokio::task::spawn_blocking(move || {
                exec_status_with(
                    &config_dir,
                    &config_path,
                    global_memory,
                    &wd,
                    prov.as_deref(),
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("status task failed: {e}"))
            .and_then(|res| res)
        }
        "cost" => {
            let sessions_root = NativeSessionManager::sessions_root();
            let wd = working_dir.clone();
            let ph = req.project_hash.clone();
            let sid = req.session_id.clone();
            tokio::task::spawn_blocking(move || {
                exec_cost_in_root(&sessions_root, &wd, ph.as_deref(), sid.as_deref())
            })
            .await
            .map_err(|e| anyhow::anyhow!("cost task failed: {e}"))
            .and_then(|res| res)
        }
        "todo" => {
            let sessions_root = NativeSessionManager::sessions_root();
            let wd = working_dir.clone();
            let ph = req.project_hash.clone();
            let sid = req.session_id.clone();
            tokio::task::spawn_blocking(move || {
                exec_todo_in_root(&sessions_root, &wd, ph.as_deref(), sid.as_deref())
            })
            .await
            .map_err(|e| anyhow::anyhow!("todo task failed: {e}"))
            .and_then(|res| res)
        }
        "goal" => {
            exec_goal(
                &state,
                &working_dir,
                req.session_id.as_deref(),
                req.provider.as_deref(),
                &req.arg,
            )
            .await
        }
        other => Err(anyhow::anyhow!("unknown command: {other}")),
    };
    match result {
        Ok(r) => Json(r),
        Err(e) => Json(CommandResult::Error {
            message: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeikcode_capabilities::session::PresentationFile;
    use jeikcode_capabilities::session::{StorageOwner, TurnStat};
    use jeikcode_config::config::memory::MemoryStore;

    #[test]
    fn context_file_status_shows_instruction_and_memory_paths() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "project instructions").unwrap();
        let project_memory = MemoryStore::project(project.path());
        std::fs::create_dir_all(project_memory.path().parent().unwrap()).unwrap();
        std::fs::write(project_memory.path(), "- remembered fact\n").unwrap();
        let status = render_context_file_status_block(project.path());
        assert!(status.contains(&project.path().join("AGENTS.md").display().to_string()));
        assert!(status.contains(&project_memory.path().display().to_string()));
        assert!(status.contains("(PROJECT)") || status.contains("（PROJECT）"));
    }

    #[test]
    fn snapshot_used_tokens_reads_latest_assistant_meta() {
        use jeikcode_kernel::message::{Message, MessageMeta};

        // No assistant turn yet → zero.
        assert_eq!(snapshot_used_tokens(&[Message::user("hi")]), 0);

        // The most recent assistant meta's recorded prompt tokens win.
        let mut a = Message::assistant("ans", Vec::new());
        a.meta = Some(MessageMeta {
            ctx_window: 128_000,
            used_tokens: 40_000,
            utilization: 0.3125,
            ..Default::default()
        });
        let msgs = vec![Message::user("hi"), a];
        assert_eq!(snapshot_used_tokens(&msgs), 40_000);
    }

    #[test]
    fn native_undo_preserves_updates_after_session_load() {
        use jeikcode_capabilities::session::{
            DisplayAnchor, PresentationEntry, PresentationRole, TurnStat,
        };

        let dir = tempfile::tempdir().unwrap();
        let manager = NativeSessionManager::with_root(dir.path());
        let id = "native-undo";
        let snapshot = jeikcode_kernel::message::SessionSnapshot::new(vec![
            jeikcode_kernel::message::Message::user("first"),
            jeikcode_kernel::message::Message::assistant("one", Vec::new()),
            jeikcode_kernel::message::Message::user("second"),
            jeikcode_kernel::message::Message::assistant("two", Vec::new()),
        ]);
        manager.save_snapshot(id, &snapshot).unwrap();
        let mut meta = NativeSessionMeta::new(id, "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 4;
        meta.turn_count = 3;
        meta.turn_stats = vec![
            TurnStat {
                after_message: 99,
                position_valid: false,
                turn_id: 99,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 10,
                errored: false,
                used_tokens: 1,
                ctx_window: 10,
                model_usage: Vec::new(),
            },
            TurnStat {
                after_message: 2,
                position_valid: true,
                turn_id: 1,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 1,
                errored: false,
                used_tokens: 1,
                ctx_window: 10,
                model_usage: Vec::new(),
            },
            TurnStat {
                after_message: 4,
                position_valid: true,
                turn_id: 2,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 1,
                total_tokens: 1,
                errored: false,
                used_tokens: 1,
                ctx_window: 10,
                model_usage: Vec::new(),
            },
        ];
        manager.write_meta(&meta).unwrap();
        std::fs::write(
            manager.jsonl_path(id).unwrap(),
            concat!(
                "{\"v\":1,\"turn_id\":1,\"session_id\":\"native-undo\",\"user\":\"first\",\"assistant\":\"one\"}\n",
                "{\"v\":1,\"turn_id\":2,\"session_id\":\"native-undo\",\"user\":\"second\",\"assistant\":\"two\"}\n",
            ),
        )
        .unwrap();
        let presentation = PresentationFile {
            v: jeikcode_capabilities::session::presentation::PRESENTATION_VERSION,
            entries: vec![
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
                    role: PresentationRole::Assistant,
                    text: "keep".into(),
                },
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 2 },
                    role: PresentationRole::Assistant,
                    text: "drop".into(),
                },
            ],
        };
        manager.write_presentation(id, &presentation).unwrap();
        let lease = manager.acquire_lease(id).unwrap();
        let loaded = LoadedSession {
            snapshot,
            meta,
            presentation,
        };

        manager.rename(id, "renamed after load").unwrap();
        manager
            .append_presentation(
                id,
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
                    role: PresentationRole::Assistant,
                    text: "late keep".into(),
                },
            )
            .unwrap();

        let result = exec_native_undo(
            NativeCommandSession {
                manager: NativeSessionManager::with_root(dir.path()),
                lease,
                loaded,
            },
            "",
        )
        .unwrap();

        assert!(matches!(result, CommandResult::Undo { undone: 1 }));
        let manager = NativeSessionManager::with_root(dir.path());
        assert_eq!(manager.load_snapshot(id).unwrap().messages.len(), 2);
        let meta = manager.read_meta(id).unwrap();
        assert_eq!(meta.turn_count, 2);
        assert!(!meta.turn_stats[0].position_valid);
        assert_eq!(meta.turn_stats[0].total_tokens, 10);
        assert_eq!(meta.name, "renamed after load");
        assert!(meta.user_renamed);
        let presentation = manager.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 2);
        assert_eq!(presentation.entries[0].text, "keep");
        assert_eq!(presentation.entries[1].text, "late keep");
        assert!(presentation
            .entries
            .iter()
            .all(|entry| entry.text != "drop"));
        let transcript = std::fs::read_to_string(manager.jsonl_path(id).unwrap()).unwrap();
        assert!(transcript.contains("\"turn_id\":1"));
        assert!(!transcript.contains("\"turn_id\":2"));
    }

    #[test]
    fn native_compaction_preserves_updates_and_prunes_removed_turn_anchors() {
        use jeikcode_capabilities::session::{
            DisplayAnchor, PresentationEntry, PresentationRole, TurnStat,
        };
        use jeikcode_coding::runtime::SnapshotCompactionMutation;

        let dir = tempfile::tempdir().unwrap();
        let manager = NativeSessionManager::with_root(dir.path());
        let id = "native-compact";
        let mut snapshot = jeikcode_kernel::message::SessionSnapshot::new(vec![
            jeikcode_kernel::message::Message::user("u1"),
            jeikcode_kernel::message::Message::assistant("a1", Vec::new()),
            jeikcode_kernel::message::Message::user("u2"),
            jeikcode_kernel::message::Message::assistant("a2", Vec::new()),
            jeikcode_kernel::message::Message::user("u3"),
            jeikcode_kernel::message::Message::assistant("a3", Vec::new()),
        ]);
        snapshot.turn_counter = 8;
        manager.save_snapshot(id, &snapshot).unwrap();
        let stat = |after_message, turn_id| TurnStat {
            after_message,
            position_valid: true,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
            model_usage: Vec::new(),
        };
        let mut meta = NativeSessionMeta::new(id, "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 6;
        meta.turn_count = 3;
        meta.turn_stats = vec![stat(2, 1), stat(4, 2), stat(6, 3)];
        manager.write_meta(&meta).unwrap();
        let presentation = PresentationFile {
            v: jeikcode_capabilities::session::presentation::PRESENTATION_VERSION,
            entries: vec![
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
                    role: PresentationRole::Assistant,
                    text: "removed".into(),
                },
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 2 },
                    role: PresentationRole::Assistant,
                    text: "kept".into(),
                },
            ],
        };
        manager.write_presentation(id, &presentation).unwrap();
        let lease = manager.acquire_lease(id).unwrap();
        let loaded = LoadedSession {
            snapshot,
            meta,
            presentation,
        };

        manager.rename(id, "renamed after load").unwrap();
        manager
            .append_presentation(
                id,
                PresentationEntry {
                    anchor: DisplayAnchor::AfterTurn { turn_id: 2 },
                    role: PresentationRole::Assistant,
                    text: "late kept".into(),
                },
            )
            .unwrap();
        // Single presentation append no longer invalidates an existing v4 log;
        // seed a fresh log afterwards so this test measures compaction, not
        // presentation.
        manager
            .write_display_backfill(
                id,
                &[jeikcode_capabilities::session::DisplayRecord {
                    v: jeikcode_capabilities::session::DISPLAY_VERSION,
                    session_id: id.into(),
                    turn_id: 1,
                    index: 0,
                    seq: 0,
                    role: "user".into(),
                    id: Some("u1".into()),
                    content: "u1".into(),
                    reasoning: None,
                    synthetic: false,
                    internal_origin: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    is_error: false,
                    images: Vec::new(),
                    created_at: Some(1),
                    elapsed_ms: None,
                }],
                3,
            )
            .unwrap();

        commit_native_compaction(
            NativeCommandSession {
                manager: NativeSessionManager::with_root(dir.path()),
                lease,
                loaded,
            },
            vec![
                jeikcode_kernel::message::Message::user("summary"),
                jeikcode_kernel::message::Message::user("u3"),
                jeikcode_kernel::message::Message::assistant("a3", Vec::new()),
            ],
            SnapshotCompactionMutation::Replace {
                old_start: 0,
                old_end: 4,
                new_end: 1,
            },
            4096,
        )
        .unwrap();

        let manager = NativeSessionManager::with_root(dir.path());
        assert!(
            manager.read_display(id).unwrap().is_some(),
            "compaction must not invalidate the v4 display log"
        );
        let snapshot = manager.load_snapshot(id).unwrap();
        assert_eq!(snapshot.messages.len(), 3);
        assert_eq!(snapshot.turn_counter, 8);
        let meta = manager.read_meta(id).unwrap();
        assert_eq!(meta.turn_count, 2);
        assert_eq!(meta.turn_stats[0].after_message, 1);
        assert_eq!(meta.turn_stats[1].after_message, 3);
        assert_eq!(meta.detached_unattributed_tokens, 1);
        assert_eq!(meta.compacted_context_tokens, Some(4096));
        assert_eq!(meta.name, "renamed after load");
        assert!(meta.user_renamed);
        let presentation = manager.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 2);
        assert_eq!(presentation.entries[0].text, "kept");
        assert_eq!(presentation.entries[1].text, "late kept");
        assert!(presentation
            .entries
            .iter()
            .all(|entry| entry.text != "removed"));
    }

    #[test]
    fn parse_remember_arg_detects_global() {
        assert_eq!(parse_remember_arg("--global 记住这个"), (true, "记住这个"));
        assert_eq!(parse_remember_arg("普通事实"), (false, "普通事实"));
        assert_eq!(
            parse_remember_arg("  --global   trimmed  "),
            (true, "trimmed")
        );
        assert_eq!(parse_remember_arg("--globalfoo"), (false, "--globalfoo"));
    }

    #[test]
    fn remember_then_memory_roundtrip_project_scope() {
        // hermetic：project 作用域写到 working_dir/.jeikcode/memory.md
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        exec_remember(wd, "阿童木用 Rust 写").unwrap();
        let store = MemoryStore::project(wd);
        assert!(store.load().iter().any(|e| e.contains("阿童木用 Rust 写")));
    }

    #[test]
    fn forget_removes_project_entry() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        exec_remember(wd, "delete-me fact").unwrap();
        exec_remember(wd, "keep-me fact").unwrap();
        // exec_forget 也会扫全局，但全局此刻应无匹配；断言项目侧被删。
        let _ = exec_forget(wd, "delete-me");
        let remaining = MemoryStore::project(wd).load();
        assert!(!remaining.iter().any(|e| e.contains("delete-me")));
        assert!(remaining.iter().any(|e| e.contains("keep-me")));
    }

    #[test]
    fn cost_sums_turn_stats_tokens() {
        let mut meta = NativeSessionMeta::new("cost", "/tmp/cost-test", 1);
        meta.turn_stats.push(TurnStat {
            after_message: 2,
            position_valid: true,
            turn_id: 1,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 100,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
            model_usage: Vec::new(),
        });
        meta.turn_stats.push(TurnStat {
            after_message: 4,
            position_valid: true,
            turn_id: 2,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 120,
            total_tokens: 250,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
            model_usage: Vec::new(),
        });
        let report = jeikcode_capabilities::session::aggregate_session_cost(&meta);
        assert_eq!(report.total_tokens, 350);
        assert_eq!(report.unattributed_tokens, 350);
    }

    #[test]
    fn todo_derives_from_last_todowrite_call() {
        use jeikcode_kernel::tool::ToolCall;

        let args = r#"{"todos":[{"content":"写测试","status":"in_progress"},{"content":"提交","status":"pending"}]}"#;
        let messages = vec![jeikcode_kernel::message::Message::assistant(
            "",
            vec![ToolCall {
                id: "call1".into(),
                name: "todowrite".into(),
                arguments: args.into(),
            }],
        )];
        let session_id = format!("daemon-todo-{}", std::process::id());
        jeikcode_capabilities::tools::commit_todo_board(
            &session_id,
            &[
                jeikcode_capabilities::tools::TodoItem {
                    id: None,
                    content: "写测试".into(),
                    status: jeikcode_capabilities::tools::todo::TodoStatus::InProgress,
                },
                jeikcode_capabilities::tools::TodoItem {
                    id: None,
                    content: "提交".into(),
                    status: jeikcode_capabilities::tools::todo::TodoStatus::Pending,
                },
            ],
        );
        let todos = todo_items_from_board(&session_id);

        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "写测试");
        assert_eq!(todos[1].content, "提交");
        let _ = messages;
        let _ = args;
    }

    #[test]
    fn todo_projection_keeps_completed_board_until_terminal_settlement() {
        let session_id = format!("daemon-todo-completed-{}", std::process::id());
        jeikcode_capabilities::tools::purge_todo_board(&session_id);
        let items = vec![jeikcode_capabilities::tools::TodoItem {
            id: None,
            content: "finished task".into(),
            status: jeikcode_capabilities::tools::todo::TodoStatus::Completed,
        }];
        let committed = jeikcode_capabilities::tools::commit_todo_board(&session_id, &items);

        let projected = todo_items_from_board(&session_id);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].content, "finished task");
        assert_eq!(
            jeikcode_capabilities::tools::read_todo_board(&session_id)
                .unwrap()
                .items,
            committed.items,
        );
        jeikcode_capabilities::tools::purge_todo_board(&session_id);
    }

    #[test]
    fn todo_empty_session_returns_empty() {
        let todos = todo_items_from_board("daemon-todo-missing");
        assert!(todos.is_empty());
    }

    #[test]
    fn exec_diff_returns_stat_in_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // init a repo with one committed file + a working-tree change
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        run(&["init"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "init"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let res = exec_diff(dir.path()).unwrap();
        match res {
            CommandResult::Diff { stat } => assert!(stat.contains("a.txt"), "stat was: {stat}"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn goal_command_helpers_parse_rounds_and_prompt_presence() {
        assert_eq!(
            parse_goal_rounds_arg("+25"),
            Some(jeikcode_coding::GoalRoundAdjustment::Add(25))
        );
        assert_eq!(
            parse_goal_rounds_arg("100"),
            Some(jeikcode_coding::GoalRoundAdjustment::Set(100))
        );
        assert_eq!(
            parse_goal_rounds_arg("0"),
            Some(jeikcode_coding::GoalRoundAdjustment::Set(0))
        );
        assert_eq!(parse_goal_rounds_arg(""), None);
        assert!(goal_launch_missing_prompt(""));
        assert!(goal_launch_missing_prompt("--max-rounds"));
        assert!(!goal_launch_missing_prompt("all tests pass"));
    }

    #[test]
    fn format_goal_status_message_includes_phase_round_and_condition() {
        let progress = crate::GoalProgressWire {
            active: true,
            phase: "pursuing".into(),
            terminal: None,
            round: 2,
            max_rounds: Some(10),
            elapsed_secs: 65,
            tokens_used: 1500,
            max_tokens: None,
            condition: "make tests pass".into(),
            last_reason: None,
            continuous: false,
        };
        let msg = format_goal_status_message(&progress);
        assert!(msg.contains("Goal [pursuing] round 3/10"));
        assert!(msg.contains("1m 05s"));
        assert!(msg.contains("1500 tok"));
        assert!(msg.contains("make tests pass"));
    }
}
