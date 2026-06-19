use std::collections::HashSet;

use crate::checker::comptime_rhs_needs_evaluation;
use crate::*;
use aven_core::{Diagnostic, Span, codes};
use aven_parser::{Item, Literal, Module, collect_declarations, parse_module};

fn annotation<'a>(module: &'a Module, name: &str) -> &'a Expr {
    module
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == name => binding.annotation.as_ref(),
            Item::Signature(signature) if signature.name == name => Some(&signature.annotation),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected annotation for {name}"))
}

fn binding_value(source: &str) -> Expr {
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics for {source:?}: {:?}",
        output.diagnostics
    );

    output
        .module
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) => Some(binding.value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected binding for {source:?}"))
}

fn named(name: &str) -> Type {
    Type::Named(name.to_owned())
}

fn variable(name: &str) -> Type {
    Type::Variable(name.to_owned())
}

fn apply(callee: Type, args: Vec<Type>) -> Type {
    Type::Apply {
        callee: Box::new(callee),
        args,
    }
}

fn function(params: Vec<Type>, result: Type) -> Type {
    Type::Function {
        params,
        result: Box::new(result),
    }
}

fn nullable(ty: Type) -> Type {
    Type::Nullable(Box::new(ty))
}

fn field(name: &str, ty: Type) -> RowEntry {
    RowEntry::Field {
        name: name.to_owned(),
        ty,
        optional: false,
    }
}

fn literal_string(raw: &str) -> RowEntry {
    RowEntry::Literal {
        value: Literal::String(raw.to_owned()),
    }
}

fn literal_number(raw: &str) -> RowEntry {
    RowEntry::Literal {
        value: Literal::Number(raw.to_owned()),
    }
}

fn row_label(entry: &RowEntry) -> &str {
    match entry {
        RowEntry::Field { name, .. } | RowEntry::Tag { name, .. } => name,
        RowEntry::Literal { value } => match value {
            Literal::Number(value)
            | Literal::String(value)
            | Literal::Regex(value)
            | Literal::Path(value)
            | Literal::Label(value) => value,
        },
    }
}

fn nth_span(source: &str, needle: &str, occurrence: usize) -> Span {
    let start = source
        .match_indices(needle)
        .nth(occurrence)
        .map(|(start, _)| start)
        .unwrap_or_else(|| panic!("expected occurrence {occurrence} of {needle:?}"));
    Span::new(start, start + needle.len())
}

#[test]
fn renders_types_as_surface_syntax() {
    assert_eq!(
        Type::Record(Row {
            entries: vec![
                field("name", named("Text")),
                RowEntry::Field {
                    name: "phone".to_owned(),
                    ty: nullable(named("Text")),
                    optional: true,
                },
            ],
            tail: RowTail::Open,
        })
        .render(),
        "{ name: Text, phone?: Text?, .. }"
    );
    assert_eq!(
        Type::Variant(Row {
            entries: vec![
                RowEntry::Tag {
                    name: "Ok".to_owned(),
                    payload: vec![variable("t")],
                },
                RowEntry::Tag {
                    name: "Err".to_owned(),
                    payload: vec![variable("e")],
                },
                RowEntry::Tag {
                    name: "Done".to_owned(),
                    payload: Vec::new(),
                },
            ],
            tail: RowTail::Var(0),
        })
        .render(),
        "@{ @Ok(t), @Err(e), @Done, .. }"
    );
    assert_eq!(
        Type::Variant(Row {
            entries: vec![literal_string("\"waiting\""), literal_string("\"running\"")],
            tail: RowTail::Closed,
        })
        .render(),
        "@{ \"waiting\", \"running\" }"
    );
    assert_eq!(
        Type::Variant(Row {
            entries: vec![
                literal_number("0"),
                literal_number("1"),
                literal_number("2"),
            ],
            tail: RowTail::Closed,
        })
        .render(),
        "@{ 0, 1, 2 }"
    );
    assert_eq!(
        function(
            vec![function(vec![named("Int")], named("Text"))],
            named("Bool")
        )
        .render(),
        "(Int -> Text) -> Bool"
    );
    assert_eq!(
        function(vec![named("Int"), named("Text")], named("Bool")).render(),
        "(Int, Text) -> Bool"
    );
    assert_eq!(
        nullable(function(vec![named("Int")], named("Text"))).render(),
        "(Int -> Text)?"
    );
    assert_eq!(
        apply(named("Result"), vec![named("Int"), variable("e")]).render(),
        "Result[Int, e]"
    );
    assert_eq!(
        Type::Tuple(vec![Type::Meta(10), Type::Meta(10), Type::Deferred]).render(),
        "(a, a, ?)"
    );
}

#[test]
fn check_output_records_unannotated_local_inferred_types() {
    let source = "value =\n  local = \"hi\"\n  local\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
    assert_eq!(
        check
            .type_at(nth_span(source, "local", 0))
            .map(Type::render),
        Some("Text".to_owned())
    );
}

#[test]
fn check_output_records_annotated_declared_types() {
    let source = "person : { name: Text, .. } = current\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
    assert_eq!(
        check
            .type_at(nth_span(source, "person", 0))
            .map(Type::render),
        Some("{ name: Text, .. }".to_owned())
    );
}

#[test]
fn lowercase_type_variables_are_not_unknown_names() {
    let output = parse_module("id : (a) -> a\nid = (value) => value\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn top_level_comptime_declarations_are_known_type_names() {
    let output = parse_module("User = { name: Text }\nvalue : User = user\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn reports_cyclic_transparent_type_aliases() {
    let output = parse_module("A = B\nB = A\n");
    let check = check_module(&output.module);

    assert_eq!(check.diagnostics.len(), 2);
    assert!(
        check
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::CYCLIC_ALIAS))
    );
}

#[test]
fn detects_comptime_rhs_artifacts_without_evaluation() {
    let output = parse_module(
        "User = { name: Text }\n\
         UserAlias = User\n\
         Color = @{@Red, @Green}\n\
         HttpOk = 200\n\
         HttpOkAlias = HttpOk\n\
         Config = { status: HttpOk }\n\
         Computed = HttpOk + 1\n\
         ok = 200\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert!(checker.comptime_rhs_is_non_liftable_artifact("User"));
    assert!(checker.comptime_rhs_is_non_liftable_artifact("UserAlias"));
    assert!(checker.comptime_rhs_is_non_liftable_artifact("Color"));
    assert!(!checker.comptime_rhs_is_non_liftable_artifact("HttpOk"));
    assert!(!checker.comptime_rhs_is_non_liftable_artifact("HttpOkAlias"));
    assert!(!checker.comptime_rhs_is_non_liftable_artifact("Config"));
    assert!(!checker.comptime_rhs_is_non_liftable_artifact("Computed"));
    assert!(!checker.comptime_rhs_is_non_liftable_artifact("ok"));
}

#[test]
fn comptime_rhs_evaluation_check_is_shallow_and_group_unwrapped() {
    for source in [
        "Value = make()\n",
        "Value = base + 1\n",
        "Value = -base\n",
        "Value = user.name\n",
        "Value = read(path)?^\n",
        "Value = result ?>\n  @Ok => 1\n",
        "Value =\n  temp = base\n  temp\n",
        "Value = (item) => item\n",
        "Value = (make())\n",
    ] {
        let value = binding_value(source);
        assert!(
            comptime_rhs_needs_evaluation(&value),
            "expected evaluation trigger for {source:?}"
        );
    }

    for source in [
        "Value = 1\n",
        "Value = @Ok\n",
        "Value = runtimeValue\n",
        "Value = User\n",
        "Value = { name: Text }\n",
        "Value = @{@Red, @Green}\n",
        "Value = [1, 2]\n",
        "Value = (Int, Text)\n",
        "Value = Text -> Text\n",
        "Value = Text?\n",
        "Value = Array[Int]\n",
        "Value = (User)\n",
    ] {
        let value = binding_value(source);
        assert!(
            !comptime_rhs_needs_evaluation(&value),
            "did not expect evaluation trigger for {source:?}"
        );
    }
}

#[test]
fn comptime_rhs_evaluation_diagnostic_is_suppressed_after_child_diagnostic() {
    let output = parse_module("Value = Missing + 1\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::EVALUATION_UNSUPPORTED),
        0
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::UNKNOWN_NAME),
        1
    );
}

#[test]
fn comptime_keysof_record_reifies_sorted_literal_union() {
    let output = parse_module("User = { name: Text, email: Text }\nUserKey = keysOf(User)\n");
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("UserKey"),
        Some(&Type::Variant(Row {
            entries: vec![literal_string("\"email\""), literal_string("\"name\"")],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_keysof_non_record_reports_reflection_mismatch() {
    let output = parse_module("Key = keysOf(Int)\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(
            &check.diagnostics,
            codes::comptime::REFLECTION_TYPE_MISMATCH
        ),
        1
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::EVALUATION_UNSUPPORTED),
        0
    );
}

#[test]
fn comptime_keysof_non_concrete_subject_defers_without_diagnostic() {
    let output = parse_module("Key = keysOf(r)\n");
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(definitions.get("Key"), Some(&Type::Deferred));

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_function_application_reifies_sorted_literal_union() {
    let output = parse_module(
        "User = { name: Text, email: Text }\nkeyUnion = (r) => keysOf(r)\nKeys = keyUnion(User)\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("Keys"),
        Some(&Type::Variant(Row {
            entries: vec![literal_string("\"email\""), literal_string("\"name\"")],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_function_application_non_concrete_argument_defers_without_diagnostic() {
    let output = parse_module("keyUnion = (r) => keysOf(r)\nKeys = keyUnion(t)\n");
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(definitions.get("Keys"), Some(&Type::Deferred));

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_function_application_reports_recursion_cycle() {
    let output = parse_module("loop = (r) => loop(r)\nUser = { name: Text }\nKeys = loop(User)\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::EVALUATION_CYCLE),
        1
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::EVALUATION_UNSUPPORTED),
        0
    );
}

#[test]
fn allows_constructor_guarded_recursive_types() {
    let output = parse_module("Tree = { value: Int, children: Tree }\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn reports_unknown_uppercase_type_names() {
    let output = parse_module("value : Missing = value\n");
    let check = check_module(&output.module);

    assert_eq!(check.diagnostics.len(), 1);
    assert_eq!(
        check.diagnostics[0].code.as_deref(),
        Some("type.unknown-name")
    );
}

#[test]
fn annotation_lowerer_lowers_declaration_annotations() {
    let output = parse_module("value : Missing? = name\n");
    let declarations = collect_declarations(&output.module);
    let lowerer = AnnotationLowerer::new(&output.module);
    let declared = lowerer
        .lower_declaration(&output.module, &declarations[0])
        .expect("declared annotation");

    assert_eq!(declared.name, "value");
    assert_eq!(declared.ty, nullable(named("Missing")));
    assert_eq!(declared.diagnostics.len(), 1);
    assert_eq!(
        declared.diagnostics[0].code.as_deref(),
        Some(codes::ty::UNKNOWN_NAME)
    );
}

#[test]
fn lowers_function_application_and_nullable_annotations() {
    let output = parse_module("mapper : (Array[a], a -> b) -> Array[b]\nvalue : Text? = name\n");

    let mapper = lower_annotation(&output.module, annotation(&output.module, "mapper"));
    let value = lower_annotation(&output.module, annotation(&output.module, "value"));

    assert_eq!(
        mapper.ty,
        function(
            vec![
                apply(named("Array"), vec![variable("a")]),
                function(vec![variable("a")], variable("b")),
            ],
            apply(named("Array"), vec![variable("b")]),
        )
    );
    assert!(mapper.diagnostics.is_empty());
    assert_eq!(value.ty, nullable(named("Text")));
    assert!(value.diagnostics.is_empty());
}

#[test]
fn lowers_normalized_rows_and_closed_transforms() {
    let output = parse_module(
        "FileError = @{@Io}\n\
             user : { name: Text, email: Text?, phone?: Text, .. } = current\n\
             error : @{@ParseError(Text), @NotFound} = value\n\
             transformed_user : { name: Text, -password } = current\n\
             transformed_error : @{@ParseError(Text), ..FileError} = value\n",
    );

    let user = lower_annotation(&output.module, annotation(&output.module, "user"));
    let error = lower_annotation(&output.module, annotation(&output.module, "error"));
    let transformed_user = lower_annotation(
        &output.module,
        annotation(&output.module, "transformed_user"),
    );
    let transformed_error = lower_annotation(
        &output.module,
        annotation(&output.module, "transformed_error"),
    );

    assert_eq!(
        user.ty,
        Type::Record(Row {
            entries: vec![
                RowEntry::Field {
                    name: "name".to_owned(),
                    ty: named("Text"),
                    optional: false,
                },
                RowEntry::Field {
                    name: "email".to_owned(),
                    ty: nullable(named("Text")),
                    optional: false,
                },
                RowEntry::Field {
                    name: "phone".to_owned(),
                    ty: named("Text"),
                    optional: true,
                },
            ],
            tail: RowTail::Open,
        })
    );
    assert!(user.diagnostics.is_empty());

    assert_eq!(
        error.ty,
        Type::Variant(Row {
            entries: vec![
                RowEntry::Tag {
                    name: "ParseError".to_owned(),
                    payload: vec![named("Text")],
                },
                RowEntry::Tag {
                    name: "NotFound".to_owned(),
                    payload: Vec::new(),
                },
            ],
            tail: RowTail::Closed,
        })
    );
    assert!(error.diagnostics.is_empty());
    assert_eq!(transformed_user.ty, Type::Deferred);
    assert_eq!(
        transformed_user
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect::<Vec<_>>(),
        vec![codes::ty::DELETE_ABSENT_FIELD]
    );
    assert_eq!(
        transformed_error.ty,
        Type::Variant(Row {
            entries: vec![
                RowEntry::Tag {
                    name: "ParseError".to_owned(),
                    payload: vec![named("Text")],
                },
                RowEntry::Tag {
                    name: "Io".to_owned(),
                    payload: Vec::new(),
                },
            ],
            tail: RowTail::Closed,
        })
    );
    assert!(transformed_error.diagnostics.is_empty());
}

#[test]
fn lowers_literal_variant_entries() {
    let output = parse_module(
        "status : @{\"waiting\", \"running\"} = value\n\
         code : @{0, 1, 2} = value\n",
    );

    let status = lower_annotation(&output.module, annotation(&output.module, "status"));
    let code = lower_annotation(&output.module, annotation(&output.module, "code"));

    assert_eq!(
        status.ty,
        Type::Variant(Row {
            entries: vec![literal_string("\"waiting\""), literal_string("\"running\"")],
            tail: RowTail::Closed,
        })
    );
    assert!(status.diagnostics.is_empty());
    assert_eq!(
        code.ty,
        Type::Variant(Row {
            entries: vec![
                literal_number("0"),
                literal_number("1"),
                literal_number("2"),
            ],
            tail: RowTail::Closed,
        })
    );
    assert!(code.diagnostics.is_empty());
}

#[test]
fn lowers_open_row_extension_and_update_transforms() {
    let output = parse_module(
        "OpenBase = { host: Text, .. }\n\
         OpenColor = @{@Red, ..}\n\
         from_var_add : { ..r, timeout: Int } = value\n\
         from_var_update : { ..r, x :: Float } = value\n\
         from_marker_update : { x: Int, .., y :: Text } = value\n\
         from_open_alias : { ..OpenBase, timeout: Int } = value\n\
         variant_from_var : @{ ..r, @Extra } = value\n\
         variant_from_open_alias : @{ ..OpenColor, @Extra } = value\n\
         deferred_delete : { ..r, -x } = value\n",
    );

    let from_var_add = lower_annotation(&output.module, annotation(&output.module, "from_var_add"));
    let from_var_update = lower_annotation(
        &output.module,
        annotation(&output.module, "from_var_update"),
    );
    let from_marker_update = lower_annotation(
        &output.module,
        annotation(&output.module, "from_marker_update"),
    );
    let from_open_alias = lower_annotation(
        &output.module,
        annotation(&output.module, "from_open_alias"),
    );
    let variant_from_var = lower_annotation(
        &output.module,
        annotation(&output.module, "variant_from_var"),
    );
    let variant_from_open_alias = lower_annotation(
        &output.module,
        annotation(&output.module, "variant_from_open_alias"),
    );
    let deferred_delete = lower_annotation(
        &output.module,
        annotation(&output.module, "deferred_delete"),
    );

    assert_eq!(
        from_var_add.ty,
        Type::Record(Row {
            entries: vec![RowEntry::Field {
                name: "timeout".to_owned(),
                ty: named("Int"),
                optional: false,
            }],
            tail: RowTail::Open,
        })
    );
    assert!(from_var_add.diagnostics.is_empty());

    assert_eq!(
        from_var_update.ty,
        Type::Record(Row {
            entries: vec![RowEntry::Field {
                name: "x".to_owned(),
                ty: named("Float"),
                optional: false,
            }],
            tail: RowTail::Open,
        })
    );
    assert!(from_var_update.diagnostics.is_empty());

    assert_eq!(
        from_marker_update.ty,
        Type::Record(Row {
            entries: vec![
                RowEntry::Field {
                    name: "x".to_owned(),
                    ty: named("Int"),
                    optional: false,
                },
                RowEntry::Field {
                    name: "y".to_owned(),
                    ty: named("Text"),
                    optional: false,
                },
            ],
            tail: RowTail::Open,
        })
    );
    assert!(from_marker_update.diagnostics.is_empty());

    assert_eq!(
        from_open_alias.ty,
        Type::Record(Row {
            entries: vec![
                RowEntry::Field {
                    name: "host".to_owned(),
                    ty: named("Text"),
                    optional: false,
                },
                RowEntry::Field {
                    name: "timeout".to_owned(),
                    ty: named("Int"),
                    optional: false,
                },
            ],
            tail: RowTail::Open,
        })
    );
    assert!(from_open_alias.diagnostics.is_empty());

    assert_eq!(
        variant_from_var.ty,
        Type::Variant(Row {
            entries: vec![RowEntry::Tag {
                name: "Extra".to_owned(),
                payload: Vec::new(),
            }],
            tail: RowTail::Open,
        })
    );
    assert!(variant_from_var.diagnostics.is_empty());

    assert_eq!(
        variant_from_open_alias.ty,
        Type::Variant(Row {
            entries: vec![
                RowEntry::Tag {
                    name: "Red".to_owned(),
                    payload: Vec::new(),
                },
                RowEntry::Tag {
                    name: "Extra".to_owned(),
                    payload: Vec::new(),
                },
            ],
            tail: RowTail::Open,
        })
    );
    assert!(variant_from_open_alias.diagnostics.is_empty());
    assert_eq!(deferred_delete.ty, Type::Deferred);
    assert!(deferred_delete.diagnostics.is_empty());
}

#[test]
fn type_definitions_compute_closed_transform_aliases() {
    let output = parse_module(
        "Base = { x: Int, old: Text }\n\
         Renamed = { ..Base, old -> name }\n\
         Color = @{@Red, @Green, @Blue}\n\
         RedGreen = @{ ..Color, -@Blue }\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("Renamed"),
        Some(&Type::Record(Row {
            entries: vec![
                RowEntry::Field {
                    name: "x".to_owned(),
                    ty: named("Int"),
                    optional: false,
                },
                RowEntry::Field {
                    name: "name".to_owned(),
                    ty: named("Text"),
                    optional: false,
                },
            ],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        definitions.get("RedGreen"),
        Some(&Type::Variant(Row {
            entries: vec![
                RowEntry::Tag {
                    name: "Red".to_owned(),
                    payload: Vec::new(),
                },
                RowEntry::Tag {
                    name: "Green".to_owned(),
                    payload: Vec::new(),
                },
            ],
            tail: RowTail::Closed,
        }))
    );
}

#[test]
fn deferred_rows_still_report_nested_annotation_diagnostics() {
    let output = parse_module("value : @{..Text, io(Missing)} = value\n");
    let lowering = lower_annotation(&output.module, annotation(&output.module, "value"));

    assert_eq!(lowering.ty, Type::Deferred);
    assert_eq!(
        lowering
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.code.as_deref())
            .collect::<Vec<_>>(),
        vec![codes::ty::LOWERCASE_VARIANT_TAG, codes::ty::UNKNOWN_NAME]
    );
}

#[test]
fn literal_bindings_accept_matching_scalar_annotations() {
    for source in [
        "value : Text = \"hi\"\n",
        "value : Int = 42\n",
        "value : Float = 42\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn literal_bindings_report_definitive_scalar_mismatches() {
    for source in [
        "value : Int = \"hi\"\n",
        "value : Text = 42\n",
        "value : Text\nvalue = 42\n",
        "value : Int = (\"hi\")\n",
        "value : Bool = \"hi\"\n",
        "value : Nil = 42\n",
        "value : Unit = \"hi\"\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    }
}

#[test]
fn literal_binding_mismatch_defers_non_literals_and_non_scalar_annotations() {
    for source in [
        "value : Float\nvalue = 42\n",
        "value : { name: Text } = \"hi\"\n",
        "value : Missing = \"hi\"\n",
        "value : Missing\nvalue = \"hi\"\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn separate_signature_binding_mismatch_reuses_declared_annotation_lookup() {
    let output = parse_module("value : Text\nvalue = 42\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn inferred_identifier_values_are_checked_against_expected_types() {
    for source in [
        "other = 42\nvalue : Text = other\n",
        "other = \"hi\"\nvalue : Int = other\n",
        "other = (1, \"a\")\nvalue : (Text, Text) = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn inferred_identifier_values_accept_compatible_types() {
    for source in [
        "other = 42\nvalue : Int = other\n",
        "other = (1, \"a\")\nvalue : (Int, Text) = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn int_and_float_identifier_values_are_not_interchangeable() {
    for source in [
        "other = 42\nvalue : Float = other\n",
        "other : Int = 1\nvalue : Float = other\n",
        "other : Float = 1\nvalue : Int = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn lambda_application_results_are_inferred_for_identifier_values() {
    let mismatch = parse_module("f = (x) => x\nresult = f(\"hi\")\nvalue : Int = result\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let accepted = parse_module("f = (x) => x\nresult = f(\"hi\")\nvalue : Text = result\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "lambda application result unexpectedly produced type.mismatch"
    );
}

#[test]
fn lambda_application_results_are_instantiated_per_use() {
    let output =
        parse_module("id = (x) => x\na = id(1)\nb = id(\"hi\")\nuseA : Int = a\nuseB : Text = b\n");
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "generic top-level lambda reused stale inference state: {:#?}",
        check.diagnostics
    );
}

#[test]
fn lambda_application_tuple_results_recurse_through_inferred_types() {
    let output = parse_module("g = (x) => (x, x)\nr = g(1)\nvalue : (Int, Text) = r\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn annotated_lambdas_are_checked_against_function_annotations() {
    for source in [
        "f : (Int) -> Int = (x: Int) => x\n",
        "f : (Int) -> Int = (x) => x\n",
        "f : (Int) -> Text = (x) => \"hi\"\n",
        "f : (Int) -> Int = (x) : Int => x\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn contextual_lambda_checking_reports_body_param_and_return_mismatches() {
    for source in [
        "f : (Int) -> Text = (x: Int) => x\n",
        "f : (Int) -> Text = (x) => x\n",
        "f : (Int) -> Int = (x: Text) => 1\n",
        "f : (Int) -> Text = (x) : Int => x\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn function_identifier_values_are_checked_against_function_annotations() {
    let output = parse_module("g = (x: Int) => x\nh : (Int) -> Text = g\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn function_parameters_are_compared_contravariantly() {
    let parameter_mismatch = parse_module("f : (Text) -> Int = (x: Int) => x\n");
    let parameter_mismatch_check = check_module(&parameter_mismatch.module);
    assert_eq!(
        matching_codes(&parameter_mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let nullable_parameter = parse_module("f : (Int) -> Int = (x: Int?) => 1\n");
    let nullable_parameter_check = check_module(&nullable_parameter.module);
    assert!(
        !has_diagnostic_code(&nullable_parameter_check.diagnostics, codes::ty::MISMATCH),
        "wider nullable parameter unexpectedly produced type.mismatch"
    );
}

#[test]
fn function_comparison_reports_arity_mismatches() {
    for source in [
        "f : (Int, Int) -> Int = (x: Int) => x\n",
        "g = (x: Int) => x\nh : (Int, Int) -> Int = g\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one function arity mismatch"
        );
    }
}

#[test]
fn direct_application_under_annotation_is_checked() {
    let mismatch = parse_module("f = (x) => x\nvalue : Int = f(\"hi\")\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let accepted = parse_module("f = (x) => x\nvalue : Text = f(\"hi\")\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "direct application unexpectedly produced type.mismatch"
    );

    let tuple = parse_module("g = (x) => (x, x)\nvalue : (Int, Text) = g(1)\n");
    let tuple_check = check_module(&tuple.module);
    assert_eq!(
        matching_codes(&tuple_check.diagnostics, codes::ty::MISMATCH),
        1
    );
}

#[test]
fn synthesized_application_checks_do_not_duplicate_existing_paths() {
    for source in ["value : Text = 42\n", "other = 42\nvalue : Text = other\n"] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce exactly one type.mismatch"
        );
    }
}

#[test]
fn fresh_literals_check_against_literal_unions_by_membership() {
    let accepted = parse_module(
        "status : @{\"waiting\", \"running\"} = \"waiting\"\n\
         code : @{0, 1, 2} = 1\n",
    );
    let accepted_check = check_module(&accepted.module);
    assert!(accepted_check.diagnostics.is_empty());

    let rejected = parse_module(
        "status : @{\"waiting\", \"running\"} = \"stopped\"\n\
         code : @{0, 1, 2} = 3\n",
    );
    let rejected_check = check_module(&rejected.module);
    assert_eq!(
        matching_codes(&rejected_check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        2
    );
}

#[test]
fn bare_literals_still_infer_base_types() {
    let output = parse_module("x = 200\ns = \"hi\"\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(checker.infer_top_level_value("x"), Some(named("Int")));
    assert_eq!(checker.infer_top_level_value("s"), Some(named("Text")));
}

#[test]
fn direct_application_under_annotation_defers_non_concrete_synthesis() {
    let output = parse_module("h = (x) => missing(x)\nvalue : Text = h(1)\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "unsolved direct application unexpectedly produced type.mismatch"
    );
}

#[test]
fn block_bodied_values_are_checked_against_annotations() {
    for source in [
        "value : (Int, Text) =\n  pair = (1, \"a\")\n  pair\n",
        "value : Int =\n  x = 1\n  x\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "value : (Int, Int) =\n  pair = (1, \"a\")\n  pair\n",
        "value : Text =\n  x = 1\n  x\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn contextual_blocks_check_final_expressions() {
    for source in [
        "value : (Int) -> Text =\n  (x) => x\n",
        "value : { name: Text } =\n  { name: 1 }\n",
        "value : Array[Text] =\n  [1]\n",
        "identity = (x) => x\nvalue : Int =\n  identity(\"hi\")\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn contextual_blocks_do_not_duplicate_prefix_diagnostics() {
    let output = parse_module("value : Text =\n  first : Text = 1\n  first\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn contextual_block_prefix_bindings_see_seeded_lambda_params() {
    let output = parse_module("f : (Int) -> Text = (x) =>\n  y : Bool = x\n  y\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 2);
}

#[test]
fn contextual_matches_check_arm_bodies_against_expected_type() {
    let output = parse_module("value : Text =\n  result ?>\n    @Ok(_) => 1\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn contextual_matches_check_block_arm_bodies_against_expected_type() {
    let output =
        parse_module("value : Text =\n  result ?>\n    @Ok(_) =>\n      local = 1\n      local\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn contextual_matches_keep_pattern_binders_unknown() {
    let output =
        parse_module("item : Text = \"hi\"\nvalue : Bool =\n  result ?>\n    @Ok(item) => item\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "contextual match arm borrowed a top-level type for a pattern binder"
    );
}

#[test]
fn match_guards_are_checked_as_bool() {
    for source in [
        "value : Text =\n  result ?>\n    @Ok(_), 1 < 2 => \"ok\"\n",
        "flag : Bool = True\nvalue : Text =\n  result ?>\n    @Ok(_), flag => \"ok\"\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "value : Text =\n  result ?>\n    @Ok(_), 1 => \"ok\"\n",
        "flag : Text = \"no\"\nvalue : Text =\n  result ?>\n    @Ok(_), flag => \"ok\"\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one guard type mismatch"
        );
    }
}

#[test]
fn match_guard_pattern_binders_stay_unknown() {
    let output = parse_module(
        "item : Text = \"hi\"\nvalue : Text =\n  result ?>\n    @Ok(item), item => \"ok\"\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "match guard borrowed a top-level type for a pattern binder"
    );
}

#[test]
fn variant_match_payload_binders_use_subject_types() {
    let body = parse_module(
        "source : @{@Ok(Text), @Err(Text)} = result\nvalue : Bool = source ?>\n  @Ok(item) => item\n  @Err(_) => False\n",
    );
    let body_check = check_module(&body.module);
    assert_eq!(
        matching_codes(&body_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let guard = parse_module(
        "source : @{@Ok(Text), @Err(Text)} = result\nvalue : Text = source ?>\n  @Ok(item), item => \"ok\"\n  @Err(_) => \"err\"\n",
    );
    let guard_check = check_module(&guard.module);
    assert_eq!(
        matching_codes(&guard_check.diagnostics, codes::ty::MISMATCH),
        1
    );
}

#[test]
fn variant_match_payload_types_feed_result_inference() {
    let output = parse_module(
        "source : @{@Ok(Text), @Err(Text)} = result\nmatched = source ?>\n  @Ok(item) => item\n  @Err(error) => error\nvalue : Int = matched\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn record_match_pattern_binders_use_subject_field_and_rest_types() {
    let output = parse_module(
        "source : { x: Int, y: Text, z: Bool } = value\n\
         picked = source ?>\n  { x, ..rest } => x\n\
         remaining = source ?>\n  { x, ..rest } => rest.y\n\
         matched_field_removed = source ?>\n  { x, ..rest } => rest.x\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(checker.infer_top_level_value("picked"), Some(named("Int")));
    assert_eq!(
        checker.infer_top_level_value("remaining"),
        Some(named("Text"))
    );
    assert_eq!(checker.infer_top_level_value("matched_field_removed"), None);
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn nested_record_match_pattern_binders_use_subject_field_types() {
    let output = parse_module(
        "source : { outer: { inner: Bool }, other: Int } = value\n\
         matched = source ?>\n  { outer: { inner } } => inner\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("matched"),
        Some(named("Bool"))
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn open_record_match_rest_binder_stays_unconstrained() {
    let output = parse_module(
        "source : { x: Int, y: Text, .. } = value\n\
         picked = source ?>\n  { x, ..rest } => x\n\
         remaining = source ?>\n  { x, ..rest } => rest.y\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(checker.infer_top_level_value("picked"), Some(named("Int")));
    assert_eq!(checker.infer_top_level_value("remaining"), None);
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn match_results_are_inferred_for_identifier_values() {
    let mismatch =
        parse_module("result = source ?>\n  @Ok(_) => 1\n  @Err(_) => 2\nvalue : Text = result\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let accepted =
        parse_module("result = source ?>\n  @Ok(_) => 1\n  @Err(_) => 2\nvalue : Int = result\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible inferred match result unexpectedly produced type.mismatch"
    );
}

#[test]
fn match_results_merge_closed_variant_rows() {
    let output = parse_module("classify = (n) =>\n  n ?>\n    0 => @Zero\n    _ => @Pos\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let scheme = checker
        .infer_top_level_scheme("classify")
        .expect("inferred classify scheme");
    let Type::Function { result, .. } = &scheme.ty else {
        panic!("classify should infer a function type");
    };
    let Type::Variant(row) = result.as_ref() else {
        panic!("classify should infer a variant result");
    };
    let tags: HashSet<_> = row
        .entries
        .iter()
        .filter_map(|entry| match entry {
            RowEntry::Tag { name, .. } => Some(name.as_str()),
            RowEntry::Field { .. } | RowEntry::Literal { .. } => None,
        })
        .collect();

    assert_eq!(tags, HashSet::from(["Zero", "Pos"]));
    assert_eq!(row.tail, RowTail::Closed);
}

#[test]
fn match_results_merge_open_variant_rows_when_an_arm_is_open() {
    let output = parse_module(
        "open : @{@Zero, ..} = value\nclassify = (n) =>\n  n ?>\n    0 => open\n    _ => @Pos\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let scheme = checker
        .infer_top_level_scheme("classify")
        .expect("inferred classify scheme");
    let Type::Function { result, .. } = &scheme.ty else {
        panic!("classify should infer a function type");
    };
    let Type::Variant(row) = result.as_ref() else {
        panic!("classify should infer a variant result");
    };
    let tags: HashSet<_> = row
        .entries
        .iter()
        .filter_map(|entry| match entry {
            RowEntry::Tag { name, .. } => Some(name.as_str()),
            RowEntry::Field { .. } | RowEntry::Literal { .. } => None,
        })
        .collect();

    assert_eq!(tags, HashSet::from(["Zero", "Pos"]));
    assert_eq!(row.tail, RowTail::Open);
}

#[test]
fn tag_literals_and_constructors_infer_closed_variant_rows() {
    let output = parse_module("zero = @Zero\nok = @Ok(1)\ntruth = True\nnil = Nil\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    for (binding, tag) in [("zero", "Zero"), ("ok", "Ok")] {
        let scheme = checker
            .infer_top_level_scheme(binding)
            .unwrap_or_else(|| panic!("inferred {binding} scheme"));
        let Type::Variant(row) = &scheme.ty else {
            panic!("{binding} should infer a variant type");
        };
        assert_eq!(row.tail, RowTail::Closed);
        assert!(scheme.row_vars.is_empty());
        assert!(matches!(
            row.entries.as_slice(),
            [RowEntry::Tag { name, .. }] if name == tag
        ));
    }

    assert_eq!(checker.infer_top_level_value("truth"), Some(named("Bool")));
    assert_eq!(checker.infer_top_level_value("nil"), Some(named("Nil")));
}

#[test]
fn bare_uppercase_values_do_not_infer_tags() {
    let output = parse_module("Answer = 42\nresolved = Answer\nmissing = Missing\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("resolved"),
        Some(named("Int"))
    );
    assert_eq!(
        checker
            .infer_top_level_scheme("missing")
            .map(|scheme| scheme.ty),
        Some(Type::Deferred)
    );
}

#[test]
fn merged_variant_rows_widen_into_open_and_closed_annotations() {
    for source in [
        "direction = n ?>\n  0 => @Zero\n  _ => @Pos\nvalue : @{@Zero, @Pos} = direction\n",
        "direction = n ?>\n  0 => @Zero\n  _ => @Pos\nvalue : @{@Zero, @Pos, ..} = direction\n",
    ] {
        let accepted = parse_module(source);
        let accepted_check = check_module(&accepted.module);
        assert!(
            accepted_check.diagnostics.is_empty(),
            "{source} unexpectedly produced diagnostics: {:?}",
            accepted_check.diagnostics
        );
    }

    let rejected =
        parse_module("direction = n ?>\n  0 => @Zero\n  _ => @Pos\nvalue : @{@Zero} = direction\n");
    let rejected_check = check_module(&rejected.module);
    assert_eq!(
        matching_codes(&rejected_check.diagnostics, codes::ty::MISMATCH),
        1
    );
}

#[test]
fn variant_match_exhaustiveness_uses_subject_rows() {
    let closed_complete =
        parse_module("source : @{@A, @B} = value\nresult = source ?>\n  @A => 1\n  @B => 2\n");
    assert!(!has_diagnostic_code(
        &check_module(&closed_complete.module).diagnostics,
        codes::ty::NON_EXHAUSTIVE_MATCH
    ));

    let closed_missing =
        parse_module("source : @{@A, @B} = value\nresult = source ?>\n  @A => 1\n");
    assert_eq!(
        matching_codes(
            &check_module(&closed_missing.module).diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH,
        ),
        1
    );

    let open_missing_default =
        parse_module("source : @{@A, ..} = value\nresult = source ?>\n  @A => 1\n");
    assert_eq!(
        matching_codes(
            &check_module(&open_missing_default.module).diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH,
        ),
        1
    );

    for source in [
        "source = A\nresult = source ?>\n  _ => 1\n",
        "source = A\nresult = source ?>\n  other => 1\n",
    ] {
        let output = parse_module(source);
        assert!(!has_diagnostic_code(
            &check_module(&output.module).diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH
        ));
    }
}

#[test]
fn literal_union_match_exhaustiveness_uses_subject_members() {
    let closed_complete = parse_module(concat!(
        "Status = @{\"waiting\", \"running\", \"done\"}\n",
        "source : Status = \"waiting\"\n",
        "result = source ?>\n",
        "  \"waiting\" => 1\n",
        "  \"running\" => 2\n",
        "  \"done\" => 3\n",
    ));
    let closed_complete_check = check_module(&closed_complete.module);
    assert!(
        !has_diagnostic_code(
            &closed_complete_check.diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH
        ),
        "complete literal-union match produced diagnostics: {:?}",
        closed_complete_check.diagnostics
    );

    let closed_with_default = parse_module(concat!(
        "Status = @{\"waiting\", \"running\", \"done\"}\n",
        "source : Status = \"waiting\"\n",
        "result = source ?>\n",
        "  \"waiting\" => 1\n",
        "  _ => 2\n",
    ));
    let closed_with_default_check = check_module(&closed_with_default.module);
    assert!(
        !has_diagnostic_code(
            &closed_with_default_check.diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH
        ),
        "defaulted literal-union match produced diagnostics: {:?}",
        closed_with_default_check.diagnostics
    );

    let closed_missing = parse_module(concat!(
        "Status = @{\"waiting\", \"running\", \"done\"}\n",
        "source : Status = \"waiting\"\n",
        "result = source ?>\n",
        "  \"waiting\" => 1\n",
        "  \"running\" => 2\n",
    ));
    assert_eq!(
        matching_codes(
            &check_module(&closed_missing.module).diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH,
        ),
        1
    );

    let open_missing_default = parse_module(concat!(
        "Status = @{\"waiting\", ..}\n",
        "source : Status = \"waiting\"\n",
        "result = source ?>\n",
        "  \"waiting\" => 1\n",
    ));
    assert_eq!(
        matching_codes(
            &check_module(&open_missing_default.module).diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH,
        ),
        1
    );
}

#[test]
fn literal_union_match_reports_unreachable_literal_arms() {
    let output = parse_module(concat!(
        "Status = @{\"waiting\", \"running\"}\n",
        "source : Status = \"waiting\"\n",
        "result = source ?>\n",
        "  \"waiting\" => 1\n",
        "  \"stopped\" => 2\n",
        "  \"running\" => 3\n",
    ));
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::UNREACHABLE_MATCH_ARM),
        1,
        "unreachable literal-arm diagnostics: {:?}",
        check.diagnostics
    );
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::NON_EXHAUSTIVE_MATCH
    ));
}

#[test]
fn match_result_inference_handles_block_arm_bodies() {
    let output = parse_module(
        "result = source ?>\n  @Ok(_) =>\n    local = 1\n    local\nvalue : Text = result\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn match_result_inference_defers_mixed_arm_types() {
    let output = parse_module(
        "result = source ?>\n  @Ok(_) => 1\n  @Err(_) => \"no\"\nvalue : Text = result\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "mixed match arm types should defer instead of reporting"
    );
}

#[test]
fn match_result_inference_keeps_pattern_binders_unknown() {
    let output = parse_module(
        "item : Text = \"hi\"\nresult = source ?>\n  @Ok(item) => item\nvalue : Bool = result\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "inferred match result borrowed a top-level type for a pattern binder"
    );
}

#[test]
fn unannotated_block_values_feed_identifier_checks() {
    let output = parse_module("data =\n  x = 1\n  (x, x)\nvalue : (Int, Text) = data\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn block_inference_defers_unsolved_values() {
    for source in [
        "value : Text =\n  x = 1\n",
        "value : Text =\n  missing(1)\n",
        "value : Text =\n  x = missing\n  x + 1\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn block_inference_prefers_local_bindings_over_top_level_bindings() {
    let output = parse_module("name = 1\nvalue : Text =\n  name = \"hi\"\n  name\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "block local binding did not shadow top-level value during inference"
    );
}

#[test]
fn polymorphic_local_block_result_reports_a_concrete_mismatch() {
    let output = parse_module("result : Int =\n  id = (x) => x\n  helper = id(1)\n  id(\"hi\")\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn polymorphic_local_block_result_accepts_each_instantiation() {
    let output = parse_module("result : Text =\n  id = (x) => x\n  helper = id(1)\n  id(\"hi\")\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn local_generalization_preserves_enclosing_lambda_metas() {
    let output = parse_module("f = (x) =>\n  get = () => x\n  get()\nresult : Int = f(\"hi\")\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn array_literals_are_checked_against_annotations() {
    let accepted = parse_module("value : Array[Int] = [1, 2, 3]\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible array literal unexpectedly produced type.mismatch"
    );

    let mismatch = parse_module("value : Array[Text] = [1, 2, 3]\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        3
    );
}

#[test]
fn inferred_array_identifier_values_are_checked_against_annotations() {
    let output = parse_module("nums = [1, 2]\nvalue : Array[Text] = nums\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn array_element_types_reuse_structural_type_comparison() {
    let accepted = parse_module("value : Array[(Int, Text)] = [(1, \"a\")]\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible nested array literal unexpectedly produced type.mismatch"
    );

    let mismatch = parse_module("value : Array[(Int, Int)] = [(1, \"a\")]\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );
}

#[test]
fn array_literals_report_per_element_mismatches() {
    let output = parse_module("value : Array[Text] = [\"a\", 2, \"b\"]\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn array_inference_defers_empty_literals() {
    let output = parse_module("value : Array[Int] = []\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "empty array unexpectedly produced type.mismatch"
    );
}

#[test]
fn set_literals_are_checked_against_annotations() {
    let accepted = parse_module("value : Set[Int] = @{1, 2, 3}\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible set literal unexpectedly produced type.mismatch"
    );

    let mismatch = parse_module("value : Set[Text] = @{1, 2, 3}\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        3
    );
}

#[test]
fn inferred_set_identifier_values_are_checked_against_annotations() {
    let output = parse_module("nums = @{1, 2}\nvalue : Set[Text] = nums\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn set_literals_report_per_element_mismatches() {
    let output = parse_module("value : Set[Text] = @{\"a\", 2, \"b\"}\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn set_inference_defers_empty_tag_and_spread_literals() {
    for source in [
        "value : Set[Int] = @{}\n",
        "value : Set[Int] = @{@Red, @Green}\n",
        "other = @{2}\nvalue : Set[Int] = @{..other, 1}\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn variant_values_are_checked_against_annotations() {
    for source in [
        "value : @{@Ok(Int), @Err(Text)} = @Ok(1)\n",
        "value : @{@Done} = @Done\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "value : @{@Ok(Text)} = @Ok(1)\n",
        "value : @{@Ok(Text), ..} = @Ok(1)\n",
        "value : @{@Ok(Int)} = @Err(1)\n",
        "value : @{@Ok(Int)} = @Ok(1, 2)\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn inferred_variant_identifier_values_are_checked_against_annotations() {
    for source in [
        "result = @Ok(1)\nvalue : @{@Ok(Int), @Err(Text), ..} = result\n",
        "done = @Done\nvalue : @{@Done, ..} = done\n",
        "result = @Ok(1)\nvalue : @{@Ok(Int), @Err(Text)} = result\n",
        "done = @Done\nvalue : @{@Done, @Other} = done\n",
        "result : @{@Ok(Int)} = @Ok(1)\nvalue : @{@Ok(Int), @Err(Text)} = result\n",
        "done : @{@Done} = @Done\nvalue : @{@Done, @Other} = done\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            check.diagnostics.is_empty(),
            "{source} unexpectedly produced diagnostics: {:?}",
            check.diagnostics
        );
    }

    for source in [
        "result : @{@Ok(Int)} = @Ok(1)\nvalue : @{@Ok(Text), @Err(Text)} = result\n",
        "result : @{@Ok(Int), ..} = @Ok(1)\nvalue : @{@Ok(Text), ..} = result\n",
        "result : @{@Err(Text)} = @Err(\"no\")\nvalue : @{@Ok(Int)} = result\n",
        "result = @Ok(1)\nvalue : @{@Err(Text)} = result\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }

    for source in [
        "result : @{@Ok(Int), ..} = @Ok(1)\nvalue : @{@Ok(Int), @Err(Text)} = result\n",
        "done : @{@Done, ..} = @Done\nvalue : @{@Done} = done\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::OPEN_VARIANT_NOT_ASSIGNABLE),
            1,
            "{source} should produce one type.open-variant-not-assignable"
        );
    }
}

#[test]
fn variant_value_checking_allows_open_row_extra_tags() {
    let output = parse_module("value : @{@Ok(Int), ..error} = @Err(\"x\")\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "open variant row extra tag unexpectedly produced type.mismatch"
    );
}

#[test]
fn lambda_application_inference_defers_unsolved_values() {
    for source in [
        "f = (x) => f(x)\nr = f(1)\nvalue : Text = r\n",
        "f = (x) => x\nx = f\nvalue : Text = x\n",
        "f = (x) => x(x)\nr = f(1)\nvalue : Text = r\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn builtin_operator_results_are_inferred() {
    for source in [
        "value : Float = 42\n",
        "value : Int = 1\n",
        "sum : Int = 1 + 2\n",
        "value : Text = \"a\" + \"b\"\n",
        "value : Bool = 1 == 2\n",
        "value : Bool = 1 < 2\n",
        "left : Bool = True\nright : Bool = False\nvalue : Bool = left && right\n",
        "f = (floatParam : Float) =>\n  mix : Float = floatParam + 1\n  mix\n",
        "f = (intParam : Int) =>\n  sum : Int = intParam + 1\n  sum\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "flo : Float = 1 + 2\n",
        "result = 1 + 2\nvalue : Text = result\n",
        "result = \"a\" + \"b\"\nvalue : Int = result\n",
        "result = 1 < 2\nvalue : Text = result\n",
        "left : Bool = True\nright : Bool = False\nresult = left && right\nvalue : Text = result\n",
        "h = (x) => x + 1\nr = h(1)\nvalue : Text = r\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn numeric_binary_literals_synthesize_int_after_defaulting() {
    let output = parse_module("n = 1 + 2\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(checker.infer_top_level_value("n"), Some(named("Int")));
}

#[test]
fn operator_inference_defers_unknown_operands() {
    for source in [
        "value : Text = missing + 1\n",
        "result = source ?>\n  @Ok(item) => item + 1\nvalue : Text = result\n",
        "value : Text = unknown && missing\n",
        // An unsupported sub-expression stays deferred rather than being
        // constrained into a concrete type by a surrounding operator.
        "value : Text = (missing + 1) + 2\n",
        "value : Text = missing[0] + 1\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn infer_value_synthesizes_literal_record_types() {
    let output = parse_module("other = { id: 1, name: \"Ada\" }\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("other"),
        Some(Type::Record(Row {
            entries: vec![
                RowEntry::Field {
                    name: "id".to_owned(),
                    ty: named("Int"),
                    optional: false,
                },
                RowEntry::Field {
                    name: "name".to_owned(),
                    ty: named("Text"),
                    optional: false,
                },
            ],
            tail: RowTail::Closed,
        }))
    );
}

#[test]
fn infer_value_synthesizes_closed_record_transform_types() {
    let output = parse_module(
        "base = { x: 1, y: \"yes\", old: True }\n\
         added = { ..base, z: 2 }\n\
         replaced = { ..base, y := \"changed\" }\n\
         deleted = { ..base, -y }\n\
         renamed = { ..base, old -> flag }\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("added"),
        Some(Type::Record(Row {
            entries: vec![
                field("x", named("Int")),
                field("y", named("Text")),
                field("old", named("Bool")),
                field("z", named("Int")),
            ],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        checker.infer_top_level_value("replaced"),
        Some(Type::Record(Row {
            entries: vec![
                field("x", named("Int")),
                field("y", named("Text")),
                field("old", named("Bool")),
            ],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        checker.infer_top_level_value("deleted"),
        Some(Type::Record(Row {
            entries: vec![field("x", named("Int")), field("old", named("Bool"))],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        checker.infer_top_level_value("renamed"),
        Some(Type::Record(Row {
            entries: vec![
                field("x", named("Int")),
                field("y", named("Text")),
                field("flag", named("Bool")),
            ],
            tail: RowTail::Closed,
        }))
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn infer_value_synthesizes_disjoint_spread_union() {
    let output = parse_module("a = { x: 1 }\nb = { y: \"ok\" }\nunion = { ..a, ..b }\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("union"),
        Some(Type::Record(Row {
            entries: vec![field("x", named("Int")), field("y", named("Text"))],
            tail: RowTail::Closed,
        }))
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn value_spread_conflict_reports_duplicate_label() {
    let output = parse_module("a = { x: 1 }\nb = { x: 2 }\nconflict = { ..a, ..b }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::DUPLICATE_SPREAD_LABEL),
        1
    );
}

#[test]
fn infer_value_record_transforms_absorb_open_sources() {
    let output = parse_module(
        "source : { x: Int, .. } = current\n\
         added = { ..source, y: 1 }\n\
         updated = { ..source, x := 2 }\n\
         deleted = { ..source, -x }\n\
         from_row_var = (p) => { y: p.x, ..p }\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("added"),
        Some(Type::Record(Row {
            entries: vec![field("x", named("Int")), field("y", named("Int"))],
            tail: RowTail::Open,
        }))
    );
    assert_eq!(
        checker.infer_top_level_value("updated"),
        Some(Type::Record(Row {
            entries: vec![field("x", named("Int"))],
            tail: RowTail::Open,
        }))
    );
    assert_eq!(
        checker
            .infer_top_level_scheme("deleted")
            .map(|scheme| scheme.ty),
        Some(Type::Deferred)
    );

    let row_var_scheme = checker
        .infer_top_level_scheme("from_row_var")
        .expect("inferred from_row_var scheme");
    let Type::Function { result, .. } = &row_var_scheme.ty else {
        panic!("from_row_var should infer a function type");
    };
    let Type::Record(row) = result.as_ref() else {
        panic!("from_row_var should infer a record result");
    };
    let labels: HashSet<_> = row
        .entries
        .iter()
        .map(|entry| row_label(entry).to_owned())
        .collect();
    assert_eq!(labels, HashSet::from(["x".to_owned(), "y".to_owned()]));
    assert_eq!(row.tail, RowTail::Open);
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn infer_value_record_spread_of_non_record_defers_without_diagnostic() {
    let output = parse_module("base = \"not a record\"\nspread = { ..base, x: 1 }\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker
            .infer_top_level_scheme("spread")
            .map(|scheme| scheme.ty),
        Some(Type::Deferred)
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn field_access_infers_an_open_record_parameter_and_result_type() {
    let output = parse_module(
        "getX = (p) => p.x\n\
         good : Int = getX({ x: 1, y: 2 })\n\
         bad : Text = getX({ x: 1, y: 2 })\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn field_access_row_variables_are_freshened_for_each_use() {
    let output = parse_module(
        "getX = (p) => p.x\n\
         number : Int = getX({ x: 1, y: 2 })\n\
         text : Text = getX({ x: \"ok\", name: \"Ada\" })\n",
    );
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn computed_value_index_with_literal_key_infers_concrete_record_field_type() {
    let output = parse_module("user = { name: \"Ada\", age: 36 }\nname = user[\"name\"]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(checker.infer_top_level_value("name"), Some(named("Text")));
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn computed_value_index_with_runtime_key_defers_without_diagnostic() {
    let output = parse_module("user = { name: \"Ada\" }\nkey = \"name\"\nvalue = user[key]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker
            .infer_top_level_scheme("value")
            .map(|scheme| scheme.ty),
        Some(Type::Deferred)
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn computed_value_index_with_non_record_receiver_defers_without_diagnostic() {
    let output = parse_module("text = \"Ada\"\nvalue = text[\"name\"]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker
            .infer_top_level_scheme("value")
            .map(|scheme| scheme.ty),
        Some(Type::Deferred)
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn comptime_pick_unrolls_key_set_to_closed_record_type() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         pick = (o: {..r}, @keys: keysOf(r)[]) => { keys -> k; (k, o[k]) }\n\
         result = pick(user, @{\"name\", \"email\"})\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("result"),
        Some(Type::Record(Row {
            entries: vec![field("name", named("Text")), field("email", named("Text"))],
            tail: RowTail::Closed,
        }))
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn comptime_pick_with_non_concrete_key_set_defers_without_diagnostic() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         keys = @{\"name\", \"email\"}\n\
         pick = (o: {..r}, @keys: keysOf(r)[]) => { keys -> k; (k, o[k]) }\n\
         result = pick(user, keys)\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker
            .infer_top_level_scheme("result")
            .map(|scheme| scheme.ty),
        Some(Type::Deferred)
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn missing_inferred_field_defers_without_a_type_mismatch() {
    let output = parse_module("getX = (p) => p.x\nvalue = getX({ y: 2 })\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));
}

#[test]
fn inferred_field_access_scheme_contains_a_quantified_row_variable() {
    let output = parse_module("getX = (p) => p.x\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let scheme = checker
        .infer_top_level_scheme("getX")
        .expect("inferred getX scheme");

    assert_eq!(scheme.vars.len(), 1);
    assert_eq!(scheme.row_vars.len(), 1);
    let Type::Function { params, result } = &scheme.ty else {
        panic!("getX should infer a function type");
    };
    assert_eq!(params.len(), 1);
    let Type::Record(row) = &params[0] else {
        panic!("getX parameter should infer a record type");
    };
    assert_eq!(row.tail, RowTail::Var(scheme.row_vars[0]));
    assert!(matches!(
        row.entries.as_slice(),
        [RowEntry::Field {
            name,
            ty,
            optional: false,
        }] if name == "x" && ty == result.as_ref()
    ));
}

#[test]
fn local_field_access_preserves_enclosing_row_and_field_variables() {
    let output = parse_module(
        "readX = (p) =>\n  getX = () => p.x\n  getX()\n\
         good : Int = readX({ x: 1, y: 2 })\n\
         bad : Text = readX({ x: 1, y: 2 })\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn inferred_record_identifier_values_report_field_type_mismatches() {
    for source in [
        "other = { id: 1 }\nvalue : { id: Text } = other\n",
        "other = { user: { name: 1 } }\nvalue : { user: { name: Text } } = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn inferred_record_identifier_values_report_missing_fields() {
    let missing = parse_module("other = { id: 1 }\nvalue : { id: Int, name: Text } = other\n");
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::MISSING_FIELD),
        1
    );
}

#[test]
fn inferred_record_identifier_values_accept_compatible_records() {
    for source in [
        "other = { id: 1 }\nvalue : { id: Int } = other\n",
        "other = { id: 1, name: \"Ada\" }\nvalue : { id: Int } = other\n",
        "other = { user: { id: 1, name: \"Ada\" } }\nvalue : { user: { id: Int } } = other\n",
        "other = { id: 1, name: \"Ada\" }\nvalue : { id: Int, .. } = other\n",
        "other = { name: \"Ada\", id: 1 }\nvalue : { id: Int, name: Text } = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
            "{source} unexpectedly produced type.missing-field"
        );
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
            "{source} unexpectedly produced type.unexpected-field"
        );
    }
}

#[test]
fn record_identifier_value_checking_defers_open_actual_types() {
    let output = parse_module("other : { id: Int, .. } = rec\nvalue : { id: Int } = other\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "open actual record unexpectedly produced type.mismatch"
    );
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
        "open actual record unexpectedly produced type.missing-field"
    );
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
        "open actual record unexpectedly produced type.unexpected-field"
    );
}

#[test]
fn annotated_identifier_values_are_checked_against_expected_types() {
    for source in [
        "other : Text = \"hi\"\nvalue : Int = other\n",
        "other : (Int, Text) = (1, \"a\")\nvalue : (Int, Int) = other\n",
        "other : Text? = Nil\nvalue : Text = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH),
            1,
            "{source} should produce one type.mismatch"
        );
    }
}

#[test]
fn annotated_identifier_values_accept_compatible_declared_types() {
    for source in [
        "other : Text = \"hi\"\nvalue : Text = other\n",
        "other : Text = \"hi\"\nvalue : Text? = other\n",
        "other : Nil = Nil\nvalue : Text? = other\n",
        "other : (Int, Text) = (1, \"a\")\nvalue : (Int, Text) = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn annotated_identifier_value_checking_defers_ambiguous_or_unstable_cases() {
    for source in [
        "other : Missing = value\nvalue : Text = other\n",
        "other : Text = \"hi\"\nother : Int = 1\nvalue : Int = other\n",
        "User = { name: Text }\nother : User = { name: \"a\" }\nvalue : { name: Text } = other\n",
        "other = name\nvalue : Int = other\n",
        "other = f(1)\nvalue : Int = other\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn shadowed_identifier_values_defer() {
    let output =
        parse_module("other : Text = \"hi\"\nf = (other : Bool) =>\n  x : Bool = other\n  x\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));
}

#[test]
fn annotated_lambda_parameters_are_checked_in_local_bindings() {
    let output = parse_module("f = (x : Int) =>\n  y : Text = x\n  y\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn annotated_sequential_locals_are_checked_in_source_order() {
    let output = parse_module("f = () =>\n  first : Int = 1\n  second : Text = first\n  second\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn unannotated_local_literals_feed_later_checks() {
    let mismatch = parse_module("f = () =>\n  first = 1\n  second : Text = first\n  second\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let accepted = parse_module("f = () =>\n  first = 1\n  second : Int = first\n  second\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible inferred local unexpectedly produced type.mismatch"
    );
}

#[test]
fn unannotated_local_applications_feed_later_checks() {
    let output = parse_module(
        "identity = (x) => x\nf = () =>\n  local = identity(\"hi\")\n  value : Int = local\n  value\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn annotated_parameters_feed_inferred_local_bindings() {
    let output =
        parse_module("f = (input : Int) =>\n  local = input\n  value : Text = local\n  value\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn inferred_local_types_are_visible_in_nested_scopes() {
    let output = parse_module(
        "f = () =>\n  outer = 1\n  g = () =>\n    value : Text = outer\n    value\n  g\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn adjacent_local_signatures_supply_known_local_types() {
    let output =
        parse_module("f = () =>\n  first : Int\n  first = 1\n  second : Text = first\n  second\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn unknown_lambda_parameters_shadow_top_level_types() {
    let output = parse_module("other : Text = \"hi\"\nf = (other) =>\n  x : Bool = other\n  x\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "unannotated parameter borrowed a same-named top-level type"
    );
}

#[test]
fn unknown_block_bindings_shadow_top_level_types() {
    let output = parse_module(
        "other : Text = \"hi\"\nf = () =>\n  other = missing\n  x : Bool = other\n  x\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "unsolved block binding borrowed a same-named top-level type"
    );
}

#[test]
fn match_pattern_bindings_shadow_top_level_types() {
    let output = parse_module(
        "item : Text = \"hi\"\nf = (result) =>\n  result ?>\n    @Ok(item) =>\n      value : Bool = item\n      value\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "pattern binding borrowed a same-named top-level type"
    );
}

#[test]
fn inferred_pattern_dependent_locals_stay_unknown() {
    let output = parse_module(
        "item : Text = \"hi\"\nf = (result) =>\n  result ?>\n    @Ok(item) =>\n      local = item\n      value : Bool = local\n      value\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "pattern-dependent local borrowed a top-level type during inference"
    );
}

#[test]
fn nearest_annotated_local_type_wins_in_nested_scopes() {
    let output = parse_module(
        "f = (value : Int) =>\n  g = (value : Text) =>\n    result : Int = value\n    result\n  g\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn tuple_values_accept_matching_tuple_annotations() {
    for source in [
        "value : (Int, Text) = (1, \"a\")\n",
        "value : (Int, Float) = (1, 2)\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn tuple_values_report_recursive_element_mismatches() {
    let output = parse_module("value : (Int, Text) = (1, 2)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn tuple_values_report_each_element_mismatch() {
    let output = parse_module("value : (Int, Text) = (\"a\", 2)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 2);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Int`, found a text literal"
    );
    assert_eq!(
        check.diagnostics[1].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn parenthesized_values_are_checked_through_groups() {
    let output = parse_module("value : Int = (\"hi\")\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Int`, found a text literal"
    );
}

#[test]
fn nullable_values_accept_nil_and_matching_inner_values() {
    for source in [
        "value : Text? = \"hi\"\n",
        "value : Text? = Nil\n",
        "value : Int? = Nil\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn nullable_values_report_inner_mismatches() {
    let output = parse_module("value : Int? = \"hi\"\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Int`, found a text literal"
    );
}

#[test]
fn nullable_values_defer_names() {
    let output = parse_module("value : Text? = other\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));
}

#[test]
fn record_values_accept_exact_literal_record_annotations() {
    let output = parse_module("value : { name: Text } = { name: \"x\" }\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn record_values_report_field_value_mismatches() {
    let output = parse_module("value : { name: Text } = { name: 42 }\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn record_values_report_missing_required_fields() {
    let output = parse_module("value : { name: Text, age: Int } = { name: \"x\" }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD),
        1
    );
}

#[test]
fn record_values_report_unexpected_fields_in_closed_records() {
    let output = parse_module("value : { name: Text } = { name: \"x\", extra: 1 }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
        1
    );
}

#[test]
fn open_record_types_allow_extra_value_fields() {
    let output = parse_module("value : { name: Text, .. } = { name: \"x\", extra: 1 }\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn optional_record_fields_may_be_absent_or_checked_when_present() {
    let output = parse_module("value : { name: Text, phone?: Text } = { name: \"x\" }\n");
    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());

    let output = parse_module("value : { phone?: Text } = { phone: 42 }\n");
    let check = check_module(&output.module);
    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn nullable_record_fields_accept_nil() {
    let output = parse_module("value : { email: Text? } = { email: Nil }\n");
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn nested_record_values_are_checked_recursively() {
    let output = parse_module("value : { user: { name: Text } } = { user: { name: 42 } }\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn nested_matched_record_markers_are_reported_once() {
    let output = parse_module("value : { r: { name: Text } } = { r: { name: 1, extra?: 2 } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn set_element_record_markers_are_reported_once() {
    let output = parse_module("value : Set[{ name: Text }] = @{ { name: 1, extra?: 2 } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn extra_field_record_markers_are_reported_once() {
    let output = parse_module("value : { name: Text } = { name: 1, blob: { inner?: 3 } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn open_extra_field_record_markers_are_reported_once() {
    let output =
        parse_module("value : { name: Text, .. } = { name: \"x\", blob: { inner?: 3 } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn record_value_checking_defers_computed_rows() {
    for source in [
        "value : { name: Text, ..base } = { name: \"x\" }\n",
        "value : { name: Text } = { ..other, extra: 1 }\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
            "{source} unexpectedly produced type.missing-field"
        );
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::UNEXPECTED_FIELD),
            "{source} unexpectedly produced type.unexpected-field"
        );
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }
}

#[test]
fn aliased_record_types_are_normalized_before_field_checking() {
    let output = parse_module("Rec = { name: Text }\nvalue : Rec = { name: 42 }\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn transparent_scalar_aliases_are_normalized_before_checking() {
    let output = parse_module("Username = Text\nvalue : Username = 42\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );

    let output = parse_module("Username = Text\nvalue : Username = \"dave\"\n");
    let check = check_module(&output.module);
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));
}

#[test]
fn transparent_tuple_aliases_are_normalized_before_checking() {
    let output = parse_module("Pair = (Int, Text)\nvalue : Pair = (1, 2)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn transparent_alias_chains_are_normalized_before_checking() {
    let output = parse_module("A = B\nB = Text\nvalue : A = 42\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );
}

#[test]
fn deferred_alias_definitions_do_not_emit_mismatches() {
    let output = parse_module("Wrapped = opaque(Text)\nvalue : Wrapped = 42\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));
}

#[test]
fn cyclic_alias_normalization_terminates() {
    let output = parse_module("A = B\nB = A\nvalue : A = 42\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));

    let output = parse_module("A = (A, Int)\nvalue : A = (1, 2)\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::MISMATCH
    ));
}

#[test]
fn tuple_values_report_arity_mismatches() {
    let output = parse_module("value : (Int, Text) = (1, \"a\", 3)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check.diagnostics[0].message,
        "expected a 2-element tuple, found a 3-element tuple"
    );
}

#[test]
fn check_module_reports_type_only_entries_in_value_records() {
    let output = parse_module("value = { name?: 1 }\n");
    let check = check_module(&output.module);

    assert_eq!(check.diagnostics.len(), 1);
    assert_eq!(
        check.diagnostics[0].code.as_deref(),
        Some(codes::ty::TYPE_ONLY_RECORD_ENTRY)
    );
}

fn has_diagnostic_code(diagnostics: &[Diagnostic], code: &str) -> bool {
    matching_codes(diagnostics, code) > 0
}

fn matching_codes(diagnostics: &[Diagnostic], code: &str) -> usize {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code.as_deref() == Some(code))
        .count()
}
