//! Error and Result aliases for `hytte-ui`.

use std::fmt;

/// Errors returned when constructing or running a hytte [`App`](crate::App).
///
/// `#[non_exhaustive]`: more variants may be added, so match with a wildcard
/// arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// `gtk::init` / `adw::init` failed.
    GtkInit(gtk::glib::BoolError),
    /// `gio::Application::run` exited with a non-zero status.
    NonZeroExit(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GtkInit(e) => write!(f, "gtk init failed: {e}"),
            Self::NonZeroExit(code) => write!(f, "application exited with status {code}"),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias for a `Result` whose error is this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
