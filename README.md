# sdoppia

A CLI tool to scan directories, hash files, and find duplicate files.

`sdoppia` walks one or more directories, SHA-256
hashes every file, stores the results in a local SQLite database, and reports
duplicate groups with the space they waste. On rescan, unchanged files (same
size and modification time) are skipped using cached hashes, so repeat scans
are fast.

## Installation

From crates.io:

```shell
cargo install sdoppia
```

Or from source:

```shell
cargo install --path .
```

## Usage

```shell
sdoppia [OPTIONS] <COMMAND>

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

```shell
sdoppia scan [OPTIONS] <PATHS>...

Arguments:
  <PATHS>...  Directories to scan (can specify multiple)

Options:
  -d, --db <DB>              Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
  -L, --follow-links         Follow symbolic links
  -r, --rehash               Force rehashing of files already in database
  -o, --output <OUTPUT>      Output file for duplicates (optional, prints to stdout if not provided)
  -m, --min-size <MIN_SIZE>  Minimum file size in bytes to include in export [default: 0]
```

The database lives in the per-OS data directory by default: `~/.local/share/sdoppia/sdoppia.db`
on Linux, `~/Library/Application Support/sdoppia/sdoppia.db` on macOS, and
`%LOCALAPPDATA%\sdoppia\sdoppia.db` on Windows. Pass `--db` to use a custom location.

## Examples

Scan a directory and print the duplicate report to stdout:

```shell
sdoppia scan ~/Documents
```

Scan multiple paths and write the report to a file:

```shell
sdoppia scan ~/Documents ~/Downloads --output duplicates.txt
```

Only report duplicates larger than 1 MB:

```shell
sdoppia scan ~/Documents --min-size 1048576
```

Force rehashing of files already in the database:

```shell
sdoppia scan ~/Documents --rehash
```

Follow symbolic links while scanning:

```shell
sdoppia scan ~/Documents --follow-links
```

Use a custom database file (default is the per-OS data directory, see above):

```shell
sdoppia scan ~/Documents --db /path/to/sdoppia.db
```

Show database statistics:

```shell
sdoppia stats
```

Clear all entries from the database:

```shell
sdoppia clear
```

## Example report

```shell
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

## Links, copies and wasted space

A path on disk and the bytes it points at are not the same thing, so `sdoppia`
keeps them apart: paths are stored exactly as you asked for them, and each file
is also identified by its `(device, inode)` pair.

- **Copies** are distinct filesystem objects. Removing every copy but one is
  what "wasted space" measures.
- **Links** — symbolic links and hard links alike — point at an object that is
  already counted. Removing a link frees nothing, so it is listed separately and
  never contributes to the wasted-space total.
- A file and its own links are therefore **not** a duplicate group. You need two
  separate objects with the same content before anything is reported.

Paths are stored as scanned, without resolving symbolic links, so scanning
through a symlinked directory reports paths under the name you typed. That also
means the same object reached by two different routes is stored twice and
recognised as one object rather than one of the rows being dropped.

Hard-link detection relies on inode data, which is available on Unix. Elsewhere
(Windows) identity is unknown and every path counts as its own copy, so hard
links may be reported as duplicates there.

## How it works

1. **Scan** — walk the given paths and collect file metadata (path, size,
   modification time, device and inode).
2. **Filter** — for each file, check the database: if the size and
   modification time match a cached entry, reuse the stored hash; otherwise
   queue the file for hashing.
3. **Hash** — SHA-256 hash queued files in parallel.
4. **Store** — batch-insert hashes into the SQLite database.
5. **Report** — group files by hash, split each group into distinct objects and
   links to them, and list groups holding more than one object.

## Exit codes

- `0` — success
- `1` — error (e.g. a scan path does not exist, database failure)

A scan also fails if the hashes could not be written to the database. The report
is only produced from a database that actually received the results, so a
partially saved scan never looks like a clean one.

## Development

```shell
cargo test          # unit and CLI integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Contributing

Contributions are welcome! Please fork the repository and submit pull requests.
Before submitting, make sure your code passes the tests and follows the coding standards.

## License

MIT
