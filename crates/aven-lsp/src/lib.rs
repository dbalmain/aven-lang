use std::collections::HashMap;
use std::sync::Arc;

use aven_core::{Diagnostic as AvenDiagnostic, Severity, Span};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, InitializeParams, InitializeResult,
    InitializedParams, Location, MessageType, OneOf, Position, Range, ServerCapabilities,
    SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::default(),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}

#[derive(Debug)]
struct Backend {
    client: Client,
    documents: Arc<std::sync::Mutex<HashMap<Url, Arc<ParsedDocument>>>>,
}

#[derive(Debug)]
struct ParsedDocument {
    source: String,
    parse: aven_parser::ParseOutput,
    declarations: Vec<aven_parser::Declaration>,
}

impl ParsedDocument {
    fn new(source: String) -> Self {
        let parse = aven_parser::parse_module(&source);
        let declarations = aven_parser::collect_declarations(&parse.module);

        Self {
            source,
            parse,
            declarations,
        }
    }
}

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
        let text = params.text_document.text;

        self.set_document(uri.clone(), text);
        self.publish_diagnostics(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };

        let uri = params.text_document.uri;
        let text = change.text;

        self.set_document(uri.clone(), text);
        self.publish_diagnostics(uri).await;
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let Some(document) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };

        let Ok(formatted) = aven_fmt::format_source(&document.source) else {
            return Ok(None);
        };

        if formatted == document.source {
            return Ok(Some(Vec::new()));
        }

        Ok(Some(vec![TextEdit {
            range: full_document_range(&document.source),
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
}

impl Backend {
    fn set_document(&self, uri: Url, text: String) {
        if let Ok(mut documents) = self.documents.lock() {
            documents.insert(uri, Arc::new(ParsedDocument::new(text)));
        }
    }

    fn document(&self, uri: &Url) -> Option<Arc<ParsedDocument>> {
        // A poisoned mutex degrades to "document missing" rather than crashing the LSP.
        self.documents
            .lock()
            .ok()
            .and_then(|documents| documents.get(uri).cloned())
    }

    async fn publish_diagnostics(&self, uri: Url) {
        let Some(document) = self.document(&uri) else {
            return;
        };

        let diagnostics = document
            .parse
            .diagnostics
            .iter()
            .map(|diagnostic| to_lsp_diagnostic(&document.source, diagnostic))
            .collect();

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }
}

fn to_lsp_diagnostic(source: &str, diagnostic: &AvenDiagnostic) -> Diagnostic {
    let span = diagnostic
        .labels
        .first()
        .map(|label| label.span)
        .unwrap_or_else(|| Span::point(0));

    Diagnostic {
        range: span_to_range(source, span),
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
        .declarations
        .iter()
        .map(|declaration| declaration_symbol(&document.source, declaration))
        .collect()
}

#[allow(deprecated)]
fn declaration_symbol(source: &str, declaration: &aven_parser::Declaration) -> DocumentSymbol {
    DocumentSymbol {
        name: declaration.name.clone(),
        detail: declaration_detail(declaration),
        kind: symbol_kind(declaration),
        tags: None,
        deprecated: None,
        range: span_to_range(source, declaration.span),
        selection_range: span_to_range(source, declaration.name_span),
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
        &document.parse.module,
        &identifier.name,
        identifier.span,
    ) {
        return Some(Location::new(uri, span_to_range(&document.source, span)));
    }

    // TODO(milestone-6): resolve block bindings and pattern bindings before
    // falling back to top-level declarations.
    let declaration = document
        .declarations
        .iter()
        .find(|declaration| declaration.name == identifier.name)?;

    Some(Location::new(
        uri,
        span_to_range(&document.source, declaration.name_span),
    ))
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
    let offset = position_to_offset(&document.source, position)?;

    document.parse.raw_tokens.iter().find_map(|token| {
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

fn span_to_range(source: &str, span: Span) -> Range {
    Range {
        start: offset_to_position(source, span.start),
        end: offset_to_position(source, span.end.max(span.start + 1)),
    }
}

fn position_to_offset(source: &str, target: Position) -> Option<usize> {
    let mut line = 0u32;
    let mut character = 0u32;

    for (offset, ch) in source.char_indices() {
        if line == target.line && character >= target.character {
            return Some(offset);
        }

        if ch == '\n' {
            if line == target.line {
                return Some(offset);
            }

            line += 1;
            character = 0;
            continue;
        }

        if line == target.line {
            let next_character = character + ch.len_utf16() as u32;
            if target.character < next_character {
                return Some(offset);
            }
            character = next_character;
        }
    }

    (line == target.line).then_some(source.len())
}

fn full_document_range(source: &str) -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: offset_to_position(source, source.len()),
    }
}

fn offset_to_position(source: &str, target: usize) -> Position {
    let mut line = 0u32;
    let mut column = 0u32;

    for (offset, ch) in source.char_indices() {
        if offset >= target {
            break;
        }

        if ch == '\n' {
            line += 1;
            column = 0;
        } else {
            column += ch.len_utf16() as u32;
        }
    }

    Position {
        line,
        character: column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_symbols_include_top_level_bindings() {
        let document = ParsedDocument::new(
            "User = { name = Text }\ndouble = (x) => x\nvalue = 1\n".to_owned(),
        );
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
        let document = ParsedDocument::new("double : (Int) -> Int\ndouble = (x) => x\n".to_owned());
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
        let document = ParsedDocument::new("value : Int\nother = 1\n".to_owned());
        let symbols = document_symbols(&document);

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "value");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(symbols[0].detail.as_deref(), Some("signature"));
        assert_eq!(symbols[1].name, "other");
        assert_eq!(symbols[1].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn definition_location_finds_top_level_runtime_bindings() {
        let document = ParsedDocument::new("value = 1\nother = value\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 9)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(0, 0));
        assert_eq!(location.range.end, position(0, 5));
    }

    #[test]
    fn definition_location_finds_top_level_comptime_bindings() {
        let document = ParsedDocument::new("User = { name = Text }\nvalue = User\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 9)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(0, 0));
        assert_eq!(location.range.end, position(0, 4));
    }

    #[test]
    fn definition_location_prefers_lambda_parameters_over_top_level_bindings() {
        let document = ParsedDocument::new("x = 1\nf = (x) => x\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 11)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(1, 5));
        assert_eq!(location.range.end, position(1, 6));
    }

    #[test]
    fn definition_location_uses_nearest_lambda_parameter() {
        let document = ParsedDocument::new("x = 1\nf = (x) => (x) => x\n".to_owned());
        let Some(location) = definition_location(&document, test_uri(), position(1, 18)) else {
            panic!("expected definition location");
        };

        assert_eq!(location.range.start, position(1, 12));
        assert_eq!(location.range.end, position(1, 13));
    }

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn test_uri() -> Url {
        match Url::parse("file:///test.av") {
            Ok(uri) => uri,
            Err(error) => panic!("failed to parse test URI: {error}"),
        }
    }
}
