use cooldown_core::{ArtifactId, RawArtifact};
use jiff::Timestamp;
use std::borrow::Cow;

pub(crate) fn artifact_id_from_filename(filename: &str) -> ArtifactId {
    if is_sdist(filename) {
        return ArtifactId("sdist".to_string());
    }
    if let Some(id) = wheel_id(filename) {
        return ArtifactId(id);
    }
    ArtifactId(fallback_file_id(filename))
}

pub(crate) fn artifact_id_from_url(url: &str) -> Option<ArtifactId> {
    let filename = url
        .split(['?', '#'])
        .next()
        .and_then(|value| value.rsplit('/').next())
        .filter(|value| !value.is_empty())?;
    Some(artifact_id_from_filename(filename))
}

pub(crate) fn newest_or_none(times: impl Iterator<Item = Option<Timestamp>>) -> Option<Timestamp> {
    let mut newest: Option<Timestamp> = None;
    for time in times {
        let time = time?;
        newest = Some(newest.map_or(time, |current| current.max(time)));
    }
    newest
}

/// Dates the locked pin by the newest upload time of its `selected` artifact classes, or of every
/// file when `selected` is empty (`--all-artifacts`).
///
/// `None` when the release has no files, when it lacks one of the `selected` classes, or when a
/// file being dated lacks an upload time.
/// The locked pin's install set is known exactly, so a missing class (a file list cached before
/// a wheel was uploaded, or one from another index) leaves its age unknown rather than borrowing
/// an older file's time.
pub(crate) fn published_at_for_artifacts(
    artifacts: &[RawArtifact],
    selected: &[ArtifactId],
) -> Option<Timestamp> {
    if artifacts.is_empty() || !offers_every(artifacts, selected) {
        return None;
    }
    let dated = artifacts
        .iter()
        .filter(|artifact| selected.is_empty() || selected.contains(&artifact.id));
    newest_or_none(dated.map(|artifact| artifact.published_at))
}

/// Dates a candidate release by the newest upload time of the files an environment would install
/// from it, given the locked pin's `selected` artifact classes.
///
/// The locked classes stand in for the candidate's install set only while the candidate still
/// offers every one of them.
/// A candidate that drops one installs files the lock never named: an abi3 or `requires-python`
/// floor bump, a new manylinux or macOS baseline, or a move to or from pure-Python wheels all
/// retag the wheels.
/// Such a candidate is dated by its whole file set, whose newest time is never earlier than that
/// of the subset an environment would install.
///
/// `None` when the candidate has no files or a file being dated lacks an upload time, so a
/// partially-known release is never treated as mature.
pub(crate) fn candidate_published_at(
    artifacts: &[RawArtifact],
    selected: &[ArtifactId],
) -> Option<Timestamp> {
    if offers_every(artifacts, selected) {
        published_at_for_artifacts(artifacts, selected)
    } else {
        newest_or_none(artifacts.iter().map(|artifact| artifact.published_at))
    }
}

fn offers_every(artifacts: &[RawArtifact], selected: &[ArtifactId]) -> bool {
    selected
        .iter()
        .all(|id| artifacts.iter().any(|artifact| &artifact.id == id))
}

fn is_sdist(filename: &str) -> bool {
    [".tar.gz", ".tgz", ".zip", ".tar.bz2", ".tar.xz", ".tar.zst"]
        .iter()
        .any(|ext| filename.ends_with(ext))
}

fn wheel_id(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".whl")?;
    let mut parts = stem.rsplitn(4, '-');
    let platform = parts.next()?;
    let abi = parts.next()?;
    let python = parts.next()?;
    let _prefix = parts.next()?;
    Some(format!("wheel:{python}-{abi}-{platform}"))
}

fn fallback_file_id(filename: &str) -> String {
    let ext = filename
        .split('.')
        .next_back()
        .filter(|value| !value.is_empty())
        .map_or_else(|| Cow::from("unknown"), Cow::from);
    format!("file:{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_ids_ignore_distribution_and_version() {
        assert_eq!(
            artifact_id_from_filename("requests-2.32.3-py3-none-any.whl").0,
            "wheel:py3-none-any"
        );
        assert_eq!(
            artifact_id_from_filename("pkg-1.0.0-1-py3-none-manylinux_2_17_x86_64.whl").0,
            "wheel:py3-none-manylinux_2_17_x86_64"
        );
    }

    fn file(filename: &str, uploaded: Option<&str>) -> RawArtifact {
        RawArtifact {
            id: artifact_id_from_filename(filename),
            published_at: uploaded.map(|time| time.parse().unwrap()),
            markers: Vec::new(),
        }
    }

    fn ids(filenames: &[&str]) -> Vec<ArtifactId> {
        filenames
            .iter()
            .map(|filename| artifact_id_from_filename(filename))
            .collect()
    }

    /// A candidate that still offers every locked artifact class is dated by those classes alone,
    /// so a late wheel for a Python the lock never covers does not hold it back.
    #[test]
    fn covered_candidate_dates_only_the_selected_files() {
        let release = [
            file(
                "pkg-2.0-cp312-cp312-manylinux_2_17_x86_64.whl",
                Some("2026-10-01T00:00:00Z"),
            ),
            file(
                "pkg-2.0-cp314-cp314-manylinux_2_17_x86_64.whl",
                Some("2026-10-20T00:00:00Z"),
            ),
        ];
        let selected = ids(&["pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl"]);

        assert_eq!(
            candidate_published_at(&release, &selected).map(|time| time.to_string()),
            Some("2026-10-01T00:00:00Z".to_string())
        );
    }

    /// A candidate whose wheels were retagged (here an abi3 floor bump from cp310 to cp311) offers
    /// none of the locked classes, so its whole file set dates it rather than leaving it undated.
    #[test]
    fn retagged_candidate_is_dated_by_its_whole_file_set() {
        let release = [
            file(
                "pymupdf-2.0-cp311-abi3-manylinux_2_28_x86_64.whl",
                Some("2026-10-19T08:00:00Z"),
            ),
            file(
                "pymupdf-2.0-cp311-abi3-win_amd64.whl",
                Some("2026-10-19T09:00:00Z"),
            ),
            file("pymupdf-2.0.tar.gz", Some("2026-10-19T07:00:00Z")),
        ];
        let selected = ids(&[
            "pymupdf-1.28.2-cp310-abi3-manylinux_2_28_x86_64.whl",
            "pymupdf-1.28.2-cp310-abi3-win_amd64.whl",
        ]);

        assert_eq!(
            candidate_published_at(&release, &selected).map(|time| time.to_string()),
            Some("2026-10-19T09:00:00Z".to_string())
        );
    }

    /// Losing even one locked class means an environment installs a file the lock never named, so
    /// the matching subset is no longer a safe stand-in and the late-uploaded wheel counts.
    #[test]
    fn partially_covered_candidate_is_dated_by_its_whole_file_set() {
        let release = [
            file(
                "pkg-2.0-cp312-cp312-manylinux_2_17_x86_64.whl",
                Some("2026-10-01T00:00:00Z"),
            ),
            file(
                "pkg-2.0-cp312-cp312-macosx_14_0_arm64.whl",
                Some("2026-10-20T00:00:00Z"),
            ),
        ];
        let selected = ids(&[
            "pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl",
            "pkg-1.0-cp312-cp312-macosx_11_0_arm64.whl",
        ]);

        assert_eq!(
            candidate_published_at(&release, &selected).map(|time| time.to_string()),
            Some("2026-10-20T00:00:00Z".to_string())
        );
    }

    /// The whole-file-set fallback stays conservative: one undated file leaves the candidate
    /// undated, and so does a candidate with no files at all.
    #[test]
    fn candidate_fallback_with_an_undated_file_stays_undated() {
        let release = [
            file(
                "pkg-2.0-cp311-abi3-win_amd64.whl",
                Some("2026-10-01T00:00:00Z"),
            ),
            file("pkg-2.0.tar.gz", None),
        ];
        let selected = ids(&["pkg-1.0-cp310-abi3-win_amd64.whl"]);

        assert_eq!(candidate_published_at(&release, &selected), None);
        assert_eq!(candidate_published_at(&[], &selected), None);
    }

    /// The locked pin's install set is known exactly, so a file list that lacks one of its wheels
    /// (for example one cached before that wheel was uploaded) leaves its age unknown instead of
    /// borrowing the older sdist's time.
    #[test]
    fn locked_pin_missing_a_selected_class_stays_undated() {
        let cached = [
            file("pkg-1.0.tar.gz", Some("2026-10-01T00:00:00Z")),
            file(
                "pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl",
                Some("2026-10-01T00:00:00Z"),
            ),
        ];
        let selected = ids(&[
            "pkg-1.0-cp312-cp312-manylinux_2_17_x86_64.whl",
            "pkg-1.0-cp312-cp312-win_amd64.whl",
        ]);

        assert_eq!(published_at_for_artifacts(&cached, &selected), None);
        assert_eq!(
            published_at_for_artifacts(&cached, &selected[..1]).map(|time| time.to_string()),
            Some("2026-10-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn sdist_ids_collapse_to_one_class() {
        assert_eq!(
            artifact_id_from_filename("requests-2.32.3.tar.gz").0,
            "sdist"
        );
        assert_eq!(artifact_id_from_filename("requests-2.32.3.zip").0, "sdist");
    }
}
