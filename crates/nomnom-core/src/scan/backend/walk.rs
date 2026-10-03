//! Portable directory-walk backend, built on the `ignore` crate's parallel
//! walker. No privileges, works on every platform, pays one `stat` per entry.

use std::path::Path;
use std::sync::Mutex;

use ignore::{WalkBuilder, WalkState};

use crate::scan::{
    BackendUsed, Entry, EntryKind, ScanError, ScanFailure, ScanOptions, ScanReport, ScanTable,
};

pub(crate) fn scan(root: &Path, opts: &ScanOptions) -> Result<ScanReport, ScanFailure> {
    // The only fatal case: a root we cannot even stat. Everything deeper is a
    // per-entry error, because a cleanup tool that dies on one locked directory
    // is useless.
    if std::fs::symlink_metadata(root).is_err() {
        return Err(ScanFailure::RootUnreadable(root.to_path_buf()));
    }

    let mut builder = WalkBuilder::new(root);
    // When gitignore is not respected, every filter goes off — a cleanup tool
    // must see `node_modules` and dotfiles, not silently skip the biggest
    // directories on the disk.
    let filter = opts.respect_gitignore;
    builder
        .git_ignore(filter)
        .git_global(filter)
        .git_exclude(filter)
        .ignore(filter)
        .parents(filter)
        .hidden(filter)
        .follow_links(opts.follow_symlinks);

    let entries = Mutex::new(Vec::new());
    let errors = Mutex::new(Vec::new());

    builder.build_parallel().run(|| {
        Box::new(|result| {
            opts.tick();
            match result {
                Ok(dent) => match to_entry(&dent) {
                    Ok(entry) => {
                        opts.add_bytes(entry.size);
                        entries.lock().unwrap().push(entry);
                    }
                    Err(err) => errors.lock().unwrap().push(err),
                },
                Err(err) => errors.lock().unwrap().push(walk_error(&err)),
            }
            WalkState::Continue
        })
    });

    let table =
        ScanTable::from_entries_with(root, entries.into_inner().unwrap(), opts.progress.as_deref());
    Ok(ScanReport {
        root: root.to_path_buf(),
        table,
        errors: errors.into_inner().unwrap(),
        backend_used: BackendUsed::Walk { mft_unavailable: None },
    })
}

fn to_entry(dent: &ignore::DirEntry) -> Result<Entry, ScanError> {
    let meta = dent
        .metadata()
        .map_err(|e| ScanError { path: Some(dent.path().to_path_buf()), message: e.to_string() })?;

    let kind = if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    };

    Ok(Entry {
        path: dent.path().to_path_buf(),
        kind,
        size: if kind == EntryKind::Dir { 0 } else { meta.len() },
        // A tree walk has no cheap answer for on-disk size; that is the MFT's
        // advantage, not this backend's.
        allocated: None,
        modified: meta.modified().ok(),
        // `None` covers both "platform does not support it" and "the volume has
        // last-access updates disabled", which is the Windows default.
        accessed: meta.accessed().ok(),
    })
}

fn walk_error(err: &ignore::Error) -> ScanError {
    // `ignore` wraps the offending path in a layer of its own, so the path has
    // to be unwrapped before the message is stringified.
    let mut path = None;
    let mut cursor = err;
    while let ignore::Error::WithPath { path: p, err } = cursor {
        path = Some(p.clone());
        cursor = err;
    }
    ScanError { path, message: err.to_string() }
}
