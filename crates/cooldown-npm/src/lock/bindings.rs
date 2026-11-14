//! Every requirement edge a pnpm lock records, one record per binding.
//!
//! A name can resolve to several copies, so judging which copy a resolve moved needs the edges
//! into each copy, not the name's version set.

use super::{
    PnpmSnapshotsDocument, parse_pnpm_document_strict, parse_pnpm_yaml, snapshot_edge_target,
    strip_pnpm_peer_suffixes, walk_pnpm_document_importers,
};
use cooldown_core::Result;

/// The lock record that holds a requirement edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BindingContext {
    /// A workspace importer, by its workspace-relative path.
    Importer(String),
    /// A resolved package's `snapshots:` entry.
    Package {
        /// The full snapshot key, peer suffix included, so each peer context of one package
        /// version stays its own record.
        key: String,
        /// The package's name.
        name: String,
        /// The package's version, peer suffix stripped, or its source reference (a tarball, git, or
        /// `file:` location) when it has no registry version.
        version: String,
    },
}

/// One requirement edge: `context` binds the dependency under `slot` to `name` at `version`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CopyBinding {
    /// The importer or snapshot that records the edge.
    pub context: BindingContext,
    /// The dependency key the edge is recorded under, an npm alias's own key included.
    pub slot: String,
    /// The real package the edge binds, the alias target for an aliased slot.
    pub name: String,
    /// The bound version, peer suffix stripped.
    pub version: String,
}

/// Reads every registry edge of a `pnpm-lock.yaml`: each importer's direct entries and each
/// snapshot's `dependencies` and `optionalDependencies`.
///
/// `link:`, `file:` and `workspace:` values and URL resolutions bind no registry version and are
/// left out.
pub(super) fn parse_pnpm_copy_bindings(content: &str) -> Result<Vec<CopyBinding>> {
    let document = parse_pnpm_document_strict(content)?;
    let mut bindings = Vec::new();
    walk_pnpm_document_importers(&document, |path, slot, _specifier, version| {
        if let Some((name, version)) = version.and_then(|value| snapshot_edge_target(slot, value)) {
            bindings.push(CopyBinding {
                context: BindingContext::Importer(path.to_string()),
                slot: slot.to_string(),
                name,
                version,
            });
        }
    });
    let snapshots: PnpmSnapshotsDocument = parse_pnpm_yaml(content)?;
    for (key, snapshot) in snapshots.snapshots.entries() {
        let Some(snapshot) = snapshot.known() else {
            continue;
        };
        let Some((name, version)) = split_snapshot_key(strip_pnpm_peer_suffixes(key)) else {
            continue;
        };
        let dependent = BindingContext::Package {
            key: key.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        };
        for group in [&snapshot.dependencies, &snapshot.optional_dependencies] {
            for (slot, value) in group.entries() {
                let Some((name, version)) = value
                    .known()
                    .and_then(|value| snapshot_edge_target(slot, value))
                else {
                    continue;
                };
                bindings.push(CopyBinding {
                    context: dependent.clone(),
                    slot: slot.to_string(),
                    name,
                    version,
                });
            }
        }
    }
    Ok(bindings)
}

/// Splits a snapshot key into the package name and its version or source reference, at the first
/// `@` after a scope's leading one: a git or tarball reference can itself contain an `@`
/// (`p@git+ssh://git@github.com/acme/p.git#…`), which a split at the last one would fold into the
/// name.
fn split_snapshot_key(key: &str) -> Option<(&str, &str)> {
    let at = key.get(1..)?.find('@')? + 1;
    let (name, reference) = key.split_at_checked(at)?;
    Some((name, reference.strip_prefix('@')?))
}
