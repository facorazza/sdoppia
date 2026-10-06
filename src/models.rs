use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct FileMetadata {
    /// The path as the user asked for it, used for progress and error messages.
    pub path: PathBuf,
    /// The same path made absolute but *not* symlink-resolved: this is the
    /// database key and the path printed in the report, so it must stay
    /// recognisable to the user instead of collapsing to some canonical target.
    pub absolute_path: PathBuf,
    pub size: i64,
    pub mtime: i64,
    /// Filesystem object identity, `(device, inode)`. Two paths with the same
    /// identity are the same bytes on disk reached twice, i.e. a symbolic or
    /// hard link, not a duplicate worth reclaiming space from. `(0, 0)` means
    /// unknown, which is what platforms without inode data report.
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

#[derive(Debug)]
pub struct Duplicates {
    pub hash: String,
    pub size: i64,
    /// One path per distinct filesystem object in the group.
    pub files: Vec<String>,
    /// Extra paths that are links to one of `files`, so deleting them reclaims
    /// nothing. Kept separate so the report never presents them as waste.
    pub aliases: Vec<String>,
}

impl Duplicates {
    /// Distinct filesystem objects in the group. Hard and symbolic links share
    /// an inode and therefore count once.
    pub fn copies(&self) -> usize {
        self.files.len()
    }

    pub fn is_duplicate(&self) -> bool {
        self.files.len() > 1
    }

    /// Bytes genuinely reclaimable: every object past the first. Links to an
    /// object already counted contribute nothing, because removing a link does
    /// not free the data it points at.
    pub fn wasted_space(&self) -> i64 {
        self.size * (self.files.len() as i64 - 1)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_bytes() {
        assert_eq!(Duplicates::format_size(0), "0 bytes");
        assert_eq!(Duplicates::format_size(1023), "1023 bytes");
    }

    #[test]
    fn format_size_kb() {
        assert_eq!(Duplicates::format_size(1024), "1.00 KB");
        assert_eq!(Duplicates::format_size(2048), "2.00 KB");
    }

    #[test]
    fn format_size_mb() {
        assert_eq!(Duplicates::format_size(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn format_size_gb() {
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
}
