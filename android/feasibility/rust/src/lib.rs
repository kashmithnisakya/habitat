//! Throwaway library that proves Wasmtime can be embedded in an Android app.

use std::ffi::c_void;

use wasmtime::{Engine, Instance, Module, Result, Store};

const ADD_MODULE: &str = r#"
    (module
        (func (export "add") (param i32 i32) (result i32)
            local.get 0
            local.get 1
            i32.add))
"#;

/// Returned to Java when Wasmtime fails, because this library has no JNI exception support.
const FAILED: i32 = i32::MIN;

/// Compiles and runs a small WebAssembly module with Wasmtime.
pub fn add_in_wasm(a: i32, b: i32) -> Result<i32> {
    let engine = Engine::default();
    let module = Module::new(&engine, ADD_MODULE)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let add = instance.get_typed_func::<(i32, i32), i32>(&mut store, "add")?;
    add.call(&mut store, (a, b))
}

/// Called from `io.github.kashmithnisakya.habitat.feasibility.NativeBridge.addInWasm`.
///
/// The JNI pointers are unused, so this needs no JNI bindings and has no unsafe code.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_kashmithnisakya_habitat_feasibility_NativeBridge_addInWasm(
    _env: *mut c_void,
    _class: *mut c_void,
    a: i32,
    b: i32,
) -> i32 {
    std::panic::catch_unwind(|| add_in_wasm(a, b).unwrap_or(FAILED)).unwrap_or(FAILED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_inside_wasmtime() {
        assert_eq!(add_in_wasm(40, 2).unwrap(), 42);
    }

    #[test]
    fn wraps_like_wasm_i32_add() {
        assert_eq!(add_in_wasm(i32::MAX, 1).unwrap(), i32::MIN);
    }
}
