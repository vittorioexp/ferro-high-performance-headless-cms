//! External identity for Ferro: single sign-on, directory login and outgoing email.

pub mod config;
pub mod ldap;
pub mod mail;
pub mod oauth;

/// Choose the TLS crypto provider for the whole process. The dependency tree contains two (aws-lc-rs through the AWS SDK
/// and the LDAP client, ring through others) and rustls refuses to guess: without this, the first TLS connection that does
/// not name a provider itself (ldaps://, SMTP over TLS) panics. Safe to call more than once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub use config::IdentityConfig;
pub use mail::Mailer;
pub use oauth::OAuth;
