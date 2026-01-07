use std::path::PathBuf;

use clap::{Parser, Subcommand};

const DATABASE_FILE: &str = "dedup.db";

#[derive(Parser)]
#[command(
    author,
    version,
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

        /// Database file path
        #[arg(short, long, default_value = DATABASE_FILE)]
        db: PathBuf,

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
        /// Database file path
        #[arg(short, long, default_value = DATABASE_FILE)]
        db: PathBuf,
    },
    /// Show database statistics
    Stats {
        /// Database file path
        #[arg(short, long, default_value = DATABASE_FILE)]
        db: PathBuf,
    },
}
