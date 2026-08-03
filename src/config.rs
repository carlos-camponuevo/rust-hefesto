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
    /// Optional: mail build reports. SMTP credentials come from env vars.
    #[serde(default)]
    pub mail: Option<MailCfg>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailCfg {
    pub to: Vec<String>,
    #[serde(default = "default_mail_from")]
    pub from: String,
    /// Env var names (values never live in config files).
    #[serde(default = "default_smtp_host_env")]
    pub smtp_host_env: String,
    #[serde(default = "default_smtp_user_env")]
    pub smtp_user_env: String,
    #[serde(default = "default_smtp_pass_env")]
    pub smtp_pass_env: String,
}

fn default_mail_from() -> String {
    "noreply@ipremios.com".into()
}

/// Fallback for configs without a `mail` block (e.g. `-git` mode):
/// HEFESTO_MAIL_TO="a@x.com,b@y.com" enables reports with defaults.
pub fn mail_from_env() -> Option<MailCfg> {
    let to: Vec<String> = std::env::var("HEFESTO_MAIL_TO")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if to.is_empty() {
        return None;
    }
    Some(MailCfg {
        to,
        from: default_mail_from(),
        smtp_host_env: default_smtp_host_env(),
        smtp_user_env: default_smtp_user_env(),
        smtp_pass_env: default_smtp_pass_env(),
    })
}
/// MailCfg with default sender/SMTP env names for an ad-hoc recipient list
/// (used by build.yml mailGroups routing).
pub fn mailcfg_for(to: Vec<String>) -> MailCfg {
    MailCfg {
        to,
        from: default_mail_from(),
        smtp_host_env: default_smtp_host_env(),
        smtp_user_env: default_smtp_user_env(),
        smtp_pass_env: default_smtp_pass_env(),
    }
}

fn default_smtp_host_env() -> String {
    "SMTP_HOST".into()
}
fn default_smtp_user_env() -> String {
    "SMTP_USER".into()
}
fn default_smtp_pass_env() -> String {
    "SMTP_PASS".into()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    /// Alternative to organization/project/repository: the repo URL as
    /// you'd pass to `-git`. When present it fills the three fields.
    #[serde(default)]
    pub url: Option<String>,
    /// "azdo" (default) or "github" — set automatically when `url` is given.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Azure DevOps organization, e.g. "ExampleOrg"
    #[serde(default)]
    pub organization: String,
    /// Azure DevOps project, e.g. "Example Project"
    #[serde(default)]
    pub project: String,
    /// Repository name, e.g. "devops-server01"
    #[serde(default)]
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
fn default_provider() -> String {
    "azdo".into()
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
    /// Load a config file, transparently handling ed.sh-style encryption
    /// (OpenSSL `Salted__` header). Returns the config and, when the file
    /// was encrypted, the key that opened it — callers reuse it for the
    /// repo decryption so the user is prompted only once.
    pub fn load(path: &str) -> Result<(Self, Option<zeroize::Zeroizing<String>>)> {
        // allow `hefesto.json` to silently mean `hefesto.json.enc`
        let actual = if !std::path::Path::new(path).exists()
            && std::path::Path::new(&format!("{path}.enc")).exists()
        {
            format!("{path}.enc")
        } else {
            path.to_string()
        };
        let raw = std::fs::read(&actual)
            .with_context(|| format!("cannot read config file '{actual}'"))?;

        let (json, key) = if crate::vault::looks_encrypted(&raw) {
            eprintln!("🔐 config '{actual}' is encrypted");
            let key = zeroize::Zeroizing::new(
                inquire::Password::new("Config decrypt key:")
                    .without_confirmation()
                    .prompt()?,
            );
            let plain = crate::vault::decrypt_openssl(&raw, key.as_bytes())
                .context("could not decrypt the config (wrong key?)")?;
            (plain, Some(key))
        } else {
            (raw, None)
        };

        // YAML parser accepts JSON too (JSON ⊂ YAML) — one parser, both formats
        let mut cfg: Config = serde_yaml::from_slice(&json)
            .with_context(|| format!("invalid YAML/JSON in '{actual}'"))?;
        cfg.resolve_repo_url()?;
        Ok((cfg, key))
    }

    /// First existing default config: hefesto.{yml,yaml,json}[.enc].
    pub fn default_path() -> String {
        for p in ["hefesto.yml", "hefesto.yaml", "hefesto.json"] {
            if std::path::Path::new(p).exists()
                || std::path::Path::new(&format!("{p}.enc")).exists()
            {
                return p.to_string();
            }
        }
        "hefesto.json".to_string()
    }

    /// If repo.url is set, derive provider + organization/project/repository.
    fn resolve_repo_url(&mut self) -> Result<()> {
        if let Some(url) = &self.repo.url {
            let (provider, o, p, r) = parse_git_url(url)
                .with_context(|| format!("repo.url is not a recognized git URL: '{url}'"))?;
            self.repo.provider = provider;
            self.repo.organization = o;
            self.repo.project = p;
            self.repo.repository = r;
        }
        anyhow::ensure!(
            !self.repo.repository.is_empty() && !self.repo.organization.is_empty(),
            "config needs either repo.url or repo.organization/repository"
        );
        anyhow::ensure!(
            self.repo.provider != "azdo" || !self.repo.project.is_empty(),
            "Azure DevOps repos also need repo.project"
        );
        Ok(())
    }

    /// Build a config straight from a git URL, defaults for the rest.
    /// Accepted forms:
    ///   https://dev.azure.com/{org}/{project}/_git/{repo}   (+ user@ / ssh v3)
    ///   https://github.com/{owner}/{repo}[.git]             (+ git@github.com:)
    pub fn from_git_url(url: &str) -> Result<Self> {
        let (provider, organization, project, repository) =
            parse_git_url(url).with_context(|| format!("unrecognized git URL: '{url}'"))?;
        Ok(Config {
            repo: Repo {
                url: None,
                provider,
                organization,
                project,
                repository,
                branch: default_branch(),
                pat_env: default_pat_env(),
                local_path: None,
            },
            exclude_folders: default_exclude_folders(),
            exclude_subfolders: default_exclude_subfolders(),
            mail: None,
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

/// Parse any supported git URL -> (provider, organization, project, repo).
/// GitHub has no "project" level — it comes back empty.
pub fn parse_git_url(url: &str) -> Option<(String, String, String, String)> {
    let u = url.trim().trim_end_matches('/');
    // GitHub SSH: git@github.com:owner/repo(.git)
    if let Some(rest) = u.strip_prefix("git@github.com:") {
        let (owner, repo) = rest.split_once('/')?;
        return Some((
            "github".into(),
            owner.to_string(),
            String::new(),
            repo.trim_end_matches(".git").to_string(),
        ));
    }
    // GitHub HTTPS: https://github.com/owner/repo(.git)
    let no_scheme = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"));
    if let Some(rest) = no_scheme {
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        if let Some(path) = rest.strip_prefix("github.com/") {
            let parts: Vec<&str> = path.split('/').collect();
            if let [owner, repo] = parts[..] {
                return Some((
                    "github".into(),
                    owner.to_string(),
                    String::new(),
                    repo.trim_end_matches(".git").to_string(),
                ));
            }
            return None;
        }
    }
    // Azure DevOps
    parse_azdo_url(url).map(|(o, p, r)| ("azdo".into(), o, p, r))
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

/// Decode %XX escapes (Azure DevOps URLs carry "Example%20Project").
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
            parse_azdo_url("https://dev.azure.com/ExampleOrg/Example%20Project/_git/devops-server01")
                .unwrap();
        assert_eq!((o.as_str(), p.as_str(), r.as_str()),
                   ("ExampleOrg", "Example Project", "devops-server01"));
    }

    #[test]
    fn parses_https_url_with_user() {
        let (o, p, r) =
            parse_azdo_url("https://ExampleOrg@dev.azure.com/ExampleOrg/Example%20Project/_git/repo1/")
                .unwrap();
        assert_eq!((o.as_str(), p.as_str(), r.as_str()), ("ExampleOrg", "Example Project", "repo1"));
    }

    #[test]
    fn parses_ssh_url() {
        let (o, p, r) =
            parse_azdo_url("git@ssh.dev.azure.com:v3/ExampleOrg/Example%20Project/devops-x").unwrap();
        assert_eq!((o.as_str(), p.as_str(), r.as_str()), ("ExampleOrg", "Example Project", "devops-x"));
    }

    #[test]
    fn parses_github_urls() {
        let (prov, o, p, r) =
            parse_git_url("https://github.com/my-org/devops-azcrpronevla03.git").unwrap();
        assert_eq!((prov.as_str(), o.as_str(), p.as_str(), r.as_str()),
                   ("github", "my-org", "", "devops-azcrpronevla03"));
        let (prov, o, _, r) = parse_git_url("git@github.com:my-org/rust-hefesto.git").unwrap();
        assert_eq!((prov.as_str(), o.as_str(), r.as_str()), ("github", "my-org", "rust-hefesto"));
        // azdo still works through the same entry point
        let (prov, ..) = parse_git_url("https://ExampleOrg@dev.azure.com/ExampleOrg/BatDevops/_git/devops-azcrnbrnevta19").unwrap();
        assert_eq!(prov, "azdo");
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
