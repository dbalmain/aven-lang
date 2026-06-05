use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aven_core::{Diagnostic, DiagnosticReport, SourceFile};
use aven_parser::{Declaration, ParseOutput};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Revision(i32);

impl Revision {
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    pub const fn as_i32(self) -> i32 {
        self.0
    }
}

impl From<i32> for Revision {
    fn from(value: i32) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone)]
pub struct DocumentSnapshot {
    revision: Revision,
    file: SourceFile,
    parse: Arc<ParseOutput>,
    declarations: Vec<Declaration>,
    semantic_diagnostics: Vec<Diagnostic>,
}

impl DocumentSnapshot {
    pub fn parse(revision: Revision, file: SourceFile) -> Self {
        let parse = aven_parser::parse_source(&file);
        Self::from_parse(revision, file, parse)
    }

    pub fn from_parse(revision: Revision, file: SourceFile, parse: ParseOutput) -> Self {
        let declarations = aven_parser::collect_declarations(&parse.module);

        Self {
            revision,
            file,
            parse: Arc::new(parse),
            declarations,
            semantic_diagnostics: Vec::new(),
        }
    }

    pub fn with_semantic_diagnostics(&self, semantic_diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            revision: self.revision,
            file: self.file.clone(),
            parse: Arc::clone(&self.parse),
            declarations: self.declarations.clone(),
            semantic_diagnostics,
        }
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn file(&self) -> &SourceFile {
        &self.file
    }

    pub fn source(&self) -> &str {
        self.file.source()
    }

    pub fn parse_output(&self) -> &ParseOutput {
        &self.parse
    }

    pub fn declarations(&self) -> &[Declaration] {
        &self.declarations
    }

    pub fn parse_diagnostics(&self) -> &[Diagnostic] {
        &self.parse.diagnostics
    }

    pub fn semantic_diagnostics(&self) -> &[Diagnostic] {
        &self.semantic_diagnostics
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.parse
            .diagnostics
            .iter()
            .chain(self.semantic_diagnostics.iter())
    }

    pub fn diagnostic_report(&self) -> DiagnosticReport {
        DiagnosticReport::new(self.file.id, self.diagnostics().cloned().collect())
    }

    fn matches(&self, revision: Revision, source: &str) -> bool {
        self.revision == revision && self.source() == source
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PhaseTimings {
    pub parse: Duration,
    pub name: Option<Duration>,
    pub check: Option<Duration>,
    pub total: Duration,
}

#[derive(Debug, Clone)]
pub struct SemanticOutput {
    pub diagnostics: Vec<Diagnostic>,
    pub name_duration: Option<Duration>,
    pub check_duration: Option<Duration>,
}

pub fn analyze_semantics(parse: &ParseOutput) -> SemanticOutput {
    if parse.diagnostics.iter().any(Diagnostic::is_error) {
        return SemanticOutput {
            diagnostics: Vec::new(),
            name_duration: None,
            check_duration: None,
        };
    }

    let (name_analysis, name_duration) = timed(|| aven_parser::analyze_names(&parse.module));
    let (check_output, check_duration) = timed(|| aven_check::check_module(&parse.module));
    let diagnostics = name_analysis
        .diagnostics
        .into_iter()
        .chain(check_output.diagnostics)
        .collect();

    SemanticOutput {
        diagnostics,
        name_duration: Some(name_duration),
        check_duration: Some(check_duration),
    }
}

#[derive(Debug, Clone)]
pub struct CheckedDocument {
    pub document: DocumentSnapshot,
    pub timings: PhaseTimings,
}

pub fn check_source_file(file: SourceFile) -> CheckedDocument {
    check_source_file_at(Revision::default(), file)
}

fn check_source_file_at(revision: Revision, file: SourceFile) -> CheckedDocument {
    let total_start = Instant::now();
    let (parse, parse_duration) = timed(|| aven_parser::parse_source(&file));
    let document = DocumentSnapshot::from_parse(revision, file, parse);
    let semantic = analyze_semantics(document.parse_output());
    let document = document.with_semantic_diagnostics(semantic.diagnostics);

    CheckedDocument {
        document,
        timings: PhaseTimings {
            parse: parse_duration,
            name: semantic.name_duration,
            check: semantic.check_duration,
            total: total_start.elapsed(),
        },
    }
}

#[derive(Debug)]
pub struct CompilerDatabase<K> {
    documents: HashMap<K, Arc<DocumentSnapshot>>,
}

impl<K> Default for CompilerDatabase<K> {
    fn default() -> Self {
        Self {
            documents: HashMap::new(),
        }
    }
}

impl<K> CompilerDatabase<K>
where
    K: Eq + Hash + Clone,
{
    pub fn set_document(
        &mut self,
        key: K,
        revision: Revision,
        file: SourceFile,
    ) -> Arc<DocumentSnapshot> {
        if let Some(document) = self.documents.get(&key)
            && document.matches(revision, file.source())
        {
            return Arc::clone(document);
        }

        let document = Arc::new(DocumentSnapshot::parse(revision, file));
        self.documents.insert(key, Arc::clone(&document));
        document
    }

    /// Whether `set_document` would reparse for this key/revision/source — i.e.
    /// no stored snapshot already matches. Callers can check this before
    /// building a `SourceFile` to skip the line-index scan on no-op updates.
    pub fn needs_update(&self, key: &K, revision: Revision, source: &str) -> bool {
        !self
            .documents
            .get(key)
            .is_some_and(|document| document.matches(revision, source))
    }

    pub fn document(&self, key: &K) -> Option<Arc<DocumentSnapshot>> {
        self.documents.get(key).cloned()
    }

    pub fn set_semantic_diagnostics(
        &mut self,
        key: &K,
        revision: Revision,
        diagnostics: Vec<Diagnostic>,
    ) -> Option<Arc<DocumentSnapshot>> {
        let document = self.documents.get(key)?;

        if document.revision != revision {
            return None;
        }

        let document = Arc::new(document.with_semantic_diagnostics(diagnostics));
        self.documents.insert(key.clone(), Arc::clone(&document));
        Some(document)
    }
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

#[cfg(test)]
mod tests {
    use aven_core::{FileId, SourceFile};

    use super::*;

    fn source_file(source: &str) -> SourceFile {
        SourceFile::new(FileId(0), "test.av", None, source)
    }

    #[test]
    fn parsed_documents_start_without_semantic_diagnostics() {
        let document =
            DocumentSnapshot::parse(Revision::new(1), source_file("value : Missing = value\n"));

        assert_eq!(document.revision(), Revision::new(1));
        assert!(document.parse_diagnostics().is_empty());
        assert!(document.semantic_diagnostics().is_empty());
        assert_eq!(document.declarations().len(), 1);
    }

    #[test]
    fn checked_documents_include_semantic_diagnostics() {
        let checked = check_source_file(source_file("value : Missing = value\n"));

        assert!(checked.document.parse_diagnostics().is_empty());
        assert_eq!(checked.document.semantic_diagnostics().len(), 1);
        assert_eq!(
            checked.document.semantic_diagnostics()[0].code.as_deref(),
            Some("type.unknown-name")
        );
        assert!(checked.timings.name.is_some());
        assert!(checked.timings.check.is_some());
    }

    #[test]
    fn checked_documents_skip_semantics_after_parse_errors() {
        let checked = check_source_file(source_file("value = )\n"));

        assert_eq!(checked.document.parse_diagnostics().len(), 1);
        assert!(checked.document.semantic_diagnostics().is_empty());
        assert!(checked.timings.name.is_none());
        assert!(checked.timings.check.is_none());
    }

    #[test]
    fn database_reuses_matching_document_revisions() {
        let mut database = CompilerDatabase::default();
        let first = database.set_document("file", Revision::new(1), source_file("value = 1\n"));
        let second = database.set_document("file", Revision::new(1), source_file("value = 1\n"));

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn database_reports_when_documents_need_updates() {
        let mut database = CompilerDatabase::default();

        assert!(database.needs_update(&"file", Revision::new(1), "value = 1\n"));

        database.set_document("file", Revision::new(1), source_file("value = 1\n"));

        assert!(!database.needs_update(&"file", Revision::new(1), "value = 1\n"));
        assert!(database.needs_update(&"file", Revision::new(2), "value = 1\n"));
        assert!(database.needs_update(&"file", Revision::new(1), "value = 2\n"));
    }

    #[test]
    fn database_rejects_stale_semantic_diagnostics() {
        let mut database = CompilerDatabase::default();
        database.set_document("file", Revision::new(1), source_file("value = 1\n"));
        database.set_document("file", Revision::new(2), source_file("value = 2\n"));

        assert!(
            database
                .set_semantic_diagnostics(
                    &"file",
                    Revision::new(1),
                    vec![Diagnostic::error("stale diagnostic")],
                )
                .is_none()
        );

        let Some(document) = database.document(&"file") else {
            panic!("expected stored document");
        };
        assert_eq!(document.revision(), Revision::new(2));
        assert!(document.semantic_diagnostics().is_empty());
    }
}
