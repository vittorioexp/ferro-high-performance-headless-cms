//! Identity configuration from environment variables.
//!
//! ```text
//! FERRO_PUBLIC_URL=https://cms.example.com     where users reach Ferro; the base of every link sent by email and of the
//!                                              SSO callback. Never taken from the request (Host header spoofing).
//! FERRO_AUTH_REDIRECT_ALLOWLIST=https://app.example.com,https://intranet.example.com
//!                                              extra origins that password-reset / invitation / SSO flows may send users
//!                                              back to (the origin of FERRO_PUBLIC_URL is always allowed)
//!
//! FERRO_SMTP_HOST=smtp.example.com             enables outgoing email
//! FERRO_SMTP_PORT=587                          (465 with SECURE=tls, 25 with SECURE=none)
//! FERRO_SMTP_SECURE=starttls                   starttls | tls | none
//! FERRO_SMTP_USER / FERRO_SMTP_PASSWORD        optional
//! FERRO_EMAIL_FROM="Ferro <no-reply@example.com>"
//! ```
//!
//! Directory login (LDAP / Active Directory): users type their directory login and password into the normal sign-in form.
//! ```text
//! FERRO_LDAP_URL=ldaps://ldap.example.com:636      ldap:// (add FERRO_LDAP_STARTTLS=true) or ldaps://
//! FERRO_LDAP_TLS_VERIFY=true                       set to false only for a test server with a self-signed certificate
//! FERRO_LDAP_BIND_DN=cn=ferro,ou=services,dc=example,dc=com   service account used to find the user (omit: anonymous search)
//! FERRO_LDAP_BIND_PASSWORD=...
//! FERRO_LDAP_USER_BASE_DN=ou=people,dc=example,dc=com
//! FERRO_LDAP_USER_FILTER="(&(objectClass=person)(|(uid={login})(mail={login})))"   {login} is escaped (RFC 4515)
//! FERRO_LDAP_EMAIL_ATTRIBUTE=mail  _FIRST_NAME_ATTRIBUTE=givenName  _LAST_NAME_ATTRIBUTE=sn
//! FERRO_LDAP_IDENTIFIER_ATTRIBUTE=entryUUID        stable id of the entry; the DN is used when the server returns none
//! FERRO_LDAP_ALLOW_PUBLIC_REGISTRATION=false  FERRO_LDAP_DEFAULT_ROLE=<role uuid>  FERRO_LDAP_LINK_EXISTING_USERS=false
//! FERRO_LDAP_TIMEOUT_SECS=10
//! ```
//!
//! Single sign-on providers (OpenID Connect and plain OAuth 2.0):
//! ```text
//! FERRO_AUTH_PROVIDERS=azure,google            names (letters, digits, - and _); used in /auth/oauth/<name>
//! FERRO_AUTH_AZURE_DRIVER=openid               openid (default) | oauth2
//! FERRO_AUTH_AZURE_CLIENT_ID / _CLIENT_SECRET
//! FERRO_AUTH_AZURE_ISSUER_URL=https://login.microsoftonline.com/<tenant>/v2.0     openid: endpoints come from discovery
//! FERRO_AUTH_AZURE_AUTHORIZE_URL / _TOKEN_URL / _PROFILE_URL / _JWKS_URL           explicit endpoints (required for oauth2)
//! FERRO_AUTH_AZURE_SCOPE="openid profile email"
//! FERRO_AUTH_AZURE_EMAIL_KEY=email  _IDENTIFIER_KEY=sub  _FIRST_NAME_KEY=given_name  _LAST_NAME_KEY=family_name
//! FERRO_AUTH_AZURE_REQUIRE_VERIFIED_EMAIL=true   openid: refuse an email the provider does not vouch for
//! FERRO_AUTH_AZURE_ALLOWED_EMAIL_DOMAINS=example.com,example.org
//! FERRO_AUTH_AZURE_ALLOW_PUBLIC_REGISTRATION=false   create the account at first sign-in
//! FERRO_AUTH_AZURE_DEFAULT_ROLE=<role uuid>          role of accounts created that way
//! FERRO_AUTH_AZURE_LINK_EXISTING_USERS=false         let a provider take over an existing password account by email
//! FERRO_AUTH_AZURE_LABEL="Sign in with Microsoft"  _ICON=microsoft
//! ```

use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub enum SmtpSecurity {
    StartTls,
    Tls,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub credentials: Option<(String, String)>,
    pub from: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthDriver {
    OpenId,
    OAuth2,
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub label: String,
    pub icon: Option<String>,
    pub driver: OAuthDriver,
    pub client_id: String,
    pub client_secret: String,
    pub issuer_url: Option<Url>,
    pub authorize_url: Option<Url>,
    pub token_url: Option<Url>,
    pub profile_url: Option<Url>,
    pub jwks_url: Option<Url>,
    pub scope: String,
    pub email_key: String,
    pub identifier_key: String,
    pub first_name_key: String,
    pub last_name_key: String,
    pub require_verified_email: bool,
    pub allowed_email_domains: Vec<String>,
    pub allow_public_registration: bool,
    pub default_role: Option<uuid::Uuid>,
    pub link_existing_users: bool,
}

#[derive(Debug, Clone)]
pub struct LdapConfig {
    pub url: String,
    pub starttls: bool,
    pub tls_verify: bool,
    pub bind_dn: Option<String>,
    pub bind_password: Option<String>,
    pub user_base_dn: String,
    pub user_filter: String,
    pub email_attribute: String,
    pub first_name_attribute: String,
    pub last_name_attribute: String,
    pub identifier_attribute: String,
    pub allow_public_registration: bool,
    pub default_role: Option<uuid::Uuid>,
    pub link_existing_users: bool,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Default)]
pub struct IdentityConfig {
    pub public_url: Option<Url>,
    pub redirect_allowlist: Vec<Url>,
    pub smtp: Option<SmtpConfig>,
    pub providers: Vec<ProviderConfig>,
    pub ldap: Option<LdapConfig>,
}

/// Secrets and codes travel to these addresses: https, except for a local test server.
pub fn endpoint_scheme_ok(u: &Url) -> bool {
    u.scheme() == "https" || matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

fn parse_url(name: &str, v: &str) -> Result<Url, String> {
    let u = Url::parse(v.trim()).map_err(|e| format!("{name}: `{v}` is not a valid URL ({e})"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return Err(format!("{name}: `{v}` must be an http(s) URL"));
    }
    Ok(u)
}

impl IdentityConfig {
    pub fn from_env() -> Result<IdentityConfig, String> {
        Self::from_vars(&|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_vars(get: &dyn Fn(&str) -> Option<String>) -> Result<IdentityConfig, String> {
        let public_url = match get("FERRO_PUBLIC_URL") {
            Some(v) => Some(parse_url("FERRO_PUBLIC_URL", &v)?),
            None => None,
        };
        let redirect_allowlist = get("FERRO_AUTH_REDIRECT_ALLOWLIST")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| parse_url("FERRO_AUTH_REDIRECT_ALLOWLIST", s))
            .collect::<Result<Vec<_>, _>>()?;
        let smtp = match get("FERRO_SMTP_HOST") {
            None => None,
            Some(host) => {
                let security = match get("FERRO_SMTP_SECURE").unwrap_or_else(|| "starttls".into()).to_ascii_lowercase().as_str() {
                    "starttls" => SmtpSecurity::StartTls,
                    "tls" | "ssl" => SmtpSecurity::Tls,
                    "none" | "false" => SmtpSecurity::None,
                    other => return Err(format!("FERRO_SMTP_SECURE: `{other}` is not starttls, tls or none")),
                };
                let default_port = match security {
                    SmtpSecurity::StartTls => 587,
                    SmtpSecurity::Tls => 465,
                    SmtpSecurity::None => 25,
                };
                let port = match get("FERRO_SMTP_PORT") {
                    Some(p) => p.parse::<u16>().map_err(|_| format!("FERRO_SMTP_PORT: `{p}` is not a port number"))?,
                    None => default_port,
                };
                let credentials = match (get("FERRO_SMTP_USER"), get("FERRO_SMTP_PASSWORD")) {
                    (Some(u), Some(p)) => Some((u, p)),
                    (None, None) => None,
                    _ => return Err("FERRO_SMTP_USER and FERRO_SMTP_PASSWORD must be set together".into()),
                };
                let from = get("FERRO_EMAIL_FROM").ok_or("FERRO_EMAIL_FROM is required when FERRO_SMTP_HOST is set")?;
                Some(SmtpConfig { host, port, security, credentials, from })
            }
        };
        let names: Vec<String> = get("FERRO_AUTH_PROVIDERS").unwrap_or_default().split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
        if let Some(dup) = names.iter().enumerate().find(|(i, n)| names[..*i].contains(n)).map(|(_, n)| n) {
            return Err(format!("FERRO_AUTH_PROVIDERS lists `{dup}` twice"));
        }
        let providers = names.iter().map(|n| Self::provider(n, get)).collect::<Result<Vec<_>, _>>()?;
        if !providers.is_empty() && public_url.is_none() {
            return Err("FERRO_PUBLIC_URL is required when FERRO_AUTH_PROVIDERS is set (it is the base of the sign-in callback)".into());
        }
        let ldap = Self::ldap(get)?;
        Ok(IdentityConfig { public_url, redirect_allowlist, smtp, providers, ldap })
    }

    fn ldap(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<LdapConfig>, String> {
        let Some(url) = get("FERRO_LDAP_URL") else { return Ok(None) };
        let v = |s: &str| get(&format!("FERRO_LDAP_{s}"));
        let flag = |s: &str, default: bool| -> Result<bool, String> {
            match v(s).map(|x| x.to_ascii_lowercase()) {
                None => Ok(default),
                Some(x) if matches!(x.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
                Some(x) if matches!(x.as_str(), "0" | "false" | "no" | "off") => Ok(false),
                Some(x) => Err(format!("FERRO_LDAP_{s}: `{x}` is not true or false")),
            }
        };
        let parsed = Url::parse(&url).map_err(|e| format!("FERRO_LDAP_URL: `{url}` is not a valid URL ({e})"))?;
        if !matches!(parsed.scheme(), "ldap" | "ldaps") || parsed.host_str().is_none() {
            return Err(format!("FERRO_LDAP_URL: `{url}` must be ldap://host[:port] or ldaps://host[:port]"));
        }
        let starttls = flag("STARTTLS", false)?;
        if starttls && parsed.scheme() == "ldaps" {
            return Err("FERRO_LDAP_STARTTLS cannot be combined with an ldaps:// URL".into());
        }
        let bind_dn = v("BIND_DN");
        let bind_password = v("BIND_PASSWORD");
        if bind_dn.is_some() != bind_password.is_some() {
            return Err("FERRO_LDAP_BIND_DN and FERRO_LDAP_BIND_PASSWORD must be set together".into());
        }
        let user_filter = v("USER_FILTER").unwrap_or_else(|| "(&(objectClass=person)(|(uid={login})(mail={login})))".into());
        if user_filter.matches("{login}").count() == 0 {
            return Err("FERRO_LDAP_USER_FILTER must contain {login}".into());
        }
        let default_role = match v("DEFAULT_ROLE") {
            Some(r) => Some(uuid::Uuid::parse_str(&r).map_err(|_| format!("FERRO_LDAP_DEFAULT_ROLE: `{r}` is not a role id (uuid)"))?),
            None => None,
        };
        let timeout_secs = match v("TIMEOUT_SECS") {
            Some(t) => t.parse::<u64>().ok().filter(|n| (1..=300).contains(n)).ok_or_else(|| format!("FERRO_LDAP_TIMEOUT_SECS: `{t}` must be between 1 and 300"))?,
            None => 10,
        };
        Ok(Some(LdapConfig {
            url,
            starttls,
            tls_verify: flag("TLS_VERIFY", true)?,
            bind_dn,
            bind_password,
            user_base_dn: v("USER_BASE_DN").ok_or("FERRO_LDAP_USER_BASE_DN is required when FERRO_LDAP_URL is set")?,
            user_filter,
            email_attribute: v("EMAIL_ATTRIBUTE").unwrap_or_else(|| "mail".into()),
            first_name_attribute: v("FIRST_NAME_ATTRIBUTE").unwrap_or_else(|| "givenName".into()),
            last_name_attribute: v("LAST_NAME_ATTRIBUTE").unwrap_or_else(|| "sn".into()),
            identifier_attribute: v("IDENTIFIER_ATTRIBUTE").unwrap_or_else(|| "entryUUID".into()),
            allow_public_registration: flag("ALLOW_PUBLIC_REGISTRATION", false)?,
            default_role,
            link_existing_users: flag("LINK_EXISTING_USERS", false)?,
            timeout_secs,
        }))
    }

    fn provider(name: &str, get: &dyn Fn(&str) -> Option<String>) -> Result<ProviderConfig, String> {
        if name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || name.starts_with('-') {
            return Err(format!("invalid sign-in provider name `{name}` (letters, digits, - and _; at most 32)"));
        }
        let p = format!("FERRO_AUTH_{}_", name.to_ascii_uppercase().replace('-', "_"));
        let v = |suffix: &str| get(&format!("{p}{suffix}"));
        let need = |suffix: &str| v(suffix).ok_or_else(|| format!("{p}{suffix} is required"));
        let url = |suffix: &str| -> Result<Option<Url>, String> {
            match v(suffix) {
                None => Ok(None),
                Some(raw) => {
                    let u = parse_url(&format!("{p}{suffix}"), &raw)?;
                    if !endpoint_scheme_ok(&u) {
                        return Err(format!("{p}{suffix}: `{raw}` must be https (plain http is only accepted for localhost)"));
                    }
                    Ok(Some(u))
                }
            }
        };
        let flag = |suffix: &str, default: bool| -> Result<bool, String> {
            match v(suffix).map(|s| s.to_ascii_lowercase()) {
                None => Ok(default),
                Some(s) if matches!(s.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
                Some(s) if matches!(s.as_str(), "0" | "false" | "no" | "off") => Ok(false),
                Some(s) => Err(format!("{p}{suffix}: `{s}` is not true or false")),
            }
        };
        let driver = match v("DRIVER").unwrap_or_else(|| "openid".into()).to_ascii_lowercase().as_str() {
            "openid" | "oidc" => OAuthDriver::OpenId,
            "oauth2" | "oauth" => OAuthDriver::OAuth2,
            other => return Err(format!("{p}DRIVER: `{other}` is not openid or oauth2")),
        };
        let (issuer_url, authorize_url, token_url, profile_url, jwks_url) = (url("ISSUER_URL")?, url("AUTHORIZE_URL")?, url("TOKEN_URL")?, url("PROFILE_URL")?, url("JWKS_URL")?);
        match driver {
            OAuthDriver::OpenId => {
                if issuer_url.is_none() && (authorize_url.is_none() || token_url.is_none() || jwks_url.is_none()) {
                    return Err(format!("{p}ISSUER_URL is required (or AUTHORIZE_URL, TOKEN_URL and JWKS_URL)"));
                }
            }
            OAuthDriver::OAuth2 => {
                if authorize_url.is_none() || token_url.is_none() || profile_url.is_none() {
                    return Err(format!("{p}AUTHORIZE_URL, {p}TOKEN_URL and {p}PROFILE_URL are required for an oauth2 provider"));
                }
            }
        }
        let default_role = match v("DEFAULT_ROLE") {
            Some(r) => Some(uuid::Uuid::parse_str(&r).map_err(|_| format!("{p}DEFAULT_ROLE: `{r}` is not a role id (uuid)"))?),
            None => None,
        };
        let openid = driver == OAuthDriver::OpenId;
        Ok(ProviderConfig {
            name: name.to_string(),
            label: v("LABEL").unwrap_or_else(|| format!("Sign in with {name}")),
            icon: v("ICON"),
            driver,
            client_id: need("CLIENT_ID")?,
            client_secret: need("CLIENT_SECRET")?,
            issuer_url,
            authorize_url,
            token_url,
            profile_url,
            jwks_url,
            scope: v("SCOPE").unwrap_or_else(|| if openid { "openid profile email".into() } else { "user:email".into() }),
            email_key: v("EMAIL_KEY").unwrap_or_else(|| "email".into()),
            identifier_key: v("IDENTIFIER_KEY").unwrap_or_else(|| if openid { "sub".into() } else { "id".into() }),
            first_name_key: v("FIRST_NAME_KEY").unwrap_or_else(|| "given_name".into()),
            last_name_key: v("LAST_NAME_KEY").unwrap_or_else(|| "family_name".into()),
            require_verified_email: flag("REQUIRE_VERIFIED_EMAIL", openid)?,
            allowed_email_domains: v("ALLOWED_EMAIL_DOMAINS").unwrap_or_default().split(',').map(|s| s.trim().trim_start_matches('@').to_ascii_lowercase()).filter(|s| !s.is_empty()).collect(),
            allow_public_registration: flag("ALLOW_PUBLIC_REGISTRATION", false)?,
            default_role,
            link_existing_users: flag("LINK_EXISTING_USERS", false)?,
        })
    }

    /// May the user be sent back to `target`? Same origin (scheme, host, port) as the public URL or an allow-listed
    /// one. Compared as parsed origins: a string prefix test would accept `https://cms.example.com.evil.net`.
    pub fn redirect_allowed(&self, target: &str) -> bool {
        let Ok(t) = Url::parse(target) else { return false };
        if !matches!(t.scheme(), "http" | "https") || !t.username().is_empty() || t.password().is_some() {
            return false;
        }
        let origin = t.origin();
        self.public_url.iter().chain(self.redirect_allowlist.iter()).any(|a| a.origin() == origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<IdentityConfig, String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        IdentityConfig::from_vars(&|k| m.get(k).cloned())
    }

    #[test]
    fn nothing_configured_is_fine() {
        let c = cfg(&[]).unwrap();
        assert!(c.public_url.is_none() && c.smtp.is_none());
        assert!(!c.redirect_allowed("https://anything.example.com"), "no public URL: nothing is allowed");
    }

    #[test]
    fn smtp_defaults_follow_the_security_mode() {
        let c = cfg(&[("FERRO_SMTP_HOST", "mail"), ("FERRO_EMAIL_FROM", "a@b.c")]).unwrap().smtp.unwrap();
        assert_eq!((c.port, c.security), (587, SmtpSecurity::StartTls));
        let c = cfg(&[("FERRO_SMTP_HOST", "mail"), ("FERRO_EMAIL_FROM", "a@b.c"), ("FERRO_SMTP_SECURE", "TLS")]).unwrap().smtp.unwrap();
        assert_eq!((c.port, c.security), (465, SmtpSecurity::Tls));
        let c = cfg(&[("FERRO_SMTP_HOST", "mail"), ("FERRO_EMAIL_FROM", "a@b.c"), ("FERRO_SMTP_SECURE", "none"), ("FERRO_SMTP_PORT", "2525"), ("FERRO_SMTP_USER", "u"), ("FERRO_SMTP_PASSWORD", "p")]).unwrap().smtp.unwrap();
        assert_eq!((c.port, c.security, c.credentials), (2525, SmtpSecurity::None, Some(("u".into(), "p".into()))));
    }

    #[test]
    fn misconfigurations_are_reported() {
        for (vars, needle) in [
            (vec![("FERRO_SMTP_HOST", "mail")], "FERRO_EMAIL_FROM"),
            (vec![("FERRO_SMTP_HOST", "mail"), ("FERRO_EMAIL_FROM", "x@y.z"), ("FERRO_SMTP_USER", "u")], "together"),
            (vec![("FERRO_SMTP_HOST", "mail"), ("FERRO_EMAIL_FROM", "x@y.z"), ("FERRO_SMTP_SECURE", "maybe")], "FERRO_SMTP_SECURE"),
            (vec![("FERRO_SMTP_HOST", "mail"), ("FERRO_EMAIL_FROM", "x@y.z"), ("FERRO_SMTP_PORT", "99999")], "FERRO_SMTP_PORT"),
            (vec![("FERRO_PUBLIC_URL", "not a url")], "FERRO_PUBLIC_URL"),
            (vec![("FERRO_PUBLIC_URL", "ftp://x.y")], "http(s)"),
            (vec![("FERRO_AUTH_REDIRECT_ALLOWLIST", "https://ok.example.com, nope")], "FERRO_AUTH_REDIRECT_ALLOWLIST"),
        ] {
            let err = cfg(&vars).unwrap_err();
            assert!(err.contains(needle), "{err} should mention {needle}");
        }
    }

    fn provider_vars<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut v = vec![("FERRO_PUBLIC_URL", "https://cms.example.com"), ("FERRO_AUTH_PROVIDERS", "Azure"), ("FERRO_AUTH_AZURE_CLIENT_ID", "id"), ("FERRO_AUTH_AZURE_CLIENT_SECRET", "secret")];
        v.extend_from_slice(extra);
        v
    }

    #[test]
    fn openid_provider_with_discovery() {
        let c = cfg(&provider_vars(&[("FERRO_AUTH_AZURE_ISSUER_URL", "https://login.example.com/tenant/v2.0"), ("FERRO_AUTH_AZURE_ALLOWED_EMAIL_DOMAINS", "@Example.com, example.org")])).unwrap();
        let p = &c.providers[0];
        assert_eq!((p.name.as_str(), p.driver, p.scope.as_str(), p.identifier_key.as_str()), ("azure", OAuthDriver::OpenId, "openid profile email", "sub"));
        assert!(p.require_verified_email && !p.allow_public_registration && !p.link_existing_users, "the safe defaults");
        assert_eq!(p.allowed_email_domains, vec!["example.com", "example.org"]);
        assert_eq!(p.label, "Sign in with azure");
    }

    #[test]
    fn oauth2_provider_needs_explicit_endpoints() {
        let all = [("FERRO_AUTH_AZURE_DRIVER", "oauth2"), ("FERRO_AUTH_AZURE_AUTHORIZE_URL", "https://x.test/a"), ("FERRO_AUTH_AZURE_TOKEN_URL", "https://x.test/t"), ("FERRO_AUTH_AZURE_PROFILE_URL", "https://x.test/p"), ("FERRO_AUTH_AZURE_DEFAULT_ROLE", "0199a000-0000-7000-8000-00000000e001"), ("FERRO_AUTH_AZURE_ALLOW_PUBLIC_REGISTRATION", "true")];
        let c = cfg(&provider_vars(&all)).unwrap();
        let p = &c.providers[0];
        assert_eq!((p.driver, p.identifier_key.as_str(), p.require_verified_email, p.allow_public_registration), (OAuthDriver::OAuth2, "id", false, true));
        assert!(p.default_role.is_some());
        assert!(cfg(&provider_vars(&all[..3])).unwrap_err().contains("PROFILE_URL"));
    }

    #[test]
    fn provider_misconfigurations_are_reported() {
        let issuer = ("FERRO_AUTH_AZURE_ISSUER_URL", "https://login.example.com");
        for (extra, needle) in [
            (vec![], "ISSUER_URL"),
            (vec![issuer, ("FERRO_AUTH_AZURE_DRIVER", "saml")], "DRIVER"),
            (vec![("FERRO_AUTH_AZURE_ISSUER_URL", "http://login.example.com")], "https"),
            (vec![issuer, ("FERRO_AUTH_AZURE_DEFAULT_ROLE", "editor")], "DEFAULT_ROLE"),
            (vec![issuer, ("FERRO_AUTH_AZURE_ALLOW_PUBLIC_REGISTRATION", "maybe")], "true or false"),
        ] {
            let err = cfg(&provider_vars(&extra)).unwrap_err();
            assert!(err.contains(needle), "{err} should mention {needle}");
        }
        let mut no_secret = provider_vars(&[issuer]);
        no_secret.retain(|(k, _)| *k != "FERRO_AUTH_AZURE_CLIENT_SECRET");
        assert!(cfg(&no_secret).unwrap_err().contains("CLIENT_SECRET"));
        let mut no_public = provider_vars(&[issuer]);
        no_public.retain(|(k, _)| *k != "FERRO_PUBLIC_URL");
        assert!(cfg(&no_public).unwrap_err().contains("FERRO_PUBLIC_URL"));
        assert!(cfg(&[("FERRO_PUBLIC_URL", "https://c.test"), ("FERRO_AUTH_PROVIDERS", "a,A")]).unwrap_err().contains("twice"));
        assert!(cfg(&[("FERRO_PUBLIC_URL", "https://c.test"), ("FERRO_AUTH_PROVIDERS", "bad name")]).unwrap_err().contains("invalid"));
        // plain http is fine for a local test provider
        assert!(cfg(&provider_vars(&[("FERRO_AUTH_AZURE_ISSUER_URL", "http://127.0.0.1:9000")])).is_ok());
    }

    #[test]
    fn ldap_defaults_and_validation() {
        let base = [("FERRO_LDAP_URL", "ldaps://ldap.example.com"), ("FERRO_LDAP_USER_BASE_DN", "ou=people,dc=example,dc=com")];
        let l = cfg(&base).unwrap().ldap.unwrap();
        assert_eq!((l.starttls, l.tls_verify, l.email_attribute.as_str(), l.identifier_attribute.as_str(), l.timeout_secs), (false, true, "mail", "entryUUID", 10));
        assert!(l.user_filter.contains("{login}") && !l.allow_public_registration && !l.link_existing_users, "safe defaults");
        assert!(cfg(&[]).unwrap().ldap.is_none());
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut v = base.to_vec();
            v.extend_from_slice(extra);
            cfg(&v)
        };
        for (extra, needle) in [
            (vec![("FERRO_LDAP_URL", "http://ldap.example.com")], "ldap://"),
            (vec![("FERRO_LDAP_URL", "nonsense")], "FERRO_LDAP_URL"),
            (vec![("FERRO_LDAP_STARTTLS", "true")], "ldaps://"),
            (vec![("FERRO_LDAP_BIND_DN", "cn=x")], "together"),
            (vec![("FERRO_LDAP_USER_FILTER", "(uid=bob)")], "{login}"),
            (vec![("FERRO_LDAP_TIMEOUT_SECS", "0")], "TIMEOUT"),
            (vec![("FERRO_LDAP_TLS_VERIFY", "perhaps")], "true or false"),
            (vec![("FERRO_LDAP_DEFAULT_ROLE", "admin")], "DEFAULT_ROLE"),
        ] {
            let err = with(&extra).unwrap_err();
            assert!(err.contains(needle), "{err} should mention {needle}");
        }
        assert!(cfg(&[("FERRO_LDAP_URL", "ldap://h")]).unwrap_err().contains("USER_BASE_DN"));
        let l = with(&[("FERRO_LDAP_URL", "ldap://h:389"), ("FERRO_LDAP_STARTTLS", "yes"), ("FERRO_LDAP_BIND_DN", "cn=svc"), ("FERRO_LDAP_BIND_PASSWORD", "pw")]).unwrap().ldap.unwrap();
        assert!(l.starttls && l.bind_dn.as_deref() == Some("cn=svc"));
    }

    #[test]
    fn redirects_are_compared_by_origin() {
        let c = cfg(&[("FERRO_PUBLIC_URL", "https://cms.example.com"), ("FERRO_AUTH_REDIRECT_ALLOWLIST", "https://app.example.com, http://localhost:3000/path")]).unwrap();
        for ok in ["https://cms.example.com/admin/#/reset", "https://cms.example.com", "https://app.example.com/login?x=1", "http://localhost:3000/anything"] {
            assert!(c.redirect_allowed(ok), "{ok}");
        }
        for bad in [
            "https://cms.example.com.evil.net/reset",
            "https://evil.net/https://cms.example.com",
            "https://cms.example.com@evil.net/",
            "https://user:pw@cms.example.com/",
            "http://cms.example.com/",
            "https://cms.example.com:8443/",
            "javascript:alert(1)",
            "//evil.net",
            "/relative",
            "",
            "https://app.example.com.evil.net",
            "http://localhost:3001/",
        ] {
            assert!(!c.redirect_allowed(bad), "{bad}");
        }
    }
}
