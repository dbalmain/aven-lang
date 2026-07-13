use std::collections::HashSet;
use std::rc::Rc;

use crate::checker::comptime_rhs_needs_evaluation;
use crate::*;
use aven_core::{Diagnostic, Severity, Span, codes};
use aven_parser::{ExprKind, Item, Literal, Module, collect_declarations, parse_module};

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

fn binding_value_named<'a>(module: &'a Module, name: &str) -> &'a Expr {
    module
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == name => Some(&binding.value),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected binding for {name}"))
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
    let required = params.len();
    Type::Function {
        params,
        result: Box::new(result),
        required,
    }
}

fn nullable(ty: Type) -> Type {
    Type::Nullable(Box::new(ty))
}

fn optional(ty: Type) -> Type {
    Type::Optional(Box::new(ty))
}

fn field(name: &str, ty: Type) -> RowEntry {
    RowEntry::Field {
        name: name.to_owned(),
        ty,
    }
}

fn tag(name: &str, payload: Vec<Type>) -> RowEntry {
    RowEntry::Tag {
        name: name.to_owned(),
        payload,
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

fn literal_bool(value: bool) -> RowEntry {
    RowEntry::Literal {
        value: Literal::Bool(value),
    }
}

fn variant_type(entries: Vec<RowEntry>, tail: RowTail) -> Type {
    Type::Variant(Row { entries, tail })
}

fn row_label(entry: &RowEntry) -> &str {
    match entry {
        RowEntry::Field { name, .. } | RowEntry::Tag { name, .. } => name,
        RowEntry::Literal { value } => match value {
            Literal::Bool(true) => "true",
            Literal::Bool(false) => "false",
            Literal::Number(value) | Literal::String(value) | Literal::Regex(value) => value,
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

fn render_top_level_value(checker: &mut Checker<'_>, name: &str) -> Option<String> {
    checker
        .infer_top_level_value(name)
        .map(|ty| crate::ty::display_inferred_type(&ty).render())
}

fn checked_binding_type(source: &str, name: &str, host: &HostGlobals) -> Type {
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let checked = check_module_with_host_globals(&output.module, host);
    assert!(
        checked.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        checked.diagnostics
    );
    let span = nth_span(source, name, 0);
    checked
        .type_at(span)
        .unwrap_or_else(|| panic!("{name} has an inferred type"))
        .clone()
}

fn format_encode_host_globals() -> HostGlobals {
    HostGlobals::default()
        .with_type_definitions(vec![
            ("Data".to_owned(), dynamic_data_type()),
            (
                "YamlError".to_owned(),
                build::variant(vec![("Decode", vec![build::text()])]),
            ),
        ])
        .with_statics(vec![
            (
                "Json".to_owned(),
                vec![(
                    "encode".to_owned(),
                    function(vec![variable("a")], named("Text")),
                )],
            ),
            (
                "Yaml".to_owned(),
                vec![
                    (
                        "decode".to_owned(),
                        function(
                            vec![named("Text")],
                            build::result(named("Data"), named("YamlError")),
                        ),
                    ),
                    (
                        "encode".to_owned(),
                        function(vec![variable("a")], named("Text")),
                    ),
                ],
            ),
        ])
}

fn dynamic_data_type() -> Type {
    build::variant(vec![
        ("Null", vec![]),
        ("Bool", vec![build::bool()]),
        ("Int", vec![build::int()]),
        ("Float", vec![build::float()]),
        ("Text", vec![build::text()]),
        ("Array", vec![build::array(build::named("Data"))]),
        (
            "Object",
            vec![build::map(build::text(), build::named("Data"))],
        ),
    ])
}

struct DecodeResultResolver;

impl HostComptimeFn for DecodeResultResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        let target = args
            .first()
            .and_then(ComptimeArg::as_type)
            .cloned()
            .unwrap_or_else(|| named("Data"));
        Ok(build::result(target, named("JsonError")))
    }
}

fn format_method_host_globals() -> HostGlobals {
    HostGlobals::new(
        Vec::new(),
        vec![(
            "Json.decode".to_owned(),
            HostComptimeFnSpec::new(Rc::new(DecodeResultResolver), vec![1]),
        )],
    )
    .with_type_definitions(vec![
        ("Data".to_owned(), dynamic_data_type()),
        (
            "JsonError".to_owned(),
            build::variant(vec![("Decode", vec![build::text()])]),
        ),
        (
            "YamlError".to_owned(),
            build::variant(vec![("Decode", vec![build::text()])]),
        ),
    ])
    .with_statics(vec![
        (
            "Json".to_owned(),
            vec![
                (
                    "decode".to_owned(),
                    build::function_opt(vec![named("Text")], vec![Type::Deferred], Type::Deferred),
                ),
                (
                    "encode".to_owned(),
                    function(vec![variable("a")], named("Text")),
                ),
            ],
        ),
        (
            "Yaml".to_owned(),
            vec![
                (
                    "decode".to_owned(),
                    function(
                        vec![named("Text")],
                        build::result(named("Data"), named("YamlError")),
                    ),
                ),
                (
                    "encode".to_owned(),
                    function(vec![variable("a")], named("Text")),
                ),
            ],
        ),
    ])
}

#[test]
fn renders_types_as_surface_syntax() {
    assert_eq!(
        Type::Record(Row {
            entries: vec![
                field("name", named("Text")),
                RowEntry::Field {
                    name: "phone".to_owned(),
                    ty: optional(nullable(named("Text"))),
                },
            ],
            tail: RowTail::Open,
        })
        .render(),
        "{ name: Text, phone: ?Text?, .. }"
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
        "@Ok(t) | @Err(e) | @Done | .."
    );
    assert_eq!(
        Type::Variant(Row {
            entries: vec![literal_string("\"waiting\""), literal_string("\"running\"")],
            tail: RowTail::Closed,
        })
        .render(),
        "\"waiting\" | \"running\""
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
        "0 | 1 | 2"
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
        optional(function(vec![named("Int")], named("Text"))).render(),
        "?(Int -> Text)"
    );
    assert_eq!(optional(nullable(named("Text"))).render(), "?Text?");
    assert_eq!(
        apply(named("Result"), vec![named("Int"), variable("e")]).render(),
        "Result(Int, e)"
    );
    assert_eq!(
        Type::Tuple(vec![Type::Meta(10), Type::Meta(10), Type::Deferred]).render(),
        "(a, a, ?)"
    );
}

#[test]
fn renders_variant_types_as_surface_unions() {
    let string_union = || {
        variant_type(
            vec![literal_string("\"a\""), literal_string("\"b\"")],
            RowTail::Closed,
        )
    };

    assert_eq!(
        variant_type(vec![literal_number("2")], RowTail::Closed).render(),
        "2"
    );
    assert_eq!(
        variant_type(vec![literal_string("\"r\"")], RowTail::Closed).render(),
        "\"r\""
    );
    assert_eq!(
        variant_type(
            vec![literal_string("\"r\""), literal_string("\"w\"")],
            RowTail::Closed,
        )
        .render(),
        "\"r\" | \"w\""
    );
    assert_eq!(
        variant_type(
            vec![
                tag("Red", Vec::new()),
                tag("Green", Vec::new()),
                tag("Blue", Vec::new()),
            ],
            RowTail::Closed,
        )
        .render(),
        "@Red | @Green | @Blue"
    );
    assert_eq!(
        variant_type(vec![tag("Ok", vec![named("Int")])], RowTail::Open).render(),
        "@Ok(Int) | .."
    );
    assert_eq!(optional(string_union()).render(), "?(\"a\" | \"b\")");
    assert_eq!(nullable(string_union()).render(), "(\"a\" | \"b\")?");
    assert_eq!(
        function(vec![string_union()], named("Int")).render(),
        "\"a\" | \"b\" -> Int"
    );
    assert_eq!(
        function(vec![named("Int")], string_union()).render(),
        "Int -> \"a\" | \"b\""
    );
    assert_eq!(
        Type::Tuple(vec![string_union(), named("Int")]).render(),
        "(\"a\" | \"b\", Int)"
    );
    assert_eq!(
        Type::Record(Row {
            entries: vec![field("mode", string_union())],
            tail: RowTail::Closed,
        })
        .render(),
        "{ mode: \"a\" | \"b\" }"
    );
    assert_eq!(variant_type(Vec::new(), RowTail::Closed).render(), "@{}");
    assert_eq!(variant_type(Vec::new(), RowTail::Open).render(), "@{ .. }");
}

#[test]
fn record_fields_query_enumerates_record_fields_and_peels_wrappers() {
    let record = Type::Record(Row {
        entries: vec![field("name", named("Text")), field("email", named("Text"))],
        tail: RowTail::Closed,
    });
    let expected = vec![
        RecordField {
            name: "name".to_owned(),
            ty: named("Text"),
        },
        RecordField {
            name: "email".to_owned(),
            ty: named("Text"),
        },
    ];

    assert_eq!(record_fields(&record), Some(expected.clone()));
    assert_eq!(
        record_fields(&optional(record.clone())),
        Some(expected.clone())
    );
    assert_eq!(record_fields(&nullable(record)), Some(expected));
    // Named primitives without methods still yield None; Text carries methods.
    assert_eq!(record_fields(&named("Int")), None);
    let text_fields = record_fields(&named("Text")).expect("Text has methods");
    assert!(
        text_fields.iter().any(|field| field.name == "isEmpty"),
        "Text methods should include isEmpty: {text_fields:?}"
    );
}

#[test]
fn variant_tags_query_enumerates_variant_tags_and_peels_wrappers() {
    let variant = Type::Variant(Row {
        entries: vec![
            tag("Red", Vec::new()),
            tag("Green", vec![named("Text")]),
            literal_string("\"literal\""),
        ],
        tail: RowTail::Closed,
    });
    let expected = vec!["Red".to_owned(), "Green".to_owned()];

    assert_eq!(variant_tags(&variant), Some(expected.clone()));
    assert_eq!(
        variant_tags(&optional(variant.clone())),
        Some(expected.clone())
    );
    assert_eq!(variant_tags(&nullable(variant)), Some(expected));
    assert_eq!(variant_tags(&named("Text")), None);
}

#[test]
fn text_literals_builder_round_trips_literal_union_members() {
    let union = build::text_literals(&["r", "w", "a", "rw"]);
    let expected = vec![
        "\"r\"".to_owned(),
        "\"w\"".to_owned(),
        "\"a\"".to_owned(),
        "\"rw\"".to_owned(),
    ];

    assert_eq!(union.render(), "\"r\" | \"w\" | \"a\" | \"rw\"");
    assert_eq!(literal_union_members(&union), Some(expected.clone()));
    assert_eq!(
        literal_union_members(&optional(union.clone())),
        Some(expected.clone())
    );
    assert_eq!(literal_union_members(&nullable(union)), Some(expected));
}

#[test]
fn literal_union_members_rejects_open_rows_and_tag_variants() {
    assert_eq!(
        literal_union_members(&variant_type(
            vec![literal_string("\"r\""), literal_string("\"w\"")],
            RowTail::Open,
        )),
        None
    );
    assert_eq!(
        literal_union_members(&variant_type(
            vec![tag("Ok", vec![named("Text")])],
            RowTail::Closed,
        )),
        None
    );
}

#[test]
fn function_signature_query_returns_params_and_result_and_peels_wrappers() {
    let signature = function(vec![named("Int"), named("Text")], named("Bool"));
    let expected = Some((vec![named("Int"), named("Text")], named("Bool")));

    assert_eq!(function_signature(&signature), expected.clone());
    assert_eq!(
        function_signature(&optional(signature.clone())),
        expected.clone()
    );
    assert_eq!(function_signature(&nullable(signature)), expected);
    assert_eq!(function_signature(&named("Text")), None);
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
        Some("\"hi\"".to_owned())
    );
}

#[test]
fn check_output_records_annotated_declared_types() {
    let source = "person : { name: Text, .. } = current\ncurrent = _\n";
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
fn check_output_records_concrete_expression_types() {
    let source = "add : (Int, Int) -> Int\nadd = (a, b) => a + b\ntotal = add(1, 2)\nconfig = { name: \"Ada\" }\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
    assert_eq!(
        check
            .type_at(binding_value_named(&output.module, "total").span)
            .map(Type::render),
        Some("Int".to_owned())
    );
    assert_eq!(
        check
            .type_at(binding_value_named(&output.module, "config").span)
            .map(Type::render),
        Some("{ name: \"Ada\" }".to_owned())
    );
}

#[test]
fn check_output_type_at_returns_narrowest_containing_expression_type() {
    let source = "name : { length: Int } = current\nvalue = name.length\ncurrent = _\n";
    let output = parse_module(source);
    let check = check_module(&output.module);
    let field_access_span = binding_value_named(&output.module, "value").span;
    let field_span = nth_span(source, "length", 1);

    assert!(check.diagnostics.is_empty());
    assert_eq!(
        check.type_at(nth_span(source, "name", 1)).map(Type::render),
        Some("{ length: Int }".to_owned())
    );
    assert_eq!(
        check.type_at(field_span).map(Type::render),
        Some("Int".to_owned())
    );
    assert_eq!(
        check.type_at(field_access_span).map(Type::render),
        Some("Int".to_owned())
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
    let output = parse_module("User = { name: Text }\nvalue : User = user\nuser = _\n");
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
        "Value = Array(Int)\n",
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
fn comptime_tagsof_variant_evaluates_sorted_label_set() {
    let subject = Type::Variant(Row {
        entries: vec![
            tag("Red", Vec::new()),
            tag("Green", Vec::new()),
            tag("Blue", Vec::new()),
        ],
        tail: RowTail::Closed,
    });
    let result = comptime::evaluate_tags_of(&subject, Span::new(0, 0), false);

    assert_eq!(
        result.evaluation,
        comptime::Evaluation::Evaluated(comptime::ComptimeValue::LabelSet(vec![
            "Blue".to_owned(),
            "Green".to_owned(),
            "Red".to_owned(),
        ]))
    );
    assert!(result.diagnostics.is_empty());
}

#[test]
fn comptime_tagsof_variant_reifies_sorted_literal_union() {
    let output = parse_module("Color = @{ @Red, @Green, @Blue }\nTags = tagsOf(Color)\n");
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("Tags"),
        Some(&Type::Variant(Row {
            entries: vec![
                literal_string("\"Blue\""),
                literal_string("\"Green\""),
                literal_string("\"Red\"")
            ],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_param_call_infers_reflection_domain_for_runtime_binding() {
    // A comptime `@param` whose domain reflects on a runtime parameter's type
    // (`tagsOf(v)`) infers a concrete literal-union type for the runtime
    // binding, with no annotation required. The generic parameter type variable
    // `v` is instantiated per call rather than rejected as a rigid type.
    let source = "Color = @{ @Red, @Green, @Blue }\n\
         color : Color = @Red\n\
         select = (variant: v, @tags: tagsOf(v)@{}) => tags\n\
         selected = select(color, @{\"Red\", \"Blue\"})\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(binding_value_named(&output.module, "selected").span)
            .map(Type::render),
        Some("\"Red\" | \"Blue\"".to_owned())
    );
}

#[test]
fn comptime_param_call_still_rejects_value_outside_reflection_domain() {
    // The instantiation fix must not weaken domain validation: a comptime
    // `@param` argument outside the reflected tag set is still rejected.
    let source = "Color = @{ @Red, @Green, @Blue }\n\
         color : Color = @Red\n\
         select = (variant: v, @tags: tagsOf(v)@{}) => tags\n\
         selected = select(color, @{\"Red\", \"Purple\"})\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::LITERAL_NOT_IN_UNION)),
        "expected literal-not-in-union diagnostic, got: {:?}",
        check.diagnostics
    );
}

#[test]
fn comptime_typeof_top_level_value_reifies_normalized_record_type() {
    let output = parse_module(
        "Config = { host: Text, port: Int }\n\
         config : Config = { host: \"x\", port: 8080 }\n\
         T = typeOf(config)\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("T"),
        Some(&Type::Record(Row {
            entries: vec![field("host", named("Text")), field("port", named("Int"))],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_typeof_direct_annotation_rejects_wrong_shape() {
    let output = parse_module(
        "config = { host: \"x\", port: 8080 }\n\
         T = typeOf(config)\n\
         other : T = { host: \"z\" }\n",
    );
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD),
        1
    );
}

#[test]
fn comptime_typeof_local_dependent_subject_defers_without_diagnostic() {
    let output = parse_module("f = (config) =>\n  value : typeOf(config) = config\n  value\n");
    let local_annotation = output
        .module
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == "f" => match &binding.value.kind {
                ExprKind::Lambda { body, .. } => match &body.kind {
                    ExprKind::Block(items) => items.iter().find_map(|item| match item {
                        Item::Binding(binding) if binding.name == "value" => {
                            binding.annotation.as_ref()
                        }
                        Item::Signature(signature) if signature.name == "value" => {
                            Some(&signature.annotation)
                        }
                        _ => None,
                    }),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .expect("expected local annotation");
    let lowering = lower_annotation(&output.module, local_annotation);

    assert_eq!(lowering.ty, Type::Deferred);
    assert!(lowering.diagnostics.is_empty());

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
fn comptime_type_position_record_comprehension_reifies_record_type() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         clone = (object) => { keysOf(object) -> k; (k, object[k]) }\n\
         Cloned = clone(User)\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("Cloned"),
        Some(&Type::Record(Row {
            entries: vec![field("email", named("Text")), field("name", named("Text"))],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_partial_wraps_fields_in_optional_types() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         clone = (object) => { keysOf(object) -> k; [k]: object[k] }\n\
         Partial = partial(User)\n\
         Cloned = clone(User)\n\
         p : Partial = { name: \"Ada\" }\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("Partial"),
        Some(&Type::Record(Row {
            entries: vec![
                field("email", optional(named("Text"))),
                field("name", optional(named("Text")))
            ],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        definitions.get("Cloned"),
        Some(&Type::Record(Row {
            entries: vec![field("email", named("Text")), field("name", named("Text"))],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_required_strips_partial_field_optional_types() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         required = (object) => { keysOf(object) -> k; [k]: !object[k] }\n\
         Restored = required(partial(User))\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("Restored"),
        Some(&Type::Record(Row {
            entries: vec![field("email", named("Text")), field("name", named("Text"))],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());
}

#[test]
fn comptime_type_position_record_comprehension_non_concrete_subject_defers_without_diagnostic() {
    let open = parse_module(
        "clone = (object) => { keysOf(object) -> k; (k, object[k]) }\n\
         value : clone({ name: Text, .. }) = x\n",
    );
    let open_lowering = lower_annotation(&open.module, annotation(&open.module, "value"));

    assert_eq!(open_lowering.ty, Type::Deferred);
    assert!(open_lowering.diagnostics.is_empty());

    let unknown = parse_module(
        "clone = (object) => { keysOf(object) -> k; (k, object[k]) }\n\
         Cloned = clone(t)\n",
    );
    let known_types = known_type_names(&unknown.module);
    let definitions = type_definitions(&unknown.module, &known_types);

    assert_eq!(definitions.get("Cloned"), Some(&Type::Deferred));

    let check = check_module(&unknown.module);
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
fn uppercase_comptime_functions_specialize_to_expanded_types() {
    let source = "Pair = (t: Type) => { first: t, second: t }\n\
        PairInt = Pair(Int)\n\
        p: Pair(Int) = { first: 1, second: 2 }\n\
        q: PairInt = { first: 1, second: \"two\" }\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert!(!definitions.contains_key("Pair"));
    assert_eq!(
        definitions.get("PairInt"),
        Some(&Type::Record(Row {
            entries: vec![field("first", named("Int")), field("second", named("Int"))],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::EVALUATION_UNSUPPORTED),
        0
    );
}

#[test]
fn comptime_type_function_application_requires_exact_arity() {
    let source = "Pair = (t: Type) => { first: t, second: t }\n\
        exact: Pair(Int) = { first: 1, second: 2 }\n\
        extra: Pair(Int, Text) = { first: 1, second: 2 }\n\
        missing: Pair() = { first: 1, second: 2 }\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    let arity_diagnostics: Vec<_> = check
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.code.as_deref() == Some(codes::ty::MISMATCH)
                && diagnostic.message.contains("comptime function `Pair`")
        })
        .collect();

    assert_eq!(arity_diagnostics.len(), 2, "{:?}", check.diagnostics);
    assert!(
        arity_diagnostics[0]
            .message
            .contains("expected 1 argument, given 2")
    );
    assert!(
        arity_diagnostics[1]
            .message
            .contains("expected 1 argument, given 0")
    );
    assert!(arity_diagnostics.iter().all(|diagnostic| {
        diagnostic
            .notes
            .iter()
            .any(|note| note.contains("expects 1 argument") && note.contains("gives"))
    }));
}

#[test]
fn builtin_type_application_arity_behavior_is_unchanged() {
    let output = parse_module("value: Array(Int, Text) = [1]\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert_eq!(
        check
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("comptime function"))
            .count(),
        0
    );
}

#[test]
fn uppercase_comptime_function_params_are_implicitly_comptime() {
    let output = parse_module("Pair = (@t) => { first: t, second: t }\n");
    let check = check_module(&output.module);

    assert_eq!(check.diagnostics.len(), 1);
    assert_eq!(check.diagnostics[0].severity, Severity::Warning);
    assert_eq!(
        check.diagnostics[0].code.as_deref(),
        Some(codes::comptime::REDUNDANT_COMPTIME_MARKER)
    );
}

#[test]
fn uppercase_comptime_functions_reject_runtime_arguments() {
    let output = parse_module(
        "Pair = (t) => { first: t, second: t }\nvalue = 1\np: Pair(value) = { first: 1, second: 2 }\n",
    );
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::ARGUMENT_NOT_KNOWN),
        1
    );
}

#[test]
fn uppercase_comptime_function_recursion_reports_a_specialization_cycle() {
    let output =
        parse_module("List = (t) => @{ @Nil, @Cons(t, List(t)) }\nvalue: List(Int) = @Nil\n");
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
fn lowers_function_application_optional_and_nullable_annotations() {
    let output = parse_module(
        "mapper : (Array(a), a -> b) -> Array(b)\noptional : ?Text = name\nnullable : Text? = name\nboth : ?Text? = name\n",
    );

    let mapper = lower_annotation(&output.module, annotation(&output.module, "mapper"));
    let optional_value = lower_annotation(&output.module, annotation(&output.module, "optional"));
    let nullable_value = lower_annotation(&output.module, annotation(&output.module, "nullable"));
    let both = lower_annotation(&output.module, annotation(&output.module, "both"));

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
    assert_eq!(optional_value.ty, optional(named("Text")));
    assert!(optional_value.diagnostics.is_empty());
    assert_eq!(nullable_value.ty, nullable(named("Text")));
    assert!(nullable_value.diagnostics.is_empty());
    assert_eq!(both.ty, optional(nullable(named("Text"))));
    assert!(both.diagnostics.is_empty());
}

#[test]
fn call_type_applications_lower_and_bracket_form_recovers() {
    let output = parse_module(
        "MyRes = Result(Int, Text)\n\
         nested : Result(Array(Int), Text) = @Ok([1])\n\
         legacy : Result[Int, Text] = @Ok(\"wrong\")\n",
    );
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::UNKNOWN_NAME),
        "standalone call-shaped type binding should resolve: {:?}",
        check.diagnostics
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::BRACKET_TYPE_APPLICATION),
        1
    );
    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn tuple_type_indexing_remains_indexing() {
    let output = parse_module("T = (Text, Int)\nvalue : T[0] = \"ok\"\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::BRACKET_TYPE_APPLICATION),
        "tuple type indexing must not be diagnosed as application: {:?}",
        check.diagnostics
    );
}

#[test]
fn lowers_optional_and_nullable_strip_annotations() {
    let output = parse_module(
        "strip_optional : !?Text = value\n\
         strip_optional_noop : !Text = value\n\
         strip_nullable : (Text?)! = value\n\
         strip_nullable_noop : Text! = value\n\
         optional_side_only : ?Text! = value\n\
         nullable_side_only : !Text? = value\n\
         optional_nullable_strip_optional : !?Text? = value\n\
         optional_nullable_strip_nullable : (?Text?)! = value\n\
         optional_nullable_strip_both : !(?Text?)! = value\n",
    );

    for name in [
        "strip_optional",
        "strip_optional_noop",
        "strip_nullable",
        "strip_nullable_noop",
        "optional_nullable_strip_both",
    ] {
        let lowering = lower_annotation(&output.module, annotation(&output.module, name));
        assert_eq!(lowering.ty, named("Text"), "{name}");
        assert!(lowering.diagnostics.is_empty(), "{name}");
    }

    let optional_side_only = lower_annotation(
        &output.module,
        annotation(&output.module, "optional_side_only"),
    );
    assert_eq!(optional_side_only.ty, optional(named("Text")));
    assert!(optional_side_only.diagnostics.is_empty());

    let nullable_side_only = lower_annotation(
        &output.module,
        annotation(&output.module, "nullable_side_only"),
    );
    assert_eq!(nullable_side_only.ty, nullable(named("Text")));
    assert!(nullable_side_only.diagnostics.is_empty());

    let optional_nullable_strip_optional = lower_annotation(
        &output.module,
        annotation(&output.module, "optional_nullable_strip_optional"),
    );
    assert_eq!(optional_nullable_strip_optional.ty, nullable(named("Text")));
    assert!(optional_nullable_strip_optional.diagnostics.is_empty());

    let optional_nullable_strip_nullable = lower_annotation(
        &output.module,
        annotation(&output.module, "optional_nullable_strip_nullable"),
    );
    assert_eq!(optional_nullable_strip_nullable.ty, optional(named("Text")));
    assert!(optional_nullable_strip_nullable.diagnostics.is_empty());
}

#[test]
fn lowers_postfix_collection_sugar_annotations() {
    let output = parse_module("array : Text[] = values\nset : Text@{} = values\n");
    assert!(output.diagnostics.is_empty());

    let array = lower_annotation(&output.module, annotation(&output.module, "array"));
    let set = lower_annotation(&output.module, annotation(&output.module, "set"));

    assert_eq!(array.ty, apply(named("Array"), vec![named("Text")]));
    assert!(array.diagnostics.is_empty());
    assert_eq!(set.ty, apply(named("Set"), vec![named("Text")]));
    assert!(set.diagnostics.is_empty());
}

#[test]
fn lowers_normalized_rows_and_closed_transforms() {
    let output = parse_module(
        "FileError = @{@Io}\n\
             user : { name: Text, email: Text?, phone: ?Text, .. } = current\n\
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
                field("name", named("Text")),
                field("email", nullable(named("Text"))),
                field("phone", optional(named("Text"))),
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
fn lowers_bare_literal_and_tag_annotations_as_singleton_variants() {
    let output = parse_module(
        "text : \"hi\" = value\n\
         text_set : @{\"hi\"} = value\n\
         number : 1 = value\n\
         number_set : @{1} = value\n\
         tagged : @A = value\n\
         tagged_set : @{@A} = value\n\
         flag : true = value\n",
    );

    let text = lower_annotation(&output.module, annotation(&output.module, "text"));
    let text_set = lower_annotation(&output.module, annotation(&output.module, "text_set"));
    let number = lower_annotation(&output.module, annotation(&output.module, "number"));
    let number_set = lower_annotation(&output.module, annotation(&output.module, "number_set"));
    let tagged = lower_annotation(&output.module, annotation(&output.module, "tagged"));
    let tagged_set = lower_annotation(&output.module, annotation(&output.module, "tagged_set"));
    let flag = lower_annotation(&output.module, annotation(&output.module, "flag"));

    assert_eq!(text.ty, text_set.ty);
    assert_eq!(
        text.ty,
        Type::Variant(Row {
            entries: vec![literal_string("\"hi\"")],
            tail: RowTail::Closed,
        })
    );
    assert!(text.diagnostics.is_empty());
    assert!(text_set.diagnostics.is_empty());

    assert_eq!(number.ty, number_set.ty);
    assert_eq!(
        number.ty,
        Type::Variant(Row {
            entries: vec![literal_number("1")],
            tail: RowTail::Closed,
        })
    );
    assert!(number.diagnostics.is_empty());
    assert!(number_set.diagnostics.is_empty());

    assert_eq!(tagged.ty, tagged_set.ty);
    assert_eq!(
        tagged.ty,
        Type::Variant(Row {
            entries: vec![tag("A", Vec::new())],
            tail: RowTail::Closed,
        })
    );
    assert!(tagged.diagnostics.is_empty());
    assert!(tagged_set.diagnostics.is_empty());

    assert_eq!(
        flag.ty,
        Type::Variant(Row {
            entries: vec![literal_bool(true)],
            tail: RowTail::Closed,
        })
    );
    assert!(flag.diagnostics.is_empty());
}

#[test]
fn lowers_pipe_union_annotations_like_set_literals() {
    let output = parse_module(
        "mode_pipe : \"r\" | \"w\" | \"rw\" = value\n\
         mode_set : @{\"r\", \"w\", \"rw\"} = value\n\
         tags_pipe : @A | @B = value\n\
         tags_set : @{@A, @B} = value\n\
         spliced_pipe : @{\"r\", \"w\"} | \"rw\" = value\n\
         spliced_set : @{\"r\", \"w\", \"rw\"} = value\n",
    );

    let mode_pipe = lower_annotation(&output.module, annotation(&output.module, "mode_pipe"));
    let mode_set = lower_annotation(&output.module, annotation(&output.module, "mode_set"));
    let tags_pipe = lower_annotation(&output.module, annotation(&output.module, "tags_pipe"));
    let tags_set = lower_annotation(&output.module, annotation(&output.module, "tags_set"));
    let spliced_pipe = lower_annotation(&output.module, annotation(&output.module, "spliced_pipe"));
    let spliced_set = lower_annotation(&output.module, annotation(&output.module, "spliced_set"));

    assert_eq!(mode_pipe.ty, mode_set.ty);
    assert!(mode_pipe.diagnostics.is_empty());
    assert!(mode_set.diagnostics.is_empty());
    assert_eq!(tags_pipe.ty, tags_set.ty);
    assert!(tags_pipe.diagnostics.is_empty());
    assert!(tags_set.diagnostics.is_empty());
    assert_eq!(spliced_pipe.ty, spliced_set.ty);
    assert!(spliced_pipe.diagnostics.is_empty());
    assert!(spliced_set.diagnostics.is_empty());
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
                },
                RowEntry::Field {
                    name: "y".to_owned(),
                    ty: named("Text"),
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
                },
                RowEntry::Field {
                    name: "timeout".to_owned(),
                    ty: named("Int"),
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
                },
                RowEntry::Field {
                    name: "name".to_owned(),
                    ty: named("Text"),
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
fn host_type_definition_stays_visible_when_user_reuses_its_name() {
    let output = parse_module("Instant = { seconds: Int }\n");
    let host_shape = build::record(vec![("epoch", named("Int"))]);
    let host = HostGlobals::default()
        .with_type_definitions(vec![("Instant".to_owned(), host_shape.clone())]);

    let checked = check_module_with_host_globals(&output.module, &host);

    assert!(
        checked
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::name::RESERVED_TYPE))
    );
    assert_eq!(checked.type_definitions.get("Instant"), Some(&host_shape));
}

#[test]
fn structured_aliases_keep_implicit_type_variables() {
    let output = parse_module("Pair = { a: t, b: t }\n");
    let checked = check_module(&output.module);

    assert!(
        !checked.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some(codes::name::RUNTIME_NAME_ALIAS)
        })
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
        "value : Undefined = 42\n",
        "value : Unit = \"hi\"\n",
        "value : { name: Text } = \"hi\"\n",
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
fn bool_keywords_evaluate_to_comptime_bools() {
    for (source, expected) in [("value = true\n", true), ("value = false\n", false)] {
        let value = binding_value(source);
        let result = comptime::evaluate_runtime_value(&value, &Default::default());

        assert!(matches!(
            result.evaluation,
            comptime::Evaluation::Evaluated(comptime::ComptimeValue::Bool(actual))
                if actual == expected
        ));
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
    let flexible = parse_module("other = 42\nvalue : Float = other\n");
    let flexible_check = check_module(&flexible.module);
    assert!(
        flexible_check.diagnostics.is_empty(),
        "unannotated number literal should stay Float-flexible: {:?}",
        flexible_check.diagnostics
    );

    for source in [
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
fn inline_lambda_return_annotation_mismatch_trusts_annotation() {
    // Body is Int, annotation says Text. The body mismatch is reported, and
    // the trusted annotation keeps `g` as `Int -> Text` so use sites see Text
    // (not Deferred poisoning).
    let source = concat!(
        "g = (x: Int): Text => x\n",
        "n: Int = g(1)\n",
        "b: Bool = g(1)\n",
        "h: (Int) -> Text = g\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    // body Int vs Text; n: Int = g(1); b: Bool = g(1). h fits once annotation
    // is trusted.
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        3,
        "expected body + n + b mismatches (h should fit); got {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "g", 0)).map(Type::render),
        Some("Int -> Text".to_owned())
    );
}

#[test]
fn inline_lambda_return_annotation_accepts_matching_bodies() {
    for source in [
        "g = (x: Int): Int => x\n",
        "g = (x: Int): Int =>\n  y = x\n  y\n",
        "g = (): Result(Int, @{@Nope}) => @Ok(1)\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch: {:?}",
            check.diagnostics
        );
    }
}

#[test]
fn inline_lambda_return_annotation_defers_unresolved_body_silently() {
    // Free type-variable param body stays incomplete (not `is_resolved_value_type`);
    // inference defers the result type and value-check must not invent a mismatch.
    let source = "g = (x: a): Text => x\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "unresolved body under return annotation should stay silent, got {:?}",
        check.diagnostics
    );
}

#[test]
fn binding_level_lambda_return_mismatch_still_reports_once() {
    let source = "g: (Int) -> Text = (x: Int) => x\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        1,
        "binding-level form should still report exactly one type.mismatch: {:?}",
        check.diagnostics
    );
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
fn structural_annotations_reject_literal_values() {
    let rejected = parse_module(concat!(
        "R = { x: Int }\n",
        "r : R = 42\n",
        "t : (Int, Text) = 42\n",
        "f : (Int) -> Int = 42\n",
    ));
    let rejected_check = check_module(&rejected.module);
    assert_eq!(
        matching_codes(&rejected_check.diagnostics, codes::ty::MISMATCH),
        3,
        "structural annotations must reject literals: {:?}",
        rejected_check.diagnostics
    );

    let accepted = parse_module(concat!(
        "R = { x: Int }\n",
        "r : R = { x: 42 }\n",
        "t : (Int, Text) = (42, \"ok\")\n",
        "f : (Int) -> Int = (x) => x\n",
        "mode : \"r\" | \"w\" = \"r\"\n",
    ));
    let accepted_check = check_module(&accepted.module);
    assert!(
        accepted_check.diagnostics.is_empty(),
        "valid structural and literal-union annotations failed: {:?}",
        accepted_check.diagnostics
    );
}

#[test]
fn fresh_literals_check_against_bare_literal_annotations_by_membership() {
    let source = concat!(
        "v : \"hi\" = \"hi\"\n",
        "w : @{\"hi\"} = \"hi\"\n",
        "n : 1 = 1\n",
        "m : @{1} = 1\n",
    );
    let accepted = parse_module(source);
    let accepted_check = check_module(&accepted.module);
    assert!(
        accepted_check.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        accepted_check.diagnostics
    );

    let v_type = accepted_check.type_at(nth_span(source, "v", 0));
    let w_type = accepted_check.type_at(nth_span(source, "w", 0));
    assert_eq!(v_type.map(Type::render), Some("\"hi\"".to_owned()));
    assert_eq!(v_type, w_type);

    let n_type = accepted_check.type_at(nth_span(source, "n", 0));
    let m_type = accepted_check.type_at(nth_span(source, "m", 0));
    assert_eq!(n_type.map(Type::render), Some("1".to_owned()));
    assert_eq!(n_type, m_type);

    let rejected = parse_module("v : \"hi\" = \"hello\"\nn : 1 = 2\n");
    let rejected_check = check_module(&rejected.module);
    assert_eq!(
        matching_codes(&rejected_check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        2
    );
}

#[test]
fn literal_composite_values_stay_runtime_after_singleton_lowering() {
    // Lowering bare literals to singleton variants (R1) also fires inside
    // composite annotations, so a tuple/record of literals lowers to a concrete
    // (non-Deferred) type. The artifact check must still treat such a value as a
    // runtime value, not a comptime type artifact, so it remains usable.
    let source = concat!(
        "pair = (1, 2)\n",
        "rec = { a = \"x\" }\n",
        "usePair = pair\n",
        "useRec = rec\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "literal-composite runtime bindings should check cleanly: {:?}",
        check.diagnostics
    );
}

#[test]
fn call_member_literal_checks_literal_union_param_by_membership() {
    let source = concat!(
        "myFun = (mode: @{\"text\", \"int\"}) => mode\n",
        "result = myFun(\"text\")\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );

    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "result", 0))
            .map(Type::render),
        Some("\"text\" | \"int\"".to_owned())
    );
}

#[test]
fn call_non_member_literal_reports_literal_not_in_union() {
    let source = concat!(
        "myFun = (mode: @{\"text\", \"int\"}) => mode\n",
        "result = myFun(\"nope\")\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );

    let check = check_module(&output.module);
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        1
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::WIDE_VALUE_INTO_LITERAL_UNION),
        0
    );
}

#[test]
fn call_base_kind_mismatched_literal_reports_literal_not_in_union() {
    let source = concat!(
        "myFun = (mode: @{\"r\", \"w\"}) => mode\n",
        "result = myFun(5)\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        1,
        "expected membership diagnostic for mismatched literal kind: {:?}",
        check.diagnostics
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        0,
        "mismatched literal kind should not use generic mismatch: {:?}",
        check.diagnostics
    );
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::LITERAL_NOT_IN_UNION))
        .expect("literal-not-in-union diagnostic");
    assert!(diagnostic.message.contains("literal 5 is not one of"));
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "5", 0));
}

#[test]
fn call_wide_value_into_literal_union_param_reports_wide_value() {
    let source = concat!(
        "myFun = (mode: @{\"r\", \"w\"}) => mode\n",
        "fromText = (s: Text) => myFun(s)\n",
        "result = fromText(\"r\")\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );

    let check = check_module(&output.module);
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::WIDE_VALUE_INTO_LITERAL_UNION),
        1
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        0
    );
}

#[test]
fn bare_value_literals_infer_rendered_singleton_types() {
    let source = "x = 5\ns = \"hi\"\nb = true\n";
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );

    assert_eq!(
        check.type_at(nth_span(source, "x", 0)).map(Type::render),
        Some("5".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "s", 0)).map(Type::render),
        Some("\"hi\"".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "b", 0)).map(Type::render),
        Some("true".to_owned())
    );

    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let scheme = checker
        .infer_top_level_scheme("x")
        .expect("inferred x scheme");
    let Type::Variant(row) = &scheme.ty else {
        panic!("x should infer a literal variant row");
    };
    assert_eq!(row.tail, RowTail::Var(scheme.row_vars[0]));

    let scheme = checker
        .infer_top_level_scheme("b")
        .expect("inferred b scheme");
    let Type::Variant(row) = &scheme.ty else {
        panic!("b should infer a literal variant row");
    };
    assert_eq!(row.entries, vec![literal_bool(true)]);
    assert_eq!(row.tail, RowTail::Var(scheme.row_vars[0]));
}

#[test]
fn literal_rows_widen_at_named_annotations() {
    let source = "n : Int = 5\nf : Float = 1\nb : Bool = true\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "n", 0)).map(Type::render),
        Some("Int".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "f", 0)).map(Type::render),
        Some("Float".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "b", 0)).map(Type::render),
        Some("Bool".to_owned())
    );
}

#[test]
fn bool_singletons_widen_at_bool_params_and_fold_boolean_ops() {
    let source = concat!(
        "accept = (input: Bool) => input\n",
        "arg = accept(true)\n",
        "flag = true\n",
        "x = flag && false\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "arg", 0)).map(Type::render),
        Some("Bool".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "flag", 0)).map(Type::render),
        Some("true".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "x", 0)).map(Type::render),
        Some("false".to_owned())
    );
}

#[test]
fn bool_base_match_exhaustiveness_uses_true_false_members() {
    let complete = parse_module(concat!(
        "source : Bool = value\n",
        "result = source ?>\n",
        "  true => 1\n",
        "  false => 2\n",
    ));
    let complete_check = check_module(&complete.module);
    assert!(
        !has_diagnostic_code(&complete_check.diagnostics, codes::ty::NON_EXHAUSTIVE_MATCH),
        "complete Bool match produced diagnostics: {:?}",
        complete_check.diagnostics
    );

    let missing = parse_module(concat!(
        "source : Bool = value\n",
        "result = source ?>\n",
        "  true => 1\n",
    ));
    assert_eq!(
        matching_codes(
            &check_module(&missing.module).diagnostics,
            codes::ty::NON_EXHAUSTIVE_MATCH,
        ),
        1
    );
}

#[test]
fn base_operations_fold_comptime_singleton_literals() {
    let source = concat!(
        "c = 1 + 2\n",
        "nested = (1 + 2) * 3\n",
        "concat = \"a\" + \"b\"\n",
        "less = 1 < 2\n",
        "equal = 1 == 2\n",
        "text_equal = \"a\" == \"a\"\n",
        "escape_equal = \"\\n\" == \"\\u{0a}\"\n",
        "bool_not_equal = true != false\n",
        "not = !false\n",
        "neg = -1\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "c", 0)).map(Type::render),
        Some("3".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "nested", 0))
            .map(Type::render),
        Some("9".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "concat", 0))
            .map(Type::render),
        Some("\"ab\"".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "less", 0)).map(Type::render),
        Some("true".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "equal", 0))
            .map(Type::render),
        Some("false".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "text_equal", 0))
            .map(Type::render),
        Some("true".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "escape_equal", 0))
            .map(Type::render),
        Some("true".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "bool_not_equal", 0))
            .map(Type::render),
        Some("true".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "not", 0)).map(Type::render),
        Some("true".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "neg", 0)).map(Type::render),
        Some("-1".to_owned())
    );
}

#[test]
fn non_foldable_base_operations_keep_widened_types() {
    let source = "x : Int = 2\nruntime = 1 + x\nzero = 1 / 0\nmixed = 1 + 2.0\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "runtime", 0))
            .map(Type::render),
        Some("Int".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "zero", 0)).map(Type::render),
        Some("Int".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "mixed", 0))
            .map(Type::render),
        Some("Float".to_owned())
    );
}

#[test]
fn folded_results_widen_and_check_like_written_literals() {
    let source = "c = 1 + 2\nd : Int = c\ne : 4 = c\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        check.type_at(nth_span(source, "c", 0)).map(Type::render),
        Some("3".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "d", 0)).map(Type::render),
        Some("Int".to_owned())
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        1,
        "expected folded literal membership failure: {:?}",
        check.diagnostics
    );
}

#[test]
fn polymorphic_arguments_join_literal_rows() {
    let source = "joined = same(1, 2)\n";
    let output = parse_module(source);
    let globals = vec![(
        "same".to_owned(),
        build::function(vec![build::var("a"), build::var("a")], build::var("a")),
    )];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "joined", 0))
            .map(Type::render),
        Some("1 | 2".to_owned())
    );
}

#[test]
fn match_results_join_literal_rows() {
    let source = concat!(
        "classify = (n: Int) =>\n",
        "  n ?>\n",
        "    0 => \"zero\"\n",
        "    _ => \"many\"\n",
        "result = classify(5)\n",
        "label : Text = result\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "result", 0))
            .map(Type::render),
        Some("\"zero\" | \"many\"".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "label", 0))
            .map(Type::render),
        Some("Text".to_owned())
    );
}

#[test]
fn collections_join_literal_rows() {
    let source = "nums = [1, 2, 3]\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "nums", 0)).map(Type::render),
        Some("Array(1 | 2 | 3)".to_owned())
    );
}

#[test]
fn wide_value_still_cannot_flow_into_literal_union_param() {
    let source = concat!(
        "pick = (m: @{\"r\", \"w\"}) => m\n",
        "bad : Text = \"x\"\n",
        "result = pick(bad)\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::WIDE_VALUE_INTO_LITERAL_UNION,),
        1
    );
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
        "value : Array(Text) =\n  [1]\n",
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
        "flag : Bool = true\nvalue : Text =\n  result ?>\n    @Ok(_), flag => \"ok\"\n",
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
        "source : @{@Ok(Text), @Err(Text)} = result\nvalue : Bool = source ?>\n  @Ok(item) => item\n  @Err(_) => false\n",
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
fn result_subject_match_reproduction_checks_clean() {
    let source = concat!(
        "g : () -> Result(Int, @{@Nope})\n",
        "g = () => @Ok(1)\n",
        "r = g() ?>\n",
        "  @Ok(v) => v\n",
        "  @Err(_) => 0\n",
        "writeLine(\"done\")\n",
    );
    let output = parse_module(source);
    let globals = vec![(
        "writeLine".to_owned(),
        build::function(vec![build::text()], build::unit()),
    )];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "r", 0)).map(Type::render),
        Some("Int".to_owned())
    );
}

#[test]
fn result_subject_match_with_inline_return_annotation_checks_clean() {
    // Same reproduction as above, but the Result annotation is written inline
    // on the lambda (`(): Result(...) =>`), which resolves through the
    // inference-direction fit check rather than the declared-signature path: a
    // variant-row body (`@Ok(1)`) must fit the Result annotation by the
    // boundary rule.
    let source = concat!(
        "g = (): Result(Int, @{@Nope}) => @Ok(1)\n",
        "r = g() ?>\n",
        "  @Ok(v) => v\n",
        "  @Err(_) => 0\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "g", 0)).map(Type::render),
        Some("() -> Result(Int, @Nope)".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "r", 0)).map(Type::render),
        Some("Int".to_owned())
    );
}

#[test]
fn result_match_payload_binders_use_result_type_arguments() {
    let source = concat!(
        "source : Result(Text, @{@Nope}) = @Ok(\"ok\")\n",
        "matched = source ?>\n",
        "  @Ok(v) => v\n",
        "  @Err(_) => \"fallback\"\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "matched", 0))
            .map(Type::render),
        Some("Text".to_owned())
    );
}

#[test]
fn result_match_exhaustiveness_uses_result_tags() {
    let missing = parse_module(concat!(
        "source : Result(Int, @{@Nope}) = @Ok(1)\n",
        "r = source ?>\n",
        "  @Ok(v) => v\n",
    ));
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::NON_EXHAUSTIVE_MATCH),
        1,
        "missing Err arm should be non-exhaustive: {:?}",
        missing_check.diagnostics
    );

    for source in [
        concat!(
            "source : Result(Int, @{@Nope}) = @Ok(1)\n",
            "r = source ?>\n",
            "  @Ok(v) => v\n",
            "  @Err(_) => 0\n",
        ),
        concat!(
            "source : Result(Int, @{@Nope}) = @Ok(1)\n",
            "r = source ?>\n",
            "  @Ok(v) => v\n",
            "  _ => 0\n",
        ),
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);
        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::NON_EXHAUSTIVE_MATCH),
            "{source} unexpectedly reported non-exhaustive match: {:?}",
            check.diagnostics
        );
    }
}

#[test]
fn result_match_with_uninhabited_error_needs_no_err_arm() {
    // A `Result(a, @{})` cannot fail — its `@Err` payload is the uninhabited
    // empty closed variant — so a lone `@Ok` arm is exhaustive.
    let source = concat!(
        "source : Result(Int, @{}) = @Ok(1)\n",
        "r = source ?>\n",
        "  @Ok(v) => v\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::NON_EXHAUSTIVE_MATCH),
        "an uninhabited @Err needs no arm: {:?}",
        check.diagnostics
    );
}

#[test]
fn result_match_reports_impossible_tag_arm() {
    let source = concat!(
        "source : Result(Int, @{@Nope}) = @Ok(1)\n",
        "r = source ?>\n",
        "  @Ok(v) => v\n",
        "  @Other(_) => 0\n",
        "  @Err(_) => 0\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        1,
        "unexpected tag arm should report a mismatch: {:?}",
        check.diagnostics
    );
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::NON_EXHAUSTIVE_MATCH
    ));
}

#[test]
fn result_match_supports_nested_error_payload_patterns() {
    let source = concat!(
        "source : Result(Int, @{@Nope}) = @Err(@Nope)\n",
        "r = source ?>\n",
        "  @Ok(v) => v\n",
        "  @Err(@Nope) => 0\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
}

#[test]
fn result_match_handles_json_decode_shaped_global() {
    let source = concat!(
        "DecodeError = @{@Decode(Text)}\n",
        "userName = Json.decode(\"{}\") ?>\n",
        "  @Ok(user) => user.name\n",
        "  @Err(err) => err ?>\n",
        "    @Decode(message) => message\n",
    );
    let output = parse_module(source);
    let globals = vec![(
        "Json".to_owned(),
        build::record(vec![(
            "decode",
            build::function(
                vec![build::text()],
                build::result(
                    build::record(vec![("name", build::text())]),
                    build::named("DecodeError"),
                ),
            ),
        )]),
    )];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "userName", 0))
            .map(Type::render),
        Some("Text".to_owned())
    );
}

#[test]
fn unannotated_constructor_match_resolves_payload_binder() {
    let source = "matched = @Some(1) ?>\n  @Some(n) => n\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "matched", 0))
            .map(Type::render),
        Some("1".to_owned())
    );
}

#[test]
fn unannotated_multi_arm_constructor_match_resolves_payload_binders() {
    let source = "matched = @Some(1) ?>\n  @Some(n) => n\n  @None => 0\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::UNRESOLVED_BINDING),
        "payload binder left unresolved: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "matched", 0))
            .map(Type::render),
        Some("1 | 0".to_owned())
    );
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

    assert!(matches!(
        checker.infer_top_level_value("matched"),
        Some(Type::Variant(Row {
            entries,
            tail: RowTail::Closed,
        })) if entries.iter().all(|entry| matches!(entry, RowEntry::Literal { value: Literal::Bool(_) }))
    ));
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn record_destructure_binders_use_subject_field_types() {
    let mismatch = parse_module(
        "u = { name: \"Ada\", age: 36 }\n\
         { name } = u\n\
         n: Int = name\n",
    );
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1,
        "record binder should retain its Text field type: {:?}",
        mismatch_check.diagnostics
    );

    let correct = parse_module(
        "u = { name: \"Ada\", person: { active: true }, age: 36 }\n\
         { name, person: { active }, ..rest } = u\n\
         text: Text = name\n\
         flag: Bool = active\n\
         remaining: { age: Int } = rest\n",
    );
    let correct_check = check_module(&correct.module);
    assert!(
        correct_check.diagnostics.is_empty(),
        "record, nested, and rest binders should retain their field types: {:?}",
        correct_check.diagnostics
    );

    let deferred = parse_module(
        "f = (subject) =>\n\
           { name } = subject\n\
           n: Int = name\n\
           n\n",
    );
    let deferred_check = check_module(&deferred.module);
    assert!(
        !has_diagnostic_code(&deferred_check.diagnostics, codes::ty::MISMATCH),
        "deferred record subjects must leave binders unknown: {:?}",
        deferred_check.diagnostics
    );

    let missing = parse_module("u = { name: \"Ada\" }\n{ age } = u\n");
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::MISSING_FIELD),
        1,
        "closed record destructures should report missing fields: {:?}",
        missing_check.diagnostics
    );
}

#[test]
fn open_record_match_rest_binder_stays_unconstrained() {
    let source = "source : { x: Int, y: Text, .. } = value\n\
         picked = source ?>\n  { x, ..rest } => x\n\
         remaining = source ?>\n  { x, ..rest } => rest.y\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(checker.infer_top_level_value("picked"), Some(named("Int")));
    assert_eq!(checker.infer_top_level_value("remaining"), None);
    assert!(checker.diagnostics.is_empty());

    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::UNRESOLVED_BINDING),
        "open record rest binder should stay intentionally unknown: {:?}",
        check.diagnostics
    );
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
fn comptime_known_match_subject_selects_single_arm_result_type() {
    let source = concat!(
        "myFun = (@x: @{\"text\", \"int\"}) =>\n",
        "  x ?>\n",
        "    \"text\" => \"hello\"\n",
        "    \"int\" => 42\n",
        "v = myFun(\"text\")\n",
        "w = myFun(\"int\")\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        check.diagnostics
    );

    assert_eq!(
        check.type_at(nth_span(source, "v", 0)).map(Type::render),
        Some("\"hello\"".to_owned())
    );
    assert_eq!(
        check.type_at(nth_span(source, "w", 0)).map(Type::render),
        Some("42".to_owned())
    );
}

#[test]
fn comptime_known_match_subject_selects_catch_all_arm() {
    let source = concat!(
        "wild = (@x: @{\"text\", \"int\"}) =>\n",
        "  x ?>\n",
        "    \"int\" => false\n",
        "    _ => \"fallback\"\n",
        "bound = (@x: @{\"text\", \"int\"}) =>\n",
        "  x ?>\n",
        "    \"int\" => false\n",
        "    selected => selected\n",
        "wildValue = wild(\"text\")\n",
        "boundValue = bound(\"text\")\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        check.diagnostics
    );

    assert_eq!(
        check
            .type_at(nth_span(source, "wildValue", 0))
            .map(Type::render),
        Some("\"fallback\"".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "boundValue", 0))
            .map(Type::render),
        Some("\"text\"".to_owned())
    );
}

#[test]
fn runtime_match_subject_reports_incompatible_arm_results() {
    let source = concat!(
        "source : @{\"text\", \"int\"} = runtime\n",
        "result = source ?>\n",
        "  \"text\" => \"hello\"\n",
        "  \"int\" => 42\n",
        "runtime = _\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::INCOMPATIBLE_MATCH_ARMS),
        1,
        "expected incompatible match-arm diagnostic: {:?}",
        check.diagnostics
    );
    assert!(check.type_at(nth_span(source, "result", 0)).is_none());
}

#[test]
fn runtime_match_subject_reports_plain_base_type_arm_conflicts() {
    let source = concat!(
        "text : Text = \"hello\"\n",
        "number : Int = 42\n",
        "source : @{\"text\", \"int\"} = runtime\n",
        "result = source ?>\n",
        "  \"text\" => text\n",
        "  \"int\" => number\n",
        "runtime = _\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::INCOMPATIBLE_MATCH_ARMS),
        1,
        "plain base-type arm conflict diagnostics: {:?}",
        check.diagnostics
    );
}

#[test]
fn runtime_match_subject_accepts_homogeneous_arm_results() {
    for (source, expected) in [
        (
            concat!(
                "source : Int = runtime\n",
                "result = source ?>\n",
                "  0 => \"a\"\n",
                "  _ => \"b\"\n",
                "runtime = _\n",
            ),
            "\"a\" | \"b\"",
        ),
        (
            concat!(
                "source : Int = runtime\n",
                "result = source ?>\n",
                "  0 => @A\n",
                "  _ => @B\n",
                "runtime = _\n",
            ),
            "@A | @B",
        ),
        (
            concat!(
                "left : Text = \"a\"\n",
                "right : Text = \"b\"\n",
                "source : Int = runtime\n",
                "result = source ?>\n",
                "  0 => left\n",
                "  _ => right\n",
                "runtime = _\n",
            ),
            "Text",
        ),
    ] {
        let output = parse_module(source);
        assert!(
            output.diagnostics.is_empty(),
            "unexpected parse diagnostics: {:?}",
            output.diagnostics
        );
        let check = check_module(&output.module);
        assert!(
            check.diagnostics.is_empty(),
            "{source} unexpectedly produced diagnostics: {:?}",
            check.diagnostics
        );
        assert_eq!(
            check
                .type_at(nth_span(source, "result", 0))
                .map(Type::render),
            Some(expected.to_owned())
        );
    }
}

#[test]
fn comptime_selected_match_subject_allows_heterogeneous_arm_results() {
    let source = concat!(
        "result = \"text\" ?>\n",
        "  \"text\" => \"hello\"\n",
        "  _ => 42\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "result", 0))
            .map(Type::render),
        Some("\"hello\"".to_owned())
    );
}

#[test]
fn unspecialized_comptime_param_match_allows_type_valued_arm_results() {
    let source = concat!(
        "typeFor = (@kind: @{\"text\", \"int\"}) =>\n",
        "  kind ?>\n",
        "    \"text\" => Text\n",
        "    \"int\" => Int\n",
    );
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected check diagnostics: {:?}",
        check.diagnostics
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
fn unannotated_result_match_helpers_are_generic_and_join_result_arms() {
    let source = "mapErr = (r, f) => r ?> @Ok(v) => @Ok(v), @Err(e) => @Err(f(e))\n\
                  orElse = (r, f) => r ?> @Ok(v) => @Ok(v), @Err(e) => f(e)\n\
                  ok : Result(Int, Text) = @Ok(1)\n\
                  err : Result(Int, Text) = @Err(\"x\")\n\
                  mapped = mapErr(err, (e) => \"wrap: ${e}\")\n\
                  recovered = orElse(err, (_) => @Ok(0))\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    for name in ["mapErr", "orElse"] {
        let scheme = checker
            .infer_top_level_scheme(name)
            .unwrap_or_else(|| panic!("scheme for {name}"));
        assert!(!crate::ty::type_contains_deferred(&scheme.ty));
        assert!(scheme.ty.render().contains("Result"));
    }
    assert!(checker.infer_top_level_value("mapped").is_some());
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn annotated_polymorphic_functions_export_and_instantiate() {
    // Declaration-form annotations with type variables must quantify (so each
    // use instantiates fresh) and reify for module export (`top_level_types`).
    let source = "id : (a) -> a\n\
                  id = (x) => x\n\
                  n : Int = id(1)\n\
                  t : Text = id(\"hi\")\n\
                  { id }\n";
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    let id = check
        .top_level_types
        .get("id")
        .expect("annotated polymorphic export should appear in top_level_types");
    assert!(
        crate::ty::type_contains_variable(id),
        "export type should retain type variables, got {}",
        id.render()
    );

    let mismatch = parse_module("id : (a) -> a\nid = (x) => x\nn : Int = id(\"hi\")\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );
}

fn generic_array_module_imports() -> ModuleImports {
    let item = variable("a");
    let accumulator = variable("b");
    let array = |item: Type| apply(named("Array"), vec![item]);
    let map = function(
        vec![
            array(item.clone()),
            function(vec![item.clone()], accumulator.clone()),
        ],
        array(accumulator.clone()),
    );
    let fold = function(
        vec![
            array(item.clone()),
            accumulator.clone(),
            function(vec![accumulator.clone(), item], accumulator.clone()),
        ],
        accumulator,
    );
    ModuleImports::new([(
        "std/array".to_owned(),
        Type::Record(Row {
            entries: vec![field("map", map), field("fold", fold)],
            tail: RowTail::Closed,
        }),
    )])
}

#[test]
fn value_position_generic_function_calls_instantiate_per_call() {
    let imports = generic_array_module_imports();

    for source in [
        "array = import(\"std/array\")\n\
         xs = [1, 2, 3]\n\
         ys: Array(Int) = array.map(xs, (x) => \"x\")\n",
        "array = import(\"std/array\")\n\
         xs = [1, 2, 3]\n\
         zs: Int = array.fold(xs, 0, (acc, x) => \"bad\")\n",
        "id = (x: a): a => x\n\
         n: Int = id(\"hi\")\n",
    ] {
        let output = parse_module(source);
        let check = check_module_with_host_globals_and_imports(
            &output.module,
            &HostGlobals::default(),
            &imports,
        );
        // The fold case reports through the literal-union path (the seed `0`
        // stays a literal union), the others as plain mismatches — either way
        // the call site must produce exactly one error.
        let errors = check
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic.code.as_deref(),
                    Some(codes::ty::MISMATCH | codes::ty::LITERAL_NOT_IN_UNION)
                )
            })
            .count();
        assert_eq!(
            errors, 1,
            "expected a call-site mismatch for {source:?}: {:?}",
            check.diagnostics
        );
    }

    let passing = parse_module(
        "array = import(\"std/array\")\n\
         xs = [1, 2, 3]\n\
         numbers: Array(Int) = array.map(xs, (x) => x)\n\
         words: Array(Text) = array.map(xs, (x) => \"x\")\n\
         total: Int = array.fold(xs, 0, (acc, x) => acc + x)\n\
         id = (x: a): a => x\n\
         n: Int = id(5)\n\
         t: Text = id(\"hi\")\n\
         { map } = import(\"std/array\")\n\
         extracted: Array(Text) = map(xs, (x) => \"x\")\n",
    );
    let passing_check = check_module_with_host_globals_and_imports(
        &passing.module,
        &HostGlobals::default(),
        &imports,
    );
    assert!(
        passing_check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        passing_check.diagnostics
    );
}

#[test]
fn annotated_polymorphic_body_cannot_pin_rigid_variables() {
    // `a` is caller-chosen; the body may not return Text.
    let ident = parse_module("ident : (a) -> a\nident = (x) => \"oops\"\n");
    let ident_check = check_module(&ident.module);
    assert!(
        matching_codes(&ident_check.diagnostics, codes::ty::MISMATCH) >= 1,
        "expected mismatch for pinned identity body, got {:?}",
        ident_check.diagnostics
    );
    assert!(
        ident_check.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .notes
                .iter()
                .any(|note| note.contains("type parameter chosen by the caller"))
        }),
        "expected rigid-variable note, got {:?}",
        ident_check.diagnostics
    );

    // Distinct result variable `b` is also rigid: body cannot pin it to Int.
    let sneaky = parse_module("sneaky : (a) -> b\nsneaky = (x) => 1\n");
    let sneaky_check = check_module(&sneaky.module);
    assert!(
        matching_codes(&sneaky_check.diagnostics, codes::ty::MISMATCH) >= 1,
        "expected mismatch for pinned result variable, got {:?}",
        sneaky_check.diagnostics
    );

    // Optional shape of the same white-lie: annotation `-> ?b`, arm returns Int.
    let optional = parse_module("sneaky : (a) -> ?b\nsneaky = (x) => 1\n");
    let optional_check = check_module(&optional.module);
    assert!(
        matching_codes(&optional_check.diagnostics, codes::ty::MISMATCH) >= 1,
        "expected mismatch for ?b pinned to Int, got {:?}",
        optional_check.diagnostics
    );
}

#[test]
fn annotated_polymorphic_bodies_accept_consistent_variables() {
    // Layout-sensitive: indented block bodies must use real leading spaces, not
    // continuation-indent padding from concatenated string literals.
    let source = r#"
ident : (a) -> a
ident = (x) => x
const : (a, b) -> a
const = (x, _) => x
fold : (Array(a), b, (b, a) -> b) -> b
fold = (xs, seed, f) => seed
map : (Array(a), (a) -> b) -> Array(b)
map = (xs, f) =>
  seed: Array(b) = []
  fold(xs, seed, (acc, x) => acc.push(f(x)))
n : Int = ident(1)
t : Text = ident("hi")
"#;
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics: {:?}",
        output.diagnostics
    );
    let check = check_module(&output.module);
    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
}

#[test]
fn result_methods_preserve_and_replace_result_type_arguments() {
    let source = "ok : Result(Int, Text) = @Ok(1)\n\
                  err : Result(Int, Text) = @Err(\"x\")\n\
                  mapped_ok = ok.mapErr((e) => e == \"\")\n\
                  mapped_err = err.mapErr((e) => e == \"\")\n\
                  recovered_ok = ok.orElse((e): Result(Bool, Bool) => @Ok(e == \"\"))\n\
                  recovered_err = err.orElse((e): Result(Bool, Bool) => @Ok(e == \"\"))\n\
                  unwrapped = err.mapErr((e) => e == \"\")?^\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let expected = crate::ty::build::result(named("Int"), named("Bool"));

    for name in ["mapped_ok", "mapped_err"] {
        let scheme = checker
            .infer_top_level_scheme(name)
            .unwrap_or_else(|| panic!("scheme for {name}"));
        assert_eq!(
            checker.infer_top_level_value(name),
            Some(expected.clone()),
            "{name}: {:?}; {:?}",
            scheme.ty,
            checker.diagnostics,
        );
    }
    let recovered = crate::ty::build::result(named("Bool"), named("Bool"));
    for name in ["recovered_ok", "recovered_err"] {
        assert_eq!(checker.infer_top_level_value(name), Some(recovered.clone()));
    }
    assert_eq!(
        checker.infer_top_level_value("unwrapped"),
        Some(named("Int"))
    );
    assert!(checker.diagnostics.is_empty(), "{:?}", checker.diagnostics);
}

#[test]
fn result_or_else_ok_only_callback_makes_error_side_uninhabited() {
    // A callback that only ever returns `@Ok` recovers every error, so the
    // chain can no longer fail: the error side becomes the empty closed
    // variant while the success type stays the receiver's.
    let source = concat!(
        "source: Result(Text, Text) = @Err(\"no\")\n",
        "recovered = source.orElse((e) => @Ok(\"recovered: ${e}\"))\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "recovered", 0))
            .map(Type::render),
        Some("Result(Text, @{})".to_owned())
    );
}

#[test]
fn result_or_else_err_only_callback_keeps_receiver_ok_and_replaces_error() {
    // A callback that only returns `@Err` never contributes a success, so the
    // ok side stays the receiver's while the error type is replaced.
    let source = concat!(
        "source: Result(Int, Text) = @Ok(1)\n",
        "remapped = source.orElse((e) => @Err(42))\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "remapped", 0))
            .map(Type::render),
        Some("Result(Int, 42)".to_owned())
    );
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
    let output = parse_module("zero = @Zero\nok = @Ok(1)\ntruth = true\nabsent = undefined\n");
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
        assert!(matches!(
            row.entries.as_slice(),
            [RowEntry::Tag { name, .. }] if name == tag
        ));
        if binding == "zero" {
            assert!(scheme.row_vars.is_empty());
        } else {
            assert_eq!(scheme.row_vars.len(), 1);
            assert!(matches!(
                row.entries.as_slice(),
                [RowEntry::Tag {
                    payload,
                    ..
                }] if matches!(
                    payload.as_slice(),
                    [Type::Variant(payload_row)]
                        if payload_row.tail == RowTail::Var(scheme.row_vars[0])
                )
            ));
        }
    }

    assert_eq!(
        render_top_level_value(&mut checker, "truth"),
        Some("true".to_owned())
    );
    assert_eq!(
        checker.infer_top_level_value("absent"),
        Some(named("Undefined"))
    );
}

#[test]
fn bare_uppercase_values_do_not_infer_tags() {
    let output = parse_module("Answer = 42\nresolved = Answer\nmissing = Missing\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "resolved"),
        Some("42".to_owned())
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
        "direction = n ?>\n  0 => @Zero\n  _ => @Pos\nvalue : @{@Zero, @Pos} = direction\nn = _\n",
        "direction = n ?>\n  0 => @Zero\n  _ => @Pos\nvalue : @{@Zero, @Pos, ..} = direction\nn = _\n",
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
fn recursive_named_variant_matches_and_accepts_inline_constructors() {
    let source = concat!(
        "Document = @{@Null, @Bool(Bool), @Int(Int), @Float(Float), @Text(Text), ",
        "@Array(Array(Document)), @Object(Map(Text, Document))}\n",
        "arrayValue : Document = @Array([@Null, @Int(5)])\n",
        "objectValue : Document = @Object(Map.from([(\"a\", @Int(1))]))\n",
        "matched = arrayValue ?>\n",
        "  @Null => \"null\"\n",
        "  @Bool(_) => \"bool\"\n",
        "  @Int(_) => \"int\"\n",
        "  @Float(_) => \"float\"\n",
        "  @Text(_) => \"text\"\n",
        "  @Array(_) => \"array\"\n",
        "  @Object(_) => \"object\"\n",
        "local = () =>\n",
        "  localValue : Document = @Null\n",
        "  localValue\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "arrayValue", 0))
            .map(Type::render),
        Some("Document".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "localValue", 0))
            .map(Type::render),
        Some("Document".to_owned())
    );
}

#[test]
fn match_arm_pattern_bindings_record_inferred_types() {
    let source = concat!(
        "Document = @{@Null, @Bool(Bool), @Int(Int), @Float(Float), @Text(Text), ",
        "@Array(Array(Document)), @Object(Map(Text, Document))}\n",
        "subject : Document = @Null\n",
        "described = subject ?>\n",
        "  @Object(objectFields) => 1\n",
        "  @Array(elements) => 2\n",
        "  _ => 0\n",
        "outcome : Result(Int, @{@Nope}) = @Ok(1)\n",
        "unwrapped = outcome ?>\n",
        "  @Ok(okValue) => okValue\n",
        "  @Err(_) => 0\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "objectFields", 0))
            .map(Type::render),
        Some("Map(Text, Document)".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "elements", 0))
            .map(Type::render),
        Some("Array(Document)".to_owned())
    );
    assert_eq!(
        check
            .type_at(nth_span(source, "okValue", 0))
            .map(Type::render),
        Some("Int".to_owned())
    );
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
fn optional_nullable_match_exhaustiveness_requires_both_empty_values() {
    let complete = parse_module(concat!(
        "source : ?Text? = undefined\n",
        "result = source ?>\n",
        "  undefined => 0\n",
        "  null => 1\n",
        "  text => 2\n",
    ));
    let complete_check = check_module(&complete.module);
    assert!(
        !has_diagnostic_code(&complete_check.diagnostics, codes::ty::NON_EXHAUSTIVE_MATCH),
        "complete optional/nullable match produced diagnostics: {:?}",
        complete_check.diagnostics
    );

    let missing = parse_module(concat!(
        "source : ?Text? = undefined\n",
        "result = source ?>\n",
        "  undefined => 0\n",
        "  text => 1\n",
    ));
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::NON_EXHAUSTIVE_MATCH),
        1
    );
}

#[test]
fn optional_nullable_match_payload_binds_inner_type() {
    let output = parse_module(concat!(
        "source : ?Text? = \"x\"\n",
        "result : Text = source ?>\n",
        "  undefined => \"absent\"\n",
        "  null => \"empty\"\n",
        "  text => text\n",
    ));
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "payload binder should be Text after peeling: {:?}",
        check.diagnostics
    );
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
fn match_result_inference_reports_mixed_arm_types() {
    let output = parse_module(
        "result = source ?>\n  @Ok(_) => 1\n  @Err(_) => \"no\"\nvalue : Text = result\n",
    );
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::INCOMPATIBLE_MATCH_ARMS),
        1,
        "mixed match arm types should report incompatible arms: {:?}",
        check.diagnostics
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
    let accepted = parse_module("value : Array(Int) = [1, 2, 3]\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible array literal unexpectedly produced type.mismatch"
    );

    let mismatch = parse_module("value : Array(Text) = [1, 2, 3]\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        3
    );
}

#[test]
fn inferred_array_identifier_values_are_checked_against_annotations() {
    let output = parse_module("nums = [1, 2]\nvalue : Array(Text) = nums\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn array_element_types_reuse_structural_type_comparison() {
    let accepted = parse_module("value : Array((Int, Text)) = [(1, \"a\")]\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible nested array literal unexpectedly produced type.mismatch"
    );

    let mismatch = parse_module("value : Array((Int, Int)) = [(1, \"a\")]\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1
    );
}

#[test]
fn array_literals_report_per_element_mismatches() {
    let output = parse_module("value : Array(Text) = [\"a\", 2, \"b\"]\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn array_inference_defers_empty_literals() {
    let output = parse_module("value : Array(Int) = []\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "empty array unexpectedly produced type.mismatch"
    );
}

#[test]
fn array_spread_unifies_element_types() {
    let output = parse_module("xs = [1, 2]\nys : Array(Int) = [..xs, 3]\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "compatible array spread unexpectedly produced type.mismatch: {:?}",
        check.diagnostics
    );
}

#[test]
fn array_spread_mismatch_reports_type_error() {
    let text_into_int = parse_module("xs = [\"a\"]\nys : Array(Int) = [..xs, 1]\n");
    let text_into_int_check = check_module(&text_into_int.module);
    assert!(
        matching_codes(&text_into_int_check.diagnostics, codes::ty::MISMATCH) >= 1,
        "spreading Array(Text) into Array(Int) should mismatch"
    );

    let non_array = parse_module("ys : Array(Int) = [..\"nope\", 1]\n");
    let non_array_check = check_module(&non_array.module);
    assert!(
        matching_codes(&non_array_check.diagnostics, codes::ty::MISMATCH) >= 1,
        "spreading Text into array literal should mismatch"
    );
}

#[test]
fn array_push_result_type_is_array() {
    // Annotate the seed so push's result is `Array(Int)`, not an open literal
    // union `Array(1 | 2 | ..)` from unannotated `[1]` / `2`.
    let output = parse_module("xs : Array(Int) = [1]\nys = xs.push(2)\n");
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "array push should type-check: {:?}",
        check.diagnostics
    );

    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let scheme = checker.infer_top_level_scheme("ys").expect("scheme for ys");
    assert_eq!(scheme.ty.render(), "Array(Int)");
}

#[test]
fn array_push_on_non_array_reports_error() {
    let int_receiver = parse_module("value = 1.push(2)\n");
    let int_check = check_module(&int_receiver.module);
    assert!(
        matching_codes(&int_check.diagnostics, codes::ty::MISSING_FIELD) >= 1
            || !int_check.diagnostics.is_empty(),
        "1.push should error: {:?}",
        int_check.diagnostics
    );

    let text_receiver = parse_module("value = \"a\".push(\"b\")\n");
    let text_check = check_module(&text_receiver.module);
    assert!(
        matching_codes(&text_check.diagnostics, codes::ty::MISSING_FIELD) >= 1
            || !text_check.diagnostics.is_empty(),
        "Text.push should error: {:?}",
        text_check.diagnostics
    );
}

#[test]
fn set_literals_are_checked_against_annotations() {
    let accepted = parse_module("value : Set(Int) = @{1, 2, 3}\n");
    let accepted_check = check_module(&accepted.module);
    assert!(
        !has_diagnostic_code(&accepted_check.diagnostics, codes::ty::MISMATCH),
        "compatible set literal unexpectedly produced type.mismatch"
    );

    let mismatch = parse_module("value : Set(Text) = @{1, 2, 3}\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        3
    );
}

#[test]
fn inferred_set_identifier_values_are_checked_against_annotations() {
    let output = parse_module("nums = @{1, 2}\nvalue : Set(Text) = nums\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn set_literals_report_per_element_mismatches() {
    let output = parse_module("value : Set(Text) = @{\"a\", 2, \"b\"}\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn set_inference_defers_empty_tag_and_spread_literals() {
    for source in [
        "value : Set(Int) = @{}\n",
        "value : Set(Int) = @{@Red, @Green}\n",
        "other = @{2}\nvalue : Set(Int) = @{..other, 1}\n",
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
fn map_empty_global_is_generalized() {
    let output = parse_module(
        "texts : Map(Text, Int) = Map.empty()\n\
         ints : Map(Int, Text) = Map.empty()\n",
    );
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
}

#[test]
fn non_core_named_types_reject_cross_named_and_structural_values() {
    // H6: Data/Map (and other non-core nominals) must not act as top/bottom.
    for source in [
        "d : Data = \"hello\"\n",
        "d : Data = 42\n",
        "d : Data = { a: 1 }\n",
        "d : Data = \"hello\"\nn : Int = d\n",
        "m : Map = Map.empty()\nx : Int = m\n",
        "m : Map(Text, Int) = Map.empty()\nx : Int = m\n",
        "x : Int = { a: 1 }\n",
        "x : Int = (1, 2)\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} should produce type.mismatch: {:?}",
            check.diagnostics
        );
    }
}

#[test]
fn data_decode_and_map_set_result_typed_uses_still_check() {
    // Dynamic Data enters via format decode; collection/result annotations work.
    let output = parse_module(
        "d : Data = Json.decode(\"\\\"hello\\\"\", Data)?!\n\
         m : Map(Text, Int) = Map.empty()\n\
         s : Set(Int) = @{1, 2}\n\
         r : Result(Int, Text) = @Ok(1)\n",
    );
    let check = check_module_with_host_globals(&output.module, &format_method_host_globals());

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "typed Data/Map/Set/Result uses unexpectedly mismatched: {:?}",
        check.diagnostics
    );
}

#[test]
fn map_from_infers_map_key_and_value_types() {
    let output = parse_module("m = Map.from([(\"a\", 1)])\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "m"),
        Some("Map(\"a\", 1)".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn map_name_in_value_position_is_a_type_value() {
    // `Map` is a type artifact: used as a bare value it is a type value
    // (`Deferred`), exactly like `Array`, not a `{ empty, from }` record.
    let output = parse_module("m = Map\na = Array\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    // A type value is `Deferred`, so no published value type (`None`) — exactly
    // like `Array`. A namespace record would publish a `{ empty, from }` type.
    assert_eq!(render_top_level_value(&mut checker, "m"), None);
    assert_eq!(render_top_level_value(&mut checker, "a"), None);
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn json_name_in_value_position_is_a_type_value() {
    let output = parse_module("x = Json\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(render_top_level_value(&mut checker, "x"), None);
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn json_encode_types_through_statics() {
    // `Json.encode` resolves through the statics table (not a namespace record):
    // `(a) -> Text`.
    let output = parse_module("t = Json.encode(1)\n");
    let host = crate::HostGlobals::default().with_statics(vec![(
        "Json".to_owned(),
        vec![(
            "encode".to_owned(),
            function(vec![variable("a")], named("Text")),
        )],
    )]);
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker =
        Checker::with_module_and_host_globals(known_types, type_definitions, &output.module, &host);

    assert_eq!(
        render_top_level_value(&mut checker, "t"),
        Some("Text".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn encode_method_types_like_format_static_form() {
    let method = "user = { name: \"Ada\" }\nencoded = user.encode(Json)\n";
    let static_form = "user = { name: \"Ada\" }\nencoded = Json.encode(user)\n";
    let host = format_encode_host_globals();

    let method_ty = checked_binding_type(method, "encoded", &host);
    let static_ty = checked_binding_type(static_form, "encoded", &host);

    assert_eq!(method_ty.render(), "Text");
    assert_eq!(method_ty, static_ty);
}

#[test]
fn encode_method_records_member_span_type() {
    let source = "Y = { y: Int }\n\
                  y: Y = { y: 2 }\n\
                  encoded = y.encode(Yaml)\n";
    let output = parse_module(source);
    let checked = check_module_with_host_globals(&output.module, &format_encode_host_globals());
    assert!(
        checked.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        checked.diagnostics
    );

    let ty = checked
        .type_at(nth_span(source, "encode", 1))
        .expect("encode member has an inferred type");

    assert_eq!(ty.render(), "Yaml -> Text");
}

#[test]
fn decode_method_records_member_span_type() {
    let source = "User = { name: Text }\n\
                  text = \"{}\"\n\
                  decoded = text.decode(Json, User)\n";
    let output = parse_module(source);
    let checked = check_module_with_host_globals(&output.module, &format_method_host_globals());
    assert!(
        checked.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        checked.diagnostics
    );

    let ty = checked
        .type_at(nth_span(source, "decode", 1))
        .expect("decode member has an inferred type");

    assert_eq!(
        ty.render(),
        "(Json, User) -> Result({ name: Text }, @Decode(Text))"
    );
}

#[test]
fn encode_method_accepts_named_annotation_receiver() {
    let repro = "Y = { y: Int }\n\
                 y: Y = { y: 2 }\n\
                 y.encode(Yaml)\n";
    let output = parse_module(repro);
    let checked = check_module_with_host_globals(&output.module, &format_encode_host_globals());
    assert!(
        checked.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        checked.diagnostics
    );

    let method = "Y = { y: Int }\n\
                  y: Y = { y: 2 }\n\
                  encoded = y.encode(Yaml)\n";
    let static_form = "Y = { y: Int }\n\
                       y: Y = { y: 2 }\n\
                       encoded = Yaml.encode(y)\n";
    let host = format_encode_host_globals();

    let method_ty = checked_binding_type(method, "encoded", &host);
    let static_ty = checked_binding_type(static_form, "encoded", &host);

    assert_eq!(method_ty.render(), "Text");
    assert_eq!(method_ty, static_ty);
}

#[test]
fn encode_method_keeps_receiver_encode_field_semantics() {
    let source = "user = { name: \"Ada\", encode: (format) => 1 }\n\
                  encoded = user.encode(Json)\n";
    let ty = checked_binding_type(source, "encoded", &format_encode_host_globals());

    assert_eq!(ty.render(), "1");
}

#[test]
fn encode_method_keeps_named_receiver_encode_field_semantics() {
    let source = "Y = { y: Int, encode: (a) -> Int }\n\
                  y: Y = { y: 2, encode: (format) => 1 }\n\
                  encoded = y.encode(Yaml)\n";
    let ty = checked_binding_type(source, "encoded", &format_encode_host_globals());

    assert_eq!(ty.render(), "Int");
}

#[test]
fn encode_method_accepts_data_receiver_from_decode_propagation() {
    let source = "text = \"name: Ada\"\n\
                  data = Yaml.decode(text)?^\n\
                  encoded = data.encode(Yaml)\n";
    let ty = checked_binding_type(source, "encoded", &format_encode_host_globals());

    assert_eq!(ty.render(), "Text");
}

#[test]
fn encode_method_requires_format_argument() {
    let output = parse_module("value = 1\nencoded = value.encode()\n");
    let checked = check_module_with_host_globals(&output.module, &format_encode_host_globals());

    assert_eq!(
        matching_codes(&checked.diagnostics, codes::ty::ENCODE_FORMAT),
        1
    );
}

#[test]
fn encode_method_rejects_non_format_first_argument() {
    let output = parse_module("value = 1\nencoded = value.encode(value)\n");
    let checked = check_module_with_host_globals(&output.module, &format_encode_host_globals());

    assert_eq!(
        matching_codes(&checked.diagnostics, codes::ty::ENCODE_FORMAT),
        1
    );
}

#[test]
fn encode_method_extra_arguments_use_static_arity_diagnostic() {
    let output = parse_module("value = 1\nencoded = value.encode(Json, 2)\n");
    let checked = check_module_with_host_globals(&output.module, &format_encode_host_globals());

    assert_eq!(
        matching_codes(&checked.diagnostics, codes::ty::ENCODE_FORMAT),
        0
    );
    assert_eq!(matching_codes(&checked.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn encode_method_accepts_non_json_format() {
    let source = "value = { name: \"Ada\" }\nencoded = value.encode(Yaml)\n";
    let ty = checked_binding_type(source, "encoded", &format_encode_host_globals());

    assert_eq!(ty.render(), "Text");
}

#[test]
fn map_get_infers_optional_value_type() {
    let output = parse_module("m = Map.from([(\"a\", 1)])\nvalue = m.get(\"a\")\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "value"),
        Some("?1".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn map_get_rejects_wrong_key_type() {
    let output = parse_module("m : Map(Text, Int) = Map.empty()\nvalue = m.get(1)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn map_index_infers_optional_value_type() {
    let output = parse_module("m = Map.from([(\"a\", 1)])\nvalue = m[\"a\"]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "value"),
        Some("?1".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn map_index_rejects_wrong_key_type() {
    let output = parse_module("m : Map(Text, Int) = Map.empty()\nvalue = m[1]\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn map_unknown_method_reports_missing_field() {
    let output = parse_module("m : Map(Text, Int) = Map.empty()\nvalue = m.nope()\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD),
        1
    );
}

#[test]
fn map_wrong_arity_application_lowers_like_ordinary_apply() {
    let output = parse_module("value : Map(Text) = Map.empty()\n");
    let check = check_module(&output.module);
    let lowering = lower_annotation(&output.module, annotation(&output.module, "value"));

    assert!(check.diagnostics.is_empty());
    assert_eq!(lowering.ty.render(), "Map(Text)");
    assert!(lowering.diagnostics.is_empty());
}

#[test]
fn map_method_field_query_returns_typed_methods() {
    let fields =
        record_fields(&apply(named("Map"), vec![named("Text"), named("Int")])).expect("Map fields");
    let get = fields
        .iter()
        .find(|field| field.name == "get")
        .expect("get method");
    let entries = fields
        .iter()
        .find(|field| field.name == "entries")
        .expect("entries method");

    assert_eq!(
        get.ty.render(),
        "Text -> ?Int",
        "get should preserve key/value type args"
    );
    assert_eq!(entries.ty.render(), "() -> Array((Text, Int))");
}

#[test]
fn map_method_names_match_evaluator_dispatch() {
    assert_eq!(crate::ty::MAP_METHOD_NAMES, aven_eval::MAP_METHOD_NAMES);
}

#[test]
fn text_method_names_match_evaluator_dispatch() {
    assert_eq!(crate::ty::TEXT_METHOD_NAMES, aven_eval::TEXT_METHOD_NAMES);
}

#[test]
fn text_method_field_query_returns_typed_methods() {
    let fields = record_fields(&named("Text")).expect("Text fields");
    let is_empty = fields
        .iter()
        .find(|field| field.name == "isEmpty")
        .expect("isEmpty method");
    let contains = fields
        .iter()
        .find(|field| field.name == "contains")
        .expect("contains method");
    let repeat = fields
        .iter()
        .find(|field| field.name == "repeat")
        .expect("repeat method");
    let split_on = fields
        .iter()
        .find(|field| field.name == "splitOn")
        .expect("splitOn method");
    let replace_each = fields
        .iter()
        .find(|field| field.name == "replaceEach")
        .expect("replaceEach method");

    assert_eq!(is_empty.ty.render(), "() -> Bool");
    assert_eq!(contains.ty.render(), "Text -> Bool");
    assert_eq!(repeat.ty.render(), "Int -> Text");
    assert_eq!(split_on.ty.render(), "Text -> Array(Text)");
    assert_eq!(replace_each.ty.render(), "(Text, Text) -> Text");
    assert!(
        !fields
            .iter()
            .any(|field| field.name == "length" || field.name == "len"),
        "Text must not expose length/len"
    );
}

#[test]
fn text_methods_type_check_and_reject_mismatches() {
    let ok = parse_module(concat!(
        "t : Text = \"hi\"\n",
        "a = t.isEmpty()\n",
        "b = t.contains(\"h\")\n",
        "c = t.startsWith(\"h\")\n",
        "d = t.endsWith(\"i\")\n",
        "e = t.trim()\n",
        "f = t.trimStart()\n",
        "g = t.trimEnd()\n",
        "h = t.toLower()\n",
        "i = t.toUpper()\n",
        "j = t.replaceEach(\"h\", \"H\")\n",
        "k = t.replaceFirst(\"h\", \"H\")\n",
        "l = t.dropPrefix(\"h\")\n",
        "m = t.dropSuffix(\"i\")\n",
        "n = t.repeat(2)\n",
        "o = t.splitOn(\"\")\n",
        "p = [\"a\", \"b\"].joinWith(\", \")\n",
    ));
    let ok_check = check_module(&ok.module);
    assert!(
        ok_check.diagnostics.is_empty(),
        "text methods should type-check: {:?}",
        ok_check.diagnostics
    );

    let mismatch = parse_module("t : Text = \"hi\"\nvalue = t.repeat(\"x\")\n");
    let mismatch_check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&mismatch_check.diagnostics, codes::ty::MISMATCH),
        1,
        "t.repeat(\"x\") should be a type mismatch: {:?}",
        mismatch_check.diagnostics
    );

    let unknown = parse_module("t : Text = \"hi\"\nvalue = t.nope()\n");
    let unknown_check = check_module(&unknown.module);
    assert_eq!(
        matching_codes(&unknown_check.diagnostics, codes::ty::MISSING_FIELD),
        1,
        "unknown Text method should be missing-field: {:?}",
        unknown_check.diagnostics
    );

    let non_text = parse_module("value = 1.isEmpty()\n");
    let non_text_check = check_module(&non_text.module);
    assert!(
        matching_codes(&non_text_check.diagnostics, codes::ty::MISSING_FIELD) >= 1
            || !non_text_check.diagnostics.is_empty(),
        "1.isEmpty should error: {:?}",
        non_text_check.diagnostics
    );
}

#[test]
fn method_calls_report_missing_fields_on_known_receivers() {
    for (source, field) in [
        ("n = 42\nr: Text = n.bar()\n", "bar"),
        ("r = { a: 1 }\nx: Int = r.get(\"a\")\n", "get"),
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert_eq!(
            matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD),
            1,
            "{source} should report type.missing-field: {:?}",
            check.diagnostics
        );
        let diagnostic = check
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::MISSING_FIELD))
            .expect("missing-field diagnostic");
        assert_eq!(
            &source[diagnostic.labels[0].span.start..diagnostic.labels[0].span.end],
            field,
            "{source} should label the missing field",
        );
    }
}

#[test]
fn known_method_calls_continue_to_type_check() {
    let output = parse_module(concat!(
        "text: Text = \"hi\"\n",
        "trimmed = text.trim()\n",
        "map: Map(Text, Int) = Map.empty()\n",
        "value = map.get(\"key\")\n",
        "items: Array(Int) = [1]\n",
        "updated = items.push(2)\n",
    ));
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "known method calls should type-check: {:?}",
        check.diagnostics
    );
}

#[test]
fn builtin_method_tables_cover_sets_and_temporals() {
    let set = apply(named("Set"), vec![named("Int")]);
    assert_eq!(
        crate::ty::builtin_collection_method_type(&set, "has")
            .expect("Set.has method")
            .render(),
        "Int -> Bool"
    );

    for (receiver, method) in [
        ("Date", "format"),
        ("Time", "format"),
        ("DateTime", "instant"),
        ("Instant", "dateTime"),
        ("Duration", "plus"),
    ] {
        assert!(
            crate::ty::builtin_collection_method_type(&named(receiver), method).is_some(),
            "{receiver}.{method} should have a method type",
        );
        assert!(
            crate::ty::builtin_collection_method_type(&named(receiver), "missing").is_none(),
            "{receiver}.missing should not have a method type",
        );
    }
}

#[test]
fn method_calls_defer_for_open_record_receivers() {
    let output = parse_module("record: { a: Int, .. } = unknown\nvalue = record.missing()\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
        "open record method access should defer: {:?}",
        check.diagnostics
    );
}

#[test]
fn array_join_with_only_on_array_of_text() {
    let ok = parse_module("parts : Array(Text) = [\"a\", \"b\"]\njoined = parts.joinWith(\",\")\n");
    let ok_check = check_module(&ok.module);
    assert!(
        ok_check.diagnostics.is_empty(),
        "Array(Text).joinWith should type-check: {:?}",
        ok_check.diagnostics
    );

    let fields =
        record_fields(&apply(named("Array"), vec![named("Text")])).expect("Array(Text) fields");
    let join = fields
        .iter()
        .find(|field| field.name == "joinWith")
        .expect("joinWith on Array(Text)");
    assert_eq!(join.ty.render(), "Text -> Text");

    let int_fields =
        record_fields(&apply(named("Array"), vec![named("Int")])).expect("Array(Int) fields");
    assert!(
        !int_fields.iter().any(|field| field.name == "joinWith"),
        "Array(Int) must not advertise joinWith"
    );

    let bad = parse_module("xs : Array(Int) = [1, 2]\nvalue = xs.joinWith(\",\")\n");
    let bad_check = check_module(&bad.module);
    assert_eq!(
        matching_codes(&bad_check.diagnostics, codes::ty::MISSING_FIELD),
        1,
        "Array(Int).joinWith should be missing-field: {:?}",
        bad_check.diagnostics
    );
}

#[test]
fn map_grouping_example_checks() {
    let output = parse_module(concat!(
        "words = [\"red\", \"blue\", \"red\"]\n",
        "count = (items: Array(Text), index: Int, acc: Map(Text, Int)) =>\n",
        "  next = items[index]\n",
        "  next ?>\n",
        "    undefined => acc\n",
        "    _ =>\n",
        "      word : Text = next ?? \"\"\n",
        "      count(items, index + 1, acc.set(word, (acc.get(word) ?? 0) + 1))\n",
        "counts : Map(Text, Int) = count(words, 0, Map.empty())\n",
    ));
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
}

#[test]
fn variant_values_are_checked_against_annotations() {
    for source in [
        "value : @{@Ok(Int), @Err(Text)} = @Ok(1)\n",
        "value : @{@Done} = @Done\n",
        "value : @Point(Int) = @Point(1)\n",
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
        "p : @Point(Int, Int) = @Point(1, \"x\")\n",
        "q : @Point(Int) = 42\n",
        "r : @Point(Int, Int) = @Point(1)\n",
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
fn singleton_payload_variant_annotations_support_matching() {
    let output = parse_module(
        "point : @Point(Int) = @Point(1)\n\
         coordinate = point ?> @Point(value) => value\n\
         checked : Int = coordinate\n",
    );
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
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
    // Recursive / self-application stays deferred (no concrete result type).
    for source in [
        "f = (x) => f(x)\nr = f(1)\nvalue : Text = r\n",
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
fn identity_function_value_mismatches_scalar_annotation() {
    // A solved function type is nominal-incompatible with `Text` (was silent
    // when Named↔Function fell through the comparator).
    let output = parse_module("f = (x) => x\nx = f\nvalue : Text = x\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        1,
        "expected type.mismatch for function into Text: {:?}",
        check.diagnostics
    );
}

#[test]
fn builtin_operator_results_are_inferred() {
    for source in [
        "value : Float = 42\n",
        "value : Int = 1\n",
        "sum : Int = 1 + 2\n",
        "flo : Float = 1 + 2\n",
        "value : Text = \"a\" + \"b\"\n",
        "value : Bool = 1 == 2\n",
        "value : Bool = 1 < 2\n",
        "left : Bool = true\nvalue : Bool = left == true\n",
        "text : Text = \"a\"\nvalue : Bool = text == \"b\"\n",
        "left : Bool = true\nright : Bool = false\nvalue : Bool = left && right\n",
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
        "result = 1 + 2\nvalue : Text = result\n",
        "result = \"a\" + \"b\"\nvalue : Int = result\n",
        "result = 1 < 2\nvalue : Text = result\n",
        "left : Bool = true\nright : Bool = false\nresult = left && right\nvalue : Text = result\n",
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
fn numeric_binary_literals_synthesize_folded_singleton() {
    let output = parse_module("n = 1 + 2\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "n"),
        Some("3".to_owned())
    );
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
        render_top_level_value(&mut checker, "other"),
        Some("{ id: 1, name: \"Ada\" }".to_owned())
    );
}

#[test]
fn infer_value_synthesizes_closed_record_transform_types() {
    let output = parse_module(
        "base = { x: 1, y: \"yes\", old: true }\n\
         added = { ..base, z: 2 }\n\
         replaced = { ..base, y := \"changed\" }\n\
         deleted = { ..base, -y }\n\
         renamed = { ..base, old -> flag }\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "added"),
        Some("{ x: 1, y: \"yes\", old: true, z: 2 }".to_owned())
    );
    assert_eq!(
        render_top_level_value(&mut checker, "replaced"),
        Some("{ x: 1, y: \"changed\", old: true }".to_owned())
    );
    assert_eq!(
        render_top_level_value(&mut checker, "deleted"),
        Some("{ x: 1, old: true }".to_owned())
    );
    assert_eq!(
        render_top_level_value(&mut checker, "renamed"),
        Some("{ x: 1, y: \"yes\", flag: true }".to_owned())
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
        render_top_level_value(&mut checker, "union"),
        Some("{ x: 1, y: \"ok\" }".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn infer_value_pipe_union_like_set_literals() {
    let output = parse_module(
        "pipe = \"r\" | \"w\" | \"rw\"\n\
         set = @{\"r\", \"w\", \"rw\"}\n\
         left = @{1}\n\
         right = @{2}\n\
         set_operands = left | right\n\
         set_literal = @{1, 2}\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    let pipe = render_top_level_value(&mut checker, "pipe");
    let set = render_top_level_value(&mut checker, "set");
    let set_operands = render_top_level_value(&mut checker, "set_operands");
    let set_literal = render_top_level_value(&mut checker, "set_literal");

    assert_eq!(pipe, set);
    assert_eq!(set_operands, set_literal);
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn optional_spread_patch_fields_preserve_base_shape() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         user : User = { name: \"Grace\", email: \"grace@example.test\" }\n\
         patch : partial(User) = { name: \"Ada\" }\n\
         fresh = { ..user, ..patch }\n\
         update = (u: User, patch: partial(User)) => { ..u, ..patch }\n\
         updated = update(user, patch)\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let user_type = Type::Record(Row {
        entries: vec![field("name", named("Text")), field("email", named("Text"))],
        tail: RowTail::Closed,
    });

    assert_eq!(
        checker.infer_top_level_value("fresh"),
        Some(user_type.clone())
    );
    assert_eq!(checker.infer_top_level_value("updated"), Some(user_type));
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn optional_spread_patch_widens_literal_defaults_to_patch_base() {
    // N-model idiom: literal-typed defaults are join artifacts, so patching
    // them with an optional base-typed field widens the merged field to the
    // base instead of reporting `type.wide-value-into-literal-union`.
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         complete = (draft: partial(User)): User => { name: \"anon\", email: \"a@b.c\", ..draft }\n\
         user = complete({ name: \"Dave\" })\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let user_type = Type::Record(Row {
        entries: vec![field("name", named("Text")), field("email", named("Text"))],
        tail: RowTail::Closed,
    });

    assert_eq!(checker.infer_top_level_value("user"), Some(user_type));
    assert!(checker.diagnostics.is_empty(), "{:?}", checker.diagnostics);

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
}

#[test]
fn top_level_coalesce_over_comptime_optional_field_infers_payload_type() {
    let output = parse_module(
        "User = { name: Text, email: Text, joined: Text }\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         Draft = partial(User)\n\
         draft: Draft = { name: \"Dave\" }\n\
         pendingEmail = draft.email ?? \"no email yet\"\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("pendingEmail"),
        Some(named("Text"))
    );
    assert!(checker.diagnostics.is_empty(), "{:?}", checker.diagnostics);

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
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
fn unused_result_warns_for_non_final_top_level_bare_expression() {
    let source = "mustUse()\n1\n";
    let check = check_with_must_use_global(source);

    assert_eq!(check.diagnostics.len(), 1);
    let diagnostic = &check.diagnostics[0];
    assert_eq!(diagnostic.severity, Severity::Warning);
    assert_eq!(diagnostic.code.as_deref(), Some(codes::ty::UNUSED_RESULT));
    assert_eq!(diagnostic.labels.len(), 1);
    assert_eq!(diagnostic.labels[0].span, Span::new(0, "mustUse()".len()));
    assert_eq!(diagnostic.labels[0].message, "this `Result` is unused");
    assert_eq!(
        diagnostic.notes,
        vec![
            "unwrap it with `?!` (panic on `@Err`), propagate it with `?^`, or discard it explicitly with `_ =`."
        ]
    );
}

#[test]
fn unused_result_warns_for_non_final_block_bare_expression() {
    let source = "value =\n  mustUse()\n  1\n";
    let check = check_with_must_use_global(source);
    let start = source
        .find("mustUse()")
        .expect("expected mustUse call in source");

    assert_eq!(check.diagnostics.len(), 1);
    assert_eq!(
        check.diagnostics[0].code.as_deref(),
        Some(codes::ty::UNUSED_RESULT)
    );
    assert_eq!(
        check.diagnostics[0].labels[0].span,
        Span::new(start, start + "mustUse()".len())
    );
}

#[test]
fn propagate_requires_result_final_expression_with_repair_note() {
    let source = "ReadError = @{@Read(Text)}\nf = (path: Text) =>\n  text = read(path)?^\n  text\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &fallible_read_globals());

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::PROPAGATE_NEEDS_RESULT),
        1
    );
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::PROPAGATE_NEEDS_RESULT))
        .expect("expected propagate-needs-result diagnostic");
    assert_eq!(
        diagnostic.labels[0].message,
        "this is the function's result, but `?^` requires it to be a Result"
    );
    assert_eq!(
        diagnostic.notes,
        vec![
            "wrap the final expression in `@Ok(...)`, or handle the errors instead of propagating them"
        ]
    );
}

#[test]
fn propagate_with_explicit_ok_infers_result_error_union() {
    let source =
        "ReadError = @{@Read(Text)}\nf = (path: Text) =>\n  text = read(path)?^\n  @Ok(text)\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &fallible_read_globals());

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "f", 0)).map(Type::render),
        Some("Text -> Result(Text, @Read(Text))".to_owned())
    );
}

#[test]
fn propagate_with_named_host_error_records_result_type() {
    let source = concat!(
        "f = (text: Text) =>\n",
        "  value = parse(text)?^\n",
        "  @Ok(\"got ${value}\")\n",
    );
    let output = parse_module(source);
    let globals = vec![(
        "parse".to_owned(),
        build::function(
            vec![build::text()],
            build::result(build::int(), build::text()),
        ),
    )];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "f", 0)).map(Type::render),
        Some("Text -> Result(Text, Text)".to_owned())
    );
}

#[test]
fn propagate_with_annotated_named_error_records_result_type() {
    let source = concat!(
        "f = (flag: Bool) =>\n",
        "  r: Result(Int, Text) = @Ok(1)\n",
        "  value = r?^\n",
        "  @Ok(value + 1)\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "f", 0)).map(Type::render),
        Some("Bool -> Result(Int, Text)".to_owned())
    );
}

#[test]
fn propagate_error_union_prefers_named_supertype_over_literal_row() {
    let source = concat!(
        "f = () =>\n",
        "  literal = literalError()?^\n",
        "  named: Result(Int, Text) = @Ok(1)\n",
        "  value = named?^\n",
        "  @Ok(literal + value)\n",
    );
    let output = parse_module(source);
    let globals = vec![(
        "literalError".to_owned(),
        build::function(
            vec![],
            build::result(build::int(), build::text_literals(&["no"])),
        ),
    )];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "f", 0)).map(Type::render),
        Some("() -> Result(Int, Text)".to_owned())
    );
}

#[test]
fn incompatible_propagated_error_union_defers() {
    let source = concat!(
        "f = () =>\n",
        "  ignored = textError()?^\n",
        "  value = intError()?^\n",
        "  @Ok(value)\n",
    );
    let output = parse_module(source);
    let globals = vec![
        (
            "textError".to_owned(),
            build::function(vec![], build::result(build::int(), build::text())),
        ),
        (
            "intError".to_owned(),
            build::function(vec![], build::result(build::int(), build::int())),
        ),
    ];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert!(check.type_at(nth_span(source, "f", 0)).is_none());
}

#[test]
fn propagate_error_union_keeps_literal_rows() {
    let source = concat!(
        "f = () =>\n",
        "  first = firstError()?^\n",
        "  second = secondError()?^\n",
        "  @Ok(first + second)\n",
    );
    let output = parse_module(source);
    let globals = vec![
        (
            "firstError".to_owned(),
            build::function(
                vec![],
                build::result(build::int(), build::text_literals(&["first"])),
            ),
        ),
        (
            "secondError".to_owned(),
            build::function(
                vec![],
                build::result(build::int(), build::text_literals(&["second"])),
            ),
        ),
    ];
    let check = check_module_with_globals(&output.module, &globals);

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "f", 0)).map(Type::render),
        Some("() -> Result(Int, \"first\" | \"second\")".to_owned())
    );
}

#[test]
fn propagate_unions_different_named_error_variant_rows() {
    let source = concat!(
        "ReadError = @{@Read(Text)}\n",
        "ParseError = @{@Parse(Text)}\n",
        "load = (path: Text) =>\n",
        "  text = read(path)?^\n",
        "  value = parse(text)?^\n",
        "  @Ok(value)\n",
    );
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &read_and_parse_globals());

    assert!(
        check.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        check.diagnostics
    );
    assert_eq!(
        check.type_at(nth_span(source, "load", 0)).map(Type::render),
        Some("Text -> Result(Int, @Read(Text) | @Parse(Text))".to_owned())
    );
}

#[test]
fn propagate_on_concrete_non_result_diagnoses_subject() {
    let source = "value = 5?^\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::PROPAGATE_NOT_RESULT),
        1
    );
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::PROPAGATE_NOT_RESULT))
        .expect("expected propagate-not-result diagnostic");
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "5", 0));
    assert_eq!(
        diagnostic.notes,
        vec!["`?^`/`?!` operate on `Result(ok, err)` values"]
    );
}

#[test]
fn annotated_result_rejects_non_fitting_propagated_error_at_site() {
    let source = concat!(
        "ReadError = @{@Read}\n",
        "ParseError = @{@Parse}\n",
        "load : Text -> Result(Int, ReadError)\n",
        "load = (path) =>\n",
        "  text = read(path)?^\n",
        "  value = parse(text)?^\n",
        "  @Ok(value)\n",
    );
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &read_and_parse_unit_error_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::MISMATCH))
        .expect("expected type mismatch diagnostic");
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "?^", 1));
}

#[test]
fn file_open_style_propagation_requires_result_final_expression() {
    let source = concat!(
        "IoError = @{@Io(Text)}\n",
        "ReadError = @{@Read(Text)}\n",
        "f = (path: Text) =>\n",
        "  h = File.open(path, \"r\")?^\n",
        "  h.readAll()?^\n",
        "x = f(\"/nonexistent\")\n",
        "y = x + \"!\"\n",
    );
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &file_style_globals());

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::PROPAGATE_NEEDS_RESULT),
        1
    );
}

#[test]
fn unused_result_allows_explicit_discard_binding() {
    let check = check_with_must_use_global("_ = mustUse()\n1\n");

    assert!(check.diagnostics.is_empty());
}

#[test]
fn unused_result_allows_named_binding_capture() {
    let check = check_with_must_use_global("x = mustUse()\n1\n");

    assert!(check.diagnostics.is_empty());
}

#[test]
fn unused_result_allows_panic_unwrap_as_non_final_item() {
    let check = check_with_must_use_global("mustUse()?!\n1\n");

    assert!(check.diagnostics.is_empty());
}

#[test]
fn unused_result_allows_final_result_expression() {
    let check = check_with_must_use_global("mustUse()\n");

    assert!(check.diagnostics.is_empty());
}

#[test]
fn unused_result_ignores_non_result_non_final_expression() {
    let check = check_with_must_use_global("1\n2\n");

    assert!(check.diagnostics.is_empty());
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
        render_top_level_value(&mut checker, "added"),
        Some("{ x: Int, y: 1, .. }".to_owned())
    );
    assert_eq!(
        render_top_level_value(&mut checker, "updated"),
        Some("{ x: 2, .. }".to_owned())
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
    assert!(matches!(row.tail, RowTail::Var(id) if row_var_scheme.row_vars.contains(&id)));
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn unannotated_record_spread_lambda_call_infers_merged_result() {
    let output = parse_module(
        "merge = (a, b) => { ..a, ..b }\n\
         u : { name: Text } = { name: \"Ada\" }\n\
         m : { age: Int } = { age: 3 }\n\
         both = merge(u, m)\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "both"),
        Some("{ name: Text, age: Int }".to_owned())
    );

    let scheme = checker
        .infer_top_level_scheme("merge")
        .expect("inferred merge scheme");
    assert_eq!(scheme.row_vars.len(), 3);
    assert_eq!(scheme.row_merges.len(), 1);
    let Type::Function { params, result, .. } = &scheme.ty else {
        panic!("merge should infer a function type");
    };
    assert!(params.iter().all(|param| {
        matches!(
            param,
            Type::Record(Row {
                entries,
                tail: RowTail::Var(id),
            }) if entries.is_empty() && scheme.row_vars.contains(id)
        )
    }));
    assert!(matches!(
        result.as_ref(),
        Type::Record(Row {
            entries,
            tail: RowTail::Var(id),
        }) if entries.is_empty() && scheme.row_vars.contains(id)
    ));
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn local_record_spread_lambda_call_infers_merged_result() {
    let output = parse_module(
        "main = () =>\n  merge = (a, b) => { ..a, ..b }\n  u : { name: Text } = { name: \"Ada\" }\n  m : { age: Int } = { age: 3 }\n  both = merge(u, m)\n  both\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "main"),
        Some("() -> { name: Text, age: Int }".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn local_record_spread_lambda_result_reports_field_type_mismatch() {
    let output = parse_module(
        "main = () =>\n  merge = (a, b) => { ..a, ..b }\n  u : { name: Text } = { name: \"Ada\" }\n  m : { age: Int } = { age: 3 }\n  both = merge(u, m)\n  bad : Text = both.age\n  both\n",
    );
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::UNRESOLVED_BINDING
    ));
}

#[test]
fn record_spread_lambda_instantiates_fresh_rows_per_call() {
    let output = parse_module(
        "merge = (a, b) => { ..a, ..b }\n\
         u : { name: Text } = { name: \"Ada\" }\n\
         m : { age: Int } = { age: 3 }\n\
         left : { title: Text } = { title: \"Dr\" }\n\
         right : { active: Bool } = { active: true }\n\
         both = merge(u, m)\n\
         other = merge(left, right)\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "both"),
        Some("{ name: Text, age: Int }".to_owned())
    );
    assert_eq!(
        render_top_level_value(&mut checker, "other"),
        Some("{ title: Text, active: Bool }".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn unannotated_record_spread_lambda_call_infers_overwrite_merge_result() {
    let output = parse_module(
        "merge = (a, b) => { ..a, :..b }\n\
         left : { id: Text, v: Int } = { id: \"a\", v: 1 }\n\
         right : { v: Text } = { v: \"hi\" }\n\
         both = merge(left, right)\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        render_top_level_value(&mut checker, "both"),
        Some("{ id: Text, v: Text }".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn type_level_record_spread_lambda_merge_still_specializes() {
    let output = parse_module(
        "User = { name: Text }\n\
         Meta = { age: Int }\n\
         merge = (a, b) => { ..a, ..b }\n\
         Both = merge(User, Meta)\n\
         value : Both = { name: \"Ada\", age: 3 }\n",
    );
    let check = check_module(&output.module);

    assert!(check.diagnostics.is_empty());
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

    assert_eq!(
        render_top_level_value(&mut checker, "name"),
        Some("\"Ada\"".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn quoted_computed_record_labels_decode_string_escapes() {
    let output = parse_module("record = { [\"a\\\"b\"]: 1 }\nvalue = record[\"a\\\"b\"]\n");
    assert!(output.diagnostics.is_empty());
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    let record = checker
        .infer_top_level_scheme("record")
        .expect("record scheme")
        .ty;
    let Type::Record(row) = record else {
        panic!("record should infer a record type");
    };
    assert_eq!(row.entries.len(), 1);
    assert_eq!(row_label(&row.entries[0]), "a\"b");
    assert_eq!(
        render_top_level_value(&mut checker, "value"),
        Some("1".to_owned())
    );
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
fn array_index_infers_optional_element_type() {
    let output = parse_module("arr = [1, 2, 3]\na1 = arr[1]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    // The element type of `[1, 2, 3]` is the open literal union `1 | 2 | 3`, and
    // array indexing wraps it in `?` (an absent index yields `undefined`).
    let scheme = checker.infer_top_level_scheme("a1").expect("scheme for a1");
    assert!(
        matches!(&scheme.ty, Type::Optional(inner) if matches!(inner.as_ref(), Type::Variant(_))),
        "expected `?(1 | 2 | 3)`, got {:?}",
        scheme.ty
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn array_index_requires_an_int_index() {
    for (source, index) in [
        ("arr = [1, 2, 3]\nvalue = arr[\"key\"]\n", "\"key\""),
        ("arr = [1, 2, 3]\nvalue = arr[0.5]\n", "0.5"),
    ] {
        let output = parse_module(source);
        let known_types = known_type_names(&output.module);
        let type_definitions = type_definitions(&output.module, &known_types);
        let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
        let _ = checker.infer_top_level_scheme("value");

        assert_eq!(
            matching_codes(&checker.diagnostics, codes::ty::MISMATCH),
            1,
            "expected array index mismatch, got {:?}",
            checker.diagnostics
        );
        let diagnostic = checker
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::MISMATCH))
            .expect("array index type mismatch");
        assert_eq!(diagnostic.labels[0].span, nth_span(source, index, 0));
    }
}

#[test]
fn array_index_accepts_literal_and_bound_int_indexes() {
    let output = parse_module("arr = [1, 2, 3]\nliteral = arr[0]\ni = 1\nbound = arr[i]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let _ = checker.infer_top_level_scheme("literal");
    let _ = checker.infer_top_level_scheme("bound");

    assert!(
        checker.diagnostics.is_empty(),
        "expected integer indexes to check, got {:?}",
        checker.diagnostics
    );
}

#[test]
fn array_index_with_deferred_index_type_defers_without_diagnostic() {
    let output = parse_module(
        "arr = [1, 2, 3]\nunknown = \"not a record\"[\"key\"]\nvalue = arr[unknown]\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);
    let _ = checker.infer_top_level_scheme("value");

    assert!(
        checker.diagnostics.is_empty(),
        "expected deferred index type to produce no diagnostic, got {:?}",
        checker.diagnostics
    );
}

#[test]
fn tuple_index_with_literal_infers_exact_element_type() {
    let output = parse_module("pair = (\"Ada\", 36)\nname = pair[0]\nage = pair[1]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    // Tuple projection returns the exact element type — including its literal.
    assert_eq!(
        render_top_level_value(&mut checker, "name"),
        Some("\"Ada\"".to_owned())
    );
    assert_eq!(
        render_top_level_value(&mut checker, "age"),
        Some("36".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn null_safe_field_access_through_array_element_propagates_optional() {
    let output = parse_module("rows = [{ name: \"Ada\" }]\nfirst = rows[0]\nlabel = first?.name\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    // `rows[0]` is `?{ name: ... }`, so `?.name` yields an optional field type.
    let scheme = checker
        .infer_top_level_scheme("label")
        .expect("scheme for label");
    assert!(
        matches!(&scheme.ty, Type::Optional(_)),
        "expected an optional field type, got {:?}",
        scheme.ty
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn plain_field_access_through_array_element_reports_unguarded_empty() {
    let output = parse_module("rows = [{ name: \"Ada\" }]\nfirst = rows[0]\nlabel = first.name\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    let _ = checker.infer_top_level_scheme("label");
    let diagnostic = checker
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::UNGUARDED_EMPTY_ACCESS))
        .unwrap_or_else(|| {
            panic!(
                "expected an unguarded-empty-access diagnostic, got {:?}",
                checker.diagnostics
            )
        });
    // The message names the receiver expression rather than "this value".
    assert!(
        diagnostic.message.contains("`first`"),
        "expected the receiver to be named, got {:?}",
        diagnostic.message
    );
}

#[test]
fn tuple_index_out_of_range_reports_diagnostic() {
    let output = parse_module("pair = (\"Ada\", 36)\nx = pair[2]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    let _ = checker.infer_top_level_scheme("x");
    assert!(
        checker
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref()
                == Some(codes::ty::TUPLE_INDEX_OUT_OF_RANGE))
    );
}

#[test]
fn tuple_index_with_runtime_index_reports_not_comptime() {
    let output = parse_module("pair = (\"Ada\", 36)\ni = 1\nx = pair[i]\n");
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    let _ = checker.infer_top_level_scheme("x");
    assert!(
        checker
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref()
                == Some(codes::ty::TUPLE_INDEX_NOT_COMPTIME))
    );
}

#[test]
fn comptime_pick_unrolls_key_set_to_closed_record_type() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }\n\
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
fn comptime_iteration_guard_filters_unrolled_record_members() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         omit2 = (o: {..r}, @drop: keysOf(r)@{}) => { keysOf(o) -> k, !drop.has(k); (k, o[k]) }\n\
         result = omit2(user, @{\"name\"})\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("result"),
        Some(Type::Record(Row {
            entries: vec![field("email", named("Text"))],
            tail: RowTail::Closed,
        }))
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn comptime_iteration_guard_all_pass_keeps_unrolled_record_members() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         pick2 = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k, keys.has(k); (k, o[k]) }\n\
         result = pick2(user, @{\"name\", \"email\"})\n",
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
fn comptime_omit_bulk_deletes_key_set_from_closed_record_type() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         omit = (object: {..r}, @keys: keysOf(r)@{}) => { ..object, -keys }\n\
         result = omit(user, @{\"name\"})\n\
         credentials = { email: \"ops@x.dev\", password: \"secret\" }\n\
         without_password = { ..credentials, -password }\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("result"),
        Some(Type::Record(Row {
            entries: vec![field("email", named("Text"))],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        render_top_level_value(&mut checker, "without_password"),
        Some("{ email: \"ops@x.dev\" }".to_owned())
    );
    assert!(checker.diagnostics.is_empty());
}

#[test]
fn comptime_drop_key_deletes_single_computed_key_from_closed_record_type() {
    let output = parse_module(
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         dropKey = (object: {..r}, @key: keysOf(r)) => { ..object, -[key] }\n\
         result = dropKey(user, \"name\")\n",
    );
    let known_types = known_type_names(&output.module);
    let type_definitions = type_definitions(&output.module, &known_types);
    let mut checker = Checker::with_module(known_types, type_definitions, &output.module);

    assert_eq!(
        checker.infer_top_level_value("result"),
        Some(Type::Record(Row {
            entries: vec![field("email", named("Text"))],
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
         pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }\n\
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
    let Type::Function { params, result, .. } = &scheme.ty else {
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
fn record_identifier_values_check_shared_open_or_optional_field_types() {
    for source in [
        "src : { name: Int, age: Text, .. } = { name: 1, age: \"x\" }\n\
         dst : { name: Text, age: Int } = src\n",
        "Expected = { name: Text, age: Int }\n\
         Source = { name: Int, age: ?Int }\n\
         s : Source = { name: 99 }\n\
         bad : Expected = s\n",
        "Source = { value: ?Int }\n\
         Expected = { value: Int }\n\
         source : Source = {}\n\
         expected : Expected = source\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} should produce type.mismatch"
        );
    }

    for source in [
        "source : { name: Text, age: Int, .. } = record\n\
         expected : { name: Text } = source\n",
        "Source = { value: ?Int }\n\
         Expected = { value: ?Int }\n\
         source : Source = {}\n\
         expected : Expected = source\n",
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
    }
}

#[test]
fn annotated_identifier_values_are_checked_against_expected_types() {
    for source in [
        "other : Text = \"hi\"\nvalue : Int = other\n",
        "other : (Int, Text) = (1, \"a\")\nvalue : (Int, Int) = other\n",
        "other : ?Text = undefined\nvalue : Text = other\n",
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
        "other : Undefined = undefined\nvalue : ?Text = other\n",
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
fn nullable_values_accept_null_and_matching_inner_values() {
    for source in [
        "value : Text? = \"hi\"\n",
        "value : Text? = null\n",
        "value : Int? = null\n",
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
fn optional_values_accept_undefined_and_matching_inner_values() {
    for source in [
        "value : ?Text = \"hi\"\n",
        "value : ?Text = undefined\n",
        "value : ?Int = undefined\n",
        "value : ?Text? = undefined\n",
        "value : ?Text? = null\n",
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
fn optional_and_nullable_values_reject_the_other_empty_value() {
    for source in ["value : Text? = undefined\n", "value : ?Text = null\n"] {
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
fn optional_and_nullable_widen_from_inner_values() {
    for source in [
        "plain : Text = \"x\"\nvalue : ?Text = plain\n",
        "plain : Text = \"x\"\nvalue : Text? = plain\n",
        "plain : Text = \"x\"\nvalue : ?Text? = plain\n",
        "nullable : Text? = null\nvalue : ?Text? = nullable\n",
        "optional : ?Text = undefined\nvalue : ?Text? = optional\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);

        assert!(
            !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
            "{source} unexpectedly produced type.mismatch: {:?}",
            check.diagnostics
        );
    }
}

#[test]
fn optional_return_annotation_widens_bare_int_match_arm() {
    // Minimal non-recursive: bare `n` / `undefined` under `-> ?Int`.
    let minimal = parse_module(concat!(
        "f : (Int) -> ?Int\n",
        "f = (n) =>\n",
        "  n ?>\n",
        "    undefined => undefined\n",
        "    m => m\n",
    ));
    let minimal_check = check_module(&minimal.module);
    assert!(
        !has_diagnostic_code(&minimal_check.diagnostics, codes::ty::MISMATCH),
        "minimal optional return match unexpectedly mismatched: {:?}",
        minimal_check.diagnostics
    );

    // indexOfGo shape: nested match, bare `index`, recursive self-call at `?Int`.
    let index_of = concat!(
        "indexOfGo : (Array(a), Int, a) -> ?Int\n",
        "indexOfGo = (xs, index, target) =>\n",
        "  next = xs[index]\n",
        "  next ?>\n",
        "    undefined => undefined\n",
        "    element =>\n",
        "      element == target ?>\n",
        "        true => index\n",
        "        false => indexOfGo(xs, index + 1, target)\n",
    );
    let index_of_output = parse_module(index_of);
    let index_of_check = check_module(&index_of_output.module);
    assert!(
        !has_diagnostic_code(&index_of_check.diagnostics, codes::ty::MISMATCH),
        "indexOfGo optional return shape unexpectedly mismatched: {:?}",
        index_of_check.diagnostics
    );
    // Signature + binding: type is recorded on the binding name (second occurrence).
    assert_eq!(
        index_of_check
            .type_at(nth_span(index_of, "indexOfGo", 1))
            .map(Type::render),
        Some("(Array(a), Int, a) -> ?Int".to_owned())
    );
}

#[test]
fn optional_return_annotation_still_rejects_wrong_arm_payload() {
    let output = parse_module(concat!(
        "f : (Int) -> ?Int\n",
        "f = (n) =>\n",
        "  n ?>\n",
        "    undefined => undefined\n",
        "    m => \"nope\"\n",
    ));
    let check = check_module(&output.module);
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        1,
        "wrong arm payload under ?Int should still mismatch: {:?}",
        check.diagnostics
    );
}

#[test]
fn normalizes_optional_and_nullable_wrappers() {
    let checker = Checker::with_type_definitions(HashSet::new(), Default::default());

    assert_eq!(
        checker.normalize(&optional(optional(named("Text")))),
        optional(named("Text"))
    );
    assert_eq!(
        checker.normalize(&nullable(nullable(named("Text")))),
        nullable(named("Text"))
    );
    assert_eq!(
        checker.normalize(&optional(nullable(named("Text")))),
        optional(nullable(named("Text")))
    );
    assert_eq!(
        checker.normalize(&nullable(optional(named("Text")))),
        optional(nullable(named("Text")))
    );
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
fn optional_typed_record_fields_may_be_absent_or_checked_when_present() {
    let output = parse_module("value : { name: Text, phone: ?Text } = { name: \"x\" }\n");
    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());

    let output = parse_module("value : { phone: ?Text } = { phone: 42 }\n");
    let check = check_module(&output.module);
    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn record_field_omission_keys_off_optional_type_not_nullability() {
    let output = parse_module("value : { phone: ?Text, email: Text? } = { email: null }\n");
    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty());

    let output = parse_module("value : { email: Text? } = {}\n");
    let check = check_module(&output.module);
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD),
        1
    );
}

#[test]
fn optional_record_fields_accept_omission_and_nullable_fields_accept_null() {
    let output = parse_module("value : { maybe: ?Text, email: Text? } = { email: null }\n");
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
    let output = parse_module("value : { r: { name: Text } } = { r: { name: 1, .. } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn set_element_record_markers_are_reported_once() {
    let output = parse_module("value : Set({ name: Text }) = @{ { name: 1, .. } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn extra_field_record_markers_are_reported_once() {
    let output = parse_module("value : { name: Text } = { name: 1, blob: { .. } }\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::TYPE_ONLY_RECORD_ENTRY),
        1
    );
}

#[test]
fn open_extra_field_record_markers_are_reported_once() {
    let output = parse_module("value : { name: Text, .. } = { name: \"x\", blob: { .. } }\n");
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
fn pick_omit_alias_annotations_enforce_result_record_types() {
    // `pick`/`omit` reify to closed records at alias definition time, so
    // transparent-alias normalization feeds structural value checking.
    let missing = parse_module(
        "User = { name: Text, age: Int }\n\
         P = pick(User, @{\"name\", \"age\"})\n\
         u: P = { name: \"a\" }\n",
    );
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::MISSING_FIELD),
        1,
        "pick alias must require all picked fields; diagnostics: {:?}",
        missing_check.diagnostics
    );

    let wrong_type = parse_module(
        "User = { name: Text, age: Int }\n\
         T = omit(User, @{\"age\"})\n\
         v: T = { name: 99 }\n",
    );
    let wrong_type_check = check_module(&wrong_type.module);
    assert_eq!(
        matching_codes(&wrong_type_check.diagnostics, codes::ty::MISMATCH),
        1,
        "omit alias must enforce remaining field types; diagnostics: {:?}",
        wrong_type_check.diagnostics
    );
    assert_eq!(
        wrong_type_check.diagnostics[0].message,
        "expected `Text`, found a number literal"
    );

    let ok = parse_module(
        "User = { name: Text, age: Int }\n\
         P = pick(User, @{\"name\", \"age\"})\n\
         T = omit(User, @{\"age\"})\n\
         u: P = { name: \"a\", age: 1 }\n\
         v: T = { name: \"a\" }\n",
    );
    let ok_check = check_module(&ok.module);
    assert!(
        !has_diagnostic_code(&ok_check.diagnostics, codes::ty::MISSING_FIELD)
            && !has_diagnostic_code(&ok_check.diagnostics, codes::ty::MISMATCH),
        "correct values against pick/omit aliases must pass; diagnostics: {:?}",
        ok_check.diagnostics
    );
}

#[test]
fn pick_omit_inline_annotations_enforce_result_record_types() {
    let missing = parse_module(
        "User = { name: Text, age: Int }\n\
         u: pick(User, @{\"name\", \"age\"}) = { name: \"a\" }\n",
    );
    let missing_check = check_module(&missing.module);
    assert_eq!(
        matching_codes(&missing_check.diagnostics, codes::ty::MISSING_FIELD),
        1,
        "inline pick annotation must require all picked fields; diagnostics: {:?}",
        missing_check.diagnostics
    );

    let wrong_type = parse_module(
        "User = { name: Text, age: Int }\n\
         v: omit(User, @{\"age\"}) = { name: 99 }\n",
    );
    let wrong_type_check = check_module(&wrong_type.module);
    assert_eq!(
        matching_codes(&wrong_type_check.diagnostics, codes::ty::MISMATCH),
        1,
        "inline omit annotation must enforce remaining field types; diagnostics: {:?}",
        wrong_type_check.diagnostics
    );

    let ok = parse_module(
        "User = { name: Text, age: Int }\n\
         u: pick(User, @{\"name\"}) = { name: \"a\" }\n\
         v: omit(User, @{\"age\"}) = { name: \"a\" }\n",
    );
    let ok_check = check_module(&ok.module);
    assert!(
        !has_diagnostic_code(&ok_check.diagnostics, codes::ty::MISSING_FIELD)
            && !has_diagnostic_code(&ok_check.diagnostics, codes::ty::MISMATCH),
        "correct values against inline pick/omit annotations must pass; diagnostics: {:?}",
        ok_check.diagnostics
    );
}

#[test]
fn pick_omit_fn_param_annotations_enforce_result_record_types() {
    // Function parameter annotations share lower_normalized_annotation; a
    // pick/omit expected type must reject a structurally wrong argument.
    let output = parse_module(
        "User = { name: Text, age: Int }\n\
         P = pick(User, @{\"name\", \"age\"})\n\
         take = (x: P) => x\n\
         bad = take({ name: \"a\" })\n",
    );
    let check = check_module(&output.module);
    assert!(
        matching_codes(&check.diagnostics, codes::ty::MISSING_FIELD) >= 1
            || matching_codes(&check.diagnostics, codes::ty::MISMATCH) >= 1,
        "pick/omit param annotations must enforce argument shape; diagnostics: {:?}",
        check.diagnostics
    );

    let ok = parse_module(
        "User = { name: Text, age: Int }\n\
         P = pick(User, @{\"name\", \"age\"})\n\
         take = (x: P) => x\n\
         good = take({ name: \"a\", age: 1 })\n",
    );
    let ok_check = check_module(&ok.module);
    assert!(
        !has_diagnostic_code(&ok_check.diagnostics, codes::ty::MISSING_FIELD)
            && !has_diagnostic_code(&ok_check.diagnostics, codes::ty::MISMATCH),
        "well-typed call against pick param must pass; diagnostics: {:?}",
        ok_check.diagnostics
    );
}

#[test]
fn pick_alias_definition_reifies_closed_record_type() {
    let output = parse_module(
        "User = { name: Text, age: Int }\n\
         P = pick(User, @{\"name\", \"age\"})\n\
         T = omit(User, @{\"age\"})\n",
    );
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("P"),
        Some(&Type::Record(Row {
            entries: vec![field("name", named("Text")), field("age", named("Int"))],
            tail: RowTail::Closed,
        })),
        "pick alias must store the selected closed record, not Deferred"
    );
    assert_eq!(
        definitions.get("T"),
        Some(&Type::Record(Row {
            entries: vec![field("name", named("Text"))],
            tail: RowTail::Closed,
        })),
        "omit alias must store the remaining closed record, not Deferred"
    );
}

#[test]
fn pick_domain_checked_bad_key_still_reports_literal_not_in_union() {
    // Domain validation on user-defined pick (`@keys: keysOf(r)@{}`) must
    // stay independent of the builtin type-position reification path.
    let source = "User = { name: Text, age: Int }\n\
         user : User = { name: \"a\", age: 1 }\n\
         pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }\n\
         result = pick(user, @{\"name\", \"nope\"})\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::LITERAL_NOT_IN_UNION)),
        "expected literal-not-in-union for out-of-domain pick key; got: {:?}",
        check.diagnostics
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

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::CYCLIC_ALIAS),
        2
    );
    // Cyclic names stay nominal after normalize; a number literal is not `A`.
    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);

    // Recursive tuple alias stays Named; a concrete tuple does not inhabit it.
    let output = parse_module("A = (A, Int)\nvalue : A = (1, 2)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
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
    let output = parse_module("value : { name: Int } = { name: 1, .. }\n");
    let check = check_module(&output.module);

    assert_eq!(check.diagnostics.len(), 1);
    assert_eq!(
        check.diagnostics[0].code.as_deref(),
        Some(codes::ty::TYPE_ONLY_RECORD_ENTRY)
    );
}

fn check_with_must_use_global(source: &str) -> CheckOutput {
    let output = parse_module(source);
    assert!(
        output.diagnostics.is_empty(),
        "unexpected parse diagnostics for {source:?}: {:?}",
        output.diagnostics
    );

    let globals = must_use_globals();
    check_module_with_globals(&output.module, &globals)
}

fn must_use_globals() -> Vec<(String, Type)> {
    vec![(
        "mustUse".to_owned(),
        build::function(vec![], build::result(build::int(), build::text())),
    )]
}

fn fallible_read_globals() -> Vec<(String, Type)> {
    vec![(
        "read".to_owned(),
        build::function(
            vec![build::text()],
            build::result(build::text(), build::named("ReadError")),
        ),
    )]
}

fn read_and_parse_globals() -> Vec<(String, Type)> {
    vec![
        (
            "read".to_owned(),
            build::function(
                vec![build::text()],
                build::result(build::text(), build::named("ReadError")),
            ),
        ),
        (
            "parse".to_owned(),
            build::function(
                vec![build::text()],
                build::result(build::int(), build::named("ParseError")),
            ),
        ),
    ]
}

fn read_and_parse_unit_error_globals() -> Vec<(String, Type)> {
    read_and_parse_globals()
}

fn file_style_globals() -> Vec<(String, Type)> {
    let read_handle = build::record(vec![(
        "readAll",
        build::function(
            vec![],
            build::result(build::text(), build::named("ReadError")),
        ),
    )]);
    let file = build::record(vec![(
        "open",
        build::function(
            vec![build::text(), build::text_literals(&["r"])],
            build::result(read_handle, build::named("IoError")),
        ),
    )]);

    vec![("File".to_owned(), file)]
}

/// A small host-style logger global: `logger : { info: (Text) -> Unit }`.
fn logger_globals() -> Vec<(String, Type)> {
    vec![(
        "logger".to_owned(),
        build::record(vec![(
            "info",
            build::function(vec![build::text()], build::unit()),
        )]),
    )]
}

fn generic_id_globals() -> Vec<(String, Type)> {
    vec![(
        "id".to_owned(),
        build::function(vec![build::var("a")], build::var("a")),
    )]
}

fn poly_mode_globals() -> Vec<(String, Type)> {
    let handle = build::record(vec![("readLine", build::function(vec![], build::text()))]);
    let io_error = build::variant(vec![("Other", vec![build::text()])]);

    vec![
        (
            "f".to_owned(),
            build::function(
                vec![build::text(), build::apply("Boxed", vec![build::var("h")])],
                build::result(build::var("h"), io_error),
            ),
        ),
        ("M".to_owned(), build::apply("Boxed", vec![handle])),
    ]
}

fn monomorphic_g_globals() -> Vec<(String, Type)> {
    vec![(
        "g".to_owned(),
        build::function(vec![build::int()], build::int()),
    )]
}

struct TableTypeResolver;

impl HostComptimeFn for TableTypeResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        let [table] = args else {
            return Err(ComptimeError::new(
                "tableType expects one compile-time table name",
            ));
        };

        match table.as_text() {
            Some("users") => Ok(build::record(vec![
                ("id", build::int()),
                ("name", build::text()),
            ])),
            Some("orders") => Ok(build::record(vec![
                ("id", build::int()),
                ("total", build::int()),
            ])),
            Some(other) => Err(ComptimeError::new(format!("unknown table `{other}`"))),
            None => Err(ComptimeError::new("tableType expects a Text table name")),
        }
    }
}

fn table_type_globals() -> HostGlobals {
    HostGlobals::new(
        vec![(
            "tableType".to_owned(),
            build::function(vec![build::text()], Type::Deferred),
        )],
        vec![(
            "tableType".to_owned(),
            HostComptimeFnSpec::new(Rc::new(TableTypeResolver), vec![0]),
        )],
    )
}

struct ModeTypeResolver;

impl HostComptimeFn for ModeTypeResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        match args {
            [mode] if mode.as_text().is_some() => Ok(build::text()),
            _ => Err(ComptimeError::new("modeType expects one mode literal")),
        }
    }
}

fn mode_type_globals() -> HostGlobals {
    HostGlobals::new(
        vec![(
            "modeType".to_owned(),
            build::function(vec![build::text_literals(&["r", "w"])], Type::Deferred),
        )],
        vec![(
            "modeType".to_owned(),
            HostComptimeFnSpec::new(Rc::new(ModeTypeResolver), vec![0]),
        )],
    )
}

#[test]
fn seeded_global_call_checks_ok() {
    let output = parse_module("logger.info(\"hi\")\n");
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert!(
        check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        check.diagnostics
    );
}

#[test]
fn seeded_global_call_rejects_wrong_argument_type() {
    let output = parse_module("logger.info(42)\n");
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn polymorphic_resolved_function_call_reports_argument_mismatch() {
    let source = "f(\"x\", 5)\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &poly_mode_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::MISMATCH))
        .expect("expected one type mismatch");
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "5", 0));

    let accepted = parse_module("f(\"x\", M)\n");
    let accepted_check = check_module_with_globals(&accepted.module, &poly_mode_globals());
    assert!(
        accepted_check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        accepted_check.diagnostics
    );
}

#[test]
fn monomorphic_resolved_function_call_reports_argument_mismatch() {
    let source = "value = g(\"x\")\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &monomorphic_g_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::MISMATCH))
        .expect("expected one type mismatch");
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "\"x\"", 0));

    let accepted = parse_module("value = g(1)\n");
    let accepted_check = check_module_with_globals(&accepted.module, &monomorphic_g_globals());
    assert!(
        accepted_check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        accepted_check.diagnostics
    );
}

#[test]
fn generic_seeded_global_accepts_different_argument_types() {
    let output = parse_module("id(42)\nid(\"x\")\n");
    let check = check_module_with_globals(&output.module, &generic_id_globals());

    assert!(
        check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        check.diagnostics
    );
}

#[test]
fn generic_seeded_global_result_flows_to_annotation() {
    let output = parse_module("x : Int = id(42)\n");
    let check = check_module_with_globals(&output.module, &generic_id_globals());

    assert!(
        check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        check.diagnostics
    );
}

#[test]
fn generic_seeded_global_result_mismatch_is_reported() {
    let output = parse_module("y : Text = id(42)\n");
    let check = check_module_with_globals(&output.module, &generic_id_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn generic_seeded_global_instantiates_fresh_per_use() {
    let source = "a : Int = id(1)\nb : Text = id(\"s\")\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &generic_id_globals());

    assert!(
        check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        check.diagnostics
    );
}

#[test]
fn host_comptime_fn_resolves_result_type_from_literal_argument() {
    let source = "users = tableType(\"users\")\norders = tableType(\"orders\")\n";
    let output = parse_module(source);
    let check = check_module_with_host_globals(&output.module, &table_type_globals());

    assert!(
        check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        check.diagnostics
    );

    let users = check
        .type_at(nth_span(source, "users", 0))
        .expect("users binding has a type");
    let orders = check
        .type_at(nth_span(source, "orders", 0))
        .expect("orders binding has a type");

    assert_eq!(
        users,
        &build::record(vec![("id", build::int()), ("name", build::text())])
    );
    assert_eq!(
        orders,
        &build::record(vec![("id", build::int()), ("total", build::int())])
    );
    assert_ne!(users, orders);
}

#[test]
fn host_comptime_fn_runtime_argument_defers_without_unresolved_binding() {
    let source = "table : Text = \"users\"\nusers = tableType(table)\n";
    let output = parse_module(source);
    let check = check_module_with_host_globals(&output.module, &table_type_globals());

    assert!(
        check.diagnostics.is_empty(),
        "runtime host-comptime argument should stay intentionally deferred: {:?}",
        check.diagnostics
    );
    assert_eq!(check.type_at(nth_span(source, "users", 0)), None);
}

#[test]
fn host_comptime_domain_base_kind_mismatch_reports_literal_not_in_union() {
    let source = "value = modeType(5)\n";
    let output = parse_module(source);
    let check = check_module_with_host_globals(&output.module, &mode_type_globals());

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        1,
        "expected membership diagnostic for host-comptime domain: {:?}",
        check.diagnostics
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH),
        0,
        "host-comptime domain mismatch should not use generic mismatch: {:?}",
        check.diagnostics
    );
    assert_eq!(check.type_at(nth_span(source, "value", 0)), None);
}

#[test]
fn seeded_global_rejects_unknown_field() {
    let output = parse_module("logger.nope\n");
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert!(
        has_diagnostic_code(&check.diagnostics, codes::ty::MISSING_FIELD),
        "expected a missing-field diagnostic, got {:?}",
        check.diagnostics
    );
}

#[test]
fn seeded_global_call_rejects_extra_argument() {
    let output = parse_module("logger.info(\"a\", \"b\")\n");
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn seeded_global_reaches_inference_name_path() {
    // Binding `x` to `logger.info` forces inference (not directed checking) of
    // the seeded global, then `x(42)` must still catch the Text/Int mismatch.
    let source = "x = logger.info\nx(42)\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn non_seeded_free_name_reports_unbound() {
    let output = parse_module("mystery.foo()\n");
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert!(
        has_diagnostic_code(&check.diagnostics, codes::name::UNBOUND),
        "expected unbound-name diagnostic for non-seeded name, got {:?}",
        check.diagnostics
    );
}

#[test]
fn user_binding_shadows_seeded_global() {
    // A user top-level `logger = 5` wins over the seeded record, so using
    // `logger` as a function/record is now the Int it was bound to.
    let source = "logger = 5\nvalue : Int = logger\n";
    let output = parse_module(source);
    let check = check_module_with_globals(&output.module, &logger_globals());

    assert!(
        check.diagnostics.is_empty(),
        "expected the user binding to shadow the seed, got {:?}",
        check.diagnostics
    );
}

#[test]
fn unbound_name_diagnostic_keeps_bound_names_clean() {
    let output = parse_module("x = y\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::name::UNBOUND), 1);

    let forward = parse_module("a = b\nb = 1\n");
    let forward_check = check_module(&forward.module);
    assert!(
        !has_diagnostic_code(&forward_check.diagnostics, codes::name::UNBOUND),
        "expected forward reference to stay clean, got {:?}",
        forward_check.diagnostics
    );

    let host = parse_module("logger.info(\"hi\")\n");
    let host_check = check_module_with_globals(&host.module, &logger_globals());
    assert!(
        !has_diagnostic_code(&host_check.diagnostics, codes::name::UNBOUND),
        "expected seeded host global to stay clean, got {:?}",
        host_check.diagnostics
    );
}

/// A host global typed with one required `Text` and one optional trailing
/// fields record: `f : function_opt([Text], [{..}]) -> Unit`.
fn optional_param_globals() -> Vec<(String, Type)> {
    vec![(
        "f".to_owned(),
        build::function_opt(
            vec![build::text()],
            vec![build::open_record(vec![])],
            build::unit(),
        ),
    )]
}

#[test]
fn lambda_default_infers_required_arity() {
    let source = "f = (x: Int, y: Int = 0) => x + y\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "expected no diagnostics, got {:?}",
        check.diagnostics
    );
    let f_type = check
        .type_at(nth_span(source, "f", 0))
        .expect("f should have an inferred type");
    assert_eq!(f_type.render(), "(Int, Int = _) -> Int");
    assert_eq!(function_required_arity(f_type), Some(1));
}

#[test]
fn lambda_default_accepts_calls_within_required_range() {
    for source in [
        "f = (x: Int, y: Int = 0) => x + y\nf(1)\n",
        "f = (x: Int, y: Int = 0) => x + y\nf(1, 2)\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);
        assert!(
            check.diagnostics.is_empty(),
            "expected no diagnostics for {source:?}, got {:?}",
            check.diagnostics
        );
    }
}

#[test]
fn lambda_default_rejects_too_few_arguments() {
    let output = parse_module("f = (x: Int, y: Int = 0) => x + y\nf()\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn lambda_default_rejects_too_many_arguments() {
    let output = parse_module("f = (x: Int, y: Int = 0) => x + y\nf(1, 2, 3)\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn unannotated_default_infers_parameter_type() {
    for source in ["g = (x = 5) => x\ng()\n", "g = (x = 5) => x\ng(9)\n"] {
        let output = parse_module(source);
        let check = check_module(&output.module);
        assert!(
            check.diagnostics.is_empty(),
            "expected no diagnostics for {source:?}, got {:?}",
            check.diagnostics
        );
    }

    let mismatch = parse_module("g = (x = 5) => x\ng(\"no\")\n");
    let check = check_module(&mismatch.module);
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::LITERAL_NOT_IN_UNION),
        1
    );
}

#[test]
fn default_mismatching_annotation_is_a_type_error() {
    let output = parse_module("h = (x: Int = \"no\") => x\n");
    let check = check_module(&output.module);

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn optional_param_global_accepts_required_and_optional_calls() {
    for source in ["f(\"hi\")\n", "f(\"hi\", { n: 1 })\n"] {
        let output = parse_module(source);
        let check = check_module_with_globals(&output.module, &optional_param_globals());
        assert!(
            check.diagnostics.is_empty(),
            "expected no diagnostics for {source:?}, got {:?}",
            check.diagnostics
        );
    }
}

#[test]
fn optional_param_global_rejects_wrong_argument_type() {
    let output = parse_module("f(42)\n");
    let check = check_module_with_globals(&output.module, &optional_param_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn optional_param_global_rejects_too_few_arguments() {
    let output = parse_module("f()\n");
    let check = check_module_with_globals(&output.module, &optional_param_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn optional_param_global_rejects_too_many_arguments() {
    let output = parse_module("f(\"a\", \"b\", \"c\")\n");
    let check = check_module_with_globals(&output.module, &optional_param_globals());

    assert_eq!(matching_codes(&check.diagnostics, codes::ty::MISMATCH), 1);
}

#[test]
fn duplicate_top_level_declaration_does_not_report_later_uses_as_unbound() {
    // A duplicated top-level name withholds its published type, but it is still
    // bound. Later references must not cascade `name.unbound` errors.
    let output = parse_module("x = \"a\"\nx = \"b\"\nlater = x\n");
    let check = check_module(&output.module);

    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::name::UNBOUND
    ));
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::UNRESOLVED_BINDING
    ));
}

#[test]
fn unresolved_top_level_runtime_binding_reports_when_value_stays_deferred() {
    let source = "someUndefinedName = _\nx = someUndefinedName()\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::UNRESOLVED_BINDING),
        1,
        "expected unresolved-binding diagnostic: {:?}",
        check.diagnostics
    );
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::name::UNBOUND
    ));
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::UNRESOLVED_BINDING))
        .expect("expected unresolved-binding diagnostic");
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "x", 0));
}

#[test]
fn bare_placeholder_runtime_binding_remains_clean() {
    let output = parse_module("runtime = _\n");
    let check = check_module(&output.module);

    assert!(
        check.diagnostics.is_empty(),
        "bare placeholder should stay valid: {:?}",
        check.diagnostics
    );
}

#[test]
fn comptime_deferred_binding_does_not_report_unresolved_binding() {
    let output = parse_module("Value = make()\n");
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::EVALUATION_UNSUPPORTED),
        1
    );
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::UNRESOLVED_BINDING
    ));
}

#[test]
fn binding_with_upstream_diagnostic_does_not_double_report_unresolved_binding() {
    let output = parse_module("x = y\n");
    let check = check_module(&output.module);

    assert_eq!(check.diagnostics.len(), 1, "{:?}", check.diagnostics);
    assert_eq!(matching_codes(&check.diagnostics, codes::name::UNBOUND), 1);
    assert!(!has_diagnostic_code(
        &check.diagnostics,
        codes::ty::UNRESOLVED_BINDING
    ));
}

#[test]
fn unresolved_local_runtime_binding_reports_when_value_stays_deferred() {
    let source = concat!(
        "someUndefinedName = _\n",
        "result =\n",
        "  x = someUndefinedName()\n",
        "  1\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::UNRESOLVED_BINDING),
        1,
        "expected unresolved local binding diagnostic: {:?}",
        check.diagnostics
    );
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::ty::UNRESOLVED_BINDING))
        .expect("expected unresolved-binding diagnostic");
    assert_eq!(diagnostic.labels[0].span, nth_span(source, "x", 0));
}

#[test]
fn overlapping_label_spread_merge_reports_only_duplicate_label() {
    // A record spread/merge with an overlapping label is one specific error
    // (`type.duplicate-spread-label`). It must not also fire the R6
    // `type.unresolved-binding` rule, even though the value's inferred type
    // finalizes to `Type::Deferred` and the duplicate diagnostic was emitted
    // during inference (inside the re-run spread-lambda body), so its label
    // lives on the helper binding's value, not on this binding's value.
    let output = parse_module("merge = (a, b) => { ..a, ..b }\nover = merge({ k: 1 }, { k: 2 })\n");
    let check = check_module(&output.module);

    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::UNRESOLVED_BINDING),
        "expected no unresolved-binding diagnostic: {:?}",
        check.diagnostics
    );
    assert_eq!(
        matching_codes(&check.diagnostics, codes::ty::DUPLICATE_SPREAD_LABEL),
        1,
        "expected exactly one duplicate-spread-label: {:?}",
        check.diagnostics
    );
}

#[test]
fn optional_params_render_with_default_marker() {
    assert_eq!(
        build::function_opt(vec![named("Text")], vec![named("Int")], named("Unit")).render(),
        "(Text, Int = _) -> Unit"
    );
    assert_eq!(
        build::function_opt(vec![], vec![named("Int")], named("Unit")).render(),
        "(Int = _) -> Unit"
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

fn singleton_number(raw: &str) -> Type {
    Type::Variant(Row {
        entries: vec![literal_number(raw)],
        tail: RowTail::Closed,
    })
}

#[test]
fn parameterized_value_param_specializes_distinctly() {
    // Value parameters flow through evaluate_args as ComptimeValue::Literal;
    // SpecializationKey hashes the full arg tuple so Sized(Int, 3) and
    // Sized(Int, 4) cache separately.
    let source = "Sized = (t: Type, n: Int) => { value: t, size: n }\n\
        Three = Sized(Int, 3)\n\
        Four = Sized(Int, 4)\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    let expected_three = Type::Record(Row {
        entries: vec![
            field("value", named("Int")),
            field("size", singleton_number("3")),
        ],
        tail: RowTail::Closed,
    });
    let expected_four = Type::Record(Row {
        entries: vec![
            field("value", named("Int")),
            field("size", singleton_number("4")),
        ],
        tail: RowTail::Closed,
    });
    assert_eq!(definitions.get("Three"), Some(&expected_three));
    assert_eq!(definitions.get("Four"), Some(&expected_four));
    assert_ne!(
        definitions.get("Three"),
        definitions.get("Four"),
        "distinct value arguments must not collide in specialization"
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
}

#[test]
fn parameterized_type_param_rejects_value_argument() {
    let source =
        "Pair = (t: Type) => { first: t, second: t }\np: Pair(3) = { first: 1, second: 2 }\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::ARGUMENT_KIND_MISMATCH),
        1
    );
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code.as_deref() == Some(codes::comptime::ARGUMENT_KIND_MISMATCH)
        })
        .expect("kind mismatch diagnostic");
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.contains("annotated as `Type`")),
        "expected annotation note, got {:?}",
        diagnostic.notes
    );
}

#[test]
fn parameterized_value_param_rejects_type_argument() {
    // Value-annotation site so the application is lowered through the comptime
    // evaluator (uppercase call RHSs on type aliases skip that path).
    let source = "Sized = (n: Int) => { size: n }\ns: Sized(Int) = { size: 1 }\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::ARGUMENT_KIND_MISMATCH),
        1
    );
}

#[test]
fn parameterized_value_param_rejects_outside_literal_union() {
    let source = "Pick = (n: 3 | 4) => { size: n }\np: Pick(5) = { size: 5 }\n";
    let output = parse_module(source);
    let check = check_module(&output.module);

    assert_eq!(
        matching_codes(&check.diagnostics, codes::comptime::ARGUMENT_BOUND),
        1
    );
    let diagnostic = check
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some(codes::comptime::ARGUMENT_BOUND))
        .expect("argument-bound diagnostic");
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.contains("annotated as `3 | 4`")),
        "expected bound note, got {:?}",
        diagnostic.notes
    );
}

#[test]
fn parameterized_value_param_accepts_int_literal() {
    let source = "Sized = (t: Type, n: Int) => { value: t, size: n }\n\
        Three = Sized(Int, 3)\n\
        value: Three = { value: 1, size: 3 }\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);
    assert_eq!(
        definitions.get("Three"),
        Some(&Type::Record(Row {
            entries: vec![
                field("value", named("Int")),
                field("size", singleton_number("3")),
            ],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
}

#[test]
fn parameterized_bare_param_accepts_type_and_value() {
    let source = "Id = (x) => { v: x }\nAsType = Id(Int)\nAsValue = Id(3)\n";
    let output = parse_module(source);
    let known_types = known_type_names(&output.module);
    let definitions = type_definitions(&output.module, &known_types);

    assert_eq!(
        definitions.get("AsType"),
        Some(&Type::Record(Row {
            entries: vec![field("v", named("Int"))],
            tail: RowTail::Closed,
        }))
    );
    assert_eq!(
        definitions.get("AsValue"),
        Some(&Type::Record(Row {
            entries: vec![field("v", singleton_number("3"))],
            tail: RowTail::Closed,
        }))
    );

    let check = check_module(&output.module);
    assert!(check.diagnostics.is_empty(), "{:?}", check.diagnostics);
}

// --- Match-checking holes (or-pattern join + literal pattern base kinds) ---

#[test]
fn or_pattern_on_narrow_subject_checks_arm_result_type() {
    // Hole A: open/narrow subject + or-pattern must still type the binder from
    // the alternatives that resolve, so the arm result is checked against Text.
    let source = concat!(
        "v = @A(1)\n",
        "r: Text = v ?>\n",
        "  @A(x) | @B(x) => x\n",
        "  _ => \"z\"\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH) >= 1,
        "expected type.mismatch for Int binder into Text: {:?}",
        check.diagnostics
    );
}

#[test]
fn simple_tag_arm_still_mismatches_wrong_result_type() {
    let source = concat!(
        "v = @A(1)\n",
        "r: Text = v ?>\n",
        "  @A(x) => x\n",
        "  _ => \"z\"\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH) >= 1,
        "simple arm should still mismatch Text: {:?}",
        check.diagnostics
    );
}

#[test]
fn closed_subject_or_pattern_still_mismatches_wrong_result_type() {
    let source = concat!(
        "v: @A(Int) | @B(Int) = @A(1)\n",
        "r: Text = v ?>\n",
        "  @A(x) | @B(x) => x\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH) >= 1,
        "closed-subject or-pattern should mismatch Text: {:?}",
        check.diagnostics
    );
}

#[test]
fn or_pattern_on_narrow_subject_accepts_matching_result_type() {
    let source = concat!(
        "v = @A(1)\n",
        "r: Int = v ?>\n",
        "  @A(x) | @B(x) => x\n",
        "  _ => 0\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "correct or-pattern Int result should pass: {:?}",
        check.diagnostics
    );
}

#[test]
fn or_pattern_conflicting_payload_types_leave_binder_unresolved() {
    // Conflicting Known payloads still collapse to Unknown (no join). The arm
    // body then stays deferred — no new mismatch is invented for the conflict.
    let source = concat!(
        "v: @A(Int) | @B(Text) = @A(1)\n",
        "r = v ?>\n",
        "  @A(x) | @B(x) => x\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "conflicting or-pattern payloads should not invent a type.mismatch: {:?}",
        check.diagnostics
    );
}

#[test]
fn match_literal_pattern_base_kind_mismatch_reports_type_mismatch() {
    // Hole B: Text literal pattern on number-base subject (`x = 5` → open `5 | ..`).
    let source = concat!(
        "x = 5\n",
        "y = x ?>\n",
        "  \"text-pattern\" => 1\n",
        "  _ => 2\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        matching_codes(&check.diagnostics, codes::ty::MISMATCH) >= 1,
        "expected type.mismatch for Text pattern on number subject: {:?}",
        check.diagnostics
    );
}

#[test]
fn match_same_base_kind_literal_patterns_pass() {
    // Non-matching same-kind literal stays legal (open number row / Int).
    let open_number = parse_module(concat!(
        "main = () =>\n",
        "  x = 5\n",
        "  y = x ?>\n",
        "    7 => 1\n",
        "    _ => 2\n",
        "  y\n",
    ));
    let open_check = check_module(&open_number.module);
    assert!(
        !has_diagnostic_code(&open_check.diagnostics, codes::ty::MISMATCH),
        "same-kind number pattern on open number subject should pass: {:?}",
        open_check.diagnostics
    );

    let annotated_int = parse_module(concat!(
        "main = () =>\n",
        "  x: Int = 5\n",
        "  y = x ?>\n",
        "    7 => 1\n",
        "    _ => 2\n",
        "  y\n",
    ));
    let int_check = check_module(&annotated_int.module);
    assert!(
        !has_diagnostic_code(&int_check.diagnostics, codes::ty::MISMATCH),
        "same-kind number pattern on Int subject should pass: {:?}",
        int_check.diagnostics
    );
}

#[test]
fn match_text_subject_with_text_literal_arms_passes() {
    let source = concat!(
        "main = () =>\n",
        "  x: Text = \"hi\"\n",
        "  y = x ?>\n",
        "    \"a\" => 1\n",
        "    _ => 2\n",
        "  y\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "Text subject with Text literal arms should pass: {:?}",
        check.diagnostics
    );
}

#[test]
fn match_bool_subject_with_bool_literal_arms_passes() {
    let source = concat!(
        "main = () =>\n",
        "  x: Bool = true\n",
        "  y = x ?>\n",
        "    true => 1\n",
        "    false => 2\n",
        "  y\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "Bool subject with true/false arms should pass: {:?}",
        check.diagnostics
    );
}

#[test]
fn match_deferred_scrutinee_stays_silent_on_literal_base_kinds() {
    // Unconstrained parameter: subject base kind is unknown — no false positive.
    let source = concat!(
        "f = (x) =>\n",
        "  x ?>\n",
        "    \"text\" => 1\n",
        "    _ => 2\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert!(
        !has_diagnostic_code(&check.diagnostics, codes::ty::MISMATCH),
        "deferred/unresolved scrutinee should not report base-kind mismatch: {:?}",
        check.diagnostics
    );
}

#[test]
fn pipe_operator_checks_like_the_equivalent_call() {
    // Mismatched result and argument both report through the call machinery.
    for source in [
        "f = (x: Int): Int => x + 1\nr: Text = 5 |> f\n",
        "g = (t: Text): Text => t\nr = 5 |> g\n",
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);
        assert!(
            check
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == Severity::Error),
            "expected a pipe mismatch for {source:?}: {:?}",
            check.diagnostics
        );
    }

    // Correct pipes pass, including with trailing arguments.
    let passing = parse_module(concat!(
        "f = (x: Int): Int => x + 1\n",
        "add = (x: Int, y: Int): Int => x + y\n",
        "r: Int = 5 |> f\n",
        "s: Int = 5 |> add(2)\n",
    ));
    let passing_check = check_module(&passing.module);
    assert!(
        passing_check.diagnostics.is_empty(),
        "valid pipes failed: {:?}",
        passing_check.diagnostics
    );
}

#[test]
fn or_pattern_binders_report_conflicting_payload_types() {
    let source = concat!(
        "v: @A(Int) | @B(Text) = @A(1)\n",
        "r: Text = v ?>\n",
        "  @A(x) | @B(x) => \"got\"\n",
        "  _ => \"z\"\n",
    );
    let output = parse_module(source);
    let check = check_module(&output.module);
    assert_eq!(
        matching_codes(
            &check.diagnostics,
            codes::ty::OR_PATTERN_BINDING_TYPE_CONFLICT
        ),
        1,
        "expected an or-pattern binder conflict: {:?}",
        check.diagnostics
    );

    // Agreeing payloads stay silent.
    let agreeing = parse_module(concat!(
        "v: @A(Int) | @B(Int) = @A(1)\n",
        "r: Int = v ?>\n",
        "  @A(x) | @B(x) => x\n",
        "  _ => 0\n",
    ));
    let agreeing_check = check_module(&agreeing.module);
    assert!(
        !has_diagnostic_code(
            &agreeing_check.diagnostics,
            codes::ty::OR_PATTERN_BINDING_TYPE_CONFLICT
        ),
        "agreeing or-pattern payloads should not conflict: {:?}",
        agreeing_check.diagnostics
    );
}

#[test]
fn polymorphic_functions_reject_impossible_monotypes() {
    // One shared instantiation: (a) -> a cannot inhabit (Int) -> Text.
    for source in [
        "id = (x: a): a => x\nf: (Int) -> Text = id\n",
        concat!(
            "app : ((Int) -> Text, Int) -> Text\n",
            "app = (f, x) => f(x)\n",
            "id = (x: a): a => x\n",
            "r = app(id, 1)\n",
        ),
        concat!(
            "getId : () -> ((Int) -> Text)\n",
            "getId = () =>\n",
            "  id = (x: a): a => x\n",
            "  id\n",
        ),
    ] {
        let output = parse_module(source);
        let check = check_module(&output.module);
        assert!(
            matching_codes(&check.diagnostics, codes::ty::MISMATCH) >= 1,
            "expected an instantiation mismatch for {source:?}: {:?}",
            check.diagnostics
        );
    }

    // Sound instantiations keep passing.
    let passing = parse_module(concat!(
        "apply : ((Int) -> Int, Int) -> Int\n",
        "apply = (f, x) => f(x)\n",
        "id = (x: a): a => x\n",
        "f: (Int) -> Int = id\n",
        "g: (Text) -> Text = id\n",
        "r = apply(id, 1)\n",
    ));
    let passing_check = check_module(&passing.module);
    assert!(
        passing_check.diagnostics.is_empty(),
        "sound polymorphic instantiations failed: {:?}",
        passing_check.diagnostics
    );
}
