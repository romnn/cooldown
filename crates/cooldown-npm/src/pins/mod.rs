//! The exact install pins a settled resolve must leave in place.
//!
//! The lock alone cannot protect every exact pin: pnpm records one specifier per importer, so a pin
//! in a second install field beside a caret (`dependencies: ^20.11.1` beside
//! `devDependencies: 20.14.5`) is visible only in the manifest.
//! When a sibling candidate's resolve moves such a package, the lock still looks consistent and the
//! move would be reported as collateral, leaving the manifest's pin violated.
//!
//! So the pins are read from the baseline manifests the prepared mutation captured, independently
//! of which packages the plan names, and each settled lock is judged per declaring member.
//! A settled lock that newly violates one is refused, and the caller's recovery isolates the
//! candidate whose resolve did it.
//!
//! Only locks that attribute a resolution to each member can be judged: pnpm's importers, npm's
//! v2/v3 install tree, the package keys of a `bun.lock` its reader supports, a Yarn classic
//! lock's selector keys read through the workspace manifests, and a Yarn Berry lock's record of
//! each workspace joined to the member manifests.
//! npm v1 locks attribute nothing per member, so their pins are reported [`Observed::Unknown`]
//! and protected by nothing here; the name's newest copy is never a stand-in, since a transitive
//! duplicate beside an unchanged member copy would read as a moved pin.
//! Neither is a Bun member whose keys the lock does not prove, nor a Bun peer declaration, which
//! Bun binds by version rather than by key.
//! Within an npm install tree, an alias occupying the owner's slot (`"foo": "npm:other@1.0.0"`)
//! installs another package, so it is unknown too rather than a version of the pinned name.
//!
//! Yarn classic is judged through each member's selector, the one Yarn's manifest normalization
//! keeps for the name, so a pin hidden in a second install field is judged against the copy the
//! first field's range binds.
//! The members and their selectors are the baseline's, rebuilt from the capture alone, with
//! cooldown's own recorded edits applied ([`ManifestEdits`]), never what a resolver or a lifecycle
//! script later writes, while the pins judged stay the baseline's.
//! A Yarn binding the lock does not prove a registry copy, such as a git or local source, a name
//! the root `resolutions` can rewrite, a workspace sibling's name, or a descriptor that vanished,
//! is unknown and protects nothing, and so is every Yarn pin when the captured workspace or the
//! lock cannot be read.
//!
//! Yarn Berry tells an unknown binding from an unproven one: a declaration whose binding the lock
//! no longer proves, because it records another descriptor, no entry, or no workspace for it.
//! A required pin the baseline lock proved bound but the settled lock leaves unproven has lost
//! its evidence, which is a violation rather than a pass ([`Observed::Unproven`]).
//! A Berry pin is the version it constrains the dependency itself to, a same-name `npm:`
//! reference unwrapped ([`NodeLock::install_pin`]).

use crate::lock::{MemberResolution, NodeLock, WorkspaceManifests};
use crate::manifest::{self, DeclarationKey, ExactDeclaration, ManifestRewrite};
use cooldown_core::{Change, CoreError, MutationBaseline, Result, RewriteMode};
use std::collections::BTreeMap;

/// The install fields whose exact pin must stay bound: an absent binding there is a violation,
/// while an absent optional dependency is the manager's legitimate answer to a failed install.
const REQUIRED_FIELDS: [&str; 2] = ["dependencies", "devDependencies"];

/// The exact install pins the baseline manifests declare, every member's, excluded ones included.
#[derive(Debug, Default)]
pub(crate) struct ProtectedPins {
    /// Sorted by key, so a refusal names its violations in a stable order.
    declarations: Vec<ExactDeclaration>,
    /// The baseline workspace manifests the lock's member attribution reads declarations from, for
    /// a manager that needs them ([`NodeLock::WORKSPACE_MANIFESTS`]).
    workspace: Option<WorkspaceManifests>,
}

/// What one settled lock resolves each protected declaration to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observed {
    /// The owning member resolves the dependency to this version.
    Resolved(String),
    /// The lock attributes resolutions per member and binds none for the owner.
    Missing,
    /// The pin cannot be judged: the lock attributes nothing per member, or the owner's binding is
    /// no registry version of the pinned name, such as an npm alias to another package.
    Unknown,
    /// The owner still declares the dependency, but the lock no longer proves which copy it binds
    /// (Yarn Berry).
    Unproven,
}

impl From<MemberResolution<'_>> for Observed {
    fn from(resolution: MemberResolution<'_>) -> Self {
        match resolution {
            MemberResolution::Resolved(version) => Observed::Resolved(version.to_string()),
            MemberResolution::Missing => Observed::Missing,
            MemberResolution::Unknown => Observed::Unknown,
            MemberResolution::Unproven => Observed::Unproven,
        }
    }
}

/// One lock's [`Observed`] value for every protected declaration.
#[derive(Debug, Default)]
pub(crate) struct PinObservations {
    observed: BTreeMap<DeclarationKey, Observed>,
}

impl PinObservations {
    fn get(&self, key: &DeclarationKey) -> &Observed {
        self.observed.get(key).unwrap_or(&Observed::Unknown)
    }
}

/// The declarations cooldown itself rewrote with authority for an active change, each with the
/// specifier it wrote.
///
/// The settled lock is judged against that replacement, recorded at the write, so a resolver's
/// later manifest edit never redefines the pin.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthorizedPins {
    replacements: BTreeMap<DeclarationKey, String>,
}

impl AuthorizedPins {
    /// Records the declarations `rewrite` wrote for `change` when the user authorized rewriting
    /// its pins.
    ///
    /// Only `--rewrite` ([`RewriteMode::Always`] as the plan's own mode) or the plan's
    /// `rewrite_pins` for a downgrade authorize a pin edit, and only for the change's own
    /// declarations.
    /// An internally chosen manifest mode authorizes nothing, so `mode` must be the plan's.
    pub(crate) fn record(
        &mut self,
        change: &Change,
        mode: RewriteMode,
        rewrite_pins: bool,
        rewrite: &ManifestRewrite,
    ) {
        let authorized =
            change.direct && (mode == RewriteMode::Always || (rewrite_pins && change.downgrade));
        if !authorized {
            return;
        }
        for declaration in &rewrite.rewritten {
            self.replacements
                .insert(declaration.key.clone(), declaration.replacement.clone());
        }
    }

    /// These authorizations together with every one `other` recorded, `other`'s winning where both
    /// rewrote one declaration.
    pub(crate) fn union(&self, other: &AuthorizedPins) -> AuthorizedPins {
        let mut replacements = self.replacements.clone();
        replacements.extend(
            other
                .replacements
                .iter()
                .map(|(key, replacement)| (key.clone(), replacement.clone())),
        );
        AuthorizedPins { replacements }
    }

    fn replacement(&self, key: &DeclarationKey) -> Option<&str> {
        self.replacements.get(key).map(String::as_str)
    }
}

/// The specifiers cooldown itself wrote, by declaration, whether or not they rewrote a pin.
///
/// A Yarn classic lock keys each entry by the selector a manifest requests, so after a widen the
/// lock names the member's copy under the replacement rather than the baseline specifier.
/// They are recorded at the write, so a resolver's later manifest edit never redefines a selector.
#[derive(Debug, Clone, Default)]
pub(crate) struct ManifestEdits {
    replacements: BTreeMap<DeclarationKey, String>,
}

/// No manifest edit, the state a lock no cooldown write preceded was resolved under.
const NO_EDITS: ManifestEdits = ManifestEdits {
    replacements: BTreeMap::new(),
};

impl ManifestEdits {
    /// Records every declaration `rewrite` wrote.
    pub(crate) fn record(&mut self, rewrite: &ManifestRewrite) {
        for declaration in &rewrite.rewritten {
            self.replacements
                .insert(declaration.key.clone(), declaration.replacement.clone());
        }
    }

    /// These edits together with every one `other` recorded, `other`'s winning where both
    /// rewrote one declaration.
    pub(crate) fn union(&self, other: &ManifestEdits) -> ManifestEdits {
        let mut replacements = self.replacements.clone();
        replacements.extend(
            other
                .replacements
                .iter()
                .map(|(key, replacement)| (key.clone(), replacement.clone())),
        );
        ManifestEdits { replacements }
    }

    /// `workspace` with these edits written into it.
    pub(crate) fn apply_to(&self, workspace: &WorkspaceManifests) -> WorkspaceManifests {
        workspace.with_replacements(
            self.replacements
                .iter()
                .map(|(key, replacement)| (key, replacement.as_str())),
        )
    }
}

/// The manifest edits each lock of a pin judgement was resolved under.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EditStates<'a> {
    /// The edits behind the baseline lock: the earlier accepted landings'.
    pub(crate) baseline: &'a ManifestEdits,
    /// The edits behind the settled lock: the baseline's and the judged candidate's.
    pub(crate) settled: &'a ManifestEdits,
}

impl EditStates<'_> {
    /// Both locks resolved under the baseline manifests, as for a manager whose attribution reads
    /// no manifest.
    pub(crate) const NONE: EditStates<'static> = EditStates {
        baseline: &NO_EDITS,
        settled: &NO_EDITS,
    };
}

/// A protected declaration the settled lock newly violates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinViolation {
    key: DeclarationKey,
    /// The pin judged: the baseline's, or the authorized replacement.
    pin: String,
    observed: Observed,
}

impl PinViolation {
    fn describe(&self) -> String {
        let DeclarationKey {
            field, dependency, ..
        } = &self.key;
        let manifest = self.key.manifest();
        let pin = &self.pin;
        match &self.observed {
            Observed::Resolved(version) => format!(
                "{dependency} resolves to {version} for {manifest}, whose {field} pins {pin}"
            ),
            Observed::Missing | Observed::Unknown => {
                format!("{dependency} no longer resolves for {manifest}, whose {field} pins {pin}")
            }
            Observed::Unproven => format!(
                "yarn.lock no longer proves which copy of {dependency} {manifest} binds, whose {field} pins {pin}"
            ),
        }
    }
}

impl ProtectedPins {
    /// Reads the exact install pins of every `package.json` in `baseline`, from the bytes read
    /// before any mutation, each as the manager reads it ([`NodeLock::install_pin`]), and for a
    /// manager whose attribution reads the workspace manifests
    /// ([`NodeLock::WORKSPACE_MANIFESTS`]), their snapshot, rebuilt from the capture alone.
    ///
    /// A recovery subset shares its operation's baseline, so every trial judges the same pins.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Parse`] when a captured manifest is not UTF-8 JSON, since an unread pin
    /// cannot be ruled out.
    pub(crate) fn from_baseline<L: NodeLock>(baseline: &MutationBaseline) -> Result<Self> {
        let mut declarations = Vec::new();
        for file in baseline.files() {
            if file.path().file_name() != Some("package.json") {
                continue;
            }
            let Some(bytes) = file.contents() else {
                continue;
            };
            let member = manifest::member_of_manifest(file.path());
            let content = std::str::from_utf8(bytes).map_err(|err| {
                CoreError::Parse(format!("{}: {err}", manifest::manifest_of_member(&member)))
            })?;
            declarations.extend(manifest::exact_install_declarations(
                &member,
                content,
                L::install_pin,
            )?);
        }
        declarations.sort_by(|left, right| left.key.cmp(&right.key));
        let workspace = L::WORKSPACE_MANIFESTS
            .map(|dialect| WorkspaceManifests::from_baseline(baseline, dialect));
        Ok(ProtectedPins {
            declarations,
            workspace,
        })
    }

    /// The baseline workspace manifests, when the manager's attribution reads them.
    pub(crate) fn workspace(&self) -> Option<&WorkspaceManifests> {
        self.workspace.as_ref()
    }

    /// Whether no manifest declares an exact install pin.
    pub(crate) fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }

    /// What `lock`, resolved under the manifests cooldown's `edits` left, resolves each protected
    /// declaration to, judged at its owning member.
    ///
    /// An absent lock binds nothing, so every declaration is [`Observed::Missing`].
    pub(crate) fn observe<L: NodeLock>(
        &self,
        lock: Option<&str>,
        edits: &ManifestEdits,
    ) -> PinObservations {
        let workspace = self
            .workspace
            .as_ref()
            .map(|workspace| edits.apply_to(workspace));
        let attribution = lock.map(|lock| L::member_attribution_in(lock, workspace.as_ref()));
        let observed = self
            .declarations
            .iter()
            .map(|ExactDeclaration { key, .. }| {
                let observed = attribution
                    .as_ref()
                    .map_or(Observed::Missing, |attribution| {
                        Observed::from(attribution.resolution(&key.member, &key.dependency))
                    });
                (key.clone(), observed)
            })
            .collect();
        PinObservations { observed }
    }

    /// The protected declarations `settled` violates that `baseline` did not already violate the
    /// same way, in key order.
    ///
    /// A pin with an authorized replacement is judged against that replacement.
    /// A violation is grandfathered only when the baseline judged the same pin and observed the same
    /// version (build metadata aside), so a moved bad version, or a mismatch in another member
    /// beside an old one, is new.
    /// A missing binding violates a required field only where the baseline bound one, and never an
    /// optional one; an unknown attribution violates nothing.
    /// An unproven binding is lost evidence, judged like a missing one: a violation of a required
    /// field the baseline proved bound, and never of an optional field or of a pin the baseline
    /// already left unproven.
    pub(crate) fn new_violations(
        &self,
        baseline: &PinObservations,
        settled: &PinObservations,
        authorized: &AuthorizedPins,
    ) -> Vec<PinViolation> {
        let mut violations = Vec::new();
        for ExactDeclaration { key, pin } in &self.declarations {
            let judged = authorized.replacement(key).unwrap_or(pin);
            let before = baseline.get(key);
            let after = settled.get(key);
            let baseline_bound = matches!(before, Observed::Resolved(_));
            if !violates(judged, key.field, after, baseline_bound) {
                continue;
            }
            let grandfathered = match (before, after) {
                (Observed::Resolved(was), Observed::Resolved(now)) => {
                    normalized_pin(judged) == normalized_pin(pin)
                        && without_build(was) == without_build(now)
                }
                _ => false,
            };
            if !grandfathered {
                violations.push(PinViolation {
                    key: key.clone(),
                    pin: judged.to_string(),
                    observed: after.clone(),
                });
            }
        }
        violations
    }

    /// The [`new_violations`](Self::new_violations) of `settled_lock` relative to `baseline_lock`,
    /// each observed under the manifest edits `edits` records for it.
    pub(crate) fn violations_between<L: NodeLock>(
        &self,
        baseline_lock: Option<&str>,
        settled_lock: &str,
        authorized: &AuthorizedPins,
        edits: EditStates<'_>,
    ) -> Vec<PinViolation> {
        if self.is_empty() {
            return Vec::new();
        }
        let baseline = self.observe::<L>(baseline_lock, edits.baseline);
        let settled = self.observe::<L>(Some(settled_lock), edits.settled);
        self.new_violations(&baseline, &settled, authorized)
    }

    /// Refuses a settled lock that newly violates a protected pin relative to `baseline_lock`.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::UnacceptableResolve`] naming every new violation, so the caller's
    /// candidate isolation holds the candidate whose resolve moved the pin and keeps the detail.
    pub(crate) fn refuse_new_violations<L: NodeLock>(
        &self,
        baseline_lock: Option<&str>,
        settled_lock: &str,
        authorized: &AuthorizedPins,
        edits: EditStates<'_>,
    ) -> Result<()> {
        let violations =
            self.violations_between::<L>(baseline_lock, settled_lock, authorized, edits);
        if violations.is_empty() {
            return Ok(());
        }
        Err(CoreError::UnacceptableResolve(refusal(&violations)))
    }
}

/// The detail of a refused resolve, naming each violated pin with its owner, field, and result.
pub(crate) fn refusal(violations: &[PinViolation]) -> String {
    format!(
        "the resolve moved an exact pin it was not asked to rewrite: {}",
        violations
            .iter()
            .map(PinViolation::describe)
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// Whether `observed` breaks the declaration `pin` in `field`.
fn violates(pin: &str, field: &str, observed: &Observed, baseline_bound: bool) -> bool {
    match observed {
        Observed::Resolved(version) => {
            // npm reads a bare version as an equality and ignores build metadata when matching,
            // so `1.2.3+build.a` satisfies the exact pin `1.2.3`.
            // The matcher reads every exact-pin spelling the planner holds (`= v1.2.3`), so the
            // guard and the planner judge one pin alike.
            !crate::version::version_in_range(pin, version)
        }
        Observed::Missing | Observed::Unproven => {
            baseline_bound && REQUIRED_FIELDS.contains(&field)
        }
        Observed::Unknown => false,
    }
}

/// The version an exact pin names, every spelling node-semver reads as equal stripped
/// (`= v1.2.3` → `1.2.3`); any other specifier is returned trimmed.
///
/// Grandfathering compares pins by this, so an authorized replacement that only respells the
/// baseline pin still judges the same pin.
fn normalized_pin(pin: &str) -> &str {
    crate::version::exact_pin_version(pin).unwrap_or_else(|| pin.trim())
}

/// `version` without its build metadata, which npm ignores when comparing versions.
fn without_build(version: &str) -> &str {
    version.split_once('+').map_or(version, |(base, _)| base)
}

#[cfg(test)]
mod tests;
