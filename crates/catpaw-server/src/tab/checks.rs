//! Before an action: whether its element can take it (enabled, not
//! moving); after a failed one, the error the agent gets, with what to
//! try next.

use catpaw_engine::{ActionError, FrameId, InputError, LoopLimits};
use catpaw_protocol::wording::{ErrorCode, advice};
use catpaw_web::agent;

use super::report::{Report, with_consequences};
use super::{Aim, GroupState};
use crate::output::Failure;

impl GroupState {
    /// The checks before acting on an element: enabled (when the action
    /// needs it), and holding still while the page animates.
    pub(super) fn actionable(&mut self, tab: u32, aim: &Aim, enabled: bool) -> Result<(), Failure> {
        let what = self.aimed(tab, aim);
        // The document is let go of before the page runs again: a
        // navigation meanwhile does not keep it alive.
        let mut last = {
            let Some(state) = self.page.frame_state(aim.frame).cloned() else {
                return Ok(());
            };
            if enabled && agent::is_disabled(&state, aim.node) {
                return Err(
                    Failure::new(ErrorCode::NotActionable, format!("{what} is disabled"))
                        .with(advice::DISABLED),
                );
            }
            let animating = self.page.pending_of(aim.frame).is_some_and(|p| p.animating);
            if !animating {
                return Ok(());
            }
            agent::element_rect(&state, aim.node)
        };
        for _ in 0..10 {
            self.page.settle(&LoopLimits {
                wall: std::time::Duration::from_millis(500),
                virtual_ms: 17.0,
                settle: None,
                ..LoopLimits::default()
            });
            let Some(state) = self.page.frame_state(aim.frame).cloned() else {
                return Ok(());
            };
            let now = agent::element_rect(&state, aim.node);
            if now == last {
                return Ok(());
            }
            last = now;
        }
        Err(
            Failure::new(ErrorCode::NotActionable, format!("{what} keeps moving"))
                .with(advice::MOVING),
        )
    }

    /// A control in or around what covers an element that would dismiss
    /// it: a close, accept or "no thanks" button.
    fn dismiss_hint(&mut self, tab: u32, frame: FrameId, cover: catpaw_dom::NodeId) -> Option<u32> {
        const WORDS: &[&str] = &[
            "close",
            "dismiss",
            "accept",
            "agree",
            "got it",
            "ok",
            "no thanks",
            "reject",
            "decline",
            "continue",
            "allow",
            "×",
            "✕",
            "✖",
        ];
        let state = self.page.frame_state(frame)?.clone();
        let found = agent::with_styles(&state, |engine, dom| {
            let oracle = crate::oracle::EngineOracle {
                engine,
                page: &state,
            };
            for around in std::iter::once(cover).chain(dom.ancestors(cover)).take(6) {
                for n in std::iter::once(around).chain(dom.descendants(around)) {
                    let role = catpaw_agent::a11y::role_for(dom, n);
                    if !matches!(role, Some("button" | "link")) {
                        continue;
                    }
                    let label = dom
                        .attr(n, "aria-label")
                        .map(str::to_string)
                        .unwrap_or_else(|| catpaw_agent::a11y::subtree_text(dom, n, &oracle))
                        .trim()
                        .to_lowercase();
                    let fits = label == "x"
                        || WORDS
                            .iter()
                            .any(|w| label == *w || (label.len() <= 30 && label.contains(w)));
                    if fits && !catpaw_agent::visibility::is_hidden(dom, n, &oracle) {
                        return Some(n);
                    }
                }
            }
            None
        })?;
        self.ref_for(tab, frame, found)
    }

    /// Words an input error about the aimed-at element.
    pub(super) fn action_failure(
        &mut self,
        tab: u32,
        aim: &Aim,
        error: ActionError,
        report: Report,
    ) -> Failure {
        let what = self.aimed(tab, aim);
        let mut failure = match error {
            ActionError::Input(InputError::Detached) => {
                Failure::new(ErrorCode::StaleRef, format!("{what} (removed)")).with(advice::STALE)
            }
            ActionError::Input(InputError::NotVisible) => {
                Failure::new(ErrorCode::NotActionable, format!("{what} is not visible"))
                    .with(advice::NOT_VISIBLE)
            }
            ActionError::Input(InputError::NotEditable) => Failure::new(
                ErrorCode::NotActionable,
                format!("{what} does not take this input"),
            )
            .with(advice::NOT_EDITABLE),
            ActionError::Input(InputError::Disabled) => {
                Failure::new(ErrorCode::NotActionable, format!("{what} is disabled"))
                    .with(advice::DISABLED)
            }
            ActionError::Input(InputError::Occluded { by }) => {
                let cover = self.ref_for(tab, aim.frame, by);
                let cover = cover
                    .map(|r| self.describe(tab, r))
                    .unwrap_or_else(|| "another element".to_string());
                let failure =
                    Failure::new(ErrorCode::Occluded, format!("{what} is covered by {cover}"));
                match self.dismiss_hint(tab, aim.frame, by) {
                    Some(r) => failure
                        .with(format!("maybe dismiss it with {}", self.describe(tab, r)))
                        .with(advice::OCCLUDED),
                    None => failure.with(advice::OCCLUDED),
                }
            }
            ActionError::Input(InputError::TooManyFiles) => {
                Failure::bad_argument(format!("{what} takes one file; pass one path"))
            }
            ActionError::NoFrame(_) => {
                Failure::new(ErrorCode::StaleRef, format!("{what} (frame closed)"))
                    .with(advice::STALE)
            }
            other => Failure::new(ErrorCode::NavigationFailed, other.to_string()),
        };
        failure = with_consequences(failure, report.lines);
        failure
    }
}
