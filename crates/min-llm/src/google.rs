//! Google Application Default Credentials (ADC) → OAuth access tokens, for
//! Vertex AI. Resolution follows the standard ADC order:
//!
//! 1. `GOOGLE_APPLICATION_CREDENTIALS` — a JSON key file
//! 2. the gcloud well-known file (`gcloud auth application-default login`):
//!    `$CLOUDSDK_CONFIG/application_default_credentials.json`, else
//!    `~/.config/gcloud/…` (`%APPDATA%\gcloud\…` on Windows)
//! 3. the GCE / GKE / Cloud Run metadata server
//!
//! Supported file types: `authorized_user` (refresh-token exchange) and
//! `service_account` (an RS256-signed JWT exchanged for a token). Tokens are
//! cached and refreshed five minutes before they expire.

use base64::Engine;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_TOKEN: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const METADATA_PROJECT: &str = "http://metadata.google.internal/computeMetadata/v1/project/project-id";

enum Source {
    User { client_id: String, client_secret: String, refresh_token: String },
    ServiceAccount { email: String, key_der: Vec<u8>, token_uri: String },
    Metadata,
}

/// An ADC token source with a cached access token.
pub struct Adc {
    source: Source,
    /// Where the credentials came from, for messages.
    pub origin: String,
    /// The project named by the credentials file, if any.
    pub project: Option<String>,
    /// `quota_project_id` from a user credential — sent as `x-goog-user-project`.
    pub quota_project: Option<String>,
    cached: RefCell<Option<(String, Instant)>>,
}

impl Adc {
    /// Locate credentials without touching the network. Fails only when a
    /// credentials file exists but can't be used.
    pub fn from_env() -> Result<Adc, String> {
        let explicit = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok().filter(|p| !p.is_empty());
        let path = match explicit {
            Some(p) => Some(p),
            None => well_known_file().filter(|p| std::path::Path::new(p).exists()),
        };
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(&p).map_err(|e| format!("reading ADC file {p}: {e}"))?;
                let v: Value = serde_json::from_str(&text).map_err(|e| format!("parsing ADC file {p}: {e}"))?;
                Adc::from_json(&v, p)
            }
            None => Ok(Adc::new(Source::Metadata, "the metadata server".into(), None, None)),
        }
    }

    fn new(source: Source, origin: String, project: Option<String>, quota_project: Option<String>) -> Adc {
        Adc { source, origin, project, quota_project, cached: RefCell::new(None) }
    }

    fn from_json(v: &Value, origin: String) -> Result<Adc, String> {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let opt = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(String::from);
        match v["type"].as_str().unwrap_or_default() {
            "authorized_user" => Ok(Adc::new(
                Source::User {
                    client_id: s("client_id"),
                    client_secret: s("client_secret"),
                    refresh_token: s("refresh_token"),
                },
                origin,
                None,
                opt("quota_project_id"),
            )),
            "service_account" => {
                let key_der = pem_to_der(&s("private_key"))
                    .ok_or_else(|| format!("{origin}: service account private_key is not a PEM key"))?;
                let token_uri = opt("token_uri").unwrap_or_else(|| TOKEN_URI.into());
                Ok(Adc::new(
                    Source::ServiceAccount { email: s("client_email"), key_der, token_uri },
                    origin,
                    opt("project_id"),
                    None,
                ))
            }
            other => Err(format!(
                "{origin}: ADC credential type {other:?} is not supported \
                 (use `gcloud auth application-default login` or a service-account key)"
            )),
        }
    }

    /// True when tokens come from the metadata server (no credentials file).
    pub fn on_metadata(&self) -> bool {
        matches!(self.source, Source::Metadata)
    }

    /// A valid access token, fetching a fresh one when the cache is stale.
    pub fn token(&self, http: &ureq::Agent) -> Result<String, String> {
        if let Some((tok, expiry)) = self.cached.borrow().as_ref() {
            if Instant::now() + Duration::from_secs(300) < *expiry {
                return Ok(tok.clone());
            }
        }
        let v = self.fetch(http).map_err(|e| format!("Google ADC ({}): {e}", self.origin))?;
        let tok = v["access_token"].as_str().ok_or("Google ADC: token response has no access_token")?;
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        *self.cached.borrow_mut() = Some((tok.to_string(), Instant::now() + Duration::from_secs(ttl)));
        Ok(tok.to_string())
    }

    fn fetch(&self, http: &ureq::Agent) -> Result<Value, String> {
        let resp = match &self.source {
            Source::User { client_id, client_secret, refresh_token } => http.post(TOKEN_URI).send_form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("refresh_token", refresh_token),
            ]),
            Source::ServiceAccount { email, key_der, token_uri } => {
                let assertion = signed_jwt(email, key_der, token_uri)?;
                http.post(token_uri).send_form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", &assertion),
                ])
            }
            Source::Metadata => metadata_agent().get(METADATA_TOKEN).set("Metadata-Flavor", "Google").call(),
        };
        match resp {
            Ok(r) => r.into_json().map_err(|e| e.to_string()),
            Err(ureq::Error::Status(code, r)) => {
                Err(format!("token request failed: HTTP {code}: {}", r.into_string().unwrap_or_default().trim()))
            }
            Err(e) if matches!(self.source, Source::Metadata) => Err(format!(
                "no credentials found and the metadata server is unreachable ({e}) — \
                 run `gcloud auth application-default login` or set GOOGLE_APPLICATION_CREDENTIALS"
            )),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The project from the metadata server (on GCP only; short timeout).
pub fn metadata_project() -> Option<String> {
    metadata_agent()
        .get(METADATA_PROJECT)
        .set("Metadata-Flavor", "Google")
        .call()
        .ok()?
        .into_string()
        .ok()
        .filter(|s| !s.is_empty())
}

/// The metadata server is plain HTTP on the link-local network: never proxied,
/// and quick to give up on when we're not on GCP.
fn metadata_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(3)).build()
}

fn well_known_file() -> Option<String> {
    let file = "application_default_credentials.json";
    if let Some(dir) = std::env::var("CLOUDSDK_CONFIG").ok().filter(|d| !d.is_empty()) {
        return Some(format!("{dir}/{file}"));
    }
    if cfg!(windows) {
        return std::env::var("APPDATA").ok().map(|d| format!("{d}\\gcloud\\{file}"));
    }
    std::env::var("HOME").ok().map(|h| format!("{h}/.config/gcloud/{file}"))
}

fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok().filter(|d| !d.is_empty())
}

/// A self-signed RS256 JWT asserting `email` for the cloud-platform scope.
fn signed_jwt(email: &str, key_der: &[u8], aud: &str) -> Result<String, String> {
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
    let header = json!({"alg": "RS256", "typ": "JWT"});
    let claims = json!({"iss": email, "scope": SCOPE, "aud": aud, "iat": now, "exp": now + 3600});
    let signing_input = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
    let key = ring::signature::RsaKeyPair::from_pkcs8(key_der)
        .map_err(|e| format!("service account private_key: {e}"))?;
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signing_input.as_bytes(), &mut sig)
        .map_err(|e| format!("signing JWT: {e}"))?;
    Ok(format!("{signing_input}.{}", b64(&sig)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorized_user_file_parses() {
        let v = json!({"type": "authorized_user", "client_id": "id", "client_secret": "s",
                       "refresh_token": "r", "quota_project_id": "my-proj"});
        let adc = Adc::from_json(&v, "test".into()).unwrap();
        assert!(matches!(adc.source, Source::User { .. }));
        assert_eq!(adc.quota_project.as_deref(), Some("my-proj"));
        assert_eq!(adc.project, None);
    }

    #[test]
    fn unsupported_type_is_an_error() {
        let v = json!({"type": "external_account"});
        assert!(Adc::from_json(&v, "test".into()).err().unwrap().contains("not supported"));
    }

    #[test]
    fn service_account_needs_a_pem_key() {
        let v = json!({"type": "service_account", "client_email": "a@b", "private_key": "", "project_id": "p"});
        assert!(Adc::from_json(&v, "test".into()).is_err());
    }

    #[test]
    fn pem_body_decodes() {
        let pem = "-----BEGIN PRIVATE KEY-----\naGVs\nbG8=\n-----END PRIVATE KEY-----\n";
        assert_eq!(pem_to_der(pem).unwrap(), b"hello");
    }
}
