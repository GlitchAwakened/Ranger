//! Native file drag to EXTERNAL applications — **Windows**.
//!
//! HYBRID approach: internal drag (between views / onto a folder) stays
//! handled by Slint (ghost, auto-scroll, menu). When the cursor LEAVES the
//! window during a drag, we switch here to a native OLE drag so that
//! Notepad++, VSCode, Explorer, etc. receive the files (shell `CF_HDROP`
//! format).
//!
//! On the way out, the `IDataObject` is built by the shell
//! (`IShellFolder::GetUIObjectOf`) and `SHDoDragDrop` provides the default
//! `IDropSource` + the drag image. On the way in, our small `IDropTarget`
//! replaces winit's so we can receive `DragOver` and reuse Ranger's
//! hover/menu.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};
use windows::Win32::Foundation::{HGLOBAL, HWND, POINTL};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows::Win32::System::Com::{
    CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject, IStream, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl, OleInitialize,
    RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_SHELLIDLIST, DragQueryFileW, FILEDESCRIPTORW,
    FILEGROUPDESCRIPTORW, HDROP, IShellFolder, SHDoDragDrop, SHGetDesktopFolder,
    SHParseDisplayName,
};
use windows::core::{PCWSTR, Ref, implement};

/// Event from an incoming OLE drag. Ranger replaces winit's minimal drop
/// target so it can also receive `DragOver` coordinates, essential for
/// real-time row hover.
pub enum IncomingFileDrag {
    Hover {
        screen_x: i32,
        screen_y: i32,
        copy: bool,
    },
    Leave,
    Drop {
        paths: Vec<PathBuf>,
        screen_x: i32,
        screen_y: i32,
        copy: bool,
        /// Present when Ranger had to take ownership of source data before
        /// returning from OLE `Drop` (for example an Outlook attachment or a
        /// temporary path exposed by an archive manager).
        staging: Option<DropStaging>,
    },
    /// The source advertised supported external data, but Ranger could not
    /// take ownership of usable paths or bytes during `Drop`.
    ExternalDropFailed,
}

/// Ranger-owned data that must stay alive until the asynchronous transfer
/// finishes. Hard-linked captures require a real copy at the destination so a
/// persistent third-party source can never share file identity with it.
pub struct DropStaging {
    pub temp_dir: PathBuf,
    pub copy_from_staging: bool,
}

type DropHandler = Box<dyn Fn(IncomingFileDrag)>;

#[implement(IDropTarget)]
struct RangerDropTarget {
    handler: DropHandler,
    /// Cheap format classification retained between `DragEnter` and `Drop`.
    /// No source-owned data is rendered until the user actually drops it.
    incoming_kind: Cell<IncomingDataKind>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum IncomingDataKind {
    ShellPaths,
    ApplicationPaths,
    VirtualFiles,
    #[default]
    None,
}

impl IncomingDataKind {
    fn accepted(self) -> bool {
        !matches!(self, Self::None)
    }

    fn copy_only(self) -> bool {
        matches!(self, Self::ApplicationPaths | Self::VirtualFiles)
    }
}

impl RangerDropTarget {
    fn ctrl_down(keys: MODIFIERKEYS_FLAGS) -> bool {
        // MK_CONTROL, defined in winuser.h. The generated type doesn't provide
        // a dedicated constant across every windows-rs feature combination.
        keys.0 & 0x0008 != 0
    }

    fn choose_copy_effect(
        allowed: windows::Win32::System::Ole::DROPEFFECT,
        accepted: bool,
    ) -> windows::Win32::System::Ole::DROPEFFECT {
        if accepted && allowed.0 & DROPEFFECT_COPY.0 != 0 {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        }
    }

    /// Selects COPY only when the source actually offered it. `pdwEffect` is
    /// an input/output parameter: returning an effect outside the input mask
    /// violates the OLE contract and can produce misleading cursor feedback.
    fn set_effect(effect: *mut windows::Win32::System::Ole::DROPEFFECT, accepted: bool) -> bool {
        if effect.is_null() {
            return false;
        }
        unsafe {
            *effect = Self::choose_copy_effect(*effect, accepted);
            *effect == DROPEFFECT_COPY
        }
    }

    fn effect_value(effect: *mut windows::Win32::System::Ole::DROPEFFECT) -> u32 {
        if effect.is_null() {
            0
        } else {
            unsafe { (*effect).0 }
        }
    }
}

impl IDropTarget_Impl for RangerDropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let kind = data
            .as_ref()
            .map(classify_incoming_data)
            .unwrap_or_default();
        self.incoming_kind.set(kind);
        let accepted = RangerDropTarget::set_effect(effect, kind.accepted());
        if accepted {
            (self.handler)(IncomingFileDrag::Hover {
                screen_x: point.x,
                screen_y: point.y,
                copy: kind.copy_only() || RangerDropTarget::ctrl_down(keys),
            });
        }
        Ok(())
    }

    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let kind = self.incoming_kind.get();
        let accepted = RangerDropTarget::set_effect(effect, kind.accepted());
        if accepted {
            (self.handler)(IncomingFileDrag::Hover {
                screen_x: point.x,
                screen_y: point.y,
                copy: kind.copy_only() || RangerDropTarget::ctrl_down(keys),
            });
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        self.incoming_kind.set(IncomingDataKind::None);
        (self.handler)(IncomingFileDrag::Leave);
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut windows::Win32::System::Ole::DROPEFFECT,
    ) -> windows::core::Result<()> {
        let allowed_effect = RangerDropTarget::effect_value(effect);
        let kind = self.incoming_kind.replace(IncomingDataKind::None);
        let mut paths = Vec::new();
        let mut staging = None;
        let copy_allowed = allowed_effect & DROPEFFECT_COPY.0 != 0;
        if copy_allowed && let Some(data) = data.as_ref() {
            match kind {
                IncomingDataKind::ShellPaths => {
                    paths = file_paths(data);
                }
                IncomingDataKind::ApplicationPaths => {
                    let rendered_paths = file_paths(data);
                    if let Some(captured) = capture_application_paths(&rendered_paths) {
                        paths = captured.paths;
                        staging = Some(DropStaging {
                            temp_dir: captured.temp_dir,
                            copy_from_staging: captured.used_hard_links,
                        });
                    }
                }
                IncomingDataKind::VirtualFiles => {
                    if let Some(materialized) = materialize_virtual_files(data) {
                        paths = materialized.paths;
                        staging = Some(DropStaging {
                            temp_dir: materialized.temp_dir,
                            copy_from_staging: false,
                        });
                    }
                }
                IncomingDataKind::None => {}
            }
            // Some providers advertise CF_HDROP but render it only
            // conditionally. Keep the already-working Outlook route as a
            // fallback when usable paths were not returned at Drop time.
            if paths.is_empty()
                && staging.is_none()
                && has_virtual_files(data)
                && let Some(materialized) = materialize_virtual_files(data)
            {
                paths = materialized.paths;
                staging = Some(DropStaging {
                    temp_dir: materialized.temp_dir,
                    copy_from_staging: false,
                });
            }
        }
        let accepted = RangerDropTarget::set_effect(effect, !paths.is_empty());
        if accepted {
            (self.handler)(IncomingFileDrag::Drop {
                paths,
                screen_x: point.x,
                screen_y: point.y,
                copy: kind.copy_only() || RangerDropTarget::ctrl_down(keys),
                staging,
            });
        } else if copy_allowed && kind.copy_only() {
            (self.handler)(IncomingFileDrag::ExternalDropFailed);
        } else {
            (self.handler)(IncomingFileDrag::Leave);
        }
        Ok(())
    }
}

struct CapturedPathDrop {
    paths: Vec<PathBuf>,
    temp_dir: PathBuf,
    used_hard_links: bool,
}

/// Captures application-provided CF_HDROP paths before returning from OLE
/// `Drop`. 7-Zip, for example, deletes its extraction directory immediately
/// after `DoDragDrop` returns, before Ranger's deferred transfer can start.
fn capture_application_paths(paths: &[PathBuf]) -> Option<CapturedPathDrop> {
    if paths.is_empty() {
        return None;
    }
    let dir = create_drop_dir("ranger-path-dnd")?;
    let mut out = Vec::with_capacity(paths.len());
    let mut used_hard_links = false;
    for (index, source) in paths.iter().enumerate() {
        let Some(name) = source.file_name() else {
            warn!(path = %source.display(), "application drop path has no file name");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        };
        // Separate roots preserve same-named items from different source
        // folders; Ranger's existing conflict resolver handles them later.
        let item_dir = dir.join(index.to_string());
        if let Err(err) = std::fs::create_dir(&item_dir) {
            warn!(error = %err, path = %item_dir.display(), "creating application drop item directory failed");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        let captured = item_dir.join(name);
        match capture_path_tree(source, &captured) {
            Ok(item_used_hard_links) => used_hard_links |= item_used_hard_links,
            Err(err) => {
                warn!(error = %err, path = %source.display(), "capturing application drop path failed");
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
        }
        out.push(captured);
    }
    Some(CapturedPathDrop {
        paths: out,
        temp_dir: dir,
        used_hard_links,
    })
}

fn create_drop_dir(prefix: &str) -> Option<PathBuf> {
    for _ in 0..32 {
        let serial = DROP_SERIAL.with(|counter| {
            let value = counter.get().wrapping_add(1);
            counter.set(value);
            value
        });
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{serial}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Some(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                warn!(error = %err, path = %dir.display(), "creating drop directory failed");
                return None;
            }
        }
    }
    warn!(prefix, "could not allocate a unique drop directory");
    None
}

/// Recreates a tree without following Windows reparse points. Regular files
/// use hard links first so the OLE callback stays independent of file size;
/// cross-volume and unsupported filesystems fall back to a physical copy.
fn capture_path_tree(source: &Path, destination: &Path) -> std::io::Result<bool> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "reparse points are not supported in transient file drops",
        ));
    }
    if metadata.is_dir() {
        std::fs::create_dir(destination)?;
        let mut used_hard_links = false;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            used_hard_links |=
                capture_path_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        Ok(used_hard_links)
    } else if metadata.is_file() {
        match std::fs::hard_link(source, destination) {
            Ok(()) => Ok(true),
            Err(_) => {
                std::fs::copy(source, destination)?;
                Ok(false)
            }
        }
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported filesystem object in transient file drop",
        ))
    }
}

thread_local! {
    /// Explicitly keeps our COM object alive. OLE also holds a reference
    /// between RegisterDragDrop and window destruction.
    static DROP_TARGET: RefCell<Option<IDropTarget>> = const { RefCell::new(None) };
}

/// Replaces the very minimal file drop target installed by winit. The latter
/// only reports "file entered/left/dropped", without the DragOver position;
/// Ranger needs that position to target the panel, row, and executable. Must
/// be called after the HWND is created, on the UI thread.
pub fn init_drop_target(hwnd: isize, handler: impl Fn(IncomingFileDrag) + 'static) -> bool {
    if hwnd == 0 {
        return false;
    }
    DROP_TARGET.with(|slot| {
        if slot.borrow().is_some() {
            return true;
        }
        unsafe {
            if let Err(err) = OleInitialize(None) {
                warn!(error = %err, "initializing OLE for Ranger drop target failed");
                return false;
            }
            // winit already registers a CF_HDROP target. It isn't exposed to
            // Slint and doesn't provide DragOver: we cleanly replace it.
            let _ = RevokeDragDrop(HWND(hwnd as *mut c_void));
        }
        let target: IDropTarget = RangerDropTarget {
            handler: Box::new(handler),
            incoming_kind: Cell::new(IncomingDataKind::None),
        }
        .into();
        match unsafe { RegisterDragDrop(HWND(hwnd as *mut c_void), &target) } {
            Ok(()) => {
                *slot.borrow_mut() = Some(target);
                debug!(hwnd, "Ranger OLE drop target registered");
                true
            }
            Err(err) => {
                warn!(error = %err, "register Ranger OLE drop target failed");
                false
            }
        }
    })
}

/// Re-registers the already-created Ranger target after the native window has
/// completed its first event-loop iterations. Slint's declarative startup
/// timer can expire while winit is still finalizing its own CF_HDROP target;
/// reusing the same COM object once the loop is stable keeps Ranger's richer
/// target authoritative without adding any recurring work.
pub fn rebind_drop_target(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    DROP_TARGET.with(|slot| {
        let Some(target) = slot.borrow().as_ref().cloned() else {
            warn!(
                hwnd,
                "Ranger OLE drop target was unavailable for delayed registration"
            );
            return false;
        };
        let revoke = unsafe { RevokeDragDrop(HWND(hwnd as *mut c_void)) };
        let register = unsafe { RegisterDragDrop(HWND(hwnd as *mut c_void), &target) };
        if let Err(err) = revoke {
            debug!(error = %err, hwnd, "revoke before delayed drop-target registration failed");
        }
        match register {
            Ok(()) => {
                debug!(hwnd, "Ranger OLE drop target re-registered after startup");
                true
            }
            Err(err) => {
                warn!(error = %err, hwnd, "delayed Ranger OLE drop-target registration failed");
                false
            }
        }
    })
}

/// Classifies the formats without rendering their data. In particular,
/// calling `GetData(CF_HDROP)` from `DragEnter` can make archive managers
/// extract files merely because the pointer crossed the window.
fn classify_incoming_data(data: &IDataObject) -> IncomingDataKind {
    let has_hdrop = has_hdrop(data);
    let has_stream_hdrop = has_stream_hdrop(data);
    let has_shell_id_list = has_shell_id_list(data);
    let has_virtual_files = has_virtual_files(data);
    classify_formats(
        has_hdrop,
        has_stream_hdrop,
        has_shell_id_list,
        has_virtual_files,
    )
}

fn classify_formats(
    has_paths: bool,
    has_stream_hdrop: bool,
    has_shell_id_list: bool,
    has_virtual_files: bool,
) -> IncomingDataKind {
    if has_paths {
        if has_shell_id_list && !has_stream_hdrop {
            IncomingDataKind::ShellPaths
        } else {
            // Standard CF_HDROP uses HGLOBAL. Some application data objects
            // additionally advertise IStream and synthesize both CF_HDROP and
            // Shell IDList Array from temporary paths (PeaZip's public
            // TDropFileSource implementation is one example). CIDA is not a
            // lifetime guarantee in that case, so capture before Drop returns.
            IncomingDataKind::ApplicationPaths
        }
    } else if has_virtual_files {
        IncomingDataKind::VirtualFiles
    } else {
        IncomingDataKind::None
    }
}

fn has_hdrop(data: &IDataObject) -> bool {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe { data.QueryGetData(&format).is_ok() }
}

/// Detects a non-standard, application-provided CF_HDROP representation.
/// Microsoft's CF_HDROP contract uses TYMED_HGLOBAL; accepting IStream as well
/// is a useful provider-level signal that the paths are synthesized rather
/// than a normal Shell filesystem selection. QueryGetData never renders them.
fn has_stream_hdrop(data: &IDataObject) -> bool {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_ISTREAM.0 as u32,
    };
    unsafe { data.QueryGetData(&format).is_ok() }
}

/// The Shell IDList Array is a positive marker for data objects produced by
/// Explorer and Ranger's own Shell-based outgoing drag. Those paths remain on
/// the existing Move/Copy/Link route.
fn has_shell_id_list(data: &IDataObject) -> bool {
    let format = FORMATETC {
        cfFormat: clip_format(CFSTR_SHELLIDLIST),
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe { data.QueryGetData(&format).is_ok() }
}

/// Extracts and COPIES the CF_HDROP paths from an IDataObject. `ReleaseStgMedium`
/// always releases the medium after reading; no Shell data leaks into the
/// application state.
fn file_paths(data: &IDataObject) -> Vec<PathBuf> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = match unsafe { data.GetData(&format) } {
        Ok(medium) => medium,
        Err(err) => {
            debug!(error = %err, "CF_HDROP could not be rendered during Drop");
            return Vec::new();
        }
    };
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        warn!(
            tymed = medium.tymed,
            "CF_HDROP source returned an unexpected storage medium"
        );
        unsafe {
            ReleaseStgMedium(&mut medium);
        }
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        let hdrop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        if out.try_reserve(count as usize).is_err() {
            warn!(count, "CF_HDROP path list is too large to allocate");
        } else {
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                if len == 0 {
                    continue;
                }
                let mut wide = vec![0u16; len + 1];
                if DragQueryFileW(hdrop, i, Some(&mut wide)) > 0 {
                    out.push(PathBuf::from(String::from_utf16_lossy(&wide[..len])));
                }
            }
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

// ---- "Virtual files" (Outlook attachments, zip entries, browser images…) ----
// These aren't on disk: `CFSTR_FILEDESCRIPTORW` names them, `CFSTR_FILECONTENTS`
// carries their bytes (usually an `IStream`). We materialize them into a temp
// folder during `Drop`, and the normal drop pipeline then MOVES them into the
// target folder. A guard removes the complete staging tree after the move.

thread_local! {
    static DROP_SERIAL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

struct MaterializedFileDrop {
    paths: Vec<PathBuf>,
    temp_dir: PathBuf,
}

/// Registered clipboard-format id for the given name. `RegisterClipboardFormatW`
/// is idempotent (same id for a given name), so this is cheap to call.
fn clip_format(name: PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// True when the drag offers a "virtual file" descriptor (a file not on disk).
fn has_virtual_files(data: &IDataObject) -> bool {
    let format = FORMATETC {
        cfFormat: clip_format(CFSTR_FILEDESCRIPTORW),
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe { data.QueryGetData(&format).is_ok() }
}

/// Materializes the drag's virtual files into a fresh temp folder and returns
/// their real paths (`None` on failure). Must run during `Drop`, while the
/// `IDataObject` is still valid.
fn materialize_virtual_files(data: &IDataObject) -> Option<MaterializedFileDrop> {
    let names = read_file_descriptor_names(data)?;
    if names.is_empty() {
        warn!("virtual file descriptor contained no items");
        return None;
    }
    let dir = create_drop_dir("ranger-dnd")?;
    let mut out = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        // A descriptor may carry a relative path (zip subfolders): keep only the
        // final component so the file always stays inside our temp folder.
        let leaf = name.rsplit(['\\', '/']).next().unwrap_or(name);
        if leaf.is_empty() || leaf == "." || leaf == ".." {
            warn!(index, "virtual file descriptor contained an invalid name");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        // Each item gets its own staging subdirectory. This preserves duplicate
        // attachment names so the existing conflict resolver can arbitrate them.
        let item_dir = dir.join(index.to_string());
        if let Err(err) = std::fs::create_dir(&item_dir) {
            warn!(error = %err, path = %item_dir.display(), "creating virtual item directory failed");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        let path = item_dir.join(leaf);
        if let Err(err) = write_file_contents(data, index as i32, &path) {
            warn!(error = %err, index, name, "materializing virtual file failed");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        out.push(path);
    }
    Some(MaterializedFileDrop {
        paths: out,
        temp_dir: dir,
    })
}

/// Reads the file NAMES from the drag's `FILEGROUPDESCRIPTORW`.
fn read_file_descriptor_names(data: &IDataObject) -> Option<Vec<String>> {
    let format = FORMATETC {
        cfFormat: clip_format(CFSTR_FILEDESCRIPTORW),
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let Ok(mut medium) = (unsafe { data.GetData(&format) }) else {
        warn!("reading virtual file descriptors failed");
        return None;
    };
    let out = unsafe {
        let hglobal = medium.u.hGlobal;
        let byte_len = GlobalSize(hglobal);
        let base = GlobalLock(hglobal) as *const u8;
        if base.is_null() {
            None
        } else {
            let descriptor_offset = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
            let descriptor_size = std::mem::size_of::<FILEDESCRIPTORW>();
            let count = if byte_len >= descriptor_offset {
                std::ptr::read_unaligned(base as *const u32) as usize
            } else {
                usize::MAX
            };
            let available = byte_len
                .saturating_sub(descriptor_offset)
                .checked_div(descriptor_size)
                .unwrap_or(0);
            let result = if count > available {
                warn!(
                    count,
                    byte_len, "virtual file descriptor block was truncated"
                );
                None
            } else {
                let first = base.add(descriptor_offset) as *const FILEDESCRIPTORW;
                let mut names = Vec::with_capacity(count);
                for i in 0..count {
                    // FILEDESCRIPTORW is packed: copy the name array through a
                    // raw pointer instead of creating an unaligned reference.
                    let name_ptr = std::ptr::addr_of!((*first.add(i)).cFileName);
                    let name: [u16; 260] = std::ptr::read_unaligned(name_ptr);
                    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                    names.push(String::from_utf16_lossy(&name[..len]));
                }
                Some(names)
            };
            let _ = GlobalUnlock(hglobal);
            result
        }
    };
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    out
}

/// Requests one supported storage medium for an indexed `FILECONTENTS` item.
/// Individual requests come first because some real-world OLE providers reject
/// a standards-compliant bitmask even though they support one of its members.
fn file_contents_medium(
    data: &IDataObject,
    index: i32,
) -> Option<windows::Win32::System::Com::STGMEDIUM> {
    let mut failures = Vec::with_capacity(3);
    for requested in [
        TYMED_ISTREAM.0 as u32,
        TYMED_HGLOBAL.0 as u32,
        (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
    ] {
        let format = FORMATETC {
            cfFormat: clip_format(CFSTR_FILECONTENTS),
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index,
            tymed: requested,
        };
        match unsafe { data.GetData(&format) } {
            Ok(medium) => return Some(medium),
            Err(err) => failures.push(format!("0x{requested:X}: {err}")),
        }
    }
    warn!(index, errors = %failures.join("; "), "requesting virtual file contents failed");
    None
}

/// Streams one virtual file straight to disk. This keeps memory use bounded
/// even for large Outlook attachments.
fn write_file_contents(data: &IDataObject, index: i32, path: &Path) -> Result<(), String> {
    let Some(mut medium) = file_contents_medium(data, index) else {
        return Err("the source did not provide a supported storage medium".into());
    };
    let result = unsafe {
        if medium.tymed == TYMED_ISTREAM.0 as u32 {
            (*medium.u.pstm)
                .as_ref()
                .ok_or_else(|| "the source returned a null IStream".to_string())
                .and_then(|stream| write_istream(stream, path))
        } else if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            write_hglobal(medium.u.hGlobal, path)
        } else {
            Err(format!(
                "the source returned unsupported TYMED 0x{:X}",
                medium.tymed
            ))
        }
    };
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    result
}

/// Writes an `IStream` to a file in fixed-size chunks.
fn write_istream(stream: &IStream, path: &Path) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|err| err.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let mut read: u32 = 0;
        let status = unsafe {
            stream.Read(
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut read),
            )
        };
        if read as usize > buf.len() {
            return Err("IStream returned an invalid byte count".into());
        }
        if read > 0 {
            file.write_all(&buf[..read as usize])
                .map_err(|err| err.to_string())?;
        }
        if status.is_err() {
            return Err(format!("IStream::Read failed with {status:?}"));
        }
        if read == 0 {
            return Ok(());
        }
    }
}

/// Writes the bytes held by an `HGLOBAL` without an intermediate allocation.
fn write_hglobal(hglobal: HGLOBAL, path: &Path) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|err| err.to_string())?;
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        return Ok(());
    }
    let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
    if ptr.is_null() {
        return Err("GlobalLock failed for virtual file contents".into());
    }
    let result = file
        .write_all(unsafe { std::slice::from_raw_parts(ptr, size) })
        .map_err(|err| err.to_string());
    unsafe {
        let _ = GlobalUnlock(hglobal);
    }
    result
}

/// NUL-terminated wide string (kept alive by the caller for as long as the
/// pointer is in use).
fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
fn wide_str(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Starts a native OLE drag of `paths` (all assumed to be in the SAME folder —
/// which is the case for a selection coming from a view). External apps
/// receive the files as `CF_HDROP` (COPY effect). Blocking (modal loop of
/// `SHDoDragDrop`) until dropped/cancelled. `true` if a drop occurred.
/// Fire-and-forget: any error is logged, never propagated (no panic).
pub fn drag_files(paths: &[PathBuf]) -> bool {
    match unsafe { drag_files_inner(paths) } {
        Ok(dropped) => dropped,
        Err(err) => {
            warn!(error = %err, "native OLE drag failed");
            false
        }
    }
}

unsafe fn drag_files_inner(paths: &[PathBuf]) -> windows::core::Result<bool> {
    unsafe {
        if paths.is_empty() {
            return Ok(false);
        }
        // Common parent folder (the selection comes from a single view).
        let Some(parent_dir) = paths[0].parent() else {
            return Ok(false);
        };

        // `SHDoDragDrop`/OLE require OLE init on the UI thread. Idempotent
        // (S_FALSE if already initialized); we ignore failure (already in MTA →
        // we try anyway).
        let _ = OleInitialize(None);

        let desktop: IShellFolder = SHGetDesktopFolder()?;

        // Absolute PIDL of the parent folder → IShellFolder of the parent.
        let parent_w = wide(parent_dir);
        let mut parent_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(PCWSTR(parent_w.as_ptr()), None, &mut parent_pidl, 0, None)?;
        // On failure `parent_pidl` (already allocated) must still be freed — a
        // bare `?` here leaked it.
        let parent: IShellFolder = match desktop.BindToObject(parent_pidl, None) {
            Ok(p) => p,
            Err(e) => {
                CoTaskMemFree(Some(parent_pidl as *const c_void));
                return Err(e);
            }
        };

        // Child (simple) PIDL of each file, relative to the parent.
        let mut child_pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
        for p in paths {
            let Some(name) = p.file_name() else { continue };
            let name_w = wide_str(name);
            let mut cpidl: *mut ITEMIDLIST = std::ptr::null_mut();
            // pcheaten=None, pdwattributes=null (attributes not requested).
            if parent
                .ParseDisplayName(
                    HWND::default(),
                    None,
                    PCWSTR(name_w.as_ptr()),
                    None,
                    &mut cpidl,
                    std::ptr::null_mut(),
                )
                .is_ok()
                && !cpidl.is_null()
            {
                child_pidls.push(cpidl);
            }
        }

        // Capture the outcome instead of `?`-returning: the shell-allocated
        // PIDLs below must be freed even when GetUIObjectOf / SHDoDragDrop fail
        // (both previously leaked parent_pidl AND every child PIDL).
        let dropped: windows::core::Result<bool> = if child_pidls.is_empty() {
            Ok(false)
        } else {
            let ptrs: Vec<*const ITEMIDLIST> = child_pidls.iter().map(|p| *p as *const _).collect();
            // IDataObject built by the shell (CF_HDROP + shell formats) — no
            // homegrown COM. `pdsrc = None` → SHDoDragDrop provides IDropSource +
            // the drag image.
            match parent.GetUIObjectOf(HWND::default(), &ptrs, None) {
                Ok(data) => {
                    let data: IDataObject = data;
                    match SHDoDragDrop(None, &data, None, DROPEFFECT_COPY) {
                        Ok(effect) => {
                            debug!(effect = effect.0, "SHDoDragDrop returned");
                            Ok(effect.0 != 0)
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        };

        // Free the PIDLs (shell-allocated → CoTaskMem) on every path.
        for c in &child_pidls {
            CoTaskMemFree(Some(*c as *const c_void));
        }
        CoTaskMemFree(Some(parent_pidl as *const c_void));
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static FILE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn incoming_format_classification_preserves_each_route() {
        assert_eq!(
            classify_formats(true, false, true, false),
            IncomingDataKind::ShellPaths
        );
        assert_eq!(
            classify_formats(true, false, false, false),
            IncomingDataKind::ApplicationPaths
        );
        assert_eq!(
            classify_formats(true, false, false, true),
            IncomingDataKind::ApplicationPaths
        );
        assert_eq!(
            classify_formats(true, true, true, false),
            IncomingDataKind::ApplicationPaths
        );
        assert_eq!(
            classify_formats(false, false, false, true),
            IncomingDataKind::VirtualFiles
        );
        assert_eq!(
            classify_formats(false, false, true, false),
            IncomingDataKind::None
        );
    }

    #[test]
    fn copy_effect_never_exceeds_the_source_mask() {
        use windows::Win32::System::Ole::{DROPEFFECT_LINK, DROPEFFECT_MOVE};

        assert_eq!(
            RangerDropTarget::choose_copy_effect(DROPEFFECT_COPY | DROPEFFECT_MOVE, true),
            DROPEFFECT_COPY
        );
        assert_eq!(
            RangerDropTarget::choose_copy_effect(DROPEFFECT_MOVE | DROPEFFECT_LINK, true),
            DROPEFFECT_NONE
        );
        assert_eq!(
            RangerDropTarget::choose_copy_effect(DROPEFFECT_COPY, false),
            DROPEFFECT_NONE
        );
    }

    #[test]
    fn application_capture_survives_source_removal_and_preserves_trees() {
        let _lock = FILE_TEST_LOCK.lock().expect("file test lock poisoned");
        let source_root = create_drop_dir("ranger-path-dnd").expect("create source root");
        let first_root = source_root.join("first");
        let second_root = source_root.join("second");
        std::fs::create_dir_all(first_root.join("album")).expect("create first tree");
        std::fs::create_dir_all(&second_root).expect("create second tree");
        std::fs::write(first_root.join("album").join("same.txt"), b"first")
            .expect("write first source");
        std::fs::write(second_root.join("same.txt"), b"second").expect("write second source");

        let captured =
            capture_application_paths(&[first_root.join("album"), second_root.join("same.txt")])
                .expect("capture application paths");
        std::fs::remove_dir_all(&source_root).expect("remove source tree");

        assert_eq!(captured.paths.len(), 2);
        assert_eq!(
            std::fs::read(captured.paths[0].join("same.txt")).expect("read captured tree"),
            b"first"
        );
        assert_eq!(
            std::fs::read(&captured.paths[1]).expect("read captured file"),
            b"second"
        );
        std::fs::remove_dir_all(&captured.temp_dir).expect("remove captured tree");
    }

    #[test]
    fn failed_application_capture_rolls_back_its_staging_tree() {
        let _lock = FILE_TEST_LOCK.lock().expect("file test lock poisoned");
        let source_root = create_drop_dir("ranger-path-dnd").expect("create source root");
        let valid = source_root.join("valid.txt");
        std::fs::write(&valid, b"valid").expect("write valid source");
        let before = application_drop_directories();

        assert!(capture_application_paths(&[valid, source_root.join("missing.txt")]).is_none());
        assert_eq!(application_drop_directories(), before);
        std::fs::remove_dir_all(source_root).expect("remove source root");
    }

    fn application_drop_directories() -> HashSet<OsString> {
        let prefix = format!("ranger-path-dnd-{}-", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .expect("read temp directory")
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&prefix)
                    .then(|| entry.file_name())
            })
            .collect()
    }
}
