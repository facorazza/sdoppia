use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use sha2::{Digest, Sha256};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use tempfile::TempDir;

fn sdoppia_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sdoppia")
}

fn run(args: &[&str]) -> Output {
    Command::new(sdoppia_bin())
        .args(args)
        // The binary's tracing filter honors RUST_LOG; keep test output
        // deterministic regardless of the caller's environment.
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run sdoppia binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn open_db(db: &Path, create: bool) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(db)
        .create_if_missing(create);
    SqlitePool::connect_with(options).await.unwrap()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn scan_nonexistent_path_fails_with_nonzero_exit() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.db");

    let output = run(&[
        "scan",
        "/definitely/not/a/real/path",
        "--db",
        db.to_str().unwrap(),
    ]);

    assert!(
        !output.status.success(),
        "scan of a nonexistent path must fail, got: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("Invalid directory path"),
        "stderr should report the invalid path, got: {}",
        stderr(&output)
    );
}

#[test]
fn scan_finds_duplicates_and_reports_them() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();

    // Two identical files and one unique file.
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("c.txt"), b"unique content 12345\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    let output = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "scan failed: {}", stderr(&output));

    let report_text = fs::read_to_string(&report).unwrap();
    assert!(report_text.contains("Duplicate groups: 1"), "{report_text}");
    assert!(
        report_text.contains("Total duplicate files: 1"),
        "{report_text}"
    );
    assert!(report_text.contains("a.txt"), "{report_text}");
    assert!(report_text.contains("b.txt"), "{report_text}");
    assert!(!report_text.contains("c.txt"), "{report_text}");
}

#[test]
fn stats_and_clear_reflect_database_contents() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");

    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(stats.status.success());
    assert!(
        stdout(&stats).contains("Total files: 2"),
        "stats should report 2 files, got: {}",
        stdout(&stats)
    );
    assert!(
        stdout(&stats).contains("Duplicate files: 1"),
        "stats should report 1 duplicate, got: {}",
        stdout(&stats)
    );

    let clear = run(&["clear", "--db", db.to_str().unwrap()]);
    assert!(clear.status.success());

    let stats_after = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(stats_after.status.success());
    assert!(
        stdout(&stats_after).contains("Total files: 0"),
        "stats after clear should report 0 files, got: {}",
        stdout(&stats_after)
    );
}

#[test]
fn rescan_skips_unchanged_files() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");

    let first = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        first.status.success(),
        "first scan failed: {}",
        stderr(&first)
    );

    // Second scan of unchanged files must succeed and not error.
    let second = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        second.status.success(),
        "second scan failed: {}",
        stderr(&second)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(stats.status.success());
    assert!(
        stdout(&stats).contains("Total files: 1"),
        "stats should still report 1 file, got: {}",
        stdout(&stats)
    );
}

#[test]
fn scan_single_file_path() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("solo.txt");
    fs::write(&file, b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let output = run(&["scan", file.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "scanning a single file failed: {}",
        stderr(&output)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(stdout(&stats).contains("Total files: 1"));
}

#[test]
fn min_size_filters_small_files_from_report() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    let scan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
        "--min-size",
        "1000000",
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let report_text = fs::read_to_string(&report).unwrap();
    assert!(
        report_text.contains("Duplicate groups: 0"),
        "small duplicates should be filtered out by min-size, got: {report_text}"
    );
}

#[test]
fn db_path_is_respected() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("custom.db");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));
    assert!(Path::new(&db).exists(), "custom db file should exist");
}

/// Two paths with identical content are only reclaimable waste when they are
/// distinct objects. A hard link shares the original's inode, so removing one
/// frees nothing and must not be counted as a copy.
#[test]
fn hard_link_is_not_counted_as_a_reclaimable_copy() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::write(dir.join("b.txt"), b"AAAA\n").unwrap();
    fs::hard_link(dir.join("a.txt"), dir.join("a_hardlink")).unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    let scan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let text = fs::read_to_string(&report).unwrap();
    assert!(
        text.contains("Copies: 2"),
        "a hard link must not inflate the copy count:\n{text}"
    );
    assert!(
        text.contains("Wasted: 5 bytes"),
        "only the genuinely separate copy is waste:\n{text}"
    );
    assert!(
        text.contains("a_hardlink"),
        "the link should still be listed, just not as a copy:\n{text}"
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Duplicate files: 1"),
        "stats must agree with the report, got: {}",
        stdout(&stats)
    );
}

/// One file plus links to it is one object, so there is nothing to reclaim and
/// no duplicate group at all.
#[test]
fn links_without_a_second_object_form_no_duplicate_group() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::hard_link(dir.join("a.txt"), dir.join("a_hardlink")).unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    let scan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let text = fs::read_to_string(&report).unwrap();
    assert!(
        text.contains("Duplicate groups: 0"),
        "a file and its own link are not a duplicate pair:\n{text}"
    );
    assert!(text.contains("Total duplicate files: 0"), "{text}");

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Duplicate files: 0"),
        "stats must agree with the report, got: {}",
        stdout(&stats)
    );
    assert!(
        stdout(&stats).contains("Wasted space: 0 bytes"),
        "stats must agree with the report, got: {}",
        stdout(&stats)
    );
}

/// The stored path must be the one the user asked for, not a symlink-resolved
/// target. Resolving links used to make a symlink and its target collide on the
/// unique path key, so one of the two rows was silently dropped.
#[cfg(unix)]
#[test]
fn follow_links_keeps_a_symlink_and_its_target_as_separate_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::write(dir.join("b.txt"), b"AAAA\n").unwrap();
    std::os::unix::fs::symlink(dir.join("a.txt"), dir.join("a_symlink")).unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    let scan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
        "--follow-links",
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let text = fs::read_to_string(&report).unwrap();
    assert!(
        text.contains("Copies: 2"),
        "a.txt and b.txt are the only distinct objects:\n{text}"
    );
    assert!(
        text.contains("a_symlink"),
        "the symlink must be listed rather than collapsed into its target:\n{text}"
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 3"),
        "all three paths should be stored, got: {}",
        stdout(&stats)
    );
}

/// Databases created before hard-link detection have no dev/ino columns. The
/// migration must add them without disturbing existing rows, which keep the
/// "unknown identity" default and count individually as they always have.
#[tokio::test]
async fn existing_database_gains_identity_columns_without_data_loss() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();

    let db = tmp.path().join("legacy.db");

    let pool = open_db(&db, true).await;
    sqlx::query(
        "CREATE TABLE hashes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO hashes (path, hash, size, mtime) VALUES (?, ?, ?, ?)")
        .bind("/legacy/a.txt")
        .bind("deadbeef")
        .bind(5i64)
        .bind(1i64)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let pool = open_db(&db, false).await;
    let legacy: (String, i64, i64) =
        sqlx::query_as("SELECT hash, dev, ino FROM hashes WHERE path = '/legacy/a.txt'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy.0, "deadbeef", "the pre-existing row must survive");
    assert_eq!(
        (legacy.1, legacy.2),
        (0, 0),
        "rows predating the migration get the unknown-identity default"
    );
    pool.close().await;

    // Running again must be idempotent: the columns already exist.
    let second = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        second.status.success(),
        "rescan failed: {}",
        stderr(&second)
    );
}

/// A cached hash may only be reused when the size **and** the mtime match the
/// stored row. The stored mtime only has one-second granularity, so a rewrite
/// that preserves it (`cp -p`, `rsync --times`, tar extraction, `git checkout`,
/// or a second edit within the same second) is invisible to an mtime-only check
/// and would keep a stale hash indefinitely.
///
/// The test plants the row such a rewrite leaves behind: b.txt's stored hash and
/// size are stale, but its stored mtime still matches the untouched file on
/// disk. A correct rescan must notice the size mismatch, rehash b.txt, and put
/// it back into the duplicate group.
#[tokio::test]
async fn size_change_forces_rehash_even_when_mtime_matches() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::write(dir.join("b.txt"), b"AAAA\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    let first = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        first.status.success(),
        "first scan failed: {}",
        stderr(&first)
    );

    let pool = open_db(&db, false).await;
    sqlx::query("UPDATE hashes SET size = 999, hash = ? WHERE path LIKE '%b.txt'")
        .bind(sha256_hex(b"ZZZZ\n"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let second = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
    ]);
    assert!(
        second.status.success(),
        "second scan failed: {}",
        stderr(&second)
    );

    let report_text = fs::read_to_string(&report).unwrap();
    assert!(
        report_text.contains("Duplicate groups: 1"),
        "b.txt was not rehashed after its size changed, so it was wrongly excluded \
         from the duplicate group. Report:\n{report_text}"
    );
}

/// Hashes that never reach the database must not be reported as a successful
/// scan. The run has to exit non-zero and must not emit a report derived from a
/// database that is missing every file it just hashed.
#[tokio::test]
async fn scan_fails_when_database_writes_fail() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::write(dir.join("b.txt"), b"AAAA\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");

    // Identical to the schema sdoppia creates, plus a constraint that rejects
    // every row it will try to insert.
    let pool = open_db(&db, true).await;
    sqlx::query(
        r#"
        CREATE TABLE hashes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL,
            CHECK (size < 0)
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let output = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
    ]);

    assert!(
        !output.status.success(),
        "a scan that stored nothing must exit non-zero, got stdout: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("Database operation failed"),
        "stderr should surface the insert failure, got: {}",
        stderr(&output)
    );
    assert!(
        !report.exists(),
        "no report may be written when the hashes never reached the database"
    );

    let pool = open_db(&db, false).await;
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hashes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "the rejected rows must not appear as stored");
    pool.close().await;
}
