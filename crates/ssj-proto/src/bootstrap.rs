//! The line that `ssj-server` prints for the client, and safe handling of other remote text.
//!
//! The output of the remote side also contains banners and output of shell startup files, which
//! are not trusted. Only an exact, complete line is accepted.

/// Start of the line that carries the connection data.
pub const PREFIX: &str = "SSJ CONNECT ";
/// The client stops reading the output of `ssh` after this many bytes.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// Port and certificate fingerprint of a running server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Connect {
    pub port: u16,
    pub fingerprint: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapError {
    /// A line starts with the prefix but has wrong fields.
    Malformed,
    /// More than one line starts with the prefix.
    Ambiguous,
}

/// Formats the line, including the final newline.
pub fn format_connect(connect: &Connect) -> String {
    let mut line = format!("{PREFIX}{} ", connect.port);
    for byte in connect.fingerprint {
        line.push_str(&format!("{byte:02x}"));
    }
    line.push('\n');
    line
}

/// Looks for the connection line in the output read so far.
///
/// Returns `Ok(None)` while no complete line carries the prefix. Only lines that end with a
/// newline count, and the prefix must be at the very start of the line.
pub fn parse_connect(output: &[u8]) -> Result<Option<Connect>, BootstrapError> {
    let mut found = None;
    let mut lines = output.split(|&b| b == b'\n').peekable();
    while let Some(line) = lines.next() {
        // the last segment has no newline behind it, so it is not complete yet
        if lines.peek().is_none() {
            break;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(fields) = line.strip_prefix(PREFIX.as_bytes()) else {
            continue;
        };
        if found.replace(parse_fields(fields)?).is_some() {
            return Err(BootstrapError::Ambiguous);
        }
    }
    Ok(found)
}

fn parse_fields(fields: &[u8]) -> Result<Connect, BootstrapError> {
    let mut parts = fields.split(|&b| b == b' ');
    let (Some(port), Some(fingerprint), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(BootstrapError::Malformed);
    };
    Ok(Connect {
        port: parse_port(port)?,
        fingerprint: parse_fingerprint(fingerprint)?,
    })
}

fn parse_port(text: &[u8]) -> Result<u16, BootstrapError> {
    let plain =
        (1..=5).contains(&text.len()) && text.iter().all(u8::is_ascii_digit) && text[0] != b'0';
    if !plain {
        return Err(BootstrapError::Malformed);
    }
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.parse::<u16>().ok())
        .ok_or(BootstrapError::Malformed)
}

fn parse_fingerprint(text: &[u8]) -> Result<[u8; 32], BootstrapError> {
    if text.len() != 64 {
        return Err(BootstrapError::Malformed);
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(BootstrapError::Malformed),
    };
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(text.chunks(2)) {
        *byte = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Ok(out)
}

/// Makes remote text safe to show on a terminal.
///
/// Control characters (including escape sequences) and bidirectional formatting characters are
/// replaced by `?`. The result has at most `max_chars` characters.
pub fn sanitize(text: &[u8], max_chars: usize) -> String {
    let lossy = String::from_utf8_lossy(text);
    let mut out = String::new();
    for (count, c) in lossy.chars().enumerate() {
        if count == max_chars {
            out.push_str("...");
            break;
        }
        let unsafe_char = c.is_control()
            || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        out.push(if unsafe_char { '?' } else { c });
    }
    out
}
