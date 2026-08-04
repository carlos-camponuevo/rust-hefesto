//! Milestone 3 — build engine.
//!
//! A stack's build definition comes from either:
//!   1. `<env>/<stack>/build.yml`   (preferred, overrides everything), or
//!   2. `<env>/<stack>/build.sh`    (legacy fallback: the `repoList=(...)`
//!      entries "org,project,repo,image,branch,tag" are parsed directly, so
//!      hefesto works on existing stacks with zero new files).
//!
//! Mirrors the three steps of the legacy build.sh:
//!   1. the repo/build list           -> `builds:`
//!   2. registry login (hub or ghcr)  -> `destinations:` + env-var creds
//!   3. build & push each repo        -> in-memory context -> docker build
//!
//! Each build: download the app repo (Azure DevOps zip) into RAM, convert
//! it to a tar stream, and pipe it into `docker build -` with stdio
//! inherited — the user watches the real build output live. Push streams
//! the same way.

use crate::config::Config;
use crate::remote;
use crate::vault::MemFs;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Outcome of one build, for on-screen summary and mailed reports.
pub struct BuildReport {
    pub image: String,
    pub name: String,
    pub source: String,
    pub platform: String,
    /// digest the registry returned on push (or the local image id)
    pub digest: String,
    pub pushed: bool,
    pub ok: bool,
    pub duration_secs: u64,
    /// captured docker output (kept to the last `LOG_KEEP_LINES` lines)
    pub log: String,
}

/// Pull the image digest out of docker output: `push` prints
/// "tag: digest: sha256:… size: …", `build` prints "writing image sha256:…".
fn digest_from_log(log: &str) -> String {
    for needle in ["digest: sha256:", "writing image sha256:"] {
        if let Some(i) = log.rfind(needle) {
            let rest = &log[i + needle.len() - "sha256:".len()..];
            let hex: String = rest
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == ':' || *c == 's' || *c == 'h' || *c == 'a' || *c == '2' || *c == '5' || *c == '6')
                .collect();
            let hex = hex.trim_end_matches(|c: char| !c.is_ascii_hexdigit()).to_string();
            if hex.len() > 20 {
                return hex;
            }
        }
    }
    String::new()
}

const LOG_KEEP_LINES: usize = 400;

/// Strip ANSI escapes and turn carriage-return progress updates into
/// plain lines, so a captured log is readable in an e-mail.
fn clean_for_report(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI ... final byte in @-~, or a two-char sequence
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('@'..='~').contains(&c2) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n'); // in-place update -> its own line
                }
            }
            c => out.push(c),
        }
    }
    // collapse the runs of identical consecutive lines a progress display leaves behind
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines() {
        if lines.last().map(|p: &&str| p.trim_end() == l.trim_end()) != Some(true) {
            lines.push(l);
        }
    }
    lines.join("\n")
}

/// Allocate a pseudo-terminal so the child believes it is talking to a
/// real terminal. Docker only renders its live, in-place progress display
/// when stdout is a TTY; through a plain pipe it repeats static lines.
#[cfg(unix)]
fn open_pty() -> Option<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let (mut master, mut slave) = (0i32, 0i32);
    // SAFETY: openpty writes two valid fds or returns non-zero.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return None;
    }
    unsafe {
        Some((
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        ))
    }
}

/// Run a command streaming its output live to the terminal AND capturing
/// it. `stdin_data` (the tar context / compose file) is fed from a thread
/// to avoid pipe-buffer deadlocks with chatty children.
pub fn run_tee_cmd(cmd: Command, stdin_data: Option<Vec<u8>>) -> Result<(bool, String)> {
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        // Only worth a pty when our own stdout is a terminal; under cron or
        // a pipe the plain, line-based output is what you want anyway.
        if std::io::stdout().is_terminal() {
            if let Some(pty) = open_pty() {
                return run_tee_pty(cmd, stdin_data, pty);
            }
        }
    }
    run_tee_piped(cmd, stdin_data)
}

/// TTY-backed variant: the child writes to a pty, we mirror those bytes
/// to our own stdout untouched (so progress redraws work) and keep a copy.
#[cfg(unix)]
fn run_tee_pty(
    mut cmd: Command,
    stdin_data: Option<Vec<u8>>,
    pty: (std::fs::File, std::fs::File),
) -> Result<(bool, String)> {
    use std::io::{Read, Write};
    let (mut master, slave) = pty;
    let slave_out = slave.try_clone().context("cloning the pty slave")?;
    let slave_err = slave.try_clone().context("cloning the pty slave")?;
    cmd.stdout(Stdio::from(slave_out));
    cmd.stderr(Stdio::from(slave_err));
    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().context("failed to run `docker` — is it installed?")?;
    // BOTH the Command and our own handle keep pty slave fds open. Until
    // every parent-side copy is closed, reading the master blocks forever
    // after the child exits — the process looks frozen mid-build.
    drop(cmd);
    drop(slave);
    let stdin_handle = stdin_data.map(|data| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        })
    });

    let mut captured: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut out = std::io::stdout();
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            // EIO is how Linux reports "the slave side closed"
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(_) => break,
            Ok(n) => {
                let _ = out.write_all(&buf[..n]);
                let _ = out.flush();
                captured.extend_from_slice(&buf[..n]);
            }
        }
    }
    if let Some(h) = stdin_handle {
        let _ = h.join();
    }
    let status = child.wait()?;
    let log = clean_for_report(&String::from_utf8_lossy(&captured));
    Ok((status.success(), tail_lines(&log, LOG_KEEP_LINES)))
}

fn run_tee_piped(mut cmd: Command, stdin_data: Option<Vec<u8>>) -> Result<(bool, String)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().context("failed to run `docker` — is it installed?")?;

    let stdin_handle = stdin_data.map(|data| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data); // drop closes the pipe
        })
    });
    let out = child.stdout.take().expect("piped stdout");
    let err = child.stderr.take().expect("piped stderr");
    let t_out = std::thread::spawn(move || {
        let mut buf = String::new();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            println!("{line}");
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = String::new();
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            eprintln!("{line}");
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    let status = child.wait()?;
    if let Some(h) = stdin_handle {
        let _ = h.join();
    }
    let mut log = t_out.join().unwrap_or_default();
    log.push_str(&t_err.join().unwrap_or_default());
    Ok((status.success(), tail_lines(&clean_for_report(&log), LOG_KEEP_LINES)))
}

/// Plain-text report for mails / logs.
pub fn report_body_refs(dir: &str, reports: &[&BuildReport]) -> String {
    let mut b = String::new();
    let ok = reports.iter().filter(|r| r.ok).count();
    b.push_str(&format!(
        "hefesto build report — {dir}\n{ok}/{} builds succeeded\n\n",
        reports.len()
    ));
    for r in reports {
        b.push_str(&format!(
            "{} {}\n    source:   {}\n    duration: {}s\n",
            if r.ok { "✅" } else { "❌" },
            r.image,
            r.source,
            r.duration_secs
        ));
    }
    for r in reports {
        b.push_str(&format!("\n===== log: {} =====\n{}\n", r.image, r.log));
    }
    b
}

/// HTML e-mail body for a set of builds: header facts first, logs last.
pub fn report_html_refs(dir: &str, reports: &[&BuildReport]) -> String {
    use crate::report::*;
    let ok_n = reports.iter().filter(|r| r.ok).count();
    let all_ok = ok_n == reports.len();
    let total: u64 = reports.iter().map(|r| r.duration_secs).sum();

    let mut body = String::new();
    let stack = dir.rsplit('/').next().unwrap_or(dir);
    body.push_str(&facts(&[
        ("Environment", esc(&env_label(dir))),
        ("Stack", format!("<b>{}</b>", esc(stack))),
        ("Result", format!("<b>{ok_n} of {}</b> image{} built", reports.len(),
                           if reports.len() == 1 { "" } else { "s" })),
        ("Duration", format!("{}m {}s", total / 60, total % 60)),
    ]));

    for r in reports {
        let mut inner = vec![
            ("Image", mono(&r.image)),
            ("Source", esc(&r.source)),
            ("Platform", esc(&r.platform)),
        ];
        if !r.digest.is_empty() {
            inner.push(("Digest", format!("{}{}", mono(&short_digest(&r.digest)),
                if r.pushed { " <span style=\"color:#12703a;font:600 11px/1 -apple-system,Arial,sans-serif\">pushed</span>" } else { "" })));
        }
        inner.push(("Duration", format!("{}s", r.duration_secs)));
        body.push_str(&card(
            &r.name,
            r.ok,
            if r.ok { "success" } else { "failed" },
            facts(&inner.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>()),
        ));
    }
    for r in reports {
        body.push_str(&log_block(&format!("log — {}", r.image), &r.log));
    }
    document(
        "Build report",
        dir,
        all_ok,
        if all_ok { "success" } else { "failed" },
        body,
    )
}

fn tail_lines(s: &str, keep: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= keep {
        return s.to_string();
    }
    format!(
        "... ({} earlier lines omitted)\n{}\n",
        lines.len() - keep,
        lines[lines.len() - keep..].join("\n")
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildFile {
    /// Push destinations, by name. Each build entry picks one via
    /// `destination:` (defaults to the single/first entry).
    #[serde(default = "default_destinations")]
    pub destinations: BTreeMap<String, Destination>,
    /// Target platform for every build (overridable per entry). The swarm
    /// nodes are x86_64, so this defaults to linux/amd64 — building on an
    /// ARM host then requires QEMU binfmt, but NEVER silently produces an
    /// image the servers cannot run.
    #[serde(default = "default_platform")]
    pub default_platform: String,
    /// Named recipient groups for build reports. Entries opt in via
    /// `mailGroup:`; entries without one send NO mail.
    #[serde(default)]
    pub mail_groups: BTreeMap<String, Vec<String>>,
    /// v2 schema (preferred name).
    #[serde(default)]
    pub repo_list: Vec<BuildSpec>,
    /// v1 schema (still accepted).
    #[serde(default)]
    pub builds: Vec<BuildSpec>,
    /// True when this list came from the ENVIRONMENT folder rather than the
    /// stack folder — i.e. one shared catalog for all stacks of the env.
    /// "Build whole stack" then builds only the catalog entries whose image
    /// the stack's compose actually uses.
    #[serde(skip)]
    pub catalog: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    /// "docker.io" (default) or "ghcr.io".
    #[serde(default = "default_registry_host")]
    pub host: String,
    /// Registry namespace — the path segment in the image reference.
    /// ALWAYS lowercase: Docker/GHCR require lowercase repository names.
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Login user for `docker login`. CASE-SENSITIVE and often different
    /// from the namespace (e.g. namespace `my-org`, user `My-Org`).
    /// Not a secret, so it can live in the config; the token never does.
    #[serde(default)]
    pub user: Option<String>,
    /// Env vars holding the docker login credentials (user + PAT).
    /// Defaults depend on the host: DOCKER_USER/DOCKER_PAT for docker.io,
    /// GHCR_USER/GHCR_PAT for ghcr.io.
    #[serde(default)]
    pub user_env: Option<String>,
    #[serde(default)]
    pub pat_env: Option<String>,
}

impl Destination {
    pub fn user_env(&self) -> String {
        self.user_env.clone().unwrap_or_else(|| {
            if self.host == "ghcr.io" { "GHCR_USER".into() } else { "DOCKER_USER".into() }
        })
    }
    pub fn pat_env(&self) -> String {
        self.pat_env.clone().unwrap_or_else(|| {
            if self.host == "ghcr.io" { "GHCR_PAT".into() } else { "DOCKER_PAT".into() }
        })
    }
    pub fn image_ref(&self, image: &str, tag: &str) -> String {
        if self.host == "docker.io" {
            format!("{}/{image}:{tag}", self.namespace)
        } else {
            format!("{}/{}/{image}:{tag}", self.host, self.namespace)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSpec {
    /// Friendly application name, used in menus and reports.
    #[serde(default)]
    pub name: Option<String>,
    /// Source repo URL (Azure DevOps or GitHub) — replaces the
    /// organization/project/repository triple when present.
    #[serde(default)]
    pub repo_url: Option<String>,
    /// FUTURE: when set, the repo will be fetched via full `git clone`
    /// instead of a zip snapshot. Accepted in the schema today, not
    /// implemented yet.
    #[serde(default)]
    pub repo_clone_url: Option<String>,
    /// Which mailGroups entry receives this build's report. None = no mail.
    #[serde(default)]
    pub mail_group: Option<String>,
    /// `false` documents the image's provenance (it appears in the runbook)
    /// while keeping it OUT of builds — the YAML equivalent of a
    /// commented-out repoList line in the legacy build.sh.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Azure DevOps organization; defaults to the devops repo's own org.
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub repository: String,
    /// Image name; defaults to the repository name.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default = "default_branch")]
    pub branch: String,
    pub tag: String,
    /// Which destination to push to; defaults to the first destination.
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    #[serde(default = "default_push")]
    pub push: bool,
    /// Per-entry platform override (e.g. "linux/arm64").
    #[serde(default)]
    pub platform: Option<String>,
    /// Local directory as build context instead of a repo download (tests).
    #[serde(default)]
    pub local_path: Option<String>,
}

fn default_namespace() -> String {
    "camponuevo".into()
}
fn default_registry_host() -> String {
    "docker.io".into()
}
fn default_destinations() -> BTreeMap<String, Destination> {
    BTreeMap::from([(
        "dockerhub".to_string(),
        Destination {
            host: default_registry_host(),
            namespace: default_namespace(),
            user: None,
            user_env: None,
            pat_env: None,
        },
    )])
}
fn default_branch() -> String {
    "main".into()
}
fn default_dockerfile() -> String {
    "Dockerfile".into()
}
fn default_push() -> bool {
    true
}
fn default_enabled() -> bool {
    true
}
fn default_platform() -> String {
    "linux/amd64".into()
}

impl BuildFile {
    /// The active build list: v2 `repoList` wins, else v1 `builds`.
    pub fn entries(&self) -> &[BuildSpec] {
        if !self.repo_list.is_empty() {
            &self.repo_list
        } else {
            &self.builds
        }
    }

    pub fn destination_for<'a>(&'a self, spec: &BuildSpec) -> Result<(&'a str, &'a Destination)> {
        match &spec.destination {
            Some(name) => self
                .destinations
                .get_key_value(name.as_str())
                .map(|(k, v)| (k.as_str(), v))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "build '{}' wants destination '{name}' but destinations are: {}",
                        spec.image_name(),
                        self.destinations.keys().cloned().collect::<Vec<_>>().join(", ")
                    )
                }),
            None => self
                .destinations
                .iter()
                .next()
                .map(|(k, v)| (k.as_str(), v))
                .ok_or_else(|| anyhow::anyhow!("no destinations defined")),
        }
    }
}

impl BuildSpec {
    /// Image name: explicit `image:` > repository name > repoUrl basename.
    pub fn image_name(&self) -> String {
        if let Some(i) = &self.image {
            return i.clone();
        }
        if !self.repository.is_empty() {
            return self.repository.clone();
        }
        self.repo_url
            .as_deref()
            .and_then(|u| crate::config::parse_git_url(u))
            .map(|(_, _, _, r)| r)
            .unwrap_or_default()
    }

    /// Friendly name for menus and reports.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.image_name())
    }

    /// Resolve the source repository (repoUrl wins; else the azdo triple,
    /// org defaulting to the devops repo's own org).
    pub fn source_repo(&self, cfg: &Config) -> Result<crate::config::Repo> {
        let (provider, organization, project, repository) = match &self.repo_url {
            Some(u) => crate::config::parse_git_url(u)
                .with_context(|| format!("build '{}': bad repoUrl '{u}'", self.display_name()))?,
            None => {
                anyhow::ensure!(
                    !self.repository.is_empty() && !self.project.is_empty(),
                    "build '{}' needs repoUrl or project+repository",
                    self.display_name()
                );
                (
                    "azdo".to_string(),
                    self.organization
                        .clone()
                        .unwrap_or_else(|| cfg.repo.organization.clone()),
                    self.project.clone(),
                    self.repository.clone(),
                )
            }
        };
        Ok(crate::config::Repo {
            url: None,
            provider,
            organization,
            project,
            repository,
            branch: self.branch.clone(),
            pat_env: cfg.repo.pat_env.clone(),
            local_path: None,
        })
    }
}

/// Step 2 of the legacy flow: registry login. Credentials are REQUIRED
/// from env vars (user + PAT) — never stored in files.
pub fn registry_login(name: &str, dest: &Destination) -> Result<()> {
    let (user_env, pat_env) = (dest.user_env(), dest.pat_env());
    // config `user:` wins; otherwise fall back to the env var
    let user = dest
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| std::env::var(&user_env).unwrap_or_default());
    let pat = std::env::var(&pat_env).unwrap_or_default();
    if user.is_empty() {
        bail!(
            "destination '{name}' ({}) has no login user: add `user:` to the destination \
             or export {user_env}",
            dest.host
        );
    }
    if pat.is_empty() {
        bail!(
            "destination '{name}' ({}) needs a token:\n  export {pat_env}='<personal access token>'",
            dest.host
        );
    }
    eprintln!("🔑 docker login {} as {user} [destination: {name}]", dest.host);
    let mut child = Command::new("docker")
        .args(["login", &dest.host, "-u", &user, "--password-stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run `docker login`")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(pat.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("docker login to {} FAILED (exit {status})", dest.host);
    }
    Ok(())
}

/// Load the stack's build definition, in order:
///   1. <dir>/build.yml                (stack-level list)
///   2. <env>/build.yml                (one catalog shared by the env's stacks)
///   3. <dir>/build.sh                 (legacy repoList)
/// `dir` is the stack folder ("zauat/admin" or a root stack like "system").
pub fn load(fs: &MemFs, dir: &str) -> Result<Option<BuildFile>> {
    if let Some(raw) = fs.get(&format!("{dir}/build.yml")) {
        let bf: BuildFile = serde_yaml::from_slice(raw)
            .with_context(|| format!("{dir}/build.yml is invalid"))?;
        return Ok(Some(bf));
    }
    if let Some((env, _)) = dir.rsplit_once('/') {
        if let Some(raw) = fs.get(&format!("{env}/build.yml")) {
            let mut bf: BuildFile = serde_yaml::from_slice(raw)
                .with_context(|| format!("{env}/build.yml is invalid"))?;
            bf.catalog = true;
            return Ok(Some(bf));
        }
    }
    if let Some(raw) = fs.get(&format!("{dir}/build.sh")) {
        let text = String::from_utf8_lossy(raw);
        let builds = parse_legacy_repo_list(&text);
        if !builds.is_empty() {
            // which creds script the legacy build.sh sources tells us the
            // target registry: build_dockerhub_creds_ghcr.sh => ghcr.io
            let host = if text.contains("_ghcr") {
                "ghcr.io".to_string()
            } else {
                default_registry_host()
            };
            let name = if host == "ghcr.io" { "ghcr" } else { "dockerhub" };
            return Ok(Some(BuildFile {
                destinations: BTreeMap::from([(
                    name.to_string(),
                    Destination {
                        host,
                        namespace: default_namespace(),
                        user: None,
                        user_env: None,
                        pat_env: None,
                    },
                )]),
                default_platform: default_platform(),
                mail_groups: BTreeMap::new(),
                repo_list: Vec::new(),
                builds,
                catalog: false,
            }));
        }
    }
    Ok(None)
}

/// Parse legacy `repoList=( "org,project,repo,image,branch,tag" ... )`.
pub fn parse_legacy_repo_list(script: &str) -> Vec<BuildSpec> {
    let mut out = Vec::new();
    let mut in_list = false;
    for line in script.lines() {
        let t = line.trim();
        if t.starts_with("repoList=(") {
            in_list = true;
        }
        if !in_list {
            continue;
        }
        // a commented-out entry is a DISABLED build — never import it
        if t.starts_with('#') {
            continue;
        }
        for quoted in t.split('"').skip(1).step_by(2) {
            let f: Vec<&str> = quoted.split(',').map(str::trim).collect();
            if f.len() == 6 {
                out.push(BuildSpec {
                    name: None,
                    repo_url: None,
                    repo_clone_url: None,
                    mail_group: None,
                    organization: Some(f[0].to_string()),
                    project: f[1].to_string(),
                    repository: f[2].to_string(),
                    image: if f[3].is_empty() {
                        None
                    } else {
                        Some(f[3].to_string())
                    },
                    branch: f[4].to_string(),
                    tag: f[5].to_string(),
                    destination: None,
                    dockerfile: default_dockerfile(),
                    args: Default::default(),
                    push: default_push(),
                    enabled: default_enabled(),
                    platform: None,
                    local_path: None,
                });
            }
        }
        if t.ends_with(')') && in_list {
            break;
        }
    }
    out
}

/// Basename of a compose `image:` ref — "my-user/admin-api:uat.latest"
/// => "admin-api". This is the key that groups services onto one build.
pub fn image_base(image_ref: &str) -> Option<String> {
    Some(image_ref.rsplit('/').next()?.split(':').next()?.to_string())
}

/// Match a compose service's `image:` to a build entry by image basename.
pub fn find_for_service_image<'a>(bf: &'a BuildFile, service_image: &str) -> Option<&'a BuildSpec> {
    let base = image_base(service_image)?;
    bf.entries().iter().find(|b| b.image_name() == base)
}

/// Find a build entry by its image basename.
pub fn find_by_image<'a>(bf: &'a BuildFile, base: &str) -> Option<&'a BuildSpec> {
    bf.entries().iter().find(|b| b.image_name() == base)
}

/// Run one build, streaming docker output to the terminal and returning
/// a captured report.
pub fn run_build(cfg: &Config, bf: &BuildFile, spec: &BuildSpec) -> Result<BuildReport> {
    let started = Instant::now();
    let (_, dest) = bf.destination_for(spec)?;
    let full_image = dest.image_ref(&spec.image_name(), &spec.tag);
    let platform = spec.platform.as_deref().unwrap_or(&bf.default_platform);
    let src_repo = spec.source_repo(cfg)?;
    let source = format!(
        "{}:{}/{} @ {}",
        src_repo.provider, src_repo.organization, src_repo.repository, src_repo.branch
    );
    eprintln!("\n🔥 forging {} — {full_image} [{platform}]", spec.display_name());
    eprintln!("   source: {source}");
    if spec.repo_clone_url.is_some() {
        eprintln!("   (repoCloneUrl is set — full clone mode is planned; using zip snapshot for now)");
    }
    let spec_name = spec.display_name();
    let platform_s = platform.to_string();
    let pushed_flag = spec.push;
    let report = |ok: bool, log: String, started: Instant| BuildReport {
        image: full_image.clone(),
        name: spec_name.clone(),
        source: source.clone(),
        platform: platform_s.clone(),
        digest: digest_from_log(&log),
        pushed: pushed_flag && ok,
        ok,
        duration_secs: started.elapsed().as_secs(),
        log,
    };

    // 1. build context into RAM (azdo or github, resolved by source_repo)
    let ctx_fs = match &spec.local_path {
        Some(dir) => MemFs::from_dir(dir)?,
        None => MemFs::from_zip(&remote::download_repo_zip(&src_repo)?)?,
    };
    // Pick the dockerfile. The plain `Dockerfile` in these repos often
    // expects artifacts compiled OUTSIDE docker (legacy flow ran gradle
    // first); `DockerfileGitHub` is the self-contained multi-stage variant.
    // Since hefesto builds from a pristine in-memory context, prefer the
    // self-contained one unless build.yml pinned a dockerfile explicitly.
    let mut dockerfile = spec.dockerfile.clone();
    if dockerfile == default_dockerfile() && ctx_fs.get("DockerfileGitHub").is_some() {
        dockerfile = "DockerfileGitHub".to_string();
        eprintln!("   using DockerfileGitHub (self-contained build)");
    }
    if ctx_fs.get(&dockerfile).is_none() {
        bail!(
            "'{}' not found in {} (files: {})",
            dockerfile,
            src_repo.repository,
            ctx_fs.files.len()
        );
    }

    // 2. RAM -> tar stream
    let tar_bytes = to_tar(&ctx_fs)?;
    eprintln!(
        "   context: {} files, {} KiB (in-memory)",
        ctx_fs.files.len(),
        tar_bytes.len() / 1024
    );

    // 3. docker build - (output streamed live AND captured for the report)
    let mut cmd = Command::new("docker");
    cmd.args(["build", "--pull", "--platform", platform, "-t", &full_image, "-f", &dockerfile]);
    for (k, v) in &spec.args {
        cmd.args(["--build-arg", &format!("{k}={v}")]);
    }
    cmd.arg("-");
    let (ok, mut log) = run_tee_cmd(cmd, Some(tar_bytes))?;
    if !ok {
        eprintln!("❌ build FAILED for {full_image}");
        return Ok(report(false, log, started));
    }
    eprintln!("✅ built {full_image}");

    // 4. optional push, same tee'd streaming
    if spec.push {
        eprintln!("📤 pushing {full_image}");
        let mut cmd = Command::new("docker");
        cmd.args(["push", &full_image]);
        let (push_ok, push_log) = run_tee_cmd(cmd, None)?;
        log.push_str("\n--- push ---\n");
        log.push_str(&push_log);
        if !push_ok {
            eprintln!("❌ push FAILED for {full_image}");
            return Ok(report(false, log, started));
        }
        eprintln!("✅ pushed {full_image}");
    }
    Ok(report(true, log, started))
}

fn to_tar(fs: &MemFs) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in &fs.files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        // Azure DevOps zips don't carry unix modes; 0755 keeps scripts
        // (gradlew, entrypoints) executable and is harmless for the rest.
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, data.as_slice())?;
    }
    Ok(builder.into_inner()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = r#"#!/usr/bin/env bash
repoList=(
   "ExampleOrg,AppProject,admin-portal-src,admin-portal,release/uat,uat.latest"
#  "ExampleOrg,AppProject,disabled-repo,disabled-image,master,uat.latest"
   "ExampleOrg,Core,admin-api,,master,uat.latest"
)
source "../../shared/build_dockerhub_creds.sh"
source "../../shared/build_azuregit_stack.sh"
buildStackAzure "zauat" "repoList[@]"
"#;

    fn legacy_buildfile() -> BuildFile {
        BuildFile {
            destinations: default_destinations(),
            default_platform: default_platform(),
            mail_groups: BTreeMap::new(),
            repo_list: Vec::new(),
            builds: parse_legacy_repo_list(LEGACY),
            catalog: false,
        }
    }

    #[test]
    fn parses_legacy_build_sh() {
        let builds = parse_legacy_repo_list(LEGACY);
        assert_eq!(builds.len(), 2, "commented-out entries must be skipped");
        assert!(builds.iter().all(|b| b.image_name() != "disabled-image"));
        assert_eq!(builds[0].image_name(), "admin-portal");
        assert_eq!(builds[0].branch, "release/uat");
        assert_eq!(builds[1].image_name(), "admin-api");
        assert_eq!(builds[1].project, "Core");
    }

    #[test]
    fn image_refs_per_destination() {
        let hub = Destination {
            host: "docker.io".into(),
            namespace: "my-user".into(),
            user: None,
            user_env: None,
            pat_env: None,
        };
        let ghcr = Destination {
            host: "ghcr.io".into(),
            namespace: "my-org".into(),
            user: Some("Carlos-Camponuevo".into()),
            user_env: None,
            pat_env: None,
        };
        assert_eq!(ghcr.user.as_deref(), Some("Carlos-Camponuevo"));
        assert_eq!(ghcr.namespace, "my-org");
        assert_eq!(
            hub.image_ref("admin-api", "uat.latest"),
            "my-user/admin-api:uat.latest"
        );
        assert_eq!(
            ghcr.image_ref("admin-api", "uat.latest"),
            "ghcr.io/my-org/admin-api:uat.latest"
        );
        assert_eq!(hub.user_env(), "DOCKER_USER");
        assert_eq!(ghcr.pat_env(), "GHCR_PAT");
    }

    #[test]
    fn matches_service_image() {
        let bf = legacy_buildfile();
        let hit = find_for_service_image(&bf, "my-user/admin-api:uat.latest").unwrap();
        assert_eq!(hit.repository, "admin-api");
        assert!(find_for_service_image(&bf, "redis:7").is_none());
    }

    #[test]
    fn destination_resolution() {
        let bf = legacy_buildfile();
        let (name, dest) = bf.destination_for(&bf.builds[0]).unwrap();
        assert_eq!(name, "dockerhub");
        assert_eq!(dest.host, "docker.io");

        let mut spec = bf.builds[0].clone();
        spec.destination = Some("missing".into());
        assert!(bf.destination_for(&spec).is_err());
    }
}

#[cfg(all(test, unix))]
mod pty_tests {
    use super::*;

    /// Regression: the pty reader must see EOF once the child exits.
    /// Before `drop(cmd)` this test hung forever.
    #[test]
    fn pty_run_terminates_and_captures() {
        let pty = open_pty().expect("openpty");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hello-from-pty; exit 0"]);
        let (ok, log) = run_tee_pty(cmd, None, pty).expect("run");
        assert!(ok, "child should exit 0");
        assert!(log.contains("hello-from-pty"), "output captured: {log:?}");
    }

    /// A failing child is reported as such, and stdin data is delivered.
    #[test]
    fn pty_reports_failure_and_feeds_stdin() {
        let pty = open_pty().expect("openpty");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat; exit 3"]);
        let (ok, log) = run_tee_pty(cmd, Some(b"piped-input\n".to_vec(), ), pty).expect("run");
        assert!(!ok, "exit 3 must be reported as failure");
        assert!(log.contains("piped-input"), "stdin echoed back: {log:?}");
    }
}

#[cfg(test)]
mod html_preview {
    use super::*;
    /// Writes a sample report to /tmp for eyeballing: cargo test -- --ignored preview
    #[test]
    #[ignore]
    fn preview() {
        let ok = BuildReport {
            image: "ghcr.io/my-org/admin-portal:br.master.latest".into(),
            name: "Admin Portal".into(),
            source: "azdo:ExampleOrg/admin-portal @ master".into(),
            platform: "linux/amd64".into(),
            digest: "sha256:3a09bd902010166df8803b6e6718d17f20073dd0b18615cafa332e457c87a7df".into(),
            pushed: true,
            ok: true,
            duration_secs: 67,
            log: "#8 [build 5/5] RUN ./gradlew assemble\n#8 42.89 BUILD SUCCESSFUL in 42s\n#8 DONE 43.5s\n\n--- push ---\nThe push refers to repository [ghcr.io/my-org/admin-portal]\n6f0165bfabd4: Pushed\nbr.master.latest: digest: sha256:3a09bd902010166df8803b6e6718d17f20073dd0b18615cafa332e457c87a7df size: 1581".into(),
        };
        let bad = BuildReport {
            image: "ghcr.io/my-org/order-tracking:br.master.latest".into(),
            name: "Order Tracking".into(),
            source: "azdo:ExampleOrg/order-tracking @ master".into(),
            platform: "linux/amd64".into(),
            digest: String::new(),
            pushed: false,
            ok: false,
            duration_secs: 12,
            log: "#7 [build 4/5] RUN ./gradlew clean\n#7 ERROR: could not resolve dependency com.example:missing:1.0\nERROR: failed to solve: process \"/bin/sh -c ./gradlew clean\" did not complete successfully: exit code: 1".into(),
        };
        std::fs::write("/tmp/hefesto-build-report.html", report_html_refs("brprod/admin", &[&ok, &bad])).unwrap();
        eprintln!("wrote /tmp/hefesto-build-report.html");
    }
}
