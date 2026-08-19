//! File and folder operations.
//!
//! Covers:
//!   - copy / move (recursive for folders),
//!   - in-place rename,
//!   - duplication (generates a unique suffix `name (1).ext`, `name (2).ext`, …),
//!   - **cross-platform** trashing via the `trash` crate (FreeDesktop
//!     on Linux, Recycle Bin via `IFileOperation` on Windows),
//!   - permanent deletion (recursive, be careful),
//!   - reading detailed properties (size, dates, permissions).
//!
//! Error policy: returns `Result<…, std::io::Error>` when possible,
//! otherwise an `Error::Workspace` (see `crate::error::Error`). Operations
//! are intentionally **simple and synchronous**: the bridge layer decides
//! whether to push them onto a background thread (`std::thread`).

// Windows copies via `CopyFileExW` → the read/write loop only exists elsewhere.
#[cfg(not(windows))]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::Error;

/// Status of a long-running, cancellable operation (see [`copy_tree_progress`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpStatus {
    Done,
    Cancelled,
}

// ---------- Renaming ----------

/// Renames `from` to `new_name` (without changing folder). `new_name` must
/// be a **file name**, not a path (safety: any path component is rejected
/// to avoid unintended moves).
///
/// **Both** separators `/` and `\` are rejected regardless of OS: on
/// Windows `MAIN_SEPARATOR` is `\`, but `/` is also a valid separator there
/// → testing only `MAIN_SEPARATOR` would let `foo/bar` through (a move).
pub fn rename_in_place(from: &Path, new_name: &str) -> Result<PathBuf> {
    if let Err(reason) = check_file_name(new_name) {
        return Err(Error::Workspace(format!(
            "invalid new name: {new_name:?} ({reason:?})"
        )));
    }
    let parent = from
        .parent()
        .ok_or_else(|| Error::Workspace(format!("no parent for {}", from.display())))?;
    let to = parent.join(new_name);
    std::fs::rename(from, &to)?;
    Ok(to)
}

/// Renames `from`, replacing the existing **file** of the same name.
///
/// The replacement is deliberately limited to files (and links treated as
/// files): recursively merging/replacing two folders under the guise of a
/// simple rename would be a far more ambiguous and destructive operation. The
/// replacement is delegated to the OS's atomic primitive (`rename` on Unix /
/// `MoveFileExW` on Windows), never the dangerous `remove_file` then `rename`
/// sequence.
pub fn rename_in_place_replacing_file(from: &Path, new_name: &str) -> Result<PathBuf> {
    if let Err(reason) = check_file_name(new_name) {
        return Err(Error::Workspace(format!(
            "invalid new name: {new_name:?} ({reason:?})"
        )));
    }
    let parent = from
        .parent()
        .ok_or_else(|| Error::Workspace(format!("no parent for {}", from.display())))?;
    let to = parent.join(new_name);
    if from == to {
        return Ok(to);
    }

    let source_meta = std::fs::symlink_metadata(from)?;
    let target_meta = std::fs::symlink_metadata(&to)?;
    if source_meta.is_dir() || target_meta.is_dir() {
        return Err(Error::Workspace(
            "force replace is limited to files".to_string(),
        ));
    }

    std::fs::rename(from, &to)?;
    Ok(to)
}

/// Universal **name** validator (not a path): non-empty, no `/` or `\`
/// separator (rejected on all OSes, see the note on `rename_in_place`), and
/// different from `.` / `..`. Deliberately independent of the filesystem
/// (does not check existence) → reusable anywhere a name is entered:
/// creating/renaming folders and files, **and** bookmark labels
/// (containers / bookmarks) — see the naming popup. The caller adds an
/// existence check where relevant (e.g. `create_entry`).
pub fn is_valid_entry_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
}

/// Why a name cannot become a directory entry. Returned instead of a plain
/// `false` so the caller can say *what* is wrong: reporting "already taken" for
/// a name the filesystem simply refuses sends the user hunting for a
/// non-existent duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameRejection {
    /// Empty, or nothing but whitespace.
    Empty,
    /// Holds a character the filesystem cannot store — see
    /// [`FORBIDDEN_NAME_CHARS`] and the control characters.
    ForbiddenChar,
    /// `.` or `..`, which already denote the folder and its parent.
    DotEntry,
    /// A name Windows keeps for a device, with or without an extension
    /// (`NUL`, `COM1`, `LPT1.txt`, …). Windows-only.
    ReservedDevice,
    /// Windows silently drops a trailing space or dot, so the entry would not
    /// carry the name that was typed. Windows-only.
    TrailingDotOrSpace,
}

/// Characters no entry name may contain on the running platform, as a
/// display-ready list. Every OS rejects its own separators; Windows adds the
/// set reserved by the Win32 naming rules.
///
/// Single source of truth: [`check_file_name`] tests against these very
/// characters, and the interface quotes the same string back to the user.
#[cfg(windows)]
pub const FORBIDDEN_NAME_CHARS: &str = "\\ / : * ? \" < > |";
#[cfg(not(windows))]
pub const FORBIDDEN_NAME_CHARS: &str = "/";

/// The characters above, in matchable form. `\` stays rejected everywhere for
/// the reason given on [`rename_in_place`]: a name is never a path.
const FORBIDDEN_CHARS: &[char] = if cfg!(windows) {
    &['/', '\\', ':', '*', '?', '"', '<', '>', '|']
} else {
    &['/', '\\']
};

/// Names Windows resolves to a device rather than a file, whatever the folder.
/// Creating one fails, or worse, writes to the device.
#[cfg(windows)]
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validates a name destined for the **filesystem**, with the running
/// platform's rules.
///
/// Deliberately stricter than [`is_valid_entry_name`], which stays the rule for
/// names that never reach the disk (favourite and container labels): a bookmark
/// may legitimately be called "Draft?" while a file may not.
pub fn check_file_name(name: &str) -> std::result::Result<(), NameRejection> {
    if name.is_empty() {
        return Err(NameRejection::Empty);
    }
    if name == "." || name == ".." {
        return Err(NameRejection::DotEntry);
    }
    // Control characters are unusable on every filesystem and invisible in the
    // field, so they are reported with the other forbidden characters.
    if name.contains(FORBIDDEN_CHARS) || name.contains(|c: char| (c as u32) < 0x20) {
        return Err(NameRejection::ForbiddenChar);
    }
    #[cfg(windows)]
    {
        if name.ends_with(' ') || name.ends_with('.') {
            return Err(NameRejection::TrailingDotOrSpace);
        }
        // The device is matched on the stem: "NUL.txt" is the NUL device too.
        let stem = name.split('.').next().unwrap_or(name);
        if RESERVED_DEVICE_NAMES
            .iter()
            .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        {
            return Err(NameRejection::ReservedDevice);
        }
    }
    Ok(())
}

/// [`check_file_name`] reduced to a yes/no, for the call sites that only gate
/// an operation and have no message to build.
pub fn is_valid_file_name(name: &str) -> bool {
    check_file_name(name).is_ok()
}

/// Creates an entry (a folder if `is_dir`, otherwise an empty file) named `name`
/// in `parent`. Rejects an invalid name ([`check_file_name`]) and a target
/// that **already exists** (never overwrites). Returns the created path.
pub fn create_entry(parent: &Path, name: &str, is_dir: bool) -> Result<PathBuf> {
    let name = name.trim();
    if let Err(reason) = check_file_name(name) {
        return Err(Error::Workspace(format!(
            "invalid name: {name:?} ({reason:?})"
        )));
    }
    let target = parent.join(name);
    // `exists()` ignores broken symlinks. `symlink_metadata`, by contrast,
    // treats any directory entry as occupied, which is exactly the rule
    // expected for a creation without replacement.
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            return Err(Error::Workspace(format!(
                "{} already exists",
                target.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    if is_dir {
        std::fs::create_dir(&target)?;
    } else {
        // `create_new` also closes the race window between the check above
        // and the creation: a file that appears in the meantime is never
        // truncated, even on a share watched by another application.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
    }
    Ok(target)
}

// ---------- Unique name generation ----------

/// Starting from `original`, finds a free path in the same folder by
/// suffixing `- Copy01`, `- Copy02`, … **after** the name (before a real
/// extension):
///   - `notes.txt`   → `notes - Copy01.txt`, `notes - Copy02.txt`, …
///   - `MyFile 5.7`  → `MyFile 5.7 - Copy01` (`.7` is NOT an extension)
///   - `folder`      → `folder - Copy01`, …
///   - `notes - Copy01.txt` (already a copy) → `notes - Copy02.txt`
///     (suffixes are not stacked `- CopyNN`)
///
/// Returns the **target path** (which does not exist yet at computation time).
/// No anti-race guarantee: to create it, the caller follows up with
/// `copy()` / `rename()` (atomic operations on the OS side).
pub fn unique_sibling(original: &Path) -> PathBuf {
    unique_sibling_where(original, |p| p.exists())
}

/// `unique_sibling` with a caller-supplied notion of an occupied name.
///
/// The default only knows the filesystem, which is not enough when several
/// operations run at once: a name can be free on disk yet already claimed as
/// the destination of a copy still in flight. Callers pass a `taken` that
/// covers those as well, so two concurrent operations never pick the same
/// target.
pub fn unique_sibling_where(original: &Path, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let parent = original.parent().unwrap_or(Path::new("."));
    let file_name = original.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // If the original name is already free, keep it as is.
    let candidate = parent.join(file_name);
    if !taken(&candidate) {
        return candidate;
    }

    let (stem, ext) = split_name(file_name);
    let base = strip_copy_suffix(&stem);

    for n in 1..=10_000 {
        let p = parent.join(compose_copy(&base, &ext, n));
        if !taken(&p) {
            return p;
        }
    }
    // Pathological case: 10,000 collisions → timestamped suffix, likely free.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let new_name = if ext.is_empty() {
        format!("{base} - Copy{stamp}")
    } else {
        format!("{base} - Copy{stamp}.{ext}")
    };
    parent.join(new_name)
}

/// Composes `base - CopyNN[.ext]` (NN zero-padded to 2 digits, ≥ 01).
fn compose_copy(base: &str, ext: &str, n: usize) -> String {
    if ext.is_empty() {
        format!("{base} - Copy{n:02}")
    } else {
        format!("{base} - Copy{n:02}.{ext}")
    }
}

/// Strips a trailing `- CopyNN` suffix (NN = digits) to avoid stacking copy
/// markers (`x - Copy01` copied again → `x - Copy02`, not
/// `x - Copy01 - Copy01`).
fn strip_copy_suffix(stem: &str) -> String {
    if let Some(pos) = stem.rfind(" - Copy") {
        let after = &stem[pos + " - Copy".len()..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            return stem[..pos].to_string();
        }
    }
    stem.to_string()
}

/// Splits `name` into `(stem, ext)`. Returns `(name, "")` if:
///   - the name starts with `.` (e.g. `.bashrc`),
///   - there is no dot,
///   - or the segment after the last dot contains **no letters**
///     (e.g. `MyFile 5.7`, `data.2024` → `.7`/`.2024` are not extensions).
///
/// So `.jpg`, `.7z`, `.mp3` remain extensions, but not `.7`.
/// Public: reused by the GUI to colorize the extension in the list.
pub fn split_name(name: &str) -> (String, String) {
    if name.starts_with('.') {
        return (name.to_string(), String::new());
    }
    match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.chars().any(|c| c.is_ascii_alphabetic()) => {
            (s.to_string(), e.to_string())
        }
        _ => (name.to_string(), String::new()),
    }
}

/// Extension of a file name FOR TYPING PURPOSES (the "ext" column, sorting by
/// type, filtering by extension), lowercased. Unlike [`split_name`] (dedicated
/// to DISPLAY, which leaves dotfiles whole), treats a dotfile's suffix as an
/// extension: `.gitignore` → `gitignore`, `.config.json` → `json`.
/// Same "at least one letter" guard as `split_name` (`data.2024` → `""`).
/// Empty if there is no dot or the suffix is not relevant.
pub fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, e)) if e.chars().any(|c| c.is_ascii_alphabetic()) => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

// ---------- Copying ----------

/// **Windows**: attempts to RECREATE the symbolic link `src` → `dst`
/// (`symlink_dir` if the target is a folder, `symlink_file` otherwise), just
/// like the Unix branch. Returns `(recreated, target_is_dir)`. `recreated = false`
/// means creation failed — typically for lack of privilege (Windows symbolic
/// links require developer mode or admin, see `link_into`); the caller must
/// then materialize the target, recursively for a folder.
#[cfg(windows)]
fn try_clone_symlink(src: &Path, dst: &Path) -> (bool, bool) {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    // `metadata` FOLLOWS the link → type of the TARGET (broken link → treated as a file).
    let target_is_dir = std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false);
    let created = match std::fs::read_link(src) {
        Ok(target) => {
            if target_is_dir {
                symlink_dir(&target, dst).is_ok()
            } else {
                symlink_file(&target, dst).is_ok()
            }
        }
        Err(_) => false,
    };
    (created, target_is_dir)
}

/// Recursive copy of `src` to `dst`. `dst` must not exist.
/// For folders, recreates the tree; for files, `std::fs::copy`.
pub fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src)?;
    if metadata.file_type().is_symlink() {
        // Copy the link as-is (re-creation of the link).
        #[cfg(unix)]
        {
            let target = std::fs::read_link(src)?;
            std::os::unix::fs::symlink(target, dst)?;
            return Ok(());
        }
        #[cfg(windows)]
        {
            let (cloned, target_is_dir) = try_clone_symlink(src, dst);
            if cloned {
                return Ok(());
            }
            // Insufficient privilege → materialize the TARGET. For a folder,
            // recursively copy the RESOLVED target (canonicalize → real folder,
            // not a link → no infinite recursion). For a file, `fs::copy`
            // follows `src` and copies the target's content.
            if target_is_dir {
                return copy_path(&std::fs::canonicalize(src)?, dst);
            }
            std::fs::copy(src, dst)?;
            return Ok(());
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::fs::copy(src, dst)?;
            return Ok(());
        }
    }

    if metadata.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_child = entry.path();
            let dst_child = dst.join(entry.file_name());
            copy_path(&src_child, &dst_child)?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Recursive size (bytes) of a file/folder, used to pre-scan for a progress
/// bar. Symbolic links count as 0 (the link is recreated, not its target).
/// Best-effort: any access error counts as 0.
pub fn path_size(path: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if md.file_type().is_symlink() {
        0
    } else if md.is_dir() {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                total += path_size(&entry.path());
            }
        }
        total
    } else {
        md.len()
    }
}

/// Recursive copy of `src` to `dst` (which must not exist) with:
///   - **byte reporting**: `on_bytes(delta)` is called as writing progresses,
///   - **cooperative cancellation**: `cancel()` is checked at file boundaries
///     AND during the copy of a large file (in chunks, see
///     `COPY_BUF_BYTES`).
///
/// If cancelled while writing a file, the partial file is deleted (a
/// truncated file is never left behind).
pub fn copy_tree_progress(
    src: &Path,
    dst: &Path,
    on_bytes: &mut dyn FnMut(u64),
    cancel: &dyn Fn() -> bool,
) -> Result<OpStatus> {
    if cancel() {
        return Ok(OpStatus::Cancelled);
    }
    let md = std::fs::symlink_metadata(src)?;

    if md.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = std::fs::read_link(src)?;
            std::os::unix::fs::symlink(target, dst)?;
        }
        #[cfg(windows)]
        {
            let (cloned, target_is_dir) = try_clone_symlink(src, dst);
            if !cloned {
                // Insufficient privilege: materialize the target with byte
                // tracking and cancellation support. See `copy_path`.
                if target_is_dir {
                    return copy_tree_progress(&std::fs::canonicalize(src)?, dst, on_bytes, cancel);
                }
                return copy_file_progress(src, dst, on_bytes, cancel);
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            std::fs::copy(src, dst)?;
        }
        return Ok(OpStatus::Done);
    }

    if md.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            if cancel() {
                return Ok(OpStatus::Cancelled);
            }
            if copy_tree_progress(
                &entry.path(),
                &dst.join(entry.file_name()),
                on_bytes,
                cancel,
            )? == OpStatus::Cancelled
            {
                return Ok(OpStatus::Cancelled);
            }
        }
        Ok(OpStatus::Done)
    } else {
        copy_file_progress(src, dst, on_bytes, cancel)
    }
}

/// Copy chunk size: 4 MiB. Each chunk is a sequential read THEN write — on a
/// NETWORK share, each call pays the SMB latency: at 256 KiB throughput
/// plateaued well below Explorer's (which does large I/O). 4 MiB roughly
/// matches `CopyFileEx`'s throughput while still allowing progress reporting
/// and cancellation per chunk. Used on Linux and macOS; Windows relies on
/// `CopyFileExW`, see below.
#[cfg(not(windows))]
const COPY_BUF_BYTES: usize = 4 * 1024 * 1024;

#[cfg(not(windows))]
fn copy_file_progress(
    src: &Path,
    dst: &Path,
    on_bytes: &mut dyn FnMut(u64),
    cancel: &dyn Fn() -> bool,
) -> Result<OpStatus> {
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::File::create(dst)?;
    let mut buf = vec![0u8; COPY_BUF_BYTES];
    loop {
        if cancel() {
            drop(writer);
            let _ = std::fs::remove_file(dst); // clean up the partial file
            return Ok(OpStatus::Cancelled);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        on_bytes(n as u64);
    }
    // Best-effort: preserve permissions (Unix mode).
    if let Ok(meta) = std::fs::metadata(src) {
        let _ = std::fs::set_permissions(dst, meta.permissions());
    }
    Ok(OpStatus::Done)
}

/// Copy of A SINGLE file via `CopyFileExW` — the same engine as Explorer:
/// pipelined I/O (reads/writes in parallel), and on an SMB share, **server-side**
/// copy (`FSCTL_SRV_COPYCHUNK`) when source and target live on the same server.
/// Progress goes through the native callback; `PROGRESS_CANCEL` interrupts the
/// operation and deletes the partial target. Attributes, including read-only,
/// are copied by the API.
#[cfg(windows)]
fn copy_file_progress(
    src: &Path,
    dst: &Path,
    on_bytes: &mut dyn FnMut(u64),
    cancel: &dyn Fn() -> bool,
) -> Result<OpStatus> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CopyFileExW(
            existing: *const u16,
            new: *const u16,
            progress: Option<CopyProgress>,
            data: *mut core::ffi::c_void,
            cancel_flag: *mut i32,
            flags: u32,
        ) -> i32;
    }
    // `LPPROGRESS_ROUTINE` — called for each chunk copied (CALLBACK_CHUNK_FINISHED).
    type CopyProgress = unsafe extern "system" fn(
        total: i64,
        transferred: i64,
        stream_size: i64,
        stream_transferred: i64,
        stream_num: u32,
        reason: u32,
        h_src: isize,
        h_dst: isize,
        data: *mut core::ffi::c_void,
    ) -> u32;
    const PROGRESS_CONTINUE: u32 = 0;
    const PROGRESS_CANCEL: u32 = 1;
    const ERROR_REQUEST_ABORTED: i32 = 1235;

    // The C callback receives the TOTAL transferred; the DELTA is handed to
    // `on_bytes` (existing contract) via this context passed through `data`.
    struct Ctx<'a> {
        on_bytes: &'a mut dyn FnMut(u64),
        cancel: &'a dyn Fn() -> bool,
        last: u64,
    }
    unsafe extern "system" fn progress(
        _total: i64,
        transferred: i64,
        _stream_size: i64,
        _stream_transferred: i64,
        _stream_num: u32,
        _reason: u32,
        _h_src: isize,
        _h_dst: isize,
        data: *mut core::ffi::c_void,
    ) -> u32 {
        unsafe {
            let ctx = &mut *(data as *mut Ctx);
            if (ctx.cancel)() {
                return PROGRESS_CANCEL; // CopyFileExW deletes the partial target
            }
            let done = transferred as u64;
            if done > ctx.last {
                (ctx.on_bytes)(done - ctx.last);
                ctx.last = done;
            }
            PROGRESS_CONTINUE
        }
    }

    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let src_w = wide(src);
    let dst_w = wide(dst);
    let mut ctx = Ctx {
        on_bytes,
        cancel,
        last: 0,
    };
    // flags = 0: overwrites an existing target; the caller has already
    // resolved name conflicts.
    let ok = unsafe {
        CopyFileExW(
            src_w.as_ptr(),
            dst_w.as_ptr(),
            Some(progress),
            &mut ctx as *mut Ctx as *mut core::ffi::c_void,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_REQUEST_ABORTED) {
            return Ok(OpStatus::Cancelled); // cancelled via PROGRESS_CANCEL
        }
        return Err(err.into());
    }
    Ok(OpStatus::Done)
}

/// Creates a **link** to `src` in `dst_dir` (unique name on conflict). Cross-OS:
///   - **Linux**: symbolic link (`symlink`), valid for both files AND folders.
///   - **Windows**: junction for a folder, hard link for a file (junctions
///     only target folders; symbolic links would require admin privileges).
///     Neither goes through the shell — see `windows_link`.
///
/// Returns the path of the created link.
pub fn link_into(src: &Path, dst_dir: &Path) -> Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| Error::Workspace(format!("no file name for {}", src.display())))?;
    let target = unique_sibling(&dst_dir.join(name));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, &target)?;
    }
    #[cfg(windows)]
    {
        windows_link(src, &target)?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = &target;
        return Err(Error::Workspace(
            "link_into is not supported on this platform".into(),
        ));
    }
    Ok(target)
}

/// Creates a link at an EXACT path `dst_path` (chosen name) pointing to `src`,
/// with the SAME mechanism as [`link_into`] (Unix symlink; junction for a
/// folder, hard link for a file on Windows). Rejects an already-existing
/// target (never overwrites). Used by the "Create shortcut" menu
/// (symlink tab), which targets the current folder with a given name.
pub fn link_as(src: &Path, dst_path: &Path) -> Result<()> {
    if dst_path.exists() {
        return Err(Error::Workspace(format!(
            "{} already exists",
            dst_path.display()
        )));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst_path)?;
    }
    #[cfg(windows)]
    {
        windows_link(src, dst_path)?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        return Err(Error::Workspace(
            "link_as is not supported on this platform".into(),
        ));
    }
    Ok(())
}

/// Creates a Windows link named `link` pointing at `src`, WITHOUT exposing the
/// paths to `cmd`'s metacharacter parsing:
///   - a FILE gets a hard link via `std::fs::hard_link` — a plain Win32 call, no
///     shell involved;
///   - a FOLDER gets a junction. `mklink /J` is a `cmd` built-in, so it must run
///     under `cmd /C`; cmd interprets `& | < > ( ) ^` BEFORE splitting argv, so a
///     folder such as `R&D` would otherwise break the command and a crafted name
///     could inject a second one. Both paths are wrapped in double quotes, built
///     as an `OsString` (so non-ASCII paths stay byte-exact) and passed via
///     `raw_arg` (Rust auto-quotes only args containing spaces); cmd treats a
///     quoted run as one literal token, and Windows paths cannot contain `"`, so
///     the quoting cannot be escaped out of. (`%VAR%` is still expanded by cmd
///     inside quotes, so a folder literally named after a defined variable could
///     mis-target — a wrong link, never code execution.)
#[cfg(windows)]
fn windows_link(src: &Path, link: &Path) -> Result<()> {
    if !src.is_dir() {
        return std::fs::hard_link(src, link)
            .map_err(|e| Error::Workspace(format!("hard link {}: {e}", link.display())));
    }
    use std::ffi::OsString;
    use std::os::windows::process::CommandExt;
    let mut cmdline = OsString::from("/C mklink /J \"");
    cmdline.push(link.as_os_str());
    cmdline.push("\" \"");
    cmdline.push(src.as_os_str());
    cmdline.push("\"");
    let status = std::process::Command::new("cmd")
        .raw_arg(&cmdline)
        .status()
        .map_err(|e| Error::Workspace(format!("spawn mklink: {e}")))?;
    if !status.success() {
        return Err(Error::Workspace(format!(
            "mklink /J failed for {}",
            link.display()
        )));
    }
    Ok(())
}

/// Copies `src` into `dst_dir`. If the name already exists there, suffixes
/// `(1)`, `(2)`… Returns the final target path.
pub fn copy_into(src: &Path, dst_dir: &Path) -> Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| Error::Workspace(format!("no file name for {}", src.display())))?;
    let proposed = dst_dir.join(name);
    let target = unique_sibling(&proposed);
    copy_path(src, &target)?;
    Ok(target)
}

/// Moves `src` into `dst_dir`. Uses `std::fs::rename` (fast, atomic on the
/// same filesystem); on EXDEV failure (different filesystems), falls back to
/// copy + remove.
pub fn move_into(src: &Path, dst_dir: &Path) -> Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| Error::Workspace(format!("no file name for {}", src.display())))?;
    let proposed = dst_dir.join(name);
    let target = unique_sibling(&proposed);

    match std::fs::rename(src, &target) {
        Ok(()) => Ok(target),
        Err(_) => {
            // Cross-device fallback: copy then remove.
            copy_path(src, &target)?;
            permanent_delete(src)?;
            Ok(target)
        }
    }
}

/// Moves `src` to the **exact** path `dst` (which must not exist).
/// Fast `rename`, cross-device copy+remove fallback. Used when the target
/// name has already been resolved (e.g. after the conflict popup).
pub fn move_to(src: &Path, dst: &Path) -> Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_path(src, dst)?;
            permanent_delete(src)?;
            Ok(())
        }
    }
}

/// Duplicates `src` in the same location with a unique suffix.
pub fn duplicate(src: &Path) -> Result<PathBuf> {
    let target = unique_sibling(src);
    copy_path(src, &target)?;
    Ok(target)
}

// ---------- Deletion ----------

/// Result of a trash request.
///
/// Windows network shares have no recycle bin: after GUI confirmation, the
/// same route then performs a permanent deletion. This distinction prevents
/// the calling layer from offering a misleading Ctrl+Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashDisposition {
    Trashed,
    PermanentlyDeleted,
}

/// Permanent deletion, recursive for a REAL folder. **No recycle bin.**
///
/// A reparse point — a symlink OR a Windows junction — is removed as a LINK and
/// never recursed into (recursing would delete the TARGET's contents). A
/// *directory* reparse point needs `remove_dir` on Windows: `remove_file` fails
/// on it (the bug this guards against), while `remove_dir_all` would follow it
/// into the target. A file symlink uses `remove_file`. On Unix, `remove_file`
/// unlinks any symlink and `symlink_metadata` never reports a symlink as a dir.
pub fn permanent_delete(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        let attrs = md.file_attributes();
        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            // Symlink/junction → drop the link itself, never its target.
            if is_dir {
                std::fs::remove_dir(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        } else if is_dir {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    #[cfg(not(windows))]
    {
        if md.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            // Regular file OR any symlink (unlinked, target untouched).
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Sends a file/folder to the trash — **cross-platform** via the `trash`
/// crate:
///   - **Linux**: FreeDesktop spec (`~/.local/share/Trash`, + volume
///     support), without depending on an external tool like `gio`.
///   - **Windows**: Recycle Bin via `IFileOperation`.
///
/// On failure (trash unavailable, volume without a recycle bin…), the error
/// is propagated; the caller can then offer permanent deletion with
/// confirmation.
pub fn trash_with_disposition(path: &Path) -> Result<TrashDisposition> {
    // Windows network volumes have no recycle bin. The GUI therefore confirms
    // permanent deletion before this call. `IFileOperation` is avoided for these
    // paths: verbatim UNC paths are not reliably accepted, and its modal
    // warning cannot be attached to the window from the operation thread.
    #[cfg(windows)]
    if crate::places::is_network_path(path) {
        permanent_delete(path)?;
        return Ok(TrashDisposition::PermanentlyDeleted);
    }
    trash::delete(path)
        .map(|()| TrashDisposition::Trashed)
        .map_err(|e| Error::Workspace(format!("trash {}: {e}", path.display())))
}

/// Legacy variant preserving the old unit signature.
pub fn trash(path: &Path) -> Result<()> {
    trash_with_disposition(path).map(|_| ())
}

/// Data-only failure reason for [`restore_from_trash`]. Carries no
/// natural-language text on purpose: the GUI translates each variant via its
/// i18n layer before showing it to the user. `detail`, when present, is the
/// underlying `trash` crate's own (English) message — arbitrary text outside
/// Ranger's control, passed through as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashError {
    /// Could not enumerate the OS trash.
    ListFailed(String),
    /// No trashed item matches the requested original path.
    ItemNotFound,
    /// The OS refused to restore the matched item.
    RestoreFailed(String),
    /// No implementation for this OS.
    PlatformUnsupported,
}

/// Restores from the trash the most recently deleted item whose original
/// path matches `original_path` exactly.
///
/// `trash::delete` does not return the identifier created by the OS. Ranger's
/// lightweight history is therefore associated with the original path, and
/// the most recent occurrence is picked here. The operation is available on
/// Windows and on FreeDesktop Linux environments, without an external tool.
#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
pub fn restore_from_trash(original_path: &Path) -> std::result::Result<(), TrashError> {
    let items = trash::os_limited::list().map_err(|e| TrashError::ListFailed(e.to_string()))?;
    // The FreeDesktop trash (Linux) records the **canonicalized** original
    // path (symbolic links resolved — e.g. `/home` → `/var/home` on Fedora
    // Atomic), whereas Ranger recorded the navigation path as-is. A version
    // whose PARENT (which still exists after trashing) has its links resolved
    // is therefore also accepted. The raw match is still tried first
    // (Windows: Recycle Bin already returns the clean path).
    let resolved = resolve_parent_symlinks(original_path);
    let item = items
        .into_iter()
        .filter(|item| {
            let ip = item.original_path();
            paths_equal(&ip, original_path) || paths_equal(&ip, &resolved)
        })
        .max_by_key(|item| item.time_deleted)
        .ok_or(TrashError::ItemNotFound)?;
    trash::os_limited::restore_all(std::iter::once(item))
        .map_err(|e| TrashError::RestoreFailed(e.to_string()))
}

/// Resolves the symbolic links of `p`'s **parent** (which still exists even
/// if `p` has been trashed) then rejoins the file name. Used to match the
/// canonicalized original path stored by the trash. If the parent is
/// missing/unresolvable, returns `p` unchanged (safe fallback).
#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
fn resolve_parent_symlinks(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            std::fs::canonicalize(parent)
                .map(|cp| cp.join(name))
                .unwrap_or_else(|_| p.to_path_buf())
        }
        _ => p.to_path_buf(),
    }
}

/// Lexical comparison following the platform's usual rules, without
/// `canonicalize` or disk access (hence safe on a slow network share).
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Platforms without a restore API exposed by `trash::os_limited`.
#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
pub fn restore_from_trash(_original_path: &Path) -> std::result::Result<(), TrashError> {
    Err(TrashError::PlatformUnsupported)
}

/// Empties the trash (**permanent** deletion of all its contents).
/// Cross-OS via `trash::os_limited` (FreeDesktop on Linux, Recycle Bin on
/// Windows). **Destructive**: the caller must confirm before calling.
/// Log-only failure (never shown in the UI as-is): the message stays in
/// English on purpose (`ranger-core` has no i18n of its own).
#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
pub fn empty_trash() -> Result<()> {
    let items = trash::os_limited::list()
        .map_err(|e| Error::Workspace(format!("listing the trash: {e}")))?;
    trash::os_limited::purge_all(items)
        .map_err(|e| Error::Workspace(format!("emptying the trash: {e}")))
}

/// Platforms without `os_limited` support (e.g. macOS): not implemented.
#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
pub fn empty_trash() -> Result<()> {
    Err(Error::Workspace(
        "empty_trash is not supported on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ranger-ops-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn path_equality_follows_platform_case_rules_without_io() {
        #[cfg(windows)]
        assert!(paths_equal(
            Path::new(r"C:\Users\User\Pictures"),
            Path::new(r"c:\users\user\pictures")
        ));
        #[cfg(not(windows))]
        assert!(!paths_equal(
            Path::new("/home/User/Pictures"),
            Path::new("/home/user/pictures")
        ));
    }

    fn write_file(p: &Path, content: &[u8]) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(content).unwrap();
    }

    #[test]
    fn copy_path_handles_directory_symlink() {
        // Depending on link-creation privilege, the copy produces either a
        // link or materialized content. In both cases, `dst` must be an
        // existing folder containing the target file.
        let dir = tempdir();
        let real = dir.join("realdir");
        std::fs::create_dir_all(&real).unwrap();
        write_file(&real.join("a.txt"), b"hello");
        let link = dir.join("linkdir");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&real, &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let made = false;
        if made {
            let dst = dir.join("copied");
            copy_path(&link, &dst).expect("copying a directory link must not fail");
            // `dst` exists and behaves like a folder (link followed or copied).
            assert!(dst.exists(), "the copy must exist");
            assert!(std::fs::metadata(&dst).map(|m| m.is_dir()).unwrap_or(false));
            assert!(
                dst.join("a.txt").exists(),
                "the target's contents must be reachable"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permanent_delete_removes_a_directory_symlink_without_touching_its_target() {
        // A folder symlink/junction must be deleted as a LINK: the link goes, the
        // target and its contents stay. Guards both the Windows error (remove_file
        // on a directory symlink) and the data-loss trap (recursing into the
        // target). Skipped where link creation needs a privilege we don't have.
        let dir = tempdir();
        let real = dir.join("realdir");
        std::fs::create_dir_all(&real).unwrap();
        write_file(&real.join("keep.txt"), b"keep");
        let link = dir.join("linkdir");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&real, &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let made = false;
        if made {
            permanent_delete(&link).expect("deleting a folder symlink must succeed");
            assert!(
                std::fs::symlink_metadata(&link).is_err(),
                "the symlink itself must be gone"
            );
            assert!(
                real.join("keep.txt").exists(),
                "the target's contents must be untouched"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_name_handles_dotfiles_and_extensions() {
        assert_eq!(split_name("notes.txt"), ("notes".into(), "txt".into()));
        assert_eq!(
            split_name("archive.tar.gz"),
            ("archive.tar".into(), "gz".into())
        );
        assert_eq!(split_name(".bashrc"), (".bashrc".into(), "".into()));
        assert_eq!(split_name("no_ext"), ("no_ext".into(), "".into()));
        // A purely numeric suffix is not an extension.
        assert_eq!(split_name("MyFile 5.7"), ("MyFile 5.7".into(), "".into()));
        assert_eq!(split_name("data.2024"), ("data.2024".into(), "".into()));
        // But a suffix containing a letter remains an extension.
        assert_eq!(split_name("archive.7z"), ("archive".into(), "7z".into()));
    }

    #[test]
    fn ext_of_types_dotfiles_and_lowercases() {
        // Typing (column/sort/filter): a dotfile's suffix IS an extension.
        assert_eq!(ext_of(".gitignore"), "gitignore");
        assert_eq!(ext_of(".config.json"), "json");
        assert_eq!(ext_of("Photo.JPG"), "jpg"); // lowercased
        assert_eq!(ext_of("archive.tar.gz"), "gz");
        // Consistent with split_name: purely numeric suffix / no dot → empty.
        assert_eq!(ext_of("data.2024"), "");
        assert_eq!(ext_of("README"), "");
        assert_eq!(ext_of("archive."), "");
    }

    #[test]
    fn strip_copy_suffix_removes_trailing_marker() {
        assert_eq!(strip_copy_suffix("notes"), "notes");
        assert_eq!(strip_copy_suffix("notes - Copy01"), "notes");
        assert_eq!(strip_copy_suffix("notes - Copy12"), "notes");
        // Not a valid marker → unchanged.
        assert_eq!(strip_copy_suffix("notes - Copy"), "notes - Copy");
        assert_eq!(strip_copy_suffix("my - Copyright"), "my - Copyright");
    }

    #[test]
    fn unique_sibling_uses_copy_scheme() {
        let dir = tempdir();
        let orig = dir.join("notes.txt");
        // Not created yet → returns the path unchanged.
        assert_eq!(unique_sibling(&orig), orig);
        write_file(&orig, b"hello");

        let p1 = unique_sibling(&orig);
        assert_eq!(p1.file_name().unwrap(), "notes - Copy01.txt");
        write_file(&p1, b"copy1");

        let p2 = unique_sibling(&orig);
        assert_eq!(p2.file_name().unwrap(), "notes - Copy02.txt");

        // Copying a copy again does not stack suffixes.
        let p3 = unique_sibling(&p1);
        assert_eq!(p3.file_name().unwrap(), "notes - Copy02.txt");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_sibling_numeric_suffix_not_extension() {
        let dir = tempdir();
        let orig = dir.join("MyFile 5.7");
        write_file(&orig, b"x");
        let p1 = unique_sibling(&orig);
        assert_eq!(p1.file_name().unwrap(), "MyFile 5.7 - Copy01");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_path_file_and_dir_recursive() {
        let dir = tempdir();
        let src_dir = dir.join("src");
        std::fs::create_dir(&src_dir).unwrap();
        write_file(&src_dir.join("a.txt"), b"A");
        std::fs::create_dir(src_dir.join("sub")).unwrap();
        write_file(&src_dir.join("sub").join("b.txt"), b"B");

        let dst_dir = dir.join("dst");
        copy_path(&src_dir, &dst_dir).unwrap();

        assert_eq!(std::fs::read(dst_dir.join("a.txt")).unwrap(), b"A");
        assert_eq!(
            std::fs::read(dst_dir.join("sub").join("b.txt")).unwrap(),
            b"B"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_size_sums_recursively() {
        let dir = tempdir();
        let sub = dir.join("d");
        std::fs::create_dir(&sub).unwrap();
        write_file(&dir.join("a.bin"), &[0u8; 100]);
        write_file(&sub.join("b.bin"), &[0u8; 250]);
        assert_eq!(path_size(&dir), 350);
        assert_eq!(path_size(&dir.join("a.bin")), 100);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_tree_progress_reports_bytes() {
        let dir = tempdir();
        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();
        write_file(&src.join("a.bin"), &[7u8; 500]);
        write_file(&src.join("b.bin"), &[7u8; 300]);

        let dst = dir.join("dst");
        let mut total = 0u64;
        let status = copy_tree_progress(&src, &dst, &mut |d| total += d, &|| false).unwrap();
        assert_eq!(status, OpStatus::Done);
        assert_eq!(total, 800);
        assert_eq!(std::fs::read(dst.join("a.bin")).unwrap().len(), 500);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_tree_progress_cancels_and_cleans() {
        let dir = tempdir();
        let src = dir.join("big.bin");
        write_file(&src, &[1u8; 1_000_000]); // > chunk size → cancellation mid-flight
        let dst = dir.join("big-copy.bin");
        // cancel() true right away → nothing is copied.
        let status = copy_tree_progress(&src, &dst, &mut |_| {}, &|| true).unwrap();
        assert_eq!(status, OpStatus::Cancelled);
        assert!(!dst.exists(), "the partial file must be cleaned up");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_creates_sibling() {
        let dir = tempdir();
        let src = dir.join("file.bin");
        write_file(&src, b"data");
        let copy = duplicate(&src).unwrap();
        assert_eq!(copy.file_name().unwrap(), "file - Copy01.bin");
        assert_eq!(std::fs::read(&copy).unwrap(), b"data");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_in_place_rejects_path_separator() {
        let dir = tempdir();
        let src = dir.join("file.txt");
        write_file(&src, b"x");
        let bad = rename_in_place(&src, "sub/file.txt");
        assert!(bad.is_err());
        let bad2 = rename_in_place(&src, "..");
        assert!(bad2.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_in_place_ok() {
        let dir = tempdir();
        let src = dir.join("file.txt");
        write_file(&src, b"x");
        let new = rename_in_place(&src, "renamed.txt").unwrap();
        assert!(new.exists());
        assert!(!src.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_replacing_file_keeps_source_contents() {
        let dir = tempdir();
        let source = dir.join("source.txt");
        let target = dir.join("target.txt");
        write_file(&source, b"new");
        write_file(&target, b"old");

        let renamed = rename_in_place_replacing_file(&source, "target.txt").unwrap();
        assert_eq!(renamed, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!source.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_replacing_file_refuses_directories() {
        let dir = tempdir();
        let source = dir.join("source");
        let target = dir.join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(rename_in_place_replacing_file(&source, "target").is_err());
        assert!(source.is_dir());
        assert!(target.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_entry_dir_file_and_guards() {
        let dir = tempdir();
        // Folder.
        let d = create_entry(&dir, "sub", true).unwrap();
        assert!(d.is_dir());
        // File (with extension).
        let f = create_entry(&dir, "notes.txt", false).unwrap();
        assert!(f.is_file());
        // Rejected: name already taken.
        assert!(create_entry(&dir, "sub", true).is_err());
        std::fs::write(&f, b"keep me").unwrap();
        assert!(create_entry(&dir, "notes.txt", false).is_err());
        assert_eq!(std::fs::read(&f).unwrap(), b"keep me");
        // Rejected: invalid names (separator, ..).
        assert!(create_entry(&dir, "a/b", false).is_err());
        assert!(create_entry(&dir, "..", true).is_err());
        assert!(create_entry(&dir, "   ", false).is_err()); // empty after trim
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_name_the_filesystem_refuses_is_told_apart_from_a_name_already_taken() {
        // The distinction this function exists for: these are rejected on their
        // spelling alone, with no folder to compare against, so reporting them
        // as a duplicate would point at an entry that does not exist.
        assert_eq!(check_file_name(""), Err(NameRejection::Empty));
        assert_eq!(check_file_name("."), Err(NameRejection::DotEntry));
        assert_eq!(check_file_name(".."), Err(NameRejection::DotEntry));
        // Both separators everywhere: a name is never a path, see
        // `rename_in_place`.
        assert_eq!(check_file_name("a/b"), Err(NameRejection::ForbiddenChar));
        assert_eq!(check_file_name("a\\b"), Err(NameRejection::ForbiddenChar));
        // Invisible in the field, unusable on disk.
        assert_eq!(check_file_name("a\tb"), Err(NameRejection::ForbiddenChar));

        // Ordinary names stay accepted, including the ones whose punctuation
        // only looks suspicious.
        for ok in [
            "notes.txt",
            "Rapport 2026",
            ".clang-format",
            "a-b_c(1)[2]{3}",
            "été & co",
            "archive.tar.gz",
        ] {
            assert_eq!(check_file_name(ok), Ok(()), "{ok} should be accepted");
        }

        // Every character the interface lists must actually be refused: the
        // message and the rule are held to the same source of truth.
        for c in FORBIDDEN_NAME_CHARS.chars().filter(|c| !c.is_whitespace()) {
            assert_eq!(
                check_file_name(&format!("a{c}b")),
                Err(NameRejection::ForbiddenChar),
                "{c} is advertised as forbidden but was accepted"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_only_naming_rules_are_enforced() {
        // Win32 reserves these whatever the folder, extension included.
        assert_eq!(check_file_name("NUL"), Err(NameRejection::ReservedDevice));
        assert_eq!(
            check_file_name("com1.txt"),
            Err(NameRejection::ReservedDevice)
        );
        // Windows drops these silently, so the entry would not carry the name
        // that was typed.
        assert_eq!(
            check_file_name("report."),
            Err(NameRejection::TrailingDotOrSpace)
        );
        assert_eq!(
            check_file_name("report "),
            Err(NameRejection::TrailingDotOrSpace)
        );
        // Reserved as a whole name only — a longer name merely starting with
        // one is a perfectly ordinary file.
        assert_eq!(check_file_name("NULL.txt"), Ok(()));
        assert_eq!(check_file_name("console.log"), Ok(()));
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_only_rules_do_not_leak_onto_unix() {
        // These are valid names on a Unix filesystem and must stay creatable.
        for ok in [
            "NUL", "com1.txt", "report.", "report ", "a:b", "what?", "a*",
        ] {
            assert_eq!(check_file_name(ok), Ok(()), "{ok} should be accepted");
        }
    }

    #[test]
    fn move_into_uses_unique_name_on_collision() {
        let dir = tempdir();
        let from = dir.join("src");
        std::fs::create_dir(&from).unwrap();
        write_file(&from.join("a.txt"), b"A");

        let into = dir.join("into");
        std::fs::create_dir(&into).unwrap();
        write_file(&into.join("src"), b"existing"); // collision

        let dst = move_into(&from, &into).unwrap();
        // The "existing" content is intact, the move generated "src - Copy01".
        assert_eq!(dst.file_name().unwrap(), "src - Copy01");
        assert!(dst.is_dir());
        assert_eq!(std::fs::read(into.join("src")).unwrap(), b"existing");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Trash restoration compares the CANONICALIZED original path (as returned
    // by the FreeDesktop trash) to the path recorded by Ranger. `resolve_parent_
    // symlinks` must resolve the PARENT's links even if the leaf no longer exists
    // (file already in the trash) — e.g. `/home` → `/var/home`, mounted volumes, etc.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn resolve_parent_symlinks_resolves_symlinked_parent_even_if_leaf_gone() {
        use std::os::unix::fs::symlink;
        let dir = tempdir();
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.join("link");
        symlink(&real, &link).unwrap();

        // Path via the link, NONEXISTENT leaf (simulates an already-deleted file).
        let via_link = link.join("gone.txt");
        let resolved = resolve_parent_symlinks(&via_link);
        let expected = std::fs::canonicalize(&real).unwrap().join("gone.txt");
        assert_eq!(resolved, expected, "the parent (link) must be resolved");
        assert_ne!(
            resolved, via_link,
            "the path must have changed (link resolved)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
