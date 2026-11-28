//! The structural reading of a `pixi.lock`: one binding per environment, platform, ecosystem and
//! package name, joined with the package record that carries its version, digest and upload time.

use crate::lock::{conda_from_url, exact_match_spec, normalize_name, pypi_from_url};
use cooldown_core::{CoreError, Result};
use jiff::Timestamp;
use std::collections::{BTreeMap, HashMap};

/// The index pixi elides from a version 7 lock, and cooldown's PyPI client reads ages from.
pub(crate) const PUBLIC_PYPI: &str = "https://pypi.org/simple";

/// The ecosystem a locked package comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    /// A conda package from a channel.
    Conda,
    /// A PyPI distribution.
    Pypi,
}

/// `name` as its ecosystem compares it: a conda name by case alone, since `foo-bar` and `foo_bar`
/// are two conda packages, and a PyPI name with its separators folded too.
pub(crate) fn normal(kind: Kind, name: &str) -> String {
    match kind {
        Kind::Conda => name.to_lowercase(),
        Kind::Pypi => normalize_name(name),
    }
}

/// Where a package is bound: one environment on one named platform, per ecosystem and
/// [`normal`] name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Slot {
    pub environment: String,
    pub platform: String,
    pub kind: Kind,
    pub name: String,
}

/// The record a [`Slot`] binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub version: String,
    /// The archive the record installs.
    pub url: String,
    /// The archive's recorded sha256 or md5 digest, or its URL when the lock records neither.
    pub identity: String,
    /// Where the archive comes from: a conda record's channel, or for a PyPI record
    /// [`PUBLIC_PYPI`], another index it names, or the URL it is taken from directly.
    pub source: String,
    /// When the channel indexed the conda build, else when it was built; PyPI records and old
    /// conda builds carry neither.
    pub uploaded: Option<Timestamp>,
    /// Whether another package bound in the same environment and platform pins this one to exactly
    /// its version, through `depends` or `constrains`.
    pub capped: bool,
}

/// What a lock holds.
#[derive(Debug, Default)]
pub(crate) struct Lock {
    pub bindings: BTreeMap<Slot, Binding>,
    /// The first bound reference the reader cannot model as one registry package, such as a
    /// source package, a local path or a reference that joins no record, described.
    pub unsupported: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawLock {
    #[serde(default)]
    environments: BTreeMap<String, RawEnvironment>,
    #[serde(default)]
    packages: Vec<RawRecord>,
}

#[derive(serde::Deserialize)]
struct RawEnvironment {
    /// The records each platform binds, by platform.
    #[serde(default)]
    packages: BTreeMap<String, Vec<RawRef>>,
}

/// A reference from an environment to a record, by its location.
#[derive(serde::Deserialize)]
struct RawRef {
    conda: Option<String>,
    conda_source: Option<String>,
    pypi: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawRecord {
    conda: Option<String>,
    conda_source: Option<String>,
    pypi: Option<String>,
    name: Option<String>,
    version: Option<String>,
    sha256: Option<String>,
    md5: Option<String>,
    /// The channel a conda record was resolved from, which rattler records apart from the URL.
    channel: Option<String>,
    /// The index a version 7 PyPI record was resolved from, elided for [`PUBLIC_PYPI`].
    index: Option<String>,
    #[serde(default)]
    depends: Vec<String>,
    #[serde(default)]
    constrains: Vec<String>,
    /// When the build was made, in seconds or milliseconds since the epoch.
    timestamp: Option<i64>,
    /// When the channel indexed the build, in milliseconds since the epoch.
    indexed_timestamp: Option<i64>,
}

impl RawRecord {
    /// When the build became available, as pixi's cutoff reads it: when the channel indexed it,
    /// else when it was built.
    fn uploaded(&self) -> Option<Timestamp> {
        if let Some(millis) = self.indexed_timestamp {
            return Timestamp::from_millisecond(millis).ok();
        }
        // rattler_lock reads a value past the last second of the year 9999 as milliseconds.
        let stamp = self.timestamp?;
        if stamp > 253_402_300_799 {
            Timestamp::from_millisecond(stamp).ok()
        } else {
            Timestamp::from_second(stamp).ok()
        }
    }
}

/// The location a reference and the record it joins share.
#[derive(PartialEq, Eq, Hash)]
enum Location<'a> {
    Conda(&'a str),
    CondaSource(&'a str),
    Pypi(&'a str),
}

impl<'a> Location<'a> {
    fn of(conda: Option<&'a str>, source: Option<&'a str>, pypi: Option<&'a str>) -> Option<Self> {
        conda
            .map(Location::Conda)
            .or(source.map(Location::CondaSource))
            .or(pypi.map(Location::Pypi))
    }

    fn reference(&self) -> &'a str {
        match self {
            Location::Conda(at) | Location::CondaSource(at) | Location::Pypi(at) => at,
        }
    }
}

/// Reads the lock `content`.
///
/// A record's name and version come from its own fields, or else from its archive's file name, as
/// pixi writes them.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] when `content` is not a lock pixi wrote.
pub(crate) fn read(content: &str) -> Result<Lock> {
    let raw: RawLock = serde_saphyr::from_str(content)
        .map_err(|err| CoreError::LockUnreadable(format!("pixi.lock: {err}")))?;
    let mut records: HashMap<Location<'_>, Vec<&RawRecord>> = HashMap::new();
    for record in &raw.packages {
        let location = Location::of(
            record.conda.as_deref(),
            record.conda_source.as_deref(),
            record.pypi.as_deref(),
        );
        if let Some(location) = location {
            records.entry(location).or_default().push(record);
        }
    }

    let mut lock = Lock::default();
    // The exact versions the records of each environment and platform pin a package to.
    let mut pins: BTreeMap<Slot, Vec<String>> = BTreeMap::new();
    for (environment, env) in &raw.environments {
        for (platform, references) in &env.packages {
            for reference in references {
                let Some(location) = Location::of(
                    reference.conda.as_deref(),
                    reference.conda_source.as_deref(),
                    reference.pypi.as_deref(),
                ) else {
                    continue;
                };
                // A reference joins exactly one record, and one binding per name and ecosystem.
                let record = match records.get(&location).map(Vec::as_slice) {
                    Some([record]) => Some(*record),
                    _ => None,
                };
                let bound = record
                    .and_then(|record| model(environment, platform, &location, record))
                    .filter(|bound| !lock.bindings.contains_key(&bound.slot));
                let (Some(record), Some(Bound { slot, binding })) = (record, bound) else {
                    lock.unsupported.get_or_insert_with(|| {
                        format!(
                            "environment {environment} binds {} on {platform}, which cooldown \
                             cannot read as one registry package",
                            location.reference()
                        )
                    });
                    continue;
                };
                if slot.kind == Kind::Conda {
                    for spec in record.depends.iter().chain(&record.constrains) {
                        if let Some(pin) = exact_match_spec(spec) {
                            let pinned = Slot {
                                name: normal(Kind::Conda, pin.name),
                                ..slot.clone()
                            };
                            pins.entry(pinned).or_default().push(pin.version.to_owned());
                        }
                    }
                }
                lock.bindings.insert(slot, binding);
            }
        }
    }
    for (slot, versions) in pins {
        if let Some(binding) = lock.bindings.get_mut(&slot) {
            binding.capped = versions.contains(&binding.version);
        }
    }
    Ok(lock)
}

/// One binding a reference stands for.
struct Bound {
    slot: Slot,
    binding: Binding,
}

/// The binding `environment`'s reference at `location` to `record` stands for on `platform`, or
/// `None` when it is no archive of a channel or an index.
fn model(
    environment: &str,
    platform: &str,
    location: &Location<'_>,
    record: &RawRecord,
) -> Option<Bound> {
    let (kind, url, from_url, source) = match *location {
        Location::Conda(url) => {
            // `<channel>/<subdir>/<file>`, unless the record names its channel itself.
            let from_url = url.rsplitn(3, '/').nth(2)?;
            let channel = record.channel.as_deref().unwrap_or(from_url);
            let channel = channel.trim_end_matches('/').to_owned();
            (Kind::Conda, url, conda_from_url(url), channel)
        }
        Location::Pypi(url) if url.starts_with("https://") || url.starts_with("http://") => {
            let index = record.index.as_deref().unwrap_or(PUBLIC_PYPI);
            let public = index.trim_end_matches('/') == PUBLIC_PYPI
                && url.starts_with("https://files.pythonhosted.org/");
            let source = if public {
                PUBLIC_PYPI.to_owned()
            } else if record.index.is_some() {
                index.to_owned()
            } else {
                url.to_owned()
            };
            (Kind::Pypi, url, pypi_from_url(url), source)
        }
        Location::Pypi(_) | Location::CondaSource(_) => return None,
    };
    let (named, versioned) = from_url.map(|dep| (dep.name, dep.version)).unzip();
    let name = record.name.clone().or(named)?;
    Some(Bound {
        slot: Slot {
            environment: environment.to_owned(),
            platform: platform.to_owned(),
            kind,
            name: normal(kind, &name),
        },
        binding: Binding {
            version: record.version.clone().or(versioned)?,
            url: url.to_owned(),
            identity: record
                .sha256
                .as_ref()
                .or(record.md5.as_ref())
                .map_or_else(|| url.to_owned(), Clone::clone),
            source,
            uploaded: record.uploaded(),
            capped: false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::{formatdoc, indoc};

    fn slot(environment: &str, platform: &str, kind: Kind, name: &str) -> Slot {
        Slot {
            environment: environment.to_owned(),
            platform: platform.to_owned(),
            kind,
            name: name.to_owned(),
        }
    }

    /// Two environments on two platforms bind their records by location, a conda record's name and
    /// version come from its file name and a PyPI record's from its fields, an exact `depends`
    /// pin caps the package it names in that environment and platform only, a record is known by
    /// its digest, a conda record's own channel wins over its URL's, and a PyPI file from the
    /// elided public index is told from one of another index.
    #[test]
    fn bindings_join_environments_with_records() -> Result<(), Box<dyn std::error::Error>> {
        let lock = indoc! {"
            version: 7
            environments:
              default:
                packages:
                  linux-64:
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/zlib-1.3.1-h0_0.conda
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/libzlib-1.3.1-h0_0.conda
                  - pypi: https://files.pythonhosted.org/packages/ab/idna-3.6-py3-none-any.whl
                  - pypi: https://example.org/files/Private_Pkg-1.0-py3-none-any.whl
              other:
                packages:
                  win-64:
                  - conda: https://conda.anaconda.org/conda-forge/win-64/libzlib-1.3.1-h1_0.conda
            packages:
            - conda: https://conda.anaconda.org/conda-forge/linux-64/libzlib-1.3.1-h0_0.conda
              sha256: 61b8
              md5: 57d7
            - conda: https://conda.anaconda.org/conda-forge/linux-64/zlib-1.3.1-h0_0.conda
              depends:
              - libzlib 1.3.1 h0_0
            - conda: https://conda.anaconda.org/conda-forge/win-64/libzlib-1.3.1-h1_0.conda
              md5: 9a1c
              channel: https://prefix.example.org/mirror/
            - pypi: https://files.pythonhosted.org/packages/ab/idna-3.6-py3-none-any.whl
              name: idna
              version: '3.6'
            - pypi: https://example.org/files/Private_Pkg-1.0-py3-none-any.whl
              name: Private_Pkg
              version: '1.0'
              index: https://example.org/simple
        "};
        let lock = read(lock)?;

        let linux = &lock.bindings[&slot("default", "linux-64", Kind::Conda, "libzlib")];
        assert_eq!(linux.version, "1.3.1");
        assert!(linux.capped);
        assert_eq!(linux.identity, "61b8");
        assert_eq!(linux.source, "https://conda.anaconda.org/conda-forge");
        let windows = &lock.bindings[&slot("other", "win-64", Kind::Conda, "libzlib")];
        assert!(!windows.capped);
        assert_eq!(windows.identity, "9a1c");
        assert_eq!(windows.source, "https://prefix.example.org/mirror");
        let idna = &lock.bindings[&slot("default", "linux-64", Kind::Pypi, "idna")];
        assert_eq!(
            (idna.version.as_str(), idna.source.as_str()),
            ("3.6", PUBLIC_PYPI)
        );
        let private = &lock.bindings[&slot("default", "linux-64", Kind::Pypi, "private-pkg")];
        assert_eq!(private.source, "https://example.org/simple");
        assert_eq!(lock.bindings.len(), 5);
        assert_eq!(lock.unsupported, None);
        Ok(())
    }

    /// A build's time is when its channel indexed it, else when it was built, and a build time in
    /// seconds is read as one, as pixi reads them all.
    #[test]
    fn an_index_time_wins_and_a_build_time_may_be_in_seconds()
    -> Result<(), Box<dyn std::error::Error>> {
        let lock = indoc! {"
            version: 7
            environments:
              default:
                packages:
                  linux-64:
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/a-1-h_0.conda
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/b-1-h_0.conda
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/c-1-h_0.conda
            packages:
            - conda: https://conda.anaconda.org/conda-forge/linux-64/a-1-h_0.conda
              timestamp: 1700000000000
              indexed_timestamp: 1710000000000
            - conda: https://conda.anaconda.org/conda-forge/linux-64/b-1-h_0.conda
              timestamp: 1700000000
            - conda: https://conda.anaconda.org/conda-forge/linux-64/c-1-h_0.conda
              timestamp: 1700000000000
        "};
        let lock = read(lock)?;
        let uploaded =
            |name: &str| lock.bindings[&slot("default", "linux-64", Kind::Conda, name)].uploaded;

        assert_eq!(uploaded("a"), Some(Timestamp::from_second(1_710_000_000)?));
        assert_eq!(uploaded("b"), Some(Timestamp::from_second(1_700_000_000)?));
        assert_eq!(uploaded("c"), Some(Timestamp::from_second(1_700_000_000)?));
        Ok(())
    }

    /// A version 7 source package is no registry package, and neither is a local PyPI path, a
    /// reference that joins no record or two, or two PyPI names one package folds to, so each
    /// marks the lock unsupported.
    #[test]
    fn a_reference_the_reader_cannot_model_marks_the_lock() -> Result<(), Box<dyn std::error::Error>>
    {
        let archive = "https://conda.anaconda.org/conda-forge/linux-64/a-1-h_0.conda";
        let unsupported = |reference: &str, records: &str| -> Result<Option<String>, CoreError> {
            let lock = formatdoc! {"
                version: 7
                environments:
                  default:
                    packages:
                      linux-64:
                      - {reference}
                packages:
                {records}
            "};
            Ok(read(&lock)?.unsupported)
        };

        let source = unsupported(
            "conda_source: demo[abcd1234] @ .",
            "- conda_source: demo[abcd1234] @ .\n  name: demo\n  version: '1.0'",
        )?;
        assert!(source.is_some_and(|detail| detail.contains("demo[abcd1234] @ .")));
        let local = unsupported(
            "pypi: ./local",
            "- pypi: ./local\n  name: local\n  version: '0.1'",
        )?;
        assert!(local.is_some_and(|detail| detail.contains("./local")));
        let elsewhere = "- conda: https://conda.anaconda.org/conda-forge/linux-64/b-1-h_0.conda";
        assert!(unsupported(&format!("conda: {archive}"), elsewhere)?.is_some());
        let twice = format!("- conda: {archive}\n- conda: {archive}\n  md5: 9a1c");
        assert!(unsupported(&format!("conda: {archive}"), &twice)?.is_some());
        assert!(
            unsupported(&format!("conda: {archive}"), &format!("- conda: {archive}"))?.is_none()
        );
        let wheel = |name: &str| {
            format!("https://files.pythonhosted.org/packages/{name}-1-py3-none-any.whl")
        };
        let folded = formatdoc! {"
            version: 7
            environments:
              default:
                packages:
                  linux-64:
                  - pypi: {a}
                  - pypi: {b}
            packages:
            - pypi: {a}
              name: Foo_Bar
              version: '1'
            - pypi: {b}
              name: foo-bar
              version: '1'
        ", a = wheel("Foo_Bar"), b = wheel("foo-bar")};
        assert!(read(&folded)?.unsupported.is_some());
        Ok(())
    }

    /// conda tells `foo-bar` from `foo_bar` apart, while PyPI takes them for one package.
    #[test]
    fn conda_names_keep_their_separators() -> Result<(), Box<dyn std::error::Error>> {
        let lock = indoc! {"
            version: 7
            environments:
              default:
                packages:
                  linux-64:
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/foo-bar-1-h_0.conda
                  - conda: https://conda.anaconda.org/conda-forge/linux-64/foo_bar-2-h_0.conda
            packages:
            - conda: https://conda.anaconda.org/conda-forge/linux-64/foo-bar-1-h_0.conda
            - conda: https://conda.anaconda.org/conda-forge/linux-64/foo_bar-2-h_0.conda
        "};
        let lock = read(lock)?;

        assert_eq!(
            lock.bindings[&slot("default", "linux-64", Kind::Conda, "foo-bar")].version,
            "1"
        );
        assert_eq!(
            lock.bindings[&slot("default", "linux-64", Kind::Conda, "foo_bar")].version,
            "2"
        );
        assert_eq!(normal(Kind::Pypi, "Foo_Bar"), normal(Kind::Pypi, "foo-bar"));
        Ok(())
    }
}
