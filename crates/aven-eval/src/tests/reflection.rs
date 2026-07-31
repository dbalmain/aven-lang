use super::*;

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
    // A record type is a record whose values are types, so `omit` runs over it
    // with no evaluator special case.
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
    assert_module_value("pick = 5\npick\n", Value::int(5));
}
