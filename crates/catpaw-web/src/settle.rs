//! When a page has settled: what an agent's action waits for.
//!
//! Left alone, the event loop stops only when nothing at all is left to do.
//! Real pages are rarely there: analytics poll, carousels turn, sockets stay
//! open. A [`SettlePolicy`] says what is worth waiting for: work due soon,
//! requests whose answers the page is waiting on, a document still being
//! changed. With one, the loop stops at
//! [`StopReason::Settled`](crate::event_loop::StopReason::Settled) as soon
//! as only the rest is left, and [`report`] names what was still going on.
//!
//! To tell polling from work, timers and requests remember who started
//! them ([`Initiator`]) and where in script ([`SourceSite`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use catpaw_dom::NodeId;
use catpaw_js::SourceSite;
use url::Url;

use crate::net::RequestKind;
use crate::page::PageState;

/// What started a piece of work: the context a timer, request or task was
/// created in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Initiator {
    /// The HTML parser: scripts, style sheets and frames of the document.
    Parser,
    /// Input from the user (or an agent acting as one).
    Input,
    /// A timer callback; `site` indexes the place it was set from.
    Timer {
        site: Option<usize>,
    },
    /// An animation frame, or the observers that run with frames.
    Frame,
    /// A task of the event loop: a response arriving, a message.
    Task(&'static str),
    /// Script the embedder evaluated.
    Embedder,
    Unknown,
}

/// A place in script that sets timers, and what it has done so far.
#[derive(Clone, Debug)]
pub struct TimerSite {
    pub site: SourceSite,
    /// Timers armed here; every repeat of an interval counts.
    pub arms: u32,
    /// Of those, the ones that re-armed themselves: an interval's repeats,
    /// and timers set here from a callback of a timer set here (polling).
    /// A timer armed again and again from elsewhere (a debounce, reset on
    /// each key) is not counted: its work is still to come.
    pub rearms: u32,
    /// The shortest delay asked for here, in milliseconds.
    pub min_delay_ms: f64,
    /// Whether an interval was set here.
    pub repeat: bool,
}

/// What a request was when it started.
#[derive(Clone, Debug)]
pub struct RequestInfo {
    pub method: String,
    pub url: Url,
    pub kind: RequestKind,
    pub initiator: Initiator,
    /// Where script started it (`fetch`, `XMLHttpRequest.send`).
    pub site: Option<SourceSite>,
    /// When it started, on the page clock and in real time.
    pub virtual_start: f64,
    pub real_start: Instant,
}

/// How much the document changed, and where.
#[derive(Clone, Debug, Default)]
pub struct DomActivity {
    /// Insertions, removals, text and attribute changes other than of
    /// `style` and `class`.
    pub content_changes: u64,
    /// Changes of `style` and `class` attributes: animations, mostly.
    pub cosmetic_changes: u64,
    /// When the last content change happened, on the page clock.
    pub last_content_ms: Option<f64>,
    /// The nodes changed most since the counts were last reset.
    pub targets: HashMap<NodeId, u32>,
}

/// The settling state a page keeps.
#[derive(Default)]
pub(crate) struct SettleState {
    pub(crate) initiator: Cell<Option<Initiator>>,
    pub(crate) sites: RefCell<Vec<TimerSite>>,
    site_index: RefCell<HashMap<SourceSite, usize>>,
    pub(crate) dom: RefCell<DomActivity>,
    /// Animation frames in a row that changed no content.
    pub(crate) quiet_frames: Cell<u32>,
}

/// What to wait for before an action counts as done.
#[derive(Clone, Debug)]
pub struct SettlePolicy {
    /// Timers due later than this (milliseconds from now) do not hold the
    /// page up. A second covers debounced search boxes and transitions;
    /// a toast's dismissal or a slow "loading" delay is left for `wait`.
    pub timer_threshold_ms: f64,
    /// A site that armed timers this many times, with delays of at least
    /// `polling_min_delay_ms`, is polling: its timers, and the requests
    /// they start, are ignored.
    pub polling_rearm: u32,
    pub polling_min_delay_ms: f64,
    /// How long the document must go without a content change.
    pub dom_quiet_ms: f64,
    /// Animation frames in a row that change no content, after which
    /// frame callbacks no longer hold the page up (a canvas animation).
    pub raf_quiet_frames: u32,
    /// Style sheets, images and fonts still loading after this are not
    /// waited for.
    pub asset_timeout: Duration,
    /// Any request still open after this is taken for a stream or a long
    /// poll and not waited for.
    pub stream_timeout: Duration,
    /// A request to another site still open after this is not waited for:
    /// third parties (tag managers, experiments, widgets) that do not
    /// answer should not hold an action up for long.
    pub third_party_timeout: Duration,
    /// Hosts (and their subdomains) whose requests are never waited for:
    /// analytics and telemetry.
    pub ignore_hosts: Vec<String>,
}

/// Analytics, telemetry and advertising hosts: their requests never hold
/// a page up.
pub const DEFAULT_IGNORED_HOSTS: &[&str] = &[
    "google-analytics.com",
    "analytics.google.com",
    "googletagmanager.com",
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "facebook.net",
    "hotjar.com",
    "hotjar.io",
    "clarity.ms",
    "segment.io",
    "segment.com",
    "mixpanel.com",
    "amplitude.com",
    "sentry.io",
    "backtrace.io",
    "bugsnag.com",
    "nr-data.net",
    "newrelic.com",
    "datadoghq.com",
    "fullstory.com",
    "mouseflow.com",
    "quantserve.com",
    "scorecardresearch.com",
    "chartbeat.net",
    "cloudflareinsights.com",
    "plausible.io",
    "optimizely.com",
    "googleoptimize.com",
    "adobedtm.com",
    "omtrdc.net",
    "demdex.net",
    "bat.bing.com",
    "ads.linkedin.com",
    "analytics.tiktok.com",
    "ct.pinterest.com",
    "tr.snapchat.com",
    "analytics.twitter.com",
    "ads-twitter.com",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "adsrvr.org",
    "amazon-adsystem.com",
    "hs-analytics.net",
];

impl Default for SettlePolicy {
    fn default() -> Self {
        Self {
            timer_threshold_ms: 1000.0,
            polling_rearm: 5,
            polling_min_delay_ms: 100.0,
            dom_quiet_ms: 100.0,
            raf_quiet_frames: 3,
            asset_timeout: Duration::from_secs(2),
            stream_timeout: Duration::from_secs(10),
            third_party_timeout: Duration::from_secs(3),
            ignore_hosts: DEFAULT_IGNORED_HOSTS
                .iter()
                .map(|h| h.to_string())
                .collect(),
        }
    }
}

impl SettlePolicy {
    /// The policy of a `wait`: every timer and frame runs (page time is
    /// what the wait is for), but requests a page is not waiting on
    /// (analytics, slow third parties, streams) do not make the page clock
    /// follow real time.
    pub fn waiting() -> Self {
        Self {
            timer_threshold_ms: f64::INFINITY,
            polling_rearm: u32::MAX,
            raf_quiet_frames: u32::MAX,
            dom_quiet_ms: 0.0,
            ..Self::default()
        }
    }

    /// Whether requests to `url` are never waited for.
    pub fn ignores_host(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        self.ignore_hosts.iter().any(|ignored| {
            host == ignored
                || host
                    .strip_suffix(ignored.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
        })
    }

    fn is_polling(&self, site: &TimerSite) -> bool {
        site.rearms >= self.polling_rearm && site.min_delay_ms >= self.polling_min_delay_ms
    }
}

/// Why a request in flight is, or is not, waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestClass {
    /// The page is waiting on it.
    Relevant,
    /// A beacon: sent in the background, its answer ignored.
    Background,
    /// Started by a polling timer.
    Polling,
    /// To an analytics or telemetry host.
    IgnoredHost,
    /// To another site, and slow to answer.
    SlowThirdParty,
    /// Open so long it is taken for a stream.
    LongStream,
    /// An asset (style sheet, image, font) slow to load.
    SlowAsset,
    /// Held back, unsent, until the embedder lets it go.
    Held,
}

impl RequestClass {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestClass::Relevant => "pending",
            RequestClass::Background => "background",
            RequestClass::Polling => "polling",
            RequestClass::IgnoredHost => "analytics",
            RequestClass::SlowThirdParty => "slow third party",
            RequestClass::LongStream => "streaming",
            RequestClass::SlowAsset => "slow asset",
            RequestClass::Held => "held",
        }
    }
}

/// Why a timer is, or is not, waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerClass {
    /// Due soon: the loop runs it.
    Near,
    /// Due later than the threshold.
    Far,
    /// Set by a polling site.
    Polling,
}

/// A request still in flight.
#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub method: String,
    pub url: Url,
    pub kind: RequestKind,
    pub class: RequestClass,
    /// Real time since it started.
    pub age: Duration,
    pub initiator: Initiator,
    /// Where script started it.
    pub site: Option<SourceSite>,
    /// Where the timer that started it was set, when a timer did.
    pub timer_site: Option<SourceSite>,
}

/// A timer still armed.
#[derive(Clone, Debug)]
pub struct PendingTimer {
    /// Milliseconds until it fires (page clock).
    pub due_in_ms: f64,
    pub delay_ms: f64,
    pub repeat: bool,
    pub class: TimerClass,
    pub site: Option<SourceSite>,
    /// How many timers its site armed.
    pub arms: u32,
}

/// What a page was still doing when its event loop stopped.
#[derive(Clone, Debug, Default)]
pub struct PendingReport {
    pub requests: Vec<PendingRequest>,
    pub timers: Vec<PendingTimer>,
    /// Animation frames are requested and still change the document.
    pub animating: bool,
    pub dom: DomActivity,
    pub sockets_open: usize,
    pub navigation_pending: bool,
}

impl PendingReport {
    /// Whether anything in it holds the page up.
    pub fn blocking(&self) -> bool {
        self.navigation_pending
            || self.animating
            || self
                .requests
                .iter()
                .any(|r| r.class == RequestClass::Relevant)
            || self.timers.iter().any(|t| t.class == TimerClass::Near)
    }
}

// ---------------------------------------------------------------- recording

/// Runs `f` with `initiator` as the context new work is attributed to.
pub fn with_initiator<R>(page: &PageState, initiator: Initiator, f: impl FnOnce() -> R) -> R {
    let saved = page.settle.initiator.replace(Some(initiator));
    let result = f();
    page.settle.initiator.set(saved);
    result
}

/// Attributes new work to an initiator until dropped (for code that
/// cannot run inside a closure).
pub struct InitiatorGuard<'a> {
    page: &'a PageState,
    saved: Option<Initiator>,
}

impl<'a> InitiatorGuard<'a> {
    pub fn new(page: &'a PageState, initiator: Initiator) -> Self {
        let saved = page.settle.initiator.replace(Some(initiator));
        Self { page, saved }
    }
}

impl Drop for InitiatorGuard<'_> {
    fn drop(&mut self) {
        self.page.settle.initiator.set(self.saved);
    }
}

/// Whether the work running now is a polling timer's, as the default
/// settle policy judges it.
pub(crate) fn polling_now(page: &PageState) -> bool {
    let Some(Initiator::Timer { site: Some(i) }) = page.settle.initiator.get() else {
        return false;
    };
    let policy = SettlePolicy::default();
    page.settle
        .sites
        .borrow()
        .get(i)
        .is_some_and(|site| policy.is_polling(site))
}

/// The context new work is attributed to now.
pub fn current_initiator(page: &PageState) -> Initiator {
    page.settle.initiator.get().unwrap_or(Initiator::Unknown)
}

/// Notes that a timer was armed at `site`; returns the site's index.
pub(crate) fn arm_site(page: &PageState, site: SourceSite, delay_ms: f64, repeat: bool) -> usize {
    let mut index = page.settle.site_index.borrow_mut();
    let mut sites = page.settle.sites.borrow_mut();
    let i = *index.entry(site.clone()).or_insert_with(|| {
        sites.push(TimerSite {
            site,
            arms: 0,
            rearms: 0,
            min_delay_ms: f64::INFINITY,
            repeat: false,
        });
        sites.len() - 1
    });
    let entry = &mut sites[i];
    entry.arms = entry.arms.saturating_add(1);
    if page.settle.initiator.get() == Some(Initiator::Timer { site: Some(i) }) {
        entry.rearms = entry.rearms.saturating_add(1);
    }
    entry.min_delay_ms = entry.min_delay_ms.min(delay_ms);
    entry.repeat |= repeat;
    i
}

/// Notes another firing of an interval set at site `i`.
pub(crate) fn rearm_site(page: &PageState, i: usize) {
    if let Some(entry) = page.settle.sites.borrow_mut().get_mut(i) {
        entry.arms = entry.arms.saturating_add(1);
        entry.rearms = entry.rearms.saturating_add(1);
    }
}

/// Notes a content change at `target`: a node inserted or removed below
/// it, its text, or an attribute other than `style` and `class`.
pub(crate) fn note_content(page: &PageState, target: NodeId) {
    let mut dom = page.settle.dom.borrow_mut();
    dom.content_changes += 1;
    dom.last_content_ms = Some(page.clock.peek());
    *dom.targets.entry(target).or_insert(0) += 1;
}

/// Notes a change of `style` or `class` at `target`.
pub(crate) fn note_cosmetic(page: &PageState, target: NodeId) {
    let mut dom = page.settle.dom.borrow_mut();
    dom.cosmetic_changes += 1;
    *dom.targets.entry(target).or_insert(0) += 1;
}

/// The content change count, to compare across a frame.
pub(crate) fn content_changes(page: &PageState) -> u64 {
    page.settle.dom.borrow().content_changes
}

/// Starts counting changed nodes afresh (each event loop run does).
pub(crate) fn reset_targets(page: &PageState) {
    page.settle.dom.borrow_mut().targets.clear();
}

// ---------------------------------------------------------------- judging

fn timer_site(page: &PageState, i: Option<usize>) -> Option<TimerSite> {
    i.and_then(|i| page.settle.sites.borrow().get(i).cloned())
}

/// How a request in flight counts.
fn classify_request(page: &PageState, policy: &SettlePolicy, info: &RequestInfo) -> RequestClass {
    if info.kind == RequestKind::Beacon {
        return RequestClass::Background;
    }
    if policy.ignores_host(&info.url) {
        return RequestClass::IgnoredHost;
    }
    if let Initiator::Timer { site } = info.initiator
        && timer_site(page, site).is_some_and(|s| policy.is_polling(&s))
    {
        return RequestClass::Polling;
    }
    let age = info.real_start.elapsed();
    if matches!(info.kind, RequestKind::Style | RequestKind::Other) && age > policy.asset_timeout {
        return RequestClass::SlowAsset;
    }
    if age > policy.third_party_timeout && !same_site(&info.url, &page.url.borrow()) {
        return RequestClass::SlowThirdParty;
    }
    if age > policy.stream_timeout {
        return RequestClass::LongStream;
    }
    RequestClass::Relevant
}

/// The site of a host, roughly: its last two labels, or three under a
/// two-letter country code with a generic second level (`co.uk`).
fn site_of(host: &str) -> &str {
    let labels: Vec<&str> = host.rsplit('.').collect();
    let take = match labels.as_slice() {
        [tld, second, ..]
            if tld.len() == 2
                && matches!(*second, "co" | "com" | "net" | "org" | "gov" | "ac" | "edu") =>
        {
            3
        }
        _ => 2,
    };
    if labels.len() <= take {
        return host;
    }
    let keep: usize = labels[..take].iter().map(|l| l.len()).sum::<usize>() + take - 1;
    &host[host.len() - keep..]
}

/// Whether two URLs are of the same site (scheme aside), the site being
/// the host's last two labels (three under `co.uk` and the like); an IP
/// address is a site of its own.
pub fn same_site(a: &Url, b: &Url) -> bool {
    use url::Host;
    match (a.host(), b.host()) {
        (Some(Host::Domain(x)), Some(Host::Domain(y))) => {
            site_of(x).eq_ignore_ascii_case(site_of(y))
        }
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// Whether the request `token` is held back by the embedder.
pub(crate) fn is_held(page: &PageState, token: u64) -> bool {
    page.net
        .borrow()
        .as_ref()
        .is_some_and(|net| net.is_held(token))
}

/// Whether `policy` waits for a request in flight.
pub(crate) fn waits_for(page: &PageState, policy: &SettlePolicy, info: &RequestInfo) -> bool {
    classify_request(page, policy, info) == RequestClass::Relevant
}

/// How many requests in flight the page is waiting on.
pub(crate) fn blocking_requests(page: &PageState, policy: &SettlePolicy) -> usize {
    let callbacks = page.net_callbacks.borrow();
    let started = page.net_started.borrow();
    let requests = started
        .iter()
        .filter(|(token, _)| callbacks.contains_key(token) && !is_held(page, **token))
        .filter(|(_, info)| classify_request(page, policy, info) == RequestClass::Relevant)
        .count();
    // A handshake that hangs (a socket a network drops) stops holding the
    // page up after a while, as a slow asset does; one with an ignored
    // host never holds it up, as requests to such hosts do not.
    requests
        + page
            .sockets
            .connecting_within(page, policy.asset_timeout, |url| !policy.ignores_host(url))
}

/// How a timer counts, `now` being the page clock.
fn classify_timer(
    policy: &SettlePolicy,
    site: Option<&TimerSite>,
    deadline: f64,
    now: f64,
) -> TimerClass {
    if site.is_some_and(|s| policy.is_polling(s)) {
        TimerClass::Polling
    } else if deadline - now > policy.timer_threshold_ms {
        TimerClass::Far
    } else {
        TimerClass::Near
    }
}

/// The earliest deadline of a timer the page is waiting on.
pub(crate) fn next_blocking_timer(
    page: &PageState,
    policy: &SettlePolicy,
    now: f64,
) -> Option<f64> {
    let sites = page.settle.sites.borrow();
    page.timers
        .borrow()
        .iter()
        .filter(|(deadline, timer)| {
            let site = timer.site.and_then(|i| sites.get(i));
            classify_timer(policy, site, *deadline, now) == TimerClass::Near
        })
        .map(|(deadline, _)| deadline)
        .next()
}

/// Whether animation frames still hold the page up: frames are requested
/// and recent ones changed content.
pub(crate) fn frames_active(page: &PageState, policy: &SettlePolicy) -> bool {
    page.raf.borrow().is_requested() && page.settle.quiet_frames.get() < policy.raf_quiet_frames
}

/// When the document will have been quiet long enough, if it has not yet.
pub(crate) fn quiet_at(page: &PageState, policy: &SettlePolicy, now: f64) -> Option<f64> {
    let last = page.settle.dom.borrow().last_content_ms?;
    let at = last + policy.dom_quiet_ms;
    (at > now).then_some(at)
}

/// What the page is still doing, judged by `policy`.
pub fn report(page: &PageState, policy: &SettlePolicy) -> PendingReport {
    let now = page.clock.peek();
    let callbacks = page.net_callbacks.borrow();
    let started = page.net_started.borrow();
    let mut requests: Vec<PendingRequest> = started
        .iter()
        .filter(|(token, _)| callbacks.contains_key(token))
        .map(|(token, info)| {
            let timer_site = match info.initiator {
                Initiator::Timer { site } => timer_site(page, site).map(|s| s.site),
                _ => None,
            };
            PendingRequest {
                method: info.method.clone(),
                url: info.url.clone(),
                kind: info.kind,
                class: if is_held(page, *token) {
                    RequestClass::Held
                } else {
                    classify_request(page, policy, info)
                },
                age: info.real_start.elapsed(),
                initiator: info.initiator,
                site: info.site.clone(),
                timer_site,
            }
        })
        .collect();
    requests.sort_by_key(|r| std::cmp::Reverse(r.age));
    let sites = page.settle.sites.borrow();
    let timers: Vec<PendingTimer> = page
        .timers
        .borrow()
        .iter()
        .map(|(deadline, timer)| {
            let site = timer.site.and_then(|i| sites.get(i));
            PendingTimer {
                due_in_ms: (deadline - now).max(0.0),
                delay_ms: timer.delay_ms,
                repeat: timer.interval.is_some(),
                class: classify_timer(policy, site, deadline, now),
                site: site.map(|s| s.site.clone()),
                arms: site.map(|s| s.arms).unwrap_or(1),
            }
        })
        .collect();
    PendingReport {
        requests,
        timers,
        animating: frames_active(page, policy),
        dom: page.settle.dom.borrow().clone(),
        sockets_open: page.sockets.open(page),
        navigation_pending: page.navigation.borrow().is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_are_told_apart() {
        assert_eq!(site_of("www.example.com"), "example.com");
        assert_eq!(site_of("example.com"), "example.com");
        assert_eq!(site_of("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(site_of("localhost"), "localhost");
        let page = Url::parse("https://shop.example.com/").unwrap();
        assert!(same_site(
            &Url::parse("https://api.example.com/x").unwrap(),
            &page
        ));
        assert!(!same_site(
            &Url::parse("https://cdn.other.net/x").unwrap(),
            &page
        ));
    }

    #[test]
    fn ignored_hosts_match_subdomains_only() {
        let policy = SettlePolicy::default();
        assert!(policy.ignores_host(&Url::parse("https://www.google-analytics.com/g").unwrap()));
        assert!(policy.ignores_host(&Url::parse("https://1.log.optimizely.com/e").unwrap()));
        assert!(!policy.ignores_host(&Url::parse("https://notgoogle-analytics.com/").unwrap()));
    }
}

#[cfg(test)]
mod site_tests {
    use super::*;

    #[test]
    fn sites_are_registrable_domains_or_addresses() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(same_site(
            &url("https://a.example.com/"),
            &url("http://b.example.com/x")
        ));
        assert!(same_site(
            &url("https://shop.example.co.uk/"),
            &url("https://example.co.uk/")
        ));
        assert!(!same_site(
            &url("https://a.example.co.uk/"),
            &url("https://other.co.uk/")
        ));
        assert!(!same_site(
            &url("http://127.0.0.1/"),
            &url("http://10.0.0.1/")
        ));
        assert!(!same_site(
            &url("http://192.168.0.1/"),
            &url("http://10.0.0.1/")
        ));
        assert!(same_site(
            &url("http://127.0.0.1:80/"),
            &url("http://127.0.0.1:8080/")
        ));
        assert!(!same_site(&url("http://[::1]/"), &url("http://[::2]/")));
        assert!(!same_site(
            &url("http://127.0.0.1/"),
            &url("http://localhost/")
        ));
    }
}
