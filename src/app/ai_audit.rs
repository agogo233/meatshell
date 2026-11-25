//! AI audit log: every tool call the agent attempts, approvals included, is
//! appended to one local JSON file (capped at 2000 entries, oldest evicted).
//!
//! Privacy: no chat content is ever written. Tool arguments are recorded
//! because they are needed to reconstruct what ran, but command text and
//! error strings go through the same redaction as chat input, so credentials
//! pasted into a command do not land on disk in clear text.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::ai::redact_sensitive_text;

/// Ring size: enough to reconstruct a session's actions, small enough that
/// the file stays a few hundred KiB.
const AI_AUDIT_MAX: usize = 2000;
const AI_AUDIT_FILE: &str = "ai_audit.json";

/// One audit record. `approval` explains how the call was gated:
/// `none` (not gated) / `approved` / `denied` / `timeout` / `rule-allow` /
/// `rule-deny` / `rule-ask` / `remembered`.
pub(crate) struct AiAuditEntry {
    pub action: String,
    pub command: String,
    pub risk_level: &'static str,
    pub hit_rule: String,
    pub approval: &'static str,
    pub executed: bool,
    pub blocked: bool,
    pub success: bool,
    pub duration_ms: u64,
    pub session_id: String,
    pub error: String,
}

impl AiAuditEntry {
    fn to_json(&self, ts: u64) -> Value {
        json!({
            "ts": ts,
            "action": self.action,
            "command": redact_sensitive_text(&self.command),
            "risk_level": self.risk_level,
            "hit_rule": redact_sensitive_text(&self.hit_rule),
            "approval": self.approval,
            "executed": self.executed,
            "blocked": self.blocked,
            "success": self.success,
            "duration_ms": self.duration_ms,
            "session_id": self.session_id,
            "error": redact_sensitive_text(&self.error),
        })
    }
}

fn audit_path() -> PathBuf {
    crate::config::data_dir().join(AI_AUDIT_FILE)
}

static LOGS: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();

fn logs() -> &'static Mutex<Vec<Value>> {
    LOGS.get_or_init(|| Mutex::new(load_from_disk()))
}

fn load_from_disk() -> Vec<Value> {
    let Ok(body) = fs::read_to_string(audit_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["logs"].as_array().cloned())
        .unwrap_or_default()
}

fn write_to_disk(logs: &[Value]) {
    let path = audit_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = serde_json::to_string_pretty(&json!({ "logs": logs })).unwrap_or_default();
    if let Err(err) = fs::write(path, body) {
        tracing::debug!("ai audit write failed: {err}");
    }
}

/// Append one record, evicting the oldest past the cap, then persist. All
/// failures are swallowed: auditing must never break the agent loop.
pub(crate) fn record(entry: AiAuditEntry) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let value = entry.to_json(ts);
    let mut guard = logs().lock().unwrap_or_else(|e| e.into_inner());
    push_and_cap(&mut guard, value);
    write_to_disk(&guard);
}

/// Ring-buffer insert, extracted for tests.
fn push_and_cap(logs: &mut Vec<Value>, value: Value) {
    logs.push(value);
    if logs.len() > AI_AUDIT_MAX {
        let overflow = logs.len() - AI_AUDIT_MAX;
        logs.drain(..overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_keeps_the_newest() {
        let mut logs: Vec<Value> = Vec::new();
        for i in 0..(AI_AUDIT_MAX + 50) {
            push_and_cap(&mut logs, json!({ "i": i }));
        }
        assert_eq!(logs.len(), AI_AUDIT_MAX);
        assert_eq!(logs[0]["i"], 50);
        assert_eq!(logs.last().unwrap()["i"], AI_AUDIT_MAX + 49);
    }

    #[test]
    fn entries_are_redacted_before_persisting() {
        let entry = AiAuditEntry {
            action: "run_command".into(),
            command: "curl -H \"Authorization: Bearer abc.def\" https://x".into(),
            risk_level: "medium",
            hit_rule: String::new(),
            approval: "approved",
            executed: true,
            blocked: false,
            success: true,
            duration_ms: 12,
            session_id: "s1".into(),
            error: "password=hunter2".into(),
        };
        let v = entry.to_json(1);
        let text = v.to_string();
        assert!(!text.contains("abc.def"));
        assert!(!text.contains("hunter2"));
        assert!(text.contains("[REDACTED]"));
    }
}
