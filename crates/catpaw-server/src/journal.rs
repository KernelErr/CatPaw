//! The flight recorder: one JSON line per tool call and per confirmation,
//! in `<dir>/session-<time>-<pid>/journal.jsonl`, with a screenshot per
//! page action in `screens/` when asked for. It keeps what an auditor
//! needs to retrace a session, and never cookies, the bodies of held
//! requests, or what was typed into a password field (only its length).

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// A journal the session and its local pages write to.
#[derive(Clone)]
pub(crate) struct SharedJournal(Arc<Mutex<Journal>>);

impl SharedJournal {
    pub fn new(journal: Journal) -> Self {
        Self(Arc::new(Mutex::new(journal)))
    }

    /// The journal, also after a thread panicked while writing it: the
    /// records after that are kept all the same.
    pub fn lock(&self) -> MutexGuard<'_, Journal> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A tool call as the journal keeps it.
pub(crate) struct CallRecord<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    /// The call typed into a password field: the text is kept as a length.
    pub secret_input: bool,
    pub tab: Option<u32>,
    pub url: Option<String>,
    /// The result's text.
    pub text: &'a str,
    pub took: Duration,
    /// The tab after the call, when screens are kept.
    pub screen: Option<Vec<u8>>,
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

    /// Writes a tool call: its arguments (what was typed into a password
    /// field as a length), its result line and consequences, how long it
    /// took, and the screen after it.
    pub(crate) fn record_call(&mut self, call: CallRecord<'_>) {
        let first = call.text.lines().next().unwrap_or("");
        let consequences: Vec<&str> = call
            .text
            .lines()
            .filter(|l| l.starts_with("! "))
            .take(10)
            .collect();
        let seq = self.write(
            "call",
            json!({
                "tool": call.tool,
                "args": redact(call.tool, call.args, call.secret_input),
                "tab": call.tab.map(|t| format!("t{t}")),
                "result": first.split_whitespace().next().unwrap_or(""),
                "line": first,
                "bytes": call.text.len(),
                "ms": call.took.as_millis() as u64,
                "url": call.url,
                "consequences": consequences,
            }),
        );
        if let Some(png) = call.screen {
            self.screen(seq, &png);
        }
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(journal.dir()), 0o700);
            assert_eq!(mode(&journal.dir().join("journal.jsonl")), 0o600);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_call_record_keeps_the_result_line_and_consequences() {
        let root = std::env::temp_dir().join(format!("catpaw-journal-call-{}", std::process::id()));
        let mut journal = Journal::open(
            &JournalConfig {
                dir: root.clone(),
                screens: false,
            },
            json!({}),
        )
        .unwrap();
        journal.record_call(CallRecord {
            tool: "click",
            args: &json!({"target": "e2"}),
            secret_input: false,
            tab: Some(1),
            url: Some("https://example.com/".into()),
            text: "ok click e2 button \"Go\"\n! network POST /api 200\n# s2 changed=1",
            took: Duration::from_millis(12),
            screen: None,
        });
        let text = std::fs::read_to_string(journal.dir().join("journal.jsonl")).unwrap();
        let call: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(call["type"], "call");
        assert_eq!(call["tab"], "t1");
        assert_eq!(call["result"], "ok");
        assert_eq!(call["consequences"], json!(["! network POST /api 200"]));
        assert_eq!(call["ms"], 12);
        let _ = std::fs::remove_dir_all(root);
    }
}
