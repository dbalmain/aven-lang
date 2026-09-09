# Comptime prelude progress

Slice 2 infrastructure introduces trusted prelude module roots. A prelude is
loaded before the entry graph node; its exported record fields become ordinary
lexical defaults during checking and runtime evaluation. User declarations
still shadow them. The mechanism is generic and has no compiler branch for a
specific prelude name.

`b589d6e` is the passing metadata checkpoint. Completed slice 2 work replaces
the temporary identity export with `comptime = (@arg) => arg` and removes
checker, evaluator, and LSP builtin handling. Test helpers install explicit
prelude metadata without prepending source. Final workspace suite: **1833 tests
pass, zero failures** (`/tmp/prelude-removal-workspace-green.log`). Workspace
clippy passes (`/tmp/prelude-removal-clippy-green.log`), as do format and diff
checks. Final owner checks include 662 checker unit plus 2 fixture tests and
42 compiler unit plus 98 module tests. Generic `@` argument typing
now refines the actual argument type, preserving named families and wrappers
instead of substituting a raw evaluated literal type.

## Generic export metadata repair

The initial infrastructure copied all top-level types through host globals.
That leaked private helpers and erased generic constraints. Both graph passes
now publish only checked record exports through the same `PreludeExports`
metadata, retaining `QualifiedType` schemes and comptime function exports.
Source declarations (including non-lambda and pattern bindings) and explicit
host globals shadow defaults in both phases.

Preludes currently export ordinary values only. Implicit type exports are
rejected with a module diagnostic; types and named families still need an
explicit import. A failed check, non-record export, dependency failure, or
runtime failure stops graph processing before consumer evaluation.

Four focused module tests pass: polymorphic reuse plus method constraints and
privacy, invalid/non-record/type-export preludes, source/host shadow values,
and runtime failure blocking the entry. Full owner tests pass: checker 658 unit
plus 2 fixture tests, compiler 42 unit plus 95 module tests. Logs:
`/tmp/prelude-metadata-owner-tests.log` and `/tmp/prelude-metadata-clippy.log`.

## Builtin removal behavior

Generic `@` calls preserve each argument's actual type when recording scalar
knowledge. A present optional and a named family keep their wrappers. Label
sets retain semantic `LabelSet` knowledge while their runtime type remains
`Set(Text)`; the old `select(@tags) => tags` expectation incorrectly called
that runtime set a text-literal union and was corrected specifically.

Demanded bindings retain the whole call, not an identity-function argument.
Nested `comptime` demands through lazy bindings evaluate ordinary prelude
functions in their defining lexical scopes. Private prelude captures cannot
resolve to caller bindings. Prelude evaluation rejects diagnostic outcomes and
conservatively rejects metadata requiring unavailable family or slot
elaborations. Prelude modules are evaluated before caller definitions; source
ambient-method dependencies still need explicit imports.

Default arguments pass preceding parameter bindings to both evaluators and
infer in the function body environment. A regression checks `@b = a + 1`
against a conflicting caller `a`. Generic artifact checks reject `pin(Int)`
and a user `@` function returning a compiler type. Their label-set counterpart
continues to be an ordinary runtime set.

The current-module named-family rendering and binding-order limitations remain
scheduled for the following knowledge-channel work. Preserving the argument's
type here is not a claim that raw evaluator values preserve family rendering.

Prelude defaults occupy a separate consumer scope; exported closures retain
their own lexical base. Foreign type-evaluator contexts cannot fall back to
caller functions, demanded bindings, type definitions, or value inference.
Regressions cover private captures and a second prelude shadowing `repr`.
