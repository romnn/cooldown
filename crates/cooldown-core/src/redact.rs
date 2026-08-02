//! Redaction helpers for user-visible package-manager source identities.

/// Removes URL user-info credentials from every authority embedded in `value`.
///
/// Cargo lock entries can wrap a URL inside a larger qualified dependency string, so this scans
/// embedded `scheme://authority` segments instead of requiring the whole value to parse as one
/// URL. Repository paths, query parameters, and fragments remain available for attribution.
#[must_use]
pub fn url_credentials(value: &str) -> String {
    let mut remaining = value;
    let mut redacted = String::with_capacity(value.len());
    while let Some(scheme) = remaining.find("://") {
        let authority_start = scheme + 3;
        let Some(after_scheme) = remaining.get(authority_start..) else {
            break;
        };
        let authority_end = after_scheme
            .char_indices()
            .find_map(|(index, character)| {
                (character.is_whitespace()
                    || matches!(character, '/' | '?' | '#' | ')' | '"' | '\''))
                .then_some(index)
            })
            .unwrap_or(after_scheme.len());
        let authority = after_scheme.get(..authority_end).unwrap_or(after_scheme);
        let keep_from = authority
            .rfind('@')
            .and_then(|index| index.checked_add(1))
            .unwrap_or(0);
        redacted.push_str(remaining.get(..authority_start).unwrap_or(remaining));
        redacted.push_str(authority.get(keep_from..).unwrap_or(authority));
        remaining = after_scheme.get(authority_end..).unwrap_or_default();
    }
    redacted.push_str(remaining);
    redacted
}

#[cfg(test)]
mod tests {
    use super::url_credentials;

    #[test]
    fn redacts_embedded_url_authorities_without_losing_attribution() {
        let value = "foo 1.0.0 (git+https://user:token@example.com/repo?branch=next#abcdef)";

        assert_eq!(
            url_credentials(value),
            "foo 1.0.0 (git+https://example.com/repo?branch=next#abcdef)"
        );
    }
}
