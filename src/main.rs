use clap::Parser;
use memmap2::Mmap;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqlitePool},
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::{self, EnvFilter};
use walkdir::WalkDir;

mod cli;
mod models;
use cli::{Cli, Commands};
use models::Duplicates;

#[instrument(skip(db_path))]
async fn init_database(db_path: &Path) -> Result<SqlitePool, Box<dyn std::error::Error>> {
    let db_url = format!("sqlite:{}", db_path.display());
    info!("Connecting to database: {}", db_url);

    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePool::connect_with(options).await?;

    // Performance optimizations
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
        CREATE TABLE IF NOT EXISTS file_hashes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_path TEXT NOT NULL UNIQUE,
            hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            scanned_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_hash ON file_hashes(hash)")
        .execute(&pool)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_size ON file_hashes(size)")
        .execute(&pool)
        .await?;

    info!("Database initialized successfully");
    Ok(pool)
}

#[instrument(skip(path))]
fn hash_file(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;

    // Use memory-mapped I/O for files larger than 1MB
    let hash = if metadata.len() > 1_000_000 {
        let mmap = unsafe { Mmap::map(&file)? };
        let mut hasher = Sha256::new();
        hasher.update(&mmap);
        hasher.finalize()
    } else {
        // Buffered reading for smaller files
        let mut hasher = Sha256::new();
        let mut buffer = [0; 65536];
        let mut file = file;

        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        hasher.finalize()
    };

    Ok(format!("{:x}", hash))
}

struct FileEntry {
    path: String,
    hash: String,
    size: i64,
}

async fn get_existing_hashes(
    pool: &SqlitePool,
    paths: &[String],
) -> Result<HashMap<String, (String, i64)>, Box<dyn std::error::Error>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }

    const BATCH_SIZE: usize = 999; // SQLite parameter limit
    let mut result = HashMap::new();

    for chunk in paths.chunks(BATCH_SIZE) {
        let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT file_path, hash, size FROM file_hashes WHERE file_path IN ({})",
            placeholders
        );

        let mut q = sqlx::query(&query);
        for path in chunk {
            q = q.bind(path);
        }

        let rows = q.fetch_all(pool).await?;

        for row in rows {
            let path: String = row.get(0);
            let hash: String = row.get(1);
            let size: i64 = row.get(2);
            result.insert(path, (hash, size));
        }
    }

    Ok(result)
}

async fn batch_insert(
    pool: &SqlitePool,
    records: Vec<FileEntry>,
) -> Result<usize, Box<dyn std::error::Error>> {
    if records.is_empty() {
        return Ok(0);
    }

    const BATCH_SIZE: usize = 500;
    let mut inserted = 0;

    for chunk in records.chunks(BATCH_SIZE) {
        let mut tx = pool.begin().await?;

        for entry in chunk {
            sqlx::query(
                "INSERT OR REPLACE INTO file_hashes (file_path, hash, size) VALUES (?, ?, ?)",
            )
            .bind(&entry.path)
            .bind(&entry.hash)
            .bind(entry.size)
            .execute(&mut *tx)
            .await?;

            inserted += 1;
        }

        tx.commit().await?;

        if inserted % 1000 == 0 {
            info!("Inserted {} files into database...", inserted);
        }
    }

    Ok(inserted)
}

#[instrument(skip(pool))]
async fn scan_directory(
    pool: &SqlitePool,
    directory: &Path,
    follow_links: bool,
    rehash: bool,
) -> Result<(usize, usize, usize, usize), Box<dyn std::error::Error>> {
    info!("Scanning directory: {}", directory.display());

    info!("Collecting file list...");
    let mut size_map: HashMap<i64, Vec<PathBuf>> = HashMap::new();
    let mut all_paths = Vec::new();

    for entry in WalkDir::new(directory)
        .follow_links(follow_links)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let size = metadata.len() as i64;
        if size == 0 {
            continue;
        }

        let absolute_path = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let path_str = absolute_path.to_string_lossy().to_string();
        all_paths.push((path_str.clone(), absolute_path.clone(), size));
        size_map.entry(size).or_default().push(absolute_path);
    }

    info!("Found {} files to process", all_paths.len());

    // Check which files already exist in database
    let existing_hashes = if !rehash {
        let paths: Vec<String> = all_paths.iter().map(|(p, _, _)| p.clone()).collect();
        get_existing_hashes(pool, &paths).await?
    } else {
        HashMap::new()
    };

    let total_files = all_paths.len();

    // Determine which files need hashing
    let files_to_hash: Vec<_> = all_paths
        .into_iter()
        .filter_map(|(path_str, absolute_path, size)| {
            if !rehash {
                if let Some((_, existing_size)) = existing_hashes.get(&path_str) {
                    if *existing_size == size {
                        return None; // Already cached
                    }
                }
            }
            Some((path_str, absolute_path, size))
        })
        .collect();

    let cached_count = total_files - files_to_hash.len();
    info!(
        "Files to hash: {}, cached: {}",
        files_to_hash.len(),
        cached_count
    );

    // Parallel hashing
    info!("Hashing files in parallel...");
    let file_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));

    let results: Vec<FileEntry> = files_to_hash
        .par_iter()
        .filter_map(
            |(path_str, absolute_path, size)| match hash_file(absolute_path) {
                Ok(hash) => {
                    let count = file_count.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 100 == 0 {
                        debug!("Hashed {} files...", count);
                    }
                    Some(FileEntry {
                        path: path_str.clone(),
                        hash,
                        size: *size,
                    })
                }
                Err(e) => {
                    error_count.fetch_add(1, Ordering::Relaxed);
                    warn!("Failed to hash {}: {}", absolute_path.display(), e);
                    None
                }
            },
        )
        .collect();

    let hashed_count = file_count.load(Ordering::Relaxed);
    let errors = error_count.load(Ordering::Relaxed);

    info!(
        "Hashing complete: {} files hashed, {} errors",
        hashed_count, errors
    );

    // Step 5: Batch insert into database
    info!("Inserting into database...");
    let inserted = batch_insert(pool, results).await?;
    info!("Inserted {} files", inserted);

    Ok((hashed_count, 0, errors, cached_count))
}

#[instrument(skip(pool))]
async fn scan_multiple_directories(
    pool: &SqlitePool,
    directories: &[PathBuf],
    follow_links: bool,
    rehash: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting scan of {} directories", directories.len());
    if !rehash {
        info!("Skipping files already in database (use --rehash to force rehashing)");
    }

    let mut total_files = 0;
    let mut total_skipped = 0;
    let mut total_errors = 0;
    let mut total_cached = 0;

    for (idx, dir) in directories.iter().enumerate() {
        info!(
            "Scanning directory {}/{}: {}",
            idx + 1,
            directories.len(),
            dir.display()
        );

        let (files, skipped, errors, cached) =
            scan_directory(pool, dir, follow_links, rehash).await?;

        total_files += files;
        total_skipped += skipped;
        total_errors += errors;
        total_cached += cached;

        info!(
            "Directory {} complete: {} files hashed, {} cached, {} skipped, {} errors",
            dir.display(),
            files,
            cached,
            skipped,
            errors
        );
    }

    info!(
        "All scans complete: {} files hashed, {} cached, {} skipped, {} errors",
        total_files, total_cached, total_skipped, total_errors
    );

    Ok(())
}

#[instrument(skip(pool))]
async fn export_duplicates(
    pool: &SqlitePool,
    output: Option<&Path>,
    min_size: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Finding duplicates...");

    let query = if min_size > 0 {
        sqlx::query(
            r#"
            SELECT hash, COUNT(*) as count, GROUP_CONCAT(file_path, '|') as paths, size
            FROM file_hashes
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
            SELECT hash, COUNT(*) as count, GROUP_CONCAT(file_path, '|') as paths, size
            FROM file_hashes
            GROUP BY hash
            HAVING count > 1
            ORDER BY size DESC
            "#,
        )
    };

    let rows = query.fetch_all(pool).await?;

    if rows.is_empty() {
        info!("No duplicates found!");
        return Ok(());
    }

    let mut duplicate_groups = Vec::new();

    for row in rows {
        let hash: String = row.get("hash");
        let paths: String = row.get("paths");
        let size: i64 = row.get("size");

        duplicate_groups.push(Duplicates {
            hash,
            size,
            files: paths.split('|').map(String::from).collect(),
        });
    }

    let total_duplicate_count: usize = duplicate_groups.iter().map(|g| g.files.len() - 1).sum();
    let wasted_space: i64 = duplicate_groups.iter().map(|g| g.wasted_space()).sum();

    let mut output_lines = Vec::new();

    output_lines.push(format!("=== DUPLICATE FILES REPORT ==="));
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

async fn show_stats(pool: &SqlitePool) -> Result<(), Box<dyn std::error::Error>> {
    let total_files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_hashes")
        .fetch_one(pool)
        .await?;

    let total_size: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size), 0) FROM file_hashes")
        .fetch_one(pool)
        .await?;

    let duplicate_files: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(count - 1), 0) FROM (
            SELECT COUNT(*) as count
            FROM file_hashes
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
            FROM file_hashes
            GROUP BY hash
            HAVING count > 1
        )
        "#,
    )
    .fetch_one(pool)
    .await?;

    info!("=== DATABASE STATISTICS ===");
    info!("Total files: {}", total_files);
    info!("Total size: {}", Duplicates::format_size(total_size));
    info!("Duplicate files: {}", duplicate_files);
    info!("Wasted space: {}", Duplicates::format_size(wasted_space));

    if total_files > 0 {
        let duplicate_percentage = (duplicate_files as f64 / total_files as f64) * 100.0;
        info!("Duplicate percentage: {:.2}%", duplicate_percentage);
    }

    Ok(())
}

async fn clear_database(pool: &SqlitePool) -> Result<(), Box<dyn std::error::Error>> {
    let rows_deleted = sqlx::query("DELETE FROM file_hashes")
        .execute(pool)
        .await?
        .rows_affected();

    info!("Cleared {} entries from database", rows_deleted);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command {
        Commands::Scan {
            paths,
            db,
            follow_links,
            rehash,
            output,
            min_size,
        } => {
            let mut invalid_paths = Vec::new();
            for path in &paths {
                if !path.exists() {
                    invalid_paths.push(path.display().to_string());
                } else if !path.is_dir() {
                    error!("{} is not a directory", path.display());
                    invalid_paths.push(path.display().to_string());
                }
            }

            if !invalid_paths.is_empty() {
                error!("Invalid paths provided:");
                for path in invalid_paths {
                    error!("  - {}", path);
                }
                return Err("One or more paths do not exist or are not directories".into());
            }

            let pool = init_database(&db).await?;
            scan_multiple_directories(&pool, &paths, follow_links, rehash).await?;
            export_duplicates(&pool, output.as_deref(), min_size).await?;
            pool.close().await;
        }
        Commands::Clear { db } => {
            if !db.exists() {
                error!("Database does not exist: {}", db.display());
                return Err("Database not found.".into());
            }

            let pool = init_database(&db).await?;
            clear_database(&pool).await?;
            pool.close().await;
        }
        Commands::Stats { db } => {
            if !db.exists() {
                error!("Database does not exist: {}", db.display());
                return Err("Database not found. Run 'init' or 'scan' first.".into());
            }

            let pool = init_database(&db).await?;
            show_stats(&pool).await?;
            pool.close().await;
        }
    }

    Ok(())
}
