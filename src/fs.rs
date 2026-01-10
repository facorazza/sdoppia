use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use indicatif::ProgressBar;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, warn};
use walkdir::WalkDir;

use crate::{
    error::{DedupError, Result},
    models::{FileMetadata, HashedFile},
};

#[instrument(skip(fs_scanner_tx, scan_pb))]
pub async fn scan(
    paths: Vec<PathBuf>,
    follow_links: bool,
    fs_scanner_tx: &mpsc::Sender<FileMetadata>,
    scan_pb: ProgressBar,
    // shutdown: Arc<tokio::sync::Notify>,
) -> Result<usize> {
    let mut file_count = 0;

    for path in &paths {
        if !path.exists() {
            error!("Path does not exist: {}", path.display());
            return Err(DedupError::InvalidPath {
                path: path.display().to_string(),
            });
        }

        if file_count % 10 == 0 {
            scan_pb.set_message(format!("{} files", file_count));
        }

        if path.is_file() {
            send_file(fs_scanner_tx, path).await?;
            file_count += 1;
        } else if path.is_dir() {
            for entry in WalkDir::new(path)
                .follow_links(follow_links)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !entry.file_type().is_file() {
                    continue;
                }

                send_file(fs_scanner_tx, entry.path()).await?;
                file_count += 1;

                if file_count % 10 == 0 {
                    scan_pb.set_message(format!("{} files", file_count));
                }
            }

            info!(
                "Scanned {} files in directory: {}",
                file_count,
                path.display()
            );
        }
        scan_pb.finish_with_message(format!("{} files", file_count));
    }

    scan_pb.finish_with_message(format!("✓ Scanned {} files", file_count));
    Ok(file_count)
}

async fn send_file(fs_scanner_tx: &mpsc::Sender<FileMetadata>, path: &Path) -> Result<()> {
    debug!("Processing file: {}", path.display());

    let metadata = std::fs::metadata(path)?;
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

    if fs_scanner_tx.send(file_meta).await.is_err() {
        debug!(
            "Channel closed while sending file metadata for: {}",
            path.display()
        );
        return Err(DedupError::ChannelClosed);
    }
    Ok(())
}

fn get_modified_time(path: &Path) -> Result<i64> {
    let metadata = std::fs::metadata(path)?;
    let modified = metadata.modified()?;
    let duration = modified.duration_since(SystemTime::UNIX_EPOCH)?;
    Ok(duration.as_secs() as i64)
}

pub fn get_num_hashers() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::cmp::max(1, cores.saturating_sub(2))
}

pub async fn hash_worker(
    scanned_files_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<FileMetadata>>>,
    db_insertion_tx: mpsc::Sender<HashedFile>,
    hash_pb: ProgressBar,
    db_pb: ProgressBar,
) {
    loop {
        let file = {
            let mut locked_rx = scanned_files_rx.lock().await;
            locked_rx.recv().await
        };

        match file {
            Some(file) => match hash_file(&file.absolute_path) {
                Ok(hash) => {
                    let hashed = HashedFile {
                        absolute_path: file.absolute_path,
                        size: file.size,
                        mtime: file.mtime,
                        hash,
                    };

                    if db_insertion_tx.send(hashed).await.is_ok() {
                        hash_pb.inc(1);
                        db_pb.inc_length(1);
                    }
                }
                Err(e) => {
                    warn!("Failed to hash {}: {}", file.path.display(), e);
                    hash_pb.inc(1);
                }
            },
            None => break,
        }
    }
}

#[instrument(skip(path))]
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];

    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let hash = hasher.finalize();
    Ok(format!("{:x}", hash))
}
