use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct FileMetadata {
    pub path: PathBuf,
    pub absolute_path: PathBuf,
    pub size: i64,
    pub mtime: i64,
}

#[derive(Clone, Debug)]
pub struct HashedFile {
    pub absolute_path: PathBuf,
    pub size: i64,
    pub mtime: i64,
    pub hash: String,
}

#[derive(Debug)]
pub struct Duplicates {
    pub hash: String,
    pub size: i64,
    pub files: Vec<String>,
}

impl Duplicates {
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

    #[test]
    fn wasted_space_counts_all_but_one_copy() {
        let group = Duplicates {
            hash: "abc".to_string(),
            size: 100,
            files: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        };
        assert_eq!(group.wasted_space(), 200);
    }

    #[test]
    fn wasted_space_single_file_is_zero() {
        let group = Duplicates {
            hash: "abc".to_string(),
            size: 100,
            files: vec!["a".to_string()],
        };
        assert_eq!(group.wasted_space(), 0);
    }
}
