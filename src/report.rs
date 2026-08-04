//! HTML e-mail reports: a branded header carrying the facts that matter
//! (project, status, image, digest), then the captured terminal output at
//! the end on a dark background.
//!
//! Written for mail clients, not browsers: tables and inline styles only,
//! no flexbox/grid, and the logo travels as an inline (cid:) attachment
//! because Gmail refuses data: URIs.

pub const LOGO_CID: &str = "hefesto-logo";

const DARK: &str = "#0d0d0d";
const GOLD: &str = "#c8973e";
const INK: &str = "#111827";
const MUTED: &str = "#6b7280";
const LINE: &str = "#e3e8f0";
const OK: &str = "#12703a";
const OK_BG: &str = "#e8f5ee";
const BAD: &str = "#b42318";
const BAD_BG: &str = "#fdecea";

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Shorten a digest for display: sha256:3a09bd90…c87a7df
pub fn short_digest(d: &str) -> String {
    let hex = d.strip_prefix("sha256:").unwrap_or(d);
    if hex.len() > 20 {
        format!("sha256:{}…{}", &hex[..10], &hex[hex.len() - 7..])
    } else {
        d.to_string()
    }
}

/// "bruat/pix" -> "BR UAT · bruat"; unknown patterns fall back to the folder.
pub fn env_label(dir: &str) -> String {
    let env = dir.split('/').next().unwrap_or("").to_lowercase();
    for suffix in ["prod", "uat", "dev", "qa"] {
        if let Some(market) = env.strip_suffix(suffix) {
            if market.len() == 2 {
                return format!("{} {} · {env}", market.to_uppercase(), suffix.to_uppercase());
            }
        }
    }
    env
}

pub fn status_pill(ok: bool, label: &str) -> String {
    let (fg, bg) = if ok { (OK, OK_BG) } else { (BAD, BAD_BG) };
    format!(
        "<span style=\"display:inline-block;padding:5px 14px;border-radius:14px;background:{bg};\
         color:{fg};font:700 12px/1.2 -apple-system,Segoe UI,Roboto,Arial,sans-serif;\
         letter-spacing:.08em;text-transform:uppercase\">{}</span>",
        esc(label)
    )
}

/// Two-column facts table.
pub fn facts(rows: &[(&str, String)]) -> String {
    let body: String = rows
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| {
            format!(
                "<tr>\
                 <td style=\"padding:7px 12px 7px 0;color:{MUTED};font:400 13px/1.5 -apple-system,Segoe UI,Roboto,Arial,sans-serif;\
                 white-space:nowrap;vertical-align:top;border-bottom:1px solid {LINE}\">{}</td>\
                 <td style=\"padding:7px 0;color:{INK};font:400 13px/1.5 -apple-system,Segoe UI,Roboto,Arial,sans-serif;\
                 vertical-align:top;border-bottom:1px solid {LINE};word-break:break-word\">{}</td></tr>",
                esc(k),
                v // callers pre-escape / may embed markup
            )
        })
        .collect();
    format!("<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%;border-collapse:collapse\">{body}</table>")
}

pub fn mono(s: &str) -> String {
    format!(
        "<span style=\"font:400 12px/1.5 SFMono-Regular,Consolas,Menlo,monospace;color:{INK}\">{}</span>",
        esc(s)
    )
}

/// Terminal-style block: the captured output, on black.
pub fn log_block(title: &str, log: &str) -> String {
    let body = if log.trim().is_empty() {
        "(no output captured)".to_string()
    } else {
        esc(log.trim_end())
    };
    format!(
        "<div style=\"margin:18px 0 0\">\
         <div style=\"font:600 12px/1.4 -apple-system,Segoe UI,Roboto,Arial,sans-serif;color:{MUTED};\
         letter-spacing:.06em;text-transform:uppercase;margin-bottom:6px\">{}</div>\
         <div style=\"background:{DARK};border-radius:6px;padding:14px 16px\">\
         <pre style=\"margin:0;color:#dfe3e8;font:400 11.5px/1.55 SFMono-Regular,Consolas,Menlo,monospace;\
         white-space:pre-wrap;word-break:break-word\">{}</pre></div></div>",
        esc(title),
        body
    )
}

/// One result card: title line with a status pill, then its facts.
pub fn card(title: &str, ok: bool, status: &str, inner: String) -> String {
    let accent = if ok { OK } else { BAD };
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%;border-collapse:separate;\
         border:1px solid {LINE};border-left:4px solid {accent};border-radius:6px;margin:0 0 14px\">\
         <tr><td style=\"padding:14px 16px\">\
         <table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%\"><tr>\
         <td style=\"font:600 15px/1.4 -apple-system,Segoe UI,Roboto,Arial,sans-serif;color:{INK};padding-bottom:10px\">{}</td>\
         <td align=\"right\" style=\"padding-bottom:10px\">{}</td></tr></table>{}\
         </td></tr></table>",
        esc(title),
        status_pill(ok, status),
        inner
    )
}

/// Full document: dark branded header, headline, body, footer.
pub fn document(kind: &str, headline: &str, ok: bool, status: &str, body: String) -> String {
    let host = crate::config::short_hostname();
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "<!doctype html><html><body style=\"margin:0;padding:0;background:#f4f6fa\">\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%;background:#f4f6fa\"><tr><td align=\"center\">\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%;max-width:720px;background:#ffffff;\
 border-radius:10px;overflow:hidden;margin:22px auto;box-shadow:0 1px 3px rgba(16,24,40,.08)\">\
  <tr><td style=\"background:{DARK};padding:12px 22px\">\
    <table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%\"><tr>\
      <td style=\"vertical-align:middle\"><img src=\"cid:{LOGO_CID}\" width=\"96\" alt=\"hefesto\" style=\"display:block;border:0;opacity:.85\"></td>\
      <td align=\"right\" style=\"vertical-align:middle;font:600 12px/1.4 -apple-system,Segoe UI,Roboto,Arial,sans-serif;\
        color:{GOLD};letter-spacing:.14em;text-transform:uppercase\">{}</td>\
    </tr></table></td></tr>\
  <tr><td style=\"padding:22px 22px 6px\">\
    <table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%\"><tr>\
      <td style=\"font:700 19px/1.35 -apple-system,Segoe UI,Roboto,Arial,sans-serif;color:{INK}\">{}</td>\
      <td align=\"right\">{}</td>\
    </tr></table></td></tr>\
  <tr><td style=\"padding:14px 22px 24px\">{}</td></tr>\
  <tr><td style=\"padding:14px 22px;border-top:1px solid {LINE};font:400 11.5px/1.5 -apple-system,Segoe UI,Roboto,Arial,sans-serif;color:{MUTED}\">\
    Generated by hefesto {version} on {} — everything is read from the deployment repository in memory; no plaintext is written to disk.\
  </td></tr>\
</table></td></tr></table></body></html>",
        esc(kind),
        esc(headline),
        status_pill(ok, status),
        body,
        esc(&host)
    )
}
