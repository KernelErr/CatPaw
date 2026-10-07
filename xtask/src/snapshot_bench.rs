//! `cargo xtask snapshot-bench`: how big what an agent reads is, on live
//! pages, per snapshot format and filter (needs `--features bench`).
//!
//! Sizes are bytes of the tool result; tokens are estimated as bytes/3.5,
//! the estimate the snapshot budget uses.

use anyhow::Result;
use catpaw_server::{Session, SessionConfig};
use clap::Args as ClapArgs;
use serde_json::json;

/// Practice sites made for automation (tier A of the task set).
const PAGES: &[&str] = &[
    "https://www.saucedemo.com/",
    "https://books.toscrape.com/",
    "https://quotes.toscrape.com/",
    "https://the-internet.herokuapp.com/",
    "https://demo.playwright.dev/todomvc/",
    "https://httpbin.org/forms/post",
];

#[derive(ClapArgs)]
pub struct Args {
    /// Pages to measure instead of the practice sites; may repeat.
    #[arg(long)]
    url: Vec<String>,
}

fn cell(bytes: usize) -> String {
    format!("{bytes} (~{})", (bytes as f64 / 3.5).round() as usize)
}

pub fn run(args: Args) -> Result<()> {
    let urls: Vec<String> = if args.url.is_empty() {
        PAGES.iter().map(|u| u.to_string()).collect()
    } else {
        args.url
    };
    let mut session = Session::new(SessionConfig::default())?;
    let columns = ["compact", "aria", "interactive", "compact+href", "markdown"];
    println!("bytes (~tokens) of each result; budgets lifted\n");
    println!("| page | {} |", columns.join(" | "));
    println!("|---|{}", "---|".repeat(columns.len()));
    let big = 1_000_000;
    for url in urls {
        let nav = session.call_tool("navigate", json!({ "url": url }));
        if nav.is_error {
            let first = nav.text.lines().next().unwrap_or("");
            println!("| {url} | {first} |");
            continue;
        }
        let mut sizes = Vec::new();
        for args in [
            json!({"format": "compact", "maxTokens": big}),
            json!({"format": "aria", "maxTokens": big}),
            json!({"format": "compact", "filter": "interactive", "maxTokens": big}),
            json!({"format": "compact", "attrs": ["href"], "maxTokens": big}),
        ] {
            sizes.push(session.call_tool("snapshot", args).text.len());
        }
        let markdown = session.call_tool("read", json!({"view": "markdown", "maxTokens": big}));
        sizes.push(markdown.text.len());
        let cells: Vec<String> = sizes.into_iter().map(cell).collect();
        println!("| {url} | {} |", cells.join(" | "));
    }
    Ok(())
}
