use super::{
    BuiltinMethodEnvironment, Environment, EvalOutcome, ModuleImports, RuntimeType,
    RuntimeTypeBindings, Value, eval_expr, eval_module, eval_module_with_globals,
    eval_module_with_globals_and_imports,
    eval_module_with_globals_imports_runtime_types_and_builtin_methods, logging,
    record_field_value,
};
use aven_core::codes;
use aven_parser::{
    Item, Module, ModuleRole, OperatorAssociativity, OperatorFixity, OperatorFixityTable,
    OperatorOrigin, OperatorPrecedence, parse_module, parse_module_with_fixities,
};
use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn evaluates_arithmetic_with_parser_precedence() {
    assert_eval("1 + 2 * 3", Value::Int(7));
}

#[test]
fn evaluates_grouping_before_multiplication() {
    assert_eval("(1 + 2) * 3", Value::Int(9));
}

#[test]
fn evaluates_unary_minus_and_bool_not() {
    assert_eval("-5", Value::Int(-5));
    assert_eval("!false", Value::Bool(true));
}

#[test]
fn evaluates_integer_and_float_division() {
    assert_eval("7 / 2", Value::Int(3));
    assert_eval("7.0 / 2", Value::Float(3.5));
    assert_eval("1.0 / 0.0", Value::Float(f64::INFINITY));
    // Literal-typed binding as divisor (type checker accepts via singleton type).
    assert_module_value("n = 10 / 2\n100 / n\n", Value::Int(20));
    assert_module_value("n = 10 / 2\n100 % n\n", Value::Int(0));
    assert_module_value("7 / (1 + 1)\n", Value::Int(3));
}

#[test]
fn evaluates_checked_integer_division_remainder_and_bound_method() {
    assert_eval("7.div(2)", Value::Int(3));
    assert_eval("7.div(0)", Value::Undefined);
    assert_eval("7.mod(2)", Value::Int(1));
    assert_eval("7.mod(0)", Value::Undefined);
    assert_eval("7 % 2", Value::Int(1));
    assert_module_value("divide = 7.div\ndivide(2)\n", Value::Int(3));
}

#[test]
fn evaluates_unbound_builtin_operator_and_div_methods() {
    assert_module_value("Int.+(10, 20)\n", Value::Int(30));
    assert_module_value("add = Int.+\nadd(3, 4)\n", Value::Int(7));
    assert_module_value("Int.div(10, 2)\n", Value::Int(5));
    assert_module_value("divide = Int.div\ndivide(10, 2)\n", Value::Int(5));
    assert_module_value("Int.<(1, 2)\n", Value::Bool(true));
}

#[test]
fn evaluates_custom_operator_method_via_explicit_access() {
    assert_module_value(
        include_str!("../tests/fixtures/custom-operator-method.av"),
        record_value(vec![
            ("explicit", Value::Float(8.0)),
            ("unbound", Value::Float(8.0)),
        ]),
    );
}

#[test]
fn evaluates_declared_custom_bare_infix_through_named_method_dispatch() {
    let fixities = OperatorFixityTable::try_from_entries([(
        "**".to_owned(),
        OperatorFixity::new(
            OperatorPrecedence::Exponentiation,
            OperatorAssociativity::Right,
            OperatorOrigin::Platform {
                registration_index: 0,
            },
        ),
    )])
    .expect("test custom operator fixity is valid");
    let parsed = parse_module_with_fixities(
        concat!(
            "Scalar = {\n",
            "  value: Float\n",
            "  **(other: Scalar): Scalar =>\n",
            "    Scalar({ value: .value ^ other.value })\n",
            "}\n",
            "left = Scalar({ value: 2.0 })\n",
            "right = Scalar({ value: 3.0 })\n",
            "(left ** right).value\n",
        ),
        &fixities,
        ModuleRole::Entry,
    );
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);

    assert_eq!(
        eval_module(&parsed.module),
        EvalOutcome {
            value: Some(Value::Float(8.0)),
            diagnostics: Vec::new(),
        }
    );
}

#[test]
fn evaluates_float_predicate_and_ieee_equals_methods() {
    assert_eval("(0.0 / 0.0).isNaN()", Value::Bool(true));
    assert_eval("(1.0 / 0.0).isInfinite()", Value::Bool(true));
    assert_eval("(1.0 / 0.0).isFinite()", Value::Bool(false));
    assert_eval("(-1.0 / 0.0).isInfinite()", Value::Bool(true));
    assert_eval("1.5.isFinite()", Value::Bool(true));
    assert_eval("1.5.isNaN()", Value::Bool(false));
    assert_eval("1.5.isInfinite()", Value::Bool(false));
    assert_eval("(0.0 / 0.0).ieeeEquals(0.0 / 0.0)", Value::Bool(false));
    assert_eval("1.0.ieeeEquals(1.0)", Value::Bool(true));
    assert_eval("0.0.ieeeEquals(-0.0)", Value::Bool(true));
    assert_module_value("check = (0.0 / 0.0).isNaN\ncheck()\n", Value::Bool(true));
    assert_module_value(
        "sameIeee = (1.0 / 0.0).ieeeEquals\nsameIeee(1.0 / 0.0)\n",
        Value::Bool(true),
    );
}

#[test]
fn float_equality_treats_nan_as_equal_and_keeps_signed_zero() {
    assert_eval("(0.0 / 0.0) == (0.0 / 0.0)", Value::Bool(true));
    assert_eval("(0.0 / 0.0) != (0.0 / 0.0)", Value::Bool(false));
    assert_eval("0.0 == -0.0", Value::Bool(true));
    assert_eval("1.0 == (0.0 / 0.0)", Value::Bool(false));
    assert_eval("[0.0 / 0.0].has(0.0 / 0.0)", Value::Bool(true));
}

#[test]
fn float_comparisons_use_total_order_with_nan_last() {
    assert_eval("(0.0 / 0.0) < (0.0 / 0.0)", Value::Bool(false));
    assert_eval("(0.0 / 0.0) <= (0.0 / 0.0)", Value::Bool(true));
    assert_eval("(0.0 / 0.0) >= (0.0 / 0.0)", Value::Bool(true));
    assert_eval("1.0 < (0.0 / 0.0)", Value::Bool(true));
    assert_eval("(0.0 / 0.0) < 1.0", Value::Bool(false));
    assert_eval("(1.0 / 0.0) < (0.0 / 0.0)", Value::Bool(true));
    assert_eval("(-1.0 / 0.0) < 0.0", Value::Bool(true));
    assert_eval("0.0 < (1.0 / 0.0)", Value::Bool(true));
    assert_eval("(0.0 / 0.0) > (1.0 / 0.0)", Value::Bool(true));
}

#[test]
fn float_non_finite_values_display_as_aven_spellings() {
    assert_eq!(Value::Float(f64::NAN).to_string(), "NaN");
    assert_eq!(Value::Float(f64::INFINITY).to_string(), "Infinity");
    assert_eq!(Value::Float(f64::NEG_INFINITY).to_string(), "-Infinity");
    assert_eval("\"${0.0 / 0.0}\"", Value::Text("NaN".to_owned()));
    assert_eval("\"${1.0 / 0.0}\"", Value::Text("Infinity".to_owned()));
    assert_eval("\"${-1.0 / 0.0}\"", Value::Text("-Infinity".to_owned()));
}

#[test]
fn float_nan_sorts_last_and_minimum_ignores_nan() {
    let array_module = parse_ok(include_str!("../../aven-host/std/array.av"));
    let builtin_methods = BuiltinMethodEnvironment::default();
    let _array_export = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &array_module,
        Vec::new(),
        &ModuleImports::default(),
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        true,
    )
    .value
    .expect("std/array should export a record");

    let source = concat!(
        "xs = [1.0, 0.0 / 0.0, 2.0, 0.5]\n",
        "{\n",
        "  sorted: xs.sortWith((a, b) => a < b),\n",
        "  minimum: xs.minimum(),\n",
        "  maximum: xs.maximum(),\n",
        "}\n",
    );
    let outcome = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &parse_ok(source),
        Vec::new(),
        &ModuleImports::default(),
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        false,
    );
    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(record_value(vec![
                (
                    "sorted",
                    array_value(vec![
                        Value::Float(0.5),
                        Value::Float(1.0),
                        Value::Float(2.0),
                        Value::Float(f64::NAN),
                    ])
                ),
                ("minimum", Value::Float(0.5)),
                ("maximum", Value::Float(f64::NAN)),
            ])),
            diagnostics: Vec::new()
        }
    );
}

#[test]
fn reports_division_by_zero() {
    let diagnostic = eval_error("1 / 0");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::DIVISION_BY_ZERO)
    );
}

#[test]
fn evaluates_comparisons() {
    assert_eval("1 < 2", Value::Bool(true));
    assert_eval("2 >= 2.0", Value::Bool(true));
    assert_eval("\"a\" == \"a\"", Value::Bool(true));
    assert_eval("true != false", Value::Bool(true));
}

#[test]
fn evaluates_boolean_short_circuiting() {
    assert_eval("false && 1 / 0", Value::Bool(false));
    assert_eval("true || 1 / 0", Value::Bool(true));
}

#[test]
fn concatenates_text_with_plus() {
    assert_eval("\"a\" + \"b\"", Value::Text("ab".to_owned()));
}

#[test]
fn evaluates_string_interpolation_with_stringified_values() {
    assert_eval("\"a${1 + 2}b\"", Value::Text("a3b".to_owned()));
}

#[test]
fn display_protocol_interpolation_and_debug_text_use_distinct_homogeneous_rendering() {
    assert_eval("\"${1.0}\"", Value::Text("1.0".to_owned()));
    assert_eval("\"${[1.0, 2.5]}\"", Value::Text("[1.0, 2.5]".to_owned()));
    assert_eval("\"${[\"a\", \"b\"]}\"", Value::Text("[a, b]".to_owned()));
    assert_module_value(
        "debugText([\"a\", \"b\"])\n",
        Value::Text("[\"a\", \"b\"]".to_owned()),
    );
    assert_eval("\"${[[\"a\"]]}\"", Value::Text("[[a]]".to_owned()));
    assert_eval("undefined.toText()", Value::Text("undefined".to_owned()));
    assert_eval("null.toText()", Value::Text("null".to_owned()));
}

#[test]
fn interpolation_falls_back_to_debug_text_for_closures() {
    assert_module_value(
        "f = (x) => x\n\"${f}\"\n",
        Value::Text("<function>".to_owned()),
    );
}

#[test]
fn debug_text_marks_slot_records_as_opaque() {
    assert_eq!(
        super::debug_text(&Value::SlotRecord {
            fields: Rc::new(Vec::new()),
            slots: Rc::new(Vec::new()),
        }),
        "<slot-record>"
    );
}

#[test]
fn evaluates_string_interpolation_text_escapes() {
    assert_eval(r#""a\n${1 + 2}b\t""#, Value::Text("a\n3b\t".to_owned()));
}

#[test]
fn evaluates_interpolation_field_access() {
    assert_module_value(
        "user = { name: \"Ada\" }\n\"${user.name}\"\n",
        Value::Text("Ada".to_owned()),
    );
}

#[test]
fn evaluates_supported_string_escapes() {
    assert_eval(
        r#""line\nquote\"slash\\tab\t""#,
        Value::Text("line\nquote\"slash\\tab\t".to_owned()),
    );
}

#[test]
fn evaluates_unicode_string_escape() {
    assert_eval(r#""\u{41}""#, Value::Text("A".to_owned()));
}

#[test]
fn evaluates_nested_record_expression_inside_interpolation() {
    assert_eval("\"${ { a: 1 }.a }\"", Value::Text("1".to_owned()));
}

#[test]
fn reports_type_errors() {
    let diagnostic = eval_error("1 + \"a\"");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
}

#[test]
fn evaluates_module_to_last_expression_value() {
    let module = parse_ok("1\n2 * 3\n");
    let outcome = eval_module(&module);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(Value::Int(6)),
            diagnostics: Vec::new()
        }
    );
}

#[test]
fn evaluates_values_annotated_with_parameterized_recursive_types() {
    assert_module_value(
        concat!(
            "List = (t: Type) => @{ @Nil, @Cons((t, List(t))) }\n",
            "xs: List(Int) = @Cons((1, @Cons((2, @Nil))))\n",
            "len : (List(Int)) -> Int\n",
            "len = (xs) => xs ?> @Nil => 0, @Cons((_, rest)) => 1 + len(rest)\n",
            "len(xs)\n",
        ),
        Value::Int(2),
    );
}

#[test]
fn evaluates_sequential_bindings() {
    assert_module_value("x = 5\ny = x + 1\ny\n", Value::Int(6));
}

#[test]
fn evaluates_record_pattern_binding() {
    assert_module_value(
        "source = { left: 2, right: 3 }\n{ left, right } = source\nleft + right\n",
        Value::Int(5),
    );
}

#[test]
fn evaluates_record_pattern_binding_rename() {
    assert_module_value(
        "source = { value: 7 }\n{ value -> renamed } = source\nrenamed\n",
        Value::Int(7),
    );
}

#[test]
fn evaluates_block_spread_binding() {
    assert_module_value(
        "result =\n  ..{ left: 2, right: 3 }\n  left + right\nresult\n",
        Value::Int(5),
    );
}

#[test]
fn evaluates_block_spread_replacement() {
    assert_module_value(
        "result =\n  value = 1\n  :..{ value: 4, extra: 2 }\n  value + extra\nresult\n",
        Value::Int(6),
    );
}

#[test]
fn evaluates_pattern_binding_rhs_once() {
    let calls = Rc::new(RefCell::new(0));
    let make_calls = Rc::clone(&calls);
    let make = Value::native(move |_| {
        *make_calls.borrow_mut() += 1;
        Ok(record_value(vec![("value", Value::Int(9))]))
    });
    let module = parse_ok("{ value } = make()\nvalue\n");
    let outcome = eval_module_with_globals(&module, vec![("make".to_owned(), make)]);

    assert_eq!(outcome.value, Some(Value::Int(9)));
    assert_eq!(outcome.diagnostics, Vec::new());
    assert_eq!(*calls.borrow(), 1);
}

#[test]
fn evaluates_spread_binding_operand_once() {
    let calls = Rc::new(RefCell::new(0));
    let make_calls = Rc::clone(&calls);
    let make = Value::native(move |_| {
        *make_calls.borrow_mut() += 1;
        Ok(record_value(vec![("value", Value::Int(9))]))
    });
    let module = parse_ok("..make()\nvalue\n");
    let outcome = eval_module_with_globals(&module, vec![("make".to_owned(), make)]);

    assert_eq!(outcome.value, Some(Value::Int(9)));
    assert_eq!(outcome.diagnostics, Vec::new());
    assert_eq!(*calls.borrow(), 1);
}

#[test]
fn evaluates_simple_function_call() {
    assert_module_value("double = (x) => x * 2\ndouble(5)\n", Value::Int(10));
}

#[test]
fn evaluates_higher_order_function_call() {
    assert_module_value(
        "twice = (f, x) => f(f(x))\ninc = (n) => n + 1\ntwice(inc, 1)\n",
        Value::Int(3),
    );
}

#[test]
fn closures_capture_their_defining_scope() {
    assert_module_value(
        "add_base =\n  base = 10\n  (x) => x + base\nbase = 1\nadd_base(2)\n",
        Value::Int(12),
    );
}

#[test]
fn reports_function_arity_mismatch() {
    let diagnostic = module_error("id = (x) => x\nid()\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::ARITY_MISMATCH)
    );
}

#[test]
fn applies_trailing_parameter_default_when_omitted() {
    assert_module_value("f = (x, y = 10) => x + y\nf(1)\n", Value::Int(11));
}

#[test]
fn supplied_argument_overrides_parameter_default() {
    assert_module_value("f = (x, y = 10) => x + y\nf(1, 2)\n", Value::Int(3));
}

#[test]
fn default_may_reference_an_earlier_parameter() {
    assert_module_value("g = (x, y = x + 1) => y\ng(5)\n", Value::Int(6));
}

#[test]
fn unannotated_single_default_applies_with_no_args() {
    assert_module_value(
        "greet = (name = \"world\") => name\ngreet()\n",
        Value::Text("world".to_owned()),
    );
}

#[test]
fn default_is_not_evaluated_when_argument_supplied() {
    assert_module_value("h = (x, y = 1 / 0) => x\nh(7, 2)\n", Value::Int(7));
}

#[test]
fn omitted_default_evaluates_and_can_fail() {
    let diagnostic = module_error("h = (x, y = 1 / 0) => x\nh(7)\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::DIVISION_BY_ZERO)
    );
}

#[test]
fn reports_too_few_arguments_below_required() {
    let diagnostic = module_error("f = (x, y = 10) => x + y\nf()\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::ARITY_MISMATCH)
    );
}

#[test]
fn reports_too_many_arguments_above_total() {
    let diagnostic = module_error("f = (x, y = 10) => x + y\nf(1, 2, 3)\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::ARITY_MISMATCH)
    );
}

#[test]
fn reports_calling_non_function_values() {
    let diagnostic = eval_error("5(1)");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::NOT_CALLABLE)
    );
}

#[test]
fn evaluates_native_host_function_through_field_access() {
    let captured = Rc::new(RefCell::new(Vec::new()));
    let capture = Rc::clone(&captured);
    let host = host_with(
        "log",
        Value::native(move |args| {
            if args.len() != 1 || args.first() != Some(&Value::Text("hi".to_owned())) {
                return Err(format!("unexpected args: {args:?}"));
            }
            capture.borrow_mut().push(args[0].to_string());
            Ok(Value::unit())
        }),
    );
    let module = parse_ok("Host.Native.log(\"hi\")\n");

    let outcome = eval_module_with_globals(&module, vec![("Host".to_owned(), host)]);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(Value::unit()),
            diagnostics: Vec::new()
        }
    );
    assert_eq!(captured.borrow().clone(), vec!["hi".to_owned()]);
}

#[test]
fn reports_native_host_errors_at_call_span() {
    let host = host_with("fail", Value::native(|_| Err("native failure".to_owned())));
    let module = parse_ok("Host.Native.fail(\"hi\")\n");
    let call_span = module_expr_span(&module);

    let outcome = eval_module_with_globals(&module, vec![("Host".to_owned(), host)]);

    assert_eq!(outcome.value, None);
    assert_eq!(outcome.diagnostics.len(), 1);
    let diagnostic = &outcome.diagnostics[0];
    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
    assert_eq!(diagnostic.labels[0].span, call_span);
    assert_eq!(diagnostic.labels[0].message, "native failure");
}

#[test]
fn log_info_emits_message_fields_and_trace_context() {
    let records = Rc::new(RefCell::new(Vec::new()));
    let logger = capturing_logger(Rc::clone(&records));
    let module = parse_ok("logger.info(\"hi\", { userId: 42 })\n");

    let outcome = eval_module_with_globals(&module, vec![("logger".to_owned(), logger)]);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(Value::unit()),
            diagnostics: Vec::new()
        }
    );
    let records = records.borrow();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.level, logging::Level::Info);
    assert_eq!(record.message, "hi");
    assert_eq!(
        record_field_value(&record.attributes, "userId"),
        Some(&Value::Int(42))
    );
    assert_eq!(record.trace, fixed_trace_context());
}

#[test]
fn child_logger_inherits_trace_and_merges_bound_context() {
    let records = Rc::new(RefCell::new(Vec::new()));
    let logger = capturing_logger(Rc::clone(&records));
    let module =
        parse_ok("requestLog = logger.child({ requestId: \"r1\" })\nrequestLog.info(\"child\")\n");

    let outcome = eval_module_with_globals(&module, vec![("logger".to_owned(), logger)]);

    assert_eq!(outcome.value, Some(Value::unit()));
    assert!(outcome.diagnostics.is_empty());
    let records = records.borrow();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        record_field_value(&record.attributes, "requestId"),
        Some(&Value::Text("r1".to_owned()))
    );
    assert_eq!(record.trace, fixed_trace_context());
}

#[test]
fn child_logger_trace_keys_update_trace_context_not_attributes() {
    let records = Rc::new(RefCell::new(Vec::new()));
    let logger = capturing_logger(Rc::clone(&records));
    let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
    let module = parse_ok(&format!(
        "child = logger.child({{ traceId: \"{trace_id}\", requestId: \"r1\" }})\nchild.info(\"child\")\n"
    ));

    let outcome = eval_module_with_globals(&module, vec![("logger".to_owned(), logger)]);

    assert_eq!(outcome.value, Some(Value::unit()));
    assert!(outcome.diagnostics.is_empty());
    let records = records.borrow();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.trace.trace_id, trace_id);
    assert_eq!(record.trace.span_id, fixed_trace_context().span_id);
    assert!(record_field_value(&record.attributes, "traceId").is_none());
    assert_eq!(
        record_field_value(&record.attributes, "requestId"),
        Some(&Value::Text("r1".to_owned()))
    );
}

#[test]
fn log_message_validation_reports_platform_error() {
    let records = Rc::new(RefCell::new(Vec::new()));
    let logger = capturing_logger(records);
    let diagnostic =
        module_error_with_globals("logger.info(5)\n", vec![("logger".to_owned(), logger)]);

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
    assert!(
        diagnostic.labels[0]
            .message
            .contains("log.info message must be Text"),
        "expected message-first validation, got {:?}",
        diagnostic.labels
    );
}

#[test]
fn log_level_severity_numbers_match_otel() {
    assert_eq!(logging::Level::Trace.severity_number(), 1);
    assert_eq!(logging::Level::Debug.severity_number(), 5);
    assert_eq!(logging::Level::Info.severity_number(), 9);
    assert_eq!(logging::Level::Warn.severity_number(), 13);
    assert_eq!(logging::Level::Error.severity_number(), 17);
    assert_eq!(logging::Level::Fatal.severity_number(), 21);
}

#[test]
fn module_bindings_can_shadow_injected_globals() {
    let toolbox = host_with(
        "log",
        Value::native(|_| Err("injected host should be shadowed".to_owned())),
    );
    let module = parse_ok(
        "toolbox = { Native: { log: (message) => message } }\ntoolbox.Native.log(\"local\")\n",
    );

    let outcome = eval_module_with_globals(&module, vec![("toolbox".to_owned(), toolbox)]);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(Value::Text("local".to_owned())),
            diagnostics: Vec::new()
        }
    );
}

#[test]
fn closures_resolve_sibling_top_level_functions_at_call_time() {
    assert_module_value("f = (x) => g(x)\ng = (x) => x + 1\nf(2)\n", Value::Int(3));
}

#[test]
fn evaluates_block_bindings_and_result() {
    assert_module_value(
        "result =\n  a = 2\n  b = a * 3\n  b + 1\nresult\n",
        Value::Int(7),
    );
}

#[test]
fn block_local_shadowing_does_not_leak() {
    assert_module_value("x = 1\nshadow =\n  x = 2\n  x\nx\n", Value::Int(1));
}

#[test]
fn explicit_shadow_rhs_sees_old_binding_and_does_not_leak() {
    assert_module_value(
        "make = (value) =>\n  inner =\n    value := value + 1\n    value\n  (inner, value)\nmake(10)\n",
        tuple_value(vec![Value::Int(11), Value::Int(10)]),
    );
}

/// Spec: `:=` creates a new binding; closures that captured the previous
/// binding must keep seeing that value (not the post-`:=` one).
#[test]
fn explicit_shadow_does_not_mutate_prior_closure_capture() {
    assert_module_value(
        "main = () =>\n  x = 1\n  f = () => x + 1\n  x := \"two\"\n  f()\nmain()\n",
        Value::Int(2),
    );
}

#[test]
fn explicit_shadow_is_visible_to_closures_created_after() {
    assert_module_value(
        "main = () =>\n  x = 1\n  x := \"two\"\n  f = () => x\n  f()\nmain()\n",
        Value::Text("two".to_owned()),
    );
}

#[test]
fn sequential_explicit_shadow_rebinding() {
    assert_module_value(
        "main = () =>\n  x = 1\n  x := x + 1\n  x\nmain()\n",
        Value::Int(2),
    );
}

/// Nested-block `:=` rebinds only inside the block (blocks already open a child
/// scope). Uses after the block still see the outer binding.
#[test]
fn nested_block_explicit_shadow_does_not_escape() {
    assert_module_value(
        "main = () =>\n  x = 1\n  inner =\n    x := 2\n    x\n  (inner, x)\nmain()\n",
        tuple_value(vec![Value::Int(2), Value::Int(1)]),
    );
}

#[test]
fn evaluates_block_without_trailing_expression_to_undefined() {
    assert_module_value("value =\n  x = 1\nvalue\n", Value::Undefined);
}

#[test]
fn reports_unbound_name_references() {
    let diagnostic = eval_error("missing");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::UNBOUND_NAME)
    );
}

#[test]
fn reports_forward_references_as_unbound() {
    let module = parse_ok("a = b\nb = 1\n");
    let outcome = eval_module(&module);

    assert_eq!(outcome.value, None);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(
        outcome.diagnostics[0].code.as_deref(),
        Some(codes::runtime::UNBOUND_NAME)
    );
}

#[test]
fn evaluates_record_literals_and_field_access() {
    assert_module_value(
        "user = { name: \"Ada\", age: 36 }\nuser.name\n",
        Value::Text("Ada".to_owned()),
    );
    assert_eq!(
        format!(
            "{}",
            record_value(vec![
                ("name", Value::Text("Ada".to_owned())),
                ("age", Value::Int(36))
            ])
        ),
        "{ name: \"Ada\", age: 36 }"
    );
}

#[test]
fn evaluates_quoted_record_field_names() {
    assert_module_value(
        "headers = { \"content-type\": \"application/json\", \"x-request-id\": \"abc\" }\nheaders.\"content-type\"\n",
        Value::Text("application/json".to_owned()),
    );
    assert_module_value(
        "headers = { \"content-type\": \"application/json\" }\nheaders?.\"content-type\"\n",
        Value::Text("application/json".to_owned()),
    );
}

#[test]
fn evaluates_record_spread_with_overwrite() {
    assert_module_value(
        "user = { name: \"Ada\", age: 36 }\nolder = { ..user, age :: 37 }\nolder.age\n",
        Value::Int(37),
    );
}

#[test]
fn evaluates_record_delete() {
    assert_module_value(
        "user = { name: \"Ada\", age: 36 }\ncleaned = { ..user, -age }\ncleaned\n",
        record_value(vec![("name", Value::Text("Ada".to_owned()))]),
    );
}

#[test]
fn evaluates_record_rename() {
    assert_module_value(
        "user = { name: \"Ada\", age: 36 }\nrenamed = { ..user, name -> handle }\nrenamed.handle\n",
        Value::Text("Ada".to_owned()),
    );
}

#[test]
fn evaluates_record_shorthand() {
    assert_module_value(
        "name = \"Ada\"\nage = 36\nuser = { name, age }\nuser.age\n",
        Value::Int(36),
    );
}

#[test]
fn evaluates_computed_record_field_and_delete() {
    assert_module_value(
        "key = \"handle\"\nremove = \"age\"\nuser = { name: \"Ada\", age: 36, [key]: \"ada\" }\ncleaned = { ..user, -[remove] }\ncleaned[\"handle\"]\n",
        Value::Text("ada".to_owned()),
    );
}

#[test]
fn evaluates_nested_record_access() {
    assert_module_value(
        "user = { profile: { name: \"Ada\" } }\nuser.profile.name\n",
        Value::Text("Ada".to_owned()),
    );
}

#[test]
fn record_equality_is_order_independent() {
    assert_eval("{ a: 1, b: 2 } == { b: 2, a: 1 }", Value::Bool(true));
}

#[test]
fn evaluates_record_comprehension_pick_over_literal_set() {
    assert_module_value(
        "user = { name: \"Ada\", email: \"ada@x.dev\", age: 36 }\n\
         pick = (o, keys) => { keys -> k; (k, o[k]) }\n\
         pick(user, @{\"name\", \"email\"})\n",
        record_value(vec![
            ("name", Value::Text("Ada".to_owned())),
            ("email", Value::Text("ada@x.dev".to_owned())),
        ]),
    );
}

#[test]
fn evaluates_record_comprehension_omit_with_keysof_and_has_guard() {
    assert_module_value(
        "user = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         omit = (o, drop) => { keysOf(o) -> k, !drop.has(k); (k, o[k]) }\n\
         omit(user, @{\"name\"})\n",
        record_value(vec![("email", Value::Text("ada@x.dev".to_owned()))]),
    );
}

#[test]
fn record_comprehension_guard_filters_iterations() {
    assert_eval(
        "{ @{\"name\", \"email\"} -> k, k == \"email\"; (k, k) }",
        record_value(vec![("email", Value::Text("email".to_owned()))]),
    );
}

#[test]
fn record_comprehension_non_bool_guard_reports_type_error() {
    let diagnostic = eval_error("{ @{\"name\"} -> k, k; (k, 1) }");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    assert_eq!(diagnostic.labels[0].message, "expected a Bool guard");
}

#[test]
fn record_comprehension_can_iterate_record_labels() {
    assert_eval(
        "{ { name: \"Ada\", email: \"ada@x.dev\" } -> k; (k, k) }",
        record_value(vec![
            ("name", Value::Text("name".to_owned())),
            ("email", Value::Text("email".to_owned())),
        ]),
    );
}

#[test]
fn record_comprehension_source_type_error_reports_type_error() {
    let diagnostic = eval_error("{ 1 -> k; (k, 1) }");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
}

#[test]
fn tuple_emit_in_record_inserts_field() {
    assert_eval(
        "{ (\"name\", \"Ada\") }",
        record_value(vec![("name", Value::Text("Ada".to_owned()))]),
    );
}

#[test]
fn tuple_emit_requires_text_label() {
    let diagnostic = eval_error("{ (1, \"Ada\") }");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    assert!(diagnostic.labels[0].message.contains("Text label"));
}

#[test]
fn tuple_emit_requires_arity_two_tuple() {
    let diagnostic = eval_error("{ (\"name\", \"Ada\", 1) }");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    assert!(diagnostic.labels[0].message.contains("Text label"));
}

#[test]
fn keyof_returns_record_labels_as_set() {
    assert_module_value(
        "keysOf({ name: \"Ada\", email: \"ada@x.dev\" })\n",
        set_value(vec![
            Value::Text("name".to_owned()),
            Value::Text("email".to_owned()),
        ]),
    );
}

#[test]
fn keyof_non_record_reports_platform_error() {
    let diagnostic = module_error("keysOf(1)\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
}

#[test]
fn pick_keeps_named_fields_in_record_order() {
    assert_module_value(
        "pick({ name: \"Ada\", email: \"a@x\", age: 3 }, @{\"name\", \"email\"})\n",
        record_value(vec![
            ("name", Value::Text("Ada".to_owned())),
            ("email", Value::Text("a@x".to_owned())),
        ]),
    );
}

#[test]
fn omit_drops_named_fields_in_record_order() {
    assert_module_value(
        "omit({ name: \"Ada\", email: \"a@x\" }, @{\"name\"})\n",
        record_value(vec![("email", Value::Text("a@x".to_owned()))]),
    );
}

#[test]
fn omit_runs_uniformly_on_a_type_record() {
    // The headline case: a record *type* is just a record whose values are
    // types, so `omit` runs at runtime over it with no special casing.
    assert_module_value(
        "omit({ name: Text, email: Text }, @{\"name\"})\n",
        record_value(vec![("email", Value::named_type("Text"))]),
    );
}

#[test]
fn pick_skips_keys_absent_from_the_record() {
    assert_module_value(
        "pick({ name: \"Ada\" }, @{\"name\", \"missing\"})\n",
        record_value(vec![("name", Value::Text("Ada".to_owned()))]),
    );
}

#[test]
fn pick_non_record_reports_platform_error() {
    let diagnostic = module_error("pick(5, @{\"a\"})\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
}

#[test]
fn pick_non_set_reports_platform_error() {
    for source in ["pick({ a: 1 }, [1])\n", "pick({ a: 1 }, \"a\")\n"] {
        let diagnostic = module_error(source);

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }
}

#[test]
fn pick_non_text_set_member_reports_platform_error() {
    let diagnostic = module_error("pick({ a: 1 }, @{1})\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
}

#[test]
fn pick_wrong_arity_reports_platform_error() {
    let diagnostic = module_error("pick({ a: 1 })\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
}

#[test]
fn user_binding_shadows_pick_builtin() {
    assert_module_value("pick = 5\npick\n", Value::Int(5));
}

#[test]
fn map_constructs_empty_and_from_entries() {
    assert_module_value("Map.empty()\n", map_value(vec![]));
    assert_module_value(
        "Map.from([(\"a\", 1), (\"b\", 2)])\n",
        map_value(vec![
            (Value::Text("a".to_owned()), Value::Int(1)),
            (Value::Text("b".to_owned()), Value::Int(2)),
        ]),
    );
}

#[test]
fn map_constructor_builds_from_pair_array() {
    assert_module_value(
        "Map([(\"a\", 1), (\"b\", 2)])\n",
        map_value(vec![
            (Value::Text("a".to_owned()), Value::Int(1)),
            (Value::Text("b".to_owned()), Value::Int(2)),
        ]),
    );
    assert_module_value("Map([])\n", map_value(vec![]));
}

#[test]
fn map_constructor_preserves_insertion_order_and_overwrites_duplicates() {
    assert_module_value(
        "Map([(\"a\", 1), (\"b\", 2), (\"a\", 3)]).entries()\n",
        array_value(vec![
            tuple_value(vec![Value::Text("a".to_owned()), Value::Int(3)]),
            tuple_value(vec![Value::Text("b".to_owned()), Value::Int(2)]),
        ]),
    );
}

#[test]
fn map_constructor_accepts_non_text_keys() {
    assert_module_value(
        "Map([(1, \"x\"), (2, \"y\")])\n",
        map_value(vec![
            (Value::Int(1), Value::Text("x".to_owned())),
            (Value::Int(2), Value::Text("y".to_owned())),
        ]),
    );
}

#[test]
fn map_constructor_display_uses_insertion_order() {
    assert_module_value(
        "\"${Map([(\"a\", 1)])}\"\n",
        Value::Text("Map{ a: 1 }".to_owned()),
    );
    assert_module_value(
        "\"${Map([(\"a\", 1), (\"b\", 2)])}\"\n",
        Value::Text("Map{ a: 1, b: 2 }".to_owned()),
    );
}

#[test]
fn map_constructor_bad_shape_reports_platform_error() {
    for source in ["Map(\"no\")\n", "Map([1])\n", "Map([(\"a\", 1, 2)])\n"] {
        let diagnostic = module_error(source);

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }
}

#[test]
fn map_type_application_yields_composite_type_value() {
    // `Map(K, V)` in value position builds a composite type value, not a
    // record index of the (type-valued) `Map`.
    assert_module_value(
        "Map(Text, Int)\n",
        Value::Type(RuntimeType::Map(
            Box::new(Value::named_type("Text")),
            Box::new(Value::named_type("Int")),
        )),
    );
}

#[test]
fn map_name_is_a_type_value() {
    assert_module_value("Map\n", Value::named_type("Map"));
}

#[test]
fn map_display_uses_insertion_order() {
    assert_module_value(
        "\"${Map.from([(\"a\", 1), (\"b\", 2)])}\"\n",
        Value::Text("Map{ a: 1, b: 2 }".to_owned()),
    );
}

#[test]
fn map_from_deduplicates_keys_with_last_value_and_first_order() {
    assert_module_value(
        "Map.from([(\"a\", 1), (\"b\", 2), (\"a\", 3)]).entries()\n",
        array_value(vec![
            tuple_value(vec![Value::Text("a".to_owned()), Value::Int(3)]),
            tuple_value(vec![Value::Text("b".to_owned()), Value::Int(2)]),
        ]),
    );
}

#[test]
fn map_get_hit_and_miss() {
    assert_module_value(
        "m = Map.from([(\"a\", 1)])\n[m.get(\"a\"), m.get(\"z\")]\n",
        array_value(vec![Value::Int(1), Value::Undefined]),
    );
}

#[test]
fn map_index_hit_and_miss() {
    assert_module_value(
        "m = Map.from([(\"a\", 1)])\n[m[\"a\"], m[\"z\"]]\n",
        array_value(vec![Value::Int(1), Value::Undefined]),
    );
}

#[test]
fn map_set_and_delete_return_new_maps() {
    assert_module_value(
        "m = Map.from([(\"a\", 1)])\n\
         n = m.set(\"a\", 2).set(\"b\", 3)\n\
         d = n.delete(\"a\")\n\
         [m.entries(), n.entries(), d.entries(), d.delete(\"missing\").entries()]\n",
        array_value(vec![
            array_value(vec![tuple_value(vec![
                Value::Text("a".to_owned()),
                Value::Int(1),
            ])]),
            array_value(vec![
                tuple_value(vec![Value::Text("a".to_owned()), Value::Int(2)]),
                tuple_value(vec![Value::Text("b".to_owned()), Value::Int(3)]),
            ]),
            array_value(vec![tuple_value(vec![
                Value::Text("b".to_owned()),
                Value::Int(3),
            ])]),
            array_value(vec![tuple_value(vec![
                Value::Text("b".to_owned()),
                Value::Int(3),
            ])]),
        ]),
    );
}

#[test]
fn map_methods_report_membership_size_keys_values_and_entries() {
    assert_module_value(
        "m = Map.from([(\"a\", 1), (\"b\", 2)])\n\
         [m.has(\"a\"), m.has(\"z\"), m.size(), m.keys(), m.values(), m.entries()]\n",
        array_value(vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(2),
            array_value(vec![
                Value::Text("a".to_owned()),
                Value::Text("b".to_owned()),
            ]),
            array_value(vec![Value::Int(1), Value::Int(2)]),
            array_value(vec![
                tuple_value(vec![Value::Text("a".to_owned()), Value::Int(1)]),
                tuple_value(vec![Value::Text("b".to_owned()), Value::Int(2)]),
            ]),
        ]),
    );
}

#[test]
fn map_merge_uses_right_hand_conflicts_and_left_order() {
    assert_module_value(
        "left = Map.from([(\"a\", 1), (\"b\", 2)])\n\
         right = Map.from([(\"b\", 20), (\"c\", 30)])\n\
         left.merge(right).entries()\n",
        array_value(vec![
            tuple_value(vec![Value::Text("a".to_owned()), Value::Int(1)]),
            tuple_value(vec![Value::Text("b".to_owned()), Value::Int(20)]),
            tuple_value(vec![Value::Text("c".to_owned()), Value::Int(30)]),
        ]),
    );
}

#[test]
fn map_equality_is_order_independent() {
    assert_module_value(
        "Map.from([(\"a\", 1), (\"b\", 2)]) == Map.from([(\"b\", 2), (\"a\", 1)])\n",
        Value::Bool(true),
    );
}

#[test]
fn map_keys_use_structural_equality() {
    assert_module_value(
        "m = Map.from([((\"x\", 1), \"hit\")])\n\
         m.get((\"x\", 1))\n",
        Value::Text("hit".to_owned()),
    );
}

#[test]
fn map_from_bad_shape_reports_platform_error() {
    for source in [
        "Map.from(\"no\")\n",
        "Map.from([1])\n",
        "Map.from([(\"a\", 1, 2)])\n",
    ] {
        let diagnostic = module_error(source);

        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::PLATFORM_ERROR)
        );
    }
}

#[test]
fn map_rejects_function_keys() {
    let diagnostic = module_error("Map.from([((x) => x, 1)])\n");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::PLATFORM_ERROR)
    );
}

#[test]
fn map_grouping_example_runs() {
    assert_module_value(
        concat!(
            "words = [\"red\", \"blue\", \"red\"]\n",
            "count = (items: Array(Text), index: Int, acc: Map(Text, Int)) =>\n",
            "  next = items[index]\n",
            "  next ?>\n",
            "    undefined => acc\n",
            "    _ =>\n",
            "      word : Text = next ?? \"\"\n",
            "      count(items, index + 1, acc.set(word, (acc.get(word) ?? 0) + 1))\n",
            "counts : Map(Text, Int) = count(words, 0, Map.empty())\n",
            "counts.entries()\n",
        ),
        array_value(vec![
            tuple_value(vec![Value::Text("red".to_owned()), Value::Int(2)]),
            tuple_value(vec![Value::Text("blue".to_owned()), Value::Int(1)]),
        ]),
    );
}

#[test]
fn std_array_combinators_run_via_import() {
    let array_source = include_str!("../../aven-host/std/array.av");
    let array_module = parse_ok(array_source);
    let builtin_methods = BuiltinMethodEnvironment::default();
    let array_export = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &array_module,
        Vec::new(),
        &ModuleImports::default(),
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        true,
    )
    .value
    .expect("std/array should export a record");

    let imports = ModuleImports::new([("std/array".to_owned(), array_export)]);
    let source = concat!(
        "{ range } = import(\"std/array\")\n",
        "xs = [10, 20, 30]\n",
        "empty = []\n",
        "emptyNested: Array(Array(Int)) = []\n",
        "zero: Int = 0\n",
        "pairs = [{k: 2, id: 1}, {k: 1, id: 2}, {k: 2, id: 3}]\n",
        "users = [{name: \"bob\", age: 30}, {name: \"alice\", age: 25}, {name: \"carol\", age: 30}]\n",
        "emptyUsers: Array({age: Int}) = []\n",
        "{\n",
        "  length: xs.length(),\n",
        "  isEmpty: empty.isEmpty(),\n",
        "  first: xs.first(),\n",
        "  firstEmpty: empty.first(),\n",
        "  last: xs.last(),\n",
        "  lastEmpty: empty.last(),\n",
        "  fold: xs.fold(zero, (acc, x) => acc + x),\n",
        "  sum: [1, 2, 3].sum(),\n",
        "  count: xs.count((x) => x > 15),\n",
        "  all: xs.all((x) => x > 0),\n",
        "  any: xs.any((x) => x == 20),\n",
        "  findHit: xs.find((x) => x == 20),\n",
        "  findMiss: xs.find((x) => x == 99),\n",
        "  indexOfHit: xs.indexOf(20),\n",
        "  indexOfMiss: xs.indexOf(99),\n",
        "  indexOfEmpty: empty.indexOf(1),\n",
        "  map: xs.map((x) => x + 1),\n",
        "  mapEmpty: empty.map((x) => x + 1),\n",
        "  flatMap: xs.flatMap((x) => [x, x + 1]),\n",
        "  flatMapEmpty: empty.flatMap((x) => [x]),\n",
        "  flatMapToEmpty: xs.flatMap((_) => []),\n",
        "  filter: xs.filter((x) => x > 15),\n",
        "  filterEmpty: empty.filter((x) => x > 15),\n",
        "  reverse: xs.reverse(),\n",
        "  reverseEmpty: empty.reverse(),\n",
        "  concat: [1].concat([2, 3]),\n",
        "  concatLeftEmpty: empty.concat(xs),\n",
        "  concatRightEmpty: xs.concat(empty),\n",
        "  composed: xs.filter((x) => x > 15).map((x) => x / 10),\n",
        "  take2: xs.take(2),\n",
        "  take0: xs.take(0),\n",
        "  takeNeg: xs.take(-1),\n",
        "  takeBig: xs.take(99),\n",
        "  takeEmpty: empty.take(2),\n",
        "  drop2: xs.drop(2),\n",
        "  drop0: xs.drop(0),\n",
        "  dropNeg: xs.drop(-1),\n",
        "  dropBig: xs.drop(99),\n",
        "  dropEmpty: empty.drop(2),\n",
        "  slice: xs.slice(1, 3),\n",
        "  sliceEmpty: xs.slice(2, 2),\n",
        "  sliceClampLow: xs.slice(-5, 2),\n",
        "  slicePastEnd: xs.slice(1, 99),\n",
        "  sliceNegEnd: xs.slice(0, -1),\n",
        "  sliceNegStart: xs.slice(-2, 99),\n",
        "  sliceNegStartFar: xs.slice(-99, 2),\n",
        "  sliceInverted: xs.slice(3, 2),\n",
        "  sliceNegInverted: xs.slice(-1, -3),\n",
        "  sliceEmptyArr: empty.slice(0, 1),\n",
        "  sliceEmptyNeg: empty.slice(-1, 0),\n",
        "  zipShort: [1, 2, 3].zip([10, 20]),\n",
        "  zipLeftEmpty: empty.zip(xs),\n",
        "  zipRightEmpty: xs.zip(empty),\n",
        "  flatten: [[1, 2], [3], [], [4]].flatten(),\n",
        "  flattenEmpty: emptyNested.flatten(),\n",
        "  range: range(1, 5),\n",
        "  rangeEmpty: range(3, 3),\n",
        "  rangeRev: range(5, 1),\n",
        "  sort: [3, 1, 2].sortWith((a, b) => a < b),\n",
        "  sortEmpty: empty.sortWith((a, b) => a < b),\n",
        "  sortStable: pairs.sortWith((a, b) => a.k < b.k),\n",
        "  sortByAge: users.sortBy((u) => u.age),\n",
        "  sortByAlready: [{age: 1}, {age: 2}].sortBy((u) => u.age),\n",
        "  sortByEmpty: emptyUsers.sortBy((u) => u.age),\n",
        "  sortByStable: pairs.sortBy((u) => u.k),\n",
        "  minimum: xs.minimum(),\n",
        "  minimumEmpty: empty.minimum(),\n",
        "  maximum: xs.maximum(),\n",
        "  maximumEmpty: empty.maximum(),\n",
        "}\n",
    );
    let module = parse_ok(source);
    let outcome = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &module,
        Vec::new(),
        &imports,
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        false,
    );

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(record_value(vec![
                ("length", Value::Int(3)),
                ("isEmpty", Value::Bool(true)),
                ("first", Value::Int(10)),
                ("firstEmpty", Value::Undefined),
                ("last", Value::Int(30)),
                ("lastEmpty", Value::Undefined),
                ("fold", Value::Int(60)),
                ("sum", Value::Int(6)),
                ("count", Value::Int(2)),
                ("all", Value::Bool(true)),
                ("any", Value::Bool(true)),
                ("findHit", Value::Int(20)),
                ("findMiss", Value::Undefined),
                ("indexOfHit", Value::Int(1)),
                ("indexOfMiss", Value::Undefined),
                ("indexOfEmpty", Value::Undefined),
                (
                    "map",
                    array_value(vec![Value::Int(11), Value::Int(21), Value::Int(31)])
                ),
                ("mapEmpty", array_value(vec![])),
                (
                    "flatMap",
                    array_value(vec![
                        Value::Int(10),
                        Value::Int(11),
                        Value::Int(20),
                        Value::Int(21),
                        Value::Int(30),
                        Value::Int(31),
                    ])
                ),
                ("flatMapEmpty", array_value(vec![])),
                ("flatMapToEmpty", array_value(vec![])),
                ("filter", array_value(vec![Value::Int(20), Value::Int(30)])),
                ("filterEmpty", array_value(vec![])),
                (
                    "reverse",
                    array_value(vec![Value::Int(30), Value::Int(20), Value::Int(10)])
                ),
                ("reverseEmpty", array_value(vec![])),
                (
                    "concat",
                    array_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
                ),
                (
                    "concatLeftEmpty",
                    array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
                ),
                (
                    "concatRightEmpty",
                    array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
                ),
                ("composed", array_value(vec![Value::Int(2), Value::Int(3)])),
                ("take2", array_value(vec![Value::Int(10), Value::Int(20)])),
                ("take0", array_value(vec![])),
                ("takeNeg", array_value(vec![])),
                (
                    "takeBig",
                    array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
                ),
                ("takeEmpty", array_value(vec![])),
                ("drop2", array_value(vec![Value::Int(30)])),
                (
                    "drop0",
                    array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
                ),
                (
                    "dropNeg",
                    array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
                ),
                ("dropBig", array_value(vec![])),
                ("dropEmpty", array_value(vec![])),
                ("slice", array_value(vec![Value::Int(20), Value::Int(30)])),
                ("sliceEmpty", array_value(vec![])),
                (
                    "sliceClampLow",
                    array_value(vec![Value::Int(10), Value::Int(20)])
                ),
                (
                    "slicePastEnd",
                    array_value(vec![Value::Int(20), Value::Int(30)])
                ),
                // wrap end -1 → 2, half-open [0, 2) → all but last
                (
                    "sliceNegEnd",
                    array_value(vec![Value::Int(10), Value::Int(20)])
                ),
                // wrap start -2 → 1; clamp end 99 → 3 → last two
                (
                    "sliceNegStart",
                    array_value(vec![Value::Int(20), Value::Int(30)])
                ),
                // wrap start -99 → still < 0 → clamp 0; end 2
                (
                    "sliceNegStartFar",
                    array_value(vec![Value::Int(10), Value::Int(20)])
                ),
                ("sliceInverted", array_value(vec![])),
                // wrap -1 → 2, -3 → 0 → inverted after wrap
                ("sliceNegInverted", array_value(vec![])),
                ("sliceEmptyArr", array_value(vec![])),
                ("sliceEmptyNeg", array_value(vec![])),
                (
                    "zipShort",
                    array_value(vec![
                        tuple_value(vec![Value::Int(1), Value::Int(10)]),
                        tuple_value(vec![Value::Int(2), Value::Int(20)]),
                    ])
                ),
                ("zipLeftEmpty", array_value(vec![])),
                ("zipRightEmpty", array_value(vec![])),
                (
                    "flatten",
                    array_value(vec![
                        Value::Int(1),
                        Value::Int(2),
                        Value::Int(3),
                        Value::Int(4)
                    ])
                ),
                ("flattenEmpty", array_value(vec![])),
                (
                    "range",
                    array_value(vec![
                        Value::Int(1),
                        Value::Int(2),
                        Value::Int(3),
                        Value::Int(4)
                    ])
                ),
                ("rangeEmpty", array_value(vec![])),
                ("rangeRev", array_value(vec![])),
                (
                    "sort",
                    array_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
                ),
                ("sortEmpty", array_value(vec![])),
                (
                    "sortStable",
                    array_value(vec![
                        record_value(vec![("k", Value::Int(1)), ("id", Value::Int(2))]),
                        record_value(vec![("k", Value::Int(2)), ("id", Value::Int(1))]),
                        record_value(vec![("k", Value::Int(2)), ("id", Value::Int(3))]),
                    ])
                ),
                (
                    "sortByAge",
                    array_value(vec![
                        record_value(vec![
                            ("name", Value::Text("alice".to_owned())),
                            ("age", Value::Int(25)),
                        ]),
                        record_value(vec![
                            ("name", Value::Text("bob".to_owned())),
                            ("age", Value::Int(30)),
                        ]),
                        record_value(vec![
                            ("name", Value::Text("carol".to_owned())),
                            ("age", Value::Int(30)),
                        ]),
                    ])
                ),
                (
                    "sortByAlready",
                    array_value(vec![
                        record_value(vec![("age", Value::Int(1))]),
                        record_value(vec![("age", Value::Int(2))]),
                    ])
                ),
                ("sortByEmpty", array_value(vec![])),
                (
                    "sortByStable",
                    array_value(vec![
                        record_value(vec![("k", Value::Int(1)), ("id", Value::Int(2))]),
                        record_value(vec![("k", Value::Int(2)), ("id", Value::Int(1))]),
                        record_value(vec![("k", Value::Int(2)), ("id", Value::Int(3))]),
                    ])
                ),
                ("minimum", Value::Int(10)),
                ("minimumEmpty", Value::Undefined),
                ("maximum", Value::Int(30)),
                ("maximumEmpty", Value::Undefined),
            ])),
            diagnostics: Vec::new()
        }
    );
}

#[test]
fn std_result_combinators_run_via_import() {
    let result_source = include_str!("../../aven-host/std/result.av");
    let result_module = parse_ok(result_source);
    let result_export = eval_module(&result_module)
        .value
        .expect("std/result should export a record");

    let imports = ModuleImports::new([("std/result".to_owned(), result_export)]);
    let source = concat!(
        "{ map, andThen, unwrapOr, isOk, isErr } = import(\"std/result\")\n",
        "ok : Result(Int, Text) = @Ok(7)\n",
        "err : Result(Int, Text) = @Err(\"boom\")\n",
        "zero: Int = 0\n",
        "{\n",
        "  mapOk: map(ok, (v) => v + 1),\n",
        "  mapErr: map(err, (v) => v + 1),\n",
        "  andThenOk: andThen(ok, (v) => @Ok(v + 1)),\n",
        "  andThenErr: andThen(err, (v) => @Ok(v + 1)),\n",
        "  unwrapOk: unwrapOr(ok, zero),\n",
        "  unwrapErr: unwrapOr(err, zero),\n",
        "  isOkOk: isOk(ok),\n",
        "  isOkErr: isOk(err),\n",
        "  isErrOk: isErr(ok),\n",
        "  isErrErr: isErr(err),\n",
        "}\n",
    );
    let module = parse_ok(source);
    let outcome = eval_module_with_globals_and_imports(&module, Vec::new(), &imports);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(record_value(vec![
                (
                    "mapOk",
                    Value::Tag {
                        name: "Ok".to_owned(),
                        payload: vec![Value::Int(8)],
                    }
                ),
                (
                    "mapErr",
                    Value::Tag {
                        name: "Err".to_owned(),
                        payload: vec![Value::Text("boom".to_owned())],
                    }
                ),
                (
                    "andThenOk",
                    Value::Tag {
                        name: "Ok".to_owned(),
                        payload: vec![Value::Int(8)],
                    }
                ),
                (
                    "andThenErr",
                    Value::Tag {
                        name: "Err".to_owned(),
                        payload: vec![Value::Text("boom".to_owned())],
                    }
                ),
                ("unwrapOk", Value::Int(7)),
                ("unwrapErr", Value::Int(0)),
                ("isOkOk", Value::Bool(true)),
                ("isOkErr", Value::Bool(false)),
                ("isErrOk", Value::Bool(false)),
                ("isErrErr", Value::Bool(true)),
            ])),
            diagnostics: Vec::new()
        }
    );
}

#[test]
fn user_binding_shadows_map_builtin() {
    assert_module_value("Map = 5\nMap\n", Value::Int(5));
}

#[test]
fn std_map_helpers_run_via_import() {
    let array_module = parse_ok(include_str!("../../aven-host/std/array.av"));
    let builtin_methods = BuiltinMethodEnvironment::default();
    let array_export = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &array_module,
        Vec::new(),
        &ModuleImports::default(),
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        true,
    )
    .value
    .expect("std/array should export a record");
    let map_module = parse_ok(include_str!("../../aven-host/std/map.av"));
    let map_export = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &map_module,
        Vec::new(),
        &ModuleImports::default(),
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        false,
    )
    .value
    .expect("std/map should export a record");
    let imports = ModuleImports::new([
        ("std/map".to_owned(), map_export),
        ("std/array".to_owned(), array_export),
    ]);
    let module = parse_ok(
        "{ getOr, update, fromEntries, toEntries, mapValues, filter } = import(\"std/map\")\n\
         entries = [(\"one\", 1), (\"two\", 2), (\"one\", 3)]\n\
         from = fromEntries(entries)\n\
         { duplicate: getOr(from, \"one\", 0), missing: getOr(from, \"missing\", 99), updated: toEntries(update(from, \"two\", (n) => n + 10)), unchanged: toEntries(update(from, \"missing\", (n) => n + 10)), mapped: toEntries(mapValues(from, (n) => n + 1)), filtered: toEntries(filter(from, (key, _) => key == \"two\")) }\n",
    );
    let outcome = eval_module_with_globals_imports_runtime_types_and_builtin_methods(
        &module,
        Vec::new(),
        &imports,
        &RuntimeTypeBindings::default(),
        &builtin_methods,
        false,
    );
    assert_eq!(outcome.diagnostics, Vec::new());
    assert_eq!(
        outcome.value,
        Some(record_value(vec![
            ("duplicate", Value::Int(3)),
            ("missing", Value::Int(99)),
            (
                "updated",
                array_value(vec![
                    tuple_value(vec![Value::Text("one".to_owned()), Value::Int(3)]),
                    tuple_value(vec![Value::Text("two".to_owned()), Value::Int(12)])
                ])
            ),
            (
                "unchanged",
                array_value(vec![
                    tuple_value(vec![Value::Text("one".to_owned()), Value::Int(3)]),
                    tuple_value(vec![Value::Text("two".to_owned()), Value::Int(2)])
                ])
            ),
            (
                "mapped",
                array_value(vec![
                    tuple_value(vec![Value::Text("one".to_owned()), Value::Int(4)]),
                    tuple_value(vec![Value::Text("two".to_owned()), Value::Int(3)])
                ])
            ),
            (
                "filtered",
                array_value(vec![tuple_value(vec![
                    Value::Text("two".to_owned()),
                    Value::Int(2)
                ])])
            ),
        ]))
    );
}

#[test]
fn set_and_array_has_report_membership() {
    assert_eval("@{\"name\", \"email\"}.has(\"name\")", Value::Bool(true));
    assert_eval("@{\"name\", \"email\"}.has(\"age\")", Value::Bool(false));
    assert_eval("[1, 2, 3].has(2)", Value::Bool(true));
    assert_eval("[1, 2, 3].has(4)", Value::Bool(false));
}

#[test]
fn array_spread_splices_elements_in_order() {
    assert_module_value(
        "xs = [1, 2]\nys = [0, ..xs, 3]\nys\n",
        array_value(vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
        ]),
    );
    assert_module_value(
        "xs = [1]\nys = [2, 3]\nzs = [..xs, 0, ..ys]\nzs\n",
        array_value(vec![
            Value::Int(1),
            Value::Int(0),
            Value::Int(2),
            Value::Int(3),
        ]),
    );
    assert_module_value(
        "empty = []\nys = [..empty, 1]\nys\n",
        array_value(vec![Value::Int(1)]),
    );
}

#[test]
fn array_push_returns_new_array_without_mutating_receiver() {
    assert_module_value(
        "xs = [1]\nys = xs.push(2)\n[xs, ys]\n",
        array_value(vec![
            array_value(vec![Value::Int(1)]),
            array_value(vec![Value::Int(1), Value::Int(2)]),
        ]),
    );
}

#[test]
fn text_methods_predicates_and_case() {
    assert_module_value(
        "t = \"Hello\"\n\
         [t.isEmpty(), \"\".isEmpty(), t.contains(\"ell\"), t.startsWith(\"He\"), t.endsWith(\"lo\"), \
          t.toLower(), t.toUpper()]\n",
        array_value(vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Text("hello".to_owned()),
            Value::Text("HELLO".to_owned()),
        ]),
    );
}

#[test]
fn text_methods_trim_replace_and_drop_affix() {
    assert_module_value(
        "t = \"  ababa  \"\n\
         [t.trim(), t.trimStart(), t.trimEnd(), \
          t.replaceEach(\"a\", \"x\"), t.replaceFirst(\"a\", \"x\"), \
          \"prefix\".dropPrefix(\"pre\"), \"prefix\".dropPrefix(\"no\"), \
          \"suffix\".dropSuffix(\"fix\"), \"suffix\".dropSuffix(\"no\")]\n",
        array_value(vec![
            Value::Text("ababa".to_owned()),
            Value::Text("ababa  ".to_owned()),
            Value::Text("  ababa".to_owned()),
            Value::Text("  xbxbx  ".to_owned()),
            Value::Text("  xbaba  ".to_owned()),
            Value::Text("fix".to_owned()),
            Value::Text("prefix".to_owned()),
            Value::Text("suf".to_owned()),
            Value::Text("suffix".to_owned()),
        ]),
    );
}

#[test]
fn text_methods_repeat_split_and_join() {
    // repeat: n <= 0 → empty; splitOn empty string / no match still ≥1 element;
    // empty separator returns the original wrapped in a one-element list (Roc).
    assert_module_value(
        "[\
           \"ab\".repeat(3), \"ab\".repeat(0), \"ab\".repeat(-2), \
           \"a,b,c\".splitOn(\",\"), \"alone\".splitOn(\",\"), \"\".splitOn(\",\"), \
           \"keep\".splitOn(\"\"), \
           [\"a\", \"b\", \"c\"].joinWith(\", \"), [].joinWith(\",\")\
         ]\n",
        array_value(vec![
            Value::Text("ababab".to_owned()),
            Value::Text(String::new()),
            Value::Text(String::new()),
            array_value(vec![
                Value::Text("a".to_owned()),
                Value::Text("b".to_owned()),
                Value::Text("c".to_owned()),
            ]),
            array_value(vec![Value::Text("alone".to_owned())]),
            array_value(vec![Value::Text(String::new())]),
            array_value(vec![Value::Text("keep".to_owned())]),
            Value::Text("a, b, c".to_owned()),
            Value::Text(String::new()),
        ]),
    );
}

#[test]
fn text_pad_left_and_right() {
    // short / exact / long text; multi-char pad truncation; empty pad unchanged.
    assert_module_value(
        "[\
           \"7\".padLeft(3, \"0\"), \"hi\".padLeft(2, \"0\"), \"hello\".padLeft(3, \"0\"), \
           \"7\".padRight(3, \"0\"), \"xy\".padLeft(5, \"ab\"), \"xy\".padRight(5, \"ab\"), \
           \"x\".padLeft(3, \"\"), \"x\".padRight(3, \"\")\
         ]\n",
        array_value(vec![
            Value::Text("007".to_owned()),
            Value::Text("hi".to_owned()),
            Value::Text("hello".to_owned()),
            Value::Text("700".to_owned()),
            Value::Text("abaxy".to_owned()),
            Value::Text("xyaba".to_owned()),
            Value::Text("x".to_owned()),
            Value::Text("x".to_owned()),
        ]),
    );
}

#[test]
fn int_to_grouped_digits() {
    assert_module_value(
        "[\
           0.toGrouped(\",\"), 999.toGrouped(\",\"), 1000.toGrouped(\",\"), \
           (-1234).toGrouped(\",\"), 1000000.toGrouped(\",\"), 1000000.toGrouped(\"\"), \
           1000000.toGrouped(\" \")\
         ]\n",
        array_value(vec![
            Value::Text("0".to_owned()),
            Value::Text("999".to_owned()),
            Value::Text("1,000".to_owned()),
            Value::Text("-1,234".to_owned()),
            Value::Text("1,000,000".to_owned()),
            Value::Text("1000000".to_owned()),
            Value::Text("1 000 000".to_owned()),
        ]),
    );
}

#[test]
fn float_to_fixed_rounding_and_non_finite() {
    // Half away from zero on the shortest decimal; decimals 0; NaN/Infinity words.
    assert_module_value(
        "[\
           3.14159.toFixed(2), 2.0.toFixed(2), 2.675.toFixed(2), \
           (-1.005).toFixed(2), 3.14159.toFixed(0), 2.5.toFixed(0), \
           (0.0 / 0.0).toFixed(2), (1.0 / 0.0).toFixed(2), (-1.0 / 0.0).toFixed(2), \
           1.234.toFixed(-1)\
         ]\n",
        array_value(vec![
            Value::Text("3.14".to_owned()),
            Value::Text("2.00".to_owned()),
            Value::Text("2.68".to_owned()),
            Value::Text("-1.01".to_owned()),
            Value::Text("3".to_owned()),
            Value::Text("3".to_owned()),
            Value::Text("NaN".to_owned()),
            Value::Text("Infinity".to_owned()),
            Value::Text("-Infinity".to_owned()),
            Value::Text("1".to_owned()),
        ]),
    );
}

#[test]
fn money_style_composition_via_int_helpers() {
    // Building-block composition without named-family branding (branding +
    // inherited methods need the checker plan; full Money e2e lives in
    // aven-compiler). Cents → dollars/remainder with grouping and pad.
    assert_module_value(
        concat!(
            "cents = 100000\n",
            "\"$${(cents / 100).toGrouped(\",\")}.${(cents % 100).toText().padLeft(2, \"0\")}\"\n",
        ),
        Value::Text("$1,000.00".to_owned()),
    );
}

#[test]
fn text_reverse_index_of_slice_capitalize() {
    // reverse: scalar-order reverse; empty stays empty.
    // indexOf: char-offset; missing → undefined.
    // slice: clamp into [0,len]; start>end → empty; no negative indexing.
    // capitalize: first scalar uppercased; empty unchanged.
    assert_module_value(
        "[\
           \"abc\".reverse(), \"\".reverse(), \"a☕b\".reverse(), \
           \"hello\".indexOf(\"ll\") ?? -1, \"hello\".indexOf(\"x\") ?? -1, \
           \"a☕b\".indexOf(\"☕\") ?? -1, \"a☕b\".indexOf(\"b\") ?? -1, \
           \"hello\".slice(1, 4), \"hello\".slice(-2, 99), \"hello\".slice(3, 1), \
           \"\".slice(0, 1), \"hello\".slice(0, 0), \
           \"hello\".capitalize(), \"\".capitalize(), \"école\".capitalize()\
         ]\n",
        array_value(vec![
            Value::Text("cba".to_owned()),
            Value::Text(String::new()),
            Value::Text("b☕a".to_owned()),
            Value::Int(2),
            Value::Int(-1),
            Value::Int(1),
            Value::Int(2),
            Value::Text("ell".to_owned()),
            Value::Text("hello".to_owned()),
            Value::Text(String::new()),
            Value::Text(String::new()),
            Value::Text(String::new()),
            Value::Text("Hello".to_owned()),
            Value::Text(String::new()),
            Value::Text("École".to_owned()),
        ]),
    );

    assert_module_value("\"missing\".indexOf(\"z\")\n", Value::Undefined);
}

#[test]
fn int_numeric_helpers() {
    assert_module_value(
        "[\
           (-5).abs(), 0.abs(), 5.abs(), \
           3.min(5), 5.min(3), (-1).min(-3), \
           3.max(5), 5.max(3), (-1).max(-3), \
           10.clamp(0, 5), (-2).clamp(0, 5), 3.clamp(0, 5), \
           3.clamp(5, 0), \
           2.pow(10), 2.pow(0), 2.pow(-3), 0.pow(0), \
           (-3).sign(), 0.sign(), 7.sign(), \
           42.toFloat()\
         ]\n",
        array_value(vec![
            Value::Int(5),
            Value::Int(0),
            Value::Int(5),
            Value::Int(3),
            Value::Int(3),
            Value::Int(-3),
            Value::Int(5),
            Value::Int(5),
            Value::Int(-1),
            Value::Int(5),
            Value::Int(0),
            Value::Int(3),
            Value::Int(5), // min > max → return min
            Value::Int(1024),
            Value::Int(1),
            Value::Int(1), // negative exponent clamps to 0 → 1
            Value::Int(1),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(1),
            Value::Float(42.0),
        ]),
    );
}

#[test]
fn float_numeric_helpers() {
    assert_module_value(
        "[\
           (-1.5).abs(), 0.0.abs(), \
           1.0.min(2.0), 2.0.min(1.0), \
           1.0.max(2.0), 2.0.max(1.0), \
           3.5.clamp(0.0, 1.0), (-2.0).clamp(0.0, 1.0), 0.5.clamp(0.0, 1.0), \
           0.5.clamp(2.0, 1.0), \
           2.0.pow(3.0), \
           1.5.round(), 1.5.floor(), 1.5.ceil(), 1.5.truncate(), \
           (-1.5).truncate(), 4.0.sqrt()\
         ]\n",
        array_value(vec![
            Value::Float(1.5),
            Value::Float(0.0),
            Value::Float(1.0),
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(2.0),
            Value::Float(1.0),
            Value::Float(0.0),
            Value::Float(0.5),
            Value::Float(2.0), // min > max → return min
            Value::Float(8.0),
            Value::Float(2.0),
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(1.0),
            Value::Float(-1.0),
            Value::Float(2.0),
        ]),
    );

    // NaN handling: f64::min/max propagate the non-NaN operand.
    let nan_min = eval_module(&parse_ok("(0.0 / 0.0).min(1.0)\n"));
    let Some(Value::Float(nan_min_v)) = nan_min.value else {
        panic!("expected Float from NaN.min, got {nan_min:?}");
    };
    assert!(nan_min.diagnostics.is_empty());
    assert!((nan_min_v - 1.0).abs() < f64::EPSILON);

    let nan_max = eval_module(&parse_ok("1.0.max(0.0 / 0.0)\n"));
    let Some(Value::Float(nan_max_v)) = nan_max.value else {
        panic!("expected Float from max(NaN), got {nan_max:?}");
    };
    assert!(nan_max.diagnostics.is_empty());
    assert!((nan_max_v - 1.0).abs() < f64::EPSILON);

    // Negative sqrt → NaN.
    let neg_sqrt = eval_module(&parse_ok("(-1.0).sqrt()\n"));
    let Some(Value::Float(neg_sqrt_v)) = neg_sqrt.value else {
        panic!("expected Float from negative sqrt, got {neg_sqrt:?}");
    };
    assert!(neg_sqrt.diagnostics.is_empty());
    assert!(neg_sqrt_v.is_nan());
}

#[test]
fn text_methods_parse_numbers() {
    assert_module_value(
        concat!(
            "[\"42\".toInt() ?? 0, \"-7\".toInt() ?? 0, \"+3\".toInt() ?? 0, ",
            "\"2.5\".toFloat() ?? 0.0, \"1e3\".toFloat() ?? 0.0]\n",
        ),
        array_value(vec![
            Value::Int(42),
            Value::Int(-7),
            Value::Int(3),
            Value::Float(2.5),
            Value::Float(1000.0),
        ]),
    );

    for source in [
        "\"\".toInt()\n",
        "\"abc\".toInt()\n",
        "\"1.5\".toInt()\n",
        "\" 42 \".toInt()\n",
        "\"9223372036854775808\".toInt()\n",
        "\"\".toFloat()\n",
        "\"abc\".toFloat()\n",
        "\" 42 \".toFloat()\n",
    ] {
        assert_module_value(source, Value::Undefined);
    }

    assert_module_value("\"not a number\".toFloat() ?? 1.5\n", Value::Float(1.5));

    for source in ["\"nan\".toFloat()\n", "\"inf\".toFloat()\n"] {
        let outcome = eval_module(&parse_ok(source));
        let Some(Value::Float(value)) = outcome.value else {
            panic!("expected Float from {source:?}, got {outcome:?}");
        };
        assert!(outcome.diagnostics.is_empty());
        assert!(value.is_nan() || value.is_infinite());
    }
}

#[test]
fn has_on_unsupported_receiver_still_reports_type_error() {
    let diagnostic = eval_error("1.has(1)");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
}

#[test]
fn evaluates_array_literals_and_indexing() {
    assert_eval(
        "[10, 20, 30]",
        array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)]),
    );
    assert_module_value("xs = [10, 20, 30]\nxs[1]\n", Value::Int(20));
    assert_module_value("xs = [10, 20, 30]\nxs[9]\n", Value::Undefined);
    // Negative indexes wrap from the end (Python-style).
    assert_module_value("xs = [10, 20, 30]\nxs[-1]\n", Value::Int(30));
    assert_module_value("xs = [10, 20, 30]\nxs[-3]\n", Value::Int(10));
    // Beyond the start after wrap → undefined (same as past-the-end positive).
    assert_module_value("xs = [10, 20, 30]\nxs[-4]\n", Value::Undefined);
    assert_module_value("xs = []\nxs[-1]\n", Value::Undefined);
    assert_eq!(
        format!(
            "{}",
            array_value(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
        ),
        "[10, 20, 30]"
    );
}

#[test]
fn evaluates_tuple_literals_and_indexing() {
    assert_eval(
        "(1, \"a\")",
        tuple_value(vec![Value::Int(1), Value::Text("a".to_owned())]),
    );
    assert_eval("(1, \"a\")[0]", Value::Int(1));
    assert_eq!(
        format!(
            "{}",
            tuple_value(vec![Value::Int(1), Value::Text("a".to_owned())])
        ),
        "(1, \"a\")"
    );
}

#[test]
fn reports_tuple_index_out_of_bounds() {
    let diagnostic = eval_error("(1, \"a\")[2]");

    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::INDEX_OUT_OF_BOUNDS)
    );
}

#[test]
fn evaluates_empty_tuple_as_unit() {
    assert_eval("()", tuple_value(Vec::new()));
    assert_eq!(format!("{}", tuple_value(Vec::new())), "()");
}

#[test]
fn evaluates_set_literals_with_deduplication() {
    assert_eval(
        "@{ 1, 2, 2, 3 }",
        set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
    );
    assert_eval("@{ 1, 2, 3 } == @{ 3, 2, 1 }", Value::Bool(true));
    assert_eq!(
        format!(
            "{}",
            set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        ),
        "@{ 1, 2, 3 }"
    );
}

#[test]
fn evaluates_set_spread_entries_with_deduplication() {
    assert_module_value(
        "a = @{ 1, 2 }\nb = @{ 2, 3 }\n@{ ..a, ..b, 4 }\n",
        set_value(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
        ]),
    );
}

#[test]
fn set_spread_of_non_set_reports_type_error() {
    let diagnostic = module_error("@{ ..[1, 2] }\n");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
    assert_eq!(diagnostic.labels[0].message, "expected Set");
}

#[test]
fn required_type_map_strips_optional_at_runtime() {
    // `!object[k]` on a type value strips the `Optional` wrapper, so
    // `required(partial(T))` bindings evaluate under lenient `aven run`.
    assert_module_value(
        "partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         required = (object) => { keysOf(object) -> k; [k]: !object[k] }\n\
         required(partial({ name: Text }))\n",
        record_value(vec![("name", Value::named_type("Text"))]),
    );
}

#[test]
fn evaluates_set_union_promotes_singletons() {
    assert_eval(
        "\"r\" | \"w\"",
        set_value(vec![
            Value::Text("r".to_owned()),
            Value::Text("w".to_owned()),
        ]),
    );
}

#[test]
fn evaluates_set_union_splices_set_operands() {
    assert_eval(
        "@{ 1, 2 } | 3",
        set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
    );
    assert_eval(
        "@{ 1, 2 } | @{ 2, 3 }",
        set_value(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
    );
}

#[test]
fn evaluates_set_union_deduplicates() {
    assert_eval("1 | 1", set_value(vec![Value::Int(1)]));
}

#[test]
fn evaluates_tuple_patterns() {
    assert_module_value("pair = (1, \"a\")\npair ?>\n  (n, t) => n\n", Value::Int(1));
}

#[test]
fn evaluates_null_safe_field_access() {
    assert_eval("undefined?.name", Value::Undefined);
    assert_eval("null?.name", Value::Null);
    assert_eval("{ name: \"Ada\" }?.name", Value::Text("Ada".to_owned()));
}

#[test]
fn field_access_yields_undefined_for_absent_record_field() {
    // Optional fields may be omitted at construction, leaving no physical key.
    // Both field access forms read that absence as `undefined`; `?.` also
    // guards an empty receiver.
    assert_eval("{ name: \"Ada\" }.phone", Value::Undefined);
    assert_eval("{ name: \"Ada\" }?.phone", Value::Undefined);
    assert_module_value(
        "user = { name: \"Ada\" }\nuser.phone ?? \"none\"\n",
        Value::Text("none".to_owned()),
    );
    assert_module_value(
        "user = { name: \"Ada\", phone: \"555\" }\nuser.phone ?? \"none\"\n",
        Value::Text("555".to_owned()),
    );
}

#[test]
fn null_safe_field_access_propagates_empty_receiver_through_variable() {
    assert_module_value("u = undefined\nu?.phone\n", Value::Undefined);
    assert_module_value("n = null\nn?.phone\n", Value::Null);
}

#[test]
fn record_patterns_and_type_statics_still_error_on_absent_fields() {
    for source in [
        "source = { name: \"Ada\" }\n{ email: address } = source\n",
        "source = { name: \"Ada\" }\n{ email -> address } = source\n",
    ] {
        let diagnostic = module_error(source);
        assert_eq!(
            diagnostic.code.as_deref(),
            Some(codes::runtime::MISSING_FIELD),
            "{source}"
        );
    }

    let diagnostic = module_error_with_globals(
        "Map.nope\n",
        vec![("Map".to_owned(), Value::named_type("Map"))],
    );
    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::MISSING_FIELD)
    );
}

#[test]
fn evaluates_null_coalescing_with_short_circuiting() {
    assert_eval("undefined ?? 5", Value::Int(5));
    assert_eval("null ?? 6", Value::Int(6));
    assert_eval("7 ?? 1 / 0", Value::Int(7));
}

#[test]
fn evaluates_variant_tags() {
    assert_eval(
        "@Ok(1)",
        Value::Tag {
            name: "Ok".to_owned(),
            payload: vec![Value::Int(1)],
        },
    );
    assert_eval(
        "@Red",
        Value::Tag {
            name: "Red".to_owned(),
            payload: Vec::new(),
        },
    );
}

#[test]
fn evaluates_variant_tags_with_multiple_payload_args() {
    assert_eval(
        "@Rgb(1, 2, 3)",
        Value::Tag {
            name: "Rgb".to_owned(),
            payload: vec![Value::Int(1), Value::Int(2), Value::Int(3)],
        },
    );
}

#[test]
fn evaluates_literal_union_match() {
    assert_eval(
        "1 ?>\n  0 => \"zero\"\n  1 => \"one\"\n  _ => \"many\"\n",
        Value::Text("one".to_owned()),
    );
}

#[test]
fn evaluates_literal_or_pattern_first_alternative() {
    assert_eval("\"r\" ?>\n  \"r\" | \"w\" => 1\n  _ => 0\n", Value::Int(1));
}

#[test]
fn evaluates_literal_or_pattern_second_alternative() {
    assert_eval("\"w\" ?>\n  \"r\" | \"w\" => 1\n  _ => 0\n", Value::Int(1));
}

#[test]
fn evaluates_tag_or_pattern() {
    assert_eval(
        "@Green ?>\n  @Red | @Green => 1\n  @Blue => 0\n",
        Value::Int(1),
    );
}

#[test]
fn evaluates_default_match_arm() {
    assert_eval(
        "2 ?>\n  0 => \"zero\"\n  1 => \"one\"\n  _ => \"many\"\n",
        Value::Text("many".to_owned()),
    );
}

#[test]
fn evaluates_variant_match_payload_bindings() {
    assert_module_value(
        "result = @Ok(41)\nresult ?>\n  @Ok(x) => x + 1\n  @Err(error) => error\n",
        Value::Int(42),
    );
}

#[test]
fn evaluates_guarded_match_arms() {
    assert_eval(
        "1 ?>\n  n, n > 0 => \"pos\"\n  _ => \"other\"\n",
        Value::Text("pos".to_owned()),
    );
    assert_eval(
        "-1 ?>\n  n, n > 0 => \"pos\"\n  _ => \"other\"\n",
        Value::Text("other".to_owned()),
    );
}

#[test]
fn variable_patterns_do_not_match_undefined() {
    assert_eval(
        "undefined ?>\n  value => value\n  undefined => \"empty\"\n",
        Value::Text("empty".to_owned()),
    );
}

#[test]
fn evaluates_record_patterns() {
    assert_module_value(
        "user = { name: \"Ada\", age: 36 }\nuser ?>\n  { name } => name\n",
        Value::Text("Ada".to_owned()),
    );
}

#[test]
fn reports_match_without_matching_arm() {
    let diagnostic = eval_error("2 ?>\n  0 => \"zero\"\n");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::NO_MATCH));
}

#[test]
fn evaluates_recursive_factorial_with_match_base_case() {
    assert_module_value(
        "fact = (n) =>\n  n ?>\n    0 => 1\n    _ => n * fact(n - 1)\nfact(5)\n",
        Value::Int(120),
    );
}

#[test]
fn evaluates_mutually_recursive_functions_with_match_base_cases() {
    assert_module_value(
        "isEven = (n) =>\n  n ?>\n    0 => true\n    _ => isOdd(n - 1)\nisOdd = (n) =>\n  n ?>\n    0 => false\n    _ => isEven(n - 1)\nisEven(6)\n",
        Value::Bool(true),
    );
}

#[test]
fn reports_field_access_on_non_record() {
    let diagnostic = eval_error("1.name");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
}

#[test]
fn primitive_type_name_evaluates_to_type_value() {
    assert_module_value("Text\n", Value::named_type("Text"));
    assert_eq!(format!("{}", Value::named_type("Text")), "Text");
}

#[test]
fn record_of_types_evaluates_and_displays_as_type_record() {
    let expected = record_value(vec![
        ("name", Value::named_type("Text")),
        ("age", Value::named_type("Int")),
    ]);
    assert_module_value("{ name: Text, age: Int }\n", expected.clone());
    assert_eq!(format!("{expected}"), "{ name: Text, age: Int }");
}

#[test]
fn type_alias_binding_yields_record_of_types_and_keysof() {
    assert_module_value(
        "User = { name: Text, email: Text }\nUser\n",
        record_value(vec![
            ("name", Value::named_type("Text")),
            ("email", Value::named_type("Text")),
        ]),
    );
    assert_module_value(
        "User = { name: Text, email: Text }\nkeysOf(User)\n",
        set_value(vec![
            Value::Text("name".to_owned()),
            Value::Text("email".to_owned()),
        ]),
    );
}

#[test]
fn type_values_compare_by_name() {
    assert_module_value("Text == Text\n", Value::Bool(true));
    assert_module_value("Text == Int\n", Value::Bool(false));
}

#[test]
fn composite_type_expressions_evaluate_to_type_values() {
    assert_module_value(
        "?Text\n",
        Value::Type(super::RuntimeType::Optional(Box::new(Value::named_type(
            "Text",
        )))),
    );
    assert_module_value(
        "Text?\n",
        Value::Type(super::RuntimeType::Nullable(Box::new(Value::named_type(
            "Text",
        )))),
    );
    assert_module_value(
        "Array({ name: Text })\n",
        Value::Type(super::RuntimeType::Array(Box::new(record_value(vec![(
            "name",
            Value::named_type("Text"),
        )])))),
    );
}

#[test]
fn user_binding_shadows_primitive_type_name() {
    assert_module_value("Text = 5\nText\n", Value::Int(5));
}

#[test]
fn propagate_unwraps_ok_payload() {
    assert_eval("@Ok(7)?^", Value::Int(7));
}

#[test]
fn result_methods_map_errors_and_recover_for_ok_and_err() {
    assert_module_value(
        "ok = @Ok(7)\nerr = @Err(\"bad\")\n[ok.mapErr((e) => \"wrapped: ${e}\"), err.mapErr((e) => \"wrapped: ${e}\"), ok.orElse((_) => @Ok(0)), err.orElse((_) => @Ok(0))]\n",
        array_value(vec![
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(7)],
            },
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("wrapped: bad".to_owned())],
            },
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(7)],
            },
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(0)],
            },
        ]),
    );
    assert_module_value(
        "parse = (text) => text ?> \"ok\" => @Ok(1), _ => @Err(text)\nparse(\"bad\").mapErr((e) => \"bad instant: ${e}\")?^\n",
        Value::Tag {
            name: "Err".to_owned(),
            payload: vec![Value::Text("bad instant: bad".to_owned())],
        },
    );
}

#[test]
fn optional_to_result_wraps_payload_and_both_empty_values() {
    assert_module_value(
        "[7.toResult(\"e\"), undefined.toResult(\"missing\"), null.toResult(@Missing)]\n",
        array_value(vec![
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(7)],
            },
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("missing".to_owned())],
            },
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Tag {
                    name: "Missing".to_owned(),
                    payload: Vec::new(),
                }],
            },
        ]),
    );

    let diagnostic = eval_error("7.toResult(1 / 0)");
    assert_eq!(
        diagnostic.code.as_deref(),
        Some(codes::runtime::DIVISION_BY_ZERO),
        "toResult must evaluate its error argument even for a present receiver"
    );
}

#[test]
fn result_methods_map_unwrap_or_and_predicates() {
    assert_module_value(
        "ok = @Ok(7)\nerr = @Err(\"bad\")\n[\n  ok.map((v) => v + 1),\n  err.map((v) => v + 1),\n  ok.unwrapOr(0),\n  err.unwrapOr(0),\n  ok.isOk(),\n  err.isOk(),\n  ok.isErr(),\n  err.isErr()\n]\n",
        array_value(vec![
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(8)],
            },
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("bad".to_owned())],
            },
            Value::Int(7),
            Value::Int(0),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
        ]),
    );
}

#[test]
fn result_methods_and_then_for_ok_and_err() {
    assert_module_value(
        "ok = @Ok(7)\nerr = @Err(\"bad\")\n[ok.andThen((v) => @Ok(v + 1)), err.andThen((v) => @Ok(v + 1))]\n",
        array_value(vec![
            Value::Tag {
                name: "Ok".to_owned(),
                payload: vec![Value::Int(8)],
            },
            Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("bad".to_owned())],
            },
        ]),
    );
}

#[test]
fn propagate_err_early_returns_enclosing_function() {
    // `?^` on `@Err` returns that whole `@Err` as the function's value, and
    // short-circuits: the unbound `missing` after it must never evaluate.
    assert_module_value(
        "f = (r) =>\n  x = r?^\n  missing\nf(@Err(\"boom\"))\n",
        Value::Tag {
            name: "Err".to_owned(),
            payload: vec![Value::Text("boom".to_owned())],
        },
    );
}

#[test]
fn propagate_ok_threads_value_through_function() {
    assert_module_value(
        "f = (r) =>\n  x = r?^\n  x + 1\nf(@Ok(41))\n",
        Value::Int(42),
    );
}

#[test]
fn top_level_propagate_err_becomes_program_value_and_stops() {
    // The `@Err` becomes the program value; the unbound `missing` after it
    // must not run.
    let module = parse_ok("@Err(\"top\")?^\nmissing\n");
    let outcome = eval_module(&module);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(Value::Tag {
                name: "Err".to_owned(),
                payload: vec![Value::Text("top".to_owned())],
            }),
            diagnostics: Vec::new(),
        }
    );
}

#[test]
fn propagate_through_binding_block_bubbles_to_enclosing_function() {
    // A `?^` inside a binding-value block must early-return the function, not
    // make `x` the `@Err` and continue.
    assert_module_value(
        "f = (r) =>\n  x =\n    a = r?^\n    a + 1\n  x + 100\nf(@Err(\"inner\"))\n",
        Value::Tag {
            name: "Err".to_owned(),
            payload: vec![Value::Text("inner".to_owned())],
        },
    );
}

#[test]
fn propagate_on_non_result_reports_type_error() {
    let diagnostic = eval_error("5?^");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
}

#[test]
fn panic_unwraps_ok_payload() {
    assert_eval("@Ok(9)?!", Value::Int(9));
}

#[test]
fn panic_on_err_reports_runtime_panic_with_payload() {
    let diagnostic = eval_error("@Err(\"kaboom\")?!");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::PANIC));
    assert!(
        diagnostic.message.contains("kaboom"),
        "panic message should embed the @Err payload, got {:?}",
        diagnostic.message
    );
}

#[test]
fn panic_on_non_result_reports_type_error() {
    let diagnostic = eval_error("5?!");

    assert_eq!(diagnostic.code.as_deref(), Some(codes::runtime::TYPE_ERROR));
}

#[test]
fn chained_propagation_returns_ok_on_happy_path_and_first_err_on_sad_path() {
    let program = "parse = (n) =>\n  n ?>\n    0 => @Err(\"zero\")\n    _ => @Ok(n)\n\
         chain = (a, b) =>\n  x = parse(a)?^\n  y = parse(b)?^\n  @Ok(x + y)\n";
    assert_module_value(
        &format!("{program}chain(2, 3)\n"),
        Value::Tag {
            name: "Ok".to_owned(),
            payload: vec![Value::Int(5)],
        },
    );
    assert_module_value(
        &format!("{program}chain(0, 3)\n"),
        Value::Tag {
            name: "Err".to_owned(),
            payload: vec![Value::Text("zero".to_owned())],
        },
    );
}

fn assert_module_value(source: &str, expected: Value) {
    let module = parse_ok(source);
    let outcome = eval_module(&module);

    assert_eq!(
        outcome,
        EvalOutcome {
            value: Some(expected),
            diagnostics: Vec::new()
        }
    );
}

fn assert_eval(source: &str, expected: Value) {
    assert_eq!(eval_source(source).expect("evaluation failed"), expected);
}

fn eval_error(source: &str) -> aven_core::Diagnostic {
    eval_source(source).expect_err("expected evaluation error")
}

fn module_error(source: &str) -> aven_core::Diagnostic {
    let module = parse_ok(source);
    let mut diagnostics = eval_module(&module).diagnostics;

    assert_eq!(diagnostics.len(), 1);
    diagnostics.remove(0)
}

fn module_error_with_globals(source: &str, globals: Vec<(String, Value)>) -> aven_core::Diagnostic {
    let module = parse_ok(source);
    let mut diagnostics = eval_module_with_globals(&module, globals).diagnostics;

    assert_eq!(diagnostics.len(), 1);
    diagnostics.remove(0)
}

fn eval_source(source: &str) -> Result<Value, aven_core::Diagnostic> {
    let module = parse_ok(source);
    let Item::Expr(expr) = &module.items[0] else {
        panic!("expected expression item");
    };
    eval_expr(expr, &Environment::new())
}

fn record_value(fields: Vec<(&str, Value)>) -> Value {
    Value::record(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn array_value(values: Vec<Value>) -> Value {
    Value::Array(Rc::new(values))
}

fn tuple_value(values: Vec<Value>) -> Value {
    Value::Tuple(Rc::new(values))
}

fn set_value(values: Vec<Value>) -> Value {
    Value::Set(Rc::new(values))
}

fn map_value(entries: Vec<(Value, Value)>) -> Value {
    Value::Map(Rc::new(entries))
}

fn host_with(name: &str, function: Value) -> Value {
    Value::record(vec![(
        "Native".to_owned(),
        Value::record(vec![(name.to_owned(), function)]),
    )])
}

#[derive(Debug, Clone, PartialEq)]
struct CapturedLogRecord {
    level: logging::Level,
    message: String,
    attributes: Vec<(String, Value)>,
    trace: logging::TraceContext,
}

struct CapturingLogSink {
    records: Rc<RefCell<Vec<CapturedLogRecord>>>,
}

impl logging::LogSink for CapturingLogSink {
    fn emit(&self, record: &logging::LogRecord<'_>) {
        self.records.borrow_mut().push(CapturedLogRecord {
            level: record.level,
            message: record.message.clone(),
            attributes: record.attributes.to_vec(),
            trace: record.trace.clone(),
        });
    }
}

fn capturing_logger(records: Rc<RefCell<Vec<CapturedLogRecord>>>) -> Value {
    logging::logger(Rc::new(CapturingLogSink { records }), fixed_trace_context())
}

fn fixed_trace_context() -> logging::TraceContext {
    logging::TraceContext {
        trace_id: "0af7651916cd43dd8448eb211c80319c".to_owned(),
        span_id: "b7ad6b7169203331".to_owned(),
        trace_flags: "01".to_owned(),
        trace_state: "test=state".to_owned(),
    }
}

fn module_expr_span(module: &Module) -> aven_core::Span {
    let Item::Expr(expr) = &module.items[0] else {
        panic!("expected expression item");
    };
    expr.span
}

fn parse_ok(source: &str) -> Module {
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    output.module
}
