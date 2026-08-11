//! Small shared Windows utilities (avoids duplication between `openwith`,
//! `winthumb`, `clipboard`) — keeps the implementations cohesive.

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW,
    HBITMAP, HGDIOBJ, ReleaseDC,
};

/// NUL-terminated wide (UTF-16) string for the Win32 `*W` APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// RGBA pixels `(buf, w, h)` of a 32 bpp `HBITMAP` via GDI (`GetObjectW` +
/// `GetDIBits`), converted BGRA→RGBA, alpha forced opaque if the bitmap has no
/// alpha channel (old icons). `None` if dimensions are zero / extraction
/// failed. **Does NOT free** the `HBITMAP` (caller's responsibility).
///
/// # Safety
/// `hbitmap` must be a valid GDI bitmap handle.
pub unsafe fn hbitmap_to_rgba(hbitmap: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let mut bm = BITMAP::default();
        GetObjectW(
            HGDIOBJ(hbitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        );
        let (w, h) = (bm.bmWidth.max(0) as u32, bm.bmHeight.max(0) as u32);
        if w == 0 || h == 0 {
            return None;
        }
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w as i32;
        bi.bmiHeader.biHeight = -(h as i32); // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(None);
        let lines = GetDIBits(
            hdc,
            hbitmap,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        if lines == 0 {
            return None;
        }
        // BGRA → RGBA.
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        // Bitmap with no alpha channel → force opaque (otherwise fully transparent).
        if !buf.chunks_exact(4).any(|px| px[3] != 0) {
            for px in buf.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        Some((buf, w, h))
    }
}
