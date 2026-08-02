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
  ... --build <env>/<stack>[/<service>] non-interactive: build a stack (or one
                                        service of it) and exit";

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // extract optional `--build <target>`
    let mut build_target: Option<String> = None;
    if let Some(i) = args.iter().position(|a| a == "--build") {
        anyhow::ensure!(i + 1 < args.len(), "--build needs <env>/<stack>[/<service>]\n{USAGE}");
        build_target = Some(args.remove(i + 1));
        args.remove(i);
    }

    let mut cfg = match args.as_slice() {
        [] => Config::load("hefesto.json")?,
        [flag, url] if flag == "-git" || flag == "--git" => Config::from_git_url(url)?,
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            return Ok(());
        }
        [path] if !path.starts_with('-') => Config::load(path)?,
        _ => anyhow::bail!("invalid arguments\n{USAGE}"),
    };

    if cfg.mail.is_none() {
        cfg.mail = config::mail_from_env();
    }

    let host = config::short_hostname();
    let expected = cfg.expected_hostname();
    let deploy_allowed = expected.as_deref() == Some(host.as_str());

    eprintln!("🔥 hefesto — {}", cfg.repo.repository);
    match &expected {
        Some(_) if deploy_allowed => eprintln!("   host {host} ✓ deploy enabled"),
        Some(exp) => eprintln!("   host {host} ≠ {exp} → build-only mode"),
        None => eprintln!("   repo name has no devops-<host> pattern → build-only mode"),
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
    };

    // 3a. non-interactive build
    if let Some(target) = build_target {
        let parts: Vec<&str> = target.split('/').collect();
        let (env, stack, svc_name) = match parts.as_slice() {
            [e, s] => (*e, *s, None),
            [e, s, svc] => (*e, *s, Some(*svc)),
            _ => anyhow::bail!("--build target must be <env>/<stack>[/<service>]"),
        };
        let service_pair = match svc_name {
            None => None,
            Some(name) => {
                let compose = fs
                    .get(&format!("{env}/{stack}/docker-compose.yml"))
                    .ok_or_else(|| anyhow::anyhow!("no docker-compose.yml in {env}/{stack}"))?;
                let doc: serde_yaml::Value = serde_yaml::from_slice(compose)?;
                let image = doc
                    .get("services")
                    .and_then(|s| s.get(name))
                    .and_then(|s| s.get("image"))
                    .and_then(|i| i.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("service '{name}' not found in {env}/{stack} compose")
                    })?
                    .to_string();
                Some((name.to_string(), image))
            }
        };
        return nav::run_builds(&session, env, stack, service_pair.as_ref());
    }

    // 3b. interactive navigation
    nav::run(&session)
}
