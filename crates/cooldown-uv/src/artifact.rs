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
        match time {
            None => return None,
            Some(time) => newest = Some(newest.map_or(time, |current| current.max(time))),
        }
    }
    newest
}

pub(crate) fn published_at_for_artifacts(
    artifacts: &[RawArtifact],
    selected: &[ArtifactId],
) -> Option<Timestamp> {
    if artifacts.is_empty() {
        return None;
    }
    if selected.is_empty() {
        return newest_or_none(artifacts.iter().map(|artifact| artifact.published_at));
    }
    let matching: Vec<Option<Timestamp>> = artifacts
        .iter()
        .filter(|artifact| selected.contains(&artifact.id))
        .map(|artifact| artifact.published_at)
        .collect();
    if matching.is_empty() {
        return None;
    }
    newest_or_none(matching.into_iter())
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

    #[test]
    fn sdist_ids_collapse_to_one_class() {
        assert_eq!(
            artifact_id_from_filename("requests-2.32.3.tar.gz").0,
            "sdist"
        );
        assert_eq!(artifact_id_from_filename("requests-2.32.3.zip").0, "sdist");
    }
}
