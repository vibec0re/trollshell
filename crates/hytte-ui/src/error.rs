//! Error and Result aliases for `hytte-ui`.

use std::fmt;

#[derive(Debug)]
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

pub type Result<T> = std::result::Result<T, Error>;
