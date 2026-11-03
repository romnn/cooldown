//! The YAML reading the pnpm and Yarn Berry lockfiles share.
//!
//! Both locks are project-local YAML documents read under one set of parser options, so a decoder
//! names only its own file in an error, and each decides itself what a duplicate key means.

use cooldown_core::{CoreError, Result};
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

/// Parses the lockfile `lockfile` into any typed view of it under the one set of parser options
/// every lock reader shares: duplicate keys kept, the size-proportional budget lifted, the
/// structural guards kept.
///
/// An empty or whitespace-only document reads as the default, which is a legitimately empty lock
/// for pnpm and a document without its marker for Berry.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] naming `lockfile` and the YAML failure.
pub(crate) fn parse_lock_yaml<T: serde::de::DeserializeOwned + Default>(
    content: &str,
    lockfile: &str,
) -> Result<T> {
    if content.trim().is_empty() {
        return Ok(T::default());
    }
    let mut options = serde_saphyr::Options::default();
    // Neither manager writes duplicate keys, but a hand-edited lock may carry them; `LastWins`
    // passes duplicate pairs through to the map visitor, so [`YamlEntries`] keeps each of them (the
    // default policy errors out instead) and the decoder decides whether they conflict.
    options.duplicate_keys = serde_saphyr::options::DuplicateKeyPolicy::LastWins;
    // The default budget caps events/nodes/scalar bytes at totals a large monorepo lock can
    // legitimately exceed, and a breach would surface as a spurious parse failure.
    // Lift the size-proportional caps — the input is a project-local file — while keeping the
    // structural guards (nesting depth, alias amplification).
    if let Some(budget) = options.budget.as_mut() {
        budget.max_events = usize::MAX;
        budget.max_nodes = usize::MAX;
        budget.max_total_scalar_bytes = usize::MAX;
    }
    serde_saphyr::from_str_with_options(content, options)
        .map_err(|err| CoreError::LockUnreadable(format!("{lockfile}: {err}")))
}

/// A YAML mapping read in document order with duplicate keys tolerated: entries surface exactly
/// as the file lists them, and each occurrence of a duplicated key (which pnpm never writes)
/// contributes its own entry instead of erroring out or collapsing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct YamlEntries<V>(Vec<(String, V)>);

impl<V> Default for YamlEntries<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> YamlEntries<V> {
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(key, _)| key.as_str())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for YamlEntries<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct EntriesVisitor<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for EntriesVisitor<V> {
            type Value = YamlEntries<V>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mapping")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = access.next_entry()? {
                    entries.push(entry);
                }
                Ok(YamlEntries(entries))
            }

            // A section header with nothing under it (`packages:` at end of file) is a null
            // value, not a mapping, and reads as an empty section.
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(YamlEntries(Vec::new()))
            }
        }
        deserializer.deserialize_map(EntriesVisitor(std::marker::PhantomData))
    }
}
