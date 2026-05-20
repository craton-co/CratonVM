//! Fuzz target for the .class file parser.
//!
//! Run with: cargo +nightly fuzz run fuzz_classfile
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The parser must never panic on arbitrary input.
    let _ = cratonvm_reader::ClassFile::parse(data);
});
