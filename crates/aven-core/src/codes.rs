//! Stable diagnostic code constants — the single source of truth for every
//! `category.what-went-wrong` code the toolchain emits.
//!
//! When adding a diagnostic: define its constant here, reference it at the
//! emission site through `with_code`, list it in [`ALL`], and add a matching
//! entry to `explain::EXPLANATIONS`. The `explanation_table_*` test keeps the
//! explanation table and [`ALL`] in sync; keeping [`ALL`] complete as new
//! constants are added is the manual step the compiler cannot check.

pub mod comptime {
    pub const EVALUATION_CYCLE: &str = "comptime.evaluation-cycle";
    pub const EVALUATION_LIMIT: &str = "comptime.evaluation-limit";
    pub const EVALUATION_UNSUPPORTED: &str = "comptime.evaluation-unsupported";
    pub const HOST_FUNCTION: &str = "comptime.host-function";
    pub const NON_LIFTABLE_INTO_RUNTIME: &str = "comptime.non-liftable-into-runtime";
    pub const REFLECTION_TYPE_MISMATCH: &str = "comptime.reflection-type-mismatch";
}

pub mod layout {
    pub const INCONSISTENT_INDENTATION: &str = "layout.inconsistent-indentation";
}

pub mod module {
    pub const DYNAMIC_IMPORT: &str = "module.dynamic-import";
    pub const IMPORT_CYCLE: &str = "module.import-cycle";
    pub const IMPORT_HAS_ERRORS: &str = "module.import-has-errors";
    pub const NOT_FOUND: &str = "module.not-found";
    pub const NOT_IMPORTABLE: &str = "module.not-importable";
    pub const UNRESOLVED_IMPORT: &str = "module.unresolved-import";
    pub const UNSUPPORTED_ROOT: &str = "module.unsupported-root";
}

pub mod lex {
    pub const LEADING_BOM: &str = "lex.leading-bom";
    pub const RESERVED_OPERATOR: &str = "lex.reserved-operator";
    pub const TAB_INDENTATION: &str = "lex.tab-indentation";
    pub const UNEXPECTED_CHARACTER: &str = "lex.unexpected-character";
    pub const UNKNOWN_ESCAPE: &str = "lex.unknown-escape";
    pub const UNTERMINATED_INTERPOLATION: &str = "lex.unterminated-interpolation";
    pub const UNTERMINATED_REGEX: &str = "lex.unterminated-regex";
    pub const UNTERMINATED_STRING: &str = "lex.unterminated-string";
}

pub mod name {
    pub const ACCIDENTAL_SHADOWING: &str = "name.accidental-shadowing";
    pub const DUPLICATE_DECLARATION: &str = "name.duplicate-declaration";
    pub const DUPLICATE_LOCAL: &str = "name.duplicate-local";
    pub const NO_TOPLEVEL_SHADOW: &str = "name.no-toplevel-shadow";
    pub const SHADOW_UNBOUND: &str = "name.shadow-unbound";
    pub const UNBOUND: &str = "name.unbound";
    pub const UNUSED_BINDING: &str = "name.unused-binding";
    pub const UPPERCASE_RUNTIME_BINDING: &str = "name.uppercase-runtime-binding";
}

pub mod parse {
    pub const EXPECTED_EXPRESSION: &str = "parse.expected-expression";
    pub const EXPECTED_FIELD_NAME: &str = "parse.expected-field-name";
    pub const EXPECTED_MATCH_ARROW: &str = "parse.expected-match-arrow";
    pub const EXPECTED_PARAMETER: &str = "parse.expected-parameter";
    pub const EXPECTED_PATTERN: &str = "parse.expected-pattern";
    pub const EXPECTED_RECORD_ENTRY: &str = "parse.expected-record-entry";
    pub const EXPECTED_RECORD_LABEL: &str = "parse.expected-record-label";
    pub const EXPECTED_TYPE: &str = "parse.expected-type";
    pub const INLINE_MATCH_ARMS: &str = "parse.inline-match-arms";
    pub const INVALID_BINDING_NAME: &str = "parse.invalid-binding-name";
    pub const MISMATCHED_DELIMITER: &str = "parse.mismatched-delimiter";
    pub const MISSING_BINDING_NAME: &str = "parse.missing-binding-name";
    pub const MISSING_BINDING_VALUE: &str = "parse.missing-binding-value";
    pub const MISSING_LAMBDA_BODY: &str = "parse.missing-lambda-body";
    pub const MISSING_MATCH_ARMS: &str = "parse.missing-match-arms";
    pub const MISSING_MATCH_BODY: &str = "parse.missing-match-body";
    pub const REQUIRED_PARAM_AFTER_DEFAULT: &str = "parse.required-param-after-default";
    pub const SINGLE_ITEM_TUPLE: &str = "parse.single-item-tuple";
    pub const UNCLOSED_DELIMITER: &str = "parse.unclosed-delimiter";
    pub const UNEXPECTED_COMPTIME_MARKER: &str = "parse.unexpected-comptime-marker";
    pub const UNEXPECTED_DELIMITER: &str = "parse.unexpected-delimiter";
    pub const UNEXPECTED_INDENTATION: &str = "parse.unexpected-indentation";
    pub const UNEXPECTED_SEPARATOR: &str = "parse.unexpected-separator";
    pub const UNSUPPORTED_SYNTAX: &str = "parse.unsupported-syntax";
}

pub mod record {
    pub const REDUNDANT_UNDEFINED: &str = "record.redundant-undefined";
}

pub mod runtime {
    pub const ARITY_MISMATCH: &str = "runtime.arity-mismatch";
    pub const DIVISION_BY_ZERO: &str = "runtime.division-by-zero";
    pub const INDEX_OUT_OF_BOUNDS: &str = "runtime.index-out-of-bounds";
    pub const MISSING_FIELD: &str = "runtime.missing-field";
    pub const NO_MATCH: &str = "runtime.no-match";
    pub const NOT_CALLABLE: &str = "runtime.not-callable";
    pub const PANIC: &str = "runtime.panic";
    pub const PLATFORM_ERROR: &str = "runtime.platform-error";
    pub const TYPE_ERROR: &str = "runtime.type-error";
    pub const UNBOUND_NAME: &str = "runtime.unbound-name";
    pub const UNSUPPORTED: &str = "runtime.unsupported";
}

pub mod ty {
    pub const CYCLIC_ALIAS: &str = "type.cyclic-alias";
    pub const DECODE_FORMAT: &str = "type.decode-format";
    pub const DELETE_ABSENT_FIELD: &str = "type.delete-absent-field";
    pub const DUPLICATE_SPREAD_LABEL: &str = "type.duplicate-spread-label";
    pub const ENCODE_FORMAT: &str = "type.encode-format";
    pub const INCOMPATIBLE_MATCH_ARMS: &str = "type.incompatible-match-arms";
    pub const LITERAL_NOT_IN_UNION: &str = "type.literal-not-in-union";
    pub const LOWERCASE_VARIANT_TAG: &str = "type.lowercase-variant-tag";
    pub const MISMATCH: &str = "type.mismatch";
    pub const MISSING_FIELD: &str = "type.missing-field";
    pub const MIXED_VARIANT_ENTRIES: &str = "type.mixed-variant-entries";
    pub const NON_EXHAUSTIVE_MATCH: &str = "type.non-exhaustive-match";
    pub const OPEN_VARIANT_NOT_ASSIGNABLE: &str = "type.open-variant-not-assignable";
    pub const OR_PATTERN_BINDING_MISMATCH: &str = "type.or-pattern-binding-mismatch";
    pub const PROPAGATE_NEEDS_RESULT: &str = "type.propagate-needs-result";
    pub const PROPAGATE_NOT_RESULT: &str = "type.propagate-not-result";
    pub const RENAME_ABSENT_FIELD: &str = "type.rename-absent-field";
    pub const REPLACE_ABSENT_FIELD: &str = "type.replace-absent-field";
    pub const TUPLE_INDEX_NOT_COMPTIME: &str = "type.tuple-index-not-comptime";
    pub const UNGUARDED_EMPTY_ACCESS: &str = "type.unguarded-empty-access";
    pub const TUPLE_INDEX_OUT_OF_RANGE: &str = "type.tuple-index-out-of-range";
    pub const TYPE_ONLY_RECORD_ENTRY: &str = "type.type-only-record-entry";
    pub const UNEXPECTED_FIELD: &str = "type.unexpected-field";
    pub const UNKNOWN_NAME: &str = "type.unknown-name";
    pub const UNREACHABLE_MATCH_ARM: &str = "type.unreachable-match-arm";
    pub const UNRESOLVED_BINDING: &str = "type.unresolved-binding";
    pub const UNUSED_RESULT: &str = "type.unused-result";
    pub const WIDE_VALUE_INTO_LITERAL_UNION: &str = "type.wide-value-into-literal-union";
}

pub const ALL: &[&str] = &[
    comptime::EVALUATION_CYCLE,
    comptime::EVALUATION_LIMIT,
    comptime::EVALUATION_UNSUPPORTED,
    comptime::HOST_FUNCTION,
    comptime::NON_LIFTABLE_INTO_RUNTIME,
    comptime::REFLECTION_TYPE_MISMATCH,
    layout::INCONSISTENT_INDENTATION,
    lex::LEADING_BOM,
    lex::RESERVED_OPERATOR,
    lex::TAB_INDENTATION,
    lex::UNEXPECTED_CHARACTER,
    lex::UNKNOWN_ESCAPE,
    lex::UNTERMINATED_INTERPOLATION,
    lex::UNTERMINATED_REGEX,
    lex::UNTERMINATED_STRING,
    module::DYNAMIC_IMPORT,
    module::IMPORT_CYCLE,
    module::IMPORT_HAS_ERRORS,
    module::NOT_FOUND,
    module::NOT_IMPORTABLE,
    module::UNRESOLVED_IMPORT,
    module::UNSUPPORTED_ROOT,
    name::ACCIDENTAL_SHADOWING,
    name::DUPLICATE_DECLARATION,
    name::DUPLICATE_LOCAL,
    name::NO_TOPLEVEL_SHADOW,
    name::SHADOW_UNBOUND,
    name::UNBOUND,
    name::UNUSED_BINDING,
    name::UPPERCASE_RUNTIME_BINDING,
    parse::EXPECTED_EXPRESSION,
    parse::EXPECTED_FIELD_NAME,
    parse::EXPECTED_MATCH_ARROW,
    parse::EXPECTED_PARAMETER,
    parse::EXPECTED_PATTERN,
    parse::EXPECTED_RECORD_ENTRY,
    parse::EXPECTED_RECORD_LABEL,
    parse::EXPECTED_TYPE,
    parse::INLINE_MATCH_ARMS,
    parse::INVALID_BINDING_NAME,
    parse::MISMATCHED_DELIMITER,
    parse::MISSING_BINDING_NAME,
    parse::MISSING_BINDING_VALUE,
    parse::MISSING_LAMBDA_BODY,
    parse::MISSING_MATCH_ARMS,
    parse::MISSING_MATCH_BODY,
    parse::REQUIRED_PARAM_AFTER_DEFAULT,
    parse::SINGLE_ITEM_TUPLE,
    parse::UNCLOSED_DELIMITER,
    parse::UNEXPECTED_COMPTIME_MARKER,
    parse::UNEXPECTED_DELIMITER,
    parse::UNEXPECTED_INDENTATION,
    parse::UNEXPECTED_SEPARATOR,
    parse::UNSUPPORTED_SYNTAX,
    record::REDUNDANT_UNDEFINED,
    runtime::ARITY_MISMATCH,
    runtime::DIVISION_BY_ZERO,
    runtime::INDEX_OUT_OF_BOUNDS,
    runtime::MISSING_FIELD,
    runtime::NO_MATCH,
    runtime::NOT_CALLABLE,
    runtime::PANIC,
    runtime::PLATFORM_ERROR,
    runtime::TYPE_ERROR,
    runtime::UNBOUND_NAME,
    runtime::UNSUPPORTED,
    ty::CYCLIC_ALIAS,
    ty::DECODE_FORMAT,
    ty::DELETE_ABSENT_FIELD,
    ty::DUPLICATE_SPREAD_LABEL,
    ty::ENCODE_FORMAT,
    ty::INCOMPATIBLE_MATCH_ARMS,
    ty::LITERAL_NOT_IN_UNION,
    ty::LOWERCASE_VARIANT_TAG,
    ty::MISMATCH,
    ty::MISSING_FIELD,
    ty::MIXED_VARIANT_ENTRIES,
    ty::NON_EXHAUSTIVE_MATCH,
    ty::OPEN_VARIANT_NOT_ASSIGNABLE,
    ty::OR_PATTERN_BINDING_MISMATCH,
    ty::PROPAGATE_NEEDS_RESULT,
    ty::PROPAGATE_NOT_RESULT,
    ty::RENAME_ABSENT_FIELD,
    ty::REPLACE_ABSENT_FIELD,
    ty::TUPLE_INDEX_NOT_COMPTIME,
    ty::TUPLE_INDEX_OUT_OF_RANGE,
    ty::TYPE_ONLY_RECORD_ENTRY,
    ty::UNEXPECTED_FIELD,
    ty::UNGUARDED_EMPTY_ACCESS,
    ty::UNKNOWN_NAME,
    ty::UNREACHABLE_MATCH_ARM,
    ty::UNRESOLVED_BINDING,
    ty::UNUSED_RESULT,
    ty::WIDE_VALUE_INTO_LITERAL_UNION,
];
