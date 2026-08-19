use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing::{debug, warn};
use tracing_subscriber::{self, EnvFilter};

mod cli;
mod db;
mod error;
mod fs;
mod models;
use cli::{Cli, Commands};
use db::{
    clear_database, database_writer, export_duplicates, filter_files, init_database, show_stats,
};
use error::Result;
use models::{FileMetadata, HashedFile};

// Channel capacity per pipeline stage. Producers block (scanner in a
// blocking thread, filter via try_send retry) when full, so this only
// needs to absorb bursts, not hold the whole workload in memory.
const QUEUE_SIZE: usize = 4096;
const PROGRESS_CHARS: &str = "█▉▊▋▌▍▎▏  ";
const TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_file(true)
        .with_line_number(true)
        .init();

    // Set up Ctrl+C handler
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                warn!("Received Ctrl+C, initiating graceful shutdown...");
                shutdown_clone.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                warn!("Failed to listen for Ctrl+C: {}", e);
            }
        }
    });

    match cli.command {
        Commands::Scan {
            paths,
            db,
            follow_links,
            rehash,
            output,
            min_size,
        } => {
            let pool = init_database(&db).await?;
            let multi_progress_bar = MultiProgress::new();

            // Create progress bars
            let scan_pb = multi_progress_bar.add(ProgressBar::new_spinner());
            scan_pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.green} Scanning files: {msg}")
                    .unwrap()
                    .tick_chars(TICK_CHARS),
            );
            scan_pb.enable_steady_tick(std::time::Duration::from_millis(30));

            let hash_pb = multi_progress_bar.add(ProgressBar::new(0));
            hash_pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.cyan} Hashing files: [{bar:40.cyan/blue}] {pos}/{len} {percent}% ({per_sec}, Elapsed: {elapsed_precise}, ETA: {eta}) {msg}")
                    .unwrap()
                    .progress_chars(PROGRESS_CHARS)
                    .tick_chars(TICK_CHARS),
            );
            hash_pb.enable_steady_tick(std::time::Duration::from_millis(30));

            let db_pb = multi_progress_bar.add(ProgressBar::new(0));
            db_pb.set_style(
                ProgressStyle::default_bar()
                    .template(
                        "{spinner:.yellow} Writing to database: [{bar:40.yellow/blue}] {pos}/{len} {percent}% ({per_sec}, Elapsed: {elapsed_precise}) {msg}",
                    )
                    .unwrap()
                    .progress_chars(PROGRESS_CHARS)
                    .tick_chars(TICK_CHARS),
            );
            db_pb.enable_steady_tick(std::time::Duration::from_millis(30));

            // Spawn channels, all using crossbeam
            let (scanned_files_tx, scanned_files_rx) =
                crossbeam_channel::bounded::<FileMetadata>(QUEUE_SIZE);

            // Use crossbeam channel for sending to rayon workers
            let (filtered_files_tx, filtered_files_rx) =
                crossbeam_channel::bounded::<FileMetadata>(QUEUE_SIZE);

            // Use crossbeam channel for receiving from rayon workers
            let (hashed_files_tx, hashed_files_rx) =
                crossbeam_channel::bounded::<HashedFile>(QUEUE_SIZE);

            // Spawn tasks
            let scan_pb_clone = scan_pb.clone();
            let shutdown_clone = Arc::clone(&shutdown);
            // The scanner does blocking filesystem I/O and blocking channel
            // sends, so run it on the blocking thread pool.
            let scan_handle = tokio::task::spawn_blocking(move || {
                fs::scan(
                    paths,
                    follow_links,
                    &scanned_files_tx,
                    scan_pb_clone,
                    shutdown_clone,
                )
            });

            let scan_pb_clone = scan_pb.clone();
            let hash_pb_clone = hash_pb.clone();
            let shutdown_clone = Arc::clone(&shutdown);
            let filtered_files_tx_clone = filtered_files_tx.clone();
            let filter_handle = tokio::spawn(filter_files(
                pool.clone(),
                scanned_files_rx,
                filtered_files_tx_clone,
                rehash,
                scan_pb_clone,
                hash_pb_clone,
                shutdown_clone,
            ));

            let num_hashers = fs::get_num_hashers();
            debug!("Using {} hash workers", num_hashers);

            // Spawn rayon-based hash workers
            let hash_pb_clone = hash_pb.clone();
            let db_pb_clone = db_pb.clone();
            let shutdown_clone = Arc::clone(&shutdown);
            let hash_handle = fs::spawn_hash_workers(
                filtered_files_rx,
                hashed_files_tx.clone(),
                hash_pb_clone,
                db_pb_clone,
                shutdown_clone,
            );

            let shutdown_clone = Arc::clone(&shutdown);
            let db_writer_handle = tokio::spawn(database_writer(
                pool.clone(),
                hashed_files_rx,
                db_pb,
                shutdown_clone,
            ));

            // Wait for scanner to finish
            let scan_result = scan_handle.await;
            debug!("Scanner finished");

            // Wait for filter to finish processing scanned files
            let filter_result = filter_handle.await;
            debug!("Filter finished");

            // Drop filtered_files_tx to signal hash workers no more files are coming
            drop(filtered_files_tx);

            // Wait for rayon hash workers to finish
            debug!("Waiting for rayon hash workers to complete...");
            if let Err(e) = hash_handle.join() {
                warn!("Hash worker thread panicked: {:?}", e);
            }
            debug!("All files have been hashed");

            // Drop hashed_files_tx to signal database writer no more hashes coming
            drop(hashed_files_tx);

            // Wait for database writer to flush all data
            debug!("Saving hashes to database...");
            let db_result = db_writer_handle.await;

            // Propagate errors from the pipeline stages now that everything
            // has drained. A failed scan must not look like a successful run.
            let scanned = scan_result??;
            let filtered = filter_result??;
            let written = db_result??;
            debug!(
                "Scanned {} files, hashed {}, wrote {}",
                scanned, filtered, written
            );

            export_duplicates(&pool, output.as_deref(), min_size).await?;
            pool.close().await;
        }
        Commands::Clear { db } => {
            let pool = init_database(&db).await?;
            clear_database(&pool).await?;
            pool.close().await;
        }
        Commands::Stats { db } => {
            let pool = init_database(&db).await?;
            show_stats(&pool).await?;
            pool.close().await;
        }
    }

    Ok(())
}
