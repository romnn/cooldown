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
//! Only locks that attribute a resolution to each member can be judged: pnpm's importers and npm's
//! v2/v3 install tree.
//! Yarn classic, bun, and npm v1 locks attribute nothing per member, so their pins are reported
//! [`Observed::Unknown`] and protected by nothing here; the name's newest copy is never a stand-in,
//! since a transitive duplicate beside an unchanged member copy would read as a moved pin.

use crate::lock::{MemberResolution, NodeLock};
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
}

/// What one settled lock resolves each protected declaration to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observed {
    /// The owning member resolves the dependency to this version.
    Resolved(String),
    /// The lock attributes resolutions per member and binds none for the owner.
    Missing,
    /// The lock attributes nothing per member, so the pin cannot be judged.
    Unknown,
}

impl From<MemberResolution<'_>> for Observed {
    fn from(resolution: MemberResolution<'_>) -> Self {
        match resolution {
            MemberResolution::Resolved(version) => Observed::Resolved(version.to_string()),
            MemberResolution::Missing => Observed::Missing,
            MemberResolution::Unknown => Observed::Unknown,
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
        }
    }
}

impl ProtectedPins {
    /// Reads the exact install pins of every `package.json` in `baseline`, from the bytes read
    /// before any mutation.
    ///
    /// A recovery subset shares its operation's baseline, so every trial judges the same pins.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Parse`] when a captured manifest is not UTF-8 JSON, since an unread pin
    /// cannot be ruled out.
    pub(crate) fn from_baseline(baseline: &MutationBaseline) -> Result<Self> {
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
            declarations.extend(manifest::exact_install_declarations(&member, content)?);
        }
        declarations.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(ProtectedPins { declarations })
    }

    /// Whether no manifest declares an exact install pin.
    pub(crate) fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }

    /// What `lock` resolves each protected declaration to, judged at its owning member.
    ///
    /// An absent lock binds nothing, so every declaration is [`Observed::Missing`].
    pub(crate) fn observe<L: NodeLock>(&self, lock: Option<&str>) -> PinObservations {
        let attribution = lock.map(L::member_attribution);
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

    /// The [`new_violations`](Self::new_violations) of `settled_lock` relative to `baseline_lock`.
    pub(crate) fn violations_between<L: NodeLock>(
        &self,
        baseline_lock: Option<&str>,
        settled_lock: &str,
        authorized: &AuthorizedPins,
    ) -> Vec<PinViolation> {
        if self.is_empty() {
            return Vec::new();
        }
        let baseline = self.observe::<L>(baseline_lock);
        let settled = self.observe::<L>(Some(settled_lock));
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
    ) -> Result<()> {
        let violations = self.violations_between::<L>(baseline_lock, settled_lock, authorized);
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
        Observed::Missing => baseline_bound && REQUIRED_FIELDS.contains(&field),
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
