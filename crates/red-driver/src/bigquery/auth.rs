//! Getting a bearer token for BigQuery, and keeping it.
//!
//! BigQuery is the only engine RED talks to that has no password: every request
//! carries a short-lived OAuth2 access token. Three sources produce one, and they
//! divide sharply by whether they need crypto:
//!
//! - **Application Default Credentials** — the `gcloud auth application-default
//!   login` file holds a *refresh token*, exchanged for an access token over plain
//!   HTTPS. No signing.
//! - **The GCE / Cloud Run metadata server** — hands over an access token directly
//!   to anything running on the instance. No signing.
//! - **A service-account key file** — the only path that signs anything: an RS256
//!   JWT assertion, exchanged under the `jwt-bearer` grant. This is why `rsa` is a
//!   dependency, and it is touched by nothing else.
//!
//! Tokens live an hour, so [`TokenSource`] caches one and refreshes it slightly
//! early ([`EXPIRY_SKEW`]). Every other driver in the tree holds a connection or a
//! pool; this one holds a credential that expires, which is the one piece of state
//! no existing driver has.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use red_core::{RedError, Result};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use serde_json::Value as Json;
use tokio::sync::Mutex;

use crate::driver_err;

/// Google's OAuth2 token endpoint, the default `token_uri` when a key file omits
/// one.
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// The scope a BigQuery session needs. `cloud-platform` would also work and is
/// what most examples use; this is the narrower of the two that still covers
/// queries, jobs, and the catalog, and a narrower scope is the right default for a
/// tool that only ever reads and writes BigQuery.
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery";

/// The in-instance metadata endpoint. Only reachable on GCE / Cloud Run / GKE, and
/// its unreachability is exactly how [`Credentials::load`] tells "running on
/// Google infrastructure" from "developer laptop with no gcloud".
const METADATA_TOKEN_URL: &str = "http://metadata.google.internal/computeMetadata/v1/\
     instance/service-accounts/default/token";

/// How long a signed assertion claims to be valid. An hour is the maximum Google
/// accepts; the token it buys expires on the same schedule.
const ASSERTION_TTL: Duration = Duration::from_secs(3600);

/// Refresh this far before the token's stated expiry, so a request never leaves
/// with a credential that dies in flight. A minute comfortably covers clock skew
/// between RED's host and Google's, which is the failure this guards: an access
/// token rejected as expired reads as an auth error, and the retry would fetch a
/// token that is *also* "expired" until the clocks agree.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// Where a token comes from. Loaded once at connect and then immutable; the
/// mutable part (the cached token) lives in [`TokenSource`].
pub(super) enum Credentials {
    /// ADC user credentials: a refresh token issued to `gcloud` itself.
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
    /// A service-account key file. `private_key` is the PEM as it appears in the
    /// JSON, newlines and all.
    ServiceAccount {
        email: String,
        private_key: String,
        token_uri: String,
    },
    /// The instance metadata server; carries no secret of its own.
    Metadata,
}

impl Credentials {
    /// Resolve credentials the way Google's own client libraries do, in the same
    /// order, so a machine already set up for `bq` or `gcloud` needs nothing else:
    ///
    /// 1. an explicit `credentials_path` on the connection,
    /// 2. `$GOOGLE_APPLICATION_CREDENTIALS`,
    /// 3. the well-known ADC file `gcloud` writes,
    /// 4. the metadata server.
    ///
    /// Only the last is not verifiable here — its absence is a connection failure
    /// at first use rather than at load, because probing it would put a
    /// multi-second timeout on every connect of every developer who is not on GCE.
    pub(super) fn load(credentials_path: &str) -> Result<Credentials> {
        if !credentials_path.trim().is_empty() {
            return Self::from_file(credentials_path.trim());
        }
        if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
            && !path.trim().is_empty()
        {
            return Self::from_file(path.trim());
        }
        match adc_path() {
            Some(path) if std::path::Path::new(&path).is_file() => Self::from_file(&path),
            _ => Ok(Credentials::Metadata),
        }
    }

    /// Read and classify one credentials JSON file. Google distinguishes the two
    /// shapes by a `type` field, which is also the only way to tell them apart:
    /// both are flat JSON objects with a handful of string fields.
    fn from_file(path: &str) -> Result<Credentials> {
        let bytes = std::fs::read(path).map_err(|e| {
            RedError::Auth(format!("cannot read BigQuery credentials at {path}: {e}"))
        })?;
        let json: Json = serde_json::from_slice(&bytes).map_err(|e| {
            RedError::Auth(format!("BigQuery credentials at {path} are not JSON: {e}"))
        })?;
        let field = |k: &str| json.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        match field("type").as_str() {
            "service_account" => {
                let email = field("client_email");
                let private_key = field("private_key");
                if email.is_empty() || private_key.is_empty() {
                    return Err(RedError::Auth(format!(
                        "the service-account key at {path} is missing client_email or private_key"
                    )));
                }
                let token_uri = match field("token_uri") {
                    s if s.is_empty() => TOKEN_URI.to_string(),
                    s => s,
                };
                Ok(Credentials::ServiceAccount {
                    email,
                    private_key,
                    token_uri,
                })
            }
            "authorized_user" => {
                let (client_id, client_secret, refresh_token) = (
                    field("client_id"),
                    field("client_secret"),
                    field("refresh_token"),
                );
                if client_id.is_empty() || refresh_token.is_empty() {
                    return Err(RedError::Auth(format!(
                        "the credentials at {path} are missing client_id or refresh_token"
                    )));
                }
                Ok(Credentials::User {
                    client_id,
                    client_secret,
                    refresh_token,
                })
            }
            // Naming the types RED understands is more useful than "unsupported":
            // the likeliest cause is an external-account (workload identity
            // federation) file, and the message should say so rather than leave the
            // user comparing JSON.
            other => Err(RedError::Auth(format!(
                "unsupported BigQuery credential type {other:?} at {path}; RED reads \
                 service_account key files and authorized_user (ADC) credentials"
            ))),
        }
    }
}

/// The well-known Application Default Credentials file.
///
/// Hand-rolled rather than `dirs::config_dir()` on purpose: `gcloud` uses
/// `~/.config/gcloud` on **macOS** too, whereas the platform config dir there is
/// `~/Library/Application Support`. Asking the OS would look more correct and
/// would miss the file on every Mac.
fn adc_path() -> Option<String> {
    if cfg!(windows) {
        let base = std::env::var("APPDATA").ok()?;
        Some(format!(
            "{base}/gcloud/application_default_credentials.json"
        ))
    } else {
        let home = std::env::var("HOME").ok()?;
        Some(format!(
            "{home}/.config/gcloud/application_default_credentials.json"
        ))
    }
}

/// A cached access token and the credentials that mint it.
pub(super) struct TokenSource {
    creds: Credentials,
    client: reqwest::Client,
    /// The live token and the instant it stops being usable (already skewed by
    /// [`EXPIRY_SKEW`]). `Arc<str>` so handing it to a request header is a refcount
    /// bump rather than a copy of the (long) token on every call.
    cached: Mutex<Option<(Arc<str>, SystemTime)>>,
}

impl TokenSource {
    pub(super) fn new(creds: Credentials, client: reqwest::Client) -> Self {
        Self {
            creds,
            client,
            cached: Mutex::new(None),
        }
    }

    /// A source whose token is already in hand and effectively never expires.
    ///
    /// The seam the mock-server tests need: without it every test would have to
    /// reach `oauth2.googleapis.com` for a credential it is not testing.
    #[cfg(test)]
    pub(super) fn preloaded(token: &str, client: reqwest::Client) -> Self {
        Self {
            creds: Credentials::Metadata,
            client,
            cached: Mutex::new(Some((
                Arc::from(token),
                SystemTime::now() + Duration::from_secs(3600),
            ))),
        }
    }

    /// A usable access token, minted on first call and after expiry.
    ///
    /// The lock is held across the refresh so a burst of concurrent requests (the
    /// browse open probe fires three at once) performs **one** exchange rather than
    /// three. That serialises those requests behind a single HTTP round trip on a
    /// cold cache, which is the right trade: the alternative spends three refresh
    /// tokens' worth of round trips to save one wait.
    pub(super) async fn token(&self) -> Result<Arc<str>> {
        let mut cached = self.cached.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && SystemTime::now() < *expires
        {
            return Ok(Arc::clone(token));
        }
        let (token, ttl) = self.fetch().await?;
        let token: Arc<str> = Arc::from(token);
        *cached = Some((
            Arc::clone(&token),
            SystemTime::now() + ttl.saturating_sub(EXPIRY_SKEW),
        ));
        Ok(token)
    }

    /// One token exchange, per credential kind. Returns the token and its lifetime.
    async fn fetch(&self) -> Result<(String, Duration)> {
        let response = match &self.creds {
            Credentials::User {
                client_id,
                client_secret,
                refresh_token,
            } => {
                self.client
                    .post(TOKEN_URI)
                    .form(&[
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                        ("refresh_token", refresh_token.as_str()),
                        ("grant_type", "refresh_token"),
                    ])
                    .send()
                    .await
            }
            Credentials::ServiceAccount {
                email,
                private_key,
                token_uri,
            } => {
                let assertion = sign_assertion(email, private_key, token_uri)?;
                self.client
                    .post(token_uri)
                    .form(&[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                        ("assertion", assertion.as_str()),
                    ])
                    .send()
                    .await
            }
            Credentials::Metadata => {
                self.client
                    .get(METADATA_TOKEN_URL)
                    .header("Metadata-Flavor", "Google")
                    .send()
                    .await
            }
        };

        let response = response.map_err(|e| {
            // The metadata server is the one source whose failure has a specific,
            // actionable meaning: it is only reachable on Google infrastructure, so
            // being unable to reach it almost always means no credentials were
            // configured at all rather than that the network is down.
            if matches!(self.creds, Credentials::Metadata) {
                RedError::Auth(
                    "no BigQuery credentials found: set a service-account key file on the \
                     connection, or run `gcloud auth application-default login`"
                        .to_string(),
                )
            } else {
                RedError::Auth(format!("BigQuery token request failed: {e}"))
            }
        })?;

        let status = response.status();
        let body = response.text().await.map_err(driver_err)?;
        if !status.is_success() {
            // Google returns `{"error": "...", "error_description": "..."}`; the
            // description is the half worth showing ("Invalid grant: account not
            // found" rather than "invalid_grant").
            let detail = serde_json::from_str::<Json>(&body)
                .ok()
                .and_then(|j| {
                    j.get("error_description")
                        .or_else(|| j.get("error"))
                        .and_then(Json::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| body.trim().to_string());
            return Err(RedError::Auth(format!(
                "BigQuery rejected the credentials ({status}): {detail}"
            )));
        }
        let json: Json = serde_json::from_str(&body).map_err(driver_err)?;
        let token = json
            .get("access_token")
            .and_then(Json::as_str)
            .ok_or_else(|| {
                RedError::Auth("BigQuery token response carried no access_token".to_string())
            })?
            .to_string();
        // A missing `expires_in` is treated as the standard hour rather than as an
        // error: the token works, and refusing it over a missing hint would be a
        // hard failure caused by an optional field.
        let ttl = json
            .get("expires_in")
            .and_then(Json::as_u64)
            .map(Duration::from_secs)
            .unwrap_or(ASSERTION_TTL);
        Ok((token, ttl))
    }
}

/// Build and RS256-sign the JWT assertion a service account exchanges for a token.
///
/// The claims are the minimum Google accepts: who is asking (`iss`), what for
/// (`scope`), who the assertion is addressed to (`aud`, which must equal the
/// endpoint it is posted to), and its validity window.
fn sign_assertion(email: &str, private_key_pem: &str, token_uri: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(driver_err)?
        .as_secs();
    let exp = now + ASSERTION_TTL.as_secs();
    // Hand-built rather than via a `Serialize` struct: red-driver takes no direct
    // `serde` derive dependency (see the ClickHouse driver's `RawValue` note), and
    // these are two fixed objects.
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let claims = serde_json::json!({
        "iss": email,
        "scope": SCOPE,
        "aud": token_uri,
        "iat": now,
        "exp": exp,
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).map_err(driver_err)?),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).map_err(driver_err)?),
    );

    // Service-account keys are PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`). The
    // error deliberately does not echo the key material.
    let key = RsaPrivateKey::from_pkcs8_pem(private_key_pem).map_err(|e| {
        RedError::Auth(format!(
            "the service account's private_key is not a valid PKCS#8 RSA key: {e}"
        ))
    })?;
    let signature = SigningKey::<Sha256>::new(key).sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}
