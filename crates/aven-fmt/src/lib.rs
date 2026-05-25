pub fn format_source(source: &str) -> String {
    let mut output = String::with_capacity(source.len() + 1);

    for line in source.lines() {
        output.push_str(line.trim_end());
        output.push('\n');
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_whitespace_and_adds_final_newline() {
        assert_eq!(format_source("x = 1   \n\n"), "x = 1\n\n");
        assert_eq!(format_source("x = 1"), "x = 1\n");
    }

    #[test]
    fn preserves_existing_leading_indentation() {
        assert_eq!(
            format_source("x =\n  y = 1   \n    z = 2\t\n"),
            "x =\n  y = 1\n    z = 2\n"
        );
    }
}
