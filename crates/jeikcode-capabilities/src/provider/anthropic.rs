//! Anthropic Messages API (`/v1/messages`) `LlmProvider` adapter (Claude Opus / Sonnet
//! / Haiku).
//!
//! Sibling of [`openai_compat`](super::openai_compat) — same seam, different wire. Design
//! notes (grounded in the kernel contract):
//!   - Anthropic's stream is EVENT-typed SSE (`message_start` / `content_block_*` /
//!     `message_delta` / `message_stop`), not OpenAI's choice-delta chunks, so this has
//!     its own [`AnthropicSseDecoder`]. Tool-call args stream as `input_json_delta`
//!     fragments BUFFERED per content-block index and emitted as one whole
//!     [`StreamEvent::ToolCall`] at `content_block_stop` (no partial-tool-call kernel
//!     variant) — plus a live [`StreamEvent::ToolCallDelta`] per fragment.
//!   - THINKING is signed: each `thinking` block carries an opaque `signature` that MUST
//!     be echoed back VERBATIM next turn. The decoder emits the thinking text as
//!     [`StreamEvent::Reasoning`] (live + flat `Message.reasoning`) and the signature as
//!     [`StreamEvent::ReasoningSignature`] at `content_block_stop`; the kernel stores one
//!     [`ReasoningBlock`](jeikcode_kernel::message::ReasoningBlock) per block, which this
//!     adapter replays as signed `thinking`/`redacted_thinking` content next request.
//!   - `system` is a TOP-LEVEL request field (not a message role); `max_tokens` is
//!     REQUIRED; tool `input_schema` (not `function.parameters`); `tool_use.input` is a
//!     JSON OBJECT (not a string). Auth is `x-api-key` + `anthropic-version`, not Bearer.
//!   - PREFIX BYTE-STABILITY for prompt caching: the body is built from a BTreeMap-backed
//!     `Map` (sorted on serialize) with no timestamps/uuids, so the same
//!     `(system, messages, tools)` always serialize identically.

use super::openai_compat::{build_http_client, SwappableClient};
use super::reasoning::{ReasoningPolicy, REASONING_PLACEHOLDER};
use super::retry::{self, RetryPolicy};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use jeikcode_kernel::message::{Message, Role};
use jeikcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort, ToolChoice};
use jeikcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use jeikcode_kernel::tool::{ToolCall, ToolDef};
use serde_json::{json, Map, Value};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Config + provider
// ---------------------------------------------------------------------------

/// Construction-time config for the Anthropic Messages API adapter. The kernel never
/// sees any of this — it enters the adapter here, off the `LlmProvider` contract.
#[derive(Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    /// API prefix. A host root (`https://api.anthropic.com`) gets `/v1/messages`.
    /// A custom version suffix (`/v1`, `/v2`, `/v3`, `/v1beta`, …) gets `/messages`
    /// on that version. A URL that already ends with `/messages` is used as-is.
    pub base_url: String,
    pub model: String,
    pub context_window: u32,
    /// REQUIRED output cap for the Messages API. Used when `ChatOptions::max_tokens` is
    /// `None` (Anthropic rejects a request with no `max_tokens`).
    pub max_tokens: u32,
    /// `anthropic-version` header value.
    pub anthropic_version: String,
    /// Enable extended thinking. Off by default. When enabled, emits standard Claude 3.7+
    /// `thinking: {type:"enabled", budget_tokens:...}` or adaptive mode if configured.
    pub thinking: bool,
    /// Explicit thinking enabled/disabled flag from model config.
    pub thinking_enabled: Option<bool>,
    /// Maximum tokens allocated to the thinking phase (`thinking.budget_tokens`).
    /// Defaults to 10000 (or the effort table) when thinking is enabled. The
    /// resolved value is a downstream constraint: it shrinks to leave
    /// [`MIN_ANSWER_TOKENS`](super::thinking::MIN_ANSWER_TOKENS) and never
    /// expands `max_tokens`.
    pub thinking_budget: Option<u32>,
    /// Thinking mode: `"enabled"` (default) or `"adaptive"`.
    pub thinking_type: Option<String>,
    /// Explicit reasoning model flag; `Some(true)` ⇒ `ReasoningPolicy::Include`, `Some(false)` ⇒ `ReasoningPolicy::Exclude`.
    pub reasoning_model: Option<bool>,
    /// Explicit reasoning round-trip policy; `None` ⇒ derived from the model name and base URL.
    pub reasoning_policy: Option<ReasoningPolicy>,
    /// Forward sampling params (`temperature` / future `top_p` / `top_k`) on the wire.
    /// **Off by default** because the default model is a modern Claude (Opus 4.7+),
    /// which REMOVED these — sending `temperature` 400s — and extended thinking is
    /// likewise incompatible with a custom temperature. Set `true` ONLY for an older
    /// Claude (e.g. Sonnet 3.x / Haiku 3.x) that still accepts them; do not combine
    /// with `thinking`.
    pub send_sampling_params: bool,
    /// Per-chunk stream-idle watchdog: no bytes for this long ⇒ terminal error.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Per-ATTEMPT first-byte (TTFB) watchdog for the OPEN call — see the matching
    /// field on `OpenAiCompatConfig`. A gateway that accepts the connection but never
    /// responds would otherwise hang forever. A timeout is classified retryable.
    pub open_timeout: Duration,
    /// Retry policy for the OPEN call only (mid-stream errors are never retried).
    pub retry: RetryPolicy,
    /// User-Agent sent on every request. `None` ⇒ [`super::DEFAULT_USER_AGENT`]; the
    /// driver injects `jeikcode/<version>` for gateway attribution. See the
    /// `OpenAiCompatConfig::user_agent` doc for why a local const won't do.
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (self-signed / internal gateways).
    /// Mirrors core's `ProviderConfig::skip_tls_verify`. Default false.
    pub skip_tls_verify: bool,
}

impl AnthropicConfig {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            context_window: 200_000,
            max_tokens: 4096,
            anthropic_version: "2023-06-01".to_string(),
            thinking: false,
            thinking_enabled: None,
            thinking_budget: None,
            thinking_type: None,
            reasoning_model: None,
            reasoning_policy: None,
            send_sampling_params: false,
            idle_timeout: jeikcode_kernel::timeouts::TimeoutPolicy::production().idle_watchdog(),
            open_timeout: jeikcode_kernel::timeouts::TimeoutPolicy::production().open,
            connect_timeout: jeikcode_kernel::timeouts::TimeoutPolicy::production().connect,
            retry: RetryPolicy::default(),
            user_agent: None,
            skip_tls_verify: false,
        }
    }
}

pub struct AnthropicProvider {
    cfg: AnthropicConfig,
    policy: ReasoningPolicy,
    client: std::sync::Arc<SwappableClient>,
    url: String,
    /// Stable per-conversation id bound ONCE by the kernel; see the field on
    /// `OpenAiCompatProvider`. `OnceLock` — constant for the provider's life.
    /// Forwarded as `x-jeikcode-session-id` and `x-session-id`. Unset ⇒ omitted.
    session_id: std::sync::OnceLock<String>,
}

impl AnthropicProvider {
    pub fn resolve_policy(cfg: &AnthropicConfig) -> ReasoningPolicy {
        cfg.reasoning_policy.unwrap_or_else(|| {
            if cfg.reasoning_model == Some(false) {
                ReasoningPolicy::Exclude
            } else if cfg.thinking {
                ReasoningPolicy::Include
            } else {
                ReasoningPolicy::derive(&cfg.model, &cfg.base_url)
            }
        })
    }

    pub fn new(cfg: AnthropicConfig) -> Result<Self, ProviderError> {
        let policy = Self::resolve_policy(&cfg);
        let connect_timeout = cfg.connect_timeout;
        let skip_tls_verify = cfg.skip_tls_verify;
        let user_agent = cfg.user_agent.clone();
        let url = messages_endpoint_url(&cfg.base_url);
        let initial_tls12 = jeikcode_config::tls::should_cap_url(&url);
        let client = std::sync::Arc::new(SwappableClient::new(initial_tls12, move |tls12| {
            build_http_client(connect_timeout, skip_tls_verify, user_agent.clone(), tls12)
        })?);
        Ok(Self {
            cfg,
            policy,
            client,
            url,
            session_id: std::sync::OnceLock::new(),
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn model_name(&self) -> &str {
        &self.cfg.model
    }

    fn context_window(&self) -> u32 {
        self.cfg.context_window
    }

    fn bind_session_id(&self, session_id: &str) {
        let _ = self.session_id.set(session_id.to_string());
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let body = build_request_body(
            &self.cfg.model,
            messages,
            tools,
            options,
            &self.cfg,
            self.policy,
        );
        super::wire_dump_request(&self.cfg.model, &body); // byte-level dump (JEIKCODE_WIRE_DUMP=1)

        // Open the stream. A hard failure here returns `Err` so the kernel's
        // agent-layer open retry still applies.
        let policy = self.cfg.retry.clone();
        let client = self.client.clone();
        let url = self.url.clone();
        let api_key = self.cfg.api_key.clone();
        let anthropic_version = self.cfg.anthropic_version.clone();
        // Snapshot the session id once; reused across the open and any mid-stream reopen.
        let session_id = self.session_id.get().cloned().unwrap_or_default();
        let idle = self.cfg.idle_timeout;
        let open_timeout = self.cfg.open_timeout;
        let rate_limit_retry_owner = options.rate_limit_retry_owner;
        let mut reopen_budget = retry::StreamReopenBudget::new();
        let resp = open_stream(
            &client,
            &url,
            &body,
            &api_key,
            &anthropic_version,
            &session_id,
            &policy,
            rate_limit_retry_owner,
            open_timeout,
        )
        .await?;

        let s = async_stream::stream! {
            // A body that dies before replay-sensitive output reaches the consumer is
            // safe to redo wholesale. Metadata may repeat; content and tool data may not.
            // Fast transient drops reopen up to 6 attempts; slow attempts (>=10s) surface
            // to the kernel after attempt 1 so the user sees a warning instead of waiting
            // silently for minutes.
            let mut resp = resp;
            'reopen: loop {
                let mut dec = AnthropicSseDecoder::new();
                let mut emitted_replay_sensitive = false;
                let mut saw_usable_content = false;
                let mut pending_metadata = Vec::new();
                let mut pending_signatures = Vec::new();
                let byte_stream = resp.bytes_stream();
                futures::pin_mut!(byte_stream);
                loop {
                    match tokio::time::timeout(idle, byte_stream.next()).await {
                        Err(_elapsed) => {
                            yield StreamEvent::Error(ProviderError {
                                retryable: false,
                                message: "stream idle timeout".to_string(),
                                ..Default::default()
                            });
                            return;
                        }
                        Ok(None) => {
                            let ended_without_stop = !dec.saw_message_stop();
                            let trailing = dec.finish();
                            if ended_without_stop && !saw_usable_content {
                                // Upstream closed the HTTP 200 SSE stream before emitting
                                // `message_stop` and without any text, reasoning text, or
                                // tool call (e.g. a `thinking_signature`-only premature drop).
                                // Surface an explicit retryable error instead of fabricating `Done`
                                // or leaking an orphan signature into kernel `saw_stream_content`.
                                let saw_signature_only = !pending_signatures.is_empty();
                                pending_signatures.clear();
                                for metadata in pending_metadata.drain(..) {
                                    if !matches!(metadata, StreamEvent::Usage(_)) {
                                        yield metadata;
                                    }
                                }
                                yield StreamEvent::Error(ProviderError {
                                    retryable: !emitted_replay_sensitive,
                                    message: if saw_signature_only {
                                        "upstream stream ended prematurely before emitting content (thinking signature only, missing message_stop)".to_string()
                                    } else {
                                        "upstream stream ended prematurely before emitting content (missing message_stop)".to_string()
                                    },
                                    ..Default::default()
                                });
                                return;
                            }
                            for ev in trailing {
                                let ready = flush_or_buffer_stream_event(
                                    ev,
                                    &mut emitted_replay_sensitive,
                                    &mut saw_usable_content,
                                    &mut pending_metadata,
                                    &mut pending_signatures,
                                );
                                for out_ev in ready {
                                    yield out_ev;
                                }
                            }
                            return;
                        }
                        Ok(Some(Err(e))) => {
                            if !emitted_replay_sensitive && reopen_budget.can_reopen_fast(&e) {
                                let delay = retry::compute_backoff(reopen_budget.attempt(), &policy);
                                reopen_budget.begin_reopen();
                                // Brief, esc-interruptible backoff before reopening so an
                                // immediate retry does not slam a gateway resetting under load.
                                tokio::time::sleep(delay).await;
                                if retry::is_stale_connection_error(&e)
                                    || retry::chain_has_tls_corruption(&e)
                                {
                                    if let Err(rebuild_error) = client.rebuild(
                                        jeikcode_config::tls::should_cap_url(&url),
                                    ) {
                                        yield StreamEvent::Error(rebuild_error);
                                        return;
                                    }
                                }
                                match open_stream(
                                    &client,
                                    &url,
                                    &body,
                                    &api_key,
                                    &anthropic_version,
                                    &session_id,
                                    &policy,
                                    rate_limit_retry_owner,
                                    open_timeout,
                                )
                                .await
                                {
                                    Ok(fresh) => {
                                        reopen_budget.commit_reopen();
                                        resp = fresh;
                                        continue 'reopen;
                                    }
                                    Err(open_err) => {
                                        yield StreamEvent::Error(open_err);
                                        return;
                                    }
                                }
                            }
                            yield StreamEvent::Error(ProviderError {
                                retryable: !emitted_replay_sensitive
                                    && reopen_budget.should_retry_in_kernel(&e),
                                message: retry::stream_read_error_message(
                                    &e,
                                    if emitted_replay_sensitive {
                                        retry::StreamReadRecovery::PartialResponse
                                    } else {
                                        retry::StreamReadRecovery::RetryExhausted {
                                            attempts: reopen_budget.reconnect_attempts(),
                                        }
                                    },
                                ),
                                ..Default::default()
                            });
                            return;
                        }
                        Ok(Some(Ok(chunk))) => {
                            let mut saw_done = false;
                            for ev in dec.feed(chunk.as_ref()) {
                                let ready = flush_or_buffer_stream_event(
                                    ev,
                                    &mut emitted_replay_sensitive,
                                    &mut saw_usable_content,
                                    &mut pending_metadata,
                                    &mut pending_signatures,
                                );
                                for out_ev in ready {
                                    if matches!(out_ev, StreamEvent::Done { .. }) {
                                        saw_done = true;
                                    }
                                    yield out_ev;
                                }
                            }
                            if saw_done { return; }
                        }
                    }
                }
            }
        };

        Ok(s.boxed())
    }
}

fn flush_or_buffer_stream_event(
    ev: StreamEvent,
    emitted_replay_sensitive: &mut bool,
    saw_usable_content: &mut bool,
    pending_metadata: &mut Vec<StreamEvent>,
    pending_signatures: &mut Vec<StreamEvent>,
) -> Vec<StreamEvent> {
    if !*emitted_replay_sensitive && retry::is_attempt_metadata_event(&ev) {
        pending_metadata.push(ev);
        return Vec::new();
    }
    if !*saw_usable_content && matches!(ev, StreamEvent::ReasoningSignature { .. }) {
        pending_signatures.push(ev);
        return Vec::new();
    }
    let mut out = Vec::new();
    if is_usable_content_event(&ev) || matches!(ev, StreamEvent::Done { .. }) {
        out.append(pending_metadata);
        for sig in pending_signatures.drain(..) {
            *emitted_replay_sensitive |= retry::is_replay_sensitive_event(&sig);
            out.push(sig);
        }
    } else if matches!(ev, StreamEvent::Error(_)) {
        out.append(pending_metadata);
        pending_signatures.clear();
    } else if retry::is_replay_sensitive_event(&ev) {
        out.append(pending_metadata);
    }
    *emitted_replay_sensitive |= retry::is_replay_sensitive_event(&ev);
    *saw_usable_content |= is_usable_content_event(&ev);
    out.push(ev);
    out
}

/// Open one `/v1/messages` stream, retrying the OPEN (transient status /
/// transport) per `policy`. Shared by the initial open and the mid-stream
/// re-open so both paths behave identically.
async fn open_stream(
    client: &SwappableClient,
    url: &str,
    body: &Value,
    api_key: &str,
    anthropic_version: &str,
    session_id: &str,
    policy: &RetryPolicy,
    rate_limit_retry_owner: jeikcode_kernel::provider::RateLimitRetryOwner,
    open_timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 1u32;
    let mut tls12_probe = false;
    loop {
        let http = client.get();
        let mut req = http
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", anthropic_version)
            .json(body);
        // Stable session id → gateway prefix-cache affinity. Empty ⇒ omitted.
        req = super::apply_session_affinity(req, session_id);
        let was_capped = tls12_probe || jeikcode_config::tls::should_cap_url(url);
        // TTFB watchdog for THIS attempt (mirrors openai_compat): bounds only the
        // silent-gateway wait, never a slow streaming body.
        let send = match retry::send_with_ttfb(req, open_timeout).await {
            retry::OpenSend::Timeout => {
                if attempt < policy.max_attempts {
                    tokio::time::sleep(retry::compute_backoff(attempt, policy)).await;
                    attempt += 1;
                    continue;
                }
                return Err(retry::ttfb_timeout_error(open_timeout));
            }
            retry::OpenSend::Transport(e) => Err(e),
            retry::OpenSend::Response(resp) => Ok(resp),
        };
        match send {
            Ok(resp) => {
                if tls12_probe {
                    jeikcode_config::tls::latch_managed_tls12();
                    tls12_probe = false;
                }
                let code = resp.status().as_u16();
                if !resp.status().is_success() {
                    if retry::should_retry_open_status(code, rate_limit_retry_owner)
                        && attempt < policy.max_attempts
                    {
                        let wait = retry::parse_retry_after(resp.headers())
                            .unwrap_or_else(|| retry::compute_backoff(attempt, policy));
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    // Capture the real `Retry-After` BEFORE `text()` consumes `resp` — the
                    // authoritative rate-limit countdown for the self-heal (vs scraping text).
                    let retry_after_secs =
                        retry::parse_retry_after(resp.headers()).map(|d| d.as_secs());
                    let text = resp.text().await.unwrap_or_default();
                    let envelope = serde_json::from_str::<serde_json::Value>(&text).ok();
                    let err_obj = envelope.as_ref().and_then(|v| v.get("error"));
                    let detail = err_obj
                        .map(parse_error_obj)
                        .unwrap_or_else(|| truncate_msg(&text));
                    let provider_code = err_obj.and_then(error_type);
                    return Err(ProviderError {
                        retryable: retry::is_retryable_status(code),
                        message: super::friendly_http_error(code, &detail),
                        http_status: Some(code),
                        code: provider_code,
                        retry_after_secs,
                    });
                }
                return Ok(resp);
            }
            Err(e) => {
                if retry::is_retryable_reqwest_error(&e) && attempt < policy.max_attempts {
                    let tls_corruption = retry::chain_has_tls_corruption(&e);
                    let try_tls12 = jeikcode_config::tls::should_try_fallback(
                        url,
                        was_capped,
                        e.is_connect() || tls_corruption,
                    );
                    let wait = retry::compute_backoff(attempt, policy);
                    tokio::time::sleep(wait).await;
                    if try_tls12 || retry::is_stale_connection_error(&e) || tls_corruption {
                        let capped = try_tls12 || was_capped;
                        client.rebuild(capped)?;
                        tls12_probe = capped;
                    }
                    attempt += 1;
                    continue;
                }
                return Err(open_error(e));
            }
        }
    }
}

/// Resolve the Anthropic Messages endpoint from a configured `base_url`.
///
/// - no version segment → `{base}/v1/messages`
/// - custom `/vN…` suffix (`/v1`, `/v2`, `/v1beta`, …) → `{base}/messages`
/// - already ends with `/messages` → used as-is
fn messages_endpoint_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/messages") {
        return base.to_string();
    }
    if ends_with_api_version(base) {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// Last path segment looks like an API version: `v1`, `v2`, `v3`, `v1beta`.
fn ends_with_api_version(base: &str) -> bool {
    let Some(segment) = base.rsplit('/').next() else {
        return false;
    };
    let Some(rest) = segment.strip_prefix('v') else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Request building (pure, deterministic)
// ---------------------------------------------------------------------------

pub(crate) fn is_official_anthropic(base_url: &str) -> bool {
    let u = base_url.to_ascii_lowercase();
    u.contains("api.anthropic.com") || u.contains("api.anthropic.test")
}

/// Build the full Messages API request body. Deterministic: keys come from a
/// BTreeMap-backed `Map` (sorted on serialize), values are ordered literals.
fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    options: &ChatOptions,
    cfg: &AnthropicConfig,
    policy: ReasoningPolicy,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    // `max_tokens` is REQUIRED. Per-call override wins; else the cfg default.
    // This is the kernel-resolved per-request output budget; thinking may shrink
    // to fit inside it, but must never inflate it.
    let max_tokens = options.max_tokens.unwrap_or(cfg.max_tokens);
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert("stream".into(), json!(true));

    let is_official = is_official_anthropic(&cfg.base_url);
    let echo_thinking = policy == ReasoningPolicy::Include;
    let leading_synthetic_user_blocks = count_leading_synthetic_user_blocks(messages);
    let (system_blocks, mut msgs) = format_messages(messages, echo_thinking, is_official);
    if !system_blocks.is_empty() {
        // Prompt-cache breakpoints: Anthropic allows at most 4 breakpoints per request:
        // [1] tools.last(), [2] system.last() (covering all system blocks including
        // <explore_strategy>), [3] last frozen synthetic-user block in messages[0]
        // (e.g. <project_instructions>), and [4] messages.last().
        let sys_len = system_blocks.len();
        let mut sys_values: Vec<Value> = Vec::with_capacity(sys_len);
        for (idx, text) in system_blocks.into_iter().enumerate() {
            let mut block = json!({
                "type": "text",
                "text": text,
            });
            if idx + 1 == sys_len {
                block["cache_control"] = json!({ "type": "ephemeral" });
            }
            sys_values.push(block);
        }
        body.insert("system".into(), json!(sys_values));
    }
    apply_synthetic_user_cache_breakpoint(&mut msgs, leading_synthetic_user_blocks);
    apply_message_cache_breakpoint(&mut msgs);
    body.insert("messages".into(), json!(msgs));

    // Sampling params are OMITTED unless the embedder opts in for an older Claude.
    // Opus 4.7+ reject `temperature` (400), and it is incompatible with thinking.
    if cfg.send_sampling_params {
        if let Some(t) = options.temperature {
            body.insert("temperature".into(), json!(t));
        }
    }
    let forces_tool_use = matches!(
        &options.tool_choice,
        ToolChoice::Required | ToolChoice::Specific(_)
    );
    match &options.tool_choice {
        ToolChoice::Auto => {} // omit → byte-identical to "no opinion"
        ToolChoice::Required => {
            body.insert("tool_choice".into(), json!({ "type": "any" }));
        }
        ToolChoice::Specific(name) => {
            body.insert(
                "tool_choice".into(),
                json!({ "type": "tool", "name": name }),
            );
        }
        ToolChoice::None => {
            body.insert("tool_choice".into(), json!({ "type": "none" }));
        }
    }
    // Anthropic rejects extended/adaptive thinking combined with forced tool use.
    // Suppress thinking for this one request; the session config remains unchanged,
    // so the following round resumes thinking automatically.
    let is_thinking_disabled = super::thinking::is_effort_off(options.reasoning_effort.as_ref())
        || cfg.thinking_type.as_deref() == Some("disabled")
        || cfg.thinking_enabled == Some(false);

    let thinking_enabled = (cfg.thinking || policy == ReasoningPolicy::Include) && !forces_tool_use;

    // Claude 4.7+ requires adaptive thinking; older models keep the manual
    // `enabled + budget_tokens` shape. The model generation wins over a stale
    // `thinking_type = "enabled"` so a 4.7+ request never 400s.
    let adaptive = matches!(
        super::thinking::anthropic_family(model),
        super::thinking::ThinkingFamily::AnthropicAdaptive
    ) || cfg.thinking_type.as_deref() == Some("adaptive");

    if is_thinking_disabled {
        // Official shape: the disabled variant carries no `budget_tokens`.
        body.insert("thinking".into(), json!({ "type": "disabled" }));
    } else if thinking_enabled {
        if adaptive {
            body.insert("thinking".into(), json!({ "type": "adaptive" }));
        } else if let Some(plan) = super::thinking::resolve_budget(
            super::thinking::ThinkingFamily::AnthropicLegacy,
            cfg.thinking_budget,
            options.reasoning_effort.as_ref(),
            max_tokens,
        ) {
            // The budget is a DOWNSTREAM constraint: it shrinks to leave
            // `MIN_ANSWER_TOKENS`, but `max_tokens` is never expanded.
            body.insert(
                "thinking".into(),
                json!({
                    "type": "enabled",
                    "budget_tokens": plan.tokens,
                }),
            );
        }
    }
    // Adaptive models steer depth through `output_config.effort`; legacy models
    // use the budget itself. Keep the effort sideband even when forced tool use
    // suppresses the `thinking` block for one request.
    if !is_thinking_disabled && adaptive {
        if let Some(effort) = super::openai_compat::resolve_wire_effort(model, options) {
            if !effort.eq_ignore_ascii_case("off") && !effort.eq_ignore_ascii_case("none") {
                body.insert("output_config".into(), json!({ "effort": effort }));
            }
        }
    }
    if !tools.is_empty() {
        let mut t: Vec<Value> = tools
            .iter()
            .map(|td| {
                json!({
                    "name": td.name,
                    "description": td.description,
                    "input_schema": super::tool_schema::project(
                        super::sanitize_schema_for_wire(&td.parameters),
                        super::tool_schema::family_for(&cfg.model, None),
                    ),
                })
            })
            .collect();
        // Prompt-cache breakpoint #2: frozen tool schemas (last tool). Anthropic
        // allows 4 breakpoints; system + tools + last history message is the
        // Claude-Code layout that keeps prefix hits across tool-loop rounds.
        if let Some(last) = t.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        body.insert("tools".into(), json!(t));
    }
    Value::Object(body)
}

/// Count leading frozen synthetic `Role::User` wire blocks (e.g. `<available_skills>`,
/// `<mcp_server_instructions>`, `<memory>`, `<user_preferences>`, `<project_instructions>`)
/// immediately after the leading prefix so `messages[0]` can anchor a dedicated cache
/// breakpoint at the end of the static repo/session prefix.
///
/// Mirrors `format_messages`: `Role::System` entries are lifted to top-level `system[]`,
/// `autonomous_turn` and compaction-anchor entries terminate the frozen prefix, and each
/// frozen message contributes the exact count of blocks produced by `format_user_message`
/// + `content_to_blocks` (post-`unwrap_strong_reminders`).
fn count_leading_synthetic_user_blocks(messages: &[Message]) -> usize {
    let mut blocks = 0usize;
    for m in messages {
        if m.role == Role::System {
            continue;
        }
        if m.is_frozen_prefix_user() {
            let formatted = format_user_message(m);
            let content = formatted.get("content").cloned().unwrap_or(Value::Null);
            blocks += content_to_blocks(content).len();
            continue;
        }
        break;
    }
    blocks
}

/// Mark the last frozen synthetic-user content block in `messages[0]` as a cache
/// breakpoint so multi-turn conversations and >20-block tool loops never lose Anthropic's
/// 20-block lookback reach into the 10–30 KB static instructions prefix.
fn apply_synthetic_user_cache_breakpoint(
    messages: &mut [Value],
    leading_synthetic_user_blocks: usize,
) {
    if leading_synthetic_user_blocks == 0 {
        return;
    }
    let Some(first) = messages.first_mut() else {
        return;
    };
    match first.get_mut("content") {
        Some(Value::String(text)) if leading_synthetic_user_blocks == 1 && !text.is_empty() => {
            let text = text.clone();
            first["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral" },
            }]);
        }
        Some(Value::Array(blocks)) => {
            let target_idx = leading_synthetic_user_blocks - 1;
            if let Some(block) = blocks.get_mut(target_idx) {
                if block.is_object() {
                    block["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
        }
        _ => {}
    }
}

/// Mark the last user/assistant content block as a cache breakpoint so the next
/// request can reuse everything up to and including this message.
///
/// Anthropic rejects empty `"text": ""` blocks with HTTP 400; skip any trailing
/// empty message so the breakpoint lands on the newest non-empty message instead.
fn apply_message_cache_breakpoint(messages: &mut [Value]) {
    for msg in messages.iter_mut().rev() {
        match msg.get_mut("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                let text = text.clone();
                msg["content"] = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" },
                }]);
                return;
            }
            Some(Value::Array(blocks)) => {
                if let Some(block) = blocks.iter_mut().rev().find(|b| {
                    b.is_object()
                        && (b.get("type").and_then(|t| t.as_str()) != Some("text")
                            || b.get("text")
                                .and_then(|t| t.as_str())
                                .is_some_and(|s| !s.is_empty()))
                }) {
                    block["cache_control"] = json!({ "type": "ephemeral" });
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Split kernel messages into the Anthropic top-level `system` text blocks (all non-empty
/// System messages) and the wire `messages[]` (User/Assistant/Tool mapped to content
/// blocks; consecutive Tool results folded into one `user` message).
fn format_messages(
    messages: &[Message],
    echo_thinking: bool,
    is_official: bool,
) -> (Vec<String>, Vec<Value>) {
    // Leading System messages lift to the top-level `system` block array. Anthropic has no
    // system message ROLE on the wire.
    let system_blocks: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.trim())
        .filter(|t| !t.is_empty())
        .map(|s| s.to_string())
        .collect();

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i];
        match m.role {
            Role::System => {} // lifted above
            Role::User => {
                out.push(format_user_message(m));
                i += 1;
                continue;
            }
            Role::Assistant => {
                // An empty model round (failed / interrupted stream that persisted nothing
                // replayable) is dropped instead of becoming `content:""` — Anthropic rejects
                // empty content, and dropping it keeps the surrounding user turns adjacent so
                // `merge_consecutive_user` reconstructs one user block (the lost reply never
                // happened).
                if let Some(msg) = format_assistant_message(m, echo_thinking, is_official) {
                    out.push(msg);
                }
                i += 1;
                continue;
            }
            Role::Tool => {
                // Fold the CONSECUTIVE run of tool results into ONE user message — the
                // typical shape after an assistant fired N parallel tool calls.
                let mut blocks: Vec<Value> = Vec::new();
                while i < messages.len() && messages[i].role == Role::Tool {
                    let tr = &messages[i];
                    if let Some(id) = tr.tool_call_id.as_deref().filter(|s| !s.is_empty()) {
                        let model_content = super::strip_tool_result_diff_for_model(&tr.text);
                        blocks.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": model_content,
                            "is_error": tr.is_error,
                        }));
                    }
                    i += 1;
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
                continue;
            }
        }
        i += 1;
    }
    // Anthropic requires STRICTLY ALTERNATING user/assistant roles. Several kernel shapes
    // produce adjacent user messages on the wire: a tool-result run folds into a user
    // message that an injected `<system-reminder>` tail then follows; a post-compaction
    // history places a synthetic-summary user beside the real user; multiple tail hooks
    // stack. Merge every consecutive `role:"user"` run into one (others — OpenAI/Ollama —
    // tolerate adjacency, so they don't need this).
    let out = merge_consecutive_user(out);
    (system_blocks, out)
}

/// Merge consecutive `role:"user"` wire entries into ONE. Text-only neighbors join into a
/// STRING (prefix-cache parity with the no-block path); any block/array content promotes the
/// merged entry to a single blocks array.
fn merge_consecutive_user(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        let is_user = m.get("role").and_then(Value::as_str) == Some("user");
        let prev_user = out
            .last()
            .and_then(|p| p.get("role").and_then(Value::as_str))
            == Some("user");
        if is_user && prev_user {
            let last = out.last_mut().unwrap();
            let merged = merge_user_content(
                last.get("content").cloned().unwrap_or(Value::Null),
                m.get("content").cloned().unwrap_or(Value::Null),
            );
            last["content"] = merged;
        } else {
            out.push(m);
        }
    }
    out
}

/// Combine two user `content` values into ONE array of blocks.
/// Each user message preserves its discrete block boundary (e.g. project instructions
/// as the first block and user input as the second block), adhering to cache stability
/// and block-level compaction protection.
fn merge_user_content(a: Value, b: Value) -> Value {
    let mut blocks = content_to_blocks(a);
    blocks.extend(content_to_blocks(b));
    Value::Array(blocks)
}

fn content_to_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::Array(arr) => arr,
        Value::String(s) if !s.is_empty() => vec![json!({ "type": "text", "text": s })],
        _ => vec![],
    }
}

/// A `user` message. Text-only → `content` is a STRING (prefix-cache parity with the
/// no-block path); with images → an array of text + base64 `image` blocks.
fn format_user_message(m: &Message) -> Value {
    let text = crate::reminder::unwrap_strong_reminders(&m.text);
    if m.images.is_empty() {
        return json!({ "role": "user", "content": text });
    }
    let mut parts: Vec<Value> = Vec::with_capacity(m.images.len() + 1);
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for img in &m.images {
        if img.data.is_empty() {
            continue;
        }
        let media_type = match img.media_type.as_str() {
            "image/jpeg" | "image/png" | "image/gif" | "image/webp" => img.media_type.as_str(),
            "image/jpg" => "image/jpeg",
            _ => {
                if img.media_type.is_empty() || img.media_type.starts_with("image/") {
                    "image/png"
                } else {
                    "application/octet-stream"
                }
            }
        };
        parts.push(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": img.data },
        }));
    }
    if parts.is_empty() {
        json!({ "role": "user", "content": text })
    } else {
        json!({ "role": "user", "content": parts })
    }
}

/// An `assistant` message. Plain text (no tool calls, no echoed thinking) → `content`
/// STRING. Otherwise an ARRAY in Anthropic's required order: signed thinking blocks
/// (only when `echo_thinking`), then the text block, then `tool_use` blocks.
/// `None` when the message carries nothing for the wire (a lost reply).
fn format_assistant_message(m: &Message, echo_thinking: bool, is_official: bool) -> Option<Value> {
    // Empty model round (failed / interrupted stream that persisted nothing
    // replayable). Drop it BEFORE thinking synthesis: on a third-party gateway
    // `echo_thinking` would otherwise turn the stored reasoning into a
    // thinking-only assistant turn, which strict gateways reject ("content or
    // tool_calls must be set"). A tool-loop round always carries tool_calls, so
    // dropping a text-less/tool-less message never breaks signature replay.
    if m.text.is_empty() && m.tool_calls.is_empty() {
        return None;
    }
    let mut parts: Vec<Value> = Vec::new();
    let mut has_thinking_blocks = false;

    if echo_thinking {
        if is_official {
            // Echo a signed thinking block back ONLY if THIS provider produced it. An opaque
            // token is PROVIDER-BOUND — replaying another vendor's `signature`/`data` to
            // Anthropic fails hard (400) — so we filter on `provider`, honoring the
            // [`ReasoningBlock`](jeikcode_kernel::message::ReasoningBlock) INVARIANT. A `None`
            // provider is treated as foreign (never echoed).
            for b in m.reasoning_blocks.iter().filter(|b| {
                matches!(
                    b.provider.as_deref(),
                    Some("anthropic") | Some("anthropic_redacted")
                )
            }) {
                has_thinking_blocks = true;
                let opaque = b.opaque.as_deref().unwrap_or_default();
                if b.text.is_empty() || b.provider.as_deref() == Some("anthropic_redacted") {
                    parts.push(json!({ "type": "redacted_thinking", "data": opaque }));
                } else {
                    parts.push(
                        json!({ "type": "thinking", "thinking": b.text, "signature": opaque }),
                    );
                }
            }
        } else {
            // Third-party gateway (Gemini, DeepSeek, New API, One API, etc.):
            // Gateways do not enforce Anthropic's private key signature HMAC, and
            // Gemini gateways expect signature-only blocks on `thinking` (with
            // `signature`), reserving `redacted_thinking` for genuine redacted blocks.
            for b in &m.reasoning_blocks {
                has_thinking_blocks = true;
                let opaque = b.opaque.as_deref().unwrap_or_default();
                if b.provider.as_deref() == Some("anthropic_redacted")
                    || (b.text.is_empty() && b.provider.is_none() && !opaque.is_empty())
                {
                    parts.push(json!({ "type": "redacted_thinking", "data": opaque }));
                } else {
                    parts.push(
                        json!({ "type": "thinking", "thinking": b.text, "signature": opaque }),
                    );
                }
            }
            // If no reasoning_blocks were present, synthesize a thinking block from m.reasoning or placeholder
            if !has_thinking_blocks {
                if let Some(text) = m.reasoning.as_deref().filter(|s| !s.is_empty()) {
                    parts.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": "",
                    }));
                    has_thinking_blocks = true;
                } else if !m.tool_calls.is_empty() {
                    parts.push(json!({
                        "type": "thinking",
                        "thinking": REASONING_PLACEHOLDER,
                        "signature": "",
                    }));
                    has_thinking_blocks = true;
                }
            }
        }
    }

    if m.tool_calls.is_empty() && !has_thinking_blocks {
        // Pure-text assistant turn without thinking — keep it a STRING.
        if m.text.is_empty() {
            return None;
        }
        return Some(json!({ "role": "assistant", "content": m.text }));
    }

    if !m.text.is_empty() {
        parts.push(json!({ "type": "text", "text": m.text }));
    }
    for tc in &m.tool_calls {
        // `tool_use.input` is a JSON OBJECT. The kernel stores raw-string args; parse
        // them (deterministic re-serialize via serde_json Map), falling back to `{}`.
        let input: Value = serde_json::from_str(tc.arguments.trim())
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        parts.push(json!({ "type": "tool_use", "id": tc.id, "name": tc.name, "input": input }));
    }
    if parts.is_empty() {
        return None;
    }
    Some(json!({ "role": "assistant", "content": parts }))
}

fn effort_str(e: &ReasoningEffort) -> &str {
    e.as_str()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn open_error(e: reqwest::Error) -> ProviderError {
    ProviderError {
        retryable: retry::is_retryable_reqwest_error(&e),
        message: format!("open failed: {}", retry::err_chain(&e)),
        ..Default::default()
    }
}

fn truncate_msg(s: &str) -> String {
    const CAP: usize = 2048;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut end = CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Format an Anthropic error OBJECT (`{"type","message"}`) as a readable
/// "[type] message" one-liner.
fn parse_error_obj(err: &serde_json::Value) -> String {
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim();
    let typ = err
        .get("type")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty());
    let tag = typ.map(|t| format!("[{t}] ")).unwrap_or_default();
    truncate_msg(&format!("{tag}{msg}"))
}

/// The Anthropic error `type` (e.g. `"overloaded_error"`) for `ProviderError.code`.
fn error_type(err: &serde_json::Value) -> Option<String> {
    err.get("type")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// SSE decoding (unit-testable, no network)
// ---------------------------------------------------------------------------

/// In-flight state for one content block (by index).
#[derive(Default)]
struct BlockState {
    kind: String,
    // tool_use:
    id: String,
    name: String,
    input_json: String,
    // thinking / redacted_thinking:
    signature: String,
    redacted_data: String,
}

/// Stateful Anthropic SSE decoder. Feed raw byte chunks; get whole kernel
/// `StreamEvent`s. Splitting the event→event mapping out here (vs inline in the network
/// loop) makes it deterministic and testable from recorded bytes.
struct AnthropicSseDecoder {
    buf: Vec<u8>,
    blocks: Vec<BlockState>,
    input_tokens: u32,
    cache_read: u32,
    cache_creation: u32,
    output_tokens: u32,
    truncated: bool,
    done: bool,
    saw_message_stop: bool,
    response_id_seen: bool,
}

fn is_usable_content_event(ev: &StreamEvent) -> bool {
    match ev {
        StreamEvent::TextDelta(t) => !t.is_empty(),
        StreamEvent::Reasoning(r) => !r.is_empty(),
        StreamEvent::ToolCall(_) | StreamEvent::ToolCallDelta { .. } => true,
        _ => false,
    }
}

impl AnthropicSseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            blocks: Vec::new(),
            input_tokens: 0,
            cache_read: 0,
            cache_creation: 0,
            output_tokens: 0,
            truncated: false,
            done: false,
            saw_message_stop: false,
            response_id_seen: false,
        }
    }

    fn saw_message_stop(&self) -> bool {
        self.saw_message_stop
    }

    /// Feed a chunk of raw bytes; return any complete `StreamEvent`s produced. Safe
    /// across arbitrary chunk boundaries (UTF-8 is only decoded on whole lines).
    fn feed(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&raw);
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            self.process_line(text, &mut out);
            if self.done {
                break;
            }
        }
        out
    }

    /// Stream ended WITHOUT a `message_stop`: flush a final usage (if any) + `Done`.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        if self.input_tokens > 0 || self.output_tokens > 0 || self.cached_total() > 0 {
            out.push(StreamEvent::Usage(self.usage()));
        }
        out.push(StreamEvent::Done {
            truncated: self.truncated,
        });
        self.done = true;
        out
    }

    fn cached_total(&self) -> u32 {
        self.cache_read
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage {
            prompt: self.input_tokens + self.cache_read + self.cache_creation,
            completion: self.output_tokens,
            cached: self.cache_read,
        }
    }

    fn block_mut(&mut self, index: usize) -> &mut BlockState {
        while self.blocks.len() <= index {
            self.blocks.push(BlockState::default());
        }
        &mut self.blocks[index]
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) {
        // Only `data:` lines carry JSON (which itself names its `type`); ignore
        // `event:` / comment / blank lines.
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return, // ignore keepalive / unparseable
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message_start" => {
                let msg = v.get("message");
                if !self.response_id_seen {
                    if let Some(id) = msg
                        .and_then(|m| m.get("id"))
                        .and_then(|i| i.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        self.response_id_seen = true;
                        out.push(StreamEvent::ResponseId(id.to_string()));
                    }
                }
                if let Some(u) = msg.and_then(|m| m.get("usage")) {
                    self.input_tokens = u32_at(u, "input_tokens");
                    self.cache_read = u32_at(u, "cache_read_input_tokens");
                    self.cache_creation = u32_at(u, "cache_creation_input_tokens");
                }
            }
            "content_block_start" => {
                let index = usize_at(&v, "index");
                let cb = v.get("content_block");
                let kind = cb
                    .and_then(|c| c.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = cb
                    .and_then(|c| c.get("id"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = cb
                    .and_then(|c| c.get("name"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let data = cb
                    .and_then(|c| c.get("data"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let sig = cb
                    .and_then(|c| {
                        c.get("signature")
                            .or_else(|| c.get("thought_signature"))
                            .or_else(|| c.get("thoughtSignature"))
                    })
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let b = self.block_mut(index);
                b.kind = kind;
                b.id = id;
                b.name = name;
                b.redacted_data = data;
                if !sig.is_empty() {
                    b.signature = sig;
                }
            }
            "content_block_delta" => {
                let index = usize_at(&v, "index");
                let delta = v.get("delta");
                let dtype = delta
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(|s| s.as_str())
                        {
                            if !t.is_empty() {
                                out.push(StreamEvent::TextDelta(t.to_string()));
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(|s| s.as_str())
                        {
                            if !t.is_empty() {
                                out.push(StreamEvent::Reasoning(t.to_string()));
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(s) = delta
                            .and_then(|d| {
                                d.get("signature")
                                    .or_else(|| d.get("thought_signature"))
                                    .or_else(|| d.get("thoughtSignature"))
                            })
                            .and_then(|s| s.as_str())
                        {
                            self.block_mut(index).signature.push_str(s);
                        }
                    }
                    "input_json_delta" => {
                        let frag = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.block_mut(index).input_json.push_str(&frag);
                        // Live display fragment; the WHOLE call is emitted at stop.
                        out.push(StreamEvent::ToolCallDelta {
                            index: index as u32,
                            id: None,
                            name: None,
                            arguments: frag,
                        });
                    }
                    _ => {}
                }
                // Also handle gateways that attach signature/thought_signature directly inside delta (e.g. alongside thinking_delta)
                if dtype != "signature_delta" {
                    if let Some(s) = delta
                        .and_then(|d| {
                            d.get("signature")
                                .or_else(|| d.get("thought_signature"))
                                .or_else(|| d.get("thoughtSignature"))
                        })
                        .and_then(|s| s.as_str())
                    {
                        if !s.is_empty() {
                            let b = self.block_mut(index);
                            if b.signature.is_empty() {
                                b.signature.push_str(s);
                            } else if b.signature != s && !b.signature.contains(s) {
                                b.signature.push_str(s);
                            }
                        }
                    }
                }
            }
            "content_block_stop" => {
                let index = usize_at(&v, "index");
                if index >= self.blocks.len() {
                    return;
                }
                // Check if gateway passes signature at content_block_stop
                if let Some(s) = v
                    .get("signature")
                    .or_else(|| v.get("thought_signature"))
                    .or_else(|| v.get("thoughtSignature"))
                    .or_else(|| {
                        v.get("content_block").and_then(|c| {
                            c.get("signature")
                                .or_else(|| c.get("thought_signature"))
                                .or_else(|| c.get("thoughtSignature"))
                        })
                    })
                    .and_then(|s| s.as_str())
                {
                    if !s.is_empty() && self.blocks[index].signature.is_empty() {
                        self.blocks[index].signature = s.to_string();
                    }
                }
                // take() the block so its buffers can move into the emitted events.
                let b = std::mem::take(&mut self.blocks[index]);
                match b.kind.as_str() {
                    "tool_use" => {
                        let args = if b.input_json.trim().is_empty() {
                            "{}".to_string()
                        } else {
                            b.input_json
                        };
                        out.push(StreamEvent::ToolCall(ToolCall {
                            id: b.id,
                            name: b.name,
                            arguments: args,
                        }));
                    }
                    "thinking" => {
                        out.push(StreamEvent::ReasoningSignature {
                            opaque: b.signature,
                            provider: "anthropic".into(),
                            id: None,
                        });
                    }
                    "redacted_thinking" => {
                        out.push(StreamEvent::ReasoningSignature {
                            opaque: b.redacted_data,
                            provider: "anthropic_redacted".into(),
                            id: None,
                        });
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    if sr == "max_tokens" {
                        self.truncated = true;
                    }
                }
                if let Some(u) = v.get("usage") {
                    let o = u32_at(u, "output_tokens");
                    if o > 0 {
                        self.output_tokens = o;
                    }
                    let inp = u32_at(u, "input_tokens");
                    if inp > 0 {
                        self.input_tokens = inp;
                    }
                    let cr = u32_at(u, "cache_read_input_tokens");
                    if cr > 0 {
                        self.cache_read = cr;
                    }
                    let cc = u32_at(u, "cache_creation_input_tokens");
                    if cc > 0 {
                        self.cache_creation = cc;
                    }
                }
            }
            "message_stop" => {
                self.saw_message_stop = true;
                out.push(StreamEvent::Usage(self.usage()));
                out.push(StreamEvent::Done {
                    truncated: self.truncated,
                });
                self.done = true;
            }
            "error" => {
                let err = v.get("error");
                let detail = err
                    .map(parse_error_obj)
                    .unwrap_or_else(|| "unknown error".to_string());
                out.push(StreamEvent::Error(ProviderError {
                    retryable: false,
                    message: format!("provider error: {detail}"),
                    http_status: None,
                    code: err.and_then(error_type),
                    retry_after_secs: None, // mid-stream error: no response headers
                }));
                self.done = true;
            }
            _ => {} // "ping" and anything else
        }
    }
}

fn u32_at(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as u32
}

fn usize_at(v: &Value, key: &str) -> usize {
    v.get(key).and_then(|n| n.as_u64()).unwrap_or(0) as usize
}

// ---------------------------------------------------------------------------
// Tests (deterministic, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jeikcode_kernel::message::{ImageContent, ReasoningBlock};

    fn format_messages(messages: &[Message], echo_thinking: bool) -> (Vec<String>, Vec<Value>) {
        super::format_messages(messages, echo_thinking, true)
    }

    fn build_request_body(
        model: &str,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
        cfg: &AnthropicConfig,
    ) -> Value {
        let policy = AnthropicProvider::resolve_policy(cfg);
        super::build_request_body(model, messages, tools, options, cfg, policy)
    }

    fn cfg() -> AnthropicConfig {
        AnthropicConfig::new("k", "https://api.anthropic.test", "claude-opus-4-8")
    }

    fn roles(out: &[Value]) -> Vec<String> {
        out.iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()).map(str::to_string))
            .collect()
    }
    fn has_consecutive_user(out: &[Value]) -> bool {
        roles(out)
            .windows(2)
            .any(|w| w[0] == "user" && w[1] == "user")
    }

    #[test]
    fn messages_endpoint_url_joins_default_or_custom_version() {
        assert_eq!(
            messages_endpoint_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_endpoint_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_endpoint_url("http://127.0.0.1:8046"),
            "http://127.0.0.1:8046/v1/messages"
        );
        assert_eq!(
            messages_endpoint_url("http://127.0.0.1:8046/v1"),
            "http://127.0.0.1:8046/v1/messages"
        );
        assert_eq!(
            messages_endpoint_url("http://127.0.0.1:8046/v1/"),
            "http://127.0.0.1:8046/v1/messages"
        );
        assert_eq!(
            messages_endpoint_url("http://127.0.0.1:8046/v2"),
            "http://127.0.0.1:8046/v2/messages"
        );
        assert_eq!(
            messages_endpoint_url("https://gateway.example/anthropic/v3"),
            "https://gateway.example/anthropic/v3/messages"
        );
        assert_eq!(
            messages_endpoint_url(" https://gateway.example/anthropic/v1beta "),
            "https://gateway.example/anthropic/v1beta/messages"
        );
        assert_eq!(
            messages_endpoint_url("http://127.0.0.1:8046/custom/messages"),
            "http://127.0.0.1:8046/custom/messages"
        );
        assert_eq!(
            messages_endpoint_url("http://127.0.0.1:8046/api"),
            "http://127.0.0.1:8046/api/v1/messages"
        );
    }

    #[test]
    fn no_consecutive_user_after_tool_fold_then_reminder() {
        use jeikcode_kernel::message::Message;
        use jeikcode_kernel::tool::ToolCall;
        // tool result folds into a user message; the injected reminder is another user
        // message right after — Anthropic would 400 without merging them.
        let msgs = vec![
            Message::user("do X"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("c1", "result text", false),
            Message::user("<system-reminder>\nstatus\n</system-reminder>"),
        ];
        let (_system, out) = format_messages(&msgs, false);
        assert!(
            !has_consecutive_user(&out),
            "no consecutive user on the wire: {:?}",
            roles(&out)
        );
        let wire = serde_json::to_string(&out).unwrap();
        assert!(
            wire.contains("system-reminder"),
            "reminder preserved (merged): {wire}"
        );
        assert!(
            wire.contains("result text"),
            "tool result preserved: {wire}"
        );
    }

    #[test]
    fn no_consecutive_user_post_compaction_summary_beside_user() {
        use jeikcode_kernel::message::Message;
        // After compaction: synthetic-summary (user) sits beside the real user turn.
        let mut summary = Message::user("summary of prior work");
        summary.synthetic = true;
        let msgs = vec![
            Message::user("prompt1"),
            summary,
            Message::user("follow up"),
        ];
        let (_system, out) = format_messages(&msgs, false);
        assert!(
            !has_consecutive_user(&out),
            "merged into one user: {:?}",
            roles(&out)
        );
        assert_eq!(out.len(), 1, "three consecutive users → one");
        assert_eq!(
            out[0]["content"],
            json!([
                {"type": "text", "text": "prompt1"},
                {"type": "text", "text": "summary of prior work"},
                {"type": "text", "text": "follow up"}
            ])
        );
    }

    #[test]
    fn reminder_above_query_merges_with_query_last() {
        let msgs = vec![
            Message::synthetic_user(
                "<system-reminder>\nCurrent date: 2026-08-22\n</system-reminder>",
            ),
            Message::user("the real question"),
        ];
        let (_system, out) = format_messages(&msgs, false);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["content"],
            json!([
                {"type": "text", "text": "<system-reminder>\nCurrent date: 2026-08-22\n</system-reminder>"},
                {"type": "text", "text": "the real question"}
            ]),
            "merged Anthropic user block must preserve discrete blocks"
        );
    }

    #[test]
    fn memory_and_instructions_stay_as_discrete_user_blocks_not_system() {
        let msgs = vec![
            Message::system("be terse"),
            Message::synthetic_user(
                "=== AUTHORITATIVE PROJECT INSTRUCTIONS & KNOWLEDGE (*.md) ===\nagents",
            ),
            Message::synthetic_user("=== MEMORY ===\n- prefers tabs"),
            Message::user("the real question"),
        ];
        let (system, out) = format_messages(&msgs, false);
        assert_eq!(
            system,
            vec!["be terse".to_string()],
            "only true system messages stay in system blocks"
        );
        assert_eq!(out.len(), 1, "consecutive users fold into one user turn");
        assert_eq!(out[0]["role"], "user");
        let content = out[0]["content"]
            .as_array()
            .expect("must be array of blocks");
        assert_eq!(
            content.len(),
            3,
            "preserves instructions, memory, and query as distinct blocks"
        );
        assert_eq!(
            content[0]["text"],
            "=== AUTHORITATIVE PROJECT INSTRUCTIONS & KNOWLEDGE (*.md) ===\nagents"
        );
        assert_eq!(content[1]["text"], "=== MEMORY ===\n- prefers tabs");
        assert_eq!(content[2]["text"], "the real question");
    }

    fn line(event: &str, v: Value) -> String {
        format!("event: {event}\ndata: {v}\n\n")
    }

    // ---- request building ----

    #[test]
    fn system_is_lifted_to_top_level_and_roles_map_to_content() {
        let msgs = vec![
            Message::system("be terse"),
            Message::user("hi"),
            Message::assistant(
                "ans",
                vec![ToolCall {
                    id: "tc1".into(),
                    name: "read".into(),
                    arguments: "{\"p\":\"a\"}".into(),
                }],
            ),
            Message::tool_result("tc1", "file body", false),
        ];
        let (system, out) = format_messages(&msgs, false);
        assert_eq!(
            system,
            vec!["be terse".to_string()],
            "leading System lifts to top-level system blocks"
        );
        // text-only user → content STRING (prefix-cache parity with no-block path).
        assert_eq!(out[0], json!({"role":"user","content":"hi"}));
        // assistant with a tool call → content ARRAY: text block then tool_use (input is an OBJECT).
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0], json!({"type":"text","text":"ans"}));
        assert_eq!(
            out[1]["content"][1],
            json!({"type":"tool_use","id":"tc1","name":"read","input":{"p":"a"}})
        );
        // tool result → a USER message carrying a tool_result block.
        assert_eq!(
            out[2],
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"tc1","content":"file body","is_error":false}]})
        );
    }

    #[test]
    fn consecutive_tool_results_fold_into_one_user_message() {
        let msgs = vec![
            Message::user("go"),
            Message::assistant(
                "",
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "x".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "y".into(),
                        arguments: "{}".into(),
                    },
                ],
            ),
            Message::tool_result("a", "ra", false),
            Message::tool_result("b", "rb", true),
        ];
        let (_sys, out) = format_messages(&msgs, false);
        // user, assistant, then ONE user with two tool_result blocks.
        assert_eq!(
            out.len(),
            3,
            "two consecutive tool results fold into one user message"
        );
        let blocks = out[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
        assert_eq!(blocks[1]["is_error"], true, "tool failure flag is echoed");
    }

    #[test]
    fn assistant_signed_thinking_is_echoed_when_enabled() {
        let mut a = Message::assistant(
            "answer",
            vec![ToolCall {
                id: "t".into(),
                name: "n".into(),
                arguments: "{}".into(),
            }],
        );
        a.reasoning_blocks = vec![
            ReasoningBlock {
                text: "let me think".into(),
                opaque: Some("sig-1".into()),
                provider: Some("anthropic".into()),
                id: None,
            },
            ReasoningBlock {
                text: String::new(),
                opaque: Some("redacted-data".into()),
                provider: Some("anthropic".into()),
                id: None,
            },
        ];
        let (_s, out) = format_messages(&[Message::user("hi"), a], true);
        let content = out[1]["content"].as_array().unwrap();
        // thinking blocks FIRST (signed), redacted next, then text, then tool_use.
        assert_eq!(
            content[0],
            json!({"type":"thinking","thinking":"let me think","signature":"sig-1"})
        );
        assert_eq!(
            content[1],
            json!({"type":"redacted_thinking","data":"redacted-data"})
        );
        assert_eq!(content[2], json!({"type":"text","text":"answer"}));
        assert_eq!(content[3]["type"], "tool_use");
    }

    #[test]
    fn signed_thinking_is_dropped_when_thinking_disabled() {
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = vec![ReasoningBlock {
            text: "x".into(),
            opaque: Some("s".into()),
            provider: Some("anthropic".into()),
            id: None,
        }];
        let (_s, out) = format_messages(&[Message::user("hi"), a], false);
        // thinking disabled → no signed blocks echoed; plain text content.
        assert_eq!(out[1], json!({"role":"assistant","content":"answer"}));
    }

    #[test]
    fn foreign_provider_thinking_block_is_not_echoed() {
        // A signed block from ANOTHER vendor must NOT be replayed to Anthropic (its
        // opaque token is provider-bound; echoing it 400s). `None` is foreign too.
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = vec![
            ReasoningBlock {
                text: "from openai".into(),
                opaque: Some("oai-sig".into()),
                provider: Some("openai".into()),
                id: None,
            },
            ReasoningBlock {
                text: "no provider".into(),
                opaque: Some("x".into()),
                provider: None,
                id: None,
            },
        ];
        // thinking ENABLED, but every block is foreign → collapses to a plain string.
        let (_s, out) = format_messages(&[Message::user("hi"), a], true);
        assert_eq!(
            out[1],
            json!({"role":"assistant","content":"answer"}),
            "foreign / unattributed thinking blocks are never echoed"
        );
    }

    #[test]
    fn mixed_provider_blocks_echo_only_anthropic_ones_in_order() {
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = vec![
            ReasoningBlock {
                text: "ours".into(),
                opaque: Some("sig-a".into()),
                provider: Some("anthropic".into()),
                id: None,
            },
            ReasoningBlock {
                text: "theirs".into(),
                opaque: Some("sig-b".into()),
                provider: Some("openai".into()),
                id: None,
            },
        ];
        let (_s, out) = format_messages(&[Message::user("hi"), a], true);
        let content = out[1]["content"].as_array().unwrap();
        // only the Anthropic block is echoed, then the text — the foreign one is dropped.
        assert_eq!(
            content[0],
            json!({"type":"thinking","thinking":"ours","signature":"sig-a"})
        );
        assert_eq!(content[1], json!({"type":"text","text":"answer"}));
        assert_eq!(content.len(), 2, "the foreign block must not appear");
    }

    #[test]
    fn gateway_echoes_reasoning_from_message_when_blocks_empty() {
        let mut a = Message::assistant("answer", vec![]);
        a.reasoning = Some("synthesized reasoning from gateway".into());
        // Non-official gateway (is_official = false) with echo_thinking = true
        let (_s, out) = super::format_messages(&[Message::user("hi"), a], true, false);
        let content = out[1]["content"]
            .as_array()
            .expect("array of content blocks");
        assert_eq!(
            content[0],
            json!({
                "type": "thinking",
                "thinking": "synthesized reasoning from gateway",
                "signature": "",
            })
        );
        assert_eq!(content[1], json!({"type":"text","text":"answer"}));
    }

    #[test]
    fn gateway_echoes_placeholder_on_tool_calls_when_empty() {
        let a = Message::assistant(
            "",
            vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
        );
        // Non-official gateway (is_official = false) with echo_thinking = true and no reasoning
        let (_s, out) = super::format_messages(&[Message::user("hi"), a], true, false);
        let content = out[1]["content"]
            .as_array()
            .expect("array of content blocks");
        assert_eq!(
            content[0],
            json!({
                "type": "thinking",
                "thinking": REASONING_PLACEHOLDER,
                "signature": "",
            })
        );
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn thinking_request_body_emits_enabled_with_budget() {
        let mut c = cfg();
        c.thinking = true;
        c.max_tokens = 16384;
        c.thinking_budget = Some(5000);
        let body = build_request_body(
            // 4.6 and earlier keep the legacy manual budget shape.
            "claude-sonnet-4-5",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &c,
        );
        assert_eq!(
            body["thinking"],
            json!({
                "type": "enabled",
                "budget_tokens": 5000,
            })
        );
    }

    #[test]
    fn thinking_request_body_emits_adaptive_when_configured() {
        let mut c = cfg();
        c.thinking = true;
        c.thinking_type = Some("adaptive".into());
        let body = build_request_body(
            "claude-opus-4-8",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &c,
        );
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
    }

    #[test]
    fn thinking_enabled_by_reasoning_policy_include() {
        let mut c = cfg();
        c.thinking = false;
        c.max_tokens = 8192;
        c.reasoning_policy = Some(ReasoningPolicy::Include);
        let body = build_request_body(
            "claude-sonnet-4-5",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &c,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        // Default 10000 shrinks to leave MIN_ANSWER_TOKENS (1024): 8192 - 1024.
        assert_eq!(body["thinking"]["budget_tokens"], 7168);
    }

    #[test]
    fn high_budget_is_clamped_to_answer_room_without_expanding_max_tokens() {
        let mut c = cfg();
        c.thinking = true;
        c.max_tokens = 16384;
        c.thinking_budget = Some(16384);
        let body = build_request_body(
            "claude-sonnet-4-5",
            &[Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &c,
        );
        // The budget is a downstream constraint: 16384 - MIN_ANSWER_TOKENS.
        assert_eq!(body["thinking"]["budget_tokens"], 15360);
        // ... and max_tokens must stay exactly the kernel-resolved budget.
        assert_eq!(body["max_tokens"].as_u64(), Some(16384));
    }

    #[test]
    fn thinking_request_body_emits_disabled_when_off() {
        let mut c = cfg();
        c.thinking = true;
        c.reasoning_policy = Some(ReasoningPolicy::Include);
        let mut opts = ChatOptions::default();
        opts.reasoning_effort = Some(ReasoningEffort::Off);
        let body = build_request_body("claude-opus-4-8", &[Message::user("hi")], &[], &opts, &c);
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn user_images_become_base64_image_blocks() {
        let m = Message::user_with_images(
            "look",
            vec![ImageContent {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        let (_s, out) = format_messages(&[m], false);
        let c = out[0]["content"].as_array().unwrap();
        assert_eq!(c[0], json!({"type":"text","text":"look"}));
        assert_eq!(
            c[1],
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}})
        );
    }

    #[test]
    fn body_has_required_max_tokens_stream_and_system() {
        let mut c = cfg();
        c.max_tokens = 1000;
        let body = build_request_body(
            "claude-opus-4-8",
            &[Message::system("s"), Message::user("hi")],
            &[],
            &ChatOptions::default(),
            &c,
        );
        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(
            body["max_tokens"].as_u64(),
            Some(1000),
            "max_tokens from cfg when options has none"
        );
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["system"][0]["text"], "s",
            "system is a cached text block"
        );
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral",
            "last message is a cache breakpoint"
        );
        assert!(body.get("tools").is_none(), "empty tools omitted");
        assert!(body.get("tool_choice").is_none(), "Auto omits tool_choice");
        assert!(body.get("thinking").is_none(), "thinking off by default");
    }

    #[test]
    fn multi_segment_system_caches_final_system_and_synthetic_user_prefix() {
        let c = cfg();
        let body = build_request_body(
            "claude-opus-4-8",
            &[
                Message::system("block1"),
                Message::system("block2"),
                Message::system("block3"),
                Message::synthetic_user("<memory>\nm\n</memory>"),
                Message::synthetic_user("<project_instructions>\np\n</project_instructions>"),
                Message::user("hi"),
            ],
            &[],
            &ChatOptions::default(),
            &c,
        );
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 3);
        assert_eq!(sys[0]["text"], "block1");
        assert!(sys[0].get("cache_control").is_none());
        assert_eq!(sys[1]["text"], "block2");
        assert!(sys[1].get("cache_control").is_none());
        assert_eq!(sys[2]["text"], "block3");
        assert_eq!(sys[2]["cache_control"]["type"], "ephemeral");

        let msg0_blocks = body["messages"][0]["content"]
            .as_array()
            .expect("merged user content is array");
        assert_eq!(msg0_blocks.len(), 3);
        assert!(msg0_blocks[0].get("cache_control").is_none());
        assert_eq!(
            msg0_blocks[1]["cache_control"]["type"], "ephemeral",
            "last frozen synthetic user block must carry cache_control"
        );
        assert_eq!(
            msg0_blocks[2]["cache_control"]["type"], "ephemeral",
            "last message block must carry cache_control"
        );
    }

    #[test]
    fn body_options_and_tools_mapped() {
        let mut c = cfg();
        c.thinking = true;
        let opts = ChatOptions {
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(2048),
            temperature: Some(0.5),
            tool_choice: ToolChoice::Required,
            rate_limit_retry_owner: Default::default(),
        };
        let tools = vec![ToolDef {
            name: "read".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = build_request_body("claude-opus-4-8", &[Message::user("hi")], &tools, &opts, &c);
        assert_eq!(
            body["max_tokens"].as_u64(),
            Some(2048),
            "options.max_tokens overrides cfg"
        );
        assert!(
            body.get("temperature").is_none(),
            "sampling params omitted by default (Opus 4.7+ reject temperature)"
        );
        assert_eq!(body["tool_choice"], json!({"type":"any"}), "Required → any");
        assert!(
            body.get("thinking").is_none(),
            "forced tool use must suppress incompatible thinking for this request"
        );
        assert_eq!(body["output_config"]["effort"], "high");
        // tools use input_schema, NOT function.parameters.
        assert_eq!(
            body["tools"][0],
            json!({
                "name":"read",
                "description":"d",
                "input_schema":{"type":"object"},
                "cache_control":{"type":"ephemeral"}
            })
        );
    }

    #[test]
    fn specific_tool_choice_uses_anthropic_tool_shape() {
        let mut c = cfg();
        c.thinking = true;
        let opts = ChatOptions {
            tool_choice: ToolChoice::Specific("todowrite".into()),
            ..Default::default()
        };
        let body = build_request_body("claude-opus-4-8", &[Message::user("hi")], &[], &opts, &c);
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], "todowrite");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn tool_choice_none_maps() {
        let opts = ChatOptions {
            tool_choice: ToolChoice::None,
            ..Default::default()
        };
        let body = build_request_body(
            "claude-opus-4-8",
            &[Message::user("hi")],
            &[],
            &opts,
            &cfg(),
        );
        assert_eq!(body["tool_choice"], json!({"type":"none"}));
    }

    #[test]
    fn temperature_sent_only_when_sampling_params_enabled() {
        let opts = ChatOptions {
            temperature: Some(0.5),
            ..Default::default()
        };
        // Default cfg (modern Claude): temperature is OMITTED — Opus 4.7+ 400 on it.
        let off = build_request_body(
            "claude-opus-4-8",
            &[Message::user("hi")],
            &[],
            &opts,
            &cfg(),
        );
        assert!(off.get("temperature").is_none(), "omitted by default");
        // Opt in for an older Claude that still accepts sampling params.
        let mut c = cfg();
        c.send_sampling_params = true;
        let on = build_request_body("claude-3-5-sonnet", &[Message::user("hi")], &[], &opts, &c);
        assert_eq!(on["temperature"], 0.5, "forwarded when opted in");
    }

    #[test]
    fn body_serialization_is_deterministic() {
        let mut c = cfg();
        c.thinking = true;
        let opts = ChatOptions {
            temperature: Some(0.7),
            tool_choice: ToolChoice::Required,
            ..Default::default()
        };
        let tools = vec![
            ToolDef {
                name: "b".into(),
                description: "db".into(),
                parameters: json!({"type":"object","properties":{"z":{"type":"string"},"a":{"type":"number"}}}),
            },
            ToolDef {
                name: "a".into(),
                description: "da".into(),
                parameters: json!({"type":"object"}),
            },
        ];
        let msgs = vec![Message::system("s"), Message::user("u")];
        let first = serde_json::to_string(&build_request_body(
            "claude-opus-4-8",
            &msgs,
            &tools,
            &opts,
            &c,
        ))
        .unwrap();
        for _ in 0..100 {
            let again = serde_json::to_string(&build_request_body(
                "claude-opus-4-8",
                &msgs,
                &tools,
                &opts,
                &c,
            ))
            .unwrap();
            assert_eq!(
                first, again,
                "request body serialization must be deterministic"
            );
        }
    }

    #[test]
    fn prefix_is_append_only_across_turns() {
        let h1 = vec![Message::system("s"), Message::user("u1")];
        let mut h2 = h1.clone();
        h2.push(Message::assistant("a1", vec![]));
        let (_s1, f1) = format_messages(&h1, false);
        let (_s2, f2) = format_messages(&h2, false);
        for i in 0..f1.len() {
            assert_eq!(
                serde_json::to_string(&f1[i]).unwrap(),
                serde_json::to_string(&f2[i]).unwrap(),
                "shared prefix message {i} must serialize identically"
            );
        }
    }

    // ---- SSE decoding ----

    fn kinds(ev: &[StreamEvent]) -> Vec<&'static str> {
        ev.iter()
            .map(|e| match e {
                StreamEvent::Reasoning(_) => "reason",
                StreamEvent::ReasoningSignature { .. } => "reasonsig",
                StreamEvent::TextDelta(_) => "text",
                StreamEvent::ToolCall(_) => "tool",
                StreamEvent::ToolCallDelta { .. } => "tooldelta",
                StreamEvent::Usage(_) => "usage",
                StreamEvent::ResponseId(_) => "response_id",
                StreamEvent::ResponseModel(_) => "response_model",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error(_) => "error",
                StreamEvent::Malformed => "malformed",
            })
            .collect()
    }

    #[test]
    fn sse_text_then_usage_then_done() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("message_start", json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":3}}})).as_bytes()));
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}})).as_bytes()));
        ev.extend(
            d.feed(
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );
        ev.extend(d.feed(line("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})).as_bytes()));
        ev.extend(d.feed(line("message_stop", json!({"type":"message_stop"})).as_bytes()));

        assert!(matches!(&ev[0], StreamEvent::ResponseId(id) if id == "msg_1"));
        assert!(matches!(&ev[1], StreamEvent::TextDelta(s) if s == "Hel"));
        assert!(matches!(&ev[2], StreamEvent::TextDelta(s) if s == "lo"));
        let u = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::Usage(u) = e {
                    Some(*u)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            u.prompt, 13,
            "prompt = input + cache_read (+ cache_creation)"
        );
        assert_eq!(u.completion, 5);
        assert_eq!(u.cached, 3);
        assert!(matches!(
            ev.last().unwrap(),
            StreamEvent::Done { truncated: false }
        ));
    }

    #[test]
    fn sse_tool_call_assembled_from_input_json_deltas() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"search","input":{}}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"hi\"}"}})).as_bytes()));
        ev.extend(
            d.feed(
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );

        // live fragments for display
        let deltas: Vec<_> = ev
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallDelta { .. }))
            .collect();
        assert_eq!(deltas.len(), 2, "one ToolCallDelta per input_json_delta");
        // one whole call at content_block_stop
        let tc = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::ToolCall(c) = e {
                    Some(c.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(tc.id, "tu_1");
        assert_eq!(tc.name, "search");
        assert_eq!(tc.arguments, "{\"q\":\"hi\"}");
    }

    #[test]
    fn sse_thinking_emits_reasoning_then_signature() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step 1"}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-xyz"}})).as_bytes()));
        ev.extend(
            d.feed(
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );
        assert_eq!(kinds(&ev), vec!["reason", "reasonsig"]);
        assert!(matches!(&ev[0], StreamEvent::Reasoning(s) if s == "step 1"));
        assert!(
            matches!(&ev[1], StreamEvent::ReasoningSignature { opaque, provider, .. } if opaque == "sig-xyz" && provider == "anthropic")
        );
    }

    #[test]
    fn sse_thinking_captures_signature_from_content_block_start() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(
            d.feed(
                line(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "thinking",
                            "thinking": "",
                            "thought_signature": "gemini-sig-start"
                        }
                    }),
                )
                .as_bytes(),
            ),
        );
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thinking..."}})).as_bytes()));
        ev.extend(
            d.feed(
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );
        assert_eq!(kinds(&ev), vec!["reason", "reasonsig"]);
        assert!(
            matches!(&ev[1], StreamEvent::ReasoningSignature { opaque, .. } if opaque == "gemini-sig-start")
        );
    }

    #[test]
    fn sse_thinking_captures_thought_signature_from_delta() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})).as_bytes()));
        ev.extend(
            d.feed(
                line(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {
                            "type": "thinking_delta",
                            "thinking": "thinking...",
                            "thoughtSignature": "gemini-sig-delta"
                        }
                    }),
                )
                .as_bytes(),
            ),
        );
        ev.extend(
            d.feed(
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );
        assert_eq!(kinds(&ev), vec!["reason", "reasonsig"]);
        assert!(
            matches!(&ev[1], StreamEvent::ReasoningSignature { opaque, .. } if opaque == "gemini-sig-delta")
        );
    }

    #[test]
    fn sse_redacted_thinking_emits_signature_only() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"enc-data"}})).as_bytes()));
        ev.extend(
            d.feed(
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                )
                .as_bytes(),
            ),
        );
        assert_eq!(kinds(&ev), vec!["reasonsig"]);
        assert!(
            matches!(&ev[0], StreamEvent::ReasoningSignature { opaque, .. } if opaque == "enc-data")
        );
    }

    #[test]
    fn sse_max_tokens_sets_truncated() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = Vec::new();
        ev.extend(d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes()));
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}})).as_bytes()));
        ev.extend(d.feed(line("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}})).as_bytes()));
        ev.extend(d.feed(line("message_stop", json!({"type":"message_stop"})).as_bytes()));
        assert!(matches!(
            ev.last().unwrap(),
            StreamEvent::Done { truncated: true }
        ));
    }

    #[test]
    fn sse_mid_stream_error_surfaces_and_terminates() {
        let mut d = AnthropicSseDecoder::new();
        let ev = d.feed(
            line(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}),
            )
            .as_bytes(),
        );
        let e = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::Error(e) = e {
                    Some(e.clone())
                } else {
                    None
                }
            })
            .expect("error surfaced");
        assert!(e.message.contains("overloaded_error"));
        assert!(e.message.contains("overloaded"));
        assert_eq!(e.code.as_deref(), Some("overloaded_error"));
        assert!(!e.retryable, "mid-stream errors are non-retryable");
    }

    #[test]
    fn sse_byte_split_robust_and_utf8_safe() {
        let payload = format!(
            "{}{}{}{}",
            line(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
            ),
            line(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"héllo世界"}})
            ),
            line(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0})
            ),
            line("message_stop", json!({"type":"message_stop"})),
        );
        let mut d1 = AnthropicSseDecoder::new();
        let whole = d1.feed(payload.as_bytes());
        let mut d2 = AnthropicSseDecoder::new();
        let mut split = Vec::new();
        for b in payload.as_bytes() {
            split.extend(d2.feed(&[*b]));
        }
        assert_eq!(format!("{whole:?}"), format!("{split:?}"));
        assert!(matches!(&whole[0], StreamEvent::TextDelta(s) if s == "héllo世界"));
    }

    #[test]
    fn sse_full_fixture_thinking_text_tool_usage() {
        let mut d = AnthropicSseDecoder::new();
        let mut sse = String::new();
        sse.push_str(&line(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_x","usage":{"input_tokens":7}}}),
        ));
        sse.push_str(&line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})));
        sse.push_str(&line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}})));
        sse.push_str(&line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}})));
        sse.push_str(&line(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ));
        sse.push_str(&line("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}})));
        sse.push_str(&line("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}})));
        sse.push_str(&line(
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        ));
        sse.push_str(&line("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu","name":"now","input":{}}})));
        sse.push_str(&line(
            "content_block_stop",
            json!({"type":"content_block_stop","index":2}),
        ));
        sse.push_str(&line("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}})));
        sse.push_str(&line("message_stop", json!({"type":"message_stop"})));

        let ev = d.feed(sse.as_bytes());
        assert_eq!(
            kinds(&ev),
            vec![
                "response_id",
                "reason",
                "reasonsig",
                "text",
                "tool",
                "usage",
                "done"
            ]
        );
        // a tool_use block that received NO input_json_delta defaults to "{}".
        let tc = ev
            .iter()
            .find_map(|e| {
                if let StreamEvent::ToolCall(c) = e {
                    Some(c.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(tc.arguments, "{}");
    }

    #[test]
    fn sse_finish_without_message_stop_flushes_done() {
        let mut d = AnthropicSseDecoder::new();
        let mut ev = d.feed(line("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes());
        ev.extend(d.feed(line("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}})).as_bytes()));
        // stream EOF without message_stop → finish() flushes a Done.
        ev.extend(d.finish());
        assert!(matches!(ev.last().unwrap(), StreamEvent::Done { .. }));
    }

    // ---- mid-stream re-open (v1 parity) ----

    /// Fully consume one HTTP request (headers + Content-Length body) so the
    /// client's `send()` always completes before the mock responds.
    fn read_http_request(s: &mut std::net::TcpStream) {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = match s.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                let clen = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut remaining = clen.saturating_sub(buf.len() - (pos + 4));
                while remaining > 0 {
                    match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => remaining = remaining.saturating_sub(n),
                    }
                }
                return;
            }
        }
    }

    #[tokio::test]
    async fn midstream_eof_before_any_event_reopens_and_succeeds() {
        use std::io::Write;
        use std::net::TcpListener;

        // Connection #1 opens a chunked 200 then drops before any event; with
        // nothing delivered the provider must transparently re-open, and #2 serves
        // a valid Anthropic SSE body.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            read_http_request(&mut s1);
            s1.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
            s1.flush().unwrap();
            drop(s1);

            let (mut s2, _) = listener.accept().unwrap();
            read_http_request(&mut s2);
            let body = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            s2.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s2.flush().unwrap();
            drop(s2);
        });

        let cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        let provider = AnthropicProvider::new(cfg).unwrap();

        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        let has_error = events.iter().any(|e| matches!(e, StreamEvent::Error(_)));
        assert!(
            !has_error,
            "must not surface a mid-stream error after a clean re-open: {events:?}"
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "ok",
            "should deliver the re-opened response: {events:?}"
        );

        let _ = handle.join();
    }

    #[tokio::test]
    async fn midstream_reset_twice_before_any_event_reopens_until_success() {
        use std::io::Write;
        use std::net::TcpListener;

        // Connections #1 and #2 both drop before any event; #3 serves a valid
        // body. A single reopen would surface an error after #2 — the provider
        // must reopen twice and deliver only #3's response.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                read_http_request(&mut s);
                s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
                s.flush().unwrap();
                drop(s);
            }
            let (mut s3, _) = listener.accept().unwrap();
            read_http_request(&mut s3);
            let body = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            s3.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s3.flush().unwrap();
            drop(s3);
        });

        let mut cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        cfg.retry.base_delay = std::time::Duration::from_millis(1);
        cfg.retry.max_delay = std::time::Duration::from_millis(2);
        let provider = AnthropicProvider::new(cfg).unwrap();

        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "two pre-event resets must be ridden through transparently: {events:?}"
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "ok",
            "should deliver the third (successful) response: {events:?}"
        );

        let _ = handle.join();
    }

    /// Capture the first request's head, drain its body, then answer 200 + close so
    /// `chat_stream` resolves and the stream ends on EOF (provider-agnostic).
    fn capture_then_ok(
        port_back: std::sync::mpsc::Sender<u16>,
    ) -> (
        std::sync::Arc<std::sync::Mutex<String>>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        port_back
            .send(listener.local_addr().unwrap().port())
            .unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = captured.clone();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = match s.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            *cap.lock().unwrap() = String::from_utf8_lossy(&buf)
                .split("\r\n\r\n")
                .next()
                .unwrap_or("")
                .to_string();
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = s.flush();
            drop(s);
        });
        (captured, handle)
    }

    #[tokio::test]
    async fn forwards_session_id_and_product_ua_alongside_anthropic_auth() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (captured, handle) = capture_then_ok(tx);
        let port = rx.recv().unwrap();

        let mut cfg = AnthropicConfig::new("ak", format!("http://127.0.0.1:{port}"), "claude-test");
        cfg.user_agent = Some("jeikcode/9.9.9".to_string());
        let provider = AnthropicProvider::new(cfg).unwrap();
        provider.bind_session_id("sess-anthropic");
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let _: Vec<StreamEvent> = stream.collect().await;
        let _ = handle.join();

        let head = captured.lock().unwrap().to_lowercase();
        assert!(
            head.contains("x-api-key: ak"),
            "anthropic auth must remain: {head}"
        );
        assert!(
            head.contains("x-jeikcode-session-id: sess-anthropic"),
            "product session header must be forwarded: {head}"
        );
        assert!(
            head.contains("x-session-id: sess-anthropic"),
            "gateway cache-affinity header must be forwarded: {head}"
        );
        assert!(
            head.contains("user-agent: jeikcode/9.9.9"),
            "product UA must be sent: {head}"
        );
    }

    #[test]
    fn reasoning_only_model_round_is_dropped_on_gateways() {
        let mut lost = Message::assistant("", vec![]);
        lost.reasoning = Some("partial thinking".into());
        let msgs = vec![Message::user("first"), lost, Message::user("retry")];
        // echo_thinking=true + non-official gateway used to synthesize a
        // thinking-only assistant block from `reasoning`.
        let (_sys, out) = super::format_messages(&msgs, true, false);
        assert_eq!(out.len(), 1, "thinking-only assistant must be dropped");
        assert_eq!(out[0]["role"], "user");
    }

    #[test]
    fn lost_model_round_is_dropped_and_users_merge() {
        let msgs = vec![
            Message::user("first"),
            Message::assistant("", vec![]),
            Message::user("retry"),
        ];
        let (sys, out) = format_messages(&msgs, false);
        assert!(sys.is_empty());
        assert_eq!(out.len(), 1, "an empty assistant turn must be dropped");
        assert_eq!(out[0]["role"], "user");
        let content = out[0]["content"].as_array().expect("must be blocks");
        assert_eq!(
            content.len(),
            2,
            "both user messages stay as discrete blocks"
        );
        assert_eq!(content[0]["text"], "first");
        assert_eq!(content[1]["text"], "retry");
    }

    #[test]
    fn test_project_instructions_stay_as_first_user_block_and_merge_with_user_input() {
        let msgs = vec![
            Message::system("<environment>env</environment>"),
            Message::synthetic_user(
                "=== AUTHORITATIVE PROJECT INSTRUCTIONS & KNOWLEDGE (*.md) ===\nagents",
            ),
            Message::user("hello world"),
        ];
        let (sys, wire_msgs) = format_messages(&msgs, false);
        assert_eq!(sys, vec!["<environment>env</environment>"]);
        assert_eq!(
            wire_msgs.len(),
            1,
            "consecutive users must be merged into one user message"
        );
        assert_eq!(wire_msgs[0]["role"], "user");
        let content = wire_msgs[0]["content"]
            .as_array()
            .expect("must be array of blocks");
        assert_eq!(content.len(), 2, "must contain two distinct blocks");
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"]
            .as_str()
            .unwrap()
            .contains("AUTHORITATIVE PROJECT INSTRUCTIONS"));
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "hello world");
    }

    #[tokio::test]
    async fn premature_eof_with_only_thinking_signature_errors_instead_of_done() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_http_request(&mut s);
            let body = format!(
                "{}{}{}",
                line(
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                ),
                line(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-only"}}),
                ),
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                ),
            );
            s.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s.flush().unwrap();
            drop(s);
        });

        let cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        let provider = AnthropicProvider::new(cfg).unwrap();
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            !events.iter().any(|e| matches!(
                e,
                StreamEvent::Done { .. } | StreamEvent::ReasoningSignature { .. }
            )),
            "must not emit Done or leak ReasoningSignature: {events:?}"
        );
        let err = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Error(err) => Some(err),
                _ => None,
            })
            .expect("must surface Error");
        assert!(
            err.retryable,
            "signature-only premature EOF must be retryable: {err:?}"
        );
        assert!(
            err.message.contains("missing message_stop")
                && err.message.contains("thinking signature only"),
            "{}",
            err.message
        );
        let _ = handle.join();
    }

    fn sig_only_sse(id: &str, sig: &str) -> String {
        format!(
            "{}{}{}{}",
            line(
                "message_start",
                json!({"type":"message_start","message":{"id":id,"usage":{"input_tokens":5}}})
            ),
            line(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})
            ),
            line(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":sig}})
            ),
            line(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0})
            ),
        )
    }

    #[tokio::test]
    async fn signature_only_block_followed_by_text_flushes_signature_before_text() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_http_request(&mut s);
            let body = format!(
                "{}{}{}{}{}{}",
                sig_only_sse("msg_sig", "sig-deferred"),
                line(
                    "content_block_start",
                    json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}})
                ),
                line(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}})
                ),
                line(
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":1})
                ),
                line(
                    "message_delta",
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})
                ),
                line("message_stop", json!({"type":"message_stop"})),
            );
            s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}").as_bytes()).unwrap();
        });
        let provider = AnthropicProvider::new(AnthropicConfig::new(
            "k",
            format!("http://127.0.0.1:{port}"),
            "claude-test",
        ))
        .unwrap();
        let events: Vec<StreamEvent> = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(
            kinds(&events),
            vec!["response_id", "reasonsig", "text", "usage", "done"]
        );
        assert!(
            matches!(&events[1], StreamEvent::ReasoningSignature { opaque, .. } if opaque == "sig-deferred")
        );
        assert!(matches!(&events[2], StreamEvent::TextDelta(t) if t == "ok"));
        let _ = handle.join();
    }

    #[tokio::test]
    async fn signature_only_block_followed_by_transport_reset_reopens_cleanly() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            read_http_request(&mut s1);
            let b1 = sig_only_sse("msg_drop", "sig-stale");
            s1.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{b1}\r\n", b1.len()).as_bytes()).unwrap();
            drop(s1);
            let (mut s2, _) = listener.accept().unwrap();
            read_http_request(&mut s2);
            let b2 = format!(
                "{}{}{}{}",
                line(
                    "message_start",
                    json!({"type":"message_start","message":{"id":"msg_ok","usage":{"input_tokens":6}}})
                ),
                line(
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
                ),
                line(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"recovered"}})
                ),
                line("message_stop", json!({"type":"message_stop"})),
            );
            s2.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{b2}").as_bytes()).unwrap();
        });
        let mut cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        cfg.retry.base_delay = std::time::Duration::from_millis(1);
        cfg.retry.max_delay = std::time::Duration::from_millis(2);
        let events: Vec<StreamEvent> = AnthropicProvider::new(cfg)
            .unwrap()
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Error(_))
            || matches!(e, StreamEvent::ReasoningSignature { opaque, .. } if opaque == "sig-stale")
            || matches!(e, StreamEvent::ResponseId(id) if id == "msg_drop")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ResponseId(id) if id == "msg_ok"))
                && events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "recovered"))
        );
        let _ = handle.join();
    }

    #[tokio::test]
    async fn premature_eof_with_usable_text_preserves_done() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_http_request(&mut s);
            let body = format!(
                "{}{}",
                line(
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                ),
                line(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial answer"}}),
                ),
            );
            s.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .unwrap();
            s.flush().unwrap();
            drop(s);
        });

        let cfg = AnthropicConfig::new("k", format!("http://127.0.0.1:{port}"), "claude-test");
        let provider = AnthropicProvider::new(cfg).unwrap();
        let stream = provider
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .expect("open should succeed");
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "partial answer")),
            "usable text must be delivered: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Done { .. })),
            "usable text before EOF without message_stop should still emit Done: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "usable text before EOF should not emit Error: {events:?}"
        );
        let _ = handle.join();
    }

    #[test]
    fn third_party_gateway_keeps_signature_only_thinking_separate_from_redacted() {
        let mut dec = AnthropicSseDecoder::new();
        let sse = format!(
            "{}{}{}{}{}{}",
            line(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            line(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"gem-sig-1"}}),
            ),
            line(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            line(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"enc-claude-1"}}),
            ),
            line(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            line("message_stop", json!({"type":"message_stop"})),
        );
        let events = dec.feed(sse.as_bytes());
        let mut blocks = Vec::new();
        for ev in events {
            if let StreamEvent::ReasoningSignature {
                opaque,
                provider,
                id,
            } = ev
            {
                blocks.push(ReasoningBlock {
                    text: String::new(),
                    opaque: Some(opaque),
                    provider: Some(provider),
                    id,
                });
            }
        }
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].provider.as_deref(), Some("anthropic"));
        assert_eq!(blocks[1].provider.as_deref(), Some("anthropic_redacted"));

        let mut a = Message::assistant("answer", vec![]);
        a.reasoning_blocks = blocks;

        // Third-party gateway (!is_official): signature-only `thinking` stays `thinking`,
        // while genuine `redacted_thinking` stays `redacted_thinking`.
        let (_s, out_gw) = super::format_messages(&[Message::user("hi"), a.clone()], true, false);
        let gw_content = out_gw[1]["content"].as_array().unwrap();
        assert_eq!(
            gw_content[0],
            json!({"type":"thinking","thinking":"","signature":"gem-sig-1"})
        );
        assert_eq!(
            gw_content[1],
            json!({"type":"redacted_thinking","data":"enc-claude-1"})
        );

        // Official Anthropic (is_official=true) also keeps both cleanly.
        let (_s, out_off) = super::format_messages(&[Message::user("hi"), a], true, true);
        let off_content = out_off[1]["content"].as_array().unwrap();
        assert_eq!(
            off_content[0],
            json!({"type":"redacted_thinking","data":"gem-sig-1"})
        );
        assert_eq!(
            off_content[1],
            json!({"type":"redacted_thinking","data":"enc-claude-1"})
        );
    }

    #[test]
    fn synthetic_user_cache_breakpoint_aligns_with_unwrapped_frozen_prefix_and_skips_autonomous() {
        let empty_after_unwrap =
            Message::synthetic_user("<strong-reminder>\n</strong-reminder>".to_string());
        let frozen_a = Message::synthetic_user("<available_skills>a</available_skills>");
        let frozen_b = Message::synthetic_user("<memory>b</memory>");
        let autonomous = Message::synthetic_turn("[autonomous turn prompt]");

        // Case 1: empty unwrapped block + 2 frozen blocks + autonomous turn -> breakpoint lands
        // strictly on `frozen_b` (index 1 in merged blocks), not on `autonomous` (index 2).
        let msgs = vec![
            Message::system("persona"),
            empty_after_unwrap,
            frozen_a,
            Message::system("late system"),
            frozen_b,
            autonomous.clone(),
            Message::assistant("reply", vec![]),
        ];
        let body = build_request_body(
            "claude-opus-4-8",
            &msgs,
            &[],
            &ChatOptions::default(),
            &cfg(),
        );
        let first_blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(first_blocks.len(), 3);
        assert!(first_blocks[0].get("cache_control").is_none());
        assert_eq!(first_blocks[1]["text"], "<memory>b</memory>");
        assert_eq!(
            first_blocks[1]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert!(first_blocks[2].get("cache_control").is_none());

        // Case 2: single frozen synthetic block before assistant + later autonomous turn ->
        // Value::String promotes to 1 cached text block so prefix caching still anchors.
        let single_prefix = vec![
            Message::system("persona"),
            Message::synthetic_user("<memory>only</memory>"),
            Message::assistant("a1", vec![]),
            autonomous,
        ];
        let body2 = build_request_body(
            "claude-opus-4-8",
            &single_prefix,
            &[],
            &ChatOptions::default(),
            &cfg(),
        );
        let single_blocks = body2["messages"][0]["content"].as_array().unwrap();
        assert_eq!(single_blocks.len(), 1);
        assert_eq!(single_blocks[0]["text"], "<memory>only</memory>");
        assert_eq!(
            single_blocks[0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn message_cache_breakpoint_skips_empty_trailing_text_message() {
        let mut msgs = vec![
            json!({"role": "user", "content": "real prompt"}),
            json!({"role": "assistant", "content": ""}),
        ];
        super::apply_message_cache_breakpoint(&mut msgs);
        assert_eq!(msgs[1]["content"], json!(""));
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["text"], "real prompt");
        assert_eq!(blocks[0]["cache_control"], json!({ "type": "ephemeral" }));
    }
}
