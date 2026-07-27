// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Prints the live Rust layout contracts used by the interpreter, JIT and GC.

use cratonvm_types::{
    CompactValue, ObjectHeader, ObjectRef, RawSlot, Value, FORWARDING_PTR_OFFSET, HEADER_SIZE,
    IDENTITY_HASH_CODE_OFFSET, MARK_WORD_OFFSET, REF_ELEMENT_SIZE, REF_FIELD_SIZE, SLOT_SIZE,
};

fn row(name: &str, value: usize) {
    println!("{name}\t{value}");
}

fn main() {
    row("pointer_size", std::mem::size_of::<usize>());
    row("Value.size", std::mem::size_of::<Value>());
    row("Value.align", std::mem::align_of::<Value>());
    row("CompactValue.size", std::mem::size_of::<CompactValue>());
    row("RawSlot.size", std::mem::size_of::<RawSlot>());
    row("ObjectRef.size", std::mem::size_of::<ObjectRef>());
    row(
        "Option<ObjectRef>.size",
        std::mem::size_of::<Option<ObjectRef>>(),
    );
    row("ObjectHeader.size", std::mem::size_of::<ObjectHeader>());
    row("ObjectHeader.align", std::mem::align_of::<ObjectHeader>());
    row("HEADER_SIZE", HEADER_SIZE);
    row("SLOT_SIZE", SLOT_SIZE);
    row("REF_FIELD_SIZE", REF_FIELD_SIZE);
    row("REF_ELEMENT_SIZE", REF_ELEMENT_SIZE);
    row("IDENTITY_HASH_CODE_OFFSET", IDENTITY_HASH_CODE_OFFSET);
    row("FORWARDING_PTR_OFFSET", FORWARDING_PTR_OFFSET);
    row("MARK_WORD_OFFSET", MARK_WORD_OFFSET);
}
