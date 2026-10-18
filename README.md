# sdoppia

A CLI tool to scan directories, hash files, and find duplicate files.

`sdoppia` walks one or more directories, SHA-256
hashes every non-empty file, stores the results in a local SQLite database, and
reports duplicate groups with the space they waste. On rescan, unchanged files
(same size and modification time) are skipped using cached hashes, so repeat
scans are fast.

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
  help   Print this message or the help of the given subcommand(s)

Options:
  -d, --db <DB>  Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
  -h, --help     Print help (see more with '--help')
  -V, --version  Print version
```

`--db` is global: it may be given before or after the subcommand, and every
subcommand reads the same database.

### Scan

```shell
sdoppia scan [OPTIONS] [PATHS]...

Arguments:
  [PATHS]...  Directories to scan (can specify multiple, defaults to the current directory) [default: .]

Options:
  -L, --follow-links               Follow symbolic links
  -r, --rehash                     Force rehashing of files already in database
  -o, --output <OUTPUT>            Output format for the duplicate report: text to read, json to parse [default: text] [possible values: text, json]
  -d, --db <DB>                    Database file path (default: per-OS data directory, e.g. ~/.local/share/sdoppia/sdoppia.db)
  -O, --output-file <OUTPUT_FILE>  Write the report to this file instead of stdout
  -m, --min-size <MIN_SIZE>        Minimum file size in bytes to include in export [default: 0]
  -p, --prune                      Also delete entries for scanned files that no longer exist
  -h, --help                       Print help (see more with '--help')
```

`--output` chooses the shape of the report and `--output-file` chooses where it
goes, so the two are independent: `-o json` writes JSON to stdout, and
`-o json -O report.json` writes it to a file.

The database lives in the per-OS data directory by default:
`~/.local/share/sdoppia/sdoppia.db` on Linux,
`~/Library/Application Support/sdoppia/sdoppia.db` on macOS, and
`%LOCALAPPDATA%\sdoppia\sdoppia.db` on Windows. Pass `--db` to use a custom
location.

One database serves every scan, from every project, which is why it is kept out
of the tree: a database sitting in a scanned directory would be scan input,
change on every run, and be reported back as content. Pointing `--db` inside a
scanned directory is therefore still a poor idea, and the database and its
`-wal`/`-shm` side files are excluded from the walk when it happens anyway.

## Examples

Scan a directory and print the duplicate report to stdout:

```shell
sdoppia scan ~/Documents
```

With no paths, `scan` scans the current directory:

```shell
cd ~/Documents && sdoppia scan
```

Scan multiple paths and write the report to a file:

```shell
sdoppia scan ~/Documents ~/Downloads --output-file duplicates.txt
```

Write the report as JSON, for another program to read:

```shell
sdoppia scan ~/Documents --output json --output-file duplicates.json
```

Or to stdout, so it can be piped:

```shell
sdoppia scan ~/Documents --output json | jq '.groups[].files[]'
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

Drop database entries for files that have been deleted since the last scan:

```shell
sdoppia scan ~/Documents --prune
```

Show database statistics:

```shell
sdoppia stats
```

Clear all entries from the database:

```shell
sdoppia clear
```

`clear` asks before deleting anything. Because a prompt nobody can answer must
not be read as consent, it refuses to run when stdin is not a terminal — pass
`--yes` to clear it from a script or a cron job.

## Example report

With `--output text` (the default):

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

With `--output json`:

```json
{
  "generated": "2026-08-19 21:30:00",
  "duplicate_groups": 1,
  "redundant_copies": 1,
  "linked_paths": 0,
  "wasted_space": 12,
  "groups": [
    {
      "hash": "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447",
      "size": 12,
      "copies": 2,
      "wasted_space": 12,
      "files": ["/home/user/docs/a.txt", "/home/user/docs/b.txt"],
      "links": []
    }
  ]
}
```

Sizes are plain byte counts in the JSON, never the rounded-up `12 bytes` the
text report prints, so nothing downstream has to parse a formatted string back
into a number.

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
recognized as one object rather than one of the rows being dropped.

Hard-link detection relies on inode data. Unix exposes it through `std`, and on
Windows the volume serial number and file index are read from a
`BY_HANDLE_FILE_INFORMATION`. Where the filesystem reports no index — FAT and
exFAT synthesize one, and a synthesized value can move — identity falls back to
the path, and hard links on such a volume may be reported as duplicates.

## How it works

1. **Scan** — walk the given paths and collect file metadata (path, size,
   modification time, device and inode). Empty files are skipped: they are all
   duplicates of each other, and hashing them would flood the report with noise.
2. **Filter** — for each file, check the database: if the size and
   modification time match a cached entry, reuse the stored hash; otherwise
   queue the file for hashing.
3. **Hash** — SHA-256 hash queued files in parallel.
4. **Store** — batch-insert hashes into the SQLite database.
5. **Report** — group files by hash, split each group into distinct objects and
   links to them, and list groups holding more than one object.

## Removing stale entries

A file deleted from disk keeps its database entry, and an entry is still
reported as a duplicate until something removes it. `--prune` is that something:

```shell
sdoppia scan ~/Documents --prune
```

It only considers the paths you asked to scan, and only removes an entry when
the filesystem confirms the file is no longer there — never merely because this
particular scan did not see it. That distinction matters: a scan cannot always
reach every file under a root, and an unreadable directory, a transient I/O
error or a Ctrl+C must not be mistaken for a mass deletion. The prune runs
before the report is built, so the report describes what is on disk now.

With the flag given, the prune always says what it did — `Removed N entries for
files that no longer exist`, or `Nothing to prune: every stored path still
exists` when there was nothing to remove, so removing no entries is never
mistaken for not having run. The line goes to stderr, under the progress bars
and above the report.

An unmounted volume is the interesting case. Its mount point directory stays on
the parent filesystem, so every path below it looks deleted while the data is
merely unreachable. To tell that apart from a real deletion, the prune compares
the filesystem recorded for the entry against the one its nearest surviving
ancestor is now on; when they differ, the volume is not mounted and the entry is
kept until you scan it again.

Where a platform cannot report which filesystem a file is on, entries are never
pruned, since there would be no way to tell a deletion from an unmount. That
also covers entries recorded by sdoppia 0.8 and earlier on Windows, where the
volume was not stored: they keep their old value because an unchanged file is
not rewritten by a rescan. Re-hashing repopulates them:

```shell
sdoppia scan ~/Documents --prune --rehash
```

## Design trade-offs

Three behaviors are deliberate. Each costs something, and none of them loses
data.

A scan reports the paths it could not read and carries on rather than failing:
one unreadable directory should not abandon a scan of everything else. Entries
beneath it are left alone rather than pruned, since "could not read" is not the
same as "gone".

Paths are compared byte for byte. That is what keeps filenames SQLite would
otherwise treat as text distinguishable, and it lets `--prune` scope a root with
an indexed range instead of a `LIKE`. The cost is that spelling matters:
`~/DOCUMENTS` and `~/Documents` are one directory on macOS but two paths, so
scanning under both stores both. Where the filesystem reports an identity for
the file, both rows carry the same one and the second is recognized as a link
to a file already counted, so only the file count is inflated. Where none is
available, the path is all there is to go on and the two spellings count as
separate files, which inflates copies and wasted space with waste that cannot be
reclaimed.

The same byte-exactness is why a root typed in a different case than it was
scanned matches nothing. Scanning `~/Docs` and then pruning `~/DOCS` reports
`Nothing to prune` and leaves the deleted file's row in place, because a stale
entry is a smaller problem than deleting something that is still there.

Rescanning from one consistent spelling avoids the second and third.

## Exit codes

- `0` — success, including a `clear` that was declined at the prompt
- `1` — error (e.g. a scan path does not exist, database failure, a `clear` that
  could not be confirmed)
- `130` — a scan was interrupted

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
