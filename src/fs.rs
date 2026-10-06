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
use tracing::{debug, error, info, instrument, warn};
use walkdir::WalkDir;

use crate::{
    error::{DedupError, Result},
    models::{FileMetadata, HashedFile},
};

#[instrument(skip(fs_scanner_tx, scan_pb, shutdown))]
pub fn scan(
    paths: Vec<PathBuf>,
    follow_links: bool,
    fs_scanner_tx: &Sender<FileMetadata>,
    scan_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> Result<usize> {
    let mut file_count = 0;

    for path in &paths {
        // Check for shutdown signal
        if shutdown.load(Ordering::Relaxed) {
            warn!("Shutdown signal received during scan");
            scan_pb.finish_with_message(format!("⚠ Interrupted: Scanned {} files", file_count));
            return Ok(file_count);
        }

        scan_pb.set_message(format!("{} files", file_count));

        if !path.exists() {
            error!("Path does not exist: {}", path.display());
            return Err(DedupError::InvalidPath {
                path: path.display().to_string(),
            });
        }

        if path.is_file() {
            if let Some(count) = send_or_stop(fs_scanner_tx, path, file_count, &shutdown)? {
                scan_pb.finish_with_message(format!("⚠ Interrupted: Scanned {} files", count));
                return Ok(count);
            }
            file_count += 1;
        } else if path.is_dir() {
            for entry in WalkDir::new(path)
                .follow_links(follow_links)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                // Check for shutdown during directory walk
                if shutdown.load(Ordering::Relaxed) {
                    warn!("Shutdown signal received during directory scan");
                    scan_pb.finish_with_message(format!(
                        "⚠ Interrupted: Scanned {} files",
                        file_count
                    ));
                    return Ok(file_count);
                }

                if !entry.file_type().is_file() {
                    continue;
                }

                if let Some(count) =
                    send_or_stop(fs_scanner_tx, entry.path(), file_count, &shutdown)?
                {
                    scan_pb.finish_with_message(format!("⚠ Interrupted: Scanned {} files", count));
                    return Ok(count);
                }
                file_count += 1;

                scan_pb.set_message(format!("{} files", file_count));
            }

            info!(
                "Scanned {} files in directory: {}",
                file_count,
                path.display()
            );
        }
    }

    scan_pb.finish_with_message(format!("✓ Scanned {} files", file_count));
    Ok(file_count)
}

/// Queue one file, treating a closed channel as the interrupt it nearly always
/// is.
///
/// Once the filter stops it drops its receiver, so a send can fail simply because
/// the interrupt reached the downstream stage before the scanner noticed it.
/// Reporting that as an error would turn every Ctrl+C into a spurious failure,
/// so it is folded into the ordinary shutdown path instead.
///
/// Returns the file count to report when the scan should stop.
fn send_or_stop(
    tx: &Sender<FileMetadata>,
    path: &Path,
    file_count: usize,
    shutdown: &AtomicBool,
) -> Result<Option<usize>> {
    match send_file(tx, path) {
        Ok(()) => Ok(None),
        Err(DedupError::ChannelClosed) if shutdown.load(Ordering::Relaxed) => {
            warn!("Downstream stopped during scan");
            Ok(Some(file_count))
        }
        Err(e) => Err(e),
    }
}

fn send_file(fs_scanner_tx: &Sender<FileMetadata>, path: &Path) -> Result<()> {
    debug!("Processing file: {}", path.display());

    let Some(file_meta) = file_metadata(path)? else {
        return Ok(());
    };

    if fs_scanner_tx.send(file_meta).is_err() {
        debug!(
            "Channel closed while sending file metadata for: {}",
            path.display()
        );
        return Err(DedupError::ChannelClosed);
    }
    Ok(())
}

/// Collect everything we need about a file from a single stat call.
///
/// Size, mtime and identity must come from one snapshot: reading them from
/// separate calls can pair a new size with an old mtime, producing a hash that
/// never matches the `(size, mtime)` pair stored alongside it and is therefore
/// never reused on the next scan.
///
/// Returns `Ok(None)` for empty files, which are not scanned.
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
    let (dev, ino) = file_identity(&metadata);

    Ok(Some(FileMetadata {
        path: path.to_path_buf(),
        // Absolute but deliberately not symlink-resolved. `canonicalize` would
        // map every link to its target, so a symlink and its target would
        // collide on the `path` unique key and the `UNIQUE` constraint would
        // silently drop one of them, hiding real duplicates. Keeping the path
        // the user scanned also keeps the report readable.
        absolute_path: std::path::absolute(path)?,
        size: metadata.len() as i64,
        mtime,
        dev,
        ino,
    }))
}

/// Filesystem object identity: `(device, inode)`.
///
/// Two paths sharing an identity are the same bytes on disk, reachable twice
/// through a symbolic or hard link. Only Unix exposes this through `std` in a
/// stable form, so other platforms report `(0, 0)` and fall back to counting
/// every row as its own copy.
#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev() as i64, metadata.ino() as i64)
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> (i64, i64) {
    (0, 0)
}

pub fn get_num_hashers() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::cmp::max(1, cores.saturating_sub(2))
}

pub fn spawn_hash_workers(
    scanned_files_rx: Receiver<FileMetadata>,
    hashed_files_tx: Sender<HashedFile>,
    hash_pb: ProgressBar,
    db_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let num_threads = get_num_hashers();
    debug!("Thread pool initialized with {} threads", num_threads);

    std::thread::spawn(move || {
        // Use rayon's parallel iterator to process files
        scanned_files_rx.into_iter().par_bridge().for_each(|file| {
            // Check for shutdown signal
            if shutdown.load(Ordering::Relaxed) {
                return;
            }

            debug!("Hashing {}", file.path.display());

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
                    warn!("Failed to hash {}: {}", file.path.display(), e);
                    hash_pb.inc(1);
                }
            }
        });
        debug!("Hash workers finished processing");
    })
}

#[instrument(skip(path))]
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
    fn hash_file_empty_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let hash = hash_file(file.path()).unwrap();
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hash_file_missing_path_errors() {
        let err = hash_file(Path::new("/definitely/not/a/real/file")).unwrap_err();
        assert!(matches!(err, DedupError::HashFile { .. }));
    }

    #[test]
    fn get_num_hashers_is_at_least_one() {
        assert!(get_num_hashers() >= 1);
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
