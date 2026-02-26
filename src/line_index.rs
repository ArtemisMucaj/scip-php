/// Maps byte offsets to (line, column) positions for SCIP range encoding.
/// All values are 0-based.
pub struct LineIndex {
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

impl LineIndex {
    /// Build a line index from source text.
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (offset, byte) in source.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push((offset + 1) as u32);
            }
        }
        LineIndex { line_starts }
    }

    /// Convert a byte offset to (line, column), both 0-based.
    /// Column is in UTF-8 byte offset from line start.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let col = offset - self.line_starts[line];
        (line as u32, col)
    }

    /// Encode a byte range as a SCIP range (3 or 4 element i32 vec).
    /// start and end are byte offsets.
    pub fn scip_range(&self, start: u32, end: u32) -> Vec<i32> {
        let (start_line, start_col) = self.line_col(start);
        let (end_line, end_col) = self.line_col(end);
        if start_line == end_line {
            vec![start_line as i32, start_col as i32, end_col as i32]
        } else {
            vec![
                start_line as i32,
                start_col as i32,
                end_line as i32,
                end_col as i32,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_line() {
        let src = "hello world";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(0), (0, 0));
        assert_eq!(idx.line_col(5), (0, 5));
        assert_eq!(idx.line_col(10), (0, 10));
    }

    #[test]
    fn test_multi_line() {
        let src = "line1\nline2\nline3";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(0), (0, 0)); // 'l' of line1
        assert_eq!(idx.line_col(5), (0, 5)); // '\n'
        assert_eq!(idx.line_col(6), (1, 0)); // 'l' of line2
        assert_eq!(idx.line_col(11), (1, 5)); // '\n'
        assert_eq!(idx.line_col(12), (2, 0)); // 'l' of line3
    }

    #[test]
    fn test_scip_range_single_line() {
        let src = "<?php\nfunction foo() {}";
        let idx = LineIndex::new(src);
        // "foo" starts at offset 15, ends at 18 (on line 1)
        let range = idx.scip_range(15, 18);
        assert_eq!(range, vec![1, 9, 12]); // line 1, col 9..12
    }

    #[test]
    fn test_scip_range_multi_line() {
        let src = "line1\nline2\nline3";
        let idx = LineIndex::new(src);
        // span from offset 0 (line 0, col 0) to offset 16 (line 2, col 4)
        let range = idx.scip_range(0, 16);
        assert_eq!(range, vec![0, 0, 2, 4]);
    }

    #[test]
    fn test_empty_source() {
        let src = "";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(0), (0, 0));
    }
}
