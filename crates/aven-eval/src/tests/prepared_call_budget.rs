//! A call into a prepared closure must spend the calling demand's fuel and
//! register its scopes with the calling demand's teardown --- not the fuel
//! and teardown captured when the closure's environment was first built.
//!
//! `ComptimeSession::prepare` builds every prelude/ambient closure once,
//! against a root environment with unlimited fuel and no scope registry.
//! Before `Environment::call_child` existed, calling into one of those
//! closures reused that captured environment's `child()`, so the call ran
//! with the *preparation* time's unlimited budget and left its scopes
//! untracked by any demand's teardown --- silently unbounded compute, and a
//! retention leak, for exactly the closures a session exists to share.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use aven_parser::{Expr, Item, Module, parse_module};

use super::super::{ComptimeDefinition, ComptimeDemand, ComptimeSession, Value};

fn parsed(source: &str) -> Module {
    let output = parse_module(source);
    assert!(
        output.diagnostics.iter().all(|d| !d.is_error()),
        "probe source must parse: {:?}",
        output.diagnostics
    );
    output.module
}

fn binding_value(source: &str) -> Expr {
    match parsed(source).items.into_iter().next() {
        Some(Item::Binding(binding)) => binding.value,
        other => panic!("probe source must be one binding, got {other:?}"),
    }
}

fn empty_demand(expr: &Expr, fuel: u64) -> ComptimeDemand {
    let _ = expr;
    ComptimeDemand {
        definitions: Rc::new(HashMap::new()),
        local_definitions: Vec::new(),
        active_boundary: Some(1000),
        locals: Vec::new(),
        blocked_locals: HashSet::new(),
        fuel,
    }
}

/// A demand-local definition and a prepared prelude export must exhaust the
/// same three-step budget the same way.
#[test]
fn a_call_into_a_prepared_closure_spends_the_demands_fuel() {
    let prelude = parsed("step = (n) => n + 1\n{ step }\n");
    let session = ComptimeSession::prepare(&[], std::slice::from_ref(&prelude))
        .expect("session must prepare");

    let demand = binding_value("y = step(1)");
    let result = session.eval(&demand, empty_demand(&demand, 3));
    assert!(
        result.is_err(),
        "a prepared closure call must be bounded by the demand's fuel, not evaluate for free: {result:?}"
    );

    // The same shape, defined inside the demand instead of prepared, must
    // fail identically --- proving the budget is real and not merely absent
    // from both paths.
    let mut definitions = HashMap::new();
    definitions.insert(
        "step".to_owned(),
        ComptimeDefinition {
            shadows_outer: false,
            expr: binding_value("step = (n) => n + 1\n"),
            initialization_boundary: Some(0),
        },
    );
    let bare_session = ComptimeSession::prepare(&[], &[]).expect("session must prepare");
    let local_result = bare_session.eval(
        &demand,
        ComptimeDemand {
            definitions: Rc::new(definitions),
            ..empty_demand(&demand, 3)
        },
    );
    assert!(
        local_result.is_err(),
        "a demand-local closure must also exhaust the same budget: {local_result:?}"
    );

    // A generous budget must still let both succeed, so the failure above is
    // really about fuel and not some other break.
    let generous = session.eval(&demand, empty_demand(&demand, 1000));
    assert_eq!(generous.ok(), Some(Value::int(2)));
}

/// A call into a prepared closure must register its scopes with the *calling*
/// demand's teardown, so they die when that demand ends rather than lingering
/// until the whole session does.
///
/// `run` is prepared; the closure passed to it is demand-local, so the
/// dynamic frame `run` builds to call it is the one under test.
#[test]
fn a_call_into_a_prepared_closure_releases_its_scopes_with_the_demand() {
    let prelude = parsed("run = (f) =>\n  inner = () => f()\n  inner()\n{ run }\n");
    let session = ComptimeSession::prepare(&[], std::slice::from_ref(&prelude))
        .expect("session must prepare");

    let demand = binding_value("y = run(() => 42)");
    let (result, teardown) = session.eval_demand_tracked(&demand, empty_demand(&demand, 1000));
    assert_eq!(result.as_ref().ok(), Some(&Value::int(42)));
    teardown.release();
    drop(result);
    assert!(
        teardown.scope_count() > 1,
        "the call into `run` must have created scopes of its own to test release for"
    );
    assert_eq!(
        teardown.live_scopes(),
        0,
        "a scope formed while calling into a prepared closure outlived the demand that called it"
    );
}
