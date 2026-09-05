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
use std::io::BufRead;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::i18n::t;

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

// ── Streaming request (worker thread) ────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn stream_chat(
    weak: slint::Weak<AppWindow>,
    base_url: String,
    api_key: String,
    model: String,
    history: Vec<(String, String)>,
    user_text: String,
    cancel: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let system_prompt = t(SYSTEM_PROMPT_ZH, SYSTEM_PROMPT_EN);
    let mut messages = vec![serde_json::json!({
        "role": "system",
        "content": system_prompt,
    })];
    for (role, content) in history {
        messages.push(serde_json::json!({ "role": role, "content": content }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": user_text }));
    let body = serde_json::json!({
        "model": model,
        "stream": true,
        "messages": messages,
    })
    .to_string();

    let agent = ai_agent(AI_STREAM_READ_TIMEOUT);
    let url = chat_completions_url(&base_url);
    let mut req = agent.post(url.as_str()).set("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }

    let resp = match req.send_string(&body) {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            finish_stream(
                &weak,
                Some(format!("HTTP {code}: {}", response_error(resp))),
                generation,
                my_gen,
            );
            return;
        }
        Err(e) => {
            finish_stream(&weak, Some(truncate_str(&e.to_string(), 200)), generation, my_gen);
            return;
        }
    };

    let reader = std::io::BufReader::new(resp.into_reader());
    let mut pending = String::new();
    let mut total: usize = 0;
    let mut last_flush = Instant::now();
    let started = Instant::now();
    let mut stream_err: Option<String> = None;

    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
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
        let Some(delta) = v["choices"][0]["delta"]["content"].as_str() else {
            continue;
        };
        if delta.is_empty() {
            continue;
        }
        total += delta.len();
        pending.push_str(delta);
        if total > AI_STREAM_CAP_BYTES {
            stream_err =
                Some(t("回复过长，已截断", "reply too long, truncated").to_string());
            break;
        }
        if last_flush.elapsed() >= AI_FLUSH_INTERVAL {
            flush_delta(&weak, &mut pending, generation.clone(), my_gen);
            last_flush = Instant::now();
        }
    }
    // The final batch always lands, even on cancel / error, so partial text
    // the user already "paid for" is kept visible.
    flush_delta(&weak, &mut pending, generation.clone(), my_gen);
    finish_stream(&weak, stream_err, generation, my_gen);
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

pub(super) fn wire_ai_callbacks(
    window: &AppWindow,
    store: &Rc<RefCell<ConfigStore>>,
    bufs: &TermBuffers,
) {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    // Bumped on every send: late finish/flush hops from a cancelled stream are
    // validated against this counter so they cannot corrupt a newer request.
    let generation = Arc::new(AtomicU64::new(0));

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
            // Show the redacted text: what the user sees is what is sent.
            let user_text = redact_sensitive_text(text);
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
            std::thread::spawn(move || {
                stream_chat(
                    weak,
                    base_url,
                    api_key,
                    model,
                    history,
                    user_text,
                    cancel,
                    generation,
                    my_gen,
                );
            });
        });
    }

    // Stop button: flip the flag; the stream loop exits after the current line.
    {
        let cancel = cancel_flag.clone();
        window.on_stop_ai_message(move || {
            cancel.store(true, Ordering::Relaxed);
        });
    }

    // Clear button: drop the in-memory conversation (and abort a running
    // stream, so no stray error bubble can land in the fresh conversation).
    {
        let weak = window.as_weak();
        let cancel = cancel_flag.clone();
        window.on_clear_ai_messages(move || {
            cancel.store(true, Ordering::Relaxed);
            let Some(w) = weak.upgrade() else { return };
            with_ai_model(&w, |vm| vm.set_vec(Vec::new()));
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
