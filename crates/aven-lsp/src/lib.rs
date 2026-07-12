use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CompletionItem, CompletionItemKind,
    CompletionOptions, CompletionParams, CompletionResponse, CompletionTextEdit, Diagnostic,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, Location, MarkupContent, MarkupKind,
    MessageType, NumberOrString, OneOf, ParameterInformation, ParameterLabel, Position, Range,
    RenameParams, SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelp,
    SignatureHelpOptions, SignatureHelpParams, SignatureInformation, SymbolKind,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

use aven_core::{
    Diagnostic as AvenDiagnostic, FileId, Severity, SourceFile, SourcePosition, Span, codes,
};
use aven_parser::RecordEntry;

mod semantic_tokens;

pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        store: Arc::default(),
        pending_semantic: Arc::default(),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}

#[derive(Debug)]
struct Backend {
    client: Client,
    store: Arc<Mutex<DocumentStore>>,
    pending_semantic: Arc<Mutex<HashMap<Url, JoinHandle<()>>>>,
}

const SEMANTIC_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Default)]
struct DocumentStore {
    file_ids: HashMap<Url, FileId>,
    database: aven_compiler::CompilerDatabase<Url>,
    module_graphs: HashMap<Url, Arc<DocumentModuleGraph>>,
}

#[derive(Debug, Clone)]
struct DocumentModuleGraph {
    revision: aven_compiler::Revision,
    entry_path: PathBuf,
    nodes: HashMap<PathBuf, ModuleNodeCache>,
}

#[derive(Debug, Clone)]
struct ModuleNodeCache {
    file: SourceFile,
    imports: Vec<aven_compiler::ModuleImportResolution>,
    export_provenance: aven_compiler::ExportProvenanceMap,
    interface: Option<aven_compiler::ModuleInterface>,
}

#[derive(Debug)]
struct DocumentSemanticAnalysis {
    diagnostics: Vec<AvenDiagnostic>,
    inferred_types: Vec<aven_compiler::InferredType>,
    type_definitions: HashMap<String, aven_compiler::Type>,
    module_graph: Option<DocumentModuleGraph>,
}

impl DocumentStore {
    fn set_document(&mut self, uri: Url, version: i32, text: String) -> FileId {
        let file_id = self.file_id_for(&uri);
        let revision = aven_compiler::Revision::from(version);

        if self.database.needs_update(&uri, revision, &text) {
            let file = SourceFile::new(file_id, source_name(&uri), uri.to_file_path().ok(), text);
            self.module_graphs.remove(&uri);
            self.database.set_document(uri, revision, file);
        }

        file_id
    }

    fn document(&self, uri: &Url) -> Option<Arc<ParsedDocument>> {
        self.database.document(uri)
    }

    fn semantic_input(
        &self,
        uri: &Url,
    ) -> Option<(Arc<ParsedDocument>, aven_compiler::SourceOverlay)> {
        Some((self.document(uri)?, self.source_overlay()))
    }

    fn source_overlay(&self) -> aven_compiler::SourceOverlay {
        let mut overlay = aven_compiler::SourceOverlay::new();

        for uri in self.file_ids.keys() {
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let Ok(path) = fs::canonicalize(path) else {
                continue;
            };
            let Some(document) = self.database.document(uri) else {
                continue;
            };

            overlay.insert(path, document.source().to_owned());
        }

        overlay
    }

    fn set_semantic(
        &mut self,
        uri: &Url,
        version: i32,
        analysis: DocumentSemanticAnalysis,
    ) -> bool {
        let revision = aven_compiler::Revision::from(version);
        let Some(document) = self.database.set_semantic(
            uri,
            revision,
            analysis.diagnostics,
            analysis.inferred_types,
            analysis.type_definitions,
        ) else {
            return false;
        };

        if let Some(module_graph) = analysis.module_graph
            && module_graph.revision == document.revision()
        {
            self.module_graphs
                .insert(uri.clone(), Arc::new(module_graph));
        }

        true
    }

    fn module_graph(&self, uri: &Url) -> Option<Arc<DocumentModuleGraph>> {
        let document = self.document(uri)?;
        let graph = self.module_graphs.get(uri)?;
        (graph.revision == document.revision()).then(|| Arc::clone(graph))
    }

    fn file_id_for(&mut self, uri: &Url) -> FileId {
        if let Some(id) = self.file_ids.get(uri).copied() {
            return id;
        }

        let id = FileId(self.file_ids.len());
        self.file_ids.insert(uri.clone(), id);
        id
    }
}

fn source_name(uri: &Url) -> String {
    uri.to_file_path()
        .ok()
        .map_or_else(|| uri.to_string(), |path| path.display().to_string())
}

type ParsedDocument = aven_compiler::DocumentSnapshot;

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_owned(), "@".to_owned(), "\"".to_owned()]),
                    ..CompletionOptions::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_owned(), ",".to_owned()]),
                    ..SignatureHelpOptions::default()
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(SemanticTokensServerCapabilities::from(
                    SemanticTokensOptions {
                        work_done_progress_options: Default::default(),
                        legend: semantic_tokens::legend(),
                        range: None,
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                    },
                )),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Aven language server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        if is_interface_cache_document(&uri) {
            return;
        }
        let version = params.text_document.version;
        let text = params.text_document.text;

        self.set_document(uri.clone(), version, text);
        self.publish_diagnostics(uri.clone()).await;
        self.schedule_semantic_diagnostics(uri, version);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };

        let uri = params.text_document.uri;
        if is_interface_cache_document(&uri) {
            return;
        }
        let version = params.text_document.version;
        let text = change.text;

        self.set_document(uri.clone(), version, text);
        self.publish_diagnostics(uri.clone()).await;
        self.schedule_semantic_diagnostics(uri, version);
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let Some(document) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };

        let Ok(formatted) =
            aven_fmt::format_parsed_source(document.source(), document.parse_output())
        else {
            return Ok(None);
        };

        if formatted == document.source() {
            return Ok(Some(Vec::new()));
        }

        Ok(Some(vec![TextEdit {
            range: full_document_range(&document),
            new_text: formatted,
        }]))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(document) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };

        Ok(Some(DocumentSymbolResponse::Nested(document_symbols(
            &document,
        ))))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(document) = self.document_with_semantics(&uri) else {
            return Ok(None);
        };
        let module_graph = self.module_graph(&uri);

        Ok(
            definition_location(&document, uri, position, module_graph.as_deref())
                .map(GotoDefinitionResponse::Scalar),
        )
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(document) = self.document_with_semantics(&uri) else {
            return Ok(None);
        };

        Ok(hover_at_position(&document, position))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(document) = self.document_with_semantics(&uri) else {
            return Ok(None);
        };

        Ok(Some(CompletionResponse::Array(
            completion_at_position_for_uri(&document, &uri, position),
        )))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(document) = self.document(&uri) else {
            return Ok(None);
        };

        let mut actions = spread_overwrite_code_actions(&document, &uri, &params.context);
        actions.extend(unused_result_code_actions(&uri, &params.context));
        if actions.is_empty() {
            return Ok(None);
        }

        Ok(Some(actions))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(document) = self.document_with_semantics(&uri) else {
            return Ok(None);
        };

        Ok(signature_help_at_position(&document, position))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Some(document) = self.document_with_semantics(&params.text_document.uri) else {
            return Ok(None);
        };

        Ok(Some(inlay_hints_in_range(&document, params.range)))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(document) = self.document(&uri) else {
            return Ok(None);
        };

        Ok(rename_workspace_edit(
            &document,
            uri,
            position,
            params.new_name,
        ))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(document) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };

        Ok(Some(SemanticTokensResult::Tokens(semantic_tokens::tokens(
            &document,
        ))))
    }
}

impl Backend {
    fn set_document(&self, uri: Url, version: i32, text: String) {
        if let Ok(mut store) = self.store.lock() {
            store.set_document(uri, version, text);
        }
    }

    fn document(&self, uri: &Url) -> Option<Arc<ParsedDocument>> {
        // A poisoned mutex degrades to "document missing" rather than crashing the LSP.
        self.store.lock().ok().and_then(|store| store.document(uri))
    }

    fn module_graph(&self, uri: &Url) -> Option<Arc<DocumentModuleGraph>> {
        self.store
            .lock()
            .ok()
            .and_then(|store| store.module_graph(uri))
    }

    /// Fetch the document with semantics for its *current* revision. The
    /// debounced task normally fills this, but type-directed requests can arrive
    /// first; compute once for the revision and store the result for later
    /// requests.
    fn document_with_semantics(&self, uri: &Url) -> Option<Arc<ParsedDocument>> {
        let (document, overlay) = {
            let store = self.store.lock().ok()?;
            let document = store.document(uri)?;
            let needs_graph = uri.to_file_path().is_ok();
            if document.has_semantic() && (!needs_graph || store.module_graph(uri).is_some()) {
                return Some(document);
            }
            (document, store.source_overlay())
        };

        let version = document.revision().as_i32();
        let analysis = analyze_document_semantics_for_uri(uri, &document, &overlay);
        self.store.lock().ok().and_then(|mut store| {
            if !store.set_semantic(uri, version, analysis) {
                return None;
            }

            store.document(uri)
        })
    }

    fn schedule_semantic_diagnostics(&self, uri: Url, version: i32) {
        self.cancel_pending_semantic_diagnostics(&uri);

        // Run semantic analysis even when the parse has errors: the recovered
        // tree still yields inferred types for the valid parts, which powers
        // type-directed completion and hover mid-edit. The compiler suppresses
        // semantic diagnostics while parse errors exist, so this publishes only
        // the parse diagnostics, as before.
        let client = self.client.clone();
        let store = Arc::clone(&self.store);
        let task_uri = uri.clone();
        let handle = tokio::spawn(async move {
            sleep(SEMANTIC_DEBOUNCE).await;
            publish_semantic_diagnostics(client, store, task_uri, version).await;
        });

        if let Ok(mut pending) = self.pending_semantic.lock() {
            pending.insert(uri, handle);
        }
    }

    fn cancel_pending_semantic_diagnostics(&self, uri: &Url) {
        if let Ok(mut pending) = self.pending_semantic.lock()
            && let Some(previous) = pending.remove(uri)
        {
            previous.abort();
        }
    }

    async fn publish_diagnostics(&self, uri: Url) {
        let Some(document) = self.document(&uri) else {
            return;
        };
        let version = document.revision().as_i32();

        self.client
            .publish_diagnostics(uri, document_diagnostics(&document), Some(version))
            .await;
    }
}

async fn publish_semantic_diagnostics(
    client: Client,
    store: Arc<Mutex<DocumentStore>>,
    uri: Url,
    version: i32,
) {
    let Some((document, overlay)) = store
        .lock()
        .ok()
        .and_then(|store| store.semantic_input(&uri))
    else {
        return;
    };

    if document.revision() != aven_compiler::Revision::from(version) {
        return;
    }

    let analysis = analyze_document_semantics_for_uri(&uri, &document, &overlay);
    let Some(document) = store.lock().ok().and_then(|mut store| {
        if !store.set_semantic(&uri, version, analysis) {
            return None;
        }

        store.document(&uri)
    }) else {
        return;
    };

    client
        .publish_diagnostics(uri, document_diagnostics(&document), Some(version))
        .await;
}

#[cfg(test)]
fn analyze_document_semantics(document: &ParsedDocument) -> aven_compiler::SemanticOutput {
    // Single-file: public surface only (no bare capability globals).
    let globals = aven_host::standard_public_check_host_globals();
    aven_compiler::analyze_semantics_with_host_globals(document.parse_output(), &globals)
}

fn analyze_document_semantics_for_uri(
    uri: &Url,
    document: &ParsedDocument,
    overlay: &aven_compiler::SourceOverlay,
) -> DocumentSemanticAnalysis {
    // Module graph needs the full set so `std/clock`/`std/zones` can pun
    // library-only names; per-node filtering strips them from user modules.
    let full_globals = aven_host::standard_check_host_globals();
    if let Some(analysis) = module_semantics_for_document(uri, document, &full_globals, overlay) {
        return analysis;
    }

    // Pathless/untitled: public surface only — no bare `now`/`zone`.
    let public_globals = aven_host::standard_public_check_host_globals();
    let semantic = aven_compiler::analyze_semantics_with_host_globals(
        document.parse_output(),
        &public_globals,
    );
    DocumentSemanticAnalysis {
        diagnostics: semantic.diagnostics,
        inferred_types: semantic.inferred_types,
        type_definitions: semantic.type_definitions,
        module_graph: None,
    }
}

/// Module roots for file-backed documents: filesystem discovery plus the
/// embedded standard library, matching the CLI's `check`/`run` wiring.
fn discover_module_roots(entry: &Path) -> aven_compiler::ModuleRoots {
    aven_compiler::ModuleRoots::discover(entry)
        .with_library(
            aven_host::STD_LIBRARY_NAME,
            aven_host::standard_std_library(),
        )
        .with_library_only_global_names(aven_host::standard_library_only_global_names())
}

fn module_semantics_for_document(
    uri: &Url,
    document: &ParsedDocument,
    globals: &aven_compiler::HostGlobals,
    overlay: &aven_compiler::SourceOverlay,
) -> Option<DocumentSemanticAnalysis> {
    let entry_path = fs::canonicalize(uri.to_file_path().ok()?).ok()?;
    let roots = discover_module_roots(&entry_path);
    let output =
        aven_compiler::check_path_with_host_globals_and_overlay_and_entry_parse_with_roots(
            &entry_path,
            globals,
            overlay,
            Some(document.parse_output()),
            &roots,
        )
        .ok()?;
    let entry_node = output
        .nodes
        .iter()
        .find(|node| node.canonical_path == entry_path)?;
    let diagnostics = output
        .reports
        .iter()
        .find(|report| report.file_id == entry_node.file.id)
        .map_or_else(Vec::new, |report| report.diagnostics.clone());
    let parse_diagnostics = document.parse_diagnostics();
    let diagnostics = diagnostics
        .into_iter()
        .filter(|diagnostic| !parse_diagnostics.contains(diagnostic))
        .collect();
    let inferred_types = entry_node.semantic.inferred_types.clone();
    let type_definitions = entry_node.semantic.type_definitions.clone();
    let module_graph = module_graph_cache(document.revision(), entry_path, output.nodes);

    Some(DocumentSemanticAnalysis {
        diagnostics,
        inferred_types,
        type_definitions,
        module_graph: Some(module_graph),
    })
}

fn module_graph_cache(
    revision: aven_compiler::Revision,
    entry_path: PathBuf,
    nodes: Vec<aven_compiler::ModuleNodeCheckOutput>,
) -> DocumentModuleGraph {
    let nodes = nodes
        .into_iter()
        .map(|node| {
            (
                node.canonical_path,
                ModuleNodeCache {
                    file: node.file,
                    imports: node.imports,
                    export_provenance: node.export_provenance,
                    interface: node.interface,
                },
            )
        })
        .collect();

    DocumentModuleGraph {
        revision,
        entry_path,
        nodes,
    }
}

fn document_diagnostics(document: &ParsedDocument) -> Vec<Diagnostic> {
    document
        .diagnostics()
        .map(|diagnostic| to_lsp_diagnostic(document, diagnostic))
        .collect()
}

fn to_lsp_diagnostic(document: &ParsedDocument, diagnostic: &AvenDiagnostic) -> Diagnostic {
    let span = diagnostic
        .labels
        .first()
        .map(|label| label.span)
        .unwrap_or_else(|| Span::point(0));

    Diagnostic {
        range: span_to_range(document, span),
        severity: Some(match diagnostic.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Note => DiagnosticSeverity::INFORMATION,
        }),
        code: diagnostic
            .code
            .clone()
            .map(tower_lsp::lsp_types::NumberOrString::String),
        source: Some("aven".to_owned()),
        message: diagnostic.message.clone(),
        related_information: None,
        tags: None,
        code_description: None,
        data: None,
    }
}

fn spread_overwrite_code_actions(
    document: &ParsedDocument,
    uri: &Url,
    context: &CodeActionContext,
) -> Vec<CodeActionOrCommand> {
    context
        .diagnostics
        .iter()
        .filter_map(|diagnostic| {
            if !is_duplicate_spread_label_diagnostic(diagnostic) {
                return None;
            }

            let offset = position_to_offset(document, diagnostic.range.start)?;
            let source_at_range = document.source().get(offset..)?;
            if !source_at_range.starts_with("..") || source_at_range.starts_with(":..") {
                return None;
            }

            let edit = TextEdit {
                range: Range {
                    start: diagnostic.range.start,
                    end: diagnostic.range.start,
                },
                new_text: ":".to_owned(),
            };

            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Overwrite-merge spread with `:..`".to_owned(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
                    document_changes: None,
                    change_annotations: None,
                }),
                is_preferred: Some(true),
                ..CodeAction::default()
            }))
        })
        .collect()
}

fn is_duplicate_spread_label_diagnostic(diagnostic: &Diagnostic) -> bool {
    matches!(
        diagnostic.code.as_ref(),
        Some(NumberOrString::String(code)) if code == codes::ty::DUPLICATE_SPREAD_LABEL
    )
}

fn unused_result_code_actions(uri: &Url, context: &CodeActionContext) -> Vec<CodeActionOrCommand> {
    context
        .diagnostics
        .iter()
        .filter_map(|diagnostic| {
            if !is_unused_result_diagnostic(diagnostic) {
                return None;
            }

            let edit = TextEdit {
                range: Range {
                    start: diagnostic.range.end,
                    end: diagnostic.range.end,
                },
                new_text: "?!".to_owned(),
            };

            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Unwrap with `?!`".to_owned(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
                    document_changes: None,
                    change_annotations: None,
                }),
                is_preferred: Some(true),
                ..CodeAction::default()
            }))
        })
        .collect()
}

fn is_unused_result_diagnostic(diagnostic: &Diagnostic) -> bool {
    matches!(
        diagnostic.code.as_ref(),
        Some(NumberOrString::String(code)) if code == codes::ty::UNUSED_RESULT
    )
}

fn document_symbols(document: &ParsedDocument) -> Vec<DocumentSymbol> {
    document
        .declarations()
        .iter()
        .map(|declaration| declaration_symbol(document, declaration))
        .collect()
}

#[allow(deprecated)]
fn declaration_symbol(
    document: &ParsedDocument,
    declaration: &aven_parser::Declaration,
) -> DocumentSymbol {
    DocumentSymbol {
        name: declaration.name.clone(),
        detail: declaration_detail(declaration),
        kind: symbol_kind(declaration),
        tags: None,
        deprecated: None,
        range: span_to_range(document, declaration.span),
        selection_range: span_to_range(document, declaration.name_span),
        children: None,
    }
}

fn declaration_detail(declaration: &aven_parser::Declaration) -> Option<String> {
    if declaration.is_annotated {
        return Some("binding with signature".to_owned());
    }

    if declaration.kind == aven_parser::DeclarationKind::Signature {
        return Some("signature".to_owned());
    }

    None
}

fn symbol_kind(declaration: &aven_parser::Declaration) -> SymbolKind {
    if declaration.phase == aven_parser::DeclarationPhase::Comptime {
        return SymbolKind::STRUCT;
    }

    match declaration.kind {
        aven_parser::DeclarationKind::Function | aven_parser::DeclarationKind::Signature => {
            SymbolKind::FUNCTION
        }
        aven_parser::DeclarationKind::Binding => SymbolKind::VARIABLE,
    }
}

#[cfg(test)]
fn completion_at_position(document: &ParsedDocument, position: Position) -> Vec<CompletionItem> {
    if let Some(uri) = document
        .file()
        .path
        .as_ref()
        .and_then(|path| Url::from_file_path(path).ok())
    {
        return completion_at_position_for_uri(document, &uri, position);
    }

    completion_at_position_without_import_paths(document, position)
}

fn completion_at_position_for_uri(
    document: &ParsedDocument,
    uri: &Url,
    position: Position,
) -> Vec<CompletionItem> {
    if let Some(items) = import_specifier_completion_at_position(document, uri, position) {
        return items;
    }

    completion_at_position_without_import_paths(document, position)
}

fn completion_at_position_without_import_paths(
    document: &ParsedDocument,
    position: Position,
) -> Vec<CompletionItem> {
    if let Some(items) = field_completion_at_position(document, position) {
        return items;
    }

    if let Some(items) = construction_completion_at_position(document, position) {
        return items;
    }

    if let Some(items) = argument_literal_completion_at_position(document, position) {
        return items;
    }

    identifier_completion_at_position(document, position)
}

fn import_specifier_completion_at_position(
    document: &ParsedDocument,
    uri: &Url,
    position: Position,
) -> Option<Vec<CompletionItem>> {
    let offset = position_to_offset(document, position)?;
    let token_index = significant_tokens(document).iter().position(|token| {
        matches!(token.kind, aven_parser::TokenKind::StringLiteral(_))
            && offset > token.span.start
            && offset <= token.span.end
    })?;
    let significant = significant_tokens(document);
    let token = significant[token_index];

    if !is_import_string_argument(&significant, token_index) {
        return None;
    }

    let content_start = token.span.start.saturating_add(1);
    let content_end = token.span.end.saturating_sub(1);
    let cursor = offset.clamp(content_start, content_end);
    let typed = document.source().get(content_start..cursor)?;

    let (directory_specifier, prefix) = typed
        .rsplit_once('/')
        .map_or(("", typed), |(directory, prefix)| {
            (&typed[..directory.len() + 1], prefix)
        });
    let replace_start = cursor.saturating_sub(prefix.len());
    let range = exact_offset_range(document, Span::new(replace_start, cursor));

    if !typed.starts_with("./") && !typed.starts_with("../") && !typed.starts_with("$/") {
        // Bare specifiers resolve through host-registered libraries: offer
        // library names first, then the chosen library's module paths.
        if typed.starts_with("~/") || typed.starts_with("//") {
            return Some(Vec::new());
        }
        return Some(library_import_completions(
            directory_specifier,
            prefix,
            range,
        ));
    }

    let search_dir = if let Some(rest) = directory_specifier.strip_prefix("$/") {
        aven_compiler::ModuleRoots::discover(&uri.to_file_path().ok()?)
            .project?
            .join(rest)
    } else {
        uri.to_file_path().ok()?.parent()?.join(directory_specifier)
    };

    let mut items = Vec::new();
    for entry in fs::read_dir(search_dir).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(prefix) {
            continue;
        }

        if file_type.is_dir() {
            items.push(import_path_completion_item(
                format!("{name}/"),
                CompletionItemKind::FOLDER,
                range,
            ));
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "av")
            && let Some(stem) = entry.path().file_stem().and_then(|stem| stem.to_str())
        {
            items.push(import_path_completion_item(
                stem.to_owned(),
                CompletionItemKind::FILE,
                range,
            ));
        }
    }

    items.sort_by(|left, right| left.label.cmp(&right.label));
    Some(items)
}

/// Completions for bare (library) import specifiers: with nothing before the
/// cursor segment, registered library names; after `std/`, that library's
/// module paths.
fn library_import_completions(
    directory_specifier: &str,
    prefix: &str,
    range: Range,
) -> Vec<CompletionItem> {
    let libraries = [(
        aven_host::STD_LIBRARY_NAME,
        aven_host::standard_std_library(),
    )];
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    if directory_specifier.is_empty() {
        for (name, _) in libraries {
            if name.starts_with(prefix) && seen.insert(name.to_owned()) {
                items.push(import_path_completion_item(
                    name.to_owned(),
                    CompletionItemKind::MODULE,
                    range,
                ));
            }
        }
    } else {
        for (_, modules) in &libraries {
            for module in modules.keys() {
                let Some(remainder) = module.strip_prefix(directory_specifier) else {
                    continue;
                };
                let (segment, is_directory) = remainder
                    .split_once('/')
                    .map_or((remainder, false), |(segment, _)| (segment, true));
                if segment.is_empty() || !segment.starts_with(prefix) {
                    continue;
                }
                let (label, kind) = if is_directory {
                    (format!("{segment}/"), CompletionItemKind::FOLDER)
                } else {
                    (segment.to_owned(), CompletionItemKind::FILE)
                };
                if seen.insert(label.clone()) {
                    items.push(import_path_completion_item(label, kind, range));
                }
            }
        }
    }

    items.sort_by(|left, right| left.label.cmp(&right.label));
    items
}

fn is_import_string_argument(tokens: &[&aven_parser::Token], string_index: usize) -> bool {
    let Some(open_index) = string_index.checked_sub(1) else {
        return false;
    };
    let Some(callee_index) = string_index.checked_sub(2) else {
        return false;
    };

    matches!(tokens[open_index].kind, aven_parser::TokenKind::OpenParen)
        && matches!(&tokens[callee_index].kind, aven_parser::TokenKind::Identifier(name) if name == "import")
}

fn import_path_completion_item(
    label: String,
    kind: CompletionItemKind,
    range: Range,
) -> CompletionItem {
    CompletionItem {
        label: label.clone(),
        kind: Some(kind),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: label,
        })),
        ..CompletionItem::default()
    }
}

fn identifier_completion_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    if let Some(offset) = position_to_offset(document, position) {
        for binding in aven_parser::visible_local_bindings(
            &document.parse_output().module,
            Span::point(offset),
        )
        .into_iter()
        .rev()
        {
            push_completion_item(
                &mut items,
                &mut seen,
                completion_item_for_binding(document, binding.name, binding.span),
            );
        }
    }

    for declaration in document.declarations() {
        push_completion_item(
            &mut items,
            &mut seen,
            completion_item_for_declaration(document, declaration),
        );
    }

    for name in BUILTIN_TYPE_NAMES {
        push_completion_item(
            &mut items,
            &mut seen,
            CompletionItem {
                label: (*name).to_owned(),
                kind: Some(CompletionItemKind::CLASS),
                ..CompletionItem::default()
            },
        );
    }

    // Host/library globals (e.g. `logger`, `writeLine`) are bound in the value
    // environment but have no in-document declaration, so offer them too. Pushed
    // last, after locals and top-level declarations have claimed their names, so
    // a user binding of the same name shadows the global. Capability internals
    // (`now`, `zone`) are not offered bare — import `std/clock` / `std/zones`.
    for (name, ty) in aven_host::standard_public_check_host_globals().types {
        push_completion_item(
            &mut items,
            &mut seen,
            CompletionItem {
                label: name,
                kind: Some(completion_kind_for_type(Some(&ty))),
                detail: Some(ty.render()),
                ..CompletionItem::default()
            },
        );
    }

    items
}

fn field_completion_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<Vec<CompletionItem>> {
    let access = field_access_at_position(document, position)?;
    let receiver_type = access
        .operator_span
        .start
        .checked_sub(1)
        .and_then(|offset| document.type_at(Span::point(offset)).cloned())
        .or_else(|| {
            // The receiver's type may not be recorded — a host-record global whose
            // field type is comptime-resolved (e.g. `File`, whose `open` returns a
            // `Deferred` base type) is not a fully resolved value type, so it never
            // lands in the type table. Fall back to an in-document definition, then
            // to the host global directly, mirroring hover/signature-help.
            let receiver = access.receiver.as_ref()?;
            definition_span_for_identifier(document, receiver)
                .and_then(|span| document.type_at(span).cloned())
                .or_else(|| host_global_type(&receiver.name))
        });

    // Fall back to type statics (`Json.`/`Map.`) when the receiver resolves to a
    // static-carrying type value rather than a value receiver.
    let fields = receiver_type
        .as_ref()
        .and_then(receiver_field_completion_fields)
        .or_else(|| {
            access
                .receiver
                .as_ref()
                .and_then(|receiver| type_statics_fields(&receiver.name))
        })?;
    let receiver_type = receiver_type.unwrap_or(aven_compiler::Type::Deferred);

    // When the receiver is itself optional/nullable, accessing a field needs
    // `?.`. If the user typed a plain `.`, offer an edit that inserts the `?`
    // alongside each field so accepting a completion yields `?.field`.
    let null_safe_edit =
        (type_is_empty_wrapped(&receiver_type) && !access.null_safe).then(|| TextEdit {
            range: exact_offset_range(document, Span::point(access.operator_span.start)),
            new_text: "?".to_owned(),
        });

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let receiver_carries_encode = receiver_type_carries_member(&receiver_type, "encode");

    for field in fields {
        let is_synthetic_encode = field.name == "encode" && !receiver_carries_encode;
        let mut item = completion_item_for_record_field(field);
        if let Some(edit) = &null_safe_edit
            && !is_synthetic_encode
        {
            item.additional_text_edits = Some(vec![edit.clone()]);
        }
        push_completion_item(&mut items, &mut seen, item);
    }

    Some(items)
}

fn type_is_empty_wrapped(ty: &aven_compiler::Type) -> bool {
    matches!(
        ty,
        aven_compiler::Type::Optional(_) | aven_compiler::Type::Nullable(_)
    )
}

fn construction_completion_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<Vec<CompletionItem>> {
    let offset = position_to_offset(document, position)?;
    let target = Span::point(offset);
    let binding = construction_binding_at_position(&document.parse_output().module.items, target)?;

    aven_parser::annotation_for_definition(&document.parse_output().module, binding.name_span)?;

    let expected = expected_type_for_construction_binding(document, binding)?;
    let (entries, kind) = match &binding.value.kind {
        aven_parser::ExprKind::Record(entries) => {
            (entries.as_slice(), ConstructionCompletionKind::RecordLabels)
        }
        aven_parser::ExprKind::Set(entries) => {
            (entries.as_slice(), ConstructionCompletionKind::Tags)
        }
        _ => return None,
    };

    if entries
        .iter()
        .any(|entry| record_entry_value_span(entry).is_some_and(|span| span.contains(target)))
    {
        return None;
    }

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    match kind {
        ConstructionCompletionKind::RecordLabels => {
            let present = entries
                .iter()
                .filter_map(record_entry_label)
                .collect::<HashSet<_>>();

            for field in aven_compiler::record_fields(expected)? {
                if present.contains(field.name.as_str()) {
                    continue;
                }

                push_completion_item(
                    &mut items,
                    &mut seen,
                    completion_item_for_record_field(field),
                );
            }
        }
        ConstructionCompletionKind::Tags => {
            let present = entries
                .iter()
                .filter_map(record_entry_tag)
                .collect::<HashSet<_>>();

            for tag in aven_compiler::variant_tags(expected)? {
                if present.contains(tag.as_str()) {
                    continue;
                }

                push_completion_item(&mut items, &mut seen, completion_item_for_variant_tag(&tag));
            }
        }
    }

    Some(items)
}

fn argument_literal_completion_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<Vec<CompletionItem>> {
    let offset = position_to_offset(document, position)?;
    let significant_tokens = significant_tokens(document);
    let call = enclosing_call_at_offset(&significant_tokens, offset)?;
    let active_parameter =
        active_parameter_for_call(&significant_tokens, call.open_index, offset) as usize;
    let (params, _) = function_signature_for_call(document, &call)?;
    let members = aven_compiler::literal_union_members(params.get(active_parameter)?)?;

    // Completing `"r"` etc. inserts the whole quoted literal. Replace any quote
    // (and partial text) the user has already typed so the result never doubles
    // the quote — including the closing quote an autopairs plugin inserts. Build
    // the range with exact offsets: `span_to_range` floors the width at 1 for
    // diagnostic highlighting, which would turn a bare insert into a replace.
    let range = exact_offset_range(
        document,
        literal_argument_replace_span(document.source(), offset),
    );

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for member in members {
        push_completion_item(
            &mut items,
            &mut seen,
            CompletionItem {
                label: member.clone(),
                kind: Some(CompletionItemKind::VALUE),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: member,
                })),
                ..CompletionItem::default()
            },
        );
    }

    Some(items)
}

/// The source range a literal-argument completion should replace: from an
/// opening quote the user has already typed (if any) through the cursor,
/// extended over a directly-following quote so an inserted `"x"` never doubles
/// the quote an autopairs plugin added. With no opening quote yet the range is
/// the empty span at the cursor (a plain insert).
fn literal_argument_replace_span(source: &str, cursor: usize) -> Span {
    let bytes = source.as_bytes();

    let mut start = cursor;
    let mut found_quote = false;
    while start > 0 {
        let byte = bytes[start - 1];
        if byte == b'"' {
            start -= 1;
            found_quote = true;
            break;
        }
        if matches!(byte, b' ' | b'\t' | b'(' | b',') {
            break;
        }
        start -= 1;
    }

    let mut end = cursor;
    if found_quote && bytes.get(end) == Some(&b'"') {
        end += 1;
    }

    Span::new(start, end)
}

fn expected_type_for_construction_binding<'a>(
    document: &'a ParsedDocument,
    binding: &aven_parser::Binding,
) -> Option<&'a aven_compiler::Type> {
    document
        .type_at(binding.name_span)
        .or_else(|| declared_type_for_definition(document, binding.name_span))
}

fn declared_type_for_definition(
    document: &ParsedDocument,
    definition: Span,
) -> Option<&aven_compiler::Type> {
    document
        .declarations()
        .iter()
        .zip(document.declaration_artifacts())
        .find(|(declaration, _)| declaration.name_span == definition)
        .and_then(|(_, artifact)| artifact.declared_type())
}

fn host_global_type(name: &str) -> Option<aven_compiler::Type> {
    aven_host::standard_public_check_host_globals()
        .types
        .into_iter()
        .find_map(|(global, ty)| (global == name).then_some(ty))
}

/// The statics carried by a type name (`Json`, `Map`), as record-like fields, so
/// completion and hover present them the same way as record fields.
fn type_statics_fields(name: &str) -> Option<Vec<aven_compiler::RecordField>> {
    aven_compiler::type_statics(&aven_host::standard_check_host_globals(), name)
}

/// Fields and format-method sugar offered on a value receiver. `decode` stays
/// Text-only; `encode` is universal sugar unless the receiver type already has
/// an `encode` member.
fn receiver_field_completion_fields(
    receiver_type: &aven_compiler::Type,
) -> Option<Vec<aven_compiler::RecordField>> {
    if matches!(
        receiver_type,
        aven_compiler::Type::Deferred
            | aven_compiler::Type::Variable(_)
            | aven_compiler::Type::Meta(_)
    ) {
        return None;
    }

    let mut fields = aven_compiler::record_fields(receiver_type).unwrap_or_default();
    if aven_compiler::is_text_type(receiver_type)
        && !fields.iter().any(|field| field.name == "decode")
    {
        fields.push(format_method_field("decode"));
    }
    if !fields.iter().any(|field| field.name == "encode") {
        fields.push(format_method_field("encode"));
    }

    (!fields.is_empty()).then_some(fields)
}

fn receiver_type_carries_member(receiver_type: &aven_compiler::Type, member: &str) -> bool {
    aven_compiler::record_fields(receiver_type)
        .is_some_and(|fields| fields.iter().any(|field| field.name == member))
}

fn format_method_field(name: &str) -> aven_compiler::RecordField {
    aven_compiler::RecordField {
        name: name.to_owned(),
        ty: format_method_type(name).unwrap_or(aven_compiler::Type::Deferred),
    }
}

/// The method-form signature for the synthetic format sugar. The checker has no
/// first-class "format type" value, so named display variables keep completion
/// details honest instead of rendering deferred parameters as `?`.
fn format_method_type(member: &str) -> Option<aven_compiler::Type> {
    let has_registered_static = aven_host::standard_check_host_globals()
        .statics
        .iter()
        .any(|(_, members)| members.iter().any(|(name, _)| name == member));
    if !has_registered_static {
        return None;
    }

    match member {
        "encode" => Some(aven_compiler::Type::Function {
            params: vec![aven_compiler::Type::Variable("fmt".to_owned())],
            result: Box::new(aven_compiler::Type::Named("Text".to_owned())),
            required: 1,
        }),
        "decode" => Some(aven_compiler::Type::Function {
            params: vec![
                aven_compiler::Type::Variable("fmt".to_owned()),
                aven_compiler::Type::Variable("target".to_owned()),
            ],
            result: Box::new(aven_compiler::Type::Variable("decoded".to_owned())),
            required: 1,
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstructionCompletionKind {
    RecordLabels,
    Tags,
}

fn construction_binding_at_position(
    items: &[aven_parser::Item],
    target: Span,
) -> Option<&aven_parser::Binding> {
    items
        .iter()
        .find_map(|item| construction_binding_in_item_at_position(item, target))
}

fn construction_binding_in_item_at_position(
    item: &aven_parser::Item,
    target: Span,
) -> Option<&aven_parser::Binding> {
    match item {
        aven_parser::Item::Binding(binding) if binding.value.span.contains(target) => {
            construction_binding_in_expr_at_position(&binding.value, target).or_else(|| {
                matches!(
                    binding.value.kind,
                    aven_parser::ExprKind::Record(_) | aven_parser::ExprKind::Set(_)
                )
                .then_some(binding)
            })
        }
        aven_parser::Item::PatternBinding(binding) => {
            construction_binding_in_expr_at_position(&binding.value, target)
        }
        aven_parser::Item::SpreadBinding(binding) => {
            construction_binding_in_expr_at_position(&binding.value, target)
        }
        aven_parser::Item::Binding(_) | aven_parser::Item::Signature(_) => None,
        aven_parser::Item::Expr(expr) => construction_binding_in_expr_at_position(expr, target),
    }
}

fn construction_binding_in_expr_at_position(
    expr: &aven_parser::Expr,
    target: Span,
) -> Option<&aven_parser::Binding> {
    if !expr.span.contains(target) {
        return None;
    }

    match &expr.kind {
        aven_parser::ExprKind::Block(items) => construction_binding_at_position(items, target),
        _ => {
            let mut found = None;
            aven_parser::walk_expr_children(expr, &mut |child| {
                if found.is_none() {
                    found = construction_binding_in_expr_at_position(child, target);
                }
            });
            found
        }
    }
}

fn record_entry_value_span(entry: &RecordEntry) -> Option<Span> {
    match entry {
        RecordEntry::Field { value, .. }
        | RecordEntry::Spread { value, .. }
        | RecordEntry::DeleteComputed { key: value, .. }
        | RecordEntry::Element(value) => Some(value.span),
        RecordEntry::FieldComputed { key, value, .. } => Some(key.span.merge(value.span)),
        RecordEntry::Iteration {
            source,
            guard,
            body,
            ..
        } => {
            let mut span = source.span;
            if let Some(guard) = guard {
                span = span.merge(guard.span);
            }
            for entry in body {
                span = span.merge(record_entry_span(entry));
            }
            Some(span)
        }
        RecordEntry::Shorthand { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::Rename { .. }
        | RecordEntry::Open { .. } => None,
    }
}

fn record_entry_label(entry: &RecordEntry) -> Option<&str> {
    match entry {
        RecordEntry::Field { name, .. } | RecordEntry::Shorthand { name, .. } => Some(name),
        RecordEntry::FieldComputed { .. }
        | RecordEntry::Spread { .. }
        | RecordEntry::Delete { .. }
        | RecordEntry::DeleteComputed { .. }
        | RecordEntry::Rename { .. }
        | RecordEntry::Iteration { .. }
        | RecordEntry::Open { .. }
        | RecordEntry::Element(_) => None,
    }
}

fn record_entry_tag(entry: &RecordEntry) -> Option<&str> {
    match entry {
        RecordEntry::Element(expr) => tag_name_from_expr(expr),
        RecordEntry::Delete { name, .. } => Some(name),
        RecordEntry::Rename { to, .. } => Some(to),
        RecordEntry::Field { .. }
        | RecordEntry::FieldComputed { .. }
        | RecordEntry::Shorthand { .. }
        | RecordEntry::Spread { .. }
        | RecordEntry::DeleteComputed { .. }
        | RecordEntry::Iteration { .. }
        | RecordEntry::Open { .. } => None,
    }
}

fn tag_name_from_expr(expr: &aven_parser::Expr) -> Option<&str> {
    match &expr.kind {
        aven_parser::ExprKind::Tag(name) => Some(name),
        aven_parser::ExprKind::Call { callee, .. } => match &callee.kind {
            aven_parser::ExprKind::Tag(name) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

fn record_entry_span(entry: &RecordEntry) -> Span {
    match entry {
        RecordEntry::Field { span, .. }
        | RecordEntry::FieldComputed { span, .. }
        | RecordEntry::Shorthand { span, .. }
        | RecordEntry::Spread { span, .. }
        | RecordEntry::Delete { span, .. }
        | RecordEntry::DeleteComputed { span, .. }
        | RecordEntry::Rename { span, .. }
        | RecordEntry::Iteration { span, .. }
        | RecordEntry::Open { span } => *span,
        RecordEntry::Element(expr) => expr.span,
    }
}

fn push_completion_item(
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    item: CompletionItem,
) {
    if seen.insert(item.label.clone()) {
        items.push(item);
    }
}

fn completion_item_for_record_field(field: aven_compiler::RecordField) -> CompletionItem {
    CompletionItem {
        label: field.name,
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(field.ty.render()),
        ..CompletionItem::default()
    }
}

fn completion_item_for_variant_tag(tag: &str) -> CompletionItem {
    CompletionItem {
        label: format!("@{tag}"),
        kind: Some(CompletionItemKind::ENUM_MEMBER),
        ..CompletionItem::default()
    }
}

fn completion_item_for_declaration(
    document: &ParsedDocument,
    declaration: &aven_parser::Declaration,
) -> CompletionItem {
    CompletionItem {
        label: declaration.name.clone(),
        kind: Some(completion_kind_for_declaration(document, declaration)),
        detail: document
            .type_at(declaration.name_span)
            .map(aven_compiler::Type::render),
        ..CompletionItem::default()
    }
}

fn completion_item_for_binding(
    document: &ParsedDocument,
    name: &str,
    name_span: Span,
) -> CompletionItem {
    CompletionItem {
        label: name.to_owned(),
        kind: Some(completion_kind_for_type(document.type_at(name_span))),
        detail: document.type_at(name_span).map(aven_compiler::Type::render),
        ..CompletionItem::default()
    }
}

fn completion_kind_for_declaration(
    document: &ParsedDocument,
    declaration: &aven_parser::Declaration,
) -> CompletionItemKind {
    if declaration.phase == aven_parser::DeclarationPhase::Comptime {
        // Uppercase/comptime declarations are type-like in the parser's phase
        // split, so completion presents them with the same class icon as builtins.
        return CompletionItemKind::CLASS;
    }

    completion_kind_for_type(document.type_at(declaration.name_span))
}

fn completion_kind_for_type(ty: Option<&aven_compiler::Type>) -> CompletionItemKind {
    if ty.and_then(aven_compiler::function_signature).is_some() {
        return CompletionItemKind::FUNCTION;
    }

    CompletionItemKind::VARIABLE
}

fn signature_help_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<SignatureHelp> {
    let offset = position_to_offset(document, position)?;
    let significant_tokens = significant_tokens(document);
    let call = enclosing_call_at_offset(&significant_tokens, offset)?;
    let (params, result) = function_signature_for_call(document, &call)?;
    let callee_label = callee_label_for_call(document, &call);
    let active_parameter = active_parameter_for_call(&significant_tokens, call.open_index, offset);

    Some(SignatureHelp {
        signatures: vec![signature_information(
            callee_label.as_deref(),
            &params,
            &result,
            active_parameter,
        )],
        active_signature: Some(0),
        active_parameter: Some(active_parameter),
    })
}

fn function_signature_for_call(
    document: &ParsedDocument,
    call: &CallAtPosition,
) -> Option<(Vec<aven_compiler::Type>, aven_compiler::Type)> {
    let callee_type = callee_type_for_call(document, call)?;
    aven_compiler::function_signature(&callee_type)
}

fn callee_type_for_call(
    document: &ParsedDocument,
    call: &CallAtPosition,
) -> Option<aven_compiler::Type> {
    if let Some(callee_type) = call
        .open_span
        .start
        .checked_sub(1)
        .and_then(|offset| document.type_at(Span::point(offset)))
    {
        return Some(callee_type.clone());
    }

    if let Some(callee) = &call.fallback_callee {
        if let Some(callee_span) = definition_span_for_identifier(document, callee) {
            return document.type_at(callee_span).cloned();
        }

        if let Some(ty) = host_global_type(&callee.name) {
            return Some(ty);
        }
    }

    call.fallback_field_callee
        .as_ref()
        .and_then(|callee| field_type_for_access(document, callee))
}

fn signature_information(
    callee_label: Option<&str>,
    params: &[aven_compiler::Type],
    result: &aven_compiler::Type,
    active_parameter: u32,
) -> SignatureInformation {
    let mut label = String::new();
    if let Some(callee_label) = callee_label {
        label.push_str(callee_label);
    }
    label.push('(');
    let mut parameters = Vec::new();

    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            label.push_str(", ");
        }

        let start = signature_label_offset(&label);
        label.push_str(&param.render());
        let end = signature_label_offset(&label);
        parameters.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([start, end]),
            documentation: None,
        });
    }

    label.push_str(") -> ");
    label.push_str(&result.render());

    SignatureInformation {
        label,
        documentation: None,
        parameters: Some(parameters),
        active_parameter: Some(active_parameter),
    }
}

fn signature_label_offset(label: &str) -> u32 {
    label.encode_utf16().count() as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallAtPosition {
    fallback_callee: Option<IdentifierAtPosition>,
    fallback_field_callee: Option<FieldAccessIdentifiers>,
    open_index: usize,
    open_span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenDelimiter {
    kind: DelimiterKind,
    call: Option<CallAtPosition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimiterKind {
    Paren,
    Bracket,
    Brace,
}

fn enclosing_call_at_offset(
    tokens: &[&aven_parser::Token],
    offset: usize,
) -> Option<CallAtPosition> {
    let mut stack = Vec::new();

    for (index, token) in tokens.iter().enumerate() {
        if token.span.start >= offset {
            break;
        }

        if let Some(kind) = opening_delimiter_kind(token) {
            let call = if kind == DelimiterKind::Paren {
                call_before_open(tokens, index)
            } else {
                None
            };

            stack.push(OpenDelimiter { kind, call });
        } else if let Some(kind) = closing_delimiter_kind(token)
            && let Some(open_index) = stack.iter().rposition(|open| open.kind == kind)
        {
            stack.truncate(open_index);
        }
    }

    stack.into_iter().rev().find_map(|open| open.call)
}

fn call_before_open(tokens: &[&aven_parser::Token], open_index: usize) -> Option<CallAtPosition> {
    let callee_index = open_index.checked_sub(1)?;
    if !token_can_end_callee_expression(tokens[callee_index]) {
        return None;
    }

    Some(CallAtPosition {
        fallback_callee: bare_identifier_callee_before_open(tokens, callee_index),
        fallback_field_callee: field_callee_before_open(tokens, callee_index),
        open_index,
        open_span: tokens[open_index].span,
    })
}

fn bare_identifier_callee_before_open(
    tokens: &[&aven_parser::Token],
    callee_index: usize,
) -> Option<IdentifierAtPosition> {
    let identifier = identifier_from_token(tokens[callee_index])?;
    if callee_index
        .checked_sub(1)
        .is_some_and(|previous| is_field_access_operator(tokens[previous]))
    {
        return None;
    }

    Some(identifier)
}

fn field_callee_before_open(
    tokens: &[&aven_parser::Token],
    callee_index: usize,
) -> Option<FieldAccessIdentifiers> {
    let field = identifier_from_token(tokens[callee_index])?;
    let operator_index = callee_index.checked_sub(1)?;
    if !is_field_access_operator(tokens[operator_index]) {
        return None;
    }

    let receiver = receiver_name_before_field_operator(tokens, operator_index)?;
    Some(FieldAccessIdentifiers { receiver, field })
}

fn token_can_end_callee_expression(token: &aven_parser::Token) -> bool {
    matches!(
        &token.kind,
        aven_parser::TokenKind::Identifier(_)
            | aven_parser::TokenKind::ComptimeIdentifier(_)
            | aven_parser::TokenKind::Number(_)
            | aven_parser::TokenKind::StringLiteral(_)
            | aven_parser::TokenKind::InterpolationEnd(_)
            | aven_parser::TokenKind::RegexLiteral(_)
            | aven_parser::TokenKind::Tag(_)
            | aven_parser::TokenKind::Keyword(_)
            | aven_parser::TokenKind::CloseParen
            | aven_parser::TokenKind::CloseBracket
            | aven_parser::TokenKind::CloseBrace
    )
}

fn callee_label_for_call(document: &ParsedDocument, call: &CallAtPosition) -> Option<String> {
    call_callee_label_span_before_open(document, call.open_span.start)
        .and_then(|span| document.source().get(span.start..span.end))
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            call.fallback_callee
                .as_ref()
                .map(|callee| callee.name.clone())
                .or_else(|| {
                    call.fallback_field_callee
                        .as_ref()
                        .map(|callee| callee.label())
                })
        })
}

fn call_callee_label_span_before_open(
    document: &ParsedDocument,
    open_start: usize,
) -> Option<Span> {
    let mut found = None;

    for item in &document.parse_output().module.items {
        collect_item_call_callee_label_span(item, open_start, &mut found);
    }

    found.map(|(span, _)| span)
}

fn collect_item_call_callee_label_span(
    item: &aven_parser::Item,
    open_start: usize,
    found: &mut Option<(Span, usize)>,
) {
    match item {
        aven_parser::Item::Binding(binding) => {
            if let Some(annotation) = &binding.annotation {
                collect_expr_call_callee_label_span(annotation, open_start, found);
            }
            collect_expr_call_callee_label_span(&binding.value, open_start, found);
        }
        aven_parser::Item::PatternBinding(binding) => {
            collect_expr_call_callee_label_span(&binding.value, open_start, found);
        }
        aven_parser::Item::SpreadBinding(binding) => {
            collect_expr_call_callee_label_span(&binding.value, open_start, found);
        }
        aven_parser::Item::Signature(signature) => {
            collect_expr_call_callee_label_span(&signature.annotation, open_start, found);
        }
        aven_parser::Item::Expr(expr) => {
            collect_expr_call_callee_label_span(expr, open_start, found);
        }
    }
}

fn collect_expr_call_callee_label_span(
    expr: &aven_parser::Expr,
    open_start: usize,
    found: &mut Option<(Span, usize)>,
) {
    if let aven_parser::ExprKind::Call { callee, .. } = &expr.kind
        && callee.span.start <= open_start
        && callee.span.end <= open_start
        && open_start < expr.span.end
    {
        let expr_len = expr.span.len();
        if found.is_none_or(|(_, found_len)| expr_len < found_len) {
            *found = Some((Span::new(callee.span.start, open_start), expr_len));
        }
    }

    aven_parser::walk_expr_children(expr, &mut |child| {
        collect_expr_call_callee_label_span(child, open_start, found);
    });
}

fn active_parameter_for_call(
    tokens: &[&aven_parser::Token],
    open_index: usize,
    offset: usize,
) -> u32 {
    let mut depth = 0usize;
    let mut active_parameter = 0;

    for token in tokens.iter().skip(open_index + 1) {
        if token.span.start >= offset {
            break;
        }

        if opening_delimiter_kind(token).is_some() {
            depth += 1;
        } else if closing_delimiter_kind(token).is_some() {
            if depth == 0 {
                break;
            }
            depth -= 1;
        } else if matches!(&token.kind, aven_parser::TokenKind::Comma) && depth == 0 {
            active_parameter += 1;
        }
    }

    active_parameter
}

fn opening_delimiter_kind(token: &aven_parser::Token) -> Option<DelimiterKind> {
    match &token.kind {
        aven_parser::TokenKind::OpenParen => Some(DelimiterKind::Paren),
        aven_parser::TokenKind::OpenBracket => Some(DelimiterKind::Bracket),
        aven_parser::TokenKind::OpenBrace => Some(DelimiterKind::Brace),
        _ => None,
    }
}

fn closing_delimiter_kind(token: &aven_parser::Token) -> Option<DelimiterKind> {
    match &token.kind {
        aven_parser::TokenKind::CloseParen => Some(DelimiterKind::Paren),
        aven_parser::TokenKind::CloseBracket => Some(DelimiterKind::Bracket),
        aven_parser::TokenKind::CloseBrace => Some(DelimiterKind::Brace),
        _ => None,
    }
}

fn inlay_hints_in_range(document: &ParsedDocument, range: Range) -> Vec<InlayHint> {
    let mut hints = Vec::new();
    collect_inlay_hints_in_items(
        document,
        &document.parse_output().module.items,
        range,
        &mut hints,
    );
    hints
}

fn collect_inlay_hints_in_items(
    document: &ParsedDocument,
    items: &[aven_parser::Item],
    range: Range,
    hints: &mut Vec<InlayHint>,
) {
    for item in items {
        match item {
            aven_parser::Item::Binding(binding) => {
                push_inlay_hint_for_name_span(document, binding.name_span, range, hints);
                collect_inlay_hints_in_expr(document, &binding.value, range, hints);
            }
            aven_parser::Item::PatternBinding(binding) => {
                for site in aven_parser::pattern_bindings(&binding.pattern) {
                    push_inlay_hint_for_name_span(document, site.span, range, hints);
                }
                collect_inlay_hints_in_expr(document, &binding.value, range, hints);
            }
            aven_parser::Item::SpreadBinding(binding) => {
                collect_inlay_hints_in_expr(document, &binding.value, range, hints);
            }
            aven_parser::Item::Signature(_) => {}
            aven_parser::Item::Expr(expr) => {
                collect_inlay_hints_in_expr(document, expr, range, hints)
            }
        }
    }
}

fn collect_inlay_hints_in_expr(
    document: &ParsedDocument,
    expr: &aven_parser::Expr,
    range: Range,
    hints: &mut Vec<InlayHint>,
) {
    match &expr.kind {
        aven_parser::ExprKind::Lambda { params, body, .. } => {
            for param in params {
                push_inlay_hint_for_name_span(document, param.name_span, range, hints);
            }
            collect_inlay_hints_in_expr(document, body, range, hints);
        }
        aven_parser::ExprKind::Block(items) => {
            collect_inlay_hints_in_items(document, items, range, hints);
        }
        aven_parser::ExprKind::Match { subject, arms, .. } => {
            collect_inlay_hints_in_expr(document, subject, range, hints);
            for arm in arms {
                for binding in aven_parser::pattern_bindings(&arm.pattern) {
                    push_inlay_hint_for_name_span(document, binding.span, range, hints);
                }
                for guard in &arm.guards {
                    collect_inlay_hints_in_expr(document, guard, range, hints);
                }
                collect_inlay_hints_in_expr(document, &arm.body, range, hints);
            }
        }
        _ => aven_parser::walk_expr_children(expr, &mut |child| {
            collect_inlay_hints_in_expr(document, child, range, hints);
        }),
    }
}

fn push_inlay_hint_for_name_span(
    document: &ParsedDocument,
    name_span: Span,
    requested_range: Range,
    hints: &mut Vec<InlayHint>,
) {
    let name_range = span_to_range(document, name_span);
    if !range_contains_range(requested_range, name_range)
        || aven_parser::annotation_for_definition(&document.parse_output().module, name_span)
            .is_some()
    {
        return;
    }

    let Some(ty) = document.type_at(name_span) else {
        return;
    };

    hints.push(InlayHint {
        position: name_range.end,
        label: InlayHintLabel::String(format!(": {}", ty.render())),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: None,
        padding_left: Some(true),
        padding_right: None,
        data: None,
    });
}

fn range_contains_range(outer: Range, inner: Range) -> bool {
    inner.start >= outer.start && inner.end <= outer.end
}

// Hardcoded with reference to aven-check's private BUILTIN_TYPES/CHECKED_NAMED_TYPES
// rather than adding an LSP dependency on the checker just for completion.
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "Array",
    "Bool",
    "Data",
    "Float",
    "Int",
    "Json",
    "JsonError",
    "Map",
    "Null",
    "Result",
    "Set",
    "Text",
    "Toml",
    "TomlError",
    "U8",
    "Undefined",
    "Unit",
    "Yaml",
    "YamlError",
];

fn definition_location(
    document: &ParsedDocument,
    uri: Url,
    position: Position,
    module_graph: Option<&DocumentModuleGraph>,
) -> Option<Location> {
    if let Some(graph) = module_graph {
        if let Some(location) = import_specifier_definition_location(document, graph, position) {
            return Some(location);
        }

        if let Some(location) = import_pattern_definition_location(document, graph, position) {
            return Some(location);
        }

        if let Some(location) = import_member_definition_location(document, graph, position) {
            return Some(location);
        }

        if let Some(location) = spread_import_definition_location(document, graph, position) {
            return Some(location);
        }
    }

    let identifier = identifier_at_position(document, position)?;

    if let Some(span) = local_definition_span(document, &identifier) {
        return Some(Location::new(uri, span_to_range(document, span)));
    }

    let declaration = document
        .declarations()
        .iter()
        .find(|declaration| declaration.name == identifier.name)?;

    Some(Location::new(
        uri,
        span_to_range(document, declaration.name_span),
    ))
}

fn import_specifier_definition_location(
    document: &ParsedDocument,
    graph: &DocumentModuleGraph,
    position: Position,
) -> Option<Location> {
    let offset = position_to_offset(document, position)?;
    let import = entry_node(graph)?.imports.iter().find(|import| {
        !import.failed
            && offset >= import.specifier_span.start
            && offset <= import.specifier_span.end
    })?;
    let target_path = import.target_path.as_ref()?;

    if let Some(node) = graph.nodes.get(target_path)
        && let Some(specifier) = aven_compiler::library_specifier(target_path)
    {
        // Whole-module goto on a library lands at the top of its generated
        // interface document.
        return library_interface_location(node, &specifier, None);
    }

    location_for_path_span(graph, target_path, Span::point(0))
}

fn import_pattern_definition_location(
    document: &ParsedDocument,
    graph: &DocumentModuleGraph,
    position: Position,
) -> Option<Location> {
    let identifier = identifier_at_position(document, position)?;

    for item in &document.parse_output().module.items {
        let aven_parser::Item::PatternBinding(binding) = item else {
            continue;
        };
        let Some(specifier) = import_specifier_from_expr(&binding.value) else {
            continue;
        };
        let Some(target_path) = target_path_for_specifier(graph, &specifier) else {
            continue;
        };
        let Some(export_name) = export_name_for_pattern_identifier(&binding.pattern, &identifier)
        else {
            continue;
        };
        let Some(provenance) = graph
            .nodes
            .get(target_path)
            .and_then(|node| node.export_provenance.get(export_name))
        else {
            continue;
        };

        return location_for_provenance(graph, export_name, provenance);
    }

    None
}

fn import_member_definition_location(
    document: &ParsedDocument,
    graph: &DocumentModuleGraph,
    position: Position,
) -> Option<Location> {
    let access = field_access_identifier_at_position(document, position)?;
    let definition_span = definition_span_for_identifier(document, &access.receiver)?;
    let specifier = top_level_import_binding_specifier(document, definition_span)?;
    let target_path = target_path_for_specifier(graph, &specifier)?;
    let provenance = graph
        .nodes
        .get(target_path)?
        .export_provenance
        .get(&access.field.name)?;

    location_for_provenance(graph, &access.field.name, provenance)
}

fn spread_import_definition_location(
    document: &ParsedDocument,
    graph: &DocumentModuleGraph,
    position: Position,
) -> Option<Location> {
    let identifier = identifier_at_position(document, position)?;

    if definition_span_for_identifier(document, &identifier).is_some() {
        return None;
    }

    for item in &document.parse_output().module.items {
        let aven_parser::Item::SpreadBinding(binding) = item else {
            continue;
        };
        let Some(specifier) = import_specifier_from_expr(&binding.value) else {
            continue;
        };
        let Some(target_path) = target_path_for_specifier(graph, &specifier) else {
            continue;
        };
        let Some(provenance) = graph
            .nodes
            .get(target_path)
            .and_then(|node| node.export_provenance.get(&identifier.name))
        else {
            continue;
        };

        return location_for_provenance(graph, &identifier.name, provenance);
    }

    None
}

fn entry_node(graph: &DocumentModuleGraph) -> Option<&ModuleNodeCache> {
    graph.nodes.get(&graph.entry_path)
}

fn target_path_for_specifier<'a>(
    graph: &'a DocumentModuleGraph,
    specifier: &str,
) -> Option<&'a PathBuf> {
    entry_node(graph)?
        .imports
        .iter()
        .find(|import| !import.failed && import.specifier == specifier)?
        .target_path
        .as_ref()
}

fn location_for_provenance(
    graph: &DocumentModuleGraph,
    export_name: &str,
    provenance: &aven_compiler::ExportProvenance,
) -> Option<Location> {
    if let Some(node) = graph.nodes.get(&provenance.canonical_path)
        && let Some(specifier) = aven_compiler::library_specifier(&provenance.canonical_path)
    {
        // Library exports resolve to their signature line in the generated
        // interface document, not the (mostly punned) implementation source.
        return library_interface_location(node, &specifier, Some(export_name));
    }

    location_for_path_span(
        graph,
        &provenance.canonical_path,
        provenance.definition_span,
    )
}

/// Location inside a library node's generated interface document, materialized
/// under the interface cache dir: the named export's signature line, or the
/// top of the document when `export_name` is `None` or the interface can't
/// name it (e.g. a rename whose provenance chased to a differently-named
/// definition) — a top-of-document landing beats a dead goto.
fn library_interface_location(
    node: &ModuleNodeCache,
    specifier: &str,
    export_name: Option<&str>,
) -> Option<Location> {
    let interface = node.interface.as_ref()?;
    let path = materialize_interface(specifier, interface)?;
    let uri = Url::from_file_path(path).ok()?;
    let span = export_name
        .and_then(|name| interface.export_spans.get(name).copied())
        .unwrap_or(Span::point(0));
    let index = aven_core::LineIndex::new(&interface.text);
    let (start, end) = index.span_to_range(&interface.text, span);
    Some(Location::new(
        uri,
        Range::new(to_lsp_position(start), to_lsp_position(end)),
    ))
}

/// Root directory for materialized library interface documents. Scoped per
/// server process so concurrent servers (possibly of different versions) never
/// clobber each other's documents.
fn interface_cache_dir() -> PathBuf {
    std::env::temp_dir()
        .join("aven-lsp-interfaces")
        .join(std::process::id().to_string())
}

/// True when the editor hands us a document from the interface cache (any
/// server instance's): those are generated goto targets, not user code, so
/// they get no module-graph or semantic analysis and no diagnostics.
fn is_interface_cache_document(uri: &Url) -> bool {
    uri.to_file_path()
        .is_ok_and(|path| path.starts_with(std::env::temp_dir().join("aven-lsp-interfaces")))
}

/// Writes the interface document to its stable cache path (`std/time` becomes
/// `<cache dir>/std/time.av`), skipping the write when the on-disk content is
/// already current.
fn materialize_interface(
    specifier: &str,
    interface: &aven_compiler::ModuleInterface,
) -> Option<PathBuf> {
    let path = interface_cache_dir().join(format!("{specifier}.av"));
    if fs::read_to_string(&path).is_ok_and(|existing| existing == interface.text) {
        return Some(path);
    }
    fs::create_dir_all(path.parent()?).ok()?;
    fs::write(&path, &interface.text).ok()?;
    Some(path)
}

fn location_for_path_span(
    graph: &DocumentModuleGraph,
    path: &Path,
    span: Span,
) -> Option<Location> {
    let file = &graph.nodes.get(path)?.file;
    let uri = Url::from_file_path(path).ok()?;
    Some(Location::new(uri, source_file_span_to_range(file, span)))
}

fn source_file_span_to_range(file: &SourceFile, span: Span) -> Range {
    let (start, end) = file.line_index().span_to_range(file.source(), span);
    Range {
        start: to_lsp_position(start),
        end: to_lsp_position(end),
    }
}

fn top_level_import_binding_specifier(
    document: &ParsedDocument,
    definition_span: Span,
) -> Option<String> {
    document
        .parse_output()
        .module
        .items
        .iter()
        .find_map(|item| match item {
            aven_parser::Item::Binding(binding) if binding.name_span == definition_span => {
                import_specifier_from_expr(&binding.value)
            }
            _ => None,
        })
}

fn import_specifier_from_expr(expr: &aven_parser::Expr) -> Option<String> {
    if let aven_parser::ExprKind::Call { callee, args } = &expr.kind
        && matches!(&callee.kind, aven_parser::ExprKind::Name(name) if name == "import")
        && let Some(aven_parser::ExprKind::Literal(aven_parser::Literal::String(raw))) =
            args.first().map(|arg| &arg.kind)
    {
        return Some(aven_parser::decode_string_literal(raw));
    }

    None
}

fn export_name_for_pattern_identifier<'a>(
    pattern: &'a aven_parser::Expr,
    identifier: &IdentifierAtPosition,
) -> Option<&'a str> {
    let aven_parser::ExprKind::Record(entries) = &pattern.kind else {
        return None;
    };

    entries.iter().find_map(|entry| match entry {
        aven_parser::RecordEntry::Shorthand {
            name, name_span, ..
        } if *name_span == identifier.span => Some(name.as_str()),
        aven_parser::RecordEntry::Rename { from, to_span, .. } if *to_span == identifier.span => {
            Some(from.as_str())
        }
        _ => None,
    })
}

fn rename_workspace_edit(
    document: &ParsedDocument,
    uri: Url,
    position: Position,
    new_name: String,
) -> Option<WorkspaceEdit> {
    if !aven_parser::is_identifier(&new_name) {
        return None;
    }

    let identifier = identifier_at_position(document, position)?;

    if aven_parser::is_comptime_identifier_name(&new_name)
        != aven_parser::is_comptime_identifier_name(&identifier.name)
    {
        return None;
    }

    let spans = aven_parser::resolve_local_references(
        &document.parse_output().module,
        &document.parse_output().raw_tokens,
        &identifier.name,
        identifier.span,
    )?;

    let edits = spans
        .into_iter()
        .map(|span| TextEdit {
            range: span_to_range(document, span),
            new_text: new_name.clone(),
        })
        .collect();

    Some(WorkspaceEdit {
        changes: Some(HashMap::from([(uri, edits)])),
        document_changes: None,
        change_annotations: None,
    })
}

fn hover_at_position(document: &ParsedDocument, position: Position) -> Option<Hover> {
    let expression_hover = expression_hover_at_position(document, position);
    let identifier_hover = identifier_hover_at_position(document, position);

    match (expression_hover, identifier_hover) {
        (Some(expression), Some(identifier)) if expression.span.len() < identifier.span.len() => {
            Some(expression.hover)
        }
        (_, Some(identifier)) => Some(identifier.hover),
        (Some(expression), None) => Some(expression.hover),
        (None, None) => None,
    }
}

#[derive(Debug, Clone)]
struct HoverCandidate {
    span: Span,
    hover: Hover,
}

fn expression_hover_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<HoverCandidate> {
    let span = expr_span_at_position(document, position)?;
    let rendered = document.type_at(span)?.render();

    Some(HoverCandidate {
        span,
        hover: Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```aven\n{rendered}\n```"),
            }),
            range: Some(span_to_range(document, span)),
        },
    })
}

fn identifier_hover_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<HoverCandidate> {
    let identifier = identifier_at_position(document, position)?;

    if let Some(field_access) = field_access_identifier_at_position(document, position)
        && field_access.field.span == identifier.span
        && let Some(field_type) = field_type_for_access(document, &field_access)
    {
        return Some(HoverCandidate {
            span: identifier.span,
            hover: Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!(
                        "```aven\n{} : {}\n```",
                        field_access.label(),
                        field_type.render()
                    ),
                }),
                range: Some(span_to_range(document, identifier.span)),
            },
        });
    }

    let rendered = if let Some(definition) = definition_span_for_identifier(document, &identifier) {
        if let Some(annotation) =
            aven_parser::annotation_for_definition(&document.parse_output().module, definition)
        {
            let rendered = aven_parser::render_annotation(document.source(), annotation);
            (!rendered.is_empty()).then_some(rendered)
        } else {
            document
                .type_at(definition)
                .map(aven_compiler::Type::render)
        }
    } else {
        host_global_type(&identifier.name).map(|ty| ty.render())
    };

    let value = rendered
        .map(|rendered| format!("```aven\n{} : {}\n```", identifier.name, rendered))
        .or_else(|| comptime_hover_value(document, &identifier))?;

    Some(HoverCandidate {
        span: identifier.span,
        hover: Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(span_to_range(document, identifier.span)),
        },
    })
}

/// Hover descriptions for the builtin comptime type functions. Membership
/// comes from [`aven_compiler::COMPTIME_BUILTIN_FUNCTIONS`]; a test asserts
/// this table covers exactly that list so the two cannot drift apart.
const COMPTIME_BUILTIN_HOVERS: &[(&str, &str)] = &[
    (
        "keysOf",
        "The keys of a record type as a literal union — `keysOf(User)` is `\"email\" | \"name\"`.",
    ),
    (
        "tagsOf",
        "The tags of a variant type as a literal union — `tagsOf(Shape)` is `\"circle\" | \"square\"`.",
    ),
    (
        "typeOf",
        "The type of a value — `typeOf(user)` is the inferred type of `user`.",
    ),
    (
        "pick",
        "A record type with only the given keys — `pick(User, \"name\")` is `{ name: Text }`.",
    ),
    (
        "omit",
        "A record type without the given keys — `omit(User, \"email\")` is `{ name: Text }`.",
    ),
];

/// Hover fallbacks for comptime type machinery, which is name-bound but has no
/// inferred type: reified comptime type bindings (`Draft = partial(User)`),
/// user-defined comptime type functions (`partial`), and the builtin comptime
/// type functions themselves. Only consulted when the ordinary
/// annotation/inference branches produced nothing, so identifiers that already
/// hover keep their rendering.
fn comptime_hover_value(
    document: &ParsedDocument,
    identifier: &IdentifierAtPosition,
) -> Option<String> {
    if let Some(ty) = document.type_definition(&identifier.name) {
        // Deferred definitions (e.g. `Bad = partial(missing)`) get no hover;
        // `Bad = <deferred>` would only restate that resolution failed.
        return (!aven_compiler::type_contains_deferred(ty))
            .then(|| format!("```aven\n{} = {}\n```", identifier.name, ty.render()));
    }

    if comptime_type_function_binding(document, &identifier.name) {
        return Some(format!(
            "```aven\n{} : comptime type function\n```",
            identifier.name
        ));
    }

    let (_, description) = COMPTIME_BUILTIN_HOVERS
        .iter()
        .find(|(name, _)| *name == identifier.name)?;
    Some(format!(
        "```aven\n{} : comptime type function\n```\n{description}",
        identifier.name
    ))
}

/// Whether `name` is a top-level unannotated lambda whose body references one
/// of the builtin comptime type functions — the conservative signature of a
/// user-defined comptime type function (mapped-type template). A plain
/// deferred runtime lambda like `merge = (a, b) => { ..a, ..b }` does not
/// qualify.
fn comptime_type_function_binding(document: &ParsedDocument, name: &str) -> bool {
    document.parse_output().module.items.iter().any(|item| {
        let aven_parser::Item::Binding(binding) = item else {
            return false;
        };
        binding.name == name
            && binding.annotation.is_none()
            && aven_parser::lambda_parts(&binding.value).is_some_and(|(_, body)| {
                binding.name.chars().next().is_some_and(char::is_uppercase)
                    || references_comptime_builtin(body)
            })
    })
}

fn references_comptime_builtin(expr: &aven_parser::Expr) -> bool {
    if let aven_parser::ExprKind::Name(name) | aven_parser::ExprKind::ComptimeName(name) =
        &expr.kind
        && aven_compiler::COMPTIME_BUILTIN_FUNCTIONS.contains(&name.as_str())
    {
        return true;
    }

    let mut found = false;
    aven_parser::walk_expr_children(expr, &mut |child| {
        found = found || references_comptime_builtin(child);
    });
    found
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentifierAtPosition {
    name: String,
    span: Span,
}

fn definition_span_for_identifier(
    document: &ParsedDocument,
    identifier: &IdentifierAtPosition,
) -> Option<Span> {
    local_definition_span(document, identifier).or_else(|| {
        document
            .declarations()
            .iter()
            .find(|declaration| declaration.name == identifier.name)
            .map(|declaration| declaration.name_span)
    })
}

fn local_definition_span(
    document: &ParsedDocument,
    identifier: &IdentifierAtPosition,
) -> Option<Span> {
    aven_parser::resolve_local_definition(
        &document.parse_output().module,
        &identifier.name,
        identifier.span,
    )
}

fn identifier_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<IdentifierAtPosition> {
    let offset = position_to_offset(document, position)?;

    document.parse_output().raw_tokens.iter().find_map(|token| {
        if offset < token.span.start || offset >= token.span.end {
            return None;
        }

        identifier_from_token(token)
    })
}

fn expr_span_at_position(document: &ParsedDocument, position: Position) -> Option<Span> {
    let offset = position_to_offset(document, position)?;
    let target = Span::point(offset);
    let mut found = None;

    for item in &document.parse_output().module.items {
        collect_item_expr_span_at(item, target, &mut found);
    }

    found.or_else(|| token_span_at_offset(document, offset))
}

fn collect_item_expr_span_at(item: &aven_parser::Item, target: Span, found: &mut Option<Span>) {
    match item {
        aven_parser::Item::Binding(binding) => {
            if let Some(annotation) = &binding.annotation {
                collect_expr_span_at(annotation, target, found);
            }
            collect_expr_span_at(&binding.value, target, found);
        }
        aven_parser::Item::PatternBinding(binding) => {
            collect_expr_span_at(&binding.pattern, target, found);
            collect_expr_span_at(&binding.value, target, found);
        }
        aven_parser::Item::SpreadBinding(binding) => {
            collect_expr_span_at(&binding.value, target, found);
        }
        aven_parser::Item::Signature(signature) => {
            collect_expr_span_at(&signature.annotation, target, found);
        }
        aven_parser::Item::Expr(expr) => collect_expr_span_at(expr, target, found),
    }
}

fn collect_expr_span_at(expr: &aven_parser::Expr, target: Span, found: &mut Option<Span>) {
    if expr.span.is_empty() || !expr.span.contains(target) {
        return;
    }

    if found.is_none_or(|span| expr.span.len() < span.len()) {
        *found = Some(expr.span);
    }

    aven_parser::walk_expr_children(expr, &mut |child| {
        collect_expr_span_at(child, target, found);
    });
}

fn token_span_at_offset(document: &ParsedDocument, offset: usize) -> Option<Span> {
    document
        .parse_output()
        .raw_tokens
        .iter()
        .find(|token| offset >= token.span.start && offset < token.span.end)
        .map(|token| token.span)
}

fn significant_tokens(document: &ParsedDocument) -> Vec<&aven_parser::Token> {
    document
        .parse_output()
        .raw_tokens
        .iter()
        .filter(|token| !is_trivia_token(token))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldAccessAtPosition {
    operator_span: Span,
    null_safe: bool,
    receiver: Option<IdentifierAtPosition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldAccessIdentifiers {
    receiver: IdentifierAtPosition,
    field: IdentifierAtPosition,
}

impl FieldAccessIdentifiers {
    fn label(&self) -> String {
        format!("{}.{}", self.receiver.name, self.field.name)
    }
}

fn field_access_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<FieldAccessAtPosition> {
    let offset = position_to_offset(document, position)?;
    let significant_tokens = significant_tokens(document);

    for (index, token) in significant_tokens.iter().enumerate() {
        if is_field_access_operator(token) && token.span.end == offset {
            return Some(field_access_at_operator(&significant_tokens, index));
        }

        if identifier_from_token(token).is_some()
            && offset >= token.span.start
            && offset <= token.span.end
        {
            let dot_index = index.checked_sub(1)?;
            if is_field_access_operator(significant_tokens[dot_index]) {
                return Some(field_access_at_operator(&significant_tokens, dot_index));
            }
        }
    }

    None
}

fn field_access_identifier_at_position(
    document: &ParsedDocument,
    position: Position,
) -> Option<FieldAccessIdentifiers> {
    let offset = position_to_offset(document, position)?;
    let significant_tokens = significant_tokens(document);

    for (index, token) in significant_tokens.iter().enumerate() {
        if offset < token.span.start || offset >= token.span.end {
            continue;
        }

        let field = identifier_from_token(token)?;
        let operator_index = index.checked_sub(1)?;
        if !is_field_access_operator(significant_tokens[operator_index]) {
            return None;
        }

        let receiver = receiver_name_before_field_operator(&significant_tokens, operator_index)?;
        return Some(FieldAccessIdentifiers { receiver, field });
    }

    None
}

fn field_access_at_operator(
    tokens: &[&aven_parser::Token],
    operator_index: usize,
) -> FieldAccessAtPosition {
    FieldAccessAtPosition {
        operator_span: tokens[operator_index].span,
        null_safe: is_null_safe_field_access_operator(tokens[operator_index]),
        receiver: receiver_name_before_field_operator(tokens, operator_index),
    }
}

fn receiver_name_before_field_operator(
    tokens: &[&aven_parser::Token],
    operator_index: usize,
) -> Option<IdentifierAtPosition> {
    let receiver_index = operator_index.checked_sub(1)?;
    identifier_from_token(tokens[receiver_index])
}

fn field_type_for_access(
    document: &ParsedDocument,
    access: &FieldAccessIdentifiers,
) -> Option<aven_compiler::Type> {
    if let Some(ty) = document.type_at(access.field.span)
        && matches!(ty, aven_compiler::Type::Function { .. })
    {
        return Some(ty.clone());
    }

    let fields = definition_span_for_identifier(document, &access.receiver)
        .and_then(|span| document.type_at(span).cloned())
        .or_else(|| host_global_type(&access.receiver.name))
        .as_ref()
        .and_then(aven_compiler::record_fields)
        .or_else(|| type_statics_fields(&access.receiver.name))?;

    fields
        .into_iter()
        .find_map(|field| (field.name == access.field.name).then_some(field.ty))
}

fn identifier_from_token(token: &aven_parser::Token) -> Option<IdentifierAtPosition> {
    match &token.kind {
        aven_parser::TokenKind::Identifier(name)
        | aven_parser::TokenKind::ComptimeIdentifier(name) => Some(IdentifierAtPosition {
            name: name.clone(),
            span: token.span,
        }),
        _ => None,
    }
}

fn is_field_access_operator(token: &aven_parser::Token) -> bool {
    matches!(&token.kind, aven_parser::TokenKind::Operator(operator) if operator == "." || operator == "?.")
}

fn is_null_safe_field_access_operator(token: &aven_parser::Token) -> bool {
    matches!(&token.kind, aven_parser::TokenKind::Operator(operator) if operator == "?.")
}

fn is_trivia_token(token: &aven_parser::Token) -> bool {
    matches!(
        &token.kind,
        aven_parser::TokenKind::RawNewline
            | aven_parser::TokenKind::RawIndent { .. }
            | aven_parser::TokenKind::Newline
            | aven_parser::TokenKind::Indent
            | aven_parser::TokenKind::Dedent
            | aven_parser::TokenKind::Comment(_)
            | aven_parser::TokenKind::DocComment(_)
    )
}

fn span_to_range(document: &ParsedDocument, span: Span) -> Range {
    let (start, end) = document
        .file()
        .line_index()
        .span_to_range(document.source(), span);

    Range {
        start: to_lsp_position(start),
        end: to_lsp_position(end),
    }
}

/// Convert a byte span to an LSP range using exact offsets. Unlike
/// [`span_to_range`], it preserves a zero-width span (an insertion point) rather
/// than flooring the width at one character.
fn exact_offset_range(document: &ParsedDocument, span: Span) -> Range {
    let line_index = document.file().line_index();
    let source = document.source();
    Range {
        start: to_lsp_position(line_index.offset_to_position(source, span.start)),
        end: to_lsp_position(line_index.offset_to_position(source, span.end)),
    }
}

fn position_to_offset(document: &ParsedDocument, target: Position) -> Option<usize> {
    document.file().line_index().position_to_offset(
        document.source(),
        SourcePosition::new(target.line, target.character),
    )
}

fn full_document_range(document: &ParsedDocument) -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: to_lsp_position(
            document
                .file()
                .line_index()
                .offset_to_position(document.source(), document.source().len()),
        ),
    }
}

fn to_lsp_position(position: SourcePosition) -> Position {
    Position {
        line: position.line,
        character: position.character,
    }
}

#[cfg(test)]
mod tests;
