//! Yarn Berry member attribution: what each workspace member's own declarations bind in the lock.
//!
//! A Berry lock records each workspace as an entry of its own.
//! Its `dependencies` are the descriptors Yarn derived from the member's manifest: the
//! `dependencies`, `optionalDependencies`, and `devDependencies` fields merged, a later field
//! winning, each range normalized under the default protocol ([WorkspaceResolver.ts][workspace]).
//! Yarn writes that record before any `resolutions` entry or plugin rewrote a descriptor, but keys
//! each package entry by the descriptors after rewriting ([Project.ts][project]).
//!
//! A member's declaration is therefore joined to the entry its own descriptor keys only when
//! nothing can have rewritten that descriptor:
//!
//! - no plugin is configured, which the workspace snapshot establishes or is unavailable;
//! - the root `resolutions` cannot reach the name;
//! - it is no `catalog:` or `patch:` reference;
//! - the lock still records exactly the descriptor the manifest declares, under a protocol the
//!   lock spells or Yarn's `defaultProtocol` establishes as `npm:`.
//!
//! The join reads the lock through the current manifests and configuration, so stale inputs can
//! misattribute: a lock written under another `resolutions` field still records the rewritten
//! descriptors, which only verifying the lock's currency detects.
//!
//! A declaration failing any condition is unproven ([`Unproven`]): no copy of its name is credited
//! to any member, and every copy of the name counts as direct, so an unproven direct dependency is
//! never read as a transitive one.
//!
//! [workspace]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/WorkspaceResolver.ts
//! [project]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Project.ts

use super::{
    BerryLock, BerrySource, Descriptor, Ident, Identified, LAST_PROTOCOL_LESS_SCHEMA, NPM_PROTOCOL,
    WorkspaceEntry, takes_default_protocol, with_default_protocol,
};
use crate::lock::yarn::{YarnBinding, YarnBindings, YarnDependency};
use crate::lock::{FixedBinding, MemberIndex, Unproven};
use crate::manifest::workspaces::{FieldSpecifier, MemberManifest, WorkspaceManifests};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The manifest field of a peer contract, which installs nothing for the member itself.
const PEER_FIELD: &str = "peerDependencies";

/// The protocols whose resolvers bind a descriptor to the package declaring it
/// (`file:./x::locator=…`), so the lock keys it under another spelling than the manifest's.
///
/// Each resolves to a package of its own protocol, which the lock records as no registry release.
const PARENT_BOUND_PROTOCOLS: [&str; 4] = ["file:", "link:", "portal:", "exec:"];

/// What `lock` binds each member of `workspace` to, or `None` when the snapshot is unavailable.
///
/// The members are the workspaces the lock records.
/// One whose manifest the snapshot lacks has every dependency its entry records unproven, and a
/// snapshot member the lock records no workspace for has every install declaration unproven.
pub(super) fn bindings(lock: &BerryLock, workspace: &WorkspaceManifests) -> Option<YarnBindings> {
    if !workspace.is_complete() {
        return None;
    }
    let manifests: BTreeMap<&str, &MemberManifest> = workspace.members().collect();
    let workspace_names: HashSet<String> = lock
        .workspaces()
        .map(|WorkspaceEntry { ident, .. }| ident.full_name())
        .collect();
    let join = Join {
        lock,
        workspace,
        workspace_names: &workspace_names,
    };
    let mut members: BTreeMap<String, BTreeMap<String, YarnDependency>> = BTreeMap::new();
    for WorkspaceEntry {
        path, dependencies, ..
    } in lock.workspaces()
    {
        let bound = match manifests.get(path) {
            Some(manifest) => join.member(manifest, dependencies),
            None => dependencies
                .keys()
                .map(|name| (name.clone(), unproven(Unproven::Unlisted)))
                .collect(),
        };
        members.insert(path.to_string(), bound);
    }
    for (path, manifest) in manifests {
        if members.contains_key(path) {
            continue;
        }
        let bound = manifest
            .declarations
            .iter()
            .map(|(name, fields)| {
                let binding = if fields.iter().all(|field| field.field == PEER_FIELD) {
                    YarnBinding::PeerOnly
                } else {
                    YarnBinding::Unproven(Unproven::NotLocked)
                };
                (name.clone(), dependency(name, fields, binding))
            })
            .collect();
        members.insert(path.to_string(), bound);
    }
    Some(YarnBindings::from_members(members))
}

/// The index of a lock whose workspace manifests are unavailable: every dependency a workspace
/// entry records is declared by that workspace, and unattributed, the members at `excluded` paths
/// left out.
pub(super) fn declared_names_index(lock: &BerryLock, excluded: &HashSet<String>) -> MemberIndex {
    let mut declarations: HashMap<String, Vec<String>> = HashMap::new();
    for WorkspaceEntry {
        path, dependencies, ..
    } in lock.workspaces()
    {
        if excluded.contains(path) {
            continue;
        }
        for name in dependencies.keys() {
            declarations
                .entry(name.clone())
                .or_default()
                .push(path.to_string());
        }
    }
    MemberIndex::unattributed_only(declarations)
}

/// The constraint an install declaration of `dependency` as `specifier` puts on the dependency
/// itself: the specifier, or the range inside a same-name `npm:` reference (`npm:1.2.3`,
/// `npm:chalk@1.2.3` for `chalk`).
///
/// `None` for an `npm:` reference to another package, an alias that constrains no copy of
/// `dependency`.
/// The reference is split as Yarn's remap resolver splits it: a package name, then `@` and the
/// range ([NpmRemapResolver.ts][remap]).
///
/// [remap]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/plugin-npm/sources/NpmRemapResolver.ts
pub(super) fn same_name_constraint<'a>(dependency: &str, specifier: &'a str) -> Option<&'a str> {
    let Some(reference) = specifier.strip_prefix(NPM_PROTOCOL) else {
        return Some(specifier);
    };
    match Ident::split(reference) {
        Some(Identified {
            ident,
            rest: Some(range),
        }) => (ident.full_name() == dependency).then_some(range),
        _ => Some(reference),
    }
}

/// A dependency with no install declaration, bound as `reason` says.
fn unproven(reason: Unproven) -> YarnDependency {
    YarnDependency {
        install: Vec::new(),
        binding: YarnBinding::Unproven(reason),
    }
}

/// The declaration of `name` in `fields`, bound to `binding`.
///
/// Each install field contributes the constraint it puts on `name` ([`same_name_constraint`]),
/// which the exact-pin check reads.
fn dependency(name: &str, fields: &[FieldSpecifier], binding: YarnBinding) -> YarnDependency {
    let install = fields
        .iter()
        .filter(|field| field.field != PEER_FIELD)
        .map(|field| {
            field
                .specifier
                .as_deref()
                .and_then(|specifier| same_name_constraint(name, specifier))
                .map(str::to_string)
        })
        .collect();
    YarnDependency { install, binding }
}

/// What one member's declarations are joined against: the lock, the workspace snapshot around the
/// member, and the package names of every workspace the lock records.
struct Join<'a> {
    lock: &'a BerryLock,
    workspace: &'a WorkspaceManifests,
    workspace_names: &'a HashSet<String>,
}

impl Join<'_> {
    /// Every dependency of the member whose manifest is `manifest` and whose lock entry records
    /// `recorded`.
    ///
    /// A dependency the entry records that the manifest no longer declares is unproven, since the
    /// lock still binds it for the member.
    fn member(
        &self,
        manifest: &MemberManifest,
        recorded: &BTreeMap<String, Option<String>>,
    ) -> BTreeMap<String, YarnDependency> {
        let mut bound: BTreeMap<String, YarnDependency> = manifest
            .declarations
            .iter()
            .map(|(name, fields)| {
                let binding = self.binding(name, fields, recorded.get(name));
                (name.clone(), dependency(name, fields, binding))
            })
            .collect();
        for name in recorded.keys() {
            bound
                .entry(name.clone())
                .or_insert_with(|| unproven(Unproven::Stale));
        }
        bound
    }

    /// What the member's declaration of `name` in `fields` binds, given what its lock entry
    /// records for it: `None` when nothing, `Some(None)` when two different ranges.
    fn binding(
        &self,
        name: &str,
        fields: &[FieldSpecifier],
        recorded: Option<&Option<String>>,
    ) -> YarnBinding {
        let specifiers: Option<Vec<&str>> = fields
            .iter()
            .filter(|field| field.field != PEER_FIELD)
            .map(|declaration| declaration.specifier.as_deref())
            .collect();
        let Some(specifiers) = specifiers else {
            return YarnBinding::Unproven(Unproven::Malformed);
        };
        let Some((specifier, others)) = specifiers.split_first() else {
            // A peer contract alone installs nothing for the member, so a dependency the lock
            // still records for it is stale.
            return match recorded {
                Some(_) => YarnBinding::Unproven(Unproven::Stale),
                None => YarnBinding::PeerOnly,
            };
        };
        let Some(ident) = Ident::parse(name) else {
            return YarnBinding::Unproven(Unproven::Malformed);
        };
        match self.descriptor_range(name, specifier, others, recorded) {
            Ok(range) => self.join(ident, range),
            Err(reason) => YarnBinding::Unproven(reason),
        }
    }

    /// The descriptor range the member's declaration resolved under, or why no lock evidence
    /// proves it.
    ///
    /// The range is the one Yarn's `normalizeDependency` gives the manifest specifier, and it must
    /// be the one the lock records for the member.
    /// A lock of a protocol-less schema records a semver range or a dist-tag without its protocol,
    /// so that record proves the protocol only where Yarn's `defaultProtocol` is established as
    /// `npm:`.
    fn descriptor_range(
        &self,
        name: &str,
        specifier: &str,
        others: &[&str],
        recorded: Option<&Option<String>>,
    ) -> Result<String, Unproven> {
        if others.iter().any(|other| other != &specifier) {
            return Err(Unproven::ConflictingFields);
        }
        if specifier.starts_with("catalog:") {
            return Err(Unproven::Catalog);
        }
        if specifier.starts_with("patch:") {
            return Err(Unproven::Patch);
        }
        if self.workspace.resolution_may_rewrite(name) {
            return Err(Unproven::Resolutions);
        }
        let protocol_less = self.lock.schema <= LAST_PROTOCOL_LESS_SCHEMA;
        if protocol_less
            && takes_default_protocol(specifier)
            && !self.workspace.npm_default_protocol()
        {
            return Err(Unproven::DefaultProtocol);
        }
        let range = with_default_protocol(specifier);
        let Some(Some(recorded)) = recorded else {
            return Err(Unproven::Stale);
        };
        let recorded = if protocol_less {
            with_default_protocol(recorded)
        } else {
            recorded.clone()
        };
        if recorded != range {
            return Err(Unproven::Stale);
        }
        Ok(range)
    }

    /// What the member's descriptor of `ident` at `range` binds in the lock.
    fn join(&self, ident: Ident, range: String) -> YarnBinding {
        if PARENT_BOUND_PROTOCOLS
            .iter()
            .any(|protocol| range.starts_with(protocol))
        {
            return YarnBinding::Local;
        }
        let descriptor = Descriptor { ident, range };
        let Some(package) = self.lock.package_for(&descriptor) else {
            return YarnBinding::Unproven(Unproven::Unjoined);
        };
        match &package.source {
            BerrySource::Workspace { .. } | BerrySource::Unregistered => YarnBinding::Local,
            BerrySource::Registry { .. } if package.locator.ident != descriptor.ident => {
                YarnBinding::Foreign
            }
            // A workspace of the name is linked in place of a registry release whenever its
            // version satisfies the declared range, so an edit of the range may relink it.
            BerrySource::Registry { version }
                if self.workspace_names.contains(&descriptor.ident.full_name()) =>
            {
                YarnBinding::Fixed {
                    version: version.clone(),
                    reason: FixedBinding::WorkspaceName,
                }
            }
            BerrySource::Registry { version } => YarnBinding::Registry(version.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
