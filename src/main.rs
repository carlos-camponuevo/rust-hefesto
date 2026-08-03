mod build;
mod config;
mod mail;
mod nav;
mod remote;
mod ui;
mod vault;

use anyhow::Result;
use config::Config;
use inquire::Password;
use vault::MemFs;
use zeroize::Zeroizing;

fn main() {
    if let Err(e) = run() {
        eprintln!("❌ {e:#}");
        std::process::exit(1);
    }
}

const USAGE: &str = "usage:
  hefesto [config.json]                 load config file (default: ./hefesto.json)
  hefesto -git <url>                    derive config from an Azure DevOps git URL
                                        (https://dev.azure.com/org/project/_git/repo
                                         or git@ssh.dev.azure.com:v3/org/project/repo)
  hefesto -build [env/stack[/image|service]]   launch in BUILD mode; with a
                                               target: build it and exit
  hefesto -deploy [env/stack[/service]]        launch in DEPLOY mode (actions
                                               land in milestone 4)

  Without -build/-deploy the mode is automatic: DEPLOY on the repo's own
  host (devops-<hostname>), BUILD everywhere else.";

/// A flag argument is a mode target when it looks like env/stack[/x]
/// (has a '/', isn't another flag, isn't a config file or URL).
fn is_target(s: &str) -> bool {
    s.contains('/') && !s.starts_with('-') && !s.ends_with(".json") && !s.starts_with("http")
}

fn run() -> Result<()> {
    let mut mode_override: Option<nav::Mode> = None;
    let mut build_target: Option<String> = None;
    let mut deploy_target: Option<String> = None;
    let mut git_url: Option<String> = None;
    let mut cfg_path: Option<String> = None;

    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-git" | "--git" => {
                git_url =
                    Some(it.next().ok_or_else(|| anyhow::anyhow!("-git needs a URL\n{USAGE}"))?);
            }
            "-build" | "--build" => {
                mode_override = Some(nav::Mode::Build);
                if it.peek().is_some_and(|n| is_target(n)) {
                    build_target = it.next();
                }
            }
            "-deploy" | "--deploy" => {
                mode_override = Some(nav::Mode::Deploy);
                if it.peek().is_some_and(|n| is_target(n)) {
                    deploy_target = it.next();
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other if !other.starts_with('-') => cfg_path = Some(other.to_string()),
            other => anyhow::bail!("unknown argument '{other}'\n{USAGE}"),
        }
    }

    let mut cfg = match (&git_url, &cfg_path) {
        (Some(url), _) => Config::from_git_url(url)?,
        (None, Some(p)) => Config::load(p)?,
        (None, None) => Config::load("hefesto.json")?,
    };

    if cfg.mail.is_none() {
        cfg.mail = config::mail_from_env();
    }

    let host = config::short_hostname();
    let expected = cfg.expected_hostname();
    let deploy_allowed = expected.as_deref() == Some(host.as_str());

    let mode = mode_override.unwrap_or(if deploy_allowed {
        nav::Mode::Deploy
    } else {
        nav::Mode::Build
    });

    eprintln!("🔥 hefesto — {}", cfg.repo.repository);
    let mode_txt = match mode {
        nav::Mode::Build => "BUILD",
        nav::Mode::Deploy => "DEPLOY",
    };
    match &expected {
        Some(_) if deploy_allowed => eprintln!("   host {host} ✓ (repo target) — mode: {mode_txt}"),
        Some(exp) => eprintln!("   host {host} ≠ {exp} — mode: {mode_txt}"),
        None => eprintln!("   repo name has no devops-<host> pattern — mode: {mode_txt}"),
    }

    // 1. repo into memory
    let mut fs = match &cfg.repo.local_path {
        Some(dir) => {
            eprintln!("📂 loading local copy '{dir}' (test mode, in-memory)");
            MemFs::from_dir(dir)?
        }
        None => MemFs::from_zip(&remote::download_repo_zip(&cfg.repo)?)?,
    };
    eprintln!("   {} files loaded", fs.files.len());

    // 2. decrypt .enc entries in memory
    let enc_count = fs.files.keys().filter(|k| k.ends_with(".enc")).count();
    if enc_count > 0 {
        let mut attempts = 0;
        loop {
            let key = Zeroizing::new(
                Password::new("Decrypt key:")
                    .without_confirmation()
                    .prompt()?,
            );
            match fs.decrypt_all(&key) {
                Ok(n) => {
                    eprintln!("🔓 {n} files decrypted (in memory only)");
                    break;
                }
                Err(e) => {
                    attempts += 1;
                    if attempts >= 3 {
                        return Err(e.context("3 failed attempts"));
                    }
                    eprintln!("   {e:#} — try again");
                }
            }
        }
    } else {
        eprintln!("   (no .enc files found — nothing to decrypt)");
    }

    let session = nav::Session {
        fs: &fs,
        cfg: &cfg,
        deploy_allowed,
        host,
        mode,
    };

    // 3a. non-interactive build
    if let Some(target) = build_target {
        let parts: Vec<&str> = target.split('/').collect();
        let (env, stack, svc_name) = match parts.as_slice() {
            [e, s] => (*e, *s, None),
            [e, s, svc] => (*e, *s, Some(*svc)),
            _ => anyhow::bail!("--build target must be <env>/<stack>[/<service>]"),
        };
        // the third segment may be an image basename OR a service name —
        // a service resolves to the image it runs
        let image_base = match svc_name {
            None => None,
            Some(name) => {
                let from_service = fs
                    .get(&format!("{env}/{stack}/docker-compose.yml"))
                    .and_then(|raw| serde_yaml::from_slice::<serde_yaml::Value>(raw).ok())
                    .and_then(|doc| {
                        doc.get("services")?
                            .get(name)?
                            .get("image")?
                            .as_str()
                            .and_then(build::image_base)
                    });
                Some(from_service.unwrap_or_else(|| name.to_string()))
            }
        };
        return nav::run_builds(&session, env, stack, image_base.as_deref());
    }

    // 3b. non-interactive deploy (milestone 4)
    if let Some(target) = deploy_target {
        eprintln!("🚀 deploy '{target}' — coming in milestone 4 (stdin compose deploy)");
        return Ok(());
    }

    // 3c. interactive navigation
    nav::run(&session)
}
