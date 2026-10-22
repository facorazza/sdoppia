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

/// Refuses to run a command without an explicit `--db`.
///
/// The default database is a real file at a fixed, discoverable path, so a test
/// that forgets `--db` does not quietly use a scratch database — it opens the
/// user's own, and `clear --yes` against that is unrecoverable. Every test here
/// passes `--db` already; this makes a future omission a failure here rather
/// than a surprise much later.
fn require_explicit_db(args: &[&str]) {
    assert!(
        args.iter().any(|a| *a == "--db" || *a == "-d"),
        "every test must pass --db: {args:?}"
    );
}

fn run(args: &[&str]) -> Output {
    require_explicit_db(args);
    Command::new(sdoppia_bin())
        .args(args)
        .output()
        .expect("failed to run sdoppia binary")
}

/// Same, but from a given working directory, for the cases where the run has no
/// paths of its own and falls back to wherever it was invoked.
fn run_in(dir: &Path, args: &[&str]) -> Output {
    require_explicit_db(args);
    Command::new(sdoppia_bin())
        .args(args)
        .current_dir(dir)
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
        stderr(&output).contains("Invalid path: /definitely/not/a/real/path"),
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
        "--output-file",
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

/// `--output json` has to parse, with the numbers as numbers rather than the
/// rounded strings the text report prints, and a report is written even when
/// there is nothing in it.
#[test]
fn json_output_is_machine_readable() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.json");
    let scan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        "json",
        "--output-file",
        report.to_str().unwrap(),
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(parsed["duplicate_groups"], 1);
    assert_eq!(parsed["redundant_copies"], 1);
    assert_eq!(parsed["linked_paths"], 0);
    assert_eq!(parsed["wasted_space"], 12);
    assert_eq!(parsed["groups"][0]["copies"], 2);
    assert_eq!(parsed["groups"][0]["size"], 12);
    assert!(
        parsed["groups"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().unwrap().ends_with("a.txt")),
        "the copies should be listed by path: {parsed}"
    );

    // With nothing to report the file must still appear, or a requested output
    // file is simply never written.
    fs::remove_file(dir.join("b.txt")).unwrap();
    fs::write(dir.join("a.txt"), b"changed\n").unwrap();
    let empty = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        "json",
        "--output-file",
        report.to_str().unwrap(),
    ]);
    assert!(empty.status.success(), "scan failed: {}", stderr(&empty));

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(parsed["duplicate_groups"], 0);
    assert!(parsed["groups"].as_array().unwrap().is_empty(), "{parsed}");
}
/// Three views of one database — text report, JSON report, `stats` — computed
/// three ways (SQL aggregates, Rust aggregation, and per-group queries). A drift
/// between them means one of them is lying about the disk.
#[test]
fn every_view_of_a_database_agrees() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();

    // Two groups of two copies, one of them also reached by a hard link.
    for i in 1..=2 {
        fs::write(dir.join(format!("a{i}.txt")), format!("hello {i}\n")).unwrap();
        fs::write(dir.join(format!("b{i}.txt")), format!("hello {i}\n")).unwrap();
    }
    fs::hard_link(dir.join("a1.txt"), dir.join("a1_link")).unwrap();
    fs::write(dir.join("solo.txt"), b"unique\n").unwrap();

    let db = tmp.path().join("test.db");
    let text_report = tmp.path().join("report.txt");
    let json_report = tmp.path().join("report.json");

    for (format, path) in [("text", &text_report), ("json", &json_report)] {
        let scan = run(&[
            "scan",
            dir.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--output",
            format,
            "--output-file",
            path.to_str().unwrap(),
        ]);
        assert!(scan.status.success(), "scan failed: {}", stderr(&scan));
    }

    let text = fs::read_to_string(&text_report).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&json_report).unwrap()).unwrap();
    let stats = stdout(&run(&["stats", "--db", db.to_str().unwrap()]));

    let reported = |label: &str| -> String {
        stats
            .lines()
            .find_map(|l| l.strip_prefix(label))
            .unwrap_or_else(|| panic!("stats should report {label}, got:\n{stats}"))
            .trim()
            .to_string()
    };

    // Pinned absolutely, so the comparisons below cannot pass vacuously.
    assert_eq!(json["duplicate_groups"], 2);
    assert_eq!(json["redundant_copies"], 2);
    assert_eq!(json["linked_paths"], 1);
    assert_eq!(json["wasted_space"], 16);

    assert!(text.contains("Duplicate groups: 2"), "{text}");
    assert!(text.contains("Total duplicate files: 2"), "{text}");
    assert!(text.contains("Copies: 2"), "{text}");
    assert!(text.contains("Wasted: 8 bytes"), "{text}");

    assert_eq!(
        reported("Duplicate files:"),
        "2",
        "stats vs report: {stats}"
    );
    assert_eq!(
        reported("Wasted space:"),
        "16 bytes",
        "stats vs report: {stats}"
    );
}

/// Paths are user data: a name containing awkward characters must not corrupt
/// the JSON around it. JSON escapes a quote and a backslash, which are exactly
/// the two characters Windows cannot store inside a name — a quote is invalid
/// there and a backslash is the separator — so Windows exercises other
/// characters the command line and JSON both have opinions about instead.
#[test]
fn json_output_escapes_awkward_paths() {
    #[cfg(windows)]
    let (first, second) = ("^", "&");
    #[cfg(not(windows))]
    let (first, second) = ("\"", "\\");

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(format!("we{first}ird"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("a{first}b.txt")), b"hello world\n").unwrap();
    fs::write(dir.join(format!("c{second}d.txt")), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.json");

    let output = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        "json",
        "--output-file",
        report.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "scan failed: {}", stderr(&output));

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    let files = parsed["groups"][0]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert!(
        files
            .iter()
            .any(|p| p.as_str().unwrap().contains(&format!("a{first}b.txt"))),
        "the first awkward name should survive round-tripping: {parsed}"
    );
    assert!(
        files
            .iter()
            .any(|p| p.as_str().unwrap().contains(&format!("c{second}d.txt"))),
        "the second awkward name should survive round-tripping: {parsed}"
    );
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

    let clear = run(&["clear", "--db", db.to_str().unwrap(), "--yes"]);
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
        "--output-file",
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

/// A hard link shares the original's inode, so it is not a separate copy.
/// Reaching the count wrong means claiming waste that cannot be reclaimed —
/// so a link must neither inflate the copies nor, on its own, form a group.
#[test]
fn links_are_never_a_reclaimable_copy() {
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
        "--output-file",
        report.to_str().unwrap(),
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let text = fs::read_to_string(&report).unwrap();
    assert!(
        text.contains("Copies: 2"),
        "a link must not inflate the count:\n{text}"
    );
    assert!(
        text.contains("Wasted: 5 bytes"),
        "only the real copy is waste:\n{text}"
    );
    assert!(
        text.contains("a_hardlink"),
        "the link should still be listed:\n{text}"
    );

    let stats = stdout(&run(&["stats", "--db", db.to_str().unwrap()]));
    assert!(stats.contains("Duplicate files: 1"), "{stats}");

    // The same tree without the second copy: one object plus its link is not a
    // duplicate at all.
    fs::remove_file(dir.join("b.txt")).unwrap();
    fs::remove_file(dir.join("a_hardlink")).unwrap();
    fs::hard_link(dir.join("a.txt"), dir.join("a_link")).unwrap();
    let second = tmp.path().join("second.db");
    let scan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        second.to_str().unwrap(),
        "--output-file",
        report.to_str().unwrap(),
    ]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let text = fs::read_to_string(&report).unwrap();
    assert!(
        text.contains("Duplicate groups: 0"),
        "a file and its own link:\n{text}"
    );
    let stats = stdout(&run(&["stats", "--db", second.to_str().unwrap()]));
    assert!(stats.contains("Duplicate files: 0"), "{stats}");
    assert!(stats.contains("Wasted space: 0 bytes"), "{stats}");
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
        "--output-file",
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
        "--output-file",
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

/// A volume mounted again can report a different device number, and a cache hit
/// used to leave the stored one in place. A hard link created after that change
/// was then recorded with the current number while the original kept the old
/// one, so one object was read as two copies and the report claimed space that
/// removing the link would not free.
#[cfg(any(unix, windows))]
#[tokio::test]
async fn rescan_refreshes_a_changed_identity() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();

    let db = tmp.path().join("test.db");
    let report = tmp.path().join("report.txt");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    // What a remount under a new number leaves behind: the row names a device
    // the file is not on any more.
    let pool = open_db(&db, false).await;
    sqlx::query("UPDATE hashes SET dev = dev + 1")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // A second name for the same object, recorded with the current number.
    fs::hard_link(dir.join("a.txt"), dir.join("b.txt")).unwrap();

    let rescan = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output-file",
        report.to_str().unwrap(),
    ]);
    assert!(
        rescan.status.success(),
        "rescan failed: {}",
        stderr(&rescan)
    );

    let text = fs::read_to_string(&report).unwrap();
    assert!(
        text.contains("Duplicate groups: 0"),
        "one object reached by two names is not a duplicate:\n{text}"
    );

    let pool = open_db(&db, false).await;
    let identities: Vec<(i64, i64)> = sqlx::query_as("SELECT DISTINCT dev, ino FROM hashes")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(
        identities.len(),
        1,
        "both paths must carry the identity the rescan saw: {identities:?}"
    );
}

/// `clear` is the only irreversible command, so it must never run on the
/// strength of a question nobody answered. A non-interactive stdin (a script, a
/// cron job, this test harness) cannot be prompted, so the command has to fail
/// rather than treat the silence as consent.
#[test]
fn clear_without_yes_refuses_and_keeps_the_database() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::write(dir.join("b.txt"), b"AAAA\n").unwrap();

    let db = tmp.path().join("test.db");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let clear = run(&["clear", "--db", db.to_str().unwrap()]);

    assert!(
        !clear.status.success(),
        "clear must not run unattended, got stdout: {}",
        stdout(&clear)
    );
    assert!(
        stderr(&clear).contains("--yes"),
        "the refusal should name the flag that proceeds, got: {}",
        stderr(&clear)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 2"),
        "a refused clear must leave the database untouched, got: {}",
        stdout(&stats)
    );
}

/// A deleted file keeps its entry, and an entry is still reported as a
/// duplicate for a file that is gone. `--prune` removes exactly those, and
/// nothing that is still on disk.
#[test]
fn prune_removes_only_entries_for_deleted_files() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("survivor.txt"), b"kept\n").unwrap();

    let db = tmp.path().join("test.db");
    assert!(
        run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()])
            .status
            .success()
    );

    fs::remove_file(dir.join("b.txt")).unwrap();
    assert!(
        run(&[
            "scan",
            dir.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--prune"
        ])
        .status
        .success()
    );

    let stats = stdout(&run(&["stats", "--db", db.to_str().unwrap()]));
    assert!(
        stats.contains("Total files: 2"),
        "the two live files must remain: {stats}"
    );
    assert!(stats.contains("Duplicate files: 0"), "{stats}");
    assert!(stats.contains("Wasted space: 0 bytes"), "{stats}");
}
/// `Duplicate percentage` is a share of objects, not of paths.
///
/// A hard link is another name for a file that is already counted, so ten paths
/// over here are five objects. Counting the paths would report the tree as half
/// duplicate when a single copy of one file is redundant.
#[test]
fn duplicate_percentage_counts_objects_not_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();

    // Five objects, each reached by two paths.
    for i in 1..=5 {
        fs::write(dir.join(format!("u{i}.txt")), format!("unique {i}\n")).unwrap();
        fs::hard_link(
            dir.join(format!("u{i}.txt")),
            dir.join(format!("u{i}.link")),
        )
        .unwrap();
    }
    // And exactly one genuine duplicate pair.
    fs::write(dir.join("d1.txt"), b"dupe\n").unwrap();
    fs::write(dir.join("d2.txt"), b"dupe\n").unwrap();

    let db = tmp.path().join("test.db");
    assert!(
        run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()])
            .status
            .success()
    );

    let stats = stdout(&run(&["stats", "--db", db.to_str().unwrap()]));
    let reported = |label: &str| -> String {
        stats
            .lines()
            .find_map(|l| l.strip_prefix(label))
            .unwrap_or_else(|| panic!("stats should report {label}, got:\n{stats}"))
            .trim()
            .to_string()
    };

    // Pinned so the comparison below cannot pass vacuously: 12 paths, 7 objects.
    assert_eq!(reported("Total files:"), "12", "{stats}");
    assert_eq!(reported("Duplicate files:"), "1", "{stats}");

    // 1 redundant copy over 7 objects is 14.29%, not 1 over 12 paths.
    assert_eq!(
        reported("Duplicate percentage:"),
        "14.29%",
        "the denominator should be objects, not paths: {stats}"
    );
}

/// Pruning is opt-in. A scan without `--prune` must leave the database exactly
/// as it found it.
#[test]
fn scan_without_prune_keeps_entries_for_deleted_files() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    fs::remove_file(dir.join("b.txt")).unwrap();
    let rescan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        rescan.status.success(),
        "rescan failed: {}",
        stderr(&rescan)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 2"),
        "without --prune the stale entry must survive, got: {}",
        stdout(&stats)
    );
}

/// Prune scoping is byte-exact: paths are stored as bytes, SQLite compares it with
/// `memcmp`, and no collation is involved. A root typed in a different case than
/// it was scanned therefore matches nothing, even on a case-insensitive
/// filesystem where it names the same directory.
///
/// That is a deliberate trade for storing paths losslessly, and it fails safe:
/// the entries stay, rather than something live being deleted. Scanned paths
/// carry the casing the filesystem reported, so this only bites if the case in
/// the command varies between runs. See README, "Known limitations".
#[cfg(target_os = "macos")]
#[tokio::test]
async fn prune_scoping_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("Documents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello\n").unwrap();

    let db = tmp.path().join("test.db");
    assert!(
        run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()])
            .status
            .success()
    );
    fs::remove_file(dir.join("b.txt")).unwrap();

    let shouted = dir.to_string_lossy().to_uppercase();
    let pruned = run(&["scan", &shouted, "--db", db.to_str().unwrap(), "--prune"]);
    assert!(
        pruned.status.success(),
        "prune scan failed: {}",
        stderr(&pruned)
    );

    let pool = open_db(&db, false).await;
    let paths: Vec<Vec<u8>> = sqlx::query_scalar("SELECT path FROM hashes")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;

    assert!(
        paths.iter().any(|p| p.ends_with(b"b.txt")),
        "a differently-cased root is out of scope, so the stale entry survives: {paths:?}"
    );

    // The same root, spelled as it was scanned, does prune it.
    let pruned = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--prune",
    ]);
    assert!(
        pruned.status.success(),
        "prune scan failed: {}",
        stderr(&pruned)
    );

    let pool = open_db(&db, false).await;
    let paths: Vec<Vec<u8>> = sqlx::query_scalar("SELECT path FROM hashes")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(
        !paths.iter().any(|p| p.ends_with(b"b.txt")),
        "the entry for the deleted file should be gone: {paths:?}"
    );
}

/// What a device number means depends on the platform: Unix reassigns it per
/// boot, so a mismatch with the surviving ancestor must not save a deleted
/// entry; a Windows volume serial survives a remount, so the same mismatch is an
/// unmounted volume and the entry must stay. A real mount cannot be created in a
/// test, but the device on the row is the only signal the guard reads, and the
/// file is gone, which is what reaches it.
#[tokio::test]
async fn a_stale_device_number_does_not_keep_a_deleted_file_by_itself() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    // Gone from disk, recorded against a device the surviving ancestor does not
    // report: a renumbered device on Unix, an unmounted volume on Windows.
    fs::remove_file(dir.join("a.txt")).unwrap();

    let pool = open_db(&db, false).await;
    sqlx::query("UPDATE hashes SET dev = ?")
        .bind(i64::MAX)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let pruned = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--prune",
    ]);
    assert!(
        pruned.status.success(),
        "prune scan failed: {}",
        stderr(&pruned)
    );

    let pool = open_db(&db, false).await;
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hashes")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;

    #[cfg(windows)]
    assert_eq!(
        remaining, 1,
        "an entry whose volume is unreachable must not be pruned away"
    );
    #[cfg(not(windows))]
    assert_eq!(
        remaining, 0,
        "a deleted file recorded under an earlier device number must be pruned"
    );
}

/// stdout exists for the report and nothing else: anything else logged there
/// would corrupt a JSON report being piped into another program.
#[test]
fn json_report_on_stdout_is_parseable() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let output = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output",
        "json",
        "--prune",
    ]);
    assert!(output.status.success(), "scan failed: {}", stderr(&output));

    let parsed: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap_or_else(|e| {
        panic!(
            "stdout must be nothing but JSON, but it did not parse: {e}\n--- stdout ---\n{}",
            stdout(&output)
        )
    });
    assert_eq!(parsed["duplicate_groups"], 1);
}

/// A subtree the walk cannot enter is a hole in the scan. Silently dropping it
/// would let the run describe the tree as fully covered, so it is reported.
#[cfg(unix)]
#[test]
fn scan_reports_paths_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    let locked = dir.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(dir.join("a.txt"), b"AAAA\n").unwrap();
    fs::write(locked.join("hidden.txt"), b"BBBB\n").unwrap();

    // Restore the mode afterwards so the temp dir can be cleaned up.
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&locked, perms).unwrap();

    let db = tmp.path().join("test.db");
    let output = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);

    let mut perms = fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&locked, perms).unwrap();

    // The run itself still succeeds — the readable part was scanned — but the
    // gap is surfaced rather than hidden.
    assert!(output.status.success(), "scan failed: {}", stderr(&output));
    assert!(
        stderr(&output).contains("unreadable"),
        "the unreadable subtree should be reported, got: {}",
        stderr(&output)
    );
}

/// The prune walks the table a page at a time, advancing past each page by the
/// highest id it saw. A row beyond the first page is only reached if that
/// bookkeeping is right, and getting it wrong would stop the prune silently
/// after one page rather than fail.
#[test]
fn prune_reaches_past_the_first_page() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();

    // Comfortably more than one page of entries.
    for i in 0..1100 {
        fs::write(dir.join(format!("f{i}.txt")), b"hello world\n").unwrap();
    }

    let db = tmp.path().join("test.db");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    for i in 0..1100 {
        fs::remove_file(dir.join(format!("f{i}.txt"))).unwrap();
    }
    fs::write(dir.join("survivor.txt"), b"still here\n").unwrap();

    let pruned = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--prune",
    ]);
    assert!(
        pruned.status.success(),
        "prune scan failed: {}",
        stderr(&pruned)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 1"),
        "every deleted entry should be pruned, including past the first page, got: {}",
        stdout(&stats)
    );
}

/// `scan` with no paths means "here", which is the only sensible reading of a
/// command that is otherwise just sitting in a directory.
#[test]
fn scan_without_paths_scans_the_current_directory() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    // The database lives outside the directory being scanned, so the run cannot
    // pick up its own files.
    let db = tmp.path().join("test.db");
    let output = run_in(&dir, &["scan", "--db", db.to_str().unwrap()]);
    assert!(output.status.success(), "scan failed: {}", stderr(&output));

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 2"),
        "the working directory's files should have been scanned, got: {}",
        stdout(&stats)
    );
    assert!(
        stdout(&stats).contains("Duplicate files: 1"),
        "got: {}",
        stdout(&stats)
    );
}

/// The default has to survive alongside `--prune`, which compares stored paths
/// against the roots it was given. A `.` that is not resolved the same way as
/// the scanner resolves it would match nothing.
#[test]
fn prune_without_paths_uses_the_current_directory_as_the_root() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let scan = run_in(&dir, &["scan", "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    fs::remove_file(dir.join("a.txt")).unwrap();
    let pruned = run_in(&dir, &["scan", "--db", db.to_str().unwrap(), "--prune"]);
    assert!(
        pruned.status.success(),
        "prune scan failed: {}",
        stderr(&pruned)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 0"),
        "`--prune` with no paths should scope to the working directory, got: {}",
        stdout(&stats)
    );
}

/// The database and its `-wal`/`-shm` side files must never be scanned.
///
/// Compared against both spellings of the path, because the scanner stores
/// `std::path::absolute(entry)`: that resolves a relative path through a
/// `getcwd` with symlinks resolved but leaves an absolute one alone, so on
/// macOS `/var/...` and `/private/var/...` name one file under two names and a
/// comparison against only the given spelling would match nothing.
#[tokio::test]
async fn the_database_does_not_scan_itself() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = dir.join("sdoppia.db");
    // Twice: the database is empty after the first run and only has content to
    // be mistaken for on the second.
    for _ in 0..2 {
        let out = run_in(&dir, &["scan", "--db", db.to_str().unwrap()]);
        assert!(out.status.success(), "scan failed: {}", stderr(&out));
    }

    let pool = open_db(&db, false).await;
    // paths are stored as bytes; decoded only for the assertion messages below.
    let paths: Vec<Vec<u8>> = sqlx::query_scalar("SELECT path FROM hashes")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;

    assert_eq!(
        paths.len(),
        2,
        "only the two real files should be stored: {paths:?}"
    );
}

/// The guard itself. `require_explicit_db` panics before the binary is ever
/// spawned, so this does not touch any database — if it ever did, the database
/// it would have touched is the one this test exists to protect.
#[test]
#[should_panic(expected = "every test must pass --db")]
fn a_test_cannot_reach_the_default_database() {
    run(&["stats"]);
}

/// `--output-file` truncates its destination, so pointing it at the database
/// replaces the only copy of every hash with a text file. That is silent and
/// unrecoverable, so the run is refused instead.
#[test]
fn the_report_is_not_written_over_the_database() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"hello world\n").unwrap();
    fs::write(dir.join("b.txt"), b"hello world\n").unwrap();

    let db = tmp.path().join("test.db");
    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let clobber = run(&[
        "scan",
        dir.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--output-file",
        db.to_str().unwrap(),
    ]);

    assert!(
        !clobber.status.success(),
        "writing the report over the database must fail, got: {}",
        stdout(&clobber)
    );
    assert!(
        stderr(&clobber).contains("that is the database"),
        "the refusal should say why, got: {}",
        stderr(&clobber)
    );

    let stats = run(&["stats", "--db", db.to_str().unwrap()]);
    assert!(
        stdout(&stats).contains("Total files: 2"),
        "the database must survive the attempt, got: {}",
        stdout(&stats)
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

    // First run establishes the schema and the migration history, so the
    // trigger below is the only thing standing between the scan and its rows.
    let setup = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        setup.status.success(),
        "setup scan failed: {}",
        stderr(&setup)
    );

    // Change the files so the next scan cannot reuse the cached hashes: without
    // new rows to write there would be nothing for the trigger to reject.
    fs::write(dir.join("a.txt"), b"changed a\n").unwrap();
    fs::write(dir.join("b.txt"), b"changed b\n").unwrap();

    let pool = open_db(&db, false).await;
    sqlx::query(
        "CREATE TRIGGER block_writes BEFORE INSERT ON hashes \
         BEGIN SELECT RAISE(ABORT, 'writes disabled'); END",
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
        "--output-file",
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
    assert_eq!(
        stored, 2,
        "the aborted batch must leave the setup rows untouched"
    );
    pool.close().await;
}

/// The reason paths are stored as bytes: two names differing only in bytes that are not
/// valid UTF-8 used to lossy-convert to one stored string, and since the column
/// is UNIQUE the second file silently replaced the first.
///
/// macOS cannot create such a file -- APFS requires valid UTF-8 -- so the rows
/// are planted directly. This covers the column keeping distinct bytes distinct;
/// `path_bytes` itself is covered by a unit test, and scanning a genuinely
/// non-UTF-8 filename end to end still needs a Linux run.
#[tokio::test]
async fn paths_that_differ_only_in_invalid_utf8_stay_distinct() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    let db = tmp.path().join("test.db");

    let scan = run(&["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(scan.status.success(), "scan failed: {}", stderr(&scan));

    let pool = open_db(&db, false).await;
    for raw in [b"/files/a\xff.txt".to_vec(), b"/files/a\xfe.txt".to_vec()] {
        sqlx::query(
            "INSERT INTO hashes (path, hash, size, mtime, dev, ino) \
             VALUES (?, ?, 1, 1, 1, 0)",
        )
        .bind(raw.clone())
        .bind(format!("h{}", raw.len()))
        .execute(&pool)
        .await
        .unwrap();
    }

    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT path FROM hashes ORDER BY path")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;

    assert_eq!(
        stored.len(),
        2,
        "the two names must not collapse: {stored:?}"
    );
    assert!(
        stored.iter().any(|p| p.contains(&0xff)) && stored.iter().any(|p| p.contains(&0xfe)),
        "the original bytes must survive storage: {stored:?}"
    );
}

/// A `SIGTERM` asks for the same graceful stop `Ctrl+C` does.
///
/// `kill`, systemd, Docker and Kubernetes all stop a process with `SIGTERM`, so
/// without it the graceful path is unreachable in exactly the deployments that
/// shut processes down deliberately: the process dies on the spot, mid-scan,
/// with nothing said and nothing to distinguish it from a crash. `SIGTERM` does
/// not exist outside Unix.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_a_scan_gracefully_like_ctrl_c() {
    // Local, not in the crate imports: `Stdio` is used by nothing else, and the
    // import at the top would be unused on the platforms this test is not for.
    use std::process::Stdio;

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("files");
    fs::create_dir_all(&dir).unwrap();

    // Sized so the scan is still running when the signal arrives. The signal is
    // sent as soon as the first row lands, roughly 30ms in, leaving the rest of
    // the scan as margin -- so this holds on a much faster machine than the one
    // it was written on. Raise the count if it ever becomes the flaky test.
    for i in 0..15_000 {
        fs::write(dir.join(format!("f{i}.bin")), b"x".repeat(2048)).unwrap();
    }

    let db = tmp.path().join("test.db");
    let mut child = Command::new(sdoppia_bin())
        .args(["scan", dir.to_str().unwrap(), "--db", db.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run sdoppia binary");

    // Wait for the run to reach the database writer rather than sleeping a
    // guessed interval: the signal only proves anything if it lands mid-scan.
    // The file itself appears first, so that is waited for too -- a run that has
    // not created its database yet has not started scanning either.
    for _ in 0..500 {
        if db.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(db.exists(), "the scan never created its database");

    let pool = open_db(&db, false).await;
    let mut started = false;
    for _ in 0..500 {
        // The file appears before the migration that creates the table, so an
        // error here means "not ready yet" rather than a broken run.
        let rows = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM hashes")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        if rows > 0 {
            started = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    pool.close().await;
    assert!(started, "the scan never reached the database writer");

    // `kill` rather than `libc::kill`: it needs no new dependency, and it is
    // exactly the command an operator or a service manager would run.
    let signalled = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        signalled && child.try_wait().unwrap().is_none(),
        "the scan finished before SIGTERM could reach it; raise the file count"
    );

    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "an interrupted scan exits 130, whatever asked it to stop: {}",
        stderr(&output)
    );

    // A partial scan must not look like a clean one: the report is the whole
    // point of the run, and printing one from half a scan would send someone to
    // delete files on the strength of it.
    assert!(
        stdout(&output).is_empty(),
        "an interrupted scan prints no report: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("Received SIGTERM"),
        "stderr should say what stopped it: {}",
        stderr(&output)
    );
}
