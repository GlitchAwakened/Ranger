//! Listing, classification, sorting, formatting, and FS operations.
//!
//! This file covers **listing** a folder, **classification** by type,
//! **sorting**, and **formatting**. File operations
//! (copy / move / trash / properties) live in the
//! [`ops`] submodule.

pub mod ops;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::i18n::Lang;

/// Category of a file — drives icon selection on the Slint side.
///
/// The `u8` values are **stable**: they are serialized to Slint
/// (`int`) on the `FileRow.kind` side and used in ternary cascades
/// `entry.kind == 0 ? @image-url(...)`. **Do not reorder.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FileKind {
    Folder = 0,
    File = 1,
    Application = 2,
    Archive = 3,
    Audio = 4,
    Document = 5,
    Image = 6,
    Video = 7,
    /// Configuration files / structured data (toml, yaml, json, ini…).
    Config = 8,
}

/// Extensions recognized as images, sorted to allow binary search.
/// This list is also the authority for the Windows gallery filter: a new
/// extension therefore never needs to be added in two places.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "af", "afdesign", "afphoto", "afpub", "arw", "avif", "bmp", "cr2", "cr3", "dds", "dng", "exr",
    "gif", "hdr", "heic", "heif", "ico", "jpeg", "jpg", "nef", "orf", "pam", "pbm", "pgm", "png",
    "pnm", "ppm", "psd", "qoi", "raf", "raw", "rw2", "srw", "svg", "tga", "tif", "tiff", "webp",
    "xcf",
];

/// `extension` must be normalized to lowercase, without a dot.
pub fn is_image_extension(extension: &str) -> bool {
    IMAGE_EXTENSIONS.binary_search(&extension).is_ok()
}

/// Extensions recognized as archives, sorted to allow binary search.
/// Single source of truth: it classifies rows AND pre-fills the extension
/// filter of the ready-made "extract" commands, so an archive format never
/// needs adding in two places.
pub const ARCHIVE_EXTENSIONS: &[&str] = &[
    "7z", "ar", "bz2", "cab", "gz", "iso", "lz", "lzma", "lzo", "rar", "tar", "tbz2", "tgz", "txz",
    "xz", "zip", "zst",
];

/// `extension` must be normalized to lowercase, without a dot.
pub fn is_archive_extension(extension: &str) -> bool {
    ARCHIVE_EXTENSIONS.binary_search(&extension).is_ok()
}

impl FileKind {
    pub fn as_i32(self) -> i32 {
        self as u8 as i32
    }

    /// Can a file of this type be a launchable PROGRAM / SCRIPT (with
    /// dropped files as arguments)? Used to filter out false positives from
    /// the Unix executable bit: on a FAT/NTFS/exFAT mount (everything is
    /// `0777`) or for a file copied from Windows, an image/video/audio/document/
    /// archive/config can appear "executable" without being a program. Only a
    /// binary/package/script of a recognized type (`Application`) or a file of
    /// an UNRECOGNIZED type (`File` — typically a binary or a script WITHOUT an
    /// extension, common on Linux) are real candidates.
    pub fn can_be_program(self) -> bool {
        matches!(self, FileKind::Application | FileKind::File)
    }
}

/// An entry listed in a folder.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: Option<u64>,
    pub mtime_unix: Option<i64>,
    pub is_dir: bool,
    pub kind: FileKind,
    /// **Hidden** entry (Unix dotfile OR Windows HIDDEN attribute) — for a
    /// discreet visual marker when showing hidden items is enabled.
    pub hidden: bool,
    /// Symbolic link (or Windows junction). `is_dir` reflects the TARGET;
    /// this flag is used for the visual marker — a "folder + arrow" icon for
    /// a folder-link, to distinguish it from a real folder.
    pub is_symlink: bool,
    /// Unix executable bit computed during listing (avoids a second `stat`
    /// in the GUI for the "launch with dropped files" feedback).
    /// Always `false` on Windows, where extensions define this action.
    pub executable: bool,
}

/// Classifies a file from its extension (lower-case, without the dot).
/// For a folder, returns `Folder` regardless of the name.
pub fn classify_kind(extension: Option<&str>, is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Folder;
    }
    let Some(ext) = extension else {
        return FileKind::File;
    };
    let ext = ext.to_ascii_lowercase();
    if is_image_extension(&ext) {
        return FileKind::Image;
    }
    if is_archive_extension(&ext) {
        return FileKind::Archive;
    }
    match ext.as_str() {
        // Applications / binaries
        "exe" | "msi" | "appimage" | "deb" | "rpm" | "flatpak" | "snap" | "apk" | "dmg" | "pkg"
        | "sh" | "bat" | "ps1" | "cmd" => FileKind::Application,

        // Audio
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "aac" | "m4a" | "wma" | "aiff" | "alac" => {
            FileKind::Audio
        }

        // Documents (text, word processing, presentations, spreadsheets)
        "txt" | "md" | "rst" | "pdf" | "doc" | "docx" | "odt" | "rtf" | "tex" | "epub" | "djvu"
        | "xls" | "xlsx" | "ods" | "csv" | "tsv" | "ppt" | "pptx" | "odp" | "pages" | "numbers"
        | "key" => FileKind::Document,

        // Video
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" | "ts"
        | "3gp" => FileKind::Video,

        // Configuration / structured data
        "toml" | "yaml" | "yml" | "json" | "json5" | "ini" | "cfg" | "conf" | "config" | "xml"
        | "plist" | "env" | "reg" => FileKind::Config,

        _ => FileKind::File,
    }
}

/// True if `md` carries the Windows `FILE_ATTRIBUTE_HIDDEN` attribute.
/// On other OSes: always false (hidden files there follow the `.` prefix
/// convention, handled separately). `const`-evaluated → optimized.
#[cfg(windows)]
fn is_hidden_attr(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}
#[cfg(not(windows))]
fn is_hidden_attr(_md: &std::fs::Metadata) -> bool {
    false
}

/// Lists the entries of a folder.
///
/// `include_hidden = false` filters out **hidden** entries:
///   - Unix convention: name starting with `.` (on all OSes);
///   - Windows: `FILE_ATTRIBUTE_HIDDEN` attribute (Windows hidden files
///     generally don't use the `.` prefix).
///
/// Per-entry errors (permission, broken link…) are **silently ignored**
/// at the item level but logged: a partial listing is preferred over
/// giving up entirely because of one exotic file.
pub fn list_dir(path: &Path, include_hidden: bool) -> Result<Vec<Entry>> {
    Ok(list_dir_counted(path, include_hidden)?.0)
}

/// Like [`list_dir`], but ALSO returns the number of hidden entries present
/// in the folder — whether they're included (`include_hidden`) or not. Used to
/// discreetly signal to the user that hidden items exist (they
/// sometimes forget to check "show hidden files").
/// UNC server root (`\\HOST` **without** a share) → host name. `None` for any
/// other path (local, or UNC with a share `\\HOST\share`). Pure string
/// analysis (no I/O). Used to enumerate a network machine's shares.
pub fn unc_server_root(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let rest = s.strip_prefix(r"\\").or_else(|| s.strip_prefix("//"))?;
    let rest = rest.trim_end_matches(['\\', '/']);
    if rest.is_empty() || rest.contains('\\') || rest.contains('/') {
        return None; // just "\\", or there's already a share
    }
    Some(rest.to_string())
}

/// Is a path listable by Ranger? A classic folder, OR (Windows) a UNC
/// server root whose shares we know how to enumerate. Allows navigating to
/// `\\HOST`, which `Path::is_dir()` refuses (it's not a folder in the FS sense).
///
pub fn is_listable(path: &Path) -> bool {
    path.is_dir() || (cfg!(windows) && unc_server_root(path).is_some())
}

/// "Parent" of a UNC SHARE ROOT: `\\HOST\share` → `\\HOST` (the
/// server root, navigable via share enumeration. `None` for any
/// other path — notably a share subfolder (`\\HOST\share\x`), for which
/// `Path::parent()` works normally. Fills the gap left by `Path::parent()`,
/// which returns `None` on a share root.
pub fn unc_share_parent(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    let mut comps = path.components();
    let Some(Component::Prefix(p)) = comps.next() else {
        return None;
    };
    let server = match p.kind() {
        Prefix::UNC(server, _) | Prefix::VerbatimUNC(server, _) => server.to_string_lossy(),
        _ => return None,
    };
    // Only the share root (nothing after prefix + root).
    if comps.any(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(PathBuf::from(format!(r"\\{server}")))
}

/// Network UNC path (`\\server…`)? Excludes **local** namespaces
/// `\\?\` (verbatim) and `\\.\` (device). Used to only offer the system
/// login on real network paths.
pub fn is_unc_path(path: &Path) -> bool {
    match path.to_str() {
        Some(s) => {
            (s.starts_with(r"\\") || s.starts_with("//"))
                && !s.starts_with(r"\\?\")
                && !s.starts_with(r"\\.\")
        }
        None => false,
    }
}

/// Lists the shares of a UNC server root as `Entry` items (folders) —
/// `\\HOST` becomes "browsable" like in Explorer. `Err(PermissionDenied)`
/// if authentication is required (→ GUI-side login prompt), `Err(NotConnected)`
/// if unreachable → "unavailable" banner.
#[cfg(windows)]
fn list_server_shares(server: &str) -> Result<(Vec<Entry>, usize)> {
    use crate::places::NetShares;
    match crate::places::net_shares(server) {
        NetShares::Ok(shares) => {
            let entries = shares
                .into_iter()
                .map(|sh| Entry {
                    path: PathBuf::from(format!(r"\\{server}\{sh}")),
                    name: sh,
                    size_bytes: None,
                    mtime_unix: None,
                    is_dir: true,
                    kind: FileKind::Folder,
                    hidden: false,
                    is_symlink: false,
                    executable: false,
                })
                .collect();
            Ok((entries, 0))
        }
        NetShares::AuthNeeded => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "network authentication required",
        )
        .into()),
        NetShares::Unreachable => Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "network server unreachable",
        )
        .into()),
    }
}

pub fn list_dir_counted(path: &Path, include_hidden: bool) -> Result<(Vec<Entry>, usize)> {
    // UNC server root (`\\HOST`): its SMB SHARES are enumerated (std::fs can
    // only list a single share, not a server). Shares are displayed as folders.
    #[cfg(windows)]
    {
        if let Some(server) = unc_server_root(path) {
            return list_server_shares(&server);
        }
    }
    let read = std::fs::read_dir(path)?;
    let mut out = Vec::with_capacity(64);
    let mut hidden_count = 0usize;
    for entry_res in read {
        let dir_entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "read_dir item failed");
                continue;
            }
        };

        let name = dir_entry.file_name().to_string_lossy().into_owned();
        // Dotfiles (Unix convention) — fast filter, no `stat`. The dotfile is
        // counted as hidden HERE, which preserves the original optimization
        // (the `stat` is skipped when hidden items aren't shown).
        let is_dotfile = name.starts_with('.');
        if is_dotfile {
            hidden_count += 1;
            if !include_hidden {
                continue;
            }
        }

        let full_path = dir_entry.path();

        let md_res = dir_entry.metadata();
        // HIDDEN attribute (Windows): read from the already-loaded `metadata`
        // (no extra `stat`). No-op on Linux/macOS.
        let hidden_attr = md_res.as_ref().map(is_hidden_attr).unwrap_or(false);
        // A non-dotfile hidden by attribute → counted here (`&& !is_dotfile`
        // avoids double-counting a dotfile that would ALSO have the hidden attribute).
        if hidden_attr && !is_dotfile {
            hidden_count += 1;
        }
        if !include_hidden && hidden_attr {
            continue;
        }
        // `hidden` = dotfile OR HIDDEN attribute → visual marker on the GUI side.
        let hidden = is_dotfile || hidden_attr;

        // `DirEntry::metadata()` does NOT follow symbolic links → a link to a
        // FOLDER would report `is_dir=false` and Ranger would open it in the
        // system file manager instead of navigating into it. The TARGET is
        // therefore resolved (via `fs::metadata`, which follows) ONLY for
        // links — the cost of an extra `stat` reserved for this rare case.
        // Broken link → target not found → treated as a non-folder.
        let is_symlink = dir_entry
            .file_type()
            .map(|t| t.is_symlink())
            .unwrap_or(false);
        let (is_dir, size_bytes, mtime_unix, executable) = match md_res {
            Ok(md) => {
                // On Unix, a link's permissions (often 0777) don't describe
                // those of its target. Reusing the followed metadata therefore
                // avoids marking every symlink as executable.
                let followed_md = is_symlink
                    .then(|| std::fs::metadata(&full_path).ok())
                    .flatten();
                let effective_md = followed_md.as_ref().unwrap_or(&md);
                let is_dir = effective_md.is_dir();
                let size = if is_dir { None } else { Some(md.len()) };
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|st| st.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    !is_dir
                        && (!is_symlink || followed_md.is_some())
                        && effective_md.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                (is_dir, size, mtime, executable)
            }
            Err(err) => {
                tracing::debug!(error = %err, path = %full_path.display(), "metadata failed");
                // The entry is kept but without size/mtime, assuming a file.
                (false, None, None, false)
            }
        };

        let ext = full_path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_owned());
        let kind = classify_kind(ext.as_deref(), is_dir);

        out.push(Entry {
            name,
            path: full_path,
            size_bytes,
            mtime_unix,
            is_dir,
            kind,
            hidden,
            is_symlink,
            executable,
        });
    }
    Ok((out, hidden_count))
}

// ----- Sorting ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortColumn {
    Name,
    Path,
    Size,
    Modified,
    /// Modification age — sorts on the same field as `Modified`
    /// (mtime), it's just the "age" column (rendered with a hot→cold color gradient).
    Age,
    /// File extension — sorts by type, then by name when extensions are equal.
    Ext,
}

impl SortColumn {
    pub fn code(self) -> &'static str {
        match self {
            SortColumn::Name => "name",
            SortColumn::Path => "path",
            SortColumn::Size => "size",
            SortColumn::Modified => "modified",
            SortColumn::Age => "age",
            SortColumn::Ext => "ext",
        }
    }

    pub fn from_code(s: &str) -> Option<SortColumn> {
        Some(match s {
            "name" => SortColumn::Name,
            "path" => SortColumn::Path,
            "size" => SortColumn::Size,
            "modified" => SortColumn::Modified,
            "age" => SortColumn::Age,
            "ext" => SortColumn::Ext,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    pub fn flip(self) -> Self {
        match self {
            SortOrder::Asc => SortOrder::Desc,
            SortOrder::Desc => SortOrder::Asc,
        }
    }
}

/// Grouping by type, orthogonal to the sort criterion (column + direction).
/// - `FoldersFirst` : folders on top, then files (default mode).
/// - `FilesFirst`   : files on top, then folders.
/// - `Mixed`        : no grouping, everything is sorted together by the criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupMode {
    FoldersFirst,
    FilesFirst,
    Mixed,
}

impl GroupMode {
    pub fn code(self) -> &'static str {
        match self {
            GroupMode::FoldersFirst => "folders",
            GroupMode::FilesFirst => "files",
            GroupMode::Mixed => "mixed",
        }
    }

    pub fn from_code(s: &str) -> Option<GroupMode> {
        Some(match s {
            "folders" => GroupMode::FoldersFirst,
            "files" => GroupMode::FilesFirst,
            "mixed" => GroupMode::Mixed,
            _ => return None,
        })
    }
}

/// Sorts in place according to the criterion (`column` + `order`) and the
/// grouping by type (`group`). Grouping takes **priority** over the criterion:
/// in `FoldersFirst`/`FilesFirst` mode, one group always precedes the other
/// regardless of sort direction; the criterion only breaks ties within a
/// group. In `Mixed` mode, folders and files are sorted together by the
/// criterion alone.
///
/// Sort by name: case-insensitive (consistent with the visual sort).
pub fn sort(entries: &mut [Entry], column: SortColumn, order: SortOrder, group: GroupMode) {
    use std::cmp::Reverse;
    let asc = matches!(order, SortOrder::Asc);
    // Grouping rank (folders vs files): it splits the list into two blocks the
    // criterion never crosses, and — unlike the criterion — it is NEVER reversed
    // by the direction (folders stay on top in FoldersFirst even when sorting
    // descending). `Mixed` puts everything in one block.
    let rank = |e: &Entry| -> u8 {
        match group {
            GroupMode::FoldersFirst => u8::from(!e.is_dir),
            GroupMode::FilesFirst => u8::from(e.is_dir),
            GroupMode::Mixed => 0,
        }
    };
    // Name/Ext sort case-insensitively. Lowercasing a name INSIDE a comparison
    // sort re-allocates it on every comparison (~N·log N times); caching the key
    // lowercases each name once per entry instead. Only the criterion is
    // reversed for a descending sort (via `Reverse`), never the grouping rank.
    match column {
        SortColumn::Name if asc => entries.sort_by_cached_key(|e| (rank(e), e.name.to_lowercase())),
        SortColumn::Name => {
            entries.sort_by_cached_key(|e| (rank(e), Reverse(e.name.to_lowercase())))
        }
        // Type sort: extension (lowercase, dotfiles included), then name so the
        // order stays stable within one type.
        SortColumn::Ext if asc => entries.sort_by_cached_key(|e| {
            (
                rank(e),
                ops::ext_of(&e.name).to_string(),
                e.name.to_lowercase(),
            )
        }),
        SortColumn::Ext => entries.sort_by_cached_key(|e| {
            (
                rank(e),
                Reverse((ops::ext_of(&e.name).to_string(), e.name.to_lowercase())),
            )
        }),
        // Size / date / path: these comparators allocate nothing, so a direct
        // comparison sort is already optimal — only the grouping is kept apart
        // from the (possibly reversed) criterion.
        SortColumn::Size | SortColumn::Modified | SortColumn::Age | SortColumn::Path => {
            entries.sort_by(|a, b| {
                rank(a).cmp(&rank(b)).then_with(|| {
                    let ord = match column {
                        SortColumn::Size => {
                            a.size_bytes.unwrap_or(0).cmp(&b.size_bytes.unwrap_or(0))
                        }
                        SortColumn::Modified | SortColumn::Age => {
                            a.mtime_unix.unwrap_or(0).cmp(&b.mtime_unix.unwrap_or(0))
                        }
                        _ => a.path.cmp(&b.path),
                    };
                    if asc { ord } else { ord.reverse() }
                })
            });
        }
    }
}

// ----- Formatting ----------------------------------------------------------

/// Size units depending on the language. French uses `o/Ko/Mo/Go/To`,
/// other languages use `B/KB/MB/GB/TB` (common convention).
pub fn size_units(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Fr => &["o", "Ko", "Mo", "Go", "To"],
        _ => &["B", "KB", "MB", "GB", "TB"],
    }
}

/// Formats a size in bytes with the most appropriate unit.
/// Base-1024 convention (binary), one decimal beyond KB.
pub fn format_size(bytes: u64, lang: Lang) -> String {
    let units = size_units(lang);
    if bytes < 1024 {
        return format!("{bytes} {}", units[0]);
    }
    let mut value = bytes as f64;
    let mut idx = 0usize;
    while value >= 1024.0 && idx + 1 < units.len() {
        value /= 1024.0;
        idx += 1;
    }
    // One decimal for intermediate values. The FR locale uses a comma.
    let s = format!("{value:.1}");
    let s = if matches!(lang, Lang::Fr) {
        s.replace('.', ",")
    } else {
        s
    };
    format!("{s} {}", units[idx])
}

/// A single bounded walk over `path` computing BOTH its recursive **max mtime**
/// (the most recent modification date among the folder and its descendants, up
/// to `mtime_depth` levels) AND its recursive **total size** (sum of descendant
/// files' bytes, up to `size_depth` levels). `depth = 1` = direct children, `2`
/// = grandchildren, …; a depth of `0` DISABLES that metric (its result is
/// `None`). Doing both in one traversal costs a single `stat` per entry instead
/// of two — the reason the two "folder date"/"folder size" options share it.
///
/// Symbolic links are NOT followed (`DirEntry::metadata`) → no loops or runaway
/// cost; only regular FILES add to the size. Errors (permission, broken link)
/// are ignored: a partial result beats none.
pub fn recursive_folder_stats(
    path: &Path,
    mtime_depth: u32,
    size_depth: u32,
) -> (Option<i64>, Option<u64>) {
    fn to_unix(md: &std::fs::Metadata) -> Option<i64> {
        md.modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    }
    fn walk(
        dir: &Path,
        mtime_left: u32,
        size_left: u32,
        max_mtime: &mut Option<i64>,
        total_size: &mut Option<u64>,
    ) {
        if mtime_left == 0 && size_left == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let Ok(md) = entry.metadata() else {
                continue; // does not follow symbolic links
            };
            if mtime_left > 0
                && let Some(m) = to_unix(&md)
                && max_mtime.is_none_or(|cur| m > cur)
            {
                *max_mtime = Some(m);
            }
            if size_left > 0
                && md.is_file()
                && let Some(total) = total_size.as_mut()
            {
                *total = total.saturating_add(md.len());
            }
            if md.is_dir() {
                walk(
                    &entry.path(),
                    mtime_left.saturating_sub(1),
                    size_left.saturating_sub(1),
                    max_mtime,
                    total_size,
                );
            }
        }
    }
    // Bases: the folder's own mtime; a directory has no intrinsic size, so the
    // size accumulator starts at 0 and only files add to it.
    let mut max_mtime = if mtime_depth > 0 {
        std::fs::metadata(path).ok().as_ref().and_then(to_unix)
    } else {
        None
    };
    let mut total_size = if size_depth > 0 { Some(0) } else { None };
    walk(
        path,
        mtime_depth,
        size_depth,
        &mut max_mtime,
        &mut total_size,
    );
    (max_mtime, total_size)
}

/// Recursive **max mtime** up to `depth` levels — the mtime-only case of
/// [`recursive_folder_stats`]. `depth = 0` = the folder's own mtime.
pub fn recursive_max_mtime(path: &Path, depth: u32) -> Option<i64> {
    // `recursive_folder_stats` treats an mtime depth of 0 as "disabled" (`None`),
    // but this function's historical contract is that depth 0 = the folder's own
    // mtime — handled directly here.
    if depth == 0 {
        return std::fs::metadata(path)
            .ok()
            .and_then(|md| md.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
    }
    recursive_folder_stats(path, depth, 0).0
}

/// ISO-like format, locale-independent: `YYYY-MM-DD HH:MM`. It stays
/// readable in every language without depending on `chrono`.
///
/// `offset_secs` shifts the Unix (UTC) timestamp before decomposition: `0` = UTC,
/// a local offset (set by the GUI) = local time. The timezone is a user
/// setting — the core stays agnostic by receiving the already-resolved offset.
pub fn format_mtime(unix: i64, offset_secs: i64) -> String {
    let unix = unix + offset_secs;
    let days_from_epoch = unix.div_euclid(86_400);
    let secs_in_day = unix.rem_euclid(86_400);
    let hh = (secs_in_day / 3600) as u32;
    let mm = ((secs_in_day % 3600) / 60) as u32;

    let (y, mo, d) = civil_from_days(days_from_epoch);
    format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}")
}

/// Compact units for the "age" column depending on the language:
/// `(minute, day, month, year, "just now")`. The hour uses `h` and
/// minutes the apostrophe `'` (compact and language-neutral, for example "2 h 50'").
fn age_units(
    lang: Lang,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    match lang {
        Lang::Fr => ("min", "j", "mois", "an", "à l'instant"),
        Lang::En => ("min", "d", "mo", "y", "just now"),
        Lang::Es => ("min", "d", "mes", "a", "ahora"),
        Lang::De => ("Min", "T", "Mon", "J", "gerade"),
        Lang::It => ("min", "g", "mes", "a", "ora"),
    }
}

/// Formats the **age** (now − mtime) compactly, readable at a glance:
/// `42 min`, `2 h 50'`, `25d`, `3mo`, `2y`. `now_unix` and
/// `mtime_unix` are Unix seconds.
pub fn format_age(mtime_unix: i64, now_unix: i64, lang: Lang) -> String {
    let secs = (now_unix - mtime_unix).max(0);
    let (min_u, day_u, month_u, year_u, now_w) = age_units(lang);
    if secs < 60 {
        return now_w.to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} {min_u}");
    }
    let hours = mins / 60;
    if hours < 24 {
        let m = mins % 60;
        return format!("{hours} h {m:02}'");
    }
    let days = hours / 24;
    if days < 31 {
        return format!("{days}{day_u}");
    }
    if days < 365 {
        let mo = days / 30;
        return format!("{mo}{month_u}");
    }
    let years = days / 365;
    format!("{years}{year_u}")
}

/// "Heat" index of the age, for the hot→cold color gradient of the
/// "age" column: `0` = very recent (hot) … `6` = old (cold).
pub fn age_bucket(mtime_unix: i64, now_unix: i64) -> i32 {
    let secs = (now_unix - mtime_unix).max(0);
    let hours = secs / 3600;
    let days = hours / 24;
    if hours < 1 {
        0
    } else if hours < 6 {
        1
    } else if days < 1 {
        2
    } else if days < 7 {
        3
    } else if days < 30 {
        4
    } else if days < 365 {
        5
    } else {
        6
    }
}

/// Converts days-since-1970 → (year, month, day) (Howard Hinnant's
/// algorithm, public domain). Avoids the dependency on `chrono` for this
/// minimal need.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_folder_overrides_extension() {
        assert_eq!(classify_kind(Some("zip"), true), FileKind::Folder);
    }

    #[test]
    fn unc_server_root_detects_bare_server_only() {
        use std::path::Path;
        // Server root (no share) → host name.
        assert_eq!(unc_server_root(Path::new(r"\\NAS")), Some("NAS".into()));
        assert_eq!(unc_server_root(Path::new(r"\\NAS\")), Some("NAS".into()));
        assert_eq!(
            unc_server_root(Path::new(r"\\192.168.1.50")),
            Some("192.168.1.50".into())
        );
        // With a share OR a local path OR just "\\" → not a server root.
        assert_eq!(unc_server_root(Path::new(r"\\NAS\media")), None);
        assert_eq!(unc_server_root(Path::new(r"\\NAS\media\sub")), None);
        assert_eq!(unc_server_root(Path::new(r"C:\Users")), None);
        assert_eq!(unc_server_root(Path::new(r"\\")), None);
    }

    #[cfg(windows)]
    #[test]
    fn unc_share_parent_only_at_share_root() {
        use std::path::Path;
        // Share root → server root.
        assert_eq!(
            unc_share_parent(Path::new(r"\\NAS\media")),
            Some(PathBuf::from(r"\\NAS"))
        );
        assert_eq!(
            unc_share_parent(Path::new(r"\\NAS\media\")),
            Some(PathBuf::from(r"\\NAS"))
        );
        // Share subfolder → None (Path::parent() is enough).
        assert_eq!(unc_share_parent(Path::new(r"\\NAS\media\sub")), None);
        // Local paths / server root: None.
        assert_eq!(unc_share_parent(Path::new(r"C:\Users")), None);
        assert_eq!(unc_share_parent(Path::new(r"\\NAS")), None);
    }

    #[test]
    fn is_unc_path_only_true_for_network_shares() {
        use std::path::Path;
        assert!(is_unc_path(Path::new(r"\\NAS\media")));
        assert!(is_unc_path(Path::new(r"\\NAS")));
        // LOCAL namespaces (verbatim / device) → not network.
        assert!(!is_unc_path(Path::new(r"\\?\C:\x")));
        assert!(!is_unc_path(Path::new(r"\\.\PhysicalDrive0")));
        assert!(!is_unc_path(Path::new(r"C:\Users")));
    }

    #[test]
    fn classify_known_extensions() {
        // Both lists are searched by bisection: unsorted, they would silently
        // stop matching some of their own entries.
        assert!(IMAGE_EXTENSIONS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(ARCHIVE_EXTENSIONS.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(classify_kind(Some("PNG"), false), FileKind::Image);
        for ext in ["af", "afdesign", "afphoto", "afpub"] {
            assert_eq!(classify_kind(Some(ext), false), FileKind::Image, "{ext}");
        }
        // Every archive extension still classifies as one after the move out of
        // the match arm into a bisected list.
        for ext in ARCHIVE_EXTENSIONS {
            assert_eq!(classify_kind(Some(ext), false), FileKind::Archive, "{ext}");
        }
        assert_eq!(classify_kind(Some("toml"), false), FileKind::Config);
        assert_eq!(classify_kind(Some("mp3"), false), FileKind::Audio);
        assert_eq!(classify_kind(Some("mp4"), false), FileKind::Video);
        assert_eq!(
            classify_kind(Some("AppImage"), false),
            FileKind::Application
        );
        assert_eq!(classify_kind(Some("zip"), false), FileKind::Archive);
        assert_eq!(classify_kind(Some("pdf"), false), FileKind::Document);
    }

    #[test]
    fn classify_unknown_extension_is_file() {
        assert_eq!(classify_kind(Some("xyz123"), false), FileKind::File);
        assert_eq!(classify_kind(None, false), FileKind::File);
    }

    #[test]
    fn only_programs_and_untyped_files_can_be_launched() {
        // Real candidates for "launch with these files".
        assert!(FileKind::Application.can_be_program()); // .sh, .appimage, binary
        assert!(FileKind::File.can_be_program()); // binary/script WITHOUT an extension
        // Data: never a program, EVEN with the executable bit (FAT/NTFS
        // 0777 mount) → fixes the incorrect "Open with this program" label
        // on an image/document/etc. on Linux.
        assert!(!FileKind::Image.can_be_program());
        assert!(!FileKind::Video.can_be_program());
        assert!(!FileKind::Audio.can_be_program());
        assert!(!FileKind::Document.can_be_program());
        assert!(!FileKind::Archive.can_be_program());
        assert!(!FileKind::Config.can_be_program());
        assert!(!FileKind::Folder.can_be_program());
    }

    #[test]
    fn format_age_buckets_and_text() {
        const H: i64 = 3600;
        const D: i64 = 86_400;
        let now = 1_000_000_000;
        // < 1 min → "à l'instant".
        assert_eq!(format_age(now - 30, now, Lang::Fr), "à l'instant");
        assert_eq!(age_bucket(now - 30, now), 0);
        // minutes.
        assert_eq!(format_age(now - 42 * 60, now, Lang::Fr), "42 min");
        // hours + minutes ("2 h 05'").
        assert_eq!(format_age(now - (2 * H + 5 * 60), now, Lang::Fr), "2 h 05'");
        // days.
        assert_eq!(format_age(now - 25 * D, now, Lang::Fr), "25j");
        assert_eq!(age_bucket(now - 25 * D, now), 4);
        // mtime in the future (clock) → clamped to 0 → "à l'instant".
        assert_eq!(format_age(now + 9999, now, Lang::Fr), "à l'instant");
    }

    #[test]
    fn age_bucket_is_monotonic_cold() {
        const D: i64 = 86_400;
        let now = 1_000_000_000;
        // The older it is, the bigger the bucket (colder).
        assert!(age_bucket(now - 30, now) < age_bucket(now - 10 * D, now));
        assert_eq!(age_bucket(now - 400 * D, now), 6);
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0, Lang::En), "0 B");
        assert_eq!(format_size(512, Lang::En), "512 B");
        assert_eq!(format_size(1024, Lang::En), "1.0 KB");
        assert_eq!(format_size(1536, Lang::En), "1.5 KB");
        assert_eq!(format_size(1024 * 1024, Lang::En), "1.0 MB");
    }

    #[test]
    fn format_size_fr_uses_o_and_comma() {
        assert_eq!(format_size(1024, Lang::Fr), "1,0 Ko");
        assert_eq!(format_size(1_500_000_000, Lang::Fr), "1,4 Go");
    }

    #[test]
    fn format_mtime_iso() {
        // Known boundaries (offset 0 = UTC):
        assert_eq!(format_mtime(0, 0), "1970-01-01 00:00");
        assert_eq!(format_mtime(86_400, 0), "1970-01-02 00:00");
        // 2000-01-01 00:00:00 UTC = 946_684_800
        assert_eq!(format_mtime(946_684_800, 0), "2000-01-01 00:00");
        // Format is always `YYYY-MM-DD HH:MM` (16 chars).
        assert_eq!(format_mtime(1_780_948_604, 0).len(), 16);
        // Local offset applied: +2 h → same clock shifted by 2 h.
        assert_eq!(format_mtime(0, 2 * 3600), "1970-01-01 02:00");
        // Negative offset crossing midnight.
        assert_eq!(format_mtime(0, -3600), "1969-12-31 23:00");
    }

    #[test]
    fn sort_folders_first_then_name_asc() {
        let mut v = vec![
            make_entry("zeta.txt", false),
            make_entry("alpha-dir", true),
            make_entry("Bravo.png", false),
            make_entry("aaa-dir", true),
        ];
        sort(
            &mut v,
            SortColumn::Name,
            SortOrder::Asc,
            GroupMode::FoldersFirst,
        );
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["aaa-dir", "alpha-dir", "Bravo.png", "zeta.txt"]);
    }

    #[test]
    fn sort_files_first_then_name_asc() {
        let mut v = vec![
            make_entry("zeta.txt", false),
            make_entry("alpha-dir", true),
            make_entry("Bravo.png", false),
            make_entry("aaa-dir", true),
        ];
        sort(
            &mut v,
            SortColumn::Name,
            SortOrder::Asc,
            GroupMode::FilesFirst,
        );
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        // Files (alpha) then folders (alpha).
        assert_eq!(names, ["Bravo.png", "zeta.txt", "aaa-dir", "alpha-dir"]);
    }

    #[test]
    fn sort_mixed_interleaves_by_name_asc() {
        let mut v = vec![
            make_entry("zeta.txt", false),
            make_entry("alpha-dir", true),
            make_entry("Bravo.png", false),
            make_entry("aaa-dir", true),
        ];
        sort(&mut v, SortColumn::Name, SortOrder::Asc, GroupMode::Mixed);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        // No grouping: everything is sorted together (case-insensitive).
        assert_eq!(names, ["aaa-dir", "alpha-dir", "Bravo.png", "zeta.txt"]);
    }

    #[test]
    fn sort_name_desc_keeps_folders_on_top() {
        let mut v = vec![
            make_entry("zeta.txt", false),
            make_entry("alpha-dir", true),
            make_entry("Bravo.png", false),
            make_entry("aaa-dir", true),
        ];
        sort(
            &mut v,
            SortColumn::Name,
            SortOrder::Desc,
            GroupMode::FoldersFirst,
        );
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        // Descending reverses the names WITHIN each block, but not the grouping:
        // folders stay on top.
        assert_eq!(names, ["alpha-dir", "aaa-dir", "zeta.txt", "Bravo.png"]);
    }

    #[test]
    fn sort_ext_groups_by_extension_then_name() {
        let mut v = vec![
            make_entry("b.txt", false),
            make_entry("a.txt", false),
            make_entry("C.png", false),
            make_entry("a.png", false),
        ];
        sort(&mut v, SortColumn::Ext, SortOrder::Asc, GroupMode::Mixed);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        // Grouped by extension (png before txt), then by name (case-insensitive).
        assert_eq!(names, ["a.png", "C.png", "a.txt", "b.txt"]);
    }

    #[test]
    fn sort_size_desc_keeps_folders_on_top() {
        let mut v = vec![
            sized_entry("big.bin", 10_000_000),
            sized_entry("small.bin", 10),
            make_entry("dir", true),
        ];
        sort(
            &mut v,
            SortColumn::Size,
            SortOrder::Desc,
            GroupMode::FoldersFirst,
        );
        assert!(v[0].is_dir, "folder must remain on top regardless of size");
        assert_eq!(v[1].name, "big.bin");
        assert_eq!(v[2].name, "small.bin");
    }

    #[test]
    fn list_dir_reads_temp() {
        let tmp = std::env::temp_dir().join(format!("ranger-fs-test-{}", nano()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join(".hidden"), b"x").unwrap();

        let mut entries = list_dir(&tmp, false).unwrap();
        sort(
            &mut entries,
            SortColumn::Name,
            SortOrder::Asc,
            GroupMode::FoldersFirst,
        );
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub", "a.txt"]);

        let entries_hidden = list_dir(&tmp, true).unwrap();
        assert_eq!(entries_hidden.len(), 3);

        // Hidden counter: returned regardless of `include_hidden`.
        let (visible, hidden_n) = list_dir_counted(&tmp, false).unwrap();
        assert_eq!(visible.len(), 2); // sub + a.txt
        assert_eq!(hidden_n, 1); // .hidden
        let (all, hidden_n2) = list_dir_counted(&tmp, true).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(hidden_n2, 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn list_dir_follows_symlink_to_dir() {
        // A symbolic link to a FOLDER must be classified `is_dir = true`
        // (otherwise Ranger would open it in the file manager instead of navigating).
        let tmp = std::env::temp_dir().join(format!("ranger-symlink-{}", nano()));
        std::fs::create_dir_all(tmp.join("realdir")).unwrap();
        let link = tmp.join("linkdir");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(tmp.join("realdir"), &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(tmp.join("realdir"), &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let made = false;
        // Link creation is often refused without privilege (Windows) → only
        // assert if the link was actually created (keeps the test non-flaky in restricted CI).
        if made {
            let (entries, _) = list_dir_counted(&tmp, false).unwrap();
            let e = entries
                .iter()
                .find(|e| e.name == "linkdir")
                .expect("linkdir listed");
            assert!(e.is_dir, "a link to a directory must have is_dir = true");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn list_dir_reports_target_executable_bits_including_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = std::env::temp_dir().join(format!("ranger-executable-{}", nano()));
        std::fs::create_dir_all(&tmp).unwrap();
        let executable = tmp.join("tool");
        let plain = tmp.join("plain");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        std::fs::write(&plain, b"text\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&executable, tmp.join("tool-link")).unwrap();
        symlink(&plain, tmp.join("plain-link")).unwrap();
        symlink(tmp.join("missing"), tmp.join("broken-link")).unwrap();

        let entries = list_dir(&tmp, false).unwrap();
        let executable_of = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.executable)
                .unwrap()
        };
        assert!(executable_of("tool"));
        assert!(executable_of("tool-link"));
        assert!(!executable_of("plain"));
        assert!(!executable_of("plain-link"));
        assert!(!executable_of("broken-link"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recursive_max_mtime_respects_depth() {
        use std::time::{Duration, UNIX_EPOCH};
        let tmp = std::env::temp_dir().join(format!("ranger-rmtime-{}", nano()));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("a.txt"), b"x").unwrap();
        let deep = tmp.join("sub").join("deep.txt");
        std::fs::write(&deep, b"y").unwrap();

        // Explicit FUTURE mtime on the deep file (level 2).
        let future: i64 = 4_000_000_000; // ~2096, > any "current" mtime
        std::fs::File::options()
            .write(true)
            .open(&deep)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(future as u64))
            .unwrap();

        // depth 2 reaches deep.txt; depth 1 (direct children) does not.
        assert_eq!(recursive_max_mtime(&tmp, 2), Some(future));
        let d1 = recursive_max_mtime(&tmp, 1).unwrap();
        assert!(d1 < future, "depth 1 must NOT reach level 2");
        // depth 0 = the folder's own mtime only (≤ depth 1).
        let d0 = recursive_max_mtime(&tmp, 0).unwrap();
        assert!(d0 <= d1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recursive_folder_stats_sums_size_by_depth() {
        let tmp = std::env::temp_dir().join(format!("ranger-fsize-{}", nano()));
        std::fs::create_dir_all(tmp.join("sub").join("deep")).unwrap();
        std::fs::write(tmp.join("a.txt"), [0u8; 10]).unwrap(); // level 1
        std::fs::write(tmp.join("sub").join("b.txt"), [0u8; 20]).unwrap(); // level 2
        std::fs::write(tmp.join("sub").join("deep").join("c.txt"), [0u8; 30]).unwrap(); // level 3

        // Size accumulates by depth; folders themselves add nothing.
        assert_eq!(recursive_folder_stats(&tmp, 0, 1).1, Some(10));
        assert_eq!(recursive_folder_stats(&tmp, 0, 2).1, Some(30));
        assert_eq!(recursive_folder_stats(&tmp, 0, 3).1, Some(60));
        // A depth of 0 disables that metric.
        assert_eq!(recursive_folder_stats(&tmp, 0, 0).1, None);
        assert_eq!(recursive_folder_stats(&tmp, 0, 2).0, None);
        // Unified: one walk returns both, and each half matches its single-metric call.
        let (mtime, size) = recursive_folder_stats(&tmp, 2, 2);
        assert!(mtime.is_some(), "mtime computed");
        assert_eq!(size, Some(30), "size at depth 2");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Benchmark for listing 10,000 entries with a 500 ms target. Creates the
    /// folder on the fly, measures `list_dir + sort`, prints the time, and
    /// checks the target. Ignored by default, since creating the files is slow:
    /// run with `cargo test -p ranger-core bench_list_10k -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_list_10k() {
        let tmp = std::env::temp_dir().join(format!("ranger-bench-{}", nano()));
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..10_000 {
            std::fs::write(tmp.join(format!("fichier_{i}.txt")), b"").unwrap();
        }

        let t0 = std::time::Instant::now();
        let mut entries = list_dir(&tmp, false).unwrap();
        let listed = t0.elapsed();
        let t1 = std::time::Instant::now();
        sort(
            &mut entries,
            SortColumn::Name,
            SortOrder::Asc,
            GroupMode::FoldersFirst,
        );
        let sorted = t1.elapsed();
        let total = t0.elapsed();

        println!(
            "bench_list_10k: {} entries | list_dir={:?} sort={:?} total={:?}",
            entries.len(),
            listed,
            sorted,
            total
        );
        assert_eq!(entries.len(), 10_000);
        assert!(
            total.as_millis() < 500,
            "listing 10k should be < 500ms, measured: {total:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn make_entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            size_bytes: if is_dir { None } else { Some(0) },
            mtime_unix: None,
            is_dir,
            kind: if is_dir {
                FileKind::Folder
            } else {
                FileKind::File
            },
            hidden: false,
            is_symlink: false,
            executable: false,
        }
    }

    fn sized_entry(name: &str, size: u64) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            size_bytes: Some(size),
            mtime_unix: None,
            is_dir: false,
            kind: FileKind::File,
            hidden: false,
            is_symlink: false,
            executable: false,
        }
    }

    fn nano() -> u64 {
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}
