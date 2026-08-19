//! On-the-fly preview (thumbnail) generation.
//!
//! - **Images**: **100% Rust** decoding via the `image` crate (no C, no
//!   network). PSD files get an additional best-effort path that extracts
//!   their embedded Photoshop JPEG thumbnail without rendering layers.
//!   Affinity files similarly expose a bounded embedded PNG without decoding
//!   the proprietary document. Other formats that can't be decoded (RAW,
//!   sometimes HEIF/AVIF…) → `None` → the caller falls back to the type icon.
//! - **MP3 / FLAC**: extracts embedded ID3v2 `APIC`/`PIC` or FLAC `PICTURE`
//!   cover art and decodes it with the existing image pipeline. Only bounded
//!   metadata is read; the audio stream itself is never decoded.
//! - **Videos**: frame extraction via the **`ffmpeg` CLI** in a **single
//!   process** (best-effort, same spirit as `gio trash` / `xdg-open`). The
//!   `thumbnail` filter picks a **representative** frame (avoids the black
//!   screen at the start, without a separate `ffprobe` probe) and `scale`
//!   bounds the largest side **inside** ffmpeg → no full-resolution PNG
//!   re-decoded on the Rust side. Falls back to the icon if `ffmpeg` is
//!   absent or fails. On Windows, the process is launched without a console
//!   window (`no_console`).
//! - **PDF**: renders the 1st page via the **`pdftoppm`** then
//!   **`pdftocairo`** CLIs (poppler-utils, same spirit as `ffmpeg` —
//!   best-effort, no dependency).
//! - **SVG**: NOT here — rendered natively by Slint on the GUI side (see
//!   `bridge`), because `image` doesn't decode SVG and Slint already embeds
//!   an SVG engine (resvg).
//!
//! **Windows (standalone)**: the GUI does NOT call these CLIs for video/PDF;
//! it uses the **shell's thumbnail API** (`IShellItemImageFactory`, see
//! `ranger-gui/src/winthumb.rs`) — OS-provided, no external binary or
//! console window. The CLIs above are therefore only used on Linux.
//!
//! **No disk cache**: these functions are pure (path in → pixels out).
//! Memoization (in-memory LRU cache, session only) is handled by the calling
//! layer (GUI).

use std::path::Path;
use std::process::Command;

use crate::fs::FileKind;

/// On Windows, prevents a **console window** from opening (a "DOS" flash)
/// when launching a CLI (ffmpeg/poppler) from a GUI app. No-op elsewhere.
fn no_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Decoded thumbnail ready for display: interleaved RGBA8 (`rgba.len() ==
/// width * height * 4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Generates a thumbnail for `path` based on its type. `max_px` bounds the
/// largest side (aspect is preserved). Returns `None` if the type has no
/// preview or if generation fails (→ icon fallback on the caller's side).
pub fn generate(path: &Path, kind: FileKind, max_px: u32) -> Option<Thumbnail> {
    match kind {
        FileKind::Image => from_image(path, max_px),
        FileKind::Audio => from_audio(path, max_px),
        FileKind::Video => from_video(path, max_px),
        // PDFs are classified as `Document`; `from_pdf` filters on the extension
        // (other documents — doc/txt/… — have no preview).
        FileKind::Document => from_pdf(path, max_px),
        _ => None,
    }
}

/// Metadata of an image read from the HEADER only (no pixel decoding):
/// `(width, height, color bits per pixel, alpha present)`. When an
/// alpha channel exists, its depth is subtracted from the third element: an
/// RGBA8 image therefore returns `(width, height, 24, true)`. `None` if the
/// format isn't handled by `image` or the file is unreadable. Fast
/// (a few KB read) → suited to filling columns on the fly.
pub fn image_meta(path: &Path) -> Option<(u32, u32, u16, bool)> {
    use image::ImageDecoder;
    let decoder = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let (w, h) = decoder.dimensions();
    let color = decoder.color_type();
    let has_alpha = color.has_alpha();
    let total_bits = color.bits_per_pixel();
    // The ColorType variants exposed by `image` have channels of equal depth
    // (RGBA8, RGBA16, RGBA32F, LA8, LA16). Removing one channel therefore gives
    // the correct color depth, without counting alpha twice in the display.
    let color_bits = if has_alpha {
        total_bits - total_bits / u16::from(color.channel_count())
    } else {
        total_bits
    };
    Some((w, h, color_bits, has_alpha))
}

/// Decodes ENCODED image bytes (PNG/BMP/JPEG…) into a thumbnail bounded to
/// `max_px`. Used for renders produced outside `image` — e.g. a PDF page rendered
/// by the WinRT API on Windows (cf. `ranger-gui/src/winthumb.rs`).
pub fn from_encoded(bytes: &[u8], max_px: u32) -> Option<Thumbnail> {
    let img = image::load_from_memory(bytes).ok()?;
    Some(downscale(img, max_px))
}

/// Thumbnail of an image file (pure-Rust decoding). `None` if the format
/// isn't supported / the file is unreadable.
pub fn from_image(path: &Path, max_px: u32) -> Option<Thumbnail> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("psd") {
        return from_psd_thumbnail(path, max_px);
    }
    if is_affinity_extension(extension) {
        return from_affinity_thumbnail(path, max_px);
    }

    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    Some(downscale(img, max_px))
}

const PSD_THUMBNAIL_RESOURCE_V4: u16 = 1033;
const PSD_THUMBNAIL_RESOURCE_V5: u16 = 1036;
const PSD_THUMBNAIL_HEADER_BYTES: u64 = 28;
const MAX_PSD_THUMBNAIL_BYTES: usize = 16 * 1024 * 1024;
const MAX_PSD_RESOURCE_BLOCKS: usize = 4096;

/// Extracts Photoshop's optional embedded JPEG thumbnail from a PSD resource
/// section. This deliberately does not attempt to render layers or decode the
/// full composite image: both require substantially broader color-mode and
/// compression support. Missing or malformed resources therefore remain a
/// normal icon fallback.
fn from_psd_thumbnail(path: &Path, max_px: u32) -> Option<Thumbnail> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();

    let mut header = [0u8; 26];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"8BPS"
        || header[4..6] != 1u16.to_be_bytes()
        || header[6..12].iter().any(|byte| *byte != 0)
    {
        return None;
    }

    let color_data_len = u64::from(read_be_u32(&mut file)?);
    let resources_len_position = file.stream_position().ok()?.checked_add(color_data_len)?;
    if resources_len_position.checked_add(4)? > file_len {
        return None;
    }
    file.seek(SeekFrom::Start(resources_len_position)).ok()?;

    let resources_len = u64::from(read_be_u32(&mut file)?);
    let resources_start = file.stream_position().ok()?;
    let resources_end = resources_start.checked_add(resources_len)?;
    if resources_end > file_len {
        return None;
    }

    let mut fallback = None;
    let mut blocks_checked = 0usize;
    loop {
        let block_start = file.stream_position().ok()?;
        if block_start == resources_end {
            break;
        }
        if blocks_checked >= MAX_PSD_RESOURCE_BLOCKS {
            return fallback;
        }
        blocks_checked += 1;

        // Signature, resource ID, shortest padded Pascal name and data size.
        // Check the declared section before reading so trailing PSD sections
        // can never be mistaken for a truncated resource header.
        if resources_end.checked_sub(block_start)? < 12 {
            return fallback;
        }

        let mut signature = [0u8; 4];
        file.read_exact(&mut signature).ok()?;
        if &signature != b"8BIM" {
            return fallback;
        }
        let resource_id = read_be_u16(&mut file)?;

        let mut name_len = [0u8; 1];
        file.read_exact(&mut name_len).ok()?;
        let pascal_bytes = 1u64.checked_add(u64::from(name_len[0]))?;
        let padded_pascal_bytes = pascal_bytes.checked_add(pascal_bytes & 1)?;
        let after_name = file
            .stream_position()
            .ok()?
            .checked_add(padded_pascal_bytes.checked_sub(1)?)?;
        if after_name.checked_add(4)? > resources_end {
            return fallback;
        }
        file.seek(SeekFrom::Start(after_name)).ok()?;

        let resource_len = u64::from(read_be_u32(&mut file)?);
        let resource_start = file.stream_position().ok()?;
        let resource_end = resource_start.checked_add(resource_len)?;
        let padded_resource_end = resource_end.checked_add(resource_len & 1)?;
        if padded_resource_end > resources_end {
            return fallback;
        }

        if matches!(
            resource_id,
            PSD_THUMBNAIL_RESOURCE_V4 | PSD_THUMBNAIL_RESOURCE_V5
        ) && resource_len >= PSD_THUMBNAIL_HEADER_BYTES
        {
            let mut thumbnail_header = [0u8; PSD_THUMBNAIL_HEADER_BYTES as usize];
            file.read_exact(&mut thumbnail_header).ok()?;
            if let Some(jpeg_len) = psd_thumbnail_jpeg_len(&thumbnail_header, resource_len) {
                let mut encoded = vec![0u8; jpeg_len];
                file.read_exact(&mut encoded).ok()?;
                if let Some(thumbnail) = from_encoded(&encoded, max_px) {
                    if resource_id == PSD_THUMBNAIL_RESOURCE_V5 {
                        return Some(thumbnail);
                    }
                    if fallback.is_none() {
                        fallback = Some(thumbnail);
                    }
                }
            }
        }

        file.seek(SeekFrom::Start(padded_resource_end)).ok()?;
    }
    fallback
}

fn read_be_u16(reader: &mut impl std::io::Read) -> Option<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes).ok()?;
    Some(u16::from_be_bytes(bytes))
}

fn read_be_u32(reader: &mut impl std::io::Read) -> Option<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes).ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn psd_thumbnail_jpeg_len(header: &[u8; 28], resource_len: u64) -> Option<usize> {
    let format = u32::from_be_bytes(header[..4].try_into().ok()?);
    let compressed_len =
        usize::try_from(u32::from_be_bytes(header[20..24].try_into().ok()?)).ok()?;
    if format != 1 || compressed_len == 0 || compressed_len > MAX_PSD_THUMBNAIL_BYTES {
        return None;
    }
    PSD_THUMBNAIL_HEADER_BYTES
        .checked_add(u64::try_from(compressed_len).ok()?)
        .filter(|required| *required <= resource_len)?;
    Some(compressed_len)
}

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const AFFINITY_SCAN_BUFFER_BYTES: usize = 64 * 1024;
const MAX_AFFINITY_SCAN_BYTES: u64 = 512 * 1024 * 1024;
const MAX_AFFINITY_PNG_SPAN_BYTES: u64 = 50 * 1024 * 1024;
const MAX_AFFINITY_THUMBNAIL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_AFFINITY_PNG_CHUNKS: usize = 4096;
const MAX_AFFINITY_PNG_PROBES: usize = 4096;
const MAX_AFFINITY_PNG_CANDIDATES: usize = 32;

#[derive(Clone, Copy)]
struct EmbeddedPng {
    offset: u64,
    len: u64,
}

fn is_affinity_extension(extension: &str) -> bool {
    ["af", "afdesign", "afphoto", "afpub"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

/// Extracts the smallest structurally valid PNG embedded in an Affinity file.
/// The format is proprietary, so this path deliberately relies only on PNG's
/// public chunk structure and never attempts to interpret document records.
/// Scanning is streamed and bounded; candidates are allocated one at a time.
fn from_affinity_thumbnail(path: &Path, max_px: u32) -> Option<Thumbnail> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let scan_end = file.metadata().ok()?.len().min(MAX_AFFINITY_SCAN_BYTES);
    let candidates = affinity_png_candidates(&mut file, scan_end).ok()?;
    for candidate in candidates {
        let encoded_len = usize::try_from(candidate.len).ok()?;
        file.seek(SeekFrom::Start(candidate.offset)).ok()?;
        let mut encoded = vec![0u8; encoded_len];
        file.read_exact(&mut encoded).ok()?;
        if let Some(thumbnail) = from_encoded(&encoded, max_px) {
            return Some(thumbnail);
        }
    }
    None
}

fn affinity_png_candidates(
    file: &mut std::fs::File,
    scan_end: u64,
) -> std::io::Result<Vec<EmbeddedPng>> {
    let mut candidates = Vec::new();
    let mut cursor = 0u64;
    let mut probes = 0usize;
    while probes < MAX_AFFINITY_PNG_PROBES
        && scan_end.saturating_sub(cursor) >= PNG_SIGNATURE.len() as u64
    {
        let Some(offset) = find_next_png_signature(file, cursor, scan_end)? else {
            break;
        };
        probes += 1;
        if let Some(end) = embedded_png_end(file, offset, scan_end)? {
            let len = end - offset;
            if len <= MAX_AFFINITY_THUMBNAIL_BYTES {
                retain_small_affinity_candidate(&mut candidates, EmbeddedPng { offset, len });
            }
            cursor = end;
        } else {
            cursor = offset + 1;
        }
    }
    candidates.sort_unstable_by_key(|candidate| (candidate.len, candidate.offset));
    Ok(candidates)
}

fn find_next_png_signature(
    file: &mut std::fs::File,
    start: u64,
    scan_end: u64,
) -> std::io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(start))?;
    let mut buffer = [0u8; AFFINITY_SCAN_BUFFER_BYTES];
    let mut cursor = start;
    let mut matched = 0usize;
    while cursor < scan_end {
        let remaining = usize::try_from((scan_end - cursor).min(buffer.len() as u64))
            .expect("the scan buffer length always fits usize");
        let read = file.read(&mut buffer[..remaining])?;
        if read == 0 {
            break;
        }
        for (index, byte) in buffer[..read].iter().enumerate() {
            if *byte == PNG_SIGNATURE[matched] {
                matched += 1;
                if matched == PNG_SIGNATURE.len() {
                    let end = cursor + index as u64 + 1;
                    return Ok(Some(end - PNG_SIGNATURE.len() as u64));
                }
            } else {
                matched = usize::from(*byte == PNG_SIGNATURE[0]);
            }
        }
        cursor += read as u64;
    }
    Ok(None)
}

fn embedded_png_end(
    file: &mut std::fs::File,
    start: u64,
    scan_end: u64,
) -> std::io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom};

    let Some(mut cursor) = start.checked_add(PNG_SIGNATURE.len() as u64) else {
        return Ok(None);
    };
    file.seek(SeekFrom::Start(cursor))?;
    for chunk_index in 0..MAX_AFFINITY_PNG_CHUNKS {
        let Some(header_end) = cursor.checked_add(8) else {
            return Ok(None);
        };
        if header_end > scan_end {
            return Ok(None);
        }

        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let data_len = u64::from(u32::from_be_bytes(
            header[..4].try_into().expect("four-byte PNG length"),
        ));
        let chunk_type: &[u8; 4] = header[4..].try_into().expect("four-byte PNG chunk type");
        if !chunk_type.iter().all(u8::is_ascii_alphabetic) {
            return Ok(None);
        }
        if chunk_index == 0 && (chunk_type != b"IHDR" || data_len != 13) {
            return Ok(None);
        }

        let Some(chunk_end) = header_end
            .checked_add(data_len)
            .and_then(|end| end.checked_add(4))
        else {
            return Ok(None);
        };
        if chunk_end > scan_end || chunk_end - start > MAX_AFFINITY_PNG_SPAN_BYTES {
            return Ok(None);
        }
        if chunk_type == b"IEND" {
            return Ok((data_len == 0).then_some(chunk_end));
        }

        cursor = chunk_end;
        file.seek(SeekFrom::Start(cursor))?;
    }
    Ok(None)
}

fn retain_small_affinity_candidate(candidates: &mut Vec<EmbeddedPng>, candidate: EmbeddedPng) {
    if candidates.len() < MAX_AFFINITY_PNG_CANDIDATES {
        candidates.push(candidate);
        return;
    }
    let Some((largest_index, largest)) = candidates
        .iter()
        .enumerate()
        .max_by_key(|(_, existing)| (existing.len, existing.offset))
    else {
        return;
    };
    if candidate.len < largest.len {
        candidates[largest_index] = candidate;
    }
}

/// Maximum amount of audio metadata inspected for one thumbnail. Album covers
/// are normally far smaller; the cap prevents a malformed file from making
/// several workers allocate or seek through an amount derived from arbitrary
/// bytes.
const MAX_AUDIO_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_AUDIO_PICTURES: usize = 16;
const MAX_FLAC_METADATA_BLOCKS: usize = 128;

/// Thumbnail from embedded audio cover art. Only formats with a small, bounded
/// parser live here: ID3v2 for MP3 and native `PICTURE` blocks for FLAC.
pub fn from_audio(path: &Path, max_px: u32) -> Option<Thumbnail> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("mp3") {
        from_id3_cover(path, max_px)
    } else if extension.eq_ignore_ascii_case("flac") {
        from_flac_cover(path, max_px)
    } else {
        None
    }
}

fn from_id3_cover(path: &Path, max_px: u32) -> Option<Thumbnail> {
    let (version, flags, mut tag) = read_id3v2_tag(path)?;
    // ID3v2.2 compression applies to the entire tag and requires a decompressor.
    // It is rare and intentionally rejected rather than misparsed.
    if version == 2 && flags & 0x40 != 0 {
        return None;
    }
    let tag_unsynchronized = flags & 0x80 != 0;
    if tag_unsynchronized {
        tag = remove_id3_unsynchronization(&tag);
    }

    let mut cursor = id3_frames_start(version, flags, &tag)?;
    let mut fallback = None;
    let mut pictures_checked = 0usize;
    while cursor < tag.len() {
        let Some((id, size, frame_flags, header_len)) = id3_frame_header(version, &tag[cursor..])
        else {
            break;
        };
        if id.iter().all(|byte| *byte == 0) {
            break; // padding after the final frame
        }
        if !id
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            break; // corrupt boundary: never search arbitrary audio bytes
        }
        let Some(data_start) = cursor.checked_add(header_len) else {
            break;
        };
        let Some(data_end) = data_start.checked_add(size) else {
            break;
        };
        let Some(raw_frame) = tag.get(data_start..data_end) else {
            break;
        };
        cursor = data_end;

        let is_picture = (version == 2 && id == b"PIC") || (version != 2 && id == b"APIC");
        if !is_picture {
            continue;
        }
        pictures_checked += 1;
        if pictures_checked > MAX_AUDIO_PICTURES {
            break;
        }
        let Some(frame) = prepare_id3_frame(version, frame_flags, raw_frame, tag_unsynchronized)
        else {
            continue;
        };
        let Some((picture_type, encoded)) = id3_picture_data(version, &frame) else {
            continue;
        };
        // ID3 picture type 3 is the front cover. Tags may contain artist,
        // leaflet or back-cover images before it, so only those are fallbacks.
        if picture_type == 3 {
            if let Some(thumbnail) = from_encoded(encoded, max_px) {
                return Some(thumbnail);
            }
        } else if fallback.is_none() {
            fallback = from_encoded(encoded, max_px);
        }
    }
    fallback
}

/// Reads native FLAC metadata until the last block, without touching audio
/// frames. Non-picture blocks are skipped directly in the file; only bounded
/// `PICTURE` payloads are allocated and decoded.
fn from_flac_cover(path: &Path, max_px: u32) -> Option<Thumbnail> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    if &magic != b"fLaC" {
        return None;
    }

    let mut inspected_bytes = magic.len();
    let mut blocks_checked = 0usize;
    let mut pictures_checked = 0usize;
    let mut fallback = None;
    loop {
        if blocks_checked >= MAX_FLAC_METADATA_BLOCKS {
            return fallback;
        }
        blocks_checked += 1;

        let mut header = [0u8; 4];
        if file.read_exact(&mut header).is_err() {
            return fallback;
        }
        let is_last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let block_len =
            (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
        let Some(next_inspected) = inspected_bytes
            .checked_add(header.len())
            .and_then(|value| value.checked_add(block_len))
        else {
            return fallback;
        };
        if next_inspected > MAX_AUDIO_METADATA_BYTES {
            return fallback;
        }
        inspected_bytes = next_inspected;

        if block_type == 6 {
            pictures_checked += 1;
            if pictures_checked > MAX_AUDIO_PICTURES {
                return fallback;
            }
            let mut block = vec![0u8; block_len];
            if file.read_exact(&mut block).is_err() {
                return fallback;
            }
            if let Some((picture_type, encoded)) = flac_picture_data(&block) {
                // FLAC uses the same picture-type registry as ID3: type 3 is
                // the front cover, while other valid pictures are fallbacks.
                if picture_type == 3 {
                    if let Some(thumbnail) = from_encoded(encoded, max_px) {
                        return Some(thumbnail);
                    }
                } else if fallback.is_none() {
                    fallback = from_encoded(encoded, max_px);
                }
            }
        } else if file.seek(SeekFrom::Current(block_len as i64)).is_err() {
            return fallback;
        }

        if is_last {
            return fallback;
        }
    }
}

fn flac_picture_data(block: &[u8]) -> Option<(u32, &[u8])> {
    fn read_u32(block: &[u8], cursor: &mut usize) -> Option<u32> {
        let end = cursor.checked_add(4)?;
        let value = u32::from_be_bytes(block.get(*cursor..end)?.try_into().ok()?);
        *cursor = end;
        Some(value)
    }

    fn read_counted<'a>(block: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
        let len = usize::try_from(read_u32(block, cursor)?).ok()?;
        let end = cursor.checked_add(len)?;
        let value = block.get(*cursor..end)?;
        *cursor = end;
        Some(value)
    }

    let mut cursor = 0usize;
    let picture_type = read_u32(block, &mut cursor)?;
    let mime = read_counted(block, &mut cursor)?;
    if mime == b"-->" {
        return None; // external URL: Ranger never fetches remote artwork
    }
    let _description = read_counted(block, &mut cursor)?;
    for _ in 0..4 {
        let _ = read_u32(block, &mut cursor)?; // width, height, depth, palette size
    }
    let encoded = read_counted(block, &mut cursor)?;
    if encoded.is_empty() || cursor != block.len() {
        return None;
    }
    Some((picture_type, encoded))
}

fn read_id3v2_tag(path: &Path) -> Option<(u8, u8, Vec<u8>)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[..3] != b"ID3" || !matches!(header[3], 2..=4) {
        return None;
    }
    let size = synchsafe_u32(&header[6..10])? as usize;
    if size == 0 || size > MAX_AUDIO_METADATA_BYTES {
        return None;
    }
    let mut tag = vec![0u8; size];
    file.read_exact(&mut tag).ok()?;
    Some((header[3], header[5], tag))
}

/// Decodes a 28-bit synchsafe integer. The top bit of every source byte must
/// be clear; accepting it would let malformed sizes jump across frame bounds.
fn synchsafe_u32(bytes: &[u8]) -> Option<u32> {
    let bytes: &[u8; 4] = bytes.try_into().ok()?;
    if bytes.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0, |value, byte| (value << 7) | u32::from(*byte)),
    )
}

fn remove_id3_unsynchronization(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        decoded.push(bytes[index]);
        if bytes[index] == 0xff && bytes.get(index + 1) == Some(&0) {
            index += 1;
        }
        index += 1;
    }
    decoded
}

fn id3_frames_start(version: u8, flags: u8, tag: &[u8]) -> Option<usize> {
    if flags & 0x40 == 0 {
        return Some(0);
    }
    match version {
        3 => {
            let size = u32::from_be_bytes(tag.get(..4)?.try_into().ok()?) as usize;
            4usize.checked_add(size).filter(|end| *end <= tag.len())
        }
        4 => {
            // In v2.4 the extended-header size includes these four bytes.
            let size = synchsafe_u32(tag.get(..4)?)? as usize;
            (size >= 4 && size <= tag.len()).then_some(size)
        }
        _ => None,
    }
}

/// `(frame id, payload size, format flags, header length)`.
fn id3_frame_header(version: u8, bytes: &[u8]) -> Option<(&[u8], usize, u8, usize)> {
    match version {
        2 => {
            let header = bytes.get(..6)?;
            let size = (usize::from(header[3]) << 16)
                | (usize::from(header[4]) << 8)
                | usize::from(header[5]);
            Some((&header[..3], size, 0, 6))
        }
        3 | 4 => {
            let header = bytes.get(..10)?;
            let size = if version == 4 {
                synchsafe_u32(&header[4..8])? as usize
            } else {
                u32::from_be_bytes(header[4..8].try_into().ok()?) as usize
            };
            Some((&header[..4], size, header[9], 10))
        }
        _ => None,
    }
}

/// Removes optional per-frame prefixes and unsynchronization. Compressed or
/// encrypted frames are rejected: silently interpreting them as images would
/// be incorrect and these uncommon tags can safely keep the audio icon.
fn prepare_id3_frame<'a>(
    version: u8,
    flags: u8,
    raw: &'a [u8],
    tag_unsynchronized: bool,
) -> Option<std::borrow::Cow<'a, [u8]>> {
    let mut prefix = 0usize;
    let frame_unsynchronized = match version {
        2 => false,
        3 => {
            if flags & (0x80 | 0x40) != 0 {
                return None; // compression or encryption
            }
            if flags & 0x20 != 0 {
                prefix = 1; // grouping identity
            }
            false
        }
        4 => {
            if flags & (0x08 | 0x04) != 0 {
                return None; // compression or encryption
            }
            if flags & 0x40 != 0 {
                prefix += 1; // grouping identity
            }
            if flags & 0x01 != 0 {
                prefix += 4; // data length indicator
            }
            flags & 0x02 != 0
        }
        _ => return None,
    };
    let payload = raw.get(prefix..)?;
    if frame_unsynchronized && !tag_unsynchronized {
        Some(std::borrow::Cow::Owned(remove_id3_unsynchronization(
            payload,
        )))
    } else {
        Some(std::borrow::Cow::Borrowed(payload))
    }
}

fn id3_picture_data(version: u8, frame: &[u8]) -> Option<(u8, &[u8])> {
    let encoding = *frame.first()?;
    if encoding > 3 {
        return None;
    }
    let mut cursor = 1usize;
    if version == 2 {
        cursor = cursor.checked_add(3)?; // fixed `PNG`/`JPG` image format
    } else {
        // MIME is always ISO-8859-1 and null-terminated, independently from
        // the description's text encoding.
        cursor =
            cursor.checked_add(frame.get(cursor..)?.iter().position(|byte| *byte == 0)? + 1)?;
    }
    let picture_type = *frame.get(cursor)?;
    cursor += 1;
    let description = frame.get(cursor..)?;
    let terminator_len = if matches!(encoding, 1 | 2) { 2 } else { 1 };
    let description_len = if terminator_len == 1 {
        description.iter().position(|byte| *byte == 0)?
    } else {
        description
            .chunks_exact(2)
            .position(|pair| pair == [0, 0])?
            * 2
    };
    let image_start = cursor.checked_add(description_len + terminator_len)?;
    let image = frame.get(image_start..)?;
    (!image.is_empty()).then_some((picture_type, image))
}

/// Position of the preview in the timeline: **30% of the duration**. Many
/// videos open on a black screen / logo; a later moment gives a
/// recognizable thumbnail, and the fraction scales just as well from a
/// 10s clip to a 2h movie.
const VIDEO_THUMB_FRACTION: f64 = 0.30;
/// Number of frames analyzed by ffmpeg's `thumbnail` filter to pick the
/// most representative one (the one furthest from the batch average → avoids
/// a uniform black screen). Used ONLY when the duration is unknown: with
/// a known duration, seeking to 30% is enough and cheaper (no decoding
/// of N extra frames).
const VIDEO_THUMB_ANALYZE_FRAMES: u32 = 30;
/// Input seek (seconds) for the "unknown duration" fallback — a rare case: neither
/// the ISO-BMFF header nor `ffprobe` yielded a duration. Far enough in to skip past an
/// intro/logo; if the video is shorter, the seek fails and the caller retries
/// without seeking (cf. [`from_video`]).
const VIDEO_THUMB_SKIP_SECS: f64 = 5.0;

/// Seek position (seconds) for a given duration. `None` (unknown duration)
/// → flat intro skip.
fn video_seek_offset(duration_secs: Option<f64>) -> f64 {
    match duration_secs {
        Some(d) if d.is_finite() && d > 0.0 => d * VIDEO_THUMB_FRACTION,
        _ => VIDEO_THUMB_SKIP_SECS,
    }
}

/// Duration of a video in seconds, at the best possible cost:
/// 1. DIRECT read of the ISO-BMFF header (`moov`/`mvhd`) for
///    MP4/MOV/M4V/3GP containers — **~0.1 ms**, no process spawned;
/// 2. otherwise (Matroska/WebM/AVI…), probes `ffprobe` — correct but ~200 ms,
///    dominated purely by the binary's startup.
///
/// `None` if neither path succeeds → the caller falls back to the
/// "intro skip + representative frame" heuristic.
fn video_duration_secs(path: &Path) -> Option<f64> {
    mp4_duration_secs(path).or_else(|| ffprobe_duration_secs(path))
}

/// Duration read from the **ISO-BMFF** header: the `mvhd` box (direct child of `moov`),
/// which carries `timescale` (units/second) and `duration` (in units). We only read
/// box headers — a few dozen bytes, without decoding the media.
/// `None` if the file isn't ISO-BMFF, if `mvhd` is absent, or if the duration
/// is zero (the case for fragmented MP4s, where it lives in the fragments).
fn mp4_duration_secs(path: &Path) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};

    /// Reads a box header at the current position → `(total_size, type)`.
    /// `None` at end of stream or if the size is inconsistent.
    fn read_box_header(f: &mut std::fs::File, limit: u64) -> Option<(u64, [u8; 4])> {
        let start = f.stream_position().ok()?;
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr).ok()?;
        let mut size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
        if size == 1 {
            // 64-bit extended size (large files).
            let mut ext = [0u8; 8];
            f.read_exact(&mut ext).ok()?;
            size = u64::from_be_bytes(ext);
        } else if size == 0 {
            // The box extends to the end of the container.
            size = limit.checked_sub(start)?;
        }
        // A box smaller than its header, or that overflows, is invalid:
        // we reject it rather than looping forever on a malformed file.
        if size < 8 || start.checked_add(size)? > limit {
            return None;
        }
        Some((size, typ))
    }

    let mut f = std::fs::File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();

    // Root level: we look for `moov` (it can follow a large `mdat`).
    let mut cursor = 0u64;
    while cursor < file_len {
        f.seek(SeekFrom::Start(cursor)).ok()?;
        let (size, typ) = read_box_header(&mut f, file_len)?;
        if &typ == b"moov" {
            // Inside `moov`, `mvhd` is a DIRECT child (no need to descend
            // into trak/mdia): we scan just this one level.
            let moov_end = cursor + size;
            let mut inner = f.stream_position().ok()?; // right after the header
            while inner < moov_end {
                f.seek(SeekFrom::Start(inner)).ok()?;
                let (isize_, ityp) = read_box_header(&mut f, moov_end)?;
                if &ityp == b"mvhd" {
                    let mut ver = [0u8; 4]; // version (1) + flags (3)
                    f.read_exact(&mut ver).ok()?;
                    let (timescale, duration) = if ver[0] == 1 {
                        let mut buf = [0u8; 28]; // creation(8) mtime(8) ts(4) duration(8)
                        f.read_exact(&mut buf).ok()?;
                        let ts = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
                        let du = u64::from_be_bytes([
                            buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
                        ]);
                        (ts, du)
                    } else {
                        let mut buf = [0u8; 16]; // creation(4) mtime(4) ts(4) duration(4)
                        f.read_exact(&mut buf).ok()?;
                        let ts = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                        let du = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]) as u64;
                        (ts, du)
                    };
                    if timescale == 0 || duration == 0 {
                        return None; // fragmented MP4 / atypical header
                    }
                    let secs = duration as f64 / timescale as f64;
                    return secs.is_finite().then_some(secs);
                }
                inner = inner.checked_add(isize_)?;
            }
            return None; // `moov` without a usable `mvhd`
        }
        cursor = cursor.checked_add(size)?;
    }
    None
}

/// Duration via the **`ffprobe`** CLI (shipped with `ffmpeg`): reads METADATA
/// only (no decoding). Fallback for non-ISO-BMFF containers. `None` if `ffprobe`
/// is missing, fails, or doesn't expose a duration.
fn ffprobe_duration_secs(path: &Path) -> Option<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ])
    .arg(path);
    no_console(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let d: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    (d.is_finite() && d > 0.0).then_some(d)
}

/// Thumbnail of a video file via the `ffmpeg` CLI, in **a single decoding
/// process**: `scale` bounds the large side to `max_px` INSIDE ffmpeg (no
/// full-resolution PNG re-decoded on the Rust side).
///
/// The targeted frame is at **30% of the timeline** ([`VIDEO_THUMB_FRACTION`]) as soon as
/// the duration is known — free for MP4s (`mvhd` header). Without a duration,
/// we fall back to the "intro skip + `thumbnail` filter" heuristic (representative
/// frame among N). In both cases, if the seek goes past the end (a very short
/// video → empty output), we retry without seeking. `None` if `ffmpeg` is
/// not found or fails everywhere.
pub fn from_video(path: &Path, max_px: u32) -> Option<Thumbnail> {
    let duration = video_duration_secs(path);
    let seek = video_seek_offset(duration);
    // Known duration → seeking to 30% is enough; otherwise we let ffmpeg pick a
    // representative frame (costs decoding N frames, hence the targeted use).
    let analyze = duration.is_none();
    from_video_frame(path, max_px, Some(seek), analyze)
        .or_else(|| from_video_frame(path, max_px, None, true))
}

fn from_video_frame(
    path: &Path,
    max_px: u32,
    seek_secs: Option<f64>,
    analyze: bool,
) -> Option<Thumbnail> {
    // `scale=…decrease` bounds the large side to max_px without ever upscaling
    // (min(max_px, dim)); `thumbnail=n=N` (optional) picks a
    // representative frame among N.
    let scale =
        format!("scale='min({max_px},iw)':'min({max_px},ih)':force_original_aspect_ratio=decrease");
    let vf = if analyze {
        format!("thumbnail=n={VIDEO_THUMB_ANALYZE_FRAMES},{scale}")
    } else {
        scale
    };
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    // -ss BEFORE -i = fast input seek (keyframe ≤ target) → doesn't decode
    // from the start. Omitted on fallback (video shorter than the target).
    if let Some(secs) = seek_secs {
        cmd.args(["-ss", &format!("{secs:.3}")]);
    }
    cmd.arg("-i").arg(path).args([
        "-frames:v",
        "1",
        "-an",
        "-vf",
        &vf,
        "-f",
        "image2pipe",
        "-vcodec",
        "png",
        "pipe:1",
    ]);
    no_console(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let img = image::load_from_memory(&output.stdout).ok()?;
    Some(downscale(img, max_px))
}

/// Thumbnail of a PDF's 1st page via the **poppler** tools (`pdftoppm` then
/// `pdftocairo`), best-effort and dependency-free (same spirit as `ffmpeg`).
/// `None` if the file isn't a `.pdf`, if no tool is present, or on
/// failure. Both tools write `<prefix>.png` with `-singlefile`.
pub fn from_pdf(path: &Path, max_px: u32) -> Option<Thumbnail> {
    // Strict filter: `generate` routes ALL `Document`s here; only PDF has a
    // render (avoids launching poppler on a .txt/.docx).
    let is_pdf = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if !is_pdf {
        return None;
    }
    // Unique output prefix in the temp folder → `<prefix>.png`.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prefix = std::env::temp_dir().join(format!("ranger-pdf-{stamp}"));
    let png = prefix.with_extension("png");

    // `-f 1 -l 1 -singlefile` = 1st page only; `-scale-to N` bounds the large side.
    let ran = run_pdf_tool("pdftoppm", path, &prefix, max_px)
        || run_pdf_tool("pdftocairo", path, &prefix, max_px);

    let result = if ran {
        std::fs::read(&png)
            .ok()
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|img| downscale(img, max_px))
    } else {
        None
    };
    let _ = std::fs::remove_file(&png); // best-effort cleanup
    result
}

/// Runs a poppler tool (`pdftoppm`/`pdftocairo`) to render `pdf`'s 1st page
/// into `<out_prefix>.png`. Returns `true` if the tool finished successfully.
fn run_pdf_tool(tool: &str, pdf: &Path, out_prefix: &Path, max_px: u32) -> bool {
    let mut cmd = Command::new(tool);
    cmd.args(["-png", "-f", "1", "-l", "1", "-singlefile", "-scale-to"])
        .arg(max_px.to_string())
        .arg(pdf)
        .arg(out_prefix);
    no_console(&mut cmd);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Shrinks `img` to fit within `max_px × max_px` (aspect preserved) and produces
/// RGBA8. `DynamicImage::thumbnail` is fast (suited to generating many
/// thumbnails).
fn downscale(img: image::DynamicImage, max_px: u32) -> Thumbnail {
    let small = img.thumbnail(max_px, max_px).to_rgba8();
    Thumbnail {
        width: small.width(),
        height: small.height(),
        rgba: small.into_raw(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ranger-thumb-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn synchsafe_bytes(value: usize) -> [u8; 4] {
        assert!(value < (1 << 28));
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    fn encoded_cover(format: image::ImageFormat, rgba: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(20, 10, image::Rgba(rgba));
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, format)
            .unwrap();
        cursor.into_inner()
    }

    fn id3v23_frame(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut frame = id.to_vec();
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]); // status + format flags
        frame.extend_from_slice(payload);
        frame
    }

    fn id3v23_picture(picture_type: u8, encoded: &[u8]) -> Vec<u8> {
        let mut payload = vec![0]; // ISO-8859-1 description
        payload.extend_from_slice(b"image/png\0");
        payload.push(picture_type);
        payload.push(0); // empty description
        payload.extend_from_slice(encoded);
        id3v23_frame(b"APIC", &payload)
    }

    fn write_id3_file(path: &Path, version: u8, flags: u8, tag: &[u8]) {
        let mut file = b"ID3".to_vec();
        file.extend_from_slice(&[version, 0, flags]);
        file.extend_from_slice(&synchsafe_bytes(tag.len()));
        file.extend_from_slice(tag);
        file.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]); // inert MPEG-like tail
        std::fs::write(path, file).unwrap();
    }

    fn flac_picture(picture_type: u32, encoded: &[u8]) -> Vec<u8> {
        let mut block = picture_type.to_be_bytes().to_vec();
        block.extend_from_slice(&(b"image/png".len() as u32).to_be_bytes());
        block.extend_from_slice(b"image/png");
        block.extend_from_slice(&0u32.to_be_bytes()); // empty description
        block.extend_from_slice(&20u32.to_be_bytes()); // width
        block.extend_from_slice(&10u32.to_be_bytes()); // height
        block.extend_from_slice(&32u32.to_be_bytes()); // color depth
        block.extend_from_slice(&0u32.to_be_bytes()); // not indexed
        block.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        block.extend_from_slice(encoded);
        block
    }

    fn write_flac_file(path: &Path, blocks: &[(u8, Vec<u8>)]) {
        assert!(!blocks.is_empty());
        let mut file = b"fLaC".to_vec();
        for (index, (block_type, payload)) in blocks.iter().enumerate() {
            assert!(*block_type < 0x80 && payload.len() <= 0x00ff_ffff);
            let last = index + 1 == blocks.len();
            file.push(*block_type | if last { 0x80 } else { 0 });
            file.extend_from_slice(&[
                ((payload.len() >> 16) & 0xff) as u8,
                ((payload.len() >> 8) & 0xff) as u8,
                (payload.len() & 0xff) as u8,
            ]);
            file.extend_from_slice(payload);
        }
        file.extend_from_slice(&[0xff, 0xf8, 0x69]); // inert FLAC-like audio tail
        std::fs::write(path, file).unwrap();
    }

    fn psd_thumbnail_data(encoded: &[u8]) -> Vec<u8> {
        let mut data = 1u32.to_be_bytes().to_vec(); // JPEG thumbnail
        data.extend_from_slice(&20u32.to_be_bytes()); // width
        data.extend_from_slice(&10u32.to_be_bytes()); // height
        data.extend_from_slice(&60u32.to_be_bytes()); // padded row bytes
        data.extend_from_slice(&600u32.to_be_bytes()); // uncompressed size
        data.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        data.extend_from_slice(&24u16.to_be_bytes()); // bits per pixel
        data.extend_from_slice(&1u16.to_be_bytes()); // planes
        data.extend_from_slice(encoded);
        data
    }

    fn psd_resource(id: u16, data: &[u8]) -> Vec<u8> {
        let mut resource = b"8BIM".to_vec();
        resource.extend_from_slice(&id.to_be_bytes());
        resource.extend_from_slice(&[0, 0]); // empty Pascal name and padding
        resource.extend_from_slice(&(data.len() as u32).to_be_bytes());
        resource.extend_from_slice(data);
        if !data.len().is_multiple_of(2) {
            resource.push(0);
        }
        resource
    }

    fn write_psd_file(path: &Path, resources: &[Vec<u8>]) {
        let resources_len: usize = resources.iter().map(Vec::len).sum();
        let mut file = b"8BPS".to_vec();
        file.extend_from_slice(&1u16.to_be_bytes()); // PSD, not PSB
        file.extend_from_slice(&[0; 6]);
        file.extend_from_slice(&3u16.to_be_bytes()); // channels
        file.extend_from_slice(&10u32.to_be_bytes()); // height
        file.extend_from_slice(&20u32.to_be_bytes()); // width
        file.extend_from_slice(&8u16.to_be_bytes()); // channel depth
        file.extend_from_slice(&3u16.to_be_bytes()); // RGB
        file.extend_from_slice(&0u32.to_be_bytes()); // color mode data
        file.extend_from_slice(&(resources_len as u32).to_be_bytes());
        for resource in resources {
            file.extend_from_slice(resource);
        }
        std::fs::write(path, file).unwrap();
    }

    fn patterned_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([
                x.wrapping_mul(17) as u8,
                y.wrapping_mul(29) as u8,
                x.wrapping_mul(11).wrapping_add(y.wrapping_mul(7)) as u8,
                255,
            ])
        });
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        cursor.into_inner()
    }

    #[test]
    fn video_seek_targets_thirty_percent_with_safe_fallback() {
        // Known duration → 30% of the timeline.
        assert_eq!(video_seek_offset(Some(100.0)), 30.0);
        assert_eq!(video_seek_offset(Some(10.0)), 3.0);
        // Unknown / invalid duration → flat 5s intro skip (value
        // hardcoded in the assertion: an accidental change must fail the test).
        assert_eq!(VIDEO_THUMB_SKIP_SECS, 5.0);
        assert_eq!(video_seek_offset(None), 5.0);
        assert_eq!(video_seek_offset(Some(0.0)), 5.0);
        assert_eq!(video_seek_offset(Some(-5.0)), 5.0);
        assert_eq!(video_seek_offset(Some(f64::NAN)), 5.0);
        assert_eq!(video_seek_offset(Some(f64::INFINITY)), 5.0);
    }

    /// Builds a minimal MP4 `ftyp` + `moov`/`mvhd` (version 0) carrying
    /// `timescale`/`duration`, to validate box traversal without depending
    /// on a committed binary file or on ffmpeg.
    fn write_fake_mp4(path: &std::path::Path, timescale: u32, duration: u32) {
        fn boxed(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(typ);
            v.extend_from_slice(payload);
            v
        }
        let mut mvhd = vec![0u8; 4]; // version 0 + flags
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification
        mvhd.extend_from_slice(&timescale.to_be_bytes());
        mvhd.extend_from_slice(&duration.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 80]); // rest of mvhd (ignored)

        let mut file = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2");
        // Large `mdat` BEFORE `moov`: traversal must skip it cleanly.
        file.extend_from_slice(&boxed(b"mdat", &vec![0u8; 4096]));
        file.extend_from_slice(&boxed(b"moov", &boxed(b"mvhd", &mvhd)));
        std::fs::write(path, file).unwrap();
    }

    #[test]
    fn mp4_duration_read_from_header_without_spawning_ffprobe() {
        let dir = tempdir();
        // 19.064s at scale 1000 (real-world case of a common MP4).
        let p = dir.join("clip.mp4");
        write_fake_mp4(&p, 1000, 19_064);
        let d = mp4_duration_secs(&p).expect("duration readable from the header");
        assert!((d - 19.064).abs() < 1e-6, "duration read = {d}");
        // The targeted seek is indeed 30% of this duration.
        assert!((video_seek_offset(Some(d)) - 5.7192).abs() < 1e-4);

        // Fragmented MP4 (zero duration in mvhd) → None (ffprobe fallback on the caller's side).
        let frag = dir.join("frag.mp4");
        write_fake_mp4(&frag, 1000, 0);
        assert_eq!(mp4_duration_secs(&frag), None);

        // Non-ISO-BMFF file → None, without panicking or looping.
        let txt = dir.join("not.mp4");
        std::fs::write(&txt, b"pas un mp4 du tout, vraiment pas").unwrap();
        assert_eq!(mp4_duration_secs(&txt), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_thumbnail_preserves_aspect_and_bounds() {
        let dir = tempdir();
        let path = dir.join("red.png");
        // 100×50 opaque red, encoded as PNG via `image`.
        let buf = image::RgbaImage::from_pixel(100, 50, image::Rgba([255, 0, 0, 255]));
        buf.save(&path).unwrap();

        let thumb = from_image(&path, 32).expect("decode");
        // 2:1 aspect preserved, bounded to 32 → 32×16.
        assert!(thumb.width <= 32 && thumb.height <= 32);
        assert_eq!(thumb.width, 32);
        assert_eq!(thumb.height, 16);
        assert_eq!(thumb.rgba.len() as u32, thumb.width * thumb.height * 4);
        // Red pixel preserved.
        assert_eq!(&thumb.rgba[0..4], &[255, 0, 0, 255]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mp3_thumbnail_prefers_the_front_cover_without_decoding_audio() {
        let dir = tempdir();
        let path = dir.join("song.mp3");
        let blue = encoded_cover(image::ImageFormat::Png, [0, 0, 255, 255]);
        let red = encoded_cover(image::ImageFormat::Png, [255, 0, 0, 255]);
        let mut tag = id3v23_picture(4, &blue); // back cover comes first
        tag.extend_from_slice(&id3v23_picture(3, &red)); // front cover wins
        write_id3_file(&path, 3, 0, &tag);

        let thumb = from_audio(&path, 12).expect("embedded front cover");
        assert_eq!((thumb.width, thumb.height), (12, 6));
        assert_eq!(&thumb.rgba[..4], &[255, 0, 0, 255]);
        assert!(generate(&path, FileKind::Audio, 12).is_some());

        // Dispatch remains strict even if another extension happens to begin
        // with bytes that look like an ID3 tag.
        let flac = dir.join("song.flac");
        std::fs::copy(&path, &flac).unwrap();
        assert!(from_audio(&flac, 12).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flac_thumbnail_prefers_front_cover_and_skips_audio_frames() {
        let dir = tempdir();
        let path = dir.join("song.flac");
        let blue = encoded_cover(image::ImageFormat::Png, [0, 0, 255, 255]);
        let red = encoded_cover(image::ImageFormat::Png, [255, 0, 0, 255]);
        write_flac_file(
            &path,
            &[
                (0, vec![0; 34]),            // mandatory STREAMINFO
                (6, flac_picture(4, &blue)), // back cover comes first
                (6, flac_picture(3, &red)),  // front cover wins
            ],
        );

        let thumb = from_audio(&path, 12).expect("embedded FLAC front cover");
        assert_eq!((thumb.width, thumb.height), (12, 6));
        assert_eq!(&thumb.rgba[..4], &[255, 0, 0, 255]);
        assert!(generate(&path, FileKind::Audio, 12).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn psd_thumbnail_prefers_the_modern_embedded_resource() {
        let dir = tempdir();
        let path = dir.join("artwork.PSD");
        let blue = encoded_cover(image::ImageFormat::Jpeg, [0, 0, 255, 255]);
        let red = encoded_cover(image::ImageFormat::Jpeg, [255, 0, 0, 255]);
        write_psd_file(
            &path,
            &[
                psd_resource(PSD_THUMBNAIL_RESOURCE_V4, &psd_thumbnail_data(&blue)),
                psd_resource(PSD_THUMBNAIL_RESOURCE_V5, &psd_thumbnail_data(&red)),
            ],
        );

        let thumb = from_image(&path, 12).expect("embedded PSD thumbnail");
        assert_eq!((thumb.width, thumb.height), (12, 6));
        assert!(thumb.rgba[0] > 200 && thumb.rgba[2] < 40);
        assert!(generate(&path, FileKind::Image, 12).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn psd_thumbnail_accepts_the_legacy_resource_as_a_fallback() {
        let dir = tempdir();
        let path = dir.join("legacy.psd");
        let cover = encoded_cover(image::ImageFormat::Jpeg, [20, 180, 70, 255]);
        write_psd_file(
            &path,
            &[psd_resource(
                PSD_THUMBNAIL_RESOURCE_V4,
                &psd_thumbnail_data(&cover),
            )],
        );

        assert!(from_image(&path, 16).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_or_oversized_psd_thumbnail_falls_back_safely() {
        let dir = tempdir();
        let coverless = dir.join("coverless.psd");
        write_psd_file(&coverless, &[]);
        assert!(from_image(&coverless, 32).is_none());

        let truncated = dir.join("truncated.psd");
        let mut bytes = std::fs::read(&coverless).unwrap();
        bytes[30..34].copy_from_slice(&100u32.to_be_bytes());
        std::fs::write(&truncated, bytes).unwrap();
        assert!(from_image(&truncated, 32).is_none());

        let mut header = [0u8; 28];
        header[..4].copy_from_slice(&1u32.to_be_bytes());
        header[20..24].copy_from_slice(&((MAX_PSD_THUMBNAIL_BYTES + 1) as u32).to_be_bytes());
        assert!(psd_thumbnail_jpeg_len(&header, u64::MAX).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn affinity_thumbnail_streams_across_buffers_and_selects_the_smallest_png() {
        let dir = tempdir();
        let path = dir.join("drawing.AFPHOTO");
        let large = patterned_png(128, 64);
        let small = encoded_cover(image::ImageFormat::Png, [240, 30, 10, 255]);
        assert!(small.len() < large.len());

        // Start the first signature three bytes before the scan-buffer edge to
        // exercise matching across reads, then place the actual thumbnail later.
        let mut affinity = vec![0x55; AFFINITY_SCAN_BUFFER_BYTES - 3];
        affinity.extend_from_slice(&large);
        affinity.extend_from_slice(b"proprietary document records");
        affinity.extend_from_slice(&small);
        std::fs::write(&path, affinity).unwrap();

        let thumb = from_image(&path, 12).expect("embedded Affinity thumbnail");
        assert_eq!((thumb.width, thumb.height), (12, 6));
        assert_eq!(&thumb.rgba[..4], &[240, 30, 10, 255]);
        assert!(generate(&path, FileKind::Image, 12).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_affinity_png_signature_falls_back_without_decoding() {
        let dir = tempdir();
        for extension in ["af", "afdesign", "afphoto", "afpub"] {
            let path = dir.join(format!("broken.{extension}"));
            let mut bytes = b"Affinity-like prefix".to_vec();
            bytes.extend_from_slice(PNG_SIGNATURE);
            bytes.extend_from_slice(&13u32.to_be_bytes());
            bytes.extend_from_slice(b"IDAT"); // a PNG must begin with IHDR
            bytes.extend_from_slice(&[0; 17]);
            std::fs::write(&path, bytes).unwrap();
            assert!(from_image(&path, 32).is_none(), "{extension}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_or_coverless_flac_falls_back_without_large_allocation() {
        let dir = tempdir();
        let coverless = dir.join("coverless.flac");
        write_flac_file(&coverless, &[(0, vec![0; 34])]);
        assert!(from_audio(&coverless, 32).is_none());

        // The largest representable FLAC block would cross Ranger's bounded
        // metadata budget. Its payload is deliberately absent: rejection must
        // happen from the four-byte header, before any allocation or read.
        let oversized = dir.join("oversized.flac");
        std::fs::write(&oversized, b"fLaC\x86\xff\xff\xff").unwrap();
        assert!(from_audio(&oversized, 32).is_none());

        // A URL-valued PICTURE block is valid FLAC metadata but must never
        // trigger network access in Ranger's fully local thumbnail pipeline.
        let mut remote = 3u32.to_be_bytes().to_vec();
        remote.extend_from_slice(&3u32.to_be_bytes());
        remote.extend_from_slice(b"-->");
        remote.extend_from_slice(&0u32.to_be_bytes());
        remote.extend_from_slice(&[0; 16]);
        remote.extend_from_slice(&19u32.to_be_bytes());
        remote.extend_from_slice(b"https://example.test");
        assert!(flac_picture_data(&remote).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mp3_thumbnail_supports_id3v22_pic_and_tag_unsynchronization() {
        let dir = tempdir();

        let png = encoded_cover(image::ImageFormat::Png, [12, 34, 56, 255]);
        let mut pic_payload = vec![0];
        pic_payload.extend_from_slice(b"PNG");
        pic_payload.push(3);
        pic_payload.push(0);
        pic_payload.extend_from_slice(&png);
        let mut pic_frame = b"PIC".to_vec();
        let size = pic_payload.len();
        pic_frame.extend_from_slice(&[
            ((size >> 16) & 0xff) as u8,
            ((size >> 8) & 0xff) as u8,
            (size & 0xff) as u8,
        ]);
        pic_frame.extend_from_slice(&pic_payload);
        let v22 = dir.join("old.mp3");
        write_id3_file(&v22, 2, 0, &pic_frame);
        assert!(from_audio(&v22, 16).is_some());

        // JPEG contains 0xff marker bytes. ID3 tag-level unsynchronization
        // inserts a zero after each one; decoding must remove it before image
        // detection while keeping the original APIC frame size meaningful.
        let jpeg = encoded_cover(image::ImageFormat::Jpeg, [90, 140, 190, 255]);
        let frame = id3v23_picture(3, &jpeg);
        let mut unsynchronized = Vec::with_capacity(frame.len());
        for byte in frame {
            unsynchronized.push(byte);
            if byte == 0xff {
                unsynchronized.push(0);
            }
        }
        let v23_unsync = dir.join("unsync.mp3");
        write_id3_file(&v23_unsync, 3, 0x80, &unsynchronized);
        assert!(from_audio(&v23_unsync, 16).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_or_coverless_mp3_falls_back_without_large_allocation() {
        let dir = tempdir();
        let coverless = dir.join("coverless.mp3");
        let title = id3v23_frame(b"TIT2", b"\0A song");
        write_id3_file(&coverless, 3, 0, &title);
        assert!(from_audio(&coverless, 32).is_none());

        let oversized = dir.join("oversized.mp3");
        let mut header = b"ID3".to_vec();
        header.extend_from_slice(&[3, 0, 0]);
        header.extend_from_slice(&synchsafe_bytes(MAX_AUDIO_METADATA_BYTES + 1));
        std::fs::write(&oversized, header).unwrap();
        assert!(from_audio(&oversized, 32).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_meta_reads_dimensions_and_depth() {
        let dir = tempdir();
        let path = dir.join("rgba.png");
        // 64×32 RGBA8: 24 color bits + one separate alpha channel.
        image::RgbaImage::from_pixel(64, 32, image::Rgba([1, 2, 3, 4]))
            .save(&path)
            .unwrap();
        let (w, h, bits, alpha) = image_meta(&path).expect("metadata readable");
        assert_eq!((w, h), (64, 32));
        assert_eq!(bits, 24);
        assert!(alpha);
        // A non-image file → None.
        let txt = dir.join("x.txt");
        std::fs::write(&txt, b"nope").unwrap();
        assert!(image_meta(&txt).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsupported_path_returns_none() {
        let dir = tempdir();
        let path = dir.join("not-an-image.txt");
        std::fs::write(&path, b"hello").unwrap();
        assert!(from_image(&path, 32).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_dispatches_by_kind() {
        let dir = tempdir();
        let path = dir.join("p.png");
        image::RgbaImage::from_pixel(10, 10, image::Rgba([0, 255, 0, 255]))
            .save(&path)
            .unwrap();
        // Image → thumbnail; other types → None (no preview).
        assert!(generate(&path, FileKind::Image, 16).is_some());
        assert!(generate(&path, FileKind::Document, 16).is_none());
        assert!(generate(&path, FileKind::Folder, 16).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
