use crate::download::client;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;

type Group = (u64, u64, Vec<(Entry, PathBuf)>);

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    method: u16,
    comp: u64,
    size: u64,
    offset: u64,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]))
}

async fn range(url: &str, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String> {
    let r = client()
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end_inclusive}"))
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if r.status().as_u16() != 206 {
        return Err(format!("{url}: range request returned HTTP {}", r.status().as_u16()));
    }
    let b = r.bytes().await.map_err(|e| e.to_string())?;
    Ok(b.to_vec())
}

pub async fn list(url: &str) -> Result<Vec<Entry>, String> {
    let head = client().head(url).send().await.map_err(|e| format!("{url}: {e}"))?;
    if !head.status().is_success() {
        return Err(format!("{url}: HTTP {}", head.status().as_u16()));
    }
    let total = head.content_length().ok_or("no content length")?;
    let tail_len = total.min(65_536 + 22 + 20);
    let tail = range(url, total - tail_len, total - 1).await?;
    let eocd = (0..=tail.len().saturating_sub(22)).rev().find(|&i| u32_at(&tail, i) == 0x0605_4b50).ok_or("not a zip file")?;
    let mut cd_size = u64::from(u32_at(&tail, eocd + 12));
    let mut cd_off = u64::from(u32_at(&tail, eocd + 16));
    if (cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF) && eocd >= 20 && u32_at(&tail, eocd - 20) == 0x0706_4b50 {
        let z64 = u64_at(&tail, eocd - 20 + 8);
        let rec = range(url, z64, z64 + 55).await?;
        cd_size = u64_at(&rec, 40);
        cd_off = u64_at(&rec, 48);
    }
    let cd = range(url, cd_off, cd_off + cd_size - 1).await?;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 46 <= cd.len() && u32_at(&cd, i) == 0x0201_4b50 {
        let method = u16_at(&cd, i + 10);
        let mut comp = u64::from(u32_at(&cd, i + 20));
        let mut size = u64::from(u32_at(&cd, i + 24));
        let nl = usize::from(u16_at(&cd, i + 28));
        let el = usize::from(u16_at(&cd, i + 30));
        let cl = usize::from(u16_at(&cd, i + 32));
        let mut offset = u64::from(u32_at(&cd, i + 42));
        let name = String::from_utf8_lossy(&cd[i + 46..i + 46 + nl]).into_owned();
        let mut e = i + 46 + nl;
        let eend = e + el;
        while e + 4 <= eend {
            let id = u16_at(&cd, e);
            let len = usize::from(u16_at(&cd, e + 2));
            if id == 1 {
                let mut p = e + 4;
                for field in [&mut size, &mut comp, &mut offset] {
                    if *field == 0xFFFF_FFFF && p + 8 <= e + 4 + len {
                        *field = u64_at(&cd, p);
                        p += 8;
                    }
                }
            }
            e += 4 + len;
        }
        out.push(Entry { name, method, comp, size, offset });
        i = eend + cl;
    }
    Ok(out)
}

pub async fn read_one(url: &str, e: &Entry) -> Result<Vec<u8>, String> {
    let buf = range(url, e.offset, e.offset + 30 + 1024 + e.comp).await?;
    decode(&buf, 0, e)
}

fn decode(buf: &[u8], at: usize, e: &Entry) -> Result<Vec<u8>, String> {
    if at + 30 > buf.len() || u32_at(buf, at) != 0x0403_4b50 {
        return Err(format!("{}: bad local header", e.name));
    }
    let start = at + 30 + usize::from(u16_at(buf, at + 26)) + usize::from(u16_at(buf, at + 28));
    let end = start + usize::try_from(e.comp).map_err(|x| x.to_string())?;
    if end > buf.len() {
        return Err(format!("{}: truncated", e.name));
    }
    let data = &buf[start..end];
    match e.method {
        0 => Ok(data.to_vec()),
        8 => {
            let mut out = Vec::with_capacity(usize::try_from(e.size).unwrap_or(0));
            flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).map_err(|x| format!("{}: {x}", e.name))?;
            Ok(out)
        }
        m => Err(format!("{}: unsupported compression {m}", e.name)),
    }
}

pub fn record_hashes(record: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(record)
        .lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let path = it.next()?;
            let h = it.next()?.strip_prefix("sha256=")?;
            Some((path.to_owned(), h.to_owned()))
        })
        .collect()
}

pub async fn extract(url: &str, entries: Vec<(Entry, PathBuf)>, hashes: &HashMap<String, String>) -> Result<u64, String> {
    let mut items = entries;
    items.sort_unstable_by_key(|(e, _)| e.offset);
    let mut groups: Vec<Group> = Vec::new();
    for (e, p) in items {
        let s = e.offset;
        let t = e.offset + 30 + 1024 + e.comp;
        match groups.last_mut() {
            Some(g) if s <= g.1 + (256 << 10) && t - g.0 < (64 << 20) => {
                g.1 = g.1.max(t);
                g.2.push((e, p));
            }
            _ => groups.push((s, t, vec![(e, p)])),
        }
    }
    let url = url.to_owned();
    let hashes = std::sync::Arc::new(hashes.clone());
    let mut set = tokio::task::JoinSet::new();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
    for (s, t, list) in groups {
        let url = url.clone();
        let hashes = hashes.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
            let buf = range(&url, s, t).await?;
            tokio::task::spawn_blocking(move || {
                let mut written = 0u64;
                for (e, dest) in list {
                    let at = usize::try_from(e.offset - s).map_err(|x| x.to_string())?;
                    let data = decode(&buf, at, &e)?;
                    if let Some(want) = hashes.get(&e.name) {
                        let got = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&data));
                        if &got != want {
                            return Err(format!("{}: checksum mismatch", e.name));
                        }
                    }
                    if let Some(d) = dest.parent() {
                        std::fs::create_dir_all(d).map_err(|x| x.to_string())?;
                    }
                    std::fs::write(&dest, &data).map_err(|x| format!("{}: {x}", dest.display()))?;
                    written += data.len() as u64;
                }
                Ok::<u64, String>(written)
            })
            .await
            .map_err(|e| e.to_string())?
        });
    }
    let mut total = 0;
    while let Some(r) = set.join_next().await {
        total += r.map_err(|e| e.to_string())??;
    }
    Ok(total)
}
