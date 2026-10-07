//! The flight recorder: one JSON line per tool call and per confirmation,
//! in `<dir>/session-<time>-<pid>/journal.jsonl`, with a screenshot per
//! page action in `screens/` when asked for. It keeps what an auditor
//! needs to retrace a session, and never cookies, the bodies of held
//! requests, or what was typed into a password field (only its length).

use std::fs::File;
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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Journal {
    /// Starts a journal in a new directory under `config.dir`.
    pub fn open(config: &JournalConfig, start: Value) -> std::io::Result<Self> {
        let dir = config
            .dir
            .join(format!("session-{}-{}", now_ms(), std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let out = BufWriter::new(File::create(dir.join("journal.jsonl"))?);
        let mut journal = Self {
            dir,
            out,
            seq: 0,
            screens: config.screens,
        };
        journal.write("start", start);
        Ok(journal)
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
        let _ = writeln!(self.out, "{record}");
        let _ = self.out.flush();
        self.seq
    }

    /// Keeps a screenshot under the record `seq`.
    pub fn screen(&mut self, seq: u64, png: &[u8]) {
        let dir = self.dir.join("screens");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(dir.join(format!("{seq:05}.png")), png);
        }
    }
}

/// A call's arguments as the journal keeps them: what was typed into a
/// password field becomes its length.
pub fn redact(tool: &str, args: &Value, secret_input: bool) -> Value {
    let mut args = args.clone();
    if tool == "type"
        && secret_input
        && let Some(text) = args.get("text").and_then(Value::as_str)
    {
        let length = text.chars().count();
        args["text"] = json!(format!("({length} characters)"));
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
