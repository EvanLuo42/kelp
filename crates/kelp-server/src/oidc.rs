//! Single sign-on with an OpenID Connect provider, using the device
//! authorization grant (RFC 8628) so that command-line clients need no
//! local web server.
//!
//! The server talks to the provider itself: it starts the flow, polls the
//! token endpoint on the client's behalf, and reads the user from the ID
//! token. Because the ID token comes straight from the token endpoint over
//! TLS, its issuer is authenticated by TLS rather than by the token's
//! signature, as OpenID Connect Core 3.1.3.7 allows; the issuer, audience
//! and expiry are still checked.

use std::path::Path;

use base64::Engine;
use kelp_proto::rpc::DeviceCode;
use kelp_proto::{ErrorKind, ErrorReply};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::OnceCell;

use crate::{Error, Result};

/// The `[oidc]` section of `server.toml`.
///
/// ```toml
/// [oidc]
/// issuer = "https://login.studio.example"
/// client_id = "kelp"
/// groups_claim = "groups"
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct OidcConfig {
    /// The provider's issuer URL; its discovery document is at
    /// `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    /// The client secret, for providers that require one.
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: String,
    /// The claim that holds the Kelp user name. Characters a user name
    /// cannot have become `_`.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
    /// The claim that a first login is matched against the single sign-on
    /// names administrators expect (`kelp user link <user> --sso <name>`):
    /// `None` for [`username_claim`](Self::username_claim), `"sub"` for the
    /// subject, or any other claim. An `"email"` claim only counts when
    /// the provider marks it verified.
    ///
    /// User names are often changeable by users or reassigned, so whoever
    /// gets a name an administrator expects takes over that Kelp user;
    /// a claim the provider controls is safer.
    #[serde(default)]
    pub expect_claim: Option<String>,
    /// A claim listing group names; users are added to the Kelp groups of
    /// the same names when they log in.
    #[serde(default)]
    pub groups_claim: Option<String>,
    /// Whether the first login creates the user.
    #[serde(default = "yes")]
    pub auto_create: bool,
    /// Whether users other than administrators must use single sign-on;
    /// their passwords stop working.
    #[serde(default)]
    pub require: bool,
    /// Groups whose membership single sign-on manages completely: a user
    /// is removed from these when the groups claim no longer lists them.
    /// Users are only ever added to other groups.
    #[serde(default)]
    pub managed_groups: Vec<String>,
}

fn default_scopes() -> String {
    "openid profile email".into()
}

fn default_username_claim() -> String {
    "preferred_username".into()
}

fn yes() -> bool {
    true
}

impl OidcConfig {
    /// Reads the `[oidc]` section of `<data_dir>/server.toml`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        #[derive(Deserialize)]
        struct ServerToml {
            oidc: Option<OidcConfig>,
        }
        match std::fs::read_to_string(data_dir.join("server.toml")) {
            Ok(text) => toml::from_str::<ServerToml>(&text)
                .map(|t| t.oidc)
                .map_err(|e| Error::Config(format!("server.toml: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
}

/// The identity a provider vouched for.
pub(crate) struct Identity {
    /// The user name, from [`OidcConfig::username_claim`].
    pub user: String,
    pub issuer: String,
    /// The provider's stable identifier for the user.
    pub subject: String,
    /// The name to look up among the names administrators expect, from
    /// [`OidcConfig::expect_claim`], if the token has one that counts.
    pub expect_name: Option<String>,
    pub groups: Vec<String>,
}

pub(crate) struct Oidc {
    pub config: OidcConfig,
    client: reqwest::Client,
    discovery: OnceCell<Discovery>,
}

impl Oidc {
    pub(crate) fn new(config: OidcConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            discovery: OnceCell::new(),
        }
    }

    async fn discovery(&self) -> Result<&Discovery, ErrorReply> {
        self.discovery
            .get_or_try_init(|| async {
                let url = format!(
                    "{}/.well-known/openid-configuration",
                    self.config.issuer.trim_end_matches('/')
                );
                let response = self.client.get(&url).send().await.map_err(unavailable)?;
                let body = response.bytes().await.map_err(unavailable)?;
                serde_json::from_slice::<Discovery>(&body).map_err(unavailable)
            })
            .await
    }

    /// Starts a device authorization.
    pub(crate) async fn start(&self) -> Result<DeviceCode, ErrorReply> {
        let discovery = self.discovery().await?;
        let mut form = vec![("client_id", self.config.client_id.as_str())];
        form.push(("scope", &self.config.scopes));
        let (status, body) = self
            .post(&discovery.device_authorization_endpoint, &form)
            .await?;
        if !status.is_success() {
            return Err(unavailable(format!("the provider answered {status}")));
        }
        let field = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_owned);
        let number =
            |name: &str, default: u64| body.get(name).and_then(Value::as_u64).unwrap_or(default);
        Ok(DeviceCode {
            device_code: field("device_code").ok_or_else(|| unavailable("no device_code"))?,
            user_code: field("user_code").ok_or_else(|| unavailable("no user_code"))?,
            verification_uri: field("verification_uri")
                .or_else(|| field("verification_url"))
                .ok_or_else(|| unavailable("no verification_uri"))?,
            verification_uri_complete: field("verification_uri_complete"),
            interval: number("interval", 5) as u32,
            expires_in: number("expires_in", 600) as u32,
        })
    }

    /// Polls the token endpoint for `device_code`. Returns `None` while the
    /// user has not approved the login yet.
    pub(crate) async fn poll(
        &self,
        device_code: &str,
        now: u64,
    ) -> Result<Option<Identity>, ErrorReply> {
        let discovery = self.discovery().await?;
        let mut form = vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", self.config.client_id.as_str()),
        ];
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret", secret));
        }
        let (status, body) = self.post(&discovery.token_endpoint, &form).await?;
        if !status.is_success() {
            let error = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return match error {
                "authorization_pending" | "slow_down" => Ok(None),
                other => Err(ErrorReply::new(
                    ErrorKind::Unauthenticated,
                    format!("single sign-on failed: {other}"),
                )),
            };
        }
        let token = body
            .get("id_token")
            .and_then(Value::as_str)
            .ok_or_else(|| unavailable("the provider sent no ID token"))?;
        let claims = decode_claims(token)?;
        self.check(&claims, &discovery.issuer, now)?;
        let user = claims
            .get(&self.config.username_claim)
            .and_then(Value::as_str)
            .map(user_name)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                ErrorReply::new(
                    ErrorKind::Unauthenticated,
                    format!("the ID token has no {} claim", self.config.username_claim),
                )
            })?;
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ErrorReply::new(ErrorKind::Unauthenticated, "the ID token has no sub claim")
            })?
            .to_owned();
        let groups = self
            .config
            .groups_claim
            .as_ref()
            .and_then(|c| claims.get(c))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let expect_name = match self.config.expect_claim.as_deref() {
            None => Some(user.clone()),
            Some("sub") => Some(subject.clone()),
            Some(claim) => {
                let verified = claims.get("email_verified").and_then(Value::as_bool) == Some(true);
                claims
                    .get(claim)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty() && (claim != "email" || verified))
                    .map(str::to_owned)
            }
        };
        Ok(Some(Identity {
            user,
            issuer: discovery.issuer.clone(),
            subject,
            expect_name,
            groups,
        }))
    }

    fn check(&self, claims: &Map<String, Value>, issuer: &str, now: u64) -> Result<(), ErrorReply> {
        let reject = |why: &str| {
            ErrorReply::new(
                ErrorKind::Unauthenticated,
                format!("rejected ID token: {why}"),
            )
        };
        if claims.get("iss").and_then(Value::as_str) != Some(issuer) {
            return Err(reject("wrong issuer"));
        }
        let client = Value::String(self.config.client_id.clone());
        let audience_ok = match claims.get("aud") {
            Some(Value::String(a)) => *a == self.config.client_id,
            Some(Value::Array(a)) => a.contains(&client),
            _ => false,
        };
        if !audience_ok {
            return Err(reject("wrong audience"));
        }
        let expires = claims.get("exp").and_then(Value::as_u64).unwrap_or(0);
        if expires.saturating_mul(1000) <= now {
            return Err(reject("expired"));
        }
        Ok(())
    }

    async fn post(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<(reqwest::StatusCode, Map<String, Value>), ErrorReply> {
        let body: Vec<String> = form
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect();
        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body.join("&"))
            .send()
            .await
            .map_err(unavailable)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(unavailable)?;
        let json = serde_json::from_slice::<Map<String, Value>>(&bytes).unwrap_or_default();
        Ok((status, json))
    }
}

/// Reads the claims of a JWT without checking its signature.
fn decode_claims(token: &str) -> Result<Map<String, Value>, ErrorReply> {
    let bad = || ErrorReply::new(ErrorKind::Unauthenticated, "malformed ID token");
    let payload = token.split('.').nth(1).ok_or_else(bad)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| bad())?;
    serde_json::from_slice(&bytes).map_err(|_| bad())
}

/// Turns a claim into a valid user name.
fn user_name(claim: &str) -> String {
    claim
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                true => c,
                false => '_',
            },
        )
        .collect::<String>()
        .trim_start_matches('.')
        .chars()
        .take(64)
        .collect()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn unavailable(e: impl std::fmt::Display) -> ErrorReply {
    ErrorReply::new(
        ErrorKind::Unavailable,
        format!("cannot reach the identity provider: {e}"),
    )
}
