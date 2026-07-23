//! Generative formatter properties: idempotence and parse-preservation.
//!
//! Complements `fixtures.rs` (point goldens) with a seed + parse-safe
//! perturbation strategy over aven-fmt's own `tests/fixtures/valid` corpus.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aven_core::Diagnostic;
use proptest::prelude::*;

const FIXTURE_VALID: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/valid");

// property-test tiering:
// - default is 64 cases for the fast PR gate (`cargo test --workspace`)
// - the scheduled heavy job (`.github/workflows/heavy.yml`) sets
//   `PROPTEST_CASES` high (e.g. 8192); proptest's env override wins over
//   this in-code default for every property that uses this pattern
// - individual slow properties use `#[ignore = "slow: <reason>"]`; the PR
//   gate skips ignored tests, and the heavy job runs them via
//   `cargo test --workspace -- --include-ignored`

/// All `.av` and `.fmt` sources under the formatter valid fixture tree.
fn all_seeds() -> &'static [String] {
    static SEEDS: OnceLock<Vec<String>> = OnceLock::new();
    SEEDS
        .get_or_init(|| load_fixture_sources(&["av", "fmt"]))
        .as_slice()
}

/// Canonical formatter outputs (`.fmt` only) for corpus fixed-point checks.
fn fmt_seeds() -> &'static [(PathBuf, String)] {
    static SEEDS: OnceLock<Vec<(PathBuf, String)>> = OnceLock::new();
    SEEDS
        .get_or_init(|| {
            let dir = Path::new(FIXTURE_VALID);
            let mut seeds = Vec::new();
            for entry in fs::read_dir(dir).expect("aven-fmt valid fixture dir must be readable") {
                let path = entry
                    .expect("fixture directory entries must be readable")
                    .path();
                if path.extension().and_then(|e| e.to_str()) == Some("fmt") {
                    let text = fs::read_to_string(&path)
                        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
                    seeds.push((path, text));
                }
            }
            seeds.sort_by(|a, b| a.0.cmp(&b.0));
            assert!(
                !seeds.is_empty(),
                "expected at least one .fmt seed under {FIXTURE_VALID}"
            );
            seeds
        })
        .as_slice()
}

fn load_fixture_sources(extensions: &[&str]) -> Vec<String> {
    let dir = Path::new(FIXTURE_VALID);
    let mut seeds = Vec::new();
    for entry in fs::read_dir(dir).expect("aven-fmt valid fixture dir must be readable") {
        let path = entry
            .expect("fixture directory entries must be readable")
            .path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !extensions.contains(&ext) {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        seeds.push(text);
    }
    seeds.sort();
    assert!(
        !seeds.is_empty(),
        "expected fixture seeds under {FIXTURE_VALID}"
    );
    seeds
}

fn line_has_content(line: &str) -> bool {
    line.chars().any(|c| !c.is_whitespace())
}

/// Count of leading ASCII spaces (Aven indent unit). Tabs in indent are errors.
fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Top-level content line: has non-whitespace and zero leading indent.
fn is_top_level_content(line: &str) -> bool {
    line_has_content(line) && leading_spaces(line) == 0
}

/// Parse-safe whitespace perturbation of a valid seed.
///
/// Safe edits only:
/// - trailing spaces/tabs on lines that already have non-whitespace content
/// - fully blank lines immediately before top-level (indent-0 content) lines
///
/// Does **not** reindent, split lines, or insert blanks inside indented blocks.
fn apply_perturbation(seed: &str, trail: &[u8], blanks_before: &[u8]) -> String {
    let lines: Vec<&str> = seed.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(seed.len() + 32);
    for (i, line) in lines.iter().enumerate() {
        if is_top_level_content(line) {
            let n = blanks_before.get(i).copied().unwrap_or(0) as usize;
            for _ in 0..n {
                out.push('\n');
            }
        }

        out.push_str(line);

        if line_has_content(line) {
            match trail.get(i).copied().unwrap_or(0) {
                0 => {}
                1 => out.push_str("  "),
                2 => out.push('\t'),
                3 => out.push_str(" \t "),
                _ => out.push_str("    "),
            }
        }

        out.push('\n');
    }
    out
}

/// Shared strategy: pick a fixture seed, then apply randomized parse-safe edits.
fn perturbed_source_strategy() -> impl Strategy<Value = String> {
    let seeds = all_seeds().to_vec();
    prop::sample::select(seeds).prop_flat_map(|seed| {
        let n = seed.lines().count().max(1);
        (
            Just(seed),
            prop::collection::vec(0u8..=4, n),
            prop::collection::vec(0u8..=3, n),
        )
            .prop_map(|(seed, trail, blanks)| apply_perturbation(&seed, &trail, &blanks))
    })
}

/// Every committed `.fmt` seed is already a formatter fixed point.
#[test]
fn corpus_fmt_seeds_are_fixed_points() {
    for (path, source) in fmt_seeds() {
        let formatted = aven_fmt::format_source(source).unwrap_or_else(|diagnostics| {
            panic!(
                ".fmt seed {} failed to format: {diagnostics:?}",
                path.display()
            )
        });
        assert_eq!(
            formatted,
            *source,
            "canonical .fmt seed {} is not a fixed point",
            path.display()
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Formatting a parse-safe perturbation of a valid seed succeeds, and the
    /// formatted document is a fixed point (`format(f) == Ok(f)`).
    #[test]
    fn format_is_idempotent_on_perturbed_seeds(s in perturbed_source_strategy()) {
        let first = aven_fmt::format_source(&s);
        prop_assert!(
            first.is_ok(),
            "parse-safe perturbation of a valid seed failed to format: \
             diagnostics={:?}\n--- source ---\n{}",
            first.as_ref().err(),
            s
        );
        let f = first.expect("just asserted Ok");
        let second = aven_fmt::format_source(&f);
        prop_assert_eq!(
            second,
            Ok(f.clone()),
            "formatter is not idempotent; formatted document:\n{}",
            f
        );
    }

    /// Successful formatting never introduces parse errors.
    #[test]
    fn format_preserves_parse_on_perturbed_seeds(s in perturbed_source_strategy()) {
        let first = aven_fmt::format_source(&s);
        prop_assert!(
            first.is_ok(),
            "parse-safe perturbation of a valid seed failed to format: \
             diagnostics={:?}\n--- source ---\n{}",
            first.as_ref().err(),
            s
        );
        let f = first.expect("just asserted Ok");
        let parse = aven_parser::parse_module(&f);
        prop_assert!(
            !parse.diagnostics.iter().any(Diagnostic::is_error),
            "formatted output has parse errors: {:?}\n--- formatted ---\n{}",
            parse.diagnostics,
            f
        );
    }
}
