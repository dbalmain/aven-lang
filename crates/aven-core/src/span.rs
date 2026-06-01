#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn point(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Returns true when `other` starts inside this span.
    ///
    /// Empty spans are treated as one-byte cursor positions, which matches the
    /// parser and LSP use cases for point diagnostics and cursor references.
    pub fn contains(self, other: Self) -> bool {
        let end = self.end.max(self.start.saturating_add(1));
        other.start >= self.start && other.start < end
    }
}

#[cfg(test)]
mod tests {
    use super::Span;

    #[test]
    fn contains_spans_starting_inside_the_span() {
        assert!(Span::new(10, 20).contains(Span::new(10, 11)));
        assert!(Span::new(10, 20).contains(Span::new(19, 30)));
        assert!(!Span::new(10, 20).contains(Span::new(20, 21)));
    }

    #[test]
    fn contains_treats_empty_spans_as_cursor_positions() {
        assert!(Span::point(10).contains(Span::point(10)));
        assert!(!Span::point(10).contains(Span::point(11)));
    }
}
