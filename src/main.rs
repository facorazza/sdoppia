use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};
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

const QUEUE_SIZE: usize = 100000;

#[tokio::main]
async fn main() -> Result<()> {
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
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_clone = Arc::clone(&shutdown);
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                warn!("Received Ctrl+C, initiating graceful shutdown...");
                shutdown_clone.notify_waiters();
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
                    .template("{spinner:.green} {msg}")
                    .unwrap(),
            );

            let filter_pb = multi_progress_bar.add(ProgressBar::new_spinner());
            filter_pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.blue} {msg}")
                    .unwrap(),
            );

            let hash_pb = Arc::new(multi_progress_bar.add(ProgressBar::new(0)));
            hash_pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({per_sec}) {msg}")
                    .unwrap()
                    .progress_chars("#>-")
            );

            let db_pb = multi_progress_bar.add(ProgressBar::new(0));
            db_pb.set_style(
                ProgressStyle::default_bar()
                    .template(
                        "{spinner:.yellow} [{elapsed_precise}] [{bar:40.yellow/blue}] {pos}/{len} {msg}",
                    )
                    .unwrap()
                    .progress_chars("#>-"),
            );

            // Spawn channels
            let (scanned_files_tx, scanned_files_rx) = mpsc::channel::<FileMetadata>(QUEUE_SIZE);

            let (filtered_files_tx, filtered_files_rx) = mpsc::channel::<FileMetadata>(QUEUE_SIZE);

            let filtered_files_rx = Arc::new(tokio::sync::Mutex::new(filtered_files_rx));
            
            let (hashed_files_tx, hashed_files_rx) = mpsc::channel::<HashedFile>(QUEUE_SIZE);

            // Spawn tasks
            let scan_handle = tokio::spawn(async move {
                fs::scan(paths, follow_links, &scanned_files_tx, &multi_progress_bar).await
            });

            let filter_handle = tokio::spawn(filter_files(
                pool.clone(),
                scanned_files_rx,
                filtered_files_tx,
                rehash,
                filter_pb,
            ));

            let num_hashers = fs::get_num_hashers();
            info!("Using {} hash workers", num_hashers);

            let mut hash_handles = Vec::new();
            for _ in 0..num_hashers {
                let rx = Arc::clone(&filtered_files_rx);
                let tx = hashed_files_tx.clone();
                let pb = Arc::clone(&hash_pb);

                hash_handles.push(tokio::spawn(async move {
                    fs::hash_worker(rx, tx, pb).await;
                }));
            }

            let db_writer_handle =
                tokio::spawn(database_writer(pool.clone(), hashed_files_rx, db_pb));

            // Wait for completion or shutdown signal
            tokio::select! {
                _ = shutdown.notified() => {
                    warn!("Shutting down gracefully...");
                }
                _ = async {
                    // Wait for pipeline stages in order
                    let _ = scan_handle.await;
                    info!("Scanner completed");

                    let _ = filter_handle.await;
                    info!("Filter completed");
                } => {}
            }

            // Wait for hash workers to finish processing
            info!("Waiting for {} hash workers to complete...", num_hashers);
            for handle in hash_handles {
                let _ = handle.await;
            }
            info!("Hash workers completed");

            // Wait for database writer to flush all data
            info!("Saving hashes to database...");
            let _ = db_writer_handle.await;

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
