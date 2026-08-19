use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender};
use indicatif::{ProgressBar, ProgressStyle};
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use tracing::{debug, info, instrument, warn};

use crate::{
    error::Result,
    models::{Duplicates, FileMetadata, HashedFile},
};

#[instrument(skip(db_path))]
pub async fn init_database(db_path: &Path) -> Result<SqlitePool> {
    let db_url = format!("sqlite:{}", db_path.display());
    debug!("Connecting to database: {}", db_url);

    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePool::connect_with(options).await?;

    sqlx::query("PRAGMA synchronous = NORMAL")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA cache_size = -64000")
        .execute(&pool)
        .await?;
    sqlx::query("PRAGMA temp_store = MEMORY")
        .execute(&pool)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS hashes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_hash ON hashes(hash)")
        .execute(&pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_size ON hashes(size)")
        .execute(&pool)
        .await?;

    debug!("Database initialized successfully");
    Ok(pool)
}

pub async fn database_writer(
    pool: SqlitePool,
    rx: Receiver<HashedFile>,
    db_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> Result<usize> {
    let mut buffer = Vec::new();
    let mut total_inserted = 0;

    loop {
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(file) => buffer.push(file),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if !buffer.is_empty() {
            match save_hashes(&pool, &buffer).await {
                Ok(count) => {
                    total_inserted += count;
                    db_pb.inc(count as u64);
                }
                Err(e) => {
                    warn!("Batch insert failed: {}", e);
                }
            }
            buffer.clear();
        }

        // Check for shutdown signal
        if shutdown.load(Ordering::Relaxed) {
            warn!("Database writer received shutdown signal");
            break;
        }

        // Exit if channel is disconnected and buffer is flushed
        if disconnected {
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    db_pb.finish_with_message(format!("Inserted {} records", total_inserted));
    Ok(total_inserted)
}

async fn save_hashes(pool: &SqlitePool, files: &[HashedFile]) -> Result<usize> {
    let mut tx = pool.begin().await?;

    for file in files {
        sqlx::query("INSERT OR REPLACE INTO hashes (path, hash, size, mtime) VALUES (?, ?, ?, ?)")
            .bind(file.absolute_path.to_string_lossy())
            .bind(&file.hash)
            .bind(file.size)
            .bind(file.mtime)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(files.len())
}

pub async fn filter_files(
    pool: SqlitePool,
    scanned_files_rx: Receiver<FileMetadata>,
    filtered_files_tx: Sender<FileMetadata>,
    rehash: bool,
    scan_pb: ProgressBar,
    hash_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> Result<usize> {
    let mut sent_count = 0;
    let mut cached_count = 0;

    loop {
        // Check for shutdown signal
        if shutdown.load(Ordering::Relaxed) {
            warn!("Filter received shutdown signal, exiting");
            break;
        }

        let file = match scanned_files_rx.try_recv() {
            Ok(file) => file,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                continue;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
        };

        if !rehash {
            match sqlx::query("SELECT mtime FROM hashes WHERE path = ?")
                .bind(file.absolute_path.to_string_lossy())
                .fetch_one(&pool)
                .await
            {
                Ok(row) => {
                    let stored_mtime: i64 = row.get("mtime");
                    if stored_mtime == file.mtime {
                        // File hasn't been modified, use cached hash
                        cached_count += 1;
                        continue;
                    }
                    // File has been modified, need to rehash
                }
                Err(sqlx::Error::RowNotFound) => (),
                Err(e) => {
                    warn!("Database query error: {}", e);
                    continue;
                }
            }
        }

        // Send without blocking the async runtime: if the hash workers are
        // behind, yield briefly and retry until space frees up or shutdown.
        let mut file = file;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(sent_count);
            }
            match filtered_files_tx.try_send(file) {
                Ok(()) => {
                    sent_count += 1;
                    break;
                }
                Err(crossbeam_channel::TrySendError::Full(f)) => {
                    file = f;
                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    return Ok(sent_count);
                }
            }
        }
        hash_pb.set_length(sent_count as u64);

        scan_pb.set_message(format!(
            "{} already hashed files, {} to hash",
            cached_count, sent_count
        ));
    }

    scan_pb.finish_with_message(format!(
        "Cached: {}, Need hashing: {}",
        cached_count, sent_count
    ));

    Ok(sent_count)
}

#[instrument(skip(pool))]
pub async fn export_duplicates(
    pool: &SqlitePool,
    output: Option<&Path>,
    min_size: i64,
) -> Result<()> {
    info!("Finding duplicates...");

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    pb.set_message("Querying database for duplicates...");

    let hash_query = if min_size > 0 {
        sqlx::query(
            r#"
            SELECT hash, COUNT(*) as count, size
            FROM hashes
            WHERE size >= ?
            GROUP BY hash
            HAVING count > 1
            ORDER BY size DESC
            "#,
        )
        .bind(min_size)
    } else {
        sqlx::query(
            r#"
            SELECT hash, COUNT(*) as count, size
            FROM hashes
            GROUP BY hash
            HAVING count > 1
            ORDER BY size DESC
            "#,
        )
    };

    let hash_rows = hash_query.fetch_all(pool).await?;

    if hash_rows.is_empty() {
        pb.finish_with_message("No duplicates found!");
        // Still write the report so an explicitly requested output file is
        // always produced, even when there is nothing to report.
        let output_text = format!(
            "=== DUPLICATE FILES REPORT ===\nGenerated: {}\nTotal duplicate files: 0\nWasted space: 0 bytes\nDuplicate groups: 0\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        if let Some(output_path) = output {
            let file = File::create(output_path)?;
            let mut writer = BufWriter::new(file);
            writer.write_all(output_text.as_bytes())?;
            writer.flush()?;
            info!("Duplicates exported to: {}", output_path.display());
        } else {
            println!("{}", output_text);
        }
        return Ok(());
    }

    pb.set_message(format!(
        "Processing {} duplicate groups...",
        hash_rows.len()
    ));
    pb.set_length(hash_rows.len() as u64);

    let mut duplicate_groups = Vec::new();

    for row in hash_rows {
        let hash: String = row.get("hash");
        let size: i64 = row.get("size");

        let file_rows = sqlx::query("SELECT path FROM hashes WHERE hash = ?")
            .bind(&hash)
            .fetch_all(pool)
            .await?;

        let files: Vec<String> = file_rows
            .into_iter()
            .map(|r| r.get::<String, _>("path"))
            .collect();

        if files.len() < 2 {
            continue;
        }

        duplicate_groups.push(Duplicates { hash, size, files });
        pb.inc(1);
    }

    pb.finish_with_message(format!("Found {} duplicate groups", duplicate_groups.len()));

    let total_duplicate_count: usize = duplicate_groups.iter().map(|g| g.files.len() - 1).sum();
    let wasted_space: i64 = duplicate_groups.iter().map(|g| g.wasted_space()).sum();

    let mut output_lines = Vec::new();

    output_lines.push("=== DUPLICATE FILES REPORT ===".to_string());
    output_lines.push(format!(
        "Generated: {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    output_lines.push(format!("Total duplicate files: {}", total_duplicate_count));
    output_lines.push(format!(
        "Wasted space: {}",
        Duplicates::format_size(wasted_space)
    ));
    output_lines.push(format!("Duplicate groups: {}", duplicate_groups.len()));
    output_lines.push(String::new());

    for (idx, group) in duplicate_groups.iter().enumerate() {
        output_lines.push(format!("--- Group {} ---", idx + 1));
        output_lines.push(format!("Hash: {}", group.hash));
        output_lines.push(format!("Size: {}", Duplicates::format_size(group.size)));
        output_lines.push(format!("Copies: {}", group.files.len()));
        output_lines.push(format!(
            "Wasted: {}",
            Duplicates::format_size(group.wasted_space())
        ));
        output_lines.push("Files:".to_string());

        for path in &group.files {
            output_lines.push(format!("  - {}", path));
        }
        output_lines.push(String::new());
    }

    let output_text = output_lines.join("\n");

    if let Some(output_path) = output {
        let file = File::create(output_path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(output_text.as_bytes())?;
        writer.flush()?;
        info!("Duplicates exported to: {}", output_path.display());
    } else {
        println!("{}", output_text);
    }

    Ok(())
}

pub async fn show_stats(pool: &SqlitePool) -> Result<()> {
    let total_files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hashes")
        .fetch_one(pool)
        .await?;

    let total_size: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size), 0) FROM hashes")
        .fetch_one(pool)
        .await?;

    let duplicate_files: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(count - 1), 0) FROM (
            SELECT COUNT(*) as count
            FROM hashes
            GROUP BY hash
            HAVING count > 1
        )
        "#,
    )
    .fetch_one(pool)
    .await?;

    let wasted_space: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(size * (count - 1)), 0) FROM (
            SELECT size, COUNT(*) as count
            FROM hashes
            GROUP BY hash
            HAVING count > 1
        )
        "#,
    )
    .fetch_one(pool)
    .await?;

    println!("=== DATABASE STATISTICS ===");
    println!("Total files: {}", total_files);
    println!("Total size: {}", Duplicates::format_size(total_size));
    println!("Duplicate files: {}", duplicate_files);
    println!("Wasted space: {}", Duplicates::format_size(wasted_space));

    if total_files > 0 {
        let duplicate_percentage = (duplicate_files as f64 / total_files as f64) * 100.0;
        println!("Duplicate percentage: {:.2}%", duplicate_percentage);
    }

    Ok(())
}

pub async fn clear_database(pool: &SqlitePool) -> Result<()> {
    let rows_deleted = sqlx::query("DELETE FROM hashes")
        .execute(pool)
        .await?
        .rows_affected();

    info!("Cleared {} entries from database", rows_deleted);
    Ok(())
}
