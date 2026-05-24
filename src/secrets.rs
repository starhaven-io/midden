use serde_json::{Map, Value};

/// Keys whose values are masked unless `--show-secrets` is set.
///
/// Match is case-insensitive *substring*. We err on the side of more masking:
/// false positives are harmless, false negatives leak credentials.
const SUSPECT_SUBSTRINGS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "auth",
    "credential",
    "private",
    "session",
    "bearer",
    "cookie",
];

/// Whether a key name looks like it holds a credential.
pub fn key_looks_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SUSPECT_SUBSTRINGS.iter().any(|n| lower.contains(n))
}

/// Mask a string for display: keep up to four leading chars, replace the rest
/// with `***`. Empty/short strings collapse to `***`.
pub fn mask(s: &str) -> String {
    let visible = s.chars().take(4).collect::<String>();
    if visible.is_empty() || visible.len() == s.len() {
        "***".into()
    } else {
        format!("{visible}***")
    }
}

/// Walk a JSON value and mask any string under a key that looks sensitive.
/// Non-string sensitive values (e.g. `{"token": {"value": "..."}}`) get their
/// strings masked recursively under the suspect subtree.
pub fn mask_value(value: &mut Value) {
    mask_inner(value, false);
}

fn mask_inner(value: &mut Value, mask_all: bool) {
    match value {
        Value::Object(map) => mask_object(map, mask_all),
        Value::Array(arr) => {
            for v in arr {
                mask_inner(v, mask_all);
            }
        }
        Value::String(s) if mask_all => {
            *s = mask(s);
        }
        _ => {}
    }
}

fn mask_object(map: &mut Map<String, Value>, mask_all: bool) {
    for (k, v) in map.iter_mut() {
        let sensitive = mask_all || key_looks_sensitive(k);
        mask_inner(v, sensitive);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mask_short_strings() {
        assert_eq!(mask(""), "***");
        assert_eq!(mask("abc"), "***");
        assert_eq!(mask("abcd"), "***");
        assert_eq!(mask("abcde"), "abcd***");
        assert_eq!(mask("sk-very-long-token-here"), "sk-v***");
    }

    #[test]
    fn key_looks_sensitive_examples() {
        assert!(key_looks_sensitive("token"));
        assert!(key_looks_sensitive("ANTHROPIC_API_KEY"));
        assert!(key_looks_sensitive("github_token"));
        assert!(key_looks_sensitive("password"));
        assert!(!key_looks_sensitive("name"));
        assert!(!key_looks_sensitive("path"));
    }

    #[test]
    fn mask_value_masks_strings_under_suspect_keys() {
        let mut v = json!({
            "name": "fine",
            "apiKey": "sk-secret-12345",
            "nested": { "GITHUB_TOKEN": "ghp_aaaaaaaa" },
            "items": [{ "secret": "shhh-very-long" }]
        });
        mask_value(&mut v);
        assert_eq!(v["name"], "fine");
        assert_eq!(v["apiKey"], "sk-s***");
        assert_eq!(v["nested"]["GITHUB_TOKEN"], "ghp_***");
        assert_eq!(v["items"][0]["secret"], "shhh***");
    }

    #[test]
    fn mask_value_recurses_into_suspect_subtree() {
        let mut v = json!({
            "credentials": { "alpha": "secret-alpha", "beta": "secret-beta" }
        });
        mask_value(&mut v);
        assert_eq!(v["credentials"]["alpha"], "secr***");
        assert_eq!(v["credentials"]["beta"], "secr***");
    }
}
