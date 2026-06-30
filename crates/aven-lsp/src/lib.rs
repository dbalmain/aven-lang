use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CompletionItem, CompletionItemKind,
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, InlayHint, InlayHintKind,
    InlayHintLabel, InlayHintParams, Location, MarkupContent, MarkupKind, MessageType,
    NumberOrString, OneOf, ParameterInformation, ParameterLabel, Position, Range, RenameParams,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelp, SignatureHelpOptions,
    SignatureHelpParams, SignatureInformation, SymbolKind, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
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
}

impl DocumentStore {
    fn set_document(&mut self, uri: Url, version: i32, text: String) -> FileId {
        let file_id = self.file_id_for(&uri);
        let revision = aven_compiler::Revision::from(version);

        if self.database.needs_update(&uri, revision, &text) {
            let file = SourceFile::new(file_id, source_name(&uri), uri.to_file_path().ok(), text);
            self.database.set_document(uri, revision, file);
        }

        file_id
    }

    fn document(&self, uri: &Url) -> Option<Arc<ParsedDocument>> {
        self.database.document(uri)
    }

    fn set_semantic(
        &mut self,
        uri: &Url,
        version: i32,
        diagnostics: Vec<AvenDiagnostic>,
        inferred_types: Vec<aven_compiler::InferredType>,
    ) -> bool {
        self.database
            .set_semantic(
                uri,
                aven_compiler::Revision::from(version),
                diagnostics,
                inferred_types,
            )
            .is_some()
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
                    trigger_characters: Some(vec![".".to_owned(), "@".to_owned()]),
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
        let Some(document) = self.document(&uri) else {
            return Ok(None);
        };

        Ok(definition_location(&document, uri, position).map(GotoDefinitionResponse::Scalar))
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

        Ok(Some(CompletionResponse::Array(completion_at_position(
            &document, position,
        ))))
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

    /// Fetch the document with semantics for its *current* revision, computing
    /// them synchronously. Semantic analysis is otherwise produced by a
    /// debounced background task (`SEMANTIC_DEBOUNCE`), so a freshly edited
    /// document has no inferred types until that fires. Type-directed features
    /// (completion, hover, signature help, inlay hints) must not depend on that
    /// timing — completing right after typing `.` would otherwise see no type
    /// and fall back to the bare identifier list. At embedded-script sizes a
    /// re-check per request is cheap.
    fn document_with_semantics(&self, uri: &Url) -> Option<Arc<ParsedDocument>> {
        let document = self.document(uri)?;
        let semantic = analyze_document_semantics(&document);
        Some(Arc::new(document.with_semantic(
            semantic.diagnostics,
            semantic.inferred_types,
        )))
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
    let Some(document) = store.lock().ok().and_then(|store| store.document(&uri)) else {
        return;
    };

    if document.revision() != aven_compiler::Revision::from(version) {
        return;
    }

    let semantic = analyze_document_semantics(&document);
    let Some(document) = store.lock().ok().and_then(|mut store| {
        if !store.set_semantic(&uri, version, semantic.diagnostics, semantic.inferred_types) {
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

fn analyze_document_semantics(document: &ParsedDocument) -> aven_compiler::SemanticOutput {
    let globals = aven_host::standard_check_host_globals();
    aven_compiler::analyze_semantics_with_host_globals(document.parse_output(), &globals)
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

fn completion_at_position(document: &ParsedDocument, position: Position) -> Vec<CompletionItem> {
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
    // a user binding of the same name shadows the global.
    for (name, ty) in aven_host::standard_check_host_globals().types {
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
        .and_then(|offset| document.type_at(Span::point(offset)));
    let fields = if let Some(receiver_type) = receiver_type {
        aven_compiler::record_fields(receiver_type)?
    } else {
        let receiver = access.receiver.as_ref()?;
        let receiver_span = definition_span_for_identifier(document, receiver)?;
        aven_compiler::record_fields(document.type_at(receiver_span)?)?
    };
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for field in fields {
        push_completion_item(
            &mut items,
            &mut seen,
            completion_item_for_record_field(field),
        );
    }

    Some(items)
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

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for member in members {
        push_completion_item(
            &mut items,
            &mut seen,
            CompletionItem {
                label: member.clone(),
                kind: Some(CompletionItemKind::VALUE),
                insert_text: Some(member),
                ..CompletionItem::default()
            },
        );
    }

    Some(items)
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
    aven_host::standard_check_host_globals()
        .types
        .into_iter()
        .find_map(|(global, ty)| (global == name).then_some(ty))
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

    let callee = call.fallback_callee.as_ref()?;
    if let Some(callee_span) = definition_span_for_identifier(document, callee) {
        return document.type_at(callee_span).cloned();
    }

    host_global_type(&callee.name)
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

fn token_can_end_callee_expression(token: &aven_parser::Token) -> bool {
    matches!(
        &token.kind,
        aven_parser::TokenKind::Identifier(_)
            | aven_parser::TokenKind::ComptimeIdentifier(_)
            | aven_parser::TokenKind::Number(_)
            | aven_parser::TokenKind::StringLiteral(_)
            | aven_parser::TokenKind::InterpolationEnd(_)
            | aven_parser::TokenKind::RegexLiteral(_)
            | aven_parser::TokenKind::PathLiteral(_)
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
    "Float",
    "Int",
    "Json",
    "Null",
    "Result",
    "Set",
    "Text",
    "U8",
    "Undefined",
    "Unit",
    "Yaml",
];

fn definition_location(
    document: &ParsedDocument,
    uri: Url,
    position: Position,
) -> Option<Location> {
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

    let rendered = if let Some(definition) = definition_span_for_identifier(document, &identifier) {
        if let Some(annotation) =
            aven_parser::annotation_for_definition(&document.parse_output().module, definition)
        {
            let rendered = aven_parser::render_annotation(document.source(), annotation);
            if rendered.is_empty() {
                return None;
            }
            rendered
        } else {
            document.type_at(definition)?.render()
        }
    } else {
        host_global_type(&identifier.name)?.render()
    };

    Some(HoverCandidate {
        span: identifier.span,
        hover: Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```aven\n{} : {}\n```", identifier.name, rendered),
            }),
            range: Some(span_to_range(document, identifier.span)),
        },
    })
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
    receiver: Option<IdentifierAtPosition>,
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

fn field_access_at_operator(
    tokens: &[&aven_parser::Token],
    operator_index: usize,
) -> FieldAccessAtPosition {
    FieldAccessAtPosition {
        operator_span: tokens[operator_index].span,
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
mod tests {
    use std::future;

    use futures_util::StreamExt;
    use serde_json::json;
    use tokio::time::advance;
    use tower::Service;
    use tower_lsp::jsonrpc::{Request, Response};
    use tower_lsp::lsp_types::{NumberOrString, PublishDiagnosticsParams};

    use super::*;

    async fn call_service(service: &mut LspService<Backend>, request: Request) -> Option<Response> {
        let ready = future::poll_fn(|cx| service.poll_ready(cx)).await;
        let Ok(()) = ready else {
            panic!("expected LSP service to be ready");
        };
        let response = service.call(request).await;
        let Ok(response) = response else {
            panic!("expected LSP service call to succeed");
        };
        response
    }

    async fn next_publish_diagnostics(
        socket: &mut tower_lsp::ClientSocket,
    ) -> PublishDiagnosticsParams {
        let Some(request) = socket.next().await else {
            panic!("expected publishDiagnostics notification");
        };
        assert_eq!(request.method(), "textDocument/publishDiagnostics");

        let Some(params) = request.params().cloned() else {
            panic!("expected publishDiagnostics params");
        };

        serde_json::from_value(params).expect("expected valid publishDiagnostics params")
    }

    fn test_backend(client: Client) -> Backend {
        Backend {
            client,
            store: Arc::default(),
            pending_semantic: Arc::default(),
        }
    }

    fn parsed_document(source: impl Into<String>) -> ParsedDocument {
        parsed_document_with_file_id(FileId(0), source)
    }

    fn parsed_document_with_file_id(file_id: FileId, source: impl Into<String>) -> ParsedDocument {
        let file = SourceFile::new(file_id, format!("lsp:{}", file_id.0), None, source);
        aven_compiler::DocumentSnapshot::parse(aven_compiler::Revision::default(), file)
    }

    fn parsed_document_with_semantics(source: impl Into<String>) -> ParsedDocument {
        let document = parsed_document(source);
        let semantic = analyze_document_semantics(&document);
        document.with_semantic(semantic.diagnostics, semantic.inferred_types)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn protocol_smoke_opens_document_and_returns_symbols() {
        let (mut service, _) = LspService::new(test_backend);
        let uri = test_uri();
        let uri_text = uri.to_string();

        let initialize = Request::build("initialize")
            .params(json!({"capabilities": {}}))
            .id(1)
            .finish();
        let Some(response) = call_service(&mut service, initialize).await else {
            panic!("expected initialize response");
        };
        let (_id, body) = response.into_parts();
        let Ok(value) = body else {
            panic!("expected successful initialize response");
        };
        let initialize_result = match serde_json::from_value::<InitializeResult>(value) {
            Ok(result) => result,
            Err(error) => panic!("expected initialize result: {error}"),
        };
        assert!(matches!(
            initialize_result
                .capabilities
                .semantic_tokens_provider
                .as_ref(),
            Some(SemanticTokensServerCapabilities::SemanticTokensOptions(options))
                if matches!(options.full, Some(SemanticTokensFullOptions::Bool(true)))
        ));
        assert!(matches!(
            initialize_result.capabilities.code_action_provider.as_ref(),
            Some(CodeActionProviderCapability::Simple(true))
        ));
        let completion_options = initialize_result
            .capabilities
            .completion_provider
            .as_ref()
            .expect("expected completion provider");
        assert!(
            completion_options
                .trigger_characters
                .as_ref()
                .is_some_and(|triggers| triggers.iter().any(|trigger| trigger == "."))
        );
        assert!(
            completion_options
                .trigger_characters
                .as_ref()
                .is_some_and(|triggers| triggers.iter().any(|trigger| trigger == "@"))
        );
        let signature_help_options = initialize_result
            .capabilities
            .signature_help_provider
            .as_ref()
            .expect("expected signature help provider");
        assert!(
            signature_help_options
                .trigger_characters
                .as_ref()
                .is_some_and(|triggers| triggers == &["(".to_owned(), ",".to_owned()])
        );
        assert!(matches!(
            initialize_result.capabilities.inlay_hint_provider,
            Some(OneOf::Left(true))
        ));

        let did_open = Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": uri_text.clone(),
                    "languageId": "aven",
                    "version": 1,
                    "text": "User = { name: Text }\nvalue = 1\n"
                }
            }))
            .finish();
        assert!(call_service(&mut service, did_open).await.is_none());

        let completion = Request::build("textDocument/completion")
            .params(json!({
                "textDocument": {
                    "uri": uri_text.clone()
                },
                "position": {
                    "line": 1,
                    "character": 3
                }
            }))
            .id(2)
            .finish();
        let Some(response) = call_service(&mut service, completion).await else {
            panic!("expected completion response");
        };
        let (_id, body) = response.into_parts();
        let Ok(value) = body else {
            panic!("expected successful completion response");
        };
        let completions = match serde_json::from_value::<Vec<CompletionItem>>(value) {
            Ok(completions) => completions,
            Err(error) => panic!("expected completion response: {error}"),
        };
        assert!(completion_item(&completions, "value").is_some());
        assert!(completion_item(&completions, "Text").is_some());

        let document_symbol = Request::build("textDocument/documentSymbol")
            .params(json!({
                "textDocument": {
                    "uri": uri_text
                }
            }))
            .id(3)
            .finish();
        let Some(response) = call_service(&mut service, document_symbol).await else {
            panic!("expected documentSymbol response");
        };
        let (_id, body) = response.into_parts();
        let Ok(value) = body else {
            panic!("expected successful documentSymbol response");
        };
        let symbols = match serde_json::from_value::<Vec<DocumentSymbol>>(value) {
            Ok(symbols) => symbols,
            Err(error) => panic!("expected document symbols response: {error}"),
        };

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "User");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);
        assert_eq!(symbols[1].name, "value");
        assert_eq!(symbols[1].kind, SymbolKind::VARIABLE);

        let semantic_tokens = Request::build("textDocument/semanticTokens/full")
            .params(json!({
                "textDocument": {
                    "uri": uri_text
                }
            }))
            .id(4)
            .finish();
        let Some(response) = call_service(&mut service, semantic_tokens).await else {
            panic!("expected semanticTokens response");
        };
        let (_id, body) = response.into_parts();
        let Ok(value) = body else {
            panic!("expected successful semanticTokens response");
        };
        let semantic_tokens = match serde_json::from_value::<SemanticTokensResult>(value) {
            Ok(SemanticTokensResult::Tokens(tokens)) => tokens,
            Ok(SemanticTokensResult::Partial(_)) => {
                panic!("expected full semantic tokens response")
            }
            Err(error) => panic!("expected semantic tokens result: {error}"),
        };
        assert!(!semantic_tokens.data.is_empty());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn protocol_inlay_hint_returns_inferred_binding_type() {
        let (mut service, mut socket) = LspService::new(test_backend);
        let uri = test_uri();
        let uri_text = uri.to_string();

        let initialize = Request::build("initialize")
            .params(json!({"capabilities": {}}))
            .id(1)
            .finish();
        let Some(response) = call_service(&mut service, initialize).await else {
            panic!("expected initialize response");
        };
        assert!(response.is_ok());

        let did_open = Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": uri_text.clone(),
                    "languageId": "aven",
                    "version": 1,
                    "text": "n = 1\n"
                }
            }))
            .finish();
        assert!(call_service(&mut service, did_open).await.is_none());

        let parse = next_publish_diagnostics(&mut socket).await;
        assert_eq!(parse.version, Some(1));
        assert!(parse.diagnostics.is_empty());

        advance(SEMANTIC_DEBOUNCE + Duration::from_millis(1)).await;

        let semantic = next_publish_diagnostics(&mut socket).await;
        assert_eq!(semantic.version, Some(1));
        assert!(semantic.diagnostics.is_empty());

        let inlay_hint = Request::build("textDocument/inlayHint")
            .params(json!({
                "textDocument": {
                    "uri": uri_text
                },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 1, "character": 0 }
                }
            }))
            .id(2)
            .finish();
        let Some(response) = call_service(&mut service, inlay_hint).await else {
            panic!("expected inlayHint response");
        };
        let (_id, body) = response.into_parts();
        let Ok(value) = body else {
            panic!("expected successful inlayHint response");
        };
        let hints = match serde_json::from_value::<Vec<InlayHint>>(value) {
            Ok(hints) => hints,
            Err(error) => panic!("expected inlay hint response: {error}"),
        };

        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].position, position(0, 1));
        assert!(matches!(
            &hints[0].label,
            InlayHintLabel::String(label) if label == ": 1"
        ));
        assert_eq!(hints[0].kind, Some(InlayHintKind::TYPE));
        assert_eq!(hints[0].padding_left, Some(true));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn completion_returns_fields_before_debounced_semantics() {
        // Time is paused, so the debounced semantic pass never fires. Completion
        // requested right after didOpen must still return type-directed fields,
        // proving the handler computes semantics on demand rather than waiting
        // for SEMANTIC_DEBOUNCE.
        let (mut service, _socket) = LspService::new(test_backend);
        let uri = test_uri();
        let uri_text = uri.to_string();

        let initialize = Request::build("initialize")
            .params(json!({"capabilities": {}}))
            .id(1)
            .finish();
        assert!(call_service(&mut service, initialize).await.is_some());

        let did_open = Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": uri_text.clone(),
                    "languageId": "aven",
                    "version": 1,
                    "text": "r = { a: 1, b: 2 }\nx = r.\n"
                }
            }))
            .finish();
        assert!(call_service(&mut service, did_open).await.is_none());

        let completion = Request::build("textDocument/completion")
            .params(json!({
                "textDocument": { "uri": uri_text },
                "position": { "line": 1, "character": 6 }
            }))
            .id(2)
            .finish();
        let Some(response) = call_service(&mut service, completion).await else {
            panic!("expected completion response");
        };
        let (_id, body) = response.into_parts();
        let value = body.expect("successful completion response");
        let labels = match serde_json::from_value::<CompletionResponse>(value)
            .expect("completion response")
        {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        }
        .into_iter()
        .map(|item| item.label)
        .collect::<Vec<_>>();

        assert!(
            labels.contains(&"a".to_owned()) && labels.contains(&"b".to_owned()),
            "expected record fields a/b before debounce, got {labels:?}"
        );
        assert!(
            !labels.contains(&"Int".to_owned()),
            "expected field completion, not the identifier fallback, got {labels:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn semantic_diagnostics_are_debounced_and_stale_results_are_cancelled() {
        let (mut service, mut socket) = LspService::new(test_backend);
        let uri = test_uri();
        let uri_text = uri.to_string();

        let initialize = Request::build("initialize")
            .params(json!({"capabilities": {}}))
            .id(1)
            .finish();
        let Some(response) = call_service(&mut service, initialize).await else {
            panic!("expected initialize response");
        };
        assert!(response.is_ok());

        let did_open = Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": uri_text.clone(),
                    "languageId": "aven",
                    "version": 1,
                    "text": "value : Missing = value\n"
                }
            }))
            .finish();
        assert!(call_service(&mut service, did_open).await.is_none());

        let first = next_publish_diagnostics(&mut socket).await;
        assert_eq!(first.version, Some(1));
        assert!(first.diagnostics.is_empty());

        advance(SEMANTIC_DEBOUNCE + Duration::from_millis(1)).await;

        let semantic = next_publish_diagnostics(&mut socket).await;
        assert_eq!(semantic.version, Some(1));
        assert_eq!(semantic.diagnostics.len(), 1);
        assert!(matches!(
            semantic.diagnostics[0].code.as_ref(),
            Some(NumberOrString::String(code)) if code == "type.unknown-name"
        ));

        let did_change_error = Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": {
                    "uri": uri_text.clone(),
                    "version": 2
                },
                "contentChanges": [
                    { "text": "value : Missing = value\n" }
                ]
            }))
            .finish();
        assert!(call_service(&mut service, did_change_error).await.is_none());

        let parse_only = next_publish_diagnostics(&mut socket).await;
        assert_eq!(parse_only.version, Some(2));
        assert!(parse_only.diagnostics.is_empty());

        let did_change_clean = Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": {
                    "uri": uri_text,
                    "version": 3
                },
                "contentChanges": [
                    { "text": "value = 1\n" }
                ]
            }))
            .finish();
        assert!(call_service(&mut service, did_change_clean).await.is_none());

        let clean_parse = next_publish_diagnostics(&mut socket).await;
        assert_eq!(clean_parse.version, Some(3));
        assert!(clean_parse.diagnostics.is_empty());

        advance(SEMANTIC_DEBOUNCE + Duration::from_millis(1)).await;

        let clean_semantic = next_publish_diagnostics(&mut socket).await;
        assert_eq!(clean_semantic.version, Some(3));
        assert!(clean_semantic.diagnostics.is_empty());
    }

    #[test]
    fn document_symbols_include_top_level_bindings() {
        let document =
            parsed_document("User = { name: Text }\ndouble = (x) => x\nvalue = 1\n".to_owned());
        let symbols = document_symbols(&document);

        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].name, "User");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);
        assert_eq!(symbols[1].name, "double");
        assert_eq!(symbols[1].kind, SymbolKind::FUNCTION);
        assert_eq!(symbols[2].name, "value");
        assert_eq!(symbols[2].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn document_symbols_merge_adjacent_signature_and_binding() {
        let document = parsed_document("double : (Int) -> Int\ndouble = (x) => x\n".to_owned());
        let symbols = document_symbols(&document);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "double");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(symbols[0].detail.as_deref(), Some("binding with signature"));
        assert_eq!(symbols[0].range.start.line, 0);
        assert_eq!(symbols[0].range.end.line, 1);
        assert_eq!(symbols[0].selection_range.start.line, 1);
        assert_eq!(symbols[0].selection_range.start.character, 0);
        assert_eq!(symbols[0].selection_range.end.character, 6);
    }

    #[test]
    fn document_symbols_keep_unmatched_signatures() {
        let document = parsed_document("value : Int\nother = 1\n".to_owned());
        let symbols = document_symbols(&document);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "value");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(symbols[0].detail.as_deref(), Some("signature"));
        assert_eq!(symbols[1].name, "other");
        assert_eq!(symbols[1].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn completion_at_position_includes_top_level_bindings_with_types() {
        let document = parsed_document_with_semantics(
            "format : (Int) -> Text = (value) => \"ok\"\ncount = 1\n",
        );
        let completions = completion_at_position(&document, position(1, 8));
        let Some(format) = completion_item(&completions, "format") else {
            panic!("expected format completion");
        };
        let Some(count) = completion_item(&completions, "count") else {
            panic!("expected count completion");
        };

        assert_eq!(format.detail.as_deref(), Some("Int -> Text"));
        assert_eq!(format.kind, Some(CompletionItemKind::FUNCTION));
        assert_eq!(count.detail.as_deref(), Some("1"));
        assert_eq!(count.kind, Some(CompletionItemKind::VARIABLE));
    }

    #[test]
    fn completion_at_position_includes_builtin_type_names() {
        let document = parsed_document_with_semantics("value = 1\n");
        let completions = completion_at_position(&document, position(0, 8));

        for name in [
            "Bool",
            "Float",
            "Int",
            "Null",
            "Text",
            "U8",
            "Undefined",
            "Unit",
        ] {
            let Some(item) = completion_item(&completions, name) else {
                panic!("expected builtin completion for {name}");
            };

            assert_eq!(item.kind, Some(CompletionItemKind::CLASS));
        }
    }

    #[test]
    fn seeded_host_globals_reach_semantic_diagnostics() {
        let document = parsed_document_with_semantics("logger.info(42)\n");

        assert!(document.parse_diagnostics().is_empty());
        assert_eq!(document.semantic_diagnostics().len(), 1);
        assert_eq!(
            document.semantic_diagnostics()[0].code.as_deref(),
            Some("type.mismatch")
        );
    }

    #[test]
    fn completion_at_seeded_logger_field_access_returns_logger_methods() {
        let document = parsed_document_with_semantics("logger.info\n");
        let completions = completion_at_position(&document, position(0, 7));
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        let Some(info) = completion_item(&completions, "info") else {
            panic!("expected logger.info completion");
        };

        assert_eq!(
            labels,
            vec!["trace", "debug", "info", "warn", "error", "fatal", "child"]
        );
        assert_eq!(info.kind, Some(CompletionItemKind::FIELD));
        assert_eq!(info.detail.as_deref(), Some("(Text, { .. } = _) -> Unit"));
    }

    #[test]
    fn completion_at_incomplete_seeded_logger_field_access_returns_logger_methods() {
        let document = parsed_document_with_semantics("logger.\n");
        let completions = completion_at_position(&document, position(0, 7));
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert!(!document.parse_diagnostics().is_empty());
        assert!(document.semantic_diagnostics().is_empty());
        assert_eq!(
            labels,
            vec!["trace", "debug", "info", "warn", "error", "fatal", "child"]
        );
        assert!(completion_item(&completions, "Text").is_none());
        assert!(completion_item(&completions, "logger").is_none());
    }

    #[test]
    fn completion_at_seeded_logger_field_access_with_parse_error_returns_logger_methods() {
        let document = parsed_document_with_semantics("logger.info\nbroken = \n");
        let completions = completion_at_position(&document, position(0, 11));
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert!(!document.parse_diagnostics().is_empty());
        assert!(document.semantic_diagnostics().is_empty());
        assert_eq!(
            labels,
            vec!["trace", "debug", "info", "warn", "error", "fatal", "child"]
        );
        assert!(completion_item(&completions, "Text").is_none());
        assert!(completion_item(&completions, "logger").is_none());
    }

    #[test]
    fn completion_at_position_includes_visible_local_bindings() {
        let document =
            parsed_document_with_semantics("value =\n  local = \"hi\"\n  local\nother = 1\n");
        let completions = completion_at_position(&document, position(2, 3));
        let Some(local) = completion_item(&completions, "local") else {
            panic!("expected local completion");
        };

        assert_eq!(local.detail.as_deref(), Some("\"hi\""));
        assert_eq!(local.kind, Some(CompletionItemKind::VARIABLE));
    }

    #[test]
    fn completion_at_position_excludes_out_of_scope_local_bindings() {
        let document =
            parsed_document_with_semantics("value =\n  local = \"hi\"\n  local\nother = 1\n");
        let completions = completion_at_position(&document, position(3, 8));

        assert!(completion_item(&completions, "local").is_none());
    }

    #[test]
    fn completion_at_position_prefers_local_shadowing_top_level() {
        let document =
            parsed_document_with_semantics("value = 1\nresult =\n  value = \"hi\"\n  value\n");
        let completions = completion_at_position(&document, position(3, 3));
        let value_items = completions
            .iter()
            .filter(|item| item.label == "value")
            .collect::<Vec<_>>();

        assert_eq!(value_items.len(), 1);
        assert_eq!(value_items[0].detail.as_deref(), Some("\"hi\""));
    }

    #[test]
    fn completion_at_field_access_returns_record_fields() {
        let document = parsed_document_with_semantics(
            "user : { name: Text, email: Text } = current\nresult = user.name\n",
        );
        let completions = completion_at_position(&document, position(1, 14));
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        let Some(name) = completion_item(&completions, "name") else {
            panic!("expected name field completion");
        };
        let Some(email) = completion_item(&completions, "email") else {
            panic!("expected email field completion");
        };

        assert_eq!(labels, vec!["name", "email"]);
        assert_eq!(name.kind, Some(CompletionItemKind::FIELD));
        assert_eq!(name.detail.as_deref(), Some("Text"));
        assert_eq!(email.kind, Some(CompletionItemKind::FIELD));
        assert_eq!(email.detail.as_deref(), Some("Text"));
        assert!(completion_item(&completions, "user").is_none());
        assert!(completion_item(&completions, "Text").is_none());
    }

    #[test]
    fn completion_at_local_record_field_access_with_parse_error_returns_record_fields() {
        let document = parsed_document_with_semantics(
            "user = { name: \"Ada\", email: \"ada@example.com\" }\nresult = user.name\nbroken = \n",
        );
        let completions = completion_at_position(&document, position(1, 18));
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert!(!document.parse_diagnostics().is_empty());
        assert!(document.semantic_diagnostics().is_empty());
        assert_eq!(labels, vec!["name", "email"]);
        assert!(completion_item(&completions, "user").is_none());
        assert!(completion_item(&completions, "Text").is_none());
    }

    #[test]
    fn completion_at_partial_field_access_returns_record_fields() {
        let document = parsed_document_with_semantics(
            "user : { name: Text, email: Text } = current\nresult = user.em\n",
        );
        let completions = completion_at_position(&document, position(1, 16));

        assert!(completion_item(&completions, "name").is_some());
        assert!(completion_item(&completions, "email").is_some());
        assert!(completion_item(&completions, "user").is_none());
    }

    #[test]
    fn completion_at_call_result_field_access_returns_record_fields() {
        let document = parsed_document_with_semantics(
            "getUser : () -> { name: Text, email: Text }\n\
             getUser = () => { name: \"Ada\", email: \"ada@example.com\" }\n\
             result = getUser().name\n",
        );
        let completions = completion_at_position(&document, position(2, 19));

        let Some(name) = completion_item(&completions, "name") else {
            panic!("expected name field completion");
        };
        let Some(email) = completion_item(&completions, "email") else {
            panic!("expected email field completion");
        };

        assert_eq!(name.kind, Some(CompletionItemKind::FIELD));
        assert_eq!(name.detail.as_deref(), Some("Text"));
        assert_eq!(email.kind, Some(CompletionItemKind::FIELD));
        assert_eq!(email.detail.as_deref(), Some("Text"));
        assert!(completion_item(&completions, "getUser").is_none());
    }

    #[test]
    fn completion_at_index_result_field_access_returns_record_fields() {
        let document = parsed_document_with_semantics(
            "usersById : { ada: { name: Text, email: Text } } = current\n\
             result = usersById[\"ada\"].name\n",
        );
        let completions = completion_at_position(&document, position(1, 26));

        assert!(completion_item(&completions, "name").is_some());
        assert!(completion_item(&completions, "email").is_some());
        assert!(completion_item(&completions, "usersById").is_none());
    }

    #[test]
    fn completion_at_record_construction_returns_missing_declared_fields() {
        let completions =
            completions_at_marker("user : { name: Text, email: Text } = { name: \"Ada\", | }\n");
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        let Some(email) = completion_item(&completions, "email") else {
            panic!("expected email field completion");
        };

        assert_eq!(labels, vec!["email"]);
        assert_eq!(email.kind, Some(CompletionItemKind::FIELD));
        assert_eq!(email.detail.as_deref(), Some("Text"));
        assert!(completion_item(&completions, "name").is_none());
    }

    #[test]
    fn completion_at_record_construction_returns_no_present_fields() {
        let completions = completions_at_marker(
            "user : { name: Text, email: Text } = { name: \"Ada\", email: \"ada@example.com\", | }\n",
        );

        assert!(completions.is_empty());
    }

    #[test]
    fn completion_at_variant_construction_returns_declared_tags() {
        let completions = completions_at_marker("color : @{ @Red, @Green } = @{ | }\n");
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        let Some(red) = completion_item(&completions, "@Red") else {
            panic!("expected @Red completion");
        };
        let Some(green) = completion_item(&completions, "@Green") else {
            panic!("expected @Green completion");
        };

        assert_eq!(labels, vec!["@Red", "@Green"]);
        assert_eq!(red.kind, Some(CompletionItemKind::ENUM_MEMBER));
        assert_eq!(green.kind, Some(CompletionItemKind::ENUM_MEMBER));
    }

    #[test]
    fn completion_at_variant_construction_after_at_omits_present_tags() {
        let completions = completions_at_marker("color : @{ @Red, @Green } = @{ @Red, @| }\n");
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(labels, vec!["@Green"]);
    }

    #[test]
    fn completion_inside_record_field_value_falls_back_to_identifiers() {
        let completions =
            completions_at_marker("user : { name: Text, email: Text } = { name: \"A|\" }\n");

        assert!(completion_item(&completions, "email").is_none());
        assert!(completion_item(&completions, "user").is_some());
        assert!(completion_item(&completions, "Text").is_some());
    }

    #[test]
    fn completion_in_record_without_expected_type_falls_back_to_identifiers() {
        let completions =
            completions_at_marker("name = \"Ada\"\nemail = \"ada@example.com\"\nuser = { | }\n");

        assert!(completion_item(&completions, "email").is_some());
        assert!(completion_item(&completions, "Text").is_some());
    }

    #[test]
    fn completion_at_non_field_position_still_returns_identifier_list() {
        let document = parsed_document_with_semantics(
            "user : { name: Text, email: Text } = current\nresult = use\n",
        );
        let completions = completion_at_position(&document, position(1, 12));
        let Some(user) = completion_item(&completions, "user") else {
            panic!("expected user identifier completion");
        };

        assert_eq!(user.detail.as_deref(), Some("{ name: Text, email: Text }"));
        assert!(completion_item(&completions, "Text").is_some());
        assert!(completion_item(&completions, "name").is_none());
    }

    #[test]
    fn completion_includes_host_globals() {
        let document = parsed_document_with_semantics("main =\n  x = \n");
        let completions = completion_at_position(&document, position(1, 6));

        let Some(logger) = completion_item(&completions, "logger") else {
            panic!("expected logger host global in completion");
        };
        assert_eq!(logger.kind, Some(CompletionItemKind::VARIABLE));
        assert!(
            logger
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("info:"))
        );
        assert!(completion_item(&completions, "dbg").is_some());
        assert!(completion_item(&completions, "write").is_some());
        assert!(completion_item(&completions, "writeLine").is_some());
        assert!(completion_item(&completions, "readLine").is_some());
        assert!(completion_item(&completions, "readAll").is_some());
        assert!(completion_item(&completions, "Platform").is_none());
    }

    #[test]
    fn completion_at_open_mode_argument_returns_mode_literals() {
        let completions = completions_at_marker("open(\"x\", |)\n");
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(labels, vec!["\"r\"", "\"w\"", "\"a\"", "\"rw\""]);
        for label in labels {
            let Some(item) = completion_item(&completions, label) else {
                panic!("expected {label} completion");
            };

            assert_eq!(item.kind, Some(CompletionItemKind::VALUE));
            assert_eq!(item.insert_text.as_deref(), Some(label));
        }
    }

    #[test]
    fn signature_help_at_name_call_returns_signature_and_active_parameter() {
        let document = parsed_document_with_semantics(
            "add : (Int, Int) -> Int\nadd = (a, b) => a + b\ntotal = add(1, 2)\n",
        );

        let Some(help) = signature_help_at_position(&document, position(2, 15)) else {
            panic!("expected signature help");
        };

        assert_eq!(help.active_signature, Some(0));
        assert_eq!(help.active_parameter, Some(1));
        assert_eq!(help.signatures.len(), 1);
        assert_eq!(help.signatures[0].label, "add(Int, Int) -> Int");
        assert_eq!(help.signatures[0].active_parameter, Some(1));

        let parameters = help.signatures[0]
            .parameters
            .as_ref()
            .expect("expected parameter labels");
        assert_eq!(parameters.len(), 2);
        assert_eq!(parameters[0].label, ParameterLabel::LabelOffsets([4, 7]));
        assert_eq!(parameters[1].label, ParameterLabel::LabelOffsets([9, 12]));
    }

    #[test]
    fn signature_help_at_call_start_uses_first_parameter() {
        let document = parsed_document_with_semantics(
            "add : (Int, Int) -> Int\nadd = (a, b) => a + b\ntotal = add(1, 2)\n",
        );

        let Some(help) = signature_help_at_position(&document, position(2, 12)) else {
            panic!("expected signature help");
        };

        assert_eq!(help.signatures[0].label, "add(Int, Int) -> Int");
        assert_eq!(help.active_parameter, Some(0));
        assert_eq!(help.signatures[0].active_parameter, Some(0));
    }

    #[test]
    fn signature_help_at_call_result_callee_returns_signature_and_active_parameter() {
        let document = parsed_document_with_semantics(
            "add : (Int, Int) -> Int\n\
             add = (a, b) => a + b\n\
             wrap : ((Int, Int) -> Int) -> (Int, Int) -> Int\n\
             wrap = (f) => f\n\
             total = wrap(add)(1, 2)\n",
        );

        let Some(help) = signature_help_at_position(&document, position(4, 21)) else {
            panic!("expected signature help");
        };

        assert_eq!(help.active_parameter, Some(1));
        assert_eq!(help.signatures[0].label, "wrap(add)(Int, Int) -> Int");
        assert_eq!(help.signatures[0].active_parameter, Some(1));

        let parameters = help.signatures[0]
            .parameters
            .as_ref()
            .expect("expected parameter labels");
        assert_eq!(parameters[0].label, ParameterLabel::LabelOffsets([10, 13]));
        assert_eq!(parameters[1].label, ParameterLabel::LabelOffsets([15, 18]));
    }

    #[test]
    fn signature_help_at_open_call_uses_host_global_signature() {
        let document = parsed_document_with_semantics("open(\"x\", )\n");

        let Some(help) = signature_help_at_position(&document, position(0, 9)) else {
            panic!("expected signature help");
        };

        assert_eq!(help.active_parameter, Some(1));
        assert_eq!(
            help.signatures[0].label,
            "open(Text, \"r\" | \"w\" | \"a\" | \"rw\") -> ?"
        );

        let parameters = help.signatures[0]
            .parameters
            .as_ref()
            .expect("expected parameter labels");
        assert_eq!(parameters.len(), 2);
        assert_eq!(parameters[0].label, ParameterLabel::LabelOffsets([5, 9]));
        assert_eq!(parameters[1].label, ParameterLabel::LabelOffsets([11, 33]));
    }

    #[test]
    fn signature_help_outside_call_returns_none() {
        let document = parsed_document_with_semantics(
            "add : (Int, Int) -> Int\nadd = (a, b) => a + b\ntotal = add(1, 2)\n",
        );

        assert!(signature_help_at_position(&document, position(0, 0)).is_none());
    }

    #[test]
    fn inlay_hints_in_range_skip_annotated_bindings() {
        let document = parsed_document_with_semantics("x : Text = \"a\"\n");
        let hints = inlay_hints_in_range(&document, full_document_range(&document));

        assert!(hints.is_empty());
    }

    #[test]
    fn parsed_documents_include_name_diagnostics() {
        let document = parsed_document_with_semantics("value = 1\nvalue = 2\n".to_owned());

        assert!(document.parse_diagnostics().is_empty());
        assert_eq!(document.semantic_diagnostics().len(), 1);
        assert_eq!(
            document.semantic_diagnostics()[0].code.as_deref(),
            Some("name.duplicate-declaration")
        );
    }

    #[test]
    fn parsed_documents_include_check_diagnostics() {
        let document = parsed_document_with_semantics("value : Missing = value\n".to_owned());

        assert!(document.parse_diagnostics().is_empty());
        assert_eq!(document.semantic_diagnostics().len(), 1);
        assert_eq!(
            document.semantic_diagnostics()[0].code.as_deref(),
            Some("type.unknown-name")
        );
    }

    #[test]
    fn spread_overwrite_code_action_inserts_colon_for_type_spread_collision() {
        let uri = test_uri();
        let document =
            parsed_document_with_semantics("Base = { x: Int }\ndup : { x: Int, ..Base } = value\n");
        let diagnostics = document_diagnostics(&document);
        let diagnostic = duplicate_spread_label_diagnostic(&diagnostics);
        let context = CodeActionContext {
            diagnostics: diagnostics.clone(),
            ..CodeActionContext::default()
        };
        let actions = spread_overwrite_code_actions(&document, &uri, &context);

        let action = single_code_action(&actions);
        assert_eq!(action.title, "Overwrite-merge spread with `:..`");
        assert_eq!(action.kind.as_ref(), Some(&CodeActionKind::QUICKFIX));
        assert_eq!(action.is_preferred, Some(true));
        assert_action_carries_diagnostic(action, diagnostic);

        let edit = single_action_text_edit(action, &uri);
        assert_eq!(edit.new_text, ":");
        assert_eq!(edit.range.start, diagnostic.range.start);
        assert_eq!(edit.range.end, diagnostic.range.start);
        assert_edit_inserts_text(&document, edit, ":..Base");
    }

    #[test]
    fn spread_overwrite_code_action_inserts_colon_for_value_spread_collision() {
        let uri = test_uri();
        let document =
            parsed_document_with_semantics("y = { x: 1 }\nz = { x: 2 }\na = { ..y, ..z }\n");
        let diagnostics = document_diagnostics(&document);
        let diagnostic = duplicate_spread_label_diagnostic(&diagnostics);
        let context = CodeActionContext {
            diagnostics: diagnostics.clone(),
            ..CodeActionContext::default()
        };
        let actions = spread_overwrite_code_actions(&document, &uri, &context);

        let action = single_code_action(&actions);
        assert_eq!(action.kind.as_ref(), Some(&CodeActionKind::QUICKFIX));
        assert_action_carries_diagnostic(action, diagnostic);

        let edit = single_action_text_edit(action, &uri);
        assert_eq!(edit.new_text, ":");
        assert_eq!(edit.range.start, diagnostic.range.start);
        assert_eq!(edit.range.end, diagnostic.range.start);
        assert_edit_inserts_text(&document, edit, "{ ..y, :..z }");
    }

    #[test]
    fn spread_overwrite_code_action_skips_duplicate_add_with_shared_code() {
        let document = parsed_document_with_semantics(
            "Base = { x: Int }\ndup : { ..Base, x: Text } = value\n",
        );
        let diagnostics = document_diagnostics(&document);
        let diagnostic = duplicate_spread_label_diagnostic(&diagnostics);
        let offset = position_to_offset(&document, diagnostic.range.start)
            .expect("expected diagnostic range start to convert to source offset");
        let source_at_range = document
            .source()
            .get(offset..)
            .expect("expected valid diagnostic source offset");
        assert!(!source_at_range.starts_with(".."));

        let context = CodeActionContext {
            diagnostics,
            ..CodeActionContext::default()
        };
        let actions = spread_overwrite_code_actions(&document, &test_uri(), &context);

        assert!(actions.is_empty());
    }

    #[test]
    fn unused_result_code_action_inserts_panic_unwrap_at_expression_end() {
        let uri = test_uri();
        let document = parsed_document_with_semantics("stdout.write(\"x\")\n1\n");
        let diagnostics = document_diagnostics(&document);
        let diagnostic = unused_result_diagnostic(&diagnostics);
        let context = CodeActionContext {
            diagnostics: diagnostics.clone(),
            ..CodeActionContext::default()
        };
        let actions = unused_result_code_actions(&uri, &context);

        let action = single_code_action(&actions);
        assert_eq!(action.title, "Unwrap with `?!`");
        assert_eq!(action.kind.as_ref(), Some(&CodeActionKind::QUICKFIX));
        assert_eq!(action.is_preferred, Some(true));
        assert_action_carries_diagnostic(action, diagnostic);

        let edit = single_action_text_edit(action, &uri);
        assert_eq!(edit.new_text, "?!");
        assert_eq!(edit.range.start, diagnostic.range.end);
        assert_eq!(edit.range.end, diagnostic.range.end);
        assert_edit_inserts_text(&document, edit, "stdout.write(\"x\")?!\n1\n");
    }

    #[test]
    fn parsed_documents_keep_parse_diagnostics_separate() {
        let document = parsed_document("value = )\n".to_owned());

        assert_eq!(document.parse_diagnostics().len(), 1);
        assert!(document.semantic_diagnostics().is_empty());
        assert_eq!(
            document.parse_diagnostics()[0].code.as_deref(),
            Some("parse.unexpected-delimiter")
        );
    }

    #[test]
    fn parsed_documents_start_without_semantic_diagnostics() {
        let document = parsed_document("value : Missing = value\n".to_owned());

        assert!(document.parse_diagnostics().is_empty());
        assert!(document.semantic_diagnostics().is_empty());
        assert_eq!(document.declarations().len(), 1);
    }

    #[test]
    fn parsed_documents_thread_file_ids_into_parse_output() {
        let document = parsed_document_with_file_id(FileId(7), "value = 1\n".to_owned());

        assert_eq!(document.file().id, FileId(7));
        assert_eq!(document.parse_output().file_id, FileId(7));
    }

    #[test]
    fn parsed_document_diagnostic_report_uses_file_id() {
        let document =
            parsed_document_with_file_id(FileId(7), "value : Missing = value\n".to_owned());
        let semantic = analyze_document_semantics(&document);
        let document = document.with_semantic(semantic.diagnostics, semantic.inferred_types);
        let report = document.diagnostic_report();

        assert_eq!(report.file_id, FileId(7));
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].code.as_deref(),
            Some("type.unknown-name")
        );
    }

    #[test]
    fn document_store_reuses_ids_for_the_same_uri() {
        let mut store = DocumentStore::default();
        let uri = test_uri();

        assert_eq!(
            store.set_document(uri.clone(), 1, "value = 1\n".to_owned()),
            FileId(0)
        );
        assert_eq!(
            store.set_document(uri.clone(), 2, "value = 2\n".to_owned()),
            FileId(0)
        );

        let Some(document) = store.document(&uri) else {
            panic!("expected stored document");
        };
        assert_eq!(document.file().id, FileId(0));
        assert_eq!(document.source(), "value = 2\n");
    }

    #[test]
    fn document_store_reuses_cached_documents_for_the_same_version() {
        let mut store = DocumentStore::default();
        let uri = test_uri();

        store.set_document(uri.clone(), 1, "value = 1\n".to_owned());
        let Some(first) = store.document(&uri) else {
            panic!("expected first stored document");
        };

        store.set_document(uri.clone(), 1, "value = 1\n".to_owned());
        let Some(second) = store.document(&uri) else {
            panic!("expected second stored document");
        };

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn document_store_replaces_documents_for_new_versions() {
        let mut store = DocumentStore::default();
        let uri = test_uri();

        store.set_document(uri.clone(), 1, "value = 1\n".to_owned());
        let Some(first) = store.document(&uri) else {
            panic!("expected first stored document");
        };

        store.set_document(uri.clone(), 2, "value = 2\n".to_owned());
        let Some(second) = store.document(&uri) else {
            panic!("expected second stored document");
        };

        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.revision().as_i32(), 2);
        assert_eq!(second.source(), "value = 2\n");
    }

    #[test]
    fn document_store_allocates_distinct_ids_for_distinct_uris() {
        let mut store = DocumentStore::default();
        let first = test_uri();
        let second = Url::parse("file:///second.av").expect("valid test URI");

        assert_eq!(
            store.set_document(first.clone(), 1, "one = 1\n".to_owned()),
            FileId(0)
        );
        assert_eq!(
            store.set_document(second, 1, "two = 2\n".to_owned()),
            FileId(1)
        );
        assert_eq!(
            store.set_document(first, 2, "one = 3\n".to_owned()),
            FileId(0)
        );
    }

    #[test]
    fn document_store_accepts_current_semantic_diagnostics() {
        let mut store = DocumentStore::default();
        let uri = test_uri();
        store.set_document(uri.clone(), 1, "value = 1\n".to_owned());

        assert!(store.set_semantic(
            &uri,
            1,
            vec![AvenDiagnostic::error("semantic diagnostic")],
            Vec::new(),
        ));

        let Some(document) = store.document(&uri) else {
            panic!("expected stored document");
        };
        assert_eq!(document.semantic_diagnostics().len(), 1);
        assert_eq!(
            document.semantic_diagnostics()[0].message,
            "semantic diagnostic"
        );
    }

    #[test]
    fn document_store_rejects_stale_semantic_diagnostics() {
        let mut store = DocumentStore::default();
        let uri = test_uri();
        store.set_document(uri.clone(), 1, "value = 1\n".to_owned());
        store.set_document(uri.clone(), 2, "value = 2\n".to_owned());

        assert!(!store.set_semantic(
            &uri,
            1,
            vec![AvenDiagnostic::error("stale diagnostic")],
            Vec::new(),
        ));

        let Some(document) = store.document(&uri) else {
            panic!("expected stored document");
        };
        assert_eq!(document.revision().as_i32(), 2);
        assert!(document.semantic_diagnostics().is_empty());
    }

    #[test]
    fn definition_location_finds_top_level_runtime_bindings() {
        let document = parsed_document("value = 1\nother = value\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 9)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(0, 0));
        assert_eq!(location.range.end, position(0, 5));
    }

    #[test]
    fn definition_location_finds_top_level_comptime_bindings() {
        let document = parsed_document("User = { name: Text }\nvalue = User\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 9)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(0, 0));
        assert_eq!(location.range.end, position(0, 4));
    }

    #[test]
    fn definition_location_prefers_lambda_parameters_over_top_level_bindings() {
        let document = parsed_document("x = 1\nf = (x) => x\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 11)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(1, 5));
        assert_eq!(location.range.end, position(1, 6));
    }

    #[test]
    fn definition_location_uses_nearest_lambda_parameter() {
        let document = parsed_document("x = 1\nf = (x) => (x) => x\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 18)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(1, 12));
        assert_eq!(location.range.end, position(1, 13));
    }

    #[test]
    fn definition_location_finds_block_bindings() {
        let document = parsed_document("f = () =>\n  x = 1\n  y = x\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(2, 6)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(1, 2));
        assert_eq!(location.range.end, position(1, 3));
    }

    #[test]
    fn definition_location_finds_match_pattern_binders() {
        let document =
            parsed_document("f = (result) =>\n  result ?>\n    @Ok(value) => value\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(2, 18)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(2, 8));
        assert_eq!(location.range.end, position(2, 13));
    }

    #[test]
    fn rename_workspace_edit_renames_nearest_local_binding() {
        let document = parsed_document("x = 1\nf = (x) => (x) => x\n".to_owned());
        let Some(edit) =
            rename_workspace_edit(&document, test_uri(), position(1, 18), "item".to_owned())
        else {
            panic!("expected rename edit");
        };

        let edits = edit
            .changes
            .as_ref()
            .and_then(|changes| changes.get(&test_uri()))
            .expect("expected edits for test URI");

        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].range.start, position(1, 12));
        assert_eq!(edits[0].range.end, position(1, 13));
        assert_eq!(edits[0].new_text, "item");
        assert_eq!(edits[1].range.start, position(1, 18));
        assert_eq!(edits[1].range.end, position(1, 19));
        assert_eq!(edits[1].new_text, "item");
    }

    #[test]
    fn rename_workspace_edit_skips_top_level_bindings() {
        let document = parsed_document("x = 1\nvalue = x\n".to_owned());
        let edit = rename_workspace_edit(&document, test_uri(), position(1, 8), "item".to_owned());

        assert!(edit.is_none());
    }

    #[test]
    fn rename_workspace_edit_rejects_invalid_identifiers() {
        let document = parsed_document("f = (x) => x\n".to_owned());
        let edit = rename_workspace_edit(&document, test_uri(), position(0, 10), "1x".to_owned());

        assert!(edit.is_none());
    }

    #[test]
    fn rename_workspace_edit_rejects_phase_class_changes() {
        let document = parsed_document("f = (x) => x\n".to_owned());
        let edit = rename_workspace_edit(&document, test_uri(), position(0, 10), "Name".to_owned());

        assert!(edit.is_none());
    }

    #[test]
    fn hover_at_position_shows_top_level_signature() {
        let document = parsed_document("double : (Int) -> Int\ndouble = (x) => x\n".to_owned());
        let Some(hover) = hover_at_position(&document, position(1, 1)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\ndouble : (Int) -> Int\n```");
    }

    #[test]
    fn hover_at_position_shows_open_host_global_signature() {
        let document = parsed_document_with_semantics("open(\"x\", \"r\")\n");
        let Some(hover) = hover_at_position(&document, position(0, 1)) else {
            panic!("expected hover");
        };

        assert_hover_value(
            hover,
            "```aven\nopen : (Text, \"r\" | \"w\" | \"a\" | \"rw\") -> ?\n```",
        );
    }

    #[test]
    fn hover_at_position_shows_lambda_parameter_annotation() {
        let document = parsed_document("id = (value : Text) => value\n".to_owned());
        let Some(hover) = hover_at_position(&document, position(0, 24)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\nvalue : Text\n```");
    }

    #[test]
    fn hover_at_position_shows_inferred_top_level_type() {
        let document = parsed_document_with_semantics("value = \"hi\"\n".to_owned());
        let Some(hover) = hover_at_position(&document, position(0, 1)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\nvalue : \"hi\"\n```");
    }

    #[test]
    fn hover_at_position_shows_inferred_local_type() {
        let document =
            parsed_document_with_semantics("value =\n  local = \"hi\"\n  local\n".to_owned());
        let Some(hover) = hover_at_position(&document, position(2, 3)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\nlocal : \"hi\"\n```");
    }

    #[test]
    fn hover_at_position_shows_call_result_expression_type() {
        let document = parsed_document_with_semantics(
            "add : (Int, Int) -> Int\nadd = (a, b) => a + b\ntotal = add(1, 2)\n",
        );
        let Some(hover) = hover_at_position(&document, position(2, 11)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\nInt\n```");
    }

    #[test]
    fn hover_at_position_shows_literal_expression_type() {
        let document = parsed_document_with_semantics("value = \"hi\"\n".to_owned());
        let Some(hover) = hover_at_position(&document, position(0, 8)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\n\"hi\"\n```");
    }

    #[test]
    fn hover_at_position_prefers_written_annotation() {
        let document =
            parsed_document_with_semantics("Alias = Text\nvalue : Alias = \"hi\"\nuse = value\n");
        let Some(hover) = hover_at_position(&document, position(2, 7)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\nvalue : Alias\n```");
    }

    #[test]
    fn hover_at_position_returns_none_when_inference_defers() {
        let document = parsed_document_with_semantics("value = missing\n".to_owned());
        let hover = hover_at_position(&document, position(0, 1));

        assert!(hover.is_none());
    }

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn completions_at_marker(source: &str) -> Vec<CompletionItem> {
        let marker = source
            .find('|')
            .unwrap_or_else(|| panic!("expected cursor marker in {source:?}"));
        let mut source = source.to_owned();
        source.remove(marker);
        let document = parsed_document_with_semantics(source);
        let position = to_lsp_position(
            document
                .file()
                .line_index()
                .offset_to_position(document.source(), marker),
        );

        completion_at_position(&document, position)
    }

    fn assert_hover_value(hover: Hover, expected: &str) {
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover");
        };

        assert_eq!(markup.value, expected);
    }

    fn duplicate_spread_label_diagnostic(diagnostics: &[Diagnostic]) -> &Diagnostic {
        diagnostics
            .iter()
            .find(|diagnostic| is_duplicate_spread_label_diagnostic(diagnostic))
            .expect("expected duplicate spread label diagnostic")
    }

    fn unused_result_diagnostic(diagnostics: &[Diagnostic]) -> &Diagnostic {
        diagnostics
            .iter()
            .find(|diagnostic| is_unused_result_diagnostic(diagnostic))
            .expect("expected unused Result diagnostic")
    }

    fn single_code_action(actions: &[CodeActionOrCommand]) -> &CodeAction {
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected code action");
        };

        action
    }

    fn single_action_text_edit<'a>(action: &'a CodeAction, uri: &Url) -> &'a TextEdit {
        let edits = action
            .edit
            .as_ref()
            .and_then(|edit| edit.changes.as_ref())
            .and_then(|changes| changes.get(uri))
            .expect("expected text edits for URI");

        assert_eq!(edits.len(), 1);
        &edits[0]
    }

    fn assert_action_carries_diagnostic(action: &CodeAction, diagnostic: &Diagnostic) {
        let action_diagnostics = action
            .diagnostics
            .as_ref()
            .expect("expected action diagnostics");

        assert_eq!(action_diagnostics.len(), 1);
        assert_eq!(&action_diagnostics[0], diagnostic);
    }

    fn assert_edit_inserts_text(document: &ParsedDocument, edit: &TextEdit, expected: &str) {
        assert_eq!(edit.range.start, edit.range.end);
        let offset = position_to_offset(document, edit.range.start)
            .expect("expected edit position to convert to source offset");
        let mut edited = document.source().to_owned();
        edited.insert_str(offset, &edit.new_text);

        assert!(
            edited.contains(expected),
            "expected edited source to contain {expected:?}, got {edited:?}"
        );
    }

    fn completion_item<'a>(items: &'a [CompletionItem], label: &str) -> Option<&'a CompletionItem> {
        items.iter().find(|item| item.label == label)
    }

    fn test_uri() -> Url {
        match Url::parse("file:///test.av") {
            Ok(uri) => uri,
            Err(error) => panic!("failed to parse test URI: {error}"),
        }
    }
}
