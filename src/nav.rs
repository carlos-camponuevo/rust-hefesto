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
    // service name -> image (needed to map a service onto a build entry)
    let services: Vec<(String, String)> = doc
        .get("services")
        .and_then(|s| s.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let name = k.as_str()?.to_string();
                    let image = v
                        .get("image")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some((name, image))
                })
                .collect()
        })
        .unwrap_or_default();

    loop {
        let whole = format!("🏗  whole stack ({} services)", services.len());
        let mut options = vec![whole.clone()];
        options.extend(services.iter().map(|(s, _)| format!("   {s}")));
        options.push(BACK.into());
        let choice = Select::new(&format!("{env}/{stack} — target:"), options).prompt()?;
        if choice == BACK {
            return Ok(());
        }
        let target = services.iter().find(|(s, _)| *s == choice.trim());
        pick_action(session, env, stack, target)?;
    }
}

fn pick_action(
    session: &Session,
    env: &str,
    stack: &str,
    service: Option<&(String, String)>,
) -> Result<()> {
    let label = service.map(|(s, _)| s.as_str()).unwrap_or("whole stack");
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
        "🔨 Build" => run_builds(session, env, stack, service),
        s if s.starts_with("🚀 Deploy") => {
            if session.deploy_allowed {
                eprintln!("  deploy for {env}/{stack} [{label}] — coming in milestone 4 (stdin compose deploy)");
            } else {
                eprintln!("  deploy refused: this binary only deploys on the repo's own host");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn run_builds(
    session: &Session,
    env: &str,
    stack: &str,
    service: Option<&(String, String)>,
) -> Result<()> {
    let Some(bf) = crate::build::load(session.fs, env, stack)? else {
        eprintln!("  no build.yml and no parseable legacy build.sh in {env}/{stack} — nothing to build");
        return Ok(());
    };
    let targets: Vec<&crate::build::BuildSpec> = match service {
        None => bf.builds.iter().collect(),
        Some((name, image)) => match crate::build::find_for_service_image(&bf, image) {
            Some(spec) => vec![spec],
            None => {
                eprintln!(
                    "  service '{name}' (image '{image}') has no matching build entry — \
                     it uses a stock image or is built by another stack"
                );
                return Ok(());
            }
        },
    };
    // legacy step 2: registry login — once per distinct destination used
    let mut seen = std::collections::BTreeSet::new();
    for spec in &targets {
        let (name, dest) = bf.destination_for(spec)?;
        if seen.insert(name.to_string()) {
            crate::build::registry_login(name, dest)?;
        }
    }

    let mut failed = 0;
    for spec in &targets {
        if let Err(e) = crate::build::run_build(session.cfg, &bf, spec) {
            eprintln!("❌ {e:#}");
            failed += 1;
        }
    }
    eprintln!(
        "\n🏁 {}/{} builds succeeded in {env}/{stack}",
        targets.len() - failed,
        targets.len()
    );
    Ok(())
}
