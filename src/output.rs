use base64::Engine;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PREVIEW_MAX: u32 = 512;

pub fn ext_of(b: &[u8]) -> &'static str {
    if b.starts_with(b"\x89PNG") {
        "png"
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "webp"
    } else if b.starts_with(b"GIF8") {
        "gif"
    } else {
        "bin"
    }
}

fn utc_parts() -> (i64, u32, u32, u32, u32, u32, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, sod / 3600, sod / 60 % 60, sod % 60, d.subsec_millis())
}

fn slug(prompt: &str) -> String {
    let mut s = String::with_capacity(48);
    for word in prompt.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        if s.len() + word.len() + 1 > 48 {
            break;
        }
        if !s.is_empty() {
            s.push('-');
        }
        s.extend(word.chars().map(|c| c.to_ascii_lowercase()));
    }
    if s.is_empty() { "image".into() } else { s }
}

fn create_unique(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    for n in 0u32.. {
        let name = if n == 0 { format!("{stem}.{ext}") } else { format!("{stem}-{n}.{ext}") };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

pub fn save(root: &Path, prompt: &str, images: &[Vec<u8>]) -> std::io::Result<Vec<PathBuf>> {
    let (y, mo, d, h, mi, s, ms) = utc_parts();
    let dir = root.join(format!("{y:04}-{mo:02}"));
    std::fs::create_dir_all(&dir)?;
    let base = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}{ms:03}-{}", slug(prompt));
    images
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let stem = if images.len() > 1 { format!("{base}-{}", i + 1) } else { base.clone() };
            create_unique(&dir, &stem, ext_of(b), b)
        })
        .collect()
}

pub fn copy_to(target: &str, files: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let t = PathBuf::from(target);
    let as_dir = target.ends_with('/') || target.ends_with('\\') || t.is_dir() || t.extension().is_none();
    let mut out = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("png");
        let dest = if as_dir {
            t.join(f.file_name().unwrap_or_default())
        } else if files.len() > 1 {
            let stem = t.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
            t.with_file_name(format!("{stem}-{}.{ext}", i + 1))
        } else {
            t.with_extension(ext)
        };
        let dir = dest.parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        std::fs::create_dir_all(&dir)?;
        let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_owned();
        let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or(ext).to_owned();
        let bytes = std::fs::read(f)?;
        let written = create_unique(&dir, &stem, &ext, &bytes)?;
        out.push(std::path::absolute(&written).unwrap_or(written));
    }
    Ok(out)
}

pub fn preview(bytes: &[u8]) -> Option<String> {
    let img = image::load_from_memory(bytes).ok()?;
    let small = if img.width() > PREVIEW_MAX || img.height() > PREVIEW_MAX {
        img.thumbnail(PREVIEW_MAX, PREVIEW_MAX)
    } else {
        img
    };
    let mut buf = Vec::with_capacity(64 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80)
        .encode_image(&small.to_rgb8())
        .ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(buf))
}
