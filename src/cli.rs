use std::path::PathBuf;

use clap::{Parser, Subcommand};

const DATABASE_FILE: &str = "sdoppia.db";

/// Default database location: the per-OS data directory (XDG data home on
/// Linux, ~/Library/Application Support on macOS, %LOCALAPPDATA% on Windows).
pub fn default_db_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sdoppia")
        .join(DATABASE_FILE)
}

#[derive(Parser)]
#[command(
    author = env!("CARGO_PKG_AUTHORS"),
    version = env!("CARGO_PKG_VERSION"),
    about = "Duplicate files finder",
    long_about = "A CLI tool to scan directories, hash files, and find duplicates"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Scan directories and hash all files
    Scan {
        /// Directories to scan (can specify multiple)
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
        #[arg(short, long)]
        db: Option<PathBuf>,

        /// Follow symbolic links
        #[arg(short = 'L', long)]
        follow_links: bool,

        /// Force rehashing of files already in database
        #[arg(short = 'r', long, default_value = "false")]
        rehash: bool,

        /// Output file for duplicates (optional, prints to stdout if not provided)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Minimum file size in bytes to include in export
        #[arg(short, long, default_value_t = 0)]
        min_size: i64,
    },
    /// Clear all entries from the database
    Clear {
        /// Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
        #[arg(short, long)]
        db: Option<PathBuf>,
    },
    /// Show database statistics
    Stats {
        /// Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
        #[arg(short, long)]
        db: Option<PathBuf>,
    },
}
