//! ECMAScript modules: the loader behind `import` and `import()`.

use std::cell::RefCell;
use std::collections::HashMap;
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
}

fn load_error(message: String) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message).into()
}

impl PageModuleLoader {
    pub fn new(page: Rc<PageState>) -> Self {
        Self {
            page,
            modules: RefCell::new(HashMap::new()),
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

    fn fetch(&self, url: &Url) -> JsResult<String> {
        let mut request = NetRequest::get(url.clone(), RequestKind::Script);
        request.referrer = Some(self.page.url.borrow().clone());
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
