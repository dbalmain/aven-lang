use crate::{FileId, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

impl Label {
    pub fn primary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code: None,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code: None,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub file_id: FileId,
    pub diagnostics: Vec<Diagnostic>,
}

impl DiagnosticReport {
    pub fn new(file_id: FileId, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            file_id,
            diagnostics,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    pub fn sort_by_primary_span(&mut self) {
        self.diagnostics.sort_by_key(diagnostic_sort_key);
    }
}

fn diagnostic_sort_key(diagnostic: &Diagnostic) -> (usize, usize) {
    diagnostic
        .labels
        .first()
        .map_or((usize::MAX, usize::MAX), |label| {
            (label.span.start, label.span.end)
        })
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, DiagnosticReport, Label, Severity};
    use crate::{FileId, Span};

    #[test]
    fn report_tracks_errors() {
        let report = DiagnosticReport::new(
            FileId(3),
            vec![Diagnostic::warning("unused"), Diagnostic::error("broken")],
        );

        assert_eq!(report.file_id, FileId(3));
        assert!(report.has_errors());
    }

    #[test]
    fn report_sorts_diagnostics_by_primary_span() {
        let mut report = DiagnosticReport::new(
            FileId(0),
            vec![
                Diagnostic::error("second").with_label(Label::primary(Span::new(10, 11), "b")),
                Diagnostic::error("first").with_label(Label::primary(Span::new(1, 2), "a")),
            ],
        );

        report.sort_by_primary_span();

        assert_eq!(report.diagnostics[0].message, "first");
        assert_eq!(report.diagnostics[1].message, "second");
        assert_eq!(report.diagnostics[0].severity, Severity::Error);
    }
}
