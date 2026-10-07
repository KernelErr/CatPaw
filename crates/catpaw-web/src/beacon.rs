//! `navigator.sendBeacon()`.
//!
//! A beacon is a POST whose response nobody reads. It goes out through the
//! same path as `fetch()` but counts as background traffic: a page waiting
//! on its beacons would never settle, since they are sent as it unloads.

use catpaw_js::{Exception, Fallible, ObjectId};

use crate::cors::{self, Credentials, Mode, Outgoing};
use crate::generated::{
    self as web, ReadableStreamOrBlobOrBufferSourceOrFormDataOrURLSearchParamsOrString,
};
use crate::net::RequestKind;
use crate::page::Cx;
use crate::{Web, fetch};

impl web::NavigatorImpl for Web {
    /// <https://w3c.github.io/beacon/#sendbeacon-method>
    fn send_beacon(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        url: String,
        data: Option<ReadableStreamOrBlobOrBufferSourceOrFormDataOrURLSearchParamsOrString>,
    ) -> Fallible<bool> {
        let Some(url) = cx.page.resolve_url(&url) else {
            return Err(Exception::type_error(format!("Invalid URL: {url}")));
        };
        let (body, content_type) = match data {
            Some(data) => {
                let (bytes, content_type) = fetch::extract_body(cx, data)?;
                (Some(bytes), content_type)
            }
            None => (None, None),
        };
        let out = Outgoing {
            method: "POST".to_string(),
            url,
            headers: content_type
                .map(|t| ("content-type".to_string(), t.to_string()))
                .into_iter()
                .collect(),
            body,
            mode: Mode::NoCors,
            credentials: Credentials::Include,
            kind: RequestKind::Beacon,
        };
        let page = cx.page;
        page.background_requests
            .set(page.background_requests.get() + 1);
        cors::send(page, out, |cx, _result| {
            let page = cx.page;
            page.background_requests
                .set(page.background_requests.get().saturating_sub(1));
        });
        Ok(true)
    }
}
