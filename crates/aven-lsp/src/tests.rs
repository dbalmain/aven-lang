use std::fs;
use std::future;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    document.with_semantic(
        semantic.diagnostics,
        semantic.inferred_types,
        semantic.type_definitions,
        semantic.recursive_type_unfoldings,
        semantic.builtin_methods,
        semantic.named_families,
    )
}

fn parsed_file_document(uri: &Url, source: impl Into<String>) -> ParsedDocument {
    let file = SourceFile::new(
        FileId(0),
        source_name(uri),
        uri.to_file_path().ok(),
        source.into(),
    );
    aven_compiler::DocumentSnapshot::parse(aven_compiler::Revision::default(), file)
}

fn analyze_file_document(
    store: &mut DocumentStore,
    uri: &Url,
    source: impl Into<String>,
) -> Arc<ParsedDocument> {
    store.set_document(uri.clone(), 1, source.into());
    let (document, overlay) = store.semantic_input(uri).expect("expected semantic input");
    let analysis = analyze_document_semantics_for_uri(uri, &document, &overlay);
    assert!(store.set_semantic(uri, 1, analysis));
    store.document(uri).expect("expected stored document")
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
        vec![
            "trace", "debug", "info", "warn", "error", "fatal", "child", "encode"
        ]
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
        vec![
            "trace", "debug", "info", "warn", "error", "fatal", "child", "encode"
        ]
    );
    assert!(completion_item(&completions, "Text").is_none());
    assert!(completion_item(&completions, "logger").is_none());
}

#[test]
fn pathless_completion_includes_ambient_array_methods() {
    let completions = completions_at_marker("xs = [3, 1, 2]\nxs.|");
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    for method in ["length", "map", "sortBy", "minimum", "sum"] {
        assert!(
            labels.contains(&method),
            "expected ambient `{method}` completion, got {labels:?}"
        );
    }
    let sort_by = completion_item(&completions, "sortBy").expect("sortBy completion");
    assert_eq!(sort_by.kind, Some(CompletionItemKind::FIELD));
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
        vec![
            "trace", "debug", "info", "warn", "error", "fatal", "child", "encode"
        ]
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
    let Some(encode) = completion_item(&completions, "encode") else {
        panic!("expected encode field completion");
    };

    assert_eq!(labels, vec!["name", "email", "encode"]);
    assert_eq!(name.kind, Some(CompletionItemKind::FIELD));
    assert_eq!(name.detail.as_deref(), Some("Text"));
    assert_eq!(email.kind, Some(CompletionItemKind::FIELD));
    assert_eq!(email.detail.as_deref(), Some("Text"));
    assert_eq!(encode.kind, Some(CompletionItemKind::FIELD));
    assert!(completion_item(&completions, "decode").is_none());
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
    let completions = completions_at_marker("m : Map(Text, Int) = Map.empty()\nm.|");
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        labels,
        vec![
            "get", "set", "delete", "has", "keys", "values", "entries", "size", "merge", "encode"
        ]
    );
    let Some(get) = completion_item(&completions, "get") else {
        panic!("expected Map.get completion");
    };
    assert_eq!(get.detail.as_deref(), Some("Text -> ?Int"));
}

#[test]
fn completion_at_result_field_access_returns_builtin_methods() {
    let completions = completions_at_marker("r : Result(Int, Text) = @Ok(1)\nvalue = r.|");
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    for name in [
        "mapErr", "orElse", "map", "andThen", "unwrapOr", "isOk", "isErr",
    ] {
        assert!(
            labels.contains(&name),
            "expected Result.{name} completion, got {labels:?}"
        );
    }
    let Some(map) = completion_item(&completions, "map") else {
        panic!("expected Result.map completion");
    };
    assert_eq!(
        map.detail.as_deref(),
        Some("(Int -> result_ok) -> Result(result_ok, Text)")
    );
    let Some(and_then) = completion_item(&completions, "andThen") else {
        panic!("expected Result.andThen completion");
    };
    assert_eq!(
        and_then.detail.as_deref(),
        Some("(Int -> Result(result_ok, Text)) -> Result(result_ok, Text)")
    );
    let Some(is_ok) = completion_item(&completions, "isOk") else {
        panic!("expected Result.isOk completion");
    };
    assert_eq!(is_ok.detail.as_deref(), Some("() -> Bool"));
}

#[test]
fn completion_at_optional_field_access_offers_to_result() {
    let completions = completions_at_marker("value : ?Int = undefined\nresult = value.|");
    let Some(to_result) = completion_item(&completions, "toResult") else {
        panic!("expected toResult completion, got {completions:?}");
    };

    assert_eq!(to_result.kind, Some(CompletionItemKind::FIELD));
    assert_eq!(
        to_result.detail.as_deref(),
        Some("result_error -> Result(Int, result_error)")
    );
    assert!(
        to_result.additional_text_edits.is_none(),
        "toResult completion must keep plain `.toResult` dispatch"
    );
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
    assert_eq!(detail, "(fmt, target = _) -> decoded");
    assert!(!detail.contains('?'));
    assert_eq!(encode.kind, Some(CompletionItemKind::FIELD));
    let detail = encode
        .detail
        .as_deref()
        .expect("encode carries a signature");
    assert_eq!(detail, "fmt -> Text");
    assert!(!detail.contains('?'));
}

#[test]
fn completion_at_text_field_access_returns_builtin_methods() {
    let completions = completions_at_marker("text : Text = \"hi\"\nresult = text.|");
    let labels = completions
        .iter()
        .map(|item| item.label.as_str())
        .collect::<Vec<_>>();

    for name in [
        "isEmpty",
        "contains",
        "startsWith",
        "endsWith",
        "trim",
        "trimStart",
        "trimEnd",
        "toLower",
        "toUpper",
        "replaceEach",
        "replaceFirst",
        "dropPrefix",
        "dropSuffix",
        "repeat",
        "splitOn",
        "toInt",
        "toFloat",
    ] {
        assert!(
            labels.contains(&name),
            "expected Text.{name} completion, got {labels:?}"
        );
    }
    assert!(!labels.contains(&"length") && !labels.contains(&"len"));

    let Some(is_empty) = completion_item(&completions, "isEmpty") else {
        panic!("expected Text.isEmpty completion");
    };
    assert_eq!(is_empty.detail.as_deref(), Some("() -> Bool"));
    let Some(repeat) = completion_item(&completions, "repeat") else {
        panic!("expected Text.repeat completion");
    };
    assert_eq!(repeat.detail.as_deref(), Some("Int -> Text"));
    let Some(to_int) = completion_item(&completions, "toInt") else {
        panic!("expected Text.toInt completion");
    };
    assert_eq!(to_int.detail.as_deref(), Some("() -> ?Int"));
    let Some(to_float) = completion_item(&completions, "toFloat") else {
        panic!("expected Text.toFloat completion");
    };
    assert_eq!(to_float.detail.as_deref(), Some("() -> ?Float"));
}

#[test]
fn completion_at_record_field_access_keeps_real_encode_member_detail() {
    let completions =
        completions_at_marker("user : { encode: (Int) -> Text } = current\nresult = user.|\n");
    let Some(encode) = completion_item(&completions, "encode") else {
        panic!("expected real encode completion, got {completions:?}");
    };

    assert_eq!(encode.detail.as_deref(), Some("Int -> Text"));
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
    assert_eq!(labels, vec!["name", "email", "encode"]);
    assert!(completion_item(&completions, "decode").is_none());
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
fn completion_on_recursive_record_receiver_unfolds_one_level() {
    let completions = completions_at_marker(
        "Node = { value: Int, next: ?Node }\nnode: Node = { value: 1 }\nresult = node.|\n",
    );

    let Some(value) = completion_item(&completions, "value") else {
        panic!("expected value field completion, got {completions:?}");
    };
    let Some(next) = completion_item(&completions, "next") else {
        panic!("expected next field completion, got {completions:?}");
    };
    assert_eq!(value.detail.as_deref(), Some("Int"));
    assert_eq!(next.detail.as_deref(), Some("?Node"));
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
fn completion_for_recursive_variant_construction_unfolds_one_level() {
    let completions = completions_at_marker(
        "List = (t: Type) => @{ @Nil, @Cons((t, List(t))) }\nxs: List(Int) = @{ | }\n",
    );

    assert!(
        completion_item(&completions, "@Nil").is_some(),
        "{completions:?}"
    );
    assert!(
        completion_item(&completions, "@Cons").is_some(),
        "{completions:?}"
    );
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
    // Capability modules only — not bare host globals.
    assert!(completion_item(&completions, "now").is_none());
    assert!(completion_item(&completions, "zone").is_none());
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
    assert_eq!(
        labels,
        vec!["get", "post", "put", "delete", "patch", "encode"]
    );
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
    assert_eq!(labels, vec!["open", "encode"]);
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
        markup.value.contains("(Text, { .. } = _)") && markup.value.contains("-> Result("),
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
    let document = document.with_semantic(
        semantic.diagnostics,
        semantic.inferred_types,
        semantic.type_definitions,
        semantic.recursive_type_unfoldings,
        semantic.builtin_methods,
        semantic.named_families,
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
fn file_backed_semantic_diagnostics_use_entry_buffer_overlay() {
    let dir = TempDir::new("lsp-entry-overlay");
    write(dir.path(), "dep.av", "value = 1\n{ value }\n");
    write(dir.path(), "main.av", "value : Int = \"disk\"\n{ value }\n");
    let main_uri = file_uri(&dir.path().join("main.av"));
    let mut store = DocumentStore::default();
    store.set_document(
        main_uri.clone(),
        1,
        "dep = import(\"./dep\")\nvalue : Int = dep.value\n{ value }\n".to_owned(),
    );
    let (document, overlay) = store
        .semantic_input(&main_uri)
        .expect("expected semantic input");

    let semantic = analyze_document_semantics_for_uri(&main_uri, &document, &overlay);

    assert_no_aven_diagnostics(&semantic.diagnostics);
}

#[test]
fn file_backed_semantic_diagnostics_use_dependency_buffer_overlay() {
    let dir = TempDir::new("lsp-dependency-overlay");
    write(dir.path(), "dep.av", "value = \"disk\"\n{ value }\n");
    write(
        dir.path(),
        "main.av",
        "dep = import(\"./dep\")\nvalue : Int = dep.value\n{ value }\n",
    );
    let dep_uri = file_uri(&dir.path().join("dep.av"));
    let main_uri = file_uri(&dir.path().join("main.av"));
    let mut store = DocumentStore::default();
    store.set_document(dep_uri, 1, "value = 1\n{ value }\n".to_owned());
    store.set_document(
        main_uri.clone(),
        1,
        "dep = import(\"./dep\")\nvalue : Int = dep.value\n{ value }\n".to_owned(),
    );
    let (document, overlay) = store
        .semantic_input(&main_uri)
        .expect("expected semantic input");

    let semantic = analyze_document_semantics_for_uri(&main_uri, &document, &overlay);

    assert_no_aven_diagnostics(&semantic.diagnostics);
}

#[test]
fn file_backed_documents_resolve_the_std_library() {
    let dir = TempDir::new("lsp-std-library");
    let source = "time = import(\"std/time\")\n\
                  start : time.Instant = time.Instant.parse(\"2026-01-01T00:00:00Z\")?!\n\
                  { start }\n";
    write(dir.path(), "main.av", source);
    let main_uri = file_uri(&dir.path().join("main.av"));
    let mut store = DocumentStore::default();
    store.set_document(main_uri.clone(), 1, source.to_owned());
    let (document, overlay) = store
        .semantic_input(&main_uri)
        .expect("expected semantic input");

    let semantic = analyze_document_semantics_for_uri(&main_uri, &document, &overlay);

    assert_no_aven_diagnostics(&semantic.diagnostics);
}

#[test]
fn file_backed_semantic_diagnostics_report_missing_dependency() {
    let dir = TempDir::new("lsp-missing-dependency");
    write(
        dir.path(),
        "main.av",
        "missing = import(\"./missing\")\n{ missing }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    let mut store = DocumentStore::default();
    store.set_document(
        main_uri.clone(),
        1,
        "missing = import(\"./missing\")\n{ missing }\n".to_owned(),
    );
    let (document, overlay) = store
        .semantic_input(&main_uri)
        .expect("expected semantic input");

    let semantic = analyze_document_semantics_for_uri(&main_uri, &document, &overlay);

    assert_has_aven_code(&semantic.diagnostics, codes::module::NOT_FOUND);
    assert_lacks_aven_code(&semantic.diagnostics, codes::module::UNRESOLVED_IMPORT);
}

#[test]
fn file_backed_member_completion_uses_import_aware_entry_semantics() {
    let dir = TempDir::new("lsp-import-completion");
    write(
        dir.path(),
        "lib/text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "text = import(\"./lib/text\")\nvalue = text.\n{ value }\n",
    );

    let completions = completion_at_position_for_uri(&document, &main_uri, position(1, 13));
    let Some(join) = completion_item(&completions, "join") else {
        panic!("expected join completion");
    };

    assert_eq!(join.detail.as_deref(), Some("Text -> Text"));
}

#[test]
fn file_backed_type_export_completion_hover_and_goto() {
    let dir = TempDir::new("lsp-type-export");
    write(
        dir.path(),
        "lib/util.av",
        "User = { name: Text, age: Int }\ngreet = (u: User): Text => u.name\n{ greet, User }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    let dep_uri = file_uri(&dir.path().join("lib/util.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "util = import(\"./lib/util\")\nf = (u: util.User): Text => u.name\nvalue = util.\n{ f, value }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let completions = completion_at_position_for_uri(&document, &main_uri, position(2, 13));
    let Some(user) = completion_item(&completions, "User") else {
        panic!("expected User type-export completion, got {completions:?}");
    };
    assert!(
        user.detail
            .as_deref()
            .is_some_and(|detail| detail.contains("name") && detail.contains("Text")),
        "expected User detail to show the reified type, got {:?}",
        user.detail
    );

    let Some(hover) = hover_at_position(&document, position(1, 14)) else {
        panic!("expected hover on util.User");
    };
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(
        markup.value.contains("User") && markup.value.contains("name"),
        "expected hover to show User type, got {}",
        markup.value
    );

    let Some(location) = definition_location(&document, main_uri, position(1, 14), Some(&graph))
    else {
        panic!("expected goto definition on util.User");
    };
    assert_eq!(location.uri, dep_uri);
    assert_eq!(location.range.start, position(0, 0));
    assert_eq!(location.range.end, position(0, 4));
}

#[test]
fn file_backed_hover_on_import_binding_shows_record_type() {
    let dir = TempDir::new("lsp-import-hover");
    write(
        dir.path(),
        "lib/text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "text = import(\"./lib/text\")\nvalue = text\n{ value }\n",
    );

    let Some(hover) = hover_at_position(&document, position(1, 9)) else {
        panic!("expected hover");
    };
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };

    assert!(markup.value.contains("join: Text -> Text"));
}

#[test]
fn file_backed_hover_on_import_member_encode_shows_method_signature() {
    let dir = TempDir::new("lsp-import-encode-hover");
    write(
        dir.path(),
        "lib/data.av",
        "someRecord = { name: \"Ada\" }\n{ someRecord }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "m = import(\"./lib/data\")\nvalue = m.someRecord.encode(Json)\n{ value }\n",
    );

    let Some(hover) = hover_at_position(&document, position(1, 23)) else {
        panic!("expected hover on encode");
    };

    assert_hover_value(hover, "```aven\nsomeRecord.encode : Json -> Text\n```");
}

#[test]
fn file_backed_goto_on_import_member_jumps_to_export_definition() {
    let dir = TempDir::new("lsp-import-member-goto");
    write(
        dir.path(),
        "lib/text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    let dep_uri = file_uri(&dir.path().join("lib/text.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "text = import(\"./lib/text\")\nvalue = text.join\n{ value }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let Some(location) = definition_location(&document, main_uri, position(1, 14), Some(&graph))
    else {
        panic!("expected definition location");
    };

    assert_eq!(location.uri, dep_uri);
    assert_eq!(location.range.start, position(0, 0));
    assert_eq!(location.range.end, position(0, 4));
}

#[test]
fn file_backed_goto_on_import_pattern_site_jumps_to_export_definition() {
    let dir = TempDir::new("lsp-import-pattern-goto");
    write(
        dir.path(),
        "lib/text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    let dep_uri = file_uri(&dir.path().join("lib/text.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "{ join } = import(\"./lib/text\")\nvalue = join\n{ value }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let Some(location) =
        definition_location(&document, main_uri.clone(), position(0, 3), Some(&graph))
    else {
        panic!("expected definition location");
    };
    assert_eq!(location.uri, dep_uri);
    assert_eq!(location.range.start, position(0, 0));
    assert_eq!(location.range.end, position(0, 4));

    let Some(local_location) =
        definition_location(&document, main_uri.clone(), position(1, 9), Some(&graph))
    else {
        panic!("expected local definition location");
    };
    assert_eq!(local_location.uri, main_uri);
    assert_eq!(local_location.range.start, position(0, 2));
    assert_eq!(local_location.range.end, position(0, 6));
}

#[test]
fn file_backed_goto_on_import_specifier_jumps_to_file_start() {
    let dir = TempDir::new("lsp-import-specifier-goto");
    write(dir.path(), "lib/text.av", "join = 1\n{ join }\n");
    let main_uri = file_uri(&dir.path().join("main.av"));
    let dep_uri = file_uri(&dir.path().join("lib/text.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "text = import(\"./lib/text\")\n{ text }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let Some(location) = definition_location(&document, main_uri, position(0, 17), Some(&graph))
    else {
        panic!("expected definition location");
    };

    assert_eq!(location.uri, dep_uri);
    assert_eq!(location.range.start, position(0, 0));
}

#[test]
fn file_backed_goto_on_project_root_import_specifier() {
    let dir = TempDir::new("lsp-project-root-import-goto");
    write(dir.path(), "Aven.toml", "");
    write(dir.path(), "lib/text.av", "join = 1\n{ join }\n");
    let main_uri = file_uri(&dir.path().join("src/main.av"));
    let dep_uri = file_uri(&dir.path().join("lib/text.av"));
    write(dir.path(), "src/main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "text = import(\"$/lib/text\")\n{ text }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    // Cursor on the specifier string: import("$/lib/text")
    let Some(location) = definition_location(&document, main_uri, position(0, 17), Some(&graph))
    else {
        panic!("expected definition location for $/ import");
    };

    assert_eq!(location.uri, dep_uri);
    assert_eq!(location.range.start, position(0, 0));
}

#[test]
fn library_interface_renders_std_result_signatures() {
    let dir = TempDir::new("lsp-std-result-interface");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    analyze_file_document(
        &mut store,
        &main_uri,
        "result = import(\"std/result\")\n{ result }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let interface = graph.nodes[Path::new("std:/result")]
        .interface
        .as_ref()
        .expect("expected std/result interface");

    let lines: Vec<&str> = interface.text.lines().collect();
    assert_eq!(
        lines[0],
        "# std/result — generated interface (shape view); not the implementation."
    );
    assert_eq!(lines[1], "");
    // Export order follows the final record: mapErr, orElse, map, andThen, unwrapOr, isOk, isErr.
    // Type-var names are normalized alphabetically in the shape view.
    assert_eq!(lines[2], "mapErr : (Result(a, b), b -> c) -> Result(a, c)");
    assert_eq!(
        lines[3],
        "orElse : (Result(a, b), b -> Result(a, c)) -> Result(a, c)"
    );
    assert_eq!(lines[4], "map : (Result(a, b), a -> c) -> Result(c, b)");
    assert_eq!(
        lines[5],
        "andThen : (Result(a, b), a -> Result(c, b)) -> Result(c, b)"
    );
    assert_eq!(lines[6], "unwrapOr : (Result(a, b), a) -> a");
    assert_eq!(lines[7], "isOk : Result(a, b) -> Bool");
    assert_eq!(lines[8], "isErr : Result(a, b) -> Bool");
    for (name, line) in [
        ("mapErr", 2),
        ("orElse", 3),
        ("map", 4),
        ("andThen", 5),
        ("unwrapOr", 6),
        ("isOk", 7),
        ("isErr", 8),
    ] {
        let span = interface.export_spans[name];
        assert_eq!(&interface.text[span.start..span.end], name);
        let index = aven_core::LineIndex::new(&interface.text);
        assert_eq!(
            index.offset_to_position(&interface.text, span.start).line,
            line
        );
    }
}

#[test]
fn library_interface_renders_std_array_signatures() {
    let dir = TempDir::new("lsp-std-array-interface");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    analyze_file_document(
        &mut store,
        &main_uri,
        "array = import(\"std/array\")\n{ array }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let interface = graph.nodes[Path::new("std:/array")]
        .interface
        .as_ref()
        .expect("expected std/array interface");

    let lines: Vec<&str> = interface.text.lines().collect();
    assert_eq!(
        lines[0],
        "# std/array — generated interface (shape view); not the implementation."
    );
    assert_eq!(lines[1], "");
    // Residual producer only; all transformers are ambient methods.
    let exported = ["range"];
    for (index, name) in exported.iter().enumerate() {
        assert!(
            lines[index + 2].starts_with(&format!("{name} : ")),
            "line: {:?}",
            lines[index + 2]
        );
    }
    for name in exported {
        let span = interface.export_spans[name];
        assert_eq!(&interface.text[span.start..span.end], name);
    }
    assert_eq!(interface.export_spans.len(), 1);
    for method in [
        "length", "isEmpty", "first", "last", "fold", "sum", "count", "all", "any", "find",
        "indexOf", "map", "flatMap", "filter", "reverse", "concat", "take", "drop", "slice", "zip",
        "flatten", "sortWith", "sortBy", "minimum", "maximum",
    ] {
        assert!(!interface.export_spans.contains_key(method));
    }
}

#[test]
fn library_interface_renders_std_map_signatures() {
    let dir = TempDir::new("lsp-std-map-interface");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    analyze_file_document(
        &mut store,
        &main_uri,
        "map = import(\"std/map\")\n{ map }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");
    let interface = graph.nodes[Path::new("std:/map")]
        .interface
        .as_ref()
        .expect("expected std/map interface");
    let lines: Vec<&str> = interface.text.lines().collect();

    assert_eq!(
        lines[0],
        "# std/map — generated interface (shape view); not the implementation."
    );
    for (index, name) in [
        "isEmpty",
        "getOr",
        "update",
        "fromEntries",
        "toEntries",
        "mapValues",
        "filter",
    ]
    .iter()
    .enumerate()
    {
        assert!(lines[index + 2].starts_with(&format!("{name} : ")));
        let span = interface.export_spans[*name];
        assert_eq!(&interface.text[span.start..span.end], *name);
    }
}

#[test]
fn library_interface_lists_std_time_type_exports() {
    let dir = TempDir::new("lsp-std-time-interface");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    analyze_file_document(
        &mut store,
        &main_uri,
        "time = import(\"std/time\")\n{ time }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    let interface = graph.nodes[Path::new("std:/time")]
        .interface
        .as_ref()
        .expect("expected std/time interface");

    let signatures: Vec<&str> = interface.text.lines().skip(2).collect();
    assert_eq!(
        signatures,
        [
            "Instant : Type",
            "Date : Type",
            "Time : Type",
            "DateTime : Type",
            "Duration : Type",
        ]
    );
}

#[test]
fn file_backed_goto_on_std_export_lands_in_generated_interface() {
    let dir = TempDir::new("lsp-std-goto");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "{ mapErr } = import(\"std/result\")\nresult = import(\"std/result\")\nvalue = result.mapErr\n{ value }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    // Pattern-site goto: { mapErr } = import("std/result").
    let Some(location) =
        definition_location(&document, main_uri.clone(), position(0, 3), Some(&graph))
    else {
        panic!("expected definition location for pattern-site goto");
    };
    let path = location.uri.to_file_path().expect("expected file URI");
    assert!(path.starts_with(interface_cache_dir()), "path: {path:?}");
    let text = fs::read_to_string(&path).expect("expected materialized interface");
    let line = text
        .lines()
        .nth(location.range.start.line as usize)
        .expect("expected target line");
    assert!(line.starts_with("mapErr :"), "line: {line:?}");
    assert_eq!(location.range.start.character, 0);

    // Member-access goto: result.mapErr.
    let Some(member_location) =
        definition_location(&document, main_uri, position(2, 16), Some(&graph))
    else {
        panic!("expected definition location for member goto");
    };
    assert_eq!(member_location, location);
}

#[test]
fn file_backed_goto_on_std_import_specifier_lands_at_interface_top() {
    let dir = TempDir::new("lsp-std-specifier-goto");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        "result = import(\"std/result\")\n{ result }\n",
    );
    let graph = store
        .module_graph(&main_uri)
        .expect("expected module graph");

    // Cursor on the specifier string: import("std/result").
    let Some(location) = definition_location(&document, main_uri, position(0, 20), Some(&graph))
    else {
        panic!("expected definition location for std specifier");
    };

    let path = location.uri.to_file_path().expect("expected file URI");
    assert_eq!(path, interface_cache_dir().join("std/result.av"));
    assert_eq!(location.range.start, position(0, 0));
}

#[test]
fn interface_cache_documents_are_detected_by_prefix() {
    let cached = file_uri(&interface_cache_dir().join("std/result.av"));
    assert!(is_interface_cache_document(&cached));

    let ordinary = file_uri(&std::env::temp_dir().join("aven-lsp-elsewhere/main.av"));
    assert!(!is_interface_cache_document(&ordinary));
}

#[tokio::test(flavor = "current_thread")]
async fn did_open_under_interface_cache_publishes_no_diagnostics() {
    let (mut service, mut socket) = LspService::new(test_backend);

    let initialize = Request::build("initialize")
        .params(json!({"capabilities": {}}))
        .id(1)
        .finish();
    assert!(call_service(&mut service, initialize).await.is_some());

    // A generated interface document; even with invalid content it must not
    // be analyzed or produce diagnostics.
    let cache_uri = file_uri(&interface_cache_dir().join("std/result.av"));
    let open_cached = Request::build("textDocument/didOpen")
        .params(json!({
            "textDocument": {
                "uri": cache_uri.to_string(),
                "languageId": "aven",
                "version": 1,
                "text": "definitely (((( not valid\n"
            }
        }))
        .finish();
    assert!(call_service(&mut service, open_cached).await.is_none());

    // Opening an ordinary document afterwards proves the cache document
    // published nothing: the first notification on the socket is for it.
    let uri = test_uri();
    let open_ordinary = Request::build("textDocument/didOpen")
        .params(json!({
            "textDocument": {
                "uri": uri.to_string(),
                "languageId": "aven",
                "version": 1,
                "text": "value = 1\n"
            }
        }))
        .finish();
    assert!(call_service(&mut service, open_ordinary).await.is_none());

    let publish = next_publish_diagnostics(&mut socket).await;
    assert_eq!(publish.uri, uri);
}

#[test]
fn file_backed_import_specifier_completion_lists_siblings() {
    let dir = TempDir::new("lsp-import-specifier-completion");
    write(dir.path(), "text.av", "value = 1\n{ value }\n");
    write(dir.path(), "lib/mod.av", "value = 1\n{ value }\n");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let document = parsed_file_document(&main_uri, "dep = import(\"./\")\n");

    let completions = completion_at_position_for_uri(&document, &main_uri, position(0, 16));

    assert!(
        completion_item(&completions, "text").is_some(),
        "labels: {:?}",
        completions
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion_item(&completions, "lib/").is_some());
}

#[test]
fn import_specifier_completion_lists_project_root_children() {
    let dir = TempDir::new("lsp-import-specifier-project-root");
    write(dir.path(), "Aven.toml", "");
    write(dir.path(), "text.av", "value = 1\n{ value }\n");
    write(dir.path(), "lib/mod.av", "value = 1\n{ value }\n");
    let main_uri = file_uri(&dir.path().join("src/main.av"));
    write(dir.path(), "src/main.av", "");
    let document = parsed_file_document(&main_uri, "dep = import(\"$/\")\n");

    let completions = completion_at_position_for_uri(&document, &main_uri, position(0, 16));

    assert!(
        completion_item(&completions, "text").is_some(),
        "labels: {:?}",
        completions
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion_item(&completions, "lib/").is_some());
}

#[test]
fn import_specifier_completion_offers_registered_library_names() {
    let dir = TempDir::new("lsp-import-specifier-library");
    write(dir.path(), "text.av", "value = 1\n{ value }\n");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let document = parsed_file_document(&main_uri, "dep = import(\"st\")\n");

    let completions = completion_at_position_for_uri(&document, &main_uri, position(0, 16));

    assert!(
        completion_item(&completions, "std").is_some(),
        "labels: {:?}",
        completions
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion_item(&completions, "text").is_none());
}

#[test]
fn import_specifier_completion_lists_std_module_paths() {
    let dir = TempDir::new("lsp-import-specifier-std-modules");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let document = parsed_file_document(&main_uri, "dep = import(\"std/\")\n");

    let completions = completion_at_position_for_uri(&document, &main_uri, position(0, 18));

    assert!(
        completion_item(&completions, "time").is_some(),
        "labels: {:?}",
        completions
            .iter()
            .map(|item| &item.label)
            .collect::<Vec<_>>()
    );
    assert!(completion_item(&completions, "clock").is_some());
    assert!(completion_item(&completions, "zones").is_some());
}

#[test]
fn import_specifier_completion_offers_nothing_for_unknown_library() {
    let dir = TempDir::new("lsp-import-specifier-unknown-library");
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let document = parsed_file_document(&main_uri, "dep = import(\"other/\")\n");

    let completions = completion_at_position_for_uri(&document, &main_uri, position(0, 20));

    assert!(completions.is_empty());
}

#[test]
fn pathless_semantic_diagnostics_keep_single_file_import_warning() {
    let document = parsed_document("dep = import(\"./dep\")\n{ dep }\n");

    let semantic = analyze_document_semantics(&document);

    assert_has_aven_code(&semantic.diagnostics, codes::module::UNRESOLVED_IMPORT);
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
        DocumentSemanticAnalysis {
            diagnostics: vec![AvenDiagnostic::error("semantic diagnostic")],
            inferred_types: Vec::new(),
            type_definitions: HashMap::new(),
            recursive_type_unfoldings: HashMap::new(),
            builtin_methods: aven_compiler::BuiltinMethodEnvironment::default(),
            named_families: HashMap::new(),
            module_graph: None,
        },
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
        DocumentSemanticAnalysis {
            diagnostics: vec![AvenDiagnostic::error("stale diagnostic")],
            inferred_types: Vec::new(),
            type_definitions: HashMap::new(),
            recursive_type_unfoldings: HashMap::new(),
            builtin_methods: aven_compiler::BuiltinMethodEnvironment::default(),
            named_families: HashMap::new(),
            module_graph: None,
        },
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
    let Some(location) = definition_location(&document, test_uri(), position(1, 9), None) else {
        panic!("expected definition location");
    };

    assert_eq!(location.range.start, position(0, 0));
    assert_eq!(location.range.end, position(0, 5));
}

#[test]
fn definition_location_finds_top_level_comptime_bindings() {
    let document = parsed_document("User = { name: Text }\nvalue = User\n".to_owned());
    let Some(location) = definition_location(&document, test_uri(), position(1, 9), None) else {
        panic!("expected definition location");
    };

    assert_eq!(location.range.start, position(0, 0));
    assert_eq!(location.range.end, position(0, 4));
}

#[test]
fn definition_location_prefers_lambda_parameters_over_top_level_bindings() {
    let document = parsed_document("x = 1\nf = (x) => x\n".to_owned());
    let Some(location) = definition_location(&document, test_uri(), position(1, 11), None) else {
        panic!("expected definition location");
    };

    assert_eq!(location.range.start, position(1, 5));
    assert_eq!(location.range.end, position(1, 6));
}

#[test]
fn definition_location_uses_nearest_lambda_parameter() {
    let document = parsed_document("x = 1\nf = (x) => (x) => x\n".to_owned());
    let Some(location) = definition_location(&document, test_uri(), position(1, 18), None) else {
        panic!("expected definition location");
    };

    assert_eq!(location.range.start, position(1, 12));
    assert_eq!(location.range.end, position(1, 13));
}

#[test]
fn definition_location_finds_block_bindings() {
    let document = parsed_document("f = () =>\n  x = 1\n  y = x\n".to_owned());
    let Some(location) = definition_location(&document, test_uri(), position(2, 6), None) else {
        panic!("expected definition location");
    };

    assert_eq!(location.range.start, position(1, 2));
    assert_eq!(location.range.end, position(1, 3));
}

#[test]
fn definition_location_finds_match_pattern_binders() {
    let document =
        parsed_document("f = (result) =>\n  result ?>\n    @Ok(value) => value\n".to_owned());
    let Some(location) = definition_location(&document, test_uri(), position(2, 18), None) else {
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
fn pathless_hover_shows_ambient_array_method_signature() {
    let hover =
        hover_at_marker("xs = [3, 1, 2]\nxs.so|rtBy((n) => n)\n").expect("ambient method hover");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };

    assert!(markup.value.contains("xs.sortBy :"), "{}", markup.value);
    assert!(markup.value.contains("Array(Int)"), "{}", markup.value);
}

#[test]
fn hover_shows_named_primitive_family_method_signature() {
    let hover = hover_at_marker(concat!(
        "Money = Int { toText(): Text => \"money\" }\n",
        "money = Money(1)\n",
        "money.to|Text\n",
    ))
    .expect("primitive-family method hover");

    assert_hover_value(hover, "```aven\nmoney.toText : () -> Text\n```");
}

#[test]
fn hover_shows_override_origin_on_family_method_declaration() {
    let hover = hover_at_marker(concat!(
        "Money = Int {\n",
        "  |+(other: Money): Money => Money(0)\n",
        "}\n",
    ))
    .expect("override declaration hover");

    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(
        markup.value.contains("Money.+") && markup.value.contains("(Money) -> Money"),
        "signature missing: {}",
        markup.value
    );
    assert!(
        markup.value.contains("overrides Int.+ (base)"),
        "override origin missing: {}",
        markup.value
    );
}

#[test]
fn definition_from_override_call_lands_on_declaration() {
    let source = concat!(
        "Money = Int {\n",
        "  toText(): Text => \"money\"\n",
        "}\n",
        "money = Money(1)\n",
        "label = money.to|Text()\n",
    );
    let marker = source
        .find('|')
        .unwrap_or_else(|| panic!("expected cursor marker"));
    let mut cleaned = source.to_owned();
    cleaned.remove(marker);
    let document = parsed_document_with_semantics(cleaned);
    let position = to_lsp_position(
        document
            .file()
            .line_index()
            .offset_to_position(document.source(), marker),
    );
    let location = definition_location(
        &document,
        Url::parse("file:///override-def.av").expect("uri"),
        position,
        None,
    )
    .expect("definition for override/local method call");

    // `toText` name starts after "Money = Int {\n  ".
    let expected_start = document.source().find("toText").expect("toText decl");
    let expected = span_to_range(
        &document,
        Span::new(expected_start, expected_start + "toText".len()),
    );
    assert_eq!(location.range, expected);
}

#[test]
fn hover_shows_container_primitive_family_method_signatures() {
    // `joinWith` is an inherited Array(Text) intrinsic; `csv` is a local method.
    // Neither needs the ambient std registry, so the bare document suffices.
    let inherited = hover_at_marker(concat!(
        "Tags = Array(Text) { csv(): Text => .joinWith(\",\") }\n",
        "tags = Tags([\"a\"])\n",
        "joined = tags.join|With(\",\")\n",
    ))
    .expect("inherited container-family method hover");
    assert_hover_value(inherited, "```aven\ntags.joinWith : Text -> Text\n```");

    let local = hover_at_marker(concat!(
        "Tags = Array(Text) { csv(): Text => .joinWith(\",\") }\n",
        "tags = Tags([\"a\"])\n",
        "rendered = tags.cs|v()\n",
    ))
    .expect("local container-family method hover");
    assert_hover_value(local, "```aven\ntags.csv : () -> Text\n```");
}

#[test]
fn file_backed_hover_preserves_imported_primitive_family_interface() {
    let dir = TempDir::new("lsp-import-primitive-family-hover");
    write(
        dir.path(),
        "money.av",
        concat!(
            "Money = Int { toText(): Text => \"money\" }\n",
            "{ Money }\n",
        ),
    );
    let main_uri = file_uri(&dir.path().join("main.av"));
    write(dir.path(), "main.av", "");
    let mut store = DocumentStore::default();
    let document = analyze_file_document(
        &mut store,
        &main_uri,
        concat!(
            "{ Money } = import(\"./money\")\n",
            "price: Money = 2599\n",
            "label = price.toText()\n",
        ),
    );

    let Some(hover) = hover_at_position(&document, position(2, 16)) else {
        panic!("expected imported primitive-family method hover");
    };

    assert_hover_value(hover, "```aven\nprice.toText : () -> Text\n```");
}

#[test]
fn hover_at_position_shows_inferred_operator_requirement() {
    let document = parsed_document_with_semantics(
        "less = (left: t, right: t): Bool => left < right\n".to_owned(),
    );
    let Some(hover) = hover_at_position(&document, position(0, 1)) else {
        panic!("expected hover");
    };

    assert_hover_value(
        hover,
        "```aven\nless : (a, a) -> Bool\n  a: { <(Self): Bool, .. }\n```",
    );
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
fn hover_at_encode_method_member_shows_method_signature() {
    let hover = hover_at_marker(
        "Y = { y: Int }\n\
         y: Y = { y: 2 }\n\
         encoded = y.en|code(Yaml)\n",
    )
    .expect("expected hover on encode member");

    assert_hover_value(hover, "```aven\ny.encode : Yaml -> Text\n```");
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
fn hover_renders_parameterized_recursive_annotation_by_name() {
    let document = parsed_document_with_semantics(
        "List = (t: Type) => @{ @Nil, @Cons((t, List(t))) }\n\
         xs: List(Int) = @Nil\n",
    );
    let Some(hover) = hover_at_position(&document, position(1, 1)) else {
        panic!("expected hover");
    };

    assert_hover_value(hover, "```aven\nxs : List(Int)\n```");
}

#[test]
fn hover_renders_zero_argument_recursive_annotation_by_name() {
    let document = parsed_document_with_semantics(
        "Node = { value: Int, next: ?Node }\nnode: Node = { value: 1 }\n",
    );
    let Some(hover) = hover_at_position(&document, position(1, 1)) else {
        panic!("expected hover");
    };

    assert_hover_value(hover, "```aven\nnode : Node\n```");
}

#[test]
fn recursive_types_do_not_loop_inlay_or_goto_queries() {
    let source = "Node = { value: Int, next: ?Node }\n\
                  node: Node = { value: 1 }\n\
                  next = node.next\n";
    let document = parsed_document_with_semantics(source);
    let hints = inlay_hints_in_range(&document, full_document_range(&document));
    assert!(hints.iter().any(|hint| {
        matches!(&hint.label, InlayHintLabel::String(label) if label == ": ?Node")
    }));

    let Some(location) = definition_location(&document, test_uri(), position(0, 29), None) else {
        panic!("expected goto definition for recursive Node reference");
    };
    assert_eq!(location.range.start, position(0, 0));
    assert_eq!(location.range.end, position(0, 4));
}

#[test]
fn hover_at_position_returns_none_when_inference_defers() {
    let document = parsed_document_with_semantics("value = missing\n".to_owned());
    let hover = hover_at_position(&document, position(0, 1));

    assert!(hover.is_none());
}

const COMPTIME_TYPE_SOURCE: &str = "User = { name: Text, email: Text }\n\
     partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
     Draft = partial(User)\n\
     value : Draft = { name: \"Ada\" }\n";

#[test]
fn hover_at_comptime_type_binding_definition_shows_reified_type() {
    let document = parsed_document_with_semantics(COMPTIME_TYPE_SOURCE);
    let Some(hover) = hover_at_position(&document, position(2, 1)) else {
        panic!("expected hover");
    };

    assert_hover_value(hover, "```aven\nDraft = { email: ?Text, name: ?Text }\n```");
}

#[test]
fn hover_at_comptime_type_binding_use_site_shows_reified_type() {
    let document = parsed_document_with_semantics(COMPTIME_TYPE_SOURCE);
    let Some(hover) = hover_at_position(&document, position(3, 9)) else {
        panic!("expected hover");
    };

    assert_hover_value(hover, "```aven\nDraft = { email: ?Text, name: ?Text }\n```");
}

#[test]
fn hover_at_comptime_type_function_definition_shows_comptime_marker() {
    let document = parsed_document_with_semantics(COMPTIME_TYPE_SOURCE);
    let Some(hover) = hover_at_position(&document, position(1, 1)) else {
        panic!("expected hover");
    };

    assert_hover_value(hover, "```aven\npartial : comptime type function\n```");
}

#[test]
fn hover_at_uppercase_comptime_type_function_definition_shows_comptime_marker() {
    let document = parsed_document_with_semantics(
        "Pair = (t: Type) => { first: t, second: t }\nvalue: Pair(Int) = { first: 1, second: 2 }\n"
            .to_owned(),
    );
    let Some(hover) = hover_at_position(&document, position(0, 1)) else {
        panic!("expected hover");
    };

    assert_hover_value(hover, "```aven\nPair : comptime type function\n```");
}

#[test]
fn hover_at_builtin_comptime_function_shows_description() {
    let document = parsed_document_with_semantics(COMPTIME_TYPE_SOURCE);
    let Some(hover) = hover_at_position(&document, position(1, 25)) else {
        panic!("expected hover");
    };

    assert_hover_value(
        hover,
        "```aven\nkeysOf : comptime type function\n```\n\
         The keys of a record type as a literal union — `keysOf(User)` is `\"email\" | \"name\"`.",
    );
}

#[test]
fn hover_at_deferred_runtime_lambda_gets_no_comptime_function_hover() {
    let document = parsed_document_with_semantics("merge = (a, b) => { ..a, ..b }\n");
    let Some(hover) = hover_at_position(&document, position(0, 1)) else {
        panic!("expected hover");
    };

    // A plain deferred runtime lambda keeps its inferred rendering; it must
    // not be misreported as a comptime type function.
    assert_hover_value(hover, "```aven\nmerge : ({ .. }, { .. }) -> { .. }\n```");
}

#[test]
fn hover_at_untyped_lambda_without_builtin_reference_gets_no_hover() {
    let document = parsed_document_with_semantics("wrap = (a) => missing(a)\n");
    let hover = hover_at_position(&document, position(0, 1));

    assert!(hover.is_none());
}

#[test]
fn hover_at_deferred_comptime_type_binding_gets_no_hover() {
    let source = format!("{COMPTIME_TYPE_SOURCE}Bad = partial(missing)\n");
    let document = parsed_document_with_semantics(source);
    let hover = hover_at_position(&document, position(4, 1));

    assert!(hover.is_none());
}

#[test]
fn builtin_comptime_hover_table_matches_checker_builtin_list() {
    let hover_names: Vec<&str> = COMPTIME_BUILTIN_HOVERS
        .iter()
        .map(|(name, _)| *name)
        .collect();

    assert_eq!(hover_names, aven_compiler::COMPTIME_BUILTIN_FUNCTIONS);
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

fn hover_at_marker(source: &str) -> Option<Hover> {
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

    hover_at_position(&document, position)
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

fn assert_no_aven_diagnostics(diagnostics: &[AvenDiagnostic]) {
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics, got {diagnostics:#?}"
    );
}

fn assert_has_aven_code(diagnostics: &[AvenDiagnostic], code: &str) {
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some(code)),
        "expected diagnostic code {code}, got {diagnostics:#?}"
    );
}

fn assert_lacks_aven_code(diagnostics: &[AvenDiagnostic], code: &str) {
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code.as_deref() != Some(code)),
        "expected no diagnostic code {code}, got {diagnostics:#?}"
    );
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

fn file_uri(path: &Path) -> Url {
    Url::from_file_path(path).unwrap_or_else(|()| panic!("failed to convert path to URI: {path:?}"))
}

fn write(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create parent directory");
    }
    fs::write(path, source).expect("failed to write source file");
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aven-lsp-{label}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("failed to create temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
