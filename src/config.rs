use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const NO_BACKEND: &str = "No image backend is configured. Run /imagegen-cc:setup to add a local model or an API.";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Openai,
    Sdcpp,
    Local,
    Comfyui,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::Openai),
            "sdcpp" => Some(Self::Sdcpp),
            "local" => Some(Self::Local),
            "comfyui" => Some(Self::Comfyui),
            _ => None,
        }
    }
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Backend {
    pub kind: Kind,
    pub base_url: String,
    pub model: Option<String>,
    pub auth_header: String,
    pub auth_prefix: String,
    pub extra: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub idle_minutes: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<PathBuf>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            kind: Kind::Openai,
            base_url: String::new(),
            model: None,
            auth_header: "Authorization".into(),
            auth_prefix: "Bearer ".into(),
            extra: Map::new(),
            engine: None,
            recipe: None,
            variant: None,
            files: BTreeMap::new(),
            args: Vec::new(),
            idle_minutes: 0,
            workflow: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub default_backend: Option<String>,
    pub output_dir: Option<PathBuf>,
    pub backends: BTreeMap<String, Backend>,
}

impl Config {
    pub fn path() -> PathBuf {
        crate::paths::data_dir().join("config.json")
    }

    pub fn load() -> Result<Self, String> {
        match std::fs::read(Self::path()) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("config.json is invalid: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Cannot read config.json: {e}")),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::paths::write_atomic(&Self::path(), &bytes).map_err(|e| format!("Cannot write config.json: {e}"))
    }

    pub fn resolve(&self, name: Option<&str>) -> Result<(String, &Backend), String> {
        let name = name
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .or_else(|| self.default_backend.clone())
            .or_else(|| (self.backends.len() == 1).then(|| self.backends.keys().next().cloned()).flatten())
            .ok_or_else(|| NO_BACKEND.to_string())?;
        match self.backends.get(&name) {
            Some(b) => Ok((name, b)),
            None if self.backends.is_empty() => Err(NO_BACKEND.to_string()),
            None => Err(format!(
                "Backend '{name}' is not configured. Configured: {}",
                self.backends.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
        }
    }

    pub fn output_dir(&self) -> PathBuf {
        self.output_dir.clone().unwrap_or_else(crate::paths::default_output_dir)
    }
}
