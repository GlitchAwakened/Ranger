//! Windows SHELL context menu — `IContextMenu` hosting.
//!
//! Enumerates the entries that installed programs (PeaZip, Git, TortoiseGit,
//! Beyond Compare, Visual Studio…) add to Explorer's context menu, re-renders
//! them in Ranger's Slint menu, then invokes the chosen entry.
//!
//! Principle (the same native Shell flow as Explorer):
//! 1. `IShellItemArray` of the targets → `BindToHandler(BHID_SFUIObject)` →
//!    the COMPOSITE `IContextMenu` object (static registry verbs + all
//!    registered COM extensions);
//! 2. `QueryContextMenu` fills in a native `HMENU` (never displayed);
//! 3. we WALK the `HMENU` (`GetMenuItemInfoW`) to extract labels,
//!    ids, and submenus (filled on the fly via `IContextMenu2::HandleMenuMsg`
//!    + `WM_INITMENUPOPUP` — the PeaZip/TortoiseGit case);
//! 4. on click: `InvokeCommand` with the id offset.
//!
//! "Dead zone" TRICK: we query the menu of the CURRENT FOLDER AS AN ITEM
//! (not the folder's "background" menu) — that's what exposes PeaZip's FULL
//! dropdown menu ("Add to archive"…), BC's "Select left folder for compare",
//! etc., whereas the background menu only offers a "Browse path with PeaZip".
//!
//! FILTERING: canonical verbs already covered by Ranger (open, copy,
//! delete, properties, sendto…) are discarded (`VERB_BLOCKLIST`) — we only
//! keep the "special" entries from third-party programs. EXTENDED verbs
//! (Shift+right-click: cmd, powershell…) are not requested (`CMF_NORMAL`).
//!
//! CONSTRAINT: third-party extensions run in-process. A slow DLL can delay
//! the menu's opening and an unstable DLL can affect Ranger, just like
//! Explorer. `Config.shell_ctx_menu` therefore allows disabling this
//! integration. All native access goes through the `windows` crate.

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, anyhow};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_SFUIObject, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW, IContextMenu,
        IContextMenu2, IShellItemArray, SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
    };
    use windows::core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR};

    /// `CMIC_MASK_UNICODE` (shellapi.h) — absent from the generated
    /// `windows` bindings.
    const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW,
        MFS_CHECKED, MFS_DISABLED, MFT_SEPARATOR, MIIM_BITMAP, MIIM_CHECKMARKS, MIIM_FTYPE,
        MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, SW_SHOWNORMAL, WM_INITMENUPOPUP,
    };

    use crate::winutil::wide;

    /// Range of ids reserved for menu entries (offsets 0..=ID_LAST-ID_FIRST).
    const ID_FIRST: u32 = 1;
    const ID_LAST: u32 = 0x7FFF;
    /// Cap on re-rendered entries (chatty extensions get truncated).
    const MAX_ENTRIES: usize = 24;
    /// Max depth of flattened submenus (PeaZip/TortoiseGit = 1 level).
    const MAX_DEPTH: u8 = 1;

    /// Canonical verbs DISCARDED: already covered by Ranger (or meaningless
    /// in a custom UI). Lowercase comparison. COM extension entries without a
    /// canonical verb pass through (that's precisely what we want to show).
    const VERB_BLOCKLIST: &[&str] = &[
        "open",
        "opennewwindow",
        "opennewprocess",
        "opencontaining",
        "openas",
        "explore",
        "find",
        "view",
        "arrange",
        "groupby",
        "sortby",
        "cut",
        "copy",
        "paste",
        "pastelink",
        "delete",
        "rename",
        "properties",
        "link",
        "copyaspath",
        "sendto",
        // The Share verbs are deliberately NOT filtered: Ranger has no sharing
        // of its own, and the system dialog is the one users expect. It anchors
        // itself on the invoking window, so `invoke` must pass a real HWND.
        "pintohome",
        "pintostartscreen",
        "pintostart",
        "restoreprevious",
        "previousversions",
        "undo",
        "redo",
        "viewcustomwizard",
        "personalize",
        "display",
        "newfolder",
        "new",
        "edit",
        "print",
        "printto",
        "runas",
        "runasuser",
    ];

    /// Entries DISCARDED by LABEL — redundant with Ranger or meaningless,
    /// but WITHOUT a usable canonical verb (the "Open with" cascade, or shell
    /// commands without a verb). Normalized comparison (lowercase, no
    /// trailing "…", no "&") and EXACT ("Open with" discarded, "Open with VS
    /// Code" kept). Multilingual: the label comes from the OS, not from
    /// Ranger.
    const LABEL_BLOCKLIST: &[&str] = &[
        // "Add to favorites" (confused with Ranger's own).
        "ajouter aux favoris",
        "add to favorites",
        "add to favourites",
        "agregar a favoritos",
        "añadir a favoritos",
        "zu favoriten hinzufügen",
        "aggiungi ai preferiti",
        // "Include in library" (useless in Ranger).
        "inclure dans la bibliothèque",
        "include in library",
        "incluir en biblioteca",
        "in bibliothek aufnehmen",
        "includi nella raccolta",
        // Generic "Open with" (Ranger's own picker is richer).
        "ouvrir avec",
        "open with",
        "abrir con",
        "öffnen mit",
        "apri con",
        // "Open in terminal": Ranger has its own, common to Linux/Windows.
        "ouvrir dans le terminal",
        "open in terminal",
        "open in windows terminal",
        "abrir en el terminal",
        "im terminal öffnen",
        "apri nel terminale",
    ];

    /// Is `label` (already "&"-cleaned) present in [`LABEL_BLOCKLIST`]?
    /// Normalizes before comparison: lowercase + strip a trailing "…"/"..." +
    /// trim.
    fn label_blocklisted(label: &str) -> bool {
        let n = label
            .trim()
            .trim_end_matches('…')
            .trim_end_matches("...")
            .trim()
            .to_lowercase();
        LABEL_BLOCKLIST.contains(&n.as_str())
    }

    /// A shell menu entry (1-level tree): non-empty `children` =
    /// cascade (PeaZip, TortoiseGit…) rendered as a dedicated FLYOUT; `id` =
    /// command OFFSET (invoke, leaves only); `icon` = the menu bitmap
    /// converted to RGBA (via `winutil::hbitmap_to_rgba`), `None` if
    /// absent/owner-drawn.
    pub struct ShellEntry {
        pub id: u32,
        pub label: String,
        pub icon: Option<(Vec<u8>, u32, u32)>,
        pub children: Vec<ShellEntry>,
    }

    type MenuIcon = (Vec<u8>, u32, u32);

    thread_local! {
        /// Some Shell extensions only set their `HBITMAP` every other time
        /// (lazy init/owner-draw). A successfully extracted icon therefore
        /// stays the session's stable reference for that label, instead of
        /// randomly falling back to the generic icon on the next right-click.
        /// Bounded cache: labels come from third-party DLLs.
        static MENU_ICON_CACHE: RefCell<HashMap<String, MenuIcon>> = RefCell::new(HashMap::new());
    }

    const MAX_ICON_CACHE: usize = 256;

    /// Session kept alive between the menu's OPENING and its INVOCATION: the
    /// ids are only valid for THIS `IContextMenu`, and the `HMENU` must
    /// outlive it. UI-thread only (COM STA) — carried by an `Rc<RefCell<…>>`.
    pub struct ShellMenuSession {
        icm: IContextMenu,
        hmenu: HMENU,
        /// Working directory (wide, NUL-terminated) passed to `InvokeCommand`.
        dir: Vec<u16>,
    }

    impl Drop for ShellMenuSession {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyMenu(self.hmenu);
            }
        }
    }

    /// Builds the shell menu for `paths` (selection, or [current folder]
    /// for the dead zone) and returns the session + the filtered entries.
    /// `None` = nothing to show (COM failure, no entries left after
    /// filtering…).
    pub fn build_for_paths(
        paths: &[PathBuf],
        work_dir: &Path,
    ) -> Option<(ShellMenuSession, Vec<ShellEntry>)> {
        if paths.is_empty() {
            return None;
        }
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            // Paths → PIDLs → IShellItemArray (copies the PIDLs → freed here).
            let mut pidls: Vec<*const ITEMIDLIST> = Vec::with_capacity(paths.len());
            for p in paths {
                let w = wide(&p.to_string_lossy());
                let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
                if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_err() {
                    for &q in &pidls {
                        CoTaskMemFree(Some(q as *const core::ffi::c_void));
                    }
                    return None;
                }
                pidls.push(pidl as *const _);
            }
            let array: Result<IShellItemArray, _> = SHCreateShellItemArrayFromIDLists(&pidls);
            for &q in &pidls {
                CoTaskMemFree(Some(q as *const core::ffi::c_void));
            }
            let array = array.ok()?;
            // The COMPOSITE menu object (static verbs + COM extensions).
            let icm: IContextMenu = array.BindToHandler(None, &BHID_SFUIObject).ok()?;
            let hmenu = CreatePopupMenu().ok()?;
            if icm
                .QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, 0 /* CMF_NORMAL */)
                .is_err()
            {
                let _ = DestroyMenu(hmenu);
                return None;
            }
            // IContextMenu2 (if supported): fills the dynamic submenus.
            let icm2: Option<IContextMenu2> = icm.cast().ok();
            // WM_INITMENUPOPUP on the ROOT menu too: some handlers only set
            // their dynamic bitmaps/items at this point.
            if let Some(icm2) = &icm2 {
                let _ = icm2.HandleMenuMsg(WM_INITMENUPOPUP, WPARAM(hmenu.0 as usize), LPARAM(0));
            }
            let out = walk(hmenu, 0, &icm, &icm2);
            if out.is_empty() {
                let _ = DestroyMenu(hmenu);
                return None;
            }
            let dir = wide(&work_dir.to_string_lossy());
            Some((ShellMenuSession { icm, hmenu, dir }, out))
        }
    }

    /// Walks `hmenu` and returns the (1-level) TREE of entries: cascades →
    /// `children` (rendered as a flyout on the Slint side). Ignores
    /// separators, grayed-out items, empty labels (owner-drawn), and
    /// blocklisted verbs.
    unsafe fn walk(
        hmenu: HMENU,
        depth: u8,
        icm: &IContextMenu,
        icm2: &Option<IContextMenu2>,
    ) -> Vec<ShellEntry> {
        unsafe {
            let mut out = Vec::new();
            let count = GetMenuItemCount(Some(hmenu));
            for i in 0..count.max(0) as u32 {
                if out.len() >= MAX_ENTRIES {
                    break;
                }
                let mut buf = [0u16; 260];
                let mut mii = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    // Two families still coexist among Shell extensions: modern
                    // ones set `hbmpItem` (`MIIM_BITMAP`), classic ones use
                    // `SetMenuItemBitmaps` and therefore `hbmpChecked` /
                    // `hbmpUnchecked` (`MIIM_CHECKMARKS`). We accept both without
                    // assuming the version or configuration of the third-party
                    // software.
                    fMask: MIIM_ID
                        | MIIM_STRING
                        | MIIM_SUBMENU
                        | MIIM_FTYPE
                        | MIIM_STATE
                        | MIIM_BITMAP
                        | MIIM_CHECKMARKS,
                    dwTypeData: PWSTR(buf.as_mut_ptr()),
                    cch: (buf.len() - 1) as u32,
                    ..Default::default()
                };
                if GetMenuItemInfoW(hmenu, i, true, &mut mii).is_err() {
                    continue;
                }
                if (mii.fType.0 & MFT_SEPARATOR.0) != 0 || (mii.fState.0 & MFS_DISABLED.0) != 0 {
                    continue;
                }
                let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
                // Empty label = owner-drawn with no text → unrecoverable, skip it.
                if len == 0 {
                    continue;
                }
                // Strips the "&" accelerator markers (Explorer).
                let label = String::from_utf16_lossy(&buf[..len])
                    .replace("&&", "\u{1}")
                    .replace('&', "")
                    .replace('\u{1}', "&");
                // Discarded by LABEL — applies to both leaves AND cascades (e.g.
                // the "Open with" cascade, which the verb filter can't see).
                if label_blocklisted(&label) {
                    continue;
                }
                let icon = stable_menu_icon(&label, menu_item_bitmap_rgba(&mii));
                if !mii.hSubMenu.is_invalid() {
                    // Cascade (PeaZip/TortoiseGit…): filled ON THE FLY by the
                    // extension on WM_INITMENUPOPUP → simulated via HandleMenuMsg.
                    if depth >= MAX_DEPTH {
                        continue;
                    }
                    if let Some(icm2) = icm2 {
                        let _ = icm2.HandleMenuMsg(
                            WM_INITMENUPOPUP,
                            WPARAM(mii.hSubMenu.0 as usize),
                            LPARAM(i as isize),
                        );
                    }
                    let children = walk(mii.hSubMenu, depth + 1, icm, icm2);
                    // Empty cascade after filtering → nothing to show.
                    if !children.is_empty() {
                        out.push(ShellEntry {
                            id: 0,
                            label,
                            icon,
                            children,
                        });
                    }
                } else {
                    if mii.wID < ID_FIRST || mii.wID > ID_LAST {
                        continue;
                    }
                    let offset = mii.wID - ID_FIRST;
                    if VERB_BLOCKLIST.contains(&canonical_verb(icm, offset).as_str()) {
                        continue;
                    }
                    out.push(ShellEntry {
                        id: offset,
                        label,
                        icon,
                        children: Vec::new(),
                    });
                }
            }
            out
        }
    }

    /// Converts a menu item's bitmap to RGBA. The `HBMMENU_*` MAGIC VALUES
    /// (-1 = owner-drawn callback, 1..11 = system bitmaps) are NOT real
    /// `HBITMAP`s → ignored. The bitmap belongs to the menu (destroyed with
    /// it): read-only, no `DeleteObject`.
    unsafe fn menu_bitmap_rgba(
        hbmp: windows::Win32::Graphics::Gdi::HBITMAP,
    ) -> Option<(Vec<u8>, u32, u32)> {
        unsafe {
            let raw = hbmp.0 as isize;
            // Whatever you do, do NOT write `raw <= 11`: on Windows x64, a real
            // GDI handle can be sign-extended (e.g. Beyond Compare) and therefore
            // appear negative while still being a perfectly valid `OBJ_BITMAP`.
            // Only the documented sentinels are not readable HBITMAPs.
            if is_menu_bitmap_sentinel(raw) {
                return None; // null, HBMMENU_CALLBACK (-1), or system HBMMENU_* (1..11)
            }
            crate::winutil::hbitmap_to_rgba(hbmp)
        }
    }

    fn is_menu_bitmap_sentinel(raw: isize) -> bool {
        raw == 0 || raw == -1 || (1..=11).contains(&raw)
    }

    /// Extracts the image attached to the item regardless of which legacy API
    /// the extension chose. `hbmpItem` takes priority; failing that, we take
    /// the bitmap matching the current state, then the other one as a last
    /// resort (some extensions only fill in one of the two handles while
    /// reusing the same image for both states).
    unsafe fn menu_item_bitmap_rgba(mii: &MENUITEMINFOW) -> Option<MenuIcon> {
        unsafe {
            let checked = (mii.fState.0 & MFS_CHECKED.0) != 0;
            let (state_bitmap, fallback_bitmap) = if checked {
                (mii.hbmpChecked, mii.hbmpUnchecked)
            } else {
                (mii.hbmpUnchecked, mii.hbmpChecked)
            };
            menu_bitmap_rgba(mii.hbmpItem)
                .or_else(|| menu_bitmap_rgba(state_bitmap))
                .or_else(|| menu_bitmap_rgba(fallback_bitmap))
        }
    }

    /// Stabilizes a command's icon between two menu builds. Classic Shell
    /// extensions are free to supply their bitmap lazily; when a pass
    /// returns nothing, we reuse the last valid extraction for the same
    /// label. Images are copied before the `HMENU` owning the `HBITMAP` is
    /// destroyed.
    fn stable_menu_icon(label: &str, fresh: Option<MenuIcon>) -> Option<MenuIcon> {
        let key = label.trim().to_lowercase();
        MENU_ICON_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(icon) = fresh {
                if cache.len() >= MAX_ICON_CACHE && !cache.contains_key(&key) {
                    // A full flush is intentionally predictable and avoids a
                    // second LRU structure for a few dozen items.
                    cache.clear();
                }
                cache.insert(key, icon.clone());
                Some(icon)
            } else {
                cache.get(&key).cloned()
            }
        })
    }

    /// A command's canonical verb (lowercase), or "" if the extension doesn't
    /// expose one (common for COM entries — which we precisely want to keep).
    unsafe fn canonical_verb(icm: &IContextMenu, offset: u32) -> String {
        unsafe {
            let mut buf = [0u16; 128];
            if icm
                .GetCommandString(
                    offset as usize,
                    GCS_VERBW,
                    None,
                    PSTR(buf.as_mut_ptr() as *mut u8),
                    buf.len() as u32,
                )
                .is_err()
            {
                return String::new();
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&buf[..len]).to_lowercase()
        }
    }

    /// Invokes the session's `offset` command (numeric MAKEINTRESOURCE verb,
    /// working directory = the view's folder).
    pub fn invoke(session: &ShellMenuSession, offset: u32) -> Result<()> {
        unsafe {
            let mut info: CMINVOKECOMMANDINFOEX = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32;
            info.fMask = CMIC_MASK_UNICODE;
            info.lpVerb = PCSTR(offset as usize as *const u8);
            info.lpVerbW = PCWSTR(offset as usize as *const u16);
            info.lpDirectoryW = PCWSTR(session.dir.as_ptr());
            info.nShow = SW_SHOWNORMAL.0;
            // Owner window for whatever the verb puts on screen. The Share
            // flyout anchors on it and does nothing without one; error boxes
            // and progress dialogs from other extensions also need an owner to
            // sit above Ranger instead of behind it.
            if let Some(hwnd) = crate::winmsg::self_hwnd() {
                info.hwnd = HWND(hwnd as *mut std::ffi::c_void);
            }
            session
                .icm
                .InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO)
                .map_err(|e| anyhow!("InvokeCommand({offset}): {e}"))
        }
    }

    /// The `offset` command's canonical verb (lowercase). Lets the caller detect
    /// the "Share" entry (its verb contains `share`) and drive the native share
    /// sheet instead of the shell verb, which fails from an unpackaged host.
    pub fn verb_for(session: &ShellMenuSession, offset: u32) -> String {
        unsafe { canonical_verb(&session.icm, offset) }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            MENU_ICON_CACHE, is_menu_bitmap_sentinel, label_blocklisted, stable_menu_icon,
        };

        #[test]
        fn label_filter_matches_variants_but_not_specifics() {
            // Normalization: case, trailing "…", spaces.
            assert!(label_blocklisted("Ouvrir avec"));
            assert!(label_blocklisted("Open with…"));
            assert!(label_blocklisted("  add to favorites  "));
            assert!(label_blocklisted("Inclure dans la bibliothèque"));
            // EXACT: an "Open with <app>" entry is NOT discarded.
            assert!(!label_blocklisted("Ouvrir avec VS Code"));
            assert!(!label_blocklisted("Open with Notepad"));
            assert!(!label_blocklisted("7-Zip"));
        }

        #[test]
        fn shell_icon_cache_fills_in_a_later_missing_bitmap() {
            MENU_ICON_CACHE.with(|cache| cache.borrow_mut().clear());
            let icon = (vec![1, 2, 3, 4], 1, 1);
            assert_eq!(
                stable_menu_icon("Example command", Some(icon.clone())),
                Some(icon.clone())
            );
            assert_eq!(stable_menu_icon(" example COMMAND ", None), Some(icon));
            assert_eq!(stable_menu_icon("Another command", None), None);
        }

        #[test]
        fn signed_gdi_handles_are_not_confused_with_menu_sentinels() {
            assert!(is_menu_bitmap_sentinel(0));
            assert!(is_menu_bitmap_sentinel(-1));
            assert!(is_menu_bitmap_sentinel(1));
            assert!(is_menu_bitmap_sentinel(11));
            assert!(!is_menu_bitmap_sentinel(12));
            // A sign-extended x64 handle remains valid: only -1 is the
            // HBMMENU_CALLBACK pseudo-handle.
            assert!(!is_menu_bitmap_sentinel(-955_575_740));
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{Result, anyhow};

    /// Inert variant outside Windows: COM shell extensions don't exist there,
    /// and `.desktop` file actions use a different mechanism.
    pub struct ShellEntry {
        pub id: u32,
        pub label: String,
        pub icon: Option<(Vec<u8>, u32, u32)>,
        pub children: Vec<ShellEntry>,
    }
    pub struct ShellMenuSession;

    pub fn build_for_paths(
        _paths: &[PathBuf],
        _work_dir: &Path,
    ) -> Option<(ShellMenuSession, Vec<ShellEntry>)> {
        None
    }
    pub fn invoke(_session: &ShellMenuSession, _offset: u32) -> Result<()> {
        Err(anyhow!("shell menu: Windows only"))
    }
    pub fn verb_for(_session: &ShellMenuSession, _offset: u32) -> String {
        String::new()
    }
}

pub use imp::{ShellMenuSession, build_for_paths, invoke, verb_for};
