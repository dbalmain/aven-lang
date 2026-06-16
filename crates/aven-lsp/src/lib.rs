use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aven_core::{Diagnostic as AvenDiagnostic, FileId, Severity, SourceFile, SourcePosition, Span};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams, Location,
    MarkupContent, MarkupKind, MessageType, OneOf, Position, Range, RenameParams,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SymbolKind, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

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

    fn set_semantic_diagnostics(
        &mut self,
        uri: &Url,
        version: i32,
        diagnostics: Vec<AvenDiagnostic>,
    ) -> bool {
        self.database
            .set_semantic_diagnostics(uri, aven_compiler::Revision::from(version), diagnostics)
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
                hover_provider: Some(HoverProviderCapability::Simple(true)),
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
        let Some(document) = self.document(&uri) else {
            return Ok(None);
        };

        Ok(hover_at_position(&document, position))
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

    fn schedule_semantic_diagnostics(&self, uri: Url, version: i32) {
        self.cancel_pending_semantic_diagnostics(&uri);

        let Some(document) = self.document(&uri) else {
            return;
        };

        if document
            .parse_diagnostics()
            .iter()
            .any(AvenDiagnostic::is_error)
        {
            return;
        }

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

    let diagnostics = aven_compiler::analyze_semantics(document.parse_output()).diagnostics;
    let Some(document) = store.lock().ok().and_then(|mut store| {
        if !store.set_semantic_diagnostics(&uri, version, diagnostics) {
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

fn definition_location(
    document: &ParsedDocument,
    uri: Url,
    position: Position,
) -> Option<Location> {
    let identifier = identifier_at_position(document, position)?;

    if let Some(span) = aven_parser::resolve_local_definition(
        &document.parse_output().module,
        &identifier.name,
        identifier.span,
    ) {
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
    let identifier = identifier_at_position(document, position)?;
    let definition = aven_parser::resolve_local_definition(
        &document.parse_output().module,
        &identifier.name,
        identifier.span,
    )
    .or_else(|| {
        document
            .declarations()
            .iter()
            .find(|declaration| declaration.name == identifier.name)
            .map(|declaration| declaration.name_span)
    })?;

    let annotation =
        aven_parser::annotation_for_definition(&document.parse_output().module, definition)?;
    let rendered = aven_parser::render_annotation(document.source(), annotation);

    if rendered.is_empty() {
        return None;
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```aven\n{} : {}\n```", identifier.name, rendered),
        }),
        range: Some(span_to_range(document, identifier.span)),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentifierAtPosition {
    name: String,
    span: Span,
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

        match &token.kind {
            aven_parser::TokenKind::Identifier(name)
            | aven_parser::TokenKind::ComptimeIdentifier(name) => Some(IdentifierAtPosition {
                name: name.clone(),
                span: token.span,
            }),
            _ => None,
        }
    })
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
        let diagnostics = aven_compiler::analyze_semantics(document.parse_output()).diagnostics;
        document.with_semantic_diagnostics(diagnostics)
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

        let document_symbol = Request::build("textDocument/documentSymbol")
            .params(json!({
                "textDocument": {
                    "uri": uri_text
                }
            }))
            .id(2)
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
            .id(3)
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
        let document = document.with_semantic_diagnostics(
            aven_compiler::analyze_semantics(document.parse_output()).diagnostics,
        );
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

        assert!(store.set_semantic_diagnostics(
            &uri,
            1,
            vec![AvenDiagnostic::error("semantic diagnostic")]
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

        assert!(!store.set_semantic_diagnostics(
            &uri,
            1,
            vec![AvenDiagnostic::error("stale diagnostic")]
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
    fn hover_at_position_shows_lambda_parameter_annotation() {
        let document = parsed_document("id = (value : Text) => value\n".to_owned());
        let Some(hover) = hover_at_position(&document, position(0, 24)) else {
            panic!("expected hover");
        };

        assert_hover_value(hover, "```aven\nvalue : Text\n```");
    }

    #[test]
    fn hover_at_position_returns_none_for_unannotated_bindings() {
        let document = parsed_document("value = 1\nother = value\n".to_owned());
        let hover = hover_at_position(&document, position(1, 9));

        assert!(hover.is_none());
    }

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn assert_hover_value(hover: Hover, expected: &str) {
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover");
        };

        assert_eq!(markup.value, expected);
    }

    fn test_uri() -> Url {
        match Url::parse("file:///test.av") {
            Ok(uri) => uri,
            Err(error) => panic!("failed to parse test URI: {error}"),
        }
    }
}
