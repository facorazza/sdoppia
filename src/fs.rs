use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use crossbeam_channel::{Receiver, Sender};
use indicatif::ProgressBar;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{
    error::{DedupError, Result},
    models::{FileMetadata, HashedFile},
    note,
};

/// What a scan covered, and what it could not reach.
#[derive(Clone, Copy, Debug)]
pub struct ScanOutcome {
    pub files: usize,
    /// Paths walkdir refused to visit. Reported rather than swallowed: a scan
    /// that quietly skipped a subtree looks exactly like one that covered it.
    pub errors: usize,
}

pub fn scan(
    paths: Vec<PathBuf>,
    follow_links: bool,
    exclude: &[PathBuf],
    fs_scanner_tx: &Sender<FileMetadata>,
    scan_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> Result<ScanOutcome> {
    let mut file_count = 0;
    let mut error_count = 0;

    for path in &paths {
        // Check for shutdown signal
        if shutdown.load(Ordering::Relaxed) {
            scan_pb.finish_with_message(format!("⚠ Interrupted: Scanned {} files", file_count));
            return Ok(ScanOutcome {
                files: file_count,
                errors: error_count,
            });
        }

        scan_pb.set_message(format!("{} files", file_count));

        if !path.exists() {
            // Returned rather than logged: `main` prints it, so logging here
            // would say it twice.
            return Err(DedupError::InvalidPath {
                path: path.display().to_string(),
            });
        }

        if path.is_file() {
            if let Some(count) = send_or_stop(fs_scanner_tx, path, file_count, exclude, &shutdown)?
            {
                scan_pb.finish_with_message(format!("⚠ Interrupted: Scanned {} files", count));
                return Ok(ScanOutcome {
                    files: count,
                    errors: error_count,
                });
            }
            file_count += 1;
        } else if path.is_dir() {
            for entry in WalkDir::new(path).follow_links(follow_links).into_iter() {
                // Check for shutdown during directory walk
                if shutdown.load(Ordering::Relaxed) {
                    scan_pb.finish_with_message(format!(
                        "⚠ Interrupted: Scanned {} files",
                        file_count
                    ));
                    return Ok(ScanOutcome {
                        files: file_count,
                        errors: error_count,
                    });
                }

                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        error_count += 1;
                        note(
                            &scan_pb,
                            &format!("Skipped an unreadable path under {}: {e}", path.display()),
                        );
                        continue;
                    }
                };

                if !entry.file_type().is_file() {
                    continue;
                }

                if let Some(count) =
                    send_or_stop(fs_scanner_tx, entry.path(), file_count, exclude, &shutdown)?
                {
                    scan_pb.finish_with_message(format!("⚠ Interrupted: Scanned {} files", count));
                    return Ok(ScanOutcome {
                        files: count,
                        errors: error_count,
                    });
                }
                file_count += 1;

                scan_pb.set_message(format!("{} files", file_count));
            }
        }
    }

    let outcome = ScanOutcome {
        files: file_count,
        errors: error_count,
    };

    if outcome.errors > 0 {
        scan_pb.finish_with_message(format!(
            "⚠ Scanned {} files, {} paths unreadable",
            outcome.files, outcome.errors
        ));
    } else {
        scan_pb.finish_with_message(format!("✓ Scanned {} files", outcome.files));
    }

    Ok(outcome)
}

/// Queue one file, treating a closed channel as the interrupt it nearly always
/// is: once the filter stops it drops its receiver, so a send can fail simply
/// because the interrupt reached it first. Reporting that as an error would turn
/// every Ctrl+C into a spurious failure.
///
/// Returns the file count to report when the scan should stop.
fn send_or_stop(
    tx: &Sender<FileMetadata>,
    path: &Path,
    file_count: usize,
    exclude: &[PathBuf],
    shutdown: &AtomicBool,
) -> Result<Option<usize>> {
    match send_file(tx, path, exclude) {
        Ok(()) => Ok(None),
        // Reported by the caller, which finishes its bar with the count.
        Err(DedupError::ChannelClosed) if shutdown.load(Ordering::Relaxed) => Ok(Some(file_count)),
        Err(e) => Err(e),
    }
}

fn send_file(fs_scanner_tx: &Sender<FileMetadata>, path: &Path, exclude: &[PathBuf]) -> Result<()> {
    let Some(file_meta) = file_metadata(path)? else {
        return Ok(());
    };

    // Compared against the absolute path the entry will be stored under, which
    // `file_metadata` has already resolved, so this costs no extra syscall.
    if exclude.contains(&file_meta.absolute_path) {
        return Ok(());
    }

    if fs_scanner_tx.send(file_meta).is_err() {
        return Err(DedupError::ChannelClosed);
    }
    Ok(())
}

/// Everything needed about a file, from a single stat call: separate calls can
/// pair a new size with an old mtime, producing a hash that never matches the
/// `(size, mtime)` stored beside it and so is never reused. Empty files are not
/// scanned, and return `Ok(None)`.
fn file_metadata(path: &Path) -> Result<Option<FileMetadata>> {
    let metadata = std::fs::metadata(path).map_err(|e| DedupError::Metadata {
        path: path.display().to_string(),
        source: e,
    })?;

    if metadata.len() == 0 {
        return Ok(None);
    }

    let modified = metadata
        .modified()
        .map_err(|e| DedupError::ModificationTime {
            path: path.display().to_string(),
            source: e,
        })?;
    let mtime = modified.duration_since(SystemTime::UNIX_EPOCH)?.as_secs() as i64;
    let (dev, ino) = file_identity(&metadata, path);

    Ok(Some(FileMetadata {
        path: path.to_path_buf(),
        // Absolute but not symlink-resolved: `canonicalize` would map a link
        // and its target onto one path, and the UNIQUE constraint would drop
        // one of the two rows, hiding a real duplicate.
        absolute_path: std::path::absolute(path)?,
        size: metadata.len() as i64,
        mtime,
        dev,
        ino,
    }))
}

/// Filesystem object identity: the device and inode behind a path, which is what
/// tells a genuine duplicate from a link to a file already counted.
///
/// An `ino` of `0` means the platform cannot report one, and the query falls back
/// to treating each path as its own file.
#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata, _path: &Path) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev() as i64, metadata.ino() as i64)
}

#[cfg(windows)]
fn file_identity(_metadata: &std::fs::Metadata, path: &Path) -> (i64, i64) {
    windows_identity(path)
}

/// Volume serial number and file index, read from one
/// `BY_HANDLE_FILE_INFORMATION` so the path is opened once.
///
/// A zero index is passed through, which sends the query back to path identity.
/// FAT and exFAT synthesize an index that can move, and one that changed between
/// scans would read as a different file.
///
/// The prune also reads the volume serial on its own, for a directory, so
/// `FILE_FLAG_BACKUP_SEMANTICS` below is what lets it open one.
#[cfg(windows)]
pub(crate) fn windows_identity(path: &Path) -> (i64, i64) {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    // SAFETY: `wide` is NUL-terminated and outlives the call; the null
    // security-attributes and template-handle arguments are legitimate; the
    // handle is closed on every path out; and the call's own return value is
    // checked before `info` is read.
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );

        if handle == INVALID_HANDLE_VALUE {
            return (0, 0);
        }

        let mut info = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        CloseHandle(handle);

        if ok == 0 {
            return (0, 0);
        }

        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        (info.dwVolumeSerialNumber as i64, index as i64)
    }
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_metadata: &std::fs::Metadata, _path: &Path) -> (i64, i64) {
    (0, 0)
}

pub fn spawn_hash_workers(
    scanned_files_rx: Receiver<FileMetadata>,
    hashed_files_tx: Sender<HashedFile>,
    hash_pb: ProgressBar,
    db_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Use rayon's parallel iterator to process files
        scanned_files_rx.into_iter().par_bridge().for_each(|file| {
            // Check for shutdown signal
            if shutdown.load(Ordering::Relaxed) {
                return;
            }

            match hash_file(&file.absolute_path) {
                Ok(hash) => {
                    let hashed = HashedFile {
                        absolute_path: file.absolute_path,
                        size: file.size,
                        mtime: file.mtime,
                        hash,
                        dev: file.dev,
                        ino: file.ino,
                    };

                    if hashed_files_tx.send(hashed).is_ok() {
                        hash_pb.inc(1);
                        db_pb.inc_length(1);
                    }
                }
                Err(e) => {
                    note(
                        &hash_pb,
                        &format!("Failed to hash {}: {e}", file.path.display()),
                    );
                    hash_pb.inc(1);
                }
            }
        });
    })
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| DedupError::HashFile {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 1024 * 1024]; // 1 MB buffer

    loop {
        let n = file.read(&mut buffer).map_err(|e| DedupError::HashFile {
            path: path.display().to_string(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let hash = hasher.finalize();
    Ok(hash.iter().map(|b| format!("{:02x}", b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn hash_file_matches_known_sha256() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"hello world\n").unwrap();
        file.flush().unwrap();

        let hash = hash_file(file.path()).unwrap();
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn hash_file_missing_path_errors() {
        let err = hash_file(Path::new("/definitely/not/a/real/file")).unwrap_err();
        assert!(matches!(err, DedupError::HashFile { .. }));
    }

    #[test]
    fn file_metadata_reports_positive_epoch_and_real_identity() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"hello world\n").unwrap();
        file.flush().unwrap();

        let meta = file_metadata(file.path()).unwrap().unwrap();
        assert_eq!(meta.size, 12);
        assert!(meta.mtime > 0);
        assert_eq!(
            meta.absolute_path,
            std::path::absolute(file.path()).unwrap()
        );

        // The identity must be stable across calls for the same file, and must
        // not be the "unknown" sentinel, otherwise hard-link detection silently
        // degrades to counting rows.
        let again = file_metadata(file.path()).unwrap().unwrap();
        assert_eq!((meta.dev, meta.ino), (again.dev, again.ino));
        #[cfg(unix)]
        assert_ne!(meta.ino, 0, "unix must report a real inode");
    }

    #[test]
    fn file_metadata_skips_empty_files() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            file_metadata(file.path()).unwrap().is_none(),
            "empty files are not scanned"
        );
    }

    #[test]
    fn file_metadata_missing_path_errors() {
        let err = file_metadata(Path::new("/definitely/not/a/real/file")).unwrap_err();
        assert!(matches!(err, DedupError::Metadata { .. }));
    }

    #[test]
    fn distinct_paths_to_one_inode_share_identity() {
        // This is what makes a hard link stop looking like reclaimable waste.
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"linked").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("hardlink");
        fs::hard_link(file.path(), &link).unwrap();

        let original = file_metadata(file.path()).unwrap().unwrap();
        let via_link = file_metadata(&link).unwrap().unwrap();

        assert_eq!(
            (original.dev, original.ino),
            (via_link.dev, via_link.ino),
            "a hard link must share the original's identity"
        );
        assert_ne!(
            original.absolute_path, via_link.absolute_path,
            "the stored paths must stay distinct so neither row is lost"
        );
    }
}
