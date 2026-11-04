//! pnpm's frozen lock verification against a scripted `pnpm`, one script per way the verification
//! can fail: only pnpm's own stale-lock refusal is a stale lock, and every other failure is a tool
//! error that `--allow-stale-lock` never waives.
//! Unix-only because the fake manager binary is a shell script that needs unix permission bits.

use super::*;
use crate::lock::Pnpm;
use color_eyre::eyre;
use cooldown_core::LockStatus;
use indoc::{formatdoc, indoc};
use std::os::unix::fs::PermissionsExt as _;

/// Where a scripted failure prints its text.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// How a scripted `pnpm` answers the frozen verification.
struct Answer {
    /// The text pnpm prints.
    output: &'static str,
    stream: Stream,
    /// The shell that ends the script: an `exit` or a self-inflicted signal.
    ending: &'static str,
}

/// A failure that exits `1` after printing `output` on `stream`, as pnpm does for every error.
fn failing(output: &'static str, stream: Stream) -> Answer {
    Answer {
        output,
        stream,
        ending: "exit 1",
    }
}

/// A project in a temporary directory whose `pnpm` answers the verification with `answer`, and
/// logs its arguments to `legs.log`.
struct Scripted {
    /// Owns the temporary root; dropping it deletes the project and its script.
    _guard: tempfile::TempDir,
    project: Project,
    tool: NpmTool<Pnpm>,
}

fn scripted(answer: &Answer) -> eyre::Result<Scripted> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    let redirect = match answer.stream {
        Stream::Stdout => "",
        Stream::Stderr => " >&2",
    };
    let script = root.join("fake-pnpm.sh");
    // The quoted heredoc delimiter keeps the shell from expanding anything pnpm printed.
    std::fs::write(
        &script,
        formatdoc! {r#"
            #!/bin/sh
            echo "$*" >> legs.log
            cat <<'PNPM_OUTPUT'{redirect}
            {output}
            PNPM_OUTPUT
            {ending}
        "#,
            output = answer.output,
            ending = answer.ending,
        },
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let tool = NpmTool::<Pnpm>::from_http(cooldown_registry::SharedHttp::new(
        root.join("http-cache").as_std_path(),
        cooldown_registry::HttpOptions {
            offline: true,
            ..cooldown_registry::HttpOptions::default()
        },
    )?)
    .with_driver(crate::nodecmd::NodeCmd::with_bin(script.as_str()));
    let project = Project {
        root: root.clone(),
        kind: Pnpm::ID,
        manifest: root.join("package.json"),
        exclude_newer: None,
        exclude_newer_package: std::collections::BTreeMap::new(),
        generated_members: cooldown_core::GeneratedMembers::undeclared(),
    };
    Ok(Scripted {
        _guard: guard,
        project,
        tool,
    })
}

/// pnpm 11.8.0's refusal of a lock that misses a newly declared dependency.
const OUTDATED_11: &str = r#"[ERR_PNPM_OUTDATED_LOCKFILE] Cannot install with "frozen-lockfile" because pnpm-lock.yaml is not up to date with <ROOT>/package.json"#;

/// pnpm 10.34.9's header for the same refusal, padded with thin spaces.
const OUTDATED_10: &str = "\u{2009}ERR_PNPM_OUTDATED_LOCKFILE\u{2009} Cannot install with \"frozen-lockfile\" because pnpm-lock.yaml is not up to date with <ROOT>/package.json";

/// A passing verification proves the lock current, and runs pnpm with its output settings pinned
/// so the user's configuration cannot hide an error code.
#[tokio::test]
async fn a_passing_verification_is_current() -> eyre::Result<()> {
    let scripted = scripted(&Answer {
        output: "Done in 77ms using pnpm v11.8.0",
        stream: Stream::Stdout,
        ending: "exit 0",
    })?;

    let report = scripted.tool.verify_lock_current(&scripted.project).await?;

    assert_eq!(report.status, LockStatus::Current);
    let legs = std::fs::read_to_string(scripted.project.root.join("legs.log"))?;
    assert_eq!(
        legs.trim(),
        "install --lockfile-only --frozen-lockfile --reporter=append-only --loglevel=error \
         --config.json=false --config.parseable=false"
    );
    Ok(())
}

/// Each of pnpm's stale-lock refusals, in the header shape of pnpm 11 and of pnpm 9 and 10, is a
/// stale lock whose detail names the lockfile ahead of pnpm's own text.
#[tokio::test]
async fn pnpm_stale_lock_refusals_are_stale() -> eyre::Result<()> {
    let refusals = [
        OUTDATED_11,
        OUTDATED_10,
        r#"[ERR_PNPM_LOCKFILE_CONFIG_MISMATCH] Cannot proceed with the frozen installation. The current "overrides" configuration doesn't match the value found in the lockfile"#,
        r#"[ERR_PNPM_NO_LOCKFILE] Cannot install with "frozen-lockfile" because pnpm-lock.yaml is absent"#,
        "[ERR_PNPM_FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE] Cannot perform a frozen installation because the version of the lockfile is incompatible with this version of pnpm",
        // A refusal naming a dependency that spells a broken-environment phrase is still stale.
        indoc! {r#"
            [ERR_PNPM_OUTDATED_LOCKFILE] Cannot install with "frozen-lockfile" because pnpm-lock.yaml is not up to date with <ROOT>/package.json

              Failure reason:
              specifiers in the lockfile don't match specifiers in package.json:
            * 1 dependencies were added: eacces@^1.0.0
        "#},
    ];
    for refusal in refusals {
        let scripted = scripted(&failing(refusal, Stream::Stdout))?;

        let report = scripted.tool.verify_lock_current(&scripted.project).await?;

        assert_eq!(report.status, LockStatus::Stale, "{refusal}");
        assert!(
            report.detail.starts_with("pnpm-lock.yaml is stale in ")
                && report.detail.contains(refusal.trim()),
            "{}",
            report.detail
        );
    }
    Ok(())
}

/// Every failure that does not prove the lock stale is a tool error carrying pnpm's own text.
#[tokio::test]
async fn other_verification_failures_are_tool_errors() -> eyre::Result<()> {
    let failures = [
        (
            "network",
            failing(
                "[ERR_PNPM_META_FETCH_FAIL] GET https://registry.npmjs.org/is-number: request to https://registry.npmjs.org/is-number failed, reason: getaddrinfo ENOTFOUND registry.npmjs.org",
                Stream::Stdout,
            ),
        ),
        (
            "authentication",
            failing(
                "[ERR_PNPM_FETCH_401] GET https://npm.example.com/private: Unauthorized - 401",
                Stream::Stdout,
            ),
        ),
        (
            "configuration",
            failing(
                "[ERROR] missed comma between flow collection entries (2:3)",
                Stream::Stderr,
            ),
        ),
        (
            "unsupported flag",
            failing(
                "\u{2009}ERROR\u{2009} Unknown option: 'reporter'",
                Stream::Stderr,
            ),
        ),
        (
            "corrupted store",
            failing(
                r#"[ERR_PNPM_TARBALL_INTEGRITY] Got unexpected checksum for "https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz""#,
                Stream::Stdout,
            ),
        ),
        ("silent failure", failing("", Stream::Stdout)),
        // A stale refusal printed by a run that then ended through an interruption proves nothing.
        (
            "interrupted",
            Answer {
                output: OUTDATED_11,
                stream: Stream::Stdout,
                ending: "exit 130",
            },
        ),
        (
            "killed",
            Answer {
                output: OUTDATED_11,
                stream: Stream::Stdout,
                ending: "kill -TERM $$",
            },
        ),
    ];
    for (kind, answer) in failures {
        let scripted = scripted(&answer)?;

        let result = scripted.tool.verify_lock_current(&scripted.project).await;

        match result {
            Err(CoreError::Tool { tool, stderr, .. }) => {
                assert!(tool.ends_with("fake-pnpm.sh"), "{kind}: {tool}");
                assert!(stderr.contains(answer.output.trim()), "{kind}: {stderr}");
            }
            other => eyre::bail!("{kind}: expected a tool error, got {other:?}"),
        }
    }
    Ok(())
}
