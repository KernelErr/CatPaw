//! The `catpaw` command line.
//!
//! M0 surface: `fetch` (retrieve a page, resolve its styles, and print a CST
//! snapshot, markdown, text, HTML, links or forms) and `keygen` (Web Bot Auth
//! key pair + key directory document).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use catpaw_agent::{
    AttributeOracle, Filter, ReadOptions, RefTable, SnapshotOptions, Snapshotter, StyleOracle,
};
use catpaw_dom::{Dom, HtmlParseOptions, NodeId, parse_html, to_html};
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
enum Cmd {
    /// Fetch a page and print what an agent would see.
    Fetch(FetchArgs),
    /// Generate a Web Bot Auth (Ed25519) key pair and its key directory document.
    Keygen(KeygenArgs),
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
    /// Soft character budget for the snapshot.
    #[arg(long)]
    max_chars: Option<usize>,
    /// Only the main content for markdown/text.
    #[arg(long)]
    main_only: bool,
    /// Skip stylesheets: decide visibility from markup alone (faster, less accurate).
    #[arg(long)]
    no_css: bool,
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
    /// Print response headers to stderr.
    #[arg(long)]
    show_headers: bool,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Fetch(args) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting the async runtime")?;
            runtime.block_on(fetch(args))
        }
        Cmd::Keygen(args) => keygen(args),
    }
}

fn keygen(args: KeygenArgs) -> Result<()> {
    if args.out.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite",
            args.out.display()
        );
    }
    let key = KeyPair::generate().context("generating an Ed25519 key")?;
    std::fs::write(&args.out, key.to_json())
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

async fn fetch(args: FetchArgs) -> Result<()> {
    let url = Url::parse(&args.url)
        .or_else(|_| Url::parse(&format!("https://{}", args.url)))
        .with_context(|| format!("invalid URL {}", args.url))?;

    let mut config = NetConfig {
        timeout: Duration::from_secs(args.timeout),
        ..NetConfig::default()
    };
    if let Some(ua) = &args.user_agent {
        config.user_agent = ua.clone();
    }
    if let (Some(path), Some(agent)) = (&args.bot_auth_key, &args.signature_agent) {
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let key = KeyPair::from_json(&json).context("parsing the key file")?;
        config.bot_auth = Some(BotAuthConfig::new(key, agent.clone()));
    }
    let client = NetClient::new(config).context("building the HTTP client")?;

    let started = Instant::now();
    let doc = fetch_document(&client, &url)
        .await
        .with_context(|| format!("fetching {url}"))?;
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

    let view = if args.markdown {
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
    };

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
    let oracle: &dyn StyleOracle = oracle.as_ref();

    match view {
        View::Snapshot => {
            let filter = Filter::parse(&args.filter)
                .with_context(|| format!("unknown filter `{}`", args.filter))?;
            let mut refs = RefTable::new();
            let mut snapshotter = Snapshotter::new(dom, oracle, &mut refs);
            let snapshot = snapshotter.snapshot(&SnapshotOptions {
                filter,
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
            for link in catpaw_agent::links(dom, oracle) {
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
