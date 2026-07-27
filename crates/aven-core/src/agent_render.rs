//! Compact `--format agent` diagnostic rendering for machine readers (LLMs).
//!
//! Quotes spans instead of pointing with carets/box-drawing. ASCII-only
//! structural glyphs. Does not depend on ariadne.

use crate::{Diagnostic, Label, LineIndex, Severity, SourceFile, Span};

/// Soft max for `in:` source line content before truncation with `...`.
const MAX_IN_LINE_CHARS: usize = 100;

/// Shorten `path` for display by stripping a leading `base` directory.
///
/// The whole point of this format is economy for a machine reader, and an
/// absolute path repeated once per diagnostic is the single largest avoidable
/// cost in it: on a two-diagnostic file it was 26% of the output, enough to eat
/// most of the saving over the caret rendering.
///
/// Only a **prefix** is stripped, never resolved into `../..` segments. A path
/// outside `base` is returned unchanged, because a chain of parent hops is both
/// longer than the absolute path and harder to act on. Purely textual so it
/// stays testable — the caller supplies `base`, since a working directory is an
/// environment fact and this crate does not read the environment.
pub fn relative_display_path<'a>(path: &'a str, base: Option<&str>) -> &'a str {
    let Some(base) = base else { return path };
    let base = base.strip_suffix('/').unwrap_or(base);
    if base.is_empty() {
        return path;
    }
    path.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
}

/// Render one diagnostic in `--format agent` style.
pub fn render_agent_diagnostic(
    diagnostic: &Diagnostic,
    path: &str,
    source: &str,
    line_index: &LineIndex,
) -> String {
    let mut out = String::new();
    write_header(&mut out, diagnostic, path, source, line_index);

    let primary_line = diagnostic.labels.first().map(|label| {
        let (start, _) = clamp_span(source, label.span);
        line_index.line_index_for_offset(start.min(source.len()))
    });

    for (index, label) in diagnostic.labels.iter().enumerate() {
        let is_primary = index == 0;
        write_label(
            &mut out,
            label,
            path,
            source,
            line_index,
            is_primary,
            primary_line,
        );
    }

    for note in &diagnostic.notes {
        out.push_str("  help: ");
        out.push_str(note);
        out.push('\n');
    }

    // Trim the trailing newline so callers control joining.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Render every diagnostic for one file, joined with newlines.
///
/// `base` shortens the displayed path — pass the working directory. See
/// [`relative_display_path`] for why this is worth doing and why it only strips
/// a prefix.
pub fn render_agent_report(
    file: &SourceFile,
    diagnostics: &[Diagnostic],
    base: Option<&str>,
) -> String {
    let path = relative_display_path(&file.name, base);
    diagnostics
        .iter()
        .map(|diagnostic| {
            render_agent_diagnostic(diagnostic, path, file.source(), file.line_index())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_header(
    out: &mut String,
    diagnostic: &Diagnostic,
    path: &str,
    source: &str,
    line_index: &LineIndex,
) {
    out.push_str(severity_word(diagnostic.severity));
    if let Some(code) = &diagnostic.code {
        out.push('[');
        out.push_str(code);
        out.push(']');
    }
    out.push(' ');

    let (line, col) = primary_location(diagnostic, source, line_index);
    // path:line:col: message
    out.push_str(path);
    out.push(':');
    out.push_str(&line.to_string());
    out.push(':');
    out.push_str(&col.to_string());
    out.push_str(": ");
    out.push_str(&diagnostic.message);
    out.push('\n');
}

fn primary_location(diagnostic: &Diagnostic, source: &str, line_index: &LineIndex) -> (u32, u32) {
    let offset = diagnostic
        .labels
        .first()
        .map(|label| clamp_span(source, label.span).0)
        .unwrap_or_else(|| source.len());
    let pos = line_index.offset_to_position(source, offset);
    (pos.line.saturating_add(1), pos.character.saturating_add(1))
}

fn write_label(
    out: &mut String,
    label: &Label,
    path: &str,
    source: &str,
    line_index: &LineIndex,
    is_primary: bool,
    primary_line: Option<usize>,
) {
    let (start, end) = clamp_span(source, label.span);
    let line_idx = line_index.line_index_for_offset(start);
    let same_line_as_primary = primary_line == Some(line_idx);

    if is_primary || !same_line_as_primary {
        write_in_line(out, path, source, line_index, start, end, is_primary);
    }

    write_at_line(out, label, source, start, end);
}

fn write_in_line(
    out: &mut String,
    path: &str,
    source: &str,
    line_index: &LineIndex,
    start: usize,
    end: usize,
    is_primary: bool,
) {
    // `  in:   ` — two spaces, `in:`, three spaces (aligns with `help:` content).
    out.push_str("  in:   ");
    if !is_primary {
        let pos = line_index.offset_to_position(source, start);
        out.push_str(path);
        out.push(':');
        out.push_str(&(pos.line.saturating_add(1)).to_string());
        out.push(':');
        out.push_str(&(pos.character.saturating_add(1)).to_string());
        out.push_str(": ");
    }

    let line_text = line_index.line_text(source, start);
    let line_start = line_index.line_start_offset(start);
    let span_start_in_line = start.saturating_sub(line_start);
    let span_end_in_line = end.saturating_sub(line_start).min(line_text.len());
    let displayed = truncate_keeping_span(line_text, span_start_in_line, span_end_in_line);
    out.push_str(&displayed);
    out.push('\n');
}

fn write_at_line(out: &mut String, label: &Label, source: &str, start: usize, end: usize) {
    // `  at:   ` — same indent as `in:`.
    out.push_str("  at:   ");
    let span_text = span_display(source, start, end);
    out.push('`');
    out.push_str(&span_text);
    out.push('`');
    if !label.message.is_empty() {
        out.push_str("  -- ");
        out.push_str(&label.message);
    }
    out.push('\n');
}

fn span_display(source: &str, start: usize, end: usize) -> String {
    if start >= source.len() && end >= source.len() {
        return "<eof>".to_owned();
    }
    if start == end {
        return String::new();
    }
    source[start..end].to_owned()
}

/// Clamp span to source bounds and snap to char boundaries.
fn clamp_span(source: &str, span: Span) -> (usize, usize) {
    let len = source.len();
    let mut start = span.start.min(len);
    let mut end = span.end.min(len);
    if end < start {
        end = start;
    }
    start = floor_char_boundary(source, start);
    end = ceil_char_boundary(source, end);
    if end < start {
        end = start;
    }
    (start, end)
}

fn floor_char_boundary(source: &str, mut offset: usize) -> usize {
    offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn ceil_char_boundary(source: &str, mut offset: usize) -> usize {
    offset = offset.min(source.len());
    while offset < source.len() && !source.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

/// Truncate a source line to ~`MAX_IN_LINE_CHARS`, keeping the span window visible.
fn truncate_keeping_span(line: &str, span_start: usize, span_end: usize) -> String {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let char_count = chars.len();
    if char_count <= MAX_IN_LINE_CHARS {
        return line.to_owned();
    }

    let span_start = span_start.min(line.len());
    let span_end = span_end.min(line.len()).max(span_start);

    // Map byte offsets to char indices.
    let start_ci = chars
        .iter()
        .position(|(off, _)| *off >= span_start)
        .unwrap_or(char_count);
    let end_ci = chars
        .iter()
        .position(|(off, _)| *off >= span_end)
        .unwrap_or(char_count);
    let end_ci = end_ci.max(start_ci);

    let span_width = end_ci.saturating_sub(start_ci).max(1);
    // Budget for left/right context after reserving span and possible ellipses.
    let ellipsis = 3; // "..."
    let mut budget = MAX_IN_LINE_CHARS;
    // Always keep span text if possible; if span alone exceeds max, hard-clip span.
    if span_width >= MAX_IN_LINE_CHARS {
        let slice_start = chars[start_ci].0;
        let slice_end = if start_ci + MAX_IN_LINE_CHARS < char_count {
            chars[start_ci + MAX_IN_LINE_CHARS].0
        } else {
            line.len()
        };
        let mut s = line[slice_start..slice_end].to_owned();
        if start_ci + MAX_IN_LINE_CHARS < char_count {
            s.push_str("...");
        }
        return s;
    }

    budget = budget.saturating_sub(span_width);
    let mut left_budget = budget / 2;
    let mut right_budget = budget - left_budget;

    let mut window_start = start_ci.saturating_sub(left_budget);
    let mut window_end = (end_ci + right_budget).min(char_count);

    // Reclaim unused side into the other.
    let actual_left = start_ci - window_start;
    let actual_right = window_end - end_ci;
    if actual_left < left_budget {
        right_budget += left_budget - actual_left;
        window_end = (end_ci + right_budget).min(char_count);
    }
    if actual_right < right_budget {
        left_budget += right_budget - actual_right;
        window_start = start_ci.saturating_sub(left_budget);
    }

    let mut result = String::new();
    if window_start > 0 {
        // Leave room for leading "..."
        if window_end - window_start + ellipsis > MAX_IN_LINE_CHARS {
            window_start = window_start.saturating_add(ellipsis).min(start_ci);
        }
        result.push_str("...");
    }
    let byte_start = if window_start < char_count {
        chars[window_start].0
    } else {
        line.len()
    };
    let byte_end = if window_end < char_count {
        chars[window_end].0
    } else {
        line.len()
    };
    result.push_str(&line[byte_start..byte_end]);
    if window_end < char_count {
        result.push_str("...");
    }
    // Final hard cap if ellipses pushed us over (rare).
    if result.chars().count() > MAX_IN_LINE_CHARS + ellipsis * 2 {
        result = result.chars().take(MAX_IN_LINE_CHARS).collect();
        result.push_str("...");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_IN_LINE_CHARS, clamp_span, relative_display_path, render_agent_diagnostic,
        render_agent_report, truncate_keeping_span,
    };
    use crate::{Diagnostic, FileId, Label, LineIndex, SourceFile, Span, codes};

    fn index(source: &str) -> LineIndex {
        LineIndex::new(source)
    }

    fn find_span(source: &str, needle: &str) -> Span {
        let start = source
            .find(needle)
            .unwrap_or_else(|| panic!("needle {needle:?} not in source"));
        Span::new(start, start + needle.len())
    }

    fn assert_ascii(s: &str) {
        assert!(
            s.is_ascii(),
            "agent output must be pure ASCII, got non-ASCII in: {s:?}"
        );
    }

    #[test]
    fn reduce_unknown_method_shape() {
        let source = "xs: Array(Int) = [1, 2, 3]\ntotal = xs.reduce(0, (a, b) => a + b)\n";
        let reduce = find_span(source, "reduce");
        let diagnostic = Diagnostic::error("`Array` has no method `reduce`")
            .with_code(codes::ty::UNKNOWN_METHOD)
            .with_label(Label::primary(reduce, "unknown method on `Array`"))
            .with_note("did you mean `fold`?");

        let rendered = render_agent_diagnostic(&diagnostic, "d1.av", source, &index(source));
        assert_ascii(&rendered);

        let expected = "\
error[type.unknown-method] d1.av:2:12: `Array` has no method `reduce`
  in:   total = xs.reduce(0, (a, b) => a + b)
  at:   `reduce`  -- unknown method on `Array`
  help: did you mean `fold`?";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn multi_label_different_lines() {
        let source = "make = (value) =>\n  inner = (value) => value\n  inner\n";
        // First `value` (outer param), second `value` (inner param).
        let outer = {
            let start = source.find("value").expect("outer value");
            Span::new(start, start + 5)
        };
        let inner = {
            let start = source[outer.end..]
                .find("value")
                .map(|rel| outer.end + rel)
                .expect("inner value");
            Span::new(start, start + 5)
        };

        let diagnostic = Diagnostic::error("accidental shadowing of `value`")
            .with_code("name.accidental-shadowing")
            .with_label(Label::primary(inner, "new binding shadows this name"))
            .with_label(Label {
                span: outer,
                message: "existing binding with the same name".into(),
            })
            .with_note("use `:=` to shadow intentionally, or rename the binding");

        let rendered = render_agent_diagnostic(&diagnostic, "shadow.av", source, &index(source));
        assert_ascii(&rendered);

        let expected = "\
error[name.accidental-shadowing] shadow.av:2:12: accidental shadowing of `value`
  in:     inner = (value) => value
  at:   `value`  -- new binding shadows this name
  in:   shadow.av:1:9: make = (value) =>
  at:   `value`  -- existing binding with the same name
  help: use `:=` to shadow intentionally, or rename the binding";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn several_notes() {
        let source = "x = 1\n";
        let diagnostic = Diagnostic::warning("something odd")
            .with_code("lint.odd")
            .with_label(Label::primary(Span::new(0, 1), "here"))
            .with_note("first tip")
            .with_note("second tip");

        let rendered = render_agent_diagnostic(&diagnostic, "n.av", source, &index(source));
        assert_ascii(&rendered);
        assert!(rendered.contains("  help: first tip\n  help: second tip"));
        assert!(rendered.starts_with("warning[lint.odd] n.av:1:1:"));
    }

    #[test]
    fn long_source_line_truncates_with_span_visible() {
        let prefix = "a".repeat(80);
        let suffix = "b".repeat(80);
        let source = format!("{prefix}NEEDLE{suffix}\n");
        let start = source.find("NEEDLE").expect("needle");
        let span = Span::new(start, start + 6);
        let diagnostic =
            Diagnostic::error("found needle").with_label(Label::primary(span, "the needle"));

        let rendered = render_agent_diagnostic(&diagnostic, "long.av", &source, &index(&source));
        assert_ascii(&rendered);
        assert!(
            rendered.contains("NEEDLE"),
            "span text must remain visible: {rendered}"
        );
        assert!(
            rendered.contains("..."),
            "long line must truncate: {rendered}"
        );
        // `in:` content should not be a multi-hundred-char dump.
        let in_line = rendered
            .lines()
            .find(|l| l.starts_with("  in:"))
            .expect("in line");
        let content = in_line.trim_start_matches("  in:   ");
        assert!(
            content.chars().count() <= MAX_IN_LINE_CHARS + 10,
            "in: content too long ({}): {content}",
            content.chars().count()
        );
        assert!(rendered.contains("  at:   `NEEDLE`  -- the needle"));
    }

    #[test]
    fn pure_ascii_invariant_across_cases() {
        // Exercise several shapes; every rendered string must be ASCII.
        let cases: Vec<(&str, Diagnostic)> = vec![
            (
                "xs: Array(Int) = [1, 2, 3]\ntotal = xs.reduce(0, (a, b) => a + b)\n",
                Diagnostic::error("`Array` has no method `reduce`")
                    .with_code(codes::ty::UNKNOWN_METHOD)
                    .with_label(Label::primary(
                        find_span(
                            "xs: Array(Int) = [1, 2, 3]\ntotal = xs.reduce(0, (a, b) => a + b)\n",
                            "reduce",
                        ),
                        "unknown method on `Array`",
                    ))
                    .with_note("did you mean `fold`?"),
            ),
            (
                "x = 1\n",
                Diagnostic::error("no code").with_label(Label::primary(Span::point(0), "")),
            ),
            (
                "",
                Diagnostic {
                    severity: crate::Severity::Note,
                    code: None,
                    message: "empty source".into(),
                    labels: Vec::new(),
                    notes: Vec::new(),
                },
            ),
        ];

        for (source, diagnostic) in cases {
            let rendered = render_agent_diagnostic(&diagnostic, "t.av", source, &index(source));
            assert_ascii(&rendered);
        }
    }

    #[test]
    fn empty_and_zero_width_spans_do_not_panic() {
        let source = "hello\n";
        let cases = [
            Span::point(0),
            Span::point(5),
            Span::point(source.len()),
            Span::new(2, 2),
            Span::new(100, 200), // out of range
            Span::new(3, 1),     // inverted
        ];
        for span in cases {
            let diagnostic = Diagnostic::error("point").with_label(Label::primary(span, "cursor"));
            let rendered = render_agent_diagnostic(&diagnostic, "e.av", source, &index(source));
            assert_ascii(&rendered);
            assert!(rendered.contains("  at:   `"), "{rendered}");
        }

        // Empty at EOF uses <eof>.
        let eof = Diagnostic::error("at end")
            .with_label(Label::primary(Span::point(source.len()), "end"));
        let rendered = render_agent_diagnostic(&eof, "e.av", source, &index(source));
        assert!(
            rendered.contains("`<eof>`"),
            "expected <eof> marker: {rendered}"
        );
    }

    #[test]
    fn no_labels_uses_fallback_location() {
        let source = "abc\n";
        let diagnostic = Diagnostic::error("no labels").with_code("x.y");
        let rendered = render_agent_diagnostic(&diagnostic, "f.av", source, &index(source));
        assert_ascii(&rendered);
        // End of file is line 2 col 1 for "abc\n" (line after final newline).
        assert!(rendered.starts_with("error[x.y] f.av:"), "{rendered}");
        assert!(!rendered.contains("error[]"));
        assert!(!rendered.contains("  in:"));
        assert!(!rendered.contains("  at:"));
    }

    #[test]
    fn omits_brackets_when_code_is_none() {
        let source = "x\n";
        let diagnostic = Diagnostic::error("bare").with_label(Label::primary(Span::new(0, 1), "x"));
        let rendered = render_agent_diagnostic(&diagnostic, "b.av", source, &index(source));
        assert!(rendered.starts_with("error b.av:1:1: bare\n"), "{rendered}");
        assert!(!rendered.contains("error["));
    }

    #[test]
    fn same_line_secondary_label_only_extra_at() {
        let source = "foo bar baz\n";
        let foo = find_span(source, "foo");
        let bar = find_span(source, "bar");
        let diagnostic = Diagnostic::error("two spots")
            .with_label(Label::primary(foo, "first"))
            .with_label(Label {
                span: bar,
                message: "second".into(),
            });
        let rendered = render_agent_diagnostic(&diagnostic, "s.av", source, &index(source));
        let in_count = rendered.matches("  in:").count();
        let at_count = rendered.matches("  at:").count();
        assert_eq!(in_count, 1, "same-line labels share one in: — {rendered}");
        assert_eq!(at_count, 2, "two at: lines — {rendered}");
        assert!(rendered.contains("  at:   `foo`  -- first"));
        assert!(rendered.contains("  at:   `bar`  -- second"));
    }

    #[test]
    fn relative_display_path_strips_only_a_prefix() {
        let base = "/home/dave/w/clex";
        assert_eq!(
            relative_display_path("/home/dave/w/clex/a.av", Some(base)),
            "a.av"
        );
        assert_eq!(
            relative_display_path("/home/dave/w/clex/sub/a.av", Some(base)),
            "sub/a.av"
        );
        // A trailing slash on the base must not leave a leading slash behind.
        assert_eq!(
            relative_display_path("/home/dave/w/clex/a.av", Some("/home/dave/w/clex/")),
            "a.av"
        );

        // Outside the base the path is returned untouched. Resolving it would
        // produce `../../..` hops that are longer than the absolute path and
        // harder to act on, which defeats the purpose.
        assert_eq!(
            relative_display_path("/etc/passwd", Some(base)),
            "/etc/passwd"
        );
        // A sibling directory sharing a textual prefix must not be mangled: the
        // separator check is what stops `/clex-other/` becoming `-other/a.av`.
        assert_eq!(
            relative_display_path("/home/dave/w/clex-other/a.av", Some(base)),
            "/home/dave/w/clex-other/a.av"
        );
        // The base itself, and an empty base, leave the path alone.
        assert_eq!(relative_display_path(base, Some(base)), base);
        assert_eq!(relative_display_path("/a.av", Some("")), "/a.av");
        assert_eq!(relative_display_path("/a.av", None), "/a.av");
    }

    #[test]
    fn render_agent_report_shortens_the_path_against_a_base() {
        let source = "a\n";
        let file = SourceFile::new(FileId(0), "/tmp/work/r.av", None, source);
        let diagnostics =
            vec![Diagnostic::error("boom").with_label(Label::primary(Span::new(0, 1), "a"))];
        let rendered = render_agent_report(&file, &diagnostics, Some("/tmp/work"));
        assert!(rendered.contains("error r.av:1:1: boom"), "{rendered}");
        assert!(!rendered.contains("/tmp/work"), "{rendered}");
    }

    #[test]
    fn render_agent_report_joins_diagnostics() {
        let source = "a\nb\n";
        let file = SourceFile::new(FileId(0), "r.av", None, source);
        let diagnostics = vec![
            Diagnostic::error("first").with_label(Label::primary(Span::new(0, 1), "a")),
            Diagnostic::warning("second").with_label(Label::primary(Span::new(2, 3), "b")),
        ];
        let rendered = render_agent_report(&file, &diagnostics, None);
        assert_ascii(&rendered);
        assert!(rendered.contains("error r.av:1:1: first"));
        assert!(rendered.contains("warning r.av:2:1: second"));
        assert_eq!(
            rendered
                .lines()
                .filter(|l| l.starts_with("error") || l.starts_with("warning"))
                .count(),
            2
        );
    }

    #[test]
    fn clamp_span_snaps_to_char_boundaries() {
        let source = "a😀b";
        // Mid-emoji byte offset.
        let (start, end) = clamp_span(source, Span::new(2, 3));
        assert!(source.is_char_boundary(start));
        assert!(source.is_char_boundary(end));
        assert!(start <= end);
    }

    #[test]
    fn truncate_keeps_short_lines() {
        assert_eq!(truncate_keeping_span("short", 0, 5), "short");
    }
}
