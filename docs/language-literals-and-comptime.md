# String literals and comptime contract

Status: the string half is implemented — lexer, formatter, and `std/cli`'s own
generated shell fragments all use it. The comptime half is implemented **at the
pin only**; whether an ordinary call folds is an open decision, recorded at the
end of this document.

## Multiline and raw strings

Ordinary strings continue to use double quotes, escapes, and `${...}`
interpolation. Triple quoted strings use the same escape and interpolation rules
but have a deliberate layout contract:

```aven
query = """
  select *
  from users
  where id = ${userId}
  """
```

The opening delimiter must be followed by a physical newline. Spaces and tabs
may sit between the two: they belong to no line of the value, and an editor
that trims on save must not change a program's meaning. The closing delimiter
must be on its own line after spaces only. The two
boundary newlines are removed from the value. The number of spaces before the
closing delimiter is the dedent margin `N`: remove exactly `N` spaces from each
content line. A nonblank line with fewer than `N` leading spaces is an error.
Whitespace-only lines with fewer than `N` leading spaces become empty; longer
lines lose exactly `N` spaces and preserve the remainder. Tabs after the margin
are payload. A tab-indented closing delimiter is invalid. Text after the
closing delimiter is ordinary expression syntax. The closing delimiter removes
the final boundary newline, so the example has no trailing LF; adding one blank
content line requests a trailing LF.

Physical CRLF and CR line endings are normalized to LF before escape processing.
An explicit `\r` escape still contributes a carriage return to the value.

Raw strings use an `r` prefix and do not process escapes or interpolation:

```aven
script = r"""
  for item in "${items[@]}"; do
    printf '%s\n' "$item"
  done
  """
```

Raw multiline strings use the same opening, closing, and dedent rules as
ordinary multiline strings. Their value contains the text after dedenting,
including dollars, backslashes, and quotes. The initial implementation uses
`r"..."` and `r"""..."""`, plus matching hash-delimited forms with any positive
number of hashes, such as `r#"..."#` and `r##"..."##` (and their triple forms).
The opening and closing delimiter must use the same hash count. Hash delimiters
allow content such as `"#` by choosing `r##"..."##`; a closing sequence with
`##` requires a larger count.

The formatter must preserve the value of a string literal. When it reindents a
multiline literal, it moves the body and closing delimiter together so the
dedent margin changes with the source layout, never the resulting text.
Blank lines, tabs, CRLF input, interpolation and delimiter collisions are
covered at value level in `crates/aven-fmt/tests/string_values.rs`, which
decodes every payload before and after a format and compares. The formatter
fixture `multiline-strings.av` covers the layouts byte for byte.

## Comptime evaluation

Comptime is ordinary Aven evaluation over arguments and known bindings. A
function does not become comptime merely because its name is capitalized. A
known argument to a pure operation should evaluate when the checker needs its
value, and the resulting liftable value may initialize a runtime binding.

```aven
lines = ["one", "two"].joinWith("\n")
```

`comptime(value)` explicitly pins a lowercase binding to compile time. The
checker must evaluate its argument and report an error at the pin if it cannot:

```aven
body = comptime(["one", "two"].joinWith("\n"))
```

The pin is a phase assertion, not a conversion. It is the identity at runtime,
and it preserves the value's own type **up to refinement**: because the value is
now known, the pin may narrow to its singleton, but only where that singleton
already sits inside the type the expression had. It never widens and never
changes a value's shape. Liftable values include text, numbers,
records, arrays, sets, and generated schema data. Types, modules, and other
compiler artifacts remain non-liftable unless explicitly reflected into a
runtime value.

Diagnostics must distinguish these cases:

* an argument is genuinely runtime and therefore unavailable at comptime;
* an operation is pure but its implementation is not yet comptime-evaluable;
* the result is a non-liftable compiler artifact.

Comptime evaluation must also fail in a bounded and diagnosable way. A
comptime failure, panic, or exhausted evaluation fuel reports at the pinned
expression, naming the resource or operation that stopped it; it must not hang
the checker or silently fall back to runtime evaluation. A cause raised inside
an ambient module carries that module's source offsets, which are not positions
in the program being checked: it contributes its message, never its span.

The second case is an implementation limitation, not a user error in the
program's phase. The diagnostic should identify the operation and capability
gap so library authors do not redesign a sound API around an accidental
restriction.

## What is implemented, and the one open decision

`comptime(e)` evaluates through ordinary helpers, ambient method bodies,
lexical captures and record shorthand, with nothing marked comptime, and
narrows to the value it produced. It narrows only to a *refinement*: the
singleton must already sit inside the type the expression had. A base kind
admits its own literals and an open literal row admits one more of its own
base. A named family such as `Money` is not a base kind, which is what stops a
branded value folding to a raw number and picking up plain-`Int` behavior.

The spec's *Comptime by inference* section asks for more than this: that **any**
call with comptime-known arguments folds, so that `double(2)` has type `4`
under a `(Int) -> Int` signature. That is not implemented. An attempt at it
narrowed `Map.get` from `?Int` to `1`, collapsed a `1 | 1.0` match join to
whichever branch ran, folded a branded `Money` to its raw `Int`, and cost an
evaluator environment clone on every inferred call — 36 checker tests, several
of them soundness properties, disagreed with it. Doing it correctly needs a
lifting rule for `Optional`, `Result`, records and named families, and a cost
story for evaluating calls the program never asked to fold.

See [`language-spec.md`](../../docs/language-spec.md), the comptime and string
literal sections, for the normative language draft. This document is the
adoption checklist for implementing that draft.
