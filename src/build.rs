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
    pub source: String,
    pub ok: bool,
    pub duration_secs: u64,
    /// captured docker output (kept to the last `LOG_KEEP_LINES` lines)
    pub log: String,
}

const LOG_KEEP_LINES: usize = 400;

/// Run a command streaming its output live to the terminal AND capturing
/// it. `stdin_data` (the tar context) is fed from a thread to avoid
/// pipe-buffer deadlocks with chatty children.
fn run_tee(mut cmd: Command, stdin_data: Option<Vec<u8>>) -> Result<(bool, String)> {
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
    Ok((status.success(), tail_lines(&log, LOG_KEEP_LINES)))
}

/// Plain-text report for mails / logs.
pub fn report_body(env: &str, stack: &str, reports: &[BuildReport]) -> String {
    let mut b = String::new();
    let ok = reports.iter().filter(|r| r.ok).count();
    b.push_str(&format!(
        "hefesto build report — {env}/{stack}\n{ok}/{} builds succeeded\n\n",
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
    pub builds: Vec<BuildSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    /// "docker.io" (default) or "ghcr.io".
    #[serde(default = "default_registry_host")]
    pub host: String,
    /// Registry namespace (Docker Hub user / GHCR owner).
    #[serde(default = "default_namespace")]
    pub namespace: String,
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
    /// Azure DevOps organization; defaults to the devops repo's own org.
    #[serde(default)]
    pub organization: Option<String>,
    pub project: String,
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

impl BuildFile {
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
    pub fn image_name(&self) -> &str {
        self.image.as_deref().unwrap_or(&self.repository)
    }
}

/// Step 2 of the legacy flow: registry login. Credentials are REQUIRED
/// from env vars (user + PAT) — never stored in files.
pub fn registry_login(name: &str, dest: &Destination) -> Result<()> {
    let (user_env, pat_env) = (dest.user_env(), dest.pat_env());
    let user = std::env::var(&user_env).unwrap_or_default();
    let pat = std::env::var(&pat_env).unwrap_or_default();
    if user.is_empty() || pat.is_empty() {
        bail!(
            "destination '{name}' ({}) needs registry credentials:\n  export {user_env}='<user>'\n  export {pat_env}='<personal access token>'",
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

/// Load the stack's build definition: build.yml first, legacy build.sh next.
pub fn load(fs: &MemFs, env: &str, stack: &str) -> Result<Option<BuildFile>> {
    if let Some(raw) = fs.get(&format!("{env}/{stack}/build.yml")) {
        let bf: BuildFile = serde_yaml::from_slice(raw)
            .with_context(|| format!("{env}/{stack}/build.yml is invalid"))?;
        return Ok(Some(bf));
    }
    if let Some(raw) = fs.get(&format!("{env}/{stack}/build.sh")) {
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
                        user_env: None,
                        pat_env: None,
                    },
                )]),
                builds,
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
        for quoted in t.split('"').skip(1).step_by(2) {
            let f: Vec<&str> = quoted.split(',').map(str::trim).collect();
            if f.len() == 6 {
                out.push(BuildSpec {
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

/// Match a compose service's `image:` (e.g. "camponuevo/mn-bat-admin-api:za.uat.latest")
/// to a build entry by image basename.
pub fn find_for_service_image<'a>(bf: &'a BuildFile, service_image: &str) -> Option<&'a BuildSpec> {
    let base = service_image
        .rsplit('/')
        .next()?
        .split(':')
        .next()?
        .to_string();
    bf.builds.iter().find(|b| b.image_name() == base)
}

/// Run one build, streaming docker output to the terminal and returning
/// a captured report.
pub fn run_build(cfg: &Config, bf: &BuildFile, spec: &BuildSpec) -> Result<BuildReport> {
    let started = Instant::now();
    let (_, dest) = bf.destination_for(spec)?;
    let full_image = dest.image_ref(spec.image_name(), &spec.tag);
    let source = format!("{}/{} @ {}", spec.project, spec.repository, spec.branch);
    eprintln!("\n🔥 forging {full_image}");
    eprintln!("   source: {source}");
    let report = |ok: bool, log: String, started: Instant| BuildReport {
        image: full_image.clone(),
        source: source.clone(),
        ok,
        duration_secs: started.elapsed().as_secs(),
        log,
    };

    // 1. build context into RAM
    let ctx_fs = match &spec.local_path {
        Some(dir) => MemFs::from_dir(dir)?,
        None => {
            let repo = crate::config::Repo {
                organization: spec
                    .organization
                    .clone()
                    .unwrap_or_else(|| cfg.repo.organization.clone()),
                project: spec.project.clone(),
                repository: spec.repository.clone(),
                branch: spec.branch.clone(),
                pat_env: cfg.repo.pat_env.clone(),
                local_path: None,
            };
            MemFs::from_zip(&remote::download_repo_zip(&repo)?)?
        }
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
            spec.repository,
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
    cmd.args(["build", "--pull", "-t", &full_image, "-f", &dockerfile]);
    for (k, v) in &spec.args {
        cmd.args(["--build-arg", &format!("{k}={v}")]);
    }
    cmd.arg("-");
    let (ok, mut log) = run_tee(cmd, Some(tar_bytes))?;
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
        let (push_ok, push_log) = run_tee(cmd, None)?;
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
   "BatDigitalI,ConectaRep,grails4-bat-admin-portal,grails-bat-admin-portal,release/uat20250503,za.uat.latest"
   "BatDigitalI,Core,mn-bat-admin-api,,master,za.uat.latest"
)
source "../../shared/build_dockerhub_creds.sh"
source "../../shared/build_azuregit_stack.sh"
buildStackAzure "zauat" "repoList[@]"
"#;

    fn legacy_buildfile() -> BuildFile {
        BuildFile {
            destinations: default_destinations(),
            builds: parse_legacy_repo_list(LEGACY),
        }
    }

    #[test]
    fn parses_legacy_build_sh() {
        let builds = parse_legacy_repo_list(LEGACY);
        assert_eq!(builds.len(), 2);
        assert_eq!(builds[0].image_name(), "grails-bat-admin-portal");
        assert_eq!(builds[0].branch, "release/uat20250503");
        assert_eq!(builds[1].image_name(), "mn-bat-admin-api");
        assert_eq!(builds[1].project, "Core");
    }

    #[test]
    fn image_refs_per_destination() {
        let hub = Destination {
            host: "docker.io".into(),
            namespace: "camponuevo".into(),
            user_env: None,
            pat_env: None,
        };
        let ghcr = Destination {
            host: "ghcr.io".into(),
            namespace: "carlos-camponuevo".into(),
            user_env: None,
            pat_env: None,
        };
        assert_eq!(
            hub.image_ref("mn-bat-admin-api", "za.uat.latest"),
            "camponuevo/mn-bat-admin-api:za.uat.latest"
        );
        assert_eq!(
            ghcr.image_ref("mn-bat-admin-api", "za.uat.latest"),
            "ghcr.io/carlos-camponuevo/mn-bat-admin-api:za.uat.latest"
        );
        assert_eq!(hub.user_env(), "DOCKER_USER");
        assert_eq!(ghcr.pat_env(), "GHCR_PAT");
    }

    #[test]
    fn matches_service_image() {
        let bf = legacy_buildfile();
        let hit = find_for_service_image(&bf, "camponuevo/mn-bat-admin-api:za.uat.latest").unwrap();
        assert_eq!(hit.repository, "mn-bat-admin-api");
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
