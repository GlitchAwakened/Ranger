//! "Open with" — enumeration of the OS's candidate applications.
//!
//! We build OUR OWN picker (Slint) instead of relying on the system
//! dialog: this lets us capture the user's choice, persist it as
//! [`ranger_core::openers::Opener`], and replay it later.
//!
//! - **Windows**: `SHAssocEnumHandlers` (what Explorer itself uses) →
//!   `IAssocHandler` (display name, key, recommended); launched via `Invoke`.
//! - **Linux**: parsing XDG `.desktop` files (no dependency), filtered by type.
//!
//! Common neutral `AppHandler` type → the GUI never sees the OS difference.

use std::path::Path;

use anyhow::Result;
use ranger_core::Lang;

use crate::i18n;

/// A candidate application, OS-neutral.
#[derive(Debug, Clone)]
pub struct AppHandler {
    /// Displayed name ("GIMP", "Notepad"…).
    pub name: String,
    /// Persistable key: exe path (classic apps) or identifier
    /// (Windows UWP / Linux `*.desktop`).
    pub key: String,
    /// Path of a directly launchable executable (if the key is one).
    /// `None` → relaunch via the OS path (Invoke / `.desktop`).
    pub exe: Option<String>,
    /// Recommended by the OS for this file type (shown first).
    pub recommended: bool,
}

// ===================== Windows =====================
#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IDataObject,
    };
    use windows::Win32::UI::Shell::{
        ASSOC_FILTER_NONE, ASSOC_FILTER_RECOMMENDED, BHID_DataObject, IAssocHandler, IShellItem,
        SHAssocEnumHandlers, SHCreateItemFromParsingName,
    };
    use windows::Win32::UI::WindowsAndMessaging::HICON;
    use windows::core::{PCWSTR, PWSTR};

    use crate::winutil::{hbitmap_to_rgba, wide};

    /// Retrieves + frees a `PWSTR` allocated by the shell (CoTaskMem).
    unsafe fn take_pwstr(p: PWSTR) -> String {
        unsafe {
            if p.is_null() {
                return String::new();
            }
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
            s
        }
    }

    /// Enumerates handlers for a `.ext` extension. `recommended_only` filters.
    unsafe fn enum_keys(ext_dot: &str, recommended_only: bool) -> Vec<(String, String)> {
        unsafe {
            let ext = wide(ext_dot);
            let filter = if recommended_only {
                ASSOC_FILTER_RECOMMENDED
            } else {
                ASSOC_FILTER_NONE
            };
            let Ok(en) = SHAssocEnumHandlers(PCWSTR(ext.as_ptr()), filter) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            loop {
                let mut slot: [Option<IAssocHandler>; 1] = [None];
                let mut fetched = 0u32;
                if en.Next(&mut slot, Some(&mut fetched)).is_err() || fetched == 0 {
                    break;
                }
                let Some(h) = slot[0].take() else { break };
                let name = h.GetName().map(|p| take_pwstr(p)).unwrap_or_default();
                let ui = h.GetUIName().map(|p| take_pwstr(p)).unwrap_or_default();
                if !name.is_empty() {
                    out.push((if ui.is_empty() { name.clone() } else { ui }, name));
                }
            }
            out
        }
    }

    pub fn handlers_for_ext(ext: &str) -> Vec<AppHandler> {
        if ext.is_empty() {
            return Vec::new();
        }
        let ext_dot = format!(".{ext}");
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            // Recommended set (keys) used to mark `recommended`.
            let recommended: std::collections::HashSet<String> = enum_keys(&ext_dot, true)
                .into_iter()
                .map(|(_, k)| k)
                .collect();
            enum_keys(&ext_dot, false)
                .into_iter()
                .map(|(name, key)| {
                    let exe = {
                        let p = Path::new(&key);
                        if p.is_file() { Some(key.clone()) } else { None }
                    };
                    AppHandler {
                        recommended: recommended.contains(&key),
                        name,
                        key,
                        exe,
                    }
                })
                .collect()
        }
    }

    /// Launches handler `key` on `path`. Classic apps (exe) → `Command`;
    /// otherwise re-enumerates the extension, finds the handler, and calls `Invoke`.
    pub fn launch(key: &str, ext: &str, path: &Path) -> Result<()> {
        // Fast path: key = a valid exe path.
        if Path::new(key).is_file() {
            return crate::actions::spawn_program(
                Path::new(key),
                &[path.to_string_lossy().into_owned()],
            );
        }
        let ext_dot = format!(".{ext}");
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let ext_w = wide(&ext_dot);
            let en = SHAssocEnumHandlers(PCWSTR(ext_w.as_ptr()), ASSOC_FILTER_NONE)
                .map_err(|e| anyhow::anyhow!("SHAssocEnumHandlers: {e}"))?;
            let path_w = wide(&path.to_string_lossy());
            loop {
                let mut slot: [Option<IAssocHandler>; 1] = [None];
                let mut fetched = 0u32;
                if en.Next(&mut slot, Some(&mut fetched)).is_err() || fetched == 0 {
                    break;
                }
                let Some(h) = slot[0].take() else { break };
                let name = h.GetName().map(|p| take_pwstr(p)).unwrap_or_default();
                if name == key {
                    let item: IShellItem =
                        SHCreateItemFromParsingName(PCWSTR(path_w.as_ptr()), None)
                            .map_err(|e| anyhow::anyhow!("SHCreateItemFromParsingName: {e}"))?;
                    let data: IDataObject = item
                        .BindToHandler(None, &BHID_DataObject)
                        .map_err(|e| anyhow::anyhow!("BindToHandler: {e}"))?;
                    h.Invoke(&data)
                        .map_err(|e| anyhow::anyhow!("Invoke: {e}"))?;
                    return Ok(());
                }
            }
        }
        Err(anyhow::anyhow!("handler not found: {key}"))
    }

    /// Extracts the icon associated with `path` as RGBA pixels `(buf, w, h)` (or `None`).
    /// `SHGetFileInfoW` (shell icon) → `GetIconInfo` → shared GDI extraction
    /// (`winutil::hbitmap_to_rgba`).
    pub fn icon_rgba(path: &str) -> Option<(Vec<u8>, u32, u32)> {
        use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
        use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGetFileInfoW};
        use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};
        if path.is_empty() || !Path::new(path).is_file() {
            return None;
        }
        unsafe {
            let wpath = wide(path);
            let mut shfi = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                PCWSTR(wpath.as_ptr()),
                Default::default(),
                Some(&mut shfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_SMALLICON,
            );
            if ok == 0 || shfi.hIcon.is_invalid() {
                return None;
            }
            let hicon = shfi.hIcon;
            let mut ii = ICONINFO::default();
            if GetIconInfo(hicon, &mut ii).is_err() {
                let _ = DestroyIcon(hicon);
                return None;
            }
            let out = hbitmap_to_rgba(ii.hbmColor);
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
            let _ = DestroyIcon(hicon);
            out
        }
    }

    /// Icon associated with the EXTENSION `ext` (no dot) as RGBA pixels, WITHOUT
    /// disk I/O. `SHGFI_USEFILEATTRIBUTES` resolves the registered icon from a fake
    /// `x.<ext>` name + `FILE_ATTRIBUTE_NORMAL` (registry lookup, no file
    /// access). `big=false` → 32 px (`SHGFI_LARGEICON`, list mode); `big=true`
    /// → **256 px** via the system's JUMBO image list (preview mode, where the row can
    /// grow up to ~216 px). Same GDI conversion (`hbitmap_to_rgba`).
    pub fn icon_rgba_for_ext(ext: &str, big: bool) -> Option<(Vec<u8>, u32, u32)> {
        if ext.is_empty() {
            return None;
        }
        // Fake name + `use_attrs` → pure REGISTRY lookup, without touching disk.
        shell_icon(&format!("x.{ext}"), true, big)
    }

    /// Icon SPECIFIC to a file: same mechanism, but on the REAL PATH
    /// and without `SHGFI_USEFILEATTRIBUTES` → the shell reads the file's resources.
    /// Essential for `.exe` files (embedded icon, different per binary):
    /// the "by extension" path can only return the generic registry icon.
    /// Costs a disk I/O → the caller caches it (cf. `self_icon` on the bridge side).
    pub fn icon_rgba_for_path(path: &str, big: bool) -> Option<(Vec<u8>, u32, u32)> {
        if path.is_empty() {
            return None;
        }
        shell_icon(path, false, big)
    }

    /// SHARED core of both paths: `SHGetFileInfoW` on `target`.
    /// `use_attrs` = resolve from the name alone (registry, no I/O) vs reading the
    /// file. `big` = 256 px via the JUMBO image list, otherwise 32 px directly.
    fn shell_icon(target: &str, use_attrs: bool, big: bool) -> Option<(Vec<u8>, u32, u32)> {
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
        use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
        use windows::Win32::UI::Shell::{
            SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
            SHGetFileInfoW, SHGetImageList, SHIL_JUMBO,
        };
        use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
        unsafe {
            let wpath = wide(target);
            let attrs = if use_attrs {
                FILE_ATTRIBUTE_NORMAL
            } else {
                Default::default()
            };
            let mut flags = if big {
                SHGFI_SYSICONINDEX
            } else {
                SHGFI_ICON | SHGFI_LARGEICON
            };
            if use_attrs {
                flags |= SHGFI_USEFILEATTRIBUTES;
            }
            let mut shfi = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                PCWSTR(wpath.as_ptr()),
                attrs,
                Some(&mut shfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                flags,
            );
            if ok == 0 {
                return None;
            }
            if big {
                // 256 px: system index → shared JUMBO image list.
                let list: IImageList = SHGetImageList(SHIL_JUMBO as i32).ok()?;
                let hicon = list.GetIcon(shfi.iIcon, ILD_TRANSPARENT.0).ok()?;
                let out = hicon_to_rgba(hicon);
                let _ = DestroyIcon(hicon);
                out
            } else {
                if shfi.hIcon.is_invalid() {
                    return None;
                }
                let out = hicon_to_rgba(shfi.hIcon);
                let _ = DestroyIcon(shfi.hIcon);
                out
            }
        }
    }

    /// `HICON` → RGBA pixels `(buf, w, h)`: `GetIconInfo` (color bitmap) → GDI.
    unsafe fn hicon_to_rgba(hicon: HICON) -> Option<(Vec<u8>, u32, u32)> {
        unsafe {
            use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
            use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};
            if hicon.is_invalid() {
                return None;
            }
            let mut ii = ICONINFO::default();
            if GetIconInfo(hicon, &mut ii).is_err() {
                return None;
            }
            let out = hbitmap_to_rgba(ii.hbmColor);
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
            out
        }
    }

    /// Opens a native file picker (IFileOpenDialog) filtered on
    /// executables. Returns the chosen path, or `None` (cancelled/error).
    pub fn browse_for_exe(lang: Lang) -> Option<String> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
        use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let dlg: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let f_name = wide(&i18n::tr(lang, "ow_dialog_programs"));
            let f_spec = wide("*.exe;*.com;*.bat;*.cmd");
            let a_name = wide(&i18n::tr(lang, "ow_dialog_all_files"));
            let a_spec = wide("*.*");
            let filters = [
                COMDLG_FILTERSPEC {
                    pszName: PCWSTR(f_name.as_ptr()),
                    pszSpec: PCWSTR(f_spec.as_ptr()),
                },
                COMDLG_FILTERSPEC {
                    pszName: PCWSTR(a_name.as_ptr()),
                    pszSpec: PCWSTR(a_spec.as_ptr()),
                },
            ];
            let _ = dlg.SetFileTypes(&filters);
            if dlg.Show(None).is_err() {
                return None; // cancelled
            }
            let item = dlg.GetResult().ok()?;
            let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            Some(take_pwstr(p))
        }
    }

    /// Resolves the TARGET of a Windows `.lnk` shortcut via `IShellLinkW` +
    /// `IPersistFile::Load` (COM). Returns the raw target path (without
    /// `Resolve`, which could search/display a UI). `None` if it's not
    /// a valid link or the target is empty.
    pub fn resolve_shortcut(path: &Path) -> Option<std::path::PathBuf> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
        use windows::core::Interface;
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
            let pf: IPersistFile = link.cast().ok()?;
            let wpath = wide(&path.to_string_lossy());
            pf.Load(PCWSTR(wpath.as_ptr()), STGM_READ).ok()?;
            let mut buf = [0u16; 260]; // MAX_PATH
            let mut fd = WIN32_FIND_DATAW::default();
            link.GetPath(&mut buf, &mut fd, SLGP_RAWPATH.0 as u32)
                .ok()?;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            if len == 0 {
                return None;
            }
            Some(std::path::PathBuf::from(OsString::from_wide(&buf[..len])))
        }
    }

    /// Creates a Windows `lnk_path` shortcut (`.lnk`) pointing to `target`, via
    /// `IShellLinkW::SetPath` + `IPersistFile::Save` (COM). The shortcut's working
    /// directory is the target's own directory.
    pub fn create_shortcut(lnk_path: &Path, target: &Path) -> Result<()> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        use windows::core::Interface;
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| anyhow::anyhow!("CoCreateInstance ShellLink: {e}"))?;
            let target_w = wide(&target.to_string_lossy());
            link.SetPath(PCWSTR(target_w.as_ptr()))
                .map_err(|e| anyhow::anyhow!("SetPath: {e}"))?;
            if let Some(dir) = target.parent() {
                let dir_w = wide(&dir.to_string_lossy());
                let _ = link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()));
            }
            let pf: IPersistFile = link
                .cast()
                .map_err(|e| anyhow::anyhow!("cast IPersistFile: {e}"))?;
            let lnk_w = wide(&lnk_path.to_string_lossy());
            pf.Save(PCWSTR(lnk_w.as_ptr()), true)
                .map_err(|e| anyhow::anyhow!("Save .lnk: {e}"))?;
            Ok(())
        }
    }

    /// Native picker to choose a shortcut's TARGET — `IFileOpenDialog`
    /// filtered to "All files" (unlike `browse_for_exe`, which is restricted
    /// to executables). Returns the chosen path, or `None`.
    pub fn browse_for_target(lang: Lang) -> Option<String> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
        use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let dlg: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let a_name = wide(&i18n::tr(lang, "ow_dialog_all_files"));
            let a_spec = wide("*.*");
            let filters = [COMDLG_FILTERSPEC {
                pszName: PCWSTR(a_name.as_ptr()),
                pszSpec: PCWSTR(a_spec.as_ptr()),
            }];
            let _ = dlg.SetFileTypes(&filters);
            if dlg.Show(None).is_err() {
                return None; // cancelled
            }
            let item = dlg.GetResult().ok()?;
            let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            Some(take_pwstr(p))
        }
    }

    /// Native FOLDER picker (`IFileOpenDialog` + `FOS_PICKFOLDERS`), for the
    /// target of a shortcut to a folder. Returns the chosen path, or `None`.
    pub fn browse_for_folder() -> Option<String> {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        use windows::Win32::UI::Shell::{
            FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let dlg: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let opts = dlg.GetOptions().unwrap_or_default();
            let _ = dlg.SetOptions(opts | FOS_PICKFOLDERS);
            if dlg.Show(None).is_err() {
                return None; // cancelled
            }
            let item = dlg.GetResult().ok()?;
            let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            Some(take_pwstr(p))
        }
    }
}

// ===================== Linux =====================
#[cfg(not(windows))]
mod imp {
    use super::*;

    /// XDG application directories (priority order).
    fn app_dirs() -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(h) = std::env::var("XDG_DATA_HOME") {
            if !h.is_empty() {
                dirs.push(std::path::PathBuf::from(h).join("applications"));
            }
        } else if let Ok(home) = std::env::var("HOME") {
            dirs.push(std::path::PathBuf::from(home).join(".local/share/applications"));
        }
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
        for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
            dirs.push(std::path::PathBuf::from(d).join("applications"));
        }
        dirs
    }

    /// The `[Desktop Entry]` fields Ranger acts on. Only those: the format has
    /// dozens more, and reading one we never consult would just be noise.
    #[derive(Default)]
    pub struct DesktopEntry {
        pub name: String,
        pub exec: String,
        pub mimes: String,
        pub nodisplay: bool,
        /// Theme icon name OR absolute path to an image file — both are legal,
        /// and the two are told apart when resolving, not here.
        pub icon: String,
        /// Working directory to start the application in (`Path=`).
        pub work_dir: String,
        /// The application wants a terminal to run in.
        pub terminal: bool,
        /// `Application`, `Link` or `Directory`. Empty when absent.
        pub kind: String,
        /// Destination of a `Type=Link` entry.
        pub url: String,
    }

    /// Minimal parse of a `.desktop` file, limited to its `[Desktop Entry]`
    /// group — a trailing `[Desktop Action …]` group carries its own `Exec`,
    /// which must not be mistaken for the main one.
    pub fn parse_desktop(content: &str) -> DesktopEntry {
        let mut entry = DesktopEntry::default();
        let mut in_entry = false;
        for line in content.lines() {
            let line = line.trim();
            // A comment may hold anything, including a line that looks like a
            // key (these files often start with a `#!` shebang).
            if line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry {
                continue;
            }
            if let Some(v) = line.strip_prefix("Name=") {
                if entry.name.is_empty() {
                    entry.name = v.to_string();
                }
            } else if let Some(v) = line.strip_prefix("Exec=") {
                entry.exec = v.to_string();
            } else if let Some(v) = line.strip_prefix("MimeType=") {
                entry.mimes = v.to_string();
            } else if let Some(v) = line.strip_prefix("NoDisplay=") {
                entry.nodisplay = v.eq_ignore_ascii_case("true");
            } else if let Some(v) = line.strip_prefix("Icon=") {
                entry.icon = v.to_string();
            } else if let Some(v) = line.strip_prefix("Path=") {
                entry.work_dir = v.to_string();
            } else if let Some(v) = line.strip_prefix("Terminal=") {
                entry.terminal = v.eq_ignore_ascii_case("true");
            } else if let Some(v) = line.strip_prefix("Type=") {
                entry.kind = v.to_string();
            } else if let Some(v) = line.strip_prefix("URL=") {
                entry.url = v.to_string();
            }
        }
        entry
    }

    /// Splits an `Exec=` value into an `argv`, following the Desktop Entry
    /// specification, and resolves its field codes.
    ///
    /// Not [`crate::bridge::split_args`]: that one also treats `'` as a quote,
    /// which is right for a command the user typed but wrong here — the
    /// specification quotes with `"` only, so an apostrophe in a path (a folder
    /// named "Bob's Apps") is an ordinary character that must survive intact.
    /// Inside quotes, `\` escapes the next character.
    ///
    /// `file` is the document to hand over, if any: `%f`/`%F`/`%u`/`%U` take it,
    /// and are simply dropped when there is nothing to pass — launching an
    /// application on its own must not leave a stray `%u` in its arguments.
    /// The remaining codes carry nothing Ranger can supply and are dropped too.
    pub fn exec_argv(exec: &str, file: Option<&str>) -> Vec<String> {
        let mut argv: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut quoted = false;
        let mut chars = exec.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if quoted => {
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                }
                '"' => {
                    quoted = !quoted;
                    started = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if started {
                        argv.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                '%' if !quoted => {
                    // A code is always two characters; `%%` is a literal `%`.
                    match chars.next() {
                        Some('%') => {
                            current.push('%');
                            started = true;
                        }
                        Some('f' | 'F' | 'u' | 'U') => {
                            if let Some(file) = file {
                                current.push_str(file);
                                started = true;
                            }
                        }
                        // %i %c %k and the deprecated ones: nothing to give.
                        Some(_) | None => {}
                    }
                }
                c => {
                    current.push(c);
                    started = true;
                }
            }
        }
        if started {
            argv.push(current);
        }
        // A code alone in its token leaves an empty argument behind, which the
        // program would receive as a genuine empty parameter.
        argv.retain(|a| !a.is_empty());
        argv
    }

    /// Rough MIME type of an extension (common cases; otherwise `None` → we
    /// exclude nothing). Good enough to filter a file browser.
    fn mime_for_ext(ext: &str) -> Option<&'static str> {
        Some(match ext {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "bmp" => "image/bmp",
            "pdf" => "application/pdf",
            "txt" | "log" | "md" => "text/plain",
            "html" | "htm" => "text/html",
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "ogg" => "audio/ogg",
            "wav" => "audio/x-wav",
            "mp4" => "video/mp4",
            "mkv" => "video/x-matroska",
            "webm" => "video/webm",
            "zip" => "application/zip",
            _ => return None,
        })
    }

    pub fn handlers_for_ext(ext: &str) -> Vec<AppHandler> {
        let mime = mime_for_ext(ext);
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for dir in app_dirs() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                let Some(id) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !seen.insert(id.to_string()) {
                    continue; // priority to the first folder (user override)
                }
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let entry = parse_desktop(&content);
                if entry.nodisplay || entry.name.is_empty() || entry.exec.is_empty() {
                    continue;
                }
                // Filter by MIME type if known; otherwise keep it (fallback).
                if let Some(m) = mime
                    && !entry.mimes.split(';').any(|x| x == m)
                {
                    continue;
                }
                out.push(AppHandler {
                    name: entry.name,
                    key: id.to_string(),
                    exe: None,
                    recommended: false,
                });
            }
        }
        out.sort_by_key(|a| a.name.to_lowercase());
        out
    }

    /// Launches the `.desktop` app `key` on `path` (substitutes XDG field codes).
    pub fn launch(key: &str, _ext: &str, path: &Path) -> Result<()> {
        for dir in app_dirs() {
            let candidate = dir.join(key);
            let Ok(content) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            let entry = parse_desktop(&content);
            if entry.exec.is_empty() {
                continue;
            }
            let file = path.to_string_lossy().into_owned();
            let mut argv = exec_argv(&entry.exec, Some(&file));
            if argv.is_empty() {
                return Err(anyhow::anyhow!("empty Exec for {key}"));
            }
            let prog = argv.remove(0);
            // An entry declaring no field code still has to receive the file.
            if !argv.iter().any(|a| a == &file) {
                argv.push(file);
            }
            return crate::actions::spawn_program(Path::new(&prog), &argv);
        }
        Err(anyhow::anyhow!("application not found: {key}"))
    }

    /// Acts on the launcher at `path`: starts the application it describes, or
    /// opens the address for a `Type=Link` entry.
    ///
    /// Unlike [`launch`], the entry is designated by its own path rather than
    /// by a name looked up in the XDG folders, so a launcher sitting anywhere —
    /// a download, a folder of games — works the same.
    pub fn launch_desktop_file(path: &Path) -> Result<()> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let entry = parse_desktop(&content);

        // A shortcut to an address: nothing to execute, the destination is
        // handed to the desktop like any other address.
        if entry.kind == "Link" {
            if entry.url.is_empty() {
                return Err(anyhow::anyhow!("no URL in {}", path.display()));
            }
            return open::that_detached(&entry.url)
                .map_err(|e| anyhow::anyhow!("opening {}: {e}", entry.url));
        }

        // No file to hand over: the launcher is being started on its own.
        let mut argv = exec_argv(&entry.exec, None);
        if argv.is_empty() {
            return Err(anyhow::anyhow!("no Exec in {}", path.display()));
        }
        let program = argv.remove(0);

        // `Path=` may be present but empty, which means "no preference".
        let work_dir = Some(entry.work_dir.as_str())
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from);

        if entry.terminal {
            // Best effort: `-e <command>` is the option the usual terminals
            // share. An entry asking for a terminal is rare for a launcher, and
            // failing to start beats starting the program with no console at
            // all, silently.
            let term = crate::actions::pick_terminal()
                .ok_or_else(|| anyhow::anyhow!("no terminal detected for {}", path.display()))?;
            let mut term_argv = vec!["-e".to_string(), program];
            term_argv.extend(argv);
            return crate::actions::spawn_program_in(
                Path::new(&term),
                &term_argv,
                work_dir.as_deref(),
            );
        }
        crate::actions::spawn_program_in(Path::new(&program), &argv, work_dir.as_deref())
    }

    /// Folders holding icon themes, in priority order: the user's own first, so
    /// a locally installed application overrides a system one of the same name.
    fn icon_base_dirs() -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        let home = std::env::var("HOME").ok();
        match std::env::var("XDG_DATA_HOME") {
            Ok(h) if !h.is_empty() => dirs.push(std::path::PathBuf::from(h).join("icons")),
            _ => {
                if let Some(home) = &home {
                    dirs.push(std::path::PathBuf::from(home).join(".local/share/icons"));
                }
            }
        }
        // Long-standing location, still used by applications that install by
        // hand rather than through a package.
        if let Some(home) = &home {
            dirs.push(std::path::PathBuf::from(home).join(".icons"));
        }
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
        for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
            dirs.push(std::path::PathBuf::from(d).join("icons"));
        }
        dirs
    }

    /// Icon theme the desktop is currently using, if it can be read cheaply.
    /// Only the two mainstream settings files are consulted; anything else
    /// falls back to `hicolor`, which every theme is required to inherit and
    /// where applications install their own icon.
    fn current_icon_theme() -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        let config = match std::env::var("XDG_CONFIG_HOME") {
            Ok(c) if !c.is_empty() => std::path::PathBuf::from(c),
            _ => std::path::PathBuf::from(&home).join(".config"),
        };
        // KDE: `[Icons] Theme=`. GTK: `gtk-icon-theme-name=`.
        for (file, key) in [
            ("kdeglobals", "Theme="),
            ("gtk-3.0/settings.ini", "gtk-icon-theme-name="),
            ("gtk-4.0/settings.ini", "gtk-icon-theme-name="),
        ] {
            let Ok(text) = std::fs::read_to_string(config.join(file)) else {
                continue;
            };
            if let Some(value) = text
                .lines()
                .map(str::trim)
                .find_map(|line| line.strip_prefix(key))
                && !value.is_empty()
            {
                return Some(value.to_string());
            }
        }
        None
    }

    /// Extensions an icon may use, best first: a vector image stays sharp at
    /// any row height, which a fixed-size bitmap does not.
    const ICON_EXTENSIONS: &[&str] = &["svg", "png", "xpm"];

    /// Icon sizes to try, largest first — the row scales it down, and scaling
    /// down keeps far more detail than scaling a 16px icon up.
    const ICON_SIZES: &[u32] = &[512, 256, 128, 96, 64, 48, 40, 36, 32, 24, 22, 16];

    /// Finds the file an `Icon=` value refers to.
    ///
    /// The value is allowed to be either an absolute path to an image or a
    /// theme icon name, and both occur in the wild — an application installed
    /// by hand points straight at its own file, while a packaged one names an
    /// icon it installed into the theme. The two are told apart here.
    ///
    /// The theme search follows the usual layout rather than reading every
    /// `index.theme`: probing a bounded list of candidate paths costs a handful
    /// of `stat` calls, where parsing the theme descriptions of a full icon set
    /// would cost far more on every listing. The result is cached by the caller.
    pub fn resolve_icon(icon: &str) -> Option<std::path::PathBuf> {
        if icon.is_empty() {
            return None;
        }
        // An absolute path is used as it stands.
        let as_path = std::path::Path::new(icon);
        if as_path.is_absolute() {
            return as_path.is_file().then(|| as_path.to_path_buf());
        }
        // A name carrying an extension is still a name: the specification says
        // to drop it before searching the theme.
        let stem = ICON_EXTENSIONS
            .iter()
            .find_map(|ext| icon.strip_suffix(&format!(".{ext}")))
            .unwrap_or(icon);

        let mut themes: Vec<String> = Vec::new();
        if let Some(theme) = current_icon_theme() {
            themes.push(theme);
        }
        // Every theme inherits from it, and an application's own icon lands
        // there whatever the desktop in use.
        themes.push("hicolor".to_string());

        find_themed_icon(&icon_base_dirs(), &themes, stem)
            .or_else(|| find_legacy_icon(&pixmap_dirs(), stem))
    }

    /// Folders of the flat, pre-theme layout, kept for what still installs
    /// there.
    fn pixmap_dirs() -> Vec<std::path::PathBuf> {
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|d| std::path::PathBuf::from(d).join("pixmaps"))
            .collect()
    }

    /// Searches the themed layout. Kept apart from the environment so the
    /// ordering rules can be tested against a folder tree built for the test.
    pub(super) fn find_themed_icon(
        bases: &[std::path::PathBuf],
        themes: &[String],
        stem: &str,
    ) -> Option<std::path::PathBuf> {
        const CONTEXTS: &[&str] = &["apps", "devices", "places", "mimetypes"];
        for base in bases {
            for theme in themes {
                let theme_dir = base.join(theme);
                // Vector first, whatever the size folders hold: one file stays
                // sharp at every row height.
                for context in CONTEXTS {
                    let svg = theme_dir
                        .join("scalable")
                        .join(context)
                        .join(format!("{stem}.svg"));
                    if svg.is_file() {
                        return Some(svg);
                    }
                }
                for size in ICON_SIZES {
                    for context in CONTEXTS {
                        for ext in ICON_EXTENSIONS {
                            // Both orderings exist across themes.
                            for candidate in [
                                theme_dir
                                    .join(format!("{size}x{size}"))
                                    .join(context)
                                    .join(format!("{stem}.{ext}")),
                                theme_dir
                                    .join(context)
                                    .join(format!("{size}"))
                                    .join(format!("{stem}.{ext}")),
                            ] {
                                if candidate.is_file() {
                                    return Some(candidate);
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Searches the flat layout, where the file sits directly in the folder.
    pub(super) fn find_legacy_icon(
        dirs: &[std::path::PathBuf],
        stem: &str,
    ) -> Option<std::path::PathBuf> {
        for dir in dirs {
            for ext in ICON_EXTENSIONS {
                let candidate = dir.join(format!("{stem}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Image a `.desktop` launcher wants to be shown with, as a path so the
    /// caller can hand it straight to the renderer — which reads SVG as well as
    /// bitmaps, where returning decoded pixels would lose the vector.
    pub fn desktop_icon_path(path: &Path) -> Option<std::path::PathBuf> {
        let content = std::fs::read_to_string(path).ok()?;
        resolve_icon(&parse_desktop(&content).icon)
    }

    /// Icon extraction — not implemented on Linux (v1; XDG icon theme
    /// resolution deferred).
    pub fn icon_rgba(_path: &str) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    /// Icon by extension — not implemented on Linux (v1; would require
    /// MIME resolution + XDG icon theme, cf. DEVBOOK). Falls back to the
    /// generic type icon on the view side.
    pub fn icon_rgba_for_ext(_ext: &str, _big: bool) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    /// File-specific icon — not implemented on Linux.
    pub fn icon_rgba_for_path(_path: &str, _big: bool) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    /// Native file picker on Linux "in its own way": we delegate to the
    /// desktop portal via `zenity` (GTK) then `kdialog` (KDE) — present on the
    /// vast majority of environments, **without adding a dependency**. The
    /// chosen path is written to stdout; cancellation ⇒ non-zero exit code.
    pub fn browse_for_exe(lang: Lang) -> Option<String> {
        use std::process::Command;

        let title = i18n::tr(lang, "ow_dialog_choose_program");

        // zenity: GTK. `--file-selection` returns the absolute path on stdout.
        // If zenity responded (choice OR cancellation), we do NOT fall back to kdialog
        // (the user has already interacted) → explicit `return None` on cancellation.
        if let Ok(out) = Command::new("zenity")
            .args(["--file-selection", &format!("--title={title}")])
            .output()
        {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() {
                    return Some(p);
                }
            }
            return None;
        }

        // zenity absent → kdialog (KDE). `--getopenfilename <dir>` → path on stdout.
        if let Ok(out) = Command::new("kdialog")
            .args(["--getopenfilename", ".", "--title", &title])
            .output()
            && out.status.success()
        {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(p);
            }
        }
        None
    }

    /// No `.lnk` shortcuts outside Windows: symbolic links are followed
    /// natively by the listing (a folder symlink is navigated directly).
    /// (Not called outside Windows — the API stays symmetric.)
    #[allow(dead_code)]
    pub fn resolve_shortcut(_path: &Path) -> Option<std::path::PathBuf> {
        None
    }

    /// Creating a `.lnk` shortcut — not applicable outside Windows (the "New
    /// shortcut" entry isn't shown there).
    pub fn create_shortcut(_lnk_path: &Path, _target: &Path) -> Result<()> {
        Err(anyhow::anyhow!(".lnk shortcuts: Windows only"))
    }

    /// Shortcut target picker — not applicable outside Windows.
    pub fn browse_for_target(_lang: Lang) -> Option<String> {
        None
    }

    /// Target folder picker — not applicable outside Windows.
    pub fn browse_for_folder() -> Option<String> {
        None
    }
}

/// Icon associated with `path` (RGBA pixels), or `None`.
pub fn icon_rgba(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    imp::icon_rgba(path)
}

/// Starts the application a `.desktop` launcher describes. Linux desktops only:
/// Windows has no such file, and its shell already runs `.lnk` shortcuts.
#[cfg(not(windows))]
pub fn launch_desktop_file(path: &Path) -> Result<()> {
    imp::launch_desktop_file(path)
}

/// Path of the image a `.desktop` launcher declares, resolved through the icon
/// theme when it names one instead of pointing at a file.
#[cfg(not(windows))]
pub fn desktop_icon_path(path: &Path) -> Option<std::path::PathBuf> {
    imp::desktop_icon_path(path)
}

/// Resolves the target of a Windows `.lnk` shortcut (pointed-to path). `None`
/// outside Windows or if it's not a valid link. (Only called under
/// `#[cfg(windows)]`; the API stays symmetric across platforms.)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn resolve_shortcut(path: &Path) -> Option<std::path::PathBuf> {
    imp::resolve_shortcut(path)
}

/// Creates a Windows `lnk_path` shortcut pointing to `target`. Errors outside
/// Windows (the caller only invokes this on Windows).
pub fn create_shortcut(lnk_path: &Path, target: &Path) -> Result<()> {
    imp::create_shortcut(lnk_path, target)
}

/// Native picker for a shortcut's target — FILE (`None` if cancelled/outside
/// Windows).
pub fn browse_for_target(lang: Lang) -> Option<String> {
    imp::browse_for_target(lang)
}

/// Native picker for a target FOLDER shortcut (`None` if cancelled/outside
/// Windows).
pub fn browse_for_folder() -> Option<String> {
    imp::browse_for_folder()
}

/// Icon of the default application ASSOCIATED WITH EXTENSION `ext` (no dot),
/// as RGBA pixels `(buf, w, h)` — WITHOUT touching disk. Used to show the OS's
/// real "file type" icon in the view (faster recognition);
/// result should be cached per extension. `big=false` → ~32 px (list mode);
/// `big=true` → ~256 px (preview mode, downscaled based on zoom). `None` if unavailable.
pub fn icon_rgba_for_ext(ext: &str, big: bool) -> Option<(Vec<u8>, u32, u32)> {
    imp::icon_rgba_for_ext(ext, big)
}

/// Icon SPECIFIC to file `path` (embedded resources) — for types where
/// each file carries its own icon (`.exe`…). `None` outside Windows.
pub fn icon_rgba_for_path(path: &str, big: bool) -> Option<(Vec<u8>, u32, u32)> {
    imp::icon_rgba_for_path(path, big)
}

/// Native file picker to choose an executable (`None` if cancelled/unavailable).
pub fn browse_for_exe(lang: Lang) -> Option<String> {
    imp::browse_for_exe(lang)
}

/// OS candidate applications for extension `ext` (no dot).
pub fn handlers_for_ext(ext: &str) -> Vec<AppHandler> {
    imp::handlers_for_ext(ext)
}

/// Launches handler `key` on `path` (`ext` = the file's extension).
pub fn launch(key: &str, ext: &str, path: &Path) -> Result<()> {
    imp::launch(key, ext, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(name: &str) -> AppHandler {
        AppHandler {
            name: name.to_string(),
            key: name.to_string(),
            exe: None,
            recommended: false,
        }
    }

    #[test]
    fn app_list_sorts_case_insensitively() {
        // Guards the `sort_by` → `sort_by_key` rewrite in `handlers_for_ext`
        // (Clippy `unnecessary_sort_by`, Rust 1.97): same one-liner, run here
        // against handlers built by hand rather than real `.desktop` files, so
        // it exercises the platform-neutral sort regardless of the OS running
        // the test — `handlers_for_ext` itself is Linux-only.
        let mut apps = [handler("vlc"), handler("GIMP"), handler("Blender")];
        apps.sort_by_key(|a| a.name.to_lowercase());
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Blender", "GIMP", "vlc"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn exec_is_split_by_the_desktop_entry_rules_not_by_shell_habits() {
        use super::imp::exec_argv;

        // Ordinary case, and the field code drops out when the launcher is
        // started on its own — a stray "%U" would reach the program as a
        // filename to open.
        assert_eq!(
            exec_argv("steam steam://rungameid/548430 %U", None),
            ["steam", "steam://rungameid/548430"]
        );
        assert_eq!(
            exec_argv("gimp %U", Some("/tmp/a.png")),
            ["gimp", "/tmp/a.png"]
        );

        // The specification quotes with `"` only. An apostrophe is an ordinary
        // character: treating it as a quote — as a shell-style splitter would —
        // swallowed the rest of the command line.
        assert_eq!(
            exec_argv("/opt/Bob's Apps/run --now", None),
            ["/opt/Bob's", "Apps/run", "--now"]
        );
        assert_eq!(
            exec_argv("\"/opt/Bob's Apps/run\" --now", None),
            ["/opt/Bob's Apps/run", "--now"]
        );

        // Quoted space stays inside one argument; inside quotes `\` escapes.
        assert_eq!(
            exec_argv("\"/usr/local/My App/bin\" %f", None),
            ["/usr/local/My App/bin"]
        );
        assert_eq!(exec_argv("\"a\\\"b\" tail", None), ["a\"b", "tail"]);

        // `%%` is a literal percent, not the start of a code.
        assert_eq!(
            exec_argv("tool --fmt 100%% -v", None),
            ["tool", "--fmt", "100%", "-v"]
        );

        // Codes Ranger has nothing to supply for are dropped, and a code alone
        // in its token leaves no empty argument behind.
        assert_eq!(exec_argv("app %i %c %k --go", None), ["app", "--go"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_launcher_icon_is_found_by_name_across_the_theme_layouts() {
        use super::imp::{find_legacy_icon, find_themed_icon, resolve_icon};

        let root = std::env::temp_dir().join(format!(
            "ranger-icons-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let touch = |rel: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
            path
        };

        // Both size-folder orderings found in the wild.
        let sized = touch("hicolor/48x48/apps/steam_icon_548430.png");
        let inverted = touch("hicolor/apps/64/othergame.png");
        // A vector wins over a bitmap of the same name: it stays sharp at any
        // row height, which is the whole reason for preferring it.
        touch("hicolor/32x32/apps/blender.png");
        let vector = touch("hicolor/scalable/apps/blender.svg");

        let bases = vec![root.clone()];
        let themes = vec!["hicolor".to_string()];
        assert_eq!(
            find_themed_icon(&bases, &themes, "steam_icon_548430"),
            Some(sized)
        );
        assert_eq!(
            find_themed_icon(&bases, &themes, "othergame"),
            Some(inverted)
        );
        assert_eq!(find_themed_icon(&bases, &themes, "blender"), Some(vector));
        assert_eq!(find_themed_icon(&bases, &themes, "absent"), None);

        // The user's theme is searched before the fallback every theme
        // inherits, so a local override wins.
        let mine = touch("Breeze/48x48/apps/steam_icon_548430.png");
        assert_eq!(
            find_themed_icon(
                &bases,
                &["Breeze".to_string(), "hicolor".to_string()],
                "steam_icon_548430"
            ),
            Some(mine)
        );

        // Flat layout, with no theme or size below it.
        let flat = touch("pixmaps/legacyapp.xpm");
        assert_eq!(
            find_legacy_icon(&[root.join("pixmaps")], "legacyapp"),
            Some(flat)
        );

        // An absolute path is taken as it stands — the common case for an
        // application installed by hand, which points at its own file.
        let direct = touch("opt/thing/logo.svg");
        assert_eq!(
            resolve_icon(&direct.display().to_string()),
            Some(direct.clone())
        );
        // ...but only when it really is there, otherwise the row would ask the
        // renderer for a missing file on every listing.
        assert_eq!(
            resolve_icon(&root.join("opt/thing/gone.svg").display().to_string()),
            None
        );
        assert_eq!(resolve_icon(""), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(not(windows))]
    #[test]
    fn only_the_main_group_of_a_launcher_is_read() {
        use super::imp::parse_desktop;

        // A shebang line and a trailing action group both carry something that
        // looks like a key; neither may override the entry itself.
        let entry = parse_desktop(
            "#!/usr/bin/env xdg-open\n\
             [Desktop Entry]\n\
             Name=Blender 5.2\n\
             Exec=/opt/blender/blender\n\
             Icon=/opt/blender/blender.svg\n\
             Path=\n\
             Terminal=false\n\
             Type=Application\n\
             [Desktop Action Render]\n\
             Name=Render\n\
             Exec=/opt/blender/blender --render\n",
        );
        assert_eq!(entry.name, "Blender 5.2");
        assert_eq!(entry.exec, "/opt/blender/blender");
        assert_eq!(entry.icon, "/opt/blender/blender.svg");
        assert_eq!(entry.kind, "Application");
        assert!(!entry.terminal);
        // `Path=` present but empty means "no preference", not the root.
        assert!(entry.work_dir.is_empty());
    }
}
