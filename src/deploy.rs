//! Milestone 4 — deploy engine.
//!
//! The decrypted repo lives only in RAM, so the compose file can't
//! reference env files on disk. Before deploying, every service's
//! `env_file:` list is resolved AGAINST THE IN-MEMORY VAULT and merged
//! into its `environment:` (compose precedence preserved: later files
//! override earlier ones, explicit environment: entries win over all).
//! The self-contained compose is then piped to
//!     docker stack deploy --resolve-image always --with-registry-auth \
//!                         --detach=false -c -  <env>-<stack>
//! so nothing plaintext ever touches disk. Output is streamed live and
//! captured for the report, exactly like builds.

use crate::vault::MemFs;
use anyhow::{Context, Result, bail};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::process::Command;

pub enum Target {
    WholeStack,
    Services(Vec<String>),
}

pub struct DeployReport {
    pub stack_name: String,
    pub what: String,
    pub ok: bool,
    pub duration_secs: u64,
    pub log: String,
}

/// Normalize a path relative to `base` ("zauat/admin"), resolving "..".
/// "var_x.env" -> "zauat/admin/var_x.env"; "../conf/y.env" -> "zauat/conf/y.env".
pub fn resolve_rel(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// KEY=VALUE parser matching compose env_file semantics (and the ed.sh
/// wrapper): skip blanks/comments, strip one layer of quotes.
fn parse_env(content: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(content).lines() {
        let line = line.trim_end_matches('\r');
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || !t.contains('=') {
            continue;
        }
        let (k, v) = t.split_once('=').unwrap();
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let mut v = v.to_string();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v = v[1..v.len() - 1].to_string();
        }
        out.push((k.to_string(), v));
    }
    out
}

/// `docker stack deploy` interpolates ${...} in the compose — literal
/// dollars in injected values must be escaped as $$ to survive.
fn escape_dollars(s: &str) -> String {
    s.replace('$', "$$")
}

/// Build the self-contained compose: services filtered to `target`,
/// env_file entries resolved from the vault and folded into environment.
/// `dir` is the stack folder ("zauat/admin" or root-level "system").
pub fn prepare_compose(fs: &MemFs, dir: &str, target: &Target) -> Result<String> {
    let base = dir.to_string();
    let compose_path = format!("{base}/docker-compose.yml");
    let raw = fs
        .get(&compose_path)
        .ok_or_else(|| anyhow::anyhow!("no docker-compose.yml in {base}"))?;
    let mut doc: Value = serde_yaml::from_slice(raw)
        .with_context(|| format!("cannot parse {compose_path}"))?;

    let services = doc
        .get_mut("services")
        .and_then(|s| s.as_mapping_mut())
        .ok_or_else(|| anyhow::anyhow!("{compose_path} has no services"))?;

    // filter to the requested services
    if let Target::Services(keep) = target {
        let all: Vec<String> = services
            .keys()
            .filter_map(|k| k.as_str().map(String::from))
            .collect();
        for name in all {
            if !keep.contains(&name) {
                services.remove(Value::String(name));
            }
        }
        if services.is_empty() {
            bail!("none of the requested services exist in {compose_path}");
        }
    }

    // fold env_file contents into environment
    let names: Vec<Value> = services.keys().cloned().collect();
    for name in names {
        let svc = services.get_mut(&name).unwrap();
        let Some(svc_map) = svc.as_mapping_mut() else { continue };

        let env_files: Vec<String> = match svc_map.get(Value::from("env_file")) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Sequence(seq)) => seq
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => Vec::new(),
        };
        if env_files.is_empty() {
            continue;
        }

        let mut merged: BTreeMap<String, String> = BTreeMap::new();
        for ef in &env_files {
            let path = resolve_rel(&base, ef);
            let content = fs.get(&path).ok_or_else(|| {
                anyhow::anyhow!(
                    "service '{}': env file '{ef}' ('{path}') not found in the decrypted repo",
                    name.as_str().unwrap_or("?")
                )
            })?;
            for (k, v) in parse_env(content) {
                merged.insert(k, v); // later files override earlier ones
            }
        }

        // explicit environment: entries win over env_file values
        match svc_map.get(Value::from("environment")) {
            Some(Value::Mapping(m)) => {
                for (k, v) in m {
                    if let (Some(ks), Some(vs)) = (k.as_str(), yaml_scalar_to_string(v)) {
                        merged.insert(ks.to_string(), vs);
                    }
                }
            }
            Some(Value::Sequence(seq)) => {
                for item in seq {
                    if let Some(s) = item.as_str() {
                        if let Some((k, v)) = s.split_once('=') {
                            merged.insert(k.trim().to_string(), v.to_string());
                        }
                    }
                }
            }
            _ => {}
        }

        let mut env_map = serde_yaml::Mapping::new();
        for (k, v) in merged {
            env_map.insert(Value::String(k), Value::String(escape_dollars(&v)));
        }
        svc_map.remove(Value::from("env_file"));
        svc_map.insert(Value::from("environment"), Value::Mapping(env_map));
    }

    Ok(serde_yaml::to_string(&doc)?)
}

fn yaml_scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

/// Deploy. Stack name follows the repo convention: dir with '/' -> '-'
/// ("zauat/admin" -> "zauat-admin", root-level "system" -> "system").
pub fn run_deploy(fs: &MemFs, dir: &str, target: Target) -> Result<DeployReport> {
    let started = std::time::Instant::now();
    let stack_name = dir.replace('/', "-");
    let what = match &target {
        Target::WholeStack => "whole stack".to_string(),
        Target::Services(list) => list.join(", "),
    };
    eprintln!("\n🚀 deploying {stack_name} [{what}]");

    let compose = prepare_compose(fs, dir, &target)?;
    eprintln!("   compose prepared in-memory ({} KiB), piping to docker stack deploy", compose.len() / 1024);

    let mut cmd = Command::new("docker");
    cmd.args([
        "stack",
        "deploy",
        // Re-resolve every tag against the registry. Without this, Swarm
        // keeps the digest already pinned in the service spec, so a moved
        // tag (`*.latest`) deploys the OLD image and looks like a no-op.
        "--resolve-image",
        "always",
        "--with-registry-auth",
        "--detach=false",
        "--compose-file",
        "-",
        &stack_name,
    ]);
    let (ok, log) = crate::build::run_tee_cmd(cmd, Some(compose.into_bytes()))?;
    if ok {
        eprintln!("✅ deployed {stack_name} [{what}]");
    } else {
        eprintln!("❌ deploy FAILED for {stack_name} [{what}]");
    }
    Ok(DeployReport {
        stack_name,
        what,
        ok,
        duration_secs: started.elapsed().as_secs(),
        log,
    })
}

pub fn report_body(r: &DeployReport) -> String {
    format!(
        "hefesto deploy report — {}\n{} {} [{}]\n    duration: {}s\n\n===== log =====\n{}\n",
        r.stack_name,
        if r.ok { "✅" } else { "❌" },
        r.stack_name,
        r.what,
        r.duration_secs,
        r.log
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_paths() {
        assert_eq!(resolve_rel("zauat/admin", "var_x.env"), "zauat/admin/var_x.env");
        assert_eq!(resolve_rel("zauat/admin", "../conf/var_y.env"), "zauat/conf/var_y.env");
        assert_eq!(resolve_rel("zauat/admin", "./var_z.env"), "zauat/admin/var_z.env");
    }

    #[test]
    fn merges_env_files_into_environment() {
        let mut fs = MemFs::default();
        fs.files.insert(
            "zauat/admin/docker-compose.yml".into(),
            b"services:\n  web:\n    image: x/y:1\n    env_file:\n      - var_a.env\n      - ../conf/var_b.env\n    environment:\n      EXPLICIT: wins\n".to_vec(),
        );
        fs.files.insert("zauat/admin/var_a.env".into(), b"K1=from_a\nSHARED=a\nPASS=p$ss\n".to_vec());
        fs.files.insert("zauat/conf/var_b.env".into(), b"# comment\nSHARED=b\nK2=\"quoted\"\n".to_vec());

        let out = prepare_compose(&fs, "zauat/admin", &Target::WholeStack).unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let envs = &doc["services"]["web"]["environment"];
        assert_eq!(envs["K1"].as_str(), Some("from_a"));
        assert_eq!(envs["SHARED"].as_str(), Some("b")); // later file wins
        assert_eq!(envs["K2"].as_str(), Some("quoted"));
        assert_eq!(envs["EXPLICIT"].as_str(), Some("wins"));
        assert_eq!(envs["PASS"].as_str(), Some("p$$ss")); // dollar escaped
        assert!(doc["services"]["web"].get("env_file").is_none());
    }

    #[test]
    fn filters_services() {
        let mut fs = MemFs::default();
        fs.files.insert(
            "e/s/docker-compose.yml".into(),
            b"services:\n  a:\n    image: i:1\n  b:\n    image: i:2\n".to_vec(),
        );
        let out = prepare_compose(&fs, "e/s", &Target::Services(vec!["b".into()])).unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        assert!(doc["services"].get("a").is_none());
        assert!(doc["services"].get("b").is_some());
    }
}
