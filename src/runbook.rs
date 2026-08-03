//! Runbook generator: merges, per stack,
//!   docker-compose.yml -> infrastructure facts (never drifts)
//!   build.yml          -> provenance (source repo, branch, tag, registry)
//!   stack.md           -> human prose (the only hand-written part)
//! and renders Markdown + print-ready HTML (+ PDF when a Chrome/Chromium
//! binary is available). Everything is read from the in-memory vault.

use crate::config::Config;
use crate::vault::MemFs;
use anyhow::{Context, Result};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub struct Service {
    pub service: String,
    pub stack: String,
    pub image: String,
    pub image_base: String,
    pub typ: String,
    pub urls: Vec<String>,
    pub https: String,
    pub priority: String,
    pub ports: Vec<String>,
    pub env_files: Vec<String>,
    pub volumes: Vec<String>,
    pub networks: Vec<String>,
    pub replicas: String,
    pub command: String,
    pub desc: String,
    pub repo_url: String,
    pub branch: String,
    pub tag: String,
    pub registry: String,
}

pub struct Stack {
    pub name: String,
    pub dir: String,
    pub market: String,
    pub environment: String,
    pub owner: String,
    pub compose: String,
    pub networks: Vec<String>,
    pub volumes: Vec<String>,
    pub secrets: Vec<String>,
    pub desc: String,
    pub services: Vec<Service>,
}

impl Stack {
    pub fn env_label(&self) -> String {
        format!("{} {}", self.market, self.environment).trim().to_string()
    }
}

/// "brprod" -> ("BR","PROD"), "zauat" -> ("ZA","UAT")
fn market_env(env_dir: &str) -> (String, String) {
    let d = env_dir.to_lowercase();
    for suffix in ["prod", "uat", "dev", "qa"] {
        if let Some(m) = d.strip_suffix(suffix) {
            if m.len() == 2 {
                return (m.to_uppercase(), suffix.to_uppercase());
            }
        }
    }
    (String::new(), String::new())
}

/// stack.md -> (front matter, stack description, section map)
/// sections are keyed by service name or "image:<name>".
fn parse_stack_md(raw: &[u8]) -> (BTreeMap<String, String>, String, BTreeMap<String, String>) {
    let text = String::from_utf8_lossy(raw).to_string();
    let mut fm = BTreeMap::new();
    let mut body = text.as_str();
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some((k, v)) = line.split_once(':') {
                    fm.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            body = &rest[end + 4..];
        }
    }
    let mut stack_desc = String::new();
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut buf: Vec<&str> = Vec::new();
    let flush = |cur: &Option<String>, buf: &Vec<&str>, sd: &mut String, map: &mut BTreeMap<String, String>| {
        let text = buf.join("\n").trim().to_string();
        match cur {
            Some(key) => {
                map.insert(key.clone(), text);
            }
            None => *sd = text,
        }
    };
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            flush(&current, &buf, &mut stack_desc, &mut sections);
            buf.clear();
            current = Some(title.trim().to_string());
        } else if line.starts_with("# ") && current.is_none() {
            continue; // the H1 is the stack name
        } else {
            buf.push(line);
        }
    }
    flush(&current, &buf, &mut stack_desc, &mut sections);
    (fm, stack_desc, sections)
}

fn labels_of(svc: &Value) -> String {
    let mut out = String::new();
    if let Some(labels) = svc.get("deploy").and_then(|d| d.get("labels")) {
        match labels {
            Value::Sequence(seq) => {
                for l in seq {
                    if let Some(s) = l.as_str() {
                        out.push_str(s);
                        out.push('\n');
                    }
                }
            }
            Value::Mapping(map) => {
                for (k, v) in map {
                    out.push_str(&format!(
                        "{}={}\n",
                        k.as_str().unwrap_or(""),
                        v.as_str().unwrap_or("")
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// Everything between the first pair of `chars` after each `needle`.
fn extract_all(haystack: &str, needle: &str, open: char, close: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = haystack;
    while let Some(i) = rest.find(needle) {
        rest = &rest[i + needle.len()..];
        if let Some(a) = rest.find(open) {
            if let Some(b) = rest[a + 1..].find(close) {
                out.push(rest[a + 1..a + 1 + b].to_string());
            }
        }
    }
    out
}

fn values_after(haystack: &str, needle: &str) -> Vec<String> {
    haystack
        .lines()
        .filter_map(|l| l.split_once(needle))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .collect()
}

fn seq_strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Sequence(s)) => s
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                Value::Mapping(m) => m
                    .get(Value::from("source"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Mapping(m)) => m
            .keys()
            .filter_map(|k| k.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn collect(fs: &MemFs, cfg: &Config) -> Vec<Stack> {
    let mut stacks = Vec::new();
    let roots: Vec<String> = fs
        .subdirs("")
        .into_iter()
        .filter(|d| !cfg.exclude_folders.contains(d) && !d.starts_with('.') && d != "docs")
        .collect();

    for root in roots {
        let dirs: Vec<String> = if fs.get(&format!("{root}/docker-compose.yml")).is_some() {
            vec![root.clone()]
        } else {
            fs.subdirs(&root)
                .into_iter()
                .map(|s| format!("{root}/{s}"))
                .filter(|d| fs.get(&format!("{d}/docker-compose.yml")).is_some())
                .collect()
        };
        for dir in dirs {
            let Some(raw) = fs.get(&format!("{dir}/docker-compose.yml")) else { continue };
            let Ok(compose) = serde_yaml::from_slice::<Value>(raw) else { continue };
            let env_dir = dir.split('/').next().unwrap_or("").to_string();
            let (mut market, mut environment) = market_env(&env_dir);
            let (fm, stack_desc, sections) = fs
                .get(&format!("{dir}/stack.md"))
                .map(parse_stack_md)
                .unwrap_or_default();
            if let Some(m) = fm.get("market") {
                market = m.clone();
            }
            if let Some(e) = fm.get("environment") {
                environment = e.clone();
            }
            let builds = crate::build::load(fs, &dir).ok().flatten();

            let mut services = Vec::new();
            if let Some(map) = compose.get("services").and_then(|s| s.as_mapping()) {
                for (k, svc) in map {
                    let name = k.as_str().unwrap_or("").to_string();
                    let labels = labels_of(svc);
                    let image = svc.get("image").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let base = crate::build::image_base(&image).unwrap_or_default();
                    let urls: Vec<String> = {
                        let mut u: Vec<String> = extract_all(&labels, "Host(", '`', '`');
                        u.sort();
                        u.dedup();
                        u
                    };
                    let ports = values_after(&labels, "loadbalancer.server.port=");
                    let schemes = values_after(&labels, "loadbalancer.server.scheme=");
                    let typ = if schemes.iter().any(|s| s == "h2c") {
                        "grpc"
                    } else if !urls.is_empty() || !ports.is_empty() {
                        "http"
                    } else {
                        ""
                    };
                    let https = if labels.contains("certresolver") {
                        "yes"
                    } else if !urls.is_empty() {
                        "no"
                    } else {
                        ""
                    };
                    let replicas = if svc.get("deploy").and_then(|d| d.get("mode")).and_then(|m| m.as_str())
                        == Some("global")
                    {
                        "global".to_string()
                    } else {
                        svc.get("deploy")
                            .and_then(|d| d.get("replicas"))
                            .map(|r| r.as_u64().unwrap_or(1).to_string())
                            .unwrap_or_else(|| "1".into())
                    };
                    let command = match svc.get("command") {
                        Some(Value::Sequence(s)) => s
                            .iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                        Some(Value::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let spec = builds.as_ref().and_then(|bf| crate::build::find_by_image(bf, &base));
                    let (repo_url, branch, tag, registry) = match (spec, builds.as_ref()) {
                        (Some(s), Some(bf)) => {
                            let dest = bf.destination_for(s).ok();
                            (
                                s.repo_url.clone().unwrap_or_else(|| {
                                    if s.repository.is_empty() { String::new() }
                                    else { format!("{}/{}", s.project, s.repository) }
                                }),
                                if s.enabled { s.branch.clone() } else { format!("{} (build disabled)", s.branch) },
                                s.tag.clone(),
                                dest.map(|(_, d)| format!("{}/{}", d.host, d.namespace)).unwrap_or_default(),
                            )
                        }
                        _ => (String::new(), String::new(), String::new(), String::new()),
                    };
                    let desc = sections
                        .get(&name)
                        .or_else(|| sections.get(&format!("image:{base}")))
                        .cloned()
                        .unwrap_or_default();
                    services.push(Service {
                        service: name,
                        stack: dir.replace('/', "-"),
                        image,
                        image_base: base,
                        typ: typ.into(),
                        urls,
                        https: https.into(),
                        priority: values_after(&labels, ".priority=").join(", "),
                        ports,
                        env_files: seq_strings(svc.get("env_file")),
                        volumes: seq_strings(svc.get("volumes")),
                        networks: seq_strings(svc.get("networks")),
                        replicas,
                        command,
                        desc,
                        repo_url,
                        branch,
                        tag,
                        registry,
                    });
                }
            }
            stacks.push(Stack {
                name: dir.replace('/', "-"),
                dir: dir.clone(),
                market,
                environment,
                owner: fm.get("owner").cloned().unwrap_or_default(),
                compose: format!("{dir}/docker-compose.yml"),
                networks: seq_strings(compose.get("networks")),
                volumes: seq_strings(compose.get("volumes")),
                secrets: seq_strings(compose.get("secrets")),
                desc: stack_desc,
                services,
            });
        }
    }
    stacks
}

fn lst(v: &[String]) -> String {
    if v.is_empty() { "—".into() } else { v.join(", ") }
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
fn anchor(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect()
}

fn envs_of(stacks: &[Stack]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in stacks {
        let e = s.env_label();
        if !e.is_empty() && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

pub fn render_markdown(stacks: &[Stack], repo: &str, today: &str) -> String {
    let svc: Vec<&Service> = stacks.iter().flat_map(|s| s.services.iter()).collect();
    let documented = svc.iter().filter(|x| !x.desc.is_empty()).count();
    let mut m = String::new();
    m.push_str(&format!("# DevOps Platform Runbook — {repo}\n\n"));
    m.push_str(&format!(
        "_Generated by hefesto on {today}. Facts from docker-compose.yml, provenance from build.yml, \
         descriptions from stack.md._\n\n**{} stacks · {} services · {documented} documented ({}%)**\n\n",
        stacks.len(),
        svc.len(),
        documented * 100 / svc.len().max(1)
    ));
    m.push_str("## Contents\n\n");
    for s in stacks {
        m.push_str(&format!("- [{}](#{}) — {} services\n", s.name, anchor(&s.name), s.services.len()));
    }
    for s in stacks {
        m.push_str(&format!("\n---\n\n## {}\n\n", s.name));
        m.push_str(&format!(
            "**Market:** {} · **Environment:** {} · **Compose:** `{}`\n\n",
            if s.market.is_empty() { "—" } else { &s.market },
            if s.environment.is_empty() { "—" } else { &s.environment },
            s.compose
        ));
        if !s.desc.is_empty() {
            m.push_str(&s.desc);
            m.push_str("\n\n");
        }
        m.push_str(&format!("**Networks:** {}\n\n", lst(&s.networks)));
        m.push_str("| Service | Type | Image | Replicas | URLs |\n|---|---|---|---|---|\n");
        for x in &s.services {
            m.push_str(&format!(
                "| `{}` | {} | `{}` | {} | {} |\n",
                x.service,
                if x.typ.is_empty() { "—" } else { &x.typ },
                if x.image_base.is_empty() { "—" } else { &x.image_base },
                x.replicas,
                lst(&x.urls)
            ));
        }
        for x in &s.services {
            m.push_str(&format!("\n### {}\n\n", x.service));
            if !x.desc.is_empty() {
                m.push_str(&x.desc);
                m.push_str("\n\n");
            }
            m.push_str("| Field | Value |\n|---|---|\n");
            let rows: Vec<(&str, String)> = vec![
                ("Image", x.image.clone()),
                ("Type", x.typ.clone()),
                ("URL(s)", lst(&x.urls)),
                ("HTTPS certificate", x.https.clone()),
                ("Router priority", x.priority.clone()),
                ("Port(s)", lst(&x.ports)),
                ("Replicas", x.replicas.clone()),
                ("Networks", lst(&x.networks)),
                ("Volumes", lst(&x.volumes)),
                ("Environment files", lst(&x.env_files)),
                ("Command", x.command.clone()),
                ("Source repository", x.repo_url.clone()),
                ("Branch", x.branch.clone()),
                ("Image tag", x.tag.clone()),
                ("Registry", x.registry.clone()),
            ];
            for (k, v) in rows.into_iter().filter(|(_, v)| !v.is_empty()) {
                m.push_str(&format!("| {k} | {} |\n", v.replace('|', "\\|")));
            }
        }
    }
    m
}

const CSS: &str = r#"
@page { size: Letter; margin: 16mm 14mm; }
:root { --navy:#1f3864; --accent:#2f5597; --grey:#6b7280; --line:#d7dee9; --soft:#f6f8fc; }
* { box-sizing:border-box; }
body { font-family:"Helvetica Neue",Arial,sans-serif; color:#111827; font-size:9.6pt; line-height:1.45; margin:0; }
h1,h2,h3 { color:var(--navy); margin:0 0 6px; line-height:1.25; }
h1 { font-size:19pt; border-bottom:2.5px solid var(--navy); padding-bottom:5px; }
h2 { font-size:13pt; margin-top:16px; }
h3 { font-size:10.8pt; color:var(--accent); margin-top:13px; }
p { margin:0 0 7px; } ul { margin:0 0 8px 16px; padding:0; } li { margin-bottom:3px; }
.cover { height:245mm; display:flex; flex-direction:column; justify-content:center; align-items:center; text-align:center; }
.cover .t { font-size:34pt; font-weight:700; color:var(--navy); letter-spacing:-.5px; }
.cover .rule { background:linear-gradient(90deg,#c8973e,#e0b45c); }
.cover .s { font-size:15pt; color:var(--grey); margin-top:10px; letter-spacing:3px; text-transform:uppercase; }
.cover .rule { width:70mm; height:3px; background:var(--navy); margin:22px 0; }
.cover .stats { display:flex; gap:26px; margin-top:8px; }
.cover .stat b { display:block; font-size:20pt; color:var(--accent); }
.cover .stat span { font-size:8.5pt; color:var(--grey); text-transform:uppercase; letter-spacing:1px; }
.cover .logo { width:86mm; margin-bottom:12mm; border-radius:3mm; }
.cover .meta { margin-top:30mm; font-size:9pt; color:var(--grey); }
.page { page-break-before:always; }
.lead { color:var(--grey); font-size:9pt; margin-bottom:10px; }
.badge { display:inline-block; background:var(--soft); border:1px solid var(--line); border-radius:9px; padding:1px 8px; font-size:8.2pt; color:var(--accent); margin-right:5px; }
table { border-collapse:collapse; width:100%; margin:6px 0 12px; } tr { page-break-inside:avoid; }
.grid th { background:var(--navy); color:#fff; font-weight:600; text-align:left; padding:5px 7px; font-size:8.6pt; }
.grid td { border:1px solid var(--line); padding:4px 7px; font-size:8.6pt; vertical-align:top; word-break:break-word; }
.grid tbody tr:nth-child(even) td { background:#fafcff; }
.facts th { width:24%; background:var(--soft); border:1px solid var(--line); text-align:left; padding:4px 7px; font-size:8.6pt; font-weight:600; color:#374151; vertical-align:top; }
.facts td { border:1px solid var(--line); padding:4px 7px; font-size:8.6pt; word-break:break-word; }
.svc { page-break-inside:avoid; border-left:3px solid var(--accent); padding-left:9px; margin:14px 0 4px; }
.todo { color:#9ca3af; font-style:italic; }
.toc a { color:#111827; text-decoration:none; }
.toc .l1 { font-weight:600; color:var(--navy); margin-top:7px; }
.toc .l2 { margin-left:14px; color:#374151; font-size:9pt; }
code { font-family:"SF Mono",Menlo,monospace; font-size:8.4pt; background:var(--soft); padding:0 3px; border-radius:3px; }
"#;

/// Logo embedded at compile time so a generated runbook is self-contained
/// (the HTML has no external references and the PDF renders offline).
const LOGO_PNG: &[u8] = include_bytes!("../docs/brand/hefesto.logo-small.png");

fn logo_data_uri() -> String {
    use base64::Engine;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(LOGO_PNG)
    )
}

fn html_prose(text: &str) -> String {
    if text.trim().is_empty() {
        return "<p class=\"todo\">No description yet — add one in this stack's stack.md.</p>".into();
    }
    let mut out = String::new();
    for block in text.split("\n\n") {
        let b = block.trim();
        if b.is_empty() {
            continue;
        }
        if b.starts_with("- ") || b.starts_with("* ") {
            out.push_str("<ul>");
            for line in b.lines() {
                let li = line.trim_start_matches(['-', '*', ' ']).trim();
                if !li.is_empty() {
                    out.push_str(&format!("<li>{}</li>", esc(li)));
                }
            }
            out.push_str("</ul>");
        } else if let Some(t) = b.strip_prefix("# ") {
            out.push_str(&format!("<h2>{}</h2>", esc(t)));
        } else if let Some(t) = b.strip_prefix("## ") {
            out.push_str(&format!("<h3>{}</h3>", esc(t)));
        } else {
            out.push_str(&format!("<p>{}</p>", esc(&b.replace('\n', " "))));
        }
    }
    out
}

fn facts_table(rows: &[(&str, String)]) -> String {
    let body: String = rows
        .iter()
        .filter(|(_, v)| !v.is_empty() && v != "—")
        .map(|(k, v)| format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(v)))
        .collect();
    format!("<table class=\"facts\">{body}</table>")
}

fn grid_table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let head: String = headers.iter().map(|h| format!("<th>{}</th>", esc(h))).collect();
    let body: String = rows
        .iter()
        .map(|r| {
            let tds: String = r.iter().map(|c| format!("<td>{}</td>", esc(c))).collect();
            format!("<tr>{tds}</tr>")
        })
        .collect();
    format!("<table class=\"grid\"><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>")
}

pub fn render_html(stacks: &[Stack], repo: &str, today: &str, intro: &str) -> String {
    let svc: Vec<&Service> = stacks.iter().flat_map(|s| s.services.iter()).collect();
    let documented = svc.iter().filter(|x| !x.desc.is_empty()).count();
    let with_build = svc.iter().filter(|x| !x.repo_url.is_empty()).count();
    let images: BTreeSet<&String> = svc.iter().filter(|x| !x.repo_url.is_empty()).map(|x| &x.image_base).collect();
    let envs = envs_of(stacks);
    let mut h = String::new();
    h.push_str(&format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>DevOps Platform Runbook — {}</title><style>{CSS}</style></head><body>",
        esc(repo)
    ));
    // cover
    h.push_str(&format!(
        "<div class=\"cover\"><img class=\"logo\" src=\"{}\" alt=\"hefesto\">\
         <div class=\"t\">DevOps Platform Runbook</div><div class=\"rule\"></div>\
         <div class=\"s\">{}</div><div class=\"stats\">\
         <div class=\"stat\"><b>{}</b><span>Stacks</span></div>\
         <div class=\"stat\"><b>{}</b><span>Services</span></div>\
         <div class=\"stat\"><b>{}</b><span>Built from source</span></div>\
         <div class=\"stat\"><b>{}</b><span>Images</span></div></div>\
         <div class=\"meta\">{} &nbsp;·&nbsp; generated automatically by hefesto on {today}</div></div>",
        logo_data_uri(),
        esc(&envs.join(" · ")), stacks.len(), svc.len(), with_build, images.len(), esc(repo)
    ));
    // contents
    h.push_str("<div class=\"page\"><h1>Contents</h1><div class=\"toc\">");
    h.push_str("<div class=\"l1\"><a href=\"#about\">About this document</a></div>");
    if !intro.trim().is_empty() {
        h.push_str("<div class=\"l1\"><a href=\"#platform\">Platform overview</a></div>");
    }
    for env in &envs {
        h.push_str(&format!(
            "<div class=\"l1\"><a href=\"#env-{}\">Environment: {}</a></div>",
            anchor(env), esc(env)
        ));
        for s in stacks.iter().filter(|s| &s.env_label() == env) {
            h.push_str(&format!(
                "<div class=\"l2\"><a href=\"#{}\">{} — {} services</a></div>",
                anchor(&s.name), esc(&s.name), s.services.len()
            ));
        }
    }
    for (id, t) in [("appx-a", "Appendix A — Endpoint index"), ("appx-b", "Appendix B — Build catalog"), ("appx-c", "Appendix C — Documentation gaps")] {
        h.push_str(&format!("<div class=\"l1\"><a href=\"#{id}\">{t}</a></div>"));
    }
    h.push_str("</div></div>");
    // about
    h.push_str(&format!(
        "<div class=\"page\"><h1 id=\"about\">About this document</h1>\
        <p>This runbook is generated, not maintained by hand. Every fact is read from the deployment \
        repository at generation time, so it cannot drift from what is actually deployed.</p><ul>\
        <li><b>Infrastructure facts</b> — services, images, networks, volumes, secrets, routing, replicas, \
        ports and environment files — come from each stack's <code>docker-compose.yml</code>.</li>\
        <li><b>Build provenance</b> — source repository, branch, tag and registry — comes from <code>build.yml</code>.</li>\
        <li><b>Descriptions</b> come from the <code>stack.md</code> beside each stack: the only hand-written part.</li></ul>\
        <p class=\"lead\">Coverage: {documented} of {} services carry a description ({}%). Appendix C lists what is missing.</p></div>",
        svc.len(), documented * 100 / svc.len().max(1)
    ));
    if !intro.trim().is_empty() {
        h.push_str(&format!(
            "<div class=\"page\"><h1 id=\"platform\">Platform overview</h1>{}</div>",
            html_prose(intro)
        ));
    }
    // environments and stacks
    for env in &envs {
        let es: Vec<&Stack> = stacks.iter().filter(|s| &s.env_label() == env).collect();
        h.push_str(&format!(
            "<div class=\"page\"><h1 id=\"env-{}\">Environment: {}</h1><p class=\"lead\">{} stacks · {} services</p>",
            anchor(env), esc(env), es.len(), es.iter().map(|s| s.services.len()).sum::<usize>()
        ));
        h.push_str(&grid_table(
            &["Stack", "Services", "Compose file", "Description"],
            es.iter()
                .map(|s| vec![
                    s.name.clone(),
                    s.services.len().to_string(),
                    s.compose.clone(),
                    s.desc.lines().next().unwrap_or("—").chars().take(150).collect(),
                ])
                .collect(),
        ));
        h.push_str("</div>");
        for s in es {
            h.push_str(&format!(
                "<div class=\"page\"><h1 id=\"{}\">{}</h1><p><span class=\"badge\">{}</span>\
                 <span class=\"badge\">{}</span><span class=\"badge\">{} services</span></p>{}",
                anchor(&s.name), esc(&s.name),
                esc(if s.market.is_empty() { "—" } else { &s.market }),
                esc(if s.environment.is_empty() { "—" } else { &s.environment }),
                s.services.len(), html_prose(&s.desc)
            ));
            h.push_str(&facts_table(&[
                ("Compose file", s.compose.clone()),
                ("Networks", lst(&s.networks)),
                ("Volumes", lst(&s.volumes)),
                ("Secrets", lst(&s.secrets)),
                ("Owner", s.owner.clone()),
            ]));
            h.push_str("<h2>Services</h2>");
            h.push_str(&grid_table(
                &["Service", "Type", "Image", "Replicas", "URLs"],
                s.services.iter().map(|x| vec![
                    x.service.clone(),
                    if x.typ.is_empty() { "—".into() } else { x.typ.clone() },
                    if x.image_base.is_empty() { "—".into() } else { x.image_base.clone() },
                    x.replicas.clone(),
                    lst(&x.urls),
                ]).collect(),
            ));
            for x in &s.services {
                h.push_str(&format!("<div class=\"svc\"><h3>{}</h3>{}", esc(&x.service), html_prose(&x.desc)));
                h.push_str(&facts_table(&[
                    ("Image", x.image.clone()),
                    ("Type", x.typ.clone()),
                    ("URL(s)", lst(&x.urls)),
                    ("HTTPS certificate", x.https.clone()),
                    ("Router priority", x.priority.clone()),
                    ("Port(s)", lst(&x.ports)),
                    ("Replicas", x.replicas.clone()),
                    ("Networks", lst(&x.networks)),
                    ("Volumes", lst(&x.volumes)),
                    ("Environment files", lst(&x.env_files)),
                    ("Command", x.command.clone()),
                    ("Source repository", x.repo_url.clone()),
                    ("Branch", x.branch.clone()),
                    ("Image tag", x.tag.clone()),
                    ("Registry", x.registry.clone()),
                ]));
                h.push_str("</div>");
            }
            h.push_str("</div>");
        }
    }
    // appendix A
    let mut urls: Vec<Vec<String>> = svc
        .iter()
        .flat_map(|x| x.urls.iter().map(move |u| vec![u.clone(), x.typ.clone(), x.service.clone(), x.stack.clone()]))
        .collect();
    urls.sort();
    h.push_str(&format!(
        "<div class=\"page\"><h1 id=\"appx-a\">Appendix A — Endpoint index</h1>\
         <p class=\"lead\">{} hostnames published through Traefik, alphabetically.</p>{}</div>",
        urls.len(),
        grid_table(&["Hostname", "Type", "Service", "Stack"], urls)
    ));
    // appendix B
    let mut seen = BTreeSet::new();
    let mut brows: Vec<Vec<String>> = Vec::new();
    for x in &svc {
        if !x.repo_url.is_empty() && seen.insert(x.image_base.clone()) {
            let users = svc.iter().filter(|y| y.image_base == x.image_base).count();
            brows.push(vec![
                x.image_base.clone(),
                x.repo_url.replace("https://", ""),
                x.branch.clone(),
                x.tag.clone(),
                users.to_string(),
            ]);
        }
    }
    brows.sort();
    h.push_str(&format!(
        "<div class=\"page\"><h1 id=\"appx-b\">Appendix B — Build catalog</h1>\
         <p class=\"lead\">{} images built from source.</p>{}</div>",
        brows.len(),
        grid_table(&["Image", "Source repository", "Branch", "Tag", "Used by"], brows)
    ));
    // appendix C
    let mut gaps: Vec<Vec<String>> = stacks
        .iter()
        .filter(|s| s.desc.trim().is_empty())
        .map(|s| vec![s.name.clone(), "—".into(), "Stack description missing".into()])
        .collect();
    gaps.extend(
        svc.iter()
            .filter(|x| x.desc.trim().is_empty())
            .map(|x| vec![x.stack.clone(), x.service.clone(), "Service description missing".into()]),
    );
    h.push_str(&format!(
        "<div class=\"page\"><h1 id=\"appx-c\">Appendix C — Documentation gaps</h1>\
         <p class=\"lead\">{} items need a description; add it to that stack's stack.md and regenerate.</p>{}</div>",
        gaps.len(),
        if gaps.is_empty() { "<p>No gaps.</p>".to_string() } else { grid_table(&["Stack", "Service", "Gap"], gaps) }
    ));
    h.push_str("</body></html>");
    h
}

/// First Chrome/Chromium binary available, for HTML -> PDF.
fn find_chrome() -> Option<String> {
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
        if std::process::Command::new(c)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(c.to_string());
        }
    }
    None
}

/// Generate the runbook files. Returns the paths written.
pub fn generate(fs: &MemFs, cfg: &Config, out_dir: &str, today: &str) -> Result<Vec<PathBuf>> {
    let stacks = collect(fs, cfg);
    anyhow::ensure!(!stacks.is_empty(), "no stacks with a docker-compose.yml were found");
    let repo = cfg.repo.repository.clone();
    let intro = fs
        .get("docs/00-platform.md")
        .map(|raw| {
            let t = String::from_utf8_lossy(raw).to_string();
            match t.strip_prefix("---\n").and_then(|r| r.find("\n---").map(|i| r[i + 4..].to_string())) {
                Some(body) => body,
                None => t,
            }
        })
        .unwrap_or_default();

    std::fs::create_dir_all(out_dir).with_context(|| format!("cannot create '{out_dir}'"))?;
    let md_path = Path::new(out_dir).join("runbook.md");
    let html_path = Path::new(out_dir).join("runbook.html");
    std::fs::write(&md_path, render_markdown(&stacks, &repo, today))?;
    std::fs::write(&html_path, render_html(&stacks, &repo, today, &intro))?;
    let mut written = vec![md_path, html_path.clone()];

    let n_svc: usize = stacks.iter().map(|s| s.services.len()).sum();
    eprintln!("📘 runbook: {} stacks, {n_svc} services", stacks.len());

    match find_chrome() {
        Some(chrome) => {
            let pdf_path = Path::new(out_dir).join("runbook.pdf");
            let status = std::process::Command::new(&chrome)
                .args([
                    "--headless",
                    "--disable-gpu",
                    "--no-pdf-header-footer",
                    "--generate-pdf-document-outline",
                    &format!("--print-to-pdf={}", pdf_path.display()),
                    &format!("file://{}", std::fs::canonicalize(&html_path)?.display()),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if status.map(|s| s.success()).unwrap_or(false) && pdf_path.exists() {
                written.push(pdf_path);
            } else {
                eprintln!("   ⚠️  PDF rendering failed — markdown and HTML were still written");
            }
        }
        None => eprintln!("   ℹ️  no Chrome/Chromium found — PDF skipped (markdown + HTML written)"),
    }
    for p in &written {
        eprintln!("   ✅ {}", p.display());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stack_md() {
        let raw = b"---\nmarket: BR\nowner: DevOps\n---\n# brprod-admin\n\nStack intro line.\n\n## svc-a\n\nDescription A.\n\n## image:shared\n\nShared text.\n";
        let (fm, desc, secs) = parse_stack_md(raw);
        assert_eq!(fm.get("market").unwrap(), "BR");
        assert_eq!(fm.get("owner").unwrap(), "DevOps");
        assert_eq!(desc, "Stack intro line.");
        assert_eq!(secs.get("svc-a").unwrap(), "Description A.");
        assert_eq!(secs.get("image:shared").unwrap(), "Shared text.");
    }

    #[test]
    fn derives_market_and_env() {
        assert_eq!(market_env("brprod"), ("BR".into(), "PROD".into()));
        assert_eq!(market_env("zauat"), ("ZA".into(), "UAT".into()));
        assert_eq!(market_env("system"), (String::new(), String::new()));
    }

    #[test]
    fn extracts_hosts_from_labels() {
        let labels = "traefik.http.routers.x.rule=Host(`a.example.com`) || Host(`b.example.com`)\n\
                      traefik.http.services.x.loadbalancer.server.port=8080\n";
        let mut hosts = extract_all(labels, "Host(", '`', '`');
        hosts.sort();
        assert_eq!(hosts, vec!["a.example.com", "b.example.com"]);
        assert_eq!(values_after(labels, "loadbalancer.server.port="), vec!["8080"]);
    }
}
