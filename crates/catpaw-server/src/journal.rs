//! The flight recorder: one JSON line per tool call and per confirmation,
//! in `<dir>/session-<time>-<pid>/journal.jsonl`, with a screenshot per
//! page action in `screens/` when asked for. It keeps what an auditor
//! needs to retrace a session, and never cookies, the bodies of held
//! requests, or what was typed into a password field (only its length).

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Where a session's journal goes.
#[derive(Clone, Debug)]
pub struct JournalConfig {
    pub dir: PathBuf,
    /// Keep a screenshot after each call that acted on a page.
    pub screens: bool,
}

pub struct Journal {
    dir: PathBuf,
    out: BufWriter<File>,
    seq: u64,
    screens: bool,
    /// The first write that failed, until reported.
    failed: Option<String>,
    reported: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Journal {
    /// Starts a journal in a new directory under `config.dir` (only its
    /// owner can read it); fails when it cannot be written.
    pub fn open(config: &JournalConfig, start: Value) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.dir)?;
        let stem = format!("session-{}-{}", now_ms(), std::process::id());
        let mut n = 1;
        let dir = loop {
            let dir = match n {
                1 => config.dir.join(&stem),
                n => config.dir.join(format!("{stem}-{n}")),
            };
            match crate::profile::private_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e),
            }
        };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let out = BufWriter::new(options.open(dir.join("journal.jsonl"))?);
        let mut journal = Self {
            dir,
            out,
            seq: 0,
            screens: config.screens,
            failed: None,
            reported: false,
        };
        journal.write("start", start);
        match journal.failed.take() {
            Some(e) => Err(std::io::Error::other(e)),
            None => Ok(journal),
        }
    }

    /// Why writing failed, the first time it did; said once.
    pub fn take_error(&mut self) -> Option<String> {
        if self.reported {
            return None;
        }
        let failed = self.failed.take();
        self.reported = failed.is_some();
        failed
    }

    fn failed(&mut self, e: std::io::Error) {
        self.failed.get_or_insert_with(|| e.to_string());
    }

    /// The directory the journal writes to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether screenshots are kept.
    pub fn keeps_screens(&self) -> bool {
        self.screens
    }

    /// Writes one record; returns its sequence number.
    pub fn write(&mut self, kind: &str, fields: Value) -> u64 {
        self.seq += 1;
        let mut record = json!({ "seq": self.seq, "at": now_ms(), "type": kind });
        if let (Some(record), Value::Object(fields)) = (record.as_object_mut(), fields) {
            record.extend(fields);
        }
        if let Err(e) = writeln!(self.out, "{record}").and_then(|()| self.out.flush()) {
            self.failed(e);
        }
        self.seq
    }

    /// Keeps a screenshot under the record `seq`.
    pub fn screen(&mut self, seq: u64, png: &[u8]) {
        let dir = self.dir.join("screens");
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join(format!("{seq:05}.png")), png))
        {
            self.failed(e);
        }
    }
}

/// A call's arguments as the journal keeps them: what was typed into a
/// password field becomes its length.
pub fn redact(tool: &str, args: &Value, secret_input: bool) -> Value {
    let mut args = args.clone();
    let length = |text: &str| json!(format!("({} characters)", text.chars().count()));
    if tool == "type"
        && secret_input
        && let Some(text) = args.get("text").and_then(Value::as_str)
    {
        args["text"] = length(text);
    }
    // A fill with a password among its fields keeps the length of each
    // text it set.
    if tool == "fill"
        && secret_input
        && let Some(fields) = args.get_mut("fields").and_then(Value::as_array_mut)
    {
        for field in fields {
            if let Some(text) = field.get("value").and_then(Value::as_str) {
                field["value"] = length(text);
            }
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_lines_of_json_and_secrets_are_lengths() {
        let root = std::env::temp_dir().join(format!("catpaw-journal-{}", std::process::id()));
        let mut journal = Journal::open(
            &JournalConfig {
                dir: root.clone(),
                screens: false,
            },
            json!({"version": "0"}),
        )
        .unwrap();
        let args = redact("type", &json!({"target": "e3", "text": "hunter2"}), true);
        journal.write("call", json!({"tool": "type", "args": args}));
        let text = std::fs::read_to_string(journal.dir().join("journal.jsonl")).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "start");
        assert_eq!(lines[1]["seq"], 2);
        assert_eq!(lines[1]["args"]["text"], "(7 characters)");
        assert!(!text.contains("hunter2"));
        let _ = std::fs::remove_dir_all(root);
    }
}
