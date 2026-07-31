pub mod agent_render;
mod builtin;
pub mod codes;
pub mod diagnostic;
pub mod explain;
mod int;
pub mod session;
pub mod sha256;
pub mod source;
pub mod span;

pub use agent_render::{render_agent_diagnostic, render_agent_report};
pub use builtin::BuiltinType;
pub use diagnostic::{Diagnostic, DiagnosticReport, Label, Severity};
pub use explain::{DiagnosticExplanation, explain};
pub use int::Int;
pub use session::{
    SESSION_LOG_ENV, SESSION_SCHEMA_VERSION, SESSION_TAG_ENV, SessionRecord, SessionRecordParts,
    SessionSummary, SessionTimings, append_session_record, append_session_record_if_enabled,
    session_log_path_from_env, session_tag_from_env,
};
pub use sha256::sha256_hex;
pub use source::{FileId, LineIndex, SourceFile, SourceMap, SourcePosition};
pub use span::Span;

/// The import-specifier prefixes that name a local module the resolver can
/// load (`./`, `../`, `$/`, `~/`, `//`). Bare specifiers (`std`, packages)
/// are library roots and diagnose as unsupported until they land.
pub fn is_local_import_specifier(specifier: &str) -> bool {
    ["./", "../", "$/", "~/", "//"]
        .iter()
        .any(|prefix| specifier.starts_with(prefix))
}
