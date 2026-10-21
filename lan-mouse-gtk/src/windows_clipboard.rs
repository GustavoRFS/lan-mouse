use std::{
    ffi::OsString,
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    ptr,
};

use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
                OpenClipboard, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
        UI::Shell::{DROPFILES, DragQueryFileW, HDROP},
    },
    core::BOOL,
};

use crate::windows_tray;

const CF_HDROP: u32 = 15;
const MAX_FILES: u32 = 16;

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
        if count == 0 || count > MAX_FILES {
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

pub(crate) fn publish_file_paths(paths: &[PathBuf]) -> bool {
    if paths.is_empty() || paths.len() > MAX_FILES as usize {
        return false;
    }
    let Some(owner) = windows_tray::clipboard_owner_hwnd() else {
        return false;
    };
    let mut names = Vec::<u16>::new();
    for path in paths {
        names.extend(path.as_os_str().encode_wide());
        names.push(0);
    }
    names.push(0);
    let byte_count = size_of::<DROPFILES>() + names.len() * size_of::<u16>();
    unsafe {
        let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, byte_count) else {
            return false;
        };
        let data = GlobalLock(memory);
        if data.is_null() {
            let _ = GlobalFree(Some(memory));
            return false;
        }
        let header = DROPFILES {
            pFiles: size_of::<DROPFILES>() as u32,
            fWide: BOOL(1),
            ..Default::default()
        };
        ptr::copy_nonoverlapping(
            &header as *const DROPFILES as *const u8,
            data as *mut u8,
            size_of::<DROPFILES>(),
        );
        ptr::copy_nonoverlapping(
            names.as_ptr() as *const u8,
            (data as *mut u8).add(size_of::<DROPFILES>()),
            names.len() * size_of::<u16>(),
        );
        let _ = GlobalUnlock(memory);
        if OpenClipboard(Some(owner)).is_err() {
            let _ = GlobalFree(Some(memory));
            return false;
        }
        let _guard = ClipboardGuard;
        if EmptyClipboard().is_err() || SetClipboardData(CF_HDROP, Some(HANDLE(memory.0))).is_err()
        {
            let _ = GlobalFree(Some(memory));
            return false;
        }
        true
    }
}
