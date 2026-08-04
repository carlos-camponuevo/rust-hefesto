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

/// Mail must never hold up a build or deploy that already finished.
const SMTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// First proxy setting found in the environment, if any.
fn proxy_from_env() -> Option<String> {
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

/// SMTP through an HTTP proxy.
///
/// An HTTP proxy can only carry SMTP inside a CONNECT tunnel, which
/// `lettre` cannot open — so on proxied hosts the message is handed to
/// `curl`, which speaks CONNECT + STARTTLS + AUTH natively. The message
/// goes to a 0600 file in RAM (/dev/shm) and the credentials arrive on
/// curl's stdin as a config file, so the password never appears in the
/// process list.
fn send_via_curl(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    from: &str,
    to: &[String],
    raw: &[u8],
    proxy: &str,
) -> Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let msg_path = dir.join(format!("hefesto-mail-{}.eml", std::process::id()));
    {
        let mut f = std::fs::File::create(&msg_path)
            .with_context(|| format!("writing the message to {}", msg_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(raw)?;
    }

    let mut cfg = vec![
        format!("url = \"smtp://{host}:{port}\""),
        "ssl-reqd".to_string(),
        format!("mail-from = \"{from}\""),
        format!("upload-file = \"{}\"", msg_path.display()),
        format!("proxy = \"{proxy}\""),
        format!("max-time = {}", SMTP_TIMEOUT.as_secs() * 2),
        "silent".to_string(),
        "show-error".to_string(),
    ];
    for t in to {
        cfg.push(format!("mail-rcpt = \"{t}\""));
    }
    if !user.is_empty() {
        cfg.push(format!("user = \"{user}:{pass}\""));
    }

    let mut child = Command::new("curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("mail needs `curl` on proxied hosts — it is not installed")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(cfg.join("\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let _ = std::fs::remove_file(&msg_path);

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "curl could not deliver through the proxy {proxy} to {host}:{port}: {}",
            err.trim()
        );
    }
    Ok(())
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

    // Behind an HTTP proxy, SMTP only travels inside a CONNECT tunnel —
    // hand the job to curl, which does that natively.
    if let Some(proxy) = proxy_from_env() {
        let port: u16 = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);
        eprintln!("   via proxy {proxy} → {host}:{port}");
        let started = std::time::Instant::now();
        send_via_curl(&host, port, &user, &pass, &cfg.from, &cfg.to, &email.formatted(), &proxy)?;
        eprintln!(
            "📧 report mailed to {} ({}s, through the proxy)",
            cfg.to.join(", "),
            started.elapsed().as_secs()
        );
        return Ok(());
    }

    let mailer = if !user.is_empty() && !pass.is_empty() {
        SmtpTransport::starttls_relay(&host)?
            .credentials(Credentials::new(user, pass))
            // Without a timeout a blocked port (no route, firewall dropping
            // packets) leaves hefesto waiting on the TCP handshake for
            // minutes and the run looks frozen after the work succeeded.
            .timeout(Some(SMTP_TIMEOUT))
            .build()
    } else {
        // unauthenticated internal relay
        SmtpTransport::builder_dangerous(&host)
            .port(25)
            .timeout(Some(SMTP_TIMEOUT))
            .build()
    };
    let started = std::time::Instant::now();
    mailer.send(&email).with_context(|| {
        format!(
            "sending mail via {host} (gave up after {}s — is the SMTP port reachable from this host?)",
            started.elapsed().as_secs()
        )
    })?;
    eprintln!(
        "📧 report mailed to {} ({}s)",
        cfg.to.join(", "),
        started.elapsed().as_secs()
    );
    Ok(())
}
