//! Interactive navigation: environment folder -> stack -> compose services
//! -> action. Left-arrow (or Esc) steps back to the previous menu; at the
//! root it exits. Deploy is gated by the hostname check (milestone 4 will
//! implement it).

use crate::config::Config;
use crate::ui::{self, Pick};
use crate::vault::MemFs;
use anyhow::Result;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Build,
    Deploy,
}

pub struct Session<'a> {
    pub fs: &'a MemFs,
    pub cfg: &'a Config,
    pub deploy_allowed: bool,
    pub host: String,
    pub mode: Mode,
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
            Pick::Item(i) => pick_image(session, env, &stacks[i])?,
        }
    }
}

/// One build unit: an image and the compose services that run it.
struct ImageGroup {
    base: String,
    buildable: bool,
    services: Vec<String>,
}

/// Level 3: images of the stack (the build units). Several services can
/// share one image — building happens once per image.
fn pick_image(session: &Session, env: &str, stack: &str) -> Result<()> {
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

    let bf = crate::build::load(session.fs, env, stack)?;

    // group services by image basename, in compose order
    let mut groups: Vec<ImageGroup> = Vec::new();
    for (name, image) in &services {
        let base = crate::build::image_base(image).unwrap_or_else(|| image.clone());
        match groups.iter_mut().find(|g| g.base == base) {
            Some(g) => g.services.push(name.clone()),
            None => groups.push(ImageGroup {
                buildable: bf
                    .as_ref()
                    .is_some_and(|bf| crate::build::find_by_image(bf, &base).is_some()),
                base,
                services: vec![name.clone()],
            }),
        }
    }
    // build entries with no service in the compose (still buildable)
    if let Some(bf) = &bf {
        for spec in &bf.builds {
            if !groups.iter().any(|g| g.base == spec.image_name()) {
                groups.push(ImageGroup {
                    base: spec.image_name().to_string(),
                    buildable: true,
                    services: Vec::new(),
                });
            }
        }
    }

    match session.mode {
        Mode::Build => pick_image_build(session, env, stack, &groups, services.len()),
        Mode::Deploy => pick_image_deploy(session, env, stack, &groups, services.len()),
    }
}

/// BUILD MODE — images are the targets; selecting one builds it directly.
fn pick_image_build(
    session: &Session,
    env: &str,
    stack: &str,
    groups: &[ImageGroup],
    n_services: usize,
) -> Result<()> {
    let buildable: Vec<&ImageGroup> = groups.iter().filter(|g| g.buildable).collect();
    if buildable.is_empty() {
        eprintln!("  ({env}/{stack} has nothing to build — stock images only)");
        return Ok(());
    }
    let mut options: Vec<String> = vec![format!(
        "🏗  build whole stack ({} images, {n_services} services)",
        buildable.len()
    )];
    options.extend(buildable.iter().map(|g| {
        format!(
            "📦 {} ({} service{})",
            g.base,
            g.services.len(),
            if g.services.len() == 1 { "" } else { "s" }
        )
    }));
    loop {
        match ui::select(&format!("{env}/{stack} — build image"), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => {
                run_builds(session, env, stack, None)?;
            }
            Pick::Item(i) => {
                run_builds(session, env, stack, Some(&buildable[i - 1].base))?;
            }
        }
    }
}

/// DEPLOY MODE — services are the targets, grouped by their image.
fn pick_image_deploy(
    session: &Session,
    env: &str,
    stack: &str,
    groups: &[ImageGroup],
    n_services: usize,
) -> Result<()> {
    let mut options: Vec<String> = vec![format!("🚀 deploy whole stack ({n_services} services)")];
    options.extend(groups.iter().map(|g| {
        let tag = if g.buildable { "📦" } else { "🧊" }; // 🧊 = stock image
        format!(
            "{tag} {} ({} service{})",
            g.base,
            g.services.len(),
            if g.services.len() == 1 { "" } else { "s" }
        )
    }));
    loop {
        match ui::select(&format!("{env}/{stack} — deploy"), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => deploy_stub(session, env, stack, "whole stack"),
            Pick::Item(i) => {
                let group = &groups[i - 1];
                pick_deploy_service(session, env, stack, group)?;
            }
        }
    }
}

/// DEPLOY MODE, level 4: services running one image.
fn pick_deploy_service(session: &Session, env: &str, stack: &str, group: &ImageGroup) -> Result<()> {
    let mut options: Vec<String> = vec![format!(
        "🚀 all {} service{} of {}",
        group.services.len(),
        if group.services.len() == 1 { "" } else { "s" },
        group.base
    )];
    options.extend(group.services.iter().map(|s| format!("   {s}")));
    loop {
        match ui::select(&format!("{env}/{stack} / {}", group.base), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => {
                deploy_stub(session, env, stack, &format!("all services of {}", group.base));
            }
            Pick::Item(i) => deploy_stub(session, env, stack, &group.services[i - 1]),
        }
    }
}

fn deploy_stub(session: &Session, env: &str, stack: &str, what: &str) {
    if session.deploy_allowed {
        eprintln!("  deploy {what} in {env}/{stack} — coming in milestone 4 (stdin compose deploy)");
    } else {
        eprintln!(
            "  deploy refused: host '{}' is not this repo's target (devops-<host>)",
            session.host
        );
    }
}

/// Build targets are IMAGES: `image = None` builds every entry of the
/// stack's build list, `Some(base)` builds exactly that image once — no
/// matter how many services run it.
pub fn run_builds(session: &Session, env: &str, stack: &str, image: Option<&str>) -> Result<()> {
    let Some(bf) = crate::build::load(session.fs, env, stack)? else {
        eprintln!("  no build.yml and no parseable legacy build.sh in {env}/{stack} — nothing to build");
        return Ok(());
    };
    let targets: Vec<&crate::build::BuildSpec> = match image {
        None => bf.builds.iter().collect(),
        Some(base) => match crate::build::find_by_image(&bf, base) {
            Some(spec) => vec![spec],
            None => {
                eprintln!(
                    "  image '{base}' has no build entry in {env}/{stack} — \
                     it's a stock image or built by another stack"
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
