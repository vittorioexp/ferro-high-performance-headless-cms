//! Single sign-on: OpenID Connect (authorization code flow with PKCE, ID token verified against the provider's keys) and
//! plain OAuth 2.0 (profile fetched from the provider).
//!
//! The browser flow is:
//! 1. `begin`: Ferro redirects to the provider and sets a short-lived, signed, `HttpOnly` cookie that holds the `state`,
//!    the `nonce`, the PKCE verifier and where to send the user afterwards.
//! 2. The provider redirects back with `code` and `state`; `finish` checks the cookie, exchanges the code, verifies the
//!    ID token (signature, issuer, audience, expiry, nonce) and returns the profile.
//! 3. The server resolves the Ferro user and hands the browser a one-time code (never a token in a URL).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use ferro_engine::auth::{random_token, ExternalPolicy, ExternalProfile};
use ferro_engine::{FerroError, Result};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use url::Url;

use crate::config::{endpoint_scheme_ok, IdentityConfig, OAuthDriver, ProviderConfig};

/// Name of the cookie that carries the flow state between the redirect and the callback.
pub const STATE_COOKIE: &str = "ferro_oauth";
const STATE_TTL_SECS: i64 = 600;
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
const JWKS_TTL: Duration = Duration::from_secs(3600);
const JWKS_MIN_REFETCH: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct StateClaims {
    iss: String,
    exp: i64,
    provider: String,
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
}

const STATE_ISSUER: &str = "ferro-oauth";

pub fn pkce_challenge(verifier: &str) -> String {
    B64URL.encode(Sha256::digest(verifier.as_bytes()))
}

/// What the sign-in page needs to draw a button.
#[derive(Debug, Clone, Serialize)]
pub struct PublicProvider {
    pub name: String,
    pub label: String,
    pub icon: Option<String>,
}

struct Endpoints {
    issuer: Option<String>,
    authorize: Url,
    token: Url,
    jwks: Option<Url>,
    userinfo: Option<Url>,
}

struct Jwks {
    fetched: Instant,
    set: JwkSet,
}

struct Provider {
    cfg: ProviderConfig,
    endpoints: RwLock<Option<(Instant, Arc<Endpoints>)>>,
    jwks: RwLock<Option<Jwks>>,
}

pub struct OAuth {
    providers: HashMap<String, Provider>,
    http: reqwest::Client,
    state_key: String,
    callback_base: Option<String>,
    secure_cookie: bool,
}

/// Where to send the browser, and the state cookie to set.
pub struct Begin {
    pub location: String,
    pub cookie_value: String,
    pub cookie_max_age: i64,
}

#[derive(Debug)]
pub struct Finished {
    pub profile: ExternalProfile,
    pub policy: ExternalPolicy,
    pub redirect: String,
}

/// A sign-in that did not complete. `redirect` is only known when the state cookie was valid; without it the caller
/// can only answer with a plain error page (there is nowhere safe to send the user).
#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub redirect: Option<String>,
}

fn fail(code: &'static str, redirect: Option<&str>, detail: impl std::fmt::Display) -> Failure {
    tracing::warn!(code, "single sign-on failed: {detail}");
    Failure { code, redirect: redirect.map(String::from) }
}

impl OAuth {
    pub fn new(cfg: &IdentityConfig, state_key: &str) -> Result<OAuth> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("ferro-cms")
            .build()
            .map_err(|e| FerroError::Internal(format!("cannot build the HTTP client: {e}")))?;
        let providers = cfg
            .providers
            .iter()
            .map(|p| (p.name.clone(), Provider { cfg: p.clone(), endpoints: RwLock::new(None), jwks: RwLock::new(None) }))
            .collect();
        Ok(OAuth {
            providers,
            http,
            state_key: state_key.to_string(),
            callback_base: cfg.public_url.as_ref().map(|u| u.as_str().trim_end_matches('/').to_string()),
            secure_cookie: cfg.public_url.as_ref().map(|u| u.scheme() == "https").unwrap_or(false),
        })
    }

    pub fn secure_cookie(&self) -> bool {
        self.secure_cookie
    }

    pub fn list(&self) -> Vec<PublicProvider> {
        let mut v: Vec<PublicProvider> = self.providers.values().map(|p| PublicProvider { name: p.cfg.name.clone(), label: p.cfg.label.clone(), icon: p.cfg.icon.clone() }).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    fn redirect_uri(&self, name: &str) -> Result<String> {
        let base = self.callback_base.as_ref().ok_or_else(|| FerroError::Internal("FERRO_PUBLIC_URL is not set".into()))?;
        Ok(format!("{base}/auth/oauth/{name}/callback"))
    }

    async fn get_json(&self, url: &Url, bearer: Option<&str>) -> Result<Value> {
        let mut req = self.http.get(url.clone()).header("Accept", "application/json");
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| FerroError::Internal(format!("GET {url}: {e}")))?;
        read_json(resp, url).await
    }

    async fn endpoints(&self, p: &Provider) -> Result<Arc<Endpoints>> {
        if let Some((at, e)) = p.endpoints.read().await.as_ref() {
            if at.elapsed() < DISCOVERY_TTL {
                return Ok(e.clone());
            }
        }
        let c = &p.cfg;
        let mut ep = Endpoints { issuer: None, authorize: c.authorize_url.clone().unwrap_or_else(|| "http://unset.invalid/".parse().expect("static url")), token: c.token_url.clone().unwrap_or_else(|| "http://unset.invalid/".parse().expect("static url")), jwks: c.jwks_url.clone(), userinfo: c.profile_url.clone() };
        if let Some(issuer) = &c.issuer_url {
            let doc_url = Url::parse(&format!("{}/.well-known/openid-configuration", issuer.as_str().trim_end_matches('/'))).map_err(|e| FerroError::Internal(e.to_string()))?;
            let doc = self.get_json(&doc_url, None).await?;
            let get = |k: &str| -> Result<Option<Url>> {
                match doc.get(k).and_then(|v| v.as_str()) {
                    None => Ok(None),
                    Some(s) => {
                        let u = Url::parse(s).map_err(|_| FerroError::Internal(format!("discovery `{k}` is not a URL")))?;
                        if !endpoint_scheme_ok(&u) {
                            return Err(FerroError::Internal(format!("discovery `{k}` is not https")));
                        }
                        Ok(Some(u))
                    }
                }
            };
            // the document must really be the issuer's (OpenID Discovery §4.3): a mismatch means a wrong or hostile document
            let claimed = doc.get("issuer").and_then(|v| v.as_str()).unwrap_or_default();
            if claimed.trim_end_matches('/') != issuer.as_str().trim_end_matches('/') {
                return Err(FerroError::Internal(format!("discovery issuer `{claimed}` does not match the configured `{issuer}`")));
            }
            ep.issuer = Some(claimed.to_string());
            if c.authorize_url.is_none() {
                ep.authorize = get("authorization_endpoint")?.ok_or_else(|| FerroError::Internal("discovery has no authorization_endpoint".into()))?;
            }
            if c.token_url.is_none() {
                ep.token = get("token_endpoint")?.ok_or_else(|| FerroError::Internal("discovery has no token_endpoint".into()))?;
            }
            if ep.jwks.is_none() {
                ep.jwks = get("jwks_uri")?;
            }
            if ep.userinfo.is_none() {
                ep.userinfo = get("userinfo_endpoint")?;
            }
        }
        if c.driver == OAuthDriver::OpenId && ep.jwks.is_none() {
            return Err(FerroError::Internal("no JWKS address: set JWKS_URL or use an issuer with discovery".into()));
        }
        let ep = Arc::new(ep);
        *p.endpoints.write().await = Some((Instant::now(), ep.clone()));
        Ok(ep)
    }

    fn sign_state(&self, claims: &StateClaims) -> Result<String> {
        jsonwebtoken::encode(&Header::default(), claims, &EncodingKey::from_secret(self.state_key.as_bytes())).map_err(|e| FerroError::Internal(format!("cannot sign the state: {e}")))
    }

    fn read_state(&self, cookie: &str) -> Option<StateClaims> {
        let mut v = Validation::new(Algorithm::HS256);
        v.set_issuer(&[STATE_ISSUER]);
        v.leeway = 5;
        v.set_required_spec_claims(&["exp", "iss"]);
        jsonwebtoken::decode::<StateClaims>(cookie, &DecodingKey::from_secret(self.state_key.as_bytes()), &v).ok().map(|d| d.claims)
    }

    /// Step 1: where to send the browser, and the cookie that binds the flow to it. `redirect` must already have been
    /// checked against the allowed origins.
    pub async fn begin(&self, name: &str, redirect: &str) -> Result<Begin> {
        let p = self.providers.get(name).ok_or_else(|| FerroError::not_found("unknown sign-in provider"))?;
        let ep = self.endpoints(p).await?;
        let state = random_token();
        let nonce = random_token();
        let verifier = B64URL.encode(hex::decode(random_token()).unwrap_or_default());
        let mut url = ep.authorize.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &p.cfg.client_id)
                .append_pair("redirect_uri", &self.redirect_uri(name)?)
                .append_pair("scope", &p.cfg.scope)
                .append_pair("state", &state)
                .append_pair("code_challenge", &pkce_challenge(&verifier))
                .append_pair("code_challenge_method", "S256");
            if p.cfg.driver == OAuthDriver::OpenId {
                q.append_pair("nonce", &nonce);
            }
        }
        let claims = StateClaims { iss: STATE_ISSUER.into(), exp: chrono_now() + STATE_TTL_SECS, provider: name.into(), state, nonce, verifier, redirect: redirect.into() };
        Ok(Begin { location: url.to_string(), cookie_value: self.sign_state(&claims)?, cookie_max_age: STATE_TTL_SECS })
    }

    /// The page the flow wanted to return to, if the state cookie is genuine (used to report a refusal by the provider).
    pub fn redirect_of(&self, cookie: Option<&str>) -> Option<String> {
        cookie.and_then(|c| self.read_state(c)).map(|c| c.redirect)
    }

    /// Step 2: the provider sent the user back. Everything is checked here; the caller only maps the result.
    pub async fn finish(&self, name: &str, code: &str, state: &str, cookie: Option<&str>) -> std::result::Result<Finished, Failure> {
        let p = self.providers.get(name).ok_or_else(|| fail("provider", None, format!("unknown provider {name}")))?;
        let claims = cookie.and_then(|c| self.read_state(c)).ok_or_else(|| fail("state", None, "missing, expired or forged state cookie"))?;
        let target = Some(claims.redirect.as_str());
        if claims.provider != name || !constant_time_eq(claims.state.as_bytes(), state.as_bytes()) {
            return Err(fail("state", target, "state does not match the cookie (CSRF or a stale tab)"));
        }
        if code.is_empty() || code.len() > 4096 {
            return Err(fail("provider", target, "no authorization code"));
        }
        let internal = |e: FerroError| fail("provider", target, e);
        let ep = self.endpoints(p).await.map_err(internal)?;

        // exchange the code
        let redirect_uri = self.redirect_uri(name).map_err(internal)?;
        let resp = self
            .http
            .post(ep.token.clone())
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", p.cfg.client_id.as_str()),
                ("client_secret", p.cfg.client_secret.as_str()),
                ("code_verifier", claims.verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|e| fail("provider", target, format!("token request: {e}")))?;
        let token = read_json(resp, &ep.token).await.map_err(internal)?;
        let access_token = token.get("access_token").and_then(|v| v.as_str()).map(String::from);

        // the identity
        let mut profile_claims = match p.cfg.driver {
            OAuthDriver::OpenId => {
                let id_token = token.get("id_token").and_then(|v| v.as_str()).ok_or_else(|| fail("token", target, "the provider returned no id_token"))?;
                self.verify_id_token(p, &ep, id_token, &claims.nonce).await.map_err(|e| fail("token", target, e))?
            }
            OAuthDriver::OAuth2 => Value::Object(Default::default()),
        };
        let has_email = lookup(&profile_claims, &p.cfg.email_key).is_some();
        if p.cfg.driver == OAuthDriver::OAuth2 || !has_email {
            let (Some(url), Some(at)) = (ep.userinfo.as_ref(), access_token.as_deref()) else {
                return Err(fail("email", target, "the provider gave no email and has no profile endpoint"));
            };
            let info = self.get_json(url, Some(at)).await.map_err(internal)?;
            if p.cfg.driver == OAuthDriver::OpenId {
                // OpenID Connect Core §5.3.2: the profile must belong to the same subject as the ID token
                let (a, b) = (profile_claims.get("sub"), info.get("sub"));
                if a.is_none() || a != b {
                    return Err(fail("token", target, "userinfo subject does not match the ID token"));
                }
                for (k, v) in info.as_object().into_iter().flatten() {
                    profile_claims.as_object_mut().map(|m| m.entry(k.clone()).or_insert(v.clone()));
                }
            } else {
                profile_claims = info;
            }
        }
        let profile = extract_profile(&p.cfg, &profile_claims).map_err(|(code, why)| fail(code, target, why))?;
        Ok(Finished {
            profile,
            policy: ExternalPolicy { default_role: p.cfg.default_role, allow_registration: p.cfg.allow_public_registration, link_existing: p.cfg.link_existing_users },
            redirect: claims.redirect.clone(),
        })
    }

    async fn jwk_set(&self, p: &Provider, url: &Url, force: bool) -> Result<JwkSet> {
        if let Some(j) = p.jwks.read().await.as_ref() {
            let age = j.fetched.elapsed();
            if (!force && age < JWKS_TTL) || (force && age < JWKS_MIN_REFETCH) {
                return Ok(j.set.clone());
            }
        }
        let doc = self.get_json(url, None).await?;
        let set: JwkSet = serde_json::from_value(doc).map_err(|e| FerroError::Internal(format!("invalid JWKS: {e}")))?;
        *p.jwks.write().await = Some(Jwks { fetched: Instant::now(), set: set.clone() });
        Ok(set)
    }

    async fn verify_id_token(&self, p: &Provider, ep: &Endpoints, id_token: &str, nonce: &str) -> Result<Value> {
        let bad = |why: &str| FerroError::Unauthorized(format!("invalid ID token: {why}"));
        let header = jsonwebtoken::decode_header(id_token).map_err(|_| bad("unreadable"))?;
        // Only asymmetric signatures: `none` and HMAC (key confusion with the public key) are never acceptable.
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 | Algorithm::PS256 | Algorithm::PS384 | Algorithm::PS512 | Algorithm::ES256 | Algorithm::ES384) {
            return Err(bad("signature algorithm not allowed"));
        }
        let jwks_url = ep.jwks.as_ref().ok_or_else(|| FerroError::Internal("no JWKS address".into()))?;
        let mut set = self.jwk_set(p, jwks_url, false).await?;
        let find = |set: &JwkSet| match &header.kid {
            Some(kid) => set.find(kid).cloned(),
            None if set.keys.len() == 1 => set.keys.first().cloned(),
            None => None,
        };
        let jwk = match find(&set) {
            Some(j) => j,
            None => {
                // the provider rotated its keys: look again (at most every 5 s)
                set = self.jwk_set(p, jwks_url, true).await?;
                find(&set).ok_or_else(|| bad("signed with an unknown key"))?
            }
        };
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| bad("unusable signing key"))?;
        let mut v = Validation::new(header.alg);
        v.set_audience(&[p.cfg.client_id.as_str()]);
        match ep.issuer.as_deref().or(p.cfg.issuer_url.as_ref().map(|u| u.as_str())) {
            Some(iss) => v.set_issuer(&[iss.trim_end_matches('/'), &format!("{}/", iss.trim_end_matches('/'))]),
            None => return Err(FerroError::Internal("no issuer to validate the ID token against".into())),
        }
        v.leeway = 30;
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = jsonwebtoken::decode::<Value>(id_token, &key, &v).map_err(|e| bad(&e.to_string()))?.claims;
        if claims.get("nonce").and_then(|n| n.as_str()).map(|n| constant_time_eq(n.as_bytes(), nonce.as_bytes())) != Some(true) {
            return Err(bad("nonce does not match (replayed token)"));
        }
        if let Some(azp) = claims.get("azp").and_then(|v| v.as_str()) {
            if azp != p.cfg.client_id {
                return Err(bad("issued to another client"));
            }
        }
        Ok(claims)
    }
}

async fn read_json(resp: reqwest::Response, url: &Url) -> Result<Value> {
    let status = resp.status();
    if resp.content_length().map(|n| n as usize > MAX_RESPONSE_BYTES).unwrap_or(false) {
        return Err(FerroError::Internal(format!("{url}: response too large")));
    }
    let bytes = resp.bytes().await.map_err(|e| FerroError::Internal(format!("{url}: {e}")))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(FerroError::Internal(format!("{url}: response too large")));
    }
    if !status.is_success() {
        return Err(FerroError::Internal(format!("{url}: HTTP {status}: {}", String::from_utf8_lossy(&bytes[..bytes.len().min(200)]))));
    }
    serde_json::from_slice(&bytes).map_err(|e| FerroError::Internal(format!("{url}: not JSON ({e})")))
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `a.b.c` into nested objects; scalars only.
fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    if cur.is_null() { None } else { Some(cur) }
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Turn the provider's claims into a Ferro profile, enforcing the provider's rules. On refusal: the short error code
/// shown to the user, and the reason for the log.
fn extract_profile(cfg: &ProviderConfig, claims: &Value) -> std::result::Result<ExternalProfile, (&'static str, String)> {
    let email = lookup(claims, &cfg.email_key).and_then(as_text).map(|e| e.to_ascii_lowercase()).ok_or(("email", format!("no `{}` claim", cfg.email_key)))?;
    let identifier = lookup(claims, &cfg.identifier_key).and_then(as_text).ok_or(("provider", format!("no `{}` claim", cfg.identifier_key)))?;
    if cfg.require_verified_email && !truthy(claims.get("email_verified")) {
        return Err(("email", format!("the provider does not vouch for {email} (email_verified)")));
    }
    let (_, domain) = email.rsplit_once('@').ok_or(("email", format!("`{email}` is not an address")))?;
    if !cfg.allowed_email_domains.is_empty() && !cfg.allowed_email_domains.iter().any(|d| d == domain) {
        return Err(("domain", format!("{email} is outside the allowed domains")));
    }
    Ok(ExternalProfile {
        provider: cfg.name.clone(),
        identifier,
        email,
        first_name: lookup(claims, &cfg.first_name_key).and_then(as_text),
        last_name: lookup(claims, &cfg.last_name_key).and_then(as_text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(f: impl FnOnce(&mut ProviderConfig)) -> ProviderConfig {
        let mut c = ProviderConfig {
            name: "mock".into(),
            label: "Mock".into(),
            icon: None,
            driver: OAuthDriver::OpenId,
            client_id: "cid".into(),
            client_secret: "sec".into(),
            issuer_url: None,
            authorize_url: None,
            token_url: None,
            profile_url: None,
            jwks_url: None,
            scope: "openid".into(),
            email_key: "email".into(),
            identifier_key: "sub".into(),
            first_name_key: "given_name".into(),
            last_name_key: "family_name".into(),
            require_verified_email: true,
            allowed_email_domains: vec![],
            allow_public_registration: false,
            default_role: None,
            link_existing_users: false,
        };
        f(&mut c);
        c
    }

    fn oauth() -> OAuth {
        let id = IdentityConfig { public_url: Some("https://cms.example.com".parse().unwrap()), ..Default::default() };
        OAuth::new(&id, &"k".repeat(32)).unwrap()
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn state_cookie_is_signed_and_expires() {
        let o = oauth();
        let claims = StateClaims { iss: STATE_ISSUER.into(), exp: chrono_now() + 60, provider: "mock".into(), state: "s".into(), nonce: "n".into(), verifier: "v".into(), redirect: "https://cms.example.com/admin/".into() };
        let cookie = o.sign_state(&claims).unwrap();
        let back = o.read_state(&cookie).unwrap();
        assert_eq!((back.state.as_str(), back.redirect.as_str()), ("s", "https://cms.example.com/admin/"));
        // another server's key, a tampered payload, expiry and a different issuer (e.g. an access token) are all refused
        let other = OAuth::new(&IdentityConfig::default(), &"z".repeat(32)).unwrap();
        assert!(other.read_state(&cookie).is_none());
        let mut parts: Vec<&str> = cookie.split('.').collect();
        let forged = B64URL.encode(json!({"iss": STATE_ISSUER, "exp": chrono_now() + 60, "provider": "mock", "state": "s", "nonce": "n", "verifier": "v", "redirect": "https://evil.net/"}).to_string());
        parts[1] = &forged;
        assert!(o.read_state(&parts.join(".")).is_none());
        let expired = o.sign_state(&StateClaims { exp: chrono_now() - 3600, ..claims }).unwrap();
        assert!(o.read_state(&expired).is_none());
        let access_like = jsonwebtoken::encode(&Header::default(), &json!({"sub": "u", "exp": chrono_now() + 60, "iss": "ferro"}), &EncodingKey::from_secret("k".repeat(32).as_bytes())).unwrap();
        assert!(o.read_state(&access_like).is_none(), "an access token is not a state cookie");
        assert!(o.read_state("garbage").is_none() && o.read_state("").is_none());
    }

    #[test]
    fn profile_extraction_and_rules() {
        let c = cfg(|_| {});
        let p = extract_profile(&c, &json!({"sub": 42, "email": " Ann@Example.com ", "email_verified": true, "given_name": "Ann", "family_name": ""})).unwrap();
        assert_eq!((p.identifier.as_str(), p.email.as_str(), p.first_name.as_deref(), p.last_name.as_deref()), ("42", "ann@example.com", Some("Ann"), None));
        for v in [json!("true"), json!("TRUE")] {
            assert!(extract_profile(&c, &json!({"sub": "1", "email": "a@b.co", "email_verified": v})).is_ok(), "string booleans (some providers)");
        }
        for claims in [json!({"sub": "1", "email": "a@b.co"}), json!({"sub": "1", "email": "a@b.co", "email_verified": false}), json!({"sub": "1", "email": "a@b.co", "email_verified": "no"})] {
            assert_eq!(extract_profile(&c, &claims).unwrap_err().0, "email", "{claims}");
        }
        assert!(extract_profile(&cfg(|c| c.require_verified_email = false), &json!({"sub": "1", "email": "a@b.co"})).is_ok());
        assert_eq!(extract_profile(&c, &json!({"email": "a@b.co", "email_verified": true})).unwrap_err().0, "provider", "no subject");
        assert_eq!(extract_profile(&c, &json!({"sub": "1", "email_verified": true})).unwrap_err().0, "email");
        assert_eq!(extract_profile(&c, &json!({"sub": "1", "email": "not-an-address", "email_verified": true})).unwrap_err().0, "email");
        let strict = cfg(|c| c.allowed_email_domains = vec!["example.com".into()]);
        assert!(extract_profile(&strict, &json!({"sub": "1", "email": "a@example.com", "email_verified": true})).is_ok());
        for e in ["a@evil.net", "a@example.com.evil.net", "a@sub.example.com", "a@notexample.com", "example.com@evil.net"] {
            assert_eq!(extract_profile(&strict, &json!({"sub": "1", "email": e, "email_verified": true})).unwrap_err().0, "domain", "{e}");
        }
        // nested keys (GitHub-style `user.email`) and numeric identifiers
        let nested = cfg(|c| {
            c.email_key = "data.mail".into();
            c.identifier_key = "data.id".into();
            c.require_verified_email = false;
        });
        let p = extract_profile(&nested, &json!({"data": {"mail": "x@y.zz", "id": 7}})).unwrap();
        assert_eq!((p.email.as_str(), p.identifier.as_str()), ("x@y.zz", "7"));
        assert!(extract_profile(&nested, &json!({"data": null})).is_err());
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"abc", b"abc") && !constant_time_eq(b"abc", b"abd") && !constant_time_eq(b"abc", b"ab") && constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn finish_rejects_a_missing_or_foreign_state_before_any_network_call() {
        let id = IdentityConfig { public_url: Some("https://cms.example.com".parse().unwrap()), providers: vec![cfg(|c| c.issuer_url = Some("https://idp.invalid".parse().unwrap()))], ..Default::default() };
        let o = OAuth::new(&id, &"k".repeat(32)).unwrap();
        assert_eq!(o.finish("mock", "code", "state", None).await.unwrap_err().code, "state");
        assert_eq!(o.finish("mock", "code", "state", Some("garbage")).await.unwrap_err().code, "state");
        assert_eq!(o.finish("nope", "code", "state", None).await.unwrap_err().code, "provider");
        let cookie = o.sign_state(&StateClaims { iss: STATE_ISSUER.into(), exp: chrono_now() + 60, provider: "mock".into(), state: "expected".into(), nonce: "n".into(), verifier: "v".into(), redirect: "https://cms.example.com/admin/".into() }).unwrap();
        let wrong = o.finish("mock", "code", "other", Some(&cookie)).await.unwrap_err();
        assert_eq!((wrong.code, wrong.redirect.as_deref()), ("state", Some("https://cms.example.com/admin/")), "a valid cookie lets us send the user back with the error");
        let other_provider = o.finish("mock", "", "expected", Some(&cookie)).await.unwrap_err();
        assert_eq!(other_provider.code, "provider", "no code");
    }
}
