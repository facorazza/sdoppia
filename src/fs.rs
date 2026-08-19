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
            send_file(fs_scanner_tx, path)?;
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

                send_file(fs_scanner_tx, entry.path())?;
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

fn send_file(fs_scanner_tx: &Sender<FileMetadata>, path: &Path) -> Result<()> {
    debug!("Processing file: {}", path.display());

    let metadata = std::fs::metadata(path).map_err(|e| DedupError::Metadata {
        path: path.display().to_string(),
        source: e,
    })?;
    let absolute_path = path.canonicalize()?;
    let size = metadata.len() as i64;
    if size == 0 {
        return Ok(());
    }
    let mtime = get_modified_time(&absolute_path)?;

    let file_meta = FileMetadata {
        path: path.to_path_buf(),
        absolute_path,
        size,
        mtime,
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

fn get_modified_time(path: &Path) -> Result<i64> {
    let metadata = std::fs::metadata(path).map_err(|e| DedupError::Metadata {
        path: path.display().to_string(),
        source: e,
    })?;
    let modified = metadata
        .modified()
        .map_err(|e| DedupError::ModificationTime {
            path: path.display().to_string(),
            source: e,
        })?;
    let duration = modified.duration_since(SystemTime::UNIX_EPOCH)?;
    Ok(duration.as_secs() as i64)
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
    use std::io::Write;

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
    fn get_modified_time_returns_positive_epoch() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mtime = get_modified_time(file.path()).unwrap();
        assert!(mtime > 0);
    }
}
