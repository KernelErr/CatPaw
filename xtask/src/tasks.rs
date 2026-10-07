//! `cargo xtask tasks`: the agent task set in `tests/tasks/<id>/`.
//!
//! Each task is a goal on a site made for automation practice, the calls
//! an agent would make (targets by `role "name"`, as a snapshot shows
//! them), and checks of the outcome. A task runs through the MCP server
//! with a fixed random seed and time origin; its transcript (each call and
//! its result) is what is compared. `record` runs a task live, keeps its
//! traffic as HAR, and keeps the recording only when two offline replays
//! of it agree byte for byte; `replay` runs the recordings offline against
//! the expected transcripts. Tasks on third-party content sites live in
//! `tests/tasks/local/` (not committed) and run with `--local`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use catpaw_net::{Misses, Recording};
use catpaw_server::{McpServer, Session, SessionConfig};
use clap::{Args as ClapArgs, Subcommand};
use serde_json::{Value, json};

use crate::wpt::workspace_root;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run tasks live, record their traffic, and keep the recording when
    /// two offline replays of it agree.
    Record(Select),
    /// Replay recorded tasks offline and compare with their transcripts.
    Replay {
        #[command(flatten)]
        select: Select,
        /// Replay each task twice and compare the runs.
        #[arg(long)]
        twice: bool,
        /// Rewrite the expected transcripts with what the replays give.
        #[arg(long)]
        update_expectations: bool,
    },
    /// Check the task files: fields, recording sizes, no local paths.
    Lint {
        #[command(flatten)]
        select: Select,
        /// Text no task file may hold (a user or host name, say); may
        /// repeat.
        #[arg(long)]
        forbid: Vec<String>,
    },
    /// Steps, sizes and outcome per task, as a Markdown table (replayed).
    Report {
        #[command(flatten)]
        select: Select,
        /// What tools/baseline/playwright-mcp.mjs measured, to set beside.
        #[arg(long)]
        baseline: Option<PathBuf>,
    },
}

#[derive(ClapArgs)]
struct Select {
    /// Tasks to take (by id); all of them when none.
    ids: Vec<String>,
    /// Take the tasks in tests/tasks/local/ instead.
    #[arg(long)]
    local: bool,
}

/// The largest recording one task may commit.
const MAX_HAR: u64 = 3 * 1024 * 1024;
/// The most all recordings together may take.
const MAX_HARS: u64 = 60 * 1024 * 1024;

/// A check of a task's outcome.
enum Check {
    Url(String),
    Text(String),
    Snapshot(String),
    /// A script's result, as `evaluate` gives it, equals this.
    Eval(String, String),
}

/// One step of a task: a call the agent makes, or the user deciding a
/// confirmation (on the approval page, with the key).
#[derive(Clone)]
enum Step {
    Call(String, Value),
    Decide(String, bool),
}

struct Task {
    id: String,
    dir: PathBuf,
    start_url: String,
    steps: Vec<Step>,
    checks: Vec<Check>,
    seed: u64,
    time_origin: f64,
}

impl Task {
    fn har(&self) -> PathBuf {
        self.dir.join("run.har.zst")
    }

    fn expected(&self) -> PathBuf {
        self.dir.join("expected.txt")
    }
}

fn tasks_dir(local: bool) -> PathBuf {
    let root = workspace_root().join("tests/tasks");
    if local { root.join("local") } else { root }
}

fn load(select: &Select) -> Result<Vec<Task>> {
    let dir = tasks_dir(select.local);
    let mut tasks = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("task.json").exists())
        .collect();
    entries.sort();
    for path in entries {
        let id = path.file_name().unwrap().to_string_lossy().to_string();
        if !select.ids.is_empty() && !select.ids.contains(&id) {
            continue;
        }
        let text = std::fs::read_to_string(path.join("task.json"))?;
        let spec: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {id}/task.json"))?;
        let start_url = spec["start_url"]
            .as_str()
            .with_context(|| format!("{id}: start_url"))?
            .to_string();
        let mut steps = Vec::new();
        for step in spec["steps"].as_array().into_iter().flatten() {
            if let Some(c) = step["approve"].as_str() {
                steps.push(Step::Decide(c.to_string(), true));
                continue;
            }
            if let Some(c) = step["decline"].as_str() {
                steps.push(Step::Decide(c.to_string(), false));
                continue;
            }
            let tool = step["tool"]
                .as_str()
                .with_context(|| format!("{id}: a step without a tool"))?;
            steps.push(Step::Call(tool.to_string(), step["args"].clone()));
        }
        let mut checks = Vec::new();
        for check in spec["success"].as_array().into_iter().flatten() {
            if let (Some(script), Some(equals)) = (check["eval"].as_str(), check["equals"].as_str())
            {
                checks.push(Check::Eval(script.to_string(), equals.to_string()));
                continue;
            }
            let Some((kind, value)) = check.as_object().and_then(|o| o.iter().next()) else {
                bail!("{id}: a check must be {{\"kind\": \"value\"}}");
            };
            let value = value.as_str().unwrap_or("").to_string();
            checks.push(match kind.as_str() {
                "url_contains" => Check::Url(value),
                "text_contains" => Check::Text(value),
                "snapshot_contains" => Check::Snapshot(value),
                other => bail!("{id}: unknown check {other}"),
            });
        }
        if spec["goal"].as_str().is_none_or(str::is_empty) {
            bail!("{id}: goal");
        }
        tasks.push(Task {
            id,
            dir: path,
            start_url,
            steps,
            checks,
            seed: spec["random_seed"].as_u64().unwrap_or(1),
            time_origin: spec["time_origin"].as_f64().unwrap_or(1_790_000_000_000.0),
        });
    }
    if tasks.is_empty() {
        bail!("no tasks in {}", dir.display());
    }
    Ok(tasks)
}

/// What one run of a task gave.
struct Run {
    transcript: String,
    passed: bool,
    calls: usize,
    result_bytes: usize,
}

fn call(server: &mut McpServer, id: &mut u64, tool: &str, args: &Value) -> Result<(String, bool)> {
    *id += 1;
    let line = json!({
        "jsonrpc": "2.0",
        "id": *id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    });
    let reply = server
        .handle_line(&line.to_string())
        .context("the server did not answer")?;
    let reply: Value = serde_json::from_str(&reply)?;
    if let Some(error) = reply.get("error") {
        bail!("{tool}: {error}");
    }
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    Ok((text, reply["result"]["isError"] == true))
}

/// The approval page's address in a result, which names a port of the
/// moment: transcripts keep `PORT` in its place.
fn approval_url(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find(|w| w.starts_with("http://127.0.0.1:") && w.contains("/confirm/c"))
}

fn without_port(text: &str) -> String {
    match approval_url(text) {
        Some(url) => {
            let rest = url.trim_start_matches("http://127.0.0.1:");
            let path = rest.split_once('/').map(|(_, p)| p).unwrap_or("");
            text.replace(url, &format!("http://127.0.0.1:PORT/{path}"))
        }
        None => text.to_string(),
    }
}

/// Decides a confirmation on the approval page, as the user would, with
/// the approval key.
fn decide(url: &str, key: &str, approve: bool) -> Result<String> {
    use std::io::{Read, Write};
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .context("not an approval URL")?;
    let (port, path) = rest.split_once('/').context("not an approval URL")?;
    let body = format!("decision={}", if approve { "approve" } else { "decline" });
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>()?))?;
    write!(
        stream,
        "POST /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let state = reply
        .split("\"state\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .context("the approval page did not say")?;
    Ok(state.to_string())
}

fn run(task: &Task, recording: Recording) -> Result<Run> {
    let mut config = SessionConfig::default();
    config.options.net.recording = Some(recording);
    config.options.page.random_seed = Some(task.seed);
    config.options.page.time_origin_unix_ms = Some(task.time_origin);
    config.files_root = Some(task.dir.clone());
    let key_file = std::env::temp_dir()
        .join(format!("catpaw-tasks-{}", std::process::id()))
        .join("approval-key");
    config.approval.key_file = Some(key_file.clone());
    let session = Session::new(config).context("starting a session")?;
    let mut server = McpServer::new(session);
    let mut id = 0;
    let mut transcript = String::new();
    let mut result_bytes = 0;
    let mut calls = 0;
    let mut steps = vec![Step::Call(
        "navigate".to_string(),
        json!({"url": task.start_url}),
    )];
    steps.extend(task.steps.iter().cloned());
    let mut last = String::new();
    for step in &steps {
        match step {
            Step::Call(tool, args) => {
                let (text, _) = call(&mut server, &mut id, tool, args)?;
                calls += 1;
                result_bytes += text.len();
                transcript.push_str(&format!("> {tool} {args}\n{}\n\n", without_port(&text)));
                last = text;
            }
            Step::Decide(confirmation, approve) => {
                let url = approval_url(&last)
                    .filter(|url| url.ends_with(&format!("/confirm/{confirmation}")))
                    .with_context(|| {
                        format!(
                            "{}: the step before did not ask for {confirmation}",
                            task.id
                        )
                    })?;
                let key = std::fs::read_to_string(&key_file)?;
                let state = decide(url, key.trim(), *approve)?;
                let verb = if *approve { "approve" } else { "decline" };
                transcript.push_str(&format!("> (user) {verb} {confirmation}\n{state}\n\n"));
            }
        }
    }
    let mut passed = true;
    for check in &task.checks {
        let (label, tool, args, wanted) = match check {
            Check::Url(v) => (
                "url_contains",
                "evaluate",
                json!({"script": "location.href"}),
                v.clone(),
            ),
            Check::Text(v) => (
                "text_contains",
                "read",
                json!({"view": "text", "maxTokens": 100000}),
                v.clone(),
            ),
            Check::Snapshot(v) => (
                "snapshot_contains",
                "snapshot",
                json!({"maxTokens": 100000}),
                v.clone(),
            ),
            Check::Eval(script, v) => (
                "eval",
                "evaluate",
                json!({"script": script}),
                format!("ok evaluate\n{v}"),
            ),
        };
        let (text, error) = call(&mut server, &mut id, tool, &args)?;
        let ok = !error
            && match check {
                Check::Eval(..) => text == wanted,
                _ => text.contains(wanted.as_str()),
            };
        passed &= ok;
        transcript.push_str(&format!(
            "? {label} {} → {}\n",
            serde_json::to_string(&wanted)?,
            if ok { "yes" } else { "no" }
        ));
    }
    if let Err(e) = server.session().save_recording() {
        bail!("writing the recording: {e}");
    }
    Ok(Run {
        transcript,
        passed,
        calls,
        result_bytes,
    })
}

fn replay(task: &Task, path: &Path) -> Result<Run> {
    run(
        task,
        Recording::Replay {
            path: path.to_path_buf(),
            misses: Misses::Fail,
        },
    )
}

/// The first line where two transcripts part.
fn first_difference(a: &str, b: &str) -> String {
    for (i, (x, y)) in a.lines().zip(b.lines()).enumerate() {
        if x != y {
            return format!("line {}:\n  - {x}\n  + {y}", i + 1);
        }
    }
    format!(
        "one ends first ({} lines against {})",
        a.lines().count(),
        b.lines().count()
    )
}

pub fn run_cmd(args: Args) -> Result<()> {
    match args.cmd {
        Cmd::Record(select) => record(&select),
        Cmd::Replay {
            select,
            twice,
            update_expectations,
        } => replay_all(&select, twice, update_expectations),
        Cmd::Lint { select, forbid } => lint(&select, &forbid),
        Cmd::Report { select, baseline } => report(&select, baseline.as_deref()),
    }
}

fn record(select: &Select) -> Result<()> {
    let mut failed = 0;
    for task in load(select)? {
        let started = Instant::now();
        let fresh = task.dir.join("run.new.har.zst");
        let live = run(&task, Recording::Record(fresh.clone()))?;
        let first_result = live.transcript.split("\n\n").next().unwrap_or("");
        if first_result.contains(" challenge=")
            || [" (403)", " (429)", " (503)"]
                .iter()
                .any(|s| first_result.contains(s))
        {
            println!(
                "{}: the site answered with a challenge or refusal; not kept",
                task.id
            );
            let _ = std::fs::remove_file(&fresh);
            failed += 1;
            continue;
        }
        let first = replay(&task, &fresh)?;
        let second = replay(&task, &fresh)?;
        if first.transcript != second.transcript {
            println!(
                "{}: two replays differ; not kept ({})",
                task.id,
                first_difference(&first.transcript, &second.transcript)
            );
            let _ = std::fs::remove_file(&fresh);
            failed += 1;
            continue;
        }
        std::fs::rename(&fresh, task.har())?;
        std::fs::write(task.expected(), &first.transcript)?;
        let size = std::fs::metadata(task.har())?.len();
        println!(
            "{}: recorded ({} calls, {} KB, live run {}, replay {}) in {:.1}s",
            task.id,
            first.calls,
            size / 1024,
            if live.passed { "passed" } else { "FAILED" },
            if first.passed { "passed" } else { "FAILED" },
            started.elapsed().as_secs_f64()
        );
        if !first.passed {
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} task(s) not recorded or not passing");
    }
    Ok(())
}

fn replay_all(select: &Select, twice: bool, update: bool) -> Result<()> {
    let mut failures = Vec::new();
    let mut count = 0;
    for task in load(select)? {
        if !task.har().exists() {
            failures.push(format!(
                "{}: no recording (run `tasks record {}`)",
                task.id, task.id
            ));
            continue;
        }
        count += 1;
        let first = replay(&task, &task.har())?;
        if twice {
            let second = replay(&task, &task.har())?;
            if first.transcript != second.transcript {
                failures.push(format!(
                    "{}: not repeatable, two replays differ at {}",
                    task.id,
                    first_difference(&first.transcript, &second.transcript)
                ));
                continue;
            }
        }
        if update {
            std::fs::write(task.expected(), &first.transcript)?;
        } else {
            let expected = std::fs::read_to_string(task.expected()).unwrap_or_default();
            if expected != first.transcript {
                failures.push(format!(
                    "{}: the transcript drifted from expected.txt at {} (`--update-expectations` takes it)",
                    task.id,
                    first_difference(&expected, &first.transcript)
                ));
                continue;
            }
        }
        if !first.passed {
            failures.push(format!("{}: the checks fail", task.id));
        }
    }
    println!("tasks: {count} replayed, {} failed", failures.len());
    for failure in &failures {
        println!("  {failure}");
    }
    if !failures.is_empty() {
        bail!("task replays failed");
    }
    Ok(())
}

fn lint(select: &Select, forbid: &[String]) -> Result<()> {
    let mut problems = Vec::new();
    let mut total = 0;
    for task in load(select)? {
        let har = task.har();
        if !har.exists() {
            problems.push(format!("{}: no recording", task.id));
            continue;
        }
        let size = std::fs::metadata(&har)?.len();
        total += size;
        if size > MAX_HAR {
            problems.push(format!(
                "{}: recording of {} KB (limit {} KB)",
                task.id,
                size / 1024,
                MAX_HAR / 1024
            ));
        }
        let bytes = zstd::decode_all(&std::fs::read(&har)?[..])?;
        let text = String::from_utf8_lossy(&bytes);
        let expected = std::fs::read_to_string(task.expected()).unwrap_or_default();
        for marker in ["/Users/", "/home/", "C:\\\\Users", "/private/var/"] {
            if text.contains(marker) || expected.contains(marker) {
                problems.push(format!(
                    "{}: a local path ({marker}) in the task files",
                    task.id
                ));
            }
        }
        let spec = std::fs::read_to_string(task.dir.join("task.json")).unwrap_or_default();
        let files = [
            text.to_lowercase(),
            expected.to_lowercase(),
            spec.to_lowercase(),
        ];
        for word in forbid {
            let word = word.to_lowercase();
            if files.iter().any(|t| t.contains(&word)) {
                problems.push(format!("{}: a forbidden text in the task files", task.id));
            }
        }
        for secret in ["\"name\":\"cookie\"", "\"name\":\"authorization\""] {
            if text.contains(secret) {
                problems.push(format!(
                    "{}: a request secret header in the recording",
                    task.id
                ));
            }
        }
    }
    if total > MAX_HARS {
        problems.push(format!(
            "recordings take {} MB (limit {} MB)",
            total >> 20,
            MAX_HARS >> 20
        ));
    }
    println!("tasks lint: {} problem(s)", problems.len());
    for problem in &problems {
        println!("  {problem}");
    }
    if !problems.is_empty() {
        bail!("task lint failed");
    }
    Ok(())
}

/// Bytes, with the tokens the snapshot budget estimates them at.
fn sized(bytes: u64) -> String {
    format!("{bytes} (~{})", (bytes as f64 / 3.5).round() as u64)
}

fn report(select: &Select, baseline: Option<&Path>) -> Result<()> {
    let baseline: Option<Value> = match baseline {
        Some(path) => Some(serde_json::from_str(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?,
        )?),
        None => None,
    };
    match &baseline {
        Some(b) => {
            let name = b["server"].as_str().unwrap_or("baseline");
            println!(
                "| task | CatPaw calls | CatPaw bytes (~tokens) | {name} calls | {name} bytes (~tokens) |"
            );
            println!("|---|---|---|---|---|");
        }
        None => {
            println!("| task | calls | outcome | result bytes (~tokens) |");
            println!("|---|---|---|---|");
        }
    }
    let (mut ours, mut theirs) = ((0, 0), (0, 0));
    for task in load(select)? {
        if !task.har().exists() {
            println!("| {} | not recorded |", task.id);
            continue;
        }
        let run = replay(&task, &task.har())?;
        let bytes = run.result_bytes as u64;
        let mine = if run.passed {
            sized(bytes)
        } else {
            "failed".into()
        };
        ours = (ours.0 + run.calls, ours.1 + bytes);
        match &baseline {
            Some(b) => {
                let other = &b["tasks"][&task.id];
                let (calls, bytes) = (other["calls"].as_u64(), other["bytes"].as_u64());
                let (calls, theirs_cell) = match (calls, bytes, other["passed"] == true) {
                    (Some(calls), Some(bytes), true) => {
                        theirs = (theirs.0 + calls, theirs.1 + bytes);
                        (calls.to_string(), sized(bytes))
                    }
                    (Some(calls), _, false) => (calls.to_string(), "failed".into()),
                    _ => ("-".into(), "-".into()),
                };
                println!(
                    "| {} | {} | {mine} | {calls} | {theirs_cell} |",
                    task.id, run.calls
                );
            }
            None => println!(
                "| {} | {} | {} | {mine} |",
                task.id,
                run.calls,
                if run.passed { "passed" } else { "failed" }
            ),
        }
    }
    if let Some(b) = &baseline {
        println!(
            "| all | {} | {} | {} | {} |",
            ours.0,
            sized(ours.1),
            theirs.0,
            sized(theirs.1)
        );
        let mut server = McpServer::new(Session::new(SessionConfig::default())?);
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let reply: Value = serde_json::from_str(
            &server
                .handle_line(&line.to_string())
                .context("tools/list went unanswered")?,
        )?;
        let tools = serde_json::to_string(&reply["result"]["tools"])?.len() as u64;
        println!(
            "\ntools/list: CatPaw {}, {} {} (measured {})",
            sized(tools),
            b["server"].as_str().unwrap_or("baseline"),
            sized(b["tools_list_bytes"].as_u64().unwrap_or(0)),
            b["date"].as_str().unwrap_or("-")
        );
    }
    Ok(())
}
