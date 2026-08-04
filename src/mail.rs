//! Mail build reports over SMTP. Credentials from env vars only.
//! With user+pass set: STARTTLS submission (port 587). Without: plain
//! relay on port 25 (internal mail relays).

use crate::config::MailCfg;
use anyhow::{Context, Result, bail};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use std::path::{Path, PathBuf};

fn mime_for(path: &Path) -> ContentType {
    let ct = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "pdf" => "application/pdf",
        "html" => "text/html; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    };
    ContentType::parse(ct).unwrap_or(ContentType::TEXT_PLAIN)
}

/// Logo shipped as an inline (cid:) attachment — Gmail and Outlook refuse
/// data: URIs, so the image has to travel as a real MIME part.
const LOGO_PNG: &[u8] = include_bytes!("../docs/brand/hefesto.logo-small.png");

/// HTML report with a plain-text alternative, the inline logo, and any
/// files attached. Structure: mixed( related( alternative(text, html),
/// logo ), attachments… ) — the layout every mail client understands.
pub fn send_html(
    cfg: &MailCfg,
    subject: &str,
    html: &str,
    plain: &str,
    files: &[PathBuf],
) -> Result<()> {
    let related = MultiPart::related()
        .multipart(MultiPart::alternative_plain_html(
            plain.to_string(),
            html.to_string(),
        ))
        .singlepart(
            Attachment::new_inline(crate::report::LOGO_CID.to_string())
                .body(LOGO_PNG.to_vec(), ContentType::parse("image/png")?),
        );

    let mut parts = MultiPart::mixed().multipart(related);
    let mut total = 0usize;
    for f in files {
        let data = std::fs::read(f).with_context(|| format!("reading attachment '{}'", f.display()))?;
        total += data.len();
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        parts = parts.singlepart(Attachment::new(name).body(data, mime_for(f)));
    }
    if total * 4 / 3 > 9_000_000 {
        eprintln!(
            "   ⚠️  attachments total {} MiB — may exceed the SMTP size limit",
            total / 1_048_576
        );
    }
    send(cfg, subject, MailBody::Multi(parts))
}

/// Same as `send_report`, with files attached.
pub fn send_report_with_files(
    cfg: &MailCfg,
    subject: &str,
    body: &str,
    files: &[PathBuf],
) -> Result<()> {
    let mut parts = MultiPart::mixed().singlepart(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string()),
    );
    let mut total = 0usize;
    for f in files {
        let data = std::fs::read(f).with_context(|| format!("reading attachment '{}'", f.display()))?;
        total += data.len();
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        parts = parts.singlepart(Attachment::new(name).body(data, mime_for(f)));
    }
    // base64 inflates by ~4/3; most providers (SES included) cap messages at 10 MB
    if total * 4 / 3 > 9_000_000 {
        eprintln!(
            "   ⚠️  attachments total {} MiB — may exceed the SMTP size limit",
            total / 1_048_576
        );
    }
    send(cfg, subject, MailBody::Multi(parts))
}

pub fn send_report(cfg: &MailCfg, subject: &str, body: &str) -> Result<()> {
    send(cfg, subject, MailBody::Text(body.to_string()))
}

enum MailBody {
    Text(String),
    Multi(MultiPart),
}

fn send(cfg: &MailCfg, subject: &str, body: MailBody) -> Result<()> {
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
        .subject(subject);
    for to in &cfg.to {
        builder = builder.to(to.parse().with_context(|| format!("invalid recipient '{to}'"))?);
    }
    let email = match body {
        MailBody::Text(t) => builder.header(ContentType::TEXT_PLAIN).body(t)?,
        MailBody::Multi(m) => builder.multipart(m)?,
    };

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
