//! The decisions of an apply that need no Maven run: which element each planned change edits,
//! the edited POM text, and whether Maven's verdict before and after the edit moved exactly what
//! cooldown planned.

use std::collections::BTreeMap;
use std::ops::Range;

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{Change, CoreError, MemberRef, Result};

use super::reactor::{Entry, Reactor, Resolution, Source, foreign_releases};
use super::verify::{Module, Verdict};
use crate::tree::slash_path;

/// One element an edit replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    /// The owned POM, by its root-relative path.
    pub(crate) pom: Utf8PathBuf,
    /// The bytes of the version text in that POM.
    pub(crate) span: Range<usize>,
}

/// The elements `change` edits: every element that spells its `from` version for one of its
/// members, or why cooldown cannot edit them.
///
/// A change moves a dependency as one, so a single element cooldown may not edit holds it whole.
/// So does an element in the POM of a member the run excludes, which the run must not rewrite.
pub(crate) fn edits_for(
    change: &Change,
    entries: &[Entry],
    excluded: &[MemberRef],
) -> std::result::Result<Vec<Edit>, String> {
    let name = &change.package.name;
    let mut edits = Vec::new();
    for entry in entries {
        let Resolution::Known { current, source } = &entry.version else {
            continue;
        };
        if entry.name != *name
            || *current != change.from.as_str()
            || !change.members.contains(&entry.member)
        {
            continue;
        }
        match source {
            Source::Fixed(reason) => {
                return Err(format!(
                    "cooldown does not edit {name} {} in {}: {reason}",
                    change.from, entry.member.path
                ));
            }
            Source::Editable { pom, span } => {
                let owner = slash_path(pom.parent().unwrap_or(Utf8Path::new("")));
                if let Some(member) = excluded.iter().find(|member| member.path == owner) {
                    return Err(format!(
                        "{name} {} is declared in {}, which belongs to {}, a member this run \
                         excludes",
                        change.from,
                        slash_path(pom),
                        member.name
                    ));
                }
                let edit = Edit {
                    pom: pom.clone(),
                    span: span.clone(),
                };
                if !edits.contains(&edit) {
                    edits.push(edit);
                }
            }
        }
    }
    if edits.is_empty() {
        return Err(format!(
            "no POM of this project declares {name} {} for the members that report it",
            change.from
        ));
    }
    Ok(edits)
}

/// The text of every owned POM that `landings` edit, each version replaced with its change's
/// target and every other byte kept.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] when two changes would write different versions
/// into one element, and [`CoreError::LockUnreadable`] when an edit falls outside its POM.
pub(crate) fn edited(
    reactor: &Reactor,
    landings: &[(&Change, Vec<Edit>)],
) -> Result<BTreeMap<Utf8PathBuf, String>> {
    let mut by_pom: BTreeMap<&Utf8Path, Vec<(Range<usize>, &Change)>> = BTreeMap::new();
    for (change, edits) in landings {
        for edit in edits {
            let spans = by_pom.entry(edit.pom.as_path()).or_default();
            if let Some((_, other)) = spans
                .iter()
                .find(|(span, other)| *span == edit.span && other.to != change.to)
            {
                return Err(CoreError::UnacceptableResolve(format!(
                    "{} {} and {} {} share one version element in {}",
                    change.package.name,
                    change.to,
                    other.package.name,
                    other.to,
                    slash_path(&edit.pom)
                )));
            }
            spans.push((edit.span.clone(), change));
        }
    }
    let mut files = BTreeMap::new();
    for (pom, mut spans) in by_pom {
        let mut text = reactor
            .owned_text(pom)
            .ok_or_else(|| {
                CoreError::LockUnreadable(format!("{pom} is not a POM of this reactor"))
            })?
            .to_owned();
        // Replacing from the end keeps every earlier span where the reader found it.
        spans.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
        spans.dedup_by(|left, right| left.0 == right.0);
        for (span, change) in spans {
            if text.get(span.clone()).is_none() {
                return Err(CoreError::LockUnreadable(format!(
                    "{pom} changed while cooldown read it"
                )));
            }
            text.replace_range(span, change.to.as_str());
        }
        files.insert(pom.to_owned(), text);
    }
    Ok(files)
}

/// The one project Maven builds in the directory of `member`, by its POM's root-relative path,
/// from Maven's verdict.
///
/// A member is named by its directory, so a directory where Maven builds two projects names
/// neither.
fn project_of<'a>(member: &MemberRef, verdict: &'a Verdict) -> Option<(&'a String, &'a Module)> {
    let mut projects = verdict.modules.iter().filter(|(pom, _)| {
        let dir = Utf8Path::new(pom).parent().map_or("", Utf8Path::as_str);
        dir == member.path || (dir.is_empty() && member.path == ".")
    });
    let project = projects.next()?;
    projects.next().is_none().then_some(project)
}

/// Why `change` cannot land as Maven resolves the reactor `before` the edit, if it cannot.
///
/// Every member has to be the directory of exactly one project Maven builds, read its
/// dependencies only from Maven Central, and select exactly the `from` version cooldown read from
/// its POMs, spelled the same, or cooldown's reading of that member is not Maven's.
pub(crate) fn refusal(change: &Change, before: &Verdict) -> Option<String> {
    let name = &change.package.name;
    for member in &change.members {
        let Some((_, module)) = project_of(member, before) else {
            return Some(format!(
                "Maven builds no project, or more than one, in {}, so cooldown cannot verify a \
                 move there",
                member.path
            ));
        };
        if module.repositories.iter().any(|repository| {
            foreign_releases(
                repository.id.as_deref(),
                repository.url.as_deref(),
                repository.releases,
            )
        }) {
            return Some(format!(
                "Maven can fetch the dependencies of {} from a repository other than Maven \
                 Central, so no Maven Central release date tells how old {name} is",
                member.path
            ));
        }
        let selected: Vec<&String> = module
            .dependencies
            .iter()
            .filter(|(artifact, _)| artifact.name == *name)
            .map(|(_, version)| version)
            .collect();
        if !selected
            .iter()
            .any(|version| version.as_str() == change.from.as_str())
        {
            return Some(format!(
                "Maven selects {name} {} in {}, not the {} its POMs declare, so a profile, a \
                 setting or a parent cooldown does not read decides it",
                selected
                    .first()
                    .map_or("nowhere", |version| version.as_str()),
                member.path,
                change.from
            ));
        }
    }
    None
}

/// Checks that Maven's verdict `after` the edit differs from the one `before` it in exactly the
/// moves `landed` plans: Maven builds the same projects, each member's dependency at a change's
/// `from` reaches its `to` in the member's project, and no other direct dependency of any project
/// Maven builds moves, appears, or disappears.
///
/// # Errors
///
/// Returns [`CoreError::UnacceptableResolve`] naming the first difference that does not match the
/// plan, so candidate isolation can hold the change that caused it.
pub(crate) fn check_moves(before: &Verdict, after: &Verdict, landed: &[&Change]) -> Result<()> {
    let refused = |detail: String| Err(CoreError::UnacceptableResolve(detail));
    // Maven's own spelling, since `1.2` and `1.2.0` are two releases to a repository.
    let same = |left: &str, right: &str| left == right;
    for (path, module) in &after.modules {
        let Some(previous) = before.modules.get(path) else {
            return refused(format!("the edit adds {path} to the projects Maven builds"));
        };
        if previous.gav != module.gav {
            return refused(format!(
                "the edit turns the project of {path} from {} into {}",
                previous.gav, module.gav
            ));
        }
        for (artifact, to) in &module.dependencies {
            let Some(from) = previous.dependencies.get(artifact) else {
                return refused(format!("the edit adds {} to {path}", artifact.display()));
            };
            let planned = landed.iter().any(|change| {
                change.package.name == artifact.name
                    && same(change.from.as_str(), from)
                    && same(change.to.as_str(), to)
                    && change.members.iter().any(|member| {
                        project_of(member, before).is_some_and(|(pom, _)| pom == path)
                    })
            });
            if !same(from, to) && !planned {
                return refused(format!(
                    "the edit would also move {} in {path} from {from} to {to}, which cooldown \
                     did not plan",
                    artifact.display()
                ));
            }
        }
        if let Some(dropped) = previous
            .dependencies
            .keys()
            .find(|artifact| !module.dependencies.contains_key(*artifact))
        {
            return refused(format!("the edit drops {} from {path}", dropped.display()));
        }
    }
    if let Some(path) = before
        .modules
        .keys()
        .find(|path| !after.modules.contains_key(*path))
    {
        return refused(format!(
            "the edit drops {path} from the projects Maven builds"
        ));
    }
    for change in landed {
        for member in &change.members {
            let Some((pom, previous)) = project_of(member, before) else {
                return refused(format!("Maven does not build {}", member.path));
            };
            let Some(module) = after.modules.get(pom) else {
                return refused(format!("Maven does not build {}", member.path));
            };
            for (artifact, from) in &previous.dependencies {
                if artifact.name != change.package.name || !same(from, change.from.as_str()) {
                    continue;
                }
                let to = module
                    .dependencies
                    .get(artifact)
                    .map_or("nothing", String::as_str);
                if !same(to, change.to.as_str()) {
                    return refused(format!(
                        "Maven selects {} {to} in {} after the edit, not the planned {}",
                        artifact.display(),
                        member.path,
                        change.to
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maven::MAVEN_ID;
    use crate::maven::verify::{Artifact, Repository};
    use cooldown_core::{PackageId, UpdateKind, Version};

    const LANG: &str = "org.apache.commons:commons-lang3";

    fn member(path: &str) -> MemberRef {
        MemberRef {
            name: path.to_owned(),
            path: path.to_owned(),
        }
    }

    fn change(name: &str, from: &str, to: &str, members: &[&str]) -> Change {
        Change {
            package: PackageId::new(MAVEN_ID, name, None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: members.iter().map(|path| member(path)).collect(),
        }
    }

    fn artifact(name: &str, kind: &str, classifier: &str) -> Artifact {
        Artifact {
            name: name.to_owned(),
            kind: kind.to_owned(),
            classifier: classifier.to_owned(),
        }
    }

    /// A verdict whose projects, by member directory, select `dependencies`, each read only from
    /// Maven Central, the member at `.` building from `pom.xml` and every other `dir` from
    /// `dir/pom.xml`.
    fn verdict(modules: &[(&str, &[(Artifact, &str)])]) -> Verdict {
        let central = Repository {
            id: Some("central".into()),
            url: Some("https://repo.maven.apache.org/maven2".into()),
            releases: true,
        };
        Verdict {
            modules: modules
                .iter()
                .map(|(dir, dependencies)| {
                    let module = Module {
                        gav: format!("demo:{dir}:1"),
                        dependencies: dependencies
                            .iter()
                            .map(|(artifact, version)| (artifact.clone(), (*version).to_owned()))
                            .collect(),
                        repositories: vec![central.clone()],
                    };
                    (pom_path(dir), module)
                })
                .collect(),
            central_mirrored: false,
        }
    }

    fn pom_path(dir: &str) -> String {
        if dir == "." {
            "pom.xml".to_owned()
        } else {
            format!("{dir}/pom.xml")
        }
    }

    /// The verdict comparison accepts exactly the planned moves: a planned member reaching its
    /// target, but not a move in a member the change does not name or in a project Maven builds
    /// that cooldown did not read, a target left behind, an artifact the edit adds or drops, or a
    /// project the edit adds, drops, or renames.
    #[test]
    fn check_moves_accepts_exactly_the_plan() {
        let jar = artifact(LANG, "jar", "");
        let tests = artifact(LANG, "test-jar", "tests");
        let before = verdict(&[
            (".", &[(jar.clone(), "3.12.0")]),
            ("app", &[(jar.clone(), "3.12.0"), (tests.clone(), "3.17.0")]),
        ]);
        let both_moved = verdict(&[
            (".", &[(jar.clone(), "3.14.0")]),
            ("app", &[(jar.clone(), "3.14.0"), (tests.clone(), "3.17.0")]),
        ]);
        let root_only = change(LANG, "3.12.0", "3.14.0", &["."]);
        let shared = change(LANG, "3.12.0", "3.14.0", &[".", "app"]);
        let check = |before: &Verdict, after: &Verdict, change: &Change| {
            check_moves(before, after, &[change])
        };

        // Both members move and the change names both, while the test-jar stays.
        assert!(check(&before, &both_moved, &shared).is_ok());

        // An inherited move in a member the change leaves out
        let moved_app = check(&before, &both_moved, &root_only);
        assert!(
            matches!(&moved_app, Err(CoreError::UnacceptableResolve(detail)) if detail.contains("in app/pom.xml")),
            "{moved_app:?}"
        );

        // A move in a project Maven builds but cooldown never read, such as one a profile adds
        let with_extra = |version: &str| {
            verdict(&[
                (".", &[(jar.clone(), version)]),
                ("app", &[(jar.clone(), version), (tests.clone(), "3.17.0")]),
                ("extra", &[(jar.clone(), version)]),
            ])
        };
        let moved_extra = check(&with_extra("3.12.0"), &with_extra("3.14.0"), &shared);
        assert!(
            matches!(&moved_extra, Err(CoreError::UnacceptableResolve(detail)) if detail.contains("extra/pom.xml")),
            "{moved_extra:?}"
        );

        // A target Maven spells otherwise, which orders equal but is another release
        let respelled = verdict(&[
            (".", &[(jar.clone(), "3.14")]),
            ("app", &[(jar.clone(), "3.14"), (tests.clone(), "3.17.0")]),
        ]);
        assert!(check(&before, &respelled, &shared).is_err());

        // A target the edit did not reach
        let unmoved = verdict(&[
            (".", &[(jar.clone(), "3.12.0")]),
            ("app", &[(jar.clone(), "3.12.0"), (tests.clone(), "3.17.0")]),
        ]);
        assert!(check(&before, &unmoved, &shared).is_err());

        // An artifact the edit adds or drops
        let added = verdict(&[
            (".", &[(jar.clone(), "3.14.0"), (tests.clone(), "3.17.0")]),
            ("app", &[(jar.clone(), "3.14.0"), (tests.clone(), "3.17.0")]),
        ]);
        assert!(check(&before, &added, &shared).is_err());
        let dropped = verdict(&[
            (".", &[(jar.clone(), "3.14.0")]),
            ("app", &[(jar.clone(), "3.14.0")]),
        ]);
        assert!(check(&before, &dropped, &shared).is_err());

        // A project the edit adds, drops, or gives another identity
        assert!(check(&before, &with_extra("3.14.0"), &shared).is_err());
        let only_root = verdict(&[(".", &[(jar.clone(), "3.14.0")])]);
        assert!(check(&before, &only_root, &shared).is_err());
        let mut renamed = both_moved;
        if let Some(app) = renamed.modules.get_mut("app/pom.xml") {
            app.gav = "demo:other:1".to_owned();
        }
        assert!(check(&before, &renamed, &shared).is_err());
    }

    /// A change cannot land in a member where Maven builds no project or two, as a profile can
    /// add one beside the member's own, one that reads from another repository, or one where
    /// Maven selects another version than cooldown read, even one that orders equal but is
    /// spelled differently.
    #[test]
    fn refusal_names_what_cooldown_cannot_verify() {
        let jar = artifact(LANG, "jar", "");
        let before = verdict(&[
            (".", &[(jar.clone(), "3.13.0")]),
            ("app", &[(jar, "3.13.0")]),
        ]);
        let mut foreign = before.clone();
        if let Some(root) = foreign.modules.get_mut("pom.xml") {
            root.repositories.push(Repository {
                id: Some("snapshots".into()),
                url: None,
                releases: false,
            });
            root.repositories.push(Repository {
                id: Some("corp".into()),
                url: None,
                releases: true,
            });
        }
        let mut profiled = before.clone();
        if let Some(app) = before.modules.get("app/pom.xml") {
            let extra = Module {
                gav: "demo:extra:1".to_owned(),
                ..app.clone()
            };
            profiled.modules.insert("app/extra.xml".to_owned(), extra);
        }
        let refused = |verdict: &Verdict, change: &Change| refusal(change, verdict);
        let lands = change(LANG, "3.13.0", "3.14.0", &["."]);
        let in_app = change(LANG, "3.13.0", "3.14.0", &["app"]);

        let says = |detail: Option<String>, text: &str| detail.is_some_and(|d| d.contains(text));
        assert!(says(
            refused(&before, &change(LANG, "3.12.0", "3.14.0", &["."])),
            "selects"
        ));
        assert!(says(
            refused(&before, &change(LANG, "3.13", "3.14.0", &["."])),
            "selects"
        ));
        assert!(says(
            refused(&before, &change(LANG, "3.13.0", "3.14.0", &["lib"])),
            "no project, or more"
        ));
        assert!(says(refused(&foreign, &lands), "repository"));
        assert!(says(refused(&profiled, &in_app), "no project, or more"));
        assert_eq!(refused(&before, &lands), None);
        assert_eq!(refused(&before, &in_app), None);
    }

    /// An entry `member` reads `name` at `current` through.
    fn entry(member_path: &str, name: &str, current: &str, source: Source) -> Entry {
        Entry {
            member: member(member_path),
            name: name.to_owned(),
            version: Resolution::Known {
                current: current.to_owned(),
                source,
            },
        }
    }

    fn editable(pom: &str, span: Range<usize>) -> Source {
        Source::Editable {
            pom: Utf8PathBuf::from(pom),
            span,
        }
    }

    /// A change edits every element that spells its `from` for one of its members, once each,
    /// while a fixed element, a declaration in an excluded member's POM, or no declaration at all
    /// holds it with the reason.
    #[test]
    fn edits_for_holds_what_cooldown_may_not_edit() {
        let shared = change(LANG, "3.12.0", "3.14.0", &[".", "app"]);
        let root_span = editable("pom.xml", 10..16);
        let entries = [
            entry(".", LANG, "3.12.0", root_span.clone()),
            entry("app", LANG, "3.12.0", root_span),
            entry("app", LANG, "3.17.0", editable("app/pom.xml", 4..10)),
        ];

        // Two members reading one element give one edit, and the other version is no part of it.
        assert_eq!(
            edits_for(&shared, &entries, &[]),
            Ok(vec![Edit {
                pom: "pom.xml".into(),
                span: 10..16,
            }])
        );

        // The element lies in the POM of a member the run excludes.
        let excluded = edits_for(&shared, &entries, &[member(".")]);
        assert!(excluded.is_err_and(|detail| detail.contains("excludes")));

        // One member reads the version through an element cooldown leaves alone.
        let fixed = [
            entry(".", LANG, "3.12.0", editable("pom.xml", 10..16)),
            entry("app", LANG, "3.12.0", Source::Fixed("shared".to_owned())),
        ];
        assert!(edits_for(&shared, &fixed, &[]).is_err_and(|detail| detail.contains("shared")));

        // Nothing declares the version the change moves from.
        let missing = change(LANG, "3.13.0", "3.14.0", &["."]);
        assert!(edits_for(&missing, &entries, &[]).is_err());
    }

    /// The edited text replaces exactly the planned spans, every other byte kept, and two changes
    /// writing different versions into one element refuse the batch.
    #[test]
    fn edited_replaces_only_the_planned_spans() -> Result<()> {
        let text = "<a>1.0</a><b>2.0</b>";
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| CoreError::PathEncoding(path.display().to_string()))?;
        let pom = indoc::formatdoc! {"
            <project><groupId>demo</groupId><artifactId>demo</artifactId><version>0</version>
            <!-- {text} -->
            </project>
        "};
        let start = pom.find(text).unwrap_or_default();
        let prepared = BTreeMap::from([(Utf8PathBuf::from("pom.xml"), pom.clone())]);
        let reactor = Reactor::read(&root, &prepared)?;
        let at = |offset: usize| Edit {
            pom: "pom.xml".into(),
            span: start + offset..start + offset + 3,
        };
        let first = change("a", "1.0", "1.1", &["."]);
        let second = change("b", "2.0", "2.10", &["."]);

        let files = edited(&reactor, &[(&first, vec![at(3)]), (&second, vec![at(13)])])?;

        assert_eq!(
            files.get(Utf8Path::new("pom.xml")),
            Some(&pom.replace(text, "<a>1.1</a><b>2.10</b>"))
        );
        let clash = edited(&reactor, &[(&first, vec![at(3)]), (&second, vec![at(3)])]);
        assert!(matches!(clash, Err(CoreError::UnacceptableResolve(_))));
        Ok(())
    }
}
