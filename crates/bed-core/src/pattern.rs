//! Compiled byte-oriented regular expressions shared by search and substitution.

use anyhow::{Context, Result};
use regex::bytes::{CaptureMatches, Regex, RegexBuilder};

#[derive(Clone, Debug)]
pub struct RegexPattern {
    source: String,
    expression: Regex,
}

impl RegexPattern {
    pub fn compile(source: &str) -> Result<Self> {
        let expression = RegexBuilder::new(source)
            .build()
            .with_context(|| format!("invalid regular expression {source:?}"))?;
        Ok(Self {
            source: source.to_owned(),
            expression,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn captures_iter<'r, 'h>(&'r self, haystack: &'h [u8]) -> CaptureMatches<'r, 'h> {
        self.expression.captures_iter(haystack)
    }

    pub(crate) fn matching_offsets(&self, bytes: &[u8]) -> Vec<usize> {
        self.expression
            .find_iter(bytes)
            .map(|matched| matched.start())
            .filter(|&offset| {
                editable_boundary(bytes, offset) && normal_cursor_boundary(bytes, offset)
            })
            .collect()
    }
}

fn normal_cursor_boundary(bytes: &[u8], offset: usize) -> bool {
    if bytes.is_empty() {
        return offset == 0;
    }
    if offset == bytes.len() {
        return bytes.last() == Some(&b'\n');
    }
    !matches!(bytes[offset], b'\r' | b'\n')
}

fn editable_boundary(bytes: &[u8], target: usize) -> bool {
    if target == bytes.len() {
        return true;
    }
    let mut offset = 0;
    while offset < bytes.len() {
        if offset == target {
            return true;
        }
        offset = next_editable_offset(bytes, offset);
        if offset > target {
            return false;
        }
    }
    false
}

fn next_editable_offset(bytes: &[u8], offset: usize) -> usize {
    let tail = &bytes[offset.min(bytes.len())..];
    let valid_length = std::str::from_utf8(tail).map_or_else(|error| error.valid_up_to(), str::len);
    if valid_length > 0 {
        let text = std::str::from_utf8(&tail[..valid_length]).expect("validated UTF-8 prefix");
        if let Some(grapheme) =
            unicode_segmentation::UnicodeSegmentation::graphemes(text, true).next()
        {
            return offset + grapheme.len();
        }
    }

    let mut next = (offset + 1).min(bytes.len());
    while next < bytes.len() && bytes[next] & 0b1100_0000 == 0b1000_0000 {
        next += 1;
    }
    next
}

#[cfg(test)]
mod tests {
    use super::RegexPattern;

    #[test]
    fn rejects_matches_inside_graphemes_but_accepts_invalid_byte_boundaries() {
        let combining = RegexPattern::compile("\\p{M}").unwrap();
        assert!(combining.matching_offsets("e\u{301}".as_bytes()).is_empty());

        let invalid = RegexPattern::compile("(?-u:.)").unwrap();
        assert_eq!(invalid.matching_offsets(&[0xff, b'x']), [0, 1]);
    }

    #[test]
    fn exposes_the_original_expression() {
        let pattern = RegexPattern::compile("(?i)bed").unwrap();
        assert_eq!(pattern.source(), "(?i)bed");
    }

    #[test]
    fn excludes_line_end_insert_positions_from_search_results() {
        let end = RegexPattern::compile("$").unwrap();
        assert!(end.matching_offsets(b"text").is_empty());
        assert_eq!(end.matching_offsets(b"text\n"), [5]);
        assert_eq!(end.matching_offsets(b""), [0]);
    }
}
