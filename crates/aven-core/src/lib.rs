pub mod codes;
pub mod diagnostic;
pub mod explain;
pub mod source;
pub mod span;

pub use diagnostic::{Diagnostic, DiagnosticReport, Label, Severity};
pub use explain::{DiagnosticExplanation, explain};
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
