# dedup

A CLI tool to scan directories, hash files, and find duplicate files.

`dedup` walks one or more directories, SHA-256 hashes every file, stores the
results in a local SQLite database, and reports duplicate groups with the
space they waste. On rescan, unchanged files (same size and modification
time) are skipped using cached hashes, so repeat scans are fast.

## Installation

```sh
cargo install --path .
```

## Usage

```
dedup [OPTIONS] <COMMAND>

Commands:
  scan   Scan directories and hash all files
  clear  Clear all entries from the database
  stats  Show database statistics

Options:
  -v, --verbose  Enable verbose logging
  -h, --help     Print help
  -V, --version  Print version
```

### Scan

```
dedup scan [OPTIONS] <PATHS>...

Arguments:
  <PATHS>...  Directories to scan (can specify multiple)

Options:
  -d, --db <DB>              Database file path [default: dedup.db]
  -L, --follow-links         Follow symbolic links
  -r, --rehash               Force rehashing of files already in database
  -o, --output <OUTPUT>      Output file for duplicates (optional, prints to stdout if not provided)
  -m, --min-size <MIN_SIZE>  Minimum file size in bytes to include in export [default: 0]
```

## Examples

Scan a directory and print the duplicate report to stdout:

```sh
dedup scan ~/Documents
```

Scan multiple paths and write the report to a file:

```sh
dedup scan ~/Documents ~/Downloads --output duplicates.txt
```

Only report duplicates larger than 1 MB:

```sh
dedup scan ~/Documents --min-size 1048576
```

Force rehashing of files already in the database:

```sh
dedup scan ~/Documents --rehash
```

Follow symbolic links while scanning:

```sh
dedup scan ~/Documents --follow-links
```

Use a custom database file (default is `dedup.db` in the current directory):

```sh
dedup scan ~/Documents --db /path/to/dedup.db
```

Show database statistics:

```sh
dedup stats
```

Clear all entries from the database:

```sh
dedup clear
```

## Example report

```
=== DUPLICATE FILES REPORT ===
Generated: 2026-08-19 21:30:00
Total duplicate files: 1
Wasted space: 12 bytes
Duplicate groups: 1

--- Group 1 ---
Hash: a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447
Size: 12 bytes
Copies: 2
Wasted: 12 bytes
Files:
  - /home/user/docs/a.txt
  - /home/user/docs/b.txt
```

## How it works

1. **Scan** — walk the given paths and collect file metadata (path, size,
   modification time).
2. **Filter** — for each file, check the database: if the size and
   modification time match a cached entry, reuse the stored hash; otherwise
   queue the file for hashing.
3. **Hash** — SHA-256 hash queued files in parallel.
4. **Store** — batch-insert hashes into the SQLite database.
5. **Report** — group files by hash and list groups with more than one copy.

## Exit codes

- `0` — success
- `1` — error (e.g. a scan path does not exist, database failure)

## Development

```sh
cargo test          # unit and CLI integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```