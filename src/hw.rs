use serde::Serialize;
use std::path::Path;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Other,
}

impl Vendor {
    fn from_pci(id: u32) -> Self {
        match id {
            0x10de => Self::Nvidia,
            0x1002 => Self::Amd,
            0x8086 => Self::Intel,
            _ => Self::Other,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Gpu {
    pub vendor: Vendor,
    pub name: String,
    pub vram_mb: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Hw {
    pub os: &'static str,
    pub arch: &'static str,
    pub ram_mb: u64,
    pub disk_free_mb: u64,
    pub gpus: Vec<Gpu>,
}

impl Hw {
    pub fn best_gpu(&self) -> Option<&Gpu> {
        self.gpus.iter().filter(|g| g.vendor != Vendor::Other).max_by_key(|g| g.vram_mb)
    }

    pub fn usable_vram_gb(&self) -> u32 {
        let mb = match self.best_gpu() {
            Some(g) if g.vendor == Vendor::Apple => self.ram_mb * 2 / 3,
            Some(g) => g.vram_mb,
            None => 0,
        };
        u32::try_from(mb.div_ceil(1024)).unwrap_or(u32::MAX)
    }

    pub fn ram_gb(&self) -> u32 {
        u32::try_from(self.ram_mb.div_ceil(1024)).unwrap_or(u32::MAX)
    }
}

fn existing_ancestor(p: &Path) -> &Path {
    let mut cur = p;
    while !cur.exists() {
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    cur
}

pub fn detect(dir: &Path) -> Hw {
    let dir = existing_ancestor(dir);
    Hw {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        ram_mb: imp::ram_mb(),
        disk_free_mb: imp::disk_free_mb(dir),
        gpus: dedupe(imp::gpus()),
    }
}

fn dedupe(mut gpus: Vec<Gpu>) -> Vec<Gpu> {
    gpus.retain(|g| !g.name.contains("Microsoft Basic") && !g.name.contains("Remote Display"));
    let mut out: Vec<Gpu> = Vec::with_capacity(gpus.len());
    for g in gpus {
        if !out.iter().any(|o| o.name == g.name && o.vram_mb == g.vram_mb) {
            out.push(g);
        }
    }
    out
}

#[cfg(windows)]
mod imp {
    use super::{Gpu, Vendor};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_ANY, RegGetValueW};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn reg_bytes(key: &[u16], value: &str) -> Option<Vec<u8>> {
        let v = wide(value);
        let mut len = 0u32;
        let mut ty = 0u32;
        let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), v.as_ptr(), RRF_RT_ANY, &raw mut ty, std::ptr::null_mut(), &raw mut len) };
        if rc != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), v.as_ptr(), RRF_RT_ANY, &raw mut ty, buf.as_mut_ptr().cast(), &raw mut len) };
        if rc != 0 {
            return None;
        }
        buf.truncate(len as usize);
        Some(buf)
    }

    fn reg_string(key: &[u16], value: &str) -> Option<String> {
        let b = reg_bytes(key, value)?;
        let w: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        Some(String::from_utf16_lossy(&w))
    }

    fn reg_u64(key: &[u16], value: &str) -> Option<u64> {
        let b = reg_bytes(key, value)?;
        match b.len() {
            8.. => Some(u64::from_le_bytes(b[..8].try_into().ok()?)),
            4..=7 => Some(u64::from(u32::from_le_bytes(b[..4].try_into().ok()?))),
            _ => None,
        }
    }

    pub fn gpus() -> Vec<Gpu> {
        let base = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
        (0..32)
            .filter_map(|i| {
                let key = wide(&format!(r"{base}\{i:04}"));
                let name = reg_string(&key, "DriverDesc")?;
                let dev = reg_string(&key, "MatchingDeviceId").unwrap_or_default().to_ascii_lowercase();
                let vendor = dev
                    .find("ven_")
                    .and_then(|p| u32::from_str_radix(dev.get(p + 4..p + 8)?, 16).ok())
                    .map_or(Vendor::Other, Vendor::from_pci);
                let bytes = reg_u64(&key, "HardwareInformation.qwMemorySize")
                    .or_else(|| reg_u64(&key, "HardwareInformation.MemorySize"))
                    .unwrap_or(0);
                Some(Gpu { vendor, name, vram_mb: bytes >> 20 })
            })
            .collect()
    }

    pub fn ram_mb() -> u64 {
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).unwrap_or(64);
        if unsafe { GlobalMemoryStatusEx(&raw mut m) } == 0 {
            return 0;
        }
        m.ullTotalPhys >> 20
    }

    pub fn disk_free_mb(dir: &Path) -> u64 {
        let p: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut avail = 0u64;
        if unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &raw mut avail, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return 0;
        }
        avail >> 20
    }
}

#[cfg(unix)]
mod imp {
    use super::{Gpu, Vendor};
    use std::path::Path;
    use std::process::Command;

    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let o = Command::new(cmd).args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    pub fn disk_free_mb(dir: &Path) -> u64 {
        run("df", &["-Pk", &dir.to_string_lossy()])
            .and_then(|s| s.lines().nth(1).and_then(|l| l.split_whitespace().nth(3)?.parse::<u64>().ok()))
            .map_or(0, |kb| kb >> 10)
    }

    #[cfg(target_os = "macos")]
    pub fn ram_mb() -> u64 {
        run("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.trim().parse::<u64>().ok()).map_or(0, |b| b >> 20)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn ram_mb() -> u64 {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map_or(0, |kb| kb >> 10)
    }

    #[cfg(target_os = "macos")]
    pub fn gpus() -> Vec<Gpu> {
        if std::env::consts::ARCH != "aarch64" {
            return Vec::new();
        }
        let name = run("sysctl", &["-n", "machdep.cpu.brand_string"]).map_or_else(|| "Apple Silicon".into(), |s| s.trim().to_owned());
        vec![Gpu { vendor: Vendor::Apple, name, vram_mb: ram_mb() }]
    }

    #[cfg(not(target_os = "macos"))]
    pub fn gpus() -> Vec<Gpu> {
        let mut out: Vec<Gpu> = run("nvidia-smi", &["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
            .map(|s| {
                s.lines()
                    .filter_map(|l| {
                        let (n, m) = l.rsplit_once(',')?;
                        Some(Gpu { vendor: Vendor::Nvidia, name: n.trim().to_owned(), vram_mb: m.trim().parse().ok()? })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Ok(rd) = std::fs::read_dir("/sys/class/drm") else { return out };
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if !n.starts_with("card") || n.contains('-') {
                continue;
            }
            let dev = e.path().join("device");
            let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok().map(|s| s.trim().to_owned());
            let vendor = read("vendor").and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()).map_or(Vendor::Other, Vendor::from_pci);
            if vendor == Vendor::Nvidia || vendor == Vendor::Other {
                continue;
            }
            let vram_mb = read("mem_info_vram_total").and_then(|v| v.parse::<u64>().ok()).map_or(0, |b| b >> 20);
            let name = read("product_name").filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{vendor:?} GPU"));
            out.push(Gpu { vendor, name, vram_mb });
        }
        out
    }
}
