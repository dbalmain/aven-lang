# String literals and comptime contract

Status: design accepted for implementation; parser, formatter, and evaluator
support are still pending. This document records the contract that library
code may target. It does not claim that the current compiler accepts every
example yet.

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

The opening delimiter must be immediately followed by a physical newline, and
the closing delimiter must be on its own line after spaces only. The two
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
Byte-level fixtures should cover blank lines, tabs, CRLF input, interpolation,
and delimiter collisions before this feature is marked implemented.

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

The pin is a phase assertion, not a conversion. It preserves the value's own
type and is the identity at runtime. Liftable values include text, numbers,
records, arrays, sets, and generated schema data. Types, modules, and other
compiler artifacts remain non-liftable unless explicitly reflected into a
runtime value.

Diagnostics must distinguish these cases:

* an argument is genuinely runtime and therefore unavailable at comptime;
* an operation is pure but its implementation is not yet comptime-evaluable;
* the result is a non-liftable compiler artifact.

Comptime evaluation must also fail in a bounded and diagnosable way. A
comptime failure, panic, or exhausted evaluation fuel reports at the pinned
expression with the operation and source span; it must not hang the checker or
silently fall back to runtime evaluation. Fuel exhaustion is a capability or
resource diagnostic, distinct from an argument that was not known.

The second case is an implementation limitation, not a user error in the
program's phase. The diagnostic should identify the operation and capability
gap so library authors do not redesign a sound API around an accidental
restriction.

See [`language-spec.md`](../../docs/language-spec.md), the comptime and string
literal sections, for the normative language draft. This document is the
adoption checklist for implementing that draft.
