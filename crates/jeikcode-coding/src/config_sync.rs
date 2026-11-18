use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const LAST_SEEN_VERSION_FILE: &str = ".last_seen_version";

/// 读取上一次同步或提示过的版本号
pub fn get_last_seen_version(jeikcode_home: &Path) -> Option<String> {
    let p = jeikcode_home.join(LAST_SEEN_VERSION_FILE);
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// 写入当前记录的版本号
pub fn set_last_seen_version(jeikcode_home: &Path, version: &str) -> std::io::Result<()> {
    let p = jeikcode_home.join(LAST_SEEN_VERSION_FILE);
    fs::write(p, version.trim())
}

/// 内置全量默认模板条目定义
pub struct BundledFileEntry {
    pub relative_path: &'static str,
    pub content: &'static str,
    pub description: &'static str,
}

/// 全量内置资产列表：包含 ~/.jeikcode 下的所有默认配置文件与提示词目录
pub const BUNDLED_ASSETS: &[BundledFileEntry] = &[
    BundledFileEntry {
        relative_path: "prompts/init.yaml",
        content: include_str!("../assets/prompts/init.yaml"),
        description: "提示词与身份定义 (init.yaml)",
    },
    BundledFileEntry {
        relative_path: "prompts/rules.yaml",
        content: include_str!("../assets/prompts/rules.yaml"),
        description: "工作流与执行规范 (rules.yaml)",
    },
    BundledFileEntry {
        relative_path: "config.toml",
        content: include_str!("../assets/default-config.toml"),
        description: "系统与工具通用配置 (config.toml — 保留用户模型/账号)",
    },
    BundledFileEntry {
        relative_path: "config_teachs.md",
        content: include_str!("../assets/config_teachs.md"),
        description: "Agent 友好配置文件教程与配置指南 (config_teachs.md)",
    },
    BundledFileEntry {
        relative_path: "builtin-tools.txt",
        content: include_str!("../../jeikcode-capabilities/assets/builtin-tools.txt"),
        description: "内置工具清单 (builtin-tools.txt)",
    },
    BundledFileEntry {
        relative_path: "mcp.json",
        content: include_str!("../../jeikcode-capabilities/assets/mcp.json"),
        description: "默认 MCP 服务器 (mcp.json)",
    },
    BundledFileEntry {
        relative_path: "user-wrap.md",
        content: include_str!("../../jeikcode-capabilities/assets/user-wrap.md"),
        description: "用户提问包装模板 (user-wrap.md — 支持 {{input}} 动态占位符)",
    },
    BundledFileEntry {
        relative_path: ".codegraphignore",
        content: include_str!("../../jeikcode-capabilities/assets/.codegraphignore"),
        description: "代码图谱 + 搜索默认忽略规则 (.codegraphignore)",
    },
    BundledFileEntry {
        relative_path: ".codegraphignore.graph",
        content: include_str!("../../jeikcode-capabilities/assets/.codegraphignore.graph"),
        description: "敏感凭据忽略规则（图谱 + 搜索默认排除）(.codegraphignore.graph)",
    },
    BundledFileEntry {
        relative_path: "thesaurus/admin_system.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/admin_system.txt"),
        description: "词林 admin_system.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/agent_core.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/agent_core.txt"),
        description: "词林 agent_core.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/ai_agent.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/ai_agent.txt"),
        description: "词林 ai_agent.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/computer_science.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/computer_science.txt"),
        description: "词林 computer_science.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/ailaierp.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/ailaierp.txt"),
        description: "词林 ailaierp.txt (Ailai ERP与电商)",
    },
    BundledFileEntry {
        relative_path: "thesaurus/fullstack_dev.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/fullstack_dev.txt"),
        description: "词林 fullstack_dev.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/medical.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/medical.txt"),
        description: "词林 medical.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/robotics.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/robotics.txt"),
        description: "词林 robotics.txt",
    },
    BundledFileEntry {
        relative_path: "thesaurus/web_http.txt",
        content: include_str!("../../jeikcode-capabilities/assets/thesaurus/web_http.txt"),
        description: "词林 web_http.txt",
    },
    BundledFileEntry {
        relative_path: "teaches/00_overview_index.md",
        content: include_str!("../../jeikcode-capabilities/assets/teaches/00_overview_index.md"),
        description: "配置知识库索引 (teaches/00_overview_index.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/01_prompts_and_context.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/01_prompts_and_context.md"
        ),
        description: "提示词与上下文指南 (teaches/01_prompts_and_context.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/02_models_and_providers.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/02_models_and_providers.md"
        ),
        description: "模型与提供商指南 (teaches/02_models_and_providers.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/03_mcp_and_skills.md",
        content: include_str!("../../jeikcode-capabilities/assets/teaches/03_mcp_and_skills.md"),
        description: "MCP与Skills指南 (teaches/03_mcp_and_skills.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/04_thesaurus_and_retrieval.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/04_thesaurus_and_retrieval.md"
        ),
        description: "词林检索相关性指南 (teaches/04_thesaurus_and_retrieval.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/05_tools_and_timeouts.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/05_tools_and_timeouts.md"
        ),
        description: "工具超时与策略指南 (teaches/05_tools_and_timeouts.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/06_directories_and_system.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/06_directories_and_system.md"
        ),
        description: "系统目录与文件全景指南 (teaches/06_directories_and_system.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/07_project_constraints_and_rules.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/07_project_constraints_and_rules.md"
        ),
        description: "项目约束与业务知识包指南 (teaches/07_project_constraints_and_rules.md)",
    },
    BundledFileEntry {
        relative_path: "teaches/08_updates_and_releases.md",
        content: include_str!(
            "../../jeikcode-capabilities/assets/teaches/08_updates_and_releases.md"
        ),
        description: "升级与发版指南 (teaches/08_updates_and_releases.md)",
    },
];

/// 官方已废弃/重命名的历史遗留文件列表（需在升级时提示清理或移除）
pub const STALE_HOME_FILES: &[&str] = &[
    "thesaurus/ecommerce.txt",
    "prompts/prompts.md",
    "prompts/内置工具.yaml",
    "prompts/内置技能.yaml",
    "prompts/root_docs_内置工具.yaml",
    "prompts/root_docs_内置技能.yaml",
    "prompts/root_docs_prompts.md",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// 新增文件（内置模板存在，本地缺失）
    New,
    /// 修改文件（本地存在，与内置模板内容有差异）
    Modified,
    /// 废弃/删除文件（旧版遗留文件，已被新版淘汰重命名）
    Obsolete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigDiffItem {
    pub relative_path: String,
    pub description: String,
    pub target_path: PathBuf,
    #[serde(skip_serializing, default)]
    pub new_content: String,
    pub kind: DiffKind,
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_preview: Option<String>,
}

/// Keys that stay on upgrade: the user's model/account/provider customisation.
pub const PRESERVE_CONFIG_KEYS: &[&str] = &[
    "models",
    "default_model",
    "default_provider",
    "providers",
    "provider_accounts",
    "provider",
    "model",
    "profiles",
    "reasoning_effort",
    "evaluator_provider",
    // The first-run / WebUI language switch. Upgrade must not flip it back
    // to the template default.
    "language",
];

fn ui_is_english() -> bool {
    jeikcode_config::i18n::current_locale() == jeikcode_config::locale::Locale::En
}

/// Description shown in the upgrade picker. Follows the global language switch.
pub fn localized_asset_description(relative_path: &str, zh_fallback: &str) -> String {
    if !ui_is_english() {
        return zh_fallback.to_string();
    }
    let en = match relative_path {
        "prompts/init.yaml" => "Identity and system prompt prefix (init.yaml)",
        "prompts/rules.yaml" => "Workflow and tool rules (rules.yaml)",
        "config.toml" => "Shared settings (config.toml — your models and accounts stay)",
        "config_teachs.md" => "Config guide for agents (config_teachs.md)",
        "builtin-tools.txt" => "Built-in tool list (builtin-tools.txt)",
        "mcp.json" => "Default MCP servers (mcp.json)",
        "user-wrap.md" => "User prompt wrap template (user-wrap.md, {{input}})",
        ".codegraphignore" => "Code graph + default search ignore rules (.codegraphignore)",
        ".codegraphignore.graph" => {
            "Sensitive-credential ignore rules (graph + default search exclusion) (.codegraphignore.graph)"
        }
        "thesaurus/admin_system.txt" => "Thesaurus: admin systems",
        "thesaurus/agent_core.txt" => "Thesaurus: agent core",
        "thesaurus/ai_agent.txt" => "Thesaurus: AI agent",
        "thesaurus/computer_science.txt" => "Thesaurus: computer science",
        "thesaurus/ailaierp.txt" => "Thesaurus: Ailai ERP and commerce",
        "thesaurus/fullstack_dev.txt" => "Thesaurus: fullstack development",
        "thesaurus/medical.txt" => "Thesaurus: medical",
        "thesaurus/robotics.txt" => "Thesaurus: robotics",
        "thesaurus/web_http.txt" => "Thesaurus: web and HTTP",
        "teaches/00_overview_index.md" => "Guide index (teaches/00_overview_index.md)",
        "teaches/01_prompts_and_context.md" => "Prompts and context guide",
        "teaches/02_models_and_providers.md" => "Models and providers guide",
        "teaches/03_mcp_and_skills.md" => "MCP and skills guide",
        "teaches/04_thesaurus_and_retrieval.md" => "Thesaurus and retrieval guide",
        "teaches/05_tools_and_timeouts.md" => "Tools and timeouts guide",
        "teaches/06_directories_and_system.md" => "Directories and system guide",
        "teaches/07_project_constraints_and_rules.md" => "Project rules guide",
        "teaches/08_updates_and_releases.md" => "Updates and releases guide",
        _ => return zh_fallback.to_string(),
    };
    en.to_string()
}

/// Recursively merge missing keys/tables from `template_tbl` into `existing_tbl`
/// without overwriting any existing user values, comments, or formatting.
fn merge_toml_tables_non_destructively(
    existing_tbl: &mut toml_edit::Table,
    template_tbl: &toml_edit::Table,
) {
    for (key, template_item) in template_tbl.iter() {
        match existing_tbl.get_mut(key) {
            None => {
                existing_tbl.insert(key, template_item.clone());
            }
            Some(existing_item) => {
                if let (Some(existing_sub), Some(template_sub)) =
                    (existing_item.as_table_mut(), template_item.as_table())
                {
                    merge_toml_tables_non_destructively(existing_sub, template_sub);
                }
            }
        }
    }
}

/// Non-destructively merge missing defaults from `new_template` into `existing`
/// using `toml_edit`, preserving `[coding]` (`compact_threshold`, etc.),
/// `[models]`, `[providers]`, `[provider_accounts]`, custom fields, and comments.
pub fn merge_user_config_preserving_models(existing: &str, new_template: &str) -> String {
    if existing.trim().is_empty() {
        return new_template.to_string();
    }
    let mut existing_doc = match existing.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => doc,
        Err(_) => return existing.to_string(),
    };
    let template_doc = match new_template.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => doc,
        Err(_) => return existing.to_string(),
    };

    merge_toml_tables_non_destructively(existing_doc.as_table_mut(), template_doc.as_table());
    existing_doc.to_string()
}

/// Non-destructively merge bundled `mcp.json` defaults into an existing `mcp.json`
/// without ever removing or overwriting existing user `mcpServers` entries.
/// Commented JSONC files are left untouched (`Ok(existing.to_string())`).
pub fn merge_mcp_json_preserving_servers(existing: &str, bundled: &str) -> Result<String> {
    if existing.trim().is_empty() {
        return Ok(bundled.to_string());
    }
    let stripped = jeikcode_capabilities::mcp::config::strip_json_comments(existing);
    if stripped != existing {
        // Respect the JSONC comment contract: never rewrite a commented file.
        return Ok(existing.to_string());
    }
    let mut existing_val: Value = match serde_json::from_str(existing) {
        Ok(v) => v,
        Err(_) => return Ok(existing.to_string()),
    };
    let bundled_val: Value = match serde_json::from_str(bundled) {
        Ok(v) => v,
        Err(_) => return Ok(existing.to_string()),
    };

    let Some(existing_obj) = existing_val.as_object_mut() else {
        return Ok(existing.to_string());
    };
    let Some(bundled_obj) = bundled_val.as_object() else {
        return Ok(existing.to_string());
    };

    let mut merged_servers = Map::new();
    if let Some(Value::Object(m)) = existing_obj.get("servers") {
        for (k, v) in m {
            merged_servers.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Object(m)) = existing_obj.get("mcpServers") {
        for (k, v) in m {
            merged_servers.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Object(m)) = bundled_obj.get("mcpServers") {
        for (k, v) in m {
            merged_servers.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    existing_obj.insert("mcpServers".to_string(), Value::Object(merged_servers));
    existing_obj.remove("servers");

    let text = serde_json::to_string_pretty(&existing_val)
        .context("Failed to serialize merged mcp.json")?;
    Ok(format!("{text}\n"))
}

fn build_diff_preview(old: &str, new: &str, kind: DiffKind, relative_path: &str) -> Option<String> {
    const MAX_PREVIEW_BYTES: usize = 3500;
    const MAX_PREVIEW_LINES: usize = 60;
    let raw = match kind {
        DiffKind::Obsolete => format!("--- {relative_path}\n+++ /dev/null\n(remove obsolete file)"),
        DiffKind::New => {
            let mut out = format!("--- /dev/null\n+++ {relative_path}\n");
            for (idx, line) in new.lines().enumerate() {
                if idx >= MAX_PREVIEW_LINES {
                    out.push_str("... (truncated)\n");
                    break;
                }
                out.push('+');
                out.push_str(line);
                out.push('\n');
            }
            out
        }
        DiffKind::Modified => {
            let diff = similar::TextDiff::from_lines(old, new);
            let mut udiff = diff.unified_diff();
            udiff.context_radius(2);
            udiff.header(
                &format!("disk/{relative_path}"),
                &format!("merged/{relative_path}"),
            );
            let rendered = udiff.to_string();
            if rendered.trim().is_empty() {
                return None;
            }
            let mut out = String::new();
            for (idx, line) in rendered.lines().enumerate() {
                if idx >= MAX_PREVIEW_LINES || out.len() + line.len() + 1 > MAX_PREVIEW_BYTES {
                    out.push_str("... (truncated)\n");
                    break;
                }
                out.push_str(line);
                out.push('\n');
            }
            out
        }
    };
    if raw.len() > MAX_PREVIEW_BYTES {
        let mut end = MAX_PREVIEW_BYTES;
        while !raw.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        Some(format!("{}...\n", &raw[..end]))
    } else {
        Some(raw)
    }
}

/// Guard against overwriting an existing `config.toml` with content that drops
/// existing user sections/keys (`[coding]`, `[providers]`, `[models]`, etc.).
fn guard_config_toml_write(target: &Path, new_content: &str) -> Result<()> {
    if !target.is_file() {
        return Ok(());
    }
    let existing = fs::read_to_string(target).unwrap_or_default();
    if existing.trim().is_empty() {
        return Ok(());
    }
    let Ok(existing_doc) = existing.parse::<toml_edit::DocumentMut>() else {
        return Ok(());
    };
    let new_doc = new_content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("Refusing to write invalid TOML to {}", target.display()))?;

    for protected in [
        "coding",
        "models",
        "providers",
        "provider_accounts",
        "default_model",
        "default_provider",
    ] {
        if existing_doc.contains_key(protected) && !new_doc.contains_key(protected) {
            bail!(
                "Refusing to overwrite {} because proposed config drops existing `{}`",
                target.display(),
                protected
            );
        }
    }
    Ok(())
}

/// MCP / skills 多为用户自定义接线与技能包，upgrade 交互勾选默认不覆盖。
/// `teaches/` 文档与 `prompts/root_docs_*` 说明文件不在此列。
pub fn is_user_custom_mcp_or_skill(relative_path: &str) -> bool {
    let p = relative_path.replace('\\', "/");
    if p.starts_with("teaches/") {
        return false;
    }
    if p == "mcp.json" || p.ends_with("/mcp.json") || p == ".mcp.json" {
        return true;
    }
    p == "skills" || p.starts_with("skills/") || p.contains("/skills/")
}

/// 交互式 upgrade 的默认勾选：MCP / skills 以及已有用户的 config.toml 默认不选（仅 opt-in），
/// 废弃清理项与新增文件仍默认勾选。
pub fn default_selected_for(relative_path: &str, kind: DiffKind) -> bool {
    match kind {
        DiffKind::Obsolete => true,
        DiffKind::New => !is_user_custom_mcp_or_skill(relative_path),
        DiffKind::Modified => {
            !is_user_custom_mcp_or_skill(relative_path) && relative_path != "config.toml"
        }
    }
}

/// 扫描 ~/.jeikcode 目录下所有涉及的内置非模型配置变更项（新增、更新修改、废弃清理）
pub fn scan_jeikcode_config_diffs(jeikcode_home: &Path) -> Vec<ConfigDiffItem> {
    let mut diffs = Vec::new();

    // 1. 扫描内置官方资产：检测新增与修改更新
    for entry in BUNDLED_ASSETS {
        let target = jeikcode_home.join(entry.relative_path);
        let bundled_content = entry.content;

        if !target.exists() {
            diffs.push(ConfigDiffItem {
                relative_path: entry.relative_path.to_string(),
                description: localized_asset_description(entry.relative_path, entry.description),
                target_path: target,
                new_content: bundled_content.to_string(),
                kind: DiffKind::New,
                selected: default_selected_for(entry.relative_path, DiffKind::New),
                diff_preview: build_diff_preview(
                    "",
                    bundled_content,
                    DiffKind::New,
                    entry.relative_path,
                ),
            });
            continue;
        }

        let existing_content = fs::read_to_string(&target).unwrap_or_default();

        if entry.relative_path == "config.toml" {
            let merged = merge_user_config_preserving_models(&existing_content, bundled_content);
            if merged.trim() != existing_content.trim() {
                diffs.push(ConfigDiffItem {
                    relative_path: entry.relative_path.to_string(),
                    description: localized_asset_description(
                        entry.relative_path,
                        entry.description,
                    ),
                    target_path: target,
                    diff_preview: build_diff_preview(
                        &existing_content,
                        &merged,
                        DiffKind::Modified,
                        entry.relative_path,
                    ),
                    new_content: merged,
                    kind: DiffKind::Modified,
                    selected: default_selected_for(entry.relative_path, DiffKind::Modified),
                });
            }
        } else if entry.relative_path == "mcp.json" {
            let merged = merge_mcp_json_preserving_servers(&existing_content, bundled_content)
                .unwrap_or_else(|_| existing_content.clone());
            if merged.trim() != existing_content.trim() {
                diffs.push(ConfigDiffItem {
                    relative_path: entry.relative_path.to_string(),
                    description: localized_asset_description(
                        entry.relative_path,
                        entry.description,
                    ),
                    target_path: target,
                    diff_preview: build_diff_preview(
                        &existing_content,
                        &merged,
                        DiffKind::Modified,
                        entry.relative_path,
                    ),
                    new_content: merged,
                    kind: DiffKind::Modified,
                    selected: default_selected_for(entry.relative_path, DiffKind::Modified),
                });
            }
        } else {
            // 对 prompts/*.yaml, thesaurus/*.txt, teaches/*.md 等文件进行全文比对
            if existing_content.trim() != bundled_content.trim() {
                diffs.push(ConfigDiffItem {
                    relative_path: entry.relative_path.to_string(),
                    description: localized_asset_description(
                        entry.relative_path,
                        entry.description,
                    ),
                    target_path: target,
                    diff_preview: build_diff_preview(
                        &existing_content,
                        bundled_content,
                        DiffKind::Modified,
                        entry.relative_path,
                    ),
                    new_content: bundled_content.to_string(),
                    kind: DiffKind::Modified,
                    selected: default_selected_for(entry.relative_path, DiffKind::Modified),
                });
            }
        }
    }

    // 2. 扫描历史已知废弃或已被替换的旧文件（如 ecommerce.txt）
    for stale_rel in STALE_HOME_FILES {
        let stale_path = jeikcode_home.join(stale_rel);
        if stale_path.is_file() {
            let description = if ui_is_english() {
                format!("Obsolete leftover ({stale_rel})")
            } else {
                format!("废弃/旧版本遗留项 ({stale_rel})")
            };
            diffs.push(ConfigDiffItem {
                relative_path: stale_rel.to_string(),
                description,
                target_path: stale_path,
                new_content: String::new(),
                kind: DiffKind::Obsolete,
                selected: default_selected_for(stale_rel, DiffKind::Obsolete),
                diff_preview: build_diff_preview("", "", DiffKind::Obsolete, stale_rel),
            });
        }
    }

    diffs
}

pub fn remove_stale_home_files(jeikcode_home: &Path) {
    for rel in STALE_HOME_FILES {
        let p = jeikcode_home.join(rel);
        if p.exists() {
            let _ = fs::remove_file(&p);
        }
    }
}

fn write_diff_item_safely(item: &ConfigDiffItem) -> Result<()> {
    if item.relative_path == "mcp.json" {
        let val: Value = serde_json::from_str(&item.new_content)
            .with_context(|| format!("Invalid MCP JSON for {}", item.target_path.display()))?;
        return jeikcode_capabilities::mcp::write_mcp_json_safely(&item.target_path, &val, false);
    }
    if item.relative_path == "config.toml" {
        guard_config_toml_write(&item.target_path, &item.new_content)?;
    }
    if let Some(parent) = item.target_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent dir {}", parent.display()))?;
    }
    if item.relative_path == "config.toml" {
        jeikcode_config::backup_sibling_bak(&item.target_path)?;
    }
    fs::write(&item.target_path, &item.new_content)
        .with_context(|| format!("Failed to write {}", item.target_path.display()))?;
    Ok(())
}

/// Checked variant of [`apply_selected_diffs`] that returns safety errors (such as
/// refusing to wipe non-empty `mcpServers` or protected `config.toml` sections) and
/// creates `.bak` backups before mutating `config.toml` or `mcp.json`.
pub fn apply_selected_diffs_checked(items: Vec<ConfigDiffItem>) -> Result<usize> {
    let mut applied_count = 0;
    for item in items.into_iter().filter(|it| it.selected) {
        match item.kind {
            DiffKind::New | DiffKind::Modified => {
                write_diff_item_safely(&item)?;
                let tag = if ui_is_english() {
                    if item.kind == DiffKind::New {
                        "added"
                    } else {
                        "updated"
                    }
                } else if item.kind == DiffKind::New {
                    "已新增"
                } else {
                    "已更新"
                };
                println!("  ✔ {tag}: {}", item.relative_path);
                applied_count += 1;
            }
            DiffKind::Obsolete => {
                if fs::remove_file(&item.target_path).is_ok() {
                    if ui_is_english() {
                        println!("  ✔ removed obsolete: {}", item.relative_path);
                    } else {
                        println!("  ✔ 已清理废弃项: {}", item.relative_path);
                    }
                    applied_count += 1;
                }
            }
        }
    }

    if applied_count > 0 {
        if ui_is_english() {
            println!(
                "Synced {applied_count} config file(s). Your models, accounts, and language choice were kept."
            );
        } else {
            println!(
                "✨ 成功同步了 {applied_count} 个配置文件（已自动保护用户模型配置、语言选择与 MCP/Skills）！"
            );
        }
    }
    Ok(applied_count)
}

/// 自动应用已选中的差异项（供 `upgrade -y` / `--auto` 以及 CLI 使用，自动保护 MCP / skills 与用户模型）
pub fn apply_selected_diffs(items: Vec<ConfigDiffItem>) -> usize {
    match apply_selected_diffs_checked(items) {
        Ok(count) => count,
        Err(err) => {
            eprintln!("⚠ Config sync skipped unsafe write: {err:#}");
            0
        }
    }
}

/// 非交互式直接写入/更新全量内置资产（供 `jeikcode setup --defaults` / `jeikcode setup -y` 及自动化脚本使用）
pub fn apply_all_bundled_assets(jeikcode_home: &Path, force: bool) -> Result<usize> {
    remove_stale_home_files(jeikcode_home);
    let mut applied_count = 0;

    for entry in BUNDLED_ASSETS {
        let target = jeikcode_home.join(entry.relative_path);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if !target.exists() {
            fs::write(&target, entry.content)?;
            println!("  ✔ [新增] {}", entry.relative_path);
            applied_count += 1;
        } else if entry.relative_path == "config.toml" {
            let existing_content = fs::read_to_string(&target).unwrap_or_default();
            let merged = merge_user_config_preserving_models(&existing_content, entry.content);
            if merged.trim() != existing_content.trim() || force {
                guard_config_toml_write(&target, &merged)?;
                jeikcode_config::backup_sibling_bak(&target)?;
                fs::write(&target, &merged)?;
                println!(
                    "  ✔ [更新] {} (已保留用户模型/账号配置)",
                    entry.relative_path
                );
                applied_count += 1;
            }
        } else if entry.relative_path == "mcp.json" {
            let existing_content = fs::read_to_string(&target).unwrap_or_default();
            let merged = merge_mcp_json_preserving_servers(&existing_content, entry.content)?;
            if merged.trim() != existing_content.trim() {
                let val: Value = serde_json::from_str(&merged)?;
                jeikcode_capabilities::mcp::write_mcp_json_safely(&target, &val, false)?;
                println!("  ✔ [合并] {}", entry.relative_path);
                applied_count += 1;
            }
        } else if force {
            fs::write(&target, entry.content)?;
            println!("  ✔ [覆盖] {}", entry.relative_path);
            applied_count += 1;
        } else {
            let existing_content = fs::read_to_string(&target).unwrap_or_default();
            if existing_content.trim() != entry.content.trim() {
                fs::write(&target, entry.content)?;
                println!("  ✔ [同步] {}", entry.relative_path);
                applied_count += 1;
            }
        }
    }

    Ok(applied_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_destructive_merge_preserves_coding_compact_threshold_and_custom_fields() {
        let existing = r#"# user comment must survive
default_model = "gpt-5.1"
language = "zh-CN"
custom_top_level = "keep-me"

[coding]
compact_threshold = 0.35
compact_max_tokens = 120000

[subagent]
max_concurrent = 3
max_rounds = 230
"#;
        let template = include_str!("../assets/default-config.toml");
        let merged = merge_user_config_preserving_models(existing, template);
        let merged_val: toml::Value = toml::from_str(&merged).unwrap();

        assert!(merged.contains("# user comment must survive"));
        assert_eq!(
            merged_val.get("custom_top_level").and_then(|v| v.as_str()),
            Some("keep-me")
        );
        let coding = merged_val.get("coding").expect("[coding] table");
        assert_eq!(
            coding.get("compact_threshold").and_then(|v| v.as_float()),
            Some(0.35)
        );
        assert_eq!(
            coding
                .get("compact_max_tokens")
                .and_then(|v| v.as_integer()),
            Some(120000)
        );
        // Backfilled missing defaults from template
        assert_eq!(
            coding.get("max_rounds").and_then(|v| v.as_integer()),
            Some(230)
        );
        // Existing user [subagent] values preserved
        let subagent = merged_val.get("subagent").expect("[subagent] table");
        assert_eq!(
            subagent.get("max_concurrent").and_then(|v| v.as_integer()),
            Some(3)
        );
        assert_eq!(
            merged_val.get("default_model").and_then(|v| v.as_str()),
            Some("gpt-5.1")
        );
        assert_eq!(
            merged_val.get("language").and_then(|v| v.as_str()),
            Some("zh-CN")
        );
    }

    #[test]
    fn stale_root_docs_are_detected_as_obsolete() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let prompts_dir = home.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(prompts_dir.join("root_docs_内置工具.yaml"), "stale").unwrap();
        std::fs::write(prompts_dir.join("root_docs_内置技能.yaml"), "stale").unwrap();
        std::fs::write(prompts_dir.join("root_docs_prompts.md"), "stale").unwrap();

        let diffs = scan_jeikcode_config_diffs(home);
        let obsolete: Vec<&str> = diffs
            .iter()
            .filter(|d| d.kind == DiffKind::Obsolete)
            .map(|d| d.relative_path.as_str())
            .collect();

        assert!(obsolete.contains(&"prompts/root_docs_内置工具.yaml"));
        assert!(obsolete.contains(&"prompts/root_docs_内置技能.yaml"));
        assert!(obsolete.contains(&"prompts/root_docs_prompts.md"));

        remove_stale_home_files(home);
        assert!(!prompts_dir.join("root_docs_内置工具.yaml").exists());
        assert!(!prompts_dir.join("root_docs_内置技能.yaml").exists());
        assert!(!prompts_dir.join("root_docs_prompts.md").exists());
    }

    #[test]
    fn apply_selected_diffs_checked_creates_bak_and_preserves_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        let existing_toml = "default_model = \"gpt-5.1\"\n[coding]\ncompact_threshold = 0.35\n";
        let existing_mcp = r#"{
  "mcpServers": {
    "context7": { "command": "npx", "args": [] }
  }
}
"#;
        std::fs::write(home.join("config.toml"), existing_toml).unwrap();
        std::fs::write(home.join("mcp.json"), existing_mcp).unwrap();

        let mut diffs = scan_jeikcode_config_diffs(home);
        // Existing config.toml and mcp.json must be opt-in (default unselected)
        let cfg_diff = diffs
            .iter()
            .find(|d| d.relative_path == "config.toml")
            .expect("config.toml diff should exist when template keys are missing");
        assert!(!cfg_diff.selected, "config.toml must not auto-select");
        assert!(cfg_diff.diff_preview.is_some());
        // mcp.json has no missing bundled keys, or if modified is unselected; verify merge preserves user server
        let merged_mcp = merge_mcp_json_preserving_servers(existing_mcp, "{}\n").unwrap();
        let merged_val: Value = serde_json::from_str(&merged_mcp).unwrap();
        assert!(merged_val["mcpServers"].get("context7").is_some());

        // Explicitly select config.toml and apply -> creates config.toml.bak and keeps compact_threshold
        for d in &mut diffs {
            if d.relative_path == "config.toml" {
                d.selected = true;
            }
        }
        let applied = apply_selected_diffs_checked(diffs).unwrap();
        assert!(applied >= 1);
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml.bak")).unwrap(),
            existing_toml
        );
        let new_toml = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&new_toml).unwrap();
        assert_eq!(parsed["coding"]["compact_threshold"].as_float(), Some(0.35));
    }

    #[test]
    fn guard_config_toml_write_blocks_dropping_coding_section() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        std::fs::write(&target, "[coding]\ncompact_threshold = 0.35\n").unwrap();

        let item = ConfigDiffItem {
            relative_path: "config.toml".into(),
            description: "test".into(),
            target_path: target.clone(),
            new_content: "language = \"en\"\n".into(),
            kind: DiffKind::Modified,
            selected: true,
            diff_preview: None,
        };
        let err = apply_selected_diffs_checked(vec![item]).unwrap_err();
        assert!(err.to_string().contains("drops existing `coding`"));
    }
}
