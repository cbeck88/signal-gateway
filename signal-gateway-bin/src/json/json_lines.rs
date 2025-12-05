//! Relaxed JSON Lines reader that allows newlines within JSON objects.
//!
//! Standard JSON Lines requires each JSON value to be on a single line.
//! This implementation relaxes that by tracking brace/bracket depth and
//! only treating newlines as delimiters when outside of JSON structures.

use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

/// Read a single JSON value from a relaxed JSON Lines stream.
///
/// Returns:
/// - `Ok(Some(bytes))` - A complete JSON value was read
/// - `Ok(None)` - EOF reached (no more values)
/// - `Err(InvalidData)` - Malformed JSON structure (imbalanced braces/brackets)
/// - `Err(other)` - I/O error
///
/// This parser tracks brace and bracket depth to allow newlines within
/// JSON objects and arrays. A newline only ends the value when we're at
/// depth 0 (outside any object or array).
pub async fn read_json_lines_value<R>(reader: &mut BufReader<R>) -> std::io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::<u8>::with_capacity(256);

    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;

    loop {
        let b = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // EOF - return what we have if anything
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Err(e) => return Err(e),
        };

        if escaped {
            escaped = false;
            buf.push(b);
            continue;
        }

        if quoted {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
            buf.push(b);
            continue;
        }

        // Not escaped, not quoted
        match b {
            b'"' => {
                quoted = true;
                buf.push(b);
            }
            b'{' => {
                brace_depth += 1;
                buf.push(b);
            }
            b'[' => {
                bracket_depth += 1;
                buf.push(b);
            }
            b'}' => {
                if brace_depth == 0 {
                    let rendered_buf = String::from_utf8_lossy(&buf);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("imbalanced braces: {rendered_buf}}}"),
                    ));
                }
                brace_depth -= 1;
                buf.push(b);
            }
            b']' => {
                if bracket_depth == 0 {
                    let rendered_buf = String::from_utf8_lossy(&buf);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("imbalanced brackets: {rendered_buf}]"),
                    ));
                }
                bracket_depth -= 1;
                buf.push(b);
            }
            b'\n' => {
                if brace_depth == 0 && bracket_depth == 0 {
                    // Clean line break at depth 0 - end of value
                    if buf.is_empty() {
                        // Skip empty lines
                        continue;
                    }
                    break;
                }
                // Newline inside object/array - keep it
                buf.push(b);
            }
            _ => {
                buf.push(b);
            }
        }
    }

    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_json_object() {
        let data: &[u8] = b"{\"msg\": \"hello\"}\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"{\"msg\": \"hello\"}".to_vec()));
    }

    #[tokio::test]
    async fn test_json_with_internal_newlines() {
        let data: &[u8] = b"{\n  \"msg\": \"hello\",\n  \"level\": \"info\"\n}\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(
            result,
            Some(b"{\n  \"msg\": \"hello\",\n  \"level\": \"info\"\n}".to_vec())
        );
    }

    #[tokio::test]
    async fn test_eof_returns_none() {
        let data: &[u8] = b"";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_eof_returns_partial_buffer() {
        // No trailing newline
        let data: &[u8] = b"{\"msg\": \"hello\"}";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"{\"msg\": \"hello\"}".to_vec()));
    }

    #[tokio::test]
    async fn test_multiple_values() {
        let data: &[u8] = b"{\"a\": 1}\n{\"b\": 2}\n";
        let mut reader = BufReader::new(data);

        let r1 = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(r1, Some(b"{\"a\": 1}".to_vec()));

        let r2 = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(r2, Some(b"{\"b\": 2}".to_vec()));

        let r3 = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(r3, None);
    }

    #[tokio::test]
    async fn test_skips_empty_lines() {
        let data: &[u8] = b"\n\n{\"msg\": \"hello\"}\n\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"{\"msg\": \"hello\"}".to_vec()));
    }

    #[tokio::test]
    async fn test_nested_braces() {
        let data: &[u8] = b"{\"nested\": {\"deep\": {}}}\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"{\"nested\": {\"deep\": {}}}".to_vec()));
    }

    #[tokio::test]
    async fn test_array_value() {
        let data: &[u8] = b"[1, 2, 3]\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"[1, 2, 3]".to_vec()));
    }

    #[tokio::test]
    async fn test_string_with_escaped_quote() {
        let data: &[u8] = b"{\"msg\": \"hello \\\"world\\\"\"}\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"{\"msg\": \"hello \\\"world\\\"\"}".to_vec()));
    }

    #[tokio::test]
    async fn test_string_with_newline() {
        // Newline inside a string should be preserved (though this is technically invalid JSON)
        let data: &[u8] = b"{\"msg\": \"line1\nline2\"}\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await.unwrap();
        assert_eq!(result, Some(b"{\"msg\": \"line1\nline2\"}".to_vec()));
    }

    #[tokio::test]
    async fn test_imbalanced_brace_error() {
        let data: &[u8] = b"{\"msg\": \"hello\"}}\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_imbalanced_bracket_error() {
        let data: &[u8] = b"[1, 2]]\n";
        let mut reader = BufReader::new(data);
        let result = read_json_lines_value(&mut reader).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }
}
