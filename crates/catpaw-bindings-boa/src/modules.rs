//! ECMAScript modules: the loader behind `import` and `import()`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use boa_engine::module::{Module, ModuleLoader, ModuleRequest, Referrer};
use boa_engine::{Context, JsNativeError, JsObject, JsResult, JsString, Source, js_string};
use catpaw_web::PageState;
use catpaw_web::net::{NetRequest, RequestKind};
use catpaw_web::scripting::resolve_module_specifier;
use url::Url;

/// Loads modules over the page's network, once per URL.
pub struct PageModuleLoader {
    page: Rc<PageState>,
    modules: RefCell<HashMap<String, Module>>,
    /// The URLs started ahead (see [`PageModuleLoader::prefetch_imports`]).
    prefetched: RefCell<HashSet<String>>,
}

/// The most modules a page's loader starts ahead.
const MAX_PREFETCHED: usize = 4096;

/// The specifiers of a module's static imports and re-exports
/// (`import … from "x"`, `import "x"`, `export … from "x"`) that name a
/// URL or a path, found by a scan of the source rather than a parse: a
/// string that only looks like one costs a request, nothing more.
/// `import("x")` is left out: it may never run.
fn static_imports(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    for keyword in ["from", "import"] {
        let mut start = 0;
        while let Some(found) = source[start..].find(keyword) {
            let at = start + found;
            start = at + keyword.len();
            let before = at.checked_sub(1).map(|i| bytes[i]);
            if before.is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'.'))
            {
                continue;
            }
            let mut i = start;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let Some(&quote) = bytes.get(i).filter(|&&b| b == b'"' || b == b'\'') else {
                continue;
            };
            let Some(len) = source[i + 1..].find(quote as char) else {
                continue;
            };
            let specifier = &source[i + 1..i + 1 + len];
            let looks_like_a_path = ["./", "../", "/", "https://", "http://"]
                .iter()
                .any(|p| specifier.starts_with(p));
            if looks_like_a_path
                && specifier.len() < 2048
                && !specifier.contains(|c: char| c.is_whitespace())
            {
                out.push(specifier.to_string());
            }
        }
    }
    out
}

fn load_error(message: String) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message).into()
}

impl PageModuleLoader {
    pub fn new(page: Rc<PageState>) -> Self {
        Self {
            page,
            modules: RefCell::new(HashMap::new()),
            prefetched: RefCell::new(HashSet::new()),
        }
    }

    /// Records the module a script element was evaluated as, so that an
    /// import of the same URL does not load it a second time.
    pub fn register(&self, url: &str, module: Module) {
        self.modules
            .borrow_mut()
            .entry(url.to_string())
            .or_insert(module);
    }

    fn request(&self, url: &Url) -> NetRequest {
        let mut request = NetRequest::get(url.clone(), RequestKind::Script);
        request.referrer = Some(self.page.url.borrow().clone());
        request
    }

    /// Starts loading the modules `source` (the module at `url`) imports
    /// statically, so that they are on their way while the engine asks
    /// for them one by one.
    pub fn prefetch_imports(&self, url: &Url, source: &str) {
        let mut prefetched = self.prefetched.borrow_mut();
        for specifier in static_imports(source) {
            if prefetched.len() >= MAX_PREFETCHED {
                return;
            }
            let Some(target) = resolve_module_specifier(&self.page, &specifier, url) else {
                continue;
            };
            if self.modules.borrow().contains_key(target.as_str())
                || !prefetched.insert(target.to_string())
            {
                continue;
            }
            catpaw_web::net::prefetch(&self.page, self.request(&target));
        }
    }

    fn fetch(&self, url: &Url) -> JsResult<String> {
        let request = self.request(url);
        let response = catpaw_web::net::fetch_blocking(&self.page, request)
            .map_err(|reason| load_error(format!("Failed to fetch module {url}: {reason}")))?;
        if !response.is_success() {
            return Err(load_error(format!(
                "Failed to fetch module {url}: HTTP {}",
                response.status
            )));
        }
        // Modules are always UTF-8.
        let text = String::from_utf8_lossy(&response.body);
        Ok(text.strip_prefix('\u{FEFF}').unwrap_or(&text).to_string())
    }
}

impl ModuleLoader for PageModuleLoader {
    async fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> JsResult<Module> {
        let specifier = request.specifier().to_std_string_lossy();
        // Scripts and modules are compiled with their URL as their path.
        let base = referrer
            .path()
            .and_then(Path::to_str)
            .and_then(|path| Url::parse(path).ok())
            .unwrap_or_else(|| self.page.base_url());
        let url = resolve_module_specifier(&self.page, &specifier, &base).ok_or_else(|| {
            load_error(format!(
                "Failed to resolve module specifier \"{specifier}\""
            ))
        })?;

        let cached = self.modules.borrow().get(url.as_str()).cloned();
        if let Some(module) = cached {
            return Ok(module);
        }

        let text = self.fetch(&url)?;
        let is_json = request
            .get_attribute("type")
            .is_some_and(|ty| ty.to_std_string_lossy() == "json");
        if !is_json {
            self.prefetch_imports(&url, &text);
        }
        let module = if is_json {
            Module::parse_json(JsString::from(text.as_str()), &mut context.borrow_mut())?
        } else {
            Module::parse(
                Source::from_bytes(&text).with_path(Path::new(url.as_str())),
                None,
                &mut context.borrow_mut(),
            )?
        };
        self.modules
            .borrow_mut()
            .insert(url.to_string(), module.clone());
        Ok(module)
    }

    fn init_import_meta(
        self: Rc<Self>,
        import_meta: &JsObject,
        module: &Module,
        context: &mut Context,
    ) {
        if let Some(url) = module.path().and_then(Path::to_str) {
            let _ = import_meta.create_data_property_or_throw(
                js_string!("url"),
                JsString::from(url),
                context,
            );
        }
    }
}
