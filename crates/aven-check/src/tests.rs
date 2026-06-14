use crate::*;
use aven_core::{Diagnostic, codes};
use aven_parser::{Item, Module, collect_declarations, parse_module};

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
fn lowers_record_and_variant_annotations() {
    let output = parse_module(
        "FileError = @{Io}\n\
             user : { .._, name: Text, email: Text?, phone?: Text, -password } = current\n\
             error : @{ParseError(Text), NotFound, ..FileError, -Internal} = value\n",
    );

    let user = lower_annotation(&output.module, annotation(&output.module, "user"));
    let error = lower_annotation(&output.module, annotation(&output.module, "error"));

    assert_eq!(
        user.ty,
        Type::Record(vec![
            TypeRowEntry::Open,
            TypeRowEntry::Field {
                name: "name".to_owned(),
                ty: named("Text"),
                overwrite: false,
                optional: false,
            },
            TypeRowEntry::Field {
                name: "email".to_owned(),
                ty: nullable(named("Text")),
                overwrite: false,
                optional: false,
            },
            TypeRowEntry::Field {
                name: "phone".to_owned(),
                ty: named("Text"),
                overwrite: false,
                optional: true,
            },
            TypeRowEntry::Delete("password".to_owned()),
        ])
    );
    assert!(user.diagnostics.is_empty());

    assert_eq!(
        error.ty,
        Type::Variant(vec![
            TypeRowEntry::Tag {
                name: "ParseError".to_owned(),
                payload: vec![named("Text")],
            },
            TypeRowEntry::Tag {
                name: "NotFound".to_owned(),
                payload: Vec::new(),
            },
            TypeRowEntry::Spread {
                ty: named("FileError"),
                overwrite: false,
            },
            TypeRowEntry::Delete("Internal".to_owned()),
        ])
    );
    assert!(error.diagnostics.is_empty());
}

#[test]
fn lower_annotation_reports_lowercase_variant_tags() {
    let output = parse_module("value : @{io} = value\n");
    let lowering = lower_annotation(&output.module, annotation(&output.module, "value"));

    assert_eq!(lowering.diagnostics.len(), 1);
    assert_eq!(
        lowering.diagnostics[0].code.as_deref(),
        Some(codes::ty::LOWERCASE_VARIANT_TAG)
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
        "other = 42\nvalue : Float = other\n",
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
    let output = parse_module("f = (x) => x\na = f(1)\nb = f(\"hi\")\nx : Int = a\ny : Text = b\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "generic top-level lambda reused stale inference state"
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
    let output = parse_module("value : Text =\n  result ?>\n    Ok(_) => 1\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn contextual_matches_check_block_arm_bodies_against_expected_type() {
    let output =
        parse_module("value : Text =\n  result ?>\n    Ok(_) =>\n      local = 1\n      local\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn contextual_matches_keep_pattern_binders_unknown() {
    let output =
        parse_module("item : Text = \"hi\"\nvalue : Bool =\n  result ?>\n    Ok(item) => item\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "contextual match arm borrowed a top-level type for a pattern binder"
    );
}

#[test]
fn match_guards_are_checked_as_bool() {
    for source in [
        "value : Text =\n  result ?>\n    Ok(_), 1 < 2 => \"ok\"\n",
        "flag : Bool = True\nvalue : Text =\n  result ?>\n    Ok(_), flag => \"ok\"\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "value : Text =\n  result ?>\n    Ok(_), 1 => \"ok\"\n",
        "flag : Text = \"no\"\nvalue : Text =\n  result ?>\n    Ok(_), flag => \"ok\"\n",
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
        "item : Text = \"hi\"\nvalue : Text =\n  result ?>\n    Ok(item), item => \"ok\"\n",
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
        "source : @{Ok(Text), Err(Text)} = result\nvalue : Bool = source ?>\n  Ok(item) => item\n  Err(_) => False\n",
    );
    let body_check = check_module(&body.module);
    assert_eq!(
        matching_codes(&body_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let guard = parse_module(
        "source : @{Ok(Text), Err(Text)} = result\nvalue : Text = source ?>\n  Ok(item), item => \"ok\"\n  Err(_) => \"err\"\n",
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
        "source : @{Ok(Text), Err(Text)} = result\nmatched = source ?>\n  Ok(item) => item\n  Err(error) => error\nvalue : Int = matched\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn match_results_are_inferred_for_identifier_values() {
    let mismatch =
        parse_module("result = source ?>\n  Ok(_) => 1\n  Err(_) => 2\nvalue : Text = result\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );

    let accepted =
        parse_module("result = source ?>\n  Ok(_) => 1\n  Err(_) => 2\nvalue : Int = result\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible inferred match result unexpectedly produced type.mismatch"
    );
}

#[test]
fn match_result_inference_handles_block_arm_bodies() {
    let output = parse_module(
        "result = source ?>\n  Ok(_) =>\n    local = 1\n    local\nvalue : Text = result\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn match_result_inference_defers_mixed_arm_types() {
    let output = parse_module(
        "result = source ?>\n  Ok(_) => 1\n  Err(_) => \"no\"\nvalue : Text = result\n",
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
        "item : Text = \"hi\"\nresult = source ?>\n  Ok(item) => item\nvalue : Bool = result\n",
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
        "value : Set[Int] = @{Red, Green}\n",
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
        "value : @{Ok(Int), Err(Text)} = Ok(1)\n",
        "value : @{Done} = Done\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "value : @{Ok(Text)} = Ok(1)\n",
        "value : @{Ok(Int)} = Err(1)\n",
        "value : @{Ok(Int)} = Ok(1, 2)\n",
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
        "result = Ok(1)\nvalue : @{Ok(Int), Err(Text)} = result\n",
        "done = Done\nvalue : @{Done} = done\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
        "result = Ok(1)\nvalue : @{Ok(Text), Err(Text)} = result\n",
        "result = Err(\"no\")\nvalue : @{Ok(Int)} = result\n",
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
fn variant_value_checking_defers_computed_rows() {
    let output = parse_module("Error = @{Err(Text)}\nvalue : @{..Error, Ok(Int)} = Ok(\"x\")\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "computed variant row unexpectedly produced type.mismatch"
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
        "value : Int = 1 + 2\n",
        "value : Text = \"a\" + \"b\"\n",
        "value : Bool = 1 < 2\n",
        "left : Bool = True\nright : Bool = False\nvalue : Bool = left && right\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch"
        );
    }

    for source in [
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
fn operator_inference_defers_unknown_operands() {
    for source in [
        "value : Text = missing + 1\n",
        "result = source ?>\n  Ok(item) => item + 1\nvalue : Text = result\n",
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
        Some(Type::Record(vec![
            TypeRowEntry::Field {
                name: "id".to_owned(),
                ty: named("Int"),
                overwrite: false,
                optional: false,
            },
            TypeRowEntry::Field {
                name: "name".to_owned(),
                ty: named("Text"),
                overwrite: false,
                optional: false,
            },
        ]))
    );
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
fn inferred_record_identifier_values_report_missing_and_unexpected_fields() {
    let missing = parse_module("other = { id: 1 }\nvalue : { id: Int, name: Text } = other\n");
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::MISSING_FIELD),
        1
    );

    let unexpected =
        parse_module("other = { id: 1, name: \"Ada\" }\nvalue : { id: Int } = other\n");
    let unexpected_check = check_module(&unexpected.module);
    assert_eq!(
        matching_codes(&unexpected_check.diagnostics, codes::ty::UNEXPECTED_FIELD),
        1
    );
}

#[test]
fn inferred_record_identifier_values_accept_compatible_records() {
    for source in [
        "other = { id: 1 }\nvalue : { id: Int } = other\n",
        "other = { id: 1, name: \"Ada\" }\nvalue : { .._, id: Int } = other\n",
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
    let output = parse_module("other : { .._, id: Int } = rec\nvalue : { id: Int } = other\n");
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
        "other : Int = 1\nvalue : Float = other\n",
        "other : Float = 1\nvalue : Int = other\n",
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
        "item : Text = \"hi\"\nf = (result) =>\n  result ?>\n    Ok(item) =>\n      value : Bool = item\n      value\n",
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
        "item : Text = \"hi\"\nf = (result) =>\n  result ?>\n    Ok(item) =>\n      local = item\n      value : Bool = local\n      value\n",
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
    let output = parse_module("value : { .._, name: Text } = { name: \"x\", extra: 1 }\n");
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
        parse_module("value : { .._, name: Text } = { name: \"x\", blob: { inner?: 3 } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn record_value_checking_defers_computed_rows() {
    for source in [
        "Base = { id: Int }\nvalue : { ..Base, name: Text } = { name: \"x\" }\n",
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
