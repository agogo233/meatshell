//! AI assistant chat panel.
//!
//! A dockable chat panel that talks to any OpenAI-compatible
//! `POST {base_url}/chat/completions` endpoint with SSE streaming. The module
//! owns three things:
//!   * `redact_sensitive_text` — scrubs obvious credentials out of everything
//!     the user sends (terminal selections frequently contain passwords or
//!     tokens); the UI shows the redacted text so users see exactly what a
//!     third-party API receives.
//!   * The streaming request loop — a detached OS thread reads the SSE body
//!     and hops deltas back to the UI thread in ~120ms batches.
//!   * `wire_ai_callbacks` — the AppWindow callback wiring (send / stop /
//!     clear / model discovery / settings persistence / send-selection).
//!
//! Conversation history lives only in the UI model (never written to disk).
//! The API key is persisted through the regular config store and encrypted at
//! rest like the WebDAV password. Message content is never logged.

use std::cell::RefCell;
use std::io::{BufRead, Read};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::*;
use crate::automation::{call as automation_call, Frontend};
use crate::i18n::t;
use crate::resource::TabStatuses;

use regex::Regex;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

/// Assistant messages sent as context with each request (most recent N).
const AI_HISTORY_MESSAGES: usize = 20;
/// Coalesce SSE deltas for at most this long before hopping to the UI thread.
const AI_FLUSH_INTERVAL: Duration = Duration::from_millis(120);
/// Hard cap on accumulated reply bytes so a runaway generation cannot fill
/// memory (the stream is cut and a note is shown).
const AI_STREAM_CAP_BYTES: usize = 1024 * 1024;
const AI_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Wall-clock read timeout per chunk: long generations keep producing data so
/// the timer only fires when the stream stalls.
const AI_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard wall-clock cap for one generation. The read timeout above only bounds
/// a stall; a server that drips one byte at a time would never trip it.
const AI_STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
const AI_MODELS_READ_TIMEOUT: Duration = Duration::from_secs(15);

// ── Agent mode (chat panel → gated SSH/SFTP tools) ───────────────────────

/// Hard wall-clock budget for one agent turn (streaming + tool execution).
const AI_AGENT_DEADLINE: Duration = Duration::from_secs(600);
/// Cap on one tool result handed back to the model. Tools already bound their
/// own raw output; this keeps the chat context small.
const AI_TOOL_RESULT_CAP_BYTES: usize = 64 * 1024;
/// How long the user has to answer an approval request before it counts as a
/// denial.
const AI_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Tools the chat panel may call. `import_sessions` is deliberately not
/// offered (config import stays a CLI / MCP-server operation).
const AI_AGENT_TOOLS: &[&str] = &[
    "list_sessions",
    "get_session",
    "run_command",
    "list_remote_files",
    "read_remote_text_file",
    "upload_file",
    "download_file",
];

const AGENT_SUFFIX_ZH: &str = "\n\n你还可以调用工具直接操作用户的服务器：查询会话、执行命令、浏览/读取远端文件、上传/下载。规则：先简述要做什么，再调用工具；调用会话相关工具前，先用 list_sessions 查询会话，把返回的 id 作为 session_id 传入；一步一步来，根据结果调整；执行命令、上传、下载等改动性操作会先请求用户确认，被拒绝时不要重试同一操作；绝不编造工具结果。";
const AGENT_SUFFIX_EN: &str = "\n\nYou can also call tools to operate on the user's servers: list sessions, run commands, browse/read remote files, upload/download. Rules: briefly say what you will do, then call the tool; before any session-scoped tool, list sessions with list_sessions and pass one returned id as session_id; work step by step and adapt to results; state-changing calls (run_command, upload_file, download_file) ask the user for approval first — if denied, do not retry the same operation; never fabricate tool results.";

const SYSTEM_PROMPT_ZH: &str = "你是一个 SSH 终端客户端里的 AI 助手。用户可能粘贴终端输出或命令行内容。请用简洁的 Markdown 回答：先给结论，再给解释；涉及命令时给出可直接复制的命令。";
const SYSTEM_PROMPT_EN: &str = "You are an AI assistant inside an SSH terminal client. Users may paste terminal output or shell commands. Answer concisely in Markdown: conclusion first, then explanation; when commands are involved, provide copy-pasteable commands.";

// ── Sensitive-data redaction ─────────────────────────────────────────────

fn redaction_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: std::sync::OnceLock<Vec<(Regex, &'static str)>> =
        std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // PEM private-key blocks (spanning lines).
            (
                Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
                "[REDACTED_PRIVATE_KEY]",
            ),
            // Authorization headers.
            (
                Regex::new(r"(?i)Authorization:\s*Bearer\s+[A-Za-z0-9._\-]+").unwrap(),
                "Authorization: Bearer [REDACTED]",
            ),
            // password=… / token=… / api_key: … style assignments.
            (
                Regex::new(r"(?i)(password|passwd|pwd)\s*[:=]\s*[^\s;&|]+").unwrap(),
                "$1=[REDACTED]",
            ),
            (
                Regex::new(r"(?i)(token|api[_-]?key|secret[_-]?key|access[_-]?key)\s*[:=]\s*[^\s;&|]+")
                    .unwrap(),
                "$1=[REDACTED]",
            ),
            // AWS access key ids.
            (
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                "[REDACTED_AWS_ACCESS_KEY]",
            ),
            // Database URLs with embedded credentials.
            (
                Regex::new(r"(?i)(postgres|mysql|mongodb)://[^@\s]+@").unwrap(),
                "$1://[REDACTED]@",
            ),
            // Proxy URLs with embedded credentials (socks5://user:pass@host:port).
            (
                Regex::new(r"(?i)(socks5h|socks5|socks|https?)://[^@\s]+@").unwrap(),
                "$1://[REDACTED]@",
            ),
        ]
    })
}

/// Replace obvious credentials in `input` with `[REDACTED]` placeholders.
/// Applied to every user message before it is sent or displayed, so what the
/// user sees in the chat is exactly what leaves the machine.
pub(super) fn redact_sensitive_text(input: &str) -> String {
    let mut output = input.to_string();
    for (pattern, replacement) in redaction_patterns() {
        output = pattern.replace_all(&output, *replacement).to_string();
    }
    output
}

/// Terminal context for the agent: the live screen text, redacted and
/// truncated to the last `max_lines` lines. Returns (context, total_lines) so
/// the model can tell when it is looking at a truncated tail.
pub(super) fn terminal_context(
    bufs: &TermBuffers,
    tab_id: &str,
    max_lines: usize,
) -> (String, usize) {
    let Some(handle) = term_buf(bufs, tab_id) else {
        return (String::new(), 0);
    };
    let buf = lock_or_recover(&handle);
    let text = buf.parser.screen().contents().to_string();
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = total.saturating_sub(max_lines);
    let mut out = String::new();
    if start > 0 {
        out.push_str(&t(
            "（终端输出过长，仅保留最后 ",
            "(terminal output truncated; showing the last ",
        ));
        out.push_str(&max_lines.to_string());
        out.push_str(&t(" 行）\n", " lines)\n"));
    }
    for line in &lines[start..] {
        out.push_str(&redact_sensitive_text(line));
        out.push('\n');
    }
    (out, total)
}

// ── URL / agent helpers ──────────────────────────────────────────────────

fn chat_completions_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

fn models_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/chat/completions").unwrap_or(base);
    format!("{base}/models")
}

fn ai_agent(read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(AI_CONNECT_TIMEOUT)
        .timeout_read(read_timeout)
        .build()
}

/// Keep an error string short enough to render in one chat bubble.
fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

// ── UI model helpers (UI thread only) ────────────────────────────────────

/// Run `f` with the AI conversation model. The `ModelRc` returned by
/// `get_ai_messages` is owned here (it cannot lend references out), so the
/// downcast and the access both happen inside this scope.
fn with_ai_model<R>(
    window: &AppWindow,
    f: impl FnOnce(&VecModel<AiMessage>) -> R,
) -> Option<R> {
    let rc = window.get_ai_messages();
    let vm = rc.as_any().downcast_ref::<VecModel<AiMessage>>()?;
    Some(f(vm))
}

fn push_ai_message(window: &AppWindow, role: &str, content: &str) {
    with_ai_model(window, |vm| {
        vm.push(AiMessage {
            role: role.into(),
            content: content.into(),
            tool_name: "".into(),
            tool_status: "".into(),
            risk_level: "".into(),
            risk_reason: "".into(),
        });
    });
}

/// Append a streamed delta to the trailing assistant placeholder row.
fn append_ai_delta(window: &AppWindow, delta: &str) {
    with_ai_model(window, |vm| {
        let Some(last) = vm.row_count().checked_sub(1) else {
            return;
        };
        let Some(mut row) = vm.row_data(last) else {
            return;
        };
        if row.role.as_str() != "assistant" {
            return;
        }
        let mut content = row.content.to_string();
        content.push_str(delta);
        row.content = content.into();
        vm.set_row_data(last, row);
    });
}

/// Snapshot the conversation as (role, content) pairs for the next request.
/// Error rows are never sent back to the API, and the window never starts
/// mid-turn (a leading assistant answer without its user question is dropped).
fn ai_history(window: &AppWindow, max_messages: usize) -> Vec<(String, String)> {
    with_ai_model(window, |vm| {
        let mut rows = Vec::new();
        for i in 0..vm.row_count() {
            let Some(row) = vm.row_data(i) else { continue };
            let role = row.role.to_string();
            if role != "user" && role != "assistant" {
                continue;
            }
            let content = row.content.trim().to_string();
            if content.is_empty() {
                continue;
            }
            rows.push((role, content));
        }
        if rows.len() > max_messages {
            rows.drain(..rows.len() - max_messages);
            while rows.first().map(|(r, _)| r.as_str()) == Some("assistant") {
                rows.remove(0);
            }
        }
        rows
    })
    .unwrap_or_default()
}

fn flush_delta(
    weak: &slint::Weak<AppWindow>,
    pending: &mut String,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let delta = std::mem::take(pending);
    if delta.is_empty() {
        return;
    }
    // A late flush from a cancelled stream must never land in the NEXT
    // request's assistant row, so every hop validates its generation.
    let _ = weak.upgrade_in_event_loop(move |w| {
        if generation.load(Ordering::Relaxed) == my_gen {
            append_ai_delta(&w, &delta);
        }
    });
}

// ── Agent mode: tool schemas, cards, approvals, execution ────────────────

/// One tool call aggregated from a streamed `delta.tool_calls` sequence.
struct AiToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Outcome of one streamed chat request.
enum AiTurnOutcome {
    /// Plain reply finished (already streamed into the assistant row).
    Done,
    /// The model requests tool calls; `content` is any text streamed alongside.
    ToolCalls { calls: Vec<AiToolCall>, content: String },
    /// The user pressed stop (or cleared the conversation).
    Cancelled,
    /// Request / parse failure.
    Error(String),
}

/// Convert the shared MCP tool schemas into the OpenAI `tools` parameter,
/// filtered to the chat-panel allowlist. `None` when nothing may be offered
/// (agent mode off, MCP off, or no schemas).
fn agent_tools_json() -> Option<serde_json::Value> {
    let defs = crate::mcp::tool_definitions();
    let arr = defs.as_array()?;
    let mut tools = Vec::new();
    for tool in arr {
        let Some(name) = tool["name"].as_str() else { continue };
        if !AI_AGENT_TOOLS.contains(&name) {
            continue;
        }
        let mut schema = tool["inputSchema"].clone();
        // `saved_session_id` is an alias for external MCP bridges; the chat
        // panel always sends a plain session_id, so hide it from the model.
        if let Some(props) = schema["properties"].as_object_mut() {
            props.remove("saved_session_id");
        }
        if let Some(required) = schema["required"].as_array_mut() {
            required.retain(|key| key.as_str() != Some("saved_session_id"));
        }
        // The alias is hidden from the model, so `session_id` is simply
        // required again. Restate it (and drop the alias wording) so weak
        // models keep passing the argument instead of dropping it.
        if schema["properties"]["session_id"].is_object() {
            if let Some(required) = schema["required"].as_array_mut() {
                if !required.iter().any(|key| key.as_str() == Some("session_id")) {
                    required.push(serde_json::json!("session_id"));
                }
            } else {
                schema["required"] = serde_json::json!(["session_id"]);
            }
            schema["properties"]["session_id"]["description"] = serde_json::json!(
                "Required. ID of a saved session, as returned by list_sessions."
            );
        }
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": tool["description"].as_str().unwrap_or(""),
                "parameters": schema,
            },
        }));
    }
    (!tools.is_empty()).then(|| serde_json::Value::Array(tools))
}

/// One-line human summary of a tool call for the chat card / approval bar.
fn tool_summary(name: &str, args: &serde_json::Value) -> String {
    let text = match name {
        "run_command" => args["command"].as_str().unwrap_or("").to_string(),
        "list_remote_files" => args["path"].as_str().unwrap_or(".").to_string(),
        "read_remote_text_file" => args["path"].as_str().unwrap_or("").to_string(),
        "upload_file" => format!(
            "{} → {}",
            args["local_path"].as_str().unwrap_or(""),
            args["remote_directory"].as_str().unwrap_or("")
        ),
        "download_file" => format!(
            "{} → {}",
            args["remote_path"].as_str().unwrap_or(""),
            args["local_directory"].as_str().unwrap_or("")
        ),
        "get_session" => args["session_id"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    };
    truncate_str(text.trim(), 160)
}

/// Serialize a tool result for the model, bounded so one verbose tool cannot
/// flood the chat context.
fn truncate_tool_result(value: &serde_json::Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if text.len() <= AI_TOOL_RESULT_CAP_BYTES {
        return text;
    }
    let mut cut = AI_TOOL_RESULT_CAP_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[truncated]", &text[..cut])
}

/// The SSH session behind the currently active terminal tab, if any. Anchors
/// "the current server" in agent prompts and provides the session_id
/// fallback when the model omits one.
fn active_ssh_session_context(
    window: &AppWindow,
    statuses: &TabStatuses,
    store: &ConfigStore,
) -> Option<(String, String)> {
    let tab_id = window.get_active_tab_id().to_string();
    let statuses = lock_or_recover(statuses);
    let session_id = statuses.get(&tab_id)?.session_id.trim().to_string();
    if session_id.is_empty() {
        return None;
    }
    let session = store.get(&session_id)?;
    if session.kind.as_str() != "ssh" {
        return None;
    }
    let label = format!("{} ({}@{})", session.name, session.user, session.host);
    Some((session_id, label))
}

/// Run one gated automation tool in-process. The chat panel uses the exact
/// same permission path as the external MCP server (`Frontend::Mcp`); config
/// import is never allowed from here.
fn execute_automation_tool(
    name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime for AI agent tools")
    });
    let frontend = Frontend::Mcp {
        allow_config_import: false,
    };
    let name = name.to_string();
    let args = args.clone();
    runtime
        .block_on(async move { automation_call(&name, &args, frontend).await })
        .map_err(|err| truncate_str(&err.to_string(), 500))
}

/// Push one tool-call card row into the conversation model. `risk_level` /
/// `risk_reason` drive the risk badge (empty = no badge for read-only calls).
fn push_tool_card(
    weak: &slint::Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    name: &str,
    summary: &str,
    status: &str,
    risk_level: &str,
    risk_reason: &str,
) {
    let name = name.to_string();
    let summary = summary.to_string();
    let status = status.to_string();
    let risk_level = risk_level.to_string();
    let risk_reason = risk_reason.to_string();
    let _ = weak.upgrade_in_event_loop(move |w| {
        if generation.load(Ordering::Relaxed) != my_gen {
            return;
        }
        with_ai_model(&w, |vm| {
            vm.push(AiMessage {
                role: "tool".into(),
                content: summary.into(),
                tool_name: name.into(),
                tool_status: status.into(),
                risk_level: risk_level.into(),
                risk_reason: risk_reason.into(),
            });
        });
    });
}

/// Update the most recent tool card (running → ok / error / denied). An empty
/// `note` keeps the current detail text.
fn set_last_tool_card(
    weak: &slint::Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    status: &str,
    note: &str,
) {
    let status = status.to_string();
    let note = note.to_string();
    let _ = weak.upgrade_in_event_loop(move |w| {
        if generation.load(Ordering::Relaxed) != my_gen {
            return;
        }
        with_ai_model(&w, |vm| {
            for i in (0..vm.row_count()).rev() {
                let Some(mut row) = vm.row_data(i) else { continue };
                if row.role.as_str() == "tool" {
                    row.tool_status = status.into();
                    if !note.is_empty() {
                        row.content = note.into();
                    }
                    vm.set_row_data(i, row);
                    break;
                }
            }
        });
    });
}

/// Remove a trailing blank assistant placeholder (e.g. the turn ended after a
/// tool round without any closing text).
fn drop_trailing_blank_assistant(
    weak: &slint::Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let _ = weak.upgrade_in_event_loop(move |w| {
        if generation.load(Ordering::Relaxed) != my_gen {
            return;
        }
        with_ai_model(&w, |vm| {
            let Some(last) = vm.row_count().checked_sub(1) else {
                return;
            };
            let blank = vm
                .row_data(last)
                .map(|r| r.role.as_str() == "assistant" && r.content.is_empty())
                .unwrap_or(false);
            if blank {
                vm.remove(last);
            }
        });
    });
}

/// Push a fresh empty assistant row so the next streamed segment has a
/// placeholder to land in (the trailing row is a tool card after a round).
fn push_assistant_placeholder(
    weak: &slint::Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let _ = weak.upgrade_in_event_loop(move |w| {
        if generation.load(Ordering::Relaxed) != my_gen {
            return;
        }
        with_ai_model(&w, |vm| {
            vm.push(AiMessage {
                role: "assistant".into(),
                content: "".into(),
                tool_name: "".into(),
                tool_status: "".into(),
                risk_level: "".into(),
                risk_reason: "".into(),
            });
        });
    });
}

/// What the user decided about a gated tool call.
#[derive(Debug, PartialEq, Eq)]
enum ApprovalDecision {
    Approved,
    /// Approved with "remember for this session" ticked: the exact call is
    /// auto-approved for the rest of the conversation.
    ApprovedRemembered,
    Denied,
    /// Stop / clear pressed while waiting: abort the whole turn.
    Cancelled,
}

/// Ask the user to approve one tool call. Blocks the worker thread until the
/// user answers, the timer runs out (denial), or stop/clear is pressed
/// (cancellation). The channel sender is parked in `slot` on the UI thread;
/// the decision callback (and stop/clear) pop it to wake this worker.
fn request_approval(
    weak: &slint::Weak<AppWindow>,
    slot: &Arc<Mutex<Option<mpsc::Sender<(bool, bool)>>>>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    summary: &str,
    risk_line: &str,
    cancel: &Arc<AtomicBool>,
) -> ApprovalDecision {
    let (tx, rx) = mpsc::channel::<(bool, bool)>();
    {
        let slot = slot.clone();
        let generation = generation.clone();
        let summary = summary.to_string();
        let risk_line = risk_line.to_string();
        let _ = weak.upgrade_in_event_loop(move |w| {
            if generation.load(Ordering::Relaxed) != my_gen {
                return;
            }
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            w.set_ai_pending_tool_summary(summary.into());
            w.set_ai_pending_tool_risk(risk_line.into());
            w.set_ai_awaiting_approval(true);
        });
    }
    let deadline = Instant::now() + AI_APPROVAL_TIMEOUT;
    let decision = loop {
        if cancel.load(Ordering::Relaxed) {
            break ApprovalDecision::Cancelled;
        }
        if Instant::now() >= deadline {
            break ApprovalDecision::Denied;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok((true, remember)) => break if remember {
                ApprovalDecision::ApprovedRemembered
            } else {
                ApprovalDecision::Approved
            },
            Ok((false, _)) => break ApprovalDecision::Denied,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            // The decision / stop / clear callback consumed the sender.
            Err(mpsc::RecvTimeoutError::Disconnected) => break ApprovalDecision::Denied,
        }
    };
    {
        let slot = slot.clone();
        let generation = generation.clone();
        let _ = weak.upgrade_in_event_loop(move |w| {
            if generation.load(Ordering::Relaxed) != my_gen {
                return;
            }
            slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            w.set_ai_awaiting_approval(false);
        });
    }
    decision
}

/// Append the endpoint-compatibility hint to a tool-related request error.
fn with_tools_hint(err: String) -> String {
    let lower = err.to_lowercase();
    if lower.contains("tool") || lower.contains("function") {
        format!(
            "{err}\n{}",
            t(
                "提示：当前模型或接口可能不支持工具调用，可在设置中关闭「AI 工具」后重试。",
                "Hint: the model or endpoint may not support tool calling; turn off AI tools in Settings and retry.",
            )
        )
    } else {
        err
    }
}

// ── Streaming request (worker thread) ────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn stream_chat(
    weak: slint::Weak<AppWindow>,
    base_url: String,
    api_key: String,
    model: String,
    history: Vec<(String, String)>,
    user_text: String,
    system_suffix: String,
    cancel: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let mut system_prompt = t(SYSTEM_PROMPT_ZH, SYSTEM_PROMPT_EN).to_string();
    system_prompt.push_str(&system_suffix);
    let mut messages = vec![serde_json::json!({
        "role": "system",
        "content": system_prompt,
    })];
    for (role, content) in history {
        messages.push(serde_json::json!({ "role": role, "content": content }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": user_text }));
    let outcome = post_chat_stream(
        &weak, &base_url, &api_key, &model, &messages, None, &cancel, &generation, my_gen,
    );
    let outcome = match outcome {
        AiTurnOutcome::Error(err) => AiTurnOutcome::Error(with_tools_hint(err)),
        other => other,
    };
    if matches!(outcome, AiTurnOutcome::Done) {
        drop_trailing_blank_assistant(&weak, generation.clone(), my_gen);
    }
    finish_stream(
        &weak,
        match outcome {
            AiTurnOutcome::Error(err) => Some(err),
            _ => None,
        },
        generation,
        my_gen,
    );
}

/// One streamed `/chat/completions` request. Content deltas stream into the
/// trailing assistant row; `delta.tool_calls` fragments are aggregated by
/// index (id / name arrive on the first fragment, arguments accumulate).
#[allow(clippy::too_many_arguments)]
fn post_chat_stream(
    weak: &slint::Weak<AppWindow>,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    cancel: &Arc<AtomicBool>,
    generation: &Arc<AtomicU64>,
    my_gen: u64,
) -> AiTurnOutcome {
    let mut body = serde_json::json!({
        "model": model,
        "stream": true,
        "messages": messages,
    });
    if let Some(tools) = tools {
        body["tools"] = tools.clone();
        body["tool_choice"] = serde_json::json!("auto");
    }
    let body = body.to_string();

    let agent = ai_agent(AI_STREAM_READ_TIMEOUT);
    let url = chat_completions_url(base_url);
    let mut req = agent.post(url.as_str()).set("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }

    let resp = match req.send_string(&body) {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            return AiTurnOutcome::Error(format!("HTTP {code}: {}", response_error(resp)));
        }
        Err(e) => return AiTurnOutcome::Error(truncate_str(&e.to_string(), 200)),
    };

    // Bound the raw stream too, not just the decoded deltas: `lines()` would
    // happily allocate an unbounded single line from a hostile endpoint. The
    // 4x headroom covers SSE framing overhead around the 1 MiB reply cap.
    let reader =
        std::io::BufReader::new(resp.into_reader().take((AI_STREAM_CAP_BYTES * 4) as u64));
    let mut pending = String::new();
    let mut content = String::new();
    let mut total: usize = 0;
    let mut last_flush = Instant::now();
    let started = Instant::now();
    let mut stream_err: Option<String> = None;
    let mut cancelled = false;
    let mut call_ids: Vec<Option<String>> = Vec::new();
    let mut call_names: Vec<Option<String>> = Vec::new();
    let mut call_args: Vec<String> = Vec::new();

    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if started.elapsed() > AI_STREAM_TOTAL_TIMEOUT {
            stream_err = Some(t("生成超时，已中止", "generation timed out; stopped").to_string());
            break;
        }
        let Ok(line) = line else {
            stream_err = Some(t("连接中断", "connection interrupted").to_string());
            break;
        };
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(err_msg) = v["error"]["message"].as_str() {
            stream_err = Some(err_msg.to_string());
            break;
        }
        if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
            if !delta.is_empty() {
                total += delta.len();
                pending.push_str(delta);
                content.push_str(delta);
                if total > AI_STREAM_CAP_BYTES {
                    stream_err =
                        Some(t("回复过长，已截断", "reply too long, truncated").to_string());
                    break;
                }
                if last_flush.elapsed() >= AI_FLUSH_INTERVAL {
                    flush_delta(weak, &mut pending, generation.clone(), my_gen);
                    last_flush = Instant::now();
                }
            }
        }
        if let Some(deltas) = v["choices"][0]["delta"]["tool_calls"].as_array() {
            for delta in deltas {
                let index = delta["index"].as_u64().unwrap_or(0) as usize;
                while call_args.len() <= index {
                    call_ids.push(None);
                    call_names.push(None);
                    call_args.push(String::new());
                }
                if let Some(id) = delta["id"].as_str().filter(|id| !id.is_empty()) {
                    call_ids[index] = Some(id.to_string());
                }
                if let Some(name) = delta["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                    call_names[index] = Some(name.to_string());
                }
                if let Some(args) = delta["function"]["arguments"].as_str() {
                    call_args[index].push_str(args);
                }
            }
        }
    }
    // The final batch always lands, even on cancel / error, so partial text
    // the user already "paid for" is kept visible.
    flush_delta(weak, &mut pending, generation.clone(), my_gen);
    if cancelled {
        return AiTurnOutcome::Cancelled;
    }
    if let Some(err) = stream_err {
        return AiTurnOutcome::Error(err);
    }
    let calls: Vec<AiToolCall> = call_names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let name = name.as_ref()?;
            Some(AiToolCall {
                id: call_ids[index].clone().unwrap_or_else(|| format!("call_{index}")),
                name: name.clone(),
                arguments: call_args[index].clone(),
            })
        })
        .collect();
    if !calls.is_empty() {
        return AiTurnOutcome::ToolCalls { calls, content };
    }
    AiTurnOutcome::Done
}

/// The agent loop: stream a reply; when the model requests tools, run them
/// through the permission-rule table + risk grading and feed the results
/// back. Bounded by a step limit (with a wrap-up summary), a wall-clock
/// deadline, and a consecutive-failure breaker.
#[allow(clippy::too_many_arguments)]
fn run_agent_turn(
    weak: slint::Weak<AppWindow>,
    base_url: String,
    api_key: String,
    model: String,
    history: Vec<(String, String)>,
    user_text: String,
    system_suffix: String,
    tools: Option<serde_json::Value>,
    policy: AgentPolicy,
    default_session_id: Option<String>,
    approval_slot: Arc<Mutex<Option<mpsc::Sender<(bool, bool)>>>>,
    cancel: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let mut system_prompt = t(SYSTEM_PROMPT_ZH, SYSTEM_PROMPT_EN).to_string();
    system_prompt.push_str(&system_suffix);
    let mut messages = vec![serde_json::json!({
        "role": "system",
        "content": system_prompt,
    })];
    for (role, content) in history {
        messages.push(serde_json::json!({ "role": role, "content": content }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": user_text }));

    let deadline = Instant::now() + AI_AGENT_DEADLINE;
    let mut rounds = 0usize;
    let mut failing: Option<(String, u32)> = None;
    let mut outcome;
    // Approvals the user asked to remember for the rest of this conversation:
    // exact (tool, resource) pairs, dropped when the app restarts.
    let mut remembered: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            outcome = AiTurnOutcome::Cancelled;
            break;
        }
        if Instant::now() > deadline {
            outcome = AiTurnOutcome::Error(t(
                "AI 工具操作总时长超限，已停止",
                "AI tool time budget exceeded; stopped",
            ).to_string());
            break;
        }
        outcome = match post_chat_stream(
            &weak, &base_url, &api_key, &model, &messages, tools.as_ref(), &cancel,
            &generation, my_gen,
        ) {
            AiTurnOutcome::Error(err) => AiTurnOutcome::Error(with_tools_hint(err)),
            other => other,
        };
        let AiTurnOutcome::ToolCalls { calls, content } = outcome else {
            break;
        };

        rounds += 1;
        if rounds > policy.max_steps {
            // Wrap-up summary instead of a hard error (oli-style): one plain
            // completion lets the model report what it achieved so far. The
            // wall-clock deadline still bounds this final request.
            push_assistant_placeholder(&weak, generation.clone(), my_gen);
            messages.push(serde_json::json!({
                "role": "user",
                "content": t(
                    "（系统提示）工具调用步数已达上限，请基于以上结果给出最终总结，不要继续请求调用工具。",
                    "(system) the tool-call step limit is reached; summarise the progress and results now, do not request more tool calls.",
                ),
            }));
            let _ = post_chat_stream(
                &weak, &base_url, &api_key, &model, &messages, None, &cancel, &generation, my_gen,
            );
            outcome = AiTurnOutcome::Done;
            break;
        }

        // Echo the assistant tool_calls message back exactly as the API
        // expects: streamed text (or null) + the reconstructed call array.
        let mut assistant = serde_json::json!({
            "role": "assistant",
            "content": if content.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(content)
            },
        });
        assistant["tool_calls"] = serde_json::Value::Array(
            calls.iter().map(|call| serde_json::json!({
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": call.arguments },
            })).collect(),
        );
        messages.push(assistant);

        let mut aborted = false;
        for call in &calls {
            if cancel.load(Ordering::Relaxed) {
                aborted = true;
                break;
            }
            // Parse the arguments first: the card summary needs them.
            let parsed = if call.arguments.trim().is_empty() {
                Ok(serde_json::json!({}))
            } else {
                serde_json::from_str::<serde_json::Value>(call.arguments.trim())
                    .map_err(|err| truncate_str(&err.to_string(), 200))
            };
            let mut args = match parsed {
                Ok(value) => value,
                Err(err) => {
                    let streak = match &failing {
                        Some((name, count)) if name == &call.name => count + 1,
                        _ => 1,
                    };
                    failing = Some((call.name.clone(), streak));
                    let note = format!("{}: {}", t("参数解析失败", "invalid tool arguments"), err);
                    push_tool_card(&weak, generation.clone(), my_gen, &call.name, &note, "error", "", "");
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "content": serde_json::to_string(&serde_json::json!({ "error": note }))
                            .unwrap_or_else(|_| "{\"error\":\"invalid tool arguments\"}".to_string()),
                    }));
                    continue;
                }
            };
            // Fill in the active tab's session when the model omitted it.
            if call.name != "list_sessions"
                && args["session_id"].as_str().map_or(true, |s| s.is_empty())
            {
                if let Some(session_id) = &default_session_id {
                    if let Some(obj) = args.as_object_mut() {
                        obj.insert("session_id".into(), serde_json::json!(session_id));
                    }
                }
            }

            let summary = tool_summary(&call.name, &args);
            let resource = rule_resource(&call.name, &args);
            let assessment = assess(&call.name, &args);
            let hit = evaluate_rules(&policy.rules, &call.name, &resource);
            let risk_reason = assessment.detail();
            push_tool_card(
                &weak,
                generation.clone(),
                my_gen,
                &call.name,
                &summary,
                "running",
                assessment.level.as_str(),
                &risk_reason,
            );
            let mut audit = AiAuditEntry {
                action: call.name.clone(),
                command: if resource.is_empty() { summary.clone() } else { resource.clone() },
                risk_level: assessment.level.as_str(),
                hit_rule: hit.as_ref().map(|(_, label)| label.clone()).unwrap_or_default(),
                approval: "none",
                executed: false,
                blocked: false,
                success: false,
                duration_ms: 0,
                session_id: args["session_id"].as_str().unwrap_or("").to_string(),
                error: String::new(),
            };

            // Gate order: deny rule → Critical floor (even an allow rule
            // cannot wave it through) → allow rule → ask rule → remembered
            // approval → risk grading (confirm_each / smart / auto).
            enum Gate {
                Reject,
                Ask,
                Run(&'static str),
            }
            let gate = if hit.as_ref().map(|(action, _)| *action) == Some(RuleAction::Deny) {
                Gate::Reject
            } else if assessment.level == RiskLevel::Critical {
                Gate::Ask
            } else if hit.as_ref().map(|(action, _)| *action) == Some(RuleAction::Allow) {
                Gate::Run("rule-allow")
            } else if hit.as_ref().map(|(action, _)| *action) == Some(RuleAction::Ask) {
                Gate::Ask
            } else if remembered.contains(&(call.name.clone(), resource.clone())) {
                Gate::Run("remembered")
            } else {
                let mut effective = assessment.level;
                if policy.intent_aware && user_requested(&user_text) {
                    effective = effective.relax_one();
                }
                let auto_run = match policy.mode.as_str() {
                    "auto" => true,
                    "smart" => effective <= policy.auto_max_risk,
                    _ => false,
                };
                if auto_run {
                    Gate::Run("none")
                } else {
                    Gate::Ask
                }
            };

            match gate {
                Gate::Reject => {
                    audit.approval = "rule-deny";
                    audit.blocked = true;
                    audit.error = t("命中禁止规则", "blocked by a permission rule").to_string();
                    set_last_tool_card(
                        &weak,
                        generation.clone(),
                        my_gen,
                        "denied",
                        &t("命中禁止规则，已拒绝执行", "blocked by a permission rule; refused"),
                    );
                    record(audit);
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "content": "{\"error\":\"blocked by a permission rule\"}",
                    }));
                    continue;
                }
                Gate::Ask => {
                    let ask = if summary.is_empty() {
                        call.name.clone()
                    } else {
                        format!("{} · {}", call.name, summary)
                    };
                    match request_approval(
                        &weak,
                        &approval_slot,
                        generation.clone(),
                        my_gen,
                        &ask,
                        &risk_reason,
                        &cancel,
                    ) {
                        ApprovalDecision::Approved => {
                            audit.approval = "approved";
                        }
                        ApprovalDecision::ApprovedRemembered => {
                            audit.approval = "approved";
                            remembered.insert((call.name.clone(), resource.clone()));
                        }
                        ApprovalDecision::Cancelled => {
                            aborted = true;
                            break;
                        }
                        ApprovalDecision::Denied => {
                            audit.approval = "denied";
                            audit.blocked = true;
                            audit.error =
                                t("已拒绝或确认超时", "denied or approval timed out").to_string();
                            set_last_tool_card(
                                &weak,
                                generation.clone(),
                                my_gen,
                                "denied",
                                &t("已拒绝或确认超时", "denied or approval timed out"),
                            );
                            record(audit);
                            messages.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": call.id,
                                "content": "{\"error\":\"denied by user\"}",
                            }));
                            continue;
                        }
                    }
                }
                Gate::Run(how) => {
                    audit.approval = how;
                }
            }

            let started = Instant::now();
            let result = execute_automation_tool(&call.name, &args);
            audit.duration_ms = started.elapsed().as_millis() as u64;
            audit.executed = true;
            match &result {
                Ok(_) => {
                    failing = None;
                    audit.success = true;
                    // Keep the call summary as the card detail on success.
                    set_last_tool_card(&weak, generation.clone(), my_gen, "ok", "");
                }
                Err(err) => {
                    let streak = match &failing {
                        Some((name, count)) if name == &call.name => count + 1,
                        _ => 1,
                    };
                    failing = Some((call.name.clone(), streak));
                    audit.error = err.clone();
                    set_last_tool_card(&weak, generation.clone(), my_gen, "error", err);
                }
            }
            record(audit);
            let tool_content = match result {
                Ok(value) => truncate_tool_result(&value),
                Err(err) => serde_json::to_string(&serde_json::json!({ "error": err }))
                    .unwrap_or_else(|_| "{\"error\":\"tool failed\"}".to_string()),
            };
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": tool_content,
            }));
        }
        if aborted {
            outcome = AiTurnOutcome::Cancelled;
            break;
        }
        // Circuit breaker: the same tool failing three times in a row stops
        // the loop instead of letting the model spin.
        if failing.as_ref().map_or(false, |(_, count)| *count >= 3) {
            outcome = AiTurnOutcome::Error(t(
                "工具连续失败，已停止",
                "tool kept failing; stopped",
            ).to_string());
            break;
        }
        // Fresh placeholder so the next streamed segment has a row to land in.
        push_assistant_placeholder(&weak, generation.clone(), my_gen);
    }

    if matches!(outcome, AiTurnOutcome::Done) {
        drop_trailing_blank_assistant(&weak, generation.clone(), my_gen);
    }
    finish_stream(
        &weak,
        match outcome {
            AiTurnOutcome::Error(err) => Some(err),
            _ => None,
        },
        generation,
        my_gen,
    );
}

/// Clear the busy flag and surface `err` (if any) as an error bubble — but
/// only when this stream is still the current generation. A cancelled stream's
/// finish hop must never clear the busy state of a newer request.
fn finish_stream(
    weak: &slint::Weak<AppWindow>,
    err: Option<String>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let _ = weak.upgrade_in_event_loop(move |w| {
        if generation.load(Ordering::Relaxed) != my_gen {
            return;
        }
        if let Some(err) = err {
            // Drop the empty assistant placeholder so a failed request leaves
            // just the error bubble instead of a blank row.
            with_ai_model(&w, |vm| {
                let last = vm.row_count().checked_sub(1);
                if let Some(i) = last {
                    let is_blank = vm
                        .row_data(i)
                        .map(|r| r.role.as_str() == "assistant" && r.content.is_empty())
                        .unwrap_or(false);
                    if is_blank {
                        vm.remove(i);
                    }
                }
            });
            push_ai_message(&w, "error", &err);
        }
        w.set_ai_busy(false);
    });
}

/// Extract `error.message` (or a truncated raw body) from a non-2xx response.
fn response_error(resp: ureq::Response) -> String {
    let body = resp.into_string().unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string));
    match msg {
        Some(msg) => truncate_str(&msg, 200),
        None => truncate_str(body.trim(), 200),
    }
}

// ── Model discovery (worker thread) ──────────────────────────────────────

fn fetch_models(base_url: &str, api_key: &str) -> Result<Vec<SharedString>, String> {
    let agent = ai_agent(AI_MODELS_READ_TIMEOUT);
    let url = models_url(base_url);
    let mut req = agent.get(url.as_str()).set("Accept", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }
    let resp = match req.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            return Err(format!("HTTP {code}: {}", response_error(resp)));
        }
        Err(e) => return Err(truncate_str(&e.to_string(), 200)),
    };
    let body = resp.into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| t("响应不是有效 JSON", "response is not valid JSON").to_string())?;
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| {
            t(
                "响应格式不是 OpenAI /models 结构",
                "response is not an OpenAI /models payload",
            )
            .to_string()
        })?;
    let mut ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(t("模型列表为空", "model list is empty").to_string());
    }
    Ok(ids.into_iter().map(Into::into).collect())
}

// ── Callback wiring ──────────────────────────────────────────────────────

/// Same-edge squeeze shared by every "open the AI panel" path: at most one
/// docked panel per edge, mirroring the quick-panel behaviour in app.slint.
fn squeeze_same_edge_panels(w: &AppWindow, dock: &str) {
    if w.get_sidebar_dock().as_str() == dock {
        w.set_sidebar_collapsed(true);
    }
    if w.get_welcome_as_sidebar() && w.get_welcome_sidebar_dock().as_str() == dock {
        w.set_welcome_collapsed(true);
    }
    if w.get_quick_panel_open() && w.get_quick_panel_dock().as_str() == dock {
        w.set_quick_panel_collapsed(true);
    }
}

/// Serialize permission rules into the settings-page text format: one
/// `tool | resource | action` per line.
pub(super) fn rules_to_text(rules: &[crate::config::PermissionRuleSer]) -> String {
    rules
        .iter()
        .map(|r| format!("{} | {} | {}", r.tool, r.resource, r.action))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the settings-page rules text back into rules. Malformed lines are
/// skipped; the config store sanitises again on the way in.
fn parse_rules_text(text: &str) -> Vec<crate::config::PermissionRuleSer> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('|').map(str::trim).collect();
            if parts.len() != 3 {
                return None;
            }
            Some(crate::config::PermissionRuleSer {
                tool: parts[0].to_string(),
                resource: parts[1].to_string(),
                action: parts[2].to_string(),
            })
        })
        .collect()
}

/// Push the resolved agent policy onto the window so the settings page shows
/// the stored values (called on window setup and whenever settings reopen).
pub(super) fn push_ai_policy(window: &AppWindow, store: &ConfigStore) {
    window.set_ai_execution_mode(store.ai_command_execution_mode().into());
    window.set_ai_auto_max_risk(store.ai_auto_execute_max_risk().into());
    window.set_ai_max_steps(store.ai_max_agent_steps().to_string().into());
    window.set_ai_context_lines(store.ai_context_line_limit().to_string().into());
    window.set_ai_rules_text(rules_to_text(&store.ai_permission_rules()).into());
    window.set_ai_intent_aware(store.ai_intent_aware_execute());
}

pub(super) fn wire_ai_callbacks(
    window: &AppWindow,
    store: &Rc<RefCell<ConfigStore>>,
    bufs: &TermBuffers,
    tab_statuses: &TabStatuses,
) {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    // Warn about a plain http:// endpoint (cleartext Bearer key) once per
    // window, not on every message.
    let http_warned = Rc::new(std::cell::Cell::new(false));
    // Bumped on every send: late finish/flush hops from a cancelled stream are
    // validated against this counter so they cannot corrupt a newer request.
    let generation = Arc::new(AtomicU64::new(0));
    // Pending tool-approval channel: the agent worker parks the sender here
    // on the UI thread; the decision / stop / clear callbacks pop it. The
    // bool pair is (allowed, remember-for-this-session).
    let approval_slot: Arc<Mutex<Option<mpsc::Sender<(bool, bool)>>>> =
        Arc::new(Mutex::new(None));

    // Settings › AI: persist base URL / key / model.
    {
        let store = store.clone();
        window.on_save_ai_settings(
            move |base_url: SharedString, api_key: SharedString, model: SharedString| {
                let mut s = store.borrow_mut();
                s.set_ai_settings(
                    base_url.to_string(),
                    api_key.to_string(),
                    model.to_string(),
                );
                let _ = s.save();
            },
        );
    }

    // Settings › AI: persist the agent execution policy — mode, smart ceiling,
    // step cap, context lines, permission rules and intent relaxation.
    {
        let store = store.clone();
        window.on_save_ai_agent_policy(
            move |mode: SharedString,
                  max_risk: SharedString,
                  steps: SharedString,
                  lines: SharedString,
                  rules_text: SharedString,
                  intent: bool| {
                let rules = parse_rules_text(&rules_text);
                let mut s = store.borrow_mut();
                s.set_ai_agent_policy(
                    mode.to_string(),
                    max_risk.to_string(),
                    steps.trim().parse().unwrap_or(8),
                    lines.trim().parse().unwrap_or(50),
                    rules,
                    intent,
                );
                let _ = s.save();
            },
        );
    }

    // Settings › AI: show/hide the AI panel toggle. Also mirrors the state back
    // into the checkbox so the X-button close path stays in sync.
    {
        let store = store.clone();
        let weak = window.as_weak();
        window.on_set_ai_panel_open(move |open: bool| {
            if let Some(w) = weak.upgrade() {
                w.set_ai_panel_enabled(open);
            }
            let mut s = store.borrow_mut();
            s.set_ai_panel_open(open);
            // Re-enabling the panel always reveals it expanded, never left stuck
            // in the edge strip.
            if open {
                s.set_ai_panel_collapsed(false);
            }
            let _ = s.save();
        });
    }

    // Dock-drag-end: persist the new dock edge.
    {
        let store = store.clone();
        window.on_set_ai_panel_dock(move |dock: SharedString| {
            let mut s = store.borrow_mut();
            s.set_ai_panel_dock(dock.to_string());
            let _ = s.save();
        });
    }

    // Panel send button: redact, snapshot history, spawn the stream thread.
    {
        let store = store.clone();
        let cancel = cancel_flag.clone();
        let generation = generation.clone();
        let http_warned = http_warned.clone();
        let approval_slot = approval_slot.clone();
        let tab_statuses = tab_statuses.clone();
        let bufs = bufs.clone();
        let weak = window.as_weak();
        window.on_send_ai_message(move |text: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if w.get_ai_busy() {
                return;
            }
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let (base_url, api_key, model) = {
                let s = store.borrow();
                (
                    s.ai_base_url().to_string(),
                    s.ai_api_key().to_string(),
                    s.ai_model().to_string(),
                )
            };
            if base_url.is_empty() || model.is_empty() {
                push_ai_message(
                    &w,
                    "error",
                    t(
                        "请先在 设置 › AI 中填写 API 地址和模型名",
                        "Configure the API base URL and model in Settings › AI first",
                    ),
                );
                // The draft is only cleared after the message is accepted, so
                // a misconfigured send keeps the user's text.
                return;
            }
            // A plain http:// endpoint carries the Bearer key in cleartext;
            // WebDAV refuses this combination outright, so surface it here too
            // (once per window — the setting is the user's to keep).
            if base_url.starts_with("http://") && !api_key.is_empty() && !http_warned.get() {
                http_warned.set(true);
                push_ai_message(
                    &w,
                    "error",
                    t(
                        "AI 地址是明文 http://，API key 会随请求被窃听；建议改用 https://",
                        "the AI endpoint is plain http:// — your API key travels in cleartext; switch to https://",
                    ),
                );
            }
            // Agent mode: offer the gated SSH/SFTP tools only when both the
            // AI-agent switch and MCP are enabled.
            let tools = {
                let s = store.borrow();
                if s.ai_agent_enabled() && s.mcp_enabled() {
                    agent_tools_json()
                } else {
                    None
                }
            };
            let policy = {
                let s = store.borrow();
                AgentPolicy {
                    mode: s.ai_command_execution_mode(),
                    auto_max_risk: RiskLevel::parse(&s.ai_auto_execute_max_risk())
                        .unwrap_or(RiskLevel::Low),
                    max_steps: s.ai_max_agent_steps(),
                    rules: s
                        .ai_permission_rules()
                        .into_iter()
                        .map(|r| PermissionRule {
                            tool: r.tool,
                            resource: r.resource,
                            action: RuleAction::parse(&r.action).unwrap_or(RuleAction::Ask),
                        })
                        .collect(),
                    intent_aware: s.ai_intent_aware_execute(),
                }
            };
            let session_ctx = if tools.is_some() {
                active_ssh_session_context(&w, &tab_statuses, &store.borrow())
            } else {
                None
            };
            let mut system_suffix = String::new();
            if tools.is_some() {
                system_suffix.push_str(t(AGENT_SUFFIX_ZH, AGENT_SUFFIX_EN));
                if let Some((session_id, label)) = &session_ctx {
                    system_suffix.push_str(&format!(
                        "{}{} — session_id=\"{}\"{}",
                        t("\n\n当前打开的会话：", "\n\nCurrently open session: "),
                        label,
                        session_id,
                        t(
                            "（用户说“当前服务器 / 这个会话”时即指它）",
                            " (what the user means by \"the current server/session\")"
                        ),
                    ));
                }
            }
            // Show the redacted text: what the user sees is what is sent.
            let mut user_text = redact_sensitive_text(text);
            // Agent mode: attach the live terminal tail so the model reasons
            // about what is actually on screen instead of guessing.
            if tools.is_some() && session_ctx.is_some() {
                let max_lines = store.borrow().ai_context_line_limit();
                let (ctx, _) = terminal_context(&bufs, w.get_active_tab_id().as_str(), max_lines);
                if !ctx.is_empty() {
                    user_text.push_str(&t(
                        "\n\n[当前终端画面]\n",
                        "\n\n[Current terminal screen]\n",
                    ));
                    user_text.push_str(&ctx);
                }
            }
            let history = ai_history(&w, AI_HISTORY_MESSAGES);
            push_ai_message(&w, "user", &user_text);
            push_ai_message(&w, "assistant", "");
            w.set_ai_busy(true);
            // Clear the draft only now that the message is accepted; a failed
            // validation above leaves the user's text in the input box.
            w.set_ai_draft("".into());
            // FnMut callbacks run once per invocation: everything captured by
            // the spawn closure must be cloned per call.
            cancel.store(false, Ordering::Relaxed);
            let my_gen = generation.fetch_add(1, Ordering::Relaxed) + 1;
            let weak = weak.clone();
            let generation = generation.clone();
            let cancel = cancel.clone();
            let approval_slot = approval_slot.clone();
            std::thread::spawn(move || {
                if let Some(tools) = tools {
                    run_agent_turn(
                        weak,
                        base_url,
                        api_key,
                        model,
                        history,
                        user_text,
                        system_suffix,
                        Some(tools),
                        policy,
                        session_ctx.map(|(session_id, _)| session_id),
                        approval_slot,
                        cancel,
                        generation,
                        my_gen,
                    );
                } else {
                    stream_chat(
                        weak,
                        base_url,
                        api_key,
                        model,
                        history,
                        user_text,
                        system_suffix,
                        cancel,
                        generation,
                        my_gen,
                    );
                }
            });
        });
    }

    // Stop button: flip the flag; the stream loop exits after the current
    // line. A pending tool approval is denied so the worker wakes at once.
    {
        let cancel = cancel_flag.clone();
        let approval_slot = approval_slot.clone();
        window.on_stop_ai_message(move || {
            cancel.store(true, Ordering::Relaxed);
            if let Some(tx) = approval_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send((false, false));
            }
        });
    }

    // Approval bar verdict: wake the waiting agent worker with the answer
    // (allowed + whether to remember the approval for this session).
    {
        let approval_slot = approval_slot.clone();
        window.on_ai_tool_decision(move |allowed: bool, remember: bool| {
            if let Some(tx) = approval_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send((allowed, remember));
            }
        });
    }

    // Clear button: drop the in-memory conversation (and abort a running
    // stream, so no stray error bubble can land in the fresh conversation).
    {
        let weak = window.as_weak();
        let cancel = cancel_flag.clone();
        let generation = generation.clone();
        let approval_slot = approval_slot.clone();
        window.on_clear_ai_messages(move || {
            cancel.store(true, Ordering::Relaxed);
            if let Some(tx) = approval_slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send((false, false));
            }
            // Retire the generation too: the aborted stream's late finish hop
            // is only suppressed while the generation still matches, so
            // without this a straggling error bubble lands in the fresh
            // conversation exactly as the comment above promises it won't.
            generation.fetch_add(1, Ordering::Relaxed);
            let Some(w) = weak.upgrade() else { return };
            // Clear was allowed mid-stream, so the retired stream's finish hop
            // will skip its own busy reset (finish_stream bails on a generation
            // mismatch) — release it here or the next send is rejected forever.
            w.set_ai_busy(false);
            with_ai_model(&w, |vm| vm.set_vec(Vec::new()));
        });
    }

    // Copy a chat message to the clipboard (Slint Text cannot host a text
    // selection, so the panel offers one-click whole-message copy instead).
    // arboard's Linux backend waits for the clipboard owner, so keep it off
    // the UI thread — same pattern as the process-window copy button.
    {
        window.on_copy_ai_message(move |text: SharedString| {
            let text = text.to_string();
            std::thread::spawn(move || super::helpers::clipboard_set_text(text));
        });
    }

    // Settings › AI: discover models via GET {base}/models. Takes the fields
    // as they currently appear in the settings page (not the persisted store),
    // so users can probe an endpoint before hitting Save.
    {
        let weak = window.as_weak();
        window.on_discover_ai_models(
            move |base_url: SharedString, api_key: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                if w.get_ai_discover_state().as_str() == "busy" {
                    return;
                }
                let base_url = base_url.trim().to_string();
                let api_key = api_key.trim().to_string();
                if base_url.is_empty() {
                    w.set_ai_discover_state("error".into());
                    w.set_ai_discover_error(
                        t("请先填写 API 地址", "enter the API base URL first").into(),
                    );
                    return;
                }
                w.set_ai_discover_state("busy".into());
                w.set_ai_discover_error("".into());
                let weak = weak.clone();
                std::thread::spawn(move || {
                    let result = fetch_models(&base_url, &api_key);
                    let _ = weak.upgrade_in_event_loop(move |w| {
                        match result {
                            Ok(models) => {
                                w.set_ai_discovered_models(ModelRc::from(Rc::new(
                                    VecModel::from(models),
                                )));
                                w.set_ai_discover_state("ok".into());
                            }
                            Err(err) => {
                                w.set_ai_discover_state("error".into());
                                w.set_ai_discover_error(err.into());
                            }
                        };
                    });
                });
            },
        );
    }

    // Terminal context menu: send the live selection into the AI draft box and
    // open the panel (never auto-sends).
    {
        let bufs_sel = bufs.clone();
        let store = store.clone();
        let weak = window.as_weak();
        window.on_send_selection_to_ai(move |tab_id: SharedString| {
            let text = term_buf(&bufs_sel, tab_id.as_str())
                .map(|h| {
                    let buf = lock_or_recover(&h);
                    buf.extract_selection_text()
                })
                .unwrap_or_default();
            let text = redact_sensitive_text(text.trim());
            if text.is_empty() {
                tracing::debug!("send-selection-to-ai: empty selection text, ignored");
                return;
            }
            {
                let mut s = store.borrow_mut();
                s.set_ai_panel_open(true);
                s.set_ai_panel_collapsed(false);
                let _ = s.save();
            }
            let Some(w) = weak.upgrade() else { return };
            w.set_ai_panel_open(true);
            w.set_ai_panel_enabled(true);
            // Reveal the panel: a collapsed AI panel must expand for the user to
            // see the draft text land.
            w.set_ai_panel_collapsed(false);
            squeeze_same_edge_panels(&w, w.get_ai_panel_dock().as_str());
            w.set_ai_draft(text.into());
            w.set_ai_draft_revision(w.get_ai_draft_revision() + 1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_values() {
        let raw = "password=secret token:abc Authorization: Bearer abc.def AKIA1234567890ABCDEF";
        let redacted = redact_sensitive_text(raw);
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("abc.def"));
        assert!(!redacted.contains("AKIA1234567890ABCDEF"));
        assert!(redacted.contains("password=[REDACTED]"));
        assert!(redacted.contains("token=[REDACTED]"));
        assert!(redacted.contains("Authorization: Bearer [REDACTED]"));
    }

    #[test]
    fn redacts_private_key_blocks() {
        let raw = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\ndef\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let redacted = redact_sensitive_text(raw);
        assert!(!redacted.contains("abc"));
        assert!(redacted.contains("[REDACTED_PRIVATE_KEY]"));
        assert!(redacted.starts_with("before"));
        assert!(redacted.ends_with("after"));
    }

    #[test]
    fn redacts_db_urls() {
        let redacted = redact_sensitive_text("mysql://root:s3cret@db.internal:3306/app");
        assert!(!redacted.contains("s3cret"));
        assert!(redacted.contains("mysql://[REDACTED]@db.internal:3306/app"));
    }

    #[test]
    fn redacts_proxy_urls_with_userinfo() {
        let redacted = redact_sensitive_text(
            "connecting via socks5://me:p4ss@proxy.corp:1080 and http://svc@relay:8080",
        );
        assert!(!redacted.contains("me:p4ss"));
        assert!(!redacted.contains("svc@"));
        assert!(redacted.contains("socks5://[REDACTED]@proxy.corp:1080"));
        assert!(redacted.contains("http://[REDACTED]@relay:8080"));
    }

    #[test]
    fn leaves_userinfo_free_proxy_urls_alone() {
        let raw = "socks5://proxy.corp:1080";
        assert_eq!(redact_sensitive_text(raw), raw);
    }

    #[test]
    fn leaves_normal_text_untouched() {
        let raw = "df -h 显示 /dev/sda1 已用 80%";
        assert_eq!(redact_sensitive_text(raw), raw);
    }

    #[test]
    fn chat_url_normalization() {
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            models_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn truncate_str_keeps_glyphs_whole() {
        let s = truncate_str("你好世界", 3);
        assert_eq!(s, "你好世…");
    }
}
