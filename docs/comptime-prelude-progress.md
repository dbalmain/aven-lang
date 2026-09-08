# Comptime prelude progress

Slice 2 infrastructure introduces trusted prelude module roots. A prelude is
loaded before the entry graph node; its exported record fields become ordinary
lexical defaults during checking and runtime evaluation. User declarations
still shadow them. The mechanism is generic and has no compiler branch for a
specific prelude name.

`std/prelude.av` currently exports only `identity` while demand-outcome repair
owns the checker changes needed to install `comptime = (@arg) => arg`.

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
