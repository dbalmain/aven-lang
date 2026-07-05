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
    let labels =
        match serde_json::from_value::<CompletionResponse>(value).expect("completion response") {
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
    let document =
        parsed_document_with_semantics("format : (Int) -> Text = (value) => \"ok\"\ncount = 1\n");
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
        "Data",
        "Float",
        "Int",
        "Map",
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
fn completion_at_json_static_access_lists_statics() {
    for format in ["Json", "Yaml", "Toml"] {
        let completions = completions_at_marker(&format!("text = {format}.|"));
        let labels = completions
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(labels, vec!["encode", "decode"], "{format} statics");
    }
}

#[test]
fn completion_at_map_static_access_lists_statics() {
    let completions = completions_at_marker("m = Map.|");
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    assert_eq!(labels, vec!["empty", "from"]);
}

#[test]
fn completion_at_map_field_access_returns_builtin_methods() {
    let completions = completions_at_marker("m : Map[Text, Int] = Map.empty()\nm.|");
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        labels,
        vec![
            "get", "set", "delete", "has", "keys", "values", "entries", "size", "merge"
        ]
    );
    let Some(get) = completion_item(&completions, "get") else {
        panic!("expected Map.get completion");
    };
    assert_eq!(get.detail.as_deref(), Some("Text -> ?Int"));
}

#[test]
fn completion_at_text_field_access_offers_format_methods() {
    let completions = completions_at_marker("text : Text = \"{}\"\nresult = text.|");
    let Some(decode) = completion_item(&completions, "decode") else {
        panic!("expected decode completion on a Text receiver, got {completions:?}");
    };
    let Some(encode) = completion_item(&completions, "encode") else {
        panic!("expected encode completion on a Text receiver, got {completions:?}");
    };

    assert_eq!(decode.kind, Some(CompletionItemKind::FIELD));
    let detail = decode
        .detail
        .as_deref()
        .expect("decode carries a signature");
    assert!(
        detail.contains("->"),
        "decode signature is a function type, got {detail}"
    );
    assert_eq!(encode.kind, Some(CompletionItemKind::FIELD));
    let detail = encode
        .detail
        .as_deref()
        .expect("encode carries a signature");
    assert!(
        detail.contains("->"),
        "encode signature is a function type, got {detail}"
    );
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
    let completions = completions_at_marker("File.open(\"x\", |)\n");
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
        // No quote typed yet: the edit is an empty-span insert of the literal.
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
            panic!("expected a text edit for {label}");
        };
        assert_eq!(edit.new_text, label);
        assert_eq!(edit.range.start, edit.range.end);
    }
}

#[test]
fn completion_after_typed_quote_replaces_the_quote() {
    // Triggered by the `"` itself: the edit replaces the lone quote so the
    // result is exactly `"r"`, not `""r"`.
    let completions = completions_at_marker("File.open(\"x\", \"|)\n");
    let Some(item) = completion_item(&completions, "\"r\"") else {
        panic!("expected \"r\" completion");
    };

    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected a text edit");
    };
    assert_eq!(edit.new_text, "\"r\"");
    // Range spans the single opening quote before the cursor.
    assert_eq!(edit.range.start.character, 15);
    assert_eq!(edit.range.end.character, 16);
}

#[test]
fn completion_inside_autopaired_quotes_consumes_the_closing_quote() {
    // With autopairs the buffer is `File.open("x", "")` and the cursor sits
    // between the quotes; the edit must replace both so we get `"r"`.
    let completions = completions_at_marker("File.open(\"x\", \"|\")\n");
    let Some(item) = completion_item(&completions, "\"r\"") else {
        panic!("expected \"r\" completion");
    };

    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected a text edit");
    };
    assert_eq!(edit.new_text, "\"r\"");
    assert_eq!(edit.range.start.character, 15);
    assert_eq!(edit.range.end.character, 17);
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
fn completion_at_host_record_field_access_returns_member_fields() {
    // `Http` is a capitalized host-record global; field access on it offers
    // the record's members the same way `logger.`/`stdout.` do, because the
    // document is checked with host globals and the receiver's type is
    // recorded at its span.
    let document = parsed_document_with_semantics("res = Http.get(\"u\")\n");
    let completions = completion_at_position(&document, position(0, 11));
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["get", "post", "put", "delete", "patch"]);
}

#[test]
fn completion_at_comptime_field_host_record_returns_member_fields() {
    // `File`'s `open` field is comptime-resolved (its base result is
    // `Deferred`), so `File`'s record type is never recorded in the type
    // table. Field completion must still offer `open` by falling back to the
    // host global — a regression guard for `File.` showing generic context.
    let document = parsed_document_with_semantics("h = File.\n");
    let completions = completion_at_position(&document, position(0, 9));
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["open"]);
}

#[test]
fn completion_through_optional_receiver_inserts_null_safe_operator() {
    // `users[0]` is `?{ name }` (an array element), so completing
    // a field through a plain `.` offers an edit inserting `?` before the
    // operator — accepting `name` yields `users[0]?.name`.
    let document = parsed_document_with_semantics("users = [{ name: \"Ada\" }]\nx = users[0].\n");
    let completions = completion_at_position(&document, position(1, 13));
    let item = completions
        .iter()
        .find(|item| item.label == "name")
        .expect("expected a `name` completion");
    let edits = item
        .additional_text_edits
        .as_ref()
        .expect("expected a `?` insertion edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "?");
    assert_eq!(edits[0].range.start, position(1, 12));
    assert_eq!(edits[0].range.end, position(1, 12));
}

#[test]
fn completion_through_already_null_safe_receiver_adds_no_edit() {
    // The user already typed `?.`, so no extra `?` should be inserted.
    let document = parsed_document_with_semantics("users = [{ name: \"Ada\" }]\nx = users[0]?.\n");
    let completions = completion_at_position(&document, position(1, 14));
    let item = completions
        .iter()
        .find(|item| item.label == "name")
        .expect("expected a `name` completion");
    assert!(item.additional_text_edits.is_none());
}

#[test]
fn completion_through_plain_record_receiver_adds_no_edit() {
    // A non-optional record receiver completes fields with no `?` edit.
    let document = parsed_document_with_semantics("user = { name: \"Ada\" }\nx = user.\n");
    let completions = completion_at_position(&document, position(1, 9));
    let item = completions
        .iter()
        .find(|item| item.label == "name")
        .expect("expected a `name` completion");
    assert!(item.additional_text_edits.is_none());
}

#[test]
fn signature_help_at_host_record_field_call_returns_member_signature() {
    let document = parsed_document_with_semantics("res = Http.get(\"u\")\n");
    let Some(help) = signature_help_at_position(&document, position(0, 15)) else {
        panic!("expected signature help inside Http.get(");
    };
    assert!(
        help.signatures[0]
            .label
            .starts_with("Http.get(Text, { .. })"),
        "unexpected signature label: {}",
        help.signatures[0].label
    );
    assert_eq!(help.active_parameter, Some(0));
}

#[test]
fn hover_at_host_record_field_shows_member_signature() {
    let document = parsed_document_with_semantics("res = Http.get(\"u\")\n");
    let Some(hover) = hover_at_position(&document, position(0, 12)) else {
        panic!("expected hover on the `get` member");
    };
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(
        markup.value.contains("(Text, { .. } = _)") && markup.value.contains("-> Result["),
        "unexpected member hover: {}",
        markup.value
    );
}

#[test]
fn hover_at_host_record_global_shows_record_type() {
    let document = parsed_document_with_semantics("res = Http.get(\"u\")\n");
    let Some(hover) = hover_at_position(&document, position(0, 7)) else {
        panic!("expected hover on the `Http` global");
    };
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(
        markup.value.contains("Http : { get:")
            && markup.value.contains("post:")
            && markup.value.contains("patch:"),
        "unexpected global hover: {}",
        markup.value
    );
}

#[test]
fn signature_help_at_open_call_uses_host_global_signature() {
    let document = parsed_document_with_semantics("File.open(\"x\", )\n");

    let Some(help) = signature_help_at_position(&document, position(0, 14)) else {
        panic!("expected signature help");
    };

    assert_eq!(help.active_parameter, Some(1));
    assert_eq!(
        help.signatures[0].label,
        "File.open(Text, \"r\" | \"w\" | \"a\" | \"rw\") -> ?"
    );

    let parameters = help.signatures[0]
        .parameters
        .as_ref()
        .expect("expected parameter labels");
    assert_eq!(parameters.len(), 2);
    assert_eq!(parameters[0].label, ParameterLabel::LabelOffsets([10, 14]));
    assert_eq!(parameters[1].label, ParameterLabel::LabelOffsets([16, 38]));
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
    let document = parsed_document_with_semantics("y = { x: 1 }\nz = { x: 2 }\na = { ..y, ..z }\n");
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
    let document =
        parsed_document_with_semantics("Base = { x: Int }\ndup : { ..Base, x: Text } = value\n");
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
    let document = parsed_document_with_file_id(FileId(7), "value : Missing = value\n".to_owned());
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
    let document = parsed_document_with_semantics("File.open(\"x\", \"r\")\n");
    let Some(hover) = hover_at_position(&document, position(0, 6)) else {
        panic!("expected hover");
    };

    assert_hover_value(
        hover,
        "```aven\nFile.open : (Text, \"r\" | \"w\" | \"a\" | \"rw\") -> ?\n```",
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
