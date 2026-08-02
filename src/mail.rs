//! Mail build reports over SMTP. Credentials from env vars only.
//! With user+pass set: STARTTLS submission (port 587). Without: plain
//! relay on port 25 (internal mail relays).

use crate::config::MailCfg;
use anyhow::{Context, Result, bail};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

pub fn send_report(cfg: &MailCfg, subject: &str, body: &str) -> Result<()> {
    let host = std::env::var(&cfg.smtp_host_env).unwrap_or_default();
    if host.is_empty() {
        bail!(
            "mail requested but {} is not set (SMTP server hostname)",
            cfg.smtp_host_env
        );
    }
    let user = std::env::var(&cfg.smtp_user_env).unwrap_or_default();
    let pass = std::env::var(&cfg.smtp_pass_env).unwrap_or_default();

    let mut builder = Message::builder()
        .from(cfg.from.parse().context("invalid `from` address")?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN);
    for to in &cfg.to {
        builder = builder.to(to.parse().with_context(|| format!("invalid recipient '{to}'"))?);
    }
    let email = builder.body(body.to_string())?;

    let mailer = if !user.is_empty() && !pass.is_empty() {
        SmtpTransport::starttls_relay(&host)?
            .credentials(Credentials::new(user, pass))
            .build()
    } else {
        // unauthenticated internal relay
        SmtpTransport::builder_dangerous(&host).port(25).build()
    };
    mailer
        .send(&email)
        .with_context(|| format!("sending mail via {host}"))?;
    eprintln!("📧 report mailed to {}", cfg.to.join(", "));
    Ok(())
}
