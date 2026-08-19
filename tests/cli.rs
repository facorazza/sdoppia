use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

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
