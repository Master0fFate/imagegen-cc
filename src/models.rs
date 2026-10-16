use crate::catalog::{CATALOG, ModelFile};
use crate::config::{Backend, Config, Kind};
use crate::download::{self, Report, human};
use crate::engine::{self, EngineKind};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct AddLocal {
    pub name: String,
    pub recipe: String,
    pub variant: Option<String>,
    pub links: BTreeMap<String, PathBuf>,
    pub search: Vec<PathBuf>,
    pub engine: Option<EngineKind>,
}

pub fn models_root() -> PathBuf {
    crate::paths::data_dir().join("models")
}

fn managed_path(f: &ModelFile) -> PathBuf {
    models_root().join(f.repo.replace('/', "__")).join(f.file_name())
}

const SKIP: [&str; 6] = ["node_modules", ".git", "venv", ".venv", "__pycache__", "site-packages"];

pub fn find_existing(f: &ModelFile, dirs: &[PathBuf]) -> Option<PathBuf> {
    let want = f.file_name();
    let mut stack: Vec<(PathBuf, u8)> = dirs.iter().map(|d| (d.clone(), 0)).collect();
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name();
            let n = name.to_string_lossy();
            if ft.is_dir() {
                if depth < 6 && !n.starts_with('.') && !SKIP.contains(&n.as_ref()) {
                    stack.push((e.path(), depth + 1));
                }
            } else if n == want && e.metadata().is_ok_and(|m| m.len() == f.size) {
                return Some(e.path());
            }
        }
    }
    None
}

pub async fn add_local(o: AddLocal, report: &Report) -> Result<String, String> {
    let recipe = CATALOG.recipe(&o.recipe).ok_or_else(|| format!("unknown recipe '{}'. Known: {}", o.recipe, CATALOG.recipes.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", ")))?;
    let hw = crate::hw::detect(&crate::paths::data_dir());
    let variant = match &o.variant {
        Some(v) => recipe.variant(v).ok_or_else(|| format!("unknown variant '{v}'"))?,
        None => recipe
            .best_fit(hw.usable_vram_gb(), hw.ram_gb())
            .ok_or_else(|| format!("{} needs at least {} GB of GPU memory; this computer has {} GB", recipe.name, recipe.variants.iter().map(|v| v.min_vram_gb).min().unwrap_or(0), hw.usable_vram_gb()))?,
    };
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut missing: Vec<(&String, &ModelFile, PathBuf)> = Vec::new();
    for (role, f) in &variant.files {
        if let Some(p) = o.links.get(role) {
            if !p.is_file() {
                return Err(format!("{role}: file not found: {}", p.display()));
            }
            report(&format!("{role}: using your file {}", p.display()));
            files.insert(role.clone(), p.clone());
            continue;
        }
        let managed = managed_path(f);
        if download::is_complete(&managed, f.size, &f.sha256) {
            files.insert(role.clone(), managed);
            continue;
        }
        if let Some(p) = find_existing(f, &o.search) {
            report(&format!("{role}: found {} (same name and size)", p.display()));
            files.insert(role.clone(), p);
            continue;
        }
        missing.push((role, f, managed));
    }
    for (role, p) in &o.links {
        if !variant.files.contains_key(role) {
            if crate::catalog::role_flag(role).is_none() || !p.is_file() {
                return Err(format!("cannot link '{role}' = {}", p.display()));
            }
            files.insert(role.clone(), p.clone());
        }
    }
    let need: u64 = missing.iter().map(|(_, f, _)| f.size).sum();
    if need > 0 {
        let free = crate::hw::detect(&models_root()).disk_free_mb << 20;
        if need + (1 << 30) > free {
            return Err(format!("Not enough disk space: need {}, free {}", human(need), human(free)));
        }
        report(&format!("Downloading {} for {} ({})", human(need), recipe.name, variant.label));
    }
    let kind = match o.engine {
        Some(k) => engine::install(k, report).await?.kind,
        None => match engine::installed_any() {
            Some(i) => i.kind,
            None => engine::install_auto(&hw, report).await?.kind,
        },
    };
    for (role, f, dest) in missing {
        download::fetch(&f.url(), &dest, Some(f.size), Some(&f.sha256), hf_headers(), &format!("{role} ({})", f.file_name()), report).await?;
        files.insert(role.clone(), dest);
    }
    let mut cfg = Config::load()?;
    let b = Backend {
        kind: Kind::Local,
        engine: Some(kind.name().into()),
        recipe: Some(recipe.id.clone()),
        variant: Some(variant.id.clone()),
        files,
        args: variant.args.clone(),
        idle_minutes: 10,
        ..Backend::default()
    };
    cfg.backends.insert(o.name.clone(), b);
    if cfg.default_backend.is_none() {
        cfg.default_backend = Some(o.name.clone());
    }
    cfg.save()?;
    Ok(format!("Backend '{}' ready: {} ({}) on the {} engine", o.name, recipe.name, variant.label, kind.name()))
}

pub fn hf_headers() -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(t) = std::env::var("HF_TOKEN").ok().or_else(|| crate::secrets::get("huggingface")).filter(|t| !t.is_empty())
        && let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    h
}
