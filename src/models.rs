use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Debug)]
pub struct FileMetadata {
    /// The path as the user asked for it, used for progress and error messages.
    pub path: PathBuf,
    /// Absolute but not symlink-resolved, so the path printed in the report
    /// stays the one the user asked for.
    pub absolute_path: PathBuf,
    pub size: i64,
    pub mtime: i64,
    /// `(0, 0)` means unknown, as reported by platforms without inode data.
    pub dev: i64,
    pub ino: i64,
}

#[derive(Clone, Debug)]
pub struct HashedFile {
    pub absolute_path: PathBuf,
    pub size: i64,
    pub mtime: i64,
    pub hash: String,
    pub dev: i64,
    pub ino: i64,
}

/// The rule the whole program turns on: a **copy** is a distinct filesystem
/// object, and a **link** is another path reaching one already counted. Removing
/// every copy but one is what "wasted space" measures; removing a link frees
/// nothing, so links are never counted as waste. A file and its own links are
/// therefore not a duplicate group at all.
#[derive(Debug)]
pub struct Duplicates {
    pub hash: String,
    pub size: i64,
    /// One path per copy.
    pub files: Vec<String>,
    /// Paths that are links to one of `files`.
    pub aliases: Vec<String>,
}

impl Duplicates {
    /// Builds a group from `(path, identity)` pairs. The first path of an
    /// identity is a copy and every later one is a link to it.
    ///
    /// `identity` is raw bytes because it is only ever compared, and where the
    /// platform reports none it embeds the stored path, which is arbitrary bytes
    /// on Unix. Decoding it to text would panic on those, and a lossy decode
    /// would fold two paths into one key and hide a real copy.
    pub fn from_paths(hash: String, size: i64, rows: &[(String, Vec<u8>)]) -> Self {
        let mut copies = Vec::new();
        let mut aliases = Vec::new();
        let mut seen_identities = HashSet::new();

        for (path, identity) in rows {
            if seen_identities.insert(identity.clone()) {
                copies.push(path.clone());
            } else {
                aliases.push(path.clone());
            }
        }

        Duplicates {
            hash,
            size,
            files: copies,
            aliases,
        }
    }

    /// Distinct filesystem objects in the group.
    pub fn copies(&self) -> usize {
        self.files.len()
    }

    pub fn is_duplicate(&self) -> bool {
        self.files.len() > 1
    }

    /// Bytes reclaimable: every object past the first.
    ///
    /// Saturating rather than plain `*`: the size is recorded by the `stat` that
    /// scanned the file and the hash by a later read, so two rows sharing a hash
    /// need not agree on a size, and the group takes whichever SQLite picks.
    /// Plain `*` wraps there, and a release build reports negative reclaimable
    /// space and exits successfully.
    pub fn wasted_space(&self) -> i64 {
        self.size
            .saturating_mul(self.files.len().saturating_sub(1) as i64)
    }

    pub fn format_size(bytes: i64) -> String {
        const KB: i64 = 1024;
        const MB: i64 = KB * 1024;
        const GB: i64 = MB * 1024;

        if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.2} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.2} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} bytes", bytes)
        }
    }
}

/// Totals across a set of duplicate groups. Every duplicate figure the program
/// prints comes from one of these three, derived in one place so the report and
/// the statistics cannot quote different numbers.
///
/// `stats` sums the same quantities in SQL rather than through here, so the two
/// agree by construction rather than by sharing a call; `every_view_of_a_database_agrees`
/// pins that they quote the same figures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub redundant_copies: usize,
    pub linked_paths: usize,
    pub wasted_space: i64,
}

impl Totals {
    pub fn of(groups: &[Duplicates]) -> Self {
        Totals {
            // Groups reaching this point hold more than one object, but summing
            // the copies of a hypothetical single-object group must not wrap
            // around, so the filter stays.
            redundant_copies: groups
                .iter()
                .filter(|g| g.is_duplicate())
                .map(|g| g.copies() - 1)
                .sum(),
            linked_paths: groups.iter().map(|g| g.aliases.len()).sum(),
            // Folded rather than `.sum()`: each group saturates on its own, but
            // the running total is a further sum that can overflow too.
            wasted_space: groups
                .iter()
                .fold(0i64, |total, g| total.saturating_add(g.wasted_space())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_picks_a_unit() {
        assert_eq!(Duplicates::format_size(0), "0 bytes");
        assert_eq!(Duplicates::format_size(1023), "1023 bytes");
        assert_eq!(Duplicates::format_size(2048), "2.00 KB");
        assert_eq!(Duplicates::format_size(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(Duplicates::format_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    fn group(files: &[&str], aliases: &[&str]) -> Duplicates {
        Duplicates {
            hash: "abc".to_string(),
            size: 100,
            files: files.iter().map(|s| s.to_string()).collect(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn wasted_space_counts_all_but_one_copy() {
        let g = group(&["a", "b", "c"], &[]);
        assert_eq!(g.copies(), 3);
        assert_eq!(g.wasted_space(), 200);
        assert!(g.is_duplicate());
    }

    #[test]
    fn wasted_space_single_file_is_zero() {
        let g = group(&["a"], &[]);
        assert_eq!(g.wasted_space(), 0);
        assert!(!g.is_duplicate());
    }

    #[test]
    fn links_to_one_object_are_not_duplicates() {
        // Two paths, one inode: reporting this as a duplicate would claim
        // reclaimable bytes that cannot be reclaimed.
        let g = group(&["a"], &["a_link", "a_hardlink"]);
        assert_eq!(g.copies(), 1);
        assert_eq!(g.wasted_space(), 0);
        assert!(!g.is_duplicate());
    }

    #[test]
    fn links_do_not_inflate_wasted_space_of_a_real_duplicate() {
        let g = group(&["a", "b"], &["a_link"]);
        assert_eq!(g.copies(), 2);
        assert_eq!(g.wasted_space(), 100);
        assert!(g.is_duplicate());
    }

    fn rows(pairs: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
        pairs
            .iter()
            .map(|(path, identity)| (path.to_string(), identity.to_vec()))
            .collect()
    }

    #[test]
    fn from_paths_keeps_one_copy_per_identity() {
        let g = Duplicates::from_paths(
            "abc".to_string(),
            100,
            &rows(&[
                ("a", b"i:1:2"),
                ("a_link", b"i:1:2"),
                ("a_hardlink", b"i:1:2"),
                ("b", b"i:1:3"),
            ]),
        );

        assert_eq!(g.files, ["a", "b"]);
        assert_eq!(g.aliases, ["a_link", "a_hardlink"]);
        assert_eq!(g.copies(), 2);
        assert_eq!(g.wasted_space(), 100);
    }

    /// Where identity is unknown the fallback in `IDENTITY_EXPR` is the path
    /// itself, so rows must stay separate copies instead of collapsing.
    #[test]
    fn from_paths_falls_back_to_one_copy_per_path() {
        let g = Duplicates::from_paths(
            "abc".to_string(),
            100,
            &rows(&[("a", b"p:a"), ("b", b"p:b")]),
        );

        assert_eq!(g.copies(), 2);
        assert!(g.aliases.is_empty());
    }

    /// The identity travels as bytes because it is arbitrary bytes wherever the
    /// platform reports no identity. Two paths differing only in a byte that is
    /// not valid UTF-8 are two objects, and a lossy key would call the second a
    /// link to the first and under-report a real copy.
    #[test]
    fn from_paths_tells_apart_identities_that_are_not_utf8() {
        let g = Duplicates::from_paths(
            "abc".to_string(),
            100,
            &rows(&[("a", b"p:/f/a\xff"), ("b", b"p:/f/a\xfe")]),
        );

        assert_eq!(g.files, ["a", "b"]);
        assert_eq!(g.copies(), 2);
        assert_eq!(g.wasted_space(), 100);
        assert!(g.aliases.is_empty(), "{:?}", g.aliases);
    }

    #[test]
    fn totals_sum_every_group() {
        let groups = [
            group(&["a", "b", "c"], &["a_link"]),
            group(&["d", "e"], &[]),
            group(&["f"], &["f_link"]),
        ];

        assert_eq!(
            Totals::of(&groups),
            Totals {
                redundant_copies: 3,
                linked_paths: 2,
                wasted_space: 300,
            }
        );
    }

    #[test]
    fn totals_of_no_groups_is_zero() {
        assert_eq!(Totals::of(&[]), Totals::default());
    }

    /// A group can be sized by a row whose stored size disagrees with the rest, so
    /// plain `*` overflows: a debug build panics and a release build reports
    /// negative reclaimable space and exits successfully.
    #[test]
    fn wasted_space_saturates_instead_of_wrapping() {
        let g = Duplicates {
            hash: "abc".to_string(),
            size: 9_000_000_000_000_000_000,
            files: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            aliases: vec![],
        };
        assert_eq!(g.wasted_space(), i64::MAX);
        assert!(g.wasted_space() > 0, "never a negative reclaimable figure");
    }

    /// The overflow is also reachable across groups rather than within one, which
    /// is why the running total is folded with a saturating add.
    #[test]
    fn totals_saturate_across_groups() {
        let big = |n: usize| Duplicates {
            hash: "abc".to_string(),
            size: 4_000_000_000_000_000_000,
            files: vec!["a".to_string(); n],
            aliases: vec![],
        };
        // 3 x 8e18 = 2.4e19, well past i64::MAX.
        let totals = Totals::of(&[big(3), big(3), big(3)]);
        assert_eq!(totals.wasted_space, i64::MAX);
        assert_eq!(totals.redundant_copies, 6);

        // Groups that individually fit must still total exactly.
        let exact = Totals::of(&[big(2), big(2)]);
        assert_eq!(exact.wasted_space, 8_000_000_000_000_000_000);
    }

    /// An empty group must reclaim nothing rather than report a negative size,
    /// which is what a plain `len() - 1` would give.
    #[test]
    fn wasted_space_is_zero_for_an_empty_group() {
        let g = Duplicates {
            hash: "abc".to_string(),
            size: 100,
            files: vec![],
            aliases: vec![],
        };
        assert_eq!(g.wasted_space(), 0);
    }
}
