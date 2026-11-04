use super::captures::{
    AUTH_BEFORE_VERDICT, CURRENT, CURRENT_AUTH_AFTER_VERDICT, CURRENT_FETCH_BLOCKED, MALFORMED_RC,
    NETWORK_NEEDED, SERVER_ERROR_BEFORE_VERDICT, STALE_FILE_FOLDER, STALE_MEMBER,
    STALE_REMOVED_RESOLUTION, STALE_SCHEMA, YARN2_CURRENT, YARN2_STALE, YARN3_CURRENT, YARN3_STALE,
};
use super::{Verdict, classify};
use indoc::indoc;

fn diff(stdout: &str) -> Vec<String> {
    match classify(stdout) {
        Verdict::Stale { diff } => diff,
        other => panic!("expected a stale verdict, got {other:?}"),
    }
}

fn errors(stdout: &str) -> Vec<String> {
    match classify(stdout) {
        Verdict::Undecided { errors } => errors,
        other => panic!("expected an undecided verdict, got {other:?}"),
    }
}

/// Every release line proves a current lock by reaching the fetch step, Yarn 2 included,
/// which prints no version line ahead of its steps.
#[test]
fn reaching_the_fetch_step_proves_the_lock_current() {
    for stdout in [CURRENT, YARN2_CURRENT, YARN3_CURRENT] {
        assert_eq!(classify(stdout), Verdict::Current);
    }
}

/// A fetch that fails after the lock check passed leaves the lock current, although Yarn exits
/// 1: the offline run blocks every tarball, and a registry may refuse anonymous downloads.
#[test]
fn a_failure_after_the_fetch_step_began_keeps_the_lock_current() {
    assert_eq!(classify(CURRENT_FETCH_BLOCKED), Verdict::Current);
    assert_eq!(classify(CURRENT_AUTH_AFTER_VERDICT), Verdict::Current);
}

/// Every `YN0028` line is the stale verdict, and the diff is quoted in Yarn's order.
#[test]
fn every_lockfile_change_yarn_reports_makes_the_lock_stale() {
    let member = diff(STALE_MEMBER);
    assert_eq!(member.len(), 9);
    assert_eq!(member[0], r#"-    has-flag: "npm:^4.0.0""#);
    assert_eq!(member[1], r#"+    has-flag: "npm:^5.0.0""#);

    // A removed `resolutions` entry drops the pinned package from the lock.
    assert!(diff(STALE_REMOVED_RESOLUTION).contains(&r#"-"chalk@npm:4.1.0":"#.to_string()));

    // A lock an older Yarn 4 wrote would be rewritten to the current schema.
    let schema = diff(STALE_SCHEMA);
    assert_eq!(schema[0], "-  version: 8");
    assert_eq!(schema[1], "+  version: 10");

    // A `file:` folder's content is hashed into its locator, so a changed folder is stale.
    assert!(
        diff(STALE_FILE_FOLDER)
            .iter()
            .any(|line| line.starts_with("+  resolution: \"local@file:./local#./local::hash="))
    );

    // Yarn 2 and 3 report the same diff in their own lock spelling.
    for stdout in [YARN2_STALE, YARN3_STALE] {
        let diff = diff(stdout);
        assert_eq!(diff.len(), 9);
        assert_eq!(diff[0], "-    has-flag: ^4.0.0");
    }
}

/// A report that stops before validation is no verdict, whatever stopped it: the offline run
/// blocking a metadata request, a registry refusing anonymous access, or a server error.
/// The errors are kept with their codes for the diagnostic.
#[test]
fn a_report_that_stops_before_validation_is_undecided() {
    let offline = errors(NETWORK_NEEDED);
    assert_eq!(
        offline,
        [
            "YN0080: has-flag@npm:^5.0.0: Request to 'https://registry.yarnpkg.com/has-flag' has \
             been blocked because of your configuration settings",
            "YN0000: Failed with errors in 0s 18ms",
        ]
    );

    let auth = errors(AUTH_BEFORE_VERDICT);
    assert_eq!(
        auth[0],
        "YN0041: has-flag@npm:^5.0.0: Invalid authentication (as an anonymous user)"
    );

    let server = errors(SERVER_ERROR_BEFORE_VERDICT);
    assert!(server[0].starts_with("YN0035: has-flag@npm:^5.0.0: The registry appears to be down"));
}

/// Yarn refusing its configuration prints a usage error instead of a report, which is no
/// verdict either.
#[test]
fn a_usage_error_is_no_report() {
    assert_eq!(classify(MALFORMED_RC), Verdict::NoReport);
    assert_eq!(classify(""), Verdict::NoReport);
}

/// Only Yarn's own unindented step header counts: the same text nested under another step, or
/// carrying a message code, is not the fetch step starting.
///
/// The lines are synthetic, shaped like the captured header.
#[test]
fn only_the_top_level_step_header_proves_the_fetch_step() {
    let nested = indoc! {r#"
        {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
        {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"┌ Fetch step"}
        {"type":"info","name":0,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
        {"type":"warning","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    "#};

    assert_eq!(classify(nested), Verdict::Undecided { errors: Vec::new() });
}

/// A stale diff wins over a fetch header, so no report a plugin or a later step adds to can
/// turn a reported change into a current lock.
///
/// The lines are synthetic, shaped like the captured ones.
#[test]
fn a_lockfile_change_outweighs_a_fetch_header() {
    let both = indoc! {r#"
        {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  version: 8"}
        {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    "#};

    assert_eq!(diff(both), ["-  version: 8"]);
}

/// Lines that are no report line are skipped, and Windows line endings are read like Unix
/// ones, so a plugin's own output or a CRLF pipe changes no verdict.
#[test]
fn stray_output_and_crlf_line_endings_change_no_verdict() {
    let stray = format!("plugin says hello\n{}", CURRENT.replace('\n', "\r\n"));

    assert_eq!(classify(&stray), Verdict::Current);
}
