mod config;
mod nav;
mod remote;
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
  hefesto [config.json]        load config file (default: ./hefesto.json)
  hefesto -git <url>           derive config from an Azure DevOps git URL
                               (https://dev.azure.com/org/project/_git/repo
                                or git@ssh.dev.azure.com:v3/org/project/repo)";

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match args.as_slice() {
        [] => Config::load("hefesto.json")?,
        [flag, url] if flag == "-git" || flag == "--git" => Config::from_git_url(url)?,
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            return Ok(());
        }
        [path] if !path.starts_with('-') => Config::load(path)?,
        _ => anyhow::bail!("invalid arguments\n{USAGE}"),
    };

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

    // 3. navigate
    let session = nav::Session {
        fs: &fs,
        cfg: &cfg,
        deploy_allowed,
        host,
    };
    nav::run(&session)
}
