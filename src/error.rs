//! Error codes a caller can match on. Mirrors `upm`'s `ErrorCode`
//! so scripts can handle failures without parsing messages.

use thiserror::Error;

/// Machine-readable failure reason attached to every [`FlashnpmError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Eoption,
    Emanifet,
    Econfig,
    Eworkspace,
    Einvalidspec,
    Enodep,
    Elock,
    Enobin,
    E404,
    Etarget,
    Enoversions,
    Ebadplatform,
    Eintegrity,
    Eregistry,
    Enetwork,
    Etimedout,
    Eoffline,
    Eio,
}

impl ErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eoption => "EOPTION",
            Self::Emanifet => "EMANIFEST",
            Self::Econfig => "ECONFIG",
            Self::Eworkspace => "EWORKSPACE",
            Self::Einvalidspec => "EINVALIDSPEC",
            Self::Enodep => "ENODEP",
            Self::Elock => "ELOCK",
            Self::Enobin => "ENOBIN",
            Self::E404 => "E404",
            Self::Etarget => "ETARGET",
            Self::Enoversions => "ENOVERSIONS",
            Self::Ebadplatform => "EBADPLATFORM",
            Self::Eintegrity => "EINTEGRITY",
            Self::Eregistry => "EREGISTRY",
            Self::Enetwork => "ENETWORK",
            Self::Etimedout => "ETIMEDOUT",
            Self::Eoffline => "EOFFLINE",
            Self::Eio => "EIO",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The single error type used by the library and the CLI.
#[derive(Debug, Error)]
#[error("[{code}] {message}")]
pub struct FlashnpmError {
    pub code: ErrorCode,
    pub message: String,
    #[source]
    pub source: Option<AnyhowBox>,
}

type AnyhowBox = Box<dyn std::error::Error + Send + Sync>;

impl FlashnpmError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}

#[allow(unused_macros)]
macro_rules! flashnpm_err {
    ($code:expr, $($arg:tt)*) => {
        $crate::error::FlashnpmError::new($code, format!($($arg)*))
    };
}

#[allow(unused_imports)]
pub(crate) use flashnpm_err;

impl From<FlashnpmError> for std::io::Error {
    fn from(e: FlashnpmError) -> Self {
        Self::new(std::io::ErrorKind::Other, e)
    }
}
