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

use super::{GroupState, View, seconds};
use crate::oracle::EngineOracle;
use crate::output::{CallResult, Failure};

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
        self.root_state(tab)?;
        // The tab's frames are part of what it shows.
        let mut all = Vec::new();
        for frame in self.frames_of(tab) {
            let Some(state) = self.page.frame_state(frame.id).cloned() else {
                continue;
            };
            all.push(agent::with_styles(&state, |engine, dom| {
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
            }));
        }
        Ok(all.join("\n"))
    }

    /// Whether a target is in the document and shown.
    fn target_visible(&mut self, tab: u32, text: &str) -> Result<bool, Failure> {
        let aim = match self.aim(tab, text) {
            Ok(aim) => aim,
            Err(f) if matches!(f.code, ErrorCode::StaleRef | ErrorCode::NotFound) => {
                return Ok(false);
            }
            // Several shown is shown.
            Err(f) if f.code == ErrorCode::AmbiguousTarget => return Ok(true),
            Err(f) => return Err(f),
        };
        let Some(state) = self.page.frame_state(aim.frame).cloned() else {
            return Ok(false);
        };
        let node = aim.node;
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
            // The session waits for hand-offs; it never sends one here.
            WaitFor::Handoff => return Err(Failure::bad_argument("no hand-off is open")),
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
        // What the page shows, as of the last check: a page that did not
        // change gives the same answer without working it out again.
        let mut checked: Option<(Vec<u64>, bool)> = None;
        let outcome: Result<(), bool> = loop {
            let shown = matches!(p.until, WaitFor::Text | WaitFor::Gone | WaitFor::Visible);
            let version = if shown {
                self.shown_versions(tab)
            } else {
                Vec::new()
            };
            let known = checked
                .as_ref()
                .filter(|(v, _)| shown && *v == version)
                .map(|(_, met)| *met);
            let met = match (known, p.until) {
                (Some(met), _) => met,
                (None, WaitFor::Handoff) => true,
                (None, WaitFor::Settled) => self.page.is_settled_in(root) && waited > 0.0,
                (None, WaitFor::Text) => self
                    .visible_text(tab)?
                    .contains(needle.as_deref().unwrap_or("")),
                (None, WaitFor::Gone) => match (&needle, &p.target) {
                    (Some(needle), _) => !self.visible_text(tab)?.contains(needle.as_str()),
                    (None, Some(target)) => !self.target_visible(tab, target)?,
                    (None, None) => true,
                },
                (None, WaitFor::Visible) => {
                    self.target_visible(tab, p.target.as_deref().unwrap_or(""))?
                }
                (None, WaitFor::Url) => self
                    .page
                    .url_of(root)
                    .is_some_and(|u| u.as_str().contains(p.url.as_deref().unwrap_or(""))),
                // An idle page has nothing for time to bring.
                (None, WaitFor::Time) => {
                    waited.max(started.elapsed().as_secs_f64() * 1000.0) >= timeout_ms
                        || (ran && !self.page.last_run_progressed())
                }
            };
            if shown {
                checked = Some((version, met));
            }
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
                if self.page.is_settled_in(root) {
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
                let page = self.page_view(tab, p.snapshot, view, false, None)?;
                if !page.text.is_empty() {
                    failure = failure.with(page.text);
                }
                Err(failure)
            }
        }
    }
}
