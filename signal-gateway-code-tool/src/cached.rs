//! Cached tarball contents and extraction logic.

use flate2::read::GzDecoder;
use globset::GlobSet;
use std::{collections::HashMap, io::Read};
use tar::Archive;
use tracing::info;

/// A file stored in memory from the tarball.
#[derive(Debug, Clone)]
pub struct CachedFile {
    /// The file contents as a string (lossy UTF-8 conversion).
    pub content: String,
    /// Whether this file looks like binary data.
    pub is_binary: bool,
}

impl CachedFile {
    /// Create a new CachedFile from content, detecting if it looks binary.
    fn new(content: String) -> Self {
        let is_binary = looks_binary(&content);
        Self { content, is_binary }
    }
}

/// Cached tarball contents.
pub struct CachedTarball {
    /// The git SHA this tarball corresponds to.
    pub sha: String,
    /// Map from file path to file contents.
    pub files: HashMap<String, CachedFile>,
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
) -> Result<HashMap<String, CachedFile>, String> {
    let decoder = GzDecoder::new(tarball);
    let mut archive = Archive::new(decoder);

    let mut files = HashMap::new();

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
        if let Some(glob_filter) = glob_filter {
            if !glob_filter.is_match(&path) {
                continue;
            }
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
