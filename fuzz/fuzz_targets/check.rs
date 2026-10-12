//! The compiler front end must never panic or hang, whatever the input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(src) = std::str::from_utf8(data) {
        let opts = catallaxy_compiler::Options { check_only: true };
        let _ = catallaxy_compiler::compile_source(std::path::Path::new("fuzz.cx"), src, &opts);
    }
});
