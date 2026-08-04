mod build;
mod report;
mod config;
mod deploy;
mod mail;
mod nav;
mod remote;
mod runbook;
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
  hefesto -deploy [env/stack[/service]]        launch in DEPLOY mode
  hefesto -runbook [out-dir]                   generate the platform runbook
                                               (markdown + HTML + PDF) from
                                               compose + build.yml + stack.md,
                                               and mail it when mail is set up

  Without -build/-deploy the mode is automatic: DEPLOY on the repo's own
  host (devops-<hostname>), BUILD everywhere else.";

/// A flag argument is a mode target ("system", "zauat/admin", "e/s/svc")
/// when it isn't another flag, a URL, or a config file.
fn is_target(s: &str) -> bool {
    !s.starts_with('-')
        && !s.starts_with("http")
        && !s.ends_with(".json")
        && !s.ends_with(".yml")
        && !s.ends_with(".yaml")
        && !s.ends_with(".enc")
}

fn run() -> Result<()> {
    let mut mode_override: Option<nav::Mode> = None;
    let mut build_target: Option<String> = None;
    let mut deploy_target: Option<String> = None;
    let mut git_url: Option<String> = None;
    let mut cfg_path: Option<String> = None;
    let mut runbook_out: Option<String> = None;

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
            "-runbook" | "--runbook" => {
                let takes_dir = it.peek().is_some_and(|n| {
                    !n.starts_with('-')
                        && !n.ends_with(".json")
                        && !n.ends_with(".yml")
                        && !n.ends_with(".yaml")
                });
                runbook_out = Some(if takes_dir {
                    it.next().unwrap()
                } else {
                    "runbook".to_string()
                });
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other if !other.starts_with('-') => cfg_path = Some(other.to_string()),
            other => anyhow::bail!("unknown argument '{other}'\n{USAGE}"),
        }
    }

    let (mut cfg, cfg_key) = match (&git_url, &cfg_path) {
        (Some(url), _) => (Config::from_git_url(url)?, None),
        (None, Some(p)) => Config::load(p)?,
        (None, None) => Config::load(&Config::default_path())?,
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

    // 2. decrypt .enc entries in memory. If the config file itself was
    //    encrypted, its key is tried first — one prompt for everything
    //    when the keys match.
    let enc_count = fs.files.keys().filter(|k| k.ends_with(".enc")).count();
    if enc_count > 0 {
        let mut reuse = cfg_key;
        let mut attempts = 0;
        loop {
            let key = match reuse.take() {
                Some(k) => k,
                None => Zeroizing::new(
                    Password::new("Decrypt key:")
                        .without_confirmation()
                        .prompt()?,
                ),
            };
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

    // A target is a stack dir plus an optional last segment (image/service).
    // The dir may be one level ("system") or two ("zauat/admin") — whichever
    // prefix actually contains a docker-compose.yml wins.
    let split_target = |t: &str| -> Result<(String, Option<String>)> {
        let full = t.trim_matches('/');
        if fs.get(&format!("{full}/docker-compose.yml")).is_some() {
            return Ok((full.to_string(), None));
        }
        if let Some((dir, last)) = full.rsplit_once('/') {
            if fs.get(&format!("{dir}/docker-compose.yml")).is_some() {
                return Ok((dir.to_string(), Some(last.to_string())));
            }
        }
        anyhow::bail!("no docker-compose.yml found for target '{t}'")
    };

    // 3a. runbook generation (+ mail)
    if let Some(out_dir) = runbook_out {
        let today = std::process::Command::new("date")
            .arg("+%Y-%m-%d")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let files = runbook::generate(&fs, &cfg, &out_dir, &today)?;
        match &cfg.mail {
            Some(mail_cfg) => {
                let subject = format!("[hefesto] runbook — {} ({today})", cfg.repo.repository);
                let body = format!(
                    "DevOps platform runbook for {} generated on {today}.\n\n\
                     Attached: {}\n\n\
                     Facts come from each stack's docker-compose.yml, build provenance from build.yml,\n\
                     and descriptions from the stack.md kept beside each stack. Regenerate any time with:\n\
                     hefesto <config> -runbook\n",
                    cfg.repo.repository,
                    files.iter()
                        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                match mail::send_report_with_files(mail_cfg, &subject, &body, &files) {
                    Ok(()) => {}
                    Err(e) => eprintln!("⚠️  could not mail the runbook: {e:#}"),
                }
            }
            None => eprintln!(
                "ℹ️  runbook not mailed — no mail configured (add a \"mail\" block to the config \
                 or set HEFESTO_MAIL_TO=addr1,addr2)"
            ),
        }
        return Ok(());
    }

    // 3b. non-interactive build
    if let Some(target) = build_target {
        let (dir, name) = split_target(&target)?;
        // the last segment may be an image basename OR a service name —
        // a service resolves to the image it runs
        let image_base = name.map(|name| {
            fs.get(&format!("{dir}/docker-compose.yml"))
                .and_then(|raw| serde_yaml::from_slice::<serde_yaml::Value>(raw).ok())
                .and_then(|doc| {
                    doc.get("services")?
                        .get(&name)?
                        .get("image")?
                        .as_str()
                        .and_then(build::image_base)
                })
                .unwrap_or(name)
        });
        return nav::run_builds(&session, &dir, image_base.as_deref());
    }

    // 3b. non-interactive deploy
    if let Some(target) = deploy_target {
        let (dir, svc) = split_target(&target)?;
        let dtarget = match svc {
            None => deploy::Target::WholeStack,
            Some(name) => deploy::Target::Services(vec![name]),
        };
        return nav::run_deploy(&session, &dir, dtarget);
    }

    // 3c. interactive navigation
    nav::run(&session)
}
