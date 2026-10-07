//! `wait`: letting a page run until something happens.
//!
//! An action stops once the page has settled, leaving timers due later for
//! later. `wait` runs the page further, in short slices, checking its
//! condition after each: a loading delay of five seconds passes at once
//! when the page only waits on a timer, and in real time when it waits on
//! the network.

use std::time::{Duration, Instant};

use catpaw_agent::ReadOptions;
use catpaw_agent::snapshot::{quote, truncate};
use catpaw_agent::visibility::is_hidden;
use catpaw_engine::{FrameId, LoopLimits, SettlePolicy};
use catpaw_protocol::params::{self, WaitFor};
use catpaw_protocol::wording::{ErrorCode, advice};
use catpaw_web::agent;

use super::{GroupState, View, is_live, query, seconds};
use crate::oracle::EngineOracle;
use crate::output::{CallResult, Failure};
use crate::target::{self, Target};

/// Page time one slice of a wait may take.
const SLICE_MS: f64 = 250.0;
/// The longest a wait may last.
const MAX_WAIT_MS: u64 = 120_000;

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl GroupState {
    /// The tab's visible text, whitespace collapsed, lowercased.
    fn visible_text(&self, tab: u32) -> Result<String, Failure> {
        let (_, state) = self.root_state(tab)?;
        Ok(agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            collapse(&catpaw_agent::text_with(
                dom,
                &oracle,
                &ReadOptions::default(),
            ))
            .to_lowercase()
        }))
    }

    /// Whether a target is in the document and shown.
    fn target_visible(&mut self, tab: u32, text: &str) -> Result<bool, Failure> {
        let (frame, state) = self.root_state(tab)?;
        let node = match target::parse(text)? {
            Target::Css(selector) => query(&state, &selector)?,
            Target::Ref(text) => {
                let page = &self.page;
                let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
                entry.sync(page);
                match entry.refs.lookup(&text, |key| is_live(page, key)) {
                    Ok(key) if FrameId(key.frame) == frame => Some(key.node),
                    Ok(_) => None,
                    Err(catpaw_agent::RefError::Stale { .. }) => None,
                    Err(_) => {
                        return Err(Failure::new(
                            ErrorCode::NotFound,
                            format!("{text} was never shown in this tab"),
                        ));
                    }
                }
            }
            Target::Point(..) => {
                return Err(Failure::bad_argument("wait takes a ref or css:<selector>"));
            }
        };
        let Some(node) = node else {
            return Ok(false);
        };
        Ok(agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            dom.is_connected(node)
                && !std::iter::once(node)
                    .chain(dom.ancestors(node))
                    .any(|n| is_hidden(dom, n, &oracle))
        }))
    }

    /// Page time of the tab's document: its epoch and clock.
    fn page_time(&self, root: FrameId) -> Option<(u64, f64)> {
        self.page
            .frame_state(root)
            .map(|s| (s.epoch, s.clock.peek()))
    }

    pub(crate) fn wait(&mut self, tab: u32, p: params::Wait, view: View) -> CallResult {
        let what = match p.until {
            WaitFor::Settled => "settled".to_string(),
            WaitFor::Text => format!(
                "text {}",
                quote(
                    p.text
                        .as_deref()
                        .ok_or_else(|| Failure::bad_argument("for text needs text"))?
                )
            ),
            WaitFor::Gone => match (&p.text, &p.target) {
                (Some(text), _) => format!("gone text {}", quote(text)),
                (None, Some(target)) => format!("gone {target}"),
                (None, None) => {
                    return Err(Failure::bad_argument("for gone needs text or target"));
                }
            },
            WaitFor::Visible => format!(
                "visible {}",
                p.target
                    .as_deref()
                    .ok_or_else(|| Failure::bad_argument("for visible needs target"))?
            ),
            WaitFor::Url => format!(
                "url {}",
                quote(
                    p.url
                        .as_deref()
                        .ok_or_else(|| Failure::bad_argument("for url needs url"))?
                )
            ),
            WaitFor::Time => format!(
                "{}ms",
                p.ms.ok_or_else(|| Failure::bad_argument("for time needs ms"))?
            ),
        };
        let timeout_ms = match p.until {
            WaitFor::Time => p.ms.unwrap_or(0).min(MAX_WAIT_MS) as f64,
            _ => p.timeout_ms.unwrap_or(10_000).clamp(1, MAX_WAIT_MS) as f64,
        };
        let (root, _) = self.root_state(tab)?;
        let base = self.baseline(tab);
        let started = Instant::now();
        let mut waited = 0.0_f64;
        let mut clock = self.page_time(root);
        let needle = p.text.as_deref().map(|t| collapse(t).to_lowercase());
        let mut ran = false;
        let outcome: Result<(), bool> = loop {
            let met = match p.until {
                WaitFor::Settled => self.page.is_settled() && waited > 0.0,
                WaitFor::Text => self
                    .visible_text(tab)?
                    .contains(needle.as_deref().unwrap_or("")),
                WaitFor::Gone => match (&needle, &p.target) {
                    (Some(needle), _) => !self.visible_text(tab)?.contains(needle.as_str()),
                    (None, Some(target)) => !self.target_visible(tab, target)?,
                    (None, None) => true,
                },
                WaitFor::Visible => self.target_visible(tab, p.target.as_deref().unwrap_or(""))?,
                WaitFor::Url => self
                    .page
                    .url_of(root)
                    .is_some_and(|u| u.as_str().contains(p.url.as_deref().unwrap_or(""))),
                // An idle page has nothing for time to bring.
                WaitFor::Time => waited >= timeout_ms || (ran && !self.page.last_run_progressed()),
            };
            if met {
                break Ok(());
            }
            ran = true;
            let elapsed = waited.max(started.elapsed().as_secs_f64() * 1000.0);
            if elapsed >= timeout_ms {
                break Err(false);
            }
            let left = timeout_ms - elapsed;
            let limits = match p.until {
                WaitFor::Settled => LoopLimits {
                    wall: Duration::from_secs_f64((left / 1000.0).min(10.0)),
                    virtual_ms: left,
                    settle: Some(SettlePolicy::default()),
                    ..LoopLimits::default()
                },
                _ => LoopLimits {
                    wall: Duration::from_secs_f64((left / 1000.0).clamp(0.05, 2.0)),
                    virtual_ms: SLICE_MS.min(left),
                    settle: Some(SettlePolicy::waiting()),
                    ..LoopLimits::default()
                },
            };
            self.page.settle(&limits);
            let _ = self.page.follow_navigations();
            let (root, _) = self.root_state(tab)?;
            let now = self.page_time(root);
            match (clock, now) {
                (Some((e1, t1)), Some((e2, t2))) if e1 == e2 => waited += (t2 - t1).max(0.0),
                _ => waited += 1.0,
            }
            clock = now;
            if p.until == WaitFor::Settled {
                if self.page.is_settled() {
                    break Ok(());
                }
            } else if !self.page.last_run_progressed() && p.until != WaitFor::Time {
                // Nothing is left to happen: check once more, then give up.
                let met_now = match p.until {
                    WaitFor::Text => self
                        .visible_text(tab)?
                        .contains(needle.as_deref().unwrap_or("")),
                    WaitFor::Url => self
                        .page
                        .url_of(root)
                        .is_some_and(|u| u.as_str().contains(p.url.as_deref().unwrap_or(""))),
                    _ => false,
                };
                if !met_now && !matches!(p.until, WaitFor::Gone | WaitFor::Visible) {
                    break Err(true);
                }
                if matches!(p.until, WaitFor::Gone | WaitFor::Visible) {
                    let again = match (p.until, &needle, &p.target) {
                        (WaitFor::Gone, Some(n), _) => {
                            !self.visible_text(tab)?.contains(n.as_str())
                        }
                        (WaitFor::Gone, None, Some(t)) => !self.target_visible(tab, t)?,
                        (WaitFor::Visible, _, Some(t)) => self.target_visible(tab, t)?,
                        _ => false,
                    };
                    if !again {
                        break Err(true);
                    }
                }
            }
        };
        // The slices ran without the settle policy: a short run under it
        // lets the page finish what the change set off and says whether it
        // settled.
        if p.until != WaitFor::Settled {
            self.page.settle(&LoopLimits {
                wall: Duration::from_secs(2),
                virtual_ms: 1000.0,
                settle: Some(SettlePolicy::default()),
                ..LoopLimits::default()
            });
            let _ = self.page.follow_navigations();
        }
        let report = self.finish(tab, &base);
        match outcome {
            Ok(()) => {
                let mut status = format!("ok wait {what}");
                let waited = if p.until == WaitFor::Time {
                    timeout_ms
                } else {
                    waited
                };
                if let Some(time) = seconds(waited) {
                    status.push_str(&format!(" ({time})"));
                }
                self.page_result(tab, status, report, p.snapshot, view)
            }
            Err(idle) => {
                let message = if idle {
                    format!("{what}: not met, and the page has nothing left to do")
                } else {
                    format!(
                        "{what}: not met within {}",
                        seconds(timeout_ms).unwrap_or_else(|| format!("{timeout_ms}ms"))
                    )
                };
                let mut failure = Failure::new(ErrorCode::Timeout, truncate(&message, 200));
                for line in report.lines {
                    failure = failure.with(line);
                }
                if idle {
                    failure = failure.with(advice::IDLE);
                }
                let page = self.page_view(tab, p.snapshot, view)?;
                if !page.is_empty() {
                    failure = failure.with(page);
                }
                Err(failure)
            }
        }
    }
}
