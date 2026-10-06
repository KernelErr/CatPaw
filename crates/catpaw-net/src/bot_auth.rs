//! Web Bot Auth: RFC 9421 HTTP Message Signatures with the `web-bot-auth`
//! profile (draft-ietf-webbotauth-httpsig-protocol).
//!
//! A signed request carries three headers:
//!
//! ```text
//! Signature-Agent: sig="https://agent.example"
//! Signature-Input: sig=("@method" "@authority" "@path" "signature-agent";key="sig");created=...;expires=...;keyid=...;nonce=...;alg="ed25519";tag="web-bot-auth"
//! Signature: sig=:...:
//! ```
//!
//! `keyid` is the RFC 7638 JWK thumbprint of the Ed25519 public key, which
//! the verifier looks up at
//! `<Signature-Agent>/.well-known/http-message-signatures-directory`.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::SigningKey;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use web_bot_auth::components::{
    CoveredComponent, DerivedComponent, HTTPField, HTTPFieldParameters, HTTPFieldParametersSet,
};
use web_bot_auth::keyring::{Algorithm, Thumbprintable};
use web_bot_auth::message_signatures::{MessageSigner, UnsignedMessage};

/// Label used for the signature dictionary members.
pub const SIGNATURE_LABEL: &str = "sig";
pub const WEB_BOT_AUTH_TAG: &str = "web-bot-auth";
pub const DIRECTORY_PATH: &str = "/.well-known/http-message-signatures-directory";

#[derive(Debug, thiserror::Error)]
pub enum BotAuthError {
    #[error("system randomness unavailable: {0}")]
    Random(String),
    #[error("invalid key: {0}")]
    InvalidKey(String),
    #[error("key file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("signature agent must be an https URL without a path: {0}")]
    InvalidAgent(String),
}

fn random_bytes<const N: usize>() -> Result<[u8; N], BotAuthError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| BotAuthError::Random(e.to_string()))?;
    Ok(buf)
}

/// An Ed25519 key pair in JWK form. The private scalar is the `d` member;
/// `kid` is the RFC 7638 thumbprint of the public part.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyPair {
    pub kty: String,
    pub crv: String,
    pub x: String,
    pub d: String,
    pub kid: String,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("kid", &self.kid)
            .field("d", &"<redacted>")
            .finish()
    }
}

impl KeyPair {
    /// Generates a fresh key pair from system randomness.
    pub fn generate() -> Result<Self, BotAuthError> {
        Ok(Self::from_seed(random_bytes::<32>()?))
    }

    /// Derives the key pair from a 32-byte Ed25519 seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&seed);
        let x = URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
        let kid = Thumbprintable::OKP {
            crv: "Ed25519".to_string(),
            x: x.clone(),
        }
        .b64_thumbprint();
        Self {
            kty: "OKP".to_string(),
            crv: "Ed25519".to_string(),
            x,
            d: URL_SAFE_NO_PAD.encode(seed),
            kid,
        }
    }

    pub fn from_json(json: &str) -> Result<Self, BotAuthError> {
        let key: Self = serde_json::from_str(json)?;
        if key.kty != "OKP" || key.crv != "Ed25519" {
            return Err(BotAuthError::InvalidKey(format!(
                "expected an OKP/Ed25519 key, found {}/{}",
                key.kty, key.crv
            )));
        }
        // Re-derive the public part so a tampered or mismatched file is rejected.
        let expected = Self::from_seed(key.seed()?);
        if expected.x != key.x {
            return Err(BotAuthError::InvalidKey(
                "public key does not match the private scalar".to_string(),
            ));
        }
        Ok(expected)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("KeyPair serializes")
    }

    pub fn seed(&self) -> Result<[u8; 32], BotAuthError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.d)
            .map_err(|e| BotAuthError::InvalidKey(format!("bad `d`: {e}")))?;
        bytes
            .try_into()
            .map_err(|_| BotAuthError::InvalidKey("`d` must be 32 bytes".to_string()))
    }

    /// The public JWK as published in the key directory.
    pub fn public_jwk(&self) -> serde_json::Value {
        serde_json::json!({
            "kty": self.kty,
            "crv": self.crv,
            "x": self.x,
            "kid": self.kid,
            "use": "sig",
        })
    }

    /// The JWKS document to serve at [`DIRECTORY_PATH`].
    pub fn directory_document(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({ "keys": [self.public_jwk()] }))
            .expect("directory serializes")
    }
}

/// How a client signs its requests.
#[derive(Debug, Clone)]
pub struct BotAuthConfig {
    pub key: KeyPair,
    /// Origin that serves the key directory, e.g. `https://agent.example`.
    pub signature_agent: String,
    /// Signature validity window. Cloudflare recommends about a minute.
    pub expires: Duration,
    /// Hosts (exact or `*.suffix`) to sign for; empty means every host.
    pub sign_for: Vec<String>,
}

impl BotAuthConfig {
    pub fn new(key: KeyPair, signature_agent: impl Into<String>) -> Self {
        Self {
            key,
            signature_agent: signature_agent.into(),
            expires: Duration::from_secs(60),
            sign_for: Vec::new(),
        }
    }
}

/// The three headers produced for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    pub signature_agent: String,
    pub signature_input: String,
    pub signature: String,
}

struct Message<'a> {
    method: &'a str,
    authority: &'a str,
    path: &'a str,
    signature_agent_value: String,
    generated: Option<(String, String)>,
}

impl UnsignedMessage for Message<'_> {
    fn fetch_components_to_cover(&self) -> IndexMap<CoveredComponent, String> {
        IndexMap::from_iter([
            (
                CoveredComponent::Derived(DerivedComponent::Method { req: false }),
                self.method.to_string(),
            ),
            (
                CoveredComponent::Derived(DerivedComponent::Authority { req: false }),
                self.authority.to_string(),
            ),
            (
                CoveredComponent::Derived(DerivedComponent::Path { req: false }),
                self.path.to_string(),
            ),
            (
                CoveredComponent::HTTP(HTTPField {
                    name: "signature-agent".to_string(),
                    parameters: HTTPFieldParametersSet(vec![HTTPFieldParameters::Key(
                        SIGNATURE_LABEL.to_string(),
                    )]),
                }),
                self.signature_agent_value.clone(),
            ),
        ])
    }

    fn register_header_contents(&mut self, signature_input: String, signature_header: String) {
        self.generated = Some((signature_input, signature_header));
    }
}

/// Signs requests for one identity.
pub struct BotAuthSigner {
    seed: [u8; 32],
    config: BotAuthConfig,
}

impl std::fmt::Debug for BotAuthSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotAuthSigner")
            .field("kid", &self.config.key.kid)
            .field("signature_agent", &self.config.signature_agent)
            .finish()
    }
}

impl BotAuthSigner {
    pub fn new(config: &BotAuthConfig) -> Result<Self, BotAuthError> {
        let agent = &config.signature_agent;
        let parsed =
            url::Url::parse(agent).map_err(|_| BotAuthError::InvalidAgent(agent.clone()))?;
        if parsed.scheme() != "https" || parsed.path() != "/" || parsed.query().is_some() {
            return Err(BotAuthError::InvalidAgent(agent.clone()));
        }
        Ok(Self {
            seed: config.key.seed()?,
            config: config.clone(),
        })
    }

    pub fn config(&self) -> &BotAuthConfig {
        &self.config
    }

    /// Whether requests to `host` should be signed under `sign_for`.
    pub fn applies_to(&self, host: &str) -> bool {
        if self.config.sign_for.is_empty() {
            return true;
        }
        self.config.sign_for.iter().any(|pattern| {
            if let Some(suffix) = pattern.strip_prefix("*.") {
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                host.eq_ignore_ascii_case(pattern)
            }
        })
    }

    /// Produces the signature headers for a request line.
    pub fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
    ) -> Result<SignedHeaders, BotAuthError> {
        let nonce = STANDARD.encode(random_bytes::<64>()?);
        let signer = MessageSigner {
            keyid: self.config.key.kid.clone(),
            nonce,
            tag: WEB_BOT_AUTH_TAG.to_string(),
        };
        // The `signature-agent;key="sig"` component value is the serialized
        // dictionary member value: an sf-string, quotes included.
        let agent_value = format!("\"{}\"", self.config.signature_agent.trim_end_matches('/'));
        let mut message = Message {
            method,
            authority,
            path,
            signature_agent_value: agent_value.clone(),
            generated: None,
        };
        signer
            .generate_signature_headers_content(
                &mut message,
                self.config.expires,
                Algorithm::Ed25519,
                &self.seed[..],
            )
            .map_err(|e| BotAuthError::Sign(e.to_string()))?;
        let (input, signature) = message
            .generated
            .expect("MessageSigner registers header contents on success");
        Ok(SignedHeaders {
            signature_agent: format!("{SIGNATURE_LABEL}={agent_value}"),
            signature_input: format!("{SIGNATURE_LABEL}={input}"),
            signature: format!("{SIGNATURE_LABEL}={signature}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vector key from draft-ietf-webbotauth-httpsig-protocol.
    const DRAFT_SEED: [u8; 32] = [
        0x9f, 0x83, 0x62, 0xf8, 0x7a, 0x48, 0x4a, 0x95, 0x4e, 0x6e, 0x74, 0x0c, 0x5b, 0x4c, 0x0e,
        0x84, 0x22, 0x91, 0x39, 0xa2, 0x0a, 0xa8, 0xab, 0x56, 0xff, 0x66, 0x58, 0x6f, 0x6a, 0x7d,
        0x29, 0xc5,
    ];

    #[test]
    fn thumbprint_matches_the_draft_test_vector() {
        let key = KeyPair::from_seed(DRAFT_SEED);
        assert_eq!(key.kid, "poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U");
        let reparsed = KeyPair::from_json(&key.to_json()).unwrap();
        assert_eq!(reparsed, key);
        assert!(key.directory_document().contains("\"keys\""));
        assert!(
            !key.directory_document().contains("\"d\""),
            "directory must not leak the private key"
        );
    }

    #[test]
    fn rejects_tampered_key_files() {
        let mut key = KeyPair::from_seed(DRAFT_SEED);
        key.x = KeyPair::from_seed([7u8; 32]).x;
        assert!(KeyPair::from_json(&key.to_json()).is_err());
    }

    #[test]
    fn signs_with_the_expected_header_shape() {
        let config = BotAuthConfig::new(KeyPair::from_seed(DRAFT_SEED), "https://agent.example");
        let signer = BotAuthSigner::new(&config).unwrap();
        let h = signer.sign("GET", "example.com", "/path").unwrap();
        assert_eq!(h.signature_agent, "sig=\"https://agent.example\"");
        assert!(
            h.signature_input.starts_with(
                "sig=(\"@method\" \"@authority\" \"@path\" \"signature-agent\";key=\"sig\")"
            ),
            "{}",
            h.signature_input
        );
        assert!(h.signature_input.contains(";tag=\"web-bot-auth\""));
        assert!(
            h.signature_input
                .contains(";keyid=\"poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U\"")
        );
        assert!(
            h.signature.starts_with("sig=:") && h.signature.ends_with(':'),
            "{}",
            h.signature
        );
    }

    #[test]
    fn host_patterns() {
        let mut config =
            BotAuthConfig::new(KeyPair::from_seed(DRAFT_SEED), "https://agent.example");
        config.sign_for = vec!["*.example.com".to_string(), "exact.test".to_string()];
        let signer = BotAuthSigner::new(&config).unwrap();
        assert!(signer.applies_to("a.example.com"));
        assert!(signer.applies_to("example.com"));
        assert!(signer.applies_to("EXACT.test"));
        assert!(!signer.applies_to("other.org"));
    }

    #[test]
    fn rejects_agent_urls_with_paths() {
        let config =
            BotAuthConfig::new(KeyPair::from_seed(DRAFT_SEED), "https://agent.example/dir");
        assert!(BotAuthSigner::new(&config).is_err());
        let config = BotAuthConfig::new(KeyPair::from_seed(DRAFT_SEED), "http://agent.example");
        assert!(BotAuthSigner::new(&config).is_err());
    }
}
