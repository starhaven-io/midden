/// Escape terminal control characters at the final human-output edge. JSON
/// serialization has its own escaping and must not use this display form.
pub fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() || is_bidi_control(character) {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_controls_but_keeps_plain_text() {
        assert_eq!(escape("plain text"), "plain text");
        assert_eq!(
            escape("a\x1b]8;;evil\x07b\r\nc\x08"),
            "a\\u{1b}]8;;evil\\u{7}b\\r\\nc\\u{8}"
        );
        assert_eq!(escape("safe café"), "safe café");
        assert_eq!(escape("left\u{202e}right"), "left\\u{202e}right");
    }
}
