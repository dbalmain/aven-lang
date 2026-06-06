use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aven_core::{Diagnostic, DiagnosticReport, SourceFile};
use aven_parser::{
    Declaration, DeclarationPhase, Expr, ExprKind, Item, Module, ParseOutput, RecordEntry,
    resolve_local_definition, walk_expr_children,
};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclarationKey {
    pub name: String,
    pub phase: DeclarationPhase,
    pub ordinal: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeclarationFingerprint(u64);

#[derive(Debug, Clone)]
pub struct DeclarationArtifact {
    key: DeclarationKey,
    fingerprint: DeclarationFingerprint,
    dependencies: Vec<DeclarationKey>,
}

impl DeclarationArtifact {
    pub fn key(&self) -> &DeclarationKey {
        &self.key
    }

    pub fn fingerprint(&self) -> DeclarationFingerprint {
        self.fingerprint
    }

    pub fn dependencies(&self) -> &[DeclarationKey] {
        &self.dependencies
    }
}

#[derive(Debug, Clone)]
pub struct DocumentSnapshot {
    revision: Revision,
    file: SourceFile,
    parse: Arc<ParseOutput>,
    declarations: Vec<Declaration>,
    declaration_artifacts: Vec<Arc<DeclarationArtifact>>,
    semantic_diagnostics: Vec<Diagnostic>,
}

impl DocumentSnapshot {
    pub fn parse(revision: Revision, file: SourceFile) -> Self {
        let parse = aven_parser::parse_source(&file);
        Self::from_parse(revision, file, parse)
    }

    pub fn from_parse(revision: Revision, file: SourceFile, parse: ParseOutput) -> Self {
        Self::from_parse_reusing(revision, file, parse, None)
    }

    fn from_parse_reusing(
        revision: Revision,
        file: SourceFile,
        parse: ParseOutput,
        previous: Option<&Self>,
    ) -> Self {
        let declarations = aven_parser::collect_declarations(&parse.module);
        let declaration_artifacts =
            collect_declaration_artifacts(file.source(), &parse.module, &declarations, previous);

        Self {
            revision,
            file,
            parse: Arc::new(parse),
            declarations,
            declaration_artifacts,
            semantic_diagnostics: Vec::new(),
        }
    }

    pub fn with_semantic_diagnostics(&self, semantic_diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            revision: self.revision,
            file: self.file.clone(),
            parse: Arc::clone(&self.parse),
            declarations: self.declarations.clone(),
            declaration_artifacts: self.declaration_artifacts.clone(),
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

    pub fn declaration_artifacts(
        &self,
    ) -> impl ExactSizeIterator<Item = &DeclarationArtifact> + '_ {
        self.declaration_artifacts.iter().map(Arc::as_ref)
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

fn collect_declaration_artifacts(
    source: &str,
    module: &Module,
    declarations: &[Declaration],
    previous: Option<&DocumentSnapshot>,
) -> Vec<Arc<DeclarationArtifact>> {
    let previous_by_key: HashMap<_, _> = previous
        .into_iter()
        .flat_map(|document| document.declaration_artifacts.iter())
        .map(|artifact| (artifact.key.clone(), Arc::clone(artifact)))
        .collect();

    let keys = declaration_keys(declarations);
    let top_level = top_level_declaration_map(&keys);
    declarations
        .iter()
        .zip(keys)
        .map(|(declaration, key)| {
            let fingerprint = declaration_fingerprint(source, declaration);
            let dependencies = declaration_dependencies(module, declaration, &key, &top_level);

            if let Some(previous) = previous_by_key.get(&key)
                && previous.fingerprint == fingerprint
                && previous.dependencies == dependencies
            {
                return Arc::clone(previous);
            }

            Arc::new(DeclarationArtifact {
                key,
                fingerprint,
                dependencies,
            })
        })
        .collect()
}

fn declaration_keys(declarations: &[Declaration]) -> Vec<DeclarationKey> {
    let mut ordinals = HashMap::new();
    declarations
        .iter()
        .map(|declaration| declaration_key(declaration, &mut ordinals))
        .collect()
}

fn declaration_key(
    declaration: &Declaration,
    ordinals: &mut HashMap<(String, DeclarationPhase), usize>,
) -> DeclarationKey {
    let ordinal_key = (declaration.name.clone(), declaration.phase);
    let ordinal = ordinals.entry(ordinal_key).or_default();
    let key = DeclarationKey {
        name: declaration.name.clone(),
        phase: declaration.phase,
        ordinal: *ordinal,
    };
    *ordinal += 1;
    key
}

fn top_level_declaration_map(
    keys: &[DeclarationKey],
) -> HashMap<(String, DeclarationPhase), Vec<DeclarationKey>> {
    let mut top_level: HashMap<_, Vec<_>> = HashMap::new();

    for key in keys {
        top_level
            .entry((key.name.clone(), key.phase))
            .or_default()
            .push(key.clone());
    }

    top_level
}

fn declaration_dependencies(
    module: &Module,
    declaration: &Declaration,
    current: &DeclarationKey,
    top_level: &HashMap<(String, DeclarationPhase), Vec<DeclarationKey>>,
) -> Vec<DeclarationKey> {
    let mut dependencies = Vec::new();

    for reference in declaration_references(module, declaration) {
        if resolve_local_definition(module, &reference.name, reference.span).is_some() {
            continue;
        }

        let Some(keys) = top_level.get(&(reference.name, reference.phase)) else {
            continue;
        };

        for key in keys {
            if key != current && !dependencies.contains(key) {
                dependencies.push(key.clone());
            }
        }
    }

    dependencies
}

fn is_declaration_item(item: &Item, declaration: &Declaration) -> bool {
    match item {
        Item::Binding(binding) => {
            binding.name == declaration.name && declaration.span.contains(binding.span)
        }
        Item::Signature(signature) => {
            signature.name == declaration.name && declaration.span.contains(signature.span)
        }
        Item::Expr(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    name: String,
    phase: DeclarationPhase,
    span: aven_core::Span,
}

fn declaration_references(module: &Module, declaration: &Declaration) -> Vec<Reference> {
    let mut references = Vec::new();

    for item in &module.items {
        if is_declaration_item(item, declaration) {
            collect_item_references(item, &mut references);
        }
    }

    references
}

fn collect_item_references(item: &Item, references: &mut Vec<Reference>) {
    match item {
        Item::Binding(binding) => {
            if let Some(annotation) = &binding.annotation {
                collect_expr_references(annotation, references);
            }
            collect_expr_references(&binding.value, references);
        }
        Item::Signature(signature) => collect_expr_references(&signature.annotation, references),
        Item::Expr(expr) => collect_expr_references(expr, references),
    }
}

fn collect_expr_references(expr: &Expr, references: &mut Vec<Reference>) {
    match &expr.kind {
        ExprKind::Name(name) => references.push(Reference {
            name: name.clone(),
            phase: DeclarationPhase::Runtime,
            span: expr.span,
        }),
        ExprKind::ComptimeName(name) => references.push(Reference {
            name: name.clone(),
            phase: DeclarationPhase::Comptime,
            span: expr.span,
        }),
        ExprKind::Record(entries) | ExprKind::Set(entries) => {
            collect_record_entry_references(entries, references);
        }
        ExprKind::Match { subject, arms, .. } => {
            collect_expr_references(subject, references);
            for arm in arms {
                collect_pattern_references(&arm.pattern, references);
                collect_expr_references_from_exprs(&arm.guards, references);
                collect_expr_references(&arm.body, references);
            }
        }
        _ => walk_expr_children(expr, &mut |child| {
            collect_expr_references(child, references)
        }),
    }
}

fn collect_expr_references_from_exprs(exprs: &[Expr], references: &mut Vec<Reference>) {
    for expr in exprs {
        collect_expr_references(expr, references);
    }
}

fn collect_record_entry_references(entries: &[RecordEntry], references: &mut Vec<Reference>) {
    for entry in entries {
        match entry {
            RecordEntry::Field { value, .. }
            | RecordEntry::Spread { value, .. }
            | RecordEntry::Element(value) => collect_expr_references(value, references),
            RecordEntry::Shorthand {
                name, name_span, ..
            } => references.push(Reference {
                name: name.clone(),
                phase: DeclarationPhase::Runtime,
                span: *name_span,
            }),
            RecordEntry::Delete { .. } | RecordEntry::Rename { .. } | RecordEntry::Open { .. } => {}
        }
    }
}

fn collect_pattern_references(pattern: &Expr, references: &mut Vec<Reference>) {
    match &pattern.kind {
        ExprKind::ComptimeName(name) => references.push(Reference {
            name: name.clone(),
            phase: DeclarationPhase::Comptime,
            span: pattern.span,
        }),
        ExprKind::Call { callee, args } | ExprKind::Index { callee, args } => {
            if matches!(callee.kind, ExprKind::ComptimeName(_)) {
                collect_expr_references(callee, references);
            }
            for arg in args {
                collect_pattern_references(arg, references);
            }
        }
        ExprKind::Record(entries) | ExprKind::Set(entries) => {
            collect_pattern_references_from_entries(entries, references);
        }
        ExprKind::Group(inner)
        | ExprKind::Nullable(inner)
        | ExprKind::Unary { value: inner, .. }
        | ExprKind::Propagate { value: inner, .. } => collect_pattern_references(inner, references),
        ExprKind::Tuple(items) | ExprKind::Array(items) => {
            for item in items {
                collect_pattern_references(item, references);
            }
        }
        ExprKind::Arrow { params, result } => {
            for param in params {
                collect_pattern_references(param, references);
            }
            collect_pattern_references(result, references);
        }
        ExprKind::FieldAccess { receiver, .. } => collect_pattern_references(receiver, references),
        ExprKind::Binary { left, right, .. } => {
            collect_pattern_references(left, references);
            collect_pattern_references(right, references);
        }
        ExprKind::Match { subject, arms, .. } => {
            collect_pattern_references(subject, references);
            for arm in arms {
                collect_pattern_references(&arm.pattern, references);
                collect_pattern_references(&arm.body, references);
            }
        }
        ExprKind::Lambda { .. }
        | ExprKind::Block(_)
        | ExprKind::Missing
        | ExprKind::Literal(_)
        | ExprKind::Name(_) => {}
    }
}

fn collect_pattern_references_from_entries(
    entries: &[RecordEntry],
    references: &mut Vec<Reference>,
) {
    for entry in entries {
        match entry {
            RecordEntry::Field { value, .. } | RecordEntry::Element(value) => {
                collect_pattern_references(value, references);
            }
            RecordEntry::Spread { value, .. } => {
                if !matches!(value.kind, ExprKind::Name(_)) {
                    collect_pattern_references(value, references);
                }
            }
            RecordEntry::Shorthand { .. }
            | RecordEntry::Delete { .. }
            | RecordEntry::Rename { .. }
            | RecordEntry::Open { .. } => {}
        }
    }
}

fn declaration_fingerprint(source: &str, declaration: &Declaration) -> DeclarationFingerprint {
    let range = declaration.span.start..declaration.span.end;
    let source_text = source.get(range);
    debug_assert!(
        source_text.is_some(),
        "declaration span must be in-bounds and on a char boundary"
    );
    let source_text = source_text.unwrap_or_default();

    let mut hasher = DefaultHasher::new();
    source_text.hash(&mut hasher);
    DeclarationFingerprint(hasher.finish())
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
        let previous = self.documents.get(&key);
        if let Some(document) = previous
            && document.matches(revision, file.source())
        {
            return Arc::clone(document);
        }

        let parse = aven_parser::parse_source(&file);
        let document = Arc::new(DocumentSnapshot::from_parse_reusing(
            revision,
            file,
            parse,
            previous.map(Arc::as_ref),
        ));
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
    use aven_core::{FileId, SourceFile, codes};

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
        assert_eq!(document.declaration_artifacts().len(), 1);
    }

    #[test]
    fn declaration_artifacts_record_stable_keys() {
        let document = DocumentSnapshot::parse(
            Revision::new(1),
            source_file("value = 1\nvalue = 2\nTypeValue = 3\n"),
        );
        let artifacts = &document.declaration_artifacts;

        assert_eq!(artifacts[0].key().name, "value");
        assert_eq!(artifacts[0].key().phase, DeclarationPhase::Runtime);
        assert_eq!(artifacts[0].key().ordinal, 0);
        assert_eq!(artifacts[1].key().name, "value");
        assert_eq!(artifacts[1].key().phase, DeclarationPhase::Runtime);
        assert_eq!(artifacts[1].key().ordinal, 1);
        assert_eq!(artifacts[2].key().name, "TypeValue");
        assert_eq!(artifacts[2].key().phase, DeclarationPhase::Comptime);
        assert_eq!(artifacts[2].key().ordinal, 0);
    }

    #[test]
    fn declaration_artifacts_record_top_level_dependencies() {
        let document = DocumentSnapshot::parse(
            Revision::new(1),
            source_file(
                "User = Text\nhelper = 1\nvalue : User\nvalue = (input) => helper + input\n",
            ),
        );
        let artifacts = &document.declaration_artifacts;

        assert_eq!(
            artifacts[2].dependencies(),
            &[
                DeclarationKey {
                    name: "User".to_owned(),
                    phase: DeclarationPhase::Comptime,
                    ordinal: 0,
                },
                DeclarationKey {
                    name: "helper".to_owned(),
                    phase: DeclarationPhase::Runtime,
                    ordinal: 0,
                },
            ]
        );
    }

    #[test]
    fn declaration_dependencies_ignore_local_binders() {
        let document = DocumentSnapshot::parse(
            Revision::new(1),
            source_file("helper = 1\nvalue = (helper) =>\n  local = helper\n  { local }\n"),
        );
        let artifacts = &document.declaration_artifacts;

        assert!(artifacts[1].dependencies().is_empty());
    }

    #[test]
    fn declaration_dependencies_capture_references_in_nested_scopes() {
        let document = DocumentSnapshot::parse(
            Revision::new(1),
            source_file("helper = 1\nvalue = () =>\n  local = helper\n  local\n"),
        );
        let artifacts = &document.declaration_artifacts;

        assert_eq!(
            artifacts[1].dependencies(),
            &[DeclarationKey {
                name: "helper".to_owned(),
                phase: DeclarationPhase::Runtime,
                ordinal: 0,
            }]
        );
    }

    #[test]
    fn checked_documents_include_semantic_diagnostics() {
        let checked = check_source_file(source_file("value : Missing = value\n"));

        assert!(checked.document.parse_diagnostics().is_empty());
        assert_eq!(checked.document.semantic_diagnostics().len(), 1);
        assert_eq!(
            checked.document.semantic_diagnostics()[0].code.as_deref(),
            Some(codes::ty::UNKNOWN_NAME)
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
    fn database_reuses_unchanged_declaration_artifacts_across_revisions() {
        let mut database = CompilerDatabase::default();
        let first = database.set_document(
            "file",
            Revision::new(1),
            source_file("first = 1\nsecond = 2\n"),
        );
        let first_artifact = Arc::clone(&first.declaration_artifacts[0]);
        let second_artifact = Arc::clone(&first.declaration_artifacts[1]);

        let second = database.set_document(
            "file",
            Revision::new(2),
            source_file("inserted = 0\nfirst = 1\nsecond = 3\n"),
        );

        assert!(Arc::ptr_eq(
            &first_artifact,
            &second.declaration_artifacts[1]
        ));
        assert!(!Arc::ptr_eq(
            &second_artifact,
            &second.declaration_artifacts[2]
        ));
    }

    #[test]
    fn database_recomputes_artifact_when_dependency_resolution_changes() {
        let mut database = CompilerDatabase::default();
        let first =
            database.set_document("file", Revision::new(1), source_file("value = missing\n"));
        let value_artifact = Arc::clone(&first.declaration_artifacts[0]);

        let second = database.set_document(
            "file",
            Revision::new(2),
            source_file("missing = 1\nvalue = missing\n"),
        );

        assert!(!Arc::ptr_eq(
            &value_artifact,
            &second.declaration_artifacts[1]
        ));
        assert_eq!(
            second.declaration_artifacts[1].dependencies(),
            &[DeclarationKey {
                name: "missing".to_owned(),
                phase: DeclarationPhase::Runtime,
                ordinal: 0,
            }]
        );
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
