use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Asset {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Engines {
    pub tag: String,
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Rocm {
    pub index: String,
    pub version: String,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ModelFile {
    pub repo: String,
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

impl ModelFile {
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{}/resolve/main/{}", self.repo, self.path)
    }

    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Variant {
    pub id: String,
    pub label: String,
    pub min_vram_gb: u32,
    #[serde(default)]
    pub min_ram_gb: u32,
    pub files: BTreeMap<String, ModelFile>,
    pub args: Vec<String>,
    pub download_bytes: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    pub about: String,
    pub license: String,
    pub edit: bool,
    pub variants: Vec<Variant>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Catalog {
    pub engines: Engines,
    pub rocm: Rocm,
    pub recipes: Vec<Recipe>,
}

pub static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../resources/catalog.json")).unwrap_or_else(|e| panic!("embedded catalog: {e}"))
});

impl Catalog {
    pub fn recipe(&self, id: &str) -> Option<&Recipe> {
        self.recipes.iter().find(|r| r.id == id)
    }
}

impl Recipe {
    pub fn variant(&self, id: &str) -> Option<&Variant> {
        self.variants.iter().find(|v| v.id == id)
    }

    pub fn best_fit(&self, vram_gb: u32, ram_gb: u32) -> Option<&Variant> {
        self.variants.iter().find(|v| v.min_vram_gb <= vram_gb && v.min_ram_gb <= ram_gb)
    }
}

pub fn role_flag(role: &str) -> Option<&'static str> {
    match role {
        "diffusion" => Some("--diffusion-model"),
        "llm" => Some("--llm"),
        "llm_vision" => Some("--llm_vision"),
        "vae" => Some("--vae"),
        "t5xxl" => Some("--t5xxl"),
        "clip_l" => Some("--clip_l"),
        "model" => Some("--model"),
        _ => None,
    }
}
