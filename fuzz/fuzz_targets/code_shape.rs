//! A package's code, which is read before anything's compiled. Reading it can't crash or hang, and
//! has to give the same answer each time.
#![no_main]

use habitat_backend_wasm::code_shape::CodeShape;
use habitat_backend_wasm::compile_limits::CompileLimits;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = CompileLimits::default();
    let first = CodeShape::of(data);
    let second = CodeShape::of(data);
    assert_eq!(first.is_ok(), second.is_ok());
    if let Ok(shape) = first {
        let _ = limits.broken_by(&shape);
    }
});
