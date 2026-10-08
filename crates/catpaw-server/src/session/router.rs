//! Which group runs which tab: the groups a session has open, the tabs in
//! them, which tab opened which, and the tab calls go to. It knows nothing
//! of confirmations or hand-offs: the tabs that closed are handed back for
//! the session to settle. A server for several clients would share one.

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

pub(super) struct Router {
    /// The context every group's pages share (cookies, connections).
    pub(super) net: SharedNet,
    /// What every tab's page starts with.
    pub(super) options: PageOptions,
    pub(super) setup: GroupSetup,
    pub(super) groups: BTreeMap<u32, Group>,
    next_group: u32,
    /// Tab → group.
    pub(super) routes: BTreeMap<u32, u32>,
    /// Tab → the tab that opened it.
    pub(super) openers: BTreeMap<u32, Option<u32>>,
    /// The tab calls go to.
    pub(super) current: Option<u32>,
    next_tab: Arc<AtomicU32>,
}

impl Router {
    pub(super) fn new(net: SharedNet, options: PageOptions, setup: GroupSetup) -> Self {
        Self {
            net,
            options,
            setup,
            groups: BTreeMap::new(),
            next_group: 1,
            routes: BTreeMap::new(),
            openers: BTreeMap::new(),
            current: None,
            next_tab: Arc::new(AtomicU32::new(1)),
        }
    }

    /// The group a tab lives in.
    pub(super) fn group_of(&self, tab: u32) -> Option<&Group> {
        self.groups.get(self.routes.get(&tab)?)
    }

    pub(super) fn place_of(&self, tab: u32) -> Option<(String, (f32, f32))> {
        self.group_of(tab)?
            .call(move |g| g.place(tab))
            .ok()
            .flatten()
            .map(|(url, scroll)| (url.to_string(), scroll))
    }

    pub(super) fn screen_of(&self, tab: u32) -> Option<Vec<u8>> {
        self.group_of(tab)?
            .call(move |g| g.screen(tab))
            .ok()
            .flatten()
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

    /// Brings the routes of a group up to date with its tabs; the tabs
    /// that closed.
    pub(super) fn reconcile(&mut self, group: u32, tabs: TabList) -> Vec<u32> {
        let alive: Vec<u32> = tabs.iter().map(|(id, _)| *id).collect();
        let gone: Vec<u32> = self
            .routes
            .iter()
            .filter(|&(tab, g)| *g == group && !alive.contains(tab))
            .map(|(&tab, _)| tab)
            .collect();
        for tab in &gone {
            self.routes.remove(tab);
        }
        for (id, opener) in tabs {
            self.routes.insert(id, group);
            self.openers.insert(id, opener);
        }
        gone
    }

    /// Closes a group; the tabs that went with it.
    pub(super) fn drop_group(&mut self, group: u32) -> Vec<u32> {
        self.groups.remove(&group);
        let lost: Vec<u32> = self
            .routes
            .iter()
            .filter(|&(_, g)| *g == group)
            .map(|(&tab, _)| tab)
            .collect();
        for tab in &lost {
            self.routes.remove(tab);
        }
        lost
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
}
