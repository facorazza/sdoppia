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
use sqlx::{AssertSqlSafe, Row, SqlitePool, sqlite::SqliteConnectOptions};
use tracing::{debug, info, instrument, warn};

use crate::{
    error::{DedupError, Result},
    models::{Duplicates, FileMetadata, HashedFile},
};

#[instrument(skip(db_path))]
pub async fn init_database(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

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
            mtime INTEGER NOT NULL,
            dev INTEGER NOT NULL DEFAULT 0,
            ino INTEGER NOT NULL DEFAULT 0
        )
        "#,
    )
    .execute(&pool)
    .await?;

    // Databases created before hard-link detection have no dev/ino columns.
    // Existing rows keep the default of 0, which reads as "unknown identity"
    // and makes them count as individual copies, exactly as they did before.
    add_missing_columns(
        &pool,
        &[
            (
                "dev",
                "ALTER TABLE hashes ADD COLUMN dev INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "ino",
                "ALTER TABLE hashes ADD COLUMN ino INTEGER NOT NULL DEFAULT 0",
            ),
        ],
    )
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

/// Idempotently add columns that older databases predate.
///
/// `table` is intentionally not a parameter: it is interpolated into the
/// `PRAGMA` below, and hardcoding it keeps that statement a literal.
async fn add_missing_columns(pool: &SqlitePool, columns: &[(&str, &'static str)]) -> Result<()> {
    let existing: Vec<String> = sqlx::query("PRAGMA table_info(hashes)")
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();

    for (name, ddl) in columns {
        if !existing.iter().any(|c| c == name) {
            debug!("Adding column {name} to hashes");
            sqlx::query(*ddl).execute(pool).await?;
        }
    }

    Ok(())
}

/// SQL expression identifying the filesystem object a row refers to.
///
/// Hard links and symbolic links share an inode, so rows with the same
/// expression are one object reached by several paths. When the identity is
/// unknown (`ino = 0`, covering pre-migration rows and platforms without inode
/// data) the path stands in, so those rows keep counting individually instead
/// of collapsing into one.
const IDENTITY_EXPR: &str =
    "CASE WHEN ino = 0 THEN 'p:' || path ELSE 'i:' || dev || ':' || ino END";

// The queries below are assembled with `format!` purely to inject
// `IDENTITY_EXPR` and a size filter chosen from a literal. Keeping that
// expression in one place matters more than avoiding `format!`, because every
// duplicate and wasted-space metric has to agree on what counts as a copy, and
// a missed edit here would silently skew the report. No user input reaches any
// of these strings; scanned paths and sizes travel as bind parameters.
//
// `sqlx::AssertSqlSafe` is the documented way to pass a SQL string that is not
// a compile-time literal. It is sound here for the reason above.

/// Hashes with more than one distinct filesystem object behind them.
fn duplicate_groups_query(min_size: i64) -> AssertSqlSafe<String> {
    let filter = if min_size > 0 { "WHERE size >= ?" } else { "" };
    AssertSqlSafe(format!(
        "SELECT hash, size FROM hashes {filter} \
         GROUP BY hash \
         HAVING COUNT(DISTINCT {IDENTITY_EXPR}) > 1 \
         ORDER BY size DESC"
    ))
}

/// Every path in a duplicate group, paired with the object it refers to and
/// ordered so each object's paths are adjacent and deterministic.
fn group_paths_query() -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "SELECT path, {IDENTITY_EXPR} AS identity FROM hashes WHERE hash = ? \
         ORDER BY identity, path"
    ))
}

/// Copies beyond the first per hash, summed over all duplicate groups.
fn redundant_copies_query() -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "SELECT COALESCE(SUM(copies - 1), 0) FROM (\
            SELECT COUNT(DISTINCT {IDENTITY_EXPR}) AS copies \
            FROM hashes GROUP BY hash HAVING copies > 1)"
    ))
}

/// Reclaimable bytes: every distinct object past the first, links excluded.
fn wasted_space_query() -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "SELECT COALESCE(SUM(size * (copies - 1)), 0) FROM (\
            SELECT size, COUNT(DISTINCT {IDENTITY_EXPR}) AS copies \
            FROM hashes GROUP BY hash HAVING copies > 1)"
    ))
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
            let batch = std::mem::take(&mut buffer);
            match save_hashes(&pool, &batch).await {
                Ok(count) => {
                    total_inserted += count;
                    db_pb.inc(count as u64);
                }
                Err(e) => {
                    // A scan that hashed everything but stored nothing looks
                    // identical to a clean scan, and the report it goes on to
                    // print would be wrong. Fail loudly instead. Batches
                    // committed earlier stay in the database; this one is lost,
                    // and retrying a rejected batch would only fail again.
                    warn!(
                        "Batch insert of {} records failed after {} were saved: {}",
                        batch.len(),
                        total_inserted,
                        e
                    );
                    db_pb.finish_with_message(format!(
                        "Failed after {} saved, {} lost",
                        total_inserted,
                        batch.len()
                    ));
                    return Err(e);
                }
            }
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
        sqlx::query(
            "INSERT OR REPLACE INTO hashes (path, hash, size, mtime, dev, ino) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(file.absolute_path.to_string_lossy())
        .bind(&file.hash)
        .bind(file.size)
        .bind(file.mtime)
        .bind(file.dev)
        .bind(file.ino)
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
    let mut query_errors = 0;

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
            match sqlx::query("SELECT size, mtime FROM hashes WHERE path = ?")
                .bind(file.absolute_path.to_string_lossy())
                .fetch_one(&pool)
                .await
            {
                Ok(row) => {
                    let stored_size: i64 = row.get("size");
                    let stored_mtime: i64 = row.get("mtime");
                    // Both size and mtime must match before the stored hash is
                    // trusted. Checking mtime alone is not enough: the stored
                    // timestamp has one-second granularity, so any rewrite that
                    // preserves the mtime (cp -p, rsync --times, tar extraction,
                    // git checkout, or a second edit within the same second) is
                    // invisible to us and would keep a stale hash forever.
                    if stored_size == file.size && stored_mtime == file.mtime {
                        cached_count += 1;
                        continue;
                    }
                    // File has changed, need to rehash
                }
                Err(sqlx::Error::RowNotFound) => (),
                Err(e) => {
                    // Dropping the file here would silently shrink the scan,
                    // so record it and fail the run once draining completes.
                    query_errors += 1;
                    warn!("Database query error for {}: {}", file.path.display(), e);
                }
            }
        }

        // Send without blocking the async runtime: if the hash workers are
        // behind, yield briefly and retry until space frees up or shutdown.
        let mut file = file;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                scan_pb.finish_with_message(format!(
                    "⚠ Interrupted: Cached: {}, Need hashing: {}",
                    cached_count, sent_count
                ));
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

    if query_errors > 0 {
        return Err(DedupError::QueryFailed(query_errors));
    }

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

    // Count distinct filesystem objects per hash, not rows: several paths may
    // point at one object through a hard or symbolic link, and those are not
    // reclaimable copies.
    let mut hash_query = sqlx::query(duplicate_groups_query(min_size));
    if min_size > 0 {
        hash_query = hash_query.bind(min_size);
    }

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

        let file_rows = sqlx::query(group_paths_query())
            .bind(&hash)
            .fetch_all(pool)
            .await?;

        // Partition the group by filesystem object: the first path seen for an
        // object is a real copy, and every later path for that same object is
        // just a link to it, so it is listed separately instead of being
        // counted as reclaimable space.
        let mut copies: Vec<String> = Vec::new();
        let mut aliases: Vec<String> = Vec::new();
        let mut seen_identities: Vec<String> = Vec::new();

        for row in file_rows {
            let path: String = row.get("path");
            let identity: String = row.get("identity");
            if seen_identities.contains(&identity) {
                aliases.push(path);
            } else {
                seen_identities.push(identity);
                copies.push(path);
            }
        }

        if !copies.is_empty() {
            duplicate_groups.push(Duplicates {
                hash,
                size,
                files: copies,
                aliases,
            });
        }
        pb.inc(1);
    }

    pb.finish_with_message(format!("Found {} duplicate groups", duplicate_groups.len()));

    let total_duplicate_count: usize = duplicate_groups
        .iter()
        .filter(|g| g.is_duplicate())
        .map(|g| g.copies() - 1)
        .sum();
    let total_links: usize = duplicate_groups.iter().map(|g| g.aliases.len()).sum();
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
    if total_links > 0 {
        output_lines.push(format!(
            "Linked paths (share storage, reclaiming none): {}",
            total_links
        ));
    }
    output_lines.push(String::new());

    for (idx, group) in duplicate_groups.iter().enumerate() {
        output_lines.push(format!("--- Group {} ---", idx + 1));
        output_lines.push(format!("Hash: {}", group.hash));
        output_lines.push(format!("Size: {}", Duplicates::format_size(group.size)));
        output_lines.push(format!("Copies: {}", group.copies()));
        output_lines.push(format!(
            "Wasted: {}",
            Duplicates::format_size(group.wasted_space())
        ));
        output_lines.push("Files:".to_string());

        for path in &group.files {
            output_lines.push(format!("  - {}", path));
        }

        if !group.aliases.is_empty() {
            output_lines.push("Links to the above (not extra copies):".to_string());
            for path in &group.aliases {
                output_lines.push(format!("  - {}", path));
            }
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

    // Both duplicate metrics count distinct filesystem objects, so that links
    // to a file already counted never inflate the numbers reported here.
    let duplicate_files: i64 = sqlx::query_scalar(redundant_copies_query())
        .fetch_one(pool)
        .await?;

    let wasted_space: i64 = sqlx::query_scalar(wasted_space_query())
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
