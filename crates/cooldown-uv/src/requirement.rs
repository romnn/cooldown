//! PEP 508 requirement strings split into the parts the adapter reads: the package name, its
//! extras, the version specifier, and the environment marker.

/// A PEP 508 requirement split at its syntactic seams.
///
/// Only the name is located; extras, specifier, and marker are kept verbatim so a caller can
/// print exactly what the author wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Requirement<'a> {
    /// The package name as written, not PEP 503 normalized.
    pub name: &'a str,
    /// The bracketed extras without the brackets (`torch` for `safetensors[torch]`).
    pub extras: Option<&'a str>,
    /// Everything between the name or extras and the marker, trimmed: a PEP 440 specifier set
    /// (`>=1,<2`), a URL reference (`@ https://…`), or empty for a bare name.
    pub specifier: &'a str,
    /// The environment marker after `;`, trimmed, when the author wrote one.
    pub marker: Option<&'a str>,
}

impl Requirement<'_> {
    /// Whether the specifier is a PEP 440 version constraint rather than a URL reference or
    /// nothing at all.
    pub(crate) fn has_version_specifier(&self) -> bool {
        !self.specifier.is_empty() && !self.specifier.starts_with('@')
    }
}

/// Splits a PEP 508 requirement, or `None` when no package name leads it.
pub(crate) fn parse(requirement: &str) -> Option<Requirement<'_>> {
    let (head, marker) = match requirement.split_once(';') {
        Some((head, marker)) => (head, Some(marker.trim())),
        None => (requirement, None),
    };
    let head = head.trim();
    let name_end = head
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .unwrap_or(head.len());
    let (name, after_name) = head.split_at(name_end);
    if name.is_empty() {
        return None;
    }
    let after_name = after_name.trim_start();
    let (extras, after_extras) = match after_name.strip_prefix('[') {
        Some(rest) => {
            let (inside, tail) = rest.split_once(']')?;
            (Some(inside), tail.trim_start())
        }
        None => (None, after_name),
    };
    Some(Requirement {
        name,
        extras,
        specifier: after_extras.trim(),
        marker,
    })
}

#[cfg(test)]
mod tests {
    use super::{Requirement, parse};

    #[test]
    fn splits_name_extras_specifier_and_marker() {
        // PyPI's normalized form: the specifier hugs the name and the marker follows `;`.
        assert_eq!(
            parse(r#"transformers!=5.0.*,<5.9.0,>=4.42.0; sys_platform == "darwin""#),
            Some(Requirement {
                name: "transformers",
                extras: None,
                specifier: "!=5.0.*,<5.9.0,>=4.42.0",
                marker: Some(r#"sys_platform == "darwin""#),
            })
        );
        // Extras and a spaced-out specifier survive verbatim.
        assert_eq!(
            parse("safetensors[torch] >= 0.4"),
            Some(Requirement {
                name: "safetensors",
                extras: Some("torch"),
                specifier: ">= 0.4",
                marker: None,
            })
        );
    }

    #[test]
    fn bare_names_and_url_references_carry_no_version_specifier() {
        let bare = parse("requests").expect("a bare name parses");
        assert_eq!(bare.specifier, "");
        assert!(!bare.has_version_specifier());
        let url = parse("pkg @ https://example.com/pkg.whl").expect("a URL reference parses");
        assert_eq!(url.specifier, "@ https://example.com/pkg.whl");
        assert!(!url.has_version_specifier());
        // Nothing leads with a name, so nothing parses.
        assert_eq!(parse(""), None);
        assert_eq!(parse("[extra]>=1"), None);
        assert_eq!(parse("pkg[unterminated>=1"), None);
    }
}
