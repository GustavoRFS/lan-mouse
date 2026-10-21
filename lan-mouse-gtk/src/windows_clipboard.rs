use std::{ffi::OsString, mem::size_of, os::windows::ffi::OsStringExt, path::PathBuf, ptr};

use gtk::{gdk, prelude::*};
use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL},
        Graphics::Gdi::{BI_BITFIELDS, BITMAPV5HEADER},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
                OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
        UI::Shell::{DragQueryFileW, HDROP},
    },
    core::w,
};

use crate::windows_tray;

const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;
const LCS_SRGB: u32 = 0x7352_4742;
const LCS_GM_IMAGES: u32 = 4;
const MAX_DIB_BYTES: usize = 256 * 1024 * 1024;

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

pub(crate) fn copied_file_paths() -> Option<Vec<PathBuf>> {
    unsafe {
        IsClipboardFormatAvailable(CF_HDROP).ok()?;
        OpenClipboard(None).ok()?;
        let _guard = ClipboardGuard;
        let handle = GetClipboardData(CF_HDROP).ok()?;
        let drop = HDROP(handle.0);
        let count = DragQueryFileW(drop, u32::MAX, None);
        if count == 0 {
            return None;
        }
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = DragQueryFileW(drop, index, None) as usize;
            if length == 0 || length > 32_767 {
                return None;
            }
            let mut buffer = vec![0u16; length + 1];
            if DragQueryFileW(drop, index, Some(&mut buffer)) as usize != length {
                return None;
            }
            paths.push(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        Some(paths)
    }
}

/// Publishes an image both as the registered "PNG" format (Chromium/Electron,
/// Office; keeps transparency) and as CF_DIBV5 (Paint and classic Win32 apps,
/// Windows synthesizes CF_DIB/CF_BITMAP from it). The data is rendered up front
/// instead of on demand, so readers do not depend on the GTK window.
pub(crate) fn publish_image(png: &[u8], texture: &gdk::Texture) -> bool {
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let png_memory = (png_format != 0)
        .then(|| global_from_parts(&[png]))
        .flatten();
    let dib_memory = dib_v5(texture)
        .and_then(|(header, pixels)| global_from_parts(&[struct_bytes(&header), &pixels]));
    if png_memory.is_none() && dib_memory.is_none() {
        return false;
    }
    publish(&mut [(png_format, png_memory), (CF_DIBV5, dib_memory)])
}

/// Replaces the clipboard contents. Returns true if at least one format was
/// set; ownership of every handle that was set passes to the system, the rest
/// is freed.
fn publish(formats: &mut [(u32, Option<HGLOBAL>)]) -> bool {
    let free_all = |formats: &mut [(u32, Option<HGLOBAL>)]| {
        for (_, memory) in formats.iter_mut() {
            if let Some(memory) = memory.take() {
                unsafe {
                    let _ = GlobalFree(Some(memory));
                }
            }
        }
    };
    let Some(owner) = windows_tray::clipboard_owner_hwnd() else {
        free_all(formats);
        return false;
    };
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            free_all(formats);
            return false;
        }
        let _guard = ClipboardGuard;
        if EmptyClipboard().is_err() {
            free_all(formats);
            return false;
        }
        let mut published = false;
        for (format, memory) in formats.iter_mut() {
            let Some(handle) = *memory else {
                continue;
            };
            if SetClipboardData(*format, Some(HANDLE(handle.0))).is_ok() {
                memory.take();
                published = true;
            }
        }
        free_all(formats);
        published
    }
}

/// Converts the texture to a bottom-up 32-bit BGRA DIB with straight alpha.
fn dib_v5(texture: &gdk::Texture) -> Option<(BITMAPV5HEADER, Vec<u8>)> {
    let width = usize::try_from(texture.width()).ok()?;
    let height = usize::try_from(texture.height()).ok()?;
    let stride = width.checked_mul(4)?;
    let size = stride.checked_mul(height)?;
    if width == 0 || height == 0 || size > MAX_DIB_BYTES {
        return None;
    }
    // download() yields cairo ARGB32: premultiplied BGRA on little endian
    let mut top_down = vec![0u8; size];
    texture.download(&mut top_down, stride);
    let mut pixels = Vec::with_capacity(size);
    for row in top_down.chunks_exact(stride).rev() {
        for pixel in row.chunks_exact(4) {
            let alpha = u32::from(pixel[3]);
            let unpremultiply = |channel: u8| match alpha {
                0 => 0,
                255 => channel,
                _ => ((u32::from(channel) * 255 + alpha / 2) / alpha).min(255) as u8,
            };
            pixels.extend_from_slice(&[
                unpremultiply(pixel[0]),
                unpremultiply(pixel[1]),
                unpremultiply(pixel[2]),
                pixel[3],
            ]);
        }
    }
    let header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: i32::try_from(width).ok()?,
        bV5Height: i32::try_from(height).ok()?,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: u32::try_from(size).ok()?,
        bV5RedMask: 0x00ff_0000,
        bV5GreenMask: 0x0000_ff00,
        bV5BlueMask: 0x0000_00ff,
        bV5AlphaMask: 0xff00_0000,
        bV5CSType: LCS_SRGB,
        bV5Intent: LCS_GM_IMAGES,
        ..Default::default()
    };
    Some((header, pixels))
}

pub(crate) fn global_from_parts(parts: &[&[u8]]) -> Option<HGLOBAL> {
    let byte_count = parts.iter().map(|part| part.len()).sum::<usize>();
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, byte_count).ok()?;
        let data = GlobalLock(memory) as *mut u8;
        if data.is_null() {
            let _ = GlobalFree(Some(memory));
            return None;
        }
        let mut offset = 0;
        for part in parts {
            ptr::copy_nonoverlapping(part.as_ptr(), data.add(offset), part.len());
            offset += part.len();
        }
        let _ = GlobalUnlock(memory);
        Some(memory)
    }
}

pub(crate) fn struct_bytes<T: Copy>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}
