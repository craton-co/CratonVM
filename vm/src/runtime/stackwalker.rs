// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.9 — `java.lang.StackWalker` / full stack-trace machinery.
//!
//! This module hosts the helpers used by:
//!
//! * `java.lang.Throwable.fillInStackTrace` / `getStackTrace` (via
//!   `NativeContext::capture_stack_trace`) — frames are populated with
//!   source file, line number, and bytecode index.
//! * `java.lang.StackWalker.walk(Function)` / `forEach(Consumer)` — see
//!   `native-builtins/src/phases_late.rs::p59_sw_walk`, which consumes
//!   `StackTraceEntry.byte_code_index` / `line_number` populated here.
//! * `java.lang.StackWalker.StackFrame.getLineNumber()` /
//!   `getByteCodeIndex()` / `getDeclaringClass()` / `getClassName()` /
//!   `getMethodName()` — see
//!   `native-builtins/src/lang_stackwalker.rs::register_lang_stackwalker`.
//!
//! Line-number lookup walks the method's `LineNumberTable` attribute
//! (JVM spec §4.7.12). The table is a sorted `(start_pc, line_number)`
//! list; for any `bci` the applicable line is the largest `start_pc`
//! that is `<= bci`.
//!
//! Bytecode index (BCI) comes from the frame's `last_instr_pc` (the PC
//! of the last-executed instruction, which is what HotSpot exposes via
//! `StackFrame.getByteCodeIndex()` — note that `pc` itself may already
//! have been advanced past the invoke instruction when the frame sits
//! above the invocation site).

use std::sync::Arc;

use cratonvm_reader::attribute::{Attribute, LineNumberEntry};

use crate::classloading::{Class, ClassId, ClassStore};
use crate::native::registry::StackTraceEntry;
use crate::runtime::frame::Frame;

/// Sentinel "unknown line number" value — `-1` matches HotSpot's
/// `StackFrame.getLineNumber()` contract for frames whose `LineNumberTable`
/// attribute is absent.
pub const LINE_NUMBER_UNKNOWN: i32 = -1;

/// Sentinel "native method" line number — `-2` matches HotSpot's
/// `StackTraceElement.getLineNumber()` contract for native frames.
pub const LINE_NUMBER_NATIVE: i32 = -2;

/// Look up the source-line corresponding to `bci` in the method's
/// `LineNumberTable` attribute.
///
/// Returns `None` when:
/// * the class isn't in the store (synthetic / bootstrap frame),
/// * the method has no `Code` attribute (abstract / native),
/// * the method has no `LineNumberTable` attribute, or
/// * `bci` precedes the first entry.
///
/// The table is scanned linearly; in practice tables are ≤16 entries for
/// typical methods, so a binary search is not worth the code size. For
/// pathological cases (huge generated lambdas with >1k entries) the scan
/// is still O(n) but with trivial per-iteration cost.
pub fn line_number_for_bci(
    class_store: &ClassStore,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    bci: usize,
) -> Option<i32> {
    let class = class_store.get(class_id)?;
    let method = class.find_method(method_name, method_descriptor)?;
    let code = method.code()?;

    // The LineNumberTable attribute nests under Code.attributes.
    let mut best_line: Option<u16> = None;
    let mut best_start: u16 = 0;
    let bci_u16 = bci.min(u16::MAX as usize) as u16;

    for attr in &code.attributes {
        if let Attribute::LineNumberTable(entries) = attr {
            // Multiple LineNumberTable attributes are permitted by the spec
            // (a compiler may split them across multiple attributes); the
            // union is the effective table. We walk all of them.
            for entry in entries {
                if entry.start_pc <= bci_u16
                    && (best_line.is_none() || entry.start_pc >= best_start)
                {
                    best_start = entry.start_pc;
                    best_line = Some(entry.line_number);
                }
            }
        }
    }

    best_line.map(|l| l as i32)
}

/// Collect all `LineNumberTable` entries declared for the given method.
/// Exposed primarily for debugging / tests; returns a flattened vector
/// sorted by `start_pc` ascending.
pub fn line_number_entries(
    class_store: &ClassStore,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> Vec<LineNumberEntry> {
    let Some(class) = class_store.get(class_id) else {
        return Vec::new();
    };
    let Some(method) = class.find_method(method_name, method_descriptor) else {
        return Vec::new();
    };
    let Some(code) = method.code() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for attr in &code.attributes {
        if let Attribute::LineNumberTable(entries) = attr {
            out.extend_from_slice(entries);
        }
    }
    out.sort_by_key(|e| e.start_pc);
    out
}

/// Build a single `StackTraceEntry` from a live [`Frame`] plus the
/// enclosing class store, resolving source-file and line-number info
/// when available.
///
/// The BCI written into the entry is the frame's `last_instr_pc` — this
/// is what HotSpot exposes via `StackFrame.getByteCodeIndex()`.
pub fn entry_from_frame(class_store: &ClassStore, frame: &Frame) -> StackTraceEntry {
    let class_name = frame.class_name_arc();
    let method_name = frame.method_name_arc();
    let method_descriptor = frame.method_descriptor_arc();
    let source_file = frame.source_file_arc();
    let bci = frame.last_instr_pc;
    let bci_i32 = bci.min(i32::MAX as usize) as i32;

    let line_number = line_number_for_bci(
        class_store,
        frame.class_id,
        &method_name,
        &method_descriptor,
        bci,
    )
    .unwrap_or(LINE_NUMBER_UNKNOWN);

    StackTraceEntry {
        class_name,
        method_name,
        source_file,
        line_number,
        byte_code_index: bci_i32,
        class_id: Some(frame.class_id),
    }
}

/// Walk a frame slice and produce a `StackTraceEntry` vector with full
/// source-file / line-number / BCI data.
///
/// Iteration order is top-of-stack → bottom (i.e. the caller chain from
/// innermost to outermost), matching the order produced by
/// `JvmThread.frames.iter()`.
pub fn capture_full_trace(class_store: &ClassStore, frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| entry_from_frame(class_store, f))
        .collect()
}

/// Like [`capture_full_trace`] but WITHOUT resolving source-line numbers — so it
/// needs no `ClassStore` and takes no lock. Used to publish a per-thread frame
/// snapshot at blocking deposit points (see `deposit_root_snapshot`) for
/// cross-thread `Thread.getStackTrace()` / `dumpThreads()`: the diagnostic only
/// needs `class.method` (+ BCI) to pinpoint where a parked thread is stuck, and
/// keeping it lock-free keeps it safe to call from every deposit site. The line
/// number is left `UNKNOWN` (callers may resolve it lazily from the BCI).
pub fn capture_frames_no_lines(frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            source_file: f.source_file_arc(),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: Some(f.class_id),
        })
        .collect()
}

/// Synthesize a synthetic "no source info" entry. Used for native frames
/// injected from outside the interpreter (JNI up-calls, host-side
/// StackWalker probes during VM bootstrap).
pub fn synthetic_entry(class_name: Arc<str>, method_name: Arc<str>) -> StackTraceEntry {
    StackTraceEntry {
        class_name,
        method_name,
        source_file: None,
        line_number: LINE_NUMBER_NATIVE,
        byte_code_index: -1,
        class_id: None,
    }
}

/// Consult a [`Class`]'s attributes for the `SourceFile` attribute.
/// Fallback for frames whose cached `source_file_arc()` is `None`.
pub fn source_file_of_class(class: &Class) -> Option<Arc<str>> {
    class.source_file.as_deref().map(Arc::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::method::ClassFileMethod;

    fn make_method_with_line_table(entries: Vec<LineNumberEntry>) -> ClassFileMethod {
        let code = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: cratonvm_reader::ByteView::from_vec(vec![0xb1]), // return
            exception_table: Vec::new(),
            attributes: vec![Attribute::LineNumberTable(entries)],
        };
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("probe"),
            descriptor: Arc::from("()V"),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(code),
            )],
        }
    }

    #[test]
    fn line_table_picks_largest_start_leq_bci() {
        let method = make_method_with_line_table(vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 10,
            },
            LineNumberEntry {
                start_pc: 5,
                line_number: 20,
            },
            LineNumberEntry {
                start_pc: 9,
                line_number: 30,
            },
        ]);
        // Reimplement the core scan logic here against the method directly
        // (the `line_number_for_bci` entry point requires a ClassStore and
        // is covered by integration tests in vm/tests/wp1_9_*).
        let code = method.code().unwrap();
        let lnt = match &code.attributes[0] {
            Attribute::LineNumberTable(e) => e,
            _ => panic!(),
        };

        let lookup = |bci: u16| -> Option<u16> {
            let mut best = None;
            let mut best_start = 0;
            for e in lnt {
                if e.start_pc <= bci && (best.is_none() || e.start_pc >= best_start) {
                    best_start = e.start_pc;
                    best = Some(e.line_number);
                }
            }
            best
        };

        assert_eq!(lookup(0), Some(10));
        assert_eq!(lookup(3), Some(10));
        assert_eq!(lookup(5), Some(20));
        assert_eq!(lookup(8), Some(20));
        assert_eq!(lookup(9), Some(30));
        assert_eq!(lookup(42), Some(30));
    }

    #[test]
    fn line_table_multiple_attrs_combined() {
        // A method with two LineNumberTable attributes (permitted by spec).
        let code = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
            exception_table: Vec::new(),
            attributes: vec![
                Attribute::LineNumberTable(vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 5,
                }]),
                Attribute::LineNumberTable(vec![LineNumberEntry {
                    start_pc: 10,
                    line_number: 99,
                }]),
            ],
        };
        let method = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("probe"),
            descriptor: Arc::from("()V"),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(code),
            )],
        };
        let code = method.code().unwrap();

        // Simulate the union semantics `line_number_for_bci` uses.
        let mut best = None;
        let mut best_start = 0;
        for attr in &code.attributes {
            if let Attribute::LineNumberTable(entries) = attr {
                for e in entries {
                    if e.start_pc <= 12 && (best.is_none() || e.start_pc >= best_start) {
                        best_start = e.start_pc;
                        best = Some(e.line_number);
                    }
                }
            }
        }
        assert_eq!(best, Some(99));
    }

    #[test]
    fn synthetic_entry_has_native_sentinel() {
        let e = synthetic_entry(Arc::from("java/lang/Object"), Arc::from("hashCode"));
        assert_eq!(e.line_number, LINE_NUMBER_NATIVE);
        assert_eq!(e.byte_code_index, -1);
        assert!(e.source_file.is_none());
    }

    #[test]
    fn unknown_is_minus_one() {
        assert_eq!(LINE_NUMBER_UNKNOWN, -1);
    }

    #[test]
    fn native_is_minus_two() {
        assert_eq!(LINE_NUMBER_NATIVE, -2);
    }
}
