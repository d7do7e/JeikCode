use super::messages::Msg;
use std::borrow::Cow;

pub(super) fn zh_tw(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::SessionShared => "已共用目前工作階段（與瀏覽器即時互通）".into(),
        Msg::SessionModelRequired => "未設定模型：請先使用 /model 或 /provider 新增提供商和模型後再開始對話。".into(),
        Msg::ServerNoServices => "沒有已安裝的 JeikCode 服務。".into(),
        Msg::WelcomeBannerLine1 => "歡迎使用 JeikCode — 你的終端 AI 程式助手".into(),
        Msg::WelcomeBannerLine2 => "直接輸入問題或任務即可開始對話；輸入 / 可開啟指令清單。".into(),
        Msg::WelcomeOptionConfigureManually => "手動設定".into(),
        Msg::WelcomeOptionConfigureManuallyHint => "手動輸入 API 金鑰與 Base URL".into(),
        Msg::WelcomeOptionSkip => "暫時跳過".into(),
        Msg::WelcomeOptionSkipHint => "直接進入對話介面，稍後設定".into(),
        Msg::WelcomeTipsHeading => "上手提示：".into(),
        Msg::WelcomeTipModel => "切換目前使用的模型".into(),
        Msg::WelcomeTipProvider => "新增自訂模型".into(),
        Msg::WelcomeTipResume => "檢視並繼續歷史工作階段".into(),
        Msg::WelcomeTipSkills => "瀏覽可用技能".into(),
        Msg::WelcomeTipPlugin => "管理外掛程式與市場".into(),
        Msg::WelcomeTipWebui => "啟動網頁介面".into(),
        Msg::WelcomeTipMcp => "檢視 MCP 伺服器狀態".into(),
        Msg::WelcomeTipPlan => "檢視或切換 Plan 模式".into(),
        Msg::WelcomeTipSession => "開始全新工作階段".into(),
        Msg::WelcomeTipLoop => "設定定時或循環任務".into(),
        Msg::WelcomeTipGoal => "設定目標驅動模式".into(),
        Msg::WelcomeTipInit => "初始化專案配置".into(),
        Msg::WelcomeTipLanguage => "切換顯示語言".into(),
        Msg::ProviderPanelTabAccounts => "帳號".into(),
        Msg::ProviderPanelTabModels => "模型".into(),
        Msg::ProviderPanelEmptyAccounts => "（尚無 Provider — 按 Ctrl+A 新增）".into(),
        Msg::ProviderPanelNoMatchingAccounts => "（無相符的 Provider）".into(),
        Msg::ProviderPanelEmptyModels => "（尚無模型 — 按 Ctrl+A 新增，將取得上游 /models）".into(),
        Msg::ProviderPanelNoMatchingModels => "（無相符的模型）".into(),
        Msg::ProviderPanelLegacyBadge => "舊版".into(),
        Msg::ProviderPanelDefaultBadge => "預設".into(),
        Msg::ProviderPanelModelCount { count } => format!("{count} 個模型").into(),
        Msg::ProviderPanelAddModelRow => "＋ 新增模型".into(),
        Msg::ProviderPanelAddProviderRow => "＋ 新增自訂 provider".into(),
        Msg::ProviderPanelAccountsHint =>
            "篩選 · ↑↓選擇 · ↵模型 · Ctrl+A新增 · Ctrl+E編輯 · Ctrl+Dx2 刪除 · Tab切換 · Esc關閉".into(),
        Msg::ProviderPanelModelsHint =>
            "篩選 · ↑↓選擇 · ↵預設/新增 · Ctrl+A新增模型並取得上游清單 · Ctrl+E編輯 · Ctrl+Dx2 刪除 · Tab切換 · Esc關閉".into(),
        Msg::ProviderPanelFilteredModelsHint { account } =>
            format!("〔{account}〕· ↑↓選擇 · ↵預設/新增 · Ctrl+A新增模型並取得上游清單 · Ctrl+E編輯 · Ctrl+Dx2 刪除 · Tab全部 · Esc關閉").into(),
        Msg::ProviderPanelModelSaved { model } => format!("已儲存模型「{model}」。").into(),
        Msg::ProviderPanelImageDirectWhileVlSet => {
            "圖片將直發本模型；要用 VL 請關閉「支援圖片輸入」。".into()
        }
        Msg::ProviderPanelAddTitle => "【新增 Provider】".into(),
        Msg::ProviderPanelEditAccountTitle { account } =>
            format!("【編輯帳號 {account}】").into(),
        Msg::ProviderPanelAddModelTitle => "【新增模型】".into(),
        Msg::ProviderPanelEditModelTitle => "【編輯模型】".into(),
        Msg::ProviderPanelFieldVendor => "廠商".into(),
        Msg::ProviderPanelFieldAccount => "帳號".into(),
        Msg::ProviderPanelFieldName => "名稱".into(),
        Msg::ProviderPanelFieldProtocol => "協定".into(),
        Msg::ProviderPanelRequired => "(必填)".into(),
        Msg::ProviderPanelLocked => "(鎖定)".into(),
        Msg::ProviderPanelFieldBaseUrl => "Base URL".into(),
        Msg::ProviderPanelFieldApiKey => "API 金鑰".into(),
        Msg::ProviderPanelFieldModel => "模型".into(),
        Msg::ProviderPanelFieldWindow => "上下文視窗".into(),
        Msg::ProviderPanelFieldSupportsVision => "支援圖片輸入？".into(),
        Msg::ProviderPanelVisionYes => "是 [✓]  （將傳送 base64 圖片）".into(),
        Msg::ProviderPanelVisionNo => "否 [ ]  （僅文字）".into(),
        Msg::ProviderPanelFieldReasoningModel => "是否為思考模型？".into(),
        Msg::ProviderPanelReasoningYes => "是 [✓]  （多輪將回傳思考過程）".into(),
        Msg::ProviderPanelReasoningNo => "否 [ ]  （一般模型/不回傳思考）".into(),
        Msg::ProviderPanelFieldMakeDefault => "設為預設".into(),
        Msg::ProviderPanelSwitchHint => "←→ 切換".into(),
        Msg::ProviderPanelEnvHint { env } => format!("留空使用 ${env}").into(),
        Msg::ProviderPanelDefaultValue => "預設".into(),
        Msg::ProviderPanelKeepOriginal => "留空保留原值".into(),
        Msg::ProviderPanelProviderFormHint =>
            "Tab 下一項  ←→ 切廠商  空格 勾選  ↵ 儲存  Esc 返回".into(),
        Msg::ProviderPanelAccountFormHint => "Tab 切換  ↵ 儲存  Esc 返回".into(),
        Msg::ProviderPanelAddAccountHint =>
            "Tab 下一項  ←→ 切協定  ↵ 儲存  Esc 返回  （名稱必填；模型至模型頁新增）".into(),
        Msg::ProviderPanelEditAccountLockedHint =>
            "Tab 下一項  ↵ 儲存  Esc 返回  （廠商協定已鎖定）".into(),
        Msg::ProviderPanelEditAccountHint =>
            "Tab 下一項  ←→ 切協定  ↵ 儲存  Esc 返回".into(),
        Msg::ProviderPanelAccountNeedsKeyHint => "(該 provider 尚未設定)".into(),
        Msg::ProviderPanelModelFormHint =>
            "開啟即取得上游清單  輸入篩選  ↑↓選擇  Tab/↵填入  ←→切帳號（模型框為空時）  空格切換  Esc返回".into(),
        Msg::ProviderPanelModelPlaceholder => "(輸入篩選上游清單)".into(),
        Msg::ProviderPanelUpstreamListLabel => "上游模型清單".into(),
        Msg::ProviderPanelUpstreamPickHint => "↑↓ 選擇 · Tab/↵ 填入".into(),
        Msg::ProviderPanelUpstreamLoading => "載入中…".into(),
        Msg::ProviderPanelUpstreamLoaded { n } =>
            format!("已載入 {n} 個上游模型 · 輸入即篩選 · ↑↓ 選擇 · Tab/↵ 填入").into(),
        Msg::ProviderPanelUpstreamEmpty => "上游無模型清單，可直接輸入模型 id".into(),
        Msg::ProviderPanelUpstreamFailed { error } =>
            format!("上游清單請求失敗（{error}），可直接輸入模型 id").into(),
        Msg::ProviderPanelUpstreamNoUrl => "無可用上游位址，可直接輸入模型 id".into(),
        Msg::ModelPickerNoModelsConfigured => "(未設定模型 — 請使用 /provider 新增)".into(),
        Msg::ModelPickerNoModelsMatch => "(無相符模型)".into(),
        Msg::ModelPickerNoModelsMatchQuery { query } =>
            format!("(無相符 “{query}” 的模型 — 按 Backspace 清除)").into(),
        Msg::ModelSwitched { provider, model } =>
            format!("  目前工作階段已切換至 {provider} · {model}\n").into(),
        Msg::ModelSwitchedAndDefault { provider, model } =>
            format!("  已切換至 {provider} · {model}，並設為新工作階段的預設值\n").into(),
        Msg::LanguageSwitched { label, locale } =>
            format!("✓ 語言已切換至 {label} ({locale})\n").into(),
        Msg::CmdDescHelp => "顯示幫助資訊與所有可用指令".into(),
        Msg::CmdDescModel => "選擇或切換目前模型".into(),
        Msg::CmdDescProvider => "管理模型提供商與 API 帳號".into(),
        Msg::CmdDescQuit => "結束 JeikCode 對話".into(),
        Msg::CmdDescGoal => "設定完成目標（支援 shift、status、pause、resume、stop、clear、rounds）".into(),
        Msg::StreamStalled => "按 esc 可取消".into(),
        Msg::CompactNothingShort => "（無需壓縮 — 對話較短）\n".into(),
        Msg::CompactStarting => "（正在使用 LLM 摘要進行壓縮...）\n".into(),
        Msg::CompactInterrupted => "（壓縮已中斷 — coding runtime 已變更或停止）\n".into(),
        Msg::CompactUnavailableDuringSync => "同步模式下 /compact 暫不可用；請先執行 /sync off".into(),
        Msg::CompactUnavailableDuringResync => "本地 runtime 尚未恢復最新同步會話，暫不能執行 /compact".into(),
        Msg::LocalRuntimeRestorePending => "本地 runtime 正在恢復同步會話，請稍候".into(),
        Msg::LocalRuntimeRestoreTimedOut => "本地 runtime 恢復逾時，已重新接回 Live 同步".into(),
        Msg::CompactNothingNoSavings { before, after } =>
            format!("（無需壓縮 — 壓縮後不會節省 token：{} → {}）\n", before, after).into(),
        Msg::CompactDropped { messages, before, after } =>
            format!("（已壓縮 — 捨棄 {} 條訊息，{} → {} tokens）\n", messages, before, after).into(),
        Msg::Compacting => "正在壓縮…".into(),
        Msg::CompactingSlow => "正在壓縮…（較慢）".into(),
        Msg::CompactMarkDrain { messages, before, after } =>
            format!("已壓縮 · 摘要 {} 條 · ~{}→~{} tok", messages, before, after).into(),
        Msg::CompactMarkStub { saved } =>
            format!("已摺疊工具輸出 · 節省 ~{} tok", saved).into(),
        Msg::CompactThresholdStatus { summary, effective, path } =>
            format!(
                "  自動壓縮閾值：{summary}\n  \
                 目前模型生效：{effective}\n  \
                 設定檔：{path}\n  \
                 用法：/compact threshold <350k | 35% | 0.35 | off | default>\n"
            )
            .into(),
        Msg::CompactThresholdUpdated { summary, effective, path } =>
            format!(
                "  已更新自動壓縮閾值 → {summary}\n  \
                 目前模型生效：{effective}\n  \
                 已儲存至：{path}\n"
            )
            .into(),
        Msg::CompactThresholdInvalid { input } =>
            format!(
                "無效的自動壓縮閾值「{input}」。請使用百分比（10%-100% 或 0.1-1.0）、Token 數（>= 8000，例如 350k）、'off' 或 'default'。"
            )
            .into(),
        Msg::GoalHelp =>
            "  /goal — 朝著設定的條件自主進行多輪工作。\n  \
             子命令：\n  \
             \u{20}\u{20}/goal <條件>                          設定新目標；循環執行直到驗證達成\n  \
             \u{20}\u{20}/goal shift <條件>                    連續多輪次班次模式（--until-stopped --fresh-epoch --rollback-on-stall）\n  \
             \u{20}\u{20}/goal status                          檢視進度、輪次、Token 用量與啟用標誌\n  \
             \u{20}\u{20}/goal pause | resume                  暫停或恢復目前目標\n  \
             \u{20}\u{20}/goal stop  | clear                   停止目前目標或清除完成橫幅\n  \
             \u{20}\u{20}/goal rounds <+N|N>                   動態調整 max_rounds 輪次上限（0 = 無限制）\n  \
             \u{20}\u{20}/goal help                            顯示本說明\n  \
             參數標誌：\n  \
             \u{20}\u{20}--max-rounds <N>                      覆蓋本次目標的輪次上限\n  \
             \u{20}\u{20}--until-stopped                       連續滾動推進並自動開啟 fresh-epoch\n  \
             \u{20}\u{20}--fresh-epoch / --no-fresh-epoch      跨批次滾動時是否重設上下文至凍結前綴\n  \
             \u{20}\u{20}--rollback-on-stall / --no-rollback-on-stall  停滯熔斷時是否自動回滾檢查點\n  \
             \u{20}\u{20}--max-tokens <N>                      累計 Token 達到 N 時自動停止\n  \
             \u{20}\u{20}--verify | --no-verify                強制或略過獨立唯讀驗證\n  \
             \u{20}\u{20}--checkpoint                          儲存非破壞性檢查點參照\n  \
             說明：\n  \
             \u{20}\u{20}- 預設值可在 config.toml 的 [coding] 或 [goal] 下設定。\n  \
             \u{20}\u{20}- 按 Esc 可暫停執行中的目標；按 Ctrl+C 停止。\n".into(),
        Msg::GoalCheatSheet {
            max_rounds,
            rollback_on_stall,
            fresh_epoch,
        } => format!(
            "  ◎ /goal — 目前沒有進行中的目標。\n  \
             子命令：\n  \
             \u{20}\u{20}/goal <條件>        循環執行直到驗證達成\n  \
             \u{20}\u{20}/goal shift <條件>  連續多輪次班次模式（--until-stopped --fresh-epoch --rollback-on-stall）\n  \
             \u{20}\u{20}/goal status        檢視目標進度與參數\n  \
             \u{20}\u{20}/goal pause | resume  暫停或恢復目標\n  \
             \u{20}\u{20}/goal stop  | clear   停止目標或清除橫幅\n  \
             \u{20}\u{20}/goal rounds <+N|N>   動態調整輪次上限\n  \
             標誌：\n  \
             \u{20}\u{20}--max-rounds <N>  --until-stopped  --fresh-epoch  --rollback-on-stall  --max-tokens <N>  --verify  --no-verify\n  \
             目前預設設定（config.toml）：\n  \
             \u{20}\u{20}max_rounds: {max_rounds}, rollback_on_stall: {rollback_on_stall}, fresh_epoch: {fresh_epoch}\n"
        )
        .into(),
        Msg::GoalStatus { condition, round, mins, secs } =>
            format!("  ◎ 目標：{}\n  輪次：{}\n  已用時：{}分 {}秒\n", condition, round, mins, secs).into(),
        Msg::GoalStatusDetailed {
            condition,
            phase,
            round,
            max_rounds,
            tokens_used,
            max_tokens,
            mins,
            secs,
            flags,
        } => format!(
            "  ◎ 目標（{phase}）：{condition}\n  \
             輪次：{round} / {max_rounds} · Token：{tokens_used} / {max_tokens} · 已用時：{mins}分 {secs}秒\n  \
             標誌：{flags}\n"
        )
        .into(),
        Msg::GoalStartedSummary {
            max_rounds,
            stop_mode,
            rollback_on_stall,
            fresh_epoch,
        } => format!(
            "  ◎ 目標已啟動（max_rounds: {max_rounds}, stop: {stop_mode}, rollback_on_stall: {rollback_on_stall}, fresh_epoch: {fresh_epoch}）\n"
        )
        .into(),
        Msg::GoalNoActive =>
            "  目前沒有進行中的目標。\n  用法：/goal <條件>  |  /goal shift <條件>  |  /goal help\n".into(),
        Msg::GoalNoPaused => "  目前沒有可恢復的已暫停目標。\n".into(),
        Msg::GoalAlreadyRunning => "  目標已在執行中。\n".into(),
        Msg::GoalRoundsUsage =>
            "  用法：/goal rounds <+N|N>（例如 /goal rounds +10 或 /goal rounds 50）\n".into(),
        Msg::GoalRoundsUpdated { max_rounds } =>
            format!("  ◎ 已更新目標輪次上限：max_rounds = {max_rounds}\n").into(),
        Msg::GoalShiftUsage =>
            "  用法：/goal shift <條件>（啟用 --until-stopped --fresh-epoch --rollback-on-stall）\n".into(),
        Msg::GoalPromptRequired =>
            "  請在參數標誌後輸入目標描述（例如 /goal --max-rounds 20 修復測試）。\n".into(),
        Msg::GoalLabelOn => "開啟".into(),
        Msg::GoalLabelOff => "關閉".into(),
        Msg::GoalLabelAuto => "自動".into(),
        Msg::GoalLabelAutoContinuous => "自動（--until-stopped 時開啟）".into(),
        Msg::GoalLabelUnbounded => "無限制".into(),
        Msg::GoalLabelDefaultRoundsAuto => "300（自動）".into(),
        Msg::GoalLabelVerifyForced => "強制".into(),
        Msg::GoalStopModeUntilStopped => "直至手動停止".into(),
        Msg::GoalStopModeWhenMet => "達成即停".into(),
        Msg::GoalPhasePursuing => "執行中".into(),
        Msg::GoalPhasePaused => "已暫停".into(),
        Msg::GoalPhasePausedAtCap => "已達上限暫停".into(),
        Msg::GoalPhaseSatisfied => "已達成".into(),
        Msg::GoalPhaseEnded => "已結束".into(),
        Msg::GoalStatusFlags {
            stop_mode,
            rollback_on_stall,
            fresh_epoch,
            verify_mode,
            checkpoint,
        } => format!(
            "stop: {stop_mode}, rollback_on_stall: {rollback_on_stall}, fresh_epoch: {fresh_epoch}, verify: {verify_mode}, checkpoint: {checkpoint}"
        )
        .into(),
        Msg::GoalPausedBanner =>
            "  ⏸ 目標已暫停；輸入 /goal resume（或繼續對話）恢復，輸入 /goal stop 結束。\n".into(),
        Msg::GoalCleared => "  已清除目標。\n".into(),
        Msg::GoalFooterPausedBody => "goal 已暫停".into(),
        Msg::GoalFooterPausedMeta => " · 繼續對話即恢復 · /goal stop 結束".into(),
        Msg::GoalFooterPausedAtCapBody => "goal 暫停".into(),
        Msg::GoalFooterPausedAtCapMeta { round } => {
            format!(" · 已達 {round} 輪 · 繼續對話即推進").into()
        }
        Msg::GoalFooterSatisfiedBody => "goal 已達成".into(),
        Msg::GoalFooterSatisfiedMeta => " · /goal clear 結束".into(),
        Msg::RoundCapHeader => "輪次上限".into(),
        Msg::RoundCapQuestion { cap } => format!("已運行 {cap} 輪，繼續嗎？").into(),
        Msg::RoundCapQuestionWithStats { cap, stats } => {
            format!("已運行 {cap} 輪（{stats}），繼續嗎？").into()
        }
        Msg::RoundCapContinue => "繼續".into(),
        Msg::RoundCapContinueDesc { base } => format!("再跑 {base} 輪後重新確認").into(),
        Msg::RoundCapStop => "停止".into(),
        Msg::RoundCapStopDesc => "結束本回合".into(),
        Msg::CustomAnswerPlaceholder => "輸入自己的答案…".into(),
        Msg::MenuSearchSessions => "搜尋工作階段…".into(),
        Msg::MenuSearchDirectories => "搜尋歷史目錄或輸入路徑…".into(),
        Msg::MenuTypeToFilter => "輸入以篩選…".into(),
        Msg::RelayShareRuntimeFailed { error } => {
            format!("共用目前 runtime 失敗：{error}").into()
        }
        Msg::RelayStopShareFailed { error } => {
            format!("停止共用目前 runtime 失敗：{error}").into()
        }
        Msg::SlashSyncOutputFailed { error } => {
            format!("斜線命令輸出同步失敗：{error}").into()
        }
        Msg::RelayClientSkipDownload { dir } => format!(
            "自動下載已停用（JEIKCODE_RELAY_CLIENT_SKIP_DOWNLOAD=1），請手動將 relay-client 放入 {dir} 目錄"
        )
        .into(),
        Msg::RelayClientUnsupportedPlatform { os, arch, dir } => format!(
            "不支援的平台：{os}/{arch}。請手動編譯 relay-client 並放到 {dir} 中"
        )
        .into(),
        Msg::RelayClientVersionUnsupportedPlatform { version, target } => {
            format!("版本 {version} 不支援目前平台 {target}").into()
        }
        Msg::RelayClientAutoDownloadFailed { error, cache } => format!(
            "自動下載 relay-client 失敗：{error}\n\
             \n\
             安全下載：\n\
             1. 開啟瀏覽器造訪\n\
                https://github.com/JeikCode/JeikCode/jeikcode_jeikcode/jeikcode-relay-release/releases\n\
             2. 下載對應平台的 binary\n\
             3. 儲存到 {cache}/jeikcode-relay-client\n\
             4. chmod +x {cache}/jeikcode-relay-client\n\
             5. /app 重試\n\
             \n\
             快速安裝：\n\
             curl -fsSL https://raw.github.com/JeikCode/JeikCode/jeikcode_jeikcode/jeikcode-relay-release/raw/main/scripts/install.sh | sh\n\
             && /app 重試"
        )
        .into(),
        Msg::RelayClientCreateCacheDirFailed { error } => {
            format!("建立快取目錄失敗：{error}").into()
        }
        Msg::RelayClientCreateHttpFailed { error } => {
            format!("建立 HTTP 用戶端失敗：{error}").into()
        }
        Msg::RelayClientFetchManifestFailed { error } => {
            format!("取得版本清單失敗：{error}").into()
        }
        Msg::RelayClientManifestHttpError { status } => {
            format!("取得版本清單傳回 HTTP {status}").into()
        }
        Msg::RelayClientReadManifestFailed { error } => {
            format!("讀取版本清單失敗：{error}").into()
        }
        Msg::RelayClientParseManifestFailed { error } => {
            format!("解析版本清單失敗：{error}").into()
        }
        Msg::RelayClientDownloadRequestFailed { error } => {
            format!("下載請求失敗：{error}（請檢查網路連線）").into()
        }
        Msg::RelayClientDownloadUnauthorized => {
            "下載驗證失敗（HTTP 401），請檢查 Release 是否公開可存取".into()
        }
        Msg::RelayClientDownloadHttpError { status } => {
            format!("下載傳回 HTTP {status}（Release 可能不存在或無權存取）").into()
        }
        Msg::RelayClientCreateFileFailed { error } => format!("建立檔案失敗：{error}").into(),
        Msg::RelayClientReadStreamFailed { error } => {
            format!("讀取下載串流失敗：{error}").into()
        }
        Msg::RelayClientWriteFileFailed { error } => format!("寫入檔案失敗：{error}").into(),
        Msg::RelayClientFlushFileFailed { error } => format!("寫入磁碟失敗：{error}").into(),
        Msg::RelayClientSizeMismatch { expected, actual } => {
            format!("檔案大小不符：預期 {expected} 位元組，實際下載 {actual} 位元組").into()
        }
        Msg::RelayClientSha256Mismatch { expected, actual } => {
            format!("SHA256 校驗失敗：\n  預期: {expected}\n  實際: {actual}").into()
        }
        _ => super::zh_cn::zh_cn(msg),
    }
}
