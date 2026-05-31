use std::collections::HashMap;
use std::sync::Arc;

use aven_core::{Diagnostic as AvenDiagnostic, Severity, Span};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    InitializeParams, InitializeResult, InitializedParams, MessageType, OneOf, Position, Range,
    ServerCapabilities, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit,
    Url,
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
    documents: Arc<std::sync::Mutex<HashMap<Url, String>>>,
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

        self.set_document(uri.clone(), text.clone());
        self.publish_diagnostics(uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };

        let uri = params.text_document.uri;
        let text = change.text;

        self.set_document(uri.clone(), text.clone());
        self.publish_diagnostics(uri, &text).await;
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let Some(source) = self.document_text(&params.text_document.uri) else {
            return Ok(None);
        };

        let Ok(formatted) = aven_fmt::format_source(&source) else {
            return Ok(None);
        };

        if formatted == source {
            return Ok(Some(Vec::new()));
        }

        Ok(Some(vec![TextEdit {
            range: full_document_range(&source),
            new_text: formatted,
        }]))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(source) = self.document_text(&params.text_document.uri) else {
            return Ok(None);
        };

        Ok(Some(DocumentSymbolResponse::Nested(document_symbols(
            &source,
        ))))
    }
}

impl Backend {
    fn set_document(&self, uri: Url, text: String) {
        if let Ok(mut documents) = self.documents.lock() {
            documents.insert(uri, text);
        }
    }

    fn document_text(&self, uri: &Url) -> Option<String> {
        // A poisoned mutex degrades to "document missing" rather than crashing the LSP.
        self.documents
            .lock()
            .ok()
            .and_then(|documents| documents.get(uri).cloned())
    }

    async fn publish_diagnostics(&self, uri: Url, text: &str) {
        let output = aven_parser::parse_module(text);
        let diagnostics = output
            .diagnostics
            .iter()
            .map(|diagnostic| to_lsp_diagnostic(text, diagnostic))
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

fn document_symbols(source: &str) -> Vec<DocumentSymbol> {
    let output = aven_parser::parse_module(source);
    let mut symbols = Vec::new();
    let mut items = output.module.items.iter().peekable();

    while let Some(item) = items.next() {
        match item {
            aven_parser::Item::Signature(signature) => {
                if let Some(aven_parser::Item::Binding(binding)) = items.peek()
                    && binding.name == signature.name
                {
                    symbols.push(binding_symbol(source, binding, Some(signature)));
                    items.next();
                    continue;
                }

                symbols.push(signature_symbol(source, signature));
            }
            aven_parser::Item::Binding(binding) => {
                symbols.push(binding_symbol(source, binding, None));
            }
            aven_parser::Item::Expr(_) => {}
        }
    }

    symbols
}

#[allow(deprecated)]
fn binding_symbol(
    source: &str,
    binding: &aven_parser::Binding,
    signature: Option<&aven_parser::Signature>,
) -> DocumentSymbol {
    let range_span = signature.map_or(binding.span, |signature| signature.span.merge(binding.span));

    DocumentSymbol {
        name: binding.name.clone(),
        detail: signature.map(|_| "binding with signature".to_owned()),
        kind: binding_symbol_kind(binding),
        tags: None,
        deprecated: None,
        range: span_to_range(source, range_span),
        selection_range: span_to_range(source, binding.name_span),
        children: None,
    }
}

#[allow(deprecated)]
fn signature_symbol(source: &str, signature: &aven_parser::Signature) -> DocumentSymbol {
    DocumentSymbol {
        name: signature.name.clone(),
        detail: Some("signature".to_owned()),
        kind: symbol_kind_for_name(&signature.name, SymbolKind::FUNCTION),
        tags: None,
        deprecated: None,
        range: span_to_range(source, signature.span),
        selection_range: span_to_range(source, signature.name_span),
        children: None,
    }
}

fn binding_symbol_kind(binding: &aven_parser::Binding) -> SymbolKind {
    if matches!(binding.value.kind, aven_parser::ExprKind::Lambda { .. }) {
        return SymbolKind::FUNCTION;
    }

    symbol_kind_for_name(&binding.name, SymbolKind::VARIABLE)
}

fn symbol_kind_for_name(name: &str, fallback: SymbolKind) -> SymbolKind {
    if name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
    {
        return SymbolKind::STRUCT;
    }

    fallback
}

fn span_to_range(source: &str, span: Span) -> Range {
    Range {
        start: offset_to_position(source, span.start),
        end: offset_to_position(source, span.end.max(span.start + 1)),
    }
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
        let symbols = document_symbols("User = { name = Text }\ndouble = (x) => x\nvalue = 1\n");

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
        let symbols = document_symbols("double : (Int) -> Int\ndouble = (x) => x\n");

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
}
