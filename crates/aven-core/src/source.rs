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
}
