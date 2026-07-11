use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use aven_check::{ModuleImports as CheckModuleImports, RowTail, Type};
use aven_core::{Diagnostic, DiagnosticReport, FileId, Label, SourceFile, SourceMap, Span, codes};
use aven_eval::{ModuleImports as EvalModuleImports, Value};
use aven_parser::{
    Expr, ExprKind, Item, Literal, Module, ParseOutput, RecordEntry, decode_string_literal,
};

use crate::{
    HostGlobals, PhaseTimings, SemanticOutput, analyze_semantics_with_host_globals_and_imports,
};

#[derive(Debug)]
pub struct ModuleCheckOutput {
    pub source_map: SourceMap,
    pub reports: Vec<DiagnosticReport>,
    pub nodes: Vec<ModuleNodeCheckOutput>,
    pub timings: PhaseTimings,
}

#[derive(Debug, Clone)]
pub struct ModuleNodeCheckOutput {
    pub canonical_path: PathBuf,
    pub file: SourceFile,
    pub semantic: SemanticOutput,
    pub imports: Vec<ModuleImportResolution>,
    pub export_provenance: ExportProvenanceMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleImportResolution {
    pub specifier: String,
    pub specifier_span: Span,
    pub call_span: Span,
    pub target_path: Option<PathBuf>,
    pub failed: bool,
}

pub type ExportProvenanceMap = HashMap<String, ExportProvenance>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportProvenance {
    pub canonical_path: PathBuf,
    pub definition_span: Span,
}

#[derive(Debug)]
pub struct ModuleEvalOutput {
    pub source_map: SourceMap,
    pub reports: Vec<DiagnosticReport>,
    pub value: Option<Value>,
}

#[derive(Debug, Default, Clone)]
pub struct SourceOverlay {
    sources: HashMap<PathBuf, String>,
}

/// Embedded library modules, keyed by module specifier (`std`, `std/time`).
pub type LibraryModules = HashMap<String, &'static str>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleRoots {
    pub project: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub filesystem: bool,
    /// Host-registered libraries resolving bare import specifiers: library
    /// name -> module specifier -> embedded source text. Empty by default.
    pub libraries: HashMap<String, LibraryModules>,
}

impl ModuleRoots {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn discover(entry: &Path) -> Self {
        let entry_dir = entry.parent().unwrap_or_else(|| Path::new("."));
        let project = entry_dir
            .ancestors()
            .find(|directory| directory.join("Aven.toml").is_file())
            .map_or_else(|| entry_dir.to_path_buf(), Path::to_path_buf);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            project: Some(project),
            home,
            filesystem: true,
            libraries: HashMap::new(),
        }
    }

    pub fn with_library(mut self, name: impl Into<String>, modules: LibraryModules) -> Self {
        self.libraries.insert(name.into(), modules);
        self
    }
}

impl SourceOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: PathBuf, source: String) -> Option<String> {
        self.sources.insert(path, source)
    }

    fn source(&self, path: &Path) -> Option<&str> {
        self.sources.get(path).map(String::as_str)
    }
}

#[derive(Debug)]
struct ModuleGraph {
    source_map: SourceMap,
    nodes: Vec<ModuleNode>,
    by_path: HashMap<PathBuf, usize>,
    order: Vec<usize>,
}

#[derive(Debug)]
struct ModuleNode {
    path: PathBuf,
    file: SourceFile,
    parse: ParseOutput,
    imports: Vec<ImportRef>,
}

#[derive(Debug, Clone)]
struct ImportRef {
    specifier: String,
    specifier_span: Span,
    call_span: Span,
    target: Option<usize>,
    failed: bool,
}

#[derive(Debug, Clone)]
enum CheckExport {
    Record {
        ty: Type,
        type_exports: HashMap<String, Type>,
    },
    HasErrors,
    UppercaseExportNotType {
        name: String,
        span: Span,
    },
    NotImportable {
        note: String,
    },
}

#[derive(Debug, Clone)]
enum EvalExport {
    Record(Value),
    HasErrors,
    NotImportable { note: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Loading,
    Loaded,
}

pub fn check_path_with_host_globals(
    path: &Path,
    globals: &HostGlobals,
) -> io::Result<ModuleCheckOutput> {
    check_path_with_host_globals_and_overlay(path, globals, &SourceOverlay::default())
}

pub fn check_path_with_host_globals_and_overlay(
    path: &Path,
    globals: &HostGlobals,
    overlay: &SourceOverlay,
) -> io::Result<ModuleCheckOutput> {
    check_path_with_host_globals_and_overlay_and_entry_parse(path, globals, overlay, None)
}

pub fn check_path_with_host_globals_and_roots(
    path: &Path,
    globals: &HostGlobals,
    roots: &ModuleRoots,
) -> io::Result<ModuleCheckOutput> {
    check_path_with_host_globals_and_overlay_and_entry_parse_with_roots(
        path,
        globals,
        &SourceOverlay::default(),
        None,
        roots,
    )
}

pub fn check_path_with_host_globals_and_overlay_and_entry_parse(
    path: &Path,
    globals: &HostGlobals,
    overlay: &SourceOverlay,
    entry_parse: Option<&ParseOutput>,
) -> io::Result<ModuleCheckOutput> {
    check_path_with_host_globals_and_overlay_and_entry_parse_with_roots(
        path,
        globals,
        overlay,
        entry_parse,
        &ModuleRoots::discover(path),
    )
}

pub fn check_path_with_host_globals_and_overlay_and_entry_parse_with_roots(
    path: &Path,
    globals: &HostGlobals,
    overlay: &SourceOverlay,
    entry_parse: Option<&ParseOutput>,
    roots: &ModuleRoots,
) -> io::Result<ModuleCheckOutput> {
    let total_start = Instant::now();
    let graph = ModuleGraph::load(path, overlay, entry_parse, roots)?;
    let mut diagnostics = parse_diagnostics(&graph);
    let mut exports = vec![CheckExport::HasErrors; graph.nodes.len()];
    let mut export_provenance = vec![ExportProvenanceMap::new(); graph.nodes.len()];
    let mut semantics = vec![None; graph.nodes.len()];
    let mut name_duration = None;
    let mut check_duration = None;

    for node_id in graph.order.iter().copied() {
        let imports = check_imports_for_node(&graph.nodes[node_id], &exports, &mut diagnostics);
        let file_id = graph.nodes[node_id].file.id;
        let semantic = analyze_semantics_with_host_globals_and_imports(
            &graph.nodes[node_id].parse,
            globals,
            &imports,
        );
        merge_semantic_timing(&mut name_duration, &mut check_duration, &semantic);
        let semantic_has_errors = semantic.diagnostics.iter().any(Diagnostic::is_error);
        let export = if semantic_has_errors {
            CheckExport::HasErrors
        } else {
            check_export_for_node(&graph.nodes[node_id], &semantic, globals)
        };
        let export = match export {
            CheckExport::UppercaseExportNotType { name, span } => {
                diagnostics.entry(file_id).or_default().push(
                    Diagnostic::error(format!("uppercase export `{name}` is not a type"))
                        .with_code(codes::module::UPPERCASE_EXPORT_NOT_TYPE)
                        .with_label(Label::primary(span, "uppercase exports must be types"))
                        .with_note("export a type binding explicitly, or use a lowercase field for a runtime value"),
                );
                CheckExport::HasErrors
            }
            other => other,
        };
        let provenance = if semantic_has_errors {
            ExportProvenanceMap::new()
        } else {
            export_provenance_for_node(&graph, node_id, &export_provenance)
        };
        diagnostics
            .entry(file_id)
            .or_default()
            .extend(semantic.diagnostics.clone());
        semantics[node_id] = Some(semantic);

        if semantic_has_errors || file_has_errors(&diagnostics, file_id) {
            exports[node_id] = CheckExport::HasErrors;
            continue;
        }

        export_provenance[node_id] = provenance;
        exports[node_id] = export;
    }

    let mut reports = reports_from_diagnostics(&graph.source_map, diagnostics);
    sort_reports(&mut reports);
    let nodes = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(node_id, node)| {
            let semantic = semantics[node_id].clone()?;
            Some(ModuleNodeCheckOutput {
                canonical_path: node.path.clone(),
                file: node.file.clone(),
                semantic,
                imports: module_import_resolutions(&graph, node),
                export_provenance: export_provenance[node_id].clone(),
            })
        })
        .collect();
    Ok(ModuleCheckOutput {
        source_map: graph.source_map,
        reports,
        nodes,
        timings: PhaseTimings {
            parse: total_start.elapsed(),
            name: name_duration,
            check: check_duration,
            total: total_start.elapsed(),
        },
    })
}

pub fn eval_path_with_globals(
    path: &Path,
    globals: Vec<(String, Value)>,
) -> io::Result<ModuleEvalOutput> {
    eval_path_with_globals_and_roots(path, globals, &ModuleRoots::discover(path))
}

pub fn eval_path_with_globals_and_roots(
    path: &Path,
    globals: Vec<(String, Value)>,
    roots: &ModuleRoots,
) -> io::Result<ModuleEvalOutput> {
    let graph = ModuleGraph::load(path, &SourceOverlay::default(), None, roots)?;
    let mut diagnostics = parse_diagnostics(&graph);
    let mut exports = vec![EvalExport::HasErrors; graph.nodes.len()];
    let mut entry_value = None;

    for node_id in graph.order.iter().copied() {
        let imports = eval_imports_for_node(&graph.nodes[node_id], &exports, &mut diagnostics);
        let file_id = graph.nodes[node_id].file.id;
        if file_has_errors(&diagnostics, file_id) {
            exports[node_id] = EvalExport::HasErrors;
            continue;
        }

        let outcome = aven_eval::eval_module_with_globals_and_imports(
            &graph.nodes[node_id].parse.module,
            globals.clone(),
            &imports,
        );
        entry_value = (node_id == 0).then_some(outcome.value.clone()).flatten();
        diagnostics
            .entry(file_id)
            .or_default()
            .extend(outcome.diagnostics);

        if file_has_errors(&diagnostics, file_id) {
            exports[node_id] = EvalExport::HasErrors;
            continue;
        }

        exports[node_id] = eval_export_for_node(&graph.nodes[node_id], outcome.value);
    }

    let mut reports = reports_from_diagnostics(&graph.source_map, diagnostics);
    sort_reports(&mut reports);
    Ok(ModuleEvalOutput {
        source_map: graph.source_map,
        reports,
        value: entry_value,
    })
}

impl ModuleGraph {
    fn load(
        entry_path: &Path,
        overlay: &SourceOverlay,
        entry_parse: Option<&ParseOutput>,
        roots: &ModuleRoots,
    ) -> io::Result<Self> {
        let entry_path = fs::canonicalize(entry_path)?;
        let mut graph = Self {
            source_map: SourceMap::new(),
            nodes: Vec::new(),
            by_path: HashMap::new(),
            order: Vec::new(),
        };
        let mut states = HashMap::new();
        let mut stack = Vec::new();
        graph.load_module(
            &entry_path,
            overlay,
            entry_parse,
            roots,
            &mut states,
            &mut stack,
        )?;
        Ok(graph)
    }

    fn load_module(
        &mut self,
        path: &Path,
        overlay: &SourceOverlay,
        entry_parse: Option<&ParseOutput>,
        roots: &ModuleRoots,
        states: &mut HashMap<PathBuf, VisitState>,
        stack: &mut Vec<PathBuf>,
    ) -> io::Result<usize> {
        if let Some(node_id) = self.by_path.get(path).copied() {
            return Ok(node_id);
        }

        let file = self.load_source(path, overlay, roots)?;
        let parse = if self.nodes.is_empty() {
            entry_parse
                .cloned()
                .unwrap_or_else(|| aven_parser::parse_source(&file))
        } else {
            aven_parser::parse_source(&file)
        };
        let node_id = self.nodes.len();
        self.by_path.insert(path.to_path_buf(), node_id);
        self.nodes.push(ModuleNode {
            path: path.to_path_buf(),
            file,
            parse,
            imports: Vec::new(),
        });

        states.insert(path.to_path_buf(), VisitState::Loading);
        stack.push(path.to_path_buf());

        let import_calls = collect_import_calls(&self.nodes[node_id].parse.module);
        let imports = import_calls
            .into_iter()
            .map(|call| self.resolve_import(node_id, call, overlay, roots, states, stack))
            .collect::<io::Result<Vec<_>>>()?;
        self.nodes[node_id].imports = imports;

        stack.pop();
        states.insert(path.to_path_buf(), VisitState::Loaded);
        self.order.push(node_id);
        Ok(node_id)
    }

    fn load_source(
        &mut self,
        path: &Path,
        overlay: &SourceOverlay,
        roots: &ModuleRoots,
    ) -> io::Result<SourceFile> {
        // Embedded library modules read from the registered library map (their
        // virtual key never exists on disk); diagnostics render the bare
        // specifier (`std/time`) as the file name.
        let (name, path_hint, source) = match library_specifier(path) {
            Some(specifier) => {
                let source = library_module_source(roots, &specifier)
                    .expect("library modules are resolved against the registry before loading");
                (specifier, None, source.to_owned())
            }
            None => {
                let source = overlay
                    .source(path)
                    .map_or_else(|| fs::read_to_string(path), |source| Ok(source.to_owned()))?;
                (path.display().to_string(), Some(path.to_path_buf()), source)
            }
        };
        let id = self.source_map.add(name, path_hint, source);
        Ok(self
            .source_map
            .get(id)
            .expect("source map returns a file immediately after insertion")
            .clone())
    }

    fn resolve_import(
        &mut self,
        importer: usize,
        call: ImportCall,
        overlay: &SourceOverlay,
        roots: &ModuleRoots,
        states: &mut HashMap<PathBuf, VisitState>,
        stack: &mut Vec<PathBuf>,
    ) -> io::Result<ImportRef> {
        let Some(specifier) = call.specifier else {
            return Ok(ImportRef {
                specifier: String::new(),
                specifier_span: call.specifier_span,
                call_span: call.call_span,
                target: None,
                failed: false,
            });
        };

        let canonical = match resolve_import_target(&self.nodes[importer].path, &specifier, roots) {
            ResolvedImport::File(resolved) => match fs::canonicalize(&resolved) {
                Ok(canonical) => canonical,
                Err(_) => {
                    self.nodes[importer].parse.diagnostics.push(not_found(
                        call.specifier_span,
                        &specifier,
                        &format!("tried {}", resolved.display()),
                    ));
                    return Ok(ImportRef {
                        specifier,
                        specifier_span: call.specifier_span,
                        call_span: call.call_span,
                        target: None,
                        failed: true,
                    });
                }
            },
            // Library keys are virtual: already canonical, never touch disk.
            ResolvedImport::Library(virtual_path) => virtual_path,
            ResolvedImport::RootUnavailable => {
                self.nodes[importer]
                    .parse
                    .diagnostics
                    .push(root_unavailable(call.specifier_span, &specifier));
                return Ok(ImportRef {
                    specifier,
                    specifier_span: call.specifier_span,
                    call_span: call.call_span,
                    target: None,
                    failed: false,
                });
            }
            ResolvedImport::UnknownLibrary => {
                self.nodes[importer]
                    .parse
                    .diagnostics
                    .push(unsupported_root(call.specifier_span, &specifier));
                return Ok(ImportRef {
                    specifier,
                    specifier_span: call.specifier_span,
                    call_span: call.call_span,
                    target: None,
                    failed: false,
                });
            }
            ResolvedImport::LibraryModuleMissing { library, tried } => {
                self.nodes[importer].parse.diagnostics.push(not_found(
                    call.specifier_span,
                    &specifier,
                    &format!("tried `{tried}` in library `{library}`"),
                ));
                return Ok(ImportRef {
                    specifier,
                    specifier_span: call.specifier_span,
                    call_span: call.call_span,
                    target: None,
                    failed: true,
                });
            }
        };

        if states.get(&canonical) == Some(&VisitState::Loading) {
            self.nodes[importer].parse.diagnostics.push(import_cycle(
                call.specifier_span,
                cycle_path(stack, &canonical),
            ));
            return Ok(ImportRef {
                specifier,
                specifier_span: call.specifier_span,
                call_span: call.call_span,
                target: self.by_path.get(&canonical).copied(),
                failed: true,
            });
        }

        let target = self.load_module(&canonical, overlay, None, roots, states, stack)?;
        Ok(ImportRef {
            specifier,
            specifier_span: call.specifier_span,
            call_span: call.call_span,
            target: Some(target),
            failed: false,
        })
    }
}

#[derive(Debug)]
struct ImportCall {
    specifier: Option<String>,
    specifier_span: Span,
    call_span: Span,
}

fn collect_import_calls(module: &Module) -> Vec<ImportCall> {
    let mut calls = Vec::new();
    for item in &module.items {
        match item {
            Item::Binding(binding) => collect_import_calls_from_expr(&binding.value, &mut calls),
            Item::PatternBinding(binding) => {
                collect_import_calls_from_expr(&binding.value, &mut calls);
            }
            Item::SpreadBinding(binding) => {
                collect_import_calls_from_expr(&binding.value, &mut calls);
            }
            Item::Signature(signature) => {
                collect_import_calls_from_expr(&signature.annotation, &mut calls);
            }
            Item::Expr(expr) => collect_import_calls_from_expr(expr, &mut calls),
        }
    }
    calls
}

fn collect_import_calls_from_expr(expr: &Expr, calls: &mut Vec<ImportCall>) {
    if let ExprKind::Call { callee, args } = &expr.kind
        && matches!(&callee.kind, ExprKind::Name(name) if name == "import")
    {
        let (specifier, span) = args.first().map_or((None, callee.span), |arg| {
            let specifier = match &arg.kind {
                ExprKind::Literal(Literal::String(raw)) => Some(decode_string_literal(raw)),
                _ => None,
            };
            (specifier, arg.span)
        });
        calls.push(ImportCall {
            specifier,
            specifier_span: span,
            call_span: expr.span,
        });
    }

    aven_parser::walk_expr_children(expr, &mut |child| {
        collect_import_calls_from_expr(child, calls);
    });
}

fn check_imports_for_node(
    node: &ModuleNode,
    exports: &[CheckExport],
    diagnostics: &mut HashMap<FileId, Vec<Diagnostic>>,
) -> CheckModuleImports {
    let mut imports = CheckModuleImports::default();
    for import in &node.imports {
        if import.specifier.is_empty() {
            continue;
        }
        match import.target.and_then(|target| exports.get(target)) {
            Some(CheckExport::Record { ty, type_exports }) if !import.failed => {
                imports.insert(import.specifier.clone(), ty.clone());
                imports.insert_type_exports(import.specifier.clone(), type_exports.clone());
            }
            Some(CheckExport::NotImportable { note }) if !import.failed => {
                imports.insert_failed(import.specifier.clone());
                diagnostics
                    .entry(node.file.id)
                    .or_default()
                    .push(not_importable(import.call_span, &import.specifier, note));
            }
            Some(CheckExport::HasErrors) if !import.failed => {
                imports.insert_failed(import.specifier.clone());
                diagnostics
                    .entry(node.file.id)
                    .or_default()
                    .push(import_has_errors(import.call_span, &import.specifier));
            }
            _ => {
                imports.insert_failed(import.specifier.clone());
            }
        }
    }
    imports
}

fn eval_imports_for_node(
    node: &ModuleNode,
    exports: &[EvalExport],
    diagnostics: &mut HashMap<FileId, Vec<Diagnostic>>,
) -> EvalModuleImports {
    let mut imports = EvalModuleImports::default();
    for import in &node.imports {
        if import.specifier.is_empty() {
            continue;
        }
        match import.target.and_then(|target| exports.get(target)) {
            Some(EvalExport::Record(value)) if !import.failed => {
                imports.insert(import.specifier.clone(), value.clone());
            }
            Some(EvalExport::NotImportable { note }) if !import.failed => {
                imports.insert_failed(import.specifier.clone());
                diagnostics
                    .entry(node.file.id)
                    .or_default()
                    .push(not_importable(import.call_span, &import.specifier, note));
            }
            Some(EvalExport::HasErrors) if !import.failed => {
                imports.insert_failed(import.specifier.clone());
                diagnostics
                    .entry(node.file.id)
                    .or_default()
                    .push(import_has_errors(import.call_span, &import.specifier));
            }
            _ => {
                imports.insert_failed(import.specifier.clone());
            }
        }
    }
    imports
}

fn check_export_for_node(
    node: &ModuleNode,
    semantic: &SemanticOutput,
    globals: &HostGlobals,
) -> CheckExport {
    let Some(final_expr) = final_expr(&node.parse.module) else {
        return CheckExport::NotImportable {
            note: format!("{} has no final expression to export", node.file.name),
        };
    };

    let ty = semantic
        .inferred_types
        .iter()
        .filter(|inferred| type_span_contains(inferred.name_span, final_expr.span))
        .min_by_key(|inferred| inferred.name_span.len())
        .map(|inferred| inferred.ty.clone())
        .or_else(|| {
            matches!(final_expr.kind, ExprKind::Record(_)).then(|| {
                Type::Record(aven_check::Row {
                    entries: Vec::new(),
                    tail: RowTail::Closed,
                })
            })
        });
    let Some(ty) = ty else {
        return CheckExport::NotImportable {
            note: format!(
                "{} final expression at {} is not a statically-known record",
                node.file.name,
                render_span(&node.file, final_expr.span)
            ),
        };
    };

    if !matches!(&ty, Type::Record(row) if row.tail == RowTail::Closed) {
        return CheckExport::NotImportable {
            note: format!(
                "{} final expression at {} has type `{}`",
                node.file.name,
                render_span(&node.file, final_expr.span),
                ty.render()
            ),
        };
    }

    let Type::Record(_) = ty else {
        unreachable!("closed record guard above");
    };
    let ExprKind::Record(entries) = &final_expr.kind else {
        return CheckExport::NotImportable {
            note: "final expression is not a record literal".to_owned(),
        };
    };
    let has_type_shaped_field = entries.iter().any(|entry| {
        let name = match entry {
            RecordEntry::Field { name, .. } | RecordEntry::Shorthand { name, .. } => name,
            RecordEntry::Rename { to, .. } => to,
            _ => return false,
        };
        name.chars().next().is_some_and(char::is_uppercase)
    });
    if !has_type_shaped_field {
        return CheckExport::Record {
            ty,
            type_exports: HashMap::new(),
        };
    }
    let mut fields = Vec::new();
    let mut type_exports = HashMap::new();
    for entry in entries {
        let (name, source_name, value_span, source_is_name) = match entry {
            RecordEntry::Field {
                name,
                value,
                name_span,
                ..
            } => (name, expr_name(value), *name_span, true),
            RecordEntry::Shorthand {
                name, name_span, ..
            } => (name, Some(name.as_str()), *name_span, true),
            RecordEntry::Rename {
                from,
                from_span,
                to,
                ..
            } => (to, Some(from.as_str()), *from_span, true),
            _ => {
                return CheckExport::NotImportable {
                    note: "final expression contains an unsupported export entry".to_owned(),
                };
            }
        };
        let is_type = name.chars().next().is_some_and(char::is_uppercase);
        let field_ty = if is_type {
            // Type exports cover module-local aliases AND host-registered type
            // definitions (`Instant`, ...), which live in the same merged map.
            let Some(definition) = source_name
                .and_then(|source| semantic.type_definitions.get(source))
                .cloned()
            else {
                return CheckExport::UppercaseExportNotType {
                    name: name.clone(),
                    span: value_span,
                };
            };
            type_exports.insert(name.clone(), definition.clone());
            // The *value* side of a type export is the type value: for a
            // statics-carrying host type that value answers `Instant.parse`
            // etc., so type its field as the statics record (mirroring the
            // evaluator, where `Value::Type` field access resolves statics).
            source_name
                .and_then(|source| aven_check::type_statics(globals, source))
                .map_or(definition, |statics| {
                    Type::Record(aven_check::Row {
                        entries: statics
                            .into_iter()
                            .map(|field| aven_check::RowEntry::Field {
                                name: field.name,
                                ty: field.ty,
                            })
                            .collect(),
                        tail: RowTail::Closed,
                    })
                })
        } else {
            let field_ty = source_name
                .and_then(|source| semantic.top_level_types.get(source))
                .cloned()
                .or_else(|| {
                    semantic
                        .inferred_types
                        .iter()
                        .filter(|inferred| {
                            source_is_name && type_span_contains(inferred.name_span, value_span)
                        })
                        .min_by_key(|inferred| inferred.name_span.len())
                        .map(|inferred| inferred.ty.clone())
                });
            let Some(field_ty) = field_ty else {
                return CheckExport::NotImportable {
                    note: format!("export field `{name}` is not statically known"),
                };
            };
            field_ty
        };
        fields.push(aven_check::RowEntry::Field {
            name: name.clone(),
            ty: field_ty,
        });
    }
    CheckExport::Record {
        ty: Type::Record(aven_check::Row {
            entries: fields,
            tail: RowTail::Closed,
        }),
        type_exports,
    }
}

fn expr_name(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Name(name) | ExprKind::ComptimeName(name) => Some(name),
        _ => None,
    }
}

fn export_provenance_for_node(
    graph: &ModuleGraph,
    node_id: usize,
    provenance_by_node: &[ExportProvenanceMap],
) -> ExportProvenanceMap {
    let node = &graph.nodes[node_id];
    let Some(ExprKind::Record(entries)) = final_expr(&node.parse.module).map(|expr| &expr.kind)
    else {
        return ExportProvenanceMap::new();
    };

    let declarations = aven_parser::collect_declarations(&node.parse.module)
        .into_iter()
        .map(|declaration| (declaration.name, declaration.name_span))
        .collect::<HashMap<_, _>>();
    let import_bindings = top_level_import_bindings(&node.parse.module);
    let imports_by_specifier = imports_by_specifier(node);
    let mut provenance = ExportProvenanceMap::new();

    collect_export_provenance_from_entries(
        entries,
        node,
        provenance_by_node,
        &declarations,
        &import_bindings,
        &imports_by_specifier,
        &mut provenance,
    );

    provenance
}

fn collect_export_provenance_from_entries(
    entries: &[RecordEntry],
    node: &ModuleNode,
    provenance_by_node: &[ExportProvenanceMap],
    declarations: &HashMap<String, Span>,
    import_bindings: &HashMap<String, String>,
    imports_by_specifier: &HashMap<String, usize>,
    provenance: &mut ExportProvenanceMap,
) {
    for entry in entries {
        match entry {
            RecordEntry::Shorthand {
                name, name_span, ..
            } => {
                provenance.insert(
                    name.clone(),
                    ExportProvenance {
                        canonical_path: node.path.clone(),
                        definition_span: declarations.get(name).copied().unwrap_or(*name_span),
                    },
                );
            }
            RecordEntry::Rename {
                from,
                from_span,
                to,
                ..
            } => {
                provenance.insert(
                    to.clone(),
                    ExportProvenance {
                        canonical_path: node.path.clone(),
                        definition_span: declarations.get(from).copied().unwrap_or(*from_span),
                    },
                );
            }
            RecordEntry::Field {
                name, name_span, ..
            } => {
                provenance.insert(
                    name.clone(),
                    ExportProvenance {
                        canonical_path: node.path.clone(),
                        definition_span: *name_span,
                    },
                );
            }
            RecordEntry::Spread { value, .. } => {
                if let Some(target) =
                    static_import_target(value, import_bindings, imports_by_specifier)
                {
                    provenance.extend(provenance_by_node[target].clone());
                } else if let Some(entries) = expr_record_kind(value) {
                    collect_export_provenance_from_entries(
                        entries,
                        node,
                        provenance_by_node,
                        declarations,
                        import_bindings,
                        imports_by_specifier,
                        provenance,
                    );
                }
            }
            RecordEntry::Delete { name, .. } => {
                provenance.remove(name);
            }
            RecordEntry::FieldComputed { .. }
            | RecordEntry::DeleteComputed { .. }
            | RecordEntry::Iteration { .. }
            | RecordEntry::Open { .. }
            | RecordEntry::Element(_) => {}
        }
    }
}

fn top_level_import_bindings(module: &Module) -> HashMap<String, String> {
    module
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Binding(binding) => aven_parser::static_import_specifier(&binding.value)
                .map(|specifier| (binding.name.clone(), specifier)),
            _ => None,
        })
        .collect()
}

fn imports_by_specifier(node: &ModuleNode) -> HashMap<String, usize> {
    node.imports
        .iter()
        .filter_map(|import| {
            let target = import.target?;
            Some((import.specifier.clone(), target))
        })
        .collect()
}

fn static_import_target(
    expr: &Expr,
    import_bindings: &HashMap<String, String>,
    imports_by_specifier: &HashMap<String, usize>,
) -> Option<usize> {
    aven_parser::static_import_specifier(expr)
        .or_else(|| match &expr.kind {
            ExprKind::Name(name) | ExprKind::ComptimeName(name) => {
                import_bindings.get(name).cloned()
            }
            _ => None,
        })
        .and_then(|specifier| imports_by_specifier.get(&specifier).copied())
}

fn expr_record_kind(expr: &Expr) -> Option<&[RecordEntry]> {
    match &expr.kind {
        ExprKind::Record(entries) => Some(entries.as_slice()),
        _ => None,
    }
}

fn module_import_resolutions(
    graph: &ModuleGraph,
    node: &ModuleNode,
) -> Vec<ModuleImportResolution> {
    node.imports
        .iter()
        .map(|import| ModuleImportResolution {
            specifier: import.specifier.clone(),
            specifier_span: import.specifier_span,
            call_span: import.call_span,
            target_path: import
                .target
                .and_then(|target| graph.nodes.get(target))
                .map(|target| target.path.clone()),
            failed: import.failed,
        })
        .collect()
}

fn eval_export_for_node(node: &ModuleNode, value: Option<Value>) -> EvalExport {
    match value {
        Some(value @ Value::Record(_)) => EvalExport::Record(value),
        Some(value) => EvalExport::NotImportable {
            note: format!(
                "{} final expression evaluated to {}",
                node.file.name,
                value_type_name(&value)
            ),
        },
        None => EvalExport::NotImportable {
            note: format!("{} has no final expression to export", node.file.name),
        },
    }
}

fn final_expr(module: &Module) -> Option<&Expr> {
    match module.items.last()? {
        Item::Expr(expr) => Some(expr),
        Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::SpreadBinding(_)
        | Item::Signature(_) => None,
    }
}

fn type_span_contains(outer: Span, inner: Span) -> bool {
    let outer_end = outer.end.max(outer.start.saturating_add(1));
    let inner_end = inner.end.max(inner.start.saturating_add(1));
    inner.start >= outer.start && inner_end <= outer_end
}

fn parse_diagnostics(graph: &ModuleGraph) -> HashMap<FileId, Vec<Diagnostic>> {
    graph
        .nodes
        .iter()
        .map(|node| (node.file.id, node.parse.diagnostics.clone()))
        .collect()
}

fn reports_from_diagnostics(
    source_map: &SourceMap,
    diagnostics: HashMap<FileId, Vec<Diagnostic>>,
) -> Vec<DiagnosticReport> {
    source_map
        .files()
        .iter()
        .filter_map(|file| {
            let diagnostics = diagnostics.get(&file.id).cloned().unwrap_or_default();
            (!diagnostics.is_empty()).then(|| DiagnosticReport::new(file.id, diagnostics))
        })
        .collect()
}

fn sort_reports(reports: &mut [DiagnosticReport]) {
    reports.sort_by_key(|report| report.file_id.0);
    for report in reports {
        report.sort_by_primary_span();
    }
}

fn file_has_errors(diagnostics: &HashMap<FileId, Vec<Diagnostic>>, file_id: FileId) -> bool {
    diagnostics
        .get(&file_id)
        .is_some_and(|diagnostics| diagnostics.iter().any(Diagnostic::is_error))
}

fn merge_semantic_timing(
    name_duration: &mut Option<std::time::Duration>,
    check_duration: &mut Option<std::time::Duration>,
    semantic: &SemanticOutput,
) {
    if let Some(duration) = semantic.name_duration {
        *name_duration = Some(name_duration.unwrap_or_default() + duration);
    }
    if let Some(duration) = semantic.check_duration {
        *check_duration = Some(check_duration.unwrap_or_default() + duration);
    }
}

fn resolve_relative_path(importer: &Path, specifier: &str) -> PathBuf {
    let parent = importer.parent().unwrap_or_else(|| Path::new("."));
    let path = parent.join(specifier);
    if specifier.ends_with(".av") {
        path
    } else {
        PathBuf::from(format!("{}.av", path.to_string_lossy()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedImport {
    File(PathBuf),
    Library(PathBuf),
    RootUnavailable,
    UnknownLibrary,
    LibraryModuleMissing { library: String, tried: String },
}

fn resolve_import_target(importer: &Path, specifier: &str, roots: &ModuleRoots) -> ResolvedImport {
    if let Some(importer_specifier) = library_specifier(importer) {
        return resolve_import_from_library(&importer_specifier, specifier, roots);
    }

    if specifier.starts_with("./") || specifier.starts_with("../") {
        return ResolvedImport::File(resolve_relative_path(importer, specifier));
    }

    if is_root_prefixed_import_specifier(specifier) {
        return resolve_root_path(specifier, roots)
            .map_or(ResolvedImport::RootUnavailable, ResolvedImport::File);
    }

    resolve_library_import(specifier, roots)
}

/// Imports from inside an embedded library module resolve within the same
/// library: relative specifiers walk the library's module-specifier space, root
/// prefixes are unavailable (libraries are self-contained), and bare names go
/// through the ordinary library registry.
fn resolve_import_from_library(
    importer_specifier: &str,
    specifier: &str,
    roots: &ModuleRoots,
) -> ResolvedImport {
    if is_root_prefixed_import_specifier(specifier) {
        return ResolvedImport::RootUnavailable;
    }

    if !specifier.starts_with("./") && !specifier.starts_with("../") {
        return resolve_library_import(specifier, roots);
    }

    let library = library_name(importer_specifier).to_owned();
    let mut segments: Vec<&str> = importer_specifier.split('/').collect();
    segments.pop();
    let relative = specifier.strip_suffix(".av").unwrap_or(specifier);
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return ResolvedImport::LibraryModuleMissing {
                        library,
                        tried: format!("{specifier} (escapes the library)"),
                    };
                }
            }
            segment => segments.push(segment),
        }
    }
    let module = segments.join("/");
    if library_name(&module) == library
        && roots
            .libraries
            .get(&library)
            .is_some_and(|modules| modules.contains_key(&module))
    {
        ResolvedImport::Library(library_virtual_path(&module))
    } else {
        ResolvedImport::LibraryModuleMissing {
            library,
            tried: module,
        }
    }
}

fn resolve_library_import(specifier: &str, roots: &ModuleRoots) -> ResolvedImport {
    let module = specifier.strip_suffix(".av").unwrap_or(specifier);
    let library = library_name(module);
    let Some(modules) = roots.libraries.get(library) else {
        return ResolvedImport::UnknownLibrary;
    };
    if modules.contains_key(module) {
        ResolvedImport::Library(library_virtual_path(module))
    } else {
        ResolvedImport::LibraryModuleMissing {
            library: library.to_owned(),
            tried: module.to_owned(),
        }
    }
}

fn library_name(specifier: &str) -> &str {
    specifier.split('/').next().unwrap_or(specifier)
}

/// Virtual module-graph key for an embedded library module: `std/time` maps to
/// `std:/time` and the bare `std` root module to `std:/`. Filesystem nodes are
/// canonicalized absolute paths, so the two key spaces cannot collide, and
/// virtual keys never reach `fs::canonicalize` or disk reads.
fn library_virtual_path(specifier: &str) -> PathBuf {
    match specifier.split_once('/') {
        Some((library, rest)) => PathBuf::from(format!("{library}:/{rest}")),
        None => PathBuf::from(format!("{specifier}:/")),
    }
}

fn library_specifier(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let (library, rest) = text.split_once(":/")?;
    if library.is_empty() || library.contains('/') {
        return None;
    }
    Some(if rest.is_empty() {
        library.to_owned()
    } else {
        format!("{library}/{rest}")
    })
}

fn library_module_source<'roots>(
    roots: &'roots ModuleRoots,
    specifier: &str,
) -> Option<&'roots str> {
    roots
        .libraries
        .get(library_name(specifier))?
        .get(specifier)
        .copied()
}

fn resolve_root_path(specifier: &str, roots: &ModuleRoots) -> Option<PathBuf> {
    let (base, rest) = if let Some(rest) = specifier.strip_prefix("$/") {
        (roots.project.as_ref()?.clone(), rest)
    } else if let Some(rest) = specifier.strip_prefix("~/") {
        (roots.home.as_ref()?.clone(), rest)
    } else if let Some(rest) = specifier.strip_prefix("//") {
        if !roots.filesystem {
            return None;
        }
        (PathBuf::from("/"), rest)
    } else {
        return None;
    };

    let path = base.join(rest);
    if rest.ends_with(".av") {
        Some(path)
    } else {
        Some(PathBuf::from(format!("{}.av", path.to_string_lossy())))
    }
}

fn is_root_prefixed_import_specifier(specifier: &str) -> bool {
    specifier.starts_with("$/") || specifier.starts_with("~/") || specifier.starts_with("//")
}

fn cycle_path(stack: &[PathBuf], target: &Path) -> Vec<PathBuf> {
    let start = stack.iter().position(|path| path == target).unwrap_or(0);
    let mut cycle = stack[start..].to_vec();
    cycle.push(target.to_path_buf());
    cycle
}

fn render_cycle(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn render_span(file: &SourceFile, span: Span) -> String {
    let position = file
        .line_index()
        .offset_to_position(file.source(), span.start);
    format!(
        "{}:{}:{}",
        file.name,
        position.line + 1,
        position.character + 1
    )
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Text(_) => "Text",
        Value::Bool(_) => "Bool",
        Value::Array(_) => "Array",
        Value::Tuple(_) => "Tuple",
        Value::Set(_) => "Set",
        Value::Map(_) => "Map",
        Value::Record(_) => "Record",
        Value::Tag { .. } => "Tag",
        Value::Closure(_) => "Function",
        Value::Native(_) => "Native",
        Value::Type(_) => "Type",
        Value::Undefined => "Undefined",
        Value::Null => "Null",
    }
}

fn not_found(span: Span, specifier: &str, tried: &str) -> Diagnostic {
    Diagnostic::error(format!("module `{specifier}` was not found"))
        .with_code(codes::module::NOT_FOUND)
        .with_label(Label::primary(span, "this import could not be loaded"))
        .with_note(tried)
        .with_note("check the path, directory, and optional `.av` extension")
}

fn unsupported_root(span: Span, specifier: &str) -> Diagnostic {
    Diagnostic::error(format!("unsupported import specifier `{specifier}`"))
        .with_code(codes::module::UNSUPPORTED_ROOT)
        .with_label(Label::primary(
            span,
            "the host provides no library by this name",
        ))
        .with_note("use a local relative specifier, a root prefix, or a host-registered library")
        .with_note("versioned packages remain unsupported until package resolution lands")
}

fn root_unavailable(span: Span, specifier: &str) -> Diagnostic {
    Diagnostic::error(format!("import root is unavailable for `{specifier}`"))
        .with_code(codes::module::ROOT_UNAVAILABLE)
        .with_label(Label::primary(
            span,
            "this import root is not provided by the host",
        ))
        .with_note("the embedding host does not provide this module root")
}

fn import_cycle(span: Span, cycle: Vec<PathBuf>) -> Diagnostic {
    Diagnostic::error("module import cycle")
        .with_code(codes::module::IMPORT_CYCLE)
        .with_label(Label::primary(span, "this import closes the cycle"))
        .with_note(render_cycle(&cycle))
        .with_note("break the cycle by moving shared bindings into another module")
}

fn import_has_errors(span: Span, specifier: &str) -> Diagnostic {
    Diagnostic::error(format!("imported module `{specifier}` has errors"))
        .with_code(codes::module::IMPORT_HAS_ERRORS)
        .with_label(Label::primary(
            span,
            "this import depends on a module with errors",
        ))
        .with_note("fix the imported module before checking or running this file")
}

fn not_importable(span: Span, specifier: &str, note: &str) -> Diagnostic {
    Diagnostic::error(format!("module `{specifier}` is not importable"))
        .with_code(codes::module::NOT_IMPORTABLE)
        .with_label(Label::primary(
            span,
            "this import needs the target to export a record",
        ))
        .with_note(note)
        .with_note("end the target file with a literal record of exported bindings")
}
