use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Album,
    Track,
    Cover,
}

pub enum Outcome {
    Downloaded,
    Existing,
    Unavailable,
    Failed(String),
}

struct Entry {
    group: Option<PathBuf>,
    name: String,
    url: String,
    detail: String,
}

#[derive(Default)]
struct Report {
    downloaded: usize,
    existing: usize,
    failed: Vec<Entry>,
    unavailable: Vec<Entry>,
    warnings: Vec<Entry>,
}
/// File-name prefixes of the files saved by [`finish`].
pub const SAVED_FILE_PREFIXES: [&str; 2] = ["lucida-report-", "lucida-failed-"];

static REPORT: Mutex<Report> = Mutex::new(Report {
    downloaded: 0,
    existing: 0,
    failed: Vec::new(),
    unavailable: Vec::new(),
    warnings: Vec::new(),
});

fn report() -> std::sync::MutexGuard<'static, Report> {
    REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records its outcome when dropped, so cancelled or panicking work is still reported.
pub struct Item {
    url: String,
    kind: Kind,
    group: Option<PathBuf>,
    name: String,
    outcome: Outcome,
}
impl Item {
    pub fn new(url: &str, kind: Kind) -> Self {
        Self {
            url: url.into(),
            kind,
            group: None,
            name: url.into(),
            outcome: Outcome::Failed("interrupted".into()),
        }
    }
    /// Names the item by its album directory and display name in the final report.
    pub fn describe(&mut self, group: &Path, name: impl Into<String>) {
        self.group = Some(group.to_owned());
        self.name = name.into();
    }
    pub fn finish(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
    pub fn fail(&mut self, reason: impl Into<String>) {
        self.outcome = Outcome::Failed(reason.into());
    }
}
impl Drop for Item {
    fn drop(&mut self) {
        let outcome = std::mem::replace(&mut self.outcome, Outcome::Downloaded);
        let track = self.kind == Kind::Track;
        let entry = Entry {
            group: self.group.take(),
            name: std::mem::take(&mut self.name),
            url: std::mem::take(&mut self.url),
            detail: String::new(),
        };
        let mut report = report();
        match outcome {
            Outcome::Downloaded if track => report.downloaded += 1,
            Outcome::Existing if track => report.existing += 1,
            Outcome::Downloaded | Outcome::Existing => (),
            Outcome::Unavailable => report.unavailable.push(Entry {
                detail: "not available for download".into(),
                ..entry
            }),
            Outcome::Failed(reason) => {
                tracing::error!("failed: {}: {reason}", entry.name);
                report.failed.push(Entry {
                    detail: reason,
                    ..entry
                });
            }
        }
    }
}

/// Notes a completed item that needs attention, such as a track left unnumbered.
pub fn warn(group: &Path, name: &str, message: impl Into<String>) {
    report().warnings.push(Entry {
        group: Some(group.to_owned()),
        name: name.into(),
        url: String::new(),
        detail: message.into(),
    });
}

fn write_section(text: &mut String, title: &str, entries: &mut [Entry], output: &Path) {
    if entries.is_empty() {
        return;
    }
    entries.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
    let _ = writeln!(text, "\n{title} ({}):", entries.len());
    let mut current = None;
    for entry in entries.iter() {
        if current != Some(&entry.group) {
            current = Some(&entry.group);
            match &entry.group {
                Some(group) => {
                    let _ = writeln!(
                        text,
                        "  {}",
                        group.strip_prefix(output).unwrap_or(group).display()
                    );
                }
                None => {
                    let _ = writeln!(text, "  (album not resolved)");
                }
            }
        }
        let _ = write!(text, "    {}: {}", entry.name, entry.detail);
        if !entry.url.is_empty() && entry.url != entry.name {
            let _ = write!(text, " <{}>", entry.url);
        }
        text.push('\n');
    }
}

/// Prints the end-of-run report and saves it, plus a `-f` retry list, when anything needs attention.
pub fn finish(output: &Path) -> bool {
    let mut report = std::mem::take(&mut *report());
    let mut text = format!(
        "Tracks: {} downloaded, {} already present, {} unavailable; {} failed items\n",
        report.downloaded,
        report.existing,
        report.unavailable.len(),
        report.failed.len()
    );
    write_section(&mut text, "Failed", &mut report.failed, output);
    write_section(
        &mut text,
        "Unavailable on lucida",
        &mut report.unavailable,
        output,
    );
    write_section(
        &mut text,
        "Downloaded with warnings",
        &mut report.warnings,
        output,
    );
    eprint!("\n{text}");

    if report.failed.is_empty() && report.unavailable.is_empty() && report.warnings.is_empty() {
        return true;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let report_path = output.join(format!("{}{stamp}.txt", SAVED_FILE_PREFIXES[0]));
    match std::fs::write(&report_path, &text) {
        Ok(()) => tracing::info!("Report saved to {}", report_path.display()),
        Err(error) => tracing::error!("Could not save report: {error}"),
    }
    if report.failed.is_empty() {
        return true;
    }
    let mut urls: Vec<&str> = report
        .failed
        .iter()
        .map(|entry| entry.url.as_str())
        .collect();
    urls.sort_unstable();
    urls.dedup();
    let retry_path = output.join(format!("{}{stamp}.txt", SAVED_FILE_PREFIXES[1]));
    match std::fs::write(&retry_path, urls.join("\n") + "\n") {
        Ok(()) => tracing::info!("Retry URLs saved to {} (use -f)", retry_path.display()),
        Err(error) => tracing::error!("Could not save retry URLs: {error}"),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_failures_by_album_relative_to_output() {
        let output = Path::new("/music");
        let entry = |group: Option<&str>, name: &str, url: &str, detail: &str| Entry {
            group: group.map(PathBuf::from),
            name: name.into(),
            url: url.into(),
            detail: detail.into(),
        };
        let mut entries = vec![
            entry(
                Some("/music/Artist/Album"),
                "02. Artist - B",
                "https://t/2",
                "lucida error: x",
            ),
            entry(
                None,
                "https://a/1",
                "https://a/1",
                "lucida returned HTTP 404",
            ),
            entry(
                Some("/music/Artist/Album"),
                "cover.jpg",
                "https://a/2",
                "interrupted",
            ),
        ];
        let mut text = String::new();
        write_section(&mut text, "Failed", &mut entries, output);
        assert_eq!(
            text,
            "\nFailed (3):\n  (album not resolved)\n    https://a/1: lucida returned HTTP 404\n  Artist/Album\n    02. Artist - B: lucida error: x <https://t/2>\n    cover.jpg: interrupted <https://a/2>\n"
        );
    }
}
