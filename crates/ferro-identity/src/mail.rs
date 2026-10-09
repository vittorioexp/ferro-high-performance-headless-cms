//! Outgoing email (SMTP). Used for password reset links and invitations.

use ferro_engine::{FerroError, Result};
use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::{SmtpConfig, SmtpSecurity};

pub struct Mailer {
    transport: Option<(AsyncSmtpTransport<Tokio1Executor>, Mailbox)>,
}

impl Mailer {
    /// No SMTP configured: `send` fails, and callers fall back to showing the link to an administrator.
    pub fn disabled() -> Mailer {
        Mailer { transport: None }
    }

    pub fn new(cfg: &SmtpConfig) -> Result<Mailer> {
        let from: Mailbox = cfg.from.parse().map_err(|e| FerroError::Internal(format!("FERRO_EMAIL_FROM `{}` is not a valid mailbox: {e}", cfg.from)))?;
        let builder = match cfg.security {
            SmtpSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host),
            SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host),
            SmtpSecurity::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)),
        }
        .map_err(|e| FerroError::Internal(format!("cannot set up SMTP for {}: {e}", cfg.host)))?;
        let mut builder = builder.port(cfg.port).timeout(Some(std::time::Duration::from_secs(20)));
        if let Some((u, p)) = &cfg.credentials {
            builder = builder.credentials(Credentials::new(u.clone(), p.clone()));
        }
        Ok(Mailer { transport: Some((builder.build(), from)) })
    }

    pub fn enabled(&self) -> bool {
        self.transport.is_some()
    }

    /// Send a message with a plain-text body and an HTML alternative.
    pub async fn send(&self, to: &str, subject: &str, text: &str, html: &str) -> Result<()> {
        let Some((transport, from)) = &self.transport else {
            return Err(FerroError::Internal("outgoing email is not configured (FERRO_SMTP_HOST)".into()));
        };
        let to: Mailbox = to.parse().map_err(|_| FerroError::invalid("invalid recipient address"))?;
        let msg = Message::builder()
            .from(from.clone())
            .to(to)
            .subject(subject)
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::builder().header(ContentType::TEXT_PLAIN).body(text.to_string()))
                    .singlepart(SinglePart::builder().header(ContentType::TEXT_HTML).body(html.to_string())),
            )
            .map_err(|e| FerroError::Internal(format!("cannot build the message: {e}")))?;
        transport.send(msg).await.map_err(|e| FerroError::Internal(format!("cannot send the email: {e}")))?;
        Ok(())
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Subject, plain text and HTML of the three messages Ferro sends.
pub struct Message3 {
    pub subject: String,
    pub text: String,
    pub html: String,
}

fn compose(app: &str, subject: String, greeting: &str, lines: &[&str], link: &str, footer: &str) -> Message3 {
    let text = format!("{greeting}\n\n{}\n\n{link}\n\n{footer}\n\n— {app}\n", lines.join("\n"));
    let body: String = lines.iter().map(|l| format!("<p>{}</p>", escape(l))).collect();
    let html = format!(
        "<div style=\"font-family:system-ui,Segoe UI,Arial,sans-serif;max-width:520px\"><p>{}</p>{body}<p><a href=\"{}\" style=\"display:inline-block;padding:10px 18px;background:#3056d3;color:#fff;border-radius:6px;text-decoration:none\">Continue</a></p><p style=\"color:#555;font-size:13px\">If the button does not work, copy this address into your browser:<br>{}</p><p style=\"color:#555;font-size:13px\">{}</p><p style=\"color:#888;font-size:12px\">{}</p></div>",
        escape(greeting),
        escape(link),
        escape(link),
        escape(footer),
        escape(app)
    );
    Message3 { subject, text, html }
}

pub fn password_reset_message(app: &str, first_name: Option<&str>, link: &str) -> Message3 {
    let greeting = format!("Hello{},", first_name.map(|n| format!(" {n}")).unwrap_or_default());
    compose(
        app,
        format!("Reset your {app} password"),
        &greeting,
        &["Someone asked to reset the password of your account. Use the button below to choose a new one; the link works once and expires in one hour."],
        link,
        "If you did not ask for this, ignore this email: your password stays as it is.",
    )
}

pub fn invitation_message(app: &str, link: &str) -> Message3 {
    compose(
        app,
        format!("You have been invited to {app}"),
        "Hello,",
        &["You have been invited to join. Use the button below to choose a password and sign in; the link works once and expires in seven days."],
        link,
        "If you were not expecting this invitation, you can ignore this email.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_escape_html_and_carry_the_link() {
        let m = password_reset_message("Ferro <script>", Some("Ann & <b>Bob</b>"), "https://x.test/r?token=a&b=\"c\"");
        assert!(m.text.contains("https://x.test/r?token=a&b=\"c\""), "plain text keeps the raw link");
        assert!(m.html.contains("href=\"https://x.test/r?token=a&amp;b=&quot;c&quot;\""));
        assert!(!m.html.contains("<script>") && !m.html.contains("<b>Bob"));
        assert!(m.subject.starts_with("Reset your "));
        assert!(m.text.contains("expires in one hour"));
        let i = invitation_message("Ferro", "https://x.test/i");
        assert!(i.subject.contains("invited") && i.text.contains("seven days"));
    }

    #[test]
    fn transports_can_be_built_for_every_security_mode_without_connecting() {
        for security in [SmtpSecurity::StartTls, SmtpSecurity::Tls, SmtpSecurity::None] {
            let m = Mailer::new(&SmtpConfig { host: "mail.example.invalid".into(), port: 587, security, credentials: Some(("u".into(), "p".into())), from: "Ferro <no-reply@example.com>".into() });
            assert!(m.map(|m| m.enabled()).unwrap_or(false));
        }
        assert!(Mailer::new(&SmtpConfig { host: "h".into(), port: 25, security: SmtpSecurity::None, credentials: None, from: "not a mailbox".into() }).is_err());
        assert!(!Mailer::disabled().enabled());
    }

    #[tokio::test]
    async fn a_disabled_mailer_refuses_to_send() {
        assert!(Mailer::disabled().send("a@b.c", "s", "t", "h").await.is_err());
    }
}
