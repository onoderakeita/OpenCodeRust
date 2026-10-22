//! ECMAScript URI encoding helpers.
//!
//! These mirror the `encodeURI`, `encodeURIComponent`, `decodeURI`, and
//! `decodeURIComponent` globals. The interpreter only stores valid UTF-8, so
//! lone surrogates cannot occur and the only malformed-input case is a broken
//! percent escape or a byte sequence that is not valid UTF-8.

/// Returns `true` for characters that neither encoder escapes.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
        )
}

/// Returns `true` for the URI syntax characters that `encodeURI` preserves.
fn is_reserved(byte: u8) -> bool {
    matches!(
        byte,
        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
    )
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        _ => char::from(b'A' + nibble - 10),
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-encodes UTF-8 bytes, optionally preserving URI syntax characters.
pub(crate) fn encode(text: &str, preserve_reserved: bool) -> String {
    let mut output = String::with_capacity(text.len());
    for byte in text.bytes() {
        if is_unreserved(byte) || (preserve_reserved && is_reserved(byte)) {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(hex_digit(byte >> 4));
            output.push(hex_digit(byte & 0x0f));
        }
    }
    output
}

/// Decodes percent escapes as strict UTF-8. When `preserve_reserved` is set,
/// escaped URI syntax characters are emitted exactly as written, matching
/// `decodeURI`.
pub(crate) fn decode(text: &str, preserve_reserved: bool) -> Result<String, ()> {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] != b'%' {
            let character = text[index..].chars().next().ok_or(())?;
            output.push(character);
            index += character.len_utf8();
            continue;
        }

        let mut decoded = Vec::new();
        let mut escapes = Vec::new();
        while index < bytes.len() && bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_value(bytes[index + 1]).ok_or(())?;
            let low = hex_value(bytes[index + 2]).ok_or(())?;
            decoded.push((high << 4) | low);
            escapes.push(&text[index..index + 3]);
            index += 3;
        }

        let decoded = std::str::from_utf8(&decoded).map_err(|_| ())?;
        if !preserve_reserved {
            output.push_str(decoded);
            continue;
        }

        let mut offset = 0usize;
        for character in decoded.chars() {
            let length = character.len_utf8();
            if length == 1 && is_reserved(character as u8) {
                output.push_str(escapes[offset]);
            } else {
                output.push(character);
            }
            offset += length;
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn encodes_unreserved_and_reserved_sets() {
        assert_eq!(encode("a b&c", false), "a%20b%26c");
        assert_eq!(encode("a b&c", true), "a%20b&c");
        assert_eq!(encode("é😀", false), "%C3%A9%F0%9F%98%80");
    }

    #[test]
    fn decodes_strict_utf8_and_keeps_reserved_escapes() {
        assert_eq!(decode("%C3%A9%F0%9F%98%80", false).unwrap(), "é😀");
        assert_eq!(decode("%3F%23", false).unwrap(), "?#");
        assert_eq!(decode("%3F%23", true).unwrap(), "%3F%23");
        assert_eq!(decode("%3f", true).unwrap(), "%3f");
        assert_eq!(decode("plain", false).unwrap(), "plain");
    }

    #[test]
    fn rejects_malformed_escapes() {
        assert!(decode("%", false).is_err());
        assert!(decode("%G0", false).is_err());
        assert!(decode("%E0%A4", false).is_err());
    }
}
