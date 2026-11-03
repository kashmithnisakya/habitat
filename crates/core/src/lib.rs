//! Supervisor, home store and shared types for Habitat.
//!
//! See `docs/architecture.md`.

// Only the Windows code in `platform` can use `unsafe`, as Windows only has a C API for what it needs.
// Each block there says why it's sound.
#![deny(unsafe_code)]

pub mod backend;
pub mod home;
pub mod manifest;
pub mod package;
pub mod platform;
pub mod run_log;
pub mod supervisor;
