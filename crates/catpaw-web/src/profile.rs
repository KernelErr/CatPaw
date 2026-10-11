//! Where a page's time goes, when asked (`catpaw fetch --profile`, or
//! `CATPAW_PROFILE` in the environment): the event loop's steps by kind,
//! the scripts by source (compiling apart from running), the parser, the
//! renderer and the waits for the network. Off, each measured place costs
//! one atomic load.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Row {
    took: Duration,
    count: u64,
    bytes: u64,
}

type Rows = HashMap<(&'static str, String), Row>;

fn store() -> &'static Mutex<Rows> {
    static STORE: OnceLock<Mutex<Rows>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// Starts keeping the profile, for pages on every thread.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether the profile is kept: asked for, or `CATPAW_PROFILE` set.
pub fn enabled() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    ENABLED.load(Ordering::Relaxed)
        || *FROM_ENV.get_or_init(|| std::env::var_os("CATPAW_PROFILE").is_some_and(|v| v != "0"))
}

/// Adds `took` to the row of `label` under `category`; `bytes` is how
/// much input the work covered, when that is what it scales with.
pub fn record(category: &'static str, label: &str, took: Duration, bytes: usize) {
    if !enabled() {
        return;
    }
    let mut rows = store().lock().unwrap_or_else(|e| e.into_inner());
    let row = rows.entry((category, label.to_string())).or_default();
    row.took += took;
    row.count += 1;
    row.bytes += bytes as u64;
}

/// Times a scope: `let _span = profile::span(…)`, recorded when it is
/// dropped. The label is only made when the profile is kept.
pub fn span(category: &'static str, label: impl FnOnce() -> String) -> Option<Span> {
    span_bytes(category, label, 0)
}

/// As [`span`], with the bytes of input the scope works through.
pub fn span_bytes(
    category: &'static str,
    label: impl FnOnce() -> String,
    bytes: usize,
) -> Option<Span> {
    enabled().then(|| Span {
        category,
        label: label(),
        bytes,
        started: Instant::now(),
    })
}

/// A scope being timed; see [`span`].
pub struct Span {
    category: &'static str,
    label: String,
    bytes: usize,
    started: Instant,
}

impl Drop for Span {
    fn drop(&mut self) {
        record(
            self.category,
            &self.label,
            self.started.elapsed(),
            self.bytes,
        );
    }
}

/// Forgets what was recorded.
pub fn reset() {
    store().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The order categories are shown in: the loop's steps, then the work
/// nested in them.
const CATEGORIES: [(&str, &str); 7] = [
    ("step", "steps of the event loop (each on its own)"),
    ("setup", "creating pages, their realms and bindings"),
    ("wait", "waiting for the network, within the steps"),
    ("parse", "parsing HTML, within the steps"),
    ("compile", "parsing and compiling scripts, within the steps"),
    ("run", "running scripts' top level, within the steps"),
    ("render", "style, layout and indexes, within the steps"),
];

/// How many rows a category shows before the rest are summed up.
const ROWS_SHOWN: usize = 25;

/// The profile as text: each category with its total, then its rows,
/// the longest first.
pub fn report() -> String {
    let rows = store().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::new();
    for (category, what) in CATEGORIES {
        let mut rows: Vec<_> = rows
            .iter()
            .filter(|((c, _), _)| *c == category)
            .map(|((_, label), row)| (label, row))
            .collect();
        if rows.is_empty() {
            continue;
        }
        rows.sort_by_key(|(_, row)| std::cmp::Reverse(row.took));
        let total: Duration = rows.iter().map(|(_, r)| r.took).sum();
        let count: u64 = rows.iter().map(|(_, r)| r.count).sum();
        let _ = writeln!(out, "{category}: {} in {count} ({what})", ms(total));
        for (label, row) in rows.iter().take(ROWS_SHOWN) {
            let size = if row.bytes > 0 {
                format!(" {:>7.0} KB", row.bytes as f64 / 1024.0)
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "  {:>10} {:>6}{size}  {label}",
                ms(row.took),
                row.count
            );
        }
        if rows.len() > ROWS_SHOWN {
            let rest: Duration = rows.iter().skip(ROWS_SHOWN).map(|(_, r)| r.took).sum();
            let _ = writeln!(
                out,
                "  {:>10}        … {} more",
                ms(rest),
                rows.len() - ROWS_SHOWN
            );
        }
    }
    if out.is_empty() {
        out.push_str("nothing was recorded\n");
    }
    out
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}
