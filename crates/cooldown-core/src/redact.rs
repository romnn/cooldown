//! Redaction helpers for user-visible package-manager source identities.

const REDACTED: &str = "REDACTED";

/// Removes credentials and non-provenance query or fragment values from embedded URLs.
///
/// Cargo lock entries can wrap a URL inside a larger qualified dependency string, so this scans
/// embedded `scheme://authority` segments instead of requiring the whole value to parse as one
/// URL.
/// User information and non-commit fragments are always redacted.
/// Precise Git commit fragments remain visible so source-distinct lock identities stay distinct.
/// Query values survive only for the Git provenance keys `branch`, `tag`, and `rev`.
#[must_use]
pub fn url_secrets(value: &str) -> String {
    let mut remaining = value;
    let mut redacted = String::with_capacity(value.len());
    while let Some(start) = remaining.find("://") {
        let url_start = remaining
            .get(..start)
            .and_then(|prefix| {
                prefix
                    .char_indices()
                    .rev()
                    .find(|(_, character)| !is_scheme_character(*character))
                    .map_or(Some(0), |(index, character)| {
                        index.checked_add(character.len_utf8())
                    })
            })
            .unwrap_or(0);
        let Some(url_tail) = remaining.get(url_start..) else {
            break;
        };
        let url_end = url_tail
            .char_indices()
            .find_map(|(index, character)| is_url_terminator(character).then_some(index))
            .unwrap_or(url_tail.len());
        let Some(url) = url_tail.get(..url_end) else {
            break;
        };
        redacted.push_str(remaining.get(..url_start).unwrap_or_default());
        redacted.push_str(&redact_url(url));
        remaining = url_tail.get(url_end..).unwrap_or_default();
    }
    redacted.push_str(remaining);
    redacted
}

/// Returns a compact, credential-free label for a package-manager source identity.
#[must_use]
pub fn source_label(source: &str) -> String {
    let source = url_secrets(source);
    let source = source.as_str();
    if source == "registry+https://github.com/rust-lang/crates.io-index" {
        return "crates.io".to_string();
    }
    let Some((kind, address)) = source.split_once('+') else {
        if source.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        }) {
            return source.to_string();
        }
        return format!("source:{:016x}", crate::fs::fnv1a_64(source));
    };
    if address.starts_with("file:") {
        return format!("{kind}:local");
    }
    let without_scheme = address
        .split_once("://")
        .map_or(address, |(_, location)| location);
    let without_credentials = without_scheme
        .rsplit_once('@')
        .map_or(without_scheme, |(_, location)| location);
    let location = without_credentials
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".git");
    let location = if matches!(kind, "registry" | "sparse") {
        location.split('/').next().unwrap_or_default()
    } else {
        location
    };
    if location.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}:{location}")
    }
}

fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://").and_then(|index| index.checked_add(3)) else {
        return url.to_string();
    };
    let Some(after_scheme) = url.get(scheme_end..) else {
        return url.to_string();
    };
    let authority_end = after_scheme
        .char_indices()
        .find_map(|(index, character)| matches!(character, '/' | '?' | '#').then_some(index))
        .unwrap_or(after_scheme.len());
    let authority = after_scheme.get(..authority_end).unwrap_or(after_scheme);
    let host_start = authority
        .rfind('@')
        .and_then(|index| index.checked_add(1))
        .unwrap_or(0);
    let rest = after_scheme.get(authority_end..).unwrap_or_default();
    let (without_fragment, fragment) = rest
        .split_once('#')
        .map_or((rest, None), |(prefix, value)| (prefix, Some(value)));
    let (path, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(path, query)| {
            (path, Some(query))
        });

    let mut redacted = String::with_capacity(url.len());
    redacted.push_str(url.get(..scheme_end).unwrap_or_default());
    redacted.push_str(authority.get(host_start..).unwrap_or(authority));
    redacted.push_str(path);
    if let Some(query) = query {
        redacted.push('?');
        for (index, pair) in query.split('&').enumerate() {
            if index > 0 {
                redacted.push('&');
            }
            let Some((key, value)) = pair.split_once('=') else {
                if provenance_query_key(pair) {
                    redacted.push_str(pair);
                } else {
                    redacted.push_str(REDACTED);
                }
                continue;
            };
            redacted.push_str(key);
            redacted.push('=');
            if provenance_query_key(key) {
                redacted.push_str(value);
            } else {
                redacted.push_str(REDACTED);
            }
        }
    }
    if let Some(fragment) = fragment {
        redacted.push('#');
        if is_git_commit_fragment(url, fragment) {
            redacted.push_str(fragment);
        } else {
            redacted.push_str(REDACTED);
        }
    }
    redacted
}

fn is_git_commit_fragment(url: &str, fragment: &str) -> bool {
    url.starts_with("git+")
        && matches!(fragment.len(), 40 | 64)
        && fragment.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn provenance_query_key(key: &str) -> bool {
    matches!(percent_decode_ascii(key).as_str(), "branch" | "tag" | "rev")
}

fn percent_decode_ascii(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = String::with_capacity(value.len());
    let mut index = 0;
    while let Some(byte) = bytes.get(index).copied() {
        if byte == b'%'
            && let (Some(high), Some(low)) = (bytes.get(index + 1), bytes.get(index + 2))
            && let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low))
        {
            decoded.push(char::from(high * 16 + low).to_ascii_lowercase());
            index += 3;
            continue;
        }
        decoded.push(char::from(byte).to_ascii_lowercase());
        index += 1;
    }
    decoded
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

const fn is_scheme_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
}

const fn is_url_terminator(character: char) -> bool {
    character.is_whitespace() || matches!(character, ')' | ']' | '}' | '"' | '\'' | '<' | '>')
}

#[cfg(test)]
mod tests {
    use super::{source_label, url_secrets};

    #[test]
    fn redacts_credentials_queries_and_fragments_without_losing_git_provenance() {
        let value = concat!(
            "foo (git+https://user:token@example.com/repo?",
            "branch=next&access_token=secret&%73ignature=signed#private)"
        );

        assert_eq!(
            url_secrets(value),
            concat!(
                "foo (git+https://example.com/repo?",
                "branch=next&access_token=REDACTED&%73ignature=REDACTED#REDACTED)"
            )
        );
    }

    #[test]
    fn redacts_multiple_embedded_urls() {
        let value = concat!(
            "https://one.example/index?token=first then ",
            "ssh://user@two.example/repo?rev=abcdef#second"
        );

        assert_eq!(
            url_secrets(value),
            concat!(
                "https://one.example/index?token=REDACTED then ",
                "ssh://two.example/repo?rev=abcdef#REDACTED"
            )
        );
    }

    #[test]
    fn redacts_valueless_query_tokens() {
        assert_eq!(
            url_secrets("https://example.com/index?bearer-secret&rev"),
            "https://example.com/index?REDACTED&rev"
        );
    }

    #[test]
    fn preserves_precise_git_commits_without_preserving_other_fragments() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let value = format!(
            "git+https://example.com/repo?branch=next#{commit} https://example.com/#access_token=secret"
        );

        assert_eq!(
            url_secrets(&value),
            format!(
                "git+https://example.com/repo?branch=next#{commit} https://example.com/#REDACTED"
            )
        );
    }

    #[test]
    fn abbreviates_sources_without_disclosing_credentials() {
        assert_eq!(
            source_label("registry+https://github.com/rust-lang/crates.io-index"),
            "crates.io"
        );
        assert_eq!(
            source_label("git+https://token@example.com/private/repo.git?branch=release#abcdef"),
            "git:example.com/private/repo"
        );
        assert_eq!(
            source_label("git+file:///home/user/private/repo#abcdef"),
            "git:local"
        );
        assert_eq!(
            source_label("registry+https://packages.example.com/bearer-secret/cargo/index"),
            "registry:packages.example.com"
        );
        assert_eq!(source_label("proxy.example"), "proxy.example");
    }
}
