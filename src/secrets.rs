use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const MASTER: &str = "master-key";

fn service() -> String {
    std::env::var("IMAGEGEN_CC_KEYRING_SERVICE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "imagegen-cc".into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    Keychain,
    File,
}

#[derive(Serialize, Deserialize, Default)]
struct Vault {
    v: u8,
    enc: bool,
    items: BTreeMap<String, String>,
}

fn file_path() -> PathBuf {
    crate::paths::data_dir().join("secrets.json")
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD_NO_PAD
}

#[cfg(target_os = "linux")]
fn keychain_reachable() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty())
        || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| std::path::Path::new(&d).join("bus").exists())
}

#[cfg(not(target_os = "linux"))]
fn keychain_reachable() -> bool {
    true
}

fn master(create: bool) -> Option<LessSafeKey> {
    if !keychain_reachable() {
        return None;
    }
    let entry = keyring::Entry::new(&service(), MASTER).ok()?;
    let raw = match entry.get_password() {
        Ok(hex) => (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect::<Option<Vec<u8>>>()?,
        Err(_) if create => {
            let mut k = [0u8; 32];
            getrandom::fill(&mut k).ok()?;
            entry.set_password(&crate::download::hex(&k)).ok()?;
            entry.get_password().ok()?;
            k.to_vec()
        }
        Err(_) => return None,
    };
    (raw.len() == 32).then(|| UnboundKey::new(&CHACHA20_POLY1305, &raw).ok().map(LessSafeKey::new)).flatten()
}

fn seal(key: &LessSafeKey, name: &str, value: &str) -> Result<String, String> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let mut buf = value.as_bytes().to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(name.as_bytes()), &mut buf).map_err(|_| "encryption failed".to_string())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&buf);
    Ok(b64().encode(out))
}

fn open(key: &LessSafeKey, name: &str, value: &str) -> Option<String> {
    let mut raw = b64().decode(value).ok()?;
    if raw.len() <= NONCE_LEN {
        return None;
    }
    let mut body = raw.split_off(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(&raw).ok()?;
    let plain = key.open_in_place(nonce, Aad::from(name.as_bytes()), &mut body).ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

fn read() -> Vault {
    std::fs::read(file_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write(v: &Vault) -> Result<(), String> {
    let path = file_path();
    if v.items.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    let bytes = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    use std::io::Write;
    opts.open(&tmp).and_then(|mut f| f.write_all(&bytes)).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

fn plain_items(v: Vault) -> BTreeMap<String, String> {
    if !v.enc {
        return v.items;
    }
    let Some(k) = master(false) else { return BTreeMap::new() };
    v.items.iter().filter_map(|(n, c)| open(&k, n, c).map(|p| (n.clone(), p))).collect()
}

pub fn get(name: &str) -> Option<String> {
    if let Some(v) = std::env::var("IMAGEGEN_CC_API_KEY").ok().filter(|v| !v.is_empty()) {
        return Some(v);
    }
    let v = read();
    let c = v.items.get(name)?;
    if v.enc { open(&master(false)?, name, c) } else { Some(c.clone()) }
}

pub fn has(name: &str) -> Option<Store> {
    let v = read();
    v.items.contains_key(name).then_some(if v.enc { Store::Keychain } else { Store::File })
}

fn save_all(items: BTreeMap<String, String>) -> Result<Store, String> {
    if items.is_empty() {
        write(&Vault::default())?;
        return Ok(Store::File);
    }
    let key = master(true);
    let mut out = Vault { v: 2, enc: key.is_some(), items: BTreeMap::new() };
    for (n, p) in items {
        let val = match &key {
            Some(k) => seal(k, &n, &p)?,
            None => p,
        };
        out.items.insert(n, val);
    }
    write(&out)?;
    Ok(if out.enc { Store::Keychain } else { Store::File })
}

pub fn set(name: &str, value: &str) -> Result<Store, String> {
    let mut items = plain_items(read());
    items.insert(name.to_owned(), value.to_owned());
    save_all(items)
}

pub fn delete(name: &str) -> Result<(), String> {
    let v = read();
    if !v.items.contains_key(name) {
        return Ok(());
    }
    let mut items = plain_items(v);
    items.remove(name);
    save_all(items).map(|_| ())
}

pub fn purge() {
    let _ = std::fs::remove_file(file_path());
    if !keychain_reachable() {
        return;
    }
    if let Ok(e) = keyring::Entry::new(&service(), MASTER) {
        let _ = e.delete_credential();
    }
}
