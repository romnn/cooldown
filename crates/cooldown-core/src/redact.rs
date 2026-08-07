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
        // A second URL glued to the first without an intervening terminator (a comma- or
        // semicolon-joined index list) would otherwise ride inside the first URL's *path* and be
        // emitted verbatim, credentials included. Splitting at the embedded scheme keeps each URL
        // independently redacted — deliberately not by making `,`/`;` terminators, which would
        // truncate a legitimate comma-carrying query before its values were masked.
        let url_end = match embedded_scheme_start(url_tail) {
            Some(next) if next < url_end => next,
            _ => url_end,
        };
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
    // Strip credentials only within the authority: an `@` later in the path (an npm-style
    // `@scope` segment on a registry index) is path data, not a userinfo separator, and must not
    // eat the host out of the label.
    let authority = without_scheme
        .get(..authority_end(without_scheme))
        .unwrap_or(without_scheme);
    let without_credentials = authority
        .rfind('@')
        .and_then(|index| index.checked_add(1))
        .and_then(|host_start| without_scheme.get(host_start..))
        .unwrap_or(without_scheme);
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
    // The authority normally ends at the first `/`, `?`, or `#`, but a credential holding an
    // unencoded reserved character pushes its `@` past that boundary, and a naive split would
    // emit the secret verbatim as path, query, or fragment.
    // This module scans arbitrary embedded segments that need not parse as URLs, so it fails
    // closed: while the text before the boundary cannot be a plain `host[:port]` (see
    // `plausible_host_port`) and an `@` still follows anywhere in the segment, everything up to
    // that `@` is treated as spilled userinfo and dropped.
    // The loop repeats because the dropped credential may itself contain further reserved
    // characters or `@`s.
    // Deliberate limit: a digits-only run after the last `:` is presumed a port, so
    // `host:8080/path@v2` survives intact — and a digits-only password with an unencoded `/`
    // (`user:1234/x@host`) is indistinguishable from that shape and passes through unredacted.
    let mut host_start = 0;
    while let Some(tail) = after_scheme.get(host_start..) {
        let boundary = authority_end(tail);
        let segment = tail.get(..boundary).unwrap_or(tail);
        if let Some(at) = segment.rfind('@') {
            // Userinfo confined to the authority: keep only what follows its last `@`.
            if let Some(index) = host_start
                .checked_add(at)
                .and_then(|index| index.checked_add(1))
            {
                host_start = index;
            }
            break;
        }
        if plausible_host_port(segment) {
            break;
        }
        match tail.find('@') {
            Some(at) => {
                let Some(index) = host_start
                    .checked_add(at)
                    .and_then(|index| index.checked_add(1))
                else {
                    break;
                };
                host_start = index;
            }
            None => break,
        }
    }
    let kept = after_scheme.get(host_start..).unwrap_or(after_scheme);
    let boundary = authority_end(kept);
    let authority = kept.get(..boundary).unwrap_or(kept);
    let rest = kept.get(boundary..).unwrap_or_default();
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
    redacted.push_str(authority);
    redacted.push_str(path);
    if let Some(query) = query {
        redacted.push('?');
        // `;` is a recognized query-pair separator alongside `&`, so both split pairs here.
        // A git refname may legally contain `;`, which means an exotic `branch=a;b` value loses
        // its tail to masking — but without the split a `;access_token=secret` suffix would ride
        // to safety inside the provenance value, and this module fails closed on that ambiguity.
        for (index, chunk) in query.split('&').enumerate() {
            if index > 0 {
                redacted.push('&');
            }
            for (sub_index, pair) in chunk.split(';').enumerate() {
                if sub_index > 0 {
                    redacted.push(';');
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

/// The byte offset where the authority portion of `text` ends: the first `/`, `?`, or `#`, or the
/// full length when none occurs.
fn authority_end(text: &str) -> usize {
    text.find(['/', '?', '#']).unwrap_or(text.len())
}

/// Whether `segment` — the text between a scheme's `://` and the first `/`, `?`, or `#` — can be
/// read as a plain `host[:port]` authority.
///
/// Whatever follows the last `:` must be empty or digits-only (a port), except inside a bracketed
/// IPv6 literal, whose colons belong to the host.
/// A non-numeric run after the last colon (`user:pa`) cannot be a port, so such a segment must be
/// userinfo whose `@` was pushed past the boundary by an unencoded reserved character.
fn plausible_host_port(segment: &str) -> bool {
    let port_region = if let Some(bracketed) = segment.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((_, after)) => after,
            // An unclosed bracket is not a valid authority; fail closed.
            None => return false,
        }
    } else {
        segment
    };
    match port_region.rsplit_once(':') {
        None => true,
        Some((_, port)) => port.bytes().all(|byte| byte.is_ascii_digit()),
    }
}

const fn is_scheme_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
}

const fn is_url_terminator(character: char) -> bool {
    character.is_whitespace() || matches!(character, ')' | ']' | '}' | '"' | '\'' | '<' | '>')
}

/// The byte offset where a *second* URL begins inside `url` — the earliest scheme whose `://`
/// follows the first one — or `None` when the text holds a single URL. The scheme's start is found
/// by walking back over scheme characters from the embedded `://`; a non-scheme boundary character
/// must precede it, or the candidate is the first URL's own scheme.
fn embedded_scheme_start(url: &str) -> Option<usize> {
    let first = url.find("://")?;
    let offset = first.checked_add(3)?;
    let rest = url.get(offset..)?;
    let second = rest.find("://")?;
    let absolute = offset.checked_add(second)?;
    let prefix = url.get(..absolute)?;
    let start = prefix
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_scheme_character(*character))
        .last()
        .map(|(index, _)| index)?;
    (start > 0).then_some(start)
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

    /// Comma-joined URLs carry no terminator between them, so the second would otherwise ride
    /// inside the first URL's path — its authority credentials emitted verbatim. Splitting at the
    /// embedded scheme must not sacrifice comma-carrying queries, whose values are masked only
    /// while the URL stays whole.
    #[test]
    fn redacts_credentials_in_a_glued_url_list() {
        assert_eq!(
            url_secrets("https://a.example/idx,https://user:token@b.example/idx"),
            "https://a.example/idx,https://b.example/idx",
        );
        assert_eq!(
            url_secrets("https://a.example/x?list=a,b&token=secret,tail"),
            "https://a.example/x?list=REDACTED&token=REDACTED",
            "a comma inside a single URL's query still masks every value"
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

    /// A credential holding an unencoded `/` or `#` pushes its `@` past the naive authority
    /// boundary; the whole prefix must still be dropped as spilled userinfo rather than leak the
    /// secret verbatim as path or fragment.
    #[test]
    fn redacts_credentials_containing_reserved_characters() {
        assert_eq!(
            url_secrets("https://user:pa/ss@h.example/x"),
            "https://h.example/x"
        );
        assert_eq!(
            url_secrets("https://user:p#ss@h.example/"),
            "https://h.example/"
        );
    }

    /// A digits-only run after the authority's last `:` is a port and an `@` beyond the authority
    /// is ordinary path or query data, so neither shape triggers the spilled-userinfo strip.
    #[test]
    fn preserves_ports_and_at_signs_beyond_the_authority() {
        assert_eq!(
            url_secrets("https://host.example:8080/path@v2"),
            "https://host.example:8080/path@v2"
        );
        assert_eq!(
            url_secrets("https://host.example/path?rev=a@b"),
            "https://host.example/path?rev=a@b"
        );
        assert_eq!(
            url_secrets("https://host.example/path?x=a@b"),
            "https://host.example/path?x=REDACTED",
            "the host and path survive; the value falls to the standard query-masking rule, \
             not the userinfo strip"
        );
    }

    /// `user:1234/x@…` is structurally identical to the legitimate `host:port/path@…` shape, so a
    /// digits-only password with an unencoded `/` passes through — the deliberate cost of keeping
    /// real `host:port` URLs readable.
    #[test]
    fn passes_a_digits_only_password_indistinguishable_from_a_port() {
        assert_eq!(
            url_secrets("https://user:1234/x@h.example/"),
            "https://user:1234/x@h.example/"
        );
    }

    /// `;` separates query pairs alongside `&`, so a `;access_token=` suffix cannot ride to
    /// safety inside a provenance value — even though a git refname may legally contain `;`.
    #[test]
    fn splits_query_pairs_on_semicolons() {
        assert_eq!(
            url_secrets("git+https://h.example/repo?branch=main;access_token=secret"),
            "git+https://h.example/repo?branch=main;access_token=REDACTED"
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

    /// An npm-style `@scope` path segment is not a credential separator: the strip stays confined
    /// to the authority so the real host names the registry, with or without actual credentials
    /// in front of it.
    #[test]
    fn labels_scoped_registry_paths_by_host() {
        assert_eq!(
            source_label("registry+https://host.example/@scope/index"),
            "registry:host.example"
        );
        assert_eq!(
            source_label("registry+https://user:token@host.example/@scope/index"),
            "registry:host.example"
        );
    }
}
