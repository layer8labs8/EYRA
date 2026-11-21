use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024; // 10 MB rotation cap

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub source: String,
    pub action: String,
    pub ok: bool,
    pub ms: u64,
    pub est_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    pub summary: String,
}

impl AuditEntry {
    pub fn to_compact_line(&self) -> String {
        let status = if self.ok { "OK " } else { "ERR" };
        let summary_short = crate::types::truncate_clean(&self.summary, 110);
        format!(
            "{} [{}] {:<3} {:<12} {:>4}ms ~{:>4}tok | {}",
            self.ts, self.source, status, self.action, self.ms, self.est_tokens, summary_short
        )
    }
}

#[derive(Debug, Clone)]
pub struct AuditLogger {
    pub path: Option<PathBuf>,
}

impl AuditLogger {
    pub fn new(custom_path: Option<PathBuf>, disabled: bool) -> Self {
        if disabled {
            return Self { path: None };
        }
        if std::env::var("EYRA_NO_LOG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return Self { path: None };
        }

        let resolved = custom_path
            .or_else(|| std::env::var("EYRA_LOG_FILE").ok().map(PathBuf::from))
            .unwrap_or_else(default_log_path);

        Self {
            path: Some(resolved),
        }
    }

    pub fn disabled() -> Self {
        Self { path: None }
    }

    // One parameter per JSONL column; a wrapper struct would only add noise at the ~40 call sites.
    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        source: &str,
        action: &str,
        ok: bool,
        ms: u64,
        est_tokens: usize,
        params: Option<Value>,
        summary: &str,
    ) {
        let Some(path) = &self.path else {
            return;
        };

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        rotate_if_needed(path);

        let entry = AuditEntry {
            ts: utc_now_iso8601(),
            source: source.to_string(),
            action: action.to_string(),
            ok,
            ms,
            est_tokens,
            params,
            summary: crate::types::truncate_clean(summary, 240),
        };

        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{}", line);
            }
        }
    }

    pub fn read_tail(&self, n: usize) -> Vec<AuditEntry> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        read_tail_from_path(path, n)
    }

    pub fn clear(&self) -> std::io::Result<()> {
        if let Some(path) = &self.path {
            if path.exists() {
                fs::write(path, b"")?;
            }
        }
        Ok(())
    }
}

pub fn default_log_path() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        if !local_app_data.trim().is_empty() {
            return PathBuf::from(local_app_data)
                .join("eyra")
                .join("audit.jsonl");
        }
    }
    std::env::temp_dir().join("eyra").join("audit.jsonl")
}

pub fn sanitize_params(action: &str, params: &Value, is_password_target: bool) -> Value {
    let mut cloned = params.clone();
    if let Some(obj) = cloned.as_object_mut() {
        if action == "type_text" || action == "type" {
            if is_password_target {
                obj.insert("text".to_string(), Value::String("[REDACTED]".to_string()));
            } else if let Some(s) = obj.get("text").and_then(Value::as_str) {
                if s.chars().count() > 80 {
                    obj.insert(
                        "text".to_string(),
                        Value::String(crate::types::truncate_clean(s, 80)),
                    );
                }
            }
        }
    }
    cloned
}

pub fn read_tail_from_path(path: &Path, n: usize) -> Vec<AuditEntry> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut entries: Vec<AuditEntry> = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<AuditEntry>(line.trim()).ok())
        .collect();

    if entries.len() > n {
        entries.drain(0..(entries.len() - n));
    }
    entries
}

fn rotate_if_needed(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() >= MAX_LOG_BYTES {
            let mut backup = path.as_os_str().to_os_string();
            backup.push(".1");
            let backup_path = PathBuf::from(backup);
            let _ = fs::remove_file(&backup_path);
            let _ = fs::rename(path, backup_path);
        }
    }
}

/// Formats current UTC timestamp as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`) without extra crates.
pub fn utc_now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_secs_iso8601(secs)
}

fn format_unix_secs_iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, m, d, hour, minute, second
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso8601_epoch_and_known_date() {
        assert_eq!(format_unix_secs_iso8601(0), "1970-01-01T00:00:00Z");
        // 2026-01-01T00:00:00Z = 1767225600
        assert_eq!(
            format_unix_secs_iso8601(1_767_225_600),
            "2026-01-01T00:00:00Z"
        );
    }

    #[test]
    fn logger_writes_reads_and_redacts_passwords() {
        let tmp =
            std::env::temp_dir().join(format!("eyra_audit_test_{}.jsonl", std::process::id()));
        let _ = fs::remove_file(&tmp);

        let logger = AuditLogger::new(Some(tmp.clone()), false);
        let safe_params =
            sanitize_params("type_text", &json!({"id": 7, "text": "my-secret"}), true);
        assert_eq!(safe_params["text"], "[REDACTED]");

        logger.log(
            "mcp",
            "type_text",
            true,
            12,
            20,
            Some(safe_params),
            "Typed 9 chars into #7",
        );
        logger.log("cli", "click", false, 5, 10, None, "Blocked in read-only");

        let rows = logger.read_tail(10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "type_text");
        assert_eq!(rows[0].params.as_ref().unwrap()["text"], "[REDACTED]");
        assert!(!rows[1].ok);

        logger.clear().unwrap();
        assert!(logger.read_tail(10).is_empty());
        let _ = fs::remove_file(&tmp);
    }
}
