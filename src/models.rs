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
