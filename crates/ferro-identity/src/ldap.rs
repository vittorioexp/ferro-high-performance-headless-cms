//! Directory login (LDAP / Active Directory).
//!
//! The classic two-step "search then bind": a service account (or an anonymous bind) looks up the entry of the person
//! who is signing in, then Ferro binds as that entry's DN with the password they typed. The directory is the only judge
//! of the password; Ferro never sees a hash.

use std::time::Duration;

use ferro_engine::auth::{ExternalPolicy, ExternalProfile};
use ldap3::{ldap_escape, LdapConnAsync, LdapConnSettings, Scope, SearchEntry};

use crate::config::LdapConfig;

/// Why a directory sign-in did not succeed.
#[derive(Debug)]
pub enum LdapError {
    /// Wrong login or password, no such person, several matches, or an entry without an email.
    InvalidCredentials,
    /// The directory could not be reached or the service account was refused: not the user's fault.
    Unavailable(String),
}

pub struct Directory {
    cfg: LdapConfig,
}

const MAX_LOGIN_LEN: usize = 256;
const MAX_PASSWORD_LEN: usize = 1024;

/// The search filter for `login`, with `login` escaped per RFC 4515 so that `*`, `(`, `)` and `\` in what the user typed
/// are plain characters and can never widen the search.
pub fn build_filter(template: &str, login: &str) -> String {
    template.replace("{login}", &ldap_escape(login))
}

fn first<'a>(entry: &'a SearchEntry, attr: &str) -> Option<&'a str> {
    entry.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(attr)).and_then(|(_, v)| v.first()).map(|s| s.as_str()).filter(|s| !s.trim().is_empty())
}

impl Directory {
    pub fn new(cfg: LdapConfig) -> Directory {
        Directory { cfg }
    }

    pub fn policy(&self) -> ExternalPolicy {
        ExternalPolicy { default_role: self.cfg.default_role, allow_registration: self.cfg.allow_public_registration, link_existing: self.cfg.link_existing_users }
    }

    async fn connect(&self) -> Result<ldap3::Ldap, LdapError> {
        let timeout = Duration::from_secs(self.cfg.timeout_secs);
        let settings = LdapConnSettings::new().set_conn_timeout(timeout).set_starttls(self.cfg.starttls).set_no_tls_verify(!self.cfg.tls_verify);
        let (conn, mut ldap) = LdapConnAsync::with_settings(settings, &self.cfg.url).await.map_err(|e| LdapError::Unavailable(format!("cannot connect to {}: {e}", self.cfg.url)))?;
        ldap3::drive!(conn);
        ldap.with_timeout(timeout);
        Ok(ldap)
    }

    /// Check a login and password against the directory and describe the person.
    pub async fn authenticate(&self, login: &str, password: &str) -> Result<ExternalProfile, LdapError> {
        let login = login.trim();
        // An empty password makes a simple bind "unauthenticated", which most servers answer with success: never try it.
        if login.is_empty() || login.len() > MAX_LOGIN_LEN || password.is_empty() || password.len() > MAX_PASSWORD_LEN || password.contains('\0') || login.contains('\0') {
            return Err(LdapError::InvalidCredentials);
        }
        let mut ldap = self.connect().await?;
        let result = self.lookup_and_bind(&mut ldap, login, password).await;
        let _ = ldap.unbind().await;
        result
    }

    async fn lookup_and_bind(&self, ldap: &mut ldap3::Ldap, login: &str, password: &str) -> Result<ExternalProfile, LdapError> {
        let unavailable = |what: &str, e: ldap3::LdapError| LdapError::Unavailable(format!("{what}: {e}"));
        if let (Some(dn), Some(pw)) = (&self.cfg.bind_dn, &self.cfg.bind_password) {
            ldap.simple_bind(dn, pw).await.map_err(|e| unavailable("service account bind", e))?.success().map_err(|e| unavailable("the service account was refused", e))?;
        }
        let filter = build_filter(&self.cfg.user_filter, login);
        let attrs = vec![self.cfg.email_attribute.as_str(), self.cfg.first_name_attribute.as_str(), self.cfg.last_name_attribute.as_str(), self.cfg.identifier_attribute.as_str()];
        let (found, _) = ldap
            .search(&self.cfg.user_base_dn, Scope::Subtree, &filter, attrs)
            .await
            .map_err(|e| unavailable("search", e))?
            .success()
            .map_err(|e| unavailable("search", e))?;
        let mut entries = found.into_iter().map(SearchEntry::construct);
        let (Some(entry), None) = (entries.next(), entries.next()) else {
            // none, or ambiguous: either way nobody can be named
            return Err(LdapError::InvalidCredentials);
        };
        let Some(email) = first(&entry, &self.cfg.email_attribute) else {
            tracing::warn!(dn = %entry.dn, "directory entry has no `{}`: it cannot become a Ferro user", self.cfg.email_attribute);
            return Err(LdapError::InvalidCredentials);
        };
        let identifier = first(&entry, &self.cfg.identifier_attribute).map(String::from).unwrap_or_else(|| entry.dn.clone());
        let profile = ExternalProfile {
            provider: "ldap".into(),
            identifier,
            email: email.trim().to_ascii_lowercase(),
            first_name: first(&entry, &self.cfg.first_name_attribute).map(String::from),
            last_name: first(&entry, &self.cfg.last_name_attribute).map(String::from),
        };
        // the password check: bind as the person
        match ldap.simple_bind(&entry.dn, password).await {
            Ok(r) if r.rc == 0 => Ok(profile),
            Ok(r) if r.rc == 49 => Err(LdapError::InvalidCredentials), // invalidCredentials
            Ok(r) => Err(LdapError::Unavailable(format!("user bind answered {} {}", r.rc, r.text))),
            Err(e) => Err(unavailable("user bind", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_cannot_change_the_shape_of_the_filter() {
        let t = "(&(objectClass=person)(|(uid={login})(mail={login})))";
        assert_eq!(build_filter(t, "alice"), "(&(objectClass=person)(|(uid=alice)(mail=alice)))");
        for evil in ["*", "alice)(uid=*", "*)(uid=*))(|(uid=*", "a\\b", "x\u{0}y", "(cn=admin)"] {
            let f = build_filter(t, evil);
            // after escaping, the only parentheses are the template's own and no wildcard is left
            assert!(!f.contains('*'), "{evil:?} -> {f}");
            assert_eq!(f.matches('(').count(), 5, "{evil:?} -> {f}");
            assert_eq!(f.matches(')').count(), 5, "{evil:?} -> {f}");
        }
        assert!(build_filter(t, "alice)(uid=*").contains("\\29\\28uid=\\2a"));
        assert_eq!(build_filter("(uid={login})", "Zoë"), "(uid=Zoë)", "UTF-8 is legal in a filter and is left alone");
    }

    #[tokio::test]
    async fn refuses_unusable_input_without_touching_the_network() {
        let d = Directory::new(LdapConfig {
            url: "ldap://127.0.0.1:1".into(),
            starttls: false,
            tls_verify: true,
            bind_dn: None,
            bind_password: None,
            user_base_dn: "dc=x".into(),
            user_filter: "(uid={login})".into(),
            email_attribute: "mail".into(),
            first_name_attribute: "givenName".into(),
            last_name_attribute: "sn".into(),
            identifier_attribute: "entryUUID".into(),
            allow_public_registration: false,
            default_role: None,
            link_existing_users: false,
            timeout_secs: 1,
        });
        for (login, pw) in [("", "pw"), ("alice", ""), ("alice", "p\0w"), ("a\0b", "pw")] {
            assert!(matches!(d.authenticate(login, pw).await, Err(LdapError::InvalidCredentials)), "{login:?}/{pw:?}");
        }
        assert!(matches!(d.authenticate(&"x".repeat(300), "pw").await, Err(LdapError::InvalidCredentials)));
        assert!(matches!(d.authenticate("alice", "pw").await, Err(LdapError::Unavailable(_))), "a refused connection is an outage, not a wrong password");
    }
}
