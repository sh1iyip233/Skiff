//! epic-core: Epic Games store client, manifest reader and chunked download engine.
//!
//! Layering (each module is independently testable):
//!
//! * [`config`] – on-disk configuration and defaults (concurrency knobs live here).
//! * [`http`]   – blocking HTTP client: WinHTTP on Windows, plain HTTP/1.1 for tests.
//! * [`auth`]   – device-code / authorization-code login, token refresh, DPAPI-protected storage.
//! * [`api`]    – entitlement, library, catalog and launcher-asset endpoints.
//! * [`manifest`] – Epic binary manifest parser (magic `0x44BEC00C`) with JSON fallback.
//! * [`chunk`]  – chunk container decoding (zlib / AES-GCM) and integrity checks.
//! * [`cdn`]    – manifest/chunk URL construction and CDN host rotation.
//! * [`download`] – user-bounded thread pool, writer fan-out, rate limiting, progress.
//! * [`install`] – install/verify/repair orchestration plus `.egstore` metadata writing.

pub mod api;
pub mod auth;
pub mod cdn;
pub mod chunk;
pub mod config;
pub mod download;
pub mod error;
pub mod http;
pub mod install;
pub mod library;
pub mod manifest;
pub mod net;
pub mod update;
pub mod util;

/// Semantic version of the core library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use error::{Error, Result};
