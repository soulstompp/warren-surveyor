// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The kit built into the bench: its questions, their bound values and `surveyor.sql`. A run that
//! names no questions reads them from the bench's cache directory, where they are written once.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::digest::fingerprint;

// `FILES`: every file under the crate's `kit/`, by its path there, written by build.rs.
include!(concat!(env!("OUT_DIR"), "/kit.rs"));

/// The questions directory, under the kit.
pub const QUESTIONS: &str = "questions";
/// The binds file, under the kit.
pub const BINDS: &str = "binds.tsv";
/// The script that turns the surveyor on or off, under the kit.
pub const SURVEYOR_SQL: &str = "surveyor.sql";

/// A file of the kit, by its path under it.
pub fn file(path: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

/// The bench's cache directory: `$XDG_CACHE_HOME/warren-bench`, else `~/.cache/warren-bench`.
pub fn cache_root() -> Result<PathBuf, String> {
    cache_root_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// `XDG_CACHE_HOME` counts only as an absolute path, and `HOME` only when not empty.
fn cache_root_from(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, String> {
    if let Some(x) = xdg.map(PathBuf::from).filter(|x| x.is_absolute()) {
        return Ok(x.join("warren-bench"));
    }
    match home.filter(|h| !h.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(".cache").join("warren-bench")),
        None => Err(
            "neither XDG_CACHE_HOME nor HOME is set, so the bench has no cache directory: \
             give --questions, --binds and --work"
                .into(),
        ),
    }
}

/// A fingerprint of every file of the kit, by path and content: its directory's name in the cache.
pub fn fingerprint_of_files() -> String {
    let parts: Vec<&[u8]> = FILES.iter().flat_map(|(p, b)| [p.as_bytes(), *b]).collect();
    fingerprint(&parts)
}

/// The kit's directory in the cache, `<root>/kit/<fingerprint>`. It is written once, its files
/// read-only, and where it exists it is read as it is.
pub fn cached(root: &Path) -> Result<PathBuf, String> {
    let parent = root.join("kit");
    let dir = parent.join(fingerprint_of_files());
    if dir.is_dir() {
        return Ok(dir);
    }
    fs::create_dir_all(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    // written beside it under another name, then renamed into place, so that a directory under
    // the kit's own name always holds every file
    let partial = parent.join(format!(
        ".{}.{}",
        fingerprint_of_files(),
        std::process::id()
    ));
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
    }
    for path in write(&partial)? {
        let mut perms = fs::metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    match fs::rename(&partial, &dir) {
        Ok(()) => Ok(dir),
        // another bench wrote it first
        Err(_) if dir.is_dir() => {
            let _ = fs::remove_dir_all(&partial);
            Ok(dir)
        }
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

/// Writes every file of the kit under `dir`, and returns their paths. Where any of them exists
/// already it writes nothing.
pub fn write(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let paths: Vec<PathBuf> = FILES.iter().map(|(p, _)| dir.join(p)).collect();
    let present: Vec<&PathBuf> = paths
        .iter()
        .filter(|p| fs::symlink_metadata(p).is_ok())
        .collect();
    if let Some(first) = present.first() {
        return Err(format!(
            "{}: the kit is written only where none of its files exists, and {} do, {} among them",
            dir.display(),
            present.len(),
            first.display()
        ));
    }
    for ((_, bytes), path) in FILES.iter().zip(&paths) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut f| f.write_all(bytes))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::questions::{files, QuestionSet};

    /// The crate's `kit/`, which the bench is built from.
    fn source() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("kit")
    }

    /// A scratch directory of its own for one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("warren-bench-kit-{tag}-{}", std::process::id()));
        if dir.exists() {
            make_writable(&dir);
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_writable(dir: &Path) {
        for p in files(dir).unwrap() {
            let p = dir.join(p);
            let mut perms = fs::metadata(&p).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&p, perms).unwrap();
        }
    }

    #[test]
    fn the_bench_holds_every_file_of_its_kit_directory_as_it_is() {
        let src = source();
        let on_disk: Vec<String> = files(&src)
            .unwrap()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let built_in: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(built_in, on_disk);
        for (p, bytes) in FILES {
            assert!(
                *bytes == fs::read(src.join(p)).unwrap().as_slice(),
                "{p} differs"
            );
        }
        assert!(file(BINDS).is_some() && file(SURVEYOR_SQL).is_some());
    }

    #[test]
    fn the_cached_questions_have_the_fingerprint_of_the_kit_directory() {
        let root = scratch("fingerprint");
        let dir = cached(&root).unwrap();
        let cached = QuestionSet::load(&dir.join(QUESTIONS), &dir.join(BINDS)).unwrap();
        let src = source();
        let in_tree = QuestionSet::load(&src.join(QUESTIONS), &src.join(BINDS)).unwrap();
        assert_eq!(cached.hash, in_tree.hash);
        let names = |s: &QuestionSet| -> Vec<String> {
            s.questions.iter().map(|q| q.name.clone()).collect()
        };
        assert_eq!(names(&cached), names(&in_tree));
        make_writable(&root);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_cache_is_written_once_under_the_kits_fingerprint_and_read_only() {
        let root = scratch("cache");
        let dir = cached(&root).unwrap();
        assert_eq!(dir, root.join("kit").join(fingerprint_of_files()));
        for (p, bytes) in FILES {
            let path = dir.join(p);
            assert!(*bytes == fs::read(&path).unwrap().as_slice(), "{p} differs");
            assert!(fs::metadata(&path).unwrap().permissions().readonly(), "{p}");
        }
        // a second run reads the directory as it is
        assert_eq!(cached(&root).unwrap(), dir);
        let left: Vec<_> = fs::read_dir(root.join("kit"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [OsString::from(fingerprint_of_files())]);
        make_writable(&root);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_cache_directory_is_xdg_cache_home_else_home_dot_cache() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            cache_root_from(os("/x/cache"), os("/home/u")).unwrap(),
            Path::new("/x/cache/warren-bench")
        );
        for xdg in [None, os(""), os("relative/cache")] {
            assert_eq!(
                cache_root_from(xdg, os("/home/u")).unwrap(),
                Path::new("/home/u/.cache/warren-bench")
            );
        }
        assert!(cache_root_from(None, None).is_err());
        assert!(cache_root_from(None, os("")).is_err());
    }

    #[test]
    fn writing_the_kit_refuses_a_directory_holding_any_of_its_files() {
        let dir = scratch("write");
        let written = write(&dir).unwrap();
        assert_eq!(written.len(), FILES.len());
        for (p, bytes) in FILES {
            assert!(
                *bytes == fs::read(dir.join(p)).unwrap().as_slice(),
                "{p} differs"
            );
        }
        let e = write(&dir).err().unwrap_or_default();
        assert!(e.contains("none of its files exists"), "{e}");
        // one file present is enough, and nothing else is written
        let one = scratch("write-one");
        fs::write(one.join(SURVEYOR_SQL), "mine\n").unwrap();
        assert!(write(&one).is_err());
        assert_eq!(
            fs::read_to_string(one.join(SURVEYOR_SQL)).unwrap(),
            "mine\n"
        );
        assert_eq!(files(&one).unwrap(), [PathBuf::from(SURVEYOR_SQL)]);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&one).unwrap();
    }
}
