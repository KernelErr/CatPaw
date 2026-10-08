//! Tabs and the groups they live in: opening, routing calls, keeping
//! track of what a call opened or closed, and the `tabs` tool.

use super::*;

pub(super) type Group = GroupHandle<GroupState>;

/// What a group says about its tabs after a call: ids and openers.
pub(super) type TabList = Vec<(u32, Option<u32>)>;

pub(super) fn tab_list(state: &GroupState) -> TabList {
    state
        .tab_ids()
        .into_iter()
        .map(|id| (id, state.opener_of(id)))
        .collect()
}

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
    pub(super) fn place_of(&self, tab: u32) -> Option<(String, (f32, f32))> {
        let group = self.groups.get(self.routes.get(&tab)?)?;
        group
            .call(move |g| g.place(tab))
            .ok()
            .flatten()
            .map(|(url, scroll)| (url.to_string(), scroll))
    }
    pub(super) fn screen_of(&self, tab: u32) -> Option<Vec<u8>> {
        let group = self.groups.get(self.routes.get(&tab)?)?;
        group.call(move |g| g.screen(tab)).ok().flatten()
    }
    pub(super) fn current_tab(&self) -> Result<u32, Failure> {
        self.current
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, "no tab is open").with(advice::NO_TAB))
    }
    /// Opens a group with one blank tab, which becomes the current tab.
    pub(super) fn open_group(&mut self) -> Result<u32, Failure> {
        let tab = self.next_tab.fetch_add(1, Ordering::SeqCst);
        let id = self.next_group;
        self.next_group += 1;
        let options = self.options.clone();
        let net = self.net.clone();
        let next_tab = self.next_tab.clone();
        let setup = self.setup.clone();
        let group = GroupHandle::spawn(&format!("catpaw-group-{id}"), move || {
            GroupState::new(&options, &net, tab, next_tab, setup)
        })
        .map_err(|e| Failure::new(ErrorCode::Crashed, format!("could not open a tab: {e}")))?;
        self.groups.insert(id, group);
        self.routes.insert(tab, id);
        self.openers.insert(tab, None);
        self.current = Some(tab);
        Ok(tab)
    }
    /// Runs `f` on the group of `tab`, then catches up with the tabs the
    /// call opened or closed.
    pub(super) fn on_tab(
        &mut self,
        tab: u32,
        f: impl FnOnce(&mut GroupState, u32, View) -> CallResult + Send + 'static,
    ) -> CallResult {
        let group_id = *self
            .routes
            .get(&tab)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let group = self
            .groups
            .get(&group_id)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let view = View {
            tabs: self.routes.len(),
            ..self.view
        };
        let reply = group.call(move |state| {
            let result = f(state, tab, view);
            (result, tab_list(state))
        });
        match reply {
            Ok((result, tabs)) => {
                self.reconcile(group_id, tabs);
                let note = self.repair_current();
                result.map(|mut output| {
                    if let Some(note) = note {
                        output.text.push('\n');
                        output.text.push_str(&note);
                    }
                    output
                })
            }
            Err(_) => {
                let lost = self.drop_group(group_id);
                let note = self.repair_current();
                let mut failure = Failure::new(
                    ErrorCode::Crashed,
                    format!("the engine failed on this page; closed {}", names(&lost)),
                );
                if let Some(note) = note {
                    failure = failure.with(note);
                }
                Err(failure)
            }
        }
    }
    /// Brings the routes of a group up to date with its tabs.
    pub(super) fn reconcile(&mut self, group: u32, tabs: TabList) {
        let alive: Vec<u32> = tabs.iter().map(|(id, _)| *id).collect();
        let gone: Vec<u32> = self
            .routes
            .iter()
            .filter(|&(tab, g)| *g == group && !alive.contains(tab))
            .map(|(&tab, _)| tab)
            .collect();
        for tab in gone {
            self.forget_tab(tab);
        }
        for (id, opener) in tabs {
            self.routes.insert(id, group);
            self.openers.insert(id, opener);
        }
    }
    /// Closes a group; returns the tabs that went with it.
    pub(super) fn drop_group(&mut self, group: u32) -> Vec<u32> {
        self.groups.remove(&group);
        let lost: Vec<u32> = self
            .routes
            .iter()
            .filter(|&(_, g)| *g == group)
            .map(|(&tab, _)| tab)
            .collect();
        for &tab in &lost {
            self.forget_tab(tab);
        }
        lost
    }
    /// A tab closed: its route goes, and with it its hand-off and what
    /// it waited to have approved.
    fn forget_tab(&mut self, tab: u32) {
        self.routes.remove(&tab);
        self.handoffs.close_for_tab(tab);
        self.confirmations.close_for_tab(tab);
    }
    /// When the current tab closed, moves to its opener (or the first
    /// tab), and says so.
    pub(super) fn repair_current(&mut self) -> Option<String> {
        let current = self.current?;
        if self.routes.contains_key(&current) {
            return None;
        }
        let opener = self.openers.get(&current).copied().flatten();
        self.current = opener
            .filter(|t| self.routes.contains_key(t))
            .or_else(|| self.routes.keys().next().copied());
        Some(match self.current {
            Some(tab) => format!("! current tab is now t{tab}"),
            None => "! no tab is open".to_string(),
        })
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
                if !self.routes.contains_key(&tab) {
                    return Err(
                        Failure::new(ErrorCode::NoTab, format!("t{tab} is not open"))
                            .with(self.list_line()),
                    );
                }
                self.current = Some(tab);
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
                let tab = self.open_group()?;
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
                    None => self.current_tab()?,
                };
                let group_id = *self
                    .routes
                    .get(&tab)
                    .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is not open")))?;
                let Some(group) = self.groups.get(&group_id) else {
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
                if let Some(note) = self.repair_current() {
                    text.push('\n');
                    text.push_str(&note);
                }
                Ok(ToolOutput::ok(text))
            }
        }
    }
    pub(super) fn summaries(&self) -> Vec<TabSummary> {
        let mut all = Vec::new();
        for group in self.groups.values() {
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
            let mark = if Some(tab.id) == self.current {
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
        let open: Vec<String> = self.routes.keys().map(|t| format!("t{t}")).collect();
        if open.is_empty() {
            "open tabs: none".to_string()
        } else {
            format!("open tabs: {}", open.join(", "))
        }
    }
}
