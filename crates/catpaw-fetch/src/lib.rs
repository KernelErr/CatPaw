//! CatPaw fetch layer. The full Fetch specification (CORS, referrer policy,
//! CSP, caching, request/response streams) lands with scripting in M1; M0
//! provides document retrieval plus the HTML encoding sniffing algorithm.

pub mod encoding;

pub use encoding::{DecodedDocument, EncodingSource, decode_document};

use catpaw_net::{NetClient, NetError, Response, Url};

/// A fetched, decoded HTML document.
#[derive(Debug)]
pub struct FetchedDocument {
    pub response: Response,
    pub decoded: DecodedDocument,
}

impl FetchedDocument {
    /// The final URL after redirects.
    pub fn url(&self) -> &Url {
        &self.response.url
    }

    pub fn html(&self) -> &str {
        &self.decoded.text
    }
}

/// Fetches `url` as a navigation request and decodes the body as HTML.
pub async fn fetch_document(client: &NetClient, url: &Url) -> Result<FetchedDocument, NetError> {
    let response = client.get(url).await?;
    let charset = response.charset();
    let decoded = decode_document(&response.body, charset.as_deref());
    Ok(FetchedDocument { response, decoded })
}
