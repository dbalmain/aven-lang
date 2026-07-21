use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml::Spanned;

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::{
    OperatorAssociativity, OperatorFixity, OperatorFixityTable, OperatorFixityTableError,
    OperatorOrigin, OperatorPrecedence, is_custom_operator_token, is_reserved_or_fixed_operator,
};

pub type OperatorConfigResult<T> = Result<T, Vec<Diagnostic>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorFixityDeclaration {
    token: String,
    fixity: OperatorFixity,
}

impl OperatorFixityDeclaration {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub const fn fixity(&self) -> &OperatorFixity {
        &self.fixity
    }

    pub fn into_parts(self) -> (String, OperatorFixity) {
        (self.token, self.fixity)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OperatorManifestSource<'a> {
    pub path: &'a Path,
    pub source: &'a str,
}

/// Parse and validate an optional manifest, the entry shebang, argv atoms, and
/// platform declarations as one disjoint operator-fixity table.
pub fn load_operator_fixity_table<A, S, P, T>(
    manifest: Option<OperatorManifestSource<'_>>,
    entry_source: &str,
    argv_atoms: A,
    platform: P,
) -> OperatorConfigResult<OperatorFixityTable>
where
    A: IntoIterator<Item = S>,
    S: AsRef<str>,
    P: IntoIterator<Item = (T, OperatorPrecedence, OperatorAssociativity)>,
    T: Into<String>,
{
    let mut declarations = Vec::new();
    let mut diagnostics = Vec::new();

    if let Some(manifest) = manifest {
        collect_result(
            parse_manifest_operator_fixities(manifest.source, manifest.path),
            &mut declarations,
            &mut diagnostics,
        );
    }
    collect_result(
        parse_shebang_operator_fixities(entry_source),
        &mut declarations,
        &mut diagnostics,
    );
    collect_result(
        parse_argv_operator_fixities(argv_atoms),
        &mut declarations,
        &mut diagnostics,
    );

    match merge_operator_fixities(declarations, platform) {
        Ok(table) if diagnostics.is_empty() => Ok(table),
        Ok(_) => Err(diagnostics),
        Err(mut merge_diagnostics) => {
            diagnostics.append(&mut merge_diagnostics);
            Err(diagnostics)
        }
    }
}

/// Parse the `[operators]` contribution from an `Aven.toml` string.
pub fn parse_manifest_operator_fixities(
    source: &str,
    manifest_path: impl Into<PathBuf>,
) -> OperatorConfigResult<Vec<OperatorFixityDeclaration>> {
    let manifest_path = manifest_path.into();
    let raw = toml::from_str::<RawManifest>(source)
        .map_err(|error| vec![invalid_manifest_toml(source, &manifest_path, &error)])?;
    let mut declarations = Vec::new();
    let mut diagnostics = Vec::new();

    for (token, definition) in raw.operators {
        let token_span = range_to_span(token.span(), source.len());
        let token = token.into_inner();
        let definition = definition.into_inner();
        let origin = OperatorOrigin::Manifest {
            path: manifest_path.clone(),
            span: token_span,
        };

        let quoted = source
            .get(token_span.start..token_span.end)
            .is_some_and(|text| matches!(text.as_bytes().first(), Some(b'"') | Some(b'\'')));
        if !quoted {
            diagnostics.push(
                Diagnostic::error(format!(
                    "operator token `{token}` must be a quoted TOML key"
                ))
                .with_code(codes::config::OPERATOR_MANIFEST_INVALID)
                .with_label(Label::primary(token_span, "quote this operator token"))
                .with_note(format!("write `\"{token}\" = {{ ... }}`")),
            );
            continue;
        }
        if let Some(diagnostic) = invalid_token_diagnostic(&token, &origin, token_span) {
            diagnostics.push(diagnostic);
            continue;
        }

        let precedence_span = range_to_span(definition.precedence.span(), source.len());
        let precedence = OperatorPrecedence::from_anchor(definition.precedence.get_ref());
        if precedence.is_none() {
            diagnostics.push(
                Diagnostic::error(format!(
                    "unknown operator precedence anchor `{}`",
                    definition.precedence.get_ref()
                ))
                .with_code(codes::config::OPERATOR_MANIFEST_INVALID)
                .with_label(Label::primary(
                    precedence_span,
                    "expected one of `|`, `|>`, `??`, `||`, `&&`, `==`, `+`, `*`, or `^`",
                ))
                .with_note("choose the existing operator whose precedence should be shared"),
            );
        }

        let associativity_span = range_to_span(definition.associativity.span(), source.len());
        let associativity = OperatorAssociativity::from_name(definition.associativity.get_ref());
        if associativity.is_none() {
            diagnostics.push(
                Diagnostic::error(format!(
                    "unknown operator associativity `{}`",
                    definition.associativity.get_ref()
                ))
                .with_code(codes::config::OPERATOR_MANIFEST_INVALID)
                .with_label(Label::primary(
                    associativity_span,
                    "expected `left`, `right`, or `none`",
                ))
                .with_note("use lowercase associativity in `Aven.toml`"),
            );
        }

        if let (Some(precedence), Some(associativity)) = (precedence, associativity) {
            declarations.push(OperatorFixityDeclaration {
                token,
                fixity: OperatorFixity::new(precedence, associativity, origin),
            });
        }
    }

    if diagnostics.is_empty() {
        Ok(declarations)
    } else {
        Err(diagnostics)
    }
}

/// Parse the entry source's first line when it is an Aven shebang.
pub fn parse_shebang_operator_fixities(
    source: &str,
) -> OperatorConfigResult<Vec<OperatorFixityDeclaration>> {
    let line = first_line(source);
    if !line.starts_with("#!") {
        return Ok(Vec::new());
    }

    let Some(words) = shebang_words(line) else {
        return Err(vec![malformed_shebang(
            Span::new(0, line.len()),
            "the shebang must use spaces between unquoted arguments",
        )]);
    };
    let flag_start = if is_env_s_form(&words) {
        4
    } else if is_direct_form(&words) {
        2
    } else {
        return Err(vec![malformed_shebang(
            Span::new(0, line.len()),
            "expected `/usr/bin/env -S aven run` or an absolute path ending in `/aven` followed by `run`",
        )]);
    };

    let mut declarations = Vec::new();
    let mut diagnostics = Vec::new();
    for word in &words[flag_start..] {
        let origin = OperatorOrigin::Shebang { span: word.span };
        match parse_operator_flag(word.text, word.span, origin, FlagContext::Shebang) {
            Ok(declaration) => declarations.push(declaration),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    if diagnostics.is_empty() {
        Ok(declarations)
    } else {
        Err(diagnostics)
    }
}

/// Parse repeatable full `--operator=TOKEN:ANCHOR:ASSOC` argv atoms.
pub fn parse_argv_operator_fixities<A, S>(
    atoms: A,
) -> OperatorConfigResult<Vec<OperatorFixityDeclaration>>
where
    A: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut declarations = Vec::new();
    let mut diagnostics = Vec::new();

    for (declaration_index, atom) in atoms.into_iter().enumerate() {
        let atom = atom.as_ref();
        let span = Span::new(0, atom.len());
        let origin = OperatorOrigin::Argv {
            declaration_index,
            span,
        };
        match parse_operator_flag(atom, span, origin, FlagContext::Argv) {
            Ok(declaration) => declarations.push(declaration),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    if diagnostics.is_empty() {
        Ok(declarations)
    } else {
        Err(diagnostics)
    }
}

/// Disjointly merge root declarations with a platform-supplied set.
pub fn merge_operator_fixities<R, P, T>(
    root: R,
    platform: P,
) -> OperatorConfigResult<OperatorFixityTable>
where
    R: IntoIterator<Item = OperatorFixityDeclaration>,
    P: IntoIterator<Item = (T, OperatorPrecedence, OperatorAssociativity)>,
    T: Into<String>,
{
    let mut entries = root
        .into_iter()
        .map(OperatorFixityDeclaration::into_parts)
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();

    for (registration_index, (token, precedence, associativity)) in platform.into_iter().enumerate()
    {
        let token = token.into();
        let origin = OperatorOrigin::Platform { registration_index };
        if let Some(diagnostic) = invalid_token_diagnostic(&token, &origin, Span::point(0)) {
            diagnostics.push(diagnostic);
        } else {
            entries.push((
                token,
                OperatorFixity::new(precedence, associativity, origin),
            ));
        }
    }

    match OperatorFixityTable::try_from_entries(entries) {
        Ok(table) if diagnostics.is_empty() => Ok(table),
        Ok(_) => Err(diagnostics),
        Err(error) => {
            diagnostics.push(table_error_diagnostic(error));
            Err(diagnostics)
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(default)]
    operators: BTreeMap<Spanned<String>, Spanned<RawOperatorFixity>>,
    #[serde(flatten)]
    _other: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOperatorFixity {
    precedence: Spanned<String>,
    associativity: Spanned<String>,
}

#[derive(Debug, Clone, Copy)]
struct ShebangWord<'a> {
    text: &'a str,
    span: Span,
}

#[derive(Debug, Clone, Copy)]
enum FlagContext {
    Shebang,
    Argv,
}

impl FlagContext {
    const fn code(self) -> &'static str {
        match self {
            Self::Shebang => codes::config::OPERATOR_SHEBANG_MALFORMED,
            Self::Argv => codes::config::OPERATOR_ARGUMENT_MALFORMED,
        }
    }

    const fn source_name(self) -> &'static str {
        match self {
            Self::Shebang => "entry shebang",
            Self::Argv => "command line",
        }
    }
}

fn parse_operator_flag(
    flag: &str,
    span: Span,
    origin: OperatorOrigin,
    context: FlagContext,
) -> Result<OperatorFixityDeclaration, Diagnostic> {
    const PREFIX: &str = "--operator=";

    let Some(value) = flag.strip_prefix(PREFIX) else {
        return Err(malformed_flag(
            context,
            span,
            "only `--operator=...` flags are allowed here",
        ));
    };
    let mut parts = value.split(':');
    let (Some(token), Some(anchor), Some(associativity), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed_flag(
            context,
            span,
            "expected exactly `TOKEN:ANCHOR:ASSOCIATIVITY`",
        ));
    };

    let token_start = span.start + PREFIX.len();
    let token_span = Span::new(token_start, token_start + token.len());
    if let Some(diagnostic) = invalid_token_diagnostic(token, &origin, token_span) {
        return Err(diagnostic);
    }

    let anchor_start = token_span.end + 1;
    let anchor_span = Span::new(anchor_start, anchor_start + anchor.len());
    let Some(precedence) = OperatorPrecedence::from_anchor(anchor) else {
        return Err(
            Diagnostic::error(format!("unknown operator precedence anchor `{anchor}`"))
                .with_code(context.code())
                .with_label(Label::primary(
                    anchor_span,
                    "expected `|`, `|>`, `??`, `||`, `&&`, `==`, `+`, `*`, or `^`",
                ))
                .with_note("choose the existing operator whose precedence should be shared"),
        );
    };

    let associativity_start = anchor_span.end + 1;
    let associativity_span = Span::new(
        associativity_start,
        associativity_start + associativity.len(),
    );
    let Some(associativity) = OperatorAssociativity::from_name(associativity) else {
        return Err(
            Diagnostic::error(format!("unknown operator associativity `{associativity}`"))
                .with_code(context.code())
                .with_label(Label::primary(
                    associativity_span,
                    "expected `left`, `right`, or `none`",
                ))
                .with_note("associativity names are lowercase"),
        );
    };

    Ok(OperatorFixityDeclaration {
        token: token.to_owned(),
        fixity: OperatorFixity::new(precedence, associativity, origin),
    })
}

fn first_line(source: &str) -> &str {
    let line = source.split_once('\n').map_or(source, |(line, _)| line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn shebang_words(line: &str) -> Option<Vec<ShebangWord<'_>>> {
    let body = line.strip_prefix("#!")?;
    let bytes = body.as_bytes();
    if bytes.is_empty()
        || bytes.first() == Some(&b' ')
        || bytes.last() == Some(&b' ')
        || bytes
            .iter()
            .any(|byte| byte.is_ascii_whitespace() && *byte != b' ')
    {
        return None;
    }

    let mut words = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor] != b' ' {
            cursor += 1;
        }
        words.push(ShebangWord {
            text: &body[start..cursor],
            span: Span::new(start + 2, cursor + 2),
        });
        while cursor < bytes.len() && bytes[cursor] == b' ' {
            cursor += 1;
        }
    }
    Some(words)
}

fn is_env_s_form(words: &[ShebangWord<'_>]) -> bool {
    words.len() >= 4
        && is_absolute_program(words[0].text, "env")
        && words[1].text == "-S"
        && words[2].text == "aven"
        && words[3].text == "run"
}

fn is_direct_form(words: &[ShebangWord<'_>]) -> bool {
    words.len() >= 2 && is_absolute_program(words[0].text, "aven") && words[1].text == "run"
}

fn is_absolute_program(path: &str, expected_name: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && path.file_name().and_then(|name| name.to_str()) == Some(expected_name)
}

fn invalid_manifest_toml(
    source: &str,
    manifest_path: &Path,
    error: &toml::de::Error,
) -> Diagnostic {
    let span = error.span().map_or(Span::point(source.len()), |range| {
        range_to_span(range, source.len())
    });

    Diagnostic::error(format!(
        "invalid `[operators]` configuration in `{}`",
        manifest_path.display()
    ))
    .with_code(codes::config::OPERATOR_MANIFEST_INVALID)
    .with_label(Label::primary(span, error.message()))
    .with_note("each operator value needs exactly `precedence` and `associativity` string fields")
    .with_note("remove unknown or duplicate fields and add any missing field")
}

fn invalid_token_diagnostic(
    token: &str,
    origin: &OperatorOrigin,
    span: Span,
) -> Option<Diagnostic> {
    if is_custom_operator_token(token) {
        return None;
    }

    let (message, code, label, repair) = if is_reserved_or_fixed_operator(token) {
        (
            format!("cannot register fixity for reserved operator `{token}`"),
            codes::config::OPERATOR_TOKEN_RESERVED,
            "this token already has language-defined syntax or fixity",
            "remove this declaration or choose a new custom token",
        )
    } else {
        (
            format!("invalid custom operator token `{token}`"),
            codes::config::OPERATOR_TOKEN_INVALID,
            "this is not a legal custom operator token",
            "use a non-empty ASCII operator run that does not start with `=` and contains no reserved syntax characters",
        )
    };
    let mut diagnostic = Diagnostic::error(message)
        .with_code(code)
        .with_note(format!("declaration came from {origin}"))
        .with_note(repair);
    if !matches!(origin, OperatorOrigin::Platform { .. }) {
        diagnostic = diagnostic.with_label(Label::primary(span, label));
    }
    Some(diagnostic)
}

fn malformed_shebang(span: Span, label: &str) -> Diagnostic {
    Diagnostic::error("malformed Aven shebang")
        .with_code(codes::config::OPERATOR_SHEBANG_MALFORMED)
        .with_label(Label::primary(span, label))
        .with_note("use `#!/usr/bin/env -S aven run --operator=**:^:right`")
        .with_note("or use an absolute direct interpreter path ending in `/aven`")
}

fn malformed_flag(context: FlagContext, span: Span, label: &str) -> Diagnostic {
    Diagnostic::error(format!(
        "malformed operator declaration in the {}",
        context.source_name()
    ))
    .with_code(context.code())
    .with_label(Label::primary(span, label))
    .with_note("write `--operator=TOKEN:ANCHOR:ASSOCIATIVITY`, for example `--operator=**:^:right`")
}

fn table_error_diagnostic(error: OperatorFixityTableError) -> Diagnostic {
    match error {
        OperatorFixityTableError::InvalidToken { token, fixity } => {
            invalid_token_diagnostic(&token, fixity.origin(), Span::point(0)).unwrap_or_else(|| {
                Diagnostic::error(format!("invalid custom operator token `{token}`"))
                    .with_code(codes::config::OPERATOR_TOKEN_INVALID)
            })
        }
        OperatorFixityTableError::Duplicate {
            token,
            first,
            second,
        } => Diagnostic::error(format!(
            "operator fixity for `{token}` has multiple origins"
        ))
        .with_code(codes::config::OPERATOR_FIXITY_CONFLICT)
        .with_note(format!("first: {first} from {}", first.origin()))
        .with_note(format!("second: {second} from {}", second.origin()))
        .with_note(
            "remove all but one declaration; configuration sources never override each other",
        ),
    }
}

fn range_to_span(range: std::ops::Range<usize>, source_len: usize) -> Span {
    let start = range.start.min(source_len);
    Span::new(start, range.end.min(source_len).max(start))
}

fn collect_result<T>(
    result: OperatorConfigResult<Vec<T>>,
    values: &mut Vec<T>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match result {
        Ok(mut parsed) => values.append(&mut parsed),
        Err(mut errors) => diagnostics.append(&mut errors),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use aven_core::codes;
    use aven_parser::{OperatorAssociativity, OperatorOrigin, OperatorPrecedence};

    use super::{
        OperatorFixityDeclaration, OperatorManifestSource, load_operator_fixity_table,
        merge_operator_fixities, parse_argv_operator_fixities, parse_manifest_operator_fixities,
        parse_shebang_operator_fixities,
    };

    #[test]
    fn manifest_loader_parses_operator_table() {
        let source = r#"
name = "example"

[operators]
"**" = { precedence = "^", associativity = "right" }
"$$" = { precedence = "*", associativity = "left" }
"#;
        let declarations = parse_manifest_operator_fixities(source, "/work/Aven.toml")
            .expect("valid operator manifest should parse");

        assert_fixity(
            &declarations[0],
            "$$",
            OperatorPrecedence::Multiplicative,
            OperatorAssociativity::Left,
        );
        assert_fixity(
            &declarations[1],
            "**",
            OperatorPrecedence::Exponentiation,
            OperatorAssociativity::Right,
        );
        assert!(matches!(
            declarations[1].fixity().origin(),
            OperatorOrigin::Manifest { path, span }
                if path == Path::new("/work/Aven.toml") && !span.is_empty()
        ));
    }

    #[test]
    fn manifest_loader_rejects_unknown_missing_and_duplicate_fields() {
        for source in [
            "[operators]\n\"**\" = { precedence = \"^\", associativity = \"right\", priority = 1 }\n",
            "[operators]\n\"**\" = { precedence = \"^\" }\n",
            "[operators]\n\"**\" = { precedence = \"^\", precedence = \"*\", associativity = \"right\" }\n",
        ] {
            assert_error_code(
                parse_manifest_operator_fixities(source, "Aven.toml"),
                codes::config::OPERATOR_MANIFEST_INVALID,
            );
        }
    }

    #[test]
    fn manifest_loader_rejects_unquoted_keys_and_invalid_values() {
        assert_error_code(
            parse_manifest_operator_fixities(
                "[operators]\n-- = { precedence = \"^\", associativity = \"right\" }\n",
                "Aven.toml",
            ),
            codes::config::OPERATOR_MANIFEST_INVALID,
        );
        assert_error_code(
            parse_manifest_operator_fixities(
                "[operators]\n\"**\" = { precedence = \"tight\", associativity = \"right\" }\n",
                "Aven.toml",
            ),
            codes::config::OPERATOR_MANIFEST_INVALID,
        );
        assert_error_code(
            parse_manifest_operator_fixities(
                "[operators]\n\"**\" = { precedence = \"^\", associativity = \"infixr\" }\n",
                "Aven.toml",
            ),
            codes::config::OPERATOR_MANIFEST_INVALID,
        );
    }

    #[test]
    fn shebang_loader_accepts_portable_env_s_form() {
        let declarations = parse_shebang_operator_fixities(
            "#!/usr/bin/env -S aven run --operator=**:^:right --operator=$$:*:left\nanswer = 42\n",
        )
        .expect("portable Aven shebang should parse");

        assert_fixity(
            &declarations[0],
            "**",
            OperatorPrecedence::Exponentiation,
            OperatorAssociativity::Right,
        );
        assert_fixity(
            &declarations[1],
            "$$",
            OperatorPrecedence::Multiplicative,
            OperatorAssociativity::Left,
        );
    }

    #[test]
    fn shebang_loader_accepts_direct_absolute_form() {
        let declarations = parse_shebang_operator_fixities(
            "#!/opt/aven/bin/aven run --operator=**:^:none\r\nanswer = 42\r\n",
        )
        .expect("direct Aven shebang should parse");

        assert_fixity(
            &declarations[0],
            "**",
            OperatorPrecedence::Exponentiation,
            OperatorAssociativity::None,
        );
    }

    #[test]
    fn shebang_loader_ignores_a_non_shebang_first_line() {
        let declarations = parse_shebang_operator_fixities(
            "answer = 42\n#!/usr/bin/env -S aven run --operator=**:^:right\n",
        )
        .expect("ordinary source has no shebang contribution");

        assert!(declarations.is_empty());
    }

    #[test]
    fn shebang_loader_rejects_other_interpreters_flags_and_tabs() {
        for source in [
            "#!/usr/bin/env aven run --operator=**:^:right\n",
            "#!/usr/bin/env -S aven run --quiet\n",
            "#!/usr/bin/env -S aven\trun --operator=**:^:right\n",
        ] {
            assert_error_code(
                parse_shebang_operator_fixities(source),
                codes::config::OPERATOR_SHEBANG_MALFORMED,
            );
        }
    }

    #[test]
    fn argv_loader_parses_repeated_atoms() {
        let declarations =
            parse_argv_operator_fixities(["--operator=**:^:right", "--operator=$$:*:left"])
                .expect("valid argv atoms should parse");

        assert_eq!(declarations.len(), 2);
        assert!(matches!(
            declarations[1].fixity().origin(),
            OperatorOrigin::Argv {
                declaration_index: 1,
                ..
            }
        ));
    }

    #[test]
    fn argv_loader_rejects_malformed_atoms_and_values() {
        for atom in [
            "**:^:right",
            "--operator=**:^",
            "--operator=**:tight:right",
            "--operator=**:^:sideways",
        ] {
            assert_error_code(
                parse_argv_operator_fixities([atom]),
                codes::config::OPERATOR_ARGUMENT_MALFORMED,
            );
        }
    }

    #[test]
    fn loaders_distinguish_invalid_and_reserved_tokens() {
        assert_error_code(
            parse_argv_operator_fixities(["--operator=word:^:right"]),
            codes::config::OPERATOR_TOKEN_INVALID,
        );
        assert_error_code(
            parse_argv_operator_fixities(["--operator=+:^:right"]),
            codes::config::OPERATOR_TOKEN_RESERVED,
        );
        assert_error_code(
            parse_manifest_operator_fixities(
                "[operators]\n\"==\" = { precedence = \"==\", associativity = \"left\" }\n",
                "Aven.toml",
            ),
            codes::config::OPERATOR_TOKEN_RESERVED,
        );
        assert_error_code(
            merge_operator_fixities(
                Vec::new(),
                [(
                    "+",
                    OperatorPrecedence::Additive,
                    OperatorAssociativity::Left,
                )],
            ),
            codes::config::OPERATOR_TOKEN_RESERVED,
        );
    }

    #[test]
    fn merge_rejects_cross_origin_conflicts_even_when_identical() {
        let manifest = parse_manifest_operator_fixities(
            "[operators]\n\"**\" = { precedence = \"^\", associativity = \"right\" }\n",
            "/project/Aven.toml",
        )
        .expect("test manifest is valid");

        let diagnostics = merge_operator_fixities(
            manifest,
            [(
                "**",
                OperatorPrecedence::Exponentiation,
                OperatorAssociativity::Right,
            )],
        )
        .expect_err("identical declarations from two origins must conflict");

        assert_eq!(
            diagnostics[0].code.as_deref(),
            Some(codes::config::OPERATOR_FIXITY_CONFLICT)
        );
        assert!(
            diagnostics[0]
                .notes
                .iter()
                .any(|note| note.contains("manifest"))
        );
        assert!(
            diagnostics[0]
                .notes
                .iter()
                .any(|note| note.contains("platform"))
        );
        assert!(
            diagnostics[0]
                .notes
                .iter()
                .filter(|note| note.contains("precedence `^`, associativity `right`"))
                .count()
                == 2
        );
    }

    #[test]
    fn merge_reports_both_different_fixities_and_source_origins() {
        let shebang =
            parse_shebang_operator_fixities("#!/usr/bin/env -S aven run --operator=**:^:right\n")
                .expect("test shebang is valid");
        let argv = parse_argv_operator_fixities(["--operator=**:*:left"])
            .expect("test argv declaration is valid");

        let diagnostics = merge_operator_fixities(
            shebang.into_iter().chain(argv),
            std::iter::empty::<(&str, OperatorPrecedence, OperatorAssociativity)>(),
        )
        .expect_err("different declarations from two origins must conflict");
        let notes = diagnostics[0].notes.join("\n");

        assert_eq!(
            diagnostics[0].code.as_deref(),
            Some(codes::config::OPERATOR_FIXITY_CONFLICT)
        );
        assert!(notes.contains("first-line shebang"));
        assert!(notes.contains("command-line operator declaration"));
        assert!(notes.contains("precedence `^`, associativity `right`"));
        assert!(notes.contains("precedence `*`, associativity `left`"));
    }

    #[test]
    fn merge_rejects_repeated_argv_declarations() {
        let argv = parse_argv_operator_fixities(["--operator=**:^:right", "--operator=**:^:right"])
            .expect("both argv atoms are individually valid");

        assert_error_code(
            merge_operator_fixities(
                argv,
                std::iter::empty::<(&str, OperatorPrecedence, OperatorAssociativity)>(),
            ),
            codes::config::OPERATOR_FIXITY_CONFLICT,
        );
    }

    #[test]
    fn load_combines_disjoint_sources_into_one_table() {
        let manifest_source =
            "[operators]\n\"**\" = { precedence = \"^\", associativity = \"right\" }\n";
        let manifest = OperatorManifestSource {
            path: Path::new("/project/Aven.toml"),
            source: manifest_source,
        };
        let table = load_operator_fixity_table(
            Some(manifest),
            "#!/usr/bin/env -S aven run --operator=$$:*:left\nanswer = 42\n",
            ["--operator=!!:==:none"],
            [("&~", OperatorPrecedence::And, OperatorAssociativity::Left)],
        )
        .expect("disjoint configuration sources should merge");

        assert_eq!(table.len(), 4);
        assert_eq!(
            table.get("!!").map(|fixity| fixity.associativity()),
            Some(OperatorAssociativity::None)
        );
    }

    fn assert_fixity(
        declaration: &OperatorFixityDeclaration,
        token: &str,
        precedence: OperatorPrecedence,
        associativity: OperatorAssociativity,
    ) {
        assert_eq!(declaration.token(), token);
        assert_eq!(declaration.fixity().precedence(), precedence);
        assert_eq!(declaration.fixity().associativity(), associativity);
    }

    fn assert_error_code<T>(result: Result<T, Vec<aven_core::Diagnostic>>, code: &str) {
        let diagnostics = match result {
            Ok(_) => panic!("test input should be rejected"),
            Err(diagnostics) => diagnostics,
        };
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some(code)),
            "expected diagnostic code {code}, got {diagnostics:#?}"
        );
    }
}
