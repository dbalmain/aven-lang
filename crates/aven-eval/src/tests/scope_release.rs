//! A comptime evaluation must not outlive itself.
//!
//! `Scope::values` memoizes closures, a closure owns an `Environment`, and
//! that environment owns an `Rc` to the scope that memoized it. Every link is
//! strong, so the chain a demand builds --- its scopes, their parents, and the
//! prelude and ambient ASTs they hold --- survives the demand unless the cycle
//! is cut. Checking a file runs one demand per site and an editor rechecks on
//! every keystroke, so an uncut cycle is unbounded growth rather than a one-off
//! cost.
//!
//! Each case here asserts the whole property: after the evaluation releases its
//! scopes and the caller drops the result, no scope the evaluation created is
//! still alive.

use std::collections::{HashMap, HashSet};

use aven_parser::{Expr, Item, Module, parse_module};

use super::super::{ComptimeDefinition, ComptimeEvalConfig, eval_comptime_expr_tracked};

fn binding_value(source: &str) -> Expr {
    let output = parse_module(source);
    assert!(
        output.diagnostics.iter().all(|d| !d.is_error()),
        "probe source must parse: {:?}",
        output.diagnostics
    );
    match output.module.items.into_iter().next() {
        Some(Item::Binding(binding)) => binding.value,
        other => panic!("probe source must be one binding, got {other:?}"),
    }
}

/// Run one demand and report how many of its scopes were created and how many
/// are still alive once it is over. A case that creates none proves nothing, so
/// every caller asserts on the count as well as the survivors.
fn scopes_after_demand(
    definition: Option<&str>,
    demand: &str,
    ambient: &[Module],
) -> (usize, usize) {
    let demand = binding_value(demand);
    let mut definitions = HashMap::new();
    if let Some(definition) = definition {
        definitions.insert(
            "f".to_owned(),
            ComptimeDefinition {
                shadows_outer: false,
                expr: binding_value(definition),
                initialization_boundary: Some(0),
            },
        );
    }
    let (result, teardown) = eval_comptime_expr_tracked(
        &demand,
        ComptimeEvalConfig {
            local_definitions: Vec::new(),
            definitions,
            active_boundary: Some(1000),
            ambient_modules: ambient,
            prelude_modules: &[],
            locals: Vec::new(),
            blocked_locals: HashSet::new(),
            fuel: 100_000,
        },
    );
    assert!(result.is_ok(), "probe must evaluate: {result:?}");
    teardown.release();
    // The caller's own reference is the one legitimate way a scope outlives the
    // evaluation, so drop it before counting.
    drop(result);
    (teardown.scope_count(), teardown.live_scopes())
}

#[track_caller]
fn assert_released(definition: Option<&str>, demand: &str) {
    assert_released_with_ambient(definition, demand, &[]);
}

#[track_caller]
fn assert_released_with_ambient(definition: Option<&str>, demand: &str, ambient: &[Module]) {
    let (created, live) = scopes_after_demand(definition, demand, ambient);
    assert!(created > 0, "probe created no scopes, so it proves nothing");
    assert_eq!(live, 0, "{live} of {created} scopes outlived the demand");
}

#[test]
fn scalar_definition_releases_its_scopes() {
    // The case that never leaked: nothing memoized points back at its scope.
    assert_released(Some("x = 41"), "y = f + 1");
}

#[test]
fn memoized_closure_releases_its_scopes() {
    // Resolving `f` memoizes a closure into the very scope it captured.
    assert_released(Some("x = (n) => n + 1"), "y = f(41)");
}

#[test]
fn block_local_closure_releases_its_scopes() {
    // A local lambda is bound, not defined, so declining to memoize it would
    // lose the name --- this is why the cycle is cut at the end instead.
    assert_released(None, "y =\n  g = (n) => n + 1\n  g(41)");
}

#[test]
fn recursive_block_local_releases_its_scopes() {
    assert_released(
        None,
        "y =\n  go = (n) => n ?> 0 => 0, _ => go(n - 1)\n  go(3)",
    );
}

#[test]
fn escaping_closure_releases_its_scopes_once_dropped() {
    // A returned closure keeps its scope alive, which is correct and bounded by
    // the caller. Once the caller drops it, nothing is left behind.
    assert_released(Some("x = (n) => n + 1"), "y = f");
}

#[test]
fn ambient_method_module_releases_its_scopes() {
    // The case that clearing scopes alone did not reach. An ambient method body
    // is a closure over the module scope that declared it, parked in a table
    // every child environment shares, so nothing reachable from a scope points
    // at it.
    let ambient = parse_module("Array(a) {\n  second(): ?a => .[1]\n}\n").module;
    assert!(
        ambient.items.len() == 1,
        "ambient probe must declare one attachment"
    );
    assert_released_with_ambient(None, "y = [1, 2, 3].second()", &[ambient]);
}
