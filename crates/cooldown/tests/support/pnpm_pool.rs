//! A machine-wide pool of finished pnpm metadata cache entries that seeds each fixture's private
//! pnpm cache.
//!
//! A fixture's cache starts as hard links to every pooled entry, and after each command the
//! fixture runs, every new entry of its cache that parses is linked into the pool.
//! pnpm never rewrites a metadata entry in place: it writes a temp file and renames it over the
//! entry, which replaces only the fixture's own directory entry and leaves the pooled file intact.
//! So no two fixtures ever write one file, while a fixture still starts with the metadata earlier
//! fixtures fetched instead of downloading it again.
//!
//! The pool takes an entry only after it parses the way pnpm reads it, because one pnpm process
//! can damage its own entry: two of its writers may share one temp file name.
//! A damaged entry therefore stays in the fixture that wrote it.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};

/// The root of a pool of pnpm metadata cache entries, mirroring their paths below a cache dir.
#[derive(Debug, Clone)]
pub(super) struct PnpmPool {
    root: PathBuf,
}

impl PnpmPool {
    /// The pool every fixture of every test process on this machine shares.
    pub(super) fn shared() -> Self {
        Self {
            root: std::env::temp_dir().join("cooldown-it-pnpm-pool"),
        }
    }

    /// A pool rooted at `root`, for a test that must observe the pool in isolation.
    pub(super) fn at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Hard-links every pooled entry into `cache` at the same relative path.
    ///
    /// Seeding is best effort: an entry that cannot be linked stays cold, and pnpm fetches it.
    pub(super) fn seed(&self, cache: &Path) {
        let mut failed = 0_usize;
        for relative in metadata_entries(&self.root) {
            let target = cache.join(&relative);
            let linked = target
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::hard_link(self.root.join(&relative), &target));
            if linked.is_err() {
                failed += 1;
            }
        }
        if failed > 0 {
            eprintln!(
                "note: {failed} pooled pnpm metadata entries could not be linked into {}",
                cache.display()
            );
        }
    }

    /// Links every entry of `cache` the pool lacks into the pool, skipping any that does not
    /// parse.
    ///
    /// Call it only while no process of the fixture runs, so no pnpm can replace an entry between
    /// its validation and its link.
    /// The first fixture to publish an entry wins.
    /// A later fixture whose pnpm refreshes the entry keeps the refreshed copy to itself, so a
    /// stale pooled entry costs the fixtures it seeds a refetch, the same as a stale entry of
    /// pnpm's own cache.
    pub(super) fn harvest(&self, cache: &Path) {
        for relative in metadata_entries(cache) {
            let pooled = self.root.join(&relative);
            if pooled.symlink_metadata().is_ok() {
                continue;
            }
            let source = cache.join(&relative);
            match std::fs::read(&source) {
                Ok(bytes) if is_complete_entry(&bytes) => {}
                Ok(_) => {
                    eprintln!(
                        "note: not pooling unreadable pnpm metadata entry {}",
                        source.display()
                    );
                    continue;
                }
                Err(_) => continue,
            }
            let published = pooled
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::hard_link(&source, &pooled));
            match published {
                Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
                    eprintln!("note: could not pool {}: {err}", source.display());
                }
                Ok(()) | Err(_) => {}
            }
        }
    }
}

/// Whether `bytes` read back the way pnpm's `loadMeta` reads a metadata entry: a JSON object on
/// the header line followed by a JSON object as the body.
///
/// Both must be objects, not merely JSON, since `loadMeta` reads the header's `etag` and sets it on
/// the body, and gives up on an entry where either step throws.
fn is_complete_entry(bytes: &[u8]) -> bool {
    let Some(newline) = bytes.iter().position(|&byte| byte == b'\n') else {
        return false;
    };
    let (header, rest) = bytes.split_at(newline);
    let Some((_, body)) = rest.split_first() else {
        return false;
    };
    let is_object = |json: &[u8]| serde_json::from_slice::<AnyObject>(json).is_ok();
    is_object(header) && is_object(body)
}

/// A JSON object of any content, parsed without keeping any of it.
struct AnyObject;

impl<'de> Deserialize<'de> for AnyObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(AnyObjectVisitor)
    }
}

struct AnyObjectVisitor;

impl<'de> Visitor<'de> for AnyObjectVisitor {
    type Value = AnyObject;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AnyObject, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(AnyObject)
    }
}

/// The paths, relative to `root`, of every finished metadata entry below it.
///
/// An entry is a `.jsonl` file below a `metadata*` directory, the layout pnpm 11 gives
/// `<cache>/v11/metadata*/<registry>/<package>.jsonl`.
/// This leaves out pnpm's in-flight `<entry>_tmp_<pid>_<thread>` files and the
/// `lockfile-verified.jsonl` log, which pnpm appends to in place.
/// A missing or unreadable directory yields nothing.
fn metadata_entries(root: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(root.join(&relative)) else {
            continue;
        };
        for entry in listing.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let child = relative.join(entry.file_name());
            if file_type.is_dir() {
                pending.push(child);
            } else if file_type.is_file() && is_metadata_entry(&child) {
                entries.push(child);
            }
        }
    }
    entries
}

fn is_metadata_entry(relative: &Path) -> bool {
    let in_metadata_dir = relative.parent().is_some_and(|parent| {
        parent.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with("metadata")
        })
    });
    let name = relative
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    in_metadata_dir && name.ends_with(".jsonl") && !name.contains("_tmp_")
}
