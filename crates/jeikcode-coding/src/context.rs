//! Native coding-runtime `/context` occupancy and prompt-breakdown computation.
//!
//! Corresponds to `coding/context` in the native migration architecture:
//! computes [`RuntimeContextStats`] from the active configuration, mounted
//! tool schemas, latest conversation snapshot, and last observed provider usage.

use std::path::PathBuf;

use jeikcode_capabilities::compaction::{extract_active_skills_in_span, is_compaction_anchor_text};
use jeikcode_kernel::message::{Message, Role, SessionSnapshot, LEGACY_COLD_SUMMARY_ORIGIN};
use jeikcode_kernel::tool::ToolDef;

use crate::config::CodingAgentConfig;
use crate::parts::CodingParts;
use crate::runtime::{ContextPrefixBlock, RuntimeContextStats};

fn classify_prefix_block_label(role: &Role, text: &str, idx: usize) -> String {
    let trimmed = text.trim_start();
    for (tag, label) in [
        ("<environment>", "environment"),
        ("<system_rules>", "system_rules"),
        ("<explore_strategy>", "explore_strategy"),
        ("<available_skills>", "available_skills"),
        ("=== AVAILABLE SKILLS ===", "available_skills"),
        ("<mcp-server-instructions>", "mcp_server_instructions"),
        ("<mcp_server_instructions>", "mcp_server_instructions"),
        ("<memory>", "memory"),
        ("<user_preferences>", "user_preferences"),
        ("<project_instructions>", "project_instructions"),
    ] {
        if trimmed.starts_with(tag) || trimmed.contains(tag) {
            return label.to_string();
        }
    }
    match role {
        Role::System => format!("system_block_{}", idx + 1),
        _ => format!("frozen_user_block_{}", idx + 1),
    }
}

fn build_baseline_prefix_blocks(blocks: &[String; 4]) -> Vec<ContextPrefixBlock> {
    let labels = [
        "environment",
        "system_rules",
        "explore_strategy",
        "system_extra",
    ];
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| !b.is_empty())
        .map(|(i, b)| ContextPrefixBlock {
            label: labels.get(i).copied().unwrap_or("system").to_string(),
            tokens: estimate_text_block_tokens(b),
        })
        .collect()
}

pub(crate) fn current_parts_snapshot(parts: &CodingParts) -> Option<SessionSnapshot> {
    let binding = parts.session.as_ref()?;
    binding.manager.load_snapshot(&binding.id).ok()
}

pub(crate) fn estimate_tool_defs_tokens(tools: &[ToolDef]) -> usize {
    if tools.is_empty() {
        return 0;
    }
    let byte_count: usize = tools
        .iter()
        .map(|tool| {
            let schema_len = tool.parameters.to_string().len();
            tool.name.len() + tool.description.len() + schema_len + 24
        })
        .sum();
    (byte_count / 4).max(1)
}

pub(crate) fn estimate_text_block_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        (text.len() / 4).max(1) + 4
    }
}

/// Mirror [`Conversation::model_history`] filtering on a raw message slice
/// without cloning every retained [`Message`].
fn is_wire_history_message(message: &Message) -> bool {
    !message.is_display_only()
        && message.internal_origin.is_none()
        && !(message.role == Role::Assistant
            && message.text.is_empty()
            && message.tool_calls.is_empty())
}

/// Mirror [`Conversation::last_pressure`] on a raw snapshot slice so `/context`
/// on a resumed session recovers persisted occupancy before the first live turn.
fn snapshot_last_used_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .and_then(|m| m.meta.as_ref())
        .map(|meta| meta.used_tokens)
        .unwrap_or(0)
}

fn join_non_empty_blocks<'a>(blocks: impl Iterator<Item = &'a str>) -> String {
    blocks
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Compute [`RuntimeContextStats`] from prepared coding parts, active config,
/// optional session snapshot, and optional live provider token usage.
pub fn compute_runtime_context_stats(
    parts: &CodingParts,
    config: &CodingAgentConfig,
    latest_snapshot: Option<&SessionSnapshot>,
    observed_used_tokens: u32,
) -> RuntimeContextStats {
    let resumed_snapshot = if latest_snapshot.is_none() {
        parts.runtime_resume()
    } else {
        None
    };
    let disk_snapshot = if latest_snapshot.is_none() && resumed_snapshot.is_none() {
        current_parts_snapshot(parts)
    } else {
        None
    };
    let snapshot = latest_snapshot
        .or(resumed_snapshot)
        .or(disk_snapshot.as_ref());

    let tool_defs_tokens = parts
        .mounted_tools()
        .map(|tools| estimate_tool_defs_tokens(&tools.defs()))
        .unwrap_or(0);

    compute_snapshot_context_stats(
        snapshot,
        || parts.baseline_system_blocks(config),
        tool_defs_tokens,
        config.context_window,
        &config.model,
        config.working_dir.clone(),
        observed_used_tokens,
    )
}

/// Pure calculation of [`RuntimeContextStats`] from snapshot messages and
/// lazily evaluated baseline system blocks.
pub fn compute_snapshot_context_stats<F>(
    snapshot: Option<&SessionSnapshot>,
    baseline_blocks: F,
    tool_defs_tokens: usize,
    context_window: u32,
    model: &str,
    working_dir: PathBuf,
    observed_used_tokens: u32,
) -> RuntimeContextStats
where
    F: FnOnce() -> [String; 4],
{
    let (
        system_prompt,
        system_tokens,
        prefix_blocks,
        cold_zone_tokens,
        est_non_sys_tokens,
        total_messages,
        snapshot_used_tokens,
        loaded_skills,
    ) = if let Some(snapshot) = snapshot {
        let messages = &snapshot.messages;
        let loaded_skills = extract_active_skills_in_span(messages);
        let wire: Vec<&Message> = messages
            .iter()
            .filter(|m| is_wire_history_message(m))
            .collect();

        let lead_system = wire.iter().take_while(|m| m.role == Role::System).count();
        let (system_prompt, system_tokens, prefix_blocks, frozen_end) = if lead_system > 0 {
            let mut end = lead_system;
            // Include the v2 frozen synthetic user prefix (skills, MCP, memory,
            // preferences, project instructions) before the first real prompt or anchor.
            while end < wire.len()
                && wire[end].role == Role::User
                && wire[end].synthetic
                && !is_compaction_anchor_text(&wire[end].text)
            {
                end += 1;
            }
            let prompt = join_non_empty_blocks(wire[..end].iter().map(|m| m.text.as_str()));
            let mut p_blocks = Vec::with_capacity(end);
            let mut tokens = 0usize;
            for (idx, m) in wire[..end].iter().enumerate() {
                let t = m.estimate_tokens() as usize;
                tokens = tokens.saturating_add(t);
                if !m.text.trim().is_empty() {
                    p_blocks.push(ContextPrefixBlock {
                        label: classify_prefix_block_label(&m.role, &m.text, idx),
                        tokens: t,
                    });
                }
            }
            (prompt, tokens, p_blocks, end)
        } else {
            let blocks = baseline_blocks();
            let prompt = join_non_empty_blocks(blocks.iter().map(String::as_str));
            let p_blocks = build_baseline_prefix_blocks(&blocks);
            let tokens: usize = p_blocks.iter().map(|b| b.tokens).sum();
            (prompt, tokens, p_blocks, 0)
        };

        let non_frozen = &wire[frozen_end..];
        let v2_cold_tokens: usize = non_frozen
            .iter()
            .filter(|m| m.role == Role::User && m.synthetic && is_compaction_anchor_text(&m.text))
            .map(|m| m.estimate_tokens() as usize)
            .sum();
        let legacy_cold_tokens: usize = messages
            .iter()
            .filter(|m| m.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN))
            .map(|m| m.estimate_tokens() as usize)
            .sum();
        let cold_tokens = v2_cold_tokens.saturating_add(legacy_cold_tokens);

        let wire_non_frozen_tokens: usize = non_frozen
            .iter()
            .map(|m| m.estimate_tokens() as usize)
            .sum();
        let est_non_frozen_tokens = wire_non_frozen_tokens.saturating_add(legacy_cold_tokens);
        let msg_count = non_frozen.len();
        let snapshot_used = snapshot_last_used_tokens(messages);

        (
            system_prompt,
            system_tokens,
            prefix_blocks,
            cold_tokens,
            est_non_frozen_tokens,
            msg_count,
            snapshot_used,
            loaded_skills,
        )
    } else {
        let blocks = baseline_blocks();
        let prompt = join_non_empty_blocks(blocks.iter().map(String::as_str));
        let p_blocks = build_baseline_prefix_blocks(&blocks);
        let tokens: usize = p_blocks.iter().map(|b| b.tokens).sum();
        (prompt, tokens, p_blocks, 0, 0, 0, 0, Vec::new())
    };

    let used_tokens = if observed_used_tokens > 0 {
        observed_used_tokens
    } else {
        snapshot_used_tokens
    };

    let sent_tokens = if used_tokens > 0 {
        let obs = used_tokens as usize;
        let overhead = system_tokens.saturating_add(tool_defs_tokens);
        if obs > overhead {
            obs.saturating_sub(overhead).max(cold_zone_tokens)
        } else {
            est_non_sys_tokens.max(cold_zone_tokens)
        }
    } else {
        est_non_sys_tokens
    };

    let utilization = if context_window > 0 {
        (used_tokens as f32 / context_window as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };

    RuntimeContextStats {
        context_window,
        used_tokens,
        utilization,
        model: model.to_string(),
        working_dir,
        system_tokens,
        tool_defs_tokens,
        cold_zone_tokens,
        sent_tokens,
        total_messages,
        system_prompt,
        prefix_blocks,
        loaded_skills,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use jeikcode_kernel::message::{
        Conversation, Message, MessageMeta, SessionSnapshot, LEGACY_COLD_SUMMARY_ORIGIN,
        LEGACY_COLD_SUMMARY_PREFIX, TURN_DIAGNOSTIC_ORIGIN,
    };
    use jeikcode_kernel::stream::TokenUsage;

    use super::compute_snapshot_context_stats;

    #[test]
    fn baseline_snapshot_reports_system_and_zero_messages() {
        let stats = compute_snapshot_context_stats(
            None,
            || {
                [
                    "Persona block".to_string(),
                    "Tool rules".to_string(),
                    String::new(),
                    "Env block".to_string(),
                ]
            },
            4_200,
            1_000_000,
            "test-model",
            PathBuf::from("."),
            0,
        );
        assert!(stats.system_tokens > 0);
        assert_eq!(stats.tool_defs_tokens, 4_200);
        assert!(stats.system_prompt.contains("Persona block"));
        assert_eq!(stats.cold_zone_tokens, 0);
        assert_eq!(stats.sent_tokens, 0);
        assert_eq!(stats.total_messages, 0);
    }

    #[test]
    fn uncompacted_snapshot_keeps_frozen_prefix_in_system_and_cold_zone_at_zero() {
        let sys_msg = Message::system("You are a coding assistant.");
        let frozen_memory = Message::synthetic_user("<memory>\nRemember Rust rules.\n</memory>");
        let first_user = Message::user("Fix the context breakdown bug.");
        let reply = Message::assistant("Sure, inspecting runtime stats.", Vec::new());
        let expected_sys_tokens =
            sys_msg.estimate_tokens() as usize + frozen_memory.estimate_tokens() as usize;
        let snap = SessionSnapshot::new(vec![sys_msg, frozen_memory, first_user, reply]);
        let tool_defs_tokens = 3_000;
        let observed = (expected_sys_tokens + tool_defs_tokens + 4_500) as u32;

        let stats = compute_snapshot_context_stats(
            Some(&snap),
            || unreachable!("snapshot already contains leading system message"),
            tool_defs_tokens,
            1_000_000,
            "test-model",
            PathBuf::from("."),
            observed,
        );
        assert_eq!(stats.used_tokens, observed);
        assert_eq!(stats.system_tokens, expected_sys_tokens);
        assert_eq!(stats.tool_defs_tokens, tool_defs_tokens);
        assert_eq!(
            stats.cold_zone_tokens, 0,
            "frozen prefix and first real user must not be billed as cold zone"
        );
        assert_eq!(stats.total_messages, 2);
        assert!(stats.system_prompt.contains("You are a coding assistant."));
        assert!(stats.system_prompt.contains("Remember Rust rules."));
        assert_eq!(
            stats.system_tokens + stats.tool_defs_tokens + stats.sent_tokens,
            observed as usize
        );
    }

    #[test]
    fn cold_zone_counts_compaction_anchors_and_legacy_cold_summaries_only() {
        let sys_msg = Message::system("System persona");
        let frozen = Message::synthetic_user("=== AVAILABLE SKILLS\n- rust");
        let first_user = Message::user("Keep going");
        let anchor = Message::synthetic_user(
            "<!-- jeikcode:anchor v1 -->\n[Compressed summary]\nEarlier files inspected.",
        );
        let legacy_cold = Message {
            internal_origin: Some(LEGACY_COLD_SUMMARY_ORIGIN.to_string()),
            ..Message::synthetic_user(format!("{LEGACY_COLD_SUMMARY_PREFIX}Legacy summary."))
        };
        let recent_reply = Message::assistant("Working on it.", Vec::new());
        let expected_cold =
            anchor.estimate_tokens() as usize + legacy_cold.estimate_tokens() as usize;
        let snap = SessionSnapshot::new(vec![
            sys_msg,
            frozen,
            first_user,
            anchor,
            legacy_cold,
            recent_reply,
        ]);

        let stats = compute_snapshot_context_stats(
            Some(&snap),
            || unreachable!("snapshot already contains leading system message"),
            1_000,
            200_000,
            "test-model",
            PathBuf::from("."),
            0,
        );
        assert_eq!(stats.cold_zone_tokens, expected_cold);
        // Frozen block stays in system_tokens/system_prompt; legacy internal_origin is excluded from wire count.
        assert_eq!(stats.total_messages, 3);
        assert!(stats.system_prompt.contains("AVAILABLE SKILLS"));
    }

    #[test]
    fn resumed_snapshot_recovers_last_pressure_and_matches_model_history_wire_filter() {
        let sys_msg = Message::system("System");
        let user = Message::user("Hi");
        let diagnostic = Message {
            internal_origin: Some(TURN_DIAGNOSTIC_ORIGIN.to_string()),
            ..Message::synthetic_user("Previous error notice")
        };
        let internal_note = Message {
            internal_origin: Some("jeikcode.internal_note".to_string()),
            ..Message::synthetic_user("Internal marker")
        };
        let empty_reasoning = Message {
            reasoning: Some("thought only".to_string()),
            ..Message::assistant("", Vec::new())
        };
        let mut assistant = Message::assistant("Final reply.", Vec::new());
        assistant.meta = Some(MessageMeta {
            ctx_window: 200_000,
            used_tokens: 42_000,
            utilization: 0.21,
            tokens: TokenUsage {
                prompt: 42_000,
                completion: 100,
                ..TokenUsage::default()
            },
            ..MessageMeta::default()
        });

        let msgs = vec![
            sys_msg,
            user,
            diagnostic,
            internal_note,
            empty_reasoning,
            assistant,
        ];
        let convo = Conversation {
            messages: msgs.clone(),
            cache_epoch: 0,
            ..Default::default()
        };
        let expected_wire_non_sys = convo
            .model_history()
            .into_iter()
            .skip(1)
            .collect::<Vec<_>>();
        let (_, expected_used, _) = convo.last_pressure();

        let snap = SessionSnapshot::new(msgs);
        let stats = compute_snapshot_context_stats(
            Some(&snap),
            || unreachable!("leading system present"),
            2_000,
            200_000,
            "test-model",
            PathBuf::from("."),
            0,
        );

        assert_eq!(stats.used_tokens, expected_used);
        assert_eq!(stats.total_messages, expected_wire_non_sys.len());
        assert_eq!(stats.used_tokens, 42_000);
        assert_eq!(
            stats.system_tokens + stats.tool_defs_tokens + stats.sent_tokens,
            42_000
        );
    }

    #[test]
    fn snapshot_context_stats_exposes_per_block_prefix_breakdown() {
        let msgs = vec![
            Message::system("<environment>\nPlatform: Windows\n</environment>"),
            Message::system("<system_rules>\nRules\n</system_rules>"),
            Message::synthetic_user("<available_skills>\n- plan-tribunal\n</available_skills>"),
            Message::synthetic_user("<memory>\n[Global]\n- fact\n</memory>"),
            Message::synthetic_user("<project_instructions>\n# AGENTS\n</project_instructions>"),
            Message::user("hello"),
        ];
        let snap = SessionSnapshot::new(msgs);
        let stats = compute_snapshot_context_stats(
            Some(&snap),
            || unreachable!(),
            500,
            200_000,
            "m",
            PathBuf::from("."),
            0,
        );
        let labels: Vec<&str> = stats
            .prefix_blocks
            .iter()
            .map(|b| b.label.as_str())
            .collect();
        assert_eq!(
            labels,
            vec![
                "environment",
                "system_rules",
                "available_skills",
                "memory",
                "project_instructions"
            ]
        );
        let sum: usize = stats.prefix_blocks.iter().map(|b| b.tokens).sum();
        assert_eq!(sum, stats.system_tokens);
    }
}
