//! Which directory holding a `Cargo.toml` is a Cargo project of its own, and which one some other
//! project's resolve already covers.
//!
//! Detection marks every `Cargo.toml` directory, then keeps only the topmost one per tree because a
//! workspace-root resolve covers what lies below it. That claim holds only for directories the
//! resolve really reaches, so each dropped one is judged here by the question the gate actually
//! asks — *does any project this run evaluates resolve it?* — computed downward from those projects
//! rather than by walking upward and guessing. A directory nothing resolves has a lock of its own
//! to gate (or a missing one to report), so it is a project in its own right.
//!
//! The projects that count are the roots the orchestrator detected plus every nested directory this
//! module itself answers [`NestedOwnership::Standalone`], which makes the rule a fixpoint: a
//! directory can be resolved by one that is itself still being decided. [`Scan::run`] reaches it by
//! iterating to a stable state rather than by recursing, so the answer does not depend on which
//! directory was asked about first.
//!
//! One case is neither covered nor a project: a manifest under a `[workspace]` root that the root
//! neither lists in `members` nor `exclude`s. Cargo refuses to build such a crate on its own
//! ("current package believes it's in a workspace when it's not"), so nothing resolves it and
//! nothing can — fixture and template crates commonly sit exactly there. It stays with that
//! workspace as [`NestedOwnership::Enclosing`], silent, unless it carries a `package.workspace`
//! pointer: that names a root which ought to list it, and a pointed-at-but-unlisted manifest is a
//! project of its own rather than a silent one.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use cooldown_core::NestedOwnership;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use toml_edit::{DocumentMut, Item};

/// `cargo vendor` writes this beside each vendored crate's manifest.
const VENDOR_CHECKSUM: &str = ".cargo-checksum.json";

/// Where `cargo package` unpacks the crate it just built. Cargo's own root search stops at this
/// directory (`find_root_iter`), so nothing below it is a package of the surrounding build.
const PACKAGE_OUTPUT: &str = "target/package";

/// The dependency tables cargo resolves for every package it activates.
const RESOLVED_SECTIONS: [&str; 2] = ["dependencies", "build-dependencies"];

/// The table cargo resolves only for a workspace member, never for a package it reached as an
/// ordinary dependency (`ResolveOpts { dev_deps: false }` when activating one).
const DEV_SECTION: &str = "dev-dependencies";

/// Reads how each of `nested` relates to the projects this run evaluates.
///
/// `primary` is the roots detection already accepted and `nested` the marked directories the
/// topmost-only rule dropped, in the orchestrator's order; the answer is index-aligned with
/// `nested`. Both together are also the candidate set a workspace's `members` globs are expanded
/// against — they are what the scan reached, so a member a prune or an ignore rule hid is simply
/// not a candidate, and the directory it would have covered is reported on its own merits.
///
/// One cache spans the batch: every manifest is parsed at most once and every project's resolved
/// set computed at most once, so a workspace with hundreds of members costs one pass over them.
pub(crate) fn nested_ownership(
    primary: &[Utf8PathBuf],
    nested: &[Utf8PathBuf],
) -> Vec<NestedOwnership> {
    Scan::new(primary, nested).run()
}

/// Whether the manifest at `path` declares a top-level `[workspace]` table, marking its directory
/// as a workspace root.
///
/// Read or parse failures deliberately read as `false`: a broken manifest cannot establish that its
/// directory is a workspace root, and detection must not fail the whole run over one.
pub(crate) fn declares_workspace(path: &Utf8Path) -> bool {
    matches!(Manifest::read(path), Manifest::Root(_))
}

/// One batch of ownership questions, with everything it derives memoized.
struct Scan<'a> {
    manifests: Manifests,
    /// The roots detection accepted; every one of them is evaluated.
    primary: &'a [Utf8PathBuf],
    /// The directories under appeal, in the orchestrator's order.
    nested: &'a [Utf8PathBuf],
    /// Every marked directory the scan reached, which is what `members` globs may match and what
    /// an in-workspace path dependency must be to join as a member.
    candidates: BTreeSet<Utf8PathBuf>,
    /// Per evaluated project, the directories its lock resolves. Derived from manifests and
    /// candidates alone, so it is stable across the fixpoint's passes.
    resolved: HashMap<Utf8PathBuf, BTreeSet<Utf8PathBuf>>,
}

impl<'a> Scan<'a> {
    fn new(primary: &'a [Utf8PathBuf], nested: &'a [Utf8PathBuf]) -> Self {
        let candidates: BTreeSet<Utf8PathBuf> = primary.iter().chain(nested).cloned().collect();
        Self {
            manifests: Manifests::default(),
            primary,
            nested,
            candidates,
            resolved: HashMap::new(),
        }
    }

    /// Iterates the answers to a stable state.
    ///
    /// Two kinds of directory answer without consulting any other: a manifest declaring
    /// `[workspace]` is a project outright, and a vendored, packaged, or unreadable one is never
    /// one. The rest — "plain" — can only be decided against the set of projects, which their own
    /// answers are part of, so they are revisited in path order until a whole pass changes nothing.
    ///
    /// Each answer is applied as soon as it is computed, so within a pass a directory sees the
    /// answers its predecessors just produced. That plus the fixed path order is what makes a
    /// mutually-resolving pair deterministic: the earlier-sorted directory settles as the project
    /// and the later one as resolved by it, whichever way the dependency happens to point.
    ///
    /// The pass count is capped. A chain of directories can need one pass per link to settle, and a
    /// pathological one could oscillate instead of converging; the cap bounds the work either way,
    /// and because every pass is deterministic so is the state it stops in.
    fn run(mut self) -> Vec<NestedOwnership> {
        let mut answers: Vec<NestedOwnership> = Vec::with_capacity(self.nested.len());
        let mut plain: Vec<usize> = Vec::new();
        for (index, dir) in self.nested.iter().enumerate() {
            if let Some(answer) = self.seed(dir) {
                answers.push(answer);
            } else {
                plain.push(index);
                // Undecided directories start out not-a-project, so a pair that only resolves each
                // other cannot both open as projects.
                answers.push(NestedOwnership::Enclosing);
            }
        }
        for _ in 0..=plain.len() {
            let mut changed = false;
            for index in plain.clone() {
                let Some(dir) = self.nested.get(index) else {
                    continue;
                };
                let answer = self.decide(&dir.clone(), &answers);
                if let Some(slot) = answers.get_mut(index)
                    && *slot != answer
                {
                    *slot = answer;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // The cap can stop mid-oscillation, leaving a `Root(p)` whose `p` is not a project after
        // all — a claim the orchestrator rejects outright, which would abort discovery for a valid
        // repository. Drop those to `Standalone`: erring towards a project of its own is the loud
        // side, and one pass is enough, since turning a directory into a project can only make
        // other claims on it valid, never invalidate one.
        let projects: BTreeSet<&Utf8PathBuf> = self
            .primary
            .iter()
            .chain(
                self.nested
                    .iter()
                    .zip(&answers)
                    .filter(|(_, answer)| **answer == NestedOwnership::Standalone)
                    .map(|(dir, _)| dir),
            )
            .collect();
        let dangling: Vec<usize> = answers
            .iter()
            .enumerate()
            .filter_map(|(index, answer)| match answer {
                NestedOwnership::Root(root) if !projects.contains(root) => Some(index),
                _ => None,
            })
            .collect();
        for index in dangling {
            if let Some(slot) = answers.get_mut(index) {
                *slot = NestedOwnership::Standalone;
            }
        }
        answers
    }

    /// The answer a directory gives on its own, or `None` when it depends on which projects exist.
    fn seed(&mut self, dir: &Utf8Path) -> Option<NestedOwnership> {
        // A vendored registry crate carries a full manifest, but cargo loads it as a *source*,
        // never as a package, so it has no lock of its own to gate. Packaging output is the same:
        // cargo's root search stops there, and the unpacked copy is a build artifact. Without both,
        // each would look exactly like an unresolved package and escape into the report.
        if dir.join(VENDOR_CHECKSUM).is_file()
            || dir
                .ancestors()
                .any(|ancestor| ancestor.ends_with(PACKAGE_OUTPUT))
        {
            return Some(NestedOwnership::Enclosing);
        }
        match self.manifests.load(dir) {
            // A workspace root can never be a member of another workspace — cargo demands the outer
            // one `exclude` it — so nothing above resolves this tree.
            Manifest::Root(_) => Some(NestedOwnership::Standalone),
            // A manifest detection cannot read establishes nothing; leaving it with the enclosing
            // root keeps one broken file from becoming a project of its own.
            Manifest::Unreadable => Some(NestedOwnership::Enclosing),
            Manifest::Pointer(_) | Manifest::Package(_) => None,
        }
    }

    fn decide(&mut self, dir: &Utf8Path, answers: &[NestedOwnership]) -> NestedOwnership {
        if let Some(root) = self.resolving_project(dir, answers) {
            return NestedOwnership::Root(root);
        }
        // A pointer asked to be a member of the root it names. Being silently absorbed by some
        // enclosing workspace instead is not that, and cargo rejects the mismatch outright, so the
        // directory answers for itself.
        if matches!(self.manifests.load(dir), Manifest::Pointer(_)) {
            return NestedOwnership::Standalone;
        }
        if self.under_silent_workspace(dir, answers) {
            return NestedOwnership::Enclosing;
        }
        NestedOwnership::Standalone
    }

    /// The project whose resolve covers `dir`, if any evaluated one does.
    ///
    /// Every evaluated project is checked, not only those above `dir`: a plain package resolves its
    /// `path` dependencies wherever they live, so the project covering a directory can be its
    /// sibling.
    fn resolving_project(
        &mut self,
        dir: &Utf8Path,
        answers: &[NestedOwnership],
    ) -> Option<Utf8PathBuf> {
        // Detected roots first: they are evaluated unconditionally, so the common case — a member
        // of the workspace the scan already found — settles on the first pass.
        for root in self.primary {
            if root != dir && self.resolves(root, dir) {
                return Some(root.clone());
            }
        }
        for (candidate, answer) in self.nested.iter().zip(answers) {
            if candidate == dir || *answer != NestedOwnership::Standalone {
                continue;
            }
            if self.resolves(candidate, dir) {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// Whether an evaluated `[workspace]` root above `dir` takes it in without listing it — the
    /// shape cargo cannot build on its own and nothing else resolves.
    fn under_silent_workspace(&mut self, dir: &Utf8Path, answers: &[NestedOwnership]) -> bool {
        let manifest = dir.join(crate::CARGO_MANIFEST);
        for ancestor in dir.ancestors().skip(1) {
            if !self.is_evaluated(ancestor, answers) {
                continue;
            }
            if let Manifest::Root(workspace) = self.manifests.load(ancestor)
                && !workspace.excludes(ancestor, &manifest)
            {
                return true;
            }
        }
        false
    }

    fn is_evaluated(&self, dir: &Utf8Path, answers: &[NestedOwnership]) -> bool {
        self.primary.iter().any(|root| root == dir)
            || self
                .nested
                .iter()
                .zip(answers)
                .any(|(nested, answer)| nested == dir && *answer == NestedOwnership::Standalone)
    }

    /// Whether the project at `root` resolves `dir` into its lock.
    fn resolves(&mut self, root: &Utf8Path, dir: &Utf8Path) -> bool {
        if !self.resolved.contains_key(root) {
            let set = self.resolved_set(root);
            self.resolved.insert(root.to_owned(), set);
        }
        self.resolved
            .get(root)
            .is_some_and(|resolved| resolved.contains(dir))
    }

    /// Every directory the project at `root` resolves, mirroring what cargo writes into its
    /// `Cargo.lock`.
    ///
    /// Cargo locks its members with every feature enabled and dev units on, so a member's path
    /// dependencies are followed whatever table declares them and whether or not they are
    /// `optional`. It activates everything reached beyond the members as an ordinary dependency,
    /// where dev-dependencies are off and an optional dependency no enabled feature turns on is
    /// skipped outright, so those edges are not followed.
    fn resolved_set(&mut self, root: &Utf8Path) -> BTreeSet<Utf8PathBuf> {
        let inherited = self.manifests.workspace_dependencies(root);
        let members = self.members_of(root);
        let mut resolved: BTreeSet<Utf8PathBuf> = members.iter().cloned().collect();
        let mut queue: VecDeque<Utf8PathBuf> = VecDeque::new();
        for member in &members {
            for dependency in self.manifests.member_reach(member, &inherited) {
                if resolved.insert(dependency.clone()) {
                    queue.push_back(dependency);
                }
            }
        }
        while let Some(package) = queue.pop_front() {
            for dependency in self.manifests.reached_reach(&package) {
                if resolved.insert(dependency.clone()) {
                    queue.push_back(dependency);
                }
            }
        }
        resolved
    }

    /// The workspace members of the project at `root`, or just `root` when it is a plain package
    /// (the sole member of its own implicit workspace).
    ///
    /// For a `[workspace]` root that is the root package itself when the manifest also declares
    /// `[package]`, every candidate its `members` globs match, and — transitively — every path
    /// dependency of a member that cargo's `find_path_deps` makes a member too: one inside the
    /// root's directory, or one outside it whose own manifest points back at this root.
    /// `exclude` then removes what it names, unless `members` names it as well.
    fn members_of(&mut self, root: &Utf8Path) -> BTreeSet<Utf8PathBuf> {
        let mut members = BTreeSet::new();
        let Some(workspace) = self.manifests.workspace(root) else {
            members.insert(root.to_owned());
            return members;
        };
        // A `[workspace]` manifest that also declares `[package]` makes the root its own member.
        if workspace.package.is_some() {
            members.insert(root.to_owned());
        }
        if let Some(globs) = workspace.member_globs(root) {
            for candidate in &self.candidates {
                if globs.is_match(relativize(root, candidate)) {
                    members.insert(candidate.clone());
                }
            }
        }
        members.retain(|member| !workspace.excludes(root, &member.join(crate::CARGO_MANIFEST)));

        // Cargo pulls a member's path dependencies in as members of their own, so their
        // dev-dependencies are resolved too. Only candidates can join: a directory the scan never
        // reached is not one this run could report on anyway.
        let mut queue: VecDeque<Utf8PathBuf> = members.iter().cloned().collect();
        while let Some(member) = queue.pop_front() {
            for dependency in self
                .manifests
                .member_reach(&member, &workspace.dependencies)
            {
                if !self.candidates.contains(&dependency)
                    || workspace.excludes(root, &dependency.join(crate::CARGO_MANIFEST))
                    || !self.joins_workspace(&dependency, root)
                {
                    continue;
                }
                if members.insert(dependency.clone()) {
                    queue.push_back(dependency);
                }
            }
        }
        members
    }

    /// Whether a member's path dependency at `dependency` becomes a member of the workspace at
    /// `root`.
    ///
    /// Inside the root's directory it always does. Outside it, cargo admits it only when the
    /// package's own root search lands back on this workspace (`find_path_deps`), so the search is
    /// replayed here: the first ancestor manifest declaring `[workspace]` is that package's root,
    /// and the first one carrying a `package.workspace` pointer names it — either way the answer is
    /// whether the root found is this one. Ancestors count, not just the dependency's own manifest:
    /// a crate below a directory that points at this workspace belongs to it too.
    fn joins_workspace(&mut self, dependency: &Utf8Path, root: &Utf8Path) -> bool {
        if dependency.starts_with(root) {
            return true;
        }
        for ancestor in dependency.ancestors() {
            match self.manifests.load(ancestor) {
                Manifest::Root(_) => return ancestor == root,
                Manifest::Pointer(pointer) => return pointer.root == root,
                Manifest::Package(_) | Manifest::Unreadable => {}
            }
        }
        false
    }
}

/// The parsed manifests of one batch, read from disk at most once each.
#[derive(Default)]
struct Manifests {
    loaded: HashMap<Utf8PathBuf, Manifest>,
}

impl Manifests {
    fn load(&mut self, dir: &Utf8Path) -> &Manifest {
        self.loaded
            .entry(dir.to_owned())
            .or_insert_with(|| Manifest::read(&dir.join(crate::CARGO_MANIFEST)))
    }

    fn workspace(&mut self, dir: &Utf8Path) -> Option<WorkspaceRoot> {
        match self.load(dir) {
            Manifest::Root(workspace) => Some(workspace.clone()),
            _ => None,
        }
    }

    /// The `[workspace.dependencies]` path entries of `dir`, which its members inherit by name.
    fn workspace_dependencies(&mut self, dir: &Utf8Path) -> BTreeMap<String, Utf8PathBuf> {
        match self.load(dir) {
            Manifest::Root(workspace) => workspace.dependencies.clone(),
            _ => BTreeMap::new(),
        }
    }

    /// The directories this package reaches while cargo is resolving it as a workspace member:
    /// every path dependency it declares, `optional` and dev ones included, with an inherited
    /// `workspace = true` entry resolved through the root's table.
    fn member_reach(
        &mut self,
        dir: &Utf8Path,
        inherited: &BTreeMap<String, Utf8PathBuf>,
    ) -> Vec<Utf8PathBuf> {
        self.edges(dir)
            .into_iter()
            .filter_map(|edge| match edge.target {
                EdgeTarget::Path(path) => Some(path),
                EdgeTarget::Inherited(name) => inherited.get(&name).cloned(),
            })
            .collect()
    }

    /// The directories this package reaches once cargo has activated it as an ordinary dependency.
    ///
    /// Dev-dependencies are off, and an optional dependency is skipped unless an enabled feature
    /// turns it on — which only the resolve knows. Treating every optional edge as not taken can
    /// only make a crate a project of its own that some feature would have covered, which is the
    /// loud side of the mistake; the quiet side would let a crate go ungated.
    /// An inherited entry needs a workspace root to inherit from, which a package reached as a
    /// dependency does not have, so those name nothing here.
    fn reached_reach(&mut self, dir: &Utf8Path) -> Vec<Utf8PathBuf> {
        self.edges(dir)
            .into_iter()
            .filter(|edge| !edge.dev && !edge.optional)
            .filter_map(|edge| match edge.target {
                EdgeTarget::Path(path) => Some(path),
                EdgeTarget::Inherited(_) => None,
            })
            .collect()
    }

    fn edges(&mut self, dir: &Utf8Path) -> Vec<PathEdge> {
        self.load(dir)
            .path_dependencies()
            .map(|deps| deps.0.clone())
            .unwrap_or_default()
    }
}

/// What one `Cargo.toml` says, with every path it names already resolved against its own directory.
enum Manifest {
    /// Declares `[workspace]`: the directory is a workspace root.
    Root(WorkspaceRoot),
    /// Declares `package.workspace = "…"`: an explicit pointer at the root that should list it.
    Pointer(RootPointer),
    /// A plain package.
    Package(PathDependencies),
    /// Absent, unreadable, or unparsable.
    Unreadable,
}

/// A manifest that names its workspace root outright.
struct RootPointer {
    /// The directory the pointer resolves to. Coverage never follows it — a root that lists the
    /// directory already resolves it through its own `members` — but cargo's `find_path_deps` does,
    /// to decide whether a path dependency outside a workspace's directory joins it anyway.
    root: Utf8PathBuf,
    deps: PathDependencies,
}

impl Manifest {
    fn read(path: &Utf8Path) -> Self {
        let dir = path.parent().unwrap_or_else(|| Utf8Path::new(""));
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Manifest::Unreadable;
        };
        let Ok(doc) = raw.parse::<DocumentMut>() else {
            return Manifest::Unreadable;
        };
        let package = doc.get("package");
        let deps = PathDependencies::read(&doc, dir);
        // A manifest can carry both `[package]` and `[workspace]`; the table is what makes the
        // directory a root, exactly as cargo reads it, and the package then is its first member.
        if let Some(workspace) = doc.get("workspace") {
            return Manifest::Root(WorkspaceRoot::read(workspace, dir, package.map(|_| deps)));
        }
        if let Some(pointer) = package
            .and_then(|package| package.get("workspace"))
            .and_then(Item::as_str)
        {
            return Manifest::Pointer(RootPointer {
                root: normalize(&dir.join(pointer)),
                deps,
            });
        }
        Manifest::Package(deps)
    }

    fn path_dependencies(&self) -> Option<&PathDependencies> {
        match self {
            Manifest::Package(deps) => Some(deps),
            Manifest::Pointer(pointer) => Some(&pointer.deps),
            Manifest::Root(workspace) => workspace.package.as_ref(),
            Manifest::Unreadable => None,
        }
    }
}

/// The parts of one `[workspace]` table that decide which directories belong to it.
#[derive(Clone)]
struct WorkspaceRoot {
    /// Absent when the table declares no `members` key, which cargo distinguishes from an empty
    /// list: with no key at all nothing is an explicit member.
    members: Option<Vec<String>>,
    exclude: Vec<String>,
    /// The `[workspace.dependencies]` entries that name a local path, resolved against the root —
    /// what a member's `foo = { workspace = true }` inherits.
    dependencies: BTreeMap<String, Utf8PathBuf>,
    /// `Some` when the manifest declares `[package]` too: the root is then a member itself, and
    /// these are the path dependencies it contributes.
    package: Option<PathDependencies>,
}

impl WorkspaceRoot {
    fn read(workspace: &Item, dir: &Utf8Path, package: Option<PathDependencies>) -> Self {
        let mut dependencies = BTreeMap::new();
        if let Some(table) = workspace.get("dependencies").and_then(Item::as_table_like) {
            for (name, entry) in table.iter() {
                if let Some(path) = entry.get("path").and_then(Item::as_str) {
                    dependencies.insert(name.to_string(), normalize(&dir.join(path)));
                }
            }
        }
        Self {
            members: workspace.get("members").map(string_list),
            exclude: workspace
                .get("exclude")
                .map(string_list)
                .unwrap_or_default(),
            dependencies,
            package,
        }
    }

    /// The `members` entries compiled as globs, matched against a candidate's path relative to the
    /// workspace root.
    ///
    /// A `members` entry is a *path* cargo joins onto the root directory, not a bare string, so
    /// each is put through that same join and normalized before being spelled relative to the root
    /// again. `./a`, `a/`, `tmp/../a` and an absolute `/repo/a` then all name the candidate `a`, as
    /// they do to cargo; wildcard components survive untouched, since to the path they are just
    /// names.
    /// `literal_separator(true)` gives `crates/*` cargo's meaning — `crates/x`, not `crates/x/y`.
    /// An entry that is not a valid glob is dropped rather than failing the run: cargo reports that
    /// manifest error itself, and detection must survive one broken workspace.
    fn member_globs(&self, root: &Utf8Path) -> Option<GlobSet> {
        let members = self.members.as_ref()?;
        let mut builder = GlobSetBuilder::new();
        for entry in members {
            let pattern = relativize(root, &normalize(&root.join(entry)));
            if let Ok(glob) = GlobBuilder::new(&pattern).literal_separator(true).build() {
                builder.add(glob);
            }
        }
        builder.build().ok()
    }

    /// Cargo's `WorkspaceRootConfig::is_excluded`, replicated: a path-prefix test against
    /// `exclude` that an explicit `members` entry overrides. Neither list is glob-expanded here,
    /// and neither is normalized, exactly as cargo does neither for this test — it compares
    /// against the raw `root_dir.join(entry)`.
    ///
    /// That rawness is load-bearing, not an oversight to smooth over. `members = ["tmp/../a"]`
    /// does *not* override `exclude = ["a"]`, because `<root>/tmp/../a` is not a component-wise
    /// prefix of `<root>/a/Cargo.toml`; cargo really does drop that crate from the workspace, and
    /// it builds on its own. Normalizing here would let the member entry cancel the exclusion and
    /// wave an independently-buildable crate through unevaluated. An absolute entry needs no
    /// normalization either: `join` on one already yields the entry itself.
    ///
    /// `root` is the directory holding this workspace's manifest and `manifest` the candidate
    /// member's own `Cargo.toml` path, matching what cargo compares.
    fn excludes(&self, root: &Utf8Path, manifest: &Utf8Path) -> bool {
        let names = |entry: &String| manifest.starts_with(root.join(entry));
        let excluded = self.exclude.iter().any(names);
        let explicit_member = self
            .members
            .as_ref()
            .is_some_and(|members| members.iter().any(names));
        !explicit_member && excluded
    }
}

/// Every `path` dependency edge one manifest declares, with what cargo needs to know about each to
/// decide whether its own resolve follows it.
#[derive(Clone, Default)]
struct PathDependencies(Vec<PathEdge>);

#[derive(Clone)]
struct PathEdge {
    target: EdgeTarget,
    /// Declared in `[dev-dependencies]` (or a `[target.<cfg>]` form): followed only for members.
    dev: bool,
    /// `optional = true`: followed for members, whose lock is resolved with every feature enabled,
    /// and not for a package reached as an ordinary dependency.
    optional: bool,
}

#[derive(Clone)]
enum EdgeTarget {
    /// `path = "…"`, resolved against the declaring manifest's directory.
    Path(Utf8PathBuf),
    /// `workspace = true`: the entry name to look up in the workspace root's
    /// `[workspace.dependencies]`.
    Inherited(String),
}

impl PathDependencies {
    /// Reads every dependency table, including the `[target.<cfg>.…]` forms — cargo resolves every
    /// platform's edges into the one lock, so a crate reached only under one `cfg` is reached like
    /// any other.
    fn read(doc: &DocumentMut, dir: &Utf8Path) -> Self {
        let mut deps = Self::default();
        deps.collect_from(doc.as_table(), dir);
        if let Some(targets) = doc.get("target").and_then(Item::as_table_like) {
            for (_, cfg) in targets.iter() {
                if let Some(cfg) = cfg.as_table_like() {
                    deps.collect_from(cfg, dir);
                }
            }
        }
        deps
    }

    fn collect_from(&mut self, table: &dyn toml_edit::TableLike, dir: &Utf8Path) {
        for section in RESOLVED_SECTIONS {
            self.collect_section(table.get(section), dir, false);
        }
        self.collect_section(table.get(DEV_SECTION), dir, true);
    }

    fn collect_section(&mut self, section: Option<&Item>, dir: &Utf8Path, dev: bool) {
        let Some(table) = section.and_then(Item::as_table_like) else {
            return;
        };
        for (name, entry) in table.iter() {
            let optional = entry
                .get("optional")
                .and_then(Item::as_bool)
                .unwrap_or(false);
            let target = if let Some(path) = entry.get("path").and_then(Item::as_str) {
                EdgeTarget::Path(normalize(&dir.join(path)))
            } else if entry
                .get("workspace")
                .and_then(Item::as_bool)
                .unwrap_or(false)
            {
                EdgeTarget::Inherited(name.to_string())
            } else {
                continue;
            };
            self.0.push(PathEdge {
                target,
                dev,
                optional,
            });
        }
    }
}

/// The string entries of a TOML array, ignoring any entry that is not a string (a manifest cargo
/// itself would reject).
fn string_list(item: &Item) -> Vec<String> {
    item.as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `target` spelled relative to `base`, climbing with `..` where it has to.
///
/// A `members` entry may name a crate outside the workspace directory (`"../bridge"`), so the
/// candidate has to be spelled the same way for the glob to match it; `strip_prefix` alone cannot.
/// The result is joined with `/`, which is how a manifest spells a member on every platform.
pub(crate) fn relativize(base: &Utf8Path, target: &Utf8Path) -> String {
    let base: Vec<&str> = base
        .components()
        .map(|component| component.as_str())
        .collect();
    let target: Vec<&str> = target
        .components()
        .map(|component| component.as_str())
        .collect();
    let shared = base
        .iter()
        .zip(&target)
        .take_while(|(from, to)| from == to)
        .count();
    let mut parts = vec![".."; base.len().saturating_sub(shared)];
    parts.extend(target.into_iter().skip(shared));
    parts.join("/")
}

/// Cargo's `paths::normalize_path`, replicated: `.` and `..` are resolved lexically, without
/// touching the filesystem, so a `path` dependency compares against a scanned directory the same
/// way cargo compares it — and a symlinked checkout is not silently rewritten by a canonicalize
/// cargo never performs.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut normalized = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::{formatdoc, indoc};

    /// A fixture tree plus the two directory sets the orchestrator would hand the adapter.
    struct Tree {
        _tmp: tempfile::TempDir,
        root: Utf8PathBuf,
        primary: Vec<Utf8PathBuf>,
        nested: Vec<Utf8PathBuf>,
    }

    impl Tree {
        /// A scan whose only detected root is the fixture root, which is where the topmost-only
        /// rule leaves it for every shape here.
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root =
                Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf-8 tempdir path");
            Self {
                primary: vec![root.clone()],
                nested: Vec::new(),
                _tmp: tmp,
                root,
            }
        }

        fn write(&self, rel: &str, contents: &str) -> &Self {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&path, contents).expect("write");
            self
        }

        /// Declare the marked directories the topmost-only rule dropped, in scan order (sorted).
        fn nested(mut self, dirs: &[&str]) -> Self {
            self.nested = dirs.iter().map(|dir| self.root.join(dir)).collect();
            self.nested.sort();
            self
        }

        fn answers(&self) -> Vec<NestedOwnership> {
            nested_ownership(&self.primary, &self.nested)
        }

        /// The answer for one nested directory.
        fn ownership(&self, rel: &str) -> NestedOwnership {
            let wanted = self.root.join(rel);
            let index = self
                .nested
                .iter()
                .position(|dir| *dir == wanted)
                .unwrap_or_else(|| panic!("{rel} is not one of the nested directories"));
            self.answers()
                .into_iter()
                .nth(index)
                .expect("one answer per nested directory")
        }

        /// The [`NestedOwnership::Root`] naming a directory relative to the fixture root (`""` is
        /// the fixture root itself).
        fn resolved_by(&self, rel: &str) -> NestedOwnership {
            NestedOwnership::Root(if rel.is_empty() {
                self.root.clone()
            } else {
                self.root.join(rel)
            })
        }
    }

    /// A plain package: no `[workspace]` table, no root pointer, no local dependencies.
    const PACKAGE: &str = indoc! {r#"
        [package]
        name = "nested"
        version = "0.1.0"
        edition = "2021"
    "#};

    /// A package that also declares its own `[workspace]`, the cargo-fuzz shape.
    const NESTED_WORKSPACE: &str = indoc! {r#"
        [package]
        name = "nested"
        version = "0.1.0"
        edition = "2021"

        [workspace]
    "#};

    /// A plain package named `name` with a single `path` dependency on `target`.
    fn depends_on(name: &str, target: &str) -> String {
        formatdoc! {r#"
            [package]
            name = "{name}"
            version = "0.1.0"
            edition = "2021"

            [dependencies]
            dep = {{ path = "{target}" }}
        "#}
    }

    /// The fixpoint must not depend on which directory is decided first. When one plain directory
    /// resolves another, the depender is the project and the dependee is resolved by it — the same
    /// answer whichever way the two names sort.
    #[test]
    fn a_resolving_pair_settles_the_same_way_whichever_name_depends() {
        for (depender, dependee) in [("a", "b"), ("b", "a")] {
            let tree = Tree::new().nested(&["a", "b"]);
            tree.write("Cargo.toml", PACKAGE)
                .write(
                    &format!("{depender}/Cargo.toml"),
                    &depends_on(depender, &format!("../{dependee}")),
                )
                .write(&format!("{dependee}/Cargo.toml"), PACKAGE);

            assert_eq!(
                tree.ownership(depender),
                NestedOwnership::Standalone,
                "`{depender}` resolves `{dependee}`, so it is the project"
            );
            assert_eq!(
                tree.ownership(dependee),
                tree.resolved_by(depender),
                "`{dependee}` is in `{depender}`'s lock"
            );
        }
    }

    /// Two directories that only resolve each other: something has to give, and path order decides
    /// it, so the answer is stable rather than an artifact of iteration.
    #[test]
    fn a_mutually_resolving_pair_settles_on_the_earlier_name() {
        let tree = Tree::new().nested(&["x", "y"]);
        tree.write("Cargo.toml", PACKAGE)
            .write("x/Cargo.toml", &depends_on("x", "../y"))
            .write("y/Cargo.toml", &depends_on("y", "../x"));

        assert_eq!(tree.ownership("x"), NestedOwnership::Standalone);
        assert_eq!(tree.ownership("y"), tree.resolved_by("x"));
    }

    /// Cargo locks its members with every feature on, so a member's `optional` path dependency is
    /// in the lock. A package reached as an ordinary dependency is activated with only the features
    /// its requirer asked for, and an optional dependency no feature turns on is skipped
    /// (`dep_cache`), so what it declares reaches nobody's lock and is a project of its own.
    #[test]
    fn an_optional_dependency_is_followed_for_members_only() {
        let tree = Tree::new().nested(&["a", "root-opt", "a/opt"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                a = { path = "a" }
                root-opt = { path = "root-opt", optional = true }
            "#},
        )
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                opt = { path = "opt", optional = true }
            "#},
        )
        .write("root-opt/Cargo.toml", PACKAGE)
        .write("a/opt/Cargo.toml", PACKAGE);

        assert_eq!(
            tree.ownership("root-opt"),
            tree.resolved_by(""),
            "the resolved package's own optional dependency is in its lock"
        );
        assert_eq!(
            tree.ownership("a/opt"),
            NestedOwnership::Standalone,
            "an optional dependency of a package reached as a dependency is in no lock"
        );
    }

    /// Cargo joins a `members` entry onto the root directory, where the filesystem erases a leading
    /// `./` and a trailing `/`; matching a relative path has to erase them the same way.
    #[test]
    fn member_patterns_ignore_no_op_path_syntax() {
        let tree = Tree::new().nested(&["a", "b"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["./a", "b/"]
            "#},
        )
        .write("a/Cargo.toml", PACKAGE)
        .write("b/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("a"), tree.resolved_by(""));
        assert_eq!(tree.ownership("b"), tree.resolved_by(""));
    }

    /// A member declaring `dep = { workspace = true }` inherits the root's
    /// `[workspace.dependencies]` entry, path and all, so the crate it names is in the workspace's
    /// lock — and, being inside the root, a member of it.
    #[test]
    fn an_inherited_workspace_dependency_is_resolved_by_the_workspace() {
        let tree = Tree::new().nested(&["a", "b", "pointed"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["a"]

                [workspace.dependencies]
                b = { path = "b" }
                pointed = { path = "pointed" }
            "#},
        )
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                b = { workspace = true }
                pointed.workspace = true
            "#},
        )
        .write("b/Cargo.toml", PACKAGE)
        .write(
            "pointed/Cargo.toml",
            indoc! {r#"
                [package]
                name = "pointed"
                version = "0.1.0"
                edition = "2021"
                workspace = ".."
            "#},
        );

        assert_eq!(tree.ownership("b"), tree.resolved_by(""));
        assert_eq!(
            tree.ownership("pointed"),
            tree.resolved_by(""),
            "a pointer at the root that inherits it is still one of its members"
        );
    }

    /// Cargo admits a path dependency outside the workspace directory as a member when that
    /// package's own root search lands back on this workspace (`find_path_deps`) — which for a
    /// manifest carrying `package.workspace` is the root that pointer names. Being a member, its
    /// dev-dependencies are resolved too.
    #[test]
    fn an_outside_dependency_pointing_back_joins_the_workspace() {
        let tree = Tree::new().nested(&["ws", "ws/a", "b", "b/fixture"]);
        tree.write("Cargo.toml", PACKAGE)
            .write(
                "ws/Cargo.toml",
                indoc! {r#"
                    [workspace]
                    members = ["a"]
                "#},
            )
            .write("ws/a/Cargo.toml", &depends_on("a", "../../b"))
            .write(
                "b/Cargo.toml",
                indoc! {r#"
                    [package]
                    name = "b"
                    version = "0.1.0"
                    edition = "2021"
                    workspace = "../ws"

                    [dev-dependencies]
                    fixture = { path = "fixture" }
                "#},
            )
            .write("b/fixture/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("ws"), NestedOwnership::Standalone);
        assert_eq!(tree.ownership("ws/a"), tree.resolved_by("ws"));
        assert_eq!(tree.ownership("b"), tree.resolved_by("ws"));
        assert_eq!(
            tree.ownership("b/fixture"),
            tree.resolved_by("ws"),
            "`b` is a member, so cargo resolves its dev-dependencies"
        );
    }

    #[test]
    fn workspace_members_are_resolved_by_their_workspace() {
        let tree = Tree::new().nested(&["crates/app", "tools/cli", "crates/app/inner"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*", "tools/cli"]
            "#},
        )
        .write("crates/app/Cargo.toml", PACKAGE)
        .write("tools/cli/Cargo.toml", PACKAGE)
        .write("crates/app/inner/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("crates/app"), tree.resolved_by(""), "a glob");
        assert_eq!(
            tree.ownership("tools/cli"),
            tree.resolved_by(""),
            "a literal"
        );
        // `literal_separator(true)` gives `crates/*` cargo's meaning: one segment, not a subtree.
        assert_eq!(
            tree.ownership("crates/app/inner"),
            NestedOwnership::Enclosing,
            "`crates/*` does not reach a second level, and nothing else lists it"
        );
    }

    /// Cargo pulls a member's in-workspace path dependencies in as members of their own
    /// (`find_path_deps`), starting from the root package when the root manifest declares one.
    #[test]
    fn the_root_packages_path_dependency_is_resolved_by_the_workspace() {
        let tree = Tree::new().nested(&["sub"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [workspace]

                [dependencies]
                sub = { path = "sub" }
            "#},
        )
        .write("sub/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("sub"), tree.resolved_by(""));
    }

    #[test]
    fn an_excluded_package_is_a_project_of_its_own() {
        let tree = Tree::new().nested(&["tools/x"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
                exclude = ["tools/x"]
            "#},
        )
        .write("tools/x/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("tools/x"), NestedOwnership::Standalone);
    }

    /// Cargo's `is_excluded` lets an explicit `members` entry override an `exclude` entry that
    /// also matches, so a directory listed in both is still a member.
    #[test]
    fn an_excluded_package_listed_in_members_is_resolved() {
        let tree = Tree::new().nested(&["tools/x"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["tools/x"]
                exclude = ["tools"]
            "#},
        )
        .write("tools/x/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("tools/x"), tree.resolved_by(""));
    }

    #[test]
    fn a_nested_workspace_root_is_a_project_of_its_own() {
        let tree = Tree::new().nested(&["fuzz"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
                exclude = ["fuzz"]
            "#},
        )
        .write("fuzz/Cargo.toml", NESTED_WORKSPACE);

        assert_eq!(tree.ownership("fuzz"), NestedOwnership::Standalone);
    }

    /// The shape cargo refuses to build on its own — "current package believes it's in a workspace
    /// when it's not". Nothing resolves it and nothing can, so it is neither a project nor a gap.
    #[test]
    fn a_manifest_a_workspace_neither_lists_nor_excludes_stays_with_it() {
        let tree = Tree::new().nested(&["fixtures"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
            "#},
        )
        .write("fixtures/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("fixtures"), NestedOwnership::Enclosing);
    }

    /// A silent manifest is not a project, so its resolve does not exist and cannot lend coverage
    /// to what it declares — least of all to a crate the workspace explicitly excluded.
    #[test]
    fn a_silent_manifest_does_not_cover_its_own_path_dependency() {
        let tree = Tree::new().nested(&["fixtures", "fixtures/helper"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
                exclude = ["fixtures/helper"]
            "#},
        )
        .write(
            "fixtures/Cargo.toml",
            indoc! {r#"
                [package]
                name = "fixtures"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                helper = { path = "helper" }
            "#},
        )
        .write("fixtures/helper/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("fixtures"), NestedOwnership::Enclosing);
        assert_eq!(
            tree.ownership("fixtures/helper"),
            NestedOwnership::Standalone,
            "nothing resolves it: the workspace excluded it and the silent manifest has no lock"
        );
    }

    /// A sibling workspace can list members outside its own directory, and cargo's search follows
    /// the pointing manifest to it, so both the pointer and what the owner lists belong to it.
    #[test]
    fn a_sibling_workspace_resolves_the_members_it_lists() {
        let tree = Tree::new().nested(&["bridge", "bridge/child", "owner"]);
        tree.write("Cargo.toml", PACKAGE)
            .write(
                "owner/Cargo.toml",
                indoc! {r#"
                    [workspace]
                    members = ["../bridge", "../bridge/child"]
                "#},
            )
            .write(
                "bridge/Cargo.toml",
                indoc! {r#"
                    [package]
                    name = "bridge"
                    version = "0.1.0"
                    edition = "2021"
                    workspace = "../owner"
                "#},
            )
            .write("bridge/child/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("owner"), NestedOwnership::Standalone);
        assert_eq!(tree.ownership("bridge"), tree.resolved_by("owner"));
        assert_eq!(tree.ownership("bridge/child"), tree.resolved_by("owner"));
    }

    /// A pointer asked to be a member of the root it names. When that root does not list it,
    /// nothing resolves it — and it must not be quietly absorbed by some enclosing workspace
    /// either, which is a mismatch cargo rejects outright.
    #[test]
    fn a_pointer_at_a_root_that_does_not_list_it_is_a_project_of_its_own() {
        let tree = Tree::new().nested(&["bridge", "owner"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
            "#},
        )
        .write(
            "owner/Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["someone-else"]
            "#},
        )
        .write(
            "bridge/Cargo.toml",
            indoc! {r#"
                [package]
                name = "bridge"
                version = "0.1.0"
                edition = "2021"
                workspace = "../owner"
            "#},
        );

        assert_eq!(tree.ownership("bridge"), NestedOwnership::Standalone);
    }

    #[test]
    fn a_pointer_at_a_manifest_without_a_workspace_is_a_project_of_its_own() {
        let tree = Tree::new().nested(&["bridge", "owner"]);
        tree.write("Cargo.toml", PACKAGE)
            .write("owner/Cargo.toml", PACKAGE)
            .write(
                "bridge/Cargo.toml",
                indoc! {r#"
                    [package]
                    name = "bridge"
                    version = "0.1.0"
                    edition = "2021"
                    workspace = "../owner"
                "#},
            );

        assert_eq!(tree.ownership("bridge"), NestedOwnership::Standalone);
    }

    /// A plain package is the sole member of its implicit workspace: its own path dependencies —
    /// dev ones included, and through a `[target.<cfg>]` table — land in its lock, directly or
    /// transitively.
    #[test]
    fn a_plain_package_resolves_its_path_dependency_closure() {
        let tree = Tree::new().nested(&["a", "b", "gated", "harness", "sibling"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                a = { path = "a" }

                [dev-dependencies]
                harness = { path = "harness" }

                [target.'cfg(unix)'.build-dependencies]
                gated = { path = "gated" }
            "#},
        )
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                b = { path = "../b" }
            "#},
        )
        .write("b/Cargo.toml", PACKAGE)
        .write("gated/Cargo.toml", PACKAGE)
        .write("harness/Cargo.toml", PACKAGE)
        .write("sibling/Cargo.toml", PACKAGE);

        assert_eq!(
            tree.ownership("a"),
            tree.resolved_by(""),
            "a direct path dep"
        );
        assert_eq!(
            tree.ownership("b"),
            tree.resolved_by(""),
            "reached transitively through `a`"
        );
        assert_eq!(
            tree.ownership("gated"),
            tree.resolved_by(""),
            "reached through a `[target.<cfg>]` table"
        );
        assert_eq!(
            tree.ownership("harness"),
            tree.resolved_by(""),
            "the package's own dev-dependency is in its lock"
        );
        assert_eq!(
            tree.ownership("sibling"),
            NestedOwnership::Standalone,
            "nothing declares it, so nothing resolves it"
        );
    }

    /// Cargo activates an ordinary dependency with dev-dependencies switched off
    /// (`ResolveOpts { dev_deps: false }`), and a plain package's path dependency is not a member,
    /// so the dev-dependency it declares reaches nobody's lock.
    #[test]
    fn a_dev_dependency_of_a_reached_package_is_a_project_of_its_own() {
        let tree = Tree::new().nested(&["a", "a/fixture"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                a = { path = "a" }
            "#},
        )
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dev-dependencies]
                fixture = { path = "fixture" }
            "#},
        )
        .write("a/fixture/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("a"), tree.resolved_by(""));
        assert_eq!(
            tree.ownership("a/fixture"),
            NestedOwnership::Standalone,
            "the root's lock never contains it, so it is unevaluated unless gated on its own"
        );
    }

    /// Inside a workspace the same crate *is* resolved: an in-workspace path dependency becomes a
    /// member, and cargo resolves its members' dev-dependencies.
    #[test]
    fn a_dev_dependency_of_a_workspace_member_is_resolved() {
        let tree = Tree::new().nested(&["a", "a/fixture"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["a"]
            "#},
        )
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dev-dependencies]
                fixture = { path = "fixture" }
            "#},
        )
        .write("a/fixture/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("a"), tree.resolved_by(""));
        assert_eq!(tree.ownership("a/fixture"), tree.resolved_by(""));
    }

    /// A project of its own resolves its own dependencies, wherever they live — the covering
    /// project can be a sibling, and is found even when it is decided after the directory it
    /// covers.
    #[test]
    fn a_standalone_project_resolves_its_sibling_path_dependency() {
        let tree = Tree::new().nested(&["a", "b"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
                exclude = ["a", "b"]
            "#},
        )
        .write("a/Cargo.toml", PACKAGE)
        .write(
            "b/Cargo.toml",
            indoc! {r#"
                [package]
                name = "b"
                version = "0.1.0"
                edition = "2021"

                [workspace]

                [dependencies]
                a = { path = "../a" }
            "#},
        );

        assert_eq!(tree.ownership("b"), NestedOwnership::Standalone);
        assert_eq!(
            tree.ownership("a"),
            tree.resolved_by("b"),
            "decided before `b`, and still resolved by it"
        );
    }

    #[test]
    fn a_standalone_workspace_resolves_its_own_members() {
        let tree = Tree::new().nested(&["fuzz", "fuzz/crate"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
                exclude = ["fuzz"]
            "#},
        )
        .write(
            "fuzz/Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crate"]
            "#},
        )
        .write("fuzz/crate/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("fuzz"), NestedOwnership::Standalone);
        assert_eq!(tree.ownership("fuzz/crate"), tree.resolved_by("fuzz"));
    }

    /// Two silent manifests that only declare each other: neither is a project, so neither resolve
    /// exists, and the search must terminate rather than let them cover one another.
    #[test]
    fn a_cycle_between_two_silent_manifests_terminates() {
        let tree = Tree::new().nested(&["x", "y"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/*"]
            "#},
        )
        .write(
            "x/Cargo.toml",
            indoc! {r#"
                [package]
                name = "x"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                y = { path = "../y" }
            "#},
        )
        .write(
            "y/Cargo.toml",
            indoc! {r#"
                [package]
                name = "y"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                x = { path = "../x" }
            "#},
        );

        assert_eq!(
            tree.answers(),
            vec![NestedOwnership::Enclosing, NestedOwnership::Enclosing]
        );
    }

    /// `cargo vendor` output and `cargo package`'s unpacked copy carry real manifests that no
    /// workspace lists, but cargo treats neither as a package of the surrounding build.
    #[test]
    fn vendored_and_packaged_copies_are_never_projects() {
        let tree = Tree::new().nested(&["vendor/itoa", "target/package/app-0.1.0"]);
        tree.write("Cargo.toml", PACKAGE)
            .write("vendor/itoa/Cargo.toml", NESTED_WORKSPACE)
            .write("vendor/itoa/.cargo-checksum.json", "{\"files\":{}}")
            .write("target/package/app-0.1.0/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("vendor/itoa"), NestedOwnership::Enclosing);
        assert_eq!(
            tree.ownership("target/package/app-0.1.0"),
            NestedOwnership::Enclosing
        );
    }

    /// A manifest detection cannot parse establishes nothing, so the enclosing root keeps it
    /// rather than the run failing (or a broken file becoming a project).
    #[test]
    fn a_broken_nested_manifest_stays_with_the_enclosing_root() {
        let tree = Tree::new().nested(&["member"]);
        tree.write("Cargo.toml", "[workspace]\n")
            .write("member/Cargo.toml", "[package\nname = ");

        assert_eq!(tree.ownership("member"), NestedOwnership::Enclosing);
    }

    /// A `members` glob that matches nothing contributes nothing; the workspace still resolves its
    /// root package's own reach, and nothing more.
    #[test]
    fn a_members_glob_matching_nothing_adds_no_members() {
        let tree = Tree::new().nested(&["sub", "other"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [workspace]
                members = ["missing/*"]

                [dependencies]
                sub = { path = "sub" }
            "#},
        )
        .write("sub/Cargo.toml", PACKAGE)
        .write("other/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("sub"), tree.resolved_by(""));
        assert_eq!(tree.ownership("other"), NestedOwnership::Enclosing);
    }

    /// `members` and `exclude` entries are paths cargo joins onto the root, not bare strings: an
    /// absolute entry stays absolute, and `./a`, `a/` and `tmp/../b` all name the directory they
    /// resolve to.
    #[test]
    fn member_and_exclude_entries_are_paths() {
        let tree = Tree::new().nested(&["a", "b", "excluded"]);
        // Serialized, not interpolated: a Windows temp path's backslashes are escapes inside a
        // TOML basic string, and an invalid escape would make the manifest unreadable instead.
        let absolute = toml_string(tree.root.join("excluded").as_str());
        tree.write(
            "Cargo.toml",
            &formatdoc! {r#"
                [workspace]
                members = ["./a", "tmp/../b"]
                exclude = [{absolute}]
            "#},
        )
        .write("a/Cargo.toml", PACKAGE)
        .write("b/Cargo.toml", PACKAGE)
        .write("tmp/keep", "")
        .write("excluded/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("a"), tree.resolved_by(""), "`./a`");
        assert_eq!(tree.ownership("b"), tree.resolved_by(""), "`tmp/../b`");
        assert_eq!(
            tree.ownership("excluded"),
            NestedOwnership::Standalone,
            "an absolute `exclude` entry excludes, rather than naming a doubled prefix that \
             matches nothing"
        );
    }

    /// Cargo's exclusion test compares raw joined paths, so a `members` entry spelled differently
    /// from the `exclude` entry does not cancel it — the crate is dropped from the workspace and
    /// builds on its own, which is what cooldown has to report.
    #[test]
    fn a_member_entry_only_overrides_an_exclusion_it_literally_prefixes() {
        let tree = Tree::new().nested(&["a"]);
        tree.write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["tmp/../a"]
                exclude = ["a"]
            "#},
        )
        .write("a/Cargo.toml", PACKAGE)
        .write("tmp/keep", "");

        assert_eq!(tree.ownership("a"), NestedOwnership::Standalone);
    }

    /// A TOML basic string holding `value`, escaped by the TOML serializer rather than by hand.
    fn toml_string(value: &str) -> String {
        toml::Value::String(value.to_string()).to_string()
    }

    /// Cargo's cycle check ignores dev edges, so `a -> b -> c -> a` through `[dev-dependencies]` is
    /// a manifest set cargo accepts. The fixpoint oscillates on it, and whichever state the pass
    /// cap stops in must still be self-consistent: a `Root` claim on a directory that is not a
    /// project would be rejected downstream and abort discovery for a valid repository.
    #[test]
    fn a_three_way_dev_cycle_leaves_no_dangling_claim() {
        for (first, second, third) in [("a", "b", "c"), ("c", "b", "a")] {
            let tree = Tree::new().nested(&["a", "b", "c"]);
            tree.write("Cargo.toml", PACKAGE);
            for (from, to) in [(first, second), (second, third), (third, first)] {
                tree.write(
                    &format!("{from}/Cargo.toml"),
                    &formatdoc! {r#"
                        [package]
                        name = "{from}"
                        version = "0.1.0"
                        edition = "2021"

                        [dev-dependencies]
                        dep = {{ path = "../{to}" }}
                    "#},
                );
            }

            let answers = tree.answers();
            assert_eq!(
                answers,
                tree.answers(),
                "the same inputs give the same answer"
            );
            let projects: BTreeSet<&Utf8PathBuf> = tree
                .primary
                .iter()
                .chain(
                    tree.nested
                        .iter()
                        .zip(&answers)
                        .filter(|(_, answer)| **answer == NestedOwnership::Standalone)
                        .map(|(dir, _)| dir),
                )
                .collect();
            for answer in &answers {
                if let NestedOwnership::Root(root) = answer {
                    assert!(
                        projects.contains(root),
                        "{root} is claimed as the resolver but is not a project: {answers:?}"
                    );
                }
            }
        }
    }

    /// Cargo's `find_root` consults pointers on ancestors, so a crate below a directory that points
    /// at a workspace belongs to that workspace too — and being a member, its own dev path
    /// dependencies are resolved into that workspace's lock in turn.
    #[test]
    fn an_ancestors_pointer_carries_a_whole_subtree_into_the_workspace() {
        let tree = Tree::new().nested(&["ws", "ws/a", "b", "b/fixture", "b/fixture/helper"]);
        tree.write("Cargo.toml", PACKAGE)
            .write(
                "ws/Cargo.toml",
                indoc! {r#"
                    [workspace]
                    members = ["a"]
                "#},
            )
            .write("ws/a/Cargo.toml", &depends_on("ws-a", "../../b"))
            .write(
                "b/Cargo.toml",
                indoc! {r#"
                    [package]
                    name = "b"
                    version = "0.1.0"
                    edition = "2021"
                    workspace = "../ws"

                    [dev-dependencies]
                    fixture = { path = "fixture" }
                "#},
            )
            .write(
                "b/fixture/Cargo.toml",
                indoc! {r#"
                    [package]
                    name = "fixture"
                    version = "0.1.0"
                    edition = "2021"

                    [dev-dependencies]
                    helper = { path = "helper" }
                "#},
            )
            .write("b/fixture/helper/Cargo.toml", PACKAGE);

        assert_eq!(tree.ownership("b"), tree.resolved_by("ws"));
        assert_eq!(
            tree.ownership("b/fixture"),
            tree.resolved_by("ws"),
            "a member through its ancestor's pointer, so its dev units are resolved too"
        );
        assert_eq!(tree.ownership("b/fixture/helper"), tree.resolved_by("ws"));
    }

    #[test]
    fn workspace_table_marks_a_workspace_root() {
        let tree = Tree::new();
        tree.write("Cargo.toml", "[workspace]\nmembers = [\"member\"]\n");

        assert!(declares_workspace(&tree.root.join("Cargo.toml")));
    }

    #[test]
    fn package_only_manifest_is_not_a_workspace_root() {
        let tree = Tree::new();
        tree.write("Cargo.toml", PACKAGE);

        assert!(!declares_workspace(&tree.root.join("Cargo.toml")));
        assert!(
            !declares_workspace(&tree.root.join("missing/Cargo.toml")),
            "an absent manifest declares nothing"
        );
        tree.write("broken/Cargo.toml", "[workspace\n");
        assert!(
            !declares_workspace(&tree.root.join("broken/Cargo.toml")),
            "neither does one that cannot be parsed"
        );
    }

    #[test]
    fn relativize_climbs_out_of_the_base_directory() {
        assert_eq!(
            relativize(Utf8Path::new("/repo/owner"), Utf8Path::new("/repo/bridge")),
            "../bridge"
        );
        assert_eq!(
            relativize(Utf8Path::new("/repo"), Utf8Path::new("/repo/crates/app")),
            "crates/app"
        );
        assert_eq!(
            relativize(Utf8Path::new("/repo"), Utf8Path::new("/repo")),
            ""
        );
    }
}
