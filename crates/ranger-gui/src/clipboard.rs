//! **File** clipboard interoperable with the native file manager.
//!
//! Ranger's file copy/paste used to rely on internal state (paths kept in
//! memory) → no exchange with desktop file managers. We now read/write each
//! platform's native formats here:
//!
//! - **Windows**: `CF_HDROP` + "Preferred DropEffect" via the `windows` crate.
//! - **Linux Wayland**: `text/uri-list` + KDE/GNOME markers via
//!   `wl-clipboard-rs` (no X11 path).
//!
//! The internal clipboard remains the fallback when the system backend is
//! unavailable, for example outside a Wayland session.

use std::path::PathBuf;

use anyhow::Result;

/// Writes the list of `paths` files to the OS clipboard.
/// `cut = true` marks a move in the platform's own format.
pub fn write_files(paths: &[PathBuf], cut: bool) -> Result<()> {
    imp::write_files(paths, cut)
}

/// Reads a list of files from the OS clipboard, along with the cut/copy flag.
/// `None` if the clipboard doesn't contain any files.
pub fn read_files() -> Option<(Vec<PathBuf>, bool /*cut*/)> {
    imp::read_files()
}

// The URI conversions are independent of Wayland so they can also be tested
// on Windows. On Linux they preserve non-UTF-8 names by directly encoding the
// bytes of the `OsStr`.
#[cfg(any(target_os = "linux", test))]
mod uri_list {
    fn hex(byte: u8) -> char {
        match byte {
            0..=9 => (b'0' + byte) as char,
            _ => (b'A' + byte - 10) as char,
        }
    }

    fn unhex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    pub fn encode_file_uri(path: &[u8]) -> String {
        let mut uri = String::with_capacity(path.len() + 7);
        uri.push_str("file://");
        for &byte in path {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
                uri.push(byte as char);
            } else {
                uri.push('%');
                uri.push(hex(byte >> 4));
                uri.push(hex(byte & 0x0f));
            }
        }
        uri
    }

    fn percent_decode(encoded: &[u8]) -> Option<Vec<u8>> {
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut index = 0;
        while index < encoded.len() {
            if encoded[index] == b'%' {
                let high = unhex(*encoded.get(index + 1)?)?;
                let low = unhex(*encoded.get(index + 2)?)?;
                decoded.push((high << 4) | low);
                index += 3;
            } else {
                decoded.push(encoded[index]);
                index += 1;
            }
        }
        (!decoded.contains(&0)).then_some(decoded)
    }

    fn strip_local_authority(uri_path: &[u8]) -> Option<&[u8]> {
        let Some(authority_and_path) = uri_path.strip_prefix(b"//") else {
            return Some(uri_path);
        };
        if authority_and_path.starts_with(b"/") {
            return Some(authority_and_path);
        }
        const LOCALHOST: &[u8] = b"localhost";
        if authority_and_path.len() >= LOCALHOST.len()
            && authority_and_path[..LOCALHOST.len()].eq_ignore_ascii_case(LOCALHOST)
            && authority_and_path.get(LOCALHOST.len()) == Some(&b'/')
        {
            return Some(&authority_and_path[LOCALHOST.len()..]);
        }
        // Ranger doesn't know how to copy a remote KIO resource
        // (`file://host`) as a local path: it's ignored rather than
        // misinterpreted.
        None
    }

    pub fn decode_file_uri(uri: &[u8]) -> Option<Vec<u8>> {
        let path = strip_local_authority(uri.strip_prefix(b"file:")?)?;
        if !path.starts_with(b"/") {
            return None;
        }
        percent_decode(path)
    }

    pub fn parse_uri_list(data: &[u8]) -> Vec<Vec<u8>> {
        data.split(|&byte| byte == b'\n')
            .filter_map(|mut line| {
                // Some legacy managers terminate the payload with NUL;
                // `text/uri-list` normally uses CRLF.
                while line.last().is_some_and(|byte| *byte == b'\r' || *byte == 0) {
                    line = &line[..line.len() - 1];
                }
                if line.is_empty() || line.starts_with(b"#") {
                    None
                } else {
                    decode_file_uri(line)
                }
            })
            .collect()
    }

    pub fn parse_gnome_copied_files(data: &[u8]) -> Option<(Vec<Vec<u8>>, bool)> {
        let split = data.iter().position(|&byte| byte == b'\n')?;
        let action = data[..split].strip_suffix(b"\r").unwrap_or(&data[..split]);
        let cut = match action {
            b"copy" => false,
            b"cut" => true,
            _ => return None,
        };
        Some((parse_uri_list(&data[split + 1..]), cut))
    }

    pub fn parse_kde_cut_selection(data: &[u8]) -> Option<bool> {
        let start = data.iter().position(|byte| !byte.is_ascii_whitespace())?;
        let end = data.iter().rposition(|byte| !byte.is_ascii_whitespace())? + 1;
        match &data[start..end] {
            b"0" => Some(false),
            b"1" => Some(true),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn file_uri_round_trip_preserves_special_and_non_utf8_bytes() {
            let path = b"/tmp/Manual outlook #1%\xFF.png";
            let uri = encode_file_uri(path);
            assert_eq!(uri, "file:///tmp/Manual%20outlook%20%231%25%FF.png");
            assert_eq!(decode_file_uri(uri.as_bytes()).as_deref(), Some(&path[..]));
        }

        #[test]
        fn uri_list_accepts_comments_crlf_and_localhost() {
            let payload = b"# generated by file manager\r\nfile:///tmp/one%20file\r\nfile://LOCALHOST/tmp/two\0\r\n";
            assert_eq!(
                parse_uri_list(payload),
                vec![b"/tmp/one file".to_vec(), b"/tmp/two".to_vec()]
            );
            assert!(decode_file_uri(b"file://remote-host/tmp/file").is_none());
        }

        #[test]
        fn desktop_cut_markers_are_parsed() {
            let (paths, cut) =
                parse_gnome_copied_files(b"cut\nfile:///tmp/one\nfile:///tmp/two\n").unwrap();
            assert!(cut);
            assert_eq!(paths.len(), 2);
            assert_eq!(parse_kde_cut_selection(b" 1\n"), Some(true));
            assert_eq!(parse_kde_cut_selection(b"0"), Some(false));
            assert_eq!(parse_kde_cut_selection(b"move"), None);
        }
    }
}

// ===================== Windows =====================
#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, POINT};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP};
    use windows::core::PCWSTR;

    use crate::winutil::wide;

    /// Id of the "Preferred DropEffect" format (registered on first request,
    /// stable for the session). `0` = failure.
    unsafe fn effect_format() -> u32 {
        unsafe {
            let name = wide("Preferred DropEffect");
            RegisterClipboardFormatW(PCWSTR(name.as_ptr()))
        }
    }

    pub fn write_files(paths: &[PathBuf], cut: bool) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        unsafe {
            // ----- DROPFILES blob: header + wide paths, final double-null -----
            let mut names: Vec<u16> = Vec::new();
            for p in paths {
                names.extend(p.as_os_str().encode_wide());
                names.push(0);
            }
            names.push(0); // list terminator
            let header = std::mem::size_of::<DROPFILES>();
            let total = header + names.len() * std::mem::size_of::<u16>();

            let hmem = GlobalAlloc(GMEM_MOVEABLE, total)
                .map_err(|e| anyhow::anyhow!("GlobalAlloc: {e}"))?;
            let base = GlobalLock(hmem) as *mut u8;
            if base.is_null() {
                let _ = GlobalFree(Some(hmem));
                return Err(anyhow::anyhow!("GlobalLock failed"));
            }
            std::ptr::write(
                base as *mut DROPFILES,
                DROPFILES {
                    pFiles: header as u32,
                    pt: POINT { x: 0, y: 0 },
                    fNC: false.into(),
                    fWide: true.into(),
                },
            );
            std::ptr::copy_nonoverlapping(
                names.as_ptr(),
                base.add(header) as *mut u16,
                names.len(),
            );
            let _ = GlobalUnlock(hmem); // returns Err(NO_ERROR) on full unlock → ignored

            // ----- Preferred DropEffect (DWORD): 1 = copy, 2 = cut -----
            let effect: u32 = if cut { 2 } else { 1 };
            let heffect = match GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>()) {
                Ok(h) => h,
                Err(e) => {
                    let _ = GlobalFree(Some(hmem));
                    return Err(anyhow::anyhow!("GlobalAlloc effect: {e}"));
                }
            };
            let eptr = GlobalLock(heffect) as *mut u32;
            if eptr.is_null() {
                let _ = GlobalFree(Some(hmem));
                let _ = GlobalFree(Some(heffect));
                return Err(anyhow::anyhow!("GlobalLock effect failed"));
            }
            *eptr = effect;
            let _ = GlobalUnlock(heffect);
            let cf_effect = effect_format();

            if let Err(e) = OpenClipboard(None) {
                let _ = GlobalFree(Some(hmem));
                let _ = GlobalFree(Some(heffect));
                return Err(anyhow::anyhow!("OpenClipboard: {e}"));
            }
            // Once `SetClipboardData` succeeds, the system OWNS that HGLOBAL and
            // it must not be freed. Track ownership so the failure paths free
            // only what the system did NOT take — and so the effect handle is
            // released when its clipboard format is unavailable (never set).
            let mut hmem_owned = false;
            let mut heffect_owned = false;
            let res = (|| -> Result<()> {
                EmptyClipboard().map_err(|e| anyhow::anyhow!("EmptyClipboard: {e}"))?;
                SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hmem.0)))
                    .map_err(|e| anyhow::anyhow!("SetClipboardData(CF_HDROP): {e}"))?;
                hmem_owned = true;
                if cf_effect != 0 {
                    SetClipboardData(cf_effect, Some(HANDLE(heffect.0)))
                        .map_err(|e| anyhow::anyhow!("SetClipboardData(effect): {e}"))?;
                    heffect_owned = true;
                }
                Ok(())
            })();
            let _ = CloseClipboard();
            if !hmem_owned {
                let _ = GlobalFree(Some(hmem));
            }
            if !heffect_owned {
                let _ = GlobalFree(Some(heffect));
            }
            res
        }
    }

    pub fn read_files() -> Option<(Vec<PathBuf>, bool)> {
        unsafe {
            // No files on the clipboard → nothing (fall back to internal).
            if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() {
                return None;
            }
            OpenClipboard(None).ok()?;
            let out = (|| -> Option<(Vec<PathBuf>, bool)> {
                let handle = GetClipboardData(CF_HDROP.0 as u32).ok()?;
                let hdrop = HDROP(handle.0);
                // ifile = 0xFFFFFFFF + null buffer → number of files.
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
                let mut files = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None); // length excluding null
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len as usize + 1];
                    let n = DragQueryFileW(hdrop, i, Some(buf.as_mut_slice()));
                    if n == 0 {
                        continue;
                    }
                    files.push(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])));
                }

                // Preferred DropEffect (DROPEFFECT_MOVE = 2 → cut).
                let mut cut = false;
                let cf_effect = effect_format();
                if cf_effect != 0
                    && IsClipboardFormatAvailable(cf_effect).is_ok()
                    && let Ok(he) = GetClipboardData(cf_effect)
                {
                    let p = GlobalLock(HGLOBAL(he.0)) as *const u32;
                    if !p.is_null() {
                        cut = (*p) & 2 != 0;
                        let _ = GlobalUnlock(HGLOBAL(he.0));
                    }
                }

                if files.is_empty() {
                    None
                } else {
                    Some((files, cut))
                }
            })();
            let _ = CloseClipboard();
            out
        }
    }
}

// ===================== Linux Wayland =====================
#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::ffi::OsString;
    use std::io::Read;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    use wl_clipboard_rs::copy::{
        ClipboardType as CopyClipboard, MimeSource, MimeType as CopyMime, Options, Source,
    };
    use wl_clipboard_rs::paste::{
        self, ClipboardType as PasteClipboard, MimeType as PasteMime, Seat,
    };

    use super::uri_list;

    const URI_LIST: &str = "text/uri-list";
    /// Plain text: without it, Ctrl+C on a file drops NO text into the
    /// clipboard, and Ctrl+V in a text field (Ranger's Rename popup, an
    /// editor, a terminal…) pastes nothing. Native managers (Dolphin,
    /// Nautilus) publish the path; we do the same.
    const TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
    const TEXT_PLAIN: &str = "text/plain";
    const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
    const KDE_CUT_SELECTION: &str = "application/x-kde-cutselection";

    fn mime_source(mime: &str, data: Vec<u8>) -> MimeSource {
        MimeSource {
            source: Source::Bytes(data.into_boxed_slice()),
            mime_type: CopyMime::Specific(mime.to_owned()),
        }
    }

    fn read_mime(mime: &str) -> Option<Vec<u8>> {
        let (mut pipe, _) = paste::get_contents(
            PasteClipboard::Regular,
            Seat::Unspecified,
            PasteMime::Specific(mime),
        )
        .ok()?;
        let mut data = Vec::new();
        pipe.read_to_end(&mut data).ok()?;
        Some(data)
    }

    fn into_paths(raw: Vec<Vec<u8>>) -> Vec<PathBuf> {
        raw.into_iter()
            .map(|path| PathBuf::from(OsString::from_vec(path)))
            .collect()
    }

    pub fn write_files(paths: &[PathBuf], cut: bool) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let uris = paths
            .iter()
            .map(|path| {
                if !path.is_absolute() {
                    return Err(anyhow::anyhow!(
                        "the Wayland clipboard requires an absolute path: {}",
                        path.display()
                    ));
                }
                Ok(uri_list::encode_file_uri(path.as_os_str().as_bytes()))
            })
            .collect::<Result<Vec<_>>>()?;

        // `text/uri-list` is the common XDG/KDE format. The two additional
        // sources carry the cut/copy intent expected by Dolphin and GNOME
        // file managers.
        let uri_payload = format!("{}\r\n", uris.join("\r\n")).into_bytes();
        let gnome_payload =
            format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n")).into_bytes();
        let kde_payload = vec![if cut { b'1' } else { b'0' }];
        // Plain paths, one per line (convention of native file managers):
        // text applications receive something useful, while file-aware ones
        // still prefer `text/uri-list`.
        let text_payload = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();

        let mut options = Options::new();
        options.clipboard(CopyClipboard::Regular);
        options
            .copy_multi(vec![
                mime_source(URI_LIST, uri_payload),
                mime_source(GNOME_COPIED_FILES, gnome_payload),
                mime_source(KDE_CUT_SELECTION, kde_payload),
                mime_source(TEXT_PLAIN_UTF8, text_payload.clone()),
                mime_source(TEXT_PLAIN, text_payload),
            ])
            .map_err(|err| anyhow::anyhow!("Wayland clipboard: {err}"))
    }

    pub fn read_files() -> Option<(Vec<PathBuf>, bool)> {
        let offered = paste::get_mime_types(PasteClipboard::Regular, Seat::Unspecified).ok()?;

        if offered.contains(GNOME_COPIED_FILES)
            && let Some((raw, cut)) = read_mime(GNOME_COPIED_FILES)
                .as_deref()
                .and_then(uri_list::parse_gnome_copied_files)
        {
            let paths = into_paths(raw);
            if !paths.is_empty() {
                return Some((paths, cut));
            }
        }

        if !offered.contains(URI_LIST) {
            return None;
        }
        let paths = into_paths(uri_list::parse_uri_list(&read_mime(URI_LIST)?));
        if paths.is_empty() {
            return None;
        }
        let cut = if offered.contains(KDE_CUT_SELECTION) {
            read_mime(KDE_CUT_SELECTION)
                .as_deref()
                .and_then(uri_list::parse_kde_cut_selection)
                .unwrap_or(false)
        } else {
            false
        };
        Some((paths, cut))
    }
}

// ===================== Other platforms =====================
#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::*;

    pub fn write_files(_paths: &[PathBuf], _cut: bool) -> Result<()> {
        Ok(())
    }

    pub fn read_files() -> Option<(Vec<PathBuf>, bool)> {
        None
    }
}
