-- Renames the index that `0001` created as `idx_dev`.
--
-- It was never a `dev` index: it covers `(size, mtime)`, which is what
-- `filter_files` actually queries when deciding whether a cached hash can be
-- reused. The name was simply wrong, and a search for "why is there an index on
-- dev" turns up a table with no `dev`-related index behind it.
--
-- Applied migrations are checksummed by sqlx, so this cannot be fixed by
-- editing `0001` in place: that would make every existing database fail with a
-- checksum mismatch.
DROP INDEX idx_dev;

CREATE INDEX idx_size_mtime ON hashes(size, mtime);