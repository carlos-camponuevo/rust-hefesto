use crate::config::Repo;
use anyhow::{Context, Result, bail};
use base64::Engine;

/// Download the repository as a zip archive, entirely into memory.
/// Azure DevOps: GET .../_apis/git/repositories/{repo}/items?$format=zip
pub fn download_repo_zip(repo: &Repo) -> Result<Vec<u8>> {
    let pat = std::env::var(&repo.pat_env).with_context(|| {
        format!(
            "PAT env var '{}' is not set (personal access token with Code:Read)",
            repo.pat_env
        )
    })?;

    let url = format!(
        "https://dev.azure.com/{}/{}/_apis/git/repositories/{}/items",
        urlencode(&repo.organization),
        urlencode(&repo.project),
        urlencode(&repo.repository),
    );
    let auth = base64::engine::general_purpose::STANDARD.encode(format!(":{pat}"));

    eprintln!(
        "⬇️  downloading {}@{} from Azure DevOps (in-memory)...",
        repo.repository, repo.branch
    );
    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(&url)
        .query(&[
            ("scopePath", "/"),
            ("download", "true"),
            ("$format", "zip"),
            ("versionDescriptor.version", repo.branch.as_str()),
            ("versionDescriptor.versionType", "branch"),
            ("resolveLfs", "true"),
            ("api-version", "7.1"),
        ])
        .header("Authorization", format!("Basic {auth}"))
        .send()
        .context("request to Azure DevOps failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        bail!(
            "Azure DevOps returned {status}: {}",
            body.chars().take(300).collect::<String>()
        );
    }
    let bytes = resp.bytes().context("reading zip body")?;
    eprintln!("   {} KiB received", bytes.len() / 1024);
    Ok(bytes.to_vec())
}

/// Minimal percent-encoding for URL path segments (spaces etc.).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
