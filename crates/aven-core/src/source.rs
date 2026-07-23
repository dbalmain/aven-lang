use std::path::PathBuf;

use crate::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourcePosition {
    pub line: u32,
    pub character: u32,
}

impl SourcePosition {
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    /// Build byte-to-position indexes for `source`.
    ///
    /// Call conversion methods with the same source text used here. Prefer
    /// using the `LineIndex` stored on `SourceFile`, where that pairing is
    /// structural. Line starts are detected from `\n` bytes; CRLF normalization
    /// remains a lexer/source-loading concern for now.
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];

        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(index + 1);
            }
        }

        Self { line_starts }
    }

    pub fn offset_to_position(&self, source: &str, offset: usize) -> SourcePosition {
        let offset = offset.min(source.len());
        let line_index = self.line_index_for_offset(offset);
        let line_start = self.line_starts[line_index];
        let character = utf16_width_until(source, line_start, offset);

        SourcePosition::new(usize_to_u32(line_index), character)
    }

    pub fn position_to_offset(&self, source: &str, position: SourcePosition) -> Option<usize> {
        let line_index = usize::try_from(position.line).ok()?;
        let line_start = *self.line_starts.get(line_index)?;
        let line_end = self.line_end(source, line_index);
        let mut character = 0u32;

        for (relative_offset, ch) in source[line_start..line_end].char_indices() {
            if character >= position.character {
                return Some(line_start + relative_offset);
            }

            let next_character = character + ch.len_utf16() as u32;
            if position.character < next_character {
                return Some(line_start + relative_offset);
            }
            character = next_character;
        }

        Some(line_end)
    }

    pub fn span_to_range(&self, source: &str, span: Span) -> (SourcePosition, SourcePosition) {
        (
            self.offset_to_position(source, span.start),
            self.offset_to_position(source, span.end.max(span.start.saturating_add(1))),
        )
    }

    fn line_index_for_offset(&self, offset: usize) -> usize {
        self.line_starts
            .partition_point(|line_start| *line_start <= offset)
            - 1
    }

    fn line_end(&self, source: &str, line_index: usize) -> usize {
        self.line_starts
            .get(line_index + 1)
            .map_or(source.len(), |next_line_start| next_line_start - 1)
    }
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: FileId,
    pub path: Option<PathBuf>,
    pub name: String,
    source: String,
    line_index: LineIndex,
}

impl SourceFile {
    pub fn new(
        id: FileId,
        name: impl Into<String>,
        path: Option<PathBuf>,
        source: impl Into<String>,
    ) -> Self {
        let source = source.into();
        let line_index = LineIndex::new(&source);

        Self {
            id,
            path,
            name: name.into(),
            source,
            line_index,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }
}

#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(
        &mut self,
        name: impl Into<String>,
        path: Option<PathBuf>,
        source: impl Into<String>,
    ) -> FileId {
        let id = FileId(self.files.len());
        self.files.push(SourceFile::new(id, name, path, source));
        id
    }

    pub fn get(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(id.0)
    }

    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }
}

fn utf16_width_until(source: &str, start: usize, end: usize) -> u32 {
    let mut width = 0u32;

    for (relative_offset, ch) in source[start..].char_indices() {
        let offset = start + relative_offset;
        if offset >= end || ch == '\n' {
            break;
        }
        width = width.saturating_add(ch.len_utf16() as u32);
    }

    width
}

fn usize_to_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests {
    use super::{LineIndex, SourcePosition};
    use crate::Span;
    use proptest::prelude::*;

    #[test]
    fn converts_offsets_to_positions() {
        let source = "one\ntwo\n";
        let index = LineIndex::new(source);

        assert_eq!(
            index.offset_to_position(source, 0),
            SourcePosition::new(0, 0)
        );
        assert_eq!(
            index.offset_to_position(source, 3),
            SourcePosition::new(0, 3)
        );
        assert_eq!(
            index.offset_to_position(source, 4),
            SourcePosition::new(1, 0)
        );
        assert_eq!(
            index.offset_to_position(source, 7),
            SourcePosition::new(1, 3)
        );
        assert_eq!(
            index.offset_to_position(source, 99),
            SourcePosition::new(2, 0)
        );
    }

    #[test]
    fn converts_positions_to_offsets() {
        let source = "one\ntwo\n";
        let index = LineIndex::new(source);

        assert_eq!(
            index.position_to_offset(source, SourcePosition::new(0, 0)),
            Some(0)
        );
        assert_eq!(
            index.position_to_offset(source, SourcePosition::new(0, 3)),
            Some(3)
        );
        assert_eq!(
            index.position_to_offset(source, SourcePosition::new(0, 99)),
            Some(3)
        );
        assert_eq!(
            index.position_to_offset(source, SourcePosition::new(1, 0)),
            Some(4)
        );
        assert_eq!(
            index.position_to_offset(source, SourcePosition::new(9, 0)),
            None
        );
    }

    #[test]
    fn uses_utf16_character_offsets() {
        let source = "a😀b\n";
        let index = LineIndex::new(source);

        assert_eq!(
            index.offset_to_position(source, 1),
            SourcePosition::new(0, 1)
        );
        assert_eq!(
            index.offset_to_position(source, 5),
            SourcePosition::new(0, 3)
        );
        assert_eq!(
            index.offset_to_position(source, 6),
            SourcePosition::new(0, 4)
        );
        assert_eq!(
            index.position_to_offset(source, SourcePosition::new(0, 3)),
            Some(5)
        );
    }

    #[test]
    fn converts_spans_to_position_ranges() {
        let source = "one\ntwo\n";
        let index = LineIndex::new(source);

        assert_eq!(
            index.span_to_range(source, Span::new(4, 7)),
            (SourcePosition::new(1, 0), SourcePosition::new(1, 3))
        );
        assert_eq!(
            index.span_to_range(source, Span::point(4)),
            (SourcePosition::new(1, 0), SourcePosition::new(1, 1))
        );
    }

    /// Hazard-heavy chunk tokens for synthetic source strings.
    ///
    /// Mix deliberately includes: ASCII, tab, LF, CRLF, lone CR, empty,
    /// 2-byte (`é`), 3-byte (`中`), and 4-byte / UTF-16 surrogate-pair (`😀`).
    fn source_chunk_strategy() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec![
            "hello", "world", "a", " ", "\t", "\n", "\r\n", "\r", "", "é", "中", "😀",
        ])
    }

    fn source_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(source_chunk_strategy(), 0..24).prop_map(|chunks| chunks.concat())
    }

    /// Char-boundary byte offsets, including `source.len()` (end-of-source).
    fn char_boundary_offsets(source: &str) -> Vec<usize> {
        let mut offsets: Vec<usize> = source.char_indices().map(|(i, _)| i).collect();
        offsets.push(source.len());
        offsets
    }

    fn pos_key(pos: SourcePosition) -> (u32, u32) {
        (pos.line, pos.character)
    }

    // property-test tiering: same 64-case default / PROPTEST_CASES override as
    // `span.rs` (see the comment there). Domain properties live here.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Char-boundary offset → position → offset is identity.
        ///
        /// Guards: UTF-16 `character` width (not bytes/codepoints); mid-char
        /// byte offsets are excluded because `offset_to_position` clamps those
        /// to the following UTF-16 column, so only char boundaries roundtrip.
        #[test]
        fn offset_position_roundtrip_at_char_boundaries(source in source_strategy()) {
            let index = LineIndex::new(&source);
            for o in char_boundary_offsets(&source) {
                let pos = index.offset_to_position(&source, o);
                prop_assert_eq!(
                    index.position_to_offset(&source, pos),
                    Some(o),
                    "roundtrip failed at offset {} in {:?}",
                    o,
                    source
                );
            }
        }

        /// Walking char-boundary offsets: positions are non-decreasing in
        /// `(line, character)`; column strictly advances on same line; a line
        /// increase means the previous boundary was a `\n` and the new
        /// character is 0.
        ///
        /// Guards: UTF-16 width + `\n`-only line splits (`\r` is ordinary).
        #[test]
        fn offset_to_position_is_monotonic_with_column_reset(
            source in source_strategy()
        ) {
            let index = LineIndex::new(&source);
            let boundaries = char_boundary_offsets(&source);
            let mut prev_offset: Option<usize> = None;
            let mut prev_pos: Option<SourcePosition> = None;

            for &o in &boundaries {
                let pos = index.offset_to_position(&source, o);
                if let (Some(po), Some(pp)) = (prev_offset, prev_pos) {
                    prop_assert!(
                        pos.line >= pp.line,
                        "line decreased: offset {} -> {:?}, {} -> {:?} in {:?}",
                        po,
                        pp,
                        o,
                        pos,
                        source
                    );
                    if pos.line == pp.line {
                        prop_assert!(
                            pos.character > pp.character,
                            "same-line character not strictly greater: \
                             offset {} -> {:?}, {} -> {:?} in {:?}",
                            po,
                            pp,
                            o,
                            pos,
                            source
                        );
                    } else {
                        // Line advance only at a `\n` boundary; next column is 0.
                        prop_assert_eq!(
                            source.as_bytes().get(po),
                            Some(&b'\n'),
                            "line increase without prior \\n at offset {} in {:?}",
                            po,
                            source
                        );
                        prop_assert_eq!(
                            pos.character,
                            0,
                            "column not reset after \\n: offset {} -> {:?} in {:?}",
                            o,
                            pos,
                            source
                        );
                    }
                }
                prev_offset = Some(o);
                prev_pos = Some(pos);
            }
        }

        /// `span_to_range` equals the two independent conversions, with end
        /// widened via `end.max(start + 1)` so a point span is one unit wide.
        ///
        /// Guards: the deliberate `max(start+1)` widening (do not assert
        /// `range.1 == offset_to_position(span.end)` for empty spans).
        #[test]
        fn span_to_range_matches_offset_conversions(source in source_strategy()) {
            let index = LineIndex::new(&source);
            let boundaries = char_boundary_offsets(&source);

            for (i, &start) in boundaries.iter().enumerate() {
                for &end in &boundaries[i..] {
                    let range = index.span_to_range(&source, Span::new(start, end));
                    let expected_end_offset = end.max(start.saturating_add(1));
                    let expected = (
                        index.offset_to_position(&source, start),
                        index.offset_to_position(&source, expected_end_offset),
                    );
                    prop_assert_eq!(
                        range,
                        expected,
                        "span_to_range mismatch for Span::new({}, {}) in {:?}",
                        start,
                        end,
                        source
                    );
                    prop_assert!(
                        pos_key(range.0) <= pos_key(range.1),
                        "range not ordered: {:?} > {:?} for Span::new({}, {}) in {:?}",
                        range.0,
                        range.1,
                        start,
                        end,
                        source
                    );
                }
            }
        }

        /// Arbitrary positions never panic; `None` iff line is past the last
        /// line. Over-wide `character` still returns `Some` (clamped to EOL).
        ///
        /// Guards: line-out-of-range is the only `None` path; character clamp
        /// and char-boundary return offsets.
        #[test]
        fn position_to_offset_total_and_line_none(
            source in source_strategy(),
            line in any::<u32>(),
            character in any::<u32>(),
        ) {
            let index = LineIndex::new(&source);
            let last_line = index.offset_to_position(&source, source.len()).line;
            let pos = SourcePosition::new(line, character);
            let result = index.position_to_offset(&source, pos);

            if line <= last_line {
                let off = result.expect("in-range line must yield Some");
                prop_assert!(
                    off <= source.len(),
                    "offset {} past source.len() {} for {:?} in {:?}",
                    off,
                    source.len(),
                    pos,
                    source
                );
                prop_assert!(
                    source.is_char_boundary(off),
                    "offset {} not a char boundary for {:?} in {:?}",
                    off,
                    pos,
                    source
                );
            } else {
                prop_assert_eq!(
                    result,
                    None,
                    "expected None for line past last_line={} got {:?} for {:?} in {:?}",
                    last_line,
                    result,
                    pos,
                    source
                );
            }
        }
    }
}
