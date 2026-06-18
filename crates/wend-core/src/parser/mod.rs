//! Transcript parsing: read a `.jsonl` file into ordered [`Routed`] records,
//! tolerating corrupt/truncated lines.

pub(crate) mod content;
pub(crate) mod routing;

use crate::error::Result;
use crate::model::Routed;
use std::io::BufRead;
use std::path::Path;

/// A line that could not be parsed and was skipped (corrupt/truncated/unreadable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedLine {
    pub line_no: usize,
    pub reason: String,
}

/// The parsed contents of one `.jsonl` transcript file, in line order.
#[derive(Debug, Default)]
pub struct ParsedFile {
    /// `(line_no, record)` in file order. File order is chronological for our
    /// purposes (verified: boundary timestamps are monotonic by line).
    pub records: Vec<(usize, Routed)>,
    /// Lines that failed to parse and were skipped, with their reason.
    pub skipped: Vec<SkippedLine>,
}

impl ParsedFile {
    /// Number of lines skipped due to parse/read errors.
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }
}

/// Parse a transcript file at `path`. Only a failure to *open* the file is an
/// error; corrupt lines inside are skipped and recorded in [`ParsedFile::skipped`].
pub fn parse_file(path: &Path) -> Result<ParsedFile> {
    let file = std::fs::File::open(path)?;
    Ok(parse_reader(std::io::BufReader::new(file)))
}

/// Parse from any buffered reader (used by tests with in-memory data). Infallible:
/// unreadable/corrupt lines are recorded in [`ParsedFile::skipped`], not returned.
pub fn parse_reader<R: BufRead>(reader: R) -> ParsedFile {
    let mut out = ParsedFile::default();
    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(line_no, error = %e, "unreadable line, skipping");
                out.skipped.push(SkippedLine {
                    line_no,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(value) => out.records.push((line_no, routing::route(&value, line_no))),
            Err(e) => {
                tracing::warn!(line_no, error = %e, "corrupt json line, skipping");
                out.skipped.push(SkippedLine {
                    line_no,
                    reason: e.to_string(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Routed;
    use std::io::Cursor;

    #[test]
    fn tolerates_corrupt_and_blank_lines() {
        let data = "\
{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"message\":{\"role\":\"user\",\"content\":\"hi\"}}

{ this is not valid json
{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}
";
        let parsed = parse_reader(Cursor::new(data));
        assert_eq!(parsed.skipped_count(), 1, "one corrupt line skipped");
        assert_eq!(parsed.skipped[0].line_no, 3, "the corrupt line is line 3");
        assert_eq!(parsed.records.len(), 2, "two valid records kept");
        assert!(matches!(parsed.records[0].1, Routed::Message(_)));
        // line numbers preserved across the blank + corrupt lines
        assert_eq!(parsed.records[0].0, 1);
        assert_eq!(parsed.records[1].0, 4);
    }
}
