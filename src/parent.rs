#[cfg(unix)]
pub async fn exited() {
    let parent = std::os::unix::process::parent_id();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    while std::os::unix::process::parent_id() == parent {
        tick.tick().await;
    }
}

#[cfg(windows)]
pub async fn exited() {
    let Some(handle) = parent_handle() else { return std::future::pending().await };
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(handle as windows_sys::Win32::Foundation::HANDLE, windows_sys::Win32::System::Threading::INFINITE) };
        let _ = tx.send(());
    });
    let _ = rx.await;
}

#[cfg(windows)]
fn parent_handle() -> Option<usize> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_SYNCHRONIZE};
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let me = GetCurrentProcessId();
        let mut entry = PROCESSENTRY32W { dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).unwrap_or(0), ..Default::default() };
        let mut parent = None;
        let mut more = Process32FirstW(snap, &raw mut entry);
        while more != 0 {
            if entry.th32ProcessID == me {
                parent = Some(entry.th32ParentProcessID);
                break;
            }
            more = Process32NextW(snap, &raw mut entry);
        }
        CloseHandle(snap);
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, parent?);
        (!handle.is_null()).then_some(handle as usize)
    }
}
