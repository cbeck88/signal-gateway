use tokio::{
    io::{Reader, BufReader},
};

pub async fn read_json_lines_value<R>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Vec<u8>>
    where R: Reader,
{
    let mut buf = Vec::<u8>::with_capacity(256);

    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;

    loop {
        let b = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break, // EOF
            Err(e) => return e;
        };

        if escaped {
            escaped = false;
        } else if quoted {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'\\' => { escaped = true; },
                b'"' => { quoted = true; },
                b'{' => { brace_depth += 1; },
                b'[' => { bracket_depth += 1; },
                b'}' => {
                    if brace_depth == 0 {
                        let rendered_buf = String::from_utf8_lossy(&buf);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("imbalanced braces: {rendered_buf}\}"),
                        ));
                    }
                    brace_depth -= 1;
                },
                b']' => {
                    if bracket_depth == 0 {
                        let rendered_buf = String::from_utf8_lossy(&buf);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("imbalanced brackets: {rendered_buf}]"),
                        ));
                    }
                    bracket_depth -= 1;
                },
                b'\n' => {
                    if !escaped && !quoted && brace_depth == 0 && bracket_depth == 0 {
                        // Clean line break
                        buf.push('\n');
                        break;
                    }
                }
                _ => {},
            }
        }
        buf.push(b);
    }
    Ok(buf)
}
