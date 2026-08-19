//! Device removal & network disconnection — **cross-platform**.
//!
//! Two operations, exposed to the GUI:
//!   - [`safe_remove`]: "safely remove" a removable device (USB, card,
//!     external drive). Linux: `udisksctl unmount` + `power-off`. Windows:
//!     clean removal via `cfgmgr32` (`CM_Request_Device_Eject` on the parent
//!     USB device, same as the notification area).
//!   - [`disconnect`]: disconnect a **mapped network drive** (Windows `Z:`
//!     → `WNetCancelConnection2W`). On Linux, shares are unmounted via the
//!     file manager — not covered here (browsing only).
//!
//! No dependencies: Linux shells out to `udisksctl` (present with udisks2, on
//! any desktop); Windows does FFI to system DLLs (`cfgmgr32`, `setupapi`,
//! `mpr`, `kernel32`).
//!
//! All functions return `Result<(), EjectError>`: a **data-only** error (no
//! embedded natural-language text) so the GUI can render it in the user's
//! language via `ranger-gui`'s i18n layer. `device` = the `Place::device`
//! field (`/dev/sdX1` on Linux; drive letter `X:` on Windows).

/// Data-only failure reason for [`safe_remove`] / [`disconnect`]. Carries no
/// natural-language text on purpose: the GUI translates each variant via its
/// i18n layer before showing it to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectError {
    /// Empty or otherwise unusable device identifier.
    UnknownDevice,
    /// Names of the processes holding the device (best-effort; may be
    /// shorter than the true list, never fabricated).
    BlockedByProcesses(Vec<String>),
    /// Windows veto category, no process could be identified.
    BlockedByService,
    BlockedByApplication,
    BlockedByOpenFile,
    DeviceBusy,
    /// Linux: neither `udisksctl` nor `umount` is installed.
    ToolNotFound,
    /// Linux: `udisksctl`/`umount` ran but failed. `detail` is the tool's own
    /// stderr when available (arbitrary text, outside Ranger's control).
    ToolFailed {
        bin: &'static str,
        detail: Option<String>,
    },
    /// Windows: `WNetCancelConnection2W` failed with this raw error code.
    DisconnectFailed(u32),
    /// Linux: network shares are not managed here (browsing only).
    LinuxNetworkDisconnectUnsupported,
    /// An OS-level lookup step failed unexpectedly (should be rare/never).
    SystemQueryFailed,
    /// No implementation for this OS.
    PlatformUnsupported,
}

/// Safely removes a removable device.
pub fn safe_remove(device: &str) -> Result<(), EjectError> {
    imp::safe_remove(device)
}

/// Disconnects a mapped network drive (Windows). No-op elsewhere.
pub fn disconnect(device: &str) -> Result<(), EjectError> {
    imp::disconnect(device)
}

// ===================== Linux =====================
#[cfg(target_os = "linux")]
mod imp {
    use super::EjectError;
    use std::process::Command;

    /// `udisksctl unmount -b <dev>` then `power-off -b <dev>` (best-effort for
    /// the power-off: cutting power can fail without invalidating the
    /// unmount). Falls back to `umount` if `udisksctl` is absent.
    pub fn safe_remove(device: &str) -> Result<(), EjectError> {
        if device.is_empty() {
            return Err(EjectError::UnknownDevice);
        }
        let unmount = match run("udisksctl", &["unmount", "-b", device]) {
            Ok(()) => Ok(()),
            // No udisks → plain unmount (may require privileges).
            Err(RunErr::NotFound) => run("umount", &[device]),
            Err(e) => Err(e),
        };
        match unmount {
            Ok(()) => {
                // Cut the power (the LED turns off) — non-blocking.
                let _ = run("udisksctl", &["power-off", "-b", device]);
                Ok(())
            }
            Err(run_err) => {
                // Unmount refused (typically "target is busy") → try to
                // identify the process(es) using the device.
                let who = blocking_processes(device);
                if who.is_empty() {
                    Err(run_err.into_eject_error())
                } else {
                    Err(EjectError::BlockedByProcesses(who))
                }
            }
        }
    }

    /// Names of the processes blocking the unmount, via `fuser -m <device>`
    /// (PIDs on stdout) + `/proc/<pid>/comm`. `fuser` is provided by psmisc
    /// (present on desktop systems). Empty if nothing found / `fuser` absent.
    fn blocking_processes(device: &str) -> Vec<String> {
        let out = match Command::new("fuser").arg("-m").arg(device).output() {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let mut names: Vec<String> = Vec::new();
        for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            // `fuser` may suffix the PID with an access-mode letter (e.g. "1234c").
            let pid: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
            if pid.is_empty() {
                continue;
            }
            if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                let name = comm.trim().to_string();
                if !name.is_empty() && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    pub fn disconnect(_device: &str) -> Result<(), EjectError> {
        // Linux network shares are unmounted via GVFS/gio or the admin;
        // not covered in v1 (browsing only).
        Err(EjectError::LinuxNetworkDisconnectUnsupported)
    }

    enum RunErr {
        NotFound,
        Failed {
            bin: &'static str,
            detail: Option<String>,
        },
    }
    impl RunErr {
        fn into_eject_error(self) -> EjectError {
            match self {
                RunErr::NotFound => EjectError::ToolNotFound,
                RunErr::Failed { bin, detail } => EjectError::ToolFailed { bin, detail },
            }
        }
    }

    fn run(bin: &'static str, args: &[&str]) -> Result<(), RunErr> {
        match Command::new(bin).args(args).output() {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(RunErr::Failed {
                    bin,
                    detail: (!msg.is_empty()).then_some(msg),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(RunErr::NotFound),
            Err(e) => Err(RunErr::Failed {
                bin,
                detail: Some(e.to_string()),
            }),
        }
    }
}

// ===================== Windows =====================
#[cfg(windows)]
mod imp {
    use super::EjectError;
    use std::os::windows::ffi::OsStrExt;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Extracts the letter from a device (`"Z:"`, `"Z:\\"`, `"z"` → `'Z'`).
    fn drive_letter(device: &str) -> Option<char> {
        device
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.is_ascii_alphabetic())
    }

    // ---- FFI ----
    #[repr(C)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }
    // GUID_DEVINTERFACE_DISK = 53F56307-B6BF-11D0-94F2-00A0C91EFB8B
    const GUID_DISK: Guid = Guid {
        d1: 0x53f5_6307,
        d2: 0xb6bf,
        d3: 0x11d0,
        d4: [0x94, 0xf2, 0x00, 0xa0, 0xc9, 0x1e, 0xfb, 0x8b],
    };

    #[repr(C)]
    struct StorageDeviceNumber {
        device_type: u32,
        device_number: u32,
        partition_number: i32,
    }
    #[repr(C)]
    struct SpDevInterfaceData {
        cb_size: u32,
        class_guid: Guid,
        flags: u32,
        reserved: usize,
    }
    #[repr(C)]
    struct SpDevinfoData {
        cb_size: u32,
        class_guid: Guid,
        dev_inst: u32,
        reserved: usize,
    }

    const INVALID_HANDLE: isize = -1;
    const FILE_SHARE_RW: u32 = 0x0000_0003;
    const OPEN_EXISTING: u32 = 3;
    const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002d_1080;
    const DIGCF_PRESENT: u32 = 0x02;
    const DIGCF_DEVICEINTERFACE: u32 = 0x10;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const CR_SUCCESS: u32 = 0;
    const CONNECT_UPDATE_PROFILE: u32 = 1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sa: *mut core::ffi::c_void,
            disp: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn DeviceIoControl(
            h: isize,
            ctl: u32,
            inbuf: *mut core::ffi::c_void,
            insz: u32,
            outbuf: *mut core::ffi::c_void,
            outsz: u32,
            ret: *mut u32,
            ov: *mut core::ffi::c_void,
        ) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn GetLastError() -> u32;
    }

    // Event log (wevtapi.dll, a system DLL → zero dependency). When a removal
    // request is refused because of open handles, the KERNEL logs Kernel-PnP
    // event 225 ("The application …\kate.exe with process id N stopped the
    // removal or ejection for the device …") to the System channel — with the
    // exe path, the PID, and the device involved, in RAW EventData fields
    // (independent of Windows' display language). This is the only source
    // that sees every veto, including a directory handle (an editor's
    // watcher, CWD sitting on the drive). Restart Manager only tracks
    // individually registered files and ignores these directories.
    #[link(name = "wevtapi")]
    unsafe extern "system" {
        fn EvtQuery(session: isize, path: *const u16, query: *const u16, flags: u32) -> isize;
        fn EvtNext(
            result_set: isize,
            events_size: u32,
            events: *mut isize,
            timeout: u32,
            flags: u32,
            returned: *mut u32,
        ) -> i32;
        fn EvtRender(
            context: isize,
            fragment: isize,
            flags: u32,
            buffer_size: u32,
            buffer: *mut core::ffi::c_void,
            buffer_used: *mut u32,
            property_count: *mut u32,
        ) -> i32;
        fn EvtClose(object: isize) -> i32;
    }
    const EVT_QUERY_CHANNEL_PATH: u32 = 0x1;
    const EVT_QUERY_REVERSE_DIRECTION: u32 = 0x200;
    const EVT_RENDER_EVENT_XML: u32 = 1;

    #[link(name = "setupapi")]
    unsafe extern "system" {
        fn SetupDiGetClassDevsW(
            guid: *const Guid,
            enumerator: *const u16,
            hwnd: isize,
            flags: u32,
        ) -> isize;
        fn SetupDiEnumDeviceInterfaces(
            devinfo: isize,
            devinfo_data: *mut core::ffi::c_void,
            guid: *const Guid,
            index: u32,
            out: *mut SpDevInterfaceData,
        ) -> i32;
        fn SetupDiGetDeviceInterfaceDetailW(
            devinfo: isize,
            iface: *const SpDevInterfaceData,
            detail: *mut u8,
            detail_size: u32,
            required: *mut u32,
            devinfo_data: *mut SpDevinfoData,
        ) -> i32;
        fn SetupDiDestroyDeviceInfoList(devinfo: isize) -> i32;
    }
    #[link(name = "cfgmgr32")]
    unsafe extern "system" {
        fn CM_Get_Parent(parent: *mut u32, devinst: u32, flags: u32) -> u32;
        fn CM_Get_Device_IDW(devinst: u32, buffer: *mut u16, buffer_len: u32, flags: u32) -> u32;
        fn CM_Request_Device_EjectW(
            devinst: u32,
            veto_type: *mut i32,
            veto_name: *mut u16,
            name_len: u32,
            flags: u32,
        ) -> u32;
    }
    #[link(name = "mpr")]
    unsafe extern "system" {
        fn WNetCancelConnection2W(name: *const u16, flags: u32, force: i32) -> u32;
    }

    /// A device instance identifier (e.g. `USB\VID_0951&PID_177F\…`), as it
    /// appears in the `DeviceInstance` field of event 225.
    unsafe fn device_id(devinst: u32) -> Option<String> {
        unsafe {
            let mut buf = [0u16; 256]; // MAX_DEVICE_ID_LEN = 200
            if CM_Get_Device_IDW(devinst, buf.as_mut_ptr(), buf.len() as u32, 0) != CR_SUCCESS {
                return None;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
            (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
        }
    }

    /// Values of the `<Data …>…</Data>` elements of an event rendered as XML,
    /// as `(name, value)` pairs. Minimal parser (good enough for EventData;
    /// quotes `'` or `"`), basic XML entities decoded.
    fn data_values(xml: &str) -> Vec<(String, String)> {
        fn unescape(s: &str) -> String {
            s.replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
        }
        let mut out = Vec::new();
        let mut rest = xml;
        while let Some(i) = rest.find("<Data") {
            rest = &rest[i + 5..];
            let Some(gt) = rest.find('>') else { break };
            let head = &rest[..gt];
            if head.ends_with('/') {
                rest = &rest[gt + 1..];
                continue; // <Data …/> self-closed: no value
            }
            // Name='…' attribute (EvtRender emits apostrophes; we also accept ").
            let name = head
                .split_once("Name=")
                .map(|(_, v)| v.trim_start_matches(['\'', '"']))
                .map(|v| v[..v.find(['\'', '"']).unwrap_or(v.len())].to_string())
                .unwrap_or_default();
            let after = &rest[gt + 1..];
            let Some(end) = after.find("</Data>") else {
                break;
            };
            out.push((name, unescape(&after[..end])));
            rest = &after[end..];
        }
        out
    }

    /// Processes responsible for an ejection veto, according to the SYSTEM LOG
    /// (Kernel-PnP 225 events emitted within the last `window_ms` ms — i.e.
    /// since the start of OUR attempt). The kernel records the exe path and
    /// PID of the culprit there, regardless of the handle type (file,
    /// directory, CWD). `expect_device` (Some = instance identifier of the
    /// ejected device) filters out vetoes from another device. "System"
    /// entries (pid 4, kernel bookkeeping, always present in duplicate) are
    /// ignored. Deduplicated result: `["kate.exe", …]`, empty if unavailable.
    unsafe fn veto_processes(window_ms: u64, expect_device: Option<&str>) -> Vec<String> {
        unsafe {
            let channel = wide("System");
            let query = wide(&format!(
                "*[System[(EventID=225) and TimeCreated[timediff(@SystemTime) <= {window_ms}]]]"
            ));
            let q = EvtQuery(
                0,
                channel.as_ptr(),
                query.as_ptr(),
                EVT_QUERY_CHANNEL_PATH | EVT_QUERY_REVERSE_DIRECTION,
            );
            if q == 0 {
                return Vec::new();
            }
            let mut names: Vec<String> = Vec::new();
            let mut events = [0isize; 16];
            let mut got = 0u32;
            if EvtNext(
                q,
                events.len() as u32,
                events.as_mut_ptr(),
                300,
                0,
                &mut got,
            ) != 0
            {
                for &evt in events.iter().take(got as usize) {
                    // 1st call: required size (in BYTES); 2nd: full XML render.
                    let mut used = 0u32;
                    let mut props = 0u32;
                    EvtRender(
                        0,
                        evt,
                        EVT_RENDER_EVENT_XML,
                        0,
                        std::ptr::null_mut(),
                        &mut used,
                        &mut props,
                    );
                    if used > 0 {
                        let mut buf = vec![0u16; (used as usize).div_ceil(2) + 1];
                        if EvtRender(
                            0,
                            evt,
                            EVT_RENDER_EVENT_XML,
                            (buf.len() * 2) as u32,
                            buf.as_mut_ptr() as *mut _,
                            &mut used,
                            &mut props,
                        ) != 0
                        {
                            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                            let xml = String::from_utf16_lossy(&buf[..end]);
                            let data = data_values(&xml);
                            let field = |k: &str| {
                                data.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
                            };
                            // Veto from ANOTHER device → not relevant.
                            if let (Some(want), Some(got_dev)) =
                                (expect_device, field("DeviceInstance"))
                                && !got_dev.eq_ignore_ascii_case(want)
                            {
                                continue;
                            }
                            let pid: u32 = field("ProcessId")
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                            // NT path (`\Device\HarddiskVolume3\…\kate.exe`) → base
                            // name; the kernel's "System" entry (pid 4) is ignored.
                            let name = field("ProcessName")
                                .filter(|p| p.contains('\\'))
                                .and_then(|p| p.rsplit(['\\', '/']).next())
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    (pid > 4)
                                        .then(|| crate::process_lock::process_executable_name(pid))
                                        .flatten()
                                });
                            if let Some(n) = name
                                && !n.is_empty()
                                && !names.iter().any(|e| e.eq_ignore_ascii_case(&n))
                            {
                                names.push(n);
                            }
                        }
                    }
                    EvtClose(evt);
                }
            }
            EvtClose(q);
            names
        }
    }

    /// Physical disk number of a volume (drive letter `X:`).
    unsafe fn disk_number_of(letter: char) -> Result<u32, EjectError> {
        unsafe {
            // `\\.\X:`: opens the VOLUME (no read/write access needed for the
            // info IOCTL).
            let path = wide(&format!(r"\\.\{letter}:"));
            let h = CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_RW,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                0,
            );
            if h == INVALID_HANDLE {
                return Err(EjectError::SystemQueryFailed);
            }
            let mut sdn = StorageDeviceNumber {
                device_type: 0,
                device_number: 0,
                partition_number: 0,
            };
            let mut ret = 0u32;
            let ok = DeviceIoControl(
                h,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null_mut(),
                0,
                &mut sdn as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<StorageDeviceNumber>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            );
            CloseHandle(h);
            if ok == 0 {
                return Err(EjectError::SystemQueryFailed);
            }
            Ok(sdn.device_number)
        }
    }

    /// Device instance (DEVINST) of physical disk number `target`.
    unsafe fn devinst_of_disk(target: u32) -> Result<u32, EjectError> {
        unsafe {
            let hdev = SetupDiGetClassDevsW(
                &GUID_DISK,
                std::ptr::null(),
                0,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            );
            if hdev == INVALID_HANDLE {
                return Err(EjectError::SystemQueryFailed);
            }
            let mut found: Option<u32> = None;
            let mut index = 0u32;
            loop {
                let mut ifd = SpDevInterfaceData {
                    cb_size: std::mem::size_of::<SpDevInterfaceData>() as u32,
                    class_guid: Guid {
                        d1: 0,
                        d2: 0,
                        d3: 0,
                        d4: [0; 8],
                    },
                    flags: 0,
                    reserved: 0,
                };
                if SetupDiEnumDeviceInterfaces(
                    hdev,
                    std::ptr::null_mut(),
                    &GUID_DISK,
                    index,
                    &mut ifd,
                ) == 0
                {
                    break; // no more interfaces
                }
                index += 1;

                // 1st call: required size; 2nd: retrieves the path + DEVINST.
                let mut needed = 0u32;
                SetupDiGetDeviceInterfaceDetailW(
                    hdev,
                    &ifd,
                    std::ptr::null_mut(),
                    0,
                    &mut needed,
                    std::ptr::null_mut(),
                );
                if GetLastError() != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
                    continue;
                }
                let mut buf = vec![0u8; needed as usize];
                // SP_DEVICE_INTERFACE_DETAIL_DATA_W.cbSize : 8 (64-bit) / 6 (32-bit).
                let cb: u32 = if cfg!(target_pointer_width = "64") {
                    8
                } else {
                    6
                };
                buf[..4].copy_from_slice(&cb.to_ne_bytes());
                let mut did = SpDevinfoData {
                    cb_size: std::mem::size_of::<SpDevinfoData>() as u32,
                    class_guid: Guid {
                        d1: 0,
                        d2: 0,
                        d3: 0,
                        d4: [0; 8],
                    },
                    dev_inst: 0,
                    reserved: 0,
                };
                if SetupDiGetDeviceInterfaceDetailW(
                    hdev,
                    &ifd,
                    buf.as_mut_ptr(),
                    needed,
                    std::ptr::null_mut(),
                    &mut did,
                ) == 0
                {
                    continue;
                }
                // Device path = right after the cbSize DWORD (offset 4).
                let path_ptr = buf.as_ptr().add(4) as *const u16;
                let h = CreateFileW(
                    path_ptr,
                    0,
                    FILE_SHARE_RW,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    0,
                );
                if h == INVALID_HANDLE {
                    continue;
                }
                let mut sdn = StorageDeviceNumber {
                    device_type: 0,
                    device_number: 0,
                    partition_number: 0,
                };
                let mut ret = 0u32;
                let ok = DeviceIoControl(
                    h,
                    IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    std::ptr::null_mut(),
                    0,
                    &mut sdn as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<StorageDeviceNumber>() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                );
                CloseHandle(h);
                if ok != 0 && sdn.device_number == target {
                    found = Some(did.dev_inst);
                    break;
                }
            }
            SetupDiDestroyDeviceInfoList(hdev);
            found.ok_or(EjectError::SystemQueryFailed)
        }
    }

    pub fn safe_remove(device: &str) -> Result<(), EjectError> {
        let letter = drive_letter(device).ok_or(EjectError::UnknownDevice)?;
        unsafe {
            let disk = disk_number_of(letter)?;
            let devinst = devinst_of_disk(disk)?;
            let mut parent = 0u32;
            if CM_Get_Parent(&mut parent, devinst, 0) != CR_SUCCESS {
                return Err(EjectError::SystemQueryFailed);
            }
            // Ejects the parent device (the entire USB disk), a SINGLE
            // attempt: as long as a process holds a handle, retrying changes
            // nothing (and each attempt costs ~400 ms of I/O on the drive) —
            // the user can click again after closing the application.
            // We ignore `veto_name` (a cryptic device instance like
            // "STORAGE\Volume\{…}"); the READABLE name of the culprit comes
            // from the log (Kernel-PnP event 225, written by the kernel at
            // the very moment of the veto — see `veto_processes`). `veto_type`
            // (category) serves as a fallback if the log stays silent.
            let parent_id = device_id(parent);
            let started = std::time::Instant::now();
            let mut veto_type = 0i32;
            let rc = CM_Request_Device_EjectW(parent, &mut veto_type, std::ptr::null_mut(), 0, 0);
            if rc == CR_SUCCESS && veto_type == 0 {
                return Ok(());
            }
            // Veto. Event 225 is logged by the kernel, but the EventLog
            // service only makes it visible to `EvtQuery` after a MEASURED
            // delay of ~1.4 s from the start of the attempt (`ejtest` harness,
            // controlled runs: blind polls up to 1.1 s, name shows up on the
            // next poll). So a single immediate read would be blind → we POLL
            // the log every 200 ms, returning as soon as a name shows up,
            // giving up after ~3 s.
            loop {
                // Window = since OUR attempt + margin (clock/latency).
                let window_ms = started.elapsed().as_millis() as u64 + 2_000;
                let procs = veto_processes(window_ms, parent_id.as_deref());
                if !procs.is_empty() {
                    return Err(EjectError::BlockedByProcesses(procs));
                }
                if started.elapsed() >= std::time::Duration::from_secs(3) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            // Still nothing in the log → veto category (PNP_VetoType),
            // NEVER the raw device instance.
            Err(match veto_type {
                4 => EjectError::BlockedByService,
                3 => EjectError::BlockedByApplication,
                2 | 5 => EjectError::BlockedByOpenFile, // PendingClose / OutstandingOpen
                _ => EjectError::DeviceBusy,
            })
        }
    }

    pub fn disconnect(device: &str) -> Result<(), EjectError> {
        let letter = drive_letter(device).ok_or(EjectError::UnknownDevice)?;
        // `Z:` (without a trailing backslash). `force = 0`: refuses if files are open.
        let name = wide(&format!("{letter}:"));
        let rc = unsafe { WNetCancelConnection2W(name.as_ptr(), CONNECT_UPDATE_PROFILE, 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(EjectError::DisconnectFailed(rc))
        }
    }
}

// ===================== Other OS =====================
#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::EjectError;

    pub fn safe_remove(_device: &str) -> Result<(), EjectError> {
        Err(EjectError::PlatformUnsupported)
    }
    pub fn disconnect(_device: &str) -> Result<(), EjectError> {
        Err(EjectError::PlatformUnsupported)
    }
}
