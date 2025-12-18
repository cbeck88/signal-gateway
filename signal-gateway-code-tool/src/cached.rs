//! Cached tarball contents and extraction logic.

use flate2::read::GzDecoder;
use globset::GlobSet;
use std::{collections::BTreeMap, io::Read};
use tar::Archive;
use tracing::info;

/// A file stored in memory from the tarball.
#[derive(Debug, Clone)]
pub struct CachedFile {
    /// The file contents as a string (lossy UTF-8 conversion).
    content: Box<str>,
    /// Indices of newline characters in content.
    newline_indices: Vec<u32>,
    /// Whether this file looks like binary data.
    is_binary: bool,
}

impl CachedFile {
    /// Create a new CachedFile from content, detecting if it looks binary.
    fn new(content: String) -> Self {
        assert!(
            content.len() <= u32::MAX as usize,
            "File content exceeds 4GB limit"
        );

        let is_binary = looks_binary(&content);
        let newline_indices: Vec<u32> = content
            .bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i as u32)
            .collect();

        Self {
            content: content.into_boxed_str(),
            newline_indices,
            is_binary,
        }
    }

    /// Get the file content as a string slice.
    pub fn as_str(&self) -> &str {
        &self.content
    }

    /// Check if file looks like binary data.
    pub fn is_binary(&self) -> bool {
        self.is_binary
    }

    /// Get the number of lines in the file.
    pub fn line_count(&self) -> u32 {
        if self.content.is_empty() {
            0
        } else {
            // Number of lines = number of newlines + 1 (unless file ends with newline)
            let count = self.newline_indices.len() as u32;
            if self.content.ends_with('\n') {
                count
            } else {
                count + 1
            }
        }
    }

    /// Iterate over a range of lines (1-indexed, inclusive).
    ///
    /// - `start`: Starting line number (1-indexed). Defaults to 1.
    /// - `end`: Ending line number (inclusive). Defaults to last line.
    pub fn line_range(&self, start: Option<u32>, end: Option<u32>) -> impl Iterator<Item = &str> {
        let total_lines = self.line_count();
        let start_line = start.unwrap_or(1).saturating_sub(1); // Convert to 0-indexed
        let end_line = end
            .unwrap_or(total_lines)
            .min(total_lines)
            .saturating_sub(1); // Convert to 0-indexed

        LineRangeIter {
            content: &self.content,
            newline_indices: &self.newline_indices,
            current_line: start_line,
            end_line,
        }
    }

    /// Find which line (1-indexed) a character index falls on.
    ///
    /// Returns the number of newline characters before `idx`, plus 1.
    pub fn idx_to_line(&self, idx: usize) -> u32 {
        let idx = idx as u32;
        // Binary search to find how many newlines are before idx
        match self.newline_indices.binary_search(&idx) {
            Ok(pos) => pos as u32 + 1,  // idx is exactly on a newline
            Err(pos) => pos as u32 + 1, // pos is the number of newlines before idx
        }
    }
}

/// Iterator over a range of lines.
struct LineRangeIter<'a> {
    content: &'a str,
    newline_indices: &'a [u32],
    current_line: u32, // 0-indexed
    end_line: u32,     // 0-indexed, inclusive
}

impl<'a> Iterator for LineRangeIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_line > self.end_line {
            return None;
        }

        let line = self.current_line as usize;

        // Get start offset: after previous line's newline, or 0 for first line
        let start = if line == 0 {
            0
        } else {
            self.newline_indices
                .get(line - 1)
                .map(|&i| i as usize + 1)?
        };

        // Get end offset: at this line's newline, or end of content for last line
        let end = self
            .newline_indices
            .get(line)
            .map(|&i| i as usize)
            .unwrap_or(self.content.len());

        if start > self.content.len() {
            return None;
        }

        self.current_line += 1;
        Some(&self.content[start..end])
    }
}

/// Cached tarball contents.
pub struct CachedTarball {
    /// The git SHA this tarball corresponds to.
    pub sha: String,
    /// Map from file path to file contents (sorted by path).
    pub files: BTreeMap<String, CachedFile>,
}

impl CachedTarball {
    /// Extract a tarball into a CachedTarball.
    ///
    /// - `glob_filter`: Optional pre-compiled glob patterns to filter files.
    /// - `include_non_utf8`: Whether to include non-UTF-8 files using lossy conversion.
    pub fn extract(
        current_sha: String,
        tarball: &[u8],
        glob_filter: Option<&GlobSet>,
        include_non_utf8: bool,
    ) -> Result<Self, String> {
        let files = extract_files(tarball, glob_filter, include_non_utf8)?;

        info!("Extracted {} files from tarball", files.len());

        Ok(Self {
            sha: current_sha,
            files,
        })
    }
}

/// Extract files from a tarball into a map.
fn extract_files(
    tarball: &[u8],
    glob_filter: Option<&GlobSet>,
    include_non_utf8: bool,
) -> Result<BTreeMap<String, CachedFile>, String> {
    let decoder = GzDecoder::new(tarball);
    let mut archive = Archive::new(decoder);

    let mut files = BTreeMap::new();

    for entry in archive
        .entries()
        .map_err(|e| format!("Failed to read tarball: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;

        // Skip directories
        if entry.header().entry_type().is_dir() {
            continue;
        }

        let path = entry
            .path()
            .map_err(|e| format!("Failed to get path: {e}"))?
            .to_string_lossy()
            .to_string();

        // GitHub tarballs have a prefix like "owner-repo-sha/"
        // Strip the first component
        let path = path.split('/').skip(1).collect::<Vec<_>>().join("/");

        if path.is_empty() {
            continue;
        }

        // Apply glob filter if configured
        if let Some(glob_filter) = glob_filter
            && !glob_filter.is_match(&path)
        {
            continue;
        }

        // Read file contents
        let mut contents = Vec::new();
        if entry.read_to_end(&mut contents).is_err() {
            continue; // Skip files we can't read
        }

        // Convert to string, handling non-UTF-8 based on config
        let content = match String::from_utf8(contents) {
            Ok(s) => s,
            Err(e) => {
                if include_non_utf8 {
                    // Use lossy conversion if configured to include non-UTF-8
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                } else {
                    // Skip non-UTF-8 files by default
                    continue;
                }
            }
        };

        files.insert(path, CachedFile::new(content));
    }

    Ok(files)
}

/// Check if content looks like binary data.
fn looks_binary(content: &str) -> bool {
    // Check first 1000 chars for null bytes or high ratio of non-printable chars
    let sample: String = content.chars().take(1000).collect();
    let non_printable = sample
        .chars()
        .filter(|c| !c.is_ascii_graphic() && !c.is_ascii_whitespace())
        .count();
    non_printable > sample.len() / 10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_file(content: &str) -> CachedFile {
        CachedFile::new(content.to_string())
    }

    #[test]
    fn test_line_count_empty() {
        let file = make_file("");
        assert_eq!(file.line_count(), 0);
    }

    #[test]
    fn test_line_count_single_line_no_newline() {
        let file = make_file("hello");
        assert_eq!(file.line_count(), 1);
    }

    #[test]
    fn test_line_count_single_line_with_newline() {
        let file = make_file("hello\n");
        assert_eq!(file.line_count(), 1);
    }

    #[test]
    fn test_line_count_multiple_lines() {
        let file = make_file("line1\nline2\nline3");
        assert_eq!(file.line_count(), 3);
    }

    #[test]
    fn test_line_count_multiple_lines_trailing_newline() {
        let file = make_file("line1\nline2\nline3\n");
        assert_eq!(file.line_count(), 3);
    }

    #[test]
    fn test_line_range_all_lines() {
        let file = make_file("line1\nline2\nline3");
        let lines: Vec<_> = file.line_range(None, None).collect();
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn test_line_range_subset() {
        let file = make_file("line1\nline2\nline3\nline4\nline5");
        let lines: Vec<_> = file.line_range(Some(2), Some(4)).collect();
        assert_eq!(lines, vec!["line2", "line3", "line4"]);
    }

    #[test]
    fn test_line_range_single_line() {
        let file = make_file("line1\nline2\nline3");
        let lines: Vec<_> = file.line_range(Some(2), Some(2)).collect();
        assert_eq!(lines, vec!["line2"]);
    }

    #[test]
    fn test_line_range_past_end() {
        let file = make_file("line1\nline2");
        let lines: Vec<_> = file.line_range(Some(1), Some(10)).collect();
        assert_eq!(lines, vec!["line1", "line2"]);
    }

    #[test]
    fn test_idx_to_line_first_line() {
        let file = make_file("hello\nworld\ntest");
        // "hello" is at indices 0-4, newline at 5
        assert_eq!(file.idx_to_line(0), 1);
        assert_eq!(file.idx_to_line(4), 1);
        assert_eq!(file.idx_to_line(5), 1); // newline itself is on line 1
    }

    #[test]
    fn test_idx_to_line_second_line() {
        let file = make_file("hello\nworld\ntest");
        // "world" is at indices 6-10, newline at 11
        assert_eq!(file.idx_to_line(6), 2);
        assert_eq!(file.idx_to_line(10), 2);
        assert_eq!(file.idx_to_line(11), 2); // newline itself is on line 2
    }

    #[test]
    fn test_idx_to_line_third_line() {
        let file = make_file("hello\nworld\ntest");
        // "test" is at indices 12-15
        assert_eq!(file.idx_to_line(12), 3);
        assert_eq!(file.idx_to_line(15), 3);
    }

    #[test]
    fn test_as_str() {
        let file = make_file("hello\nworld");
        assert_eq!(file.as_str(), "hello\nworld");
    }
}
