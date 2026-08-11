//! **Native Windows** thumbnails for a **standalone** Ranger
//! (no external binary: no ffmpeg, no poppler, no console window).
//!
//! - **Audio / video**: shell thumbnail API (`IShellItemImageFactory::GetImage`)
//!   provides album art or a representative frame through Windows providers.
//! - **PDF**: PDF engine **built into Windows** via WinRT (`Windows.Data.Pdf`) —
//!   renders the 1st page in memory, independent of any installed PDF reader.
//!
//! The calling thread (thumbnail worker) is initialized in **MTA**: required
//! to block on WinRT `IAsyncOperation` (`.join()`) without a message pump,
//! and compatible with the shell API.

use std::path::Path;

use ranger_core::thumbnail::{Thumbnail, from_encoded};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_RESIZETOFIT,
    SIIGBF_THUMBNAILONLY,
};
use windows::core::{HSTRING, PCWSTR};

use crate::winutil::{hbitmap_to_rgba, wide};

/// Initializes COM in **MTA** on the current thread. Idempotent (returns
/// `S_FALSE` if already initialized in the same mode).
unsafe fn ensure_com_mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// **Video** thumbnail (and any type handled by the shell) via
/// `IShellItemImageFactory`. `None` if the file doesn't exist or no provider
/// responds.
pub fn shell_thumbnail(path: &Path, max_px: u32) -> Option<Thumbnail> {
    if !path.is_file() {
        return None;
    }
    let wpath = wide(&path.to_string_lossy());
    unsafe {
        ensure_com_mta();
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wpath.as_ptr()), None).ok()?;
        let size = SIZE {
            cx: max_px as i32,
            cy: max_px as i32,
        };
        // THUMBNAILONLY: a REAL thumbnail (video frame), not the type icon;
        // RESIZETOFIT (=0): fits within max_px while keeping the aspect ratio.
        let flags = SIIGBF(SIIGBF_RESIZETOFIT.0 | SIIGBF_THUMBNAILONLY.0);
        let hbitmap = factory.GetImage(size, flags).ok()?;
        let out = hbitmap_to_rgba(hbitmap).map(|(rgba, width, height)| Thumbnail {
            width,
            height,
            rgba,
        });
        let _ = DeleteObject(HGDIOBJ(hbitmap.0));
        out
    }
}

/// **PDF** thumbnail via the PDF engine built into Windows (WinRT
/// `Windows.Data.Pdf`) — renders the 1st page in memory (PNG/BMP) then decodes
/// it (`image`). Standalone: does not depend on any PDF reader or external
/// binary. `None` on failure.
pub fn pdf_thumbnail(path: &Path, max_px: u32) -> Option<Thumbnail> {
    if !path.is_file() {
        return None;
    }
    unsafe {
        ensure_com_mta();
        let s = path.to_string_lossy();
        let hpath = HSTRING::from(s.as_ref());
        let file = StorageFile::GetFileFromPathAsync(&hpath)
            .ok()?
            .join()
            .ok()?;
        let doc = PdfDocument::LoadFromFileAsync(&file).ok()?.join().ok()?;
        if doc.PageCount().ok()? == 0 {
            return None;
        }
        let page = doc.GetPage(0).ok()?;
        let stream = InMemoryRandomAccessStream::new().ok()?;
        let opts = PdfPageRenderOptions::new().ok()?;
        let _ = opts.SetDestinationWidth(max_px);
        page.RenderWithOptionsToStreamAsync(&stream, &opts)
            .ok()?
            .join()
            .ok()?;
        // Reads the stream's bytes (encoded image) → decoded via `image` (core).
        let size = stream.Size().ok()?;
        if size == 0 {
            return None;
        }
        let input = stream.GetInputStreamAt(0).ok()?;
        let reader = DataReader::CreateDataReader(&input).ok()?;
        reader.LoadAsync(size as u32).ok()?.join().ok()?;
        let mut buf = vec![0u8; size as usize];
        reader.ReadBytes(&mut buf).ok()?;
        from_encoded(&buf, max_px)
    }
}
