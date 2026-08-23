#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::install_exclusive;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::{install_exclusive, replace_existing};
