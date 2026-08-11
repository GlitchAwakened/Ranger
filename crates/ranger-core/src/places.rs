//! Locations for the "Places" sidebar — **cross-platform**.
//!
//! Three families:
//!   - **Shortcuts**: standard user folders (`dirs`) — Home, Desktop,
//!     Documents, Downloads, Pictures, Music, Videos.
//!   - **Drives**: mounted volumes (`sysinfo`) — `C:`/`D:`/USB on Windows;
//!     `/`, `/home`, `/run/media/$USER/*`… on Linux (pseudo-mounts filtered out).
//!   - **Trash**: a single location. On Linux it's *navigable*
//!     (`~/.local/share/Trash/files`); on Windows it's virtual → the GUI
//!     opens it in Explorer (the GUI decides based on `kind`).
//!
//! Display (i18n labels for fixed entries, icons) is decided on the GUI side
//! from the `kind`; the core only returns a raw `name` (mostly useful for
//! drives).

use std::path::{Path, PathBuf};

/// Category of a location — drives the icon and label on the GUI side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceKind {
    /// Home folder (`$HOME` / `%USERPROFILE%`).
    Home,
    /// Standard user folder (Documents, Downloads…).
    Folder,
    /// Local mounted volume (drive, partition, removable device).
    Drive,
    /// Network location: mapped drive (`Z:` → `\\srv\part`), NFS/CIFS/SSHFS/GVFS
    /// mount (Linux), or WSL distribution (`\\wsl$\…`, Windows).
    Network,
    /// Trash.
    Trash,
}

/// A location listable in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub kind: PlaceKind,
    /// Target path (empty for the Windows trash, which has no real path).
    pub path: PathBuf,
    /// Raw displayable name (volume name / folder name). The GUI may
    /// replace it with an i18n label for `Home`/`Trash`.
    pub name: String,
    /// Removable device (USB, CD…) → offers "Safely Remove".
    pub removable: bool,
    /// Eject/disconnect target: `/dev/sdX1` (Linux), drive letter `X:`
    /// (local or mapped-network Windows drive), otherwise empty (WSL, GVFS…).
    pub device: String,
}

impl Place {
    /// Constructor for a simple location (not removable, no device) —
    /// shortcuts, trash, root.
    fn simple(kind: PlaceKind, path: PathBuf, name: String) -> Self {
        Place {
            kind,
            path,
            name,
            removable: false,
            device: String::new(),
        }
    }
}

/// Shortcuts to standard user folders (the ones that exist).
pub fn user_places() -> Vec<Place> {
    let mut out = Vec::new();
    let mut push = |dir: Option<PathBuf>, kind: PlaceKind| {
        if let Some(p) = dir
            && p.is_dir()
        {
            let name = file_name_of(&p);
            out.push(Place::simple(kind, p, name));
        }
    };
    push(dirs::home_dir(), PlaceKind::Home);
    push(dirs::desktop_dir(), PlaceKind::Folder);
    push(dirs::document_dir(), PlaceKind::Folder);
    push(dirs::download_dir(), PlaceKind::Folder);
    push(dirs::picture_dir(), PlaceKind::Folder);
    push(dirs::audio_dir(), PlaceKind::Folder);
    push(dirs::video_dir(), PlaceKind::Folder);
    out
}

/// LIGHTWEIGHT "signature" of the set of drives/mounts, to cheaply detect an
/// external change (subst, `net use`, USB, mount…) and refresh the sidebar
/// without constantly re-scanning.
///   - **Windows**: bitmask of mounted letters (`GetLogicalDrives`,
///     instantaneous) → catches a drive appearing/disappearing.
///   - **Linux**: hash of `/proc/self/mountinfo` (mounts) + of the GVFS
///     folder (user network shares) → catches (un)mounts.
///
/// Two calls returning the SAME value ⇒ nothing has changed.
pub fn drives_signature() -> u64 {
    #[cfg(windows)]
    {
        windrives::logical_drives_mask() as u64
    }
    #[cfg(target_os = "linux")]
    {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        if let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") {
            s.hash(&mut h);
        }
        // GVFS (user network shares, outside mountinfo): entry names.
        if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR")
            && let Ok(rd) = std::fs::read_dir(format!("{rt}/gvfs"))
        {
            let mut entries: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            entries.sort();
            entries.hash(&mut h);
        }
        h.finish()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}

/// Mounted volumes "visible to the user": LOCAL drives
/// (`PlaceKind::Drive`) **and** network locations (`PlaceKind::Network`) —
/// mapped drives, NFS/CIFS/SSHFS/GVFS mounts, WSL distributions. The
/// GUI splits the two families into "Drives" / "Network" sections.
pub fn drives() -> Vec<Place> {
    let mut out: Vec<Place> = Vec::new();

    // ----- Linux (and other non-Windows): mounts via `sysinfo` -----
    // Windows does NOT use `sysinfo` here: `GetLogicalDrives` (below) already
    // sees ALL letters — including mapped network, SUBST, VHD — and properly
    // computes type/removable/device. Avoiding the double pass removes any
    // ambiguity on `device`/`removable`.
    #[cfg(not(windows))]
    {
        use sysinfo::Disks;
        let disks = Disks::new_with_refreshed_list();
        for d in disks.list() {
            let mount = d.mount_point().to_path_buf();
            let fs = d.file_system().to_string_lossy().to_string();
            let is_net = is_network_fs(&fs);
            // A mount is kept if it's network (regardless of its path) OR
            // if it passes the "user-facing" filter for local mounts.
            if !is_net && !is_user_facing_mount(&mount, &fs) {
                continue;
            }
            // Deduplicate by mount point (sysinfo can list duplicates).
            if out.iter().any(|p| p.path == mount) {
                continue;
            }
            let name = drive_label(&mount, &d.name().to_string_lossy());
            out.push(Place {
                kind: if is_net {
                    PlaceKind::Network
                } else {
                    PlaceKind::Drive
                },
                path: mount.clone(),
                name,
                // Removable → ejectable. udisks heuristic: mount under
                // /run/media|/media. (Network is never "removable".)
                removable: !is_net && is_removable_mount(&mount),
                // Eject device: `/dev/sdX1` (= `Disk::name()` on Linux).
                device: if is_net {
                    String::new()
                } else {
                    d.name().to_string_lossy().to_string()
                },
            });
        }
    }

    // ----- Windows: all letters via the Win32 API -----
    #[cfg(windows)]
    for p in windrives::logical_drives() {
        if !out.iter().any(|e| e.path == p.path) {
            out.push(p);
        }
    }

    // Additional network locations WITHOUT a classic mount point:
    // GVFS (Linux) and WSL distributions (Windows).
    out.extend(network_extra());
    // Stable sort: first by family (local before network), then by path —
    // keeps sections grouped regardless of discovery order.
    out.sort_by(|a, b| {
        (a.kind == PlaceKind::Network)
            .cmp(&(b.kind == PlaceKind::Network))
            .then_with(|| a.path.cmp(&b.path))
    });
    // Guarantees the root "/" comes first: some systems (atomic / overlay /
    // composefs) don't report it via `sysinfo` → without this the Drives
    // section could be empty. (Windows always lists its letters.)
    #[cfg(not(windows))]
    if !out.iter().any(|p| p.path == Path::new("/")) {
        out.insert(
            0,
            Place::simple(PlaceKind::Drive, PathBuf::from("/"), String::new()),
        );
    }
    out
}

/// Does the path live on a NETWORK filesystem? Used to spare the network
/// expensive convenience work (e.g. recursive folder mtime, which walks the
/// whole tree — Explorer does nothing like that on a share).
/// Detection WITHOUT touching the network:
///   - **Windows**: UNC path, or `DRIVE_REMOTE` mapped letter
///     (`GetDriveTypeW` reads the LOCAL mount table — instantaneous);
///   - **Linux**: network fstype (`is_network_fs`) of the path's most
///     specific mount point (`/proc/self/mountinfo`, local read).
pub fn is_network_path(path: &Path) -> bool {
    if crate::fs::is_unc_path(path) {
        return true;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(p)) = path.components().next()
            && let Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) = p.kind()
        {
            return windrives::drive_is_remote(letter as char);
        }
        false
    }
    #[cfg(target_os = "linux")]
    {
        // Mount point with the longest prefix → its fstype.
        let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        let mut best: Option<(usize, bool)> = None; // (mount point length, network?)
        for line in info.lines() {
            // Format: … field[4] = mount point … "-" fstype source options
            let mut parts = line.split(' ');
            let Some(mount) = parts.nth(4) else { continue };
            let Some(fs) = line.split(" - ").nth(1).and_then(|r| r.split(' ').next()) else {
                continue;
            };
            if path.starts_with(mount) && best.is_none_or(|(l, _)| mount.len() > l) {
                best = Some((mount.len(), is_network_fs(fs)));
            }
        }
        best.map(|(_, net)| net).unwrap_or(false)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Result of enumerating a network server's shares.
pub enum NetShares {
    /// Visible shares (possibly empty).
    Ok(Vec<String>),
    /// Access denied / failed logon → offer a system login.
    AuthNeeded,
    /// Server unreachable (offline, invalid host, firewall…).
    Unreachable,
}

/// VISIBLE SMB shares of a server (`server` = host name without `\\`). Allows
/// "browsing" a `\\HOST` machine (like Explorer), where `std::fs` can only
/// list a single share. Windows only (`Unreachable` elsewhere).
pub fn net_shares(server: &str) -> NetShares {
    #[cfg(windows)]
    {
        windrives::shares_of(server)
    }
    #[cfg(not(windows))]
    {
        let _ = server;
        NetShares::Unreachable
    }
}

/// Establishes an authenticated network connection to `resource` (`\\HOST\share`
/// or `\\HOST\IPC$`), showing the **Windows credentials dialog** if needed.
/// `true` if connected (or already). Windows only. Blocking (modal).
pub fn net_connect_prompt(resource: &str) -> bool {
    #[cfg(windows)]
    {
        windrives::connect_prompt(resource)
    }
    #[cfg(not(windows))]
    {
        let _ = resource;
        false
    }
}

/// Network locations that don't show up as a "disk" mount point:
/// GVFS shares (Linux) and WSL distributions (Windows).
fn network_extra() -> Vec<Place> {
    #[cfg(target_os = "linux")]
    {
        gvfs_mounts()
    }
    #[cfg(windows)]
    {
        windrives::wsl_distros()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Vec::new()
    }
}

/// Enumeration of Windows drives via the Win32 API (complement to `sysinfo`).
///
/// `GetLogicalDrives` returns a bitmask of mounted letters (bit 0 = A:
/// … bit 25 = Z:) — it also sees **mapped network** drives, **SUBST** and
/// **mounted VHD** that `sysinfo` ignores. Each letter is enriched with its
/// type (`GetDriveTypeW`) and its volume label (`GetVolumeInformationW`).
///
/// A direct FFI to `kernel32`, linked by default under `windows-msvc`, avoids
/// an extra dependency.
#[cfg(windows)]
mod windrives {
    use super::{Place, PlaceKind};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
        fn GetVolumeInformationW(
            lp_root_path_name: *const u16,
            lp_volume_name_buffer: *mut u16,
            n_volume_name_size: u32,
            lp_volume_serial_number: *mut u32,
            lp_maximum_component_length: *mut u32,
            lp_file_system_flags: *mut u32,
            lp_file_system_name_buffer: *mut u16,
            n_file_system_name_size: u32,
        ) -> i32;
        // Querying a volume's bus (USB? → ejectable), see `bus_ejectable`.
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
    }
    const INVALID_HANDLE: isize = -1;
    const FILE_SHARE_RW: u32 = 0x0000_0003;
    const OPEN_EXISTING: u32 = 3;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002d_1400;

    // Target of a mapped network drive (`Z:` → `\\server\share`) + enumeration
    // of a `\\HOST` server's SMB shares. `mpr.dll` is a system DLL, which
    // avoids an extra crate.
    #[link(name = "mpr")]
    unsafe extern "system" {
        fn WNetGetConnectionW(local: *const u16, remote: *mut u16, len: *mut u32) -> u32;
        fn WNetOpenEnumW(
            scope: u32,
            ty: u32,
            usage: u32,
            netres: *mut NetResourceW,
            handle: *mut isize,
        ) -> u32;
        fn WNetEnumResourceW(
            handle: isize,
            count: *mut u32,
            buf: *mut core::ffi::c_void,
            bufsize: *mut u32,
        ) -> u32;
        fn WNetCloseEnum(handle: isize) -> u32;
        // Authenticated connection with system PROMPT (network login).
        fn WNetAddConnection2W(
            netres: *mut NetResourceW,
            password: *const u16,
            user: *const u16,
            flags: u32,
        ) -> u32;
    }
    // Win32 error codes "authentication required" vs "unreachable".
    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_ALREADY_ASSIGNED: u32 = 85;
    const ERROR_INVALID_PASSWORD: u32 = 86;
    const ERROR_SESSION_CREDENTIAL_CONFLICT: u32 = 1219;
    const ERROR_LOGON_FAILURE: u32 = 1326;
    const CONNECT_INTERACTIVE: u32 = 0x0000_0008;
    const CONNECT_PROMPT: u32 = 0x0000_0010;

    fn is_auth_error(rc: u32) -> bool {
        matches!(
            rc,
            ERROR_ACCESS_DENIED
                | ERROR_INVALID_PASSWORD
                | ERROR_SESSION_CREDENTIAL_CONFLICT
                | ERROR_LOGON_FAILURE
        )
    }

    /// Establishes an authenticated connection to `resource` (`\\HOST\share` or
    /// `\\HOST\IPC$`), showing the **Windows credentials dialog** if
    /// necessary (`CONNECT_PROMPT`). `true` if connected (or already connected),
    /// `false` if cancelled / failed. Blocking (modal dialog).
    pub fn connect_prompt(resource: &str) -> bool {
        let mut remote_w = wide(resource);
        let mut nr = NetResourceW {
            dw_scope: 0,
            dw_type: RESOURCETYPE_DISK,
            dw_display_type: 0,
            dw_usage: 0,
            lp_local_name: std::ptr::null_mut(),
            lp_remote_name: remote_w.as_mut_ptr(),
            lp_comment: std::ptr::null_mut(),
            lp_provider: std::ptr::null_mut(),
        };
        let rc = unsafe {
            WNetAddConnection2W(
                &mut nr,
                std::ptr::null(),
                std::ptr::null(),
                CONNECT_INTERACTIVE | CONNECT_PROMPT,
            )
        };
        matches!(rc, NO_ERROR | ERROR_ALREADY_ASSIGNED)
    }
    // `NETRESOURCEW` (Win32): 4 DWORDs then 4 pointers → `#[repr(C)]`.
    #[repr(C)]
    struct NetResourceW {
        dw_scope: u32,
        dw_type: u32,
        dw_display_type: u32,
        dw_usage: u32,
        lp_local_name: *mut u16,
        lp_remote_name: *mut u16,
        lp_comment: *mut u16,
        lp_provider: *mut u16,
    }
    const RESOURCE_GLOBALNET: u32 = 0x0000_0002;
    const RESOURCETYPE_DISK: u32 = 0x0000_0001;
    const NO_ERROR: u32 = 0;
    const ERROR_MORE_DATA: u32 = 234;

    /// Reads a NUL-terminated UTF-16 string from a pointer (remote share).
    unsafe fn pwstr_to_string(p: *const u16) -> String {
        unsafe {
            if p.is_null() {
                return String::new();
            }
            let mut len = 0isize;
            while *p.offset(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len as usize))
        }
    }

    /// Enumerates a server's VISIBLE disk shares (`server` = host name,
    /// without `\\`). Ignores hidden administrative shares (`C$`, `IPC$`…). A
    /// BLOCKING call (network timeout) — like `read_dir` on a remote path.
    /// Distinguishes "access denied" (→ offer a login) from "unreachable".
    pub fn shares_of(server: &str) -> super::NetShares {
        use super::NetShares;
        let mut remote_w = wide(&format!(r"\\{server}"));
        let mut nr = NetResourceW {
            dw_scope: RESOURCE_GLOBALNET,
            dw_type: RESOURCETYPE_DISK,
            dw_display_type: 0,
            dw_usage: 0,
            lp_local_name: std::ptr::null_mut(),
            lp_remote_name: remote_w.as_mut_ptr(),
            lp_comment: std::ptr::null_mut(),
            lp_provider: std::ptr::null_mut(),
        };
        let mut handle: isize = 0;
        let rc = unsafe {
            WNetOpenEnumW(
                RESOURCE_GLOBALNET,
                RESOURCETYPE_DISK,
                0,
                &mut nr,
                &mut handle,
            )
        };
        if rc != NO_ERROR {
            return if is_auth_error(rc) {
                NetShares::AuthNeeded
            } else {
                NetShares::Unreachable
            };
        }
        let mut shares = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let mut count: u32 = 0xFFFF_FFFF; // as many as the buffer can hold
            let mut bufsize: u32 = buf.len() as u32;
            let rc = unsafe {
                WNetEnumResourceW(
                    handle,
                    &mut count,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    &mut bufsize,
                )
            };
            if rc == ERROR_MORE_DATA {
                buf.resize(bufsize as usize, 0);
                continue;
            }
            if rc != NO_ERROR {
                break; // ERROR_NO_MORE_ITEMS (259) or end
            }
            let items = unsafe {
                std::slice::from_raw_parts(buf.as_ptr() as *const NetResourceW, count as usize)
            };
            for it in items {
                let remote = unsafe { pwstr_to_string(it.lp_remote_name) };
                // `\\HOST\share` → last segment = share name.
                if let Some(name) = remote.rsplit('\\').next()
                    && !name.is_empty()
                    && !name.ends_with('$')
                {
                    shares.push(name.to_string());
                }
            }
        }
        unsafe { WNetCloseEnum(handle) };
        super::NetShares::Ok(shares)
    }

    // Enumeration of WSL distributions via the registry (advapi32, system DLL).
    type Hkey = isize;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(hkey: Hkey, sub: *const u16, opts: u32, sam: u32, out: *mut Hkey) -> i32;
        fn RegEnumKeyExW(
            hkey: Hkey,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            last_write: *mut u64,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: Hkey,
            value: *const u16,
            reserved: *mut u32,
            ty: *mut u32,
            data: *mut u8,
            data_len: *mut u32,
        ) -> i32;
        fn RegCloseKey(hkey: Hkey) -> i32;
    }
    // `((HKEY)(LONG)0x80000001)` sign-extended to pointer size.
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as Hkey;
    const KEY_READ: u32 = 0x2_0019;

    // Drive types relevant to the user (UNKNOWN=0 and NO_ROOT_DIR=1 are
    // excluded).
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4; // mapped network
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Bitmask of mounted letters (bit 0 = A: … 25 = Z:) — instantaneous.
    /// Used as a "signature" to detect a drive appearing/disappearing.
    pub fn logical_drives_mask() -> u32 {
        unsafe { GetLogicalDrives() }
    }

    /// Is the letter a mapped NETWORK drive? `GetDriveTypeW` reads the local
    /// mount table (no network access, instantaneous).
    pub fn drive_is_remote(letter: char) -> bool {
        let root_w = wide(&format!("{letter}:\\"));
        unsafe { GetDriveTypeW(root_w.as_ptr()) == DRIVE_REMOTE }
    }

    pub fn logical_drives() -> Vec<Place> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let root_w = wide(&root);
            let dtype = unsafe { GetDriveTypeW(root_w.as_ptr()) };
            if !matches!(
                dtype,
                DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_REMOTE | DRIVE_CDROM | DRIVE_RAMDISK
            ) {
                continue;
            }
            let letter_str = format!("{letter}:");
            let is_net = dtype == DRIVE_REMOTE;
            // Mapped network: label "\\server\share (P:)" (resolved target),
            // otherwise fallback "P:". Local: "Label (P:)" or "P:".
            let name = if is_net {
                match mapped_remote(&letter_str) {
                    Some(r) => format!("{r} ({letter_str})"),
                    None => letter_str.clone(),
                }
            } else {
                match volume_label(&root_w) {
                    Some(l) if !l.is_empty() => format!("{l} ({letter_str})"),
                    _ => letter_str.clone(),
                }
            };
            out.push(Place {
                kind: if is_net {
                    PlaceKind::Network
                } else {
                    PlaceKind::Drive
                },
                path: PathBuf::from(&root),
                name,
                // Ejectable if removable/CD, OR if it's a "fixed" disk but on
                // an external bus (USB/1394/SD/MMC): Windows classifies many
                // USB flash drives/SSDs as DRIVE_FIXED, hence the bus check.
                removable: match dtype {
                    DRIVE_REMOVABLE | DRIVE_CDROM => true,
                    DRIVE_FIXED => bus_ejectable(letter),
                    _ => false, // network, ramdisk
                },
                // Eject/disconnect target: the letter (`P:`).
                device: letter_str,
            });
        }
        out
    }

    /// Is a "fixed" disk actually **ejectable**? The volume's bus is queried
    /// via `IOCTL_STORAGE_QUERY_PROPERTY`: USB / 1394 / SD / MMC (or
    /// removable media) ⇒ yes. Covers USB flash drives & SSDs that Windows
    /// files under `DRIVE_FIXED`. Best-effort: `false` if opening/IOCTL fails.
    fn bus_ejectable(letter: char) -> bool {
        let path = wide(&format!(r"\\.\{letter}:"));
        // 0 access rights: the property IOCTL is FILE_ANY_ACCESS.
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_RW,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if h == INVALID_HANDLE {
            return false;
        }
        // STORAGE_PROPERTY_QUERY { PropertyId = 0 (StorageDeviceProperty),
        // QueryType = 0 (PropertyStandardQuery), AdditionalParameters }.
        let query: [u32; 3] = [0, 0, 0];
        let mut buf = [0u8; 512];
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_STORAGE_QUERY_PROPERTY,
                query.as_ptr() as *mut core::ffi::c_void,
                std::mem::size_of_val(&query) as u32,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(h) };
        if ok == 0 || ret < 32 {
            return false;
        }
        // STORAGE_DEVICE_DESCRIPTOR : RemovableMedia @10 (BOOLEAN), BusType @28 (DWORD).
        let removable_media = buf[10] != 0;
        let bus_type = u32::from_ne_bytes([buf[28], buf[29], buf[30], buf[31]]);
        // External buses: 1394=0x4, USB=0x7, SD=0xC, MMC=0xD.
        removable_media || matches!(bus_type, 0x4 | 0x7 | 0xC | 0xD)
    }

    /// Target of a mapped network drive (`Z:` → `\\server\share`) via
    /// `WNetGetConnectionW`. `None` if not mapped / unavailable.
    fn mapped_remote(letter: &str) -> Option<String> {
        let local = wide(letter); // "Z:" (without backslash)
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let rc = unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) };
        if rc != 0 {
            return None; // NO_ERROR = 0
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
    }

    /// WSL distributions (including `docker-desktop`) exposed as `\\wsl$\<distro>`.
    /// Read from the registry (`HKCU\…\Lxss\<guid>\DistributionName`) — fast,
    /// without launching `wsl.exe` (no console flash, no service latency).
    pub fn wsl_distros() -> Vec<Place> {
        let mut out = Vec::new();
        let sub = wide(r"Software\Microsoft\Windows\CurrentVersion\Lxss");
        let mut lxss: Hkey = 0;
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut lxss) } != 0 {
            return out; // WSL not installed
        }
        let mut idx = 0u32;
        loop {
            let mut name = [0u16; 128];
            let mut nlen = name.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    lxss,
                    idx,
                    name.as_mut_ptr(),
                    &mut nlen,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                break; // ERROR_NO_MORE_ITEMS
            }
            idx += 1;
            let subname: Vec<u16> = name[..nlen as usize]
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .collect();
            let mut hsub: Hkey = 0;
            if unsafe { RegOpenKeyExW(lxss, subname.as_ptr(), 0, KEY_READ, &mut hsub) } != 0 {
                continue;
            }
            if let Some(distro) = reg_read_sz(hsub, "DistributionName") {
                out.push(Place {
                    kind: PlaceKind::Network,
                    path: PathBuf::from(format!(r"\\wsl$\{distro}")),
                    name: distro,
                    removable: false,
                    device: String::new(),
                });
            }
            unsafe { RegCloseKey(hsub) };
        }
        unsafe { RegCloseKey(lxss) };
        out
    }

    /// Reads a `REG_SZ` (string) value. `None` if absent / different type.
    fn reg_read_sz(hkey: Hkey, value: &str) -> Option<String> {
        let vw = wide(value);
        let mut buf = [0u16; 260];
        let mut len = (buf.len() * 2) as u32; // bytes
        let rc = unsafe {
            RegQueryValueExW(
                hkey,
                vw.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                &mut len,
            )
        };
        if rc != 0 {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
    }

    fn volume_label(root_w: &[u16]) -> Option<String> {
        let mut buf = [0u16; 261]; // MAX_PATH + 1
        let ok = unsafe {
            GetVolumeInformationW(
                root_w.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return None; // volume inaccessible (e.g. disconnected network)
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// The trash (a single location). On Linux: navigable. On Windows:
/// empty path → the GUI opens it in Explorer.
pub fn trash_place() -> Place {
    #[cfg(windows)]
    {
        Place::simple(PlaceKind::Trash, PathBuf::new(), String::new())
    }
    #[cfg(not(windows))]
    {
        let path = dirs::data_dir()
            .map(|d| d.join("Trash").join("files"))
            .unwrap_or_default();
        Place::simple(PlaceKind::Trash, path, String::new())
    }
}

// ----- Helpers -----

fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().to_string())
}

/// A drive's label (its **name**, not its path):
///   - **Windows**: `"Windows (C:)"` (volume label + letter);
///   - **Linux**: volume label (`/dev/disk/by-label`) → otherwise the mount
///     point's name (udisks removable mounts carry the label) → otherwise
///     empty for the root `/` (the GUI then sets an i18n "System" label).
///
/// `disk_name` = `Disk::name()` = **device path** (`/dev/sda1`…) on
/// Linux — hence the need to resolve it. (Windows builds its label directly
/// in `windrives`, without going through here.)
#[cfg(not(windows))]
fn drive_label(mount: &Path, disk_name: &str) -> String {
    #[cfg(target_os = "linux")]
    if let Some(label) = linux_volume_label(Path::new(disk_name))
        && !label.is_empty()
    {
        return label;
    }
    let _ = disk_name;
    if mount != Path::new("/") {
        let base = file_name_of(mount);
        if !base.is_empty() && base != "/" {
            return base;
        }
    }
    // Root without a label → the GUI will show an i18n "System" label.
    String::new()
}

/// Resolves a device's volume label via `/dev/disk/by-label/*`
/// (symlinks `LABEL → ../../<device>`). `None` if not found.
#[cfg(target_os = "linux")]
fn linux_volume_label(device: &Path) -> Option<String> {
    let target = std::fs::canonicalize(device).ok()?;
    let dir = std::fs::read_dir("/dev/disk/by-label").ok()?;
    for entry in dir.flatten() {
        if let Ok(resolved) = std::fs::canonicalize(entry.path())
            && resolved == target
        {
            return Some(udev_unescape(&entry.file_name().to_string_lossy()));
        }
    }
    None
}

/// Decodes udev escaping of `by-label` names (`\xNN` hex, e.g. `\x20`
/// for space). Rebuilds as bytes then UTF-8 (accented labels handled correctly).
#[cfg(target_os = "linux")]
fn udev_unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1] == b'x'
            && let Ok(code) = u8::from_str_radix(&s[i + 2..i + 4], 16)
        {
            out.push(code);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A **network** filesystem? (remote mount → "Network" section).
/// Covers common protocols and FUSE backends (`fuse.sshfs`, GVFS…).
#[cfg(not(windows))]
fn is_network_fs(fs: &str) -> bool {
    const NET_FS: &[&str] = &[
        "nfs",
        "nfs4",
        "cifs",
        "smbfs",
        "smb3",
        "9p",
        "afs",
        "ncpfs",
        "coda",
        "davfs",
        "ceph",
        "glusterfs",
        "fuse.sshfs",
        "fuse.gvfsd-fuse",
        "fuse.rclone",
        "fuse.s3fs",
        "fuse.davfs2",
    ];
    NET_FS.iter().any(|n| fs.eq_ignore_ascii_case(n))
}

/// Removable mount (USB, card, external disk) → ejectable.
/// udisks heuristic: removable devices are auto-mounted under
/// `/run/media/<user>/…` (Fedora/Arch) or `/media/…` (Debian/Ubuntu).
#[cfg(not(windows))]
fn is_removable_mount(mount: &Path) -> bool {
    let s = mount.to_string_lossy();
    s.starts_with("/run/media/") || s.starts_with("/media/")
}

/// Shares mounted by **GVFS** (GNOME/Nautilus) under
/// `/run/user/<uid>/gvfs/<backend>:<params>` — SMB, SFTP, MTP, DAV… They do
/// NOT show up as "disk" mounts (a single FUSE mount point), so the folder
/// is listed instead.
#[cfg(target_os = "linux")]
fn gvfs_mounts() -> Vec<Place> {
    let uid = unsafe { libc_getuid() };
    let root = PathBuf::from(format!("/run/user/{uid}/gvfs"));
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let raw = e.file_name().to_string_lossy().to_string();
            out.push(Place {
                kind: PlaceKind::Network,
                path,
                name: gvfs_pretty_name(&raw),
                removable: false,
                device: String::new(),
            });
        }
    }
    out
}

/// Turns a GVFS mount name into something readable: `smb-share:server=nas,share=media`
/// → `media (nas)`; `sftp:host=host.tld,user=me` → `host.tld (sftp)`. Kept
/// language-neutral on purpose (no embedded word like "on"/"at"): per this
/// module's own convention, the core only returns raw, un-translated names.
#[cfg(target_os = "linux")]
fn gvfs_pretty_name(raw: &str) -> String {
    let (backend, rest) = raw.split_once(':').unwrap_or((raw, ""));
    let mut params = std::collections::HashMap::new();
    for kv in rest.split(',') {
        if let Some((k, v)) = kv.split_once('=') {
            params.insert(k, v);
        }
    }
    let server = params.get("server").or_else(|| params.get("host")).copied();
    let share = params.get("share").copied();
    match (backend, share, server) {
        ("smb-share", Some(sh), Some(sv)) => format!("{sh} ({sv})"),
        (_, _, Some(sv)) => {
            let proto = backend.trim_end_matches("-share");
            format!("{sv} ({proto})")
        }
        _ => raw.to_string(),
    }
}

// `getuid(2)` — direct FFI to libc (already linked) → no dependency.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Should this mount point be shown to the user? Pseudo-filesystems and
/// irrelevant system mounts are excluded.
#[cfg(not(windows))]
fn is_user_facing_mount(mount: &Path, fs: &str) -> bool {
    // Pseudo / ephemeral filesystems → hidden. (NETWORK filesystems do NOT
    // pass through here: `drives()` handles them upstream via `is_network_fs`.)
    const HIDDEN_FS: &[&str] = &[
        "squashfs",
        "tmpfs",
        "devtmpfs",
        "overlay",
        "proc",
        "sysfs",
        "cgroup",
        "cgroup2",
        "ramfs",
        "autofs",
        "fuse.portal",
        "fuse.gvfsd-fuse",
    ];
    if HIDDEN_FS.iter().any(|h| fs.eq_ignore_ascii_case(h)) {
        return false;
    }
    if mount == Path::new("/") {
        return true;
    }
    let s = mount.to_string_lossy();
    // System mounts → hidden; common user mounts → kept.
    if s.starts_with("/boot")
        || s.starts_with("/snap")
        || s.starts_with("/var/lib/docker")
        || s.starts_with("/var/snap")
        || s == "/dev"
    {
        return false;
    }
    s == "/home"
        || s.starts_with("/home/")
        || s.starts_with("/run/media/")
        || s.starts_with("/media/")
        || s.starts_with("/mnt/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_places_includes_existing_home() {
        let places = user_places();
        // The home folder always exists in a test environment.
        if dirs::home_dir().map(|h| h.is_dir()).unwrap_or(false) {
            assert!(places.iter().any(|p| p.kind == PlaceKind::Home));
        }
        // All returned paths exist and are folders.
        for p in &places {
            assert!(
                p.path.is_dir(),
                "{} should be a directory",
                p.path.display()
            );
        }
    }

    #[test]
    fn drives_are_deduplicated_and_grouped() {
        let d = drives();
        // No duplicate mount point.
        for i in 1..d.len() {
            assert_ne!(d[i - 1].path, d[i].path, "no duplicate mount point");
        }
        // Only local or network drives.
        assert!(
            d.iter()
                .all(|p| matches!(p.kind, PlaceKind::Drive | PlaceKind::Network))
        );
        // Local ones come before network (grouped sections), and each family
        // is sorted by path.
        let first_net = d.iter().position(|p| p.kind == PlaceKind::Network);
        if let Some(k) = first_net {
            assert!(
                d[k..].iter().all(|p| p.kind == PlaceKind::Network),
                "all network places must be grouped at the end"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn network_fs_and_removable_detection() {
        assert!(is_network_fs("nfs4"));
        assert!(is_network_fs("cifs"));
        assert!(is_network_fs("fuse.sshfs"));
        assert!(!is_network_fs("ext4"));
        assert!(!is_network_fs("vfat"));
        assert!(is_removable_mount(Path::new("/run/media/u/USB")));
        assert!(is_removable_mount(Path::new("/media/u/Stick")));
        assert!(!is_removable_mount(Path::new("/")));
        assert!(!is_removable_mount(Path::new("/home/u")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gvfs_names_are_prettified() {
        assert_eq!(
            gvfs_pretty_name("smb-share:server=nas,share=media"),
            "media (nas)"
        );
        assert_eq!(
            gvfs_pretty_name("sftp:host=host.tld,user=me"),
            "host.tld (sftp)"
        );
        // Unknown format → returned as-is.
        assert_eq!(gvfs_pretty_name("weird-thing"), "weird-thing");
    }

    #[test]
    fn trash_place_kind() {
        assert_eq!(trash_place().kind, PlaceKind::Trash);
    }

    #[cfg(not(windows))]
    #[test]
    fn drives_always_include_root() {
        assert!(
            drives().iter().any(|p| p.path == Path::new("/")),
            "the / root must always be present"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn filters_pseudo_filesystems() {
        assert!(!is_user_facing_mount(Path::new("/proc"), "proc"));
        assert!(!is_user_facing_mount(
            Path::new("/snap/core/123"),
            "squashfs"
        ));
        assert!(is_user_facing_mount(Path::new("/"), "ext4"));
        assert!(is_user_facing_mount(Path::new("/run/media/u/USB"), "vfat"));
    }
}
