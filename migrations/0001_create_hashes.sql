-- The schema, versioned from here on.
--
-- `dev` and `ino` identify the filesystem object behind a path, which is how a
-- genuine duplicate is told apart from a hard or symbolic link (see
-- export_duplicates). They are 0 where the platform does not expose inode data,
-- which read as "unknown identity" and fall back to counting each path as its
-- own copy.
CREATE TABLE hashes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    dev INTEGER NOT NULL DEFAULT 0,
    ino INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_hash ON hashes(hash);
CREATE INDEX idx_size ON hashes(size);
CREATE INDEX idx_mtime ON hashes(mtime);
CREATE INDEX idx_dev ON hashes(size, mtime);
