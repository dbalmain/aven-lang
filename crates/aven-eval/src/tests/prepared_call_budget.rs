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

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use aven_parser::{Expr, Item, Module, parse_module};

use aven_core::codes;

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

/// The same two properties, reached through a *native callback* rather than a
/// direct call.
///
/// `[1].flatMap(step)` runs `step` from inside `apply_array_flat_map`, which
/// has no evaluator environment of its own --- only the `NativeContext` it was
/// handed. Before that context carried the driving environment, the callback
/// fell back to the closure's captured state: a prepared export ran on
/// preparation time's unlimited fuel and produced `[2]` for free, while the
/// identical demand-local definition exhausted the budget. Two spellings of
/// one program disagreeing about whether it is affordable is the bug.
#[test]
fn a_native_callback_spends_the_demands_fuel() {
    const BUDGET: u64 = 6;

    let prelude = parsed("step = (n) => [n + 1]\n{ step }\n");
    let session = ComptimeSession::prepare(&[], std::slice::from_ref(&prelude))
        .expect("session must prepare");

    let demand = binding_value("y = [1].flatMap(step)");
    let prepared = session.eval(&demand, empty_demand(&demand, BUDGET));
    assert!(
        prepared.is_err(),
        "a prepared callback must spend the demand's fuel: {prepared:?}"
    );

    // The demand-local spelling of the same function, on the same budget.
    let mut definitions = HashMap::new();
    definitions.insert(
        "step".to_owned(),
        ComptimeDefinition {
            shadows_outer: false,
            expr: binding_value("step = (n) => [n + 1]\n"),
            initialization_boundary: Some(0),
        },
    );
    let bare = ComptimeSession::prepare(&[], &[]).expect("session must prepare");
    let local = bare.eval(
        &demand,
        ComptimeDemand {
            definitions: Rc::new(definitions),
            ..empty_demand(&demand, BUDGET)
        },
    );
    assert!(
        local.is_err(),
        "a demand-local callback must exhaust the same budget: {local:?}"
    );

    // And both succeed given room, so the failures above are the budget and
    // not a broken probe.
    let generous = session.eval(&demand, empty_demand(&demand, 1000));
    assert_eq!(
        generous.ok(),
        Some(Value::Array(Rc::new(vec![Value::int(2)]))),
        "the same call must succeed with a sufficient budget"
    );
}

/// A frame built while a native callback runs belongs to the calling demand,
/// however prepared its lexical parent is.
///
/// `run` is a prelude export, so `inner`'s scope hangs off the *session's*
/// chain. Reached through `flatMap`, that frame went unregistered with any
/// demand teardown, and the argument it held --- here the only strong
/// reference to a marker --- stayed alive until the session itself dropped.
/// The `Weak` is the assertion: it must be dead the moment the demand is over,
/// with the session still usable afterwards.
#[test]
fn a_native_callback_frame_is_released_with_its_demand() {
    let prelude = parsed("run = (f) =>\n  inner = () => f()\n  [inner()]\n{ run }\n");
    let session = ComptimeSession::prepare(&[], std::slice::from_ref(&prelude))
        .expect("session must prepare");

    let marker = Rc::new(Cell::new(0));
    let watch: Weak<Cell<u32>> = Rc::downgrade(&marker);
    let callback = {
        let marker = Rc::clone(&marker);
        Value::native(move |_args| {
            marker.set(marker.get() + 1);
            Ok(Value::int(1))
        })
    };
    // The demand's locals now hold the only strong reference besides `marker`
    // itself, which is dropped before the assertion.
    drop(marker);

    let demand = binding_value("y = [f].flatMap(run)");
    let (result, teardown) = session.eval_demand_tracked(
        &demand,
        ComptimeDemand {
            locals: vec![("f".to_owned(), callback)],
            ..empty_demand(&demand, 10_000)
        },
    );
    assert_eq!(
        result.as_ref().ok(),
        Some(&Value::Array(Rc::new(vec![Value::int(1)]))),
        "the probe must actually run the callback: {result:?}"
    );
    teardown.release();
    drop(result);

    assert!(
        teardown.scope_count() > 1,
        "the callback must have built frames of its own to test release for"
    );
    assert!(
        watch.upgrade().is_none(),
        "a frame built inside a native callback outlived the demand that drove it"
    );

    // The session survives its demand: a released demand must not have taken
    // the shared prelude with it.
    let again = session.eval(
        &binding_value("y = run(() => 7)"),
        empty_demand(&demand, 1000),
    );
    assert_eq!(again.ok(), Some(Value::Array(Rc::new(vec![Value::int(7)]))));
}

/// Preparation is bounded too, and by its own budget.
///
/// A prelude's top level runs during `prepare`, which happens once per check
/// and outside any demand. Before this it ran with no budget at all, so a
/// prelude that computed forever hung the checker with nothing to report ---
/// the one place where "compile time" had no ceiling. The failure must name
/// the evaluation limit, and it must be the *same* failure every time: the
/// checker caches a preparation result and reports it to every demand in the
/// file.
#[test]
fn prelude_preparation_fails_at_its_own_limit() {
    let prelude = parsed("count = (n) => count(n + 1)\nx = count(0)\n{ x }\n");

    for _ in 0..2 {
        let failure = ComptimeSession::prepare_with_fuel(&[], std::slice::from_ref(&prelude), 64)
            .err()
            .expect("an unbounded prelude must not prepare");
        assert_eq!(
            failure.code.as_deref(),
            Some(codes::comptime::EVALUATION_LIMIT),
            "preparation must fail at the evaluation limit: {failure:?}"
        );
    }

    // The same prelude prepares when its top level terminates, so the budget
    // is what rejected it rather than the shape.
    let fine = parsed("count = (n) => n + 1\nx = count(0)\n{ x }\n");
    assert!(
        ComptimeSession::prepare_with_fuel(&[], std::slice::from_ref(&fine), 64).is_ok(),
        "a terminating prelude must prepare within the same budget"
    );
}

/// Each demand gets its own allowance; a demand that exhausts one does not
/// leave the next one short.
///
/// The session is shared, so a budget stored on it rather than per demand
/// would make a file's later proofs depend on how much folding happened above
/// them --- whether a program checks would become a function of its own
/// preamble.
#[test]
fn a_failed_demand_does_not_spend_the_next_demands_allowance() {
    let prelude = parsed("step = (n) => [n + 1]\n{ step }\n");
    let session = ComptimeSession::prepare(&[], std::slice::from_ref(&prelude))
        .expect("session must prepare");

    let demand = binding_value("y = [1].flatMap(step)");
    let expected = Value::Array(Rc::new(vec![Value::int(2)]));

    assert_eq!(
        session.eval(&demand, empty_demand(&demand, 1000)).ok(),
        Some(expected.clone())
    );
    assert!(session.eval(&demand, empty_demand(&demand, 6)).is_err());
    assert_eq!(
        session.eval(&demand, empty_demand(&demand, 1000)).ok(),
        Some(expected),
        "a starved demand must not have drawn on the next demand's budget"
    );
}

/// The least fuel a demand needs before it stops failing.
fn minimum_fuel(session: &ComptimeSession, demand: &Expr) -> u64 {
    for fuel in 1..5_000 {
        if session.eval(demand, empty_demand(demand, fuel)).is_ok() {
            return fuel;
        }
    }
    panic!("probe never succeeded within the search range");
}

/// Every callback route a demand can reach spends the demand's fuel, not only
/// `flatMap`.
///
/// The routes differ in implementation --- array `flatMap` and `fold`, set
/// folding, the three lazy stream stages --- and each reached its callback
/// through its own `NativeContext`, so one of them still falling back to the
/// closure's captured budget would be invisible until someone wrote that
/// spelling.
///
/// Pass/fail at a fixed budget cannot see this: the surrounding expression
/// spends fuel too, so a starved demand fails whether or not the callback was
/// free. What discriminates is whether the callback's *body* is priced at all.
/// Each route is measured twice against the same prepared session, with two
/// exported callbacks that differ only in how much arithmetic they do. A
/// callback running on preparation's unlimited budget costs the demand
/// nothing, so the two measurements come out equal; a callback on the demand's
/// fuel makes the longer body cost more.
#[test]
fn every_callback_route_prices_its_callbacks_body() {
    let prelude = parsed(concat!(
        "cheapMap = (n) => [n]\n",
        "costlyMap = (n) => [n + 0 + 0 + 0 + 0 + 0 + 0 + 0 + 0]\n",
        "cheapAdd = (a, b) => a + b\n",
        "costlyAdd = (a, b) => a + b + 0 + 0 + 0 + 0 + 0 + 0 + 0 + 0\n",
        "cheapBump = (n) => n\n",
        "costlyBump = (n) => n + 0 + 0 + 0 + 0 + 0 + 0 + 0 + 0\n",
        "cheapKeep = (n) => n > 0\n",
        "costlyKeep = (n) => n + 0 + 0 + 0 + 0 + 0 + 0 + 0 + 0 > 0\n",
        "{ cheapMap, costlyMap, cheapAdd, costlyAdd, cheapBump, costlyBump, cheapKeep, costlyKeep }\n",
    ));
    let session = ComptimeSession::prepare(&[], std::slice::from_ref(&prelude))
        .expect("session must prepare");

    for (route, cheap, costly) in [
        (
            "Array.flatMap",
            "y = [1, 2, 3].flatMap(cheapMap)",
            "y = [1, 2, 3].flatMap(costlyMap)",
        ),
        (
            "Array.fold",
            "y = [1, 2, 3].fold(0, cheapAdd)",
            "y = [1, 2, 3].fold(0, costlyAdd)",
        ),
        (
            "Set.fold",
            "y = Set.collect([1, 2, 3]).fold(0, cheapAdd)",
            "y = Set.collect([1, 2, 3]).fold(0, costlyAdd)",
        ),
        (
            "Stream.map",
            "y = (1..4).map(cheapBump).toArray()",
            "y = (1..4).map(costlyBump).toArray()",
        ),
        (
            "Stream.filter",
            "y = (1..4).filter(cheapKeep).toArray()",
            "y = (1..4).filter(costlyKeep).toArray()",
        ),
        (
            "Stream.fold",
            "y = (1..4).fold(0, cheapAdd)",
            "y = (1..4).fold(0, costlyAdd)",
        ),
    ] {
        let cheap = minimum_fuel(&session, &binding_value(cheap));
        let costly = minimum_fuel(&session, &binding_value(costly));
        assert!(
            costly > cheap,
            "{route} priced both callbacks at {cheap}: its callback body is not spending the demand's fuel"
        );
    }
}
