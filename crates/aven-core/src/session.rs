//! Structured per-invocation session records for `AVEN_SESSION_LOG`.
//!
//! When the environment variable is set to a non-empty path, each `aven`
//! process appends one JSONL line describing the invocation. The harness
//! (`aven-bench`) uses this to reconstruct repair sequences without scraping
//! stderr across parallel runs.
//!
//! Schema design notes:
//! - `schema_version` bumps on any breaking field/shape change.
//! - `timestamp` is Unix epoch milliseconds (UTC).
//! - `diagnostics` reuse [`Diagnostic`]'s serde shape (same as
//!   `aven check --format json`).
//! - `timings` reuse the phase-timing field set from `check --timings`
//!   (`parse` / `name` / `check` / `total`, milliseconds as f64).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Diagnostic;
use crate::sha256::sha256_hex;

/// Current session-record schema version. Bump on breaking shape changes.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// Environment variable: path of the JSONL session log file.
pub const SESSION_LOG_ENV: &str = "AVEN_SESSION_LOG";

/// Environment variable: opaque harness tag copied into each record.
pub const SESSION_TAG_ENV: &str = "AVEN_SESSION_TAG";

/// One JSONL record per `aven` invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Integer schema version; bump on breaking changes.
    pub schema_version: u32,
    /// Unix epoch milliseconds (UTC).
    pub timestamp: u64,
    /// Value of `AVEN_SESSION_TAG` when set; otherwise null.
    pub tag: Option<String>,
    /// Binary / workspace package version (`CARGO_PKG_VERSION`).
    pub aven_version: String,
    /// `option_env!("AVEN_BUILD_COMMIT")` from the binary; null when unset.
    pub aven_build_commit: Option<String>,
    /// Subcommand name: `check`, `run`, `test`, `fmt`, …
    pub subcommand: String,
    /// Process arguments as received (after shebang normalization, if any).
    pub argv: Vec<String>,
    /// Entry source path when the subcommand takes one.
    pub entry_path: Option<String>,
    /// Lowercase hex SHA-256 of the entry file's source text.
    pub entry_source_sha256: Option<String>,
    /// Phase timings in milliseconds, same field set as `check --timings`.
    pub timings: Option<SessionTimings>,
    /// Every diagnostic produced, in the existing `check --format json` shape.
    pub diagnostics: Vec<Diagnostic>,
    /// Process exit code actually used.
    pub exit_code: i32,
    /// Optional subcommand-specific summary (e.g. test counts).
    pub summary: Option<SessionSummary>,
}

/// Phase timings in milliseconds — mirrors `timingsMs` from `check --format json --timings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTimings {
    pub parse: f64,
    pub name: Option<f64>,
    pub check: Option<f64>,
    pub total: f64,
}

/// Subcommand-specific summary payload.
///
/// Currently only `aven test` populates this; other subcommands leave
/// `summary` null. Extend with new variants (and bump `schema_version` if the
/// shape of an existing variant changes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionSummary {
    Test {
        total: usize,
        passed: usize,
        failed: usize,
        errored: usize,
    },
}

/// Inputs for [`SessionRecord::from_parts`], excluding auto-filled
/// `schema_version` and `timestamp`.
#[derive(Debug, Clone)]
pub struct SessionRecordParts {
    pub tag: Option<String>,
    pub aven_version: String,
    pub aven_build_commit: Option<String>,
    pub subcommand: String,
    pub argv: Vec<String>,
    pub entry_path: Option<String>,
    /// Entry source text; hashed into `entry_source_sha256` when present.
    pub entry_source: Option<String>,
    pub timings: Option<SessionTimings>,
    pub diagnostics: Vec<Diagnostic>,
    pub exit_code: i32,
    pub summary: Option<SessionSummary>,
}

impl SessionRecord {
    /// Build a record with `schema_version` and `timestamp` filled in.
    pub fn from_parts(parts: SessionRecordParts) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            timestamp: unix_time_ms(),
            tag: parts.tag,
            aven_version: parts.aven_version,
            aven_build_commit: parts.aven_build_commit,
            subcommand: parts.subcommand,
            argv: parts.argv,
            entry_path: parts.entry_path,
            entry_source_sha256: parts
                .entry_source
                .as_deref()
                .map(|source| sha256_hex(source.as_bytes())),
            timings: parts.timings,
            diagnostics: parts.diagnostics,
            exit_code: parts.exit_code,
            summary: parts.summary,
        }
    }
}

/// Path from `AVEN_SESSION_LOG` when set to a non-empty value.
pub fn session_log_path_from_env() -> Option<PathBuf> {
    match std::env::var_os(SESSION_LOG_ENV) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Value of `AVEN_SESSION_TAG` when set (including empty string); `None` when unset.
pub fn session_tag_from_env() -> Option<String> {
    std::env::var(SESSION_TAG_ENV).ok()
}

/// Append one complete JSONL line for `record` when `AVEN_SESSION_LOG` is set.
///
/// Failures (bad path, permissions, full disk, serialization) are swallowed:
/// session logging must never change the command's exit code or stdout/stderr.
///
/// Writes use a single `write` of one full line (including the trailing newline)
/// against a file opened with `O_APPEND`. Concurrent appenders therefore rely on
/// `O_APPEND` atomicity for reasonably-sized writes on local filesystems — the
/// harness fans out many `aven` processes against one log file.
pub fn append_session_record_if_enabled(record: &SessionRecord) {
    let Some(path) = session_log_path_from_env() else {
        return;
    };
    let _ = append_session_record(&path, record);
}

/// Append one complete JSONL line to `path`. Returns `Err` on I/O or serialize
/// failure; callers that must not fail the command should use
/// [`append_session_record_if_enabled`] instead.
pub fn append_session_record(path: &Path, record: &SessionRecord) -> Result<(), SessionLogError> {
    let mut line = serde_json::to_string(record).map_err(SessionLogError::Serialize)?;
    line.push('\n');
    // Single write of one complete line under O_APPEND — see module docs.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(SessionLogError::Io)?;
    file.write_all(line.as_bytes())
        .map_err(SessionLogError::Io)?;
    Ok(())
}

/// Errors from an explicit [`append_session_record`] call. Not used as a
/// diagnostic channel — logging failures are swallowed at the CLI boundary.
#[derive(Debug)]
pub enum SessionLogError {
    Io(std::io::Error),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for SessionLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "session log I/O error: {error}"),
            Self::Serialize(error) => write!(f, "session log serialize error: {error}"),
        }
    }
}

impl std::error::Error for SessionLogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

fn unix_time_ms() -> u64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Label, Severity, Span};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sample_record() -> SessionRecord {
        SessionRecord::from_parts(SessionRecordParts {
            tag: Some("task-42-attempt-3".to_owned()),
            aven_version: "0.1.0".to_owned(),
            aven_build_commit: None,
            subcommand: "check".to_owned(),
            argv: vec!["aven".to_owned(), "check".to_owned(), "file.av".to_owned()],
            entry_path: Some("file.av".to_owned()),
            entry_source: Some("value : Missing = value\n".to_owned()),
            timings: Some(SessionTimings {
                parse: 1.5,
                name: Some(0.2),
                check: Some(0.8),
                total: 2.5,
            }),
            diagnostics: vec![
                Diagnostic::error("unknown type name `Missing`")
                    .with_code("type.unknown-name")
                    .with_label(Label::primary(Span::new(8, 15), "unknown type")),
            ],
            exit_code: 1,
            summary: None,
        })
    }

    #[test]
    fn record_round_trips_through_jsonl_line() {
        let record = sample_record();
        let line = serde_json::to_string(&record).expect("serialize");
        let back: SessionRecord = serde_json::from_str(&line).expect("deserialize");
        assert_eq!(back.schema_version, SESSION_SCHEMA_VERSION);
        assert_eq!(back.tag.as_deref(), Some("task-42-attempt-3"));
        assert_eq!(back.subcommand, "check");
        assert_eq!(back.exit_code, 1);
        assert_eq!(back.diagnostics.len(), 1);
        assert_eq!(
            back.diagnostics[0].code.as_deref(),
            Some("type.unknown-name")
        );
        assert_eq!(back.diagnostics[0].severity, Severity::Error);
        assert!(back.entry_source_sha256.is_some());
        assert_eq!(back.timings.as_ref().map(|t| t.total), Some(2.5));
    }

    #[test]
    fn diagnostic_json_shape_matches_check_format_json() {
        let diagnostic = Diagnostic::error("operator `+` is not defined for `Int` and `Text`")
            .with_code("type.invalid-operator-operands")
            .with_label(Label::primary(
                Span::new(8, 15),
                "these operand types do not support this operator",
            ))
            .with_note("`+` accepts two numbers or two Text values");

        let value = serde_json::to_value(&diagnostic).expect("serialize diagnostic");
        assert_eq!(value["severity"], "error");
        assert_eq!(value["code"], "type.invalid-operator-operands");
        assert_eq!(
            value["message"],
            "operator `+` is not defined for `Int` and `Text`"
        );
        assert_eq!(value["labels"][0]["span"]["start"], 8);
        assert_eq!(value["labels"][0]["span"]["end"], 15);
        assert_eq!(
            value["labels"][0]["message"],
            "these operand types do not support this operator"
        );
        assert_eq!(
            value["notes"][0],
            "`+` accepts two numbers or two Text values"
        );
    }

    #[test]
    fn append_writes_one_line_per_record() {
        let path = temp_path("session-append");
        let _ = fs::remove_file(&path);

        let mut first = sample_record();
        first.exit_code = 1;
        append_session_record(&path, &first).expect("first append");

        let mut second = sample_record();
        second.exit_code = 0;
        second.diagnostics.clear();
        append_session_record(&path, &second).expect("second append");

        let contents = fs::read_to_string(&path).expect("read log");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let r0: SessionRecord = serde_json::from_str(lines[0]).expect("line 0");
        let r1: SessionRecord = serde_json::from_str(lines[1]).expect("line 1");
        assert_eq!(r0.exit_code, 1);
        assert_eq!(r1.exit_code, 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_summary_serializes_counts() {
        let record = SessionRecord::from_parts(SessionRecordParts {
            tag: None,
            aven_version: "0.1.0".to_owned(),
            aven_build_commit: Some("abc123".to_owned()),
            subcommand: "test".to_owned(),
            argv: vec!["aven".to_owned(), "test".to_owned(), "suite.av".to_owned()],
            entry_path: Some("suite.av".to_owned()),
            entry_source: Some("suite source".to_owned()),
            timings: None,
            diagnostics: Vec::new(),
            exit_code: 1,
            summary: Some(SessionSummary::Test {
                total: 3,
                passed: 1,
                failed: 1,
                errored: 1,
            }),
        });
        let value = serde_json::to_value(&record).expect("serialize");
        assert_eq!(value["summary"]["total"], 3);
        assert_eq!(value["summary"]["passed"], 1);
        assert_eq!(value["summary"]["failed"], 1);
        assert_eq!(value["summary"]["errored"], 1);
        assert_eq!(value["aven_build_commit"], "abc123");
    }

    fn temp_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "aven-core-session-{label}-{}-{unique}.jsonl",
            std::process::id()
        ))
    }
}
