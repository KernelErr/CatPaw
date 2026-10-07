//! One page: fetch the document, parse it while running its scripts, and
//! drive the event loop until the page settles.

use std::cell::{Ref, RefCell};
use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_dom::Dom;
use catpaw_dom::NodeId;
use catpaw_fetch::{FetchedDocument, fetch_document, fetch_document_with};
use catpaw_js::Value;
use catpaw_net::{NetConfig, NetError};
use catpaw_web::event_loop::{self, LoopLimits, LoopReport, StopReason};
use catpaw_web::page::NavigationRequest;
use catpaw_web::{PageConfig, PageState, promises, scripting};
use url::Url;

use crate::net::EngineNet;

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

/// A loaded page. Lives on the thread that created it.
pub struct Page {
    // Dropped before `net`: the page state refers to it.
    boa: BoaPage,
    net: Rc<EngineNet>,
    document: DocumentInfo,
    /// The documents loaded on the way here, oldest first.
    navigations: Vec<Url>,
    options: PageOptions,
    report: LoopReport,
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
}

fn load(
    net: &Rc<EngineNet>,
    info: &DocumentInfo,
    html: &str,
    referrer: Option<&Url>,
    options: &PageOptions,
) -> Result<(BoaPage, LoopReport), EngineError> {
    let mut config = options.page.clone();
    config.user_agent = options.net.user_agent.clone();
    let state = Rc::new(PageState::new(info.url.clone(), config));
    state.set_net(net.clone());
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
    let report = boa.with_cx(|cx| {
        scripting::load_document(cx, html);
        event_loop::run(cx, &options.limits)
    });
    Ok((boa, report))
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
        let (boa, report) = load(&net, &info, fetched.html(), None, options)?;
        let mut page = Self {
            boa,
            net,
            document: info.clone(),
            navigations: vec![info.url],
            options: options.clone(),
            report,
        };
        page.follow_navigations()?;
        Ok(page)
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
        let (boa, report) = load(
            &self.net,
            &info,
            fetched.html(),
            Some(&referrer),
            &self.options,
        )?;
        self.boa = boa;
        self.report = report;
        self.navigations.push(info.url.clone());
        self.document = info;
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
        let (boa, report) = load(&net, &info, html, None, options)?;
        Ok(Self {
            boa,
            net,
            document: info,
            navigations: vec![url.clone()],
            options: options.clone(),
            report,
        })
    }

    pub fn state(&self) -> &Rc<PageState> {
        self.boa.page()
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
    }

    /// Evaluates a script in the page and renders its completion value the
    /// way a console would. `Err` describes an uncaught exception.
    pub fn eval(&mut self, source: &str) -> Result<String, String> {
        self.boa.eval_to_string(source)
    }

    /// Evaluates a script and, when its value is a promise, runs the event
    /// loop (within `limits`) until the promise settles, then renders the
    /// outcome: what `await` would give. `Err` describes an exception or
    /// a rejection, or says that the promise never settled.
    pub fn eval_awaited(&mut self, source: &str, limits: &LoopLimits) -> Result<String, String> {
        let value = self.boa.eval(source)?;
        let outcome: Rc<RefCell<Option<Result<Value, Value>>>> = Rc::default();
        let slot = outcome.clone();
        self.boa.with_cx(|cx| {
            promises::when_settled(cx, value, move |_, result| {
                *slot.borrow_mut() = Some(result);
            });
        });
        self.report = self.boa.with_cx(|cx| event_loop::run(cx, limits));
        let settled = outcome.borrow_mut().take();
        match settled {
            Some(Ok(value)) => Ok(self.boa.with_cx(|cx| cx.script.display(&[value]))),
            Some(Err(reason)) => Err(self.boa.with_cx(|cx| {
                format!("the promise was rejected: {}", cx.script.display(&[reason]))
            })),
            None => Err("the promise did not settle".to_string()),
        }
    }

    /// Runs the event loop again (after `eval` queued more work, say).
    pub fn settle(&mut self, limits: &LoopLimits) -> &LoopReport {
        self.report = self.boa.with_cx(|cx| event_loop::run(cx, limits));
        &self.report
    }

    /// The first element matching a CSS selector.
    pub fn find(&self, selector: &str) -> Result<NodeId, ActionError> {
        let selectors = catpaw_style::Selectors::parse(selector)
            .ok_or_else(|| ActionError::BadSelector(selector.to_string()))?;
        let dom = self.dom();
        catpaw_style::query::query_first(&dom, dom.document(), &selectors)
            .ok_or_else(|| ActionError::NotFound(selector.to_string()))
    }

    /// Runs an input action, settles the page and follows any navigation
    /// it started.
    fn act(
        &mut self,
        action: impl FnOnce(&mut catpaw_web::page::Cx<'_>) -> Result<(), catpaw_web::input::InputError>,
    ) -> Result<(), ActionError> {
        self.boa.with_cx(action)?;
        let limits = self.options.limits.clone();
        self.settle(&limits);
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
