/// Parse a boolean from common human-readable representations.
///
/// Returns `Some(true)` for `1`, `true`, `yes`, `on` (case-insensitive),
/// `Some(false)` for `0`, `false`, `no`, `off`, and `None` for anything else.
pub fn parse_bool_override(raw: &str) -> Option<bool> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bool_override;

    #[test]
    fn accepts_common_true_spellings() {
        for input in ["1", "true", "yes", "on", "TRUE", "Yes", " on "] {
            assert_eq!(
                parse_bool_override(input),
                Some(true),
                "expected Some(true) for {input:?}"
            );
        }
    }

    #[test]
    fn accepts_common_false_spellings() {
        for input in ["0", "false", "no", "off", "FALSE", "No", " off "] {
            assert_eq!(
                parse_bool_override(input),
                Some(false),
                "expected Some(false) for {input:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_input() {
        for input in ["", "maybe", "2", "yep", "nope", "enabled"] {
            assert_eq!(
                parse_bool_override(input),
                None,
                "expected None for {input:?}"
            );
        }
    }
}
