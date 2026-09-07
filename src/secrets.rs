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

/// Words that turn a credential noun into metadata about a credential rather
/// than credential material itself (`tokenEndpoint`, `passwordLength`,
/// `privateKeyPath`). This boundary affects doctor's Error findings, so false
/// positives must not be treated as committed secrets.
const NON_SECRET_METADATA_WORDS: &[&str] = &[
    "algorithm",
    "budget",
    "command",
    "count",
    "duration",
    "endpoint",
    "env",
    "expiry",
    "file",
    "format",
    "header",
    "id",
    "identifier",
    "length",
    "limit",
    "method",
    "mode",
    "name",
    "path",
    "port",
    "prefix",
    "prompt",
    "provider",
    "scope",
    "source",
    "ttl",
    "type",
    "url",
    "var",
    "variable",
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
        Value::String(s) if !is_env_expansion(s) => *s = mask(s),
        Value::String(_) => {}
        Value::Array(arr) => arr.iter_mut().for_each(mask_value),
        Value::Object(map) => map.values_mut().for_each(mask_value),
        Value::Number(_) | Value::Bool(_) => *value = Value::String("***".into()),
        Value::Null => {}
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
            Some((k, v)) if !v.is_empty() && key_looks_sensitive(&percent_decode(k)))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn authorization_value_ranges(s: &str) -> Vec<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let marker = "authorization:";
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(marker) {
        let marker_start = cursor + relative;
        let mut start = marker_start + marker.len();
        while s.as_bytes().get(start).is_some_and(u8::is_ascii_whitespace) {
            start += 1;
        }
        for scheme in ["bearer", "basic"] {
            if lower[start..].starts_with(scheme) {
                start += scheme.len();
                while s.as_bytes().get(start).is_some_and(u8::is_ascii_whitespace) {
                    start += 1;
                }
                break;
            }
        }
        let end = s[start..]
            .char_indices()
            .find_map(|(offset, character)| {
                (character.is_whitespace() || matches!(character, '\'' | '"' | ';' | ','))
                    .then_some(start + offset)
            })
            .unwrap_or(s.len());
        if end > start && !is_env_expansion(&s[start..end]) {
            ranges.push((start, end));
        }
        cursor = (marker_start + marker.len()).max(end);
    }
    ranges
}

/// Whether a string is purely a `${VAR}` environment reference. A
/// `${VAR:-default}` ships whatever the default holds, so it is still scanned.
pub fn is_env_expansion(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    !inner.is_empty()
        && inner
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether an argv token names an option whose next token is a credential.
///
/// Split argv is a stronger claim than key-name masking: treating broad words
/// such as `auth`, `credential`, or `session` as proof would hide innocent
/// values such as `--auth oauth` and `--credentials-file path`. Restrict this
/// boundary to option names that specifically identify secret material.
pub fn argument_expects_secret(argument: &str) -> bool {
    let argument = argument.trim_matches(['\'', '"']);
    if argument == "--" || argument.contains('=') || !argument.starts_with('-') {
        return false;
    }
    let name = argument.trim_start_matches('-');
    option_name_expects_secret(name)
}

fn option_name_expects_secret(name: &str) -> bool {
    let words = split_words(name);
    if words
        .iter()
        .any(|word| NON_SECRET_METADATA_WORDS.contains(&word.as_str()))
    {
        return false;
    }
    words.iter().any(|word| {
        matches!(
            word.as_str(),
            "token" | "secret" | "password" | "passwd" | "apikey" | "bearer" | "cookie"
        )
    }) || words
        .windows(2)
        .any(|pair| pair == ["api", "key"] || pair == ["api", "keys"])
        || words
            .windows(2)
            .any(|pair| pair == ["private", "key"] || pair == ["private", "keys"])
}

/// Whether a structured key specifically names secret material rather than a
/// broader authentication/session object whose descendants need independent
/// inspection.
pub fn key_expects_secret_value(name: &str) -> bool {
    let words = split_words(name);
    if words
        .iter()
        .any(|word| NON_SECRET_METADATA_WORDS.contains(&word.as_str()))
    {
        return false;
    }
    words.iter().any(|word| {
        matches!(
            word.as_str(),
            "token"
                | "secret"
                | "password"
                | "passwd"
                | "apikey"
                | "credential"
                | "credentials"
                | "bearer"
                | "cookie"
        )
    }) || words
        .windows(2)
        .any(|pair| pair == ["api", "key"] || pair == ["api", "keys"])
        || words
            .windows(2)
            .any(|pair| pair == ["private", "key"] || pair == ["private", "keys"])
}

/// Return the credential part of `--token=value` or `TOKEN=value`.
pub fn sensitive_argument_value(argument: &str) -> Option<&str> {
    let argument = argument.trim_matches(['\'', '"']);
    let (name, value) = argument.split_once('=')?;
    let option = name.starts_with('-');
    let name = name.trim_start_matches('-');
    let env_name = !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    let sensitive_name = if option {
        option_name_expects_secret(name)
    } else {
        env_name && key_expects_secret_value(name)
    };
    (!value.is_empty() && sensitive_name).then_some(value)
}

/// Whether a string value itself looks like a credential, regardless of the
/// key it sits under.
pub fn value_looks_sensitive(s: &str) -> bool {
    is_private_key_block(s)
        || url_ranges(s).iter().any(|(start, end)| {
            url_has_password(&s[*start..*end]) || url_has_sensitive_query(&s[*start..*end])
        })
        || !authorization_value_ranges(s).is_empty()
        || sensitive_argument_value(s).is_some()
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
fn mask_single_url(url: &str) -> String {
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
                    if !v.is_empty()
                        && (key_looks_sensitive(&percent_decode(k))
                            || value_looks_sensitive(v)) =>
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

fn url_ranges(value: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].find("://") {
        let marker = cursor + relative;
        let start = value[..marker]
            .char_indices()
            .rev()
            .take_while(|(_, character)| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
            .last()
            .map_or(marker, |(index, _)| index);
        let end = value[marker + 3..]
            .char_indices()
            .find_map(|(offset, character)| {
                (character.is_whitespace() || matches!(character, '\'' | '"' | '<' | '>'))
                    .then_some(marker + 3 + offset)
            })
            .unwrap_or(value.len());
        if start < marker && end > marker + 3 {
            ranges.push((start, end));
        }
        cursor = end.max(marker + 3);
    }
    ranges
}

fn mask_urls(value: &str) -> String {
    let mut masked = value.to_string();
    for (start, end) in url_ranges(value).into_iter().rev() {
        masked.replace_range(start..end, &mask_single_url(&value[start..end]));
    }
    masked
}

/// Mask credential-shaped parts embedded in free-form text — hook commands,
/// MCP URLs — keeping everything else readable. Returns the input unchanged
/// when nothing matches.
pub fn mask_embedded(s: &str) -> String {
    let mut masked = mask_urls(s);
    let mut ranges = argument_value_ranges(&masked);
    ranges.extend(authorization_value_ranges(&masked));
    for (start, end) in merged_ranges(ranges).into_iter().rev() {
        let replacement = mask(&masked[start..end]);
        masked.replace_range(start..end, &replacement);
    }
    mask_inline(&masked)
}

/// Mask credentials in prose without treating adjacent words as shell
/// arguments. Structured command and argv fields should use `mask_embedded`;
/// diagnostics use this narrower form so text such as "for --token with" does
/// not cause the word after the option name to disappear.
pub fn mask_free_text(s: &str) -> String {
    let mut masked = mask_urls(s);
    let mut ranges = inline_argument_value_ranges(&masked);
    ranges.extend(authorization_value_ranges(&masked));
    for (start, end) in merged_ranges(ranges).into_iter().rev() {
        let replacement = mask(&masked[start..end]);
        masked.replace_range(start..end, &replacement);
    }
    mask_inline(&masked)
}

fn merged_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn inline_argument_value_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, character) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                let token = s[start..index].trim_matches(['\'', '"']);
                if let Some(value) = sensitive_argument_value(token) {
                    let offset = s[start..index].find(value).unwrap_or(0);
                    ranges.push((start + offset, start + offset + value.len()));
                }
            }
        } else {
            start.get_or_insert(index);
        }
    }
    ranges
}

fn argument_value_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, character) in s.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                tokens.push((start, index));
            }
        } else {
            start.get_or_insert(index);
        }
    }
    if let Some(start) = start {
        tokens.push((start, s.len()));
    }

    let mut ranges = Vec::new();
    for (index, &(start, end)) in tokens.iter().enumerate() {
        let token = s[start..end].trim_matches(['\'', '"']);
        if let Some(value) = sensitive_argument_value(token) {
            let offset = s[start..end].find(value).unwrap_or(0);
            ranges.push((start + offset, start + offset + value.len()));
        } else if argument_expects_secret(token)
            && let Some(&(next_start, next_end)) = tokens.get(index + 1)
        {
            let next = &s[next_start..next_end];
            let leading = next.len() - next.trim_start_matches(['\'', '"']).len();
            let trailing = next.len() - next.trim_end_matches(['\'', '"']).len();
            let unquoted = &next[leading..next.len().saturating_sub(trailing)];
            if !is_env_expansion(unquoted)
                && next_start + leading < next_end.saturating_sub(trailing)
            {
                ranges.push((next_start + leading, next_end - trailing));
            }
        }
    }
    ranges
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
        Value::Array(arr) => {
            let mut mask_next = false;
            for item in arr {
                if mask_next {
                    if !item.as_str().is_some_and(is_env_expansion) {
                        mask_value(item);
                    }
                    mask_next = false;
                    continue;
                }
                if let Value::String(argument) = item {
                    mask_next = argument_expects_secret(argument);
                }
                mask_sensitive_values(item);
            }
        }
        Value::Object(map) => map.values_mut().for_each(mask_sensitive_values),
        _ => {}
    }
}

/// Apply both key-shaped and value-shaped masking to an arbitrary serialized
/// tree. This is the final JSON-output safety net for free-form report fields.
pub fn mask_tree(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key_looks_sensitive(key) {
                    mask_value(child);
                } else {
                    mask_tree(child);
                }
            }
        }
        Value::Array(arr) => {
            let mut mask_next = false;
            for item in arr {
                if mask_next {
                    if !item.as_str().is_some_and(is_env_expansion) {
                        mask_value(item);
                    }
                    mask_next = false;
                    continue;
                }
                if let Value::String(argument) = item {
                    mask_next = argument_expects_secret(argument);
                }
                mask_tree(item);
            }
        }
        Value::String(s) => {
            if value_looks_sensitive(s) {
                *s = mask_free_text(s);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
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
        assert!(value_looks_sensitive(
            "https://example.com/path?api%5Fkey=abc123"
        ));
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
    fn mask_embedded_masks_argument_and_authorization_context() {
        assert_eq!(
            mask_embedded("cmd --token hunter2 ok"),
            "cmd --token hunt*** ok"
        );
        assert_eq!(
            mask_embedded("cmd --api-key=short"),
            "cmd --api-key=shor***"
        );
        assert_eq!(
            mask_embedded("Authorization: bearer opaquevalue"),
            "Authorization: bearer opaq***"
        );
        assert_eq!(
            mask_embedded("Authorization: bearer first-secret, Authorization: Basic second-secret"),
            "Authorization: bearer firs***, Authorization: Basic seco***"
        );
        assert_eq!(
            mask_embedded("Authorization: Bearer --token=overlapping-secret"),
            "Authorization: Bearer --to***"
        );
    }

    #[test]
    fn free_text_does_not_treat_prose_as_argv() {
        assert_eq!(
            mask_free_text(
                "value for --token with a masked credential ghp_AAAA1111bbbb2222cccc3333dddd4444"
            ),
            "value for --token with a masked credential ghp_***"
        );
        assert_eq!(
            mask_free_text("use --token=plain-secret"),
            "use --token=plai***"
        );
        assert_eq!(
            mask_free_text("Authorization: Bearer --token=overlapping-secret"),
            "Authorization: Bearer --to***"
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
            mask_embedded(
                "curl https://example.test/ok https://user:second-secret@example.test/private"
            ),
            "curl https://example.test/ok https://***@example.test/private"
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
    fn mask_tree_honors_sensitive_keys_inside_arrays() {
        let mut value = json!({
            "items": [
                {"password": "hunter2", "nested": [{"api_key": 12345}]},
                "ordinary"
            ]
        });

        mask_tree(&mut value);

        assert_eq!(value["items"][0]["password"], "hunt***");
        assert_eq!(value["items"][0]["nested"][0]["api_key"], "***");
        assert_eq!(value["items"][1], "ordinary");
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
        // A non-string leaf under a sensitive key is hidden as well.
        assert_eq!(v["count"], "***");
    }

    #[test]
    fn mask_value_masks_a_bare_string_array() {
        let mut v = json!(["sk-real-aaaa", "sk-real-bbbb"]);
        mask_value(&mut v);
        assert_eq!(v[0], "sk-r***");
        assert_eq!(v[1], "sk-r***");
    }

    #[test]
    fn mask_value_masks_non_string_sensitive_leaves() {
        let mut value = json!({"numeric": 1234, "enabled": true, "unset": null});
        mask_value(&mut value);
        assert_eq!(value["numeric"], "***");
        assert_eq!(value["enabled"], "***");
        assert!(value["unset"].is_null());
    }

    #[test]
    fn mask_sensitive_values_understands_split_argv() {
        let mut value = json!(["cmd", "--token", "hunter2", "--mode", "safe"]);
        mask_sensitive_values(&mut value);
        assert_eq!(value[2], "hunt***");
        assert_eq!(value[4], "safe");
    }

    #[test]
    fn split_argv_does_not_mask_environment_references_or_option_metadata() {
        let mut value = json!([
            "cmd",
            "--token",
            "${SERVICE_TOKEN}",
            "--auth",
            "oauth",
            "--credentials-file",
            "/tmp/credentials.json",
            "--session-name",
            "work"
        ]);

        mask_tree(&mut value);

        assert_eq!(value[2], "${SERVICE_TOKEN}");
        assert_eq!(value[4], "oauth");
        assert_eq!(value[6], "/tmp/credentials.json");
        assert_eq!(value[8], "work");
        assert_eq!(
            mask_embedded(
                "cmd --token ${SERVICE_TOKEN} --auth oauth --credentials-file=/tmp/key.json"
            ),
            "cmd --token ${SERVICE_TOKEN} --auth oauth --credentials-file=/tmp/key.json"
        );
    }
}
