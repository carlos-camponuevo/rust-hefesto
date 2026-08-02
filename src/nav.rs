//! Interactive navigation: environment folder -> stack -> compose services
//! -> action. Build/Deploy are milestone 3/4 stubs for now; Deploy is
//! additionally gated by the hostname check.

use crate::config::Config;
use crate::vault::MemFs;
use anyhow::Result;
use inquire::Select;

const BACK: &str = "⬅ back";
const QUIT: &str = "✖ quit";

pub struct Session<'a> {
    pub fs: &'a MemFs,
    pub cfg: &'a Config,
    pub deploy_allowed: bool,
    pub host: String,
}

pub fn run(session: &Session) -> Result<()> {
    loop {
        let mut envs: Vec<String> = session
            .fs
            .subdirs("")
            .into_iter()
            .filter(|d| !session.cfg.exclude_folders.contains(d) && !d.starts_with('.'))
            .collect();
        if envs.is_empty() {
            eprintln!("no environment folders found");
            return Ok(());
        }
        envs.push(QUIT.into());
        let env = Select::new("Environment folder:", envs).prompt()?;
        if env == QUIT {
            return Ok(());
        }
        pick_stack(session, &env)?;
    }
}

fn pick_stack(session: &Session, env: &str) -> Result<()> {
    loop {
        let mut stacks: Vec<String> = session
            .fs
            .subdirs(env)
            .into_iter()
            .filter(|d| !session.cfg.exclude_subfolders.contains(d))
            .collect();
        if stacks.is_empty() {
            eprintln!("  ({env} has no stack subfolders)");
            return Ok(());
        }
        stacks.push(BACK.into());
        let stack = Select::new(&format!("Stack in {env}:"), stacks).prompt()?;
        if stack == BACK {
            return Ok(());
        }
        pick_service(session, env, &stack)?;
    }
}

fn pick_service(session: &Session, env: &str, stack: &str) -> Result<()> {
    let compose_path = format!("{env}/{stack}/docker-compose.yml");
    let Some(raw) = session.fs.get(&compose_path) else {
        eprintln!("  (no docker-compose.yml in {env}/{stack} — was the repo decrypted?)");
        return Ok(());
    };
    let doc: serde_yaml::Value = match serde_yaml::from_slice(raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  cannot parse {compose_path}: {e}");
            return Ok(());
        }
    };
    let services: Vec<String> = doc
        .get("services")
        .and_then(|s| s.as_mapping())
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    loop {
        let whole = format!("🏗  whole stack ({} services)", services.len());
        let mut options = vec![whole.clone()];
        options.extend(services.iter().map(|s| format!("   {s}")));
        options.push(BACK.into());
        let choice = Select::new(&format!("{env}/{stack} — target:"), options).prompt()?;
        if choice == BACK {
            return Ok(());
        }
        let target = if choice == whole {
            None
        } else {
            Some(choice.trim().to_string())
        };
        pick_action(session, env, stack, target.as_deref())?;
    }
}

fn pick_action(session: &Session, env: &str, stack: &str, service: Option<&str>) -> Result<()> {
    let label = service.unwrap_or("whole stack");
    let deploy_label = if session.deploy_allowed {
        "🚀 Deploy".to_string()
    } else {
        format!(
            "🚀 Deploy (disabled: host '{}' is not the target of this repo)",
            session.host
        )
    };
    let actions = vec!["🔨 Build".to_string(), deploy_label, BACK.into()];
    let choice = Select::new(&format!("Action for {env}/{stack} [{label}]:"), actions).prompt()?;
    match choice.as_str() {
        "🔨 Build" => {
            eprintln!("  build for {env}/{stack} [{label}] — coming in milestone 3 (build.yml + in-memory docker build)");
        }
        s if s.starts_with("🚀 Deploy") => {
            if session.deploy_allowed {
                eprintln!("  deploy for {env}/{stack} [{label}] — coming in milestone 4 (stdin compose deploy)");
            } else {
                eprintln!("  deploy refused: this binary only deploys on the repo's own host");
            }
        }
        _ => {}
    }
    Ok(())
}
