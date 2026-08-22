#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::replace_existing;
