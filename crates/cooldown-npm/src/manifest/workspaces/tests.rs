use super::*;
use color_eyre::eyre;
use indoc::indoc;

/// A project written into a temporary directory.
struct TempRoot {
    /// Owns the temporary directory; dropping it deletes the project, so it must stay bound for
    /// as long as the root is read.
    _guard: tempfile::TempDir,
    root: Utf8PathBuf,
}

/// A project of `files`, each a project-relative path and its contents.
fn project(files: &[(&str, &str)]) -> eyre::Result<TempRoot> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    for (rel, contents) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(TempRoot {
        _guard: guard,
        root,
    })
}

/// A member manifest named `name` at version 1.0.0.
fn member(name: &str) -> String {
    format!(r#"{{ "name": "{name}", "version": "1.0.0" }}"#)
}

/// A private root manifest whose `workspaces` field is `workspaces`, given as JSON.
fn root_with(workspaces: &str) -> String {
    format!(r#"{{ "name": "root", "private": true, "workspaces": {workspaces} }}"#)
}

/// The sorted member paths of the snapshot read under `root`, or its unavailability reason.
fn discovered(root: &Utf8Path) -> Result<Vec<String>, String> {
    let manifests = WorkspaceManifests::read(root);
    if let Some(reason) = manifests.unavailable_reason() {
        return Err(reason.to_string());
    }
    let mut paths: Vec<String> = manifests.member_paths().into_iter().collect();
    paths.sort();
    Ok(paths)
}

/// Both `workspaces` shapes name the same members, the root always among them, and a member
/// without dependencies counts like any other.
#[test]
fn both_workspace_shapes_discover_the_same_members() -> eyre::Result<()> {
    for workspaces in [
        r#"["packages/*"]"#,
        r#"{ "packages": ["packages/*"], "nohoist": ["**/x"] }"#,
    ] {
        let TempRoot { _guard, root } = project(&[
            ("package.json", &root_with(workspaces)),
            ("packages/a/package.json", &member("pkg-a")),
            ("packages/b/package.json", &member("pkg-b")),
        ])?;
        assert_eq!(
            discovered(&root),
            Ok(vec![
                ".".to_string(),
                "packages/a".to_string(),
                "packages/b".to_string()
            ]),
            "{workspaces}"
        );
    }
    Ok(())
}

/// A project without `workspaces`, or with an empty pattern list, is its root alone, and needs no
/// private root.
#[test]
fn a_project_without_workspaces_is_its_root() -> eyre::Result<()> {
    for manifest in [
        r#"{ "name": "single" }"#,
        r#"{ "name": "single", "workspaces": [] }"#,
        r#"{ "name": "single", "workspaces": { "nohoist": ["**"] } }"#,
    ] {
        let TempRoot { _guard, root } = project(&[
            ("package.json", manifest),
            ("packages/a/package.json", &member("pkg-a")),
        ])?;
        assert_eq!(discovered(&root), Ok(vec![".".to_string()]), "{manifest}");
    }
    Ok(())
}

/// Nested and overlapping patterns match each member once, a `**` matches at any depth including
/// none, and wildcards inside a name, a leading `./`, and a trailing `/` are spelling only.
#[test]
fn nested_and_overlapping_patterns_match_each_member_once() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[
        (
            "package.json",
            &root_with(r#"["./packages/*/", "packages/**", "tools/*/c", "apps/app-?"]"#),
        ),
        ("packages/package.json", &member("packages-dir")),
        ("packages/a/package.json", &member("pkg-a")),
        ("packages/group/b/package.json", &member("pkg-b")),
        ("tools/group/c/package.json", &member("tool-c")),
        ("tools/group/d/package.json", &member("tool-d")),
        ("apps/app-1/package.json", &member("app-1")),
        ("apps/app-10/package.json", &member("app-10")),
    ])?;
    assert_eq!(
        discovered(&root),
        Ok(vec![
            ".".to_string(),
            "apps/app-1".to_string(),
            "packages".to_string(),
            "packages/a".to_string(),
            "packages/group/b".to_string(),
            "tools/group/c".to_string(),
        ])
    );
    Ok(())
}

/// Yarn's own membership rules hold: a match without a name or a version is no member, a
/// directory without a manifest, a dot directory, and anything below `node_modules` are not
/// matched, and a directory no pattern names is no member.
#[test]
fn only_named_and_versioned_matches_are_members() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[
        ("package.json", &root_with(r#"["packages/**"]"#)),
        ("packages/named/package.json", &member("named")),
        (
            "packages/nameless/package.json",
            r#"{ "version": "1.0.0" }"#,
        ),
        (
            "packages/versionless/package.json",
            r#"{ "name": "versionless" }"#,
        ),
        (
            "packages/empty-name/package.json",
            r#"{ "name": "", "version": "1.0.0" }"#,
        ),
        ("packages/no-manifest/readme.md", "nothing here"),
        ("packages/.hidden/package.json", &member("hidden")),
        (
            "packages/named/node_modules/dep/package.json",
            &member("dep"),
        ),
        ("elsewhere/package.json", &member("elsewhere")),
    ])?;
    assert_eq!(
        discovered(&root),
        Ok(vec![".".to_string(), "packages/named".to_string()])
    );
    // The skipped matches are still captured, so a baseline can tell them apart from members.
    let mut rels: Vec<String> = BaselineDiscovery::run(&root)
        .manifests
        .iter()
        .map(|rel| display_rel(rel))
        .collect();
    rels.sort();
    assert_eq!(
        rels,
        [
            "packages/empty-name/package.json",
            "packages/named/package.json",
            "packages/nameless/package.json",
            "packages/versionless/package.json",
        ]
    );
    Ok(())
}

/// Every configuration Yarn refuses, and every pattern the reader does not model, leaves the
/// snapshot unavailable instead of reporting a partial member set as complete.
#[test]
fn unsupported_or_refused_workspaces_are_unavailable() -> eyre::Result<()> {
    let refused = [
        // A published root
        (
            r#"{ "name": "root", "workspaces": ["packages/*"] }"#.to_string(),
            vec![("packages/a/package.json", member("pkg-a"))],
        ),
        // Two members of one name
        (
            root_with(r#"["packages/*"]"#),
            vec![
                ("packages/a/package.json", member("same")),
                ("packages/b/package.json", member("same")),
            ],
        ),
        // A malformed member manifest
        (
            root_with(r#"["packages/*"]"#),
            vec![("packages/a/package.json", "{ not json".to_string())],
        ),
        // A dependency field that is no object
        (
            root_with(r#"["packages/*"]"#),
            vec![(
                "packages/a/package.json",
                r#"{ "name": "a", "version": "1.0.0", "dependencies": ["x"] }"#.to_string(),
            )],
        ),
        // A `workspaces` field of another type
        (
            r#"{ "private": true, "workspaces": "packages/*" }"#.to_string(),
            Vec::new(),
        ),
        (
            r#"{ "private": true, "workspaces": { "packages": "packages/*" } }"#.to_string(),
            Vec::new(),
        ),
        (root_with("[1]"), Vec::new()),
    ];
    for (root_manifest, members) in refused {
        let mut files: Vec<(&str, &str)> = vec![("package.json", root_manifest.as_str())];
        files.extend(
            members
                .iter()
                .map(|(rel, content)| (*rel, content.as_str())),
        );
        let TempRoot { _guard, root } = project(&files)?;
        assert!(discovered(&root).is_err(), "{root_manifest} {members:?}");
    }

    for pattern in [
        "packages/{a,b}",
        "packages/[ab]",
        "!packages/a",
        "packages/+(a)",
        "../outside/*",
        "/absolute/*",
        "packages//a",
        "packages/a**",
        "node_modules/*",
        "**",
        "",
        "packages\\\\a",
    ] {
        let TempRoot { _guard, root } = project(&[
            (
                "package.json",
                &root_with(&serde_json::to_string(&[pattern])?),
            ),
            ("packages/a/package.json", &member("pkg-a")),
        ])?;
        assert!(discovered(&root).is_err(), "{pattern}");
    }
    Ok(())
}

/// A `**` followed by more pattern would have to search `node_modules` the way Yarn's glob does,
/// so it refuses instead, while a trailing `**` skips it as Yarn's ignore list does.
#[test]
fn a_globstar_never_reads_node_modules() -> eyre::Result<()> {
    let files = [
        ("packages/a/c/package.json", member("pkg-c")),
        (
            "packages/a/node_modules/x/c/package.json",
            member("nested-c"),
        ),
    ];
    let outer = root_with(r#"["packages/**"]"#);
    let mut trailing: Vec<(&str, &str)> = vec![("package.json", &outer)];
    trailing.extend(files.iter().map(|(rel, content)| (*rel, content.as_str())));
    let TempRoot { _guard, root } = project(&trailing)?;
    assert_eq!(
        discovered(&root),
        Ok(vec![".".to_string(), "packages/a/c".to_string()])
    );

    let inner = root_with(r#"["packages/**/c"]"#);
    let mut inner_files: Vec<(&str, &str)> = vec![("package.json", &inner)];
    inner_files.extend(files.iter().map(|(rel, content)| (*rel, content.as_str())));
    let TempRoot { _guard, root } = project(&inner_files)?;
    assert!(discovered(&root).is_err());
    Ok(())
}

/// A symbolic link to a member directory or a linked manifest refuses discovery, since Yarn's glob
/// follows some links and not others, while a linked file the walk only passes by is harmless.
#[cfg(unix)]
#[test]
fn symbolic_links_on_the_way_are_unavailable() -> eyre::Result<()> {
    let linked_directory = project(&[
        ("package.json", &root_with(r#"["packages/*"]"#)),
        ("elsewhere/a/package.json", &member("pkg-a")),
    ])?;
    std::fs::create_dir_all(linked_directory.root.join("packages"))?;
    std::os::unix::fs::symlink(
        linked_directory.root.join("elsewhere/a"),
        linked_directory.root.join("packages/a"),
    )?;
    assert!(discovered(&linked_directory.root).is_err());

    let linked_manifest = project(&[
        ("package.json", &root_with(r#"["packages/*"]"#)),
        ("shared/a.json", &member("pkg-a")),
    ])?;
    std::fs::create_dir_all(linked_manifest.root.join("packages/a"))?;
    std::os::unix::fs::symlink(
        linked_manifest.root.join("shared/a.json"),
        linked_manifest.root.join("packages/a/package.json"),
    )?;
    assert!(discovered(&linked_manifest.root).is_err());

    let linked_file = project(&[
        ("package.json", &root_with(r#"["packages/**"]"#)),
        ("packages/a/package.json", &member("pkg-a")),
        ("packages/a/bin/real.sh", "echo"),
    ])?;
    std::os::unix::fs::symlink(
        linked_file.root.join("packages/a/bin/real.sh"),
        linked_file.root.join("packages/a/bin/tool"),
    )?;
    assert_eq!(
        discovered(&linked_file.root),
        Ok(vec![".".to_string(), "packages/a".to_string()])
    );
    Ok(())
}

/// The snapshot keeps every declaration of each member, the root's included, in normalization
/// order with each field's verbatim specifier, and names the members that may be linked.
#[test]
fn the_snapshot_keeps_each_members_raw_declarations() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[
        (
            "package.json",
            indoc! {r#"
                {
                  "private": true,
                  "workspaces": ["packages/*"],
                  "dependencies": { "chalk": "^4.1.0" },
                  "devDependencies": { "chalk": "4.1.0" },
                  "optionalDependencies": { "chalk": "*" },
                  "peerDependencies": { "chalk": "^4.0.0", "odd": 7 }
                }
            "#},
        ),
        ("packages/a/package.json", &member("pkg-a")),
    ])?;
    let manifests = WorkspaceManifests::read(&root);

    let (_, root_manifest) = manifests
        .members()
        .find(|(path, _)| *path == ".")
        .ok_or_else(|| eyre::eyre!("no root member"))?;
    assert_eq!(root_manifest.name, None);
    let field = |field: &'static str, specifier: Option<&str>| FieldSpecifier {
        field,
        specifier: specifier.map(str::to_string),
    };
    assert_eq!(
        root_manifest.declarations.get("chalk"),
        Some(&vec![
            field("optionalDependencies", Some("*")),
            field("dependencies", Some("^4.1.0")),
            field("devDependencies", Some("4.1.0")),
            field("peerDependencies", Some("^4.0.0")),
        ])
    );
    assert_eq!(
        root_manifest.declarations.get("odd"),
        Some(&vec![field("peerDependencies", None)])
    );
    // The root's own name, if any, is never a sibling.
    assert_eq!(manifests.sibling_names(), HashSet::from(["pkg-a"]));
    Ok(())
}

/// A `resolutions` key targets the last package of its path, scoped or not, and taints that name
/// alone, while a key Yarn rejects, or a field of another type, taints every name.
#[test]
fn resolutions_taint_the_package_each_key_targets() -> eyre::Result<()> {
    let tainted = |resolutions: &str, name: &str| -> eyre::Result<bool> {
        let TempRoot { _guard, root } = project(&[(
            "package.json",
            &format!(r#"{{ "name": "root", "resolutions": {resolutions} }}"#),
        )])?;
        Ok(WorkspaceManifests::read(&root).resolution_may_rewrite(name))
    };

    // Bare, nested, and scoped keys
    let keys = r#"{ "lodash": "4.17.21", "a/**/b": "1.0.0", "a/**/@scope/c": "1.0.0", "@scope/d/e": "1.0.0" }"#;
    for name in ["lodash", "b", "@scope/c", "e"] {
        assert!(tainted(keys, name)?, "{name}");
    }
    // Neither a key's parents nor an unrelated name
    for name in ["a", "@scope/d", "chalk", "@scope"] {
        assert!(!tainted(keys, name)?, "{name}");
    }

    // An unreadable key or field
    for resolutions in [
        r#"{ "a/": "1.0.0" }"#,
        r#"{ "a//b": "1.0.0" }"#,
        r#"{ "a/**": "1.0.0" }"#,
        r#"{ "": "1.0.0" }"#,
        r#"["lodash"]"#,
    ] {
        assert!(tainted(resolutions, "chalk")?, "{resolutions}");
    }
    assert!(!tainted("null", "chalk")?);
    Ok(())
}

/// The baseline `paths` of `root` with the workspace discovery recorded beside them, as the
/// adapter captures a mutation baseline.
fn captured_baseline(root: &Utf8Path, paths: &[&str]) -> eyre::Result<MutationBaseline> {
    let discovery = BaselineDiscovery::run(root);
    let mut rels: Vec<Utf8PathBuf> = paths.iter().map(Utf8PathBuf::from).collect();
    rels.extend(discovery.manifests.iter().cloned());
    let mut baseline = MutationBaseline::read(root, rels)?;
    discovery.record(&mut baseline);
    Ok(baseline)
}

/// The member paths and `chalk` specifiers of a snapshot, or its unavailability reason.
fn chalk_by_member(manifests: &WorkspaceManifests) -> Result<Vec<String>, String> {
    if let Some(reason) = manifests.unavailable_reason() {
        return Err(reason.to_string());
    }
    Ok(manifests
        .members()
        .map(|(path, manifest)| {
            let specifier = manifest
                .declarations
                .get("chalk")
                .and_then(|fields| fields.first())
                .and_then(|field| field.specifier.clone())
                .unwrap_or_default();
            format!("{path} {specifier}")
        })
        .collect())
}

/// The baseline snapshot is rebuilt from the capture alone: a later edit, a member directory a
/// trial creates or removes, and a root manifest rewritten on disk change neither the members nor
/// their declarations.
#[test]
fn the_baseline_snapshot_ignores_later_project_changes() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[
        ("package.json", &root_with(r#"["packages/*"]"#)),
        (
            "packages/a/package.json",
            r#"{ "name": "pkg-a", "version": "1.0.0", "dependencies": { "chalk": "^4.1.0" } }"#,
        ),
        (
            "packages/b/package.json",
            r#"{ "name": "pkg-b", "version": "1.0.0", "dependencies": { "chalk": "4.1.2" } }"#,
        ),
    ])?;
    let baseline = captured_baseline(&root, &["package.json"])?;
    let captured = Ok(vec![
        ". ".to_string(),
        "packages/a ^4.1.0".to_string(),
        "packages/b 4.1.2".to_string(),
    ]);
    assert_eq!(
        chalk_by_member(&WorkspaceManifests::from_baseline(&baseline)),
        captured
    );

    // A lifecycle script adds a member, another manifest disappears, one is rewritten, and the
    // root stops naming the workspace at all.
    std::fs::create_dir_all(root.join("packages/generated"))?;
    std::fs::write(
        root.join("packages/generated/package.json"),
        member("generated"),
    )?;
    std::fs::remove_file(root.join("packages/b/package.json"))?;
    std::fs::write(
        root.join("packages/a/package.json"),
        r#"{ "name": "pkg-a", "version": "1.0.0", "dependencies": { "chalk": "^5.0.0" } }"#,
    )?;
    std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#)?;

    assert_eq!(
        chalk_by_member(&WorkspaceManifests::from_baseline(&baseline)),
        captured
    );
    Ok(())
}

/// A baseline that recorded no discovery, recorded it unavailable, or missed a member's manifest
/// attributes nothing.
#[test]
fn an_incomplete_baseline_capture_is_unavailable() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[
        ("package.json", &root_with(r#"["packages/*"]"#)),
        ("packages/a/package.json", &member("pkg-a")),
    ])?;
    // Without the record
    let unrecorded = MutationBaseline::read(&root, ["package.json", "packages/a/package.json"])?;
    assert!(!WorkspaceManifests::from_baseline(&unrecorded).is_complete());

    // With the record but without the member's manifest
    let mut partial = MutationBaseline::read(&root, ["package.json"])?;
    BaselineDiscovery::run(&root).record(&mut partial);
    assert!(!WorkspaceManifests::from_baseline(&partial).is_complete());

    // With discovery unavailable at capture
    std::fs::write(
        root.join("package.json"),
        root_with(r#"["packages/{a,b}"]"#),
    )?;
    let unavailable = captured_baseline(&root, &["package.json"])?;
    assert!(!WorkspaceManifests::from_baseline(&unavailable).is_complete());
    Ok(())
}

/// A root manifest rewritten between the walk and the capture leaves discovery unavailable, since
/// the captured patterns may name other members than the walk found.
#[test]
fn a_root_changed_during_capture_records_discovery_unavailable() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[
        ("package.json", &root_with(r#"["packages/*"]"#)),
        ("packages/a/package.json", &member("pkg-a")),
    ])?;
    let discovery = BaselineDiscovery::run(&root);
    std::fs::write(root.join("package.json"), root_with(r#"["apps/*"]"#))?;
    let mut rels = vec![Utf8PathBuf::from("package.json")];
    rels.extend(discovery.manifests.iter().cloned());
    let mut baseline = MutationBaseline::read(&root, rels)?;
    discovery.record(&mut baseline);

    assert_eq!(
        WorkspaceManifests::from_baseline(&baseline).unavailable_reason(),
        Some("package.json changed while the mutation baseline was captured")
    );
    Ok(())
}

/// A replacement rewrites exactly the declaration it names, and one the snapshot does not hold
/// changes nothing.
#[test]
fn replacements_rewrite_only_their_own_declaration() -> eyre::Result<()> {
    let TempRoot { _guard, root } = project(&[(
        "package.json",
        r#"{ "dependencies": { "chalk": "^4.1.0" }, "devDependencies": { "chalk": "4.1.0" } }"#,
    )])?;
    let manifests = WorkspaceManifests::read(&root);
    let key = |member: &str, field: &'static str, dependency: &str| DeclarationKey {
        member: member.to_string(),
        field,
        dependency: dependency.to_string(),
    };
    let rewritten = key(".", "dependencies", "chalk");
    let absent = key("packages/a", "dependencies", "chalk");

    let replaced = manifests.with_replacements([(&rewritten, "^4.1.2"), (&absent, "^9.0.0")]);

    let specifiers: Vec<Option<String>> = replaced
        .members()
        .flat_map(|(_, manifest)| {
            manifest
                .declarations
                .get("chalk")
                .cloned()
                .unwrap_or_default()
        })
        .map(|field| field.specifier)
        .collect();
    assert_eq!(
        specifiers,
        [Some("^4.1.2".to_string()), Some("4.1.0".to_string())]
    );
    assert_eq!(replaced.member_paths(), HashSet::from([".".to_string()]));
    Ok(())
}

/// `*` matches any run of characters and `?` exactly one, anchored at both ends.
#[test]
fn wildcards_match_whole_names() {
    for (pattern, name) in [
        ("*", "a"),
        ("*", ""),
        ("pkg-*", "pkg-a"),
        ("*-app", "web-app"),
        ("a*b*c", "aXbYc"),
        ("a*b*c", "abc"),
        ("app-?", "app-1"),
        ("??", "ab"),
    ] {
        assert!(wildcard_matches(pattern, name), "{pattern} {name}");
    }
    for (pattern, name) in [
        ("pkg-*", "lib-a"),
        ("*-app", "web-apps"),
        ("app-?", "app-10"),
        ("a*b*c", "acb"),
        ("??", "a"),
    ] {
        assert!(!wildcard_matches(pattern, name), "{pattern} {name}");
    }
}
