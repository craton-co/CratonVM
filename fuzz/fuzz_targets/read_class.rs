//! Fuzz target for the high-level `reader::read_class` entry point.
//!
//! Complements `fuzz_classfile` (which targets `ClassFile::parse` directly)
//! by driving the parser through the same surface used by the VM
//! classloader. The parser MUST NEVER panic on arbitrary input.
//!
//! Run with: cargo +nightly fuzz run read_class
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `read_class` is the workspace entry point — same byte-slice surface
    // exposed to the VM. A `Result` (any `Err`) is acceptable; only a
    // panic / abort indicates a parser bug.
    let _ = cratonvm_reader::read_class(data);
});
