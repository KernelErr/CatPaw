//! Tabs and the groups they live in: opening, routing calls, keeping
//! track of what a call opened or closed, and the `tabs` tool.

use super::*;
use router::{TabList, tab_list};

/// `t2` (or `2`) as a number.
pub(super) fn parse_tab(text: &str) -> Result<u32, Failure> {
    let t = text.trim();
    let digits = t.strip_prefix('t').unwrap_or(t);
    digits
        .parse()
        .map_err(|_| Failure::bad_argument(format!("{t:?} is not a tab id (t1, t2, ...)")))
}

pub(super) fn names(tabs: &[u32]) -> String {
    if tabs.is_empty() {
        return "no tabs".to_string();
    }
    tabs.iter()
        .map(|t| format!("t{t}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Session {
    /// Whether a tab's fields hold what the user typed in a hand-off.
    pub(super) fn holds_user_input(&self, tab: u32) -> bool {
        self.router
            .routes
            .get(&tab)
            .and_then(|g| self.router.groups.get(g))
            .and_then(|g| g.call(move |g| g.holds_user_input(tab)).ok())
            .unwrap_or(false)
    }
    /// Runs `f` on the current tab's group (see [`Session::on_tab`]).
    pub(super) fn on_current(
        &mut self,
        f: impl FnOnce(&mut GroupState, u32, View) -> CallResult + Send + 'static,
    ) -> CallResult {
        let tab = self.router.current_tab()?;
        self.on_tab(tab, f)
    }
    /// Runs `f` on the group of `tab`, then catches up with the tabs the
    /// call opened or closed.
    pub(super) fn on_tab(
        &mut self,
        tab: u32,
        f: impl FnOnce(&mut GroupState, u32, View) -> CallResult + Send + 'static,
    ) -> CallResult {
        let group_id = *self
            .router
            .routes
            .get(&tab)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let group = self
            .router
            .groups
            .get(&group_id)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let view = View {
            tabs: self.router.routes.len(),
            ..self.view
        };
        let reply = group.call(move |state| {
            let result = f(state, tab, view);
            (result, tab_list(state))
        });
        match reply {
            Ok((result, tabs)) => {
                self.reconcile(group_id, tabs);
                let note = self.router.repair_current();
                result.map(|mut output| {
                    if let Some(note) = note {
                        output.text.push('\n');
                        output.text.push_str(&note);
                    }
                    output
                })
            }
            Err(error) => {
                let lost = self.drop_group(group_id);
                let note = self.router.repair_current();
                // A group that is simply gone closed its tabs; one a call
                // took down crashed.
                let mut failure = match error {
                    EngineError::GroupClosed => Failure::new(
                        ErrorCode::NoTab,
                        format!("t{tab} is closed (with {})", names(&lost)),
                    ),
                    _ => Failure::new(
                        ErrorCode::Crashed,
                        format!("the engine failed on this page; closed {}", names(&lost)),
                    ),
                };
                if let Some(note) = note {
                    failure = failure.with(note);
                }
                Err(failure)
            }
        }
    }
    /// Brings the routes of a group up to date with its tabs, and lets go
    /// of what the tabs that closed had.
    pub(super) fn reconcile(&mut self, group: u32, tabs: TabList) {
        for tab in self.router.reconcile(group, tabs) {
            self.forget_tab(tab);
        }
    }
    /// Closes a group; returns the tabs that went with it.
    pub(super) fn drop_group(&mut self, group: u32) -> Vec<u32> {
        let lost = self.router.drop_group(group);
        for &tab in &lost {
            self.forget_tab(tab);
        }
        lost
    }
    /// A tab closed: its hand-off goes, and what it waited to have
    /// approved.
    fn forget_tab(&mut self, tab: u32) {
        self.handoffs.close_for_tab(tab);
        self.confirmations.close_for_tab(tab);
    }
    pub(super) fn tabs(&mut self, p: params::Tabs) -> CallResult {
        match p.op {
            TabsOp::List => self.list_tabs(),
            TabsOp::Switch => {
                let tab = parse_tab(
                    p.tab
                        .as_deref()
                        .ok_or_else(|| Failure::bad_argument("switch needs tab"))?,
                )?;
                if !self.router.routes.contains_key(&tab) {
                    return Err(
                        Failure::new(ErrorCode::NoTab, format!("t{tab} is not open"))
                            .with(self.list_line()),
                    );
                }
                self.router.current = Some(tab);
                let snapshot = self.on_tab(tab, move |g, tab, view| {
                    g.snapshot_text(tab, &SnapRequest::default(), view)
                        .map(ToolOutput::ok)
                })?;
                Ok(ToolOutput::ok(format!(
                    "ok switch t{tab}\n{}",
                    snapshot.text
                )))
            }
            TabsOp::Open => {
                let tab = self.router.open_group()?;
                match p.url {
                    Some(url) => {
                        let p = params::Navigate {
                            url: Some(url),
                            go: None,
                            snapshot: None,
                            confirmation: None,
                        };
                        let mut output =
                            self.on_tab(tab, move |g, tab, view| g.navigate(tab, p, view))?;
                        if let Some(rest) = output.text.strip_prefix("ok navigate") {
                            output.text = format!("ok open t{tab}{rest}");
                        }
                        Ok(output)
                    }
                    None => Ok(ToolOutput::ok(format!("ok open t{tab} about:blank"))),
                }
            }
            TabsOp::Close => {
                let tab = match &p.tab {
                    Some(text) => parse_tab(text)?,
                    None => self.router.current_tab()?,
                };
                let group_id =
                    *self.router.routes.get(&tab).ok_or_else(|| {
                        Failure::new(ErrorCode::NoTab, format!("t{tab} is not open"))
                    })?;
                let Some(group) = self.router.groups.get(&group_id) else {
                    self.drop_group(group_id);
                    return Err(Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")));
                };
                let is_top = group.call(move |g| g.is_top(tab)).unwrap_or(true);
                let mut text = format!("ok close t{tab}");
                if is_top {
                    let lost = self.drop_group(group_id);
                    for other in lost.iter().filter(|&&t| t != tab) {
                        text.push_str(&format!("\n! {} t{other}", consequence::TAB_CLOSED));
                    }
                } else {
                    let reply = group.call(move |g| (g.close_popup(tab), tab_list(g)));
                    match reply {
                        Ok((result, tabs)) => {
                            result?;
                            self.reconcile(group_id, tabs);
                        }
                        Err(_) => {
                            self.drop_group(group_id);
                        }
                    }
                }
                if let Some(note) = self.router.repair_current() {
                    text.push('\n');
                    text.push_str(&note);
                }
                Ok(ToolOutput::ok(text))
            }
        }
    }
    pub(super) fn summaries(&self) -> Vec<TabSummary> {
        let mut all = Vec::new();
        for group in self.router.groups.values() {
            if let Ok(list) = group.call(|g| g.summaries()) {
                all.extend(list);
            }
        }
        all.sort_by_key(|t| t.id);
        all
    }
    pub(super) fn list_tabs(&mut self) -> CallResult {
        let mut text = "ok tabs".to_string();
        let summaries = self.summaries();
        if summaries.is_empty() {
            text.push_str("\n(no tab is open)");
        }
        for tab in summaries {
            let mark = if Some(tab.id) == self.router.current {
                "*"
            } else {
                ""
            };
            text.push_str(&format!(
                "\nt{}{mark} {} {}",
                tab.id,
                catpaw_agent::snapshot::truncate(&tab.url, 120),
                catpaw_agent::snapshot::quote(&catpaw_agent::snapshot::truncate(&tab.title, 80))
            ));
            if let Some(opener) = tab.opener {
                text.push_str(&format!(" (opened by t{opener})"));
            }
        }
        Ok(ToolOutput::ok(text))
    }
    /// The open tabs on one line, for errors.
    pub(super) fn list_line(&self) -> String {
        let open: Vec<String> = self.router.routes.keys().map(|t| format!("t{t}")).collect();
        if open.is_empty() {
            "open tabs: none".to_string()
        } else {
            format!("open tabs: {}", open.join(", "))
        }
    }
}
