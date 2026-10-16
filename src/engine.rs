use crate::catalog::{CATALOG, role_flag};
use crate::config::Backend;
use crate::download::{self, Report};
use crate::hw::{Hw, Vendor};
use crate::remote_zip;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Rocm,
    Cuda,
    Vulkan,
    Metal,
    Cpu,
}

impl EngineKind {
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rocm => "rocm",
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::Metal => "metal",
            Self::Cpu => "cpu",
        }
    }

    fn device_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::Rocm => &["ROCm", "HIP"],
            Self::Cuda => &["CUDA"],
            Self::Vulkan => &["Vulkan"],
            Self::Metal => &["MTL", "Metal"],
            Self::Cpu => &["CPU"],
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Installed {
    pub tag: String,
    pub kind: EngineKind,
    pub devices: Vec<String>,
    pub gfx: Option<String>,
}

fn os_key() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() }
}

pub fn engines_root() -> PathBuf {
    crate::paths::data_dir().join("engines")
}

pub fn dir_of(kind: EngineKind) -> PathBuf {
    engines_root().join(format!("{}-{}", CATALOG.engines.tag, kind.name()))
}

pub fn installed(kind: EngineKind) -> Option<Installed> {
    let b = std::fs::read(dir_of(kind).join("installed.json")).ok()?;
    serde_json::from_slice::<Installed>(&b).ok().filter(|i| i.tag == CATALOG.engines.tag)
}

pub fn installed_any() -> Option<Installed> {
    [EngineKind::Cuda, EngineKind::Rocm, EngineKind::Metal, EngineKind::Vulkan, EngineKind::Cpu].into_iter().find_map(installed)
}

pub fn preference(hw: &Hw) -> Vec<EngineKind> {
    let v = hw.best_gpu().map(|g| g.vendor);
    match (os_key(), v) {
        ("macos", Some(Vendor::Apple)) => vec![EngineKind::Metal],
        ("macos", _) => vec![],
        ("windows", Some(Vendor::Nvidia)) => vec![EngineKind::Cuda, EngineKind::Vulkan, EngineKind::Cpu],
        (_, Some(Vendor::Amd)) => vec![EngineKind::Rocm, EngineKind::Vulkan, EngineKind::Cpu],
        (_, Some(Vendor::Nvidia | Vendor::Intel)) => vec![EngineKind::Vulkan, EngineKind::Cpu],
        _ => vec![EngineKind::Cpu],
    }
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == name) {
                return Some(p);
            }
        }
    }
    None
}

pub fn bin_dir(kind: EngineKind) -> Option<PathBuf> {
    find_file(&dir_of(kind), &exe("sd-server")).and_then(|p| p.parent().map(Path::to_path_buf))
}

fn no_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let _ = cmd;
}

pub async fn list_devices(bin: &Path) -> Result<(Vec<String>, Option<String>), String> {
    let mut cmd = tokio::process::Command::new(bin.join(exe("sd-cli")));
    cmd.arg("--list-devices").current_dir(bin).stdin(Stdio::null()).kill_on_drop(true);
    no_window(&mut cmd);
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| "engine device check timed out".to_string())?
        .map_err(|e| format!("cannot start engine: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let devices: Vec<String> = text.lines().filter(|l| l.contains('\t')).map(|l| l.replace('\t', ": ")).collect();
    let gfx = text.split(|c: char| !c.is_ascii_alphanumeric()).find(|w| w.starts_with("gfx") && w.len() > 3).map(str::to_owned);
    if !out.status.success() && devices.is_empty() {
        return Err(format!("engine failed to start: {}", text.lines().rev().take(3).collect::<Vec<_>>().join(" | ")));
    }
    Ok((devices, gfx))
}

async fn wheel_entries(name: &str) -> Result<(String, Vec<remote_zip::Entry>), String> {
    let r = &CATALOG.rocm;
    let idx = name.replace('_', "-");
    let file = format!("{name}-{}-py3-none-win_amd64.whl", r.version);
    let url = format!("{}/{idx}/{file}", r.index.trim_end_matches('/'));
    let entries = remote_zip::list(&url).await?;
    Ok((url, entries))
}

async fn fetch_wheel_subset(name: &str, keep: impl Fn(&str) -> bool, dest: &Path) -> Result<u64, String> {
    let (url, entries) = wheel_entries(name).await?;
    let record = entries.iter().find(|e| e.name.ends_with(".dist-info/RECORD")).cloned().ok_or("wheel has no RECORD")?;
    let hashes = remote_zip::record_hashes(&remote_zip::read_one(&url, &record).await?);
    let picks: Vec<(remote_zip::Entry, PathBuf)> = entries
        .into_iter()
        .filter_map(|e| {
            let rel = e.name.split_once("/bin/")?.1.to_owned();
            let safe = Path::new(&rel).components().all(|c| matches!(c, std::path::Component::Normal(_)));
            (safe && !e.name.ends_with('/') && keep(&rel)).then(|| (e, dest.join(rel)))
        })
        .collect();
    if picks.is_empty() {
        return Err(format!("{name}: no matching files"));
    }
    remote_zip::extract(&url, picks, &hashes).await
}

async fn rocm_runtime(bin: &Path, report: &Report) -> Result<String, String> {
    if os_key() != "windows" {
        return Ok(String::new());
    }
    report("ROCm runtime: shared libraries");
    let libs = ["hipblas.dll", "rocblas.dll", "rocsolver.dll", "libhipblaslt.dll", "origami.dll"];
    let n = fetch_wheel_subset("rocm_sdk_libraries", |p| libs.contains(&p) || p.starts_with("rocblas/") || p.starts_with("hipblaslt/"), bin).await?;
    report(&format!("ROCm runtime: shared libraries {}", download::human(n)));
    let (_, gfx) = list_devices(bin).await?;
    let gfx = gfx.ok_or("ROCm did not report a GPU architecture")?;
    report(&format!("ROCm runtime: kernels for {gfx}"));
    let n = fetch_wheel_subset(&format!("rocm_sdk_device_{gfx}"), |p| p.starts_with("rocblas/") || p.starts_with("hipblaslt/"), bin).await?;
    report(&format!("ROCm runtime: kernels {}", download::human(n)));
    Ok(gfx)
}

fn unzip(zip: &Path, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut a = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(|e| e.to_string())?;
    a.extract(dest).map_err(|e| format!("unzip {}: {e}", zip.display()))
}

pub async fn install(kind: EngineKind, report: &Report) -> Result<Installed, String> {
    if let Some(i) = installed(kind) {
        return Ok(i);
    }
    let key = format!("{}-{}", os_key(), kind.name());
    let asset = CATALOG.engines.assets.get(&key).ok_or_else(|| format!("no {} engine for {}", kind.name(), os_key()))?.clone();
    let root = engines_root();
    let dl = root.join("downloads");
    let dir = dir_of(kind);
    let tmp = root.join(format!(".{}-{}.tmp", CATALOG.engines.tag, kind.name()));
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    let mut zips = vec![(asset, format!("engine ({})", kind.name()))];
    if kind == EngineKind::Cuda && os_key() == "windows"
        && let Some(rt) = CATALOG.engines.assets.get("windows-cuda-runtime") {
            zips.push((rt.clone(), "CUDA runtime".into()));
        }
    for (a, label) in &zips {
        let file = dl.join(a.url.rsplit('/').next().unwrap_or("engine.zip"));
        download::fetch(&a.url, &file, Some(a.size), Some(&a.sha256), reqwest::header::HeaderMap::new(), label, report).await?;
        report(&format!("{label}: unpacking"));
        let (f, t) = (file.clone(), tmp.clone());
        tokio::task::spawn_blocking(move || unzip(&f, &t)).await.map_err(|e| e.to_string())??;
        let _ = tokio::fs::remove_file(&file).await;
        let _ = tokio::fs::remove_file(format!("{}.ok", file.display())).await;
    }
    let _ = tokio::fs::remove_dir(&dl).await;
    let bin = find_file(&tmp, &exe("sd-server")).and_then(|p| p.parent().map(Path::to_path_buf)).ok_or("engine archive has no sd-server")?;
    #[cfg(unix)]
    for n in ["sd-server", "sd-cli"] {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(bin.join(n), std::fs::Permissions::from_mode(0o755));
    }
    let gfx = if kind == EngineKind::Rocm { Some(rocm_runtime(&bin, report).await?).filter(|g| !g.is_empty()) } else { None };
    report(&format!("engine ({}): checking GPU", kind.name()));
    let (devices, _) = list_devices(&bin).await?;
    if kind != EngineKind::Cpu && !devices.iter().any(|d| kind.device_prefixes().iter().any(|p| d.starts_with(p))) {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(format!("{} engine found no usable GPU ({})", kind.name(), devices.join(", ")));
    }
    let info = Installed { tag: CATALOG.engines.tag.clone(), kind, devices, gfx };
    tokio::fs::write(tmp.join("installed.json"), serde_json::to_vec_pretty(&info).unwrap_or_default()).await.map_err(|e| e.to_string())?;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::rename(&tmp, &dir).await.map_err(|e| format!("{}: {e}", dir.display()))?;
    prune_old();
    Ok(info)
}

fn prune_old() {
    let tag = &CATALOG.engines.tag;
    if let Ok(rd) = std::fs::read_dir(engines_root()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && !n.starts_with(tag.as_str()) && n != "downloads" {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

pub async fn install_auto(hw: &Hw, report: &Report) -> Result<Installed, String> {
    let prefs = preference(hw);
    if prefs.is_empty() {
        return Err("No supported engine for this computer (Intel Macs are not supported). Use an API backend".into());
    }
    let mut errors = Vec::new();
    for k in prefs {
        match install(k, report).await {
            Ok(i) => return Ok(i),
            Err(e) => {
                report(&format!("{} engine not usable: {e}. Trying the next option", k.name()));
                errors.push(format!("{}: {e}", k.name()));
            }
        }
    }
    Err(format!("No engine works on this computer. {}", errors.join("; ")))
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn job_for(child: &tokio::process::Child) -> Option<Job> {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    let handle = child.raw_handle()?;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0);
        let job = Job(job);
        if SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, (&raw const info).cast(), size) == 0 {
            return None;
        }
        if AssignProcessToJobObject(job.0, handle.cast()) == 0 {
            return None;
        }
        Some(job)
    }
}

struct Running {
    key: String,
    url: String,
    child: tokio::process::Child,
    idle: Duration,
    #[cfg(windows)]
    _job: Option<Job>,
}

pub struct Manager {
    run: Mutex<Option<Running>>,
    active: AtomicUsize,
    last_used: AtomicU64,
}

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ms() -> u64 {
    u64::try_from(EPOCH.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn halt(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        drop(child.stdin.take());
        if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

pub static MANAGER: LazyLock<Arc<Manager>> = LazyLock::new(|| {
    let m = Arc::new(Manager { run: Mutex::new(None), active: AtomicUsize::new(0), last_used: AtomicU64::new(now_ms()) });
    let w = Arc::downgrade(&m);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let Some(m) = w.upgrade() else { return };
            if m.active.load(Ordering::Acquire) > 0 {
                continue;
            }
            let idle_for = Duration::from_millis(now_ms().saturating_sub(m.last_used.load(Ordering::Acquire)));
            let mut g = m.run.lock().await;
            if m.active.load(Ordering::Acquire) == 0 && g.as_ref().is_some_and(|r| idle_for >= r.idle)
                && let Some(mut r) = g.take() {
                    halt(&mut r.child).await;
                }
        }
    });
    m
});

pub struct Lease(Arc<Manager>);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.last_used.store(now_ms(), Ordering::Release);
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn log_path() -> PathBuf {
    crate::paths::data_dir().join("logs").join("engine.log")
}

fn tail(path: &Path, n: usize) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.lines().rev().filter(|l| !l.trim().is_empty() && !l.contains("s/it")).take(n).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

impl Manager {
    pub async fn stop(&self) {
        if let Some(mut r) = self.run.lock().await.take() {
            halt(&mut r.child).await;
        }
    }

    pub async fn ensure(self: &Arc<Self>, b: &Backend, report: &Report) -> Result<(String, Lease), String> {
        let kind = b.engine.as_deref().and_then(EngineKind::parse).ok_or("local backend has no engine set; run setup again")?;
        for (role, p) in &b.files {
            if !p.is_file() {
                return Err(format!("Model file for '{role}' is missing: {}. Run /imagegen-cc:setup to repair", p.display()));
            }
        }
        let key = serde_json::json!([kind, &b.files, &b.args]).to_string();
        self.active.fetch_add(1, Ordering::AcqRel);
        let lease = Lease(self.clone());
        let mut g = self.run.lock().await;
        if let Some(r) = g.as_mut()
            && r.key == key && r.child.try_wait().ok().flatten().is_none() {
                self.last_used.store(now_ms(), Ordering::Release);
                return Ok((r.url.clone(), lease));
            }
        if let Some(mut old) = g.take() {
            halt(&mut old.child).await;
        }
        if installed(kind).is_none() {
            install(kind, report).await?;
        }
        let bin = bin_dir(kind).ok_or("engine files are missing; run setup again")?;
        let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
        let mut args: Vec<String> = vec!["--listen-ip".into(), "127.0.0.1".into(), "--listen-port".into(), port.to_string()];
        for (role, p) in &b.files {
            if let Some(flag) = role_flag(role) {
                args.push(flag.into());
                args.push(p.to_string_lossy().into_owned());
            }
        }
        args.extend(b.args.iter().cloned());
        let log = log_path();
        if let Some(d) = log.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let out = std::fs::File::create(&log).map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let server = bin.join(exe("sd-server"));
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new(&server);
            c.args(&args).stdin(Stdio::null());
            c
        };
        #[cfg(unix)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c")
                .arg("\"$@\" </dev/null & p=$!; cat >/dev/null; kill $p 2>/dev/null; wait $p")
                .arg("sh")
                .arg(&server)
                .args(&args)
                .stdin(Stdio::piped());
            c
        };
        cmd.current_dir(&bin).stdout(out).stderr(err).kill_on_drop(true);
        no_window(&mut cmd);
        report("Starting the image engine");
        let mut child = cmd.spawn().map_err(|e| format!("cannot start engine: {e}"))?;
        #[cfg(windows)]
        let job = job_for(&child);
        let url = format!("http://127.0.0.1:{port}/v1");
        let started = Instant::now();
        loop {
            if let Ok(Some(st)) = child.try_wait() {
                return Err(format!("The image engine stopped ({st}). Last log lines:\n{}", tail(&log, 8)));
            }
            if download::client().get(format!("{url}/models")).timeout(Duration::from_secs(2)).send().await.is_ok_and(|r| r.status().is_success()) {
                break;
            }
            if started.elapsed() > Duration::from_secs(90) {
                halt(&mut child).await;
                return Err(format!("The image engine did not start in 90 s. Last log lines:\n{}", tail(&log, 8)));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let idle = Duration::from_secs(u64::from(if b.idle_minutes == 0 { 10 } else { b.idle_minutes }) * 60);
        self.last_used.store(now_ms(), Ordering::Release);
        *g = Some(Running {
            key,
            url: url.clone(),
            child,
            idle,
            #[cfg(windows)]
            _job: job,
        });
        Ok((url, lease))
    }
}
