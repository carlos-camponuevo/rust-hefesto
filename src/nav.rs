//! Interactive navigation: environment folder -> stack -> compose services
//! -> action. Left-arrow (or Esc) steps back to the previous menu; at the
//! root it exits. Deploy is gated by the hostname check (milestone 4 will
//! implement it).

use crate::config::Config;
use crate::ui::{self, Pick};
use crate::vault::MemFs;
use anyhow::Result;

pub struct Session<'a> {
    pub fs: &'a MemFs,
    pub cfg: &'a Config,
    pub deploy_allowed: bool,
    pub host: String,
}

pub fn run(session: &Session) -> Result<()> {
    let envs: Vec<String> = session
        .fs
        .subdirs("")
        .into_iter()
        .filter(|d| !session.cfg.exclude_folders.contains(d) && !d.starts_with('.'))
        .collect();
    if envs.is_empty() {
        eprintln!("no environment folders found");
        return Ok(());
    }
    loop {
        match ui::select("Environment folder", &envs)? {
            Pick::Back => return Ok(()), // ← at root exits
            Pick::Item(i) => pick_stack(session, &envs[i])?,
        }
    }
}

fn pick_stack(session: &Session, env: &str) -> Result<()> {
    let stacks: Vec<String> = session
        .fs
        .subdirs(env)
        .into_iter()
        .filter(|d| !session.cfg.exclude_subfolders.contains(d))
        .collect();
    if stacks.is_empty() {
        eprintln!("  ({env} has no stack subfolders)");
        return Ok(());
    }
    loop {
        match ui::select(&format!("Stack in {env}"), &stacks)? {
            Pick::Back => return Ok(()),
            Pick::Item(i) => pick_service(session, env, &stacks[i])?,
        }
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

    let mut options: Vec<String> = vec![format!("🏗  whole stack ({} services)", services.len())];
    options.extend(services.iter().map(|(s, _)| s.clone()));

    loop {
        match ui::select(&format!("{env}/{stack} — target"), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => pick_action(session, env, stack, None)?,
            Pick::Item(i) => pick_action(session, env, stack, services.get(i - 1))?,
        }
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
    let actions = vec!["🔨 Build".to_string(), deploy_label];
    loop {
        match ui::select(&format!("Action for {env}/{stack} [{label}]"), &actions)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => {
                run_builds(session, env, stack, service)?;
                return Ok(()); // back to target menu after a build run
            }
            Pick::Item(_) => {
                if session.deploy_allowed {
                    eprintln!(
                        "  deploy for {env}/{stack} [{label}] — coming in milestone 4 (stdin compose deploy)"
                    );
                } else {
                    eprintln!("  deploy refused: this binary only deploys on the repo's own host");
                }
                return Ok(());
            }
        }
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

    // legacy step 2: registry login — once per distinct destination that
    // will actually be pushed to (build-only runs need no credentials)
    let mut seen = std::collections::BTreeSet::new();
    for spec in targets.iter().filter(|s| s.push) {
        let (name, dest) = bf.destination_for(spec)?;
        if seen.insert(name.to_string()) {
            crate::build::registry_login(name, dest)?;
        }
    }

    let mut reports: Vec<crate::build::BuildReport> = Vec::new();
    for spec in &targets {
        match crate::build::run_build(session.cfg, &bf, spec) {
            Ok(r) => reports.push(r),
            Err(e) => {
                // build never started (download/config error) — still report it
                eprintln!("❌ {e:#}");
                reports.push(crate::build::BuildReport {
                    image: spec.image_name().to_string(),
                    source: format!("{}/{} @ {}", spec.project, spec.repository, spec.branch),
                    ok: false,
                    duration_secs: 0,
                    log: format!("{e:#}"),
                });
            }
        }
    }
    let ok = reports.iter().filter(|r| r.ok).count();
    eprintln!("\n🏁 {ok}/{} builds succeeded in {env}/{stack}", reports.len());

    if let Some(mail_cfg) = &session.cfg.mail {
        let subject = format!(
            "[hefesto] {env}/{stack}: {ok}/{} builds {}",
            reports.len(),
            if ok == reports.len() { "OK ✅" } else { "with FAILURES ❌" }
        );
        let body = crate::build::report_body(env, stack, &reports);
        if let Err(e) = crate::mail::send_report(mail_cfg, &subject, &body) {
            eprintln!("⚠️  could not mail the report: {e:#}");
        }
    } else {
        eprintln!(
            "ℹ️  report not mailed — no mail configured (add a \"mail\" block to hefesto.json \
             or set HEFESTO_MAIL_TO=addr1,addr2)"
        );
    }
    Ok(())
}
