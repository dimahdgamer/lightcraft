//! Nextcloud for LightCraft: WebDAV paths, listings and credentials, in pure Rust (L1).
//!
//! What a server tells us is never trusted: [`path::RemotePath`] holds only names that are safe
//! on every platform, and [`dav::parse_multistatus`] reads an answer without letting a bad entry
//! hide the good ones. Nothing here touches the network or the disk, so all of it runs on wasm32
//! and is tested without a server; the transfers come on top of `lightcraft-fetch`.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod auth;
pub mod dav;
pub mod path;

pub use auth::{AuthError, Credentials};
pub use dav::{DavError, DavRoot, Depth, Entry, Listing, Skipped};
pub use path::{PathError, RemotePath};
