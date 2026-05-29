use serde_json::Value;

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

/// Mask every string anywhere inside `value`. Callers invoke this only once a
/// key is already known sensitive, so the entire subtree must be masked —
/// including bare strings inside arrays (e.g. `apiKeys: ["sk-…"]`), which have
/// no key of their own and would otherwise leak through unmasked.
pub fn mask_value(value: &mut Value) {
    match value {
        Value::String(s) => *s = mask(s),
        Value::Array(arr) => arr.iter_mut().for_each(mask_value),
        Value::Object(map) => map.values_mut().for_each(mask_value),
        _ => {}
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
    fn mask_value_masks_every_string_in_subtree() {
        // Callers gate this on an already-sensitive key, so every string under
        // it is masked — including array elements, the case that used to leak.
        let mut v = json!({
            "scalar": "sk-secret-12345",
            "nested": { "GITHUB_TOKEN": "ghp_aaaaaaaa" },
            "list": ["sk-one-aaaa", "sk-two-bbbb"],
            "count": 7
        });
        mask_value(&mut v);
        assert_eq!(v["scalar"], "sk-s***");
        assert_eq!(v["nested"]["GITHUB_TOKEN"], "ghp_***");
        assert_eq!(v["list"][0], "sk-o***");
        assert_eq!(v["list"][1], "sk-t***");
        // Non-string leaves are left untouched.
        assert_eq!(v["count"], 7);
    }

    #[test]
    fn mask_value_masks_a_bare_string_array() {
        let mut v = json!(["sk-real-aaaa", "sk-real-bbbb"]);
        mask_value(&mut v);
        assert_eq!(v[0], "sk-r***");
        assert_eq!(v[1], "sk-r***");
    }
}
