use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

mod cli;
mod db;
mod error;
mod fs;
mod models;
mod report;
use cli::{Cli, Commands, database_files, default_db_path, output_would_clobber_database};
use db::{
    clear_database, database_writer, export_duplicates, filter_files, init_database, show_stats,
};
use error::{DedupError, Result};
use models::{FileMetadata, HashedFile};

// Channel capacity per pipeline stage. Producers block when full, so this
// absorbs bursts rather than the whole workload.
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

/// Asks the user to confirm an irreversible action.
///
/// A prompt nobody can answer would be read as approval, so with no terminal on
/// stdin the action is refused outright and the caller has to pass the flag.
/// Anything other than an explicit `y`/`yes` — including a closed stdin — is a
/// refusal, because the cost of a wrong `y` here is the database.
fn confirm(action: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // The flag name is printed here rather than baked into the error type,
        // so renaming it is a one-line change instead of a hunt through the
        // error strings.
        eprintln!("Re-run with --yes to proceed without confirmation.");
        return Err(DedupError::ConfirmationRequired {
            action: action.to_string(),
        });
    }

    print!("{action}? [y/N] ");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    stdin.read_line(&mut answer)?;

    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Writes a message above the progress bars rather than underneath one.
///
/// `indicatif` redraws by moving the cursor up over its own rows and rewriting
/// them, so a line written straight to stderr lands on one of those rows and is
/// then overwritten by the next redraw -- it comes out truncated mid-path with a
/// bar row in its place. `println` clears the bars, writes the line above them
/// and redraws.
///
/// It prints nothing at all when the draw target is hidden (stderr redirected to
/// a file or a pipe). That is also when the bars are not drawing and there is
/// nothing to get in the way, so those go to stderr instead.
///
/// Only for a message raised while the bars are still live. Once they have been
/// finished the region stops moving, and a plain `eprintln!` lands below it and
/// stays -- `println` there is what leaves a stale copy of a bar behind.
pub(crate) fn note(bar: &ProgressBar, msg: &str) {
    if bar.is_hidden() {
        eprintln!("{msg}");
    } else {
        bar.println(msg);
    }
}

/// The next request to stop, and the name to report it under.
///
/// Ctrl+C everywhere. SIGTERM on Unix as well, because that is what `kill`,
/// systemd, Docker and Kubernetes send: a process manager shuts its children
/// down with SIGTERM precisely because it is the polite request, so handling
/// only Ctrl+C leaves the graceful path unreachable in exactly the deployments
/// that stop things deliberately. The first one is still a graceful stop; a
/// second one of either kind is read as the sender insisting.
#[cfg(unix)]
async fn next_stop_signal() -> std::io::Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok("Ctrl+C"),
        received = sigterm.recv() => match received {
            Some(()) => Ok("SIGTERM"),
            // The stream only ends if no more SIGTERMs can ever arrive, so
            // there is nothing left to wait for and nothing to report.
            None => Err(std::io::Error::other("the SIGTERM listener closed")),
        },
    }
}

/// No SIGTERM outside Unix, so Ctrl+C is the only stop request there.
#[cfg(not(unix))]
async fn next_stop_signal() -> std::io::Result<&'static str> {
    tokio::signal::ctrl_c().await.map(|()| "Ctrl+C")
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let shutdown = Arc::new(AtomicBool::new(false));
    let db_path = cli.db.unwrap_or_else(default_db_path);
    let pool = init_database(&db_path).await?;

    match cli.command {
        Commands::Scan {
            paths,
            follow_links,
            rehash,
            output,
            output_file,
            min_size,
            prune,
        } => {
            // The report is written by truncating its destination, so pointing
            // `--output-file` at the database would replace it with a text file,
            // silently, after all the scanning is done. Whether it clashes is
            // decided in `cli`, which knows that the destination is resolved
            // through any symlink pointing at it.
            if let Some(output_file) = &output_file
                && output_would_clobber_database(&db_path, output_file)
            {
                return Err(DedupError::OutputOverwritesDatabase {
                    path: output_file.display().to_string(),
                });
            }

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

            let filter_pb = multi_progress_bar.add(ProgressBar::new_spinner());
            filter_pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.magenta} Checking cache: {msg}")
                    .unwrap()
                    .tick_chars(TICK_CHARS),
            );
            filter_pb.enable_steady_tick(std::time::Duration::from_millis(30));

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

            // The handler has to stay armed: tokio installs its signal handlers
            // for the lifetime of the process, so awaiting one signal and
            // returning would leave every later one swallowed with nobody
            // listening. It lives here rather than around the whole run because
            // the flag is only ever read by scan stages, and because it needs a
            // bar to print through.
            let shutdown_clone = Arc::clone(&shutdown);
            let interrupt_pb = scan_pb.clone();
            tokio::spawn(async move {
                loop {
                    let signal_name = match next_stop_signal().await {
                        Ok(name) => name,
                        Err(e) => {
                            note(
                                &interrupt_pb,
                                &format!("Failed to listen for stop signals: {e}"),
                            );
                            return;
                        }
                    };

                    // swap reports the previous value, so the first signal
                    // asks for a graceful stop and any later one is read as
                    // the user insisting. Shutdown is cooperative, so a stage
                    // stuck in a long-running query can only be escaped this way.
                    if shutdown_clone.swap(true, Ordering::SeqCst) {
                        note(
                            &interrupt_pb,
                            &format!("Received {signal_name} again, exiting immediately"),
                        );
                        std::process::exit(130);
                    }
                    note(
                        &interrupt_pb,
                        &format!("Received {signal_name}, stopping after the current stage..."),
                    );
                }
            });

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
            // Resolved before `paths` moves into the scanner, and only when
            // pruning: the database stores absolute paths.
            let excluded = database_files(&std::path::absolute(&db_path)?);

            let prune_roots = if prune {
                paths
                    .iter()
                    .map(std::path::absolute)
                    .collect::<std::result::Result<Vec<PathBuf>, _>>()?
            } else {
                Vec::new()
            };

            let scan_pb_clone = scan_pb.clone();
            let shutdown_clone = Arc::clone(&shutdown);
            // The scanner does blocking filesystem I/O and blocking channel
            // sends, so run it on the blocking thread pool.
            let scan_handle = tokio::task::spawn_blocking(move || {
                fs::scan(
                    paths,
                    follow_links,
                    &excluded,
                    &scanned_files_tx,
                    scan_pb_clone,
                    shutdown_clone,
                )
            });

            let filter_pb_clone = filter_pb.clone();
            let hash_pb_clone = hash_pb.clone();
            let shutdown_clone = Arc::clone(&shutdown);
            let filtered_files_tx_clone = filtered_files_tx.clone();
            let filter_handle = tokio::spawn(filter_files(
                pool.clone(),
                scanned_files_rx,
                filtered_files_tx_clone,
                rehash,
                filter_pb_clone,
                hash_pb_clone,
                shutdown_clone,
            ));

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

            // Wait for filter to finish processing scanned files
            let filter_result = filter_handle.await;

            // Drop filtered_files_tx to signal hash workers no more files are coming
            drop(filtered_files_tx);

            // Wait for rayon hash workers to finish
            if let Err(e) = hash_handle.join() {
                note(&scan_pb, &format!("Hash worker thread panicked: {e:?}"));
            }

            // Drop hashed_files_tx to signal database writer no more hashes coming
            drop(hashed_files_tx);

            // Wait for database writer to flush all data
            let db_result = db_writer_handle.await;

            // Propagate errors from the pipeline stages now that everything
            // has drained. A failed scan must not look like a successful run.
            let outcome = scan_result??;
            let filtered = filter_result??;
            let written = db_result??;

            // The only bar whose owner never finishes it. Left running, its
            // steady tick keeps redrawing the row -- and that redraw lands
            // after the report, on top of it. Finishing keeps the line it has
            // already drawn and stops the redraws; clearing is not needed.
            hash_pb.finish_with_message(format!("Hashed {filtered} files"));

            // The progress bar reporting this is transient, and an unwalked
            // subtree is a gap in the cache and the report both.
            if outcome.errors > 0 {
                note(
                    &scan_pb,
                    &format!(
                        "{} path(s) could not be read and were not scanned",
                        outcome.errors
                    ),
                );
            }

            // Pruned before the report is built, so the report describes the
            // disk as it is now. Skipped when interrupted: the run is about to
            // exit 130 without a report anyway.
            let removed = if prune && !shutdown.load(Ordering::Relaxed) {
                Some(db::prune_missing(&pool, &prune_roots, &shutdown).await?)
            } else {
                None
            };

            // Every bar is finished by now and none will redraw, so the line
            // below them is ours to write on and it stays there. Always
            // reported when the flag was passed: silence would leave "removed
            // nothing" and "did not run" looking the same.
            match removed {
                Some(0) => eprintln!("Nothing to prune: every stored path still exists"),
                Some(removed) => {
                    eprintln!("Removed {removed} entries for files that no longer exist")
                }
                None => {}
            }

            export_duplicates(
                &pool,
                output_file.as_deref(),
                output,
                min_size,
                Arc::clone(&shutdown),
            )
            .await?;
            pool.close().await;

            // A partial scan must not look successful even though every stage
            // drained cleanly. export_duplicates watches the same flag and
            // declines to write a report from a partial scan.
            if shutdown.load(Ordering::Relaxed) {
                note(
                    &scan_pb,
                    &format!(
                        "Scan interrupted: {} scanned ({} unreadable), {} hashed, {} written",
                        outcome.files, outcome.errors, filtered, written
                    ),
                );
                std::process::exit(130);
            }
        }
        Commands::Clear { yes } => {
            if confirm("delete every entry from the database", yes)? {
                clear_database(&pool).await?;
                pool.close().await;
            } else {
                println!("Aborted: the database was left untouched.");
            }
        }
        Commands::Stats => {
            show_stats(&pool).await?;
            pool.close().await;
        }
    }

    Ok(())
}
