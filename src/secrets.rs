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
    if SUSPECT_SUBSTRINGS.iter().any(|n| lower.contains(n)) {
        return true;
    }
    // Bare "key" only counts as a whole word — `OPENAI_KEY`, `sshKey`,
    // `api-keys` — so `keybindings` and `monkey` stay unmasked. A substring
    // match would over-trigger; no match at all misses real credentials.
    // "public" exempts the one kind of key that is meant to be shared.
    let words = split_words(name);
    words.iter().any(|w| w == "key" || w == "keys") && !words.iter().any(|w| w == "public")
}

/// Lowercased word segments of a key name, split on non-alphanumerics and
/// lower-to-upper camelCase boundaries.
fn split_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
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

// -- value-shaped detection ----------------------------------------------------
//
// Key names only catch secrets stored under honestly-named keys. Tokens also
// hide in `args` arrays, env vars like DATABASE_URL, and MCP server URLs, so
// values are additionally judged by their own shape. Detection is
// deliberately prefix/structure-based rather than entropy-based: doctor turns
// these into Error findings, and entropy heuristics would condemn every
// commit SHA and UUID in sight.

/// Known credential prefixes. A long single token starting with one of these
/// (and showing the digit/uppercase mix real keys have) is treated as a
/// secret regardless of the key it sits under.
const VALUE_PREFIXES: &[&str] = &[
    "sk-",         // OpenAI/Anthropic-style API keys (sk-ant-…, sk-proj-…)
    "ghp_",        // GitHub personal access token
    "gho_",        // GitHub OAuth token
    "ghu_",        // GitHub user-to-server token
    "ghs_",        // GitHub server-to-server token
    "ghr_",        // GitHub refresh token
    "github_pat_", // GitHub fine-grained PAT
    "glpat-",      // GitLab personal access token
    "xoxb-",       // Slack bot token
    "xoxp-",       // Slack user token
    "xoxa-",       // Slack app token (legacy)
    "xoxs-",       // Slack session token
    "xapp-",       // Slack app-level token
    "npm_",        // npm access token
    "AIza",        // Google API key
];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=')
}

/// Byte ranges of credential-shaped token runs inside `s`.
fn token_runs(s: &str) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if is_token_char(c) {
            start.get_or_insert(i);
        } else if let Some(st) = start.take() {
            runs.push((st, i));
        }
    }
    if let Some(st) = start {
        runs.push((st, s.len()));
    }
    runs.retain(|&(st, en)| run_is_credential(s, st, en));
    runs
}

fn run_is_credential(s: &str, start: usize, end: usize) -> bool {
    let run = &s[start..end];
    // Real keys mix in digits or uppercase; an all-lowercase kebab slug that
    // happens to start with "sk-" does not.
    let strong = |t: &str| {
        t.chars()
            .any(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
    };

    // JWT: three non-empty dot-separated base64url segments.
    if run.starts_with("eyJ")
        && run.len() >= 40
        && run.split('.').count() == 3
        && !run.split('.').any(str::is_empty)
    {
        return true;
    }
    // AWS access key ids are exactly AKIA/ASIA + 16 uppercase alphanumerics —
    // anything looser flags prose like "ASIA-PACIFIC-…".
    if (run.starts_with("AKIA") || run.starts_with("ASIA"))
        && run.len() == 20
        && run[4..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return true;
    }
    if run.len() >= 20
        && let Some(p) = VALUE_PREFIXES.iter().find(|p| run.starts_with(**p))
    {
        return strong(&run[p.len()..]);
    }
    // An opaque token following a Bearer marker.
    if run.len() >= 16 && strong(run) {
        return s[..start]
            .trim_end()
            .to_ascii_lowercase()
            .ends_with("bearer");
    }
    false
}

fn is_private_key_block(s: &str) -> bool {
    s.contains("-----BEGIN") && s.contains("PRIVATE KEY")
}

/// `scheme://user:pass@host` — the password is a committed credential even
/// though the key it sits under (DATABASE_URL, …) looks innocent.
fn url_has_password(s: &str) -> bool {
    let Some(scheme_end) = s.find("://") else {
        return false;
    };
    let rest = &s[scheme_end + 3..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    authority
        .rfind('@')
        .is_some_and(|at| authority[..at].contains(':'))
}

/// A URL whose query string carries a credential-named parameter with a value
/// (`?api_key=…`, `&token=…`).
fn url_has_sensitive_query(s: &str) -> bool {
    if !s.contains("://") {
        return false;
    }
    let Some(q) = s.find('?') else {
        return false;
    };
    let query = s[q + 1..].split('#').next().unwrap_or("");
    query.split('&').any(|pair| {
        matches!(pair.split_once('='),
            Some((k, v)) if !v.is_empty() && key_looks_sensitive(k))
    })
}

/// Whether a string value itself looks like a credential, regardless of the
/// key it sits under.
pub fn value_looks_sensitive(s: &str) -> bool {
    is_private_key_block(s)
        || url_has_password(s)
        || url_has_sensitive_query(s)
        || !token_runs(s).is_empty()
}

/// Mask credential-shaped token runs inside free-form text, leaving the
/// surrounding context readable. A private-key block is masked wholesale.
fn mask_inline(s: &str) -> String {
    if is_private_key_block(s) {
        return "***".into();
    }
    let runs = token_runs(s);
    if runs.is_empty() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (st, en) in runs {
        out.push_str(&s[last..st]);
        out.push_str(&mask(&s[st..en]));
        last = en;
    }
    out.push_str(&s[last..]);
    out
}

/// Mask the credential-bearing parts of a URL: any `user:pass` userinfo and
/// the values of credential-named (or token-shaped) query parameters.
fn mask_url(url: &str) -> String {
    let mut out = url.to_string();
    if let Some(scheme_end) = out.find("://") {
        let auth_start = scheme_end + 3;
        let rest = &out[auth_start..];
        let end = rest
            .find(['/', '?', '#'])
            .map(|i| auth_start + i)
            .unwrap_or(out.len());
        if let Some(at_rel) = out[auth_start..end].rfind('@') {
            let at = auth_start + at_rel;
            if out[auth_start..at].contains(':') {
                out.replace_range(auth_start..at, "***");
            }
        }
    }
    if let Some(q) = out.find('?') {
        let (head, tail) = out.split_at(q + 1);
        let (query, frag) = match tail.split_once('#') {
            Some((query, frag)) => (query, Some(frag)),
            None => (tail, None),
        };
        let masked: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, v))
                    if !v.is_empty() && (key_looks_sensitive(k) || value_looks_sensitive(v)) =>
                {
                    format!("{k}={}", mask(v))
                }
                _ => pair.to_string(),
            })
            .collect();
        let rebuilt = match frag {
            Some(frag) => format!("{head}{}#{frag}", masked.join("&")),
            None => format!("{head}{}", masked.join("&")),
        };
        out = rebuilt;
    }
    out
}

/// Mask credential-shaped parts embedded in free-form text — hook commands,
/// MCP URLs — keeping everything else readable. Returns the input unchanged
/// when nothing matches.
pub fn mask_embedded(s: &str) -> String {
    mask_inline(&mask_url(s))
}

/// Display form of a value already judged sensitive: mask just the
/// credential-shaped parts, and fall back to masking the whole value when
/// nothing matches by shape (e.g. flagged by key name alone).
pub fn masked_for_display(s: &str) -> String {
    let embedded = mask_embedded(s);
    if embedded == s { mask(s) } else { embedded }
}

/// Mask any string anywhere inside `value` whose *content* looks like a
/// credential. Complements `mask_value`, which masks by key name: this pass
/// catches tokens under innocent keys (`args`, `env.DATABASE_URL`, URLs).
pub fn mask_sensitive_values(value: &mut Value) {
    match value {
        Value::String(s) => {
            if value_looks_sensitive(s) {
                *s = masked_for_display(s);
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(mask_sensitive_values),
        Value::Object(map) => map.values_mut().for_each(mask_sensitive_values),
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
    fn key_matches_bare_key_as_a_word_only() {
        assert!(key_looks_sensitive("OPENAI_KEY"));
        assert!(key_looks_sensitive("sshKey"));
        assert!(key_looks_sensitive("api-keys"));
        assert!(!key_looks_sensitive("keybindings"));
        assert!(!key_looks_sensitive("monkey"));
        assert!(!key_looks_sensitive("hotkeys"), "hotkeys is one word");
        assert!(
            !key_looks_sensitive("publicKey"),
            "public keys are shareable"
        );
    }

    /// Joins a credential prefix to a dummy body at runtime, so the source
    /// never holds a contiguous scanner-matching literal — GitHub secret
    /// scanning flagged the PEM fixture when it appeared verbatim.
    fn fake(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    #[test]
    fn value_detection_catches_known_token_shapes() {
        assert!(value_looks_sensitive(&fake(
            "sk-ant-api03-",
            "AbCd1234efGh5678ijKl"
        )));
        assert!(value_looks_sensitive(&fake(
            "ghp_",
            "AAAAbbbb1111cccc2222dddd3333eeee"
        )));
        assert!(value_looks_sensitive(&fake(
            "github_pat_",
            "11ABCDE0aaaabbbbccccdd"
        )));
        assert!(value_looks_sensitive(&fake(
            "xoxb-",
            "1234567890-abcdefABCDEF"
        )));
        assert!(
            value_looks_sensitive("AKIAIOSFODNN7EXAMPLE"),
            "AWS's documented example key id"
        );
        // JWT: header.payload.signature.
        let jwt = [
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxMjM0In0",
            "SflKxwRJSMeKKF2QT4fwpM",
        ]
        .join(".");
        assert!(value_looks_sensitive(&jwt));
        let pem = format!(
            "-----BEGIN OPENSSH {k}-----\nb3BlbnNzaA==\n-----END OPENSSH {k}-----",
            k = "PRIVATE KEY"
        );
        assert!(value_looks_sensitive(&pem));
    }

    #[test]
    fn value_detection_catches_url_credentials() {
        assert!(value_looks_sensitive(
            "postgres://app:hunter2@db.example.com/prod"
        ));
        assert!(value_looks_sensitive(
            "https://mcp.example.com/sse?api_key=abc123"
        ));
        assert!(!value_looks_sensitive("https://example.com/path?page=2"));
        assert!(
            !value_looks_sensitive("ssh://git@github.com/x.git"),
            "username alone is not a credential"
        );
    }

    #[test]
    fn value_detection_avoids_lookalikes() {
        assert!(
            !value_looks_sensitive("sk-learn-compatible-models"),
            "all-lowercase slug"
        );
        assert!(!value_looks_sensitive("ghp_short"));
        assert!(
            !value_looks_sensitive("${GITHUB_TOKEN}"),
            "env expansion, no content"
        );
        assert!(
            !value_looks_sensitive("ASIA-PACIFIC-DEPLOY-2024"),
            "prose, not an AWS key id"
        );
        assert!(!value_looks_sensitive("node_modules/.bin/eslint"));
        assert!(!value_looks_sensitive("plain words with spaces"));
        assert!(!value_looks_sensitive("Read(./.env)"));
    }

    #[test]
    fn mask_embedded_masks_tokens_in_context() {
        let cmd = "curl -H 'Authorization: Bearer SECRETtoken1234567890' https://api.example.com";
        let masked = mask_embedded(cmd);
        assert!(!masked.contains("SECRETtoken1234567890"), "{masked}");
        assert!(masked.contains("SECR***"), "{masked}");
        assert!(
            masked.contains("https://api.example.com"),
            "context survives: {masked}"
        );
    }

    #[test]
    fn mask_embedded_masks_url_parts() {
        let url = "https://user:s3cretpass@host.example.com/sse?token=abcd1234&page=2";
        let masked = mask_embedded(url);
        assert!(!masked.contains("s3cretpass"), "{masked}");
        assert!(!masked.contains("abcd1234"), "{masked}");
        assert!(masked.contains("token=abcd***"), "{masked}");
        assert!(
            masked.contains("page=2"),
            "innocent params survive: {masked}"
        );
        assert_eq!(
            mask_embedded("echo done"),
            "echo done",
            "clean text untouched"
        );
    }

    #[test]
    fn masked_for_display_falls_back_to_whole_value() {
        // Flagged by key name, no recognizable shape: mask everything.
        assert_eq!(masked_for_display("hunter2"), "hunt***");
        // Shaped: keep the recognizable prefix only.
        assert_eq!(
            masked_for_display(&fake("ghp_", "AAAAbbbb1111cccc2222dddd3333eeee")),
            "ghp_***"
        );
    }

    #[test]
    fn mask_sensitive_values_masks_by_content_only() {
        let mut v = json!({
            "args": ["--token", fake("ghp_", "AAAAbbbb1111cccc2222dddd3333eeee")],
            "env": { "DATABASE_URL": "postgres://app:hunter2@db/prod" },
            "name": "left-alone",
        });
        mask_sensitive_values(&mut v);
        assert_eq!(v["args"][0], "--token");
        assert_eq!(v["args"][1], "ghp_***");
        assert_eq!(v["env"]["DATABASE_URL"], "postgres://***@db/prod");
        assert_eq!(v["name"], "left-alone");
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
