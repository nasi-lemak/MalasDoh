//! Turning a remote file's bytes into editable text and back without changing
//! anything the user did not touch: line endings, a UTF-8 BOM and a missing
//! final newline all survive a round trip.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextFormat {
    pub line_ending: LineEnding,
    pub bom: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NotText {
    #[error("this looks like a binary file (it contains NUL bytes)")]
    Binary,
    #[error("this file is not valid UTF-8 (first bad byte at offset {0})")]
    NotUtf8(usize),
}

/// Decode for editing. Text is returned with `\n` line endings.
///
/// A file with mixed endings is treated as CRLF only if every newline is CRLF;
/// otherwise it is left exactly as is (lone `\r` characters stay in the text), so
/// saving never rewrites lines the user did not edit.
pub fn decode(bytes: &[u8]) -> Result<(String, TextFormat), NotText> {
    let (bom, body) = match bytes.strip_prefix(b"\xEF\xBB\xBF") {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    if body.contains(&0) {
        return Err(NotText::Binary);
    }
    let text = std::str::from_utf8(body).map_err(|e| NotText::NotUtf8(e.valid_up_to() + bom as usize * 3))?;
    let lf = text.matches('\n').count();
    let crlf = text.matches("\r\n").count();
    if lf > 0 && crlf == lf {
        Ok((text.replace("\r\n", "\n"), TextFormat { line_ending: LineEnding::CrLf, bom }))
    } else {
        Ok((text.to_string(), TextFormat { line_ending: LineEnding::Lf, bom }))
    }
}

pub fn encode(text: &str, format: &TextFormat) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 3);
    if format.bom {
        out.extend_from_slice(b"\xEF\xBB\xBF");
    }
    match format.line_ending {
        LineEnding::Lf => out.extend_from_slice(text.as_bytes()),
        LineEnding::CrLf => out.extend_from_slice(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes()),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) {
        let (text, fmt) = decode(bytes).unwrap();
        assert_eq!(encode(&text, &fmt), bytes, "round trip of {:?}", String::from_utf8_lossy(bytes));
    }

    #[test]
    fn round_trips_preserve_bytes() {
        round_trip(b"a\nb\n");
        round_trip(b"a\nb");
        round_trip(b"a\r\nb\r\n");
        round_trip(b"\xEF\xBB\xBFkey=value\r\n");
        round_trip(b"mixed\r\nends\n");
        round_trip(b"");
    }

    #[test]
    fn crlf_is_hidden_while_editing() {
        let (text, fmt) = decode(b"a\r\nb\r\n").unwrap();
        assert_eq!(text, "a\nb\n");
        assert_eq!(fmt.line_ending, LineEnding::CrLf);
        assert_eq!(encode("a\nb\nc\n", &fmt), b"a\r\nb\r\nc\r\n");
    }

    #[test]
    fn rejects_binary_and_bad_utf8() {
        assert_eq!(decode(b"\x7fELF\0\0"), Err(NotText::Binary));
        assert_eq!(decode(b"ok \xff"), Err(NotText::NotUtf8(3)));
    }
}
