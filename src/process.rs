//! Process discovery utilities.
//!
//! Most callers will already know the PID of the game they want to inject
//! into (e.g. via Riot's GEP, telemetry, or the tray-app's own process
//! list). [`find_by_name`] is provided as a convenience for the common
//! "give me the running League/Valorant/etc. PID by exe name" case.

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::mem;

    use windows::Win32::{
        Foundation::CloseHandle,
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };

    /// Look up a running process by its executable file name (case-
    /// insensitive). Returns the first matching PID. Names like
    /// `"League of Legends.exe"` are expected.
    pub fn find_by_name(name: &str) -> Option<u32> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry: PROCESSENTRY32W = mem::zeroed();
            entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;

            let mut found: Option<u32> = None;
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let exe = wstr_to_string(&entry.szExeFile);
                    if exe.eq_ignore_ascii_case(name) {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
            found
        }
    }

    fn wstr_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::find_by_name;

#[cfg(not(target_os = "windows"))]
pub fn find_by_name(_name: &str) -> Option<u32> {
    None
}
