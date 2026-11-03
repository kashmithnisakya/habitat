//! Policy engine, secrets vault, gateway and audit log for Habitat.
//!
//! So far this holds the policy engine and the grant store. See `docs/architecture.md`.

pub mod grants;
pub mod policy;
