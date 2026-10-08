//! Confirmations: asking before a call runs, or about what it left held,
//! and carrying on once the user decided (ADR 0006, decision 8).
//!
//! A confirmation is tied to the holds (numbered navigations and requests
//! of the tab's page) the action created: approving releases those and
//! nothing else, declining or letting it run out drops those alone, and
//! one whose holds the page has since replaced or dropped no longer
//! applies.

use std::time::Duration;

use super::*;
use crate::confirm::{NewConfirmation, span};

/// The arguments of a call as one canonical string (keys sorted), so a
/// repeated call can be recognised.
pub(super) fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// A call as a confirmation remembers it: the tool and its arguments,
/// without `confirmation`.
pub(super) fn fingerprint(name: &str, arguments: &Value) -> String {
    let mut args = match arguments {
        Value::Null => Value::Object(Default::default()),
        other => other.clone(),
    };
    if let Value::Object(map) = &mut args {
        map.remove("confirmation");
    }
    format!("{name} {}", canonical(&args))
}

/// The call to repeat once the user approved: the same tool and
/// arguments, with `confirmation`.
fn repeat_call(name: &str, arguments: &Value, id: u32) -> String {
    let mut args = match arguments {
        Value::Object(map) => Value::Object(map.clone()),
        _ => json!({}),
    };
    args["confirmation"] = json!(format!("c{id}"));
    format!("{name} {}", canonical(&args))
}

/// The confirmation a re-issued call carries.
pub(super) fn confirmation_of(call: &Call) -> Option<&str> {
    match call {
        Call::Navigate(p) => p.confirmation.as_deref(),
        Call::Click(p) => p.confirmation.as_deref(),
        Call::Type(p) => p.confirmation.as_deref(),
        Call::Press(p) => p.confirmation.as_deref(),
        Call::Select(p) => p.confirmation.as_deref(),
        Call::Act(p) => p.confirmation.as_deref(),
        Call::Evaluate(p) => p.confirmation.as_deref(),
        _ => None,
    }
}

fn approval_message(what: &str) -> String {
    format!("CatPaw: allow this? {what}")
}

/// The call being run, as a confirmation it leads to remembers it.
struct CallRef<'a> {
    name: &'a str,
    arguments: &'a Value,
    fingerprint: &'a str,
}

/// `blocked <reason> cN`: final; retrying will not help.
fn blocked(reason: &str, id: u32) -> ToolOutput {
    ToolOutput::ok(format!("{} {reason} c{id}", outcome::BLOCKED))
}

impl Session {
    /// Runs a call: asks first when the policy says so, resumes a
    /// confirmed one, and asks about what the call left held.
    pub(super) fn run(
        &mut self,
        name: &str,
        arguments: &Value,
        call: Call,
        fingerprint: &str,
        host: &mut dyn Host,
    ) -> CallResult {
        if let Some(id) = confirmation_of(&call) {
            let id = id.to_string();
            return self.resume(name, arguments, &id, call, fingerprint, host);
        }
        self.refuse_handed_over(&call)?;
        self.refuse_key_upload(&call)?;
        if let Call::Wait(p) = &call
            && p.until == params::WaitFor::Handoff
        {
            return self.wait_handoff(p.clone(), host);
        }
        if let Some(what) = self.ask_first(&call)? {
            let tab = self.current_tab()?;
            let repeat = |id| repeat_call(name, arguments, id);
            let id = self.confirmations.create(NewConfirmation {
                tab,
                fingerprint: fingerprint.to_string(),
                what: what.clone(),
                action: what.clone(),
                stage: Stage::BeforeRunning,
                holds: Vec::new(),
                repeat: String::new(),
            });
            self.confirmations.set_repeat(id, repeat(id));
            self.journal(
                "confirmation",
                json!({"id": format!("c{id}"), "what": what}),
            );
            match host.approve(&approval_message(&what)) {
                Approval::Approved => self.decided(id, true, "host"),
                Approval::Declined => {
                    self.decided(id, false, "host");
                    return Ok(blocked(outcome::DECLINED, id));
                }
                Approval::Cancelled => {
                    self.decided(id, false, "host");
                    return Ok(blocked(outcome::CANCELLED, id));
                }
                Approval::Unavailable => {
                    self.confirmations.restart(id);
                    return self.needs_confirmation(id, &what, "", &repeat(id));
                }
            }
        }
        let output = self.dispatch(call)?;
        self.after_action(name, arguments, output, fingerprint, host)
    }

    /// Records a decision and forgets the confirmation.
    fn decided(&mut self, id: u32, approved: bool, via: &str) {
        self.confirmations.remove(id);
        self.journal(
            "decision",
            json!({"id": format!("c{id}"), "approved": approved, "via": via}),
        );
    }

    /// What the policy wants approved before the call runs.
    fn ask_first(&mut self, call: &Call) -> Result<Option<String>, Failure> {
        Ok(match call {
            Call::Act(p)
                if p.kind == ActKind::Upload && self.policy.upload() == Verdict::Confirm =>
            {
                let paths = p.files.clone().unwrap_or_default();
                if paths.is_empty() {
                    return Err(Failure::bad_argument(
                        "upload needs files: paths of local files",
                    ));
                }
                let files = crate::files::describe_paths(self.setup.files_root.as_deref(), &paths)?;
                let target = p.target.clone().unwrap_or_default();
                let into = self.describe_target(&target).unwrap_or(target);
                Some(format!("upload {files} into {into}"))
            }
            Call::Evaluate(p) if self.policy.evaluate() == Verdict::Confirm => Some(format!(
                "run a script in the page: {}",
                quote(&truncate(p.script.trim(), 200))
            )),
            _ => None,
        })
    }

    /// Refuses to put the approval key into a page: the agent must never
    /// hold it.
    fn refuse_key_upload(&self, call: &Call) -> Result<(), Failure> {
        let Call::Act(p) = call else {
            return Ok(());
        };
        let (ActKind::Upload, Some(paths)) = (p.kind, &p.files) else {
            return Ok(());
        };
        let Ok(key_file) = crate::confirm::key_file(self.confirmations.config()) else {
            return Ok(());
        };
        if crate::files::names_file(self.setup.files_root.as_deref(), paths, &key_file) {
            return Err(Failure::bad_argument(
                "that file is CatPaw's approval key, which never goes to a page",
            ));
        }
        Ok(())
    }

    /// The element a target names in the current tab (`e5 button
    /// "Choose file"`), when it resolves.
    fn describe_target(&mut self, target: &str) -> Option<String> {
        let tab = self.current?;
        let target = target.to_string();
        self.on_tab(tab, move |g, tab, _| {
            g.describe_target(tab, &target).map(ToolOutput::ok)
        })
        .ok()
        .map(|o| o.text)
        .filter(|text| !text.is_empty())
    }

    /// After an action: voids the confirmations whose holds the page
    /// dropped, and asks about what the action left held.
    fn after_action(
        &mut self,
        name: &str,
        arguments: &Value,
        mut output: ToolOutput,
        fingerprint: &str,
        host: &mut dyn Host,
    ) -> CallResult {
        let Some(tab) = self.current else {
            return Ok(output);
        };
        let voided = if output.dropped_holds.is_empty() {
            Vec::new()
        } else {
            self.confirmations.supersede(tab, &output.dropped_holds)
        };
        let Some(held) = output.held.take() else {
            for old in voided {
                output.text.push_str(&format!(
                    "\n! c{old} no longer applies: the page dropped what it held"
                ));
            }
            return Ok(output);
        };
        let (first, rest) = output
            .text
            .split_once('\n')
            .unwrap_or((output.text.as_str(), ""));
        let action = first.strip_prefix("ok ").unwrap_or(first).to_string();
        // Nothing happened yet: a diff that says so is left out.
        let mut rest = rest
            .lines()
            .filter(|l| !(l.starts_with("# s") && l.ends_with(" no changes")))
            .collect::<Vec<_>>()
            .join("\n");
        let what = format!("{action} would {}", held.what);
        let id = self.confirmations.create(NewConfirmation {
            tab,
            fingerprint: fingerprint.to_string(),
            what: what.clone(),
            action: action.clone(),
            stage: Stage::Held,
            holds: held.ids.clone(),
            repeat: repeat_call(name, arguments, 0),
        });
        let repeat = repeat_call(name, arguments, id);
        self.confirmations.set_repeat(id, repeat.clone());
        for old in voided {
            let note = format!("! c{old} no longer applies: c{id} replaces it");
            rest = if rest.is_empty() {
                note
            } else {
                format!("{note}\n{rest}")
            };
        }
        self.journal(
            "confirmation",
            json!({"id": format!("c{id}"), "what": what}),
        );
        match host.approve(&approval_message(&what)) {
            Approval::Approved => {
                self.decided(id, true, "host");
                let call = CallRef {
                    name,
                    arguments,
                    fingerprint,
                };
                self.release(&call, tab, id, &action, &held.ids, host)
            }
            Approval::Declined => {
                self.decided(id, false, "host");
                self.drop_holds(tab, &held.ids);
                Ok(blocked(outcome::DECLINED, id))
            }
            Approval::Cancelled => {
                self.decided(id, false, "host");
                self.drop_holds(tab, &held.ids);
                Ok(blocked(outcome::CANCELLED, id))
            }
            Approval::Unavailable => {
                self.confirmations.restart(id);
                self.needs_confirmation(id, &what, &rest, &repeat)
            }
        }
    }

    /// `needs_confirmation cN: …` with where the user approves it and the
    /// exact call to repeat.
    fn needs_confirmation(&mut self, id: u32, what: &str, rest: &str, repeat: &str) -> CallResult {
        let url = self.approval_url(id)?;
        let mut text = format!(
            "{} c{id}: {what}\n  {} {url} (expires in {}); once they approve, repeat: {repeat}",
            outcome::NEEDS_CONFIRMATION,
            outcome::ASK_USER,
            span(self.confirmations.config().lifetime),
        );
        if !rest.is_empty() {
            text.push('\n');
            text.push_str(rest);
        }
        Ok(ToolOutput::ok(text))
    }

    fn approval_url(&mut self, id: u32) -> Result<String, Failure> {
        self.local_url(&format!("/confirm/c{id}")).map_err(|e| {
            Failure::new(
                ErrorCode::Unsupported,
                format!(
                    "c{id} needs the user's approval, and the approval page could not start: {e}"
                ),
            )
        })
    }

    /// A call re-issued with `confirmation: "cN"`.
    fn resume(
        &mut self,
        name: &str,
        arguments: &Value,
        text: &str,
        call: Call,
        fingerprint: &str,
        host: &mut dyn Host,
    ) -> CallResult {
        let id: u32 = text
            .trim()
            .strip_prefix('c')
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                Failure::bad_argument(format!("{text:?} is not a confirmation (c1, c2, ...)"))
            })?;
        let Some(mut confirmation) = self.confirmations.get(id) else {
            return Err(Failure::bad_argument(format!(
                "there is no confirmation c{id} (it was used, or never asked)"
            )));
        };
        if confirmation.fingerprint != fingerprint {
            return Err(Failure::bad_argument(format!(
                "c{id} is for another call; repeat that one exactly: {}",
                confirmation.repeat
            )));
        }
        // An unanswered one waits a while for the user.
        let started = Instant::now();
        let wait = self.confirmations.config().decision_wait;
        while confirmation.state == State::Pending
            && !confirmation.expired()
            && started.elapsed() < wait
        {
            if host.pause(Duration::from_millis(250)) {
                return Err(Failure::new(
                    ErrorCode::Timeout,
                    format!("cancelled while c{id} waited for the user"),
                ));
            }
            match self.confirmations.get(id) {
                Some(now) => confirmation = now,
                None => return Err(Failure::bad_argument(format!("c{id} is gone"))),
            }
        }
        let drop = |session: &mut Self| {
            session.confirmations.remove(id);
            if confirmation.stage == Stage::Held {
                session.drop_holds(confirmation.tab, &confirmation.holds);
            }
        };
        if confirmation.state == State::Pending && confirmation.expired() {
            drop(self);
            return Ok(ToolOutput::ok(format!(
                "{} {}: c{id} was not approved in time",
                outcome::BLOCKED,
                outcome::EXPIRED
            )));
        }
        match confirmation.state {
            State::Pending => {
                let url = self.approval_url(id)?;
                Ok(ToolOutput::ok(format!(
                    "{} c{id} {}: {}\n  {} {url}; once they approve, repeat: {}",
                    outcome::NEEDS_CONFIRMATION,
                    outcome::STILL_PENDING,
                    confirmation.what,
                    outcome::ASK_USER,
                    confirmation.repeat
                )))
            }
            // The page journaled the decision when the user made it.
            State::Declined => {
                drop(self);
                Ok(blocked(outcome::DECLINED, id))
            }
            State::Superseded => {
                self.confirmations.remove(id);
                Ok(ToolOutput::ok(format!(
                    "{} {}: c{id} no longer applies (the page replaced or dropped what it held)",
                    outcome::BLOCKED,
                    outcome::SUPERSEDED
                )))
            }
            State::Approved => {
                self.confirmations.remove(id);
                match confirmation.stage {
                    Stage::Held => {
                        let call = CallRef {
                            name,
                            arguments,
                            fingerprint,
                        };
                        self.release(
                            &call,
                            confirmation.tab,
                            id,
                            &confirmation.action,
                            &confirmation.holds,
                            host,
                        )
                    }
                    Stage::BeforeRunning => {
                        // On the tab where it was asked, whichever is current.
                        if !self.routes.contains_key(&confirmation.tab) {
                            return Err(Failure::new(
                                ErrorCode::NoTab,
                                format!("t{}, where c{id} was asked, is closed", confirmation.tab),
                            ));
                        }
                        let previous = self.current.replace(confirmation.tab);
                        let result = self.dispatch(call).and_then(|output| {
                            self.after_action(name, arguments, output, fingerprint, host)
                        });
                        if let Some(previous) = previous
                            && previous != confirmation.tab
                            && self.routes.contains_key(&previous)
                        {
                            self.current = Some(previous);
                        }
                        result
                    }
                }
            }
        }
    }

    /// Lets the holds an approved confirmation was about go. What they lead
    /// to may be held in turn: that is asked about as part of the same
    /// call.
    fn release(
        &mut self,
        call: &CallRef<'_>,
        tab: u32,
        id: u32,
        action: &str,
        holds: &[u64],
        host: &mut dyn Host,
    ) -> CallResult {
        let status = format!("ok {action} (confirmed c{id})");
        let ids = holds.to_vec();
        let output = self.on_tab(tab, move |g, tab, view| {
            g.release_holds(tab, &ids, status, view)
                .map(|output| output.unwrap_or_default())
        })?;
        if output.text.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "{} {}: c{id} no longer applies (the page replaced or dropped what it held)",
                outcome::BLOCKED,
                outcome::SUPERSEDED
            )));
        }
        self.after_action(call.name, call.arguments, output, call.fingerprint, host)
    }

    /// Drops what a confirmation held.
    fn drop_holds(&mut self, tab: u32, holds: &[u64]) {
        if holds.is_empty() {
            return;
        }
        let ids = holds.to_vec();
        let _ = self.on_tab(tab, move |g, _, _| {
            g.drop_holds(&ids);
            Ok(ToolOutput::default())
        });
    }
}
