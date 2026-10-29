// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Test helpers - load real `.class` files compiled by `build.rs` into
//! the crate's generated fixture directory.
//!
//! **No synthetic bytecode** anywhere in this module. The whole point
//! of this fixture set is to exercise the real reader/analyzer/emitter
//! path against real Java compilation output. If you find yourself
//! about to write `vec![0xCA, 0xFE, 0xBA, 0xBE, ...]`, stop and add a
//! Java source under `test_classes/gpu/` instead.

#![cfg(test)]

use cratonvm_reader::class_reader::read_class;
use cratonvm_reader::method::ClassFileMethod;

/// Load `class_name.class` from the fixtures directory and return the
/// named method. Panics if the class or method cannot be found - tests
/// that depend on a fixture should fail loudly when it goes missing.
///
/// The reader builds every attribute as a `LazyAttribute::Raw` and
/// never structurally parses it; `ClassFileMethod::code()` only returns
/// `Some` once the `Code` attribute has been *force-decoded* (see its
/// doc comment). The analyzer and lowering pipeline both call `code()`,
/// so this helper force-decodes every method's attributes against the
/// class's constant pool before handing the method back — otherwise
/// every fixture method would look like it had no `Code` attribute and
/// the analyzer would (wrongly) reject it with `Reason::NoCode`.
pub fn load_method(class_name: &str, method_name: &str, descriptor: &str) -> ClassFileMethod {
    let path = fixture_path(class_name);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    let mut class = read_class(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
    // The reader keeps method attributes lazy (`LazyAttribute::Raw`);
    // `ClassFileMethod::code()` only returns an already-decoded Code
    // attribute. Force-decode the `Code` attribute (in place, while the
    // constant pool is still borrowable) so the analyzer can see the
    // method body — otherwise `analyze()` returns `Rejected(NoCode)`.
    // Only `Code` is decoded: force-decoding every attribute can hit a
    // ByteView range panic on certain malformed annotation attributes
    // (`reader/src/byte_view.rs` — a separate reader-crate issue), and
    // the analyzer only needs the method body anyway.
    let cp = &class.constant_pool;
    for method in class.methods.iter_mut() {
        for attr in method.attributes.iter_mut() {
            if attr.name() == "Code" {
                let _ = attr.decode(cp);
            }
        }
    }
    class
        .methods
        .into_iter()
        .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)
        .unwrap_or_else(|| {
            panic!(
                "method {method_name}{descriptor} not found in {} \
                 (did the Java source change without recompiling?)",
                path.display()
            )
        })
}

fn fixture_path(class_name: &str) -> std::path::PathBuf {
    let fixture_dir = env!("JIT_CUDA_FIXTURE_DIR");
    std::path::Path::new(fixture_dir).join(format!("{class_name}.class"))
}
