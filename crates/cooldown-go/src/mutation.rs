use crate::semver;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{Change, CoreError, Plan, ProjectMutationJournal, Result};
use std::collections::HashSet;

pub(crate) fn mutation_journal(root: &Utf8Path, plan: &Plan) -> Result<ProjectMutationJournal> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for rel in [Utf8Path::new("go.mod"), Utf8Path::new("go.sum")] {
        if let Some(path) = capture_once(rel, &mut seen) {
            paths.push(path);
        }
    }
    for change in &plan.changes {
        let Some(old_path) = old_import_path(change) else {
            continue;
        };
        if old_path == change.package.name {
            continue;
        }
        capture_import_targets(root, root, &old_path, &mut seen, &mut paths)?;
    }
    ProjectMutationJournal::capture(root, paths)
}

pub(crate) fn rewrite_imports(
    root: &Utf8Path,
    old: &str,
    new: &str,
    journal: &ProjectMutationJournal,
) -> Result<usize> {
    let mut count = 0;
    for file in journal.files() {
        if file.path().extension() != Some("go") {
            continue;
        }
        let path = root.join(file.path());
        let src = std::fs::read_to_string(&path)?;
        let replaced = rewrite_import_path(&src, old, new);
        if replaced != src {
            std::fs::write(&path, replaced)?;
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn old_import_path(change: &Change) -> Option<String> {
    let new_path = &change.package.name;
    let split = semver::split_path_version(new_path);
    if !split.well_formed {
        return None;
    }
    // A `+incompatible` module (e.g. `github.com/docker/cli`) tags its v2+ majors on the bare base
    // path, so a move that stays there — the common within-line bump, or a drop back to a
    // compatible v0/v1 — has nothing to rewrite, and synthesizing a `…/vN` old path from the
    // version's major would trigger a spurious import-tree scan. The one `+incompatible` move that
    // does change the path is landing on a discovered `…/vN` target (the module adopted
    // module-style versioning, so `discover_major_paths` found the suffixed path): the old import
    // path is then the base prefix the `+incompatible` line lived on — without it no import moves,
    // `go mod tidy` drops the unreferenced new require, and the skip is misreported under the
    // target identity. The current pin's `+incompatible` marker is what identifies the line; the
    // new path's suffix cannot, since a legitimate downgrade *to* the v1 base path also has none.
    if semver::is_incompatible(change.from.as_str()) {
        return (!split.path_major.is_empty()).then_some(split.prefix);
    }
    // The old import path is the one for `from`'s major (`new_path` already encodes `to`'s major); v1
    // lives at the base path — except on gopkg.in, which encodes *every* major in the path (v0/v1
    // import as `prefix.v0`/`prefix.v1`, never the bare prefix; the same carve-out
    // `discover_lower_major_paths` makes). A move that does not change the path (same major, or a
    // base↔base patch) has nothing to rewrite.
    let from_major = semver::major(change.from.as_str());
    let n: u32 = from_major.trim_start_matches('v').parse().ok()?;
    let old = if n <= 1 && !split.prefix.starts_with("gopkg.in/") {
        split.prefix
    } else {
        semver::major_path(&split.prefix, n)
    };
    (&old != new_path).then_some(old)
}

fn capture_import_targets(
    root: &Utf8Path,
    dir: &Utf8Path,
    old: &str,
    seen: &mut HashSet<Utf8PathBuf>,
    out: &mut Vec<Utf8PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // Skip exactly what the `go` tool itself ignores in the parent module's package walk:
        // directories whose names begin with `.` or `_` (so `.git`, `_data` container volumes,
        // `_cache`, … are never descended into), `vendor` and `testdata`, symlinked directories,
        // and any subdirectory carrying its own `go.mod` (a nested module, pruned below). Beyond
        // matching Go's own package discovery, this keeps the scan off non-source trees that may
        // be large or unreadable (e.g. a Docker volume owned by another user), which would
        // otherwise fail the whole upgrade — as would a captured symlink or other non-regular
        // file, which `ProjectMutationJournal::capture` refuses to journal.
        let file_type = entry.file_type()?;
        // Signed-off trade-off: a symlinked *`.go` file* inside a package directory is NOT
        // ignored by `go build` (it compiles like any regular source file), yet the scan skips
        // it, so a cross-major apply can leave such a file importing the old major. Capturing it
        // would be worse: `ProjectMutationJournal::capture` refuses non-regular files because
        // restore would write through the alias and clobber the link target outside the journal's
        // control — main's old write-through-symlink behavior broke both directions.
        // (`entry.file_type()`, unlike `Path::is_dir`, does not follow links, so symlinked
        // directories and files both land here.)
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || name.starts_with('_')
                || matches!(name.as_ref(), "vendor" | "testdata")
            {
                continue;
            }
            let child = utf8_path(path)?;
            // A subdirectory with its own `go.mod` is a nested module: `go` prunes it from the
            // parent module's package walk, and cooldown plans it as its own project with its own
            // journal. Rewriting its files from here would advance a sibling project's imports to
            // `…/vN` while that project's `go.mod` still requires the old major — a committed
            // build break in another project with no rollback, since this apply succeeds. Only
            // subdirectories are pruned; the scan root holds the module's own `go.mod`.
            if child.join("go.mod").is_file() {
                continue;
            }
            capture_import_targets(root, &child, old, seen, out)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("go") {
            continue;
        }
        let utf8 = utf8_path(path)?;
        let source = std::fs::read_to_string(&utf8)?;
        if !contains_import_target(&source, old) {
            continue;
        }
        let rel = utf8
            .strip_prefix(root)
            .map_err(|e| CoreError::PathEncoding(format!("{utf8}: {e}")))?;
        if let Some(path) = capture_once(rel, seen) {
            out.push(path);
        }
    }
    Ok(())
}

fn capture_once(rel: &Utf8Path, seen: &mut HashSet<Utf8PathBuf>) -> Option<Utf8PathBuf> {
    let rel = rel.to_owned();
    if !seen.insert(rel.clone()) {
        return None;
    }
    Some(rel)
}

fn utf8_path(path: std::path::PathBuf) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path)
        .map_err(|path| CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display())))
}

fn contains_import_target(source: &str, old: &str) -> bool {
    source.contains(&format!("\"{old}\"")) || source.contains(&format!("\"{old}/"))
}

/// Rewrites each import-path string at or below the `old` module path onto `new`, token by token,
/// in one lexical pass.
///
/// Sequential `str::replace` passes cannot express this: on a base → `/vN` move (`new` extends
/// `old`) the exact-form pass emits `"…/vN"`, whose `"{old}/` prefix the subpackage pass then
/// re-matches, double-appending the suffix (`…/v2/v2`). Per-token matching also keeps the rewrite
/// idempotent — a token already at the target is recognized and left alone instead of compounding.
///
/// The pass tracks Go's lexical structure — interpreted strings (with escapes), raw strings, rune
/// literals, line and block comments — so an unmatched quote inside a comment cannot flip which
/// spans look quoted for the rest of the file, which is exactly what global `split('"')` parity
/// got wrong (every later import silently skipped while go.mod moved to the new major). Comments
/// and raw strings still have their *locally balanced* `"…"` tokens rewritten, deliberately
/// matching [`contains_import_target`]: a commented-out import kept in sync is more useful than
/// one silently left on a module path the build no longer resolves.
fn rewrite_import_path(source: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => rewrite_interpreted_string(&mut chars, &mut out, old, new),
            '\'' => copy_rune_literal(&mut chars, &mut out),
            '`' => rewrite_raw_string(&mut chars, &mut out, old, new),
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                rewrite_line_comment(&mut chars, &mut out, old, new);
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                rewrite_block_comment(&mut chars, &mut out, old, new);
            }
            _ => out.push(ch),
        }
    }
    out
}

type Cursor<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// An interpreted string literal (opening quote already consumed): its content is one candidate
/// token. Escapes (`\"`, `\\`) never terminate the literal; a token carrying any escape cannot
/// match a module path, so the content round-trips verbatim in that case. An unterminated literal
/// (malformed Go) is copied through untouched.
fn rewrite_interpreted_string(chars: &mut Cursor<'_>, out: &mut String, old: &str, new: &str) {
    let mut content = String::new();
    let mut closed = false;
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            content.push(ch);
            if let Some(escaped) = chars.next() {
                content.push(escaped);
            }
            continue;
        }
        if ch == '"' {
            closed = true;
            break;
        }
        content.push(ch);
    }
    out.push('"');
    match rewrite_token(&content, old, new).filter(|_| closed) {
        Some(rewritten) => out.push_str(&rewritten),
        None => out.push_str(&content),
    }
    if closed {
        out.push('"');
    }
}

/// A rune literal (opening quote already consumed), copied verbatim through its closing quote so a
/// quote rune (`'"'`) is never mistaken for a string delimiter. Escapes are honored (`'\''`).
fn copy_rune_literal(chars: &mut Cursor<'_>, out: &mut String) {
    out.push('\'');
    while let Some(ch) = chars.next() {
        out.push(ch);
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
            continue;
        }
        if ch == '\'' {
            break;
        }
    }
}

/// A raw string literal (opening backtick already consumed). Raw strings have no escapes and may
/// span lines and contain quotes; the locally balanced quoted tokens inside are rewritten like
/// comment text so generated-code templates keep working, while the quotes stay invisible to the
/// surrounding pass.
fn rewrite_raw_string(chars: &mut Cursor<'_>, out: &mut String, old: &str, new: &str) {
    let mut content = String::new();
    let mut closed = false;
    for ch in chars.by_ref() {
        if ch == '`' {
            closed = true;
            break;
        }
        content.push(ch);
    }
    out.push('`');
    if closed {
        out.push_str(&rewrite_quoted_tokens(&content, old, new));
        out.push('`');
    } else {
        out.push_str(&content);
    }
}

/// A `//` comment (both slashes already consumed), running to end of line or EOF.
fn rewrite_line_comment(chars: &mut Cursor<'_>, out: &mut String, old: &str, new: &str) {
    let mut content = String::new();
    while let Some(&ch) = chars.peek() {
        if ch == '\n' {
            break;
        }
        content.push(ch);
        chars.next();
    }
    out.push_str("//");
    out.push_str(&rewrite_quoted_tokens(&content, old, new));
}

/// A `/* */` comment (the opener already consumed). Go block comments do not nest. An unterminated
/// comment (malformed Go) is copied through untouched.
fn rewrite_block_comment(chars: &mut Cursor<'_>, out: &mut String, old: &str, new: &str) {
    let mut content = String::new();
    let mut closed = false;
    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek() == Some(&'/') {
            chars.next();
            closed = true;
            break;
        }
        content.push(ch);
    }
    out.push_str("/*");
    if closed {
        out.push_str(&rewrite_quoted_tokens(&content, old, new));
        out.push_str("*/");
    } else {
        out.push_str(&content);
    }
}

/// Rewrites the locally balanced `"…"` tokens inside comment or raw-string text. Pairing is
/// confined to this span, so an aside with an unpaired quote (`// "unfinished example`) leaves its
/// own tail — and, unlike the old global parity, the rest of the file — untouched.
fn rewrite_quoted_tokens(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut segments = text.split('"');
    if let Some(leading) = segments.next() {
        out.push_str(leading);
    }
    while let Some(token) = segments.next() {
        let Some(following) = segments.next() else {
            // A final unpaired quote: what follows it is prose, not a token.
            out.push('"');
            out.push_str(token);
            break;
        };
        out.push('"');
        match rewrite_token(token, old, new) {
            Some(rewritten) => out.push_str(&rewritten),
            None => out.push_str(token),
        }
        out.push('"');
        out.push_str(following);
    }
    out
}

/// The rewritten form of one quoted token, or `None` when it is not an import at/below `old` that
/// still needs to move.
fn rewrite_token(token: &str, old: &str, new: &str) -> Option<String> {
    let below = |path: &str| {
        token == path
            || token
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    // A token below both paths is ambiguous only lexically; the more specific (longer) path wins.
    // On a base → `/vN` upgrade a token already at `…/vN` is read as already at the target. A v1
    // module that itself ships a literal `vN/` package directory would be misread by that rule,
    // but the layout is ambiguous by construction — once module `…/vN` exists the same import
    // path resolves to it — and telling the cases apart needs the dependency's directory listing,
    // which this lexical pass does not have. Preferring the already-rewritten reading keeps repeat
    // rewrite rounds idempotent. On a `/vN` → base downgrade the token at `…/vN` is still the old
    // form and must move.
    if below(new) && (!below(old) || new.len() >= old.len()) {
        return None;
    }
    if token == old {
        return Some(new.to_string());
    }
    token
        .strip_prefix(old)
        .filter(|rest| rest.starts_with('/'))
        .map(|rest| format!("{new}{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::{PackageId, ToolId, UpdateKind, Version};

    fn change(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(ToolId("go"), name, None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    #[test]
    fn incompatible_module_has_no_import_path_to_rewrite() {
        // A `+incompatible` module (a v2+ major that never adopted `/vN` paths, e.g.
        // github.com/docker/cli) stays on one import path across its majors, so a within-line bump
        // must not synthesize a bogus `…/v29` old path — doing so would trigger a spurious,
        // potentially failing import-tree scan.
        assert_eq!(
            old_import_path(&change(
                "github.com/docker/cli",
                "v29.2.1+incompatible",
                "v29.5.2+incompatible",
            )),
            None,
        );
    }

    #[test]
    fn incompatible_move_onto_a_versioned_path_rewrites_from_the_base_path() {
        // The one `+incompatible` case where the import path DOES change: the module adopted
        // module-style versioning, so `discover_major_paths` (which probes `…/vN` regardless of
        // the marker) found a suffixed target. The old path is the bare base the `+incompatible`
        // line imported as; returning None here would leave every import on the base path, let
        // `go mod tidy` drop the unreferenced `…/v3` require, and misreport the resulting skip
        // under the target identity.
        assert_eq!(
            old_import_path(&change(
                "example.com/foo/v3",
                "v2.9.0+incompatible",
                "v3.0.2"
            ))
            .as_deref(),
            Some("example.com/foo"),
        );
    }

    #[test]
    fn incompatible_downgrade_to_the_base_path_stays_path_stable() {
        // Rolling a `+incompatible` pin back to a compatible v1 keeps the bare base import path
        // on both sides — nothing to rewrite, and no bogus `…/v29` old path may be synthesized
        // from the from-version's major.
        assert_eq!(
            old_import_path(&change("example.com/foo", "v29.2.1+incompatible", "v1.5.0")),
            None,
        );
    }

    #[test]
    fn import_scan_prunes_nested_module_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 root");
        let src = "package x\nimport \"example.com/foo/v2\"\n";
        // The scan root carries the parent module's own `go.mod`; only *sub*directories with a
        // `go.mod` are module boundaries to prune.
        std::fs::write(root.join("go.mod"), "module example.com/parent\n").expect("write go.mod");
        std::fs::write(root.join("main.go"), src).expect("write main.go");
        std::fs::create_dir_all(root.join("pkg")).expect("mkdir pkg");
        std::fs::write(root.join("pkg").join("dep.go"), src).expect("write pkg dep.go");
        // `nested` is its own module: `go` prunes it from the parent's package walk and cooldown
        // plans it as its own project. Capturing (and so rewriting) its matching import from the
        // parent's apply would break that sibling project with no rollback.
        std::fs::create_dir_all(root.join("nested")).expect("mkdir nested");
        std::fs::write(
            root.join("nested").join("go.mod"),
            "module example.com/parent/nested\n",
        )
        .expect("write nested go.mod");
        std::fs::write(root.join("nested").join("dep.go"), src).expect("write nested dep.go");

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        capture_import_targets(root, root, "example.com/foo/v2", &mut seen, &mut out)
            .expect("scan succeeds");

        let mut captured: Vec<&str> = out.iter().map(|path| path.as_str()).collect();
        captured.sort_unstable();
        // Captured paths stay in the platform's own spelling because they are journal keys the
        // restore reopens, so the expectation is built with `join` rather than a `/` literal.
        let nested_source = Utf8Path::new("pkg").join("dep.go");
        assert_eq!(
            captured,
            vec!["main.go", nested_source.as_str()],
            "the parent module's files are captured; the nested module's are not"
        );
    }

    #[test]
    fn import_scan_skips_dot_underscore_vendor_and_testdata_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 root");
        let src = "package x\nimport \"example.com/foo/v2\"\n";
        std::fs::write(root.join("main.go"), src).expect("write main.go");
        // Each of these directories would match the import too, but `go` (and so cooldown) ignores
        // them — `_data` stands in for an unreadable Docker volume that must not break the scan.
        for skip in ["_data", ".git", "vendor", "testdata"] {
            std::fs::create_dir_all(root.join(skip)).expect("mkdir");
            std::fs::write(root.join(skip).join("x.go"), src).expect("write skipped .go");
        }

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        capture_import_targets(root, root, "example.com/foo/v2", &mut seen, &mut out)
            .expect("scan succeeds");

        let captured: Vec<&str> = out.iter().map(|path| path.as_str()).collect();
        assert_eq!(
            captured,
            vec!["main.go"],
            "only top-level source is captured"
        );
    }

    #[cfg(unix)]
    #[test]
    fn import_scan_skips_symlinked_dirs_and_files() {
        use std::os::unix::fs::symlink;
        // `go` never traverses symlinks during package discovery, and
        // `ProjectMutationJournal::capture` refuses symlinked ancestors and non-regular final
        // paths — a scan that followed either symlink here would hard-fail the whole upgrade.
        let target = tempfile::tempdir().expect("target tempdir");
        let target_root = Utf8Path::from_path(target.path()).expect("utf8 target");
        let src = "package x\nimport \"example.com/foo\"\n";
        std::fs::create_dir(target_root.join("pkg")).expect("mkdir pkg");
        std::fs::write(target_root.join("pkg").join("dep.go"), src).expect("write dep.go");
        std::fs::write(target_root.join("linked.go"), src).expect("write linked.go");

        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 root");
        std::fs::write(root.join("main.go"), src).expect("write main.go");
        // Neither link name trips the `.`/`_` name skip, so only the symlink check excludes them.
        symlink(target_root.join("pkg"), root.join("linkdir")).expect("symlink dir");
        symlink(target_root.join("linked.go"), root.join("link.go")).expect("symlink file");

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        capture_import_targets(root, root, "example.com/foo", &mut seen, &mut out)
            .expect("scan succeeds");

        let captured: Vec<&str> = out.iter().map(|path| path.as_str()).collect();
        assert_eq!(
            captured,
            vec!["main.go"],
            "symlinked directory contents and symlinked files are never captured"
        );
    }

    #[test]
    fn base_to_v2_rewrite_does_not_double_the_version_suffix() {
        // The luup5 armsubscriptions failure: with `new = old + "/v2"`, sequential replaces
        // re-matched their own output and produced `…/armsubscriptions/v2/v2`.
        let old =
            "github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/resources/armsubscriptions";
        let new =
            "github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/resources/armsubscriptions/v2";
        let source = format!("import (\n\t\"{old}\"\n\t\"{old}/fake\"\n)\n");
        let rewritten = rewrite_import_path(&source, old, new);
        assert_eq!(
            rewritten,
            format!("import (\n\t\"{new}\"\n\t\"{new}/fake\"\n)\n"),
        );
        // Idempotent: a second pass over already-rewritten source changes nothing.
        assert_eq!(rewrite_import_path(&rewritten, old, new), rewritten);
    }

    #[test]
    fn v2_to_base_downgrade_still_rewrites_the_versioned_path() {
        // The `fix` direction: `old` extends `new`, so the specificity rule must not mistake the
        // still-versioned token for one already at the (shorter) target.
        let source = "import (\n\t\"example.com/foo/v2\"\n\t\"example.com/foo/v2/sub\"\n)\n";
        let rewritten = rewrite_import_path(source, "example.com/foo/v2", "example.com/foo");
        assert_eq!(
            rewritten,
            "import (\n\t\"example.com/foo\"\n\t\"example.com/foo/sub\"\n)\n",
        );
        assert_eq!(
            rewrite_import_path(&rewritten, "example.com/foo/v2", "example.com/foo"),
            rewritten,
        );
    }

    #[test]
    fn commented_imports_are_rewritten_in_sync() {
        // Quote-delimited matching reaches commented-out imports on purpose, so revived code does
        // not resurrect a module path the build no longer resolves.
        let source = "// \t\"example.com/foo\"\n";
        assert_eq!(
            rewrite_import_path(source, "example.com/foo", "example.com/foo/v2"),
            "// \t\"example.com/foo/v2\"\n",
        );
    }

    #[test]
    fn unmatched_comment_quote_does_not_shift_later_imports() {
        // The parity regression: global `split('"')` counted the stray quote in the comment, so
        // every later import span landed at an even index and was silently skipped while go.mod
        // moved to the new major — a guaranteed build break.
        let source = indoc::indoc! {r#"
            package demo
            // "unfinished example
            import "example.com/foo"
        "#};
        let expected = indoc::indoc! {r#"
            package demo
            // "unfinished example
            import "example.com/foo/v2"
        "#};
        assert_eq!(
            rewrite_import_path(source, "example.com/foo", "example.com/foo/v2"),
            expected,
        );
    }

    #[test]
    fn escaped_quotes_and_quote_runes_do_not_break_the_lexer() {
        // `\"` inside an interpreted string and a `'"'` rune are not string delimiters; the
        // import after them must still be found, and the literals must round-trip verbatim.
        let source = indoc::indoc! {r#"
            package demo

            const banner = "say \"hi\""
            const quote = '"'
            import "example.com/foo"
        "#};
        let expected = indoc::indoc! {r#"
            package demo

            const banner = "say \"hi\""
            const quote = '"'
            import "example.com/foo/v2"
        "#};
        assert_eq!(
            rewrite_import_path(source, "example.com/foo", "example.com/foo/v2"),
            expected,
        );
    }

    #[test]
    fn raw_strings_and_block_comments_keep_their_own_quotes_local() {
        // A struct tag's raw string carries quotes that must stay invisible to the surrounding
        // pass, while a balanced commented-out import inside a block comment is still kept in
        // sync (the same deliberate behavior as line comments).
        let source = indoc::indoc! {r#"
            package demo

            /* once imported:
               "example.com/foo/sub"
            */
            type T struct {
                Name string `json:"name"`
            }

            import "example.com/foo"
        "#};
        let expected = indoc::indoc! {r#"
            package demo

            /* once imported:
               "example.com/foo/v2/sub"
            */
            type T struct {
                Name string `json:"name"`
            }

            import "example.com/foo/v2"
        "#};
        assert_eq!(
            rewrite_import_path(source, "example.com/foo", "example.com/foo/v2"),
            expected,
        );
    }

    #[test]
    fn unrelated_and_prefix_named_imports_are_untouched() {
        // `example.com/foobar` shares the textual prefix but is a different module; only a `/`
        // boundary continues the module path.
        let source = "import (\n\t\"example.com/foobar\"\n\t\"other.dev/x\"\n)\n";
        assert_eq!(
            rewrite_import_path(source, "example.com/foo", "example.com/foo/v2"),
            source,
        );
    }

    #[test]
    fn versioned_module_rewrites_from_the_old_major_path() {
        // A real `/vN` module crossing majors: the old import path drops to the `from` major.
        assert_eq!(
            old_import_path(&change("example.com/foo/v3", "v2.4.0", "v3.0.0")).as_deref(),
            Some("example.com/foo/v2"),
        );
        // From v1, the old path is the unversioned base.
        assert_eq!(
            old_import_path(&change("example.com/foo/v2", "v1.5.0", "v2.0.0")).as_deref(),
            Some("example.com/foo"),
        );
    }

    #[test]
    fn gopkg_in_low_majors_keep_their_versioned_import_path() {
        // gopkg.in has no unversioned base path: v0/v1 import as `prefix.v0`/`prefix.v1`
        // (`gopkg.in/yaml.v1`, never `gopkg.in/yaml`). Scanning for the bare prefix would match
        // no import, so the rewrite would silently no-op and the tidied-away require would be
        // misreported as a resolver-conflict skip.
        assert_eq!(
            old_import_path(&change("gopkg.in/yaml.v3", "v1.2.7", "v3.0.1")).as_deref(),
            Some("gopkg.in/yaml.v1"),
        );
        assert_eq!(
            old_import_path(&change("gopkg.in/pkg.v2", "v0.3.0", "v2.0.0")).as_deref(),
            Some("gopkg.in/pkg.v0"),
        );
        // A within-major gopkg.in bump changes no path — the bare prefix must not be synthesized
        // as a bogus old path either.
        assert_eq!(
            old_import_path(&change("gopkg.in/yaml.v1", "v1.2.0", "v1.3.0")),
            None,
        );
    }

    #[test]
    fn gopkg_in_cross_major_rewrites_the_dotted_old_path() {
        // End to end for gopkg.in v1 → v3: the old path derived from the change drives the same
        // rewrite pass as `/vN` modules, moving the dotted `.v1` import (and its subpackages) to
        // `.v3`.
        let change = change("gopkg.in/yaml.v3", "v1.2.7", "v3.0.1");
        let old = old_import_path(&change).expect("cross-major move has an old path");
        let source =
            "import (\n\t\"gopkg.in/yaml.v1\"\n\t\"gopkg.in/yaml.v1/sub\"\n\t\"other.dev/x\"\n)\n";
        let rewritten = rewrite_import_path(source, &old, &change.package.name);
        assert_eq!(
            rewritten,
            "import (\n\t\"gopkg.in/yaml.v3\"\n\t\"gopkg.in/yaml.v3/sub\"\n\t\"other.dev/x\"\n)\n",
        );
    }
}
