//! Windows portable devices shown in the sidebar.
//!
//! Android typically exposes its storage via MTP/WPD in the Shell's
//! namespace, with no drive letter and therefore no path usable by Ranger's
//! `std::fs` backend. Detection combines two system APIs:
//!
//! - the Shell provides the label and the exact parsing name to open in
//!   Explorer;
//! - WPD confirms Android devices when their Shell item doesn't expose
//!   the optional `System.Devices.DeviceInstanceId` property.
//!
//! The scan remains fully asynchronous. Its resolution is cached, and
//! WPD is only consulted for ambiguous items, never on the UI thread.

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tracing::{debug, warn};
use windows::Win32::Devices::PortableDevices::{IPortableDeviceManager, PortableDeviceManager};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_ComputerFolder, IEnumShellItems, IShellItem, IShellItem2,
    KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY,
};
use windows::core::{Interface, PCWSTR, PWSTR};

/// `System.Devices.DeviceInstanceId` / `PKEY_Devices_DeviceInstanceId`.
/// Some MTP drivers don't publish this property on their Shell item; its
/// absence therefore isn't enough to rule out a phone.
const PKEY_DEVICES_DEVICE_INSTANCE_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x78c34fc8_104a_4aca_9ea4_524d52996e57),
    pid: 256,
};

/// Avoids recreating the WPD manager on every poll when a non-WPD virtual
/// folder is present in "This PC". Any Shell change immediately short-circuits
/// this delay so that plugging/unplugging is detected.
const WPD_RETRY_INTERVAL: Duration = Duration::from_secs(10);
const MAX_WPD_STRING_LEN: u32 = 32_768;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableDevice {
    pub name: String,
    pub shell_path: String,
    device_instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellCandidate {
    name: String,
    shell_path: String,
    device_instance_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WpdDevice {
    name: Option<String>,
    device_instance_id: String,
}

#[derive(Debug)]
struct ResolutionOutcome {
    devices: Vec<PortableDevice>,
    has_unresolved_candidates: bool,
}

#[derive(Debug)]
struct ResolutionCache {
    candidates: Vec<ShellCandidate>,
    devices: Vec<PortableDevice>,
    has_unresolved_candidates: bool,
    last_wpd_attempt: Instant,
}

static DEVICES: OnceLock<RwLock<Vec<PortableDevice>>> = OnceLock::new();
static RESOLUTION_CACHE: OnceLock<Mutex<Option<ResolutionCache>>> = OnceLock::new();
static SCAN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static REVISION: AtomicU64 = AtomicU64::new(0);

fn devices_cache() -> &'static RwLock<Vec<PortableDevice>> {
    DEVICES.get_or_init(|| RwLock::new(Vec::new()))
}

fn resolution_cache() -> &'static Mutex<Option<ResolutionCache>> {
    RESOLUTION_CACHE.get_or_init(|| Mutex::new(None))
}

/// Instant snapshot of the last completed scan. Never touches the Shell and
/// can therefore be called from the UI thread.
pub fn devices() -> Vec<PortableDevice> {
    devices_cache()
        .read()
        .map(|devices| devices.clone())
        .unwrap_or_default()
}

/// Monotonic cache revision, mixed into the drive-letter signature by the
/// bridge. It only changes if the visible list actually changes.
pub fn signature() -> u64 {
    REVISION.load(Ordering::Acquire)
}

/// Requests an asynchronous scan. Slint polls may call this function
/// every 1.5 s: only one scan is allowed at a time, and the UI thread never
/// blocks on COM or on a slow phone driver.
pub fn request_refresh() {
    if SCAN_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    std::thread::spawn(|| {
        struct ScanGuard;
        impl Drop for ScanGuard {
            fn drop(&mut self) {
                SCAN_IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        let _guard = ScanGuard;

        let next = match scan_shell_devices() {
            Ok(devices) => devices,
            Err(err) => {
                debug!(error = %err, "portable device scan failed");
                return;
            }
        };
        let Ok(mut current) = devices_cache().write() else {
            warn!("portable device cache lock poisoned");
            return;
        };
        if *current != next {
            *current = next;
            REVISION.fetch_add(1, Ordering::AcqRel);
        }
    });
}

/// Opens the exact virtual object. `shell_path` comes exclusively from
/// `SIGDN_DESKTOPABSOLUTEPARSING` and is passed as an opaque argument: no
/// command-line concatenation or shell interpretation.
pub fn open(shell_path: &str) -> Result<()> {
    if shell_path.trim().is_empty() {
        return Err(anyhow!("empty device Shell path"));
    }
    Command::new("explorer.exe")
        .arg(shell_path)
        .spawn()
        .map(|_| ())
        .map_err(|err| anyhow!("opening portable device: {err}"))
}

fn scan_shell_devices() -> Result<Vec<PortableDevice>> {
    // Fresh dedicated thread: STA suits Shell extensions. We only call
    // `CoUninitialize` if this thread actually acquired COM.
    let com_initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    if !com_initialized {
        return Err(anyhow!("COM initialization failed"));
    }

    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _com_guard = ComGuard;

    unsafe {
        let candidates = enumerate_shell_candidates()?;
        resolve_with_cache(candidates)
    }
}

/// Reuses the last result as long as the Shell view hasn't changed.
/// Candidates that stayed ambiguous are retried at a bounded rate, which
/// covers a WPD driver still initializing without continuously loading
/// Ranger.
unsafe fn resolve_with_cache(candidates: Vec<ShellCandidate>) -> Result<Vec<PortableDevice>> {
    unsafe {
        let now = Instant::now();
        if let Ok(cache) = resolution_cache().lock()
            && let Some(cached) = cache.as_ref()
        {
            let retry_due = cached.has_unresolved_candidates
                && now.duration_since(cached.last_wpd_attempt) >= WPD_RETRY_INTERVAL;
            if cached.candidates == candidates && !retry_due {
                return Ok(cached.devices.clone());
            }
        }

        let needs_wpd = candidates
            .iter()
            .any(|candidate| candidate.device_instance_id.is_none());
        let wpd_devices = if needs_wpd {
            match enumerate_wpd_devices() {
                Ok(devices) => devices,
                Err(err) => {
                    // The legacy path remains available for items that already
                    // publish DeviceInstanceId. The others will be retried.
                    debug!(error = %err, "WPD portable device enumeration failed");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let outcome = reconcile_candidates(&candidates, &wpd_devices);
        let devices = outcome.devices;
        if let Ok(mut cache) = resolution_cache().lock() {
            *cache = Some(ResolutionCache {
                candidates,
                devices: devices.clone(),
                has_unresolved_candidates: outcome.has_unresolved_candidates,
                last_wpd_attempt: now,
            });
        }
        Ok(devices)
    }
}

unsafe fn enumerate_shell_candidates() -> Result<Vec<ShellCandidate>> {
    unsafe {
        let computer: IShellItem =
            SHGetKnownFolderItem(&FOLDERID_ComputerFolder, KF_FLAG_DEFAULT, None)?;
        let enumerator: IEnumShellItems = computer.BindToHandler(None, &BHID_EnumItems)?;
        let mut out = Vec::new();

        loop {
            let mut slot: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0u32;
            enumerator.Next(&mut slot, Some(&mut fetched))?;
            if fetched == 0 {
                break;
            }
            let Some(item) = slot[0].take() else {
                continue;
            };

            // Classic C:/D:/USB drives are already handled by
            // GetLogicalDrives. A success with an empty string isn't a valid
            // file path and must not mask certain MTP drivers.
            if let Ok(file_path_ptr) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                let file_path = take_shell_string(file_path_ptr);
                if !file_path.trim().is_empty() {
                    continue;
                }
            }

            let name = match item.GetDisplayName(SIGDN_NORMALDISPLAY) {
                Ok(value) => take_shell_string(value),
                Err(err) => {
                    debug!(error = %err, "ignored Shell item without display name");
                    continue;
                }
            };
            let shell_path = match item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) {
                Ok(value) => take_shell_string(value),
                Err(err) => {
                    debug!(item = %name, error = %err, "ignored Shell item without parsing name");
                    continue;
                }
            };
            if name.trim().is_empty() || shell_path.trim().is_empty() {
                continue;
            }

            // This property is useful when it exists, but it's
            // optional for a Shell item. WPD will arbitrate its absence.
            let device_instance_id = item
                .cast::<IShellItem2>()
                .ok()
                .and_then(|item2| item2.GetString(&PKEY_DEVICES_DEVICE_INSTANCE_ID).ok())
                .map(|value| take_shell_string(value))
                .filter(|id| !id.trim().is_empty());

            if is_apple_device(
                &name,
                device_instance_id.as_deref().unwrap_or_default(),
                &shell_path,
            ) {
                continue;
            }
            out.push(ShellCandidate {
                name,
                shell_path,
                device_instance_id,
            });
        }

        out.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.shell_path.cmp(&b.shell_path))
        });
        out.dedup_by(|a, b| a.shell_path.eq_ignore_ascii_case(&b.shell_path));
        Ok(out)
    }
}

/// Official enumeration of WPD devices. The manager is ephemeral: creating it
/// already produces a fresh list, so `RefreshDeviceList` (documented as
/// expensive) is intentionally not called.
unsafe fn enumerate_wpd_devices() -> Result<Vec<WpdDevice>> {
    unsafe {
        let manager: IPortableDeviceManager =
            CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)?;
        let mut count = 0u32;
        manager.GetDevices(std::ptr::null_mut(), &mut count)?;
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut raw_ids = TaskMemStrings(vec![PWSTR::null(); count as usize]);
        manager.GetDevices(raw_ids.0.as_mut_ptr(), &mut count)?;
        let returned = usize::min(count as usize, raw_ids.0.len());
        let mut out = Vec::with_capacity(returned);
        for index in 0..returned {
            let raw_id = std::mem::replace(&mut raw_ids.0[index], PWSTR::null());
            let device_instance_id = take_shell_string(raw_id);
            if device_instance_id.trim().is_empty() {
                continue;
            }
            let name = wpd_friendly_name(&manager, &device_instance_id);
            if is_apple_device(name.as_deref().unwrap_or_default(), &device_instance_id, "") {
                continue;
            }
            out.push(WpdDevice {
                name,
                device_instance_id,
            });
        }
        out.sort_by(|a, b| {
            a.device_instance_id
                .to_ascii_lowercase()
                .cmp(&b.device_instance_id.to_ascii_lowercase())
        });
        out.dedup_by(|a, b| {
            a.device_instance_id
                .eq_ignore_ascii_case(&b.device_instance_id)
        });
        Ok(out)
    }
}

unsafe fn wpd_friendly_name(
    manager: &IPortableDeviceManager,
    device_instance_id: &str,
) -> Option<String> {
    unsafe {
        let device_id: Vec<u16> = device_instance_id
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut len = 0u32;
        manager
            .GetDeviceFriendlyName(PCWSTR(device_id.as_ptr()), PWSTR::null(), &mut len)
            .ok()?;
        if len == 0 || len > MAX_WPD_STRING_LEN {
            return None;
        }

        let mut buffer = vec![0u16; len as usize];
        manager
            .GetDeviceFriendlyName(
                PCWSTR(device_id.as_ptr()),
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
            .ok()?;
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        let name = String::from_utf16_lossy(&buffer[..end]);
        (!name.trim().is_empty()).then_some(name)
    }
}

fn reconcile_candidates(
    candidates: &[ShellCandidate],
    wpd_devices: &[WpdDevice],
) -> ResolutionOutcome {
    let mut devices = Vec::new();
    let mut has_unresolved_candidates = false;

    for candidate in candidates {
        if let Some(device_instance_id) = candidate.device_instance_id.as_ref() {
            devices.push(PortableDevice {
                name: candidate.name.clone(),
                shell_path: candidate.shell_path.clone(),
                device_instance_id: device_instance_id.clone(),
            });
            continue;
        }

        let path_matches = unique_wpd_matches(wpd_devices.iter().filter(|device| {
            contains_ignore_ascii_case(&candidate.shell_path, &device.device_instance_id)
        }));
        let matched = path_matches.or_else(|| {
            let candidate_name = normalize_device_name(&candidate.name);
            unique_wpd_matches(wpd_devices.iter().filter(|device| {
                device
                    .name
                    .as_deref()
                    .map(normalize_device_name)
                    .is_some_and(|name| name == candidate_name)
            }))
        });

        if let Some(device) = matched {
            debug!(
                device = %candidate.name,
                "Shell portable device resolved through WPD fallback"
            );
            devices.push(PortableDevice {
                name: candidate.name.clone(),
                shell_path: candidate.shell_path.clone(),
                device_instance_id: device.device_instance_id.clone(),
            });
        } else {
            // Never guess: a non-WPD virtual folder or two devices with the
            // same name must not become a false clickable entry.
            has_unresolved_candidates = true;
        }
    }

    devices.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.device_instance_id.cmp(&b.device_instance_id))
    });
    devices.dedup_by(|a, b| {
        a.device_instance_id
            .eq_ignore_ascii_case(&b.device_instance_id)
    });
    ResolutionOutcome {
        devices,
        has_unresolved_candidates,
    }
}

fn unique_wpd_matches<'a>(matches: impl Iterator<Item = &'a WpdDevice>) -> Option<&'a WpdDevice> {
    let mut matches = matches;
    let first = matches.next()?;
    (!matches.any(|other| {
        !other
            .device_instance_id
            .eq_ignore_ascii_case(&first.device_instance_id)
    }))
    .then_some(first)
}

fn normalize_device_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn contains_ignore_ascii_case(value: &str, needle: &str) -> bool {
    !needle.is_empty()
        && value
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
}

/// Also frees any strings that `GetDevices` may have written before a
/// partial error return.
struct TaskMemStrings(Vec<PWSTR>);

impl Drop for TaskMemStrings {
    fn drop(&mut self) {
        for value in &self.0 {
            unsafe { drop_shell_string(*value) };
        }
    }
}

unsafe fn take_shell_string(value: PWSTR) -> String {
    unsafe {
        if value.is_null() {
            return String::new();
        }
        let text = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as *const core::ffi::c_void));
        text
    }
}

unsafe fn drop_shell_string(value: PWSTR) {
    unsafe {
        if !value.is_null() {
            CoTaskMemFree(Some(value.0 as *const core::ffi::c_void));
        }
    }
}

fn is_apple_device(name: &str, device_id: &str, shell_path: &str) -> bool {
    let value = format!("{name}\n{device_id}\n{shell_path}").to_ascii_lowercase();
    // 05AC is Apple's official USB vendor ID. The labels also cover
    // non-USB connections or drivers that might mask the VID.
    ["vid_05ac", "apple", "iphone", "ipad", "ipod"]
        .iter()
        .any(|needle| value.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::{ShellCandidate, WpdDevice, is_apple_device, reconcile_candidates};

    fn candidate(name: &str, device_instance_id: Option<&str>) -> ShellCandidate {
        ShellCandidate {
            name: name.to_owned(),
            shell_path: format!("::{{shell-item}}\\{name}"),
            device_instance_id: device_instance_id.map(str::to_owned),
        }
    }

    fn wpd(name: &str, device_instance_id: &str) -> WpdDevice {
        WpdDevice {
            name: Some(name.to_owned()),
            device_instance_id: device_instance_id.to_owned(),
        }
    }

    #[test]
    fn apple_portable_devices_are_explicitly_excluded() {
        assert!(is_apple_device(
            "Mon téléphone",
            r"USB\VID_05AC&PID_12A8",
            "::{shell-item}"
        ));
        assert!(is_apple_device("Apple iPhone", "WPD-1", "::{shell-item}"));
    }

    #[test]
    fn android_mtp_device_is_kept() {
        assert!(!is_apple_device(
            "Pixel 8",
            r"USB\VID_18D1&PID_4EE1",
            "::{shell-item}"
        ));
    }

    #[test]
    fn missing_shell_property_is_resolved_by_unique_wpd_name() {
        let result = reconcile_candidates(
            &[candidate("HONOR Magic5 Pro", None)],
            &[wpd("HONOR Magic5 Pro", r"USB\VID_0BB4&WPD")],
        );

        assert!(!result.has_unresolved_candidates);
        assert_eq!(result.devices.len(), 1);
        assert_eq!(result.devices[0].name, "HONOR Magic5 Pro");
        assert_eq!(result.devices[0].device_instance_id, r"USB\VID_0BB4&WPD");
    }

    #[test]
    fn duplicate_wpd_names_are_not_guessed() {
        let result = reconcile_candidates(
            &[candidate("Android", None)],
            &[wpd("Android", "WPD-1"), wpd("Android", "WPD-2")],
        );

        assert!(result.has_unresolved_candidates);
        assert!(result.devices.is_empty());
    }

    #[test]
    fn existing_shell_device_id_does_not_require_wpd() {
        let result = reconcile_candidates(&[candidate("Pixel 8", Some("WPD-PIXEL"))], &[]);

        assert!(!result.has_unresolved_candidates);
        assert_eq!(result.devices.len(), 1);
        assert_eq!(result.devices[0].device_instance_id, "WPD-PIXEL");
    }

    #[test]
    fn shell_path_device_id_disambiguates_duplicate_names() {
        let candidates = [ShellCandidate {
            name: "Android".to_owned(),
            shell_path: "::{shell-item}\\WPD-2".to_owned(),
            device_instance_id: None,
        }];
        let result = reconcile_candidates(
            &candidates,
            &[wpd("Android", "WPD-1"), wpd("Android", "WPD-2")],
        );

        assert!(!result.has_unresolved_candidates);
        assert_eq!(result.devices[0].device_instance_id, "WPD-2");
    }
}
