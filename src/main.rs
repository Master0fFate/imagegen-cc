mod backend;
mod catalog;
mod comfyui;
mod config;
mod dashboard;
mod download;
mod engine;
mod hw;
mod mcp;
mod models;
mod output;
mod paths;
mod remote_zip;
mod secrets;

use catalog::CATALOG;
use config::{Backend, Config, Kind};
use rmcp::ServiceExt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "imagegen-cc                                        run the MCP server (stdio)
imagegen-cc setup [--no-open]                      open the setup dashboard
imagegen-cc hw                                     detected hardware and which models fit
imagegen-cc catalog                                available local models
imagegen-cc engine install [auto|rocm|cuda|vulkan|metal|cpu]
imagegen-cc engine devices
imagegen-cc local add <name> <recipe> [--variant v] [--link role=path]... [--search dir]... [--engine kind]
imagegen-cc comfyui add <name> <base_url> <api-workflow.json> [--set key=value]...
imagegen-cc backend set <name> <openai|sdcpp> <base_url> [model]
imagegen-cc backend remove <name>
imagegen-cc backend default <name>
imagegen-cc key set <backend>                      reads the key from stdin
imagegen-cc key delete <backend>
imagegen-cc key purge                              delete all keys and the master key
imagegen-cc paths";

type Flags<'a> = BTreeMap<&'a str, Vec<&'a str>>;

fn flags<'a>(rest: &[&'a str]) -> Result<(Vec<&'a str>, Flags<'a>), String> {
    let mut pos = Vec::new();
    let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if let Some(k) = a.strip_prefix("--") {
            let v = it.next().ok_or_else(|| format!("--{k} needs a value"))?;
            map.entry(k).or_default().push(v);
        } else {
            pos.push(*a);
        }
    }
    Ok((pos, map))
}

fn pairs(v: Option<&Vec<&str>>) -> Result<Vec<(String, String)>, String> {
    v.into_iter()
        .flatten()
        .map(|s| s.split_once('=').map(|(a, b)| (a.to_owned(), b.to_owned())).ok_or_else(|| format!("expected key=value, got '{s}'")))
        .collect()
}

fn cli_report() -> download::Report {
    Arc::new(|m: &str| eprintln!("  {m}"))
}

pub(crate) fn prune_models(cfg: &Config) {
    let used: BTreeSet<PathBuf> = cfg.backends.values().flat_map(|b| b.files.values().cloned()).collect();
    let mut stack = vec![models::models_root()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_none_or(|x| x != "ok") && !used.contains(&p) {
                let _ = std::fs::remove_file(&p);
                let _ = std::fs::remove_file(format!("{}.ok", p.display()));
            }
        }
        let _ = std::fs::remove_dir(&d);
    }
}

async fn cli(args: &[String]) -> Result<String, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["setup", rest @ ..] => {
            let url = dashboard::start().await?;
            println!("{url}");
            if !rest.contains(&"--no-open") {
                dashboard::open_browser(&url);
            }
            std::future::pending::<()>().await;
            Ok(String::new())
        }
        ["hw"] => {
            let h = hw::detect(&paths::data_dir());
            let (vram, ram) = (h.usable_vram_gb(), h.ram_gb());
            let fits: Vec<serde_json::Value> = CATALOG
                .recipes
                .iter()
                .map(|r| serde_json::json!({ "recipe": r.id, "best_variant": r.best_fit(vram, ram).map(|v| &v.id) }))
                .collect();
            let engines: Vec<&str> = engine::preference(&h).into_iter().map(engine::EngineKind::name).collect();
            Ok(serde_json::to_string_pretty(&serde_json::json!({ "hardware": h, "usable_vram_gb": vram, "engine_order": engines, "fits": fits })).unwrap_or_default())
        }
        ["catalog"] => {
            let mut s = String::new();
            for r in &CATALOG.recipes {
                s.push_str(&format!("{}  {} ({}){}\n  {}\n", r.id, r.name, r.license, if r.edit { ", can edit" } else { "" }, r.about));
                for v in &r.variants {
                    s.push_str(&format!("    {:<11} {:<34} {:>8}  min {} GB VRAM\n", v.id, v.label, download::human(v.download_bytes), v.min_vram_gb));
                }
            }
            Ok(s)
        }
        ["engine", "install", rest @ ..] => {
            let r = cli_report();
            let i = match rest.first().copied().unwrap_or("auto") {
                "auto" => engine::install_auto(&hw::detect(&paths::data_dir()), &r).await?,
                k => engine::install(engine::EngineKind::parse(k).ok_or("unknown engine")?, &r).await?,
            };
            Ok(format!("engine {} installed ({})\n{}", i.kind.name(), i.gfx.unwrap_or_default(), i.devices.join("\n")))
        }
        ["engine", "devices"] => {
            let i = engine::installed_any().ok_or("no engine installed")?;
            let bin = engine::bin_dir(i.kind).ok_or("engine files missing")?;
            let (d, gfx) = engine::list_devices(&bin).await?;
            Ok(format!("{} engine{}\n{}", i.kind.name(), gfx.map(|g| format!(" ({g})")).unwrap_or_default(), d.join("\n")))
        }
        ["local", "add", name, recipe, rest @ ..] => {
            let (_, f) = flags(rest)?;
            let links = pairs(f.get("link"))?.into_iter().map(|(k, v)| (k, PathBuf::from(v))).collect();
            let o = models::AddLocal {
                name: (*name).to_owned(),
                recipe: (*recipe).to_owned(),
                variant: f.get("variant").and_then(|v| v.first()).map(|s| (*s).to_owned()),
                links,
                search: f.get("search").into_iter().flatten().map(PathBuf::from).collect(),
                engine: match f.get("engine").and_then(|v| v.first()) {
                    Some(k) => Some(engine::EngineKind::parse(k).ok_or("unknown engine")?),
                    None => None,
                },
            };
            models::add_local(o, &cli_report()).await
        }
        ["comfyui", "add", name, url, workflow, rest @ ..] => {
            let (_, f) = flags(rest)?;
            let wf = std::path::absolute(workflow).map_err(|e| e.to_string())?;
            let text = std::fs::read_to_string(&wf).map_err(|e| format!("{}: {e}", wf.display()))?;
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("workflow is not JSON: {e}"))?;
            if v.get("nodes").is_some() {
                return Err("This workflow is in UI format. In ComfyUI use Workflow > Export (API)".into());
            }
            if !text.contains("{{prompt}}") {
                return Err("The workflow has no {{prompt}} placeholder. Put {{prompt}} in the positive prompt text".into());
            }
            let mut extra = serde_json::Map::new();
            for (k, val) in pairs(f.get("set"))? {
                extra.insert(k, val.parse::<f64>().map_or(serde_json::Value::String(val), |n| serde_json::json!(n)));
            }
            let mut cfg = Config::load()?;
            cfg.backends.insert((*name).to_owned(), Backend { kind: Kind::Comfyui, base_url: (*url).to_owned(), workflow: Some(wf), extra, ..Backend::default() });
            if cfg.default_backend.is_none() {
                cfg.default_backend = Some((*name).to_owned());
            }
            cfg.save()?;
            Ok(format!("backend '{name}' saved"))
        }
        ["backend", "set", name, kind, url, rest @ ..] => {
            let kind = Kind::parse(kind).filter(|k| matches!(k, Kind::Openai | Kind::Sdcpp)).ok_or("kind must be openai or sdcpp")?;
            let mut cfg = Config::load()?;
            let b = cfg.backends.entry((*name).to_owned()).or_insert_with(Backend::default);
            b.kind = kind;
            b.base_url = (*url).to_owned();
            b.model = rest.first().map(|m| (*m).to_owned());
            if cfg.default_backend.is_none() {
                cfg.default_backend = Some((*name).to_owned());
            }
            cfg.save()?;
            Ok(format!("backend '{name}' saved"))
        }
        ["backend", "remove", name] => {
            let mut cfg = Config::load()?;
            cfg.backends.remove(*name);
            if cfg.default_backend.as_deref() == Some(*name) {
                cfg.default_backend = cfg.backends.keys().next().cloned();
            }
            cfg.save()?;
            secrets::delete(name)?;
            prune_models(&cfg);
            Ok(format!("backend '{name}' removed"))
        }
        ["backend", "default", name] => {
            let mut cfg = Config::load()?;
            if !cfg.backends.contains_key(*name) {
                return Err(format!("backend '{name}' is not configured"));
            }
            cfg.default_backend = Some((*name).to_owned());
            cfg.save()?;
            Ok(format!("default backend is '{name}'"))
        }
        ["key", "set", name] => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
            let key = s.trim();
            if key.is_empty() {
                return Err("empty key on stdin".into());
            }
            Ok(match secrets::set(name, key)? {
                secrets::Store::Keychain => format!("key for '{name}' stored in the OS keychain"),
                secrets::Store::File => format!("No OS keychain found. Key for '{name}' stored in {} (readable only by you)", paths::data_dir().join("secrets.json").display()),
            })
        }
        ["key", "delete", name] => secrets::delete(name).map(|()| format!("key for '{name}' deleted")),
        ["key", "purge"] => {
            secrets::purge();
            Ok("all keys and the master key deleted".into())
        }
        ["paths"] => {
            let cfg = Config::load()?;
            Ok(format!(
                "data:    {}\nconfig:  {}\noutput:  {}\nmodels:  {}\nengines: {}\nlog:     {}",
                paths::data_dir().display(),
                Config::path().display(),
                cfg.output_dir().display(),
                models::models_root().display(),
                engine::engines_root().display(),
                engine::log_path().display()
            ))
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        if !args.is_empty() {
            return match cli(&args).await {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::SUCCESS
                }
                Err(msg) => {
                    eprintln!("{msg}");
                    ExitCode::FAILURE
                }
            };
        }
        let code = match mcp::Server::new().serve(rmcp::transport::stdio()).await {
            Ok(s) => {
                let _ = s.waiting().await;
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("mcp: {e}");
                ExitCode::FAILURE
            }
        };
        engine::MANAGER.stop().await;
        code
    })
}
