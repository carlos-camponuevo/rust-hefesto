use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub repo: Repo,
    #[serde(default = "default_exclude_folders")]
    pub exclude_folders: Vec<String>,
    #[serde(default = "default_exclude_subfolders")]
    pub exclude_subfolders: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    /// Azure DevOps organization, e.g. "BatDigitalI"
    pub organization: String,
    /// Azure DevOps project, e.g. "Data Bridge"
    pub project: String,
    /// Repository name, e.g. "devops-azcrpzanevla04"
    pub repository: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Name of the environment variable holding the PAT
    #[serde(default = "default_pat_env")]
    pub pat_env: String,
    /// Optional: read a local directory instead of downloading (testing)
    #[serde(default)]
    pub local_path: Option<String>,
}

fn default_branch() -> String {
    "main".into()
}
fn default_pat_env() -> String {
    "AZDO_PAT".into()
}
fn default_exclude_folders() -> Vec<String> {
    ["shared", "server", "xfiles"].map(String::from).to_vec()
}
fn default_exclude_subfolders() -> Vec<String> {
    ["config", "conf"].map(String::from).to_vec()
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file '{path}'"))?;
        let cfg: Config =
            serde_json::from_str(&raw).with_context(|| format!("invalid JSON in '{path}'"))?;
        Ok(cfg)
    }

    /// Build a config straight from an Azure DevOps git URL, defaults for
    /// the rest. Accepted forms:
    ///   https://dev.azure.com/{org}/{project}/_git/{repo}
    ///   https://{user}@dev.azure.com/{org}/{project}/_git/{repo}
    ///   git@ssh.dev.azure.com:v3/{org}/{project}/{repo}
    pub fn from_git_url(url: &str) -> Result<Self> {
        let (organization, project, repository) = parse_azdo_url(url)
            .with_context(|| format!("unrecognized Azure DevOps git URL: '{url}'"))?;
        Ok(Config {
            repo: Repo {
                organization,
                project,
                repository,
                branch: default_branch(),
                pat_env: default_pat_env(),
                local_path: None,
            },
            exclude_folders: default_exclude_folders(),
            exclude_subfolders: default_exclude_subfolders(),
        })
    }

    /// Hostname the repo is bound to: "devops-<host>" -> "<host>".
    /// Deploy is only allowed when the machine's short hostname matches.
    pub fn expected_hostname(&self) -> Option<String> {
        self.repo
            .repository
            .strip_prefix("devops-")
            .map(|h| h.to_ascii_lowercase())
    }
}

fn parse_azdo_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim().trim_end_matches('/');

    // SSH form: git@ssh.dev.azure.com:v3/{org}/{project}/{repo}
    if let Some(rest) = url.strip_prefix("git@ssh.dev.azure.com:v3/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if let [org, project, repo] = parts[..] {
            return Some((org.into(), percent_decode(project), repo.into()));
        }
        return None;
    }

    // HTTPS forms, with or without a user@ prefix
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = match rest.split_once('@') {
        Some((_user, host_path)) => host_path,
        None => rest,
    };
    let rest = rest.strip_prefix("dev.azure.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    // {org}/{project}/_git/{repo}
    if let [org, project, "_git", repo] = parts[..] {
        return Some((org.into(), percent_decode(project), percent_decode(repo)));
    }
    None
}

/// Decode %XX escapes (Azure DevOps URLs carry "Data%20Bridge").
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_url() {
        let (o, p, r) =
            parse_azdo_url("https://dev.azure.com/BatDigitalI/Data%20Bridge/_git/devops-azcrpzanevla04")
                .unwrap();
        assert_eq!((o.as_str(), p.as_str(), r.as_str()),
                   ("BatDigitalI", "Data Bridge", "devops-azcrpzanevla04"));
    }

    #[test]
    fn parses_https_url_with_user() {
        let (o, p, r) =
            parse_azdo_url("https://BatDigitalI@dev.azure.com/BatDigitalI/Data%20Bridge/_git/repo1/")
                .unwrap();
        assert_eq!((o.as_str(), p.as_str(), r.as_str()), ("BatDigitalI", "Data Bridge", "repo1"));
    }

    #[test]
    fn parses_ssh_url() {
        let (o, p, r) =
            parse_azdo_url("git@ssh.dev.azure.com:v3/BatDigitalI/Data%20Bridge/devops-x").unwrap();
        assert_eq!((o.as_str(), p.as_str(), r.as_str()), ("BatDigitalI", "Data Bridge", "devops-x"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_azdo_url("https://github.com/foo/bar").is_none());
        assert!(parse_azdo_url("not a url").is_none());
    }
}

/// Machine short hostname (first DNS label), lowercase.
pub fn short_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}
