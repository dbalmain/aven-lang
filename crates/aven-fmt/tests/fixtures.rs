use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const FORMATTER_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

#[test]
fn valid_formatter_fixtures_match_expected_output() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(FORMATTER_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let actual = aven_fmt::format_source(&source).map_err(|diagnostics| {
            format!("{} produced diagnostics: {diagnostics:?}", path.display())
        })?;
        let expected_path = path.with_extension("fmt");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "formatted output for {} did not match {}",
            path.display(),
            expected_path.display()
        );
        assert_eq!(
            aven_fmt::format_source(&actual),
            Ok(actual),
            "formatter fixture {} is not idempotent",
            path.display()
        );
    }

    Ok(())
}

fn fixture_files(root: &str, group: &str) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(Path::new(root).join(group))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("av") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}
