//! Unified error type.

use std::fmt;

/// Everything epic-core can fail with.
#[derive(Debug)]
pub enum Error {
    /// Filesystem failure.
    Io(std::io::Error),
    /// User or terminal input is not acceptable; the CLI maps this to exit code 2.
    Usage(String),
    /// Network/transport failure (connection, TLS, timeout, non-2xx status).
    Net(String),
    /// The server returned a well-formed error payload.
    Api {
        /// HTTP status code, when a response was received.
        status: u16,
        /// Machine readable error code from Epic, when present.
        code: String,
        /// Human readable message.
        message: String,
    },
    /// Authentication is required, expired, or was rejected.
    Auth(String),
    /// A manifest could not be parsed or is of an unsupported version.
    Manifest(String),
    /// Local integrity check failed.
    Integrity(String),
    /// The user stopped an install.
    Cancelled {
        /// Files deleted while cleaning up a cancelled (as opposed to paused) install.
        removed: usize,
        /// True when a completed install was left untouched because this run was an update.
        kept_existing_install: bool,
    },
    /// Downloaded chunk content did not match its recorded hash.
    ChunkCorrupt {
        /// Chunk identifier as printed in the CDN URL.
        chunk: String,
        /// Expected SHA-1 (hex).
        expected: String,
        /// Observed SHA-1 (hex).
        actual: String,
    },
}

impl Error {
    /// Stable, lowercase machine tag used by `--json` output.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Io(_) => "io",
            Error::Usage(_) => "usage",
            Error::Net(_) => "net",
            Error::Api { .. } => "api",
            Error::Auth(_) => "auth",
            Error::Manifest(_) => "manifest",
            Error::Integrity(_) => "integrity",
            Error::Cancelled { .. } => "cancelled",
            Error::ChunkCorrupt { .. } => "chunk_corrupt",
        }
    }

    /// Convenience constructor for usage errors.
    pub fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    /// Convenience constructor for network errors.
    pub fn net(msg: impl Into<String>) -> Self {
        Error::Net(msg.into())
    }

    /// Convenience constructor for integrity errors.
    pub fn integrity(msg: impl Into<String>) -> Self {
        Error::Integrity(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Usage(m) => write!(f, "{m}"),
            Error::Net(m) => write!(f, "network error: {m}"),
            Error::Api { status, code, message } => {
                if code.is_empty() {
                    write!(f, "api error (HTTP {status}): {message}")
                } else {
                    write!(f, "api error (HTTP {status}, {code}): {message}")
                }
            }
            Error::Auth(m) => write!(f, "authentication error: {m}"),
            Error::Manifest(m) => write!(f, "manifest error: {m}"),
            Error::Integrity(m) => write!(f, "integrity error: {m}"),
            Error::Cancelled { removed, kept_existing_install } => {
                if *kept_existing_install {
                    write!(f, "cancelled by user (kept the existing install)")
                } else if *removed > 0 {
                    write!(f, "cancelled by user (removed {removed} partial file(s))")
                } else {
                    write!(f, "cancelled by user")
                }
            }
            Error::ChunkCorrupt { chunk, expected, actual } => {
                write!(f, "chunk {chunk} corrupt: expected sha1 {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
