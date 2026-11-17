//! The "expected state" q records after pushing or refreshing a patch: a copy
//! of every file the patch touches as the patch left it, kept under
//! `.pc/.q/<patch>/`. While the patch file and its tracked files are
//! unchanged, this lets pop and diff compare against it instead of
//! re-applying the patch in a scratch tree.
//!
//! Alongside the copies, `stat` records the inode, size and timestamps of the
//! patch file and of each live file. A file whose stat still matches has not
//! been written since, so most checks need no reading at all.

use crate::fsutil::{copy_creating_dirs, files_match};
use crate::patchfile::patch_text;
use crate::state::{State, sort_tracked};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) fn meta_dir(state: &State, patch: &str) -> PathBuf {
    state.pc_dir.join(".q").join(patch)
}

pub(crate) fn expected_dir(state: &State, patch: &str) -> PathBuf {
    meta_dir(state, patch).join("expected")
}

pub(crate) fn expected_file(state: &State, patch: &str, file: &Path) -> PathBuf {
    expected_dir(state, patch).join(file)
}

/// Files the patch is expected to have deleted.
pub(crate) fn expected_deleted(state: &State, patch: &str) -> Result<HashSet<PathBuf>> {
    let list = meta_dir(state, patch).join("deleted");
    if !list.exists() {
        return Ok(HashSet::new());
    }
    Ok(fs::read_to_string(list)?
        .lines()
        .map(PathBuf::from)
        .collect())
}

/// Record the working tree state of `tracked` as `patch`'s expected state,
/// using the patch file's current text.
pub(crate) fn record_expected_state(state: &State, patch: &str, tracked: &[PathBuf]) -> Result<()> {
    let text = patch_text(&state.patch_path(patch))?;
    record_expected_state_with_text(state, patch, tracked, &text)
}

pub(crate) fn record_expected_state_with_text(
    state: &State,
    patch: &str,
    tracked: &[PathBuf],
    patch_text: &str,
) -> Result<()> {
    let meta = meta_dir(state, patch);
    let expected = meta.join("expected");
    let mut tracked = tracked.to_vec();
    sort_tracked(&mut tracked);
    let patch_path = state.patch_path(patch);

    // Only the patch text changed (as after `push --refresh`): keep the copies.
    if let Some(snapshot) = recorded_files_unchanged(state, &meta, &tracked) {
        fs::write(meta.join("patch"), patch_text)?;
        write_snapshot(&meta, &patch_path, &snapshot.files)?;
        touch_backup_timestamp(state, patch)?;
        return Ok(());
    }

    let _ = fs::remove_dir_all(&meta);
    fs::create_dir_all(&expected)?;
    let mut deleted = String::new();
    let mut stats = HashMap::new();
    for file in &tracked {
        let src = state.root.join(file);
        match fs::metadata(&src) {
            Ok(metadata) => {
                copy_creating_dirs(&src, &expected.join(file))?;
                if let Some(key) = stat_key(&metadata) {
                    stats.insert(file.clone(), key);
                }
            }
            Err(_) => {
                deleted.push_str(&file.to_string_lossy());
                deleted.push('\n');
            }
        }
    }
    fs::write(meta.join("deleted"), deleted)?;
    fs::write(meta.join("patch"), patch_text)?;
    fs::write(meta.join("files"), file_list(&tracked))?;
    write_snapshot(&meta, &patch_path, &stats)?;
    touch_backup_timestamp(state, patch)?;
    Ok(())
}

fn touch_backup_timestamp(state: &State, patch: &str) -> Result<()> {
    fs::create_dir_all(state.backup_dir(patch))?;
    File::create(state.backup_dir(patch).join(".timestamp"))?;
    Ok(())
}

/// The recorded snapshot, if the recorded expected state still holds for
/// `tracked` exactly as the working tree has them now: same file list, no
/// file written or recreated since.
fn recorded_files_unchanged(state: &State, meta: &Path, tracked: &[PathBuf]) -> Option<Snapshot> {
    let recorded_files = fs::read_to_string(meta.join("files")).ok()?;
    if recorded_files != file_list(tracked) {
        return None;
    }
    let snapshot = Snapshot::load(meta)?;
    let deleted: HashSet<PathBuf> = fs::read_to_string(meta.join("deleted"))
        .ok()?
        .lines()
        .map(PathBuf::from)
        .collect();
    for file in tracked {
        let live = state.root.join(file);
        if deleted.contains(file) {
            if live.exists() {
                return None;
            }
        } else if !snapshot.file_unchanged(file, &live) {
            return None;
        }
    }
    Some(snapshot)
}

fn file_list(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|file| format!("{}\n", file.to_string_lossy()))
        .collect()
}

/// Whether the recorded expected state still describes this patch: same patch
/// text and same tracked files as when it was recorded.
pub(crate) fn expected_state_is_current(state: &State, patch: &str) -> bool {
    state
        .tracked_files(patch)
        .is_ok_and(|tracked| expected_state_is_current_for(state, patch, &tracked))
}

/// [`expected_state_is_current`] with the patch's tracked files (as
/// [`State::tracked_files`] lists them) already at hand.
pub(crate) fn expected_state_is_current_for(
    state: &State,
    patch: &str,
    tracked: &[PathBuf],
) -> bool {
    let meta = meta_dir(state, patch);
    let Ok(recorded_files) = fs::read_to_string(meta.join("files")) else {
        return false;
    };
    if file_list(tracked) != recorded_files {
        return false;
    }
    let patch_path = state.patch_path(patch);
    if Snapshot::load(&meta).is_some_and(|snapshot| snapshot.patch_unchanged(&patch_path)) {
        return true;
    }
    let Ok(recorded_patch) = fs::read_to_string(meta.join("patch")) else {
        return false;
    };
    patch_text(&patch_path).is_ok_and(|text| text == recorded_patch)
}

/// Whether the working tree matches `patch`'s expected state for `files`.
pub(crate) fn expected_state_matches(
    state: &State,
    patch: &str,
    files: &[PathBuf],
) -> Result<bool> {
    let deleted = expected_deleted(state, patch)?;
    let snapshot = Snapshot::load(&meta_dir(state, patch));
    for file in files {
        let live = state.root.join(file);
        if deleted.contains(file) {
            if live.exists() {
                return Ok(false);
            }
            continue;
        }
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.file_unchanged(file, &live))
        {
            continue;
        }
        let expected = expected_file(state, patch, file);
        if !expected.exists() || !files_match(&expected, &live)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The stat keys recorded in `.pc/.q/<patch>/stat`. The file's own mtime is
/// the reference for racy entries: a file whose mtime is not older than it
/// may have been rewritten within the same timestamp tick, so its stat
/// proves nothing (git's "racily clean" rule).
struct Snapshot {
    patch: Option<String>,
    files: HashMap<PathBuf, String>,
    stamp: SystemTime,
}

impl Snapshot {
    fn load(meta: &Path) -> Option<Self> {
        let path = meta.join("stat");
        let text = fs::read_to_string(&path).ok()?;
        let stamp = fs::metadata(&path).ok()?.modified().ok()?;
        let mut lines = text.lines();
        let patch = lines.next()?;
        let patch = (patch != "-").then(|| patch.to_string());
        let files = lines
            .filter_map(|line| line.split_once('\t'))
            .map(|(key, file)| (PathBuf::from(file), key.to_string()))
            .collect();
        Some(Self {
            patch,
            files,
            stamp,
        })
    }

    fn patch_unchanged(&self, path: &Path) -> bool {
        self.patch
            .as_deref()
            .is_some_and(|key| self.unchanged(key, path))
    }

    fn file_unchanged(&self, file: &Path, live: &Path) -> bool {
        self.files
            .get(file)
            .is_some_and(|key| self.unchanged(key, live))
    }

    fn unchanged(&self, key: &str, path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        stat_key(&metadata).as_deref() == Some(key)
            && metadata
                .modified()
                .is_ok_and(|modified| modified < self.stamp)
    }
}

/// Write the snapshot after the files it describes have been stat'ed, so its
/// own mtime is never older than theirs.
fn write_snapshot(meta: &Path, patch_path: &Path, files: &HashMap<PathBuf, String>) -> Result<()> {
    let mut out = fs::metadata(patch_path)
        .ok()
        .and_then(|metadata| stat_key(&metadata))
        .unwrap_or_else(|| "-".to_string());
    out.push('\n');
    let mut files: Vec<_> = files.iter().collect();
    files.sort();
    for (file, key) in files {
        out.push_str(key);
        out.push('\t');
        out.push_str(&file.to_string_lossy());
        out.push('\n');
    }
    // A fresh file, so the stamp is its creation time even when the
    // contents happen to be unchanged.
    let path = meta.join("stat");
    let _ = fs::remove_file(&path);
    fs::write(path, out)?;
    Ok(())
}

/// What changes whenever a file is written, replaced or re-created.
#[cfg(unix)]
fn stat_key(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!(
        "{}:{}:{}:{}.{}:{}.{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    ))
}

#[cfg(not(unix))]
fn stat_key(_metadata: &fs::Metadata) -> Option<String> {
    None
}
