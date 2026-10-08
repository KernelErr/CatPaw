//! Reading a tab as text (markdown, text, links, forms, tables, find,
//! html) and its logs (console, network, events).

use std::fmt::Write as _;

use catpaw_agent::snapshot::{quote, truncate};
use catpaw_agent::{LinkStyle, ReadOptions, RefScope};
use catpaw_engine::ConsoleLevel;
use catpaw_protocol::params::{self, LogKind, LogLevel, ReadView};
use catpaw_protocol::wording::{ErrorCode, advice};
use catpaw_web::agent;

use super::pending::{kind_word, outcome_word, short_url};
use super::{GroupState, READ_TOKENS, floor_char_boundary, resolve_ref, token_bytes};
use crate::oracle::EngineOracle;
use crate::output::{CallResult, Failure, ToolOutput};

/// The most hits `find` lists.
const FIND_HITS: usize = 20;
/// Log entries listed when the call gives no limit.
const LOG_LIMIT: usize = 50;

/// Finds the byte ranges of matches in a text.
type Matcher = Box<dyn Fn(&str) -> Vec<(usize, usize)>>;

/// A matcher for `find`: plain text (any case), or `/regex/flags`.
fn matcher(query: &str) -> Result<Matcher, Failure> {
    let q = query.trim();
    if q.is_empty() {
        return Err(Failure::bad_argument("find needs a query"));
    }
    if let Some(rest) = q.strip_prefix('/')
        && let Some(end) = rest.rfind('/')
        && end > 0
    {
        let (pattern, flags) = (&rest[..end], &rest[end + 1..]);
        let mut builder = regex::RegexBuilder::new(pattern);
        builder.case_insensitive(flags.contains('i'));
        builder.size_limit(1 << 20);
        let re = builder
            .build()
            .map_err(|e| Failure::bad_argument(format!("{q} is not a valid regex: {e}")))?;
        return Ok(Box::new(move |text: &str| {
            re.find_iter(text)
                .filter(|m| !m.is_empty())
                .map(|m| (m.start(), m.end()))
                .collect()
        }));
    }
    let needle = q.to_lowercase();
    Ok(Box::new(move |text: &str| {
        // Byte offsets of the lowercase text fit the text for ASCII, and
        // are checked against char boundaries otherwise.
        let lower = text.to_lowercase();
        if lower.len() != text.len() {
            return Vec::new();
        }
        lower
            .match_indices(&needle)
            .map(|(i, m)| (i, i + m.len()))
            .filter(|&(a, b)| text.is_char_boundary(a) && text.is_char_boundary(b))
            .collect()
    }))
}

fn level_of(level: ConsoleLevel) -> LogLevel {
    match level {
        ConsoleLevel::Debug => LogLevel::Debug,
        ConsoleLevel::Log => LogLevel::Log,
        ConsoleLevel::Info => LogLevel::Info,
        ConsoleLevel::Warn => LogLevel::Warn,
        ConsoleLevel::Error => LogLevel::Error,
    }
}

impl GroupState {
    pub(crate) fn read(&mut self, tab: u32, p: params::Read) -> CallResult {
        let (top, _) = self.root_state(tab)?;
        let frames = self.frames_of(tab);
        let allowed: Vec<catpaw_engine::FrameId> = frames.iter().map(|f| f.id).collect();
        // A root inside a frame reads that frame's document.
        let (frame, root) = match &p.root {
            Some(text) => {
                let page = &self.page;
                let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
                entry.sync(page);
                let (frame, node, _) = resolve_ref(page, &mut entry.refs, text, &allowed)?;
                // An `iframe` as root reads the document inside it.
                match self.hosted_frame(frame, node) {
                    Some(inner) => (inner, None),
                    None => (frame, Some(node)),
                }
            }
            None => (top, None),
        };
        let view = p.view;
        let find = match (view, &p.query) {
            (ReadView::Find, Some(query)) => Some(matcher(query)?),
            (ReadView::Find, None) => return Err(Failure::bad_argument("find needs a query")),
            _ => None,
        };
        let main = p.main;
        let (mut full, mut found) = if view == ReadView::Download {
            (self.download_text(p.query.as_deref())?, 0)
        } else {
            self.read_frame(tab, frame, root, view, find.as_ref(), main)?
        };
        // A whole document (the tab's, or a frame's given as root): the
        // frames inside it follow, each under the line of its frame
        // element.
        if root.is_none() && view != ReadView::Download {
            let inside: Vec<_> = frames
                .iter()
                .filter(|f| f.id != frame && self.frame_within(f.id, frame))
                .cloned()
                .collect();
            for inner in inside.iter() {
                let (part, hits) =
                    self.read_frame(tab, inner.id, None, view, find.as_ref(), main)?;
                if part.trim().is_empty() {
                    continue;
                }
                let host = match (inner.parent, inner.element) {
                    (Some(parent), Some(element)) => self
                        .ref_for(tab, parent, element)
                        .map(|r| self.describe(tab, r)),
                    _ => None,
                };
                if !full.is_empty() && !full.ends_with('\n') {
                    full.push('\n');
                }
                let _ = writeln!(
                    full,
                    "--- frame {}",
                    host.unwrap_or_else(|| format!("f{}", inner.id.0))
                );
                full.push_str(&part);
                found += hits;
            }
        }
        let offset = floor_char_boundary(&full, p.offset.unwrap_or(0).min(full.len()));
        let budget = token_bytes(p.max_tokens.unwrap_or(READ_TOKENS).max(200));
        let rest = &full[offset..];
        let mut end = rest.len();
        if rest.len() > budget {
            end = floor_char_boundary(rest, budget);
            // End at a line break when one is near.
            if let Some(nl) = rest[..end].rfind('\n')
                && nl > end * 4 / 5
            {
                end = nl + 1;
            }
        }
        let mut text = format!("ok read {}", view.as_str());
        if view == ReadView::Find {
            let query = p.query.as_deref().unwrap_or("");
            let _ = write!(
                text,
                " {} ({found} match{})",
                quote(query),
                if found == 1 { "" } else { "es" }
            );
        }
        if offset > 0 || end < rest.len() {
            let _ = write!(
                text,
                " (bytes {}-{} of {})",
                offset,
                offset + end,
                full.len()
            );
        }
        text.push('\n');
        let body = rest[..end].trim_end();
        if body.is_empty() {
            text.push_str("(nothing to read)");
        } else {
            text.push_str(body);
        }
        if end < rest.len() {
            let mut args = format!("{{\"view\":\"{}\"", view.as_str());
            if let Some(query) = &p.query {
                let _ = write!(
                    args,
                    ",\"query\":{}",
                    serde_json::to_string(query).unwrap_or_default()
                );
            }
            if let Some(root) = &p.root {
                let _ = write!(
                    args,
                    ",\"root\":{}",
                    serde_json::to_string(root).unwrap_or_default()
                );
            }
            if main {
                args.push_str(",\"main\":true");
            }
            if let Some(tokens) = p.max_tokens {
                let _ = write!(args, ",\"maxTokens\":{tokens}");
            }
            let _ = write!(args, ",\"offset\":{}}}", offset + end);
            let _ = write!(
                text,
                "\n[truncated at {} of {}; read({args}) {}]",
                offset + end,
                full.len(),
                advice::READ_CONTINUES
            );
        }
        Ok(ToolOutput::ok(text))
    }

    /// A file a navigation brought (the latest, or the one `name` names),
    /// as text: a line saying what it is, then its content when it is text.
    fn download_text(&self, name: Option<&str>) -> Result<String, Failure> {
        let downloads = self.page.downloads();
        let chosen = match name {
            Some(name) => downloads.iter().rev().find(|d| d.name == name),
            None => downloads.last(),
        };
        let Some(file) = chosen else {
            let kept: Vec<String> = downloads.iter().map(|d| quote(&d.name)).collect();
            let message = match (name, kept.is_empty()) {
                (_, true) => "no file was downloaded in this tab's group".to_string(),
                (Some(name), false) => {
                    format!(
                        "no download is called {}; kept: {}",
                        quote(name),
                        kept.join(", ")
                    )
                }
                (None, false) => unreachable!("a list that is not empty has a last"),
            };
            return Err(Failure::new(ErrorCode::NotFound, message));
        };
        let size = crate::files::size(file.size as u64);
        let kind = if file.mime.is_empty() {
            String::new()
        } else {
            format!("{}, ", file.mime)
        };
        let mut text = format!("{} ({kind}{size})", quote(&file.name));
        if file.bytes.len() < file.size {
            let _ = write!(
                text,
                ", first {} kept",
                crate::files::size(file.bytes.len() as u64)
            );
        }
        match std::str::from_utf8(&file.bytes) {
            Ok(content) if !content.contains('\0') => {
                text.push('\n');
                text.push_str(content);
            }
            _ => text.push_str("\n(not text)"),
        }
        Ok(text)
    }

    /// One frame's document (or a subtree of it) in a read view; with
    /// `find`, the number of matches too.
    fn read_frame(
        &mut self,
        tab: u32,
        frame: catpaw_engine::FrameId,
        root: Option<catpaw_dom::NodeId>,
        view: ReadView,
        find: Option<&Matcher>,
        main: bool,
    ) -> Result<(String, usize), Failure> {
        let state = self
            .page
            .frame_state(frame)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let epoch = state.epoch;
        let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
        let mut found = 0;
        let text = agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            let scope = RefScope::new(&mut entry.refs, frame.0, epoch);
            let options = ReadOptions {
                link_style: LinkStyle::Ref,
                main_only: main,
                root,
            };
            match view {
                // Read from the page's downloads, not a document.
                ReadView::Download => String::new(),
                ReadView::Markdown => catpaw_agent::markdown(dom, &oracle, Some(scope), &options),
                ReadView::Text => catpaw_agent::text_with(
                    dom,
                    &oracle,
                    &ReadOptions {
                        link_style: LinkStyle::Url,
                        ..options
                    },
                ),
                ReadView::Links => {
                    let mut out = String::new();
                    for link in catpaw_agent::links(dom, &oracle, Some(scope)) {
                        let _ = writeln!(
                            out,
                            "{} {} {}",
                            link.r#ref.unwrap_or_default(),
                            quote(&truncate(&link.text, 100)),
                            link.href
                        );
                    }
                    out
                }
                ReadView::Forms => render_forms(&catpaw_agent::forms(dom, &oracle, Some(scope))),
                ReadView::Tables => catpaw_agent::tables(dom, &oracle, Some(scope), root),
                ReadView::Html => catpaw_agent::html(dom, &oracle, root),
                ReadView::Find => {
                    let Some(matches) = find else {
                        return String::new();
                    };
                    let (hits, total) =
                        catpaw_agent::find(dom, &oracle, Some(scope), root, matches, FIND_HITS);
                    found = total;
                    let mut out = String::new();
                    for hit in hits {
                        let _ = writeln!(
                            out,
                            "{} {}: {}",
                            hit.r#ref.unwrap_or_default(),
                            hit.role,
                            hit.context
                        );
                    }
                    if total > FIND_HITS {
                        let _ = writeln!(out, "[+{} more matches]", total - FIND_HITS);
                    }
                    out
                }
            }
        });
        Ok((text, found))
    }

    pub(crate) fn logs(&mut self, tab: u32, p: params::Logs) -> CallResult {
        let (_, state) = self.root_state(tab)?;
        let entry = self.tabs.get(&tab).expect("root_state found the tab");
        let since = match &p.since {
            Some(text) => {
                let id: u64 =
                    text.trim().trim_start_matches('s').parse().map_err(|_| {
                        Failure::bad_argument(format!("{text:?} is not a snapshot id"))
                    })?;
                let mark = entry.marks.iter().find(|m| m.id == id).copied();
                Some(mark.ok_or_else(|| {
                    let kept: Vec<String> =
                        entry.marks.iter().map(|m| format!("s{}", m.id)).collect();
                    Failure::new(
                        ErrorCode::NotFound,
                        format!(
                            "s{id} is not remembered; kept: {}",
                            if kept.is_empty() {
                                "none".to_string()
                            } else {
                                kept.join(", ")
                            }
                        ),
                    )
                })?)
            }
            None => None,
        };
        let limit = p.limit.map(|l| l as usize).unwrap_or(LOG_LIMIT).max(1);
        let pattern = p.pattern.as_ref().map(|m| m.to_lowercase());
        let matches = |text: &str| {
            pattern
                .as_ref()
                .is_none_or(|m| text.to_lowercase().contains(m))
        };
        let page_url = state.url.borrow().clone();
        let lines: Vec<String> = match p.kind {
            LogKind::Console => {
                let from = since
                    .filter(|m| m.epoch == state.epoch)
                    .map(|m| m.console)
                    .unwrap_or(0);
                let floor = p.level.unwrap_or(LogLevel::Debug);
                state
                    .console_since(from)
                    .into_iter()
                    .filter(|m| level_of(m.level) >= floor)
                    .filter(|m| matches(&m.text))
                    .map(|m| {
                        let mut text = m.text.lines().next().unwrap_or("").to_string();
                        if m.text.contains('\n') {
                            text.push_str(" …");
                        }
                        format!("{}: {}", m.level.as_str(), truncate(&text, 300))
                    })
                    .collect()
            }
            LogKind::Network => {
                let from = since.map(|m| m.requests).unwrap_or(0);
                self.page
                    .net()
                    .requests_since(from)
                    .into_iter()
                    .filter(|r| matches(r.url.as_str()))
                    .map(|r| {
                        format!(
                            "{} {} {} ({})",
                            r.method,
                            short_url(&r.url, Some(&page_url)),
                            outcome_word(&r, true),
                            kind_word(r.kind)
                        )
                    })
                    .collect()
            }
            LogKind::Events => {
                let from = since.map(|m| m.events).unwrap_or(0);
                entry
                    .events
                    .iter()
                    .skip(from)
                    .filter(|e| matches(e))
                    .cloned()
                    .collect()
            }
        };
        let total = lines.len();
        let shown = &lines[total.saturating_sub(limit)..];
        let kind = match p.kind {
            LogKind::Console => "console",
            LogKind::Network => "network",
            LogKind::Events => "events",
        };
        let mut text = format!("ok logs {kind}");
        if shown.len() < total {
            let _ = write!(text, " (last {} of {total})", shown.len());
        }
        if shown.is_empty() {
            text.push_str("\n(none)");
        }
        for line in shown {
            text.push('\n');
            text.push_str(line);
        }
        Ok(ToolOutput::ok(text))
    }
}

fn render_forms(forms: &[catpaw_agent::FormInfo]) -> String {
    let mut out = String::new();
    for form in forms {
        let _ = write!(out, "form");
        if let Some(r) = &form.r#ref {
            let _ = write!(out, " {r}");
        }
        match &form.action {
            Some(action) => {
                let _ = writeln!(out, " {} {}", form.method, action);
            }
            None => {
                let _ = writeln!(out, " (fields outside a form)");
            }
        }
        for field in &form.fields {
            out.push_str("  ");
            if let Some(r) = &field.r#ref {
                let _ = write!(out, "{r} ");
            }
            let _ = write!(out, "{}", field.kind);
            if !field.label.is_empty() {
                let _ = write!(out, " {}", quote(&truncate(&field.label, 80)));
            }
            if let Some(name) = &field.name {
                let _ = write!(out, " name={name}");
            }
            let is_button = matches!(field.kind.as_str(), "submit" | "button" | "reset");
            if !field.value.is_empty()
                && field.checked.is_none()
                && !(is_button && field.value == field.label)
            {
                let _ = write!(out, " value={}", quote(&truncate(&field.value, 80)));
            }
            if field.checked == Some(true) {
                out.push_str(" checked");
            }
            if field.required {
                out.push_str(" required");
            }
            if !field.options.is_empty() {
                let shown: Vec<String> = field
                    .options
                    .iter()
                    .take(20)
                    .map(|o| quote(&truncate(o, 40)))
                    .collect();
                let _ = write!(out, " options={}", shown.join("|"));
                if field.options.len() > 20 {
                    let _ = write!(out, "|+{}", field.options.len() - 20);
                }
            }
            out.push('\n');
        }
    }
    out
}
