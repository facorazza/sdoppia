use clap::{Parser};
use sqlx::{sqlite::{SqlitePool, SqliteConnectOptions}, Row};
use std::path::{Path, PathBuf};
use tracing::{info, warn, error, debug, instrument};
use tracing_subscriber::{self, EnvFilter};
use sha2::{Sha256, Digest};
use std::fs::File;
use std::io::{Read, BufWriter, Write};
use walkdir::WalkDir;
use std::str::FromStr;

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

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS file_hashes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_path TEXT NOT NULL UNIQUE,
            hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            scanned_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#
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
    debug!("Hashing file: {}", path.display());
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536]; // Increased buffer size for better performance

    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[instrument(skip(pool))]
async fn scan_directory(
    pool: &SqlitePool,
    directory: &Path,
    follow_links: bool,
) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    info!("Scanning directory: {}", directory.display());

    let mut file_count = 0;
    let mut error_count = 0;
    let mut skipped_count = 0;

    for entry in WalkDir::new(directory)
        .follow_links(follow_links)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        // Skip files we can't read metadata for
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                warn!("Cannot read metadata for {}: {}", path.display(), e);
                skipped_count += 1;
                continue;
            }
        };

        // Skip empty files
        let size = metadata.len() as i64;
        if size == 0 {
            debug!("Skipping empty file: {}", path.display());
            skipped_count += 1;
            continue;
        }

        // Convert to absolute path
        let absolute_path = match path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                warn!("Cannot get absolute path for {}: {}", path.display(), e);
                error_count += 1;
                continue;
            }
        };

        let path_str = absolute_path.to_string_lossy().to_string();

        match hash_file(path) {
            Ok(hash) => {
                match sqlx::query(
                    "INSERT OR REPLACE INTO file_hashes (file_path, hash, size) VALUES (?, ?, ?)"
                )
                .bind(&path_str)
                .bind(&hash)
                .bind(size)
                .execute(pool)
                .await
                {
                    Ok(_) => {
                        file_count += 1;
                        if file_count % 100 == 0 {
                            info!("Processed {} files...", file_count);
                        }
                    }
                    Err(e) => {
                        error!("Failed to insert {}: {}", path_str, e);
                        error_count += 1;
                    }
                }
            }
            Err(e) => {
                warn!("Failed to hash {}: {}", path.display(), e);
                error_count += 1;
            }
        }
    }

    Ok((file_count, skipped_count, error_count))
}

#[instrument(skip(pool))]
async fn scan_multiple_directories(
    pool: &SqlitePool,
    directories: &[PathBuf],
    follow_links: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting scan of {} directories", directories.len());

    let mut total_files = 0;
    let mut total_skipped = 0;
    let mut total_errors = 0;

    for (idx, dir) in directories.iter().enumerate() {
        info!("Scanning directory {}/{}: {}", idx + 1, directories.len(), dir.display());

        let (files, skipped, errors) = scan_directory(pool, dir, follow_links).await?;

        total_files += files;
        total_skipped += skipped;
        total_errors += errors;

        info!(
            "Directory {} complete: {} files, {} skipped, {} errors",
            dir.display(),
            files,
            skipped,
            errors
        );
    }

    info!(
        "All scans complete: {} total files processed, {} skipped, {} errors",
        total_files,
        total_skipped,
        total_errors
    );

    Ok(())
}

#[instrument(skip(pool))]
async fn export_duplicates(
    pool: &SqlitePool,
    output: Option<&Path>,
    min_size: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Finding duplicates...");

    let query = if let Some(size) = min_size {
        sqlx::query(
            r#"
            SELECT hash, COUNT(*) as count, GROUP_CONCAT(file_path, '|') as paths, size
            FROM file_hashes
            WHERE size >= ?
            GROUP BY hash
            HAVING count > 1
            ORDER BY size DESC
            "#
        )
        .bind(size)
    } else {
        sqlx::query(
            r#"
            SELECT hash, COUNT(*) as count, GROUP_CONCAT(file_path, '|') as paths, size
            FROM file_hashes
            GROUP BY hash
            HAVING count > 1
            ORDER BY size DESC
            "#
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

    let total_duplicate_count: usize = duplicate_groups.iter()
        .map(|g| g.files.len() - 1)
        .sum();
    let wasted_space: i64 = duplicate_groups.iter()
        .map(|g| g.wasted_space())
        .sum();

    // Build output
    let mut output_lines = Vec::new();

    output_lines.push(format!("=== DUPLICATE FILES REPORT ==="));
    output_lines.push(format!("Generated: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    output_lines.push(format!("Total duplicate files: {}", total_duplicate_count));
    output_lines.push(format!("Wasted space: {}", Duplicates::format_size(wasted_space)));
    output_lines.push(format!("Duplicate groups: {}", duplicate_groups.len()));
    output_lines.push(String::new());

    for (idx, group) in duplicate_groups.iter().enumerate() {
        output_lines.push(format!("--- Group {} ---", idx + 1));
        output_lines.push(format!("Hash: {}", group.hash));
        output_lines.push(format!("Size: {}", Duplicates::format_size(group.size)));
        output_lines.push(format!("Copies: {}", group.files.len()));
        output_lines.push(format!("Wasted: {}", Duplicates::format_size(group.wasted_space())));
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
        "#
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
        "#
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

    // Setup tracing with environment variable support
    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    match cli.command {
        Commands::Init { db } => {
            init_database(&db).await?;
            info!("Database initialized at: {}", db.display());
        }
        Commands::Scan { paths, db, follow_links } => {
            // Validate all paths exist before starting
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
            scan_multiple_directories(&pool, &paths, follow_links).await?;
            pool.close().await;
        }
        Commands::Export { db, output, min_size } => {
            if !db.exists() {
                error!("Database does not exist: {}", db.display());
                return Err("Database not found. Run 'init' or 'scan' first.".into());
            }

            let pool = init_database(&db).await?;
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
