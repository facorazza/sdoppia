use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::report::Format;

const DATABASE_FILE: &str = "sdoppia.db";

/// Default database location: the per-OS data directory (XDG data home on
/// Linux, ~/Library/Application Support on macOS, %LOCALAPPDATA% on Windows).
///
/// Out of the scanned tree on purpose: the database accumulates every directory
/// ever scanned, so inside one it would be both scan input and content that
/// changes as a side effect of running the tool.
pub fn default_db_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sdoppia")
        .join(DATABASE_FILE)
}

/// The database and its `-wal`/`-shm` side files, so a scan pointed at a
/// directory containing them does not record them as content.
///
/// Both spellings of each path: the scanner stores `std::path::absolute(entry)`,
/// which resolves a relative path through a `getcwd` with symlinks resolved but
/// leaves an absolute one alone, so on macOS `/var/...` and `/private/var/...`
/// are one file under two names.
pub fn database_files(db_path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    // The database exists by now, so this resolves; the side files may not, so
    // the suffixes are appended afterwards rather than resolved individually.
    let resolved = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());

    for base in [db_path.to_path_buf(), resolved] {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = base.as_os_str().to_os_string();
            name.push(suffix);
            files.push(PathBuf::from(name));
        }
    }

    files.sort();
    files.dedup();
    files
}

/// Whether writing the report to `output_file` would destroy the database.
///
/// The report is written by truncating its destination, and `File::create`
/// follows symlinks, so an output path that is a link to the database destroys
/// it while looking like a different path. Both sides are resolved before being
/// compared; comparing the spellings misses the link, and the `/var` vs
/// `/private/var` case on macOS alike.
pub fn output_would_clobber_database(db_path: &Path, output_file: &Path) -> bool {
    let resolve = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
    };

    database_files(db_path)
        .iter()
        .any(|db_file| resolve(db_file) == resolve(output_file))
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

    /// Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
    // Global rather than per-subcommand because every subcommand reads one, so
    // declaring it once here beats repeating the same option three times. Clap
    // keeps accepting it after the subcommand name too.
    #[arg(short, long, global = true)]
    pub db: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Scan directories and hash all files
    Scan {
        /// Directories to scan (can specify multiple, defaults to the current directory)
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,

        /// Follow symbolic links
        #[arg(short = 'L', long)]
        follow_links: bool,

        /// Force rehashing of files already in database
        #[arg(short = 'r', long, default_value = "false")]
        rehash: bool,

        /// Output format for the duplicate report: text to read, json to parse
        #[arg(short = 'o', long, value_enum, default_value = "text")]
        output: Format,

        /// Write the report to this file instead of stdout
        #[arg(short = 'O', long)]
        output_file: Option<PathBuf>,

        /// Minimum file size in bytes to include in export
        #[arg(short, long, default_value_t = 0)]
        min_size: i64,

        /// Also delete entries for scanned files that no longer exist
        #[arg(short = 'p', long)]
        prune: bool,
    },
    /// Clear all entries from the database
    Clear {
        /// Clear without asking for confirmation
        #[arg(short, long, default_value = "false")]
        yes: bool,
    },
    /// Show database statistics
    Stats,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The database exists by the time the guard runs.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sdoppia.db");
        std::fs::write(&db, b"not really a database").unwrap();
        (dir, db)
    }

    #[test]
    fn the_database_itself_is_refused() {
        let (_dir, db) = fixture();
        assert!(output_would_clobber_database(&db, &db));
    }

    /// A destination that does not exist yet cannot be resolved, so the
    /// comparison falls back to spelling. It must not refuse every fresh report
    /// path, least of all one written beside the database.
    #[test]
    fn an_unrelated_destination_is_allowed() {
        let (dir, db) = fixture();

        for ok in [
            dir.path().join("report.txt"),
            db.with_file_name("report.txt"),
            db.with_file_name("sdoppia.db.bak"),
            dir.path().join("sub").join("report.json"),
        ] {
            assert!(!output_would_clobber_database(&db, &ok), "{ok:?}");
        }
    }

    /// `File::create` follows links, so the report would land on the database.
    /// Comparing spellings misses this, because two different names are one
    /// file. Covers both a link to the database itself and a symlinked directory
    /// on the way to it, which is the macOS `/var` vs `/private/var` shape.
    #[cfg(unix)]
    #[test]
    fn links_to_the_database_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let db = real_dir.join("sdoppia.db");
        std::fs::write(&db, b"not really a database").unwrap();

        let alias = db.with_file_name("alias.db");
        std::os::unix::fs::symlink(&db, &alias).unwrap();
        assert!(output_would_clobber_database(&db, &alias));

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        assert!(output_would_clobber_database(&db, &link.join("sdoppia.db")));
    }

    /// The side files carry the same data as the database itself.
    #[test]
    fn the_side_files_are_refused_too() {
        let (_dir, db) = fixture();
        for suffix in ["-wal", "-shm"] {
            let side = db.with_file_name(format!("sdoppia.db{suffix}"));
            assert!(output_would_clobber_database(&db, &side), "{suffix}");
        }
    }
}
