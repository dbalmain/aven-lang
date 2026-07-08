use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use aven_check::{ModuleImports as CheckModuleImports, RowTail, Type};
use aven_core::{Diagnostic, DiagnosticReport, FileId, Label, SourceFile, SourceMap, Span, codes};
use aven_eval::{ModuleImports as EvalModuleImports, Value};
use aven_parser::{Expr, ExprKind, Item, Literal, Module, ParseOutput, decode_string_literal};

use crate::{
    HostGlobals, PhaseTimings, SemanticOutput, analyze_semantics_with_host_globals_and_imports,
};

#[derive(Debug)]
pub struct ModuleCheckOutput {
    pub source_map: SourceMap,
    pub reports: Vec<DiagnosticReport>,
    pub timings: PhaseTimings,
}

#[derive(Debug)]
pub struct ModuleEvalOutput {
    pub source_map: SourceMap,
    pub reports: Vec<DiagnosticReport>,
    pub value: Option<Value>,
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
    call_span: Span,
    target: Option<usize>,
    failed: bool,
}

#[derive(Debug, Clone)]
enum CheckExport {
    Record(Type),
    HasErrors,
    NotImportable { note: String },
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
    let total_start = Instant::now();
    let graph = ModuleGraph::load(path)?;
    let mut diagnostics = parse_diagnostics(&graph);
    let mut exports = vec![CheckExport::HasErrors; graph.nodes.len()];
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
            check_export_for_node(&graph.nodes[node_id], &semantic)
        };
        diagnostics
            .entry(file_id)
            .or_default()
            .extend(semantic.diagnostics);

        if semantic_has_errors || file_has_errors(&diagnostics, file_id) {
            exports[node_id] = CheckExport::HasErrors;
            continue;
        }

        exports[node_id] = export;
    }

    let mut reports = reports_from_diagnostics(&graph.source_map, diagnostics);
    sort_reports(&mut reports);
    Ok(ModuleCheckOutput {
        source_map: graph.source_map,
        reports,
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
    let graph = ModuleGraph::load(path)?;
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
    fn load(entry_path: &Path) -> io::Result<Self> {
        let entry_path = fs::canonicalize(entry_path)?;
        let mut graph = Self {
            source_map: SourceMap::new(),
            nodes: Vec::new(),
            by_path: HashMap::new(),
            order: Vec::new(),
        };
        let mut states = HashMap::new();
        let mut stack = Vec::new();
        graph.load_module(&entry_path, &mut states, &mut stack)?;
        Ok(graph)
    }

    fn load_module(
        &mut self,
        path: &Path,
        states: &mut HashMap<PathBuf, VisitState>,
        stack: &mut Vec<PathBuf>,
    ) -> io::Result<usize> {
        if let Some(node_id) = self.by_path.get(path).copied() {
            return Ok(node_id);
        }

        let file = self.load_source(path)?;
        let parse = aven_parser::parse_source(&file);
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
            .map(|call| self.resolve_import(node_id, call, states, stack))
            .collect::<io::Result<Vec<_>>>()?;
        self.nodes[node_id].imports = imports;

        stack.pop();
        states.insert(path.to_path_buf(), VisitState::Loaded);
        self.order.push(node_id);
        Ok(node_id)
    }

    fn load_source(&mut self, path: &Path) -> io::Result<SourceFile> {
        let source = fs::read_to_string(path)?;
        let name = path.display().to_string();
        let id = self
            .source_map
            .add(name.clone(), Some(path.to_path_buf()), source);
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
        states: &mut HashMap<PathBuf, VisitState>,
        stack: &mut Vec<PathBuf>,
    ) -> io::Result<ImportRef> {
        let Some(specifier) = call.specifier else {
            return Ok(ImportRef {
                specifier: String::new(),
                call_span: call.call_span,
                target: None,
                failed: false,
            });
        };

        if !is_relative_import_specifier(&specifier) {
            self.nodes[importer]
                .parse
                .diagnostics
                .push(unsupported_root(call.specifier_span, &specifier));
            return Ok(ImportRef {
                specifier,
                call_span: call.call_span,
                target: None,
                failed: false,
            });
        }

        let resolved = resolve_relative_path(&self.nodes[importer].path, &specifier);
        let Ok(canonical) = fs::canonicalize(&resolved) else {
            self.nodes[importer].parse.diagnostics.push(not_found(
                call.specifier_span,
                &specifier,
                &resolved,
            ));
            return Ok(ImportRef {
                specifier,
                call_span: call.call_span,
                target: None,
                failed: true,
            });
        };

        if states.get(&canonical) == Some(&VisitState::Loading) {
            self.nodes[importer].parse.diagnostics.push(import_cycle(
                call.specifier_span,
                cycle_path(stack, &canonical),
            ));
            return Ok(ImportRef {
                specifier,
                call_span: call.call_span,
                target: self.by_path.get(&canonical).copied(),
                failed: true,
            });
        }

        let target = self.load_module(&canonical, states, stack)?;
        Ok(ImportRef {
            specifier,
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
            Some(CheckExport::Record(ty)) if !import.failed => {
                imports.insert(import.specifier.clone(), ty.clone());
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

fn check_export_for_node(node: &ModuleNode, semantic: &SemanticOutput) -> CheckExport {
    let Some(final_expr) = final_expr(&node.parse.module) else {
        return CheckExport::NotImportable {
            note: format!("{} has no final expression to export", node.file.name),
        };
    };

    let Some(ty) = semantic
        .inferred_types
        .iter()
        .filter(|inferred| type_span_contains(inferred.name_span, final_expr.span))
        .min_by_key(|inferred| inferred.name_span.len())
        .map(|inferred| inferred.ty.clone())
    else {
        return CheckExport::NotImportable {
            note: format!(
                "{} final expression at {} is not a statically-known record",
                node.file.name,
                render_span(&node.file, final_expr.span)
            ),
        };
    };

    if matches!(&ty, Type::Record(row) if row.tail == RowTail::Closed) {
        CheckExport::Record(ty)
    } else {
        CheckExport::NotImportable {
            note: format!(
                "{} final expression at {} has type `{}`",
                node.file.name,
                render_span(&node.file, final_expr.span),
                ty.render()
            ),
        }
    }
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
        Item::Binding(_) | Item::Signature(_) => None,
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

fn is_relative_import_specifier(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
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

fn not_found(span: Span, specifier: &str, resolved: &Path) -> Diagnostic {
    Diagnostic::error(format!("module `{specifier}` was not found"))
        .with_code(codes::module::NOT_FOUND)
        .with_label(Label::primary(
            span,
            "this relative import could not be loaded",
        ))
        .with_note(format!("tried {}", resolved.display()))
        .with_note("check the path, directory, and optional `.av` extension")
}

fn unsupported_root(span: Span, specifier: &str) -> Diagnostic {
    Diagnostic::error(format!("unsupported import specifier `{specifier}`"))
        .with_code(codes::module::UNSUPPORTED_ROOT)
        .with_label(Label::primary(
            span,
            "this import root is not supported in this milestone",
        ))
        .with_note("use a local relative specifier beginning with `./` or `../`")
        .with_note("`$/`, `~/`, `//`, standard libraries, and packages are deferred to Milestone Z")
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
