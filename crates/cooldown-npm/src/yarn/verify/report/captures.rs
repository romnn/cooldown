//! Real `yarn install --immutable --json` reports, captured from mise's Yarn releases under
//! Node 26.3.1 in throwaway projects.
//!
//! Every capture is Yarn's stdout byte for byte, unless its doc calls it an excerpt.
//! Each notes Yarn's exit status, which the verdict never reads.

use indoc::indoc;

/// Yarn 4.18.1 verifying a current lock with a warm cache: every step runs and the install succeeds.
///
/// Yarn exited with status 0.
pub(in crate::yarn) const CURRENT: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Link step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Done in 0s 35ms"}
"#};

/// Yarn 4.18.1 after a member's `has-flag` range moved from `^4.0.0` to `^5.0.0`.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const STALE_MEMBER: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"+ has-flag@npm:5.0.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed in 0s 273ms"}

    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Post-resolution validation"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -17,9 +17,9 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"app@workspace:packages/app\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 0.0.0-use.local"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   resolution: \"app@workspace:packages/app\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-    has-flag: \"npm:^4.0.0\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+    has-flag: \"npm:^5.0.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: unknown"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: soft"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"berry-root@workspace:.\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -78,8 +78,14 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   checksum: 10c0/2e789c61b7888d66993e14e8331449e525ef42aac53c627cc53d1c3334e768bcb6abdc4f5f0de1478a25beec6f0bd62c7549058b7ac53e924040d4f301f02fd1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: node"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+\"has-flag@npm:^5.0.0\":"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  version: 5.0.1"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  resolution: \"has-flag@npm:5.0.1\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  languageName: node"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  linkType: hard"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"hasown@npm:^2.0.4\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 2.0.4"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   resolution: \"hasown@npm:2.0.4\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"The lockfile would have been modified by this install, which is explicitly forbidden."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 280ms"}
"#};

/// Yarn 4.18.1 after the root `resolutions` entry pinning `chalk` to 4.1.0 was removed.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const STALE_REMOVED_RESOLUTION: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"+ chalk@npm:4.1.2"}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"- chalk@npm:4.1.0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}

    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Post-resolution validation"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -31,15 +31,14 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     resolve: \"npm:^1.22.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: unknown"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: soft"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-\"chalk@npm:4.1.0\":"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  version: 4.1.0"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  resolution: \"chalk@npm:4.1.0\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+\"chalk@npm:^4.1.0\":"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  version: 4.1.2"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  resolution: \"chalk@npm:4.1.2\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     ansi-styles: \"npm:^4.1.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     supports-color: \"npm:^7.1.0\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  checksum: 10c0/3787bd65ecd98ab3a1acc3b4f71d006268a675875e49ee6ea75fb54ba73d268b97544368358c18c42445e408e076ae8ad5cec8fbad36942a2c7ac654883dc61e"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: node"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"color-convert@npm:^2.0.1\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"The lockfile would have been modified by this install, which is explicitly forbidden."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 111ms"}
"#};

/// Yarn 4.18.1 verifying a current lock offline with an empty cache: the fetch step fails on every tarball after the lock check passed.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const CURRENT_FETCH_BLOCKED: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"ansi-styles@npm:4.3.0: Request to 'https://registry.yarnpkg.com/ansi-styles/-/ansi-styles-4.3.0.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"chalk@npm:4.1.2: Request to 'https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"color-convert@npm:2.0.1: Request to 'https://registry.yarnpkg.com/color-convert/-/color-convert-2.0.1.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"color-name@npm:1.1.4: Request to 'https://registry.yarnpkg.com/color-name/-/color-name-1.1.4.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"es-errors@npm:1.3.0: Request to 'https://registry.yarnpkg.com/es-errors/-/es-errors-1.3.0.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"function-bind@npm:1.1.2: Request to 'https://registry.yarnpkg.com/function-bind/-/function-bind-1.1.2.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"has-flag@npm:4.0.0: Request to 'https://registry.yarnpkg.com/has-flag/-/has-flag-4.0.0.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"hasown@npm:2.0.4: Request to 'https://registry.yarnpkg.com/hasown/-/hasown-2.0.4.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"is-core-module@npm:2.17.0: Request to 'https://registry.yarnpkg.com/is-core-module/-/is-core-module-2.17.0.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"path-parse@npm:1.0.7: Request to 'https://registry.yarnpkg.com/path-parse/-/path-parse-1.0.7.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"resolve@npm:1.22.13: Request to 'https://registry.yarnpkg.com/resolve/-/resolve-1.22.13.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"supports-color@npm:7.2.0: Request to 'https://registry.yarnpkg.com/supports-color/-/supports-color-7.2.0.tgz' has been blocked because of your configuration settings"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"supports-preserve-symlinks-flag@npm:1.0.0: Request to 'https://registry.yarnpkg.com/supports-preserve-symlinks-flag/-/supports-preserve-symlinks-flag-1.0.0.tgz' has been blocked because of your configuration settings"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 25ms"}
"#};

/// Yarn 4.18.1 offline on the stale lock of [`STALE_MEMBER`]: resolving `has-flag@npm:^5.0.0` needs the registry, so the lock check never runs.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const NETWORK_NEEDED: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"error","name":80,"displayName":"YN0080","indent":"│ ","data":"has-flag@npm:^5.0.0: Request to 'https://registry.yarnpkg.com/has-flag' has been blocked because of your configuration settings"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 18ms"}
"#};

/// Yarn 4.18.1 refusing a malformed `.yarnrc.yml`: a coloured usage error on stdout, no report.
///
/// An excerpt: the project path is shortened to `/project` and the banner's colour gradient is cut.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const MALFORMED_RC: &str = concat!(
    "\u{1b}[31m\u{1b}[1mUsage Error\u{1b}[22m\u{1b}[39m: Parse error when loading /project/.yarnrc.yml; please check it's proper Yaml (in particular, make sure you list the colons after each key name)\n",
    "\n",
    "\u{1b}[1m━━━ Yarn Package Manager - 4.18.1 ━━━━━━━━━━━━━━━━━━━━━━━\u{1b}[0m\n",
    "\n",
    "  \u{1b}[1m$ \u{1b}[22myarn <command>\n",
    "\n",
    "You can also print more details about any of these commands by calling them with \n",
    "the \u{1b}[36m`-h,--help`\u{1b}[39m flag right after the command name.\n",
);

/// Yarn 4.18.1 verifying a lock Yarn 4.13.0 wrote (schema 8), which it would rewrite to schema 10.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const STALE_SCHEMA: &str = indoc! {r#"

    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"+ resolve@patch:resolve@npm%3A1.22.14#optional!builtin<compat/resolve>::version=1.22.14&hash=9bd1a5"}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"- resolve@patch:resolve@npm%3A1.22.14#optional!builtin<compat/resolve>::version=1.22.14&hash=c3c19d"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}

    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Post-resolution validation"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -1,9 +1,9 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" # This file is generated by running \"yarn install\" inside your project."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" # Manual changes might be lost - proceed with caution!"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" __metadata:"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  version: 8"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  version: 10"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   cacheKey: 10c0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"ansi-styles@npm:^4.1.0\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 4.3.0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -126,17 +126,16 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 1.22.14"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  resolution: \"resolve@patch:resolve@npm%3A1.22.14#optional!builtin<compat/resolve>::version=1.22.14&hash=c3c19d\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  resolution: \"resolve@patch:resolve@npm%3A1.22.14#optional!builtin<compat/resolve>::version=1.22.14&hash=9bd1a5\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     es-errors: \"npm:^1.3.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     is-core-module: \"npm:^2.18.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     path-parse: \"npm:^1.0.7\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     supports-preserve-symlinks-flag: \"npm:^1.0.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   bin:"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"     resolve: bin/resolve"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  checksum: 10c0/1e098939ebbdfe6ce355ce60523f2219d6a7072534f9deece27016acda0905768fc578d107afef45ec3b5ff8ccbdc8722c16b09056edd86b210a7df24e5ab198"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: node"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"supports-color@npm:^7.1.0\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"The lockfile would have been modified by this install, which is explicitly forbidden."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 205ms"}
"#};

/// Yarn 4.18.1 on the stale lock of [`STALE_MEMBER`] against a registry answering 401.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const AUTH_BEFORE_VERDICT: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"has-flag@npm:^5.0.0: Invalid authentication (as an anonymous user)"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 31ms"}
"#};

/// Yarn 4.18.1 on the stale lock of [`STALE_MEMBER`] against a registry answering 500, without retries.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const SERVER_ERROR_BEFORE_VERDICT: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"error","name":35,"displayName":"YN0035","indent":"│ ","data":"has-flag@npm:^5.0.0: The registry appears to be down (using a local cache might have protected you against such outages)"}
    {"type":"error","name":35,"displayName":"YN0035","indent":"│ ","data":"  Response Code: 500 (Internal Server Error)"}
    {"type":"error","name":35,"displayName":"YN0035","indent":"│ ","data":"  Request Method: GET"}
    {"type":"error","name":35,"displayName":"YN0035","indent":"│ ","data":"  Request URL: http://127.0.0.1:18752/has-flag"}
    {"type":"error","name":35,"displayName":"YN0035","indent":"│ ","data":"  Request Retry Count: 0 (can be increased via httpRetry)"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 29ms"}
"#};

/// Yarn 4.18.1 verifying a current lock with an empty cache against a registry answering 401: the lock check passes before the fetch fails.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const CURRENT_AUTH_AFTER_VERDICT: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"ansi-styles@npm:4.3.0: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"chalk@npm:4.1.2: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"color-convert@npm:2.0.1: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"color-name@npm:1.1.4: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"es-errors@npm:1.3.0: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"function-bind@npm:1.1.2: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"has-flag@npm:4.0.0: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"supports-preserve-symlinks-flag@npm:1.0.0: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"supports-color@npm:7.2.0: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"resolve@npm:1.22.13: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"path-parse@npm:1.0.7: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"is-core-module@npm:2.17.0: Invalid authentication (as an anonymous user)"}
    {"type":"error","name":41,"displayName":"YN0041","indent":"│ ","data":"hasown@npm:2.0.4: Invalid authentication (as an anonymous user)"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed in 1s 31ms"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 1s 51ms"}
"#};

/// Yarn 4.18.1 after a file was added to the `file:./local` dependency's folder, which changes its locator's hash.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const STALE_FILE_FOLDER: &str = indoc! {r#"
    {"type":"info","name":0,"displayName":"YN0000","indent":"· ","data":"Yarn 4.18.1"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"+ local@file:./local#./local::hash=05a3a6&locator=berry-root%40workspace%3A."}
    {"type":"info","name":85,"displayName":"YN0085","indent":"│ ","data":"- local@file:./local#./local::hash=94559b&locator=berry-root%40workspace%3A."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}

    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Post-resolution validation"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -98,10 +98,9 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"local@file:./local::locator=berry-root%40workspace%3A.\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 1.0.0"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  resolution: \"local@file:./local#./local::hash=94559b&locator=berry-root%40workspace%3A.\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-  checksum: 10c0/b951b8155dd942a71f44606344c3b9d4829589cf7e3a3d2b3aa91bb9c3bfbc33b4a1c1068f6f8a94ef8d474da4892c5c4955870aa39dd614716bfe6506310f40"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  resolution: \"local@file:./local#./local::hash=05a3a6&locator=berry-root%40workspace%3A.\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: node"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"path-parse@npm:^1.0.7\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"The lockfile would have been modified by this install, which is explicitly forbidden."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"· ","data":"Failed with errors in 0s 20ms"}
"#};

/// Yarn 2.4.3 (`--skip-builds`) verifying a current lock; Yarn 2 prints no version line.
///
/// Yarn exited with status 0.
pub(in crate::yarn) const YARN2_CURRENT: &str = indoc! {r#"
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"ansi-styles@npm:4.3.0 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"chalk@npm:4.1.2 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"color-convert@npm:2.0.1 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"color-name@npm:1.1.4 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"has-flag@npm:4.0.0 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"supports-color@npm:7.2.0 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Link step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":0,"displayName":"YN0000","indent":"","data":"Done in 0s 25ms"}
"#};

/// Yarn 2.4.3 after a member's `has-flag` range moved to `^5.0.0`.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const YARN2_STALE: &str = indoc! {r#"
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}

    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Post-resolution validation"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -17,9 +17,9 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"app@workspace:packages/app\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 0.0.0-use.local"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   resolution: \"app@workspace:packages/app\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-    has-flag: ^4.0.0"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+    has-flag: ^5.0.0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: unknown"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: soft"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"berry-root@workspace:.\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -62,8 +62,14 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   checksum: 2e5391139d3d287231ccb58659702392f6e3abeac3296fb4721afaff46493f3d9b99a9329ae015dfe973aa206ed5c75f43e86aec0267dce79aa5c2b6e811b3ad"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: node"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+\"has-flag@npm:^5.0.0\":"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  version: 5.0.1"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  resolution: \"has-flag@npm:5.0.1\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  languageName: node"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  linkType: hard"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"supports-color@npm:^7.1.0\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 7.2.0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   resolution: \"supports-color@npm:7.2.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"The lockfile would have been modified by this install, which is explicitly forbidden."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"","data":"Failed with errors in 0s 93ms"}
"#};

/// Yarn 3.8.7 verifying a current lock.
///
/// Yarn exited with status 0.
pub(in crate::yarn) const YARN3_CURRENT: &str = indoc! {r#"
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Fetch step"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"ansi-styles@npm:4.3.0 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"chalk@npm:4.1.2 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"color-convert@npm:2.0.1 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"color-name@npm:1.1.4 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"has-flag@npm:4.0.0 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":13,"displayName":"YN0013","indent":"│ ","data":"supports-color@npm:7.2.0 can't be found in the cache and will be fetched from the remote registry"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Link step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"info","name":0,"displayName":"YN0000","indent":"","data":"Done in 0s 23ms"}
"#};

/// Yarn 3.8.7 after a member's `has-flag` range moved to `^5.0.0`.
///
/// Yarn exited with status 1.
pub(in crate::yarn) const YARN3_STALE: &str = indoc! {r#"
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Resolution step"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}

    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"┌ Post-resolution validation"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -17,9 +17,9 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"app@workspace:packages/app\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 0.0.0-use.local"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   resolution: \"app@workspace:packages/app\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"-    has-flag: ^4.0.0"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+    has-flag: ^5.0.0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: unknown"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: soft"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"berry-root@workspace:.\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"@@ -62,8 +62,14 @@"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   checksum: 261a1357037ead75e338156b1f9452c016a37dcd3283a972a30d9e4a87441ba372c8b81f818cd0fbcd9c0354b4ae7e18b9e1afa1971164aef6d18c2b6095a8ad"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   languageName: node"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   linkType: hard"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" "}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+\"has-flag@npm:^5.0.0\":"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  version: 5.0.1"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  resolution: \"has-flag@npm:5.0.1\""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  languageName: node"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+  linkType: hard"}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"+"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":" \"supports-color@npm:^7.1.0\":"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   version: 7.2.0"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   resolution: \"supports-color@npm:7.2.0\""}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":"   dependencies:"}
    {"type":"info","name":null,"displayName":"YN0000","indent":"│ ","data":""}
    {"type":"error","name":28,"displayName":"YN0028","indent":"│ ","data":"The lockfile would have been modified by this install, which is explicitly forbidden."}
    {"type":"info","name":null,"displayName":"YN0000","indent":"","data":"└ Completed"}
    {"type":"error","name":0,"displayName":"YN0000","indent":"","data":"Failed with errors in 0s 68ms"}
"#};
