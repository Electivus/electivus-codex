use crate::LogEntry;
use chrono::DateTime;
use chrono::Utc;

pub(super) const LOG_RETENTION_DAYS: i64 = 10;

pub(super) fn estimated_log_bytes(entry: &LogEntry) -> i64 {
    let feedback_log_body = entry.feedback_log_body.as_ref().or(entry.message.as_ref());
    feedback_log_body.map_or(0, String::len) as i64
        + entry.level.len() as i64
        + entry.target.len() as i64
        + entry.module_path.as_ref().map_or(0, String::len) as i64
        + entry.file.as_ref().map_or(0, String::len) as i64
}

pub(super) fn format_feedback_log_line(
    ts: i64,
    ts_nanos: i64,
    level: &str,
    feedback_log_body: &str,
) -> String {
    let nanos = u32::try_from(ts_nanos).unwrap_or(0);
    let timestamp = match DateTime::<Utc>::from_timestamp(ts, nanos) {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        None => format!("{ts}.{ts_nanos:09}Z"),
    };
    let mut line = format!("{timestamp} {level:>5} {feedback_log_body}");
    if !line.ends_with('\n') {
        line.push('\n');
    }
    line
}
