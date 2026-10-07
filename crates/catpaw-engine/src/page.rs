//! One page: fetch the document, parse it while running its scripts, and
//! drive the event loop until the page settles.
//!
//! A page is a tree of frames. Each frame is a page state and script
//! realm of its own (see `catpaw_web::frames`), all on this thread; the
//! engine opens the frames the documents ask for, runs their event loops
//! in turns, and carries messages between them.

use std::cell::{Ref, RefCell};
use std::rc::Rc;
use std::time::Instant;

use catpaw_bindings_boa::BoaPage;
use catpaw_dom::Dom;
use catpaw_dom::NodeId;
use catpaw_fetch::{FetchedDocument, fetch_document, fetch_document_with};
use catpaw_js::Value;
use catpaw_net::{NetConfig, NetError};
use catpaw_web::event_loop::{self, LoopLimits, LoopReport, StopReason};
use catpaw_web::frames::{self, FrameCommand, FrameId, FrameTree};
use catpaw_web::generated::DocumentReadyState;
use catpaw_web::page::NavigationRequest;
use catpaw_web::workers::{self, WorkerCommand, WorkerId};
use catpaw_web::{ConsoleLevel, PageConfig, PageState, promises, scripting};
use url::Url;

use crate::net::EngineNet;

/// How many frames a page may have besides the top one.
pub const MAX_FRAMES: usize = 32;
/// How deep frames may nest (the top page is at depth 0).
pub const MAX_FRAME_DEPTH: u32 = 8;
/// How many workers a page (frames included) may run at once.
pub const MAX_WORKERS: usize = 16;
/// Virtual time one frame or worker may advance before the others get a
/// turn.
const FRAME_SLICE_MS: f64 = 1000.0;

/// The stack of a page thread. Deeply nested documents and scripts recurse
/// in native code; the memory is only committed as it is used.
pub const PAGE_STACK_SIZE: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PageOptions {
    pub net: NetConfig,
    /// The page's view of itself. Its user agent is taken from `net`.
    pub page: PageConfig,
    /// Bounds on the event loop run that follows each document load.
    pub limits: LoopLimits,
    /// How many script-initiated navigations (`location.href = ...`) to
    /// follow before giving up.
    pub max_navigations: usize,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            net: NetConfig::default(),
            page: PageConfig::default(),
            limits: LoopLimits::default(),
            max_navigations: 5,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("script engine: {0}")]
    Script(String),
    #[error("could not start the page thread: {0}")]
    Thread(std::io::Error),
    #[error("the page thread panicked")]
    Panicked,
}

/// Facts about the response the current document was parsed from.
#[derive(Clone, Debug)]
pub struct DocumentInfo {
    /// The document URL (after redirects).
    pub url: Url,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub mime: Option<String>,
    pub encoding: &'static str,
    pub body_bytes: usize,
    pub redirects: usize,
    pub cloudflare_challenge: bool,
}

impl DocumentInfo {
    fn from_fetch(doc: &FetchedDocument) -> Self {
        let response = &doc.response;
        Self {
            url: response.url.clone(),
            status: response.status.as_u16(),
            headers: response
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            mime: response.mime_essence(),
            encoding: doc.decoded.encoding,
            body_bytes: response.body.len(),
            redirects: response.redirect_chain.len(),
            cloudflare_challenge: response.is_cloudflare_challenge(),
        }
    }

    fn local(url: &Url, html: &str) -> Self {
        Self {
            url: url.clone(),
            status: 200,
            headers: Vec::new(),
            mime: Some("text/html".to_string()),
            encoding: "UTF-8",
            body_bytes: html.len(),
            redirects: 0,
            cloudflare_challenge: false,
        }
    }
}

/// A frame below the top one.
struct Frame {
    id: FrameId,
    parent: FrameId,
    /// The `iframe` element in the parent's document.
    element: NodeId,
    depth: u32,
    // Dropped before `net`: the page state refers to it.
    boa: BoaPage,
    #[allow(dead_code)]
    net: Rc<EngineNet>,
    /// The serialized origin its messages carry (`srcdoc` and blank
    /// frames take their parent's).
    origin: String,
    report: LoopReport,
    virtual_used: f64,
    /// Whether the parent has been told the frame loaded.
    load_reported: bool,
    navigations: usize,
}

/// A script realm the engine runs: the top page, a frame, or a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeId {
    Frame(FrameId),
    Worker(u32),
}

/// A running dedicated worker.
struct WorkerRun {
    key: u32,
    /// The scope that made it.
    owner: ScopeId,
    /// Its number in the owner's page.
    local: WorkerId,
    // Dropped before `net`: the page state refers to it.
    boa: BoaPage,
    #[allow(dead_code)]
    net: Rc<EngineNet>,
    report: LoopReport,
    virtual_used: f64,
    /// How many of the scope's uncaught errors were relayed to the owner.
    errors_seen: usize,
}

/// A worker of a page, as the embedder sees it.
#[derive(Clone, Debug)]
pub struct WorkerInfo {
    pub key: u32,
    pub owner: ScopeId,
    pub url: Url,
}

/// A frame of a page, as the embedder sees it.
#[derive(Clone, Debug)]
pub struct FrameInfo {
    pub id: FrameId,
    pub parent: Option<FrameId>,
    pub url: Url,
    pub depth: u32,
}

/// A loaded page. Lives on the thread that created it.
pub struct Page {
    // Dropped before `net`: the page state refers to it.
    boa: BoaPage,
    /// The frames below the top one, in opening order.
    frames: Vec<Frame>,
    /// The tree the frames' pages share.
    tree: Rc<RefCell<FrameTree>>,
    /// The dedicated workers of the page and its frames.
    workers: Vec<WorkerRun>,
    next_worker: u32,
    /// The frame actions and evaluations address.
    current: FrameId,
    net: Rc<EngineNet>,
    document: DocumentInfo,
    /// The documents loaded on the way here, oldest first.
    navigations: Vec<Url>,
    options: PageOptions,
    report: LoopReport,
    top_virtual_used: f64,
}

/// Why an action could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("no element matches {0:?}")]
    NotFound(String),
    #[error("{0:?} is not a valid selector")]
    BadSelector(String),
    #[error("{0}")]
    Input(#[from] catpaw_web::input::InputError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("no frame is open for {0:?}")]
    NoFrame(String),
}

/// Where a document goes in the frame tree.
struct Placement {
    frame: FrameId,
    tree: Rc<RefCell<FrameTree>>,
    viewport: Option<(u32, u32)>,
}

/// Parses `html` as the document at `info.url` in a new realm, running its
/// scripts. The event loop is left to the frame scheduler.
fn load(
    net: &Rc<EngineNet>,
    info: &DocumentInfo,
    html: &str,
    referrer: Option<&Url>,
    options: &PageOptions,
    placement: Placement,
) -> Result<BoaPage, EngineError> {
    let mut config = options.page.clone();
    config.user_agent = options.net.user_agent.clone();
    if let Some((width, height)) = placement.viewport {
        config.viewport_width = width;
        config.viewport_height = height;
    }
    let state = Rc::new(PageState::new(info.url.clone(), config));
    state.set_net(net.clone());
    state.frames.place(placement.frame, placement.tree);
    {
        let mut document = state.document_state.borrow_mut();
        document.charset = info.encoding.to_string();
        if let Some(mime) = &info.mime {
            document.content_type = mime.clone();
        }
        if let Some(referrer) = referrer {
            document.referrer = referrer.to_string();
        }
    }
    let mut boa = BoaPage::new(state).map_err(EngineError::Script)?;
    boa.with_cx(|cx| scripting::load_document(cx, html));
    Ok(boa)
}

fn idle_report() -> LoopReport {
    LoopReport {
        stop: StopReason::Idle,
        steps: 0,
        virtual_advanced_ms: 0.0,
        pending_timers: 0,
        inflight_requests: 0,
    }
}

fn top_placement(tree: &Rc<RefCell<FrameTree>>) -> Placement {
    Placement {
        frame: FrameId(0),
        tree: tree.clone(),
        viewport: None,
    }
}

/// Whether a run of the event loop did anything.
fn progressed(report: &LoopReport) -> bool {
    report.steps > 0 || report.virtual_advanced_ms > 0.0 || report.stop == StopReason::Navigation
}

impl Page {
    /// Fetches `url` and loads it, following script-initiated navigations.
    ///
    /// Blocks the calling thread, which must not be inside an async runtime;
    /// see [`with_page`].
    pub fn open(url: &Url, options: &PageOptions) -> Result<Self, EngineError> {
        let net = Rc::new(EngineNet::new(options.net.clone())?);
        let fetched = net.block_on(fetch_document(net.client(), url))?;
        let info = DocumentInfo::from_fetch(&fetched);
        let tree = Rc::new(RefCell::new(FrameTree::default()));
        let boa = load(
            &net,
            &info,
            fetched.html(),
            None,
            options,
            top_placement(&tree),
        )?;
        let mut page = Self::with_top(boa, tree, net, info, options);
        page.run_frames();
        page.follow_navigations()?;
        Ok(page)
    }

    fn with_top(
        boa: BoaPage,
        tree: Rc<RefCell<FrameTree>>,
        net: Rc<EngineNet>,
        info: DocumentInfo,
        options: &PageOptions,
    ) -> Self {
        Self {
            boa,
            frames: Vec::new(),
            tree,
            workers: Vec::new(),
            next_worker: 1,
            current: FrameId(0),
            net,
            document: info.clone(),
            navigations: vec![info.url],
            options: options.clone(),
            report: idle_report(),
            top_virtual_used: 0.0,
        }
    }

    /// Loads the document a navigation request asks for, in place of the
    /// current one: the same network (cookies included), a new page.
    fn navigate(&mut self, request: NavigationRequest) -> Result<(), EngineError> {
        let referrer = self.url();
        let fetched = self.net.block_on(fetch_document_with(
            self.net.client(),
            &request.method,
            &request.url,
            request.body,
            Some(&referrer),
        ))?;
        let info = DocumentInfo::from_fetch(&fetched);
        let tree = Rc::new(RefCell::new(FrameTree::default()));
        let boa = load(
            &self.net,
            &info,
            fetched.html(),
            Some(&referrer),
            &self.options,
            top_placement(&tree),
        )?;
        // The frames and workers belonged to the old document.
        self.workers.clear();
        self.frames.clear();
        self.tree = tree;
        self.current = FrameId(0);
        self.boa = boa;
        self.top_virtual_used = 0.0;
        self.navigations.push(info.url.clone());
        self.document = info;
        self.run_frames();
        Ok(())
    }

    /// Follows the navigations the page asks for (links, form submissions,
    /// `location` assignments), up to the configured number.
    pub fn follow_navigations(&mut self) -> Result<(), EngineError> {
        loop {
            let requested = self.boa.page().navigation.borrow_mut().take();
            match requested {
                Some(request)
                    if !request.reload
                        && self.navigations.len() <= self.options.max_navigations =>
                {
                    self.navigate(request)?;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Loads `html` as the document at `url` without fetching it. Scripts
    /// and other subresources are still fetched from the network.
    pub fn from_html(url: &Url, html: &str, options: &PageOptions) -> Result<Self, EngineError> {
        let net = Rc::new(EngineNet::new(options.net.clone())?);
        let info = DocumentInfo::local(url, html);
        let tree = Rc::new(RefCell::new(FrameTree::default()));
        let boa = load(&net, &info, html, None, options, top_placement(&tree))?;
        let mut page = Self::with_top(boa, tree, net, info, options);
        page.run_frames();
        page.follow_navigations()?;
        Ok(page)
    }

    /// The top frame's state.
    pub fn state(&self) -> &Rc<PageState> {
        self.boa.page()
    }

    // ---- frames -----------------------------------------------------------

    fn frame(&self, id: FrameId) -> Option<&Frame> {
        self.frames.iter().find(|f| f.id == id)
    }

    fn frame_mut(&mut self, id: FrameId) -> Option<&mut Frame> {
        self.frames.iter_mut().find(|f| f.id == id)
    }

    /// The state of a frame, the top one for `FrameId(0)`.
    fn page_of(&self, id: FrameId) -> Option<&Rc<PageState>> {
        if id == FrameId(0) {
            Some(self.boa.page())
        } else {
            self.frame(id).map(|f| f.boa.page())
        }
    }

    fn boa_of(&mut self, id: FrameId) -> Option<&mut BoaPage> {
        if id == FrameId(0) {
            Some(&mut self.boa)
        } else {
            self.frame_mut(id).map(|f| &mut f.boa)
        }
    }

    fn origin_of(&self, id: FrameId) -> String {
        match self.frame(id) {
            Some(frame) => frame.origin.clone(),
            None => frames::origin_of(&self.url()),
        }
    }

    fn depth_of(&self, id: FrameId) -> u32 {
        self.frame(id).map(|f| f.depth).unwrap_or(0)
    }

    /// The state of a frame's page, the top one for `FrameId(0)`.
    pub fn frame_state(&self, id: FrameId) -> Option<&Rc<PageState>> {
        self.page_of(id)
    }

    /// How the most recent event loop run of a frame ended.
    pub fn frame_report(&self, id: FrameId) -> Option<&LoopReport> {
        if id == FrameId(0) {
            Some(&self.report)
        } else {
            self.frame(id).map(|f| &f.report)
        }
    }

    /// The frames of the page, the top one first.
    pub fn frames(&self) -> Vec<FrameInfo> {
        let mut out = vec![FrameInfo {
            id: FrameId(0),
            parent: None,
            url: self.url(),
            depth: 0,
        }];
        out.extend(self.frames.iter().map(|f| FrameInfo {
            id: f.id,
            parent: Some(f.parent),
            url: f.boa.page().url.borrow().clone(),
            depth: f.depth,
        }));
        out
    }

    /// The frame actions and evaluations address; the top one at first.
    pub fn current_frame(&self) -> FrameId {
        self.current
    }

    /// Addresses the frame of the first `iframe` matching `selector` in
    /// the current frame's document.
    pub fn select_frame(&mut self, selector: &str) -> Result<FrameId, ActionError> {
        let el = self.find(selector)?;
        let frame = self
            .page_of(self.current)
            .and_then(|page| page.frames.child_of(el))
            .filter(|id| self.frame(*id).is_some())
            .ok_or_else(|| ActionError::NoFrame(selector.to_string()))?;
        self.current = frame;
        Ok(frame)
    }

    /// Addresses the parent of the current frame.
    pub fn select_parent_frame(&mut self) -> FrameId {
        self.current = self
            .frame(self.current)
            .map(|f| f.parent)
            .unwrap_or(FrameId(0));
        self.current
    }

    /// Addresses the top frame again.
    pub fn select_top_frame(&mut self) {
        self.current = FrameId(0);
    }

    /// Opens the frame `id` for `element` in the frame `parent`.
    fn open_frame(
        &mut self,
        parent: FrameId,
        id: FrameId,
        element: NodeId,
        url: Option<Url>,
        srcdoc: Option<String>,
    ) {
        let Some(parent_page) = self.page_of(parent).cloned() else {
            return;
        };
        if parent_page.frames.child_of(element) != Some(id) {
            // Pointed elsewhere or removed since: a later command covers it.
            return;
        }
        let depth = self.depth_of(parent) + 1;
        if depth > MAX_FRAME_DEPTH || self.frames.len() >= MAX_FRAMES {
            parent_page.log(
                ConsoleLevel::Warn,
                format!(
                    "Not loading a frame for {}: too many frames",
                    describe_frame(&url, &srcdoc)
                ),
            );
            frames::frame_failed(&parent_page, element);
            return;
        }
        let referrer = parent_page.url.borrow().clone();
        let parent_origin = self.origin_of(parent);
        let (info, html, origin) = match (srcdoc, url) {
            (Some(html), _) => {
                let url = Url::parse("about:srcdoc").expect("about:srcdoc parses");
                (DocumentInfo::local(&url, &html), html, parent_origin)
            }
            (None, Some(url)) => {
                let fetched = self.net.block_on(fetch_document_with(
                    self.net.client(),
                    "GET",
                    &url,
                    None,
                    Some(&referrer),
                ));
                match fetched {
                    Ok(fetched) => {
                        let info = DocumentInfo::from_fetch(&fetched);
                        self.net.record_document("GET", &url, Some(info.status));
                        let origin = frames::origin_of(&info.url);
                        (info, fetched.html().to_string(), origin)
                    }
                    Err(e) => {
                        self.net.record_document("GET", &url, None);
                        parent_page.log(
                            ConsoleLevel::Error,
                            format!("Failed to load frame {url}: {e}"),
                        );
                        frames::frame_failed(&parent_page, element);
                        return;
                    }
                }
            }
            (None, None) => {
                let url = Url::parse("about:blank").expect("about:blank parses");
                (DocumentInfo::local(&url, ""), String::new(), parent_origin)
            }
        };
        let placement = Placement {
            frame: id,
            tree: self.tree.clone(),
            viewport: Some(frames::frame_viewport(&parent_page, element)),
        };
        let net = Rc::new(self.net.child());
        let boa = match load(
            &net,
            &info,
            &html,
            Some(&referrer),
            &self.options,
            placement,
        ) {
            Ok(boa) => boa,
            Err(e) => {
                parent_page.log(ConsoleLevel::Error, format!("Failed to open frame: {e}"));
                frames::frame_failed(&parent_page, element);
                return;
            }
        };
        self.frames.push(Frame {
            id,
            parent,
            element,
            depth,
            boa,
            net,
            origin,
            report: idle_report(),
            virtual_used: 0.0,
            load_reported: false,
            navigations: 0,
        });
    }

    /// Closes a frame and the frames inside it.
    fn close_frame(&mut self, id: FrameId) {
        let inner: Vec<FrameId> = self
            .frames
            .iter()
            .filter(|f| f.parent == id)
            .map(|f| f.id)
            .collect();
        for child in inner {
            self.close_frame(child);
        }
        self.tree.borrow_mut().remove(id);
        self.drop_workers_of(ScopeId::Frame(id));
        if let Some(index) = self.frames.iter().position(|f| f.id == id) {
            let frame = self.frames.remove(index);
            if self.current == id {
                self.current = frame.parent;
            }
        }
    }

    // ---- workers ----------------------------------------------------------

    /// The page state of a scope.
    fn scope_page(&self, scope: ScopeId) -> Option<&Rc<PageState>> {
        match scope {
            ScopeId::Frame(id) => self.page_of(id),
            ScopeId::Worker(key) => self
                .workers
                .iter()
                .find(|w| w.key == key)
                .map(|w| w.boa.page()),
        }
    }

    fn scope_boa(&mut self, scope: ScopeId) -> Option<&mut BoaPage> {
        match scope {
            ScopeId::Frame(id) => self.boa_of(id),
            ScopeId::Worker(key) => self
                .workers
                .iter_mut()
                .find(|w| w.key == key)
                .map(|w| &mut w.boa),
        }
    }

    /// Every scope: the top page first, then frames, then workers.
    fn scopes(&self) -> Vec<ScopeId> {
        std::iter::once(ScopeId::Frame(FrameId(0)))
            .chain(self.frames.iter().map(|f| ScopeId::Frame(f.id)))
            .chain(self.workers.iter().map(|w| ScopeId::Worker(w.key)))
            .collect()
    }

    fn worker_key(&self, owner: ScopeId, local: WorkerId) -> Option<u32> {
        self.workers
            .iter()
            .find(|w| w.owner == owner && w.local == local)
            .map(|w| w.key)
    }

    /// Drops a worker and the workers it made.
    fn drop_worker(&mut self, key: u32) {
        self.drop_workers_of(ScopeId::Worker(key));
        self.workers.retain(|w| w.key != key);
    }

    fn drop_workers_of(&mut self, owner: ScopeId) {
        let owned: Vec<u32> = self
            .workers
            .iter()
            .filter(|w| w.owner == owner)
            .map(|w| w.key)
            .collect();
        for key in owned {
            self.drop_worker(key);
        }
    }

    /// The workers of the page, oldest first.
    pub fn workers(&self) -> Vec<WorkerInfo> {
        self.workers
            .iter()
            .map(|w| WorkerInfo {
                key: w.key,
                owner: w.owner,
                url: w.boa.page().url.borrow().clone(),
            })
            .collect()
    }

    /// The state of a worker's global scope.
    pub fn worker_state(&self, key: u32) -> Option<&Rc<PageState>> {
        self.scope_page(ScopeId::Worker(key))
    }

    pub fn worker_report(&self, key: u32) -> Option<&LoopReport> {
        self.workers
            .iter()
            .find(|w| w.key == key)
            .map(|w| &w.report)
    }

    /// Starts the worker `local` of `owner` with the script at `url`.
    fn spawn_worker(
        &mut self,
        owner: ScopeId,
        local: WorkerId,
        url: Url,
        name: String,
        module: bool,
    ) {
        let Some(owner_page) = self.scope_page(owner).cloned() else {
            return;
        };
        if self.workers.len() >= MAX_WORKERS {
            workers::worker_error(
                &owner_page,
                local,
                format!("Not starting worker {url}: too many workers"),
                true,
            );
            return;
        }
        let fetched = match catpaw_web::net::local_response(&owner_page, &url) {
            Some(Ok(response)) => Ok((response.url, response.body)),
            Some(Err(e)) => Err(e),
            None => {
                let result = self.net.block_on(self.net.client().get(&url));
                self.net.record_document(
                    "GET",
                    &url,
                    result.as_ref().ok().map(|r| r.status.as_u16()),
                );
                match result {
                    Ok(response) => Ok((response.url, response.body.to_vec())),
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        let (script_url, body) = match fetched {
            Ok(fetched) => fetched,
            Err(e) => {
                workers::worker_error(
                    &owner_page,
                    local,
                    format!("Failed to load worker script {url}: {e}"),
                    true,
                );
                return;
            }
        };
        let mut config = self.options.page.clone();
        config.user_agent = self.options.net.user_agent.clone();
        let mut state = PageState::new(script_url.clone(), config);
        // One object-URL store per origin: the worker resolves the blob
        // URLs its owner makes.
        state.blob_urls = owner_page.blob_urls.clone();
        let state = Rc::new(state);
        let net = Rc::new(self.net.child());
        state.set_net(net.clone());
        state.workers.set_role(local, name);
        let mut boa = match BoaPage::new_worker(state) {
            Ok(boa) => boa,
            Err(e) => {
                workers::worker_error(
                    &owner_page,
                    local,
                    format!("Failed to start worker: {e}"),
                    true,
                );
                return;
            }
        };
        let source = String::from_utf8_lossy(&body).into_owned();
        boa.with_cx(|cx| workers::run_script(cx, &source, &script_url, module));
        let key = self.next_worker;
        self.next_worker += 1;
        self.workers.push(WorkerRun {
            key,
            owner,
            local,
            boa,
            net,
            report: idle_report(),
            virtual_used: 0.0,
            errors_seen: 0,
        });
    }

    /// Carries out what the scopes asked of their workers. Returns whether
    /// anything was done.
    fn pump_workers(&mut self) -> bool {
        let mut did = false;
        for scope in self.scopes() {
            let Some(page) = self.scope_page(scope).cloned() else {
                continue;
            };
            for command in page.workers.take_commands() {
                did = true;
                match command {
                    WorkerCommand::Spawn {
                        worker,
                        url,
                        name,
                        module,
                    } => self.spawn_worker(scope, worker, url, name, module),
                    WorkerCommand::PostMessage { worker, data } => {
                        if let Some(key) = self.worker_key(scope, worker)
                            && let Some(target) = self.scope_page(ScopeId::Worker(key))
                        {
                            workers::deliver_to_worker(target, data);
                        }
                    }
                    WorkerCommand::Terminate { worker } => {
                        if let Some(key) = self.worker_key(scope, worker) {
                            self.drop_worker(key);
                        }
                    }
                    WorkerCommand::ToOwner { data } => {
                        if let ScopeId::Worker(key) = scope
                            && let Some(run) = self.workers.iter().find(|w| w.key == key)
                            && let Some(owner) = self.scope_page(run.owner)
                        {
                            workers::deliver_to_owner(owner, run.local, data);
                        }
                    }
                    WorkerCommand::Close => {
                        if let ScopeId::Worker(key) = scope
                            && let Some(run) = self.workers.iter().find(|w| w.key == key)
                            && let Some(owner) = self.scope_page(run.owner)
                        {
                            workers::worker_ended(owner, run.local);
                            self.drop_worker(key);
                        }
                    }
                }
            }
            // Uncaught errors in a worker reach its owner's `Worker`
            // object, after the messages it posted before them.
            if let ScopeId::Worker(key) = scope {
                let mut relay = Vec::new();
                if let Some(run) = self.workers.iter_mut().find(|w| w.key == key) {
                    let errors = run.boa.page().errors.borrow();
                    for text in errors.iter().skip(run.errors_seen) {
                        let line = text.lines().next().unwrap_or_default().to_string();
                        relay.push((run.owner, run.local, line));
                    }
                    run.errors_seen = errors.len();
                }
                for (owner, local, text) in relay {
                    did = true;
                    if let Some(owner) = self.scope_page(owner) {
                        workers::worker_error(owner, local, text, false);
                    }
                }
            }
        }
        did
    }

    fn pump_all(&mut self) -> bool {
        let frames = self.pump_frames();
        let workers = self.pump_workers();
        frames || workers
    }

    /// Loads the document a frame's navigation request asks for, in place
    /// of its current one.
    fn navigate_frame(&mut self, id: FrameId, request: NavigationRequest) {
        let Some(index) = self.frames.iter().position(|f| f.id == id) else {
            return;
        };
        let (parent, element) = (self.frames[index].parent, self.frames[index].element);
        let Some(parent_page) = self.page_of(parent).cloned() else {
            return;
        };
        if self.frames[index].navigations >= self.options.max_navigations {
            parent_page.log(
                ConsoleLevel::Warn,
                format!(
                    "Frame navigated too many times; not loading {}",
                    request.url
                ),
            );
            return;
        }
        let referrer = self.frames[index].boa.page().url.borrow().clone();
        let fetched = self.net.block_on(fetch_document_with(
            self.net.client(),
            &request.method,
            &request.url,
            request.body,
            Some(&referrer),
        ));
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err(e) => {
                self.net
                    .record_document(&request.method, &request.url, None);
                parent_page.log(
                    ConsoleLevel::Error,
                    format!("Failed to load frame {}: {e}", request.url),
                );
                return;
            }
        };
        let info = DocumentInfo::from_fetch(&fetched);
        self.net
            .record_document(&request.method, &request.url, Some(info.status));
        let inner: Vec<FrameId> = self
            .frames
            .iter()
            .filter(|f| f.parent == id)
            .map(|f| f.id)
            .collect();
        for child in inner {
            self.close_frame(child);
        }
        let index = self
            .frames
            .iter()
            .position(|f| f.id == id)
            .expect("frame still open");
        let depth = self.frames[index].depth;
        let placement = Placement {
            frame: id,
            tree: self.tree.clone(),
            viewport: Some(frames::frame_viewport(&parent_page, element)),
        };
        let net = Rc::new(self.net.child());
        match load(
            &net,
            &info,
            fetched.html(),
            Some(&referrer),
            &self.options,
            placement,
        ) {
            Ok(boa) => {
                let frame = &mut self.frames[index];
                frame.boa = boa;
                frame.net = net;
                frame.origin = frames::origin_of(&info.url);
                frame.depth = depth;
                frame.virtual_used = 0.0;
                frame.load_reported = false;
                frame.navigations += 1;
            }
            Err(e) => {
                parent_page.log(ConsoleLevel::Error, format!("Failed to open frame: {e}"));
            }
        }
    }

    /// Carries out what the frames asked for since the last pump. Returns
    /// whether anything was done.
    fn pump_frames(&mut self) -> bool {
        let mut did = false;
        let ids: Vec<FrameId> = std::iter::once(FrameId(0))
            .chain(self.frames.iter().map(|f| f.id))
            .collect();
        for id in ids {
            let Some(page) = self.page_of(id).cloned() else {
                continue;
            };
            for command in page.frames.take_commands() {
                did = true;
                match command {
                    FrameCommand::Open {
                        frame,
                        element,
                        url,
                        srcdoc,
                    } => self.open_frame(id, frame, element, url, srcdoc),
                    FrameCommand::Close { frame } => self.close_frame(frame),
                    FrameCommand::PostMessage {
                        to,
                        data,
                        target_origin,
                    } => {
                        let origin = self.origin_of(id);
                        if let Some(target) = self.page_of(to)
                            && frames::origin_allows(&target_origin, &self.origin_of(to))
                        {
                            frames::deliver_message(target, data, origin, Some(id));
                        }
                    }
                }
            }
            // A frame that navigates loads another document in place.
            if id != FrameId(0)
                && let Some(request) = page.navigation.borrow_mut().take()
            {
                did = true;
                if !request.reload {
                    self.navigate_frame(id, request);
                }
            }
        }
        // Frames whose documents finished loading are reported to their
        // parents.
        let mut loaded = Vec::new();
        for frame in self.frames.iter_mut().filter(|f| !f.load_reported) {
            if frame.boa.page().document_state.borrow().ready_state == DocumentReadyState::Complete
            {
                frame.load_reported = true;
                loaded.push((frame.parent, frame.element));
            }
        }
        for (parent, element) in loaded {
            did = true;
            if let Some(parent) = self.page_of(parent) {
                frames::frame_loaded(parent, element);
            }
        }
        did
    }

    /// Runs one scope's event loop within `limits`. The scope's report
    /// adds up what its runs did since the scheduler started.
    fn run_one(&mut self, scope: ScopeId, limits: &LoopLimits) -> LoopReport {
        let Some(boa) = self.scope_boa(scope) else {
            return idle_report();
        };
        let report = boa.with_cx(|cx| event_loop::run(cx, limits));
        let (used, total) = match scope {
            ScopeId::Frame(FrameId(0)) => (&mut self.top_virtual_used, &mut self.report),
            ScopeId::Frame(id) => match self.frame_mut(id) {
                Some(frame) => (&mut frame.virtual_used, &mut frame.report),
                None => return report,
            },
            ScopeId::Worker(key) => match self.workers.iter_mut().find(|w| w.key == key) {
                Some(run) => (&mut run.virtual_used, &mut run.report),
                None => return report,
            },
        };
        *used += report.virtual_advanced_ms;
        total.stop = report.stop;
        total.steps += report.steps;
        total.virtual_advanced_ms += report.virtual_advanced_ms;
        total.pending_timers = report.pending_timers;
        total.inflight_requests = report.inflight_requests;
        report
    }

    fn virtual_left(&self, scope: ScopeId) -> f64 {
        let used = match scope {
            ScopeId::Frame(FrameId(0)) => self.top_virtual_used,
            ScopeId::Frame(id) => self.frame(id).map(|f| f.virtual_used).unwrap_or(0.0),
            ScopeId::Worker(key) => self
                .workers
                .iter()
                .find(|w| w.key == key)
                .map(|w| w.virtual_used)
                .unwrap_or(0.0),
        };
        (self.options.limits.virtual_ms - used).max(0.0)
    }

    /// Runs the event loops of the page, its frames and its workers, in
    /// turns, until they are idle or the limits are reached. The top
    /// frame's report is kept as the page's.
    fn run_frames(&mut self) {
        let limits = self.options.limits.clone();
        let started = Instant::now();
        let mut steps_left = limits.max_steps;
        self.report = idle_report();
        for frame in &mut self.frames {
            frame.report = idle_report();
        }
        for run in &mut self.workers {
            run.report = idle_report();
        }
        let top = ScopeId::Frame(FrameId(0));
        loop {
            let remaining = limits.wall.saturating_sub(started.elapsed());
            if remaining.is_zero() || steps_left == 0 {
                break;
            }
            let mut progress = false;
            let mut starved = Vec::new();
            for scope in self.scopes() {
                if self.scope_page(scope).is_none() {
                    continue;
                }
                let remaining = limits.wall.saturating_sub(started.elapsed());
                let report = self.run_one(
                    scope,
                    &LoopLimits {
                        wall: remaining,
                        virtual_ms: FRAME_SLICE_MS.min(self.virtual_left(scope)),
                        max_steps: steps_left,
                    },
                );
                steps_left = steps_left.saturating_sub(report.steps);
                if progressed(&report) {
                    progress = true;
                } else if report.stop == StopReason::VirtualBudget {
                    starved.push(scope);
                }
                if scope == top && report.stop == StopReason::Navigation {
                    return;
                }
                if self.pump_all() {
                    progress = true;
                }
            }
            if progress {
                continue;
            }
            // Nothing ran: scopes waiting on timers past their slice get
            // the rest of their virtual budget.
            let mut woke = false;
            for scope in starved {
                let remaining = limits.wall.saturating_sub(started.elapsed());
                let report = self.run_one(
                    scope,
                    &LoopLimits {
                        wall: remaining,
                        virtual_ms: self.virtual_left(scope),
                        max_steps: steps_left,
                    },
                );
                steps_left = steps_left.saturating_sub(report.steps);
                woke |= progressed(&report);
                if scope == top && report.stop == StopReason::Navigation {
                    return;
                }
                woke |= self.pump_all();
            }
            if !woke {
                break;
            }
        }
    }

    /// A PNG of the page: the viewport, or the whole document.
    pub fn screenshot(&self, full_page: bool) -> Vec<u8> {
        catpaw_web::screenshot(self.boa.page(), full_page)
    }

    pub fn dom(&self) -> Ref<'_, Dom> {
        self.boa.page().dom.borrow()
    }

    /// The current document URL.
    pub fn url(&self) -> Url {
        self.boa.page().url.borrow().clone()
    }

    pub fn document(&self) -> &DocumentInfo {
        &self.document
    }

    pub fn navigations(&self) -> &[Url] {
        &self.navigations
    }

    pub fn net(&self) -> &EngineNet {
        &self.net
    }

    /// How the most recent event loop run ended.
    pub fn report(&self) -> &LoopReport {
        &self.report
    }

    /// Whether the page had nothing left to do when the event loop stopped.
    pub fn is_settled(&self) -> bool {
        self.report.stop == StopReason::Idle
            && self
                .frames
                .iter()
                .all(|f| f.report.stop == StopReason::Idle)
            && self
                .workers
                .iter()
                .all(|w| w.report.stop == StopReason::Idle)
    }

    /// The script realm of the current frame.
    fn current_boa(&mut self) -> &mut BoaPage {
        let current = self.current;
        if self.boa_of(current).is_none() {
            self.current = FrameId(0);
        }
        self.boa_of(self.current)
            .expect("the top frame is always open")
    }

    /// Evaluates a script in the current frame and renders its completion
    /// value the way a console would. `Err` describes an uncaught
    /// exception.
    pub fn eval(&mut self, source: &str) -> Result<String, String> {
        self.current_boa().eval_to_string(source)
    }

    /// Evaluates a script and, when its value is a promise, runs the event
    /// loop (within `limits`) until the promise settles, then renders the
    /// outcome: what `await` would give. `Err` describes an exception or
    /// a rejection, or says that the promise never settled.
    pub fn eval_awaited(&mut self, source: &str, limits: &LoopLimits) -> Result<String, String> {
        let value = self.current_boa().eval(source)?;
        let outcome: Rc<RefCell<Option<Result<Value, Value>>>> = Rc::default();
        let slot = outcome.clone();
        self.current_boa().with_cx(|cx| {
            promises::when_settled(cx, value, move |_, result| {
                *slot.borrow_mut() = Some(result);
            });
        });
        self.settle(limits);
        let settled = outcome.borrow_mut().take();
        match settled {
            Some(Ok(value)) => Ok(self.current_boa().with_cx(|cx| cx.script.display(&[value]))),
            Some(Err(reason)) => Err(self.current_boa().with_cx(|cx| {
                format!("the promise was rejected: {}", cx.script.display(&[reason]))
            })),
            None => Err("the promise did not settle".to_string()),
        }
    }

    /// Runs the event loops again (after `eval` queued more work, say),
    /// within `limits`; every frame gets the virtual budget anew.
    pub fn settle(&mut self, limits: &LoopLimits) -> &LoopReport {
        let saved = std::mem::replace(&mut self.options.limits, limits.clone());
        self.top_virtual_used = 0.0;
        for frame in &mut self.frames {
            frame.virtual_used = 0.0;
        }
        for run in &mut self.workers {
            run.virtual_used = 0.0;
        }
        self.run_frames();
        self.options.limits = saved;
        &self.report
    }

    /// The first element matching a CSS selector in the current frame.
    pub fn find(&self, selector: &str) -> Result<NodeId, ActionError> {
        let selectors = catpaw_style::Selectors::parse(selector)
            .ok_or_else(|| ActionError::BadSelector(selector.to_string()))?;
        let page = self
            .page_of(self.current)
            .unwrap_or_else(|| self.boa.page());
        let dom = page.dom.borrow();
        catpaw_style::query::query_first(&dom, dom.document(), &selectors)
            .ok_or_else(|| ActionError::NotFound(selector.to_string()))
    }

    /// Runs an input action in the current frame, settles the page and
    /// follows any navigation it started.
    fn act(
        &mut self,
        action: impl FnOnce(&mut catpaw_web::page::Cx<'_>) -> Result<(), catpaw_web::input::InputError>,
    ) -> Result<(), ActionError> {
        self.current_boa().with_cx(action)?;
        self.run_frames();
        self.follow_navigations()?;
        Ok(())
    }

    /// Clicks the first element matching `selector`.
    pub fn click(&mut self, selector: &str) -> Result<(), ActionError> {
        let el = self.find(selector)?;
        self.act(|cx| catpaw_web::input::click_element(cx, el).map(drop))
    }

    /// Replaces the value of the first element matching `selector`.
    pub fn fill(&mut self, selector: &str, text: &str) -> Result<(), ActionError> {
        let el = self.find(selector)?;
        let text = text.to_string();
        self.act(move |cx| catpaw_web::input::fill(cx, el, &text))
    }

    /// Types into the focused element, key by key.
    pub fn type_text(&mut self, text: &str) -> Result<(), ActionError> {
        let text = text.to_string();
        self.act(move |cx| catpaw_web::input::type_text(cx, &text))
    }

    /// Presses a key on the focused element.
    pub fn press(&mut self, key: &str) -> Result<(), ActionError> {
        let key = key.to_string();
        self.act(move |cx| catpaw_web::input::press(cx, &key))
    }

    /// Focuses the first element matching `selector`.
    pub fn focus(&mut self, selector: &str) -> Result<(), ActionError> {
        let el = self.find(selector)?;
        self.act(move |cx| catpaw_web::input::focus(cx, el))
    }

    /// Moves the pointer over the first element matching `selector`.
    pub fn hover(&mut self, selector: &str) -> Result<(), ActionError> {
        let el = self.find(selector)?;
        self.act(move |cx| catpaw_web::input::hover_element(cx, el))
    }

    /// Checks or unchecks the first element matching `selector`.
    pub fn set_checked(&mut self, selector: &str, checked: bool) -> Result<(), ActionError> {
        let el = self.find(selector)?;
        self.act(move |cx| catpaw_web::input::set_checked(cx, el, checked))
    }

    /// Selects the option with `value` in the first element matching
    /// `selector`.
    pub fn select(&mut self, selector: &str, value: &str) -> Result<(), ActionError> {
        let el = self.find(selector)?;
        let value = value.to_string();
        self.act(move |cx| catpaw_web::input::select_option(cx, el, &value))
    }
}

/// Opens a page on a dedicated thread with a stack large enough for the
/// engine, runs `f` on it, and returns the result. Safe to call from async
/// code (it blocks the calling thread until the page is done).
pub fn with_page<R: Send + 'static>(
    url: Url,
    options: PageOptions,
    f: impl FnOnce(&mut Page) -> R + Send + 'static,
) -> Result<R, EngineError> {
    on_page_thread(move || {
        let mut page = Page::open(&url, &options)?;
        Ok(f(&mut page))
    })
}

/// Like [`with_page`], for a document given as a string.
pub fn with_html<R: Send + 'static>(
    url: Url,
    html: String,
    options: PageOptions,
    f: impl FnOnce(&mut Page) -> R + Send + 'static,
) -> Result<R, EngineError> {
    on_page_thread(move || {
        let mut page = Page::from_html(&url, &html, &options)?;
        Ok(f(&mut page))
    })
}

fn describe_frame(url: &Option<Url>, srcdoc: &Option<String>) -> String {
    match (url, srcdoc) {
        (_, Some(_)) => "a srcdoc frame".to_string(),
        (Some(url), None) => url.to_string(),
        (None, None) => "about:blank".to_string(),
    }
}

fn on_page_thread<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, EngineError> + Send + 'static,
) -> Result<R, EngineError> {
    std::thread::Builder::new()
        .name("catpaw-page".to_string())
        .stack_size(PAGE_STACK_SIZE)
        .spawn(f)
        .map_err(EngineError::Thread)?
        .join()
        .map_err(|_| EngineError::Panicked)?
}
