//! Past transcripts, kept on disk so they survive a restart.
//!
//! Stored as JSON Lines under the XDG state directory — that's where the spec
//! puts history files, as opposed to the data directory where the model lives.
//! One object per line means a single corrupt entry costs you that entry rather
//! than the whole file.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single transcription, as it is written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Unix microseconds when the recording was taken; also the identity used
    /// by the UI, since two recordings can't start in the same microsecond.
    pub id: i64,
    pub text: String,
    /// Seconds of audio that produced this text.
    pub duration_secs: f32,
}

impl Entry {
    /// Unix seconds, for display.
    pub fn recorded_at(&self) -> i64 {
        self.id / 1_000_000
    }

    /// The first line, for the row title. Long transcripts are left intact —
    /// the row ellipsizes them rather than the data being truncated.
    pub fn summary(&self) -> &str {
        self.text.lines().next().unwrap_or("").trim()
    }

    pub fn duration_label(&self) -> String {
        mm_ss(self.duration_secs.round() as u32)
    }
}

pub struct History {
    path: PathBuf,
    /// Newest first, which is the order the list shows them in.
    entries: Vec<Entry>,
    limit: usize,
}

impl History {
    /// Read the history file. A missing or damaged file is not fatal: whatever
    /// can be parsed is kept, so a crash mid-write can't lock you out of your
    /// own transcripts.
    pub fn load(limit: usize) -> Self {
        Self::load_at(history_path(), limit)
    }

    /// As [`Self::load`], but against a given file. Tests use this; the app
    /// uses the XDG location.
    pub fn load_at(path: PathBuf, limit: usize) -> Self {
        let mut entries = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| {
                    serde_json::from_str::<Entry>(line)
                        .inspect_err(|err| {
                            eprintln!("yapper: skipping unreadable history entry: {err}")
                        })
                        .ok()
                })
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                eprintln!("yapper: could not read {}: {err}", path.display());
                Vec::new()
            }
        };

        entries.sort_by_key(|entry: &Entry| std::cmp::Reverse(entry.id));
        entries.truncate(limit);

        Self {
            path,
            entries,
            limit,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Record a new transcript and return its id.
    pub fn add(&mut self, text: String, duration_secs: f32) -> Result<i64> {
        let id = unique_id(self.entries.first().map(|entry| entry.id));
        self.entries.insert(
            0,
            Entry {
                id,
                text,
                duration_secs,
            },
        );
        self.entries.truncate(self.limit);
        self.save()?;
        Ok(id)
    }

    /// Returns whether anything was actually removed.
    pub fn remove(&mut self, id: i64) -> Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// Written to a neighbouring temporary file and renamed, so an interrupted
    /// save leaves the previous history intact rather than a half-written one.
    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }

        let mut buffer = String::new();
        // Oldest first on disk, so the file reads chronologically.
        for entry in self.entries.iter().rev() {
            buffer.push_str(&serde_json::to_string(entry)?);
            buffer.push('\n');
        }

        let temporary = self.path.with_extension("jsonl.tmp");
        std::fs::write(&temporary, buffer)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))
    }
}

pub fn history_path() -> PathBuf {
    gtk::glib::user_state_dir().join("yapper/history.jsonl")
}

/// Minutes and seconds, as a clock would show them: `1:05`.
pub fn mm_ss(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Unix microseconds, nudged forward if the clock hasn't moved since the last
/// entry so ids stay unique and ordered.
fn unique_id(newest: Option<i64>) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_micros() as i64)
        .unwrap_or(0);
    match newest {
        Some(previous) if previous >= now => previous + 1,
        _ => now,
    }
}

/// "just now", "12 minutes ago", "Yesterday 14:05", "3 days ago", then a date.
pub fn relative_time(unix_secs: i64) -> String {
    gtk::glib::DateTime::now_local()
        .map(|now| relative_time_at(unix_secs, &now))
        .unwrap_or_default()
}

/// As [`relative_time`], against a `now` the caller already has. Restamping a
/// whole list wants one clock reading, not one per row.
pub fn relative_time_at(unix_secs: i64, now: &gtk::glib::DateTime) -> String {
    gtk::glib::DateTime::from_unix_local(unix_secs)
        .map(|then| describe(&then, now))
        .unwrap_or_default()
}

fn describe(then: &gtk::glib::DateTime, now: &gtk::glib::DateTime) -> String {
    let seconds = now.difference(then).as_seconds();
    // Calendar days, not elapsed 24-hour periods: 11pm yesterday to 9am today
    // is "Yesterday", however few hours that actually is.
    let days = calendar_days_between(then, now);

    if days == 0 {
        let minutes = seconds / 60;
        if seconds < 45 {
            "just now".to_string()
        } else if minutes < 60 {
            let minutes = minutes.max(1);
            format!("{minutes} minute{} ago", plural(minutes))
        } else {
            let hours = minutes / 60;
            format!("{hours} hour{} ago", plural(hours))
        }
    } else if days == 1 {
        let time_of_day = then
            .format("%H:%M")
            .map(|formatted| formatted.to_string())
            .unwrap_or_default();
        format!("Yesterday {time_of_day}")
    } else if days < 7 {
        format!("{days} days ago")
    } else {
        then.format("%e %b %Y")
            .map(|formatted| formatted.to_string().trim().to_string())
            .unwrap_or_default()
    }
}

/// Whole days between the two dates, ignoring the time of day.
fn calendar_days_between(then: &gtk::glib::DateTime, now: &gtk::glib::DateTime) -> i64 {
    let midnight = |moment: &gtk::glib::DateTime| {
        gtk::glib::DateTime::new(
            &moment.timezone(),
            moment.year(),
            moment.month(),
            moment.day_of_month(),
            0,
            0,
            0.0,
        )
        .ok()
    };
    match (midnight(now), midnight(then)) {
        (Some(today), Some(that_day)) => today.difference(&that_day).as_days(),
        _ => 0,
    }
}

/// The suffix that makes `word` into `words`, for any count but one.
pub fn plural<N: PartialEq + From<u8>>(n: N) -> &'static str {
    if n == N::from(1) { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, text: &str) -> Entry {
        Entry {
            id,
            text: text.to_string(),
            duration_secs: 1.0,
        }
    }

    #[test]
    fn summary_takes_the_first_line() {
        assert_eq!(entry(1, "hello\nworld").summary(), "hello");
        assert_eq!(entry(1, "").summary(), "");
    }

    #[test]
    fn duration_is_minutes_and_seconds() {
        let mut e = entry(1, "x");
        e.duration_secs = 75.4;
        assert_eq!(e.duration_label(), "1:15");
        e.duration_secs = 4.0;
        assert_eq!(e.duration_label(), "0:04");
    }

    #[test]
    fn ids_stay_unique_when_the_clock_stands_still() {
        let first = unique_id(None);
        let second = unique_id(Some(first));
        assert!(second > first);
        // A clock that jumped backwards must not produce a duplicate either.
        assert!(unique_id(Some(i64::MAX - 1)) > i64::MAX - 1);
    }

    fn at(spec: &str) -> gtk::glib::DateTime {
        let parts: Vec<i32> = spec
            .split(['-', ' ', ':'])
            .map(|p| p.parse().unwrap())
            .collect();
        gtk::glib::DateTime::new(
            &gtk::glib::TimeZone::local(),
            parts[0],
            parts[1],
            parts[2],
            parts[3],
            parts[4],
            0.0,
        )
        .unwrap()
    }

    #[test]
    fn relative_times_read_the_way_a_person_would_say_them() {
        let now = at("2026-09-15 09:00");
        assert_eq!(describe(&at("2026-09-15 09:00"), &now), "just now");
        assert_eq!(describe(&at("2026-09-15 08:59"), &now), "1 minute ago");
        assert_eq!(describe(&at("2026-09-15 08:48"), &now), "12 minutes ago");
        assert_eq!(describe(&at("2026-09-15 07:55"), &now), "1 hour ago");
        assert_eq!(describe(&at("2026-09-15 03:00"), &now), "6 hours ago");
        // Ten hours earlier, but a different calendar day.
        assert_eq!(describe(&at("2026-09-14 23:00"), &now), "Yesterday 23:00");
        assert_eq!(describe(&at("2026-09-12 10:00"), &now), "3 days ago");
        assert_eq!(describe(&at("2026-08-15 10:00"), &now), "15 Aug 2026");
    }

    #[test]
    fn recorded_at_converts_micros_to_seconds() {
        assert_eq!(entry(1_700_000_000_000_123, "x").recorded_at(), 1_700_000_000);
    }

    /// A directory of our own under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "yapper-test-{name}-{}-{}",
                std::process::id(),
                unique_id(None)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("history.jsonl")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn entries_survive_a_reload_and_a_delete() {
        let scratch = Scratch::new("reload");
        let mut history = History::load_at(scratch.file(), 10);
        let first = history.add("hello".into(), 1.0).unwrap();
        let second = history.add("world".into(), 2.0).unwrap();

        let reloaded = History::load_at(scratch.file(), 10);
        assert_eq!(reloaded.entries().len(), 2);
        // Newest first, which is the order the list shows.
        assert_eq!(reloaded.entries()[0].id, second);
        assert_eq!(reloaded.entries()[0].text, "world");
        assert_eq!(reloaded.entries()[0].duration_secs, 2.0);
        assert_eq!(reloaded.entries()[1].id, first);

        let mut history = reloaded;
        assert!(history.remove(first).unwrap());
        assert!(!history.remove(first).unwrap(), "deleting twice is a no-op");

        let reloaded = History::load_at(scratch.file(), 10);
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.entries()[0].id, second);
    }

    #[test]
    fn the_limit_drops_the_oldest() {
        let scratch = Scratch::new("limit");
        let mut history = History::load_at(scratch.file(), 2);
        history.add("one".into(), 1.0).unwrap();
        history.add("two".into(), 1.0).unwrap();
        history.add("three".into(), 1.0).unwrap();

        let reloaded = History::load_at(scratch.file(), 2);
        let texts: Vec<&str> = reloaded
            .entries()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, ["three", "two"]);
    }

    #[test]
    fn a_damaged_line_costs_only_that_entry() {
        let scratch = Scratch::new("damaged");
        let mut history = History::load_at(scratch.file(), 10);
        history.add("kept".into(), 1.0).unwrap();
        history.add("also kept".into(), 1.0).unwrap();

        let mut raw = std::fs::read_to_string(scratch.file()).unwrap();
        raw.push_str("{ this is not json\n");
        std::fs::write(scratch.file(), raw).unwrap();

        let reloaded = History::load_at(scratch.file(), 10);
        assert_eq!(reloaded.entries().len(), 2);
    }

    #[test]
    fn a_missing_file_is_an_empty_history() {
        let scratch = Scratch::new("missing");
        assert!(History::load_at(scratch.file(), 10).entries().is_empty());
    }
}
