# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.1] — 2026-10-22

### Fixed

- `scan --prune` no longer keeps entries for files deleted from a filesystem
  whose device number changed since they were recorded. Device numbers are
  assigned per boot on Unix and move under a volume — a device-mapper major is
  renumbered across reboots — so every entry recorded before the change looked
  like an unmounted volume and could never be pruned: the files were gone, the
  number no longer matched their surviving ancestor, and the report kept naming
  a deleted tree. On Unix a missing file with a recorded device is now pruned.
  Windows keeps the unmount check, where the recorded number is a volume serial
  that survives a remount.

## [0.9.0] — 2026-10-20

### Added

- `scan` with no paths now scans the current directory, instead of erroring out
  as a missing argument.
- Paths are stored as bytes rather than text. Paths on Unix are arbitrary bytes,
  but the column was TEXT, so every write went through a lossy conversion that
  mapped `a\xff.txt` and `a\xfe.txt` to the same stored string — and since the
  column is UNIQUE, the second file silently replaced the first. The stored path
  also no longer named anything on disk, so prune's existence check saw a missing
  file and deleted the entry on every run. Migration 0003 converts the stored
  rows in place; the column stays TEXT, which leaves blobs alone.
- The report can no longer be written over the database. `--output-file` naming
  the same path as `--db` is refused rather than silently replacing every hash
  with a text file. The refusal resolves both paths before comparing them, so an
  `--output-file` that is a symlink to the database, or that reaches it through a
  symlinked directory, as on macOS where `/var` is a link to `/private/var`, is
  caught too, along with the database's `-wal` and `-shm` side files.
- The database and its `-wal`/`-shm` side files are excluded from the scan.
  Pointing `--db` inside a scanned directory would otherwise have the tool
  record its own database: three files that change on every scan, so re-hashed
  every time and counted as content nobody asked about.
- `scan --prune` deletes database entries for scanned files that no longer
  exist. It only removes an entry when the filesystem confirms the file is gone,
  never because a scan did not happen to see it, so an unreadable directory or
  an interrupted run cannot be mistaken for a mass deletion. An unmounted volume
  is detected by comparing the filesystem recorded for the entry against the one
  its nearest surviving ancestor is now on. Windows records which volume a file
  is on, so the check works there too; entries stored by 0.8 and earlier there
  predate that, and are never pruned until they are rehashed.
- `-o`/`--output` accepts `json`, for consuming the report from another program.
- `SIGTERM` now stops a scan gracefully, as Ctrl+C already did. `kill`, systemd,
  Docker and Kubernetes all stop a process with `SIGTERM` — it is the polite
  request, and only Ctrl+C was handled, so a scan under a process manager died on
  the spot with nothing said and nothing to tell it from a crash. Either signal
  now finishes the current stage, writes no report and exits `130`; a second of
  either kind exits immediately, for a stage stuck in a query.

### Changed

- **`--output` is now the report format, not the output path.** It takes `text`
  or `json` and defaults to `text`. The path moved to `-O`/`--output-file`, and
  the report goes to stdout when no file is given.

  ```shell
  sdoppia scan ~/Documents --output duplicates.txt   # before
  sdoppia scan ~/Documents --output-file duplicates.txt   # now
  ```

- **`clear` asks for confirmation.** It refuses to run when stdin is not a
  terminal rather than treating an unanswered prompt as consent. Scripts and
  cron jobs need `--yes`.

- `stats` counts duplicates in a single aggregate query instead of building the
  whole report in memory. Its `Duplicate percentage` now excludes links from the
  denominator, so the figure reflects distinct objects rather than paths: ten
  names over five files are five objects, and counting the extra names would
  report the tree as half duplicate when one copy of one file is redundant.

- The `--db` option is global: it may now be given before or after the
  subcommand.

- Prune scoping is a byte range served by the UNIQUE index, walked one root at a
  time and paginated by `path`: no `LIKE`, no wildcard escaping, no collation,
  and it cannot disagree with the Rust-side check about `/a/b` versus `/a/bc`.
  Paginating by `id` instead would order by the rowid index and turn the range
  back into a filter over the whole table. The range is byte-exact as a result,
  so a root typed in a different case than it was scanned matches nothing; see
  Design trade-offs. `--prune` now says what it removed either way, so removing
  nothing is distinguishable from not having run.

- A scan reports the paths it could not read. An unreadable directory used to be
  dropped silently, making a partial scan indistinguishable from a complete one.

- Progress and status messages go to stderr, so stdout carries nothing but the
  report and `--output json` can be piped. A message raised while the bars are
  live is written through them rather than over their rows; once they are
  finished the line below them is written directly. `--prune` always reports
  what it removed, including nothing, so that removing no entries is
  distinguishable from not having run.

### Removed

- `-v`/`--verbose` and the `tracing` stack. Verbose mode logged every file as it
  was processed and hashed, so it printed tens of thousands of lines for an
  ordinary scan and told you nothing the progress bars were not already showing.

### Fixed

- Windows now identifies files by their file index as well as their volume, so
  hard links there are recognized instead of being reported as separate copies,
  and a directory scanned under two spellings of its name no longer inflates the
  wasted-space figure. `path_device` already opened a handle and read a
  `BY_HANDLE_FILE_INFORMATION` to get the volume serial number, so the index came
  from the same call rather than a second open. A filesystem that reports no
  index keeps the old path-based behaviour, which is the safe direction: a
  varying index would read as a different file and invent duplicates.
- The minimum supported Rust version is declared as 1.85, the floor edition 2024
  already implies. Without it, anyone building on an older compiler gets a
  language-edition error instead of being told what is required.
- The report is no longer mangled by the progress display. Its first line was
  swallowed by the export spinner, the hashing bar was never finished and kept
  redrawing over the report after it had been printed, and status lines were cut
  off mid-path with a bar row in their place. The bars and the messages now
  share one region, so a redraw can no longer overwrite text that was already on
  screen.
- `--prune` no longer leaves a stale copy of a progress bar on screen.
- The report and `stats` count copies, links and wasted space from one shared
  definition, so they can no longer quote different numbers for the same database.
- The index named `idx_dev` is renamed `idx_size_mtime`, which is the columns it
  actually covers.
- A scan path that does not exist is reported as `Invalid path` rather than
  `Invalid directory path`, which was wrong for a file argument.

## [0.8.0]

### Added

- Database migrations via sqlx.
- Graceful shutdown on Ctrl+C for scanning and reporting.

### Fixed

- File identity is separated from path, so hard and symbolic links are no longer
  reported as reclaimable waste.