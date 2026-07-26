# Integer representation — arbitrary-precision `Int`

Status: implemented 2026-07-27.

The Exercism-derived benchmark campaign found four cases that Aven omitted
because their values exceeded signed 64-bit range. This note originally
recommended keeping `Int` as `i64`. Dave overruled that recommendation on the
authority of the language spec:

> Definitely want to implement this. Performance isn't an issue right now -
> we'll address that when we move to a virtual machine.

That decision is now implemented. Aven's `Int` is arbitrary precision
end-to-end, with no small-integer fast path. This keeps the implementation
obvious and leaves representation optimization to the VM phase.

## Representation

`aven-core::Int` is a newtype over `num_bigint::BigInt`. The wrapper keeps the
dependency and conversion vocabulary in one place while allowing the evaluator,
checker, and hosts to share the same integer type.

| Layer | Representation |
| ----- | -------------- |
| Runtime value | `Value::Int(Int)` |
| Branded payload | `PrimitivePayload::Int(Int)` |
| Comptime canonical form | `CanonicalLiteral::Int(Int)` |
| Format codecs | `FormatNumber::Int(Int)` |
| AST / tokens | Decimal string lexeme, parsed at the checker/evaluator boundary |
| Host marshal | Lossless `Int` and checked `i64` implementations |

The lexer and parser still retain decimal integer text. They do not impose a
range, and the checker and evaluator both parse integer-shaped lexemes as
`Int`. This makes `aven check` and `aven run` agree and naturally makes
`-9223372036854775808` writable: the positive operand is representable before
unary negation is applied.

## Arithmetic and values

Integer `+`, `-`, `*`, `/`, `%`, unary `-`, comparisons, equality, hashing,
display, `repr`, interpolation, literal patterns, and constant folding all use
the arbitrary-precision representation. The old `integer arithmetic overflow`
diagnostic has been removed. Division by zero remains a distinct error.

`Int.pow` accepts a non-negative exponent that fits `u32`, matching
`num-bigint`'s exponent API. That is an operational argument limit, not an
integer value range or arithmetic overflow path. Collection indexes, lengths,
repeat counts, padding widths, HTTP timeouts, and similar interactions still
convert to the machine-sized type required by the operation and report a clean
error when the value cannot be used.

Mixed `Int`/`Float` operations remain explicitly lossy because `Float` is
`f64`. Conversion of a finite `Int` too large for `f64` produces the
corresponding infinity. Pure integer operations never convert through
floating point.

## Typed host boundary

The host boundary deliberately supports both common use cases:

- `AvenMarshal for Int` crosses the boundary losslessly. Host APIs that need
  arbitrary-precision values can accept or return the re-exported
  `aven_host::Int`.
- `AvenMarshal for i64` remains available for ordinary Rust platform
  functions. When an Aven value is outside signed 64-bit range,
  `from_value` returns a platform error such as
  `Int value 18446744073709551615 does not fit Rust i64`.

There is no truncation, wrapping, or implicit float conversion. A host chooses
its contract through its Rust signature.

Temporal host values remain internally signed 64-bit nanoseconds because that
is a property of those platform types, not of Aven `Int`. Their constructors
perform checked conversion and retain their documented supported range.

## Codec boundary

### JSON

JSON integer lexemes decode as arbitrary-precision `@Int` values, and encoding
writes their exact unquoted decimal form. `serde_json` is built with
`arbitrary_precision` so its intermediate number representation does not
discard the lexeme before Aven sees it.

Before this change, the behavior was verified directly:

```text
Json.decode("18446744073709551615")?!  # @Float(18446744073709552000.0)
Json.encode(...)                       # 1.8446744073709552e+19
```

That silent precision loss is gone. The same value now decodes as
`@Int(18446744073709551615)` and round-trips as
`18446744073709551615`. Fraction or exponent lexemes still decode as `@Float`;
the split is syntactic rather than range-based.

The CLI's structured JSON output also emits arbitrary-precision Aven integers
as JSON numbers rather than demoting them to float or string.

### YAML and TOML

YAML signed and unsigned integer inputs both become exact Aven `Int` values.
The current YAML library's output number API accepts only `i64` or `u64`, so
encoding an Aven integer beyond those ranges returns a clean codec error.

TOML integers are signed 64-bit by specification and in the library API.
Decoding produces an Aven `Int`; encoding rejects values outside TOML's range
with a clean codec error. Neither codec silently truncates or converts to
float.

## Why this option

The earlier note treated the typed host boundary as the strongest argument
against arbitrary precision. The implemented boundary resolves that concern
without changing Aven's user model: full-width hosts opt into `Int`, while
fixed-width hosts receive an explicit conversion error. JSON, the format where
large integer identifiers most commonly occur, can preserve decimal integer
lexemes losslessly.

Using `num-bigint` adds allocation and cloning to integer-heavy evaluator code,
including small integers. That is accepted for the direct evaluator: correctness
and a small uniform implementation matter more now, and Dave explicitly
deferred the small-integer optimization to the VM.

## Deliberately separate work

- Hexadecimal, binary, and octal literals (`0x`, `0b`, `0o`) remain
  unimplemented by the lexer even though the language spec lists them.
- Fixed-width Aven interop types such as `I32` and `U8` remain future work for
  binary protocols and explicitly width-sensitive host APIs.
- A tagged small/large integer representation remains VM-phase performance
  work.
- Temporal nanoseconds, collection indexes and sizes, HTTP timeouts, Unicode
  scalar conversion, float formatting precision, and exponent counts retain
  the finite ranges imposed by the operations they drive.

## Decision

**Aven `Int` is arbitrary precision by default.** The former recommendation to
keep signed 64-bit `Int`, retain checked-overflow diagnostics, and demote large
JSON integers to `@Float` is superseded. Future work should not reintroduce a
fixed-width assumption into ordinary integer literals or arithmetic; bounded
conversions belong at explicitly bounded host or operational boundaries.
