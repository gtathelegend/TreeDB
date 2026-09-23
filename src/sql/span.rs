use std::fmt;

/// Represents a byte-offset range within the original SQL query text `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SourceSpan {
    /// Zero-based starting byte offset (inclusive).
    pub start: usize,
    /// Zero-based ending byte offset (exclusive).
    pub end: usize,
}

impl SourceSpan {
    /// Creates a new `SourceSpan` covering the range `[start, end)`.
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Creates a zero-length span at a single point.
    pub const fn point(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    /// Combines two spans into a single span enclosing both ranges.
    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Returns the length in bytes covered by this span.
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Returns `true` if the span covers zero bytes.
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

impl fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}
