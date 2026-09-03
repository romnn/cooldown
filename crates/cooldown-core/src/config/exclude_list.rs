//! One layer's `exclude-folders` / `exclude-packages` list, together with how it combines with
//! the same list inherited from a lower-precedence layer.
//!
//! Exclude lists layer across config files (global → repo root → `--config`) and across sections
//! (`[global]` → `[<command>]`), and a plain array *extends* what it inherits: a repo adds to an
//! org-wide global config without restating it, and a `[<command>]` table adds to `[global]`.
//! Concatenation cannot express the two things a nearer layer sometimes needs — undoing the
//! inherited entries, or starting over — so those have explicit forms:
//!
//! | Form | Effect on the inherited list |
//! |------|------------------------------|
//! | key absent | unchanged |
//! | `exclude-folders = ["a"]` | `a` is added |
//! | `exclude-folders = []` | **cleared** |
//! | `exclude-folders = { replace = ["a"] }` | **replaced** by `a` |
//! | `exclude-folders = { extend = ["a"] }` | `a` is added (the explicit spelling of the array) |
//!
//! A bare `[]` clears rather than extending by nothing: extending by nothing is a no-op no author
//! writes on purpose, whereas "no excludes here, whatever the parent says" is exactly what an empty
//! list reads as.
//! For the same reason `{ extend = [] }` is refused rather than accepted as a no-op.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// An exclude list and its merge mode against the list inherited from a lower-precedence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExcludeList {
    /// Add these patterns to the inherited list (a plain non-empty array, or `{ extend = [...] }`).
    Extend(Vec<String>),
    /// Discard the inherited list and use exactly these patterns (`{ replace = [...] }`, or a bare
    /// `[]` for an empty replacement).
    Replace(Vec<String>),
}

impl Default for ExcludeList {
    /// The absent key: nothing is added and the inherited list stands.
    fn default() -> Self {
        ExcludeList::Extend(Vec::new())
    }
}

impl ExcludeList {
    /// A list that adds `patterns` to whatever is inherited.
    #[must_use]
    pub fn extend(patterns: impl Into<Vec<String>>) -> Self {
        ExcludeList::Extend(patterns.into())
    }

    /// A list that replaces whatever is inherited with exactly `patterns`.
    #[must_use]
    pub fn replace(patterns: impl Into<Vec<String>>) -> Self {
        ExcludeList::Replace(patterns.into())
    }

    /// The patterns this list carries, whichever way it merges.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        match self {
            ExcludeList::Extend(patterns) | ExcludeList::Replace(patterns) => patterns,
        }
    }

    /// Folds the higher-precedence `layer` over `self`.
    ///
    /// A replacing layer wins outright.
    /// An extending layer appends to `self` and keeps `self`'s mode, so a replacement made at one
    /// layer still shadows everything below it after later layers add to it: `Replace(a)` then
    /// `Extend(b)` is `Replace(a, b)`.
    #[must_use]
    pub fn merge(self, layer: ExcludeList) -> ExcludeList {
        match layer {
            ExcludeList::Replace(patterns) => ExcludeList::Replace(patterns),
            ExcludeList::Extend(added) => match self {
                ExcludeList::Extend(mut base) => {
                    base.extend(added);
                    ExcludeList::Extend(base)
                }
                ExcludeList::Replace(mut base) => {
                    base.extend(added);
                    ExcludeList::Replace(base)
                }
            },
        }
    }
}

const MERGE_KEYS: &[&str] = &["replace", "extend"];

impl<'de> serde::Deserialize<'de> for ExcludeList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ExcludeListVisitor;

        impl<'de> Visitor<'de> for ExcludeListVisitor {
            type Value = ExcludeList;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "an array of glob patterns, or a table `{ replace = [...] }` / `{ extend = [...] }`",
                )
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut patterns = Vec::new();
                while let Some(pattern) = seq.next_element::<String>()? {
                    patterns.push(pattern);
                }
                Ok(if patterns.is_empty() {
                    ExcludeList::Replace(patterns)
                } else {
                    ExcludeList::Extend(patterns)
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut list: Option<ExcludeList> = None;
                while let Some(key) = map.next_key::<String>()? {
                    let build: fn(Vec<String>) -> ExcludeList = match key.as_str() {
                        "replace" => ExcludeList::Replace,
                        "extend" => ExcludeList::Extend,
                        other => return Err(de::Error::unknown_field(other, MERGE_KEYS)),
                    };
                    if list.is_some() {
                        return Err(de::Error::custom(
                            "`replace` and `extend` are mutually exclusive; set exactly one",
                        ));
                    }
                    let patterns = map.next_value::<Vec<String>>()?;
                    // Extending by nothing is a no-op, yet it sits one keyword away from the
                    // clearing forms; refuse it so a slip never silently keeps the inherited list.
                    if key == "extend" && patterns.is_empty() {
                        return Err(de::Error::custom(
                            "`{ extend = [] }` changes nothing; write `[]` or `{ replace = [] }` \
                             to clear the inherited list, or remove the key",
                        ));
                    }
                    list = Some(build(patterns));
                }
                list.ok_or_else(|| {
                    de::Error::custom("expected `replace = [...]` or `extend = [...]`")
                })
            }
        }

        deserializer.deserialize_any(ExcludeListVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::ExcludeList;

    #[derive(serde::Deserialize)]
    struct Doc {
        list: ExcludeList,
    }

    fn parse(toml: &str) -> Result<ExcludeList, toml::de::Error> {
        toml::from_str::<Doc>(toml).map(|doc| doc.list)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_plain_array_extends_and_an_empty_array_clears() {
        assert_eq!(
            parse(r#"list = ["a", "b"]"#).expect("array"),
            ExcludeList::extend(strings(&["a", "b"]))
        );
        assert_eq!(
            parse("list = []").expect("empty"),
            ExcludeList::replace(vec![])
        );
    }

    #[test]
    fn the_table_form_names_the_merge_mode() {
        assert_eq!(
            parse(r#"list = { replace = ["a"] }"#).expect("replace"),
            ExcludeList::replace(strings(&["a"]))
        );
        assert_eq!(
            parse(r#"list = { extend = ["a"] }"#).expect("extend"),
            ExcludeList::extend(strings(&["a"]))
        );
        assert_eq!(
            parse("list = { replace = [] }").expect("empty replace"),
            ExcludeList::replace(vec![])
        );
    }

    #[test]
    fn malformed_tables_are_rejected_with_the_valid_keys_named() {
        let unknown = parse(r#"list = { replac = ["a"] }"#).expect_err("typo");
        assert!(
            unknown.to_string().contains("replace") && unknown.to_string().contains("extend"),
            "the error names the accepted keys: {unknown}"
        );
        let both = parse(r#"list = { replace = ["a"], extend = ["b"] }"#).expect_err("both");
        assert!(
            both.to_string().contains("mutually exclusive"),
            "the error explains the conflict: {both}"
        );
        assert!(parse("list = {}").is_err(), "an empty table names no mode");
        assert!(parse(r#"list = "a""#).is_err(), "a scalar is not a list");
        let inert = parse("list = { extend = [] }").expect_err("extend by nothing");
        assert!(
            inert.to_string().contains("replace = []"),
            "the error names a form that clears: {inert}"
        );
    }

    #[test]
    fn merge_replaces_or_appends_and_keeps_the_base_mode() {
        let a = || ExcludeList::extend(strings(&["a"]));
        let b = || ExcludeList::extend(strings(&["b"]));
        assert_eq!(a().merge(b()), ExcludeList::extend(strings(&["a", "b"])));
        assert_eq!(
            a().merge(ExcludeList::replace(strings(&["b"]))),
            ExcludeList::replace(strings(&["b"]))
        );
        assert_eq!(
            a().merge(ExcludeList::replace(vec![])),
            ExcludeList::replace(vec![])
        );
        // A replacement stays a replacement after a later layer adds to it, so `resolved()` still
        // drops the layer below it.
        assert_eq!(
            ExcludeList::replace(strings(&["a"])).merge(b()),
            ExcludeList::replace(strings(&["a", "b"]))
        );
        // An absent key and an explicit clear are different values even though both carry no
        // patterns: only the latter overrides what it inherits.
        assert_ne!(ExcludeList::default(), ExcludeList::replace(vec![]));
    }
}
