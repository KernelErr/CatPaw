//! One page: fetch the document, parse it while running its scripts, and
//! drive the event loop until the page settles.

use std::cell::Ref;
use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_dom::Dom;
use catpaw_fetch::{FetchedDocument, fetch_document};
use catpaw_net::{NetConfig, NetError};
use catpaw_web::event_loop::{self, LoopLimits, LoopReport, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
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
    report: LoopReport,
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
        let mut url = url.clone();
        let mut navigations: Vec<Url> = Vec::new();
        loop {
            let fetched = net.block_on(fetch_document(net.client(), &url))?;
            let info = DocumentInfo::from_fetch(&fetched);
            let (boa, report) = load(&net, &info, fetched.html(), navigations.last(), options)?;
            navigations.push(info.url.clone());

            let requested = boa.page().navigation.borrow_mut().take();
            if let Some(navigation) = requested
                && !navigation.reload
                && navigations.len() <= options.max_navigations
            {
                url = navigation.url;
                continue;
            }
            return Ok(Self {
                boa,
                net,
                document: info,
                navigations,
                report,
            });
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
            report,
        })
    }

    pub fn state(&self) -> &Rc<PageState> {
        self.boa.page()
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

    /// Runs the event loop again (after `eval` queued more work, say).
    pub fn settle(&mut self, limits: &LoopLimits) -> &LoopReport {
        self.report = self.boa.with_cx(|cx| event_loop::run(cx, limits));
        &self.report
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
