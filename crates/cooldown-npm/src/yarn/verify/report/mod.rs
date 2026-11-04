//! The verdict a `yarn install --immutable --json` report gives on the lock.
//!
//! Every Yarn Berry release from 2.4 on runs an install in the same steps: resolution, then
//! post-resolution validation, then fetch, then link ([Project.ts][project]).
//! Validation regenerates the lock and, under `--immutable`, reports each line that would change
//! as a `YN0028` error before it throws; that code comes from nowhere else in an install.
//! The fetch step's header is printed only once validation returned, so reaching it proves the
//! lock current, whatever fails afterwards.
//!
//! Each step's header is an info line with no message code and no indent
//! (`{"type":"info","name":null,"indent":"","data":"┌ Fetch step"}`), as
//! [StreamReport.ts][report] prints it.
//!
//! [project]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Project.ts
//! [report]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/StreamReport.ts

use serde::Deserialize;

/// Yarn's `FROZEN_LOCKFILE_EXCEPTION`: the lockfile would have been modified.
const FROZEN_LOCKFILE: u64 = 28;

/// The header Yarn prints as the fetch step starts.
const FETCH_STEP: &str = "┌ Fetch step";

/// What a report says about the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Yarn would rewrite the lock: every `YN0028` line, the diff included.
    Stale { diff: Vec<String> },
    /// Yarn reached the fetch step, so the regenerated lock matched the one on disk.
    Current,
    /// Yarn reported, but stopped before deciding: every error it reported.
    Undecided { errors: Vec<String> },
    /// Yarn printed no report at all, as it does for a usage or configuration error.
    NoReport,
}

/// One NDJSON line of a Yarn report.
#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    /// The message code, `null` for a step header.
    name: Option<u64>,
    #[serde(default)]
    indent: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// The verdict of the install report `stdout`.
///
/// A line that is no report line, such as a plugin's own output, is ignored.
pub(super) fn classify(stdout: &str) -> Verdict {
    let mut reported = false;
    let mut fetched = false;
    let mut diff = Vec::new();
    let mut errors = Vec::new();
    for raw in stdout.lines() {
        let Ok(line) = serde_json::from_str::<Line>(raw.trim_end_matches('\r')) else {
            continue;
        };
        reported = true;
        let data = match &line.data {
            serde_json::Value::String(data) => data.clone(),
            other => other.to_string(),
        };
        match (line.kind.as_str(), line.name) {
            ("error", Some(FROZEN_LOCKFILE)) => diff.push(data),
            ("error", name) => errors.push(match name {
                Some(name) => format!("YN{name:04}: {data}"),
                None => data,
            }),
            ("info", None) if line.indent.is_empty() && data == FETCH_STEP => fetched = true,
            _ => {}
        }
    }
    if !diff.is_empty() {
        Verdict::Stale { diff }
    } else if fetched {
        Verdict::Current
    } else if reported {
        Verdict::Undecided { errors }
    } else {
        Verdict::NoReport
    }
}

#[cfg(test)]
pub(in crate::yarn) mod captures;
#[cfg(test)]
mod tests;
