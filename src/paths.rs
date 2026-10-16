use std::path::PathBuf;

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn data_dir() -> PathBuf {
    env_dir("CLAUDE_PLUGIN_DATA")
        .or_else(|| env_dir("IMAGEGEN_CC_DATA"))
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("imagegen-cc"))
}

pub fn default_output_dir() -> PathBuf {
    dirs::picture_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("imagegen-cc")
}

pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
