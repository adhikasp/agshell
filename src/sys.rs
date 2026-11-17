//! Platform bits: finding and killing a command's whole process tree, and
//! asking the kernel to keep orphaned grandchildren reachable.

use std::time::Duration;

/// Kills every process descended from this one.
///
/// The shell's children get their own process groups, so signalling agshell's
/// group would miss them. Both implementations walk the process tree instead.
pub fn kill_tree() {
    #[cfg(target_os = "linux")]
    {
        let me = i32::try_from(std::process::id()).unwrap_or(0);
        for _ in 0..5 {
            let children = linux::descendants_of(me);
            if children.is_empty() {
                break;
            }
            for pid in children {
                // SAFETY: `kill` on a pid read from /proc; a failure just means
                // the process already exited.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    {
        for _ in 0..5 {
            let children = windows::descendants_of(std::process::id());
            if children.is_empty() {
                break;
            }
            for pid in children {
                windows::terminate(pid);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Asks the kernel to reparent orphaned grandchildren to us, so a timeout can
/// still find them. (Linux only; on Windows the tree walk is enough.)
pub fn become_subreaper() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain prctl with the documented subreaper option.
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    /// Pids whose parent chain leads to `root`.
    pub fn descendants_of(root: i32) -> Vec<i32> {
        let mut edges: Vec<(i32, i32)> = Vec::new();

        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<i32>().ok())
                else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                // `comm` may contain spaces and parentheses; the fields we want
                // start after the last ')'.
                let Some(paren) = stat.rfind(')') else {
                    continue;
                };
                let mut fields = stat[paren + 1..].split_whitespace();
                let _state = fields.next();
                if let Some(ppid) = fields.next().and_then(|f| f.parse::<i32>().ok()) {
                    edges.push((pid, ppid));
                }
            }
        }

        let mut found = Vec::new();
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for (pid, ppid) in &edges {
                if *ppid == parent && *pid > 1 && !found.contains(pid) {
                    found.push(*pid);
                    frontier.push(*pid);
                }
            }
        }
        found
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    /// Pids whose parent chain leads to `root`.
    pub fn descendants_of(root: u32) -> Vec<u32> {
        let mut edges: Vec<(u32, u32)> = Vec::new();

        // SAFETY: the snapshot handle is checked, entry sizes are set per the
        // API contract, and the handle is closed on every path out.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot.is_null() || snapshot as isize == -1 {
                return Vec::new();
            }

            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0);

            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    edges.push((entry.th32ProcessID, entry.th32ParentProcessID));
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }

        let mut found = Vec::new();
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for (pid, ppid) in &edges {
                if *ppid == parent && *pid != 0 && !found.contains(pid) {
                    found.push(*pid);
                    frontier.push(*pid);
                }
            }
        }
        found
    }

    pub fn terminate(pid: u32) {
        // SAFETY: OpenProcess may fail (the process already exited); we check
        // before using the handle and close it.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}
