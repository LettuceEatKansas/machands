//! The local action log: one JSON line per action in `~/Library/Logs/MacHands/browser/<date>.jsonl`,
//! files older than 14 days removed. Page content and typed text are never written, only what was
//! done: action, ref, host, outcome. Nothing leaves the machine.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Files older than this many days are deleted.
pub const KEEP_DAYS: u64 = 14;

/// The log directory. `MACHANDS_LOG_DIR` overrides it (tests).
pub fn default_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("MACHANDS_LOG_DIR") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Logs/MacHands/browser")
    } else {
        home.join(".local/state/machands/browser")
    })
}

/// `YYYY-MM-DD` for a Unix time in seconds (UTC), no date crate.
pub fn date_string(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// An append-only log in one directory.
pub struct ActionLog {
    dir: Option<PathBuf>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl ActionLog {
    /// A log in `dir`; `None` logs nothing. Prunes old files straight away.
    pub fn new(dir: Option<PathBuf>) -> Self {
        let log = Self { dir };
        log.prune(now_secs());
        log
    }

    /// Append one entry. Never fails the action: a log that cannot be written is skipped.
    pub fn write(&self, action: &str, detail: Value, ok: bool) {
        let Some(dir) = &self.dir else { return };
        let _ = std::fs::create_dir_all(dir);
        let ts = now_secs();
        let line = json!({"ts": ts, "action": action, "detail": detail, "ok": ok});
        let path = dir.join(format!("{}.jsonl", date_string(ts)));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Delete day files older than [`KEEP_DAYS`] relative to `now`.
    pub fn prune(&self, now: u64) {
        let Some(dir) = &self.dir else { return };
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let cutoff = date_string(now.saturating_sub(KEEP_DAYS * 86_400));
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(day) = name.strip_suffix(".jsonl")
                && day.len() == 10
                && day < cutoff.as_str()
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// The directory, if logging is on.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_come_out_right() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(1_791_460_800), "2026-10-08");
        assert_eq!(date_string(951_782_400), "2000-02-29");
    }

    #[test]
    fn entries_land_in_the_day_file_and_old_files_go() {
        let dir = std::env::temp_dir().join(format!("wh-browser-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2020-01-01.jsonl"), "old\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "keep\n").unwrap();
        let log = ActionLog::new(Some(dir.clone()));
        log.write("browser_click", json!({"ref": "e3"}), true);
        assert!(!dir.join("2020-01-01.jsonl").exists());
        assert!(dir.join("notes.txt").exists());
        let today = dir.join(format!("{}.jsonl", date_string(now_secs())));
        let text = std::fs::read_to_string(today).unwrap();
        assert!(text.contains("browser_click") && text.contains("e3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_without_a_directory_does_nothing() {
        ActionLog::new(None).write("x", json!({}), true);
    }
}
