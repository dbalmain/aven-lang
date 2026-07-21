use crate::codes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticExplanation {
    pub code: &'static str,
    pub text: &'static str,
}

pub fn explain(code: &str) -> Option<DiagnosticExplanation> {
    EXPLANATIONS
        .iter()
        .copied()
        .find(|explanation| explanation.code == code)
}

const EXPLANATIONS: &[DiagnosticExplanation] = &[
    DiagnosticExplanation {
        code: codes::comptime::ARGUMENT_BOUND,
        text: "A comptime function argument is a value whose type does not fit the parameter's annotated bound. Pass a comptime-known value that fits the declared type (for example a literal that is a member of a literal union).",
    },
    DiagnosticExplanation {
        code: codes::comptime::ARGUMENT_KIND_MISMATCH,
        text: "A comptime function argument has the wrong kind for its parameter annotation: a parameter annotated `Type` needs a type, and a parameter annotated with a value type needs a comptime-known value. Swap the argument or correct the parameter annotation.",
    },
    DiagnosticExplanation {
        code: codes::comptime::ARGUMENT_NOT_KNOWN,
        text: "An uppercase comptime function was applied to an argument that is not known at compile time. Pass a type or another comptime-known value instead of a runtime value.",
    },
    DiagnosticExplanation {
        code: codes::comptime::EVALUATION_CYCLE,
        text: "A compile-time function specialization recursively depends on the same function and compile-time argument tuple while that result is still being evaluated. Rewrite the function so recursive specializations bottom out before repeating the same tuple.",
    },
    DiagnosticExplanation {
        code: codes::comptime::EVALUATION_LIMIT,
        text: "Compile-time evaluation exceeded the evaluator's fuel budget. Simplify the compile-time computation or make recursion terminate in fewer specialization steps.",
    },
    DiagnosticExplanation {
        code: codes::comptime::EVALUATION_UNSUPPORTED,
        text: "A compile-time binding has a right-hand side that must be evaluated, but the compile-time evaluator is not implemented yet. Use a literal type or value, or move runtime computations to lowercase bindings.",
    },
    DiagnosticExplanation {
        code: codes::comptime::HOST_FUNCTION,
        text: "A host-provided compile-time function could not resolve a result type for the supplied compile-time arguments. Check the argument value or the host function's accepted domain.",
    },
    DiagnosticExplanation {
        code: codes::comptime::NON_LIFTABLE_INTO_RUNTIME,
        text: "A lowercase runtime binding tried to store a compile-time-only artifact such as a type. Keep type artifacts under capitalized names, or compute a runtime-representable value instead.",
    },
    DiagnosticExplanation {
        code: codes::comptime::REDUNDANT_COMPTIME_MARKER,
        text: "Parameters of an uppercase comptime function are already compile-time parameters. Remove the `@` marker.",
    },
    DiagnosticExplanation {
        code: codes::comptime::REFLECTION_TYPE_MISMATCH,
        text: "A compile-time reflection function was applied to a concrete type it cannot inspect. Pass the kind of type the reflection function expects, or leave the expression deferred until the subject type is known.",
    },
    DiagnosticExplanation {
        code: codes::comptime::UNEXPANDABLE_IMPORT,
        text: "An imported name was applied in type position, but the importer could not expand it to a concrete type. Export a comptime type function from the dependency (params + body that evaluate at compile time), or write the type annotation without that application.",
    },
    DiagnosticExplanation {
        code: codes::config::OPERATOR_ARGUMENT_MALFORMED,
        text: "A command-line operator declaration does not have the required --operator=TOKEN:ANCHOR:ASSOCIATIVITY form. Use a valid custom token, one of the nine fixed precedence anchors, and left, right, or none associativity.",
    },
    DiagnosticExplanation {
        code: codes::config::OPERATOR_FIXITY_CONFLICT,
        text: "More than one configuration authority declared fixity for the same custom operator token. Remove every declaration except one; manifest, shebang, command-line, and platform declarations never override or deduplicate each other.",
    },
    DiagnosticExplanation {
        code: codes::config::OPERATOR_MANIFEST_INVALID,
        text: "The Aven.toml operators table is not in its fixed shape. Quote each custom-token key and give it exactly the string fields precedence and associativity, with no missing, duplicate, or unknown fields.",
    },
    DiagnosticExplanation {
        code: codes::config::OPERATOR_SHEBANG_MALFORMED,
        text: "The entry's first line starts as a shebang but is not one of Aven's restricted run forms. Use /usr/bin/env -S aven run or an absolute path ending in aven followed only by repeated --operator=TOKEN:ANCHOR:ASSOCIATIVITY flags.",
    },
    DiagnosticExplanation {
        code: codes::config::OPERATOR_TOKEN_INVALID,
        text: "An operator fixity declaration names text that is not a legal custom operator token. Use a non-empty ASCII run from Aven's custom-operator alphabet that does not start with = and does not use a reserved syntax family.",
    },
    DiagnosticExplanation {
        code: codes::config::OPERATOR_TOKEN_RESERVED,
        text: "An operator fixity declaration tries to register an existing fixed or syntax-reserved token. Custom fixity applies only to new custom tokens; remove this declaration or choose a different token.",
    },
    DiagnosticExplanation {
        code: codes::layout::INCONSISTENT_INDENTATION,
        text: "A line dedented to a column that does not match an open layout block. Align it with an existing block level or change the surrounding indentation.",
    },
    DiagnosticExplanation {
        code: codes::lex::LEADING_BOM,
        text: "The file starts with a UTF-8 byte-order mark. Aven source files should be plain UTF-8 without a leading BOM.",
    },
    DiagnosticExplanation {
        code: codes::lex::RESERVED_OPERATOR,
        text: "This symbolic operator starts with a reserved character sequence. Operators beginning with =, :, ., ?, or @ are reserved for language syntax.",
    },
    DiagnosticExplanation {
        code: codes::lex::TAB_INDENTATION,
        text: "Tabs are not accepted in indentation for v0. Use spaces so layout depth is stable across editors.",
    },
    DiagnosticExplanation {
        code: codes::lex::UNEXPECTED_CHARACTER,
        text: "The lexer found a character that is not part of Aven source syntax. Remove it or rewrite it using a supported literal, identifier, or operator form.",
    },
    DiagnosticExplanation {
        code: codes::lex::UNKNOWN_ESCAPE,
        text: "A string literal contains an unsupported or malformed escape sequence. Use one of the supported escapes: \\\\, \\\", \\n, \\r, \\t, or \\u{H} with a valid Unicode scalar value.",
    },
    DiagnosticExplanation {
        code: codes::lex::UNTERMINATED_INTERPOLATION,
        text: "A string interpolation was opened but the interpolated string did not reach its closing quote. Close the interpolation with } and close the surrounding string with a quote.",
    },
    DiagnosticExplanation {
        code: codes::lex::UNTERMINATED_REGEX,
        text: "A regex literal was opened but not closed. Add the closing /, or use Regex.compile(pattern) when the pattern is dynamic.",
    },
    DiagnosticExplanation {
        code: codes::lex::UNTERMINATED_STRING,
        text: "A string literal was opened but not closed. Add the closing quote, or use a raw string form once multi-line string support exists.",
    },
    DiagnosticExplanation {
        code: codes::module::CAPABILITY_UNAVAILABLE,
        text: "The imported module is a platform-capability module the embedding host did not enable. The host controls capabilities; register the named capability (e.g. Host::register_clock()) or drop the import.",
    },
    DiagnosticExplanation {
        code: codes::module::DYNAMIC_IMPORT,
        text: "The import specifier is not a static string literal. Import specifiers are comptime-only; dynamic imports never run at runtime.",
    },
    DiagnosticExplanation {
        code: codes::module::IMPORT_CYCLE,
        text: "A group of modules import each other in a cycle. Break the cycle by moving shared declarations to a lower-level module or inlining one dependency.",
    },
    DiagnosticExplanation {
        code: codes::module::IMPORT_HAS_ERRORS,
        text: "An imported module produced errors while being loaded, checked, or evaluated. Fix the dependency first, then check or run the importer again.",
    },
    DiagnosticExplanation {
        code: codes::module::NOT_FOUND,
        text: "A module import resolved to a path that does not exist. Check the specifier spelling, root, directory, and optional .av extension.",
    },
    DiagnosticExplanation {
        code: codes::module::NOT_IMPORTABLE,
        text: "A module's final expression is not a statically-known record, so it cannot be imported as a namespace. End the file with a literal record of exported bindings.",
    },
    DiagnosticExplanation {
        code: codes::module::ROOT_UNAVAILABLE,
        text: "This import uses a project, home, or filesystem root that the host did not provide. Configure the embedding host with the required root, or use a root it exposes.",
    },
    DiagnosticExplanation {
        code: codes::module::UNRESOLVED_IMPORT,
        text: "This context checks one file at a time, so the imported module is not loaded and its contents are unknown here. A module-graph host such as `aven check` or `aven run` can resolve it.",
    },
    DiagnosticExplanation {
        code: codes::module::UNSUPPORTED_ROOT,
        text: "This import uses a bare library, package, or otherwise unsupported root. Project, home, and filesystem roots are supported when the host provides them.",
    },
    DiagnosticExplanation {
        code: codes::module::UPPERCASE_EXPORT_NOT_TYPE,
        text: "Uppercase fields in a module export record must name explicitly exported type aliases.",
    },
    DiagnosticExplanation {
        code: codes::name::ACCIDENTAL_SHADOWING,
        text: "A local binding introduced with = reuses a name that is already visible. Rename it, or use := to shadow the existing binding intentionally.",
    },
    DiagnosticExplanation {
        code: codes::name::DUPLICATE_DECLARATION,
        text: "Two top-level declarations have the same name and are not a clearly typed overload set. Rename one declaration or add complete annotations for overloads.",
    },
    DiagnosticExplanation {
        code: codes::name::DUPLICATE_LOCAL,
        text: "Two local binders introduce the same name in one scope. Keep only one binding or rename one of them.",
    },
    DiagnosticExplanation {
        code: codes::name::NO_TOPLEVEL_SHADOW,
        text: "`:=` shadows a binding inside a block, but top-level declarations are one mutually-recursive group with unique names, so there is nothing to sequentially shadow. Use a distinct name, or move the shadow into a block.",
    },
    DiagnosticExplanation {
        code: codes::name::NO_TOPLEVEL_SPREAD_SHADOW,
        text: "The :.. block-spread replacement form is sequential rebinding, but top-level declarations are mutually recursive and cannot be replaced in order. Use .. at top level, or move :.. into a block.",
    },
    DiagnosticExplanation {
        code: codes::name::RESERVED_TYPE,
        text: "This type name belongs to Aven's builtins or to a host-provided type definition. Pick another name so annotations and type operations continue to resolve the reserved type consistently.",
    },
    DiagnosticExplanation {
        code: codes::name::RUNTIME_NAME_ALIAS,
        text: "An uppercase binding is a type alias, but its bare lowercase right-hand side is a runtime name rather than a known type. Bind a type instead, or rename the binding to lowercase for a runtime value.",
    },
    DiagnosticExplanation {
        code: codes::name::SHADOW_UNBOUND,
        text: "An explicit shadow binding introduced with := has no visible binding to shadow. Use = to introduce a fresh binding, or move the shadow after the binding it replaces.",
    },
    DiagnosticExplanation {
        code: codes::name::UNBOUND,
        text: "A runtime value expression referenced a name that is not bound by a local binding, top-level declaration, or host global. Check the spelling, or define the name before it is used.",
    },
    DiagnosticExplanation {
        code: codes::name::UNUSED_BINDING,
        text: "A local binding, parameter, or pattern binder is never used. Remove it, use it, or prefix the name with _ to mark the unused binding as intentional.",
    },
    DiagnosticExplanation {
        code: codes::name::UPPERCASE_MODULE_BINDING,
        text: "Uppercase names are reserved for types, but this binding's value is a module record, not a type. Bind the module with a lowercase name, or extract an exported type with a record pattern such as { User } = import(...).",
    },
    DiagnosticExplanation {
        code: codes::name::UPPERCASE_RUNTIME_BINDING,
        text: "Uppercase names are reserved for compile-time identifiers. Runtime parameters must use lowercase names.",
    },
    DiagnosticExplanation {
        code: codes::parse::CUSTOM_INFIX_NOT_ROOT,
        text: "A custom operator token was used as bare infix syntax in a dependency module. Bare custom infix is reserved for the designated compilation entry; rewrite the expression as an explicit left-receiver method call such as left.**(right).",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_EXPRESSION,
        text: "The parser expected an expression term such as a literal, identifier, call, collection, lambda, or parenthesized group.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_FIELD_NAME,
        text: "A field access or nil-safe field access must be followed by a field name.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_MATCH_ARM,
        text: "A match arm separator comma must be followed by another arm of the form pattern => expression. Remove the trailing comma, or add the missing arm. Inline matches greedily own commas, so nest them in parentheses when they appear inside a call, collection, or another inline arm body.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_MATCH_ARROW,
        text: "A match arm pattern must be followed by => before its body expression.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_PARAMETER,
        text: "A lambda parameter must be an identifier, or _ when the argument is intentionally ignored.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_PATTERN,
        text: "The parser expected a pattern term in a match arm. Use a literal, name, constructor call, tuple, record pattern, or _ wildcard.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_RECORD_ENTRY,
        text: "The parser expected a record entry such as a field, shorthand, spread, overwrite spread, delete, rename, or comprehension entry.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_RECORD_LABEL,
        text: "A record entry needs a valid field name. Use an identifier-style field name for now; quoted string field names are reserved for a later parser slice.",
    },
    DiagnosticExplanation {
        code: codes::parse::EXPECTED_TYPE,
        text: "The parser expected a type annotation term after :. Type syntax uses the same expression grammar as value syntax.",
    },
    DiagnosticExplanation {
        code: codes::parse::INVALID_BINDING_NAME,
        text: "A binding name must be a single identifier. Use a lowercase runtime identifier or uppercase compile-time identifier before =.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISMATCHED_DELIMITER,
        text: "A closing delimiter does not match the most recent open delimiter. Change it to the expected delimiter or fix the nested grouping.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISSING_BINDING_NAME,
        text: "A binding is missing the name before =. Add a name such as value = expression.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISSING_BINDING_VALUE,
        text: "A binding is missing its value expression. Add an expression after =, or put an indented block on the following lines.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISSING_LAMBDA_BODY,
        text: "A lambda was introduced with => but no body followed. Add an expression body on the same line or an indented block.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISSING_MATCH_ARMS,
        text: "A ?> match expression must be followed by an indented block of pattern => body arms.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISSING_MATCH_BODY,
        text: "A match arm has a pattern and => but no body expression. Add the expression returned by that arm.",
    },
    DiagnosticExplanation {
        code: codes::parse::MISSING_METHOD_BOUND_OPEN,
        text: "A method bound must end with .. so it can accept additional methods. Insert , .. before the closing brace.",
    },
    DiagnosticExplanation {
        code: codes::parse::OPERATOR_ASSOCIATIVITY_CONFLICT,
        text: "Two unparenthesized operators at the same precedence level have incompatible associativity, or one is non-associative. Add parentheses to state the intended grouping.",
    },
    DiagnosticExplanation {
        code: codes::parse::OPERATOR_FIXITY_UNDECLARED,
        text: "A legal custom operator token was used as bare infix syntax in the compilation entry without a fixity declaration. Declare it in Aven.toml, the entry shebang, or a --operator argument, or use an explicit left-receiver method call such as left.**(right).",
    },
    DiagnosticExplanation {
        code: codes::parse::OPERATOR_MEMBER_PARAMETER_LIST,
        text: "An operator member must be followed by a parameter list. Write an operator such as <(Self): Bool.",
    },
    DiagnosticExplanation {
        code: codes::parse::QUOTED_METHOD_MEMBER,
        text: "Method members use bare lowercase names or bare operator tokens. Remove the quotes from the member name.",
    },
    DiagnosticExplanation {
        code: codes::parse::REQUIRED_PARAM_AFTER_DEFAULT,
        text: "A parameter without a default may not follow one with a default. Give it a default, or move it before the defaulted parameters so defaults stay trailing.",
    },
    DiagnosticExplanation {
        code: codes::parse::SINGLE_ITEM_TUPLE,
        text: "Anonymous one-item tuples are not allowed. Remove the comma for grouping, or use a tagged one-item tuple such as @Ok(value).",
    },
    DiagnosticExplanation {
        code: codes::parse::UNCLOSED_DELIMITER,
        text: "A delimiter such as (, [, {, or @{ was opened but not closed. Add the matching closing delimiter.",
    },
    DiagnosticExplanation {
        code: codes::parse::UNEXPECTED_COMPTIME_MARKER,
        text: "The @ comptime marker is declaration-only. Use it only before a lowercase parameter name in a lambda parameter list, and refer to that parameter by its ordinary name in the body.",
    },
    DiagnosticExplanation {
        code: codes::parse::UNEXPECTED_DELIMITER,
        text: "A closing delimiter appeared where no matching opener was active. Remove it or add the missing opener.",
    },
    DiagnosticExplanation {
        code: codes::parse::UNEXPECTED_INDENTATION,
        text: "An indented line appeared where the parser was not expecting a layout block. Remove the indentation or introduce a block-form expression.",
    },
    DiagnosticExplanation {
        code: codes::parse::UNEXPECTED_SEPARATOR,
        text: "A collection contains an extra comma or semicolon separator. Remove the duplicate separator.",
    },
    DiagnosticExplanation {
        code: codes::parse::UNSUPPORTED_SYNTAX,
        text: "The syntax is intentionally not supported by the current parser slice. Rewrite using currently supported operators or wait for the planned syntax milestone.",
    },
    DiagnosticExplanation {
        code: codes::parse::VARIANT_METHOD,
        text: "Type-carried methods may be declared only on named records in this version. Variant method carriage is not implemented yet.",
    },
    DiagnosticExplanation {
        code: codes::record::REDUNDANT_UNDEFINED,
        text: "A record field explicitly set to undefined is equivalent to omitting the field. Omit the field, or use the delete-entry form when a spread field should be removed.",
    },
    DiagnosticExplanation {
        code: codes::runtime::ARITY_MISMATCH,
        text: "A runtime function call supplied the wrong number of arguments. Pass exactly the number of arguments declared by the lambda parameters.",
    },
    DiagnosticExplanation {
        code: codes::runtime::DIVISION_BY_ZERO,
        text: "Runtime evaluation tried to divide or take a remainder by zero. Change the right operand of `/` or `%` so it evaluates to a non-zero number before running the program.",
    },
    DiagnosticExplanation {
        code: codes::runtime::INDEX_OUT_OF_BOUNDS,
        text: "Runtime evaluation tried to read a fixed-arity tuple at an index outside its bounds. Use an in-range tuple index, match the tuple shape, or use an array when out-of-bounds lookup should produce undefined.",
    },
    DiagnosticExplanation {
        code: codes::runtime::MISSING_FIELD,
        text: "Runtime evaluation tried to read a record field that is not present on the record value. Add the field before the lookup, change the field name, or handle the absent-field case once optional access exists.",
    },
    DiagnosticExplanation {
        code: codes::runtime::NO_MATCH,
        text: "Runtime pattern matching reached the end of the arm list without finding a matching pattern whose guards all passed. Add a default arm, handle the missing case explicitly, or run the checker to catch non-exhaustive matches before evaluation.",
    },
    DiagnosticExplanation {
        code: codes::runtime::NOT_CALLABLE,
        text: "Runtime evaluation tried to call a value that is not a function. Only closures produced by lambda expressions and host-injected native functions are callable in the current evaluator.",
    },
    DiagnosticExplanation {
        code: codes::runtime::PANIC,
        text: "An explicit panic operator (`?!`) unwrapped an `@Err` result. Panics abort evaluation with the error payload; use `?^` instead to propagate the `@Err` out of the enclosing function, or handle the `@Err` with a match.",
    },
    DiagnosticExplanation {
        code: codes::runtime::PLATFORM_ERROR,
        text: "A host-provided platform function reported an error while running. The platform boundary is effectful, so inspect the call and the host error message attached to it.",
    },
    DiagnosticExplanation {
        code: codes::runtime::TYPE_ERROR,
        text: "Runtime evaluation reached an operator with operands it cannot accept. Use operands of the expected runtime kind, or add a static check once the relevant language feature exists.",
    },
    DiagnosticExplanation {
        code: codes::runtime::UNBOUND_NAME,
        text: "Runtime evaluation referenced a name that has not been bound in the current environment. Define the name before the reference, or move the reference after the binding because runtime evaluation is sequential.",
    },
    DiagnosticExplanation {
        code: codes::runtime::UNSUPPORTED,
        text: "Runtime evaluation reached syntax that is parsed but not implemented by the current evaluator slice. Rewrite the program using supported expression forms or wait for the planned evaluator milestone.",
    },
    DiagnosticExplanation {
        code: codes::ty::BRACKET_TYPE_APPLICATION,
        text: "Bracket type application has been removed. Use ordinary call syntax such as Result(Int, Text); postfix square brackets are reserved for indexing.",
    },
    DiagnosticExplanation {
        code: codes::ty::COALESCE_NEVER_EMPTY,
        text: "The left operand of `??` has a type that cannot be `null` or `undefined`, so the fallback expression is dead code. Remove the `??` fallback, or give the left operand an optional/nullable type when emptiness is intended.",
    },
    DiagnosticExplanation {
        code: codes::ty::CYCLIC_ALIAS,
        text: "A transparent type alias eventually refers back to itself without passing through a type constructor. Wrap one member in a record or variant to make the recursion well-founded, or remove the alias.",
    },
    DiagnosticExplanation {
        code: codes::ty::DECODE_FORMAT,
        text: "A `text.decode(...)` call is missing its format argument or was given a first argument that is not a format type. Pass a format type such as `Json`, `Yaml`, or `Toml` as the first argument so it can supply the decoder.",
    },
    DiagnosticExplanation {
        code: codes::ty::DELETE_ABSENT_FIELD,
        text: "A row transform tried to delete a record field or variant tag that is not present in the closed row accumulated so far. Spread or add that label first, or remove the delete.",
    },
    DiagnosticExplanation {
        code: codes::ty::DIVISION_BY_ZERO,
        text: "Integer division and remainder require a statically non-zero divisor. Replace the zero literal, use the checked Int.div or Int.mod method, or convert the operands to Float.",
    },
    DiagnosticExplanation {
        code: codes::ty::DIVISOR_NOT_STATIC,
        text: "Integer division and remainder require a non-zero integer literal as the divisor. Use the checked Int.div or Int.mod method for a divisor chosen at runtime, or convert the operands to Float.",
    },
    DiagnosticExplanation {
        code: codes::ty::DUPLICATE_SPREAD_LABEL,
        text: "A disjoint row spread or add introduced a label that is already present. Use an overwrite form such as `:..source` or `field :: Type` when replacement is intended.",
    },
    DiagnosticExplanation {
        code: codes::ty::ENCODE_FORMAT,
        text: "A `value.encode(...)` call is missing its format argument or was given a first argument that is not a format type. Pass a format type such as `Json`, `Yaml`, or `Toml` as the first argument so it can supply the encoder.",
    },
    DiagnosticExplanation {
        code: codes::ty::INCOMPATIBLE_MATCH_ARMS,
        text: "An unannotated runtime match has arms whose result types cannot be combined into one type. Make every arm return the same kind of value, or add a result annotation so each arm is checked against it.",
    },
    DiagnosticExplanation {
        code: codes::ty::INVALID_OPERATOR_OPERANDS,
        text: "This operator has no rule for the resolved operand types. Use operands accepted by the operator, or choose an operation that matches their types.",
    },
    DiagnosticExplanation {
        code: codes::ty::LITERAL_NOT_IN_UNION,
        text: "A fresh literal value or literal-union type contains a member that is not listed by the expected closed literal union. Use one of the listed literal values or widen the annotation.",
    },
    DiagnosticExplanation {
        code: codes::ty::LOWERCASE_VARIANT_TAG,
        text: "Variant type members must use uppercase `@`-tags. Rename the tag to an uppercase marker such as @Ok or @Error.",
    },
    DiagnosticExplanation {
        code: codes::ty::MISMATCH,
        text: "A literal binding value cannot satisfy its declared scalar annotation. Change the value or change the annotation so they agree.",
    },
    DiagnosticExplanation {
        code: codes::ty::MISSING_FIELD,
        text: "A record value is missing a field required by its declared record type. Add the field or make the type field optional.",
    },
    DiagnosticExplanation {
        code: codes::ty::MIXED_VARIANT_ENTRIES,
        text: "A variant row mixes tag entries and literal entries. This slice keeps rows homogeneous: use either variant tags or literal values in one row.",
    },
    DiagnosticExplanation {
        code: codes::ty::NON_EXHAUSTIVE_MATCH,
        text: "A variant match must cover every tag in a closed row. Matches on open variant rows also need a default arm because additional tags may be present.",
    },
    DiagnosticExplanation {
        code: codes::ty::NOT_INDEXABLE,
        text: "This value does not support square-bracket indexing. Index an Array, Map, tuple, or record instead.",
    },
    DiagnosticExplanation {
        code: codes::ty::OPEN_VARIANT_NOT_ASSIGNABLE,
        text: "An inferred variant with an open row tail may carry tags not listed in a closed annotation. Make the annotation open with `..` or constrain the value to a closed variant row before assigning it.",
    },
    DiagnosticExplanation {
        code: codes::ty::OR_PATTERN_BINDING_MISMATCH,
        text: "Every alternative in an or-pattern must bind the same names. Rename, add, or remove binders so the match arm body sees one consistent local environment.",
    },
    DiagnosticExplanation {
        code: codes::ty::OR_PATTERN_BINDING_TYPE_CONFLICT,
        text: "An or-pattern binder has different resolved payload types across alternatives. Use separate match arms when the alternatives need different binder types.",
    },
    DiagnosticExplanation {
        code: codes::ty::PROPAGATE_NEEDS_RESULT,
        text: "A function body used ?^ to propagate errors, but its final expression is not a Result. Wrap the successful final value in @Ok(...), or handle the error instead of propagating it.",
    },
    DiagnosticExplanation {
        code: codes::ty::PROPAGATE_NOT_RESULT,
        text: "The ?^ and ?! operators unwrap Result(ok, err) values. Apply them only to expressions with Result type, or handle the value without a propagation operator.",
    },
    DiagnosticExplanation {
        code: codes::ty::RECORD_INDEX_NOT_COMPTIME,
        text: "Record fields are selected with a comptime-known string. Use a literal or comptime key, or use a Map when the key is chosen at runtime.",
    },
    DiagnosticExplanation {
        code: codes::ty::RENAME_ABSENT_FIELD,
        text: "A row transform tried to rename a missing label, or rename onto a label that already exists in the closed row accumulated so far. Make the source label present and the target label absent before the rename.",
    },
    DiagnosticExplanation {
        code: codes::ty::REPLACE_ABSENT_FIELD,
        text: "A row transform used replacement syntax for a label that is not present in the closed row accumulated so far. Use an add form or spread a closed row containing that label first.",
    },
    DiagnosticExplanation {
        code: codes::ty::SPREAD_SHAPE_UNKNOWN,
        text: "A block spread binding can only open a record whose field names are statically known. Use a static import, a closed record literal or transform, or a binding whose closed record type is already known.",
    },
    DiagnosticExplanation {
        code: codes::ty::TUPLE_INDEX_NOT_COMPTIME,
        text: "A tuple was indexed with a value that is not a compile-time integer. Tuple projection needs a literal index; convert the tuple to an array for runtime indexing, or supply a comptime index.",
    },
    DiagnosticExplanation {
        code: codes::ty::TUPLE_INDEX_OUT_OF_RANGE,
        text: "A tuple index points past the tuple's last element. Use an in-range compile-time index, or convert the tuple to an array for runtime indexing.",
    },
    DiagnosticExplanation {
        code: codes::ty::TYPE_ONLY_RECORD_ENTRY,
        text: "This record entry form is only meaningful in type position. Use it inside an annotation or replace it with a value-level record entry.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNBOUND_METHOD_PARAMETERIZED_OWNER,
        text: "An unbound method value such as `Array.sortBy` was formed from a parameterized type constructor. Unlike scalar owners (`Int`, `Text`) or unparameterized named families, a type constructor has no single concrete method value until it is instantiated. Call the method on a value of a concrete instantiation instead (for example `xs.sortBy(...)` where `xs : Array(...)`).",
    },
    DiagnosticExplanation {
        code: codes::ty::UNEXPECTED_FIELD,
        text: "A closed record value contains a field that is not present in its declared type. Remove the field or open the record type with `..`.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNGUARDED_EMPTY_ACCESS,
        text: "A field was accessed on a value that may be `undefined` or `null` (for example an array element) without `?.`. Use `?.` to propagate the empty, `??` to supply a default, or match the empty before access.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNKNOWN_MODULE_TYPE,
        text: "A module-qualified type name is not one of the explicitly exported type fields of that module.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNKNOWN_NAME,
        text: "A type annotation references an uppercase name that is not a known builtin or in-scope compile-time declaration. Define it or correct the spelling.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNPRODUCTIVE_RECURSION,
        text: "A recursive type has no finite value because every construction must first construct another member of the same recursion. Add a terminating alternative, make the recursive field optional or nullable, put it in a collection, or return it from a function.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNREACHABLE_MATCH_ARM,
        text: "A match arm can never run because its pattern is outside the statically known subject values. Remove the arm or change the subject type to include that value.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNRESOLVED_BINDING,
        text: "A runtime binding reached the end of checking without a concrete inferred type and without another diagnostic explaining why. Add a type annotation, or change the value so inference can resolve it.",
    },
    DiagnosticExplanation {
        code: codes::ty::UNUSED_RESULT,
        text: "A Result value was produced in statement position and then dropped. Unwrap it with `?!` when an `@Err` should panic, propagate it with `?^`, handle it explicitly, or assign it to `_` to document an intentional discard.",
    },
    DiagnosticExplanation {
        code: codes::ty::UPPERCASE_PATTERN_BINDER_UNSUPPORTED,
        text: "Uppercase record-pattern binders extract only explicitly exported types from a static module import.",
    },
    DiagnosticExplanation {
        code: codes::ty::WIDE_VALUE_INTO_LITERAL_UNION,
        text: "A value with a base type such as Text or Int may contain values outside a narrower literal union. Keep the value at the literal-union type or use a fresh literal at the expected-type boundary.",
    },
];

#[cfg(test)]
mod tests {
    use super::{EXPLANATIONS, codes, explain};

    #[test]
    fn looks_up_known_diagnostic_codes() {
        let explanation = explain(codes::parse::UNCLOSED_DELIMITER).expect("expected explanation");

        assert_eq!(explanation.code, codes::parse::UNCLOSED_DELIMITER);
        assert!(explanation.text.contains("opened but not closed"));

        let explanation = explain(codes::ty::OPEN_VARIANT_NOT_ASSIGNABLE)
            .expect("expected open variant explanation");
        assert_eq!(explanation.code, codes::ty::OPEN_VARIANT_NOT_ASSIGNABLE);
        assert!(explanation.text.contains("open row tail"));

        let explanation =
            explain(codes::ty::UNUSED_RESULT).expect("expected unused Result explanation");
        assert_eq!(explanation.code, codes::ty::UNUSED_RESULT);
        assert!(explanation.text.contains("assign it to `_`"));
    }

    #[test]
    fn returns_none_for_unknown_codes() {
        assert!(explain("parse.not-real").is_none());
    }

    #[test]
    fn explanation_codes_are_sorted_and_unique() {
        for pair in EXPLANATIONS.windows(2) {
            assert!(pair[0].code < pair[1].code);
        }
    }

    #[test]
    fn explanation_table_matches_code_registry() {
        let explanation_codes: Vec<_> = EXPLANATIONS
            .iter()
            .map(|explanation| explanation.code)
            .collect();

        assert_eq!(explanation_codes, codes::ALL);
    }
}
