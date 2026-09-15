//! Shared error and result types.
//!
//! The crate intentionally avoids `anyhow` / `thiserror`: a boxed, thread safe
//! trait object is all that is needed here and it keeps the dependency tree
//! (and therefore the memory footprint) minimal.

/// Boxed error returned by every fallible function of this crate.
///
/// `Send + Sync` is required because worker errors travel back through
/// `std::thread::JoinHandle`.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Build an [`Error`] from a plain message.
///
/// `Box<dyn Error + Send + Sync>` implements `From<String>` / `From<&str>`,
/// so no wrapper type is required.
pub fn err<S: Into<String>>(text: S) -> Error {
    text.into().into()
}

/// Build an [`Error`] that carries a Win32 / RAS error code.
pub fn os_err(context: &str, code: u32) -> Error {
    err(format!("{context} failed with error code {code}"))
}
