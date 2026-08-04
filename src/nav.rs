//! Interactive navigation. Left-arrow (or Esc) steps back; at the root it
//! exits. Two layouts exist in the repos:
//!   env/stack/docker-compose.yml   ("zauat/admin")
//!   stack/docker-compose.yml       ("system", "systools" — deploy-only)
//! Both are handled; a stack is always addressed by its dir path, and the
//! swarm stack name is the dir with '/' replaced by '-'.

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

fn has_compose(fs: &MemFs, dir: &str) -> bool {
    fs.get(&format!("{dir}/docker-compose.yml")).is_some()
}

/// Image basenames referenced by a stack's compose services.
fn compose_images(fs: &MemFs, dir: &str) -> std::collections::BTreeSet<String> {
    fs.get(&format!("{dir}/docker-compose.yml"))
        .and_then(|raw| serde_yaml::from_slice::<serde_yaml::Value>(raw).ok())
        .and_then(|doc| {
            Some(
                doc.get("services")?
                    .as_mapping()?
                    .values()
                    .filter_map(|v| v.get("image")?.as_str().and_then(crate::build::image_base))
                    .collect(),
            )
        })
        .unwrap_or_default()
}

pub fn run(session: &Session) -> Result<()> {
    // root folders: a folder with its own docker-compose.yml is itself a
    // stack (system/systools); otherwise it's an environment of stacks.
    let mut options: Vec<(String, bool)> = Vec::new(); // (dir, is_stack)
    for d in session.fs.subdirs("") {
        if session.cfg.exclude_folders.contains(&d) || d.starts_with('.') {
            continue;
        }
        let is_stack = has_compose(session.fs, &d);
        options.push((d, is_stack));
    }
    if options.is_empty() {
        eprintln!("no environment folders found");
        return Ok(());
    }
    let labels: Vec<String> = options
        .iter()
        .map(|(d, is_stack)| {
            if *is_stack {
                format!("📄 {d} (stack)")
            } else {
                d.clone()
            }
        })
        .collect();
    loop {
        match ui::select("Environment / stack", &labels)? {
            Pick::Back => return Ok(()), // ← at root exits
            Pick::Item(i) => {
                let (dir, is_stack) = &options[i];
                if *is_stack {
                    pick_image(session, dir)?;
                } else {
                    pick_stack(session, dir)?;
                }
            }
        }
    }
}

fn pick_stack(session: &Session, env: &str) -> Result<()> {
    // only subfolders that actually contain a compose file are stacks
    let stacks: Vec<String> = session
        .fs
        .subdirs(env)
        .into_iter()
        .filter(|d| !session.cfg.exclude_subfolders.contains(d))
        .filter(|d| has_compose(session.fs, &format!("{env}/{d}")))
        .collect();
    if stacks.is_empty() {
        eprintln!("  ({env} has no stack subfolders with a docker-compose.yml)");
        return Ok(());
    }
    loop {
        match ui::select(&format!("Stack in {env}"), &stacks)? {
            Pick::Back => return Ok(()),
            Pick::Item(i) => pick_image(session, &format!("{env}/{}", stacks[i]))?,
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
fn pick_image(session: &Session, dir: &str) -> Result<()> {
    let compose_path = format!("{dir}/docker-compose.yml");
    let Some(raw) = session.fs.get(&compose_path) else {
        eprintln!("  (no docker-compose.yml in {dir} — was the repo decrypted?)");
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

    let bf = crate::build::load(session.fs, dir)?;

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
    // Build entries with no service in this compose are still buildable —
    // but only for a stack-level list. An environment catalog covers every
    // stack, so listing its extra entries here would show the whole
    // environment in each stack's menu.
    if let Some(bf) = bf.as_ref().filter(|bf| !bf.catalog) {
        for spec in bf.entries() {
            if !groups.iter().any(|g| g.base == spec.image_name()) {
                groups.push(ImageGroup {
                    base: spec.image_name(),
                    buildable: true,
                    services: Vec::new(),
                });
            }
        }
    }

    match session.mode {
        Mode::Build => pick_image_build(session, dir, &groups, services.len()),
        Mode::Deploy => pick_image_deploy(session, dir, &groups, services.len()),
    }
}

/// BUILD MODE — images are the targets; selecting one builds it directly.
fn pick_image_build(
    session: &Session,
    dir: &str,
    groups: &[ImageGroup],
    n_services: usize,
) -> Result<()> {
    let buildable: Vec<&ImageGroup> = groups.iter().filter(|g| g.buildable).collect();
    if buildable.is_empty() {
        eprintln!("  ({dir} has nothing to build — stock images only)");
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
        match ui::select(&format!("{dir} — build image"), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => {
                run_builds(session, dir, None)?;
            }
            Pick::Item(i) => {
                run_builds(session, dir, Some(&buildable[i - 1].base))?;
            }
        }
    }
}

/// DEPLOY MODE — services are the targets, grouped by their image.
fn pick_image_deploy(
    session: &Session,
    dir: &str,
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
        match ui::select(&format!("{dir} — deploy"), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => {
                run_deploy(session, dir, crate::deploy::Target::WholeStack)?;
            }
            Pick::Item(i) => {
                let group = &groups[i - 1];
                pick_deploy_service(session, dir, group)?;
            }
        }
    }
}

/// DEPLOY MODE, level 4: services running one image.
fn pick_deploy_service(session: &Session, dir: &str, group: &ImageGroup) -> Result<()> {
    let mut options: Vec<String> = vec![format!(
        "🚀 all {} service{} of {}",
        group.services.len(),
        if group.services.len() == 1 { "" } else { "s" },
        group.base
    )];
    options.extend(group.services.iter().map(|s| format!("   {s}")));
    loop {
        match ui::select(&format!("{dir} / {}", group.base), &options)? {
            Pick::Back => return Ok(()),
            Pick::Item(0) => {
                run_deploy(
                    session,
                    dir,
                    crate::deploy::Target::Services(group.services.clone()),
                )?;
            }
            Pick::Item(i) => {
                run_deploy(
                    session,
                    dir,
                    crate::deploy::Target::Services(vec![group.services[i - 1].clone()]),
                )?;
            }
        }
    }
}

/// The hostname gate is enforced HERE — the single entry point to deploys.
pub fn run_deploy(session: &Session, dir: &str, target: crate::deploy::Target) -> Result<()> {
    if !session.deploy_allowed {
        eprintln!(
            "  deploy refused: host '{}' is not this repo's target (devops-<host>)",
            session.host
        );
        return Ok(());
    }
    let report = match crate::deploy::run_deploy(session.fs, dir, target) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("❌ {e:#}");
            return Ok(());
        }
    };
    if let Some(mail_cfg) = &session.cfg.mail {
        let subject = format!(
            "[hefesto] deploy {}: {} [{}]",
            report.stack_name,
            if report.ok { "OK ✅" } else { "FAILED ❌" },
            report.what
        );
        let body = crate::deploy::report_body(&report);
        eprintln!("📧 mailing the deploy report to {}", mail_cfg.to.join(", "));
        if let Err(e) = crate::mail::send_report(mail_cfg, &subject, &body) {
            eprintln!("⚠️  could not mail the report: {e:#}");
        }
    } else {
        eprintln!(
            "ℹ️  deploy report not mailed — no mail configured (add a \"mail\" block to the \
             config or set HEFESTO_MAIL_TO=addr1,addr2)"
        );
    }
    Ok(())
}

/// Build targets are IMAGES: `image = None` builds every entry of the
/// stack's build list, `Some(base)` builds exactly that image once.
pub fn run_builds(session: &Session, dir: &str, image: Option<&str>) -> Result<()> {
    let Some(bf) = crate::build::load(session.fs, dir)? else {
        eprintln!("  no build.yml and no parseable legacy build.sh in {dir} — nothing to build");
        return Ok(());
    };
    let targets: Vec<&crate::build::BuildSpec> = match image {
        // an explicitly disabled entry is documentation only
        Some(base)
            if bf.entries().iter().any(|s| s.image_name() == base && !s.enabled) =>
        {
            eprintln!(
                "  image '{base}' is present for documentation but has enabled: false — not built"
            );
            return Ok(());
        }
        // "whole stack": a stack-level list builds everything it declares;
        // an environment catalog builds only what THIS stack's compose uses.
        None if bf.catalog => {
            let used = compose_images(session.fs, dir);
            let picked: Vec<&crate::build::BuildSpec> = bf
                .entries()
                .iter()
                .filter(|s| s.enabled && used.contains(&s.image_name()))
                .collect();
            if picked.is_empty() {
                eprintln!(
                    "  none of the environment catalog's images are used by {dir}'s compose — nothing to build"
                );
                return Ok(());
            }
            picked
        }
        None => bf.entries().iter().filter(|s| s.enabled).collect(),
        Some(base) => match crate::build::find_by_image(&bf, base) {
            Some(spec) => vec![spec],
            None => {
                eprintln!(
                    "  image '{base}' has no build entry in {dir} — \
                     it's a stock image or built by another stack"
                );
                return Ok(());
            }
        },
    };

    // registry login — once per distinct destination that will be pushed to
    let mut seen = std::collections::BTreeSet::new();
    for spec in targets.iter().filter(|s| s.push) {
        let (name, dest) = bf.destination_for(spec)?;
        if seen.insert(name.to_string()) {
            crate::build::registry_login(name, dest)?;
        }
    }

    let mut reports: Vec<(Option<String>, crate::build::BuildReport)> = Vec::new();
    for spec in &targets {
        let group = spec.mail_group.clone();
        match crate::build::run_build(session.cfg, &bf, spec) {
            Ok(r) => reports.push((group, r)),
            Err(e) => {
                eprintln!("❌ {e:#}");
                reports.push((
                    group,
                    crate::build::BuildReport {
                        image: spec.image_name(),
                        source: spec.display_name(),
                        ok: false,
                        duration_secs: 0,
                        log: format!("{e:#}"),
                    },
                ));
            }
        }
    }
    let ok = reports.iter().filter(|(_, r)| r.ok).count();
    eprintln!("\n🏁 {ok}/{} builds succeeded in {dir}", reports.len());

    mail_build_reports(session, dir, &bf, reports);
    Ok(())
}

/// Mail routing. With mailGroups defined: one mail per group covering its
/// entries; entries without a mailGroup send nothing. Without mailGroups:
/// the global config `mail` block (or HEFESTO_MAIL_TO) gets everything.
fn mail_build_reports(
    session: &Session,
    dir: &str,
    bf: &crate::build::BuildFile,
    reports: Vec<(Option<String>, crate::build::BuildReport)>,
) {
    let send = |to: Vec<String>, reports: Vec<&crate::build::BuildReport>| {
        let ok = reports.iter().filter(|r| r.ok).count();
        let subject = format!(
            "[hefesto] {dir}: {ok}/{} builds {}",
            reports.len(),
            if ok == reports.len() { "OK ✅" } else { "with FAILURES ❌" }
        );
        let body = crate::build::report_body_refs(dir, &reports);
        let cfg = crate::config::mailcfg_for(to);
        if let Err(e) = crate::mail::send_report(&cfg, &subject, &body) {
            eprintln!("⚠️  could not mail the report: {e:#}");
        }
    };

    if !bf.mail_groups.is_empty() {
        let mut by_group: std::collections::BTreeMap<String, Vec<&crate::build::BuildReport>> =
            Default::default();
        let mut unrouted = 0;
        for (group, report) in &reports {
            match group {
                Some(g) => by_group.entry(g.clone()).or_default().push(report),
                None => unrouted += 1,
            }
        }
        for (group, grouped) in by_group {
            match bf.mail_groups.get(&group) {
                Some(recipients) if !recipients.is_empty() => {
                    send(recipients.clone(), grouped);
                }
                _ => eprintln!("⚠️  mailGroup '{group}' is not defined in mailGroups — skipped"),
            }
        }
        if unrouted > 0 {
            eprintln!("ℹ️  {unrouted} build(s) have no mailGroup — no mail for them");
        }
    } else if let Some(mail_cfg) = &session.cfg.mail {
        let refs: Vec<&crate::build::BuildReport> = reports.iter().map(|(_, r)| r).collect();
        let ok = refs.iter().filter(|r| r.ok).count();
        let subject = format!(
            "[hefesto] {dir}: {ok}/{} builds {}",
            refs.len(),
            if ok == refs.len() { "OK ✅" } else { "with FAILURES ❌" }
        );
        let body = crate::build::report_body_refs(dir, &refs);
        if let Err(e) = crate::mail::send_report(mail_cfg, &subject, &body) {
            eprintln!("⚠️  could not mail the report: {e:#}");
        }
    } else {
        eprintln!(
            "ℹ️  report not mailed — no mailGroups in build.yml and no global mail config"
        );
    }
}
