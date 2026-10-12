//! Nextcloud for LightCraft: WebDAV paths, listings and credentials, in pure Rust (L1).
//!
//! What a server tells us is never trusted: [`path::RemotePath`] holds only names that are safe
//! on every platform, and [`dav::parse_multistatus`] reads an answer without letting a bad entry
//! hide the good ones. Paths, listings and credentials do no I/O, so they run on wasm32 and are
//! tested without a server; the transfers (`client`, native only) are built on
//! `lightcraft-fetch` and tested against a small in-memory WebDAV server.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod auth;
#[cfg(not(target_arch = "wasm32"))]
pub mod client;
pub mod dav;
pub mod pair;
pub mod path;
pub mod plan;
#[cfg(not(target_arch = "wasm32"))]
pub mod sync;

// two attributes, not `cfg(all(test, …))`: clippy only treats a `cfg(test)` module as test code
#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod client_tests;
#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod sync_tests;
#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod testserver;

pub use auth::{AuthError, Credentials};
#[cfg(not(target_arch = "wasm32"))]
pub use client::{Client, ClientError, Options, Precondition, Remote, Uploaded};
pub use dav::{DavError, DavRoot, Depth, Entry, Listing, Skipped};
pub use pair::{FolderPair, State};
pub use path::{PathError, RemotePath};
pub use plan::{Action, Base, Conflict, LocalFile, Plan, RemoteFile, Replaces};
