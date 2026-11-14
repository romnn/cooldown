//! The culprits a failed `go get` names.
//!
//! `go get` reports every version conflict of a joint resolve at once, one `go: …` line each, before
//! it writes `go.mod` (`cmd/go/internal/modget`, `updateBuildList`).
//! An argument cooldown passed as an exact `path@version` query is spelled exactly that way in those
//! lines, so a conflict names its arguments without any guessing.

/// One conflict of a failed `go get`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Conflict<'a> {
    /// `go: <requirer> [indirectly ]requires <path>@<needed>, not <constrained>`: the requirer, at
    /// the version requested for it, needs a newer version of a module than the one requested.
    Requirement {
        /// The requiring argument, as `path@version`.
        requirer: &'a str,
        /// The argument requesting the older version, as `path@version`.
        constrained: &'a str,
        /// Go's whole line, without the `go: ` prefix.
        line: &'a str,
    },
    /// `go: <argument> requires <argument>: parsing go.mod:` followed by `module declares its path
    /// as: …`: the requested version's `go.mod` declares another module path.
    ForeignPath {
        /// The argument, as `path@version`.
        argument: &'a str,
        /// Go's explanation, its lines joined by single spaces.
        detail: String,
    },
}

/// The conflicts in the stderr of a failed `go get`, in the order Go printed them.
/// Lines of any other shape are skipped.
pub(super) fn conflicts(stderr: &str) -> Vec<Conflict<'_>> {
    let lines: Vec<&str> = stderr.lines().collect();
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("go: ") else {
            continue;
        };
        if let Some(conflict) = requirement(rest) {
            found.push(conflict);
            continue;
        }
        // Go indents the continuation lines of a multi-line error with a tab.
        let continuation = lines
            .get(index + 1..)
            .unwrap_or_default()
            .iter()
            .copied()
            .take_while(|next| next.starts_with('\t'));
        if let Some(conflict) = foreign_path(rest, continuation) {
            found.push(conflict);
        }
    }
    found
}

/// A single `path@version` token, as Go spells an exact query.
fn exact_argument(token: &str) -> Option<&str> {
    let (path, version) = token.rsplit_once('@')?;
    let plain = !path.is_empty() && !version.is_empty() && !token.contains(char::is_whitespace);
    plain.then_some(token)
}

fn module_path(argument: &str) -> Option<&str> {
    argument.rsplit_once('@').map(|(path, _)| path)
}

fn requirement(rest: &str) -> Option<Conflict<'_>> {
    let (requirer, requirement) = rest
        .split_once(" indirectly requires ")
        .or_else(|| rest.split_once(" requires "))?;
    let (needed, constrained) = requirement.rsplit_once(", not ")?;
    let requirer = exact_argument(requirer)?;
    let needed = exact_argument(needed)?;
    let constrained = exact_argument(constrained)?;
    // The requested version and the needed one are versions of the same module.
    if module_path(needed)? != module_path(constrained)? {
        return None;
    }
    Some(Conflict::Requirement {
        requirer,
        constrained,
        line: rest,
    })
}

fn foreign_path<'a>(
    rest: &'a str,
    continuation: impl Iterator<Item = &'a str>,
) -> Option<Conflict<'a>> {
    let (argument, error) = rest.split_once(" requires ")?;
    let (required, error) = error.split_once(": ")?;
    let argument = exact_argument(argument)?;
    if required != argument || !error.starts_with("parsing go.mod:") {
        return None;
    }
    let mut detail = vec![error.trim()];
    detail.extend(continuation.map(str::trim));
    let detail = detail.join(" ");
    detail
        .contains("module declares its path as:")
        .then_some(Conflict::ForeignPath { argument, detail })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirement_conflicts_name_both_arguments() {
        // Go prints every conflict of one resolve, direct and indirect alike.
        let stderr = indoc::indoc! {"
            go: github.com/grpc-ecosystem/grpc-gateway/v2@v2.33.0 requires go.opentelemetry.io/otel/log@v1.47.0, not go.opentelemetry.io/otel/log@v0.22.0
            go: example.com/a@v1.2.0 indirectly requires example.com/b@v2.1.0+incompatible, not example.com/b@v2.0.0+incompatible
        "};
        assert_eq!(
            conflicts(stderr),
            [
                Conflict::Requirement {
                    requirer: "github.com/grpc-ecosystem/grpc-gateway/v2@v2.33.0",
                    constrained: "go.opentelemetry.io/otel/log@v0.22.0",
                    line: "github.com/grpc-ecosystem/grpc-gateway/v2@v2.33.0 requires go.opentelemetry.io/otel/log@v1.47.0, not go.opentelemetry.io/otel/log@v0.22.0",
                },
                Conflict::Requirement {
                    requirer: "example.com/a@v1.2.0",
                    constrained: "example.com/b@v2.0.0+incompatible",
                    line: "example.com/a@v1.2.0 indirectly requires example.com/b@v2.1.0+incompatible, not example.com/b@v2.0.0+incompatible",
                },
            ]
        );
    }

    #[test]
    fn a_foreign_module_path_names_the_argument() {
        // Go indents the explanation's continuation lines with a tab.
        let stderr = indoc::indoc! {"
            go: github.com/google/cel-go@v0.32.0 requires github.com/google/cel-go@v0.32.0: parsing go.mod:
            \tmodule declares its path as: cel.dev/cel-go
            \t        but was required as: github.com/google/cel-go
        "};
        assert_eq!(
            conflicts(stderr),
            [Conflict::ForeignPath {
                argument: "github.com/google/cel-go@v0.32.0",
                detail: "parsing go.mod: module declares its path as: cel.dev/cel-go but was required as: github.com/google/cel-go".to_string(),
            }]
        );
    }

    #[test]
    fn other_failures_name_nothing() {
        // A non-exact spelling, a mismatched module, a different error and plain output all stay
        // unattributed, so the caller falls back to bisection.
        let stderr = indoc::indoc! {"
            go: example.com/a@v1.2.0 (matching example.com/a/...@v1.2.0) requires example.com/b@v1.1.0, not example.com/b@v1.0.0
            go: example.com/a@v1.2.0 requires example.com/b@v1.1.0, not example.com/c@v1.0.0
            go: example.com/a@v1.2.0 requires example.com/a@v1.2.0: reading go.mod: 404 Not Found
            go: example.com/a@v1.2.0: invalid version: unknown revision v1.2.0
            # example.com/demo
            ./main.go:3:2: undefined: b.New
        "};
        assert_eq!(conflicts(stderr), []);
    }
}
