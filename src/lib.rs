//! flashnpm — Flash npm in idiomatic Rust.
//!
//! Library layout mirrors `upm`'s `src/*.ts` so behavior can be compared
//! module by module:
//!
//! - [`spec`] / [`semver`] — spec parsing + npm range matching
//! - [`integrity`] — SSRI subset
//! - [`config`] — `.npmrc` hierarchy
//! - [`registry`] — packument client + metadata cache
//! - [`pick`] — version selection incl. release-age gate
//! - [`resolve`] — dependency walk (no hoisting, like upm)
//! - [`lock`] — `flashnpm.lock` round-trip
//! - [`store`] — content-addressed `~/.flashnpm/store`
//! - [`tarball`] — `file:`/`https:` tarball deps
//! - [`link`] — `node_modules/.flashnpm` linker + bins
//! - [`state`] — install-state fast path
//! - [`api`] — commands as functions (no printing)
//! - [`cli`] — argv parsing + output formatting
//! - [`run`] — script running (`run.ts`)
//! - [`exec`] — bin lookup for exec (`exec.ts`)

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod exec;
pub mod integrity;
pub mod link;
pub mod lock;
pub mod package_json;
pub mod pick;
pub mod profile;
pub mod registry;
pub mod resolve;
pub mod run;
pub mod semver;
pub mod spec;
pub mod state;
pub mod store;
pub mod tarball;
pub mod types;
pub mod workspaces;

pub use error::{FlashnpmError, ErrorCode};
