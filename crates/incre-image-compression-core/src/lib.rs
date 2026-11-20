//! Recompresses one picture (JPEG, PNG, GIF, BMP or WebP) to AVIF or JPEG XL, whichever is
//! smaller while SSIMULACRA2 says the result still looks like the source, line art included;
//! the source is kept when nothing beats it.

pub mod avif;
pub mod choose;
pub mod img;
pub mod jxl;
pub mod metric;

use std::fmt;

pub use choose::{Choice, Chooser, Codec, Config, Cpu, Gates, Metric};

/// What went wrong, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// An error saying `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}
