use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};

pub type Report = Arc<dyn Fn(&str) + Send + Sync>;

const CHUNK: u64 = 16 << 20;
const WORKERS: usize = 8;

pub fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("imagegen-cc/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 { format!("{:.2} GB", b / 1e9) } else { format!("{:.0} MB", b / 1e6) }
}

pub async fn sha256_file(path: &Path) -> Result<String, String> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 8 << 20];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(hex(&h.finalize()))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn marker(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".ok");
    PathBuf::from(s)
}

fn sibling(dest: &Path, ext: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

pub fn is_complete(dest: &Path, size: u64, sha256: &str) -> bool {
    std::fs::metadata(dest).is_ok_and(|m| m.len() == size) && std::fs::read_to_string(marker(dest)).is_ok_and(|s| s.trim() == sha256)
}

async fn probe(url: &str, headers: &reqwest::header::HeaderMap) -> Result<(String, Option<u64>, bool), String> {
    let r = client()
        .get(url)
        .headers(headers.clone())
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let status = r.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(format!("{url}: HTTP {} (this file may need a Hugging Face token or license acceptance)", status.as_u16()));
    }
    if !status.is_success() {
        return Err(format!("{url}: HTTP {}", status.as_u16()));
    }
    let final_url = r.url().to_string();
    if status.as_u16() == 206 {
        let total = r
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next()?.parse::<u64>().ok());
        return Ok((final_url, total, total.is_some()));
    }
    Ok((final_url, r.content_length(), false))
}

struct Shared {
    url: String,
    headers: reqwest::header::HeaderMap,
    part: PathBuf,
    state: PathBuf,
    done: Mutex<BTreeSet<u64>>,
    bytes: AtomicU64,
    queue: Mutex<Vec<u64>>,
    total: u64,
}

async fn worker(s: Arc<Shared>) -> Result<(), String> {
    let mut f = tokio::fs::OpenOptions::new().write(true).open(&s.part).await.map_err(|e| e.to_string())?;
    loop {
        let Some(idx) = s.queue.lock().map_err(|e| e.to_string())?.pop() else { return Ok(()) };
        let start = idx * CHUNK;
        let end = (start + CHUNK).min(s.total) - 1;
        let mut attempt = 0;
        loop {
            let mut got = 0u64;
            let res = async {
                let mut r = client()
                    .get(&s.url)
                    .headers(s.headers.clone())
                    .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if r.status().as_u16() != 206 {
                    return Err(format!("HTTP {}", r.status().as_u16()));
                }
                f.seek(std::io::SeekFrom::Start(start)).await.map_err(|e| e.to_string())?;
                let mut w = BufWriter::with_capacity(1 << 20, &mut f);
                while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
                    w.write_all(&c).await.map_err(|e| e.to_string())?;
                    got += c.len() as u64;
                    s.bytes.fetch_add(c.len() as u64, Ordering::Relaxed);
                }
                w.flush().await.map_err(|e| e.to_string())?;
                if got != end - start + 1 {
                    return Err(format!("short read {got}"));
                }
                Ok(())
            }
            .await;
            match res {
                Ok(()) => break,
                Err(e) => {
                    s.bytes.fetch_sub(got, Ordering::Relaxed);
                    if attempt >= 4 {
                        return Err(format!("download failed at byte {start}: {e}"));
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                }
            }
        }
        let snapshot = {
            let mut d = s.done.lock().map_err(|e| e.to_string())?;
            d.insert(idx);
            serde_json::to_vec(&*d).unwrap_or_default()
        };
        let _ = tokio::fs::write(&s.state, snapshot).await;
    }
}

pub async fn fetch(url: &str, dest: &Path, size: Option<u64>, sha256: Option<&str>, headers: reqwest::header::HeaderMap, label: &str, report: &Report) -> Result<(), String> {
    if let (Some(n), Some(h)) = (size, sha256)
        && is_complete(dest, n, h) {
            return Ok(());
        }
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let (final_url, probed, ranged) = probe(url, &headers).await?;
    let total = size.or(probed);
    let part = sibling(dest, ".part");
    let state = sibling(dest, ".part.state");
    let started = Instant::now();
    if let (true, Some(total)) = (ranged, total) {
        let chunks = total.div_ceil(CHUNK);
        let done: BTreeSet<u64> = match (tokio::fs::metadata(&part).await, tokio::fs::read(&state).await) {
            (Ok(m), Ok(b)) if m.len() == total => serde_json::from_slice(&b).unwrap_or_default(),
            _ => BTreeSet::new(),
        };
        if done.is_empty() {
            let f = tokio::fs::File::create(&part).await.map_err(|e| format!("{}: {e}", part.display()))?;
            f.set_len(total).await.map_err(|e| e.to_string())?;
        }
        let already: u64 = done.iter().map(|&i| ((i + 1) * CHUNK).min(total) - i * CHUNK).sum();
        let queue: Vec<u64> = (0..chunks).rev().filter(|i| !done.contains(i)).collect();
        let shared = Arc::new(Shared { url: final_url, headers, part: part.clone(), state: state.clone(), done: Mutex::new(done), bytes: AtomicU64::new(already), queue: Mutex::new(queue), total });
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..WORKERS.min(usize::try_from(chunks).unwrap_or(WORKERS)) {
            set.spawn(worker(shared.clone()));
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last = (Instant::now(), already);
        loop {
            tokio::select! {
                r = set.join_next() => match r {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(e))) => { set.abort_all(); return Err(format!("{label}: {e}")); }
                    Some(Err(e)) => { set.abort_all(); return Err(format!("{label}: {e}")); }
                    None => break,
                },
                _ = tick.tick() => {
                    let b = shared.bytes.load(Ordering::Relaxed);
                    let dt = last.0.elapsed().as_secs_f64().max(0.001);
                    let speed = (b - last.1) as f64 / dt;
                    last = (Instant::now(), b);
                    report(&format!("{label}: {} / {} ({:.0}%) {:.0} MB/s", human(b), human(total), b as f64 * 100.0 / total as f64, speed / 1e6));
                }
            }
        }
    } else {
        let mut r = client().get(&final_url).headers(headers).send().await.map_err(|e| format!("{label}: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("{label}: HTTP {}", r.status().as_u16()));
        }
        let f = tokio::fs::File::create(&part).await.map_err(|e| format!("{}: {e}", part.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        let mut got = 0u64;
        let mut last = Instant::now();
        while let Some(c) = r.chunk().await.map_err(|e| format!("{label}: {e}"))? {
            w.write_all(&c).await.map_err(|e| e.to_string())?;
            got += c.len() as u64;
            if last.elapsed() >= Duration::from_secs(1) {
                last = Instant::now();
                report(&format!("{label}: {}", human(got)));
            }
        }
        w.flush().await.map_err(|e| e.to_string())?;
    }
    if let Some(n) = total {
        let len = tokio::fs::metadata(&part).await.map_err(|e| e.to_string())?.len();
        if len != n {
            return Err(format!("{label}: size {len} != expected {n}"));
        }
    }
    if let Some(want) = sha256 {
        report(&format!("{label}: verifying"));
        let got = sha256_file(&part).await?;
        if got != want {
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&state).await;
            return Err(format!("{label}: checksum mismatch (got {got}, want {want}). The partial file was deleted; run again"));
        }
    }
    let _ = tokio::fs::remove_file(dest).await;
    tokio::fs::rename(&part, dest).await.map_err(|e| format!("{}: {e}", dest.display()))?;
    let _ = tokio::fs::remove_file(&state).await;
    if let Some(h) = sha256 {
        let _ = tokio::fs::write(marker(dest), h).await;
    }
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let n = total.unwrap_or(0);
    report(&format!("{label}: done, {} in {secs:.0}s ({:.0} MB/s)", human(n), n as f64 / secs / 1e6));
    Ok(())
}
