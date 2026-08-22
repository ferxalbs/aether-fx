use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt, path::Path};

use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

fn wide(path: &Path) -> Vec<u16> {
    OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect()
}

pub(crate) fn replace_existing(destination: &Path, replacement: &Path) -> io::Result<()> {
    let destination = wide(destination);
    let replacement = wide(replacement);
    // SAFETY: both vectors are NUL-terminated UTF-16 strings that remain alive for this call;
    // the optional backup, exclusion, and preserved-metadata pointers are explicitly null as
    // permitted by ReplaceFileW when no backup or metadata exclusion is requested.
    let result = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
