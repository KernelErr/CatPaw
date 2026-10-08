//! The `catpaw` command line.
//!
//! `fetch` retrieves a page (optionally running its scripts with `--js`),
//! resolves its styles, and prints a CST snapshot, markdown, text, HTML,
//! links or forms. `keygen` creates a Web Bot Auth key pair and its key
//! directory document. `mcp` serves the agent tools; `setup` registers
//! them with an agent host.

mod setup;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use catpaw_agent::{
    AttributeOracle, ExtraAttrs, Filter, Format, ReadOptions, RefTable, SnapshotOptions,
    Snapshotter, StyleOracle,
};
use catpaw_dom::{Dom, HtmlParseOptions, NodeId, parse_html, to_html};
use catpaw_engine::{LoopLimits, PageConfig, PageOptions, SettlePolicy, StopReason};
use catpaw_fetch::fetch_document;
use catpaw_net::{BotAuthConfig, KeyPair, NetClient, NetConfig, Url};
use catpaw_style::{StyleEngine, StyleOptions};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "catpaw",
    version,
    about = "A headless-first browser for AI agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Fetch a page and print what an agent would see.
    Fetch(FetchArgs),
    /// Generate a Web Bot Auth (Ed25519) key pair and its key directory document.
    Keygen(KeygenArgs),
    /// Serve the agent tools over MCP (Model Context Protocol).
    Mcp(McpArgs),
    /// Register `catpaw mcp --stdio` with an agent host (claude-code,
    /// codex, cursor): print the command or config, or write it.
    Setup(setup::SetupArgs),
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum View {
    Snapshot,
    Markdown,
    Text,
    Html,
    Links,
    Forms,
}

#[derive(Args)]
struct FetchArgs {
    /// URL to fetch (http or https).
    url: String,
    /// What to print.
    #[arg(long, value_enum, default_value_t = View::Snapshot)]
    view: View,
    /// Shortcut for `--view snapshot`.
    #[arg(long, conflicts_with_all = ["markdown", "text", "html", "links", "forms"])]
    snapshot: bool,
    #[arg(long)]
    markdown: bool,
    #[arg(long)]
    text: bool,
    #[arg(long)]
    html: bool,
    #[arg(long)]
    links: bool,
    #[arg(long)]
    forms: bool,
    /// Snapshot filter: all, interesting, interactive.
    #[arg(long, default_value = "interesting")]
    filter: String,
    /// Snapshot line format: compact (`e12 link "Home"`) or aria
    /// (`- link "Home" [ref=e12]`, Playwright's aria-snapshot syntax).
    #[arg(long, default_value = "compact")]
    format: String,
    /// Optional snapshot attributes, comma-separated: href, src,
    /// description; `none` for none.
    #[arg(long, default_value = "href")]
    attrs: String,
    /// Soft character budget for the snapshot.
    #[arg(long)]
    max_chars: Option<usize>,
    /// Only the main content for markdown/text.
    #[arg(long)]
    main_only: bool,
    /// Skip stylesheets: decide visibility from markup alone (faster, less accurate).
    #[arg(long)]
    no_css: bool,
    #[command(flatten)]
    net: NetArgs,
    /// Print response headers to stderr.
    #[arg(long)]
    show_headers: bool,
    /// Run the page's scripts before reading it.
    #[arg(long)]
    js: bool,
    /// With --js: how far (in milliseconds) timers may be fast-forwarded
    /// while waiting for the page to settle.
    #[arg(long, default_value_t = 10_000, requires = "js")]
    time_budget: u64,
    /// With --js: how long (in milliseconds) one run of script may take
    /// before it is stopped; 0 lets scripts run as long as they like.
    #[arg(long, default_value_t = 10_000, requires = "js")]
    script_budget: u64,
    /// With --js: print the page's console output to stderr (the frames'
    /// too).
    #[arg(long, requires = "js")]
    console: bool,
    /// With --js: the time zone dates show, as an offset from UTC
    /// (`+08:00`, `-0430`, `+9`) or `UTC`; UTC by default, whatever the
    /// machine's.
    #[arg(long, requires = "js", default_value = "UTC", value_parser = parse_timezone, allow_hyphen_values = true)]
    timezone: i32,
    /// With --js: list the page's frames and workers once it has settled.
    #[arg(long, requires = "js")]
    frames: bool,
    /// With --js: a JSON file holding `localStorage` by origin, read before
    /// the page loads and written back afterwards.
    #[arg(long, requires = "js")]
    storage: Option<PathBuf>,
    /// With --js: list every request the page made, with a preview of
    /// request bodies, on stderr.
    #[arg(long, requires = "js")]
    requests: bool,
    /// With --js: evaluate this script once the page has settled and print
    /// its result (awaiting a promise) instead of a view of the page.
    #[arg(long, requires = "js")]
    eval: Option<String>,
    /// With --js: an action to take once the page has settled, before the
    /// output; may repeat. One of `click <selector>`, `fill <selector> <text>`,
    /// `type <text>`, `press <key>`, `check <selector>`, `uncheck <selector>`,
    /// `select <selector> <value>`, `hover <selector>`, `focus <selector>`,
    /// `frame <selector>` (address the frame of that iframe for the actions
    /// and --eval that follow), `frame parent`, `frame top`, `frame popup`
    /// (the window opened last), `back`, `forward`.
    #[arg(long, requires = "js")]
    action: Vec<String>,
    /// With --js: write a PNG of the page (the viewport) to this path once
    /// it has settled.
    #[arg(long, requires = "js")]
    screenshot: Option<PathBuf>,
    /// With --screenshot: capture the whole document, not just the viewport.
    #[arg(long, requires = "screenshot")]
    full_page: bool,
}

/// How to reach the network: identity, limits, proxy, cookies.
#[derive(Args, Clone)]
struct NetArgs {
    /// User-Agent header.
    #[arg(long)]
    user_agent: Option<String>,
    /// Path to a key file from `catpaw keygen`; enables Web Bot Auth signing.
    #[arg(long, requires = "signature_agent")]
    bot_auth_key: Option<PathBuf>,
    /// Origin serving the key directory, e.g. https://agent.example
    #[arg(long)]
    signature_agent: Option<String>,
    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    /// The most megabytes a response body may have (on the wire; decoded
    /// bodies may be twice that).
    #[arg(long, default_value_t = 32)]
    max_response_mb: usize,
    /// Let requests reach loopback, private and link-local addresses.
    #[arg(long)]
    allow_private_network: bool,
    /// An HTTP (CONNECT) or SOCKS5 proxy, e.g. http://user:pass@host:3128
    /// or socks5h://host:1080.
    #[arg(long)]
    proxy: Option<String>,
    /// A cookie file (JSON) to load before the request and save after it.
    #[arg(long)]
    cookie_jar: Option<PathBuf>,
}

/// `catpaw mcp`: serve the agent tools.
#[derive(Args)]
struct McpArgs {
    /// Speak MCP over stdin and stdout (newline-delimited JSON-RPC).
    #[arg(long)]
    stdio: bool,
    #[command(flatten)]
    net: NetArgs,
    /// Snapshot line format results start with: compact (`e12 link
    /// "Home"`) or aria (`- link "Home" [ref=e12]`).
    #[arg(long, default_value = "compact")]
    format: String,
    /// A JSON file holding `localStorage` by origin, read at start and
    /// written back at exit.
    #[arg(long)]
    storage: Option<PathBuf>,
    /// How long (in milliseconds) one run of script may take before it is
    /// stopped; 0 lets scripts run as long as they like.
    #[arg(long, default_value_t = 10_000)]
    script_budget: u64,
    /// Record every request and response to this HAR file (`.har.zst` is
    /// compressed), written at exit.
    #[arg(long, conflicts_with = "replay_har")]
    record_har: Option<PathBuf>,
    /// Answer every request from this HAR recording instead of the network.
    #[arg(long)]
    replay_har: Option<PathBuf>,
    /// With --replay-har: let requests the recording has no answer for go
    /// to the network instead of failing.
    #[arg(long, requires = "replay_har")]
    replay_misses_live: bool,
    /// Seed `Math.random` and `crypto.getRandomValues` (repeatable runs).
    #[arg(long)]
    random_seed: Option<u64>,
    /// Start page clocks at this Unix time in milliseconds (repeatable
    /// runs).
    #[arg(long)]
    time_origin: Option<f64>,
    /// The time zone dates show, as an offset from UTC (`+08:00`, `-0430`,
    /// `+9`) or `UTC`; UTC by default, whatever the machine's.
    #[arg(long, default_value = "UTC", value_parser = parse_timezone, allow_hyphen_values = true)]
    timezone: i32,
    /// What needs the user's approval: default (form submissions and
    /// uploads), strict (also script sending data to other sites, and
    /// evaluate) or open (nothing; for test runs).
    #[arg(long, default_value = "default")]
    policy: String,
    /// A host (with its subdomains) whose submissions need no approval;
    /// may repeat.
    #[arg(long = "trust", value_name = "HOST")]
    trusted: Vec<String>,
    /// Let tabs show documents (pages, popups, frames) of this domain (with
    /// its subdomains) only; the requests pages make for resources and data
    /// are not limited. May repeat.
    #[arg(long = "allowed-domain", value_name = "DOMAIN")]
    allowed_domains: Vec<String>,
    /// The file with the key that approves confirmations on the local
    /// approval page (made when missing; by default in the user's data
    /// directory).
    #[arg(long)]
    approval_key_file: Option<PathBuf>,
    /// The port of the local approval and hand-off pages (by default
    /// 47115 while it is free, so that the browser keeps the key; 0 for
    /// any free one).
    #[arg(long)]
    approval_port: Option<u16>,
    /// Keep a journal of every call (and confirmation) in this directory.
    #[arg(long)]
    flight_log: Option<PathBuf>,
    /// With a journal (--flight-log, or a profile's): keep a screenshot
    /// after each page action.
    #[arg(long)]
    flight_screens: bool,
    /// A directory that keeps cookies, localStorage, checkpoints and the
    /// journal between sessions.
    #[arg(long, conflicts_with_all = ["cookie_jar", "storage"])]
    profile: Option<PathBuf>,
    /// Offer an optional tool: session (checkpoints); may repeat.
    #[arg(long = "tools", value_name = "TOOL")]
    tools: Vec<String>,
}

#[derive(Args)]
struct KeygenArgs {
    /// Where to write the private key (JWK JSON). Defaults to ./catpaw-agent-key.json
    #[arg(long, default_value = "catpaw-agent-key.json")]
    out: PathBuf,
    /// Overwrite an existing file.
    #[arg(long)]
    force: bool,
}

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Fetch(args) if args.js => fetch_with_scripts(args),
        Cmd::Fetch(args) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting the async runtime")?;
            runtime.block_on(fetch(args))
        }
        Cmd::Keygen(args) => keygen(args),
        Cmd::Mcp(args) => mcp(args),
        Cmd::Setup(args) => setup::run(args),
    }
}

fn mcp(args: McpArgs) -> Result<()> {
    if !args.stdio {
        bail!("pass --stdio: MCP over stdin and stdout is the only transport so far");
    }
    let format = catpaw_agent::Format::parse(&args.format)
        .with_context(|| format!("--format {:?}: expected compact or aria", args.format))?;
    let mut config = catpaw_server::SessionConfig {
        format,
        ..catpaw_server::SessionConfig::default()
    };
    config.options.net = net_config(&args.net)?;
    config.options.page.script_budget =
        (args.script_budget > 0).then(|| Duration::from_millis(args.script_budget));
    config.options.storage = read_storage_file(args.storage.as_deref())?;
    config.options.net.recording = match (&args.record_har, &args.replay_har) {
        (Some(path), _) => Some(catpaw_net::Recording::Record(path.clone())),
        (None, Some(path)) => Some(catpaw_net::Recording::Replay {
            path: path.clone(),
            misses: if args.replay_misses_live {
                catpaw_net::Misses::Live
            } else {
                catpaw_net::Misses::Fail
            },
        }),
        (None, None) => None,
    };
    config.options.page.random_seed = args.random_seed;
    config.options.page.time_origin_unix_ms = args.time_origin;
    config.options.page.timezone_offset_minutes = args.timezone;
    config.policy = catpaw_server::Policy {
        preset: catpaw_server::Preset::parse(&args.policy).with_context(|| {
            format!(
                "--policy {:?}: expected default, strict or open",
                args.policy
            )
        })?,
        trusted: args.trusted.clone(),
        allowed_domains: args.allowed_domains.clone(),
    };
    config.approval = catpaw_server::ApprovalConfig {
        key_file: args.approval_key_file.clone(),
        port: args.approval_port,
        ..catpaw_server::ApprovalConfig::default()
    };
    // A profile keeps a journal of its own; screenshots go with whichever
    // journal there is.
    let journal_dir = args.flight_log.clone().or_else(|| {
        args.profile
            .as_deref()
            .map(catpaw_server::profile::journal_dir)
    });
    if args.flight_screens && journal_dir.is_none() {
        bail!("--flight-screens keeps screenshots in a journal: pass --flight-log or --profile");
    }
    config.journal = journal_dir.map(|dir| catpaw_server::JournalConfig {
        dir,
        screens: args.flight_screens,
    });
    config.profile = args.profile.clone();
    for tool in &args.tools {
        if !catpaw_server::OPTIONAL_TOOLS.iter().any(|t| t.name == tool) {
            bail!("--tools {tool:?}: the optional tools are session");
        }
    }
    config.tools = args.tools.clone();
    let outcome = catpaw_server::serve_stdio_with(config, |session| {
        if let Err(e) = session.save_profile() {
            eprintln!("catpaw: saving the profile: {e}");
        }
        match session.save_recording() {
            Ok(Some(n)) => eprintln!("catpaw: recorded {n} requests"),
            Ok(None) => {}
            Err(e) => eprintln!("catpaw: writing the recording: {e}"),
        }
        if let Err(e) = save_cookie_jar(&args.net, session.cookies()) {
            eprintln!("catpaw: {e:#}");
        }
        if let Some(path) = &args.storage
            && let Err(e) = write_storage_file(path, session.storage())
        {
            eprintln!("catpaw: {e:#}");
        }
    });
    outcome.context("serving MCP on stdio")
}

fn keygen(args: KeygenArgs) -> Result<()> {
    if args.out.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite",
            args.out.display()
        );
    }
    let key = KeyPair::generate().context("generating an Ed25519 key")?;
    // A private key: only its owner may read it.
    catpaw_server::profile::write_whole(&args.out, &key.to_json())
        .with_context(|| format!("writing {}", args.out.display()))?;
    eprintln!("wrote private key to {}", args.out.display());
    eprintln!("kid (JWK thumbprint): {}", key.kid);
    eprintln!(
        "serve this document at https://<your-agent-origin>{} :",
        catpaw_net::bot_auth::DIRECTORY_PATH
    );
    println!("{}", key.directory_document());
    Ok(())
}

/// Adapts the style engine to the agent layer's visibility questions.
struct EngineOracle(StyleEngine);

impl StyleOracle for EngineOracle {
    fn is_display_none(&self, dom: &Dom, id: NodeId) -> bool {
        self.0.is_display_none(dom, id)
    }

    fn is_visibility_hidden(&self, _dom: &Dom, id: NodeId) -> bool {
        self.0.is_visibility_hidden(id)
    }

    fn is_block_level(&self, _dom: &Dom, id: NodeId) -> Option<bool> {
        self.0.is_block_level(id)
    }
}

/// `media` attribute check for `<style>`/`<link>`: screen or all qualify.
fn media_applies(media: Option<&str>) -> bool {
    match media {
        None => true,
        Some(m) => {
            let m = m.trim().to_ascii_lowercase();
            m.is_empty() || m.contains("screen") || m.contains("all") || !m.contains("print")
        }
    }
}

enum SheetSource {
    Inline(String),
    Link(Url),
}

/// Collects `<style>` text and `<link rel=stylesheet>` sheets in document
/// order, fetches the linked ones, and resolves styles.
async fn style_document(client: &NetClient, dom: &Dom, url: &Url) -> (StyleEngine, usize) {
    let mut engine = StyleEngine::new(&StyleOptions {
        base_url: url.clone(),
        ..StyleOptions::default()
    });
    engine.set_quirks_mode(dom.quirks_mode());

    let mut sources = Vec::new();
    for n in dom.descendants(dom.document()) {
        let Some(el) = dom.element(n) else { continue };
        if !el.is_html() {
            continue;
        }
        match &*el.name.local {
            "style" if media_applies(el.attr("media")) => {
                sources.push(SheetSource::Inline(dom.text_content(n)));
            }
            "link" => {
                let is_stylesheet = el.attr("rel").is_some_and(|rel| {
                    rel.split_ascii_whitespace()
                        .any(|t| t.eq_ignore_ascii_case("stylesheet"))
                });
                if is_stylesheet
                    && media_applies(el.attr("media"))
                    && let Some(href) = el.attr("href")
                    && let Ok(sheet_url) = url.join(href.trim())
                {
                    sources.push(SheetSource::Link(sheet_url));
                }
            }
            _ => {}
        }
    }

    let mut fetched = 0usize;
    for source in sources {
        match source {
            SheetSource::Inline(css) => engine.add_author_stylesheet(&css),
            SheetSource::Link(sheet_url) => match client.get(&sheet_url).await {
                Ok(resp) if resp.status.is_success() => {
                    fetched += 1;
                    engine.add_author_stylesheet(&String::from_utf8_lossy(&resp.body));
                }
                Ok(resp) => eprintln!("stylesheet {sheet_url}: HTTP {}", resp.status.as_u16()),
                Err(err) => eprintln!("stylesheet {sheet_url}: {err}"),
            },
        }
    }
    engine.restyle(dom);
    (engine, fetched)
}

fn parse_url(input: &str) -> Result<Url> {
    Url::parse(input)
        .or_else(|_| Url::parse(&format!("https://{input}")))
        .with_context(|| format!("invalid URL {input}"))
}

/// `--timezone`: `UTC` (or `GMT`, `Z`), or an offset from it such as
/// `+08:00`, `-0430`, `+9` or `UTC+8`, as minutes east of UTC.
fn parse_timezone(input: &str) -> Result<i32, String> {
    let text = input.trim();
    let upper = text.to_ascii_uppercase();
    let offset = ["UTC", "GMT"]
        .iter()
        .find_map(|zone| upper.strip_prefix(zone))
        .unwrap_or(&upper);
    if offset.is_empty() || offset == "Z" {
        return Ok(0);
    }
    let invalid = || format!("{input:?}: expected UTC or an offset such as +08:00 or -0430");
    let (sign, digits) = match offset.as_bytes().first() {
        Some(b'+') => (1, &offset[1..]),
        Some(b'-') => (-1, &offset[1..]),
        _ => return Err(invalid()),
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if digits.len() > 2 => digits.split_at(digits.len() - 2),
        None => (digits, "0"),
    };
    let all_digits =
        |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(hours) || !all_digits(minutes) {
        return Err(invalid());
    }
    let (hours, minutes): (i32, i32) = (
        hours.parse().map_err(|_| invalid())?,
        minutes.parse().map_err(|_| invalid())?,
    );
    if hours > 23 || minutes > 59 {
        return Err(invalid());
    }
    Ok(sign * (hours * 60 + minutes))
}

fn net_config(args: &NetArgs) -> Result<NetConfig> {
    let mut config = NetConfig {
        timeout: Duration::from_secs(args.timeout),
        ..NetConfig::default()
    };
    config.max_response_bytes = args.max_response_mb.saturating_mul(1024 * 1024);
    config.max_decoded_bytes = config.max_response_bytes.saturating_mul(2);
    config.allow_private_network = args.allow_private_network;
    if let Some(proxy) = &args.proxy {
        config.proxy =
            Some(Url::parse(proxy).with_context(|| format!("parsing the proxy URL {proxy}"))?);
    }
    if let Some(path) = &args.cookie_jar
        && path.exists()
    {
        config.cookies_json = Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading the cookie file {}", path.display()))?,
        );
    }
    if let Some(ua) = &args.user_agent {
        config.user_agent = ua.clone();
    }
    if let (Some(path), Some(agent)) = (&args.bot_auth_key, &args.signature_agent) {
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let key = KeyPair::from_json(&json).context("parsing the key file")?;
        config.bot_auth = Some(BotAuthConfig::new(key, agent.clone()));
    }
    Ok(config)
}

fn chosen_view(args: &FetchArgs) -> View {
    if args.markdown {
        View::Markdown
    } else if args.text {
        View::Text
    } else if args.html {
        View::Html
    } else if args.links {
        View::Links
    } else if args.forms {
        View::Forms
    } else {
        args.view
    }
}

async fn fetch(args: FetchArgs) -> Result<()> {
    let url = parse_url(&args.url)?;
    let client = NetClient::new(net_config(&args.net)?).context("building the HTTP client")?;

    let started = Instant::now();
    let doc = fetch_document(&client, &url)
        .await
        .with_context(|| format!("fetching {url}"))?;
    save_cookie_jar(&args.net, client.cookies())?;
    let fetch_ms = started.elapsed().as_millis();
    let response = &doc.response;
    eprintln!(
        "GET {} -> {} {} ({} bytes, {} redirect(s), {} ms, {} via {})",
        response.url,
        response.status.as_u16(),
        response.mime_essence().unwrap_or_default(),
        response.body.len(),
        response.redirect_chain.len(),
        fetch_ms,
        doc.decoded.encoding,
        match doc.decoded.source {
            catpaw_fetch::EncodingSource::Bom => "BOM",
            catpaw_fetch::EncodingSource::Transport => "Content-Type",
            catpaw_fetch::EncodingSource::MetaPrescan => "<meta>",
            catpaw_fetch::EncodingSource::Default => "default",
        }
    );
    if response.is_cloudflare_challenge() {
        eprintln!("note: the response is a Cloudflare challenge page (cf-mitigated: challenge)");
    }
    if args.show_headers {
        for (name, value) in &response.headers {
            eprintln!("  {name}: {}", String::from_utf8_lossy(value.as_bytes()));
        }
    }

    let view = chosen_view(&args);

    let parse_opts = HtmlParseOptions {
        url: Some(response.url.clone()),
        ..HtmlParseOptions::default()
    };
    let parse_started = Instant::now();
    let parsed = parse_html(doc.html(), &parse_opts);
    let dom = &parsed.dom;
    let parse_ms = parse_started.elapsed().as_millis();

    let oracle: Box<dyn StyleOracle> = if args.no_css || view == View::Html {
        Box::new(AttributeOracle)
    } else {
        let style_started = Instant::now();
        let (engine, fetched) = style_document(&client, dom, &response.url).await;
        eprintln!(
            "parsed {} nodes in {} ms; styled with {} author sheet(s) ({} fetched) in {} ms",
            dom.len(),
            parse_ms,
            engine.author_sheet_count(),
            fetched,
            style_started.elapsed().as_millis()
        );
        Box::new(EngineOracle(engine))
    };
    render(&args, view, dom, oracle.as_ref())
}

/// Prints `view` of the document.
fn render(args: &FetchArgs, view: View, dom: &Dom, oracle: &dyn StyleOracle) -> Result<()> {
    match view {
        View::Snapshot => {
            let filter = Filter::parse(&args.filter)
                .with_context(|| format!("unknown filter `{}`", args.filter))?;
            let format = Format::parse(&args.format)
                .with_context(|| format!("unknown format `{}`", args.format))?;
            let extra = ExtraAttrs::parse_list(&args.attrs).map_err(anyhow::Error::msg)?;
            let mut refs = RefTable::new();
            let mut snapshotter = Snapshotter::new(dom, oracle, &mut refs);
            let snapshot = snapshotter.snapshot(&SnapshotOptions {
                filter,
                format,
                extra,
                max_chars: args.max_chars,
                ..SnapshotOptions::default()
            });
            print!("{}", snapshot.text);
        }
        View::Markdown => {
            let options = ReadOptions {
                main_only: args.main_only,
                ..ReadOptions::default()
            };
            print!("{}", catpaw_agent::markdown(dom, oracle, None, &options));
        }
        View::Text => print!("{}", catpaw_agent::text(dom, oracle)),
        View::Html => println!("{}", to_html(dom, dom.document(), true)),
        View::Links => {
            for link in catpaw_agent::links(dom, oracle, None) {
                println!("{}\t{}", link.href, link.text);
            }
        }
        View::Forms => {
            for (i, form) in catpaw_agent::forms(dom, oracle, None).iter().enumerate() {
                println!(
                    "form #{i}: {} {}",
                    form.method,
                    form.action.as_deref().unwrap_or("(no action)")
                );
                for field in &form.fields {
                    let mut line = format!(
                        "  [{}] {}",
                        field.kind,
                        field.name.as_deref().unwrap_or("-")
                    );
                    if !field.label.is_empty() {
                        line.push_str(&format!(" label={:?}", field.label));
                    }
                    if !field.value.is_empty() {
                        line.push_str(&format!(" value={:?}", field.value));
                    }
                    if field.checked == Some(true) {
                        line.push_str(" checked");
                    }
                    if field.required {
                        line.push_str(" required");
                    }
                    if !field.options.is_empty() {
                        line.push_str(&format!(" options={:?}", field.options));
                    }
                    println!("{line}");
                }
            }
        }
    }
    Ok(())
}

/// Lists the frames below the top one, with their console output.
fn print_frames(page: &catpaw_engine::Page, list: bool, console: bool) {
    for frame in page.frames().into_iter().skip(1) {
        let id = frame.id.0;
        if list {
            let stop = page
                .frame_report(frame.id)
                .map(describe_stop)
                .unwrap_or_default();
            eprintln!(
                "[frame {id}] {}={} depth={} {} ({stop})",
                if frame.popup { "opener" } else { "parent" },
                frame.parent.map(|p| p.0).unwrap_or(0),
                frame.depth,
                frame.url
            );
        }
        if console && let Some(state) = page.frame_state(frame.id) {
            for message in state.console_messages() {
                eprintln!(
                    "[frame {id} console.{}] {}",
                    message.level.as_str(),
                    message.text
                );
            }
            for (name, count) in state.stub_calls.borrow().iter() {
                eprintln!("[frame {id} stub] {name} x{count}");
            }
            for error in state.errors.borrow().iter().take(5) {
                eprintln!(
                    "[frame {id} error] {}",
                    error.lines().next().unwrap_or_default()
                );
            }
        }
    }
    for worker in page.workers() {
        let key = worker.key;
        if list {
            let stop = page
                .worker_report(key)
                .map(describe_stop)
                .unwrap_or_default();
            let owner = match worker.owner {
                catpaw_engine::ScopeId::Frame(f) => format!("frame {}", f.0),
                catpaw_engine::ScopeId::Worker(w) => format!("worker {w}"),
            };
            eprintln!("[worker {key}] owner={owner} {} ({stop})", worker.url);
        }
        if console && let Some(state) = page.worker_state(key) {
            for message in state.console_messages() {
                eprintln!(
                    "[worker {key} console.{}] {}",
                    message.level.as_str(),
                    message.text
                );
            }
            for error in state.errors.borrow().iter().take(5) {
                eprintln!(
                    "[worker {key} error] {}",
                    error.lines().next().unwrap_or_default()
                );
            }
        }
    }
}

fn describe_stop(report: &catpaw_engine::LoopReport) -> String {
    match report.stop {
        StopReason::Idle => "settled".to_string(),
        StopReason::Settled => format!(
            "settled with {} timer(s) and {} request(s) left that it does not wait for",
            report.pending_timers, report.inflight_requests
        ),
        StopReason::VirtualBudget => format!(
            "stopped at the time budget with {} timer(s) pending",
            report.pending_timers
        ),
        StopReason::WallBudget => format!(
            "stopped at the wall-clock limit with {} request(s) in flight",
            report.inflight_requests
        ),
        StopReason::StepBudget => "stopped at the step limit".to_string(),
        StopReason::Navigation => "stopped by a navigation".to_string(),
    }
}

/// Runs one `--action` specification.
fn run_action(page: &mut catpaw_engine::Page, spec: &str) -> Result<()> {
    let spec = spec.trim();
    let (verb, rest) = spec.split_once(' ').unwrap_or((spec, ""));
    let rest = rest.trim();
    let two = || -> Result<(&str, &str)> {
        rest.split_once(' ')
            .map(|(a, b)| (a.trim(), b.trim()))
            .ok_or_else(|| anyhow::anyhow!("`{verb}` needs a selector and a value: {spec:?}"))
    };
    let outcome = match verb {
        "click" => page.click(rest),
        "fill" => {
            let (selector, text) = two()?;
            page.fill(selector, text)
        }
        "type" => page.type_text(rest),
        "press" => page.press(rest),
        "check" => page.set_checked(rest, true),
        "uncheck" => page.set_checked(rest, false),
        "select" => {
            let (selector, value) = two()?;
            page.select(selector, value)
        }
        "hover" => page.hover(rest),
        "focus" => page.focus(rest),
        "back" => page.back().map_err(Into::into),
        "forward" => page.forward().map_err(Into::into),
        "frame" => match rest {
            "top" => {
                page.select_top_frame();
                Ok(())
            }
            "parent" => {
                page.select_parent_frame();
                Ok(())
            }
            "popup" => page.select_latest_popup().map(drop),
            selector => page.select_frame(selector).map(drop),
        },
        other => bail!("unknown action {other:?} in {spec:?}"),
    };
    outcome.with_context(|| format!("action {spec:?}"))?;
    eprintln!("[action] {spec}: {}", describe_stop(page.report()));
    Ok(())
}

/// `fetch --js`: load the page in the engine, let its scripts run until it
/// settles, then print the requested view of the resulting document.
fn fetch_with_scripts(args: FetchArgs) -> Result<()> {
    let url = parse_url(&args.url)?;
    let options = PageOptions {
        net: net_config(&args.net)?,
        page: PageConfig {
            script_budget: (args.script_budget > 0)
                .then(|| Duration::from_millis(args.script_budget)),
            timezone_offset_minutes: args.timezone,
            ..PageConfig::default()
        },
        limits: LoopLimits {
            virtual_ms: args.time_budget as f64,
            // Analytics, polling and far timers are not waited for, as an
            // agent session does not wait for them.
            settle: Some(SettlePolicy::default()),
            ..LoopLimits::default()
        },
        storage: read_storage_file(args.storage.as_deref())?,
        ..PageOptions::default()
    };
    let started = Instant::now();
    catpaw_engine::with_page(url.clone(), options, move |page| -> Result<()> {
        let result = fetch_with_scripts_on(&args, page, started);
        save_cookie_jar(&args.net, page.net().client().cookies())?;
        if let Some(path) = &args.storage {
            write_storage_file(path, page.storage_snapshot())?;
        }
        result
    })
    .with_context(|| format!("loading {url}"))?
}

/// The output of `fetch --js`, once the page is open.
fn fetch_with_scripts_on(
    args: &FetchArgs,
    page: &mut catpaw_engine::Page,
    started: Instant,
) -> Result<()> {
    {
        let document = page.document().clone();
        eprintln!(
            "GET {} -> {} {} ({} bytes, {} redirect(s), {})",
            document.url,
            document.status,
            document.mime.as_deref().unwrap_or_default(),
            document.body_bytes,
            document.redirects,
            document.encoding,
        );
        if document.cloudflare_challenge {
            eprintln!(
                "note: the response is a Cloudflare challenge page (cf-mitigated: challenge)"
            );
        }
        if args.show_headers {
            for (name, value) in &document.headers {
                eprintln!("  {name}: {value}");
            }
        }
        for hop in page.navigations().iter().skip(1) {
            eprintln!("script navigated to {hop}");
        }
        let mut hops_seen = page.navigations().len();

        let requests = page.net().requests();
        let failed = requests.iter().filter(|r| r.status.is_none()).count();
        let state = page.state().clone();
        eprintln!(
            "scripts ran for {} ms: {} request(s) ({} failed), {} step(s), {} ms of timers skipped, {}; {} uncaught error(s)",
            started.elapsed().as_millis(),
            requests.len(),
            failed,
            page.report().steps,
            page.report().virtual_advanced_ms.round(),
            describe_stop(page.report()),
            state.errors.borrow().len(),
        );
        if args.requests {
            for request in &requests {
                let status = request
                    .status
                    .map_or_else(|| "failed".to_string(), |s| s.to_string());
                eprintln!("[request] {} {} -> {}", request.method, request.url, status);
                if let Some(body) = &request.body_preview {
                    for line in body.lines().take(20) {
                        eprintln!("    {line}");
                    }
                }
            }
        }
        if args.console {
            for message in state.console_messages() {
                eprintln!("[console.{}] {}", message.level.as_str(), message.text);
            }
            for (name, count) in state.stub_calls.borrow().iter() {
                eprintln!("[stub] {name} x{count}");
            }
        } else {
            for error in state.errors.borrow().iter().take(5) {
                eprintln!("[page error] {}", error.lines().next().unwrap_or_default());
            }
        }
        if args.frames || args.console {
            print_frames(page, args.frames, args.console);
        }

        for spec in &args.action {
            run_action(page, spec)?;
            for hop in page.navigations().iter().skip(hops_seen) {
                eprintln!("navigated to {hop}");
            }
            hops_seen = page.navigations().len();
        }
        if let Some(path) = &args.screenshot {
            let png = page.screenshot(args.full_page);
            std::fs::write(path, &png).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("screenshot: {} ({} bytes)", path.display(), png.len());
            if args.eval.is_none() {
                return Ok(());
            }
        }
        if let Some(source) = &args.eval {
            // A promise is awaited, within the same time budget as the page.
            let limits = LoopLimits {
                virtual_ms: args.time_budget as f64,
                ..LoopLimits::default()
            };
            match page.eval_awaited(source, &limits) {
                Ok(value) => println!("{value}"),
                Err(e) => bail!("the script threw: {e}"),
            }
            return Ok(());
        }

        let view = chosen_view(args);
        let page_url = page.url();
        let dom = page.dom();
        let oracle: Box<dyn StyleOracle> = if args.no_css || view == View::Html {
            Box::new(AttributeOracle)
        } else {
            let style_started = Instant::now();
            let (engine, fetched) =
                page.net()
                    .block_on(style_document(page.net().client(), &dom, &page_url));
            eprintln!(
                "{} nodes; styled with {} author sheet(s) ({} fetched) in {} ms",
                dom.len(),
                engine.author_sheet_count(),
                fetched,
                style_started.elapsed().as_millis()
            );
            Box::new(EngineOracle(engine))
        };
        render(args, view, &dom, oracle.as_ref())
    }
}

/// Reads a `--storage` file: an object of origins, each an object of
/// `localStorage` keys and values. A missing file is empty storage.
fn read_storage_file(path: Option<&std::path::Path>) -> Result<catpaw_server::profile::Storage> {
    let Some(path) = path.filter(|p| p.exists()) else {
        return Ok(catpaw_server::profile::Storage::new());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the storage file {}", path.display()))?;
    catpaw_server::profile::parse_storage(&text)
        .map_err(|e| anyhow::anyhow!("parsing the storage file {}: {e}", path.display()))
}

/// Writes storage as `--storage` reads it, readable by its owner alone.
fn write_storage_file(
    path: &std::path::Path,
    storage: catpaw_server::profile::Storage,
) -> Result<()> {
    let text = catpaw_server::profile::storage_json(&storage);
    catpaw_server::profile::write_whole(path, &text)
        .with_context(|| format!("writing the storage file {}", path.display()))
}

/// Writes the cookie jar where `--cookie-jar` says, readable by its owner
/// alone.
fn save_cookie_jar(args: &NetArgs, jar: &catpaw_net::CookieJar) -> Result<()> {
    if let Some(path) = &args.cookie_jar {
        catpaw_server::profile::write_whole(path, &jar.to_json())
            .with_context(|| format!("writing the cookie file {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_timezone;

    #[test]
    fn time_zones_are_utc_or_an_offset() {
        for (input, minutes) in [
            ("UTC", 0),
            ("gmt", 0),
            ("Z", 0),
            ("+08:00", 480),
            ("-04:00", -240),
            ("+0530", 330),
            ("-0430", -270),
            ("+9", 540),
            ("UTC+8", 480),
            ("GMT-03:30", -210),
        ] {
            assert_eq!(parse_timezone(input), Ok(minutes), "{input}");
        }
        for input in [
            "Asia/Shanghai",
            "8",
            "+24:00",
            "+08:60",
            "+",
            "UTC+x",
            "+123:00",
        ] {
            assert!(parse_timezone(input).is_err(), "{input}");
        }
    }
}
