use crate::config::Repo;
use anyhow::{Context, Result, bail};
use base64::Engine;

/// Ask git's credential system (store/cache/manager — whatever this
/// machine uses for `git pull`) for the password saved for `host`.
/// Prompting is disabled so a machine without stored creds fails fast.
fn pat_from_git_credentials(host: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true") // "true" binary: returns empty, never prompts
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())
        .ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("password=").map(str::to_string))
        .filter(|p| !p.is_empty())
}

/// Download the repository as a zip archive, entirely into memory.
pub fn download_repo_zip(repo: &Repo) -> Result<Vec<u8>> {
    if repo.provider == "github" {
        return download_github_zip(repo);
    }
    download_azdo_zip(repo)
}

/// GitHub: GET api.github.com/repos/{owner}/{repo}/zipball/{branch}.
/// Token from the config's pat_env, then GITHUB_TOKEN, then git stored
/// credentials; anonymous works for public repos.
fn download_github_zip(repo: &Repo) -> Result<Vec<u8>> {
    let token = std::env::var(&repo.pat_env)
        .ok()
        .filter(|v| !v.is_empty() && repo.pat_env != "AZDO_PAT")
        .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|v| !v.is_empty()))
        .or_else(|| pat_from_git_credentials("github.com"));

    let url = format!(
        "https://api.github.com/repos/{}/{}/zipball/{}",
        repo.organization, repo.repository, repo.branch
    );
    eprintln!(
        "⬇️  downloading {}/{}@{} from GitHub (in-memory{})...",
        repo.organization,
        repo.repository,
        repo.branch,
        if token.is_some() { ", authenticated" } else { ", anonymous" }
    );
    let mut req = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(&url)
        .header("User-Agent", "hefesto");
    if let Some(t) = &token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().context("request to GitHub failed")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        bail!(
            "GitHub returned {status}: {} (private repo needs GITHUB_TOKEN or git stored credentials)",
            body.chars().take(200).collect::<String>()
        );
    }
    let bytes = resp.bytes().context("reading zip body")?;
    eprintln!("   {} KiB received", bytes.len() / 1024);
    Ok(bytes.to_vec())
}

/// Azure DevOps: GET .../_apis/git/repositories/{repo}/items?$format=zip
fn download_azdo_zip(repo: &Repo) -> Result<Vec<u8>> {
    let pat = match std::env::var(&repo.pat_env) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!(
                "   ({} not set — trying git stored credentials for dev.azure.com)",
                repo.pat_env
            );
            pat_from_git_credentials("dev.azure.com").with_context(|| {
                format!(
                    "no credentials: set {} (PAT with Code:Read) or store git \
                     credentials for dev.azure.com (git config credential.helper store + one git pull)",
                    repo.pat_env
                )
            })?
        }
    };

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
