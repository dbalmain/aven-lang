//! Generative parser/lexer robustness properties.
//!
//! Encodes "recovery over early abort" (parse_module never panics) and
//! "precise source mapping" (token and diagnostic spans are well-formed)
//! over adversarial input. Complements `fixtures.rs` point goldens.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use aven_parser::ParseOutput;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// property-test tiering:
// - default is 64 cases for the fast PR gate (`cargo test --workspace`)
// - the scheduled heavy job (`.github/workflows/heavy.yml`) sets
//   `PROPTEST_CASES` high (e.g. 8192); proptest's env override wins over
//   this in-code default for every property that uses this pattern
// - individual slow properties use `#[ignore = "slow: <reason>"]`; the PR
//   gate skips ignored tests, and the heavy job runs them via
//   `cargo test --workspace -- --include-ignored`

/// All `.av` sources under the parser fixture tree (recursive).
fn all_fixture_sources() -> &'static [String] {
    static SEEDS: OnceLock<Vec<String>> = OnceLock::new();
    SEEDS
        .get_or_init(|| {
            let root = Path::new(FIXTURE_ROOT);
            let mut seeds = Vec::new();
            load_av_sources_recursively(root, &mut seeds);
            seeds.sort();
            assert!(
                !seeds.is_empty(),
                "expected at least one .av fixture under {FIXTURE_ROOT}"
            );
            seeds
        })
        .as_slice()
}

fn load_av_sources_recursively(dir: &Path, out: &mut Vec<String>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| {
        panic!(
            "parser fixture dir must be readable ({}): {err}",
            dir.display()
        )
    });
    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("fixture directory entry must be readable: {err}"))
            .path();
        if path.is_dir() {
            load_av_sources_recursively(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("av") {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            out.push(text);
        }
    }
}

/// Aven punctuation, keywords, identifiers, numbers, whitespace, UTF-8 hazards.
fn hazard_token_strategy() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "(", ")", "{", "}", "[", "]", ",", ";", "@", "#", "\"", "/", "\\", ":", "=", "->", "true",
        "null", "false", "let", "fn", "match", "type", "foo", "Bar", "x", "_", "0", "1", "42",
        "3.14", "\n", "\r\n", "  ", "    ", "\t", "é", "中", "😀", "'", "`", "${", "}", "...", "?",
        "??", "!.", "//", "/*", "*/",
    ])
}

fn hazard_soup_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(hazard_token_strategy(), 0..40).prop_map(|parts| parts.concat())
}

/// Char-boundary byte offsets including `source.len()`.
fn char_boundaries(source: &str) -> Vec<usize> {
    let mut offsets: Vec<usize> = source.char_indices().map(|(i, _)| i).collect();
    offsets.push(source.len());
    offsets
}

fn truncate_at_char_boundary(seed: &str, index: prop::sample::Index) -> String {
    let boundaries = char_boundaries(seed);
    let k = boundaries[index.index(boundaries.len())];
    seed[..k].to_string()
}

fn delete_random_char(seed: &str, index: prop::sample::Index) -> String {
    if seed.is_empty() {
        return String::new();
    }
    let chars: Vec<(usize, char)> = seed.char_indices().collect();
    let i = index.index(chars.len());
    let (start, ch) = chars[i];
    let end = start + ch.len_utf8();
    format!("{}{}", &seed[..start], &seed[end..])
}

fn duplicate_random_char(seed: &str, index: prop::sample::Index) -> String {
    if seed.is_empty() {
        return String::new();
    }
    let chars: Vec<(usize, char)> = seed.char_indices().collect();
    let i = index.index(chars.len());
    let (start, ch) = chars[i];
    format!("{}{}{}", &seed[..start], ch, &seed[start..])
}

fn fixture_mutation_strategy() -> impl Strategy<Value = String> {
    let seeds = all_fixture_sources().to_vec();
    prop_oneof![
        (
            prop::sample::select(seeds.clone()),
            any::<prop::sample::Index>()
        )
            .prop_map(|(s, idx)| truncate_at_char_boundary(&s, idx)),
        (
            prop::sample::select(seeds.clone()),
            any::<prop::sample::Index>()
        )
            .prop_map(|(s, idx)| delete_random_char(&s, idx)),
        (
            prop::sample::select(seeds.clone()),
            any::<prop::sample::Index>()
        )
            .prop_map(|(s, idx)| duplicate_random_char(&s, idx)),
        (
            prop::sample::select(seeds.clone()),
            prop::sample::select(seeds)
        )
            .prop_map(|(a, b)| format!("{a}{b}")),
    ]
}

/// Shared adversarial strategy: arbitrary Unicode, hazard soup, fixture mutations.
fn adversarial_source_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        hazard_soup_strategy(),
        fixture_mutation_strategy(),
    ]
}

/// Assert token and diagnostic span invariants for one parse.
///
/// Token invariants (both streams where noted):
/// - raw_tokens: ordered and non-overlapping (`a.end <= b.start`)
/// - raw_tokens and layout_tokens: each span is in-bounds and on char boundaries
///
/// Diagnostic invariants:
/// - every diagnostic has at least one label
/// - every label span is in-bounds
/// - every label span is on char boundaries (flagged separately in messages)
fn assert_spans_wellformed(source: &str, parse: &ParseOutput) -> Result<(), TestCaseError> {
    let len = source.len();

    // Ordered & non-overlapping over consecutive raw tokens.
    for pair in parse.raw_tokens.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        prop_assert!(
            a.span.start <= a.span.end,
            "raw token span inverted: start={} end={} in {:?}",
            a.span.start,
            a.span.end,
            source
        );
        prop_assert!(
            a.span.end <= b.span.start,
            "raw tokens overlap or out of order: {:?} then {:?} in {:?}",
            a.span,
            b.span,
            source
        );
    }

    for (stream_name, tokens) in [
        ("raw_tokens", parse.raw_tokens.as_slice()),
        ("layout_tokens", parse.layout_tokens.as_slice()),
    ] {
        for token in tokens {
            let span = token.span;
            prop_assert!(
                span.start <= span.end && span.end <= len,
                "{stream_name} span out of bounds: start={} end={} len={len} in {:?}",
                span.start,
                span.end,
                source
            );
            prop_assert!(
                source.is_char_boundary(span.start),
                "{stream_name} span.start not on char boundary: {} in {:?}",
                span.start,
                source
            );
            prop_assert!(
                source.is_char_boundary(span.end),
                "{stream_name} span.end not on char boundary: {} in {:?}",
                span.end,
                source
            );
        }
    }

    for (diag_i, diag) in parse.diagnostics.iter().enumerate() {
        prop_assert!(
            !diag.labels.is_empty(),
            "diagnostic {diag_i} has no labels (message={:?}) in {:?}",
            diag.message,
            source
        );
        for (label_i, label) in diag.labels.iter().enumerate() {
            let span = label.span;
            // In-bounds first so a char-boundary-only failure is isolatable.
            prop_assert!(
                span.start <= span.end && span.end <= len,
                "diagnostic {diag_i} label {label_i} span out of bounds: \
                 start={} end={} len={len} in {:?}",
                span.start,
                span.end,
                source
            );
            prop_assert!(
                source.is_char_boundary(span.start),
                "diagnostic {diag_i} label {label_i} span.start not on char boundary: \
                 {} in {:?}",
                span.start,
                source
            );
            prop_assert!(
                source.is_char_boundary(span.end),
                "diagnostic {diag_i} label {label_i} span.end not on char boundary: \
                 {} in {:?}",
                span.end,
                source
            );
        }
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// `parse_module` returns for all inputs (panic → proptest failure) and
    /// every emitted token/diagnostic span is well-formed.
    #[test]
    fn parse_module_never_panics_and_spans_wellformed(s in adversarial_source_strategy()) {
        let parse = aven_parser::parse_module(&s);
        assert_spans_wellformed(&s, &parse)?;
    }
}
