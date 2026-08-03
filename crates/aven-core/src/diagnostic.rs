use crate::{FileId, Span};

/// Label attached to the other source sites folded into one repeated diagnostic.
pub const REPEATED_OCCURRENCE_LABEL: &str = "same fault also occurs here";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Machine-readable diagnostic shape shared by `aven check --format json`,
/// session logs, and (later) LSP telemetry. Field names and nesting match the
/// existing CLI JSON contract exactly — do not invent a parallel shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Build the shared presentation report, folding repeated fault shapes while
    /// retaining every distinct primary location as a related label.
    pub fn new(file_id: FileId, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            file_id,
            diagnostics: collapse_repeated_diagnostics(diagnostics),
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

#[derive(PartialEq, Eq)]
struct DiagnosticFault {
    severity: Severity,
    code: Option<String>,
    message: String,
    label_messages: Vec<String>,
    notes: Vec<String>,
}

impl From<&Diagnostic> for DiagnosticFault {
    fn from(diagnostic: &Diagnostic) -> Self {
        Self {
            severity: diagnostic.severity,
            code: diagnostic.code.clone(),
            message: diagnostic.message.clone(),
            label_messages: diagnostic
                .labels
                .iter()
                .map(|label| label.message.clone())
                .collect(),
            notes: diagnostic.notes.clone(),
        }
    }
}

struct DiagnosticGroup {
    fault: DiagnosticFault,
    diagnostic: Diagnostic,
    occurrence_spans: Vec<Span>,
}

fn collapse_repeated_diagnostics(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut groups: Vec<DiagnosticGroup> = Vec::new();

    for diagnostic in diagnostics {
        let Some(primary_span) = diagnostic.labels.first().map(|label| label.span) else {
            groups.push(DiagnosticGroup {
                fault: DiagnosticFault::from(&diagnostic),
                diagnostic,
                occurrence_spans: Vec::new(),
            });
            continue;
        };
        // Compare the structured fields rather than serialized or rendered
        // output. Semantic arguments carried in those fields (such as a missing
        // field's name) keep distinct faults separate; only spans are ignored.
        let fault = DiagnosticFault::from(&diagnostic);

        if let Some(group) = groups.iter_mut().find(|group| group.fault == fault) {
            if !group.occurrence_spans.contains(&primary_span) {
                group.occurrence_spans.push(primary_span);
            }
            if diagnostic_sort_key(&diagnostic) < diagnostic_sort_key(&group.diagnostic) {
                group.diagnostic = diagnostic;
            }
        } else {
            groups.push(DiagnosticGroup {
                fault,
                diagnostic,
                occurrence_spans: vec![primary_span],
            });
        }
    }

    groups
        .into_iter()
        .map(|mut group| {
            if group.occurrence_spans.len() <= 1 {
                return group.diagnostic;
            }

            let primary_span = group.diagnostic.labels[0].span;
            group
                .occurrence_spans
                .sort_unstable_by_key(|span| (span.start, span.end));
            group.diagnostic.labels.extend(
                group
                    .occurrence_spans
                    .iter()
                    .copied()
                    .filter(|span| *span != primary_span)
                    .map(|span| Label::primary(span, REPEATED_OCCURRENCE_LABEL)),
            );
            group.diagnostic.notes.push(format!(
                "also occurs at {} other locations",
                group.occurrence_spans.len() - 1
            ));
            group.diagnostic
        })
        .collect()
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
    use super::{Diagnostic, DiagnosticReport, Label, REPEATED_OCCURRENCE_LABEL, Severity};
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

    #[test]
    fn report_collapses_repeated_faults_and_keeps_occurrence_spans() {
        let repeated = |span| {
            Diagnostic::error("missing field `run`")
                .with_code("type.missing-field")
                .with_label(Label::primary(span, "record has no `run` field"))
                .with_note("add `run: ...`")
        };

        let report = DiagnosticReport::new(
            FileId(0),
            vec![
                repeated(Span::new(30, 33)),
                repeated(Span::new(10, 13)),
                repeated(Span::new(20, 23)),
            ],
        );

        assert_eq!(report.diagnostics.len(), 1);
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.labels[0].span, Span::new(10, 13));
        assert_eq!(diagnostic.labels.len(), 3);
        assert!(
            diagnostic.labels[1..]
                .iter()
                .all(|label| label.message == REPEATED_OCCURRENCE_LABEL)
        );
        assert_eq!(
            diagnostic.notes,
            ["add `run: ...`", "also occurs at 2 other locations"]
        );
    }

    #[test]
    fn report_keeps_different_faults_with_the_same_code() {
        let missing = |name: &str, span| {
            Diagnostic::error(format!("missing field `{name}`"))
                .with_code("type.missing-field")
                .with_label(Label::primary(span, "record is missing a field"))
        };

        let report = DiagnosticReport::new(
            FileId(0),
            vec![
                missing("start", Span::new(10, 15)),
                missing("stop", Span::new(20, 24)),
            ],
        );

        assert_eq!(report.diagnostics.len(), 2);
        assert_eq!(report.diagnostics[0].message, "missing field `start`");
        assert_eq!(report.diagnostics[1].message, "missing field `stop`");
    }
}
