//! Test helpers — load real `.class` files compiled by `build.rs` from
//! `test_classes/gpu/`.
//!
//! **No synthetic bytecode** anywhere in this module. The whole point
//! of this fixture set is to exercise the real reader/analyzer/emitter
//! path against real Java compilation output. If you find yourself
//! about to write `vec![0xCA, 0xFE, 0xBA, 0xBE, ...]`, stop and add a
//! Java source under `test_classes/gpu/` instead.

#![cfg(test)]

use rustjvm_reader::attribute::force_decode_all;
use rustjvm_reader::class_reader::read_class;
use rustjvm_reader::method::ClassFileMethod;

/// Load `class_name.class` from the fixtures directory and return the
/// named method. Panics if the class or method cannot be found — tests
/// that depend on a fixture should fail loudly when it goes missing.
pub fn load_method(class_name: &str, method_name: &str, descriptor: &str) -> ClassFileMethod {
    let path = fixture_path(class_name);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    let class = read_class(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
    let mut method = class
        .methods
        .into_iter()
        .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)
        .unwrap_or_else(|| {
            panic!(
                "method {method_name}{descriptor} not found in {} \
                 (did the Java source change without recompiling?)",
                path.display()
            )
        });
    // `read_class` leaves method attributes (including `Code`) lazily
    // undecoded; `ClassFileMethod::code()` returns `None` for a `Raw`
    // attribute. Force-decode so the analyzer/lowerer see the bytecode.
    force_decode_all(&mut method.attributes, &class.constant_pool)
        .unwrap_or_else(|e| panic!("failed to decode attributes of {method_name}{descriptor}: {e:?}"));
    method
}

fn fixture_path(class_name: &str) -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR points at jit-cuda/. Fixtures live one level
    // up under test_classes/gpu/.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent()
        .expect("workspace root is jit-cuda/..")
        .join("test_classes")
        .join("gpu")
        .join(format!("{class_name}.class"))
}
