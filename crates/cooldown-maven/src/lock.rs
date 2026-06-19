//! Parsing the two Java dependency surfaces cooldown reads: Maven's `pom.xml` (declared
//! dependencies, the direct set — transitive resolution would need `mvn`) and Gradle's
//! `gradle.lockfile` (the fully resolved graph from Gradle's dependency-locking feature), with
//! `build.gradle` supplying the direct/transitive split for the latter.
//!
//! Coordinates are normalised to `group:artifact`, which is how the Maven Central registry and the
//! cooldown [`PackageId`](cooldown_core::PackageId) name them.

use std::collections::HashSet;

/// Extracts the text content of the first `<tag>…</tag>` within `block`.
fn tag_value<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let rest = block.get(start..)?;
    let end = rest.find(&close)?;
    Some(rest.get(..end)?.trim())
}

/// Parses the `<dependency>` entries of a `pom.xml` into `(group:artifact, version)`. Dependencies
/// whose version is absent or a `${property}`/BOM-managed placeholder are skipped, since cooldown
/// cannot resolve those to a concrete version without running Maven.
#[must_use]
pub fn parse_pom(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<dependency>") {
        let after = &rest[start..];
        let end = after
            .find("</dependency>")
            .map_or(after.len(), |e| e + "</dependency>".len());
        let block = &after[..end];
        if let (Some(group), Some(artifact), Some(version)) = (
            tag_value(block, "groupId"),
            tag_value(block, "artifactId"),
            tag_value(block, "version"),
        ) && !version.contains("${")
        {
            out.push((format!("{group}:{artifact}"), version.to_string()));
        }
        rest = &after[end..];
    }
    out
}

/// Parses a `gradle.lockfile` into resolved `(group:artifact, version)`. Each non-comment line is
/// `group:artifact:version=configurations`; the trailing `empty=` line and `#` comments are
/// skipped.
#[must_use]
pub fn parse_gradle_lock(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("empty=") {
            continue;
        }
        let coord = line.split('=').next().unwrap_or("");
        // coord == group:artifact:version — split the version off the right.
        if let Some((ga, version)) = coord.rsplit_once(':')
            && ga.contains(':')
            && !version.is_empty()
        {
            out.push((ga.to_string(), version.to_string()));
        }
    }
    out
}

/// Returns the `group:artifact` coordinates declared directly in a `build.gradle(.kts)`: the
/// dependency-string literals (`'group:artifact:version'`) found in the build script.
#[must_use]
pub fn parse_gradle_direct(content: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for literal in quoted_literals(content) {
        let mut parts = literal.splitn(3, ':');
        if let (Some(group), Some(artifact)) = (parts.next(), parts.next())
            && !group.is_empty()
            && !artifact.is_empty()
        {
            out.insert(format!("{group}:{artifact}"));
        }
    }
    out
}

/// Yields the contents of every single- or double-quoted string literal in `src`.
fn quoted_literals(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while let Some(&q) = bytes.get(i) {
        if (q == b'\'' || q == b'"')
            && let Some(rel) = src.get(i + 1..).and_then(|s| s.find(q as char))
        {
            if let Some(inner) = src.get(i + 1..i + 1 + rel) {
                out.push(inner);
            }
            i += rel + 2;
            continue;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pom_dependencies_skip_property_versions() {
        let pom = "<project><dependencies>\
            <dependency><groupId>com.google.code.gson</groupId><artifactId>gson</artifactId><version>2.8.0</version></dependency>\
            <dependency><groupId>org.slf4j</groupId><artifactId>slf4j-api</artifactId><version>${slf4j.version}</version></dependency>\
            </dependencies></project>";
        assert_eq!(
            parse_pom(pom),
            vec![("com.google.code.gson:gson".to_string(), "2.8.0".to_string())]
        );
    }

    #[test]
    fn gradle_lock_and_direct() {
        let lock = "# This is a Gradle generated file...\ncom.google.code.gson:gson:2.8.0=compileClasspath\norg.slf4j:slf4j-api:1.7.30=compileClasspath\nempty=\n";
        let mut got = parse_gradle_lock(lock);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("com.google.code.gson:gson".to_string(), "2.8.0".to_string()),
                ("org.slf4j:slf4j-api".to_string(), "1.7.30".to_string()),
            ]
        );

        let build = "dependencies {\n  implementation 'com.google.code.gson:gson:2.8.0'\n}";
        let direct = parse_gradle_direct(build);
        assert!(direct.contains("com.google.code.gson:gson"));
        assert!(!direct.contains("org.slf4j:slf4j-api"));
    }
}
