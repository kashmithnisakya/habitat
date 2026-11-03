//! Rust library for building Habitat agents.
//!
//! So far this only holds the bindings generated from `wit/agent-home.wit`.

/// The agent API, as generated from `wit/agent-home.wit`.
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "agent",
        pub_export_macro: true,
        export_macro_name: "export_agent",
        default_bindings_module: "habitat_sdk::bindings",
    });
}
