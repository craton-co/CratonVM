// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Prints the live Rust layout contracts used by the interpreter, JIT and GC.

use cratonvm_types::{
    CompactValue, ObjectHeader, ObjectRef, RawSlot, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE,
    IDENTITY_HASH_CODE_OFFSET, MARK_FORWARDED, MARK_STATE_MASK, MARK_WORD_OFFSET,
    REF_ELEMENT_SIZE, REF_FIELD_SIZE, SLOT_SIZE,
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
    row("MARK_WORD_OFFSET", MARK_WORD_OFFSET);
    row("ARRAY_LENGTH_OFFSET", ARRAY_LENGTH_OFFSET);
    // `FORWARDING_PTR_OFFSET` was printed here until 2026-08-06. The field is
    // gone: the header carried TWO forwarding mechanisms and the mark word is
    // the surviving one, so relocation is now a mark-word state rather than a
    // dedicated slot. Printing the encoding keeps this probe answering the
    // same question ("where does a forwarding pointer live?") after the answer
    // changed, instead of failing to compile in silence -- which is what it
    // did, because nothing but this example reads the constant.
    row("MARK_FORWARDED (state bits)", MARK_FORWARDED as usize);
    row("MARK_STATE_MASK", MARK_STATE_MASK as usize);
}
