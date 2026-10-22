use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    error::{DedupError, Result},
    models::{Duplicates, FileMetadata, HashedFile},
    note,
    report::{Format, Report},
};
use crossbeam_channel::{Receiver, Sender};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use sqlx::{
    AssertSqlSafe, Row, SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

pub async fn init_database(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    let db_url = format!("sqlite:{}", db_path.display());

    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .pragma("cache_size", "-64000")
        .pragma("temp_store", "MEMORY");

    let pool = SqlitePool::connect_with(options).await?;

    MIGRATOR.run(&pool).await?;

    Ok(pool)
}

/// SQL expression identifying the filesystem object a row refers to: rows that
/// match are one object reached by several paths. Where the identity is unknown
/// the path stands in, so those rows count individually rather than collapsing.
const IDENTITY_EXPR: &str = "CASE WHEN ino = 0 \
     THEN 'p:' || CAST(path AS BLOB) ELSE 'i:' || dev || ':' || ino END";

// `format!` injects only `IDENTITY_EXPR` and a filter chosen from a literal;
// paths and sizes travel as bind parameters. `AssertSqlSafe` is the documented
// way to pass a SQL string that is not a compile-time literal.

/// One row per hash holding more than one distinct object, with the number of
/// objects behind it. The group listing and the totals both read from this, so
/// the two cannot disagree about what a copy is.
fn copy_counts_sql(min_size: i64) -> String {
    let filter = if min_size > 0 { "WHERE size >= ?" } else { "" };
    format!(
        "SELECT hash, size, COUNT(*) AS paths, COUNT(DISTINCT {IDENTITY_EXPR}) AS copies \
         FROM hashes {filter} GROUP BY hash HAVING copies > 1"
    )
}

fn duplicate_groups_query(min_size: i64) -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "SELECT hash, size FROM ({}) ORDER BY size DESC",
        copy_counts_sql(min_size)
    ))
}

/// Four scalars in one pass. `stats` does not need every path, and building the
/// report to sum them costs a query per group plus the memory to hold it all.
///
/// No `HAVING`: the first two are also the report's figures and a single-object
/// group contributes zero to both, while the last two cover the whole table and
/// need the groups a link to a file with no duplicate never gets to join.
///
/// Every column is cast back to INTEGER. SQLite promotes an overflowing product
/// to REAL and `Row::get::<i64>` panics on a REAL rather than erroring. The cast
/// saturates at `i64::MAX`, so the figure reads as a floor instead of failing the
/// run; the other three are counts and cannot overflow.
fn duplicate_totals_query() -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "SELECT CAST(COALESCE(SUM(copies - 1), 0) AS INTEGER), \
                CAST(COALESCE(SUM(CASE WHEN copies > 1 \
                                     THEN size * (copies - 1) ELSE 0 END), 0) AS INTEGER), \
                CAST(COALESCE(SUM(paths - copies), 0) AS INTEGER), \
                CAST(COALESCE(SUM(copies), 0) AS INTEGER) \
         FROM (SELECT size, COUNT(*) AS paths, COUNT(DISTINCT {IDENTITY_EXPR}) AS copies \
               FROM hashes GROUP BY hash)"
    ))
}

/// Every path in a group, paired with its object. Ordered so each object's
/// paths are adjacent, which is what `Duplicates::from_paths` relies on.
fn group_paths_query() -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "SELECT path, {IDENTITY_EXPR} AS identity FROM hashes WHERE hash = ? \
         ORDER BY identity, path"
    ))
}

/// One page of entries within a root's range, resuming after `cursor`.
///
/// Paginated by `path`, not `id`. `path` is UNIQUE, so it is a total order and
/// the cursor is well defined; deleting a row cannot shift it. Ordering by `id`
/// instead would force the rowid index and turn the range below into a filter
/// over the whole table, which is what this exists to avoid.
fn prune_page_query() -> AssertSqlSafe<String> {
    AssertSqlSafe(
        "SELECT id, path, dev FROM hashes \
         WHERE path >= ? AND path < ? AND path > ? ORDER BY path LIMIT ?"
            .to_string(),
    )
}

/// A path as the bytes the `path` column stores and compares.
///
/// Native per platform, so nothing is lost either way: raw bytes on Unix,
/// UTF-16LE on Windows, where the filesystem is UTF-16. The `path` column is
/// declared TEXT, which does not matter — TEXT affinity leaves blobs alone.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        PathBuf::from(std::ffi::OsString::from_wide(&units))
    }
    #[cfg(not(any(unix, windows)))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The bytes covering a root and everything under it.
///
/// Paths are stored as bytes, so SQLite compares them with `memcmp` and a prefix
/// match is a range: from the root itself up to the byte after the separator.
/// That makes `/a/b` cover `/a/b` and `/a/b/...` but not `/a/bc`, and it relies
/// on nothing but ordering, so no collation or wildcard escaping is involved.
///
/// `root` is kept alongside the range because the delete is gated on a scope
/// check over these same bytes. Deriving it from the range by popping a byte off
/// the end only worked while the separator was one byte long.
struct RootRange {
    lower: Vec<u8>,
    upper: Vec<u8>,
    root: Vec<u8>,
    sep: Vec<u8>,
}

impl RootRange {
    fn of(root: &Path) -> Self {
        // The separator as `path_bytes` writes it: one byte on Unix, two on
        // Windows where paths are UTF-16LE. Measured by pushing an empty
        // component, which adds exactly one, so it cannot drift from
        // `path_bytes`. Hard-coding `/` built a range matching no Windows path,
        // which made `--prune` a silent no-op there.
        let head = path_bytes(Path::new("a"));
        let mut probe = PathBuf::from("a");
        probe.push("");
        let sep = path_bytes(&probe)[head.len()..].to_vec();

        Self::new(path_bytes(root), sep)
    }

    /// Takes the separator separately so the Windows encoding can be exercised
    /// on any platform.
    fn new(mut base: Vec<u8>, sep: Vec<u8>) -> Self {
        while base.len() >= sep.len() && base.ends_with(&sep) {
            base.truncate(base.len() - sep.len());
        }

        let mut lower = base.clone();
        lower.extend_from_slice(&sep);

        // One past the separator's leading byte. Every path continuing past the
        // root carries that byte there; anything else is a sibling.
        let mut upper = base.clone();
        upper.push(sep[0].saturating_add(1));

        RootRange {
            lower,
            upper,
            root: base,
            sep,
        }
    }
}

/// Whether stored `path` lies under `root`, over the same bytes the range
/// compared. The delete is gated on this, so it has to agree with the range
/// exactly rather than approximate it.
fn in_scope(path: &[u8], root: &RootRange) -> bool {
    path == root.root
        || (path.len() > root.root.len()
            && path.starts_with(&root.root)
            && path[root.root.len()..].starts_with(&root.sep))
}

fn delete_id_query() -> AssertSqlSafe<String> {
    AssertSqlSafe("DELETE FROM hashes WHERE id = ?".to_string())
}

const PRUNE_PAGE: i64 = 1000;

/// Deletes entries under `roots` whose file is gone.
///
/// Keyed on whether the path still exists, never on whether this scan saw it: a
/// scan cannot reach every file under a root, and "not seen" would delete
/// entries for files that are sitting there untouched.
pub async fn prune_missing(
    pool: &SqlitePool,
    roots: &[PathBuf],
    shutdown: &AtomicBool,
) -> Result<usize> {
    let pb = spinner();
    pb.set_message("Checking for removed files...");

    let mut removed = 0usize;
    let mut checked = 0usize;

    // One pass per root, each a contiguous range of `path`, so a prune of one
    // small directory reads only that directory's rows.
    for root in roots {
        let scope = RootRange::of(root);
        let mut cursor: Vec<u8> = Vec::new();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                pb.finish_and_clear();
                return Ok(removed);
            }

            let rows = sqlx::query(prune_page_query())
                .bind(&scope.lower)
                .bind(&scope.upper)
                .bind(&cursor)
                .bind(PRUNE_PAGE)
                .fetch_all(pool)
                .await?;

            if rows.is_empty() {
                break;
            }

            cursor = rows
                .last()
                .map(|r| r.get::<Vec<u8>, _>("path"))
                .unwrap_or(cursor);

            let candidates: Vec<(i64, PathBuf, i64)> = rows
                .into_iter()
                .filter_map(|row| {
                    let id: i64 = row.get("id");
                    let raw: Vec<u8> = row.get("path");
                    let dev: i64 = row.get("dev");
                    in_scope(&raw, &scope).then(|| (id, path_from_bytes(&raw), dev))
                })
                .collect();

            let stale: Vec<i64> = candidates
                .par_iter()
                .filter(|(_, path, dev)| is_gone(path, *dev))
                .map(|(id, _, _)| *id)
                .collect();

            if !stale.is_empty() {
                // One transaction per page, so an interrupted prune leaves whole
                // pages deleted and none half-applied.
                let mut tx = pool.begin().await?;
                for id in &stale {
                    sqlx::query(delete_id_query())
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                removed += stale.len();
            }

            // The bar is a spinner with no length, so `inc` would advance
            // nothing visible; the running count goes in the message instead,
            // which is what tells a long prune from a stalled one.
            checked += candidates.len();
            pb.set_message(format!("Checked {checked} entries, removed {removed}"));
        }
    }

    pb.finish_and_clear();
    Ok(removed)
}

/// Whether a stored path is a file that is genuinely gone.
///
/// Only `NotFound` counts. A permission problem or any other error keeps the
/// entry: a stale row costs a few bytes, a dropped live one corrupts the report.
///
/// With no recorded device there is no way to tell a deletion from an unmounted
/// volume, so the entry is kept. Entries written by sdoppia 0.8 and earlier on
/// Windows land here, because an unchanged file is not rewritten by a rescan.
fn is_gone(path: &Path, recorded_dev: i64) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => false,
        Err(_) if recorded_dev == 0 => false,
        // Device numbers are assigned per boot on Unix, so a recorded one cannot
        // say whether its volume is still mounted; comparing it with the
        // surviving ancestor kept every entry recorded before a renumber forever.
        #[cfg(not(windows))]
        Err(_) => true,
        // A Windows volume serial survives a remount. A surviving ancestor that
        // reports another one means the volume is not mounted, so keep the entry.
        #[cfg(windows)]
        Err(_) => path
            .ancestors()
            .skip(1)
            .find(|a| std::fs::metadata(a).is_ok())
            .is_some_and(|a| crate::fs::windows_identity(a).0 == recorded_dev),
    }
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
                    note(
                        &db_pb,
                        &format!(
                            "Batch insert of {} records failed after {} were saved: {e}",
                            batch.len(),
                            total_inserted
                        ),
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
            note(&db_pb, "Database writer received shutdown signal");
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
        .bind(path_bytes(&file.absolute_path))
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
    filter_pb: ProgressBar,
    hash_pb: ProgressBar,
    shutdown: Arc<AtomicBool>,
) -> Result<usize> {
    let mut sent_count = 0;
    let mut cached_count = 0;
    let mut query_errors = 0;

    loop {
        // Check for shutdown signal
        if shutdown.load(Ordering::Relaxed) {
            note(&filter_pb, "Filter received shutdown signal, exiting");
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
                .bind(path_bytes(&file.absolute_path))
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
                    note(
                        &filter_pb,
                        &format!("Database query error for {}: {e}", file.path.display()),
                    );
                }
            }
        }

        // Send without blocking the async runtime: if the hash workers are
        // behind, yield briefly and retry until space frees up or shutdown.
        let mut file = file;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                filter_pb.finish_with_message(format!(
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

        filter_pb.set_message(format!(
            "{} already hashed files, {} to hash",
            cached_count, sent_count
        ));
    }

    filter_pb.finish_with_message(format!(
        "Cached: {}, Need hashing: {}",
        cached_count, sent_count
    ));

    if query_errors > 0 {
        return Err(DedupError::QueryFailed(query_errors));
    }

    Ok(sent_count)
}

/// A spinner for a step that runs on its own, outside the scan's `MultiProgress`.
///
/// Such a bar does not reserve a row: it draws wherever the cursor happens to
/// be and leaves the cursor at the end of its own line. Anything printed next
/// therefore lands inside that line. `finish_and_clear` erases it instead of
/// leaving a finished line behind, which is what callers want before writing to
/// stdout.
fn spinner() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(30));
    pb
}

/// Every duplicate group, each already split into copies and links.
///
/// `Ok(None)` means the run was interrupted part-way, which callers must not
/// mistake for a database without duplicates. `stats` does not come through
/// here: it sums `copy_counts_sql` directly rather than build the groups.
async fn collect_duplicate_groups(
    pool: &SqlitePool,
    min_size: i64,
    pb: &ProgressBar,
    shutdown: &AtomicBool,
) -> Result<Option<Vec<Duplicates>>> {
    pb.set_message("Querying database for duplicates...");

    // Checked before the group query, not just inside the loop: on a large
    // database that single query is seconds of uninterruptible work, so waiting
    // for it to finish before noticing would make Ctrl+C feel ignored.
    if shutdown.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // Count distinct filesystem objects per hash, not rows: several paths may
    // point at one object through a hard or symbolic link, and those are not
    // reclaimable copies.
    let mut hash_query = sqlx::query(duplicate_groups_query(min_size));
    if min_size > 0 {
        hash_query = hash_query.bind(min_size);
    }

    let hash_rows = hash_query.fetch_all(pool).await?;

    pb.set_message(format!(
        "Processing {} duplicate groups...",
        hash_rows.len()
    ));
    pb.set_length(hash_rows.len() as u64);

    let mut duplicate_groups = Vec::new();

    for row in hash_rows {
        // The report walks one query per duplicate group, which on a large
        // database is long enough that a user pressing Ctrl+C expects it to
        // take effect. Without this the signal is ignored until the whole
        // report is built.
        if shutdown.load(Ordering::Relaxed) {
            return Ok(None);
        }

        let hash: String = row.get("hash");
        let size: i64 = row.get("size");

        // The identity is a comparison key and, where the platform reports none, it
        // carries raw path bytes. Kept as bytes: decoding it would either panic
        // on anything that is not valid UTF-8, or lossily fold two paths into
        // one key and hide a real copy.
        let paths: Vec<(String, Vec<u8>)> = sqlx::query(group_paths_query())
            .bind(&hash)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| {
                // The report needs text, so the stored bytes go back through a
                // real path first: raw bytes on Unix, UTF-16LE on Windows.
                // Decoding them as UTF-8 read Windows paths with NUL between
                // every character and could not even hash-group those bytes.
                let path = path_from_bytes(&row.get::<Vec<u8>, _>("path"))
                    .to_string_lossy()
                    .into_owned();
                (path, row.get::<Vec<u8>, _>("identity"))
            })
            .collect();

        duplicate_groups.push(Duplicates::from_paths(hash, size, &paths));
        pb.inc(1);
    }

    Ok(Some(duplicate_groups))
}

pub async fn export_duplicates(
    pool: &SqlitePool,
    output_file: Option<&Path>,
    format: Format,
    min_size: i64,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let pb = spinner();

    let Some(duplicate_groups) = collect_duplicate_groups(pool, min_size, &pb, &shutdown).await?
    else {
        pb.finish_and_clear();
        return Ok(());
    };

    let report = Report::new(&duplicate_groups);

    // Cleared rather than finished: the report follows immediately, and a
    // finished line would be where its first line lands.
    pb.finish_and_clear();

    // Written either way, so an explicitly requested output file is produced even
    // when there is nothing to report.
    match output_file {
        Some(output_path) => {
            let mut writer = BufWriter::new(File::create(output_path)?);
            writer.write_all(report.render(format)?.as_bytes())?;
            writer.flush()?;
            note(
                &pb,
                &format!("Duplicates exported to: {}", output_path.display()),
            );
        }
        None => println!("{}", report.render(format)?),
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

    // Counted in one pass rather than by building the report, and through the
    // same copy counts the report uses, so the two agree on what a copy is.
    let row = sqlx::query(duplicate_totals_query())
        .fetch_one(pool)
        .await?;

    // The first two are the report's figures. `objects` is not one of them: it
    // counts objects across the whole table, where a report totals only its
    // groups, so it has no place in `Totals`.
    let redundant_copies = row.get::<i64, _>(0) as usize;
    let wasted_space = row.get::<i64, _>(1);
    let objects: i64 = row.get(3);

    println!("=== DATABASE STATISTICS ===");
    println!("Total files: {}", total_files);
    println!("Total size: {}", Duplicates::format_size(total_size));
    println!("Duplicate files: {}", redundant_copies);
    println!("Wasted space: {}", Duplicates::format_size(wasted_space));

    // Divided by objects rather than paths: a directory full of links is the
    // same handful of files under many names, not a clean one.
    if objects > 0 {
        let duplicate_percentage = (redundant_copies as f64 / objects as f64) * 100.0;
        println!("Duplicate percentage: {:.2}%", duplicate_percentage);
    }

    Ok(())
}

pub async fn clear_database(pool: &SqlitePool) -> Result<()> {
    let rows_deleted = sqlx::query("DELETE FROM hashes")
        .execute(pool)
        .await?
        .rows_affected();

    // No bars are drawing for `clear`, so stderr is free.
    eprintln!("Cleared {rows_deleted} entries from database");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn path_bytes_keeps_bytes_that_are_not_valid_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // The conversion that used to run on every write. Two names differing
        // only in these bytes must not both come back as the same string.
        let path = Path::new("/files").join(OsStr::from_bytes(b"a\xff.txt"));
        let bytes = path_bytes(&path);
        assert!(
            bytes.windows(2).any(|w| w == [0xff, b'.']),
            "the 0xFF byte must survive, got {bytes:?}"
        );
        assert_ne!(path.to_string_lossy().as_bytes(), bytes.as_slice());
    }

    /// Asserts both halves of the scoping decision. The range is what the page query
    /// reads and `in_scope` is what gates the delete; a row in one and not the
    /// other is pruned wrongly, so they are checked against each other.
    fn assert_scope(scope: &RootRange, cases: &[(&[u8], bool, bool)]) {
        for (path, in_range, scoped) in cases {
            let path: &[u8] = path;
            let shown = String::from_utf8_lossy(path).into_owned();

            assert_eq!(
                path >= scope.lower.as_slice() && path < scope.upper.as_slice(),
                *in_range,
                "byte range [{:02x?}, {:02x?}) disagreed for {shown:?}",
                scope.lower,
                scope.upper
            );
            assert_eq!(
                in_scope(path, scope),
                *scoped,
                "scope check disagreed for {shown:?}"
            );
        }
    }

    /// How `path_bytes` writes a path and a separator on Windows.
    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// `path_bytes` writes the Windows separator as `5C 00`.
    const WIN_SEP: [u8; 2] = [0x5C, 0x00];

    /// The measured separator has to be what actually joins two components.
    /// Diffing two encodings goes wrong quietly: the difference between `a/b`
    /// and `a` is `/b`, a "separator" that scopes nothing.
    #[test]
    fn separator_is_what_joins_two_components() {
        let scope = RootRange::of(Path::new("/"));

        let left = PathBuf::from("a");
        let mut joined = left.clone();
        joined.push("b");
        let encoded = path_bytes(&joined);

        // The separator sits between the components, so shaving off one byte
        // per side only works while components are one byte long. Measure the
        // widths the way `RootRange::of` does — one byte on Unix, two on
        // Windows' UTF-16LE.
        let head = path_bytes(&left).len();
        let tail = path_bytes(Path::new("b")).len();
        assert_eq!(
            &encoded[head..encoded.len() - tail],
            scope.sep,
            "the measured separator is not what joins two components"
        );
        assert_eq!(scope.sep[0], std::path::MAIN_SEPARATOR as u8);
    }

    /// And the range built from it has to select real paths under a real root,
    /// through the same code `prune_missing` runs.
    #[test]
    fn the_measured_separator_scopes_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("deep")).unwrap();
        let file = dir.path().join("deep").join("f.txt");
        fs::write(&file, b"x").unwrap();

        let scope = RootRange::of(dir.path());
        let stored = path_bytes(&file);
        assert!(
            in_scope(&stored, &scope),
            "a file under the root is in scope"
        );
        assert!(
            stored >= scope.lower && stored < scope.upper,
            "a file under the root falls inside the byte range"
        );

        // A sibling sharing the prefix, built in the stored encoding rather than
        // on disk: its component starts with a byte other than the separator.
        let mut sibling = path_bytes(dir.path());
        sibling.push(b'x');
        sibling.push(std::path::MAIN_SEPARATOR as u8);
        sibling.extend_from_slice(b"f.txt");

        assert!(
            !in_scope(&sibling, &scope),
            "a sibling sharing the prefix is not in scope"
        );
    }

    #[test]
    fn unix_separator_scopes_the_root_and_its_children() {
        let scope = RootRange::new(b"/data/b".to_vec(), b"/".to_vec());
        assert_scope(
            &scope,
            &[
                (b"/data/b/f.txt", true, true),
                (b"/data/b/deep/f.txt", true, true),
                (b"/data/bc/f.txt", false, false),
                (b"/data/b.txt", false, false),
                (b"/data/c.txt", false, false),
                (b"/data", false, false),
                (b"/data/", false, false),
            ],
        );
    }

    /// The two-byte separator is the case that made `--prune` a silent no-op on
    /// Windows: a range built around a one-byte `/` selected no path there, so
    /// nothing was read and nothing was deleted while still reporting that it
    /// had run. Reproduced everywhere so it does not need a Windows runner.
    #[test]
    fn windows_separator_scopes_the_root_and_its_children() {
        let scope = RootRange::new(utf16("C:\\data\\b"), WIN_SEP.to_vec());
        assert_scope(
            &scope,
            &[
                (&utf16("C:\\data\\b\\f.txt"), true, true),
                (&utf16("C:\\data\\b\\deep\\f.txt"), true, true),
                // A character above U+007F is not valid UTF-8 on its own.
                (&utf16("C:\\data\\b\\caf\u{e9}.txt"), true, true),
                (&utf16("C:\\data\\bc\\f.txt"), false, false),
                (&utf16("C:\\data\\b.txt"), false, false),
                (&utf16("C:\\data"), false, false),
                (&utf16("D:\\data\\b\\f.txt"), false, false),
            ],
        );
    }

    /// A trailing separator has to be stripped whole. Popping a single byte left
    /// the `5C` half behind, which no stored path begins with, so every
    /// descendant read as out of scope.
    #[test]
    fn trailing_separators_are_stripped_whole() {
        for (given, sep, expected_root, descendant, sibling) in [
            (
                b"/data/b/".to_vec(),
                b"/".to_vec(),
                b"/data/b".to_vec(),
                b"/data/b/deep/f.txt".to_vec(),
                b"/data/bc/f.txt".to_vec(),
            ),
            (
                utf16("C:\\data\\b\\"),
                WIN_SEP.to_vec(),
                utf16("C:\\data\\b"),
                utf16("C:\\data\\b\\f.txt"),
                utf16("C:\\data\\bc\\f.txt"),
            ),
        ] {
            let scope = RootRange::new(given, sep);
            assert_eq!(scope.root, expected_root, "root kept a trailing separator");
            assert!(in_scope(&descendant, &scope), "{descendant:?}");
            assert!(!in_scope(&sibling, &scope), "{sibling:?}");
        }
    }

    /// A file that exists is never gone, whatever device the row recorded.
    #[test]
    fn an_existing_file_is_not_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("here.txt");
        fs::write(&path, b"x").unwrap();

        assert!(!is_gone(&path, 0));
        assert!(!is_gone(&path, 1));
    }

    /// A missing path with no recorded device cannot be told from one on an
    /// unmounted volume, so it stays.
    #[test]
    fn a_missing_path_without_an_identity_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-existed.txt");

        assert!(!is_gone(&path, 0));
    }

    /// A deleted file recorded under a device number the surviving ancestor no
    /// longer reports is still gone.
    #[cfg(unix)]
    #[test]
    fn a_renumbered_device_does_not_keep_a_deleted_file() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.txt");
        fs::write(&path, b"x").unwrap();

        let recorded = fs::metadata(&path).unwrap().dev() as i64;
        assert_ne!(recorded, 0, "the platform must report a device here");
        fs::remove_file(&path).unwrap();

        assert!(is_gone(&path, recorded));
        assert!(is_gone(&path, recorded.wrapping_add(1)));
    }

    /// A hash group whose stored sizes disagree pushes `size * (copies - 1)`
    /// past `i64::MAX` while the table total stays inside it. The size is
    /// recorded at scan time and the hash at read time, so a file edited in
    /// between splits them. SQLite promotes the product to REAL, and reading the
    /// row as `i64` panicked on it; the cast saturates instead.
    #[tokio::test]
    async fn totals_survive_a_size_that_overflows_the_product() {
        let dir = tempfile::tempdir().unwrap();
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("overflow.db"))
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        for (i, size) in [9_000_000_000_000_000_000i64, 1, 1].into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO hashes (path, hash, size, mtime, dev, ino) \
                 VALUES (?, ?, ?, 1, 1, ?)",
            )
            .bind(path_bytes(Path::new(&format!("/p{i}"))))
            .bind("same-hash")
            .bind(size)
            .bind(100 + i as i64)
            .execute(&pool)
            .await
            .unwrap();
        }

        let row = sqlx::query(duplicate_totals_query())
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(row.get::<i64, _>(0), 2, "3 objects over one hash");
        assert_eq!(row.get::<i64, _>(2), 0, "no two paths share an object");
        assert_eq!(row.get::<i64, _>(3), 3);
        assert_eq!(
            row.get::<i64, _>(1),
            i64::MAX,
            "an unrepresentable total saturates rather than overflowing"
        );
    }

    /// And the casts must be a no-op for ordinary data.
    #[tokio::test]
    async fn totals_are_exact_for_ordinary_data() {
        let dir = tempfile::tempdir().unwrap();
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("ordinary.db"))
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        // Two objects of 10 bytes, a link to the first, and a solo file.
        for (path, hash, size, ino) in [
            ("/a", "ha", 10i64, 1i64),
            ("/b", "ha", 10, 2),
            ("/a_link", "ha", 10, 1),
            ("/solo", "hb", 7, 3),
        ] {
            sqlx::query(
                "INSERT INTO hashes (path, hash, size, mtime, dev, ino) \
                 VALUES (?, ?, ?, 1, 1, ?)",
            )
            .bind(path_bytes(Path::new(path)))
            .bind(hash)
            .bind(size)
            .bind(ino)
            .execute(&pool)
            .await
            .unwrap();
        }

        let row = sqlx::query(duplicate_totals_query())
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(row.get::<i64, _>(0), 1, "one redundant copy");
        assert_eq!(row.get::<i64, _>(1), 10, "10 bytes reclaimable");
        assert_eq!(row.get::<i64, _>(2), 1, "one linked path");
        assert_eq!(row.get::<i64, _>(3), 3, "three distinct objects");
    }
}
