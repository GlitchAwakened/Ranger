//! Lightweight diagnostic for files held open by another process.
//!
//! Ranger's hot path never goes through here: the diagnostic is only run
//! after a destructive operation (delete, rename, or replace) has actually
//! failed. On Windows, we first detect sharing conflicts with
//! `CreateFileW`, then hand off only the affected files to Restart Manager
//! in a single batched call. This avoids costly per-file registry writes
//! and bounds the work on large folders.

use std::path::Path;

/// Diagnostic of a lock likely preventing a file operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLock {
    /// Readable, deduplicated names (`Visual Studio Code (Code.exe)`, etc.).
    pub processes: Vec<String>,
    /// The diagnostic was deliberately stopped before exhausting all
    /// possible files or processes. The UI can then add an ellipsis
    /// without claiming the attribution is exhaustive.
    pub truncated: bool,
}

/// Best-effort identification of the processes holding `path` without
/// sufficient sharing to delete, rename, or replace it.
///
/// `None` means the error doesn't have the signature of a sharing conflict;
/// `Some` with an empty list means Windows confirms the lock but doesn't
/// let us attribute an owner (permissions, folder handle, race).
pub fn diagnose(path: &Path) -> Option<PathLock> {
    imp::diagnose(path)
}

#[cfg(windows)]
pub(crate) use imp::process_executable_name;

#[cfg(windows)]
mod imp {
    use std::collections::VecDeque;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::PathLock;

    const INVALID_HANDLE_VALUE: isize = -1;
    const DELETE_ACCESS: u32 = 0x0001_0000;
    const FILE_SHARE_ALL: u32 = 0x0000_0007;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_SHARING_VIOLATION: u32 = 32;
    const ERROR_MORE_DATA: u32 = 234;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    // An error diagnostic must stay bounded even if the user tries to
    // delete a gigantic tree. Four resources are enough to get an
    // actionable message; the `truncated` flag makes the limit visible.
    const MAX_PROBED_ENTRIES: usize = 1_024;
    const MAX_LOCKED_RESOURCES: usize = 4;
    const MAX_REPORTED_PROCESSES: usize = 4;

    const RM_APP_NAME_LEN: usize = 256;
    const RM_SERVICE_NAME_LEN: usize = 64;
    const RM_SESSION_KEY_LEN: usize = 32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RmUniqueProcess {
        process_id: u32,
        process_start_time: FileTime,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RmProcessInfo {
        process: RmUniqueProcess,
        app_name: [u16; RM_APP_NAME_LEN],
        service_short_name: [u16; RM_SERVICE_NAME_LEN],
        application_type: i32,
        app_status: u32,
        terminal_session_id: u32,
        restartable: i32,
    }

    impl RmProcessInfo {
        fn empty() -> Self {
            Self {
                process: RmUniqueProcess {
                    process_id: 0,
                    process_start_time: FileTime {
                        low_date_time: 0,
                        high_date_time: 0,
                    },
                },
                app_name: [0; RM_APP_NAME_LEN],
                service_short_name: [0; RM_SERVICE_NAME_LEN],
                application_type: 0,
                app_status: 0,
                terminal_session_id: 0,
                restartable: 0,
            }
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security_attributes: *mut core::ffi::c_void,
            creation_disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
        fn QueryFullProcessImageNameW(
            process: isize,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
    }

    #[link(name = "rstrtmgr")]
    unsafe extern "system" {
        fn RmStartSession(handle: *mut u32, flags: u32, session_key: *mut u16) -> u32;
        fn RmRegisterResources(
            handle: u32,
            file_count: u32,
            file_names: *const *const u16,
            application_count: u32,
            applications: *const RmUniqueProcess,
            service_count: u32,
            service_names: *const *const u16,
        ) -> u32;
        fn RmGetList(
            handle: u32,
            needed: *mut u32,
            count: *mut u32,
            affected_apps: *mut RmProcessInfo,
            reboot_reasons: *mut u32,
        ) -> u32;
        fn RmEndSession(handle: u32) -> u32;
    }

    struct RestartManagerSession(u32);

    impl RestartManagerSession {
        unsafe fn start() -> Option<Self> {
            unsafe {
                let mut handle = 0u32;
                let mut key = [0u16; RM_SESSION_KEY_LEN + 1];
                let result = RmStartSession(&mut handle, 0, key.as_mut_ptr());
                (result == ERROR_SUCCESS).then_some(Self(handle))
            }
        }
    }

    impl Drop for RestartManagerSession {
        fn drop(&mut self) {
            unsafe {
                let _ = RmEndSession(self.0);
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn wide_text(value: &[u16]) -> String {
        let end = value
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(value.len());
        String::from_utf16_lossy(&value[..end]).trim().to_owned()
    }

    /// Executable name of a PID, shared with the eject diagnostic.
    pub(crate) unsafe fn process_executable_name(process_id: u32) -> Option<String> {
        unsafe {
            if process_id == 0 {
                return None;
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
            if process == 0 || process == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut buffer = [0u16; 1_024];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len);
            CloseHandle(process);
            if ok == 0 || len == 0 {
                return None;
            }
            let full = String::from_utf16_lossy(&buffer[..len as usize]);
            full.rsplit(['\\', '/'])
                .next()
                .map(str::to_owned)
                .filter(|name| !name.is_empty())
        }
    }

    /// Attempts to open the object with DELETE rights while allowing all
    /// sharing modes. `ERROR_SHARING_VIOLATION` therefore means an existing
    /// handle didn't grant `FILE_SHARE_DELETE` and can genuinely block the action.
    unsafe fn has_delete_share_conflict(path: &Path) -> bool {
        unsafe {
            let path = wide(path);
            let handle = CreateFileW(
                path.as_ptr(),
                DELETE_ACCESS,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                // Necessary for folders, with no adverse effect on files.
                FILE_FLAG_BACKUP_SEMANTICS,
                0,
            );
            if handle != INVALID_HANDLE_VALUE {
                CloseHandle(handle);
                return false;
            }
            GetLastError() == ERROR_SHARING_VIOLATION
        }
    }

    struct LockProbe {
        sharing_violation: bool,
        files: Vec<PathBuf>,
        truncated: bool,
    }

    fn probe_locked_files(path: &Path, scan_children: bool) -> LockProbe {
        let mut result = LockProbe {
            sharing_violation: false,
            files: Vec::new(),
            truncated: false,
        };
        // Probe BEFORE metadata: a handle opened with share=0 can prevent
        // even reading the attributes, precisely in the case we're diagnosing.
        let root_locked = unsafe { has_delete_share_conflict(path) };
        result.sharing_violation = root_locked;
        let Ok(root_metadata) = std::fs::symlink_metadata(path) else {
            if root_locked {
                // Unknown type: Restart Manager will accept a file and cleanly
                // refuse a folder, which will keep the generic message.
                result.files.push(path.to_path_buf());
            }
            return result;
        };
        let root_is_dir = root_metadata.is_dir() && !root_metadata.file_type().is_symlink();
        if root_locked && !root_is_dir {
            result.files.push(path.to_path_buf());
            return result;
        }
        if !root_is_dir {
            return result;
        }
        // Restart Manager can only attribute local resources. On a network
        // share, recursively probing each child would add network round-trips
        // without being able to identify the process running on the server.
        if !scan_children {
            return result;
        }

        let mut pending = VecDeque::from([path.to_path_buf()]);
        let mut probed = 0usize;
        while let Some(directory) = pending.pop_front() {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                if probed >= MAX_PROBED_ENTRIES {
                    result.truncated = true;
                    return result;
                }
                probed += 1;
                let child = entry.path();
                // `DirEntry::file_type` normally reuses the attributes from the
                // parent's enumeration and stays available for a file whose
                // sharing is refused by another process.
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let is_dir = file_type.is_dir() && !file_type.is_symlink();
                if unsafe { has_delete_share_conflict(&child) } {
                    result.sharing_violation = true;
                    // Restart Manager refuses folders. A folder lock will
                    // therefore stay a generic diagnostic, while a file child
                    // usually lets us identify the program.
                    if !is_dir {
                        result.files.push(child.clone());
                        if result.files.len() >= MAX_LOCKED_RESOURCES {
                            result.truncated = true;
                            return result;
                        }
                    }
                }
                if is_dir {
                    pending.push_back(child);
                }
            }
        }
        result
    }

    unsafe fn restart_manager_processes(files: &[PathBuf]) -> (Vec<String>, bool) {
        unsafe {
            if files.is_empty() {
                return (Vec::new(), false);
            }
            let Some(session) = RestartManagerSession::start() else {
                return (Vec::new(), false);
            };
            let wide_files: Vec<Vec<u16>> = files.iter().map(|path| wide(path)).collect();
            let file_ptrs: Vec<*const u16> = wide_files.iter().map(|path| path.as_ptr()).collect();
            let register = RmRegisterResources(
                session.0,
                file_ptrs.len() as u32,
                file_ptrs.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            );
            if register != ERROR_SUCCESS {
                return (Vec::new(), false);
            }

            let mut needed = 0u32;
            let mut count = 0u32;
            let mut reboot_reasons = 0u32;
            let first = RmGetList(
                session.0,
                &mut needed,
                &mut count,
                std::ptr::null_mut(),
                &mut reboot_reasons,
            );
            if first == ERROR_SUCCESS || needed == 0 {
                return (Vec::new(), false);
            }
            if first != ERROR_MORE_DATA {
                return (Vec::new(), false);
            }

            // A file locked by hundreds of processes would be abnormal;
            // this cap protects memory against a misbehaving driver.
            let capacity = usize::min(needed as usize, 256);
            let mut infos = vec![RmProcessInfo::empty(); capacity];
            count = capacity as u32;
            let second = RmGetList(
                session.0,
                &mut needed,
                &mut count,
                infos.as_mut_ptr(),
                &mut reboot_reasons,
            );
            if second != ERROR_SUCCESS {
                return (Vec::new(), false);
            }

            let mut names = Vec::new();
            let mut truncated = false;
            for info in infos.iter().take(count as usize) {
                let app = wide_text(&info.app_name);
                let exe = process_executable_name(info.process.process_id);
                let display = match (app.is_empty(), exe) {
                    (true, Some(exe)) => exe,
                    (false, Some(exe))
                        if !app
                            .to_ascii_lowercase()
                            .contains(&exe.trim_end_matches(".exe").to_ascii_lowercase()) =>
                    {
                        format!("{app} ({exe})")
                    }
                    (false, _) => app,
                    (true, None) => continue,
                };
                if !names
                    .iter()
                    .any(|name: &String| name.eq_ignore_ascii_case(&display))
                {
                    if names.len() >= MAX_REPORTED_PROCESSES {
                        truncated = true;
                        break;
                    }
                    names.push(display);
                }
            }
            (names, truncated)
        }
    }

    pub(super) fn diagnose(path: &Path) -> Option<PathLock> {
        let scan_children = !crate::places::is_network_path(path);
        let probe = probe_locked_files(path, scan_children);
        if !probe.sharing_violation {
            return None;
        }
        let (processes, processes_truncated) = unsafe { restart_manager_processes(&probe.files) };
        Some(PathLock {
            processes,
            truncated: probe.truncated || processes_truncated,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::{
            CloseHandle, CreateFileW, INVALID_HANDLE_VALUE, MAX_LOCKED_RESOURCES, OPEN_EXISTING,
            diagnose, probe_locked_files, wide, wide_text,
        };

        struct HandleGuard(isize);

        impl Drop for HandleGuard {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }

        #[test]
        fn wide_process_name_stops_at_the_first_nul() {
            assert_eq!(wide_text(&[b'C' as u16, b'o' as u16, 0, b'x' as u16]), "Co");
        }

        #[test]
        fn delete_share_conflict_is_detected_without_modifying_the_file() {
            let path = std::env::temp_dir().join(format!(
                "ranger-lock-probe-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::write(&path, b"kept").unwrap();
            let encoded = wide(&path);
            // `share = 0` reproduces a program that doesn't grant
            // FILE_SHARE_DELETE, without deleting or altering the file.
            let handle = unsafe {
                CreateFileW(
                    encoded.as_ptr(),
                    0x8000_0000, // GENERIC_READ
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    0,
                )
            };
            assert_ne!(handle, INVALID_HANDLE_VALUE);
            let guard = HandleGuard(handle);

            let lock = diagnose(&path).unwrap();
            assert!(!lock.truncated);
            drop(guard);
            assert_eq!(std::fs::read(&path).unwrap(), b"kept");
            std::fs::remove_file(path).unwrap();
        }

        #[test]
        fn directory_probe_stops_after_four_locked_files() {
            let directory = std::env::temp_dir().join(format!(
                "ranger-lock-probe-directory-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir(&directory).unwrap();
            let mut guards = Vec::new();
            for index in 0..=MAX_LOCKED_RESOURCES {
                let path = directory.join(format!("locked-{index}.txt"));
                std::fs::write(&path, b"kept").unwrap();
                let encoded = wide(&path);
                let handle = unsafe {
                    CreateFileW(
                        encoded.as_ptr(),
                        0x8000_0000, // GENERIC_READ
                        0,
                        std::ptr::null_mut(),
                        OPEN_EXISTING,
                        0,
                        0,
                    )
                };
                assert_ne!(handle, INVALID_HANDLE_VALUE);
                guards.push(HandleGuard(handle));
            }

            let shallow_probe = probe_locked_files(&directory, false);
            assert!(!shallow_probe.sharing_violation);
            assert!(shallow_probe.files.is_empty());

            let probe = probe_locked_files(&directory, true);
            assert!(probe.sharing_violation);
            assert_eq!(probe.files.len(), MAX_LOCKED_RESOURCES);
            assert!(probe.truncated);

            drop(guards);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    use super::PathLock;

    pub(super) fn diagnose(_path: &Path) -> Option<PathLock> {
        None
    }
}
