//! Rendering of the duplicate report.
//!
//! Separate from `models` so that the data model stays free of the CLI and
//! serialization concerns: `Format` is a `clap` value enum and `Report` is a
//! `serde` shape, and neither belongs in the module describing what a scanned
//! file is.

use clap::ValueEnum;
use serde::Serialize;

use crate::models::{Duplicates, Totals};

/// Shape the report is written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Human-readable
    #[default]
    Text,
    /// Machine-readable
    Json,
}

/// A finished report, ready to be rendered.
///
/// These are their own structs rather than derives on `Duplicates`: the JSON is
/// a contract with whatever consumes it and must not follow along silently when
/// the internal model is reshaped.
#[derive(Debug, Serialize)]
pub struct Report {
    pub generated: String,
    pub duplicate_groups: usize,
    pub redundant_copies: usize,
    pub linked_paths: usize,
    /// Raw bytes. The text report rounds these; a JSON consumer should not have
    /// to parse "1.00 MB" back into a number.
    pub wasted_space: i64,
    pub groups: Vec<GroupReport>,
}

#[derive(Debug, Serialize)]
pub struct GroupReport {
    pub hash: String,
    pub size: i64,
    pub copies: usize,
    pub wasted_space: i64,
    /// One path per copy.
    pub files: Vec<String>,
    /// Paths that are links to one of `files`.
    pub links: Vec<String>,
}

impl Report {
    pub fn new(groups: &[Duplicates]) -> Self {
        let totals = Totals::of(groups);

        Report {
            generated: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            duplicate_groups: groups.len(),
            redundant_copies: totals.redundant_copies,
            linked_paths: totals.linked_paths,
            wasted_space: totals.wasted_space,
            groups: groups
                .iter()
                .map(|g| GroupReport {
                    hash: g.hash.clone(),
                    size: g.size,
                    copies: g.copies(),
                    wasted_space: g.wasted_space(),
                    files: g.files.clone(),
                    links: g.aliases.clone(),
                })
                .collect(),
        }
    }

    /// `serde_json` handles escaping: a path may contain quotes, backslashes or
    /// control characters, and hand-assembling JSON would produce something a
    /// parser rejects.
    pub fn render(&self, format: Format) -> Result<String, serde_json::Error> {
        if format == Format::Json {
            return serde_json::to_string_pretty(self);
        }

        let mut lines = vec![
            "=== DUPLICATE FILES REPORT ===".to_string(),
            format!("Generated: {}", self.generated),
            format!("Total duplicate files: {}", self.redundant_copies),
            format!(
                "Wasted space: {}",
                Duplicates::format_size(self.wasted_space)
            ),
            format!("Duplicate groups: {}", self.duplicate_groups),
        ];

        if self.linked_paths > 0 {
            lines.push(format!(
                "Linked paths (share storage, reclaiming none): {}",
                self.linked_paths
            ));
        }
        lines.push(String::new());

        for (idx, group) in self.groups.iter().enumerate() {
            lines.push(format!("--- Group {} ---", idx + 1));
            lines.push(format!("Hash: {}", group.hash));
            lines.push(format!("Size: {}", Duplicates::format_size(group.size)));
            lines.push(format!("Copies: {}", group.copies));
            lines.push(format!(
                "Wasted: {}",
                Duplicates::format_size(group.wasted_space)
            ));
            lines.push("Files:".to_string());
            lines.extend(group.files.iter().map(|path| format!("  - {}", path)));

            if !group.links.is_empty() {
                lines.push("Links to the above (not extra copies):".to_string());
                lines.extend(group.links.iter().map(|path| format!("  - {}", path)));
            }

            lines.push(String::new());
        }

        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Duplicates;

    fn group(files: &[&str], aliases: &[&str]) -> Duplicates {
        Duplicates {
            hash: "abc".to_string(),
            size: 100,
            files: files.iter().map(|s| s.to_string()).collect(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn json_report_carries_links_and_copies_separately() {
        let groups = [group(&["a", "b"], &["a_link"])];
        let json: serde_json::Value =
            serde_json::from_str(&Report::new(&groups).render(Format::Json).unwrap()).unwrap();

        // The link is reported, but nowhere near the wasted-space figure: a
        // consumer must not read it as a reclaimable copy.
        assert_eq!(json["duplicate_groups"], 1);
        assert_eq!(json["redundant_copies"], 1);
        assert_eq!(json["linked_paths"], 1);
        // Raw bytes, not the text report's rounded "100 bytes".
        assert_eq!(json["wasted_space"], 100);
        assert_eq!(json["groups"][0]["copies"], 2);
        assert_eq!(json["groups"][0]["links"][0], "a_link");
    }

    /// The JSON reports raw bytes so nothing downstream has to parse "1.00 MB"
    /// back into a number.
    #[test]
    fn json_report_sizes_are_numbers_not_rounded_strings() {
        let groups = [Duplicates {
            hash: "abc".to_string(),
            size: 5 * 1024 * 1024,
            files: vec!["a".to_string(), "b".to_string()],
            aliases: vec![],
        }];
        let json: serde_json::Value =
            serde_json::from_str(&Report::new(&groups).render(Format::Json).unwrap()).unwrap();

        assert_eq!(json["wasted_space"], 5 * 1024 * 1024);
        assert!(json["wasted_space"].is_i64());
    }

    #[test]
    fn text_report_renders_every_group() {
        let groups = [group(&["a", "b"], &["a_link"])];
        let text = Report::new(&groups).render(Format::Text).unwrap();

        assert!(text.contains("=== DUPLICATE FILES REPORT ==="), "{text}");
        assert!(text.contains("Total duplicate files: 1"), "{text}");
        assert!(text.contains("Wasted space: 100 bytes"), "{text}");
        assert!(text.contains("Duplicate groups: 1"), "{text}");
        assert!(text.contains("Copies: 2"), "{text}");
        assert!(
            text.contains("Links to the above (not extra copies):"),
            "{text}"
        );
        assert!(text.contains("  - a_link"), "{text}");
    }

    #[test]
    fn empty_report_renders_in_both_formats() {
        let text = Report::new(&[]).render(Format::Text).unwrap();
        assert!(text.contains("Duplicate groups: 0"), "{text}");
        assert!(text.contains("Total duplicate files: 0"), "{text}");
        assert!(
            !text.contains("Linked paths"),
            "a report with no links should not mention them: {text}"
        );

        let json: serde_json::Value =
            serde_json::from_str(&Report::new(&[]).render(Format::Json).unwrap()).unwrap();
        assert_eq!(json["duplicate_groups"], 0);
        assert_eq!(json["groups"].as_array().unwrap().len(), 0);
    }
}
