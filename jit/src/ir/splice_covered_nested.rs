// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 7 (lane T1L1, CF23-1 of
//! `docs/internal/proposals/jit/tier1/jit-r23-w1-L1-proposals.md`, and the
//! DECLOOP page
//! `docs/known-issues/jit/j27-covered-try-nested-bodies-stay-calls-charsetdecoder-wrap-proposal-20261028.md`):
//! a nested body inside a spliced callee's covered `try` range kept SPLICED
//! when every way it can leave is a guard deopt (which resumes in the
//! interpreter through the chain, where the callee's real table runs) or an
//! exit the covering handlers cannot catch.
//!
//! Today every nested row there is turned back into its call
//! (`lib.rs` `ir_splice_strip_handler_covered_nested`), and a row whose call
//! has no surviving form refuses the whole site
//! (`ir-splice-callee-handler-covered-nested-body`). The reason is sound: a
//! raise inside a spliced body leaves through the OUTERMOST `invoke` and would
//! skip the enclosing callee's `catch`. The three opt-in switches below keep
//! that rule while widening what may stand in the range:
//!
//! * `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED`
//!   ([`ir_splice_covered_nested_guarded_enabled`]): the planner keeps a
//!   covered nested row spliced when its body passes
//!   [`covered_nested_row_admitted`] (no handler table, not synchronized, no
//!   indy rows, unguarded; forward branches only; every instruction one the
//!   covered-range rule admits, a field access, or an invoke that is itself
//!   such a nested row or an elided `Object.<init>`). The builder then checks
//!   EVERY instruction of that body as if it stood in the covered range
//!   ([`IrBuilder::enter_covered_nested_instruction`]: the per-instruction
//!   `splice_handler_nodes_armed` check, at any depth under a covered
//!   `invoke`), and puts the body's combined-pc span on
//!   `Graph::splice_handler_covered`, so the lowering's
//!   `refuse_unarmed_splice_handler_exits` judges its nodes too. An unarmed
//!   exit anywhere in it still refuses the compile (the W5-2 rebuild drops the
//!   splice): the planner's opcode list only avoids planning bodies that
//!   would certainly be refused.
//! * `CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN`
//!   ([`ir_splice_handler_lower_proven_enabled`]): the builder already admits
//!   a field access whose base it proved non-null inside a covered range
//!   (`CRATONVM_JIT_IR_SPLICE_HANDLER_NON_NULL_FIELD_ACCESS`, default ON), but
//!   the lowering's check knew nothing of the proof and refused every
//!   `Op::Load` / `Op::Store` there (`may_raise`), so such a compile was
//!   refused one step later. Under this switch the builder records the nodes
//!   it proved ([`IrBuilder::note_splice_handler_proven_exits`],
//!   `Graph::splice_handler_proven_exits`) and the lowering admits exactly
//!   those ([`splice_handler_exit_proven`]).
//! * `CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH`
//!   ([`ir_splice_covered_new_non_error_catch_enabled`]): refinement 1 of the
//!   DECLOOP page. An `Op::New` raises only `Error`s (`OutOfMemoryError`, or a
//!   linkage / initialization `Error`: JVMS 5.5 wraps a non-`Error` from
//!   `<clinit>` in `ExceptionInInitializerError`). When no row covering the
//!   instruction -- in its own body, and in every enclosing spliced body at
//!   the `invoke` that leads to it -- names a catch type that can catch an
//!   `Error` ([`catch_type_cannot_catch_error`], a whitelist of JDK
//!   `Exception` classes), the raise leaving through the outermost `invoke`
//!   is exactly what the interpreter would do: no spliced frame on the way
//!   catches it, and the compiling method's own table is consulted at that
//!   `invoke`. `ByteBuffer.wrap`'s `new HeapByteBuffer` under its
//!   `catch (IllegalArgumentException)` is the shape. Its node is recorded as
//!   proven for the lowering like a non-null access (so it needs
//!   `..._LOWER_PROVEN` too).
//!
//! Every switch is read per decision; no static.

use super::{
    splice_handler_range_division_admitted, splice_handler_range_opcode_admitted, Graph,
    IrBuilder, Node, NodeId, Op,
};

/// `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED` (j27 wave 7, lane T1L1;
/// perf, **opt-in** `=1`, unmeasured): see the module doc. Acts only under
/// the callee-handler family (a body with handler rows is spliced only there).
pub fn ir_splice_covered_nested_guarded_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED")
}

/// `CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN` (j27 wave 7, lane T1L1;
/// perf, **opt-in** `=1`): the lowering's covered-range check admits the
/// field accesses and allocations the builder proved cannot deliver anything
/// a covering handler catches. See the module doc.
pub fn ir_splice_handler_lower_proven_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN")
}

/// `CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH` (j27 wave 7, lane
/// T1L1; perf, **opt-in** `=1`): an `Op::New` inside a covered range whose
/// covering rows cannot catch an `Error`. See the module doc.
pub fn ir_splice_covered_new_non_error_catch_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH")
}

/// How deep a chain of covered nested bodies the planner admits.
const COVERED_NESTED_MAX_DEPTH: usize = 8;

/// JDK `Exception` classes (never an `Error`, never `Throwable`): a row with
/// one of these catch types cannot catch what an allocation raises. Only
/// `java.*` names, which no class loader but the boot loader may define, so
/// the name alone decides the class.
const NON_ERROR_CATCH_TYPES: &[&str] = &[
    "java/lang/Exception",
    "java/lang/RuntimeException",
    "java/lang/IllegalArgumentException",
    "java/lang/IllegalStateException",
    "java/lang/IndexOutOfBoundsException",
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/StringIndexOutOfBoundsException",
    "java/lang/NullPointerException",
    "java/lang/ClassCastException",
    "java/lang/ArithmeticException",
    "java/lang/NumberFormatException",
    "java/lang/UnsupportedOperationException",
    "java/lang/CloneNotSupportedException",
    "java/lang/InterruptedException",
    "java/lang/ReflectiveOperationException",
    "java/io/IOException",
    "java/io/UncheckedIOException",
    "java/nio/BufferOverflowException",
    "java/nio/BufferUnderflowException",
    "java/nio/charset/CharacterCodingException",
    "java/util/NoSuchElementException",
    "java/util/ConcurrentModificationException",
];

/// Can a handler whose catch type is `name` (internal or dotted form) never
/// catch an `Error`? Only for a listed JDK `Exception` class; anything else
/// (`Throwable`, `Error` and its subclasses, a catch-any `""`, an
/// application class whose hierarchy the name does not tell) answers `false`.
pub fn catch_type_cannot_catch_error(name: &str) -> bool {
    NON_ERROR_CATCH_TYPES.iter().any(|w| {
        w.len() == name.len()
            && w
                .bytes()
                .zip(name.bytes())
                .all(|(a, b)| a == b || (a == b'/' && b == b'.'))
    })
}

/// The planner half of `..._COVERED_NESTED_GUARDED`: may the nested row `n`,
/// at an `invoke` inside a covered `try` of its enclosing spliced body, stay
/// spliced? Read by `lib.rs` `ir_splice_strip_handler_covered_nested`.
pub(crate) fn covered_nested_row_admitted(n: &crate::NestedInlineSite) -> bool {
    ir_splice_covered_nested_guarded_enabled() && row_admitted(n, 0)
}

fn row_admitted(n: &crate::NestedInlineSite, depth: usize) -> bool {
    depth < COVERED_NESTED_MAX_DEPTH
        && n.guard_class_id == 0
        && n.bimorphic_second.is_none()
        && body_admitted(&n.site, depth)
}

fn body_admitted(site: &crate::InlineSite, depth: usize) -> bool {
    if site.callee_is_synchronized
        || site.class_id == 0
        || !site.ir_handler_rows.is_empty()
        || !site.ir_indy_rows.is_empty()
    {
        return false;
    }
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    if len == 0 {
        return false;
    }
    let mut pc = 0usize;
    while pc < len {
        if !opcode_admitted(site, len, pc, depth) {
            return false;
        }
        pc += crate::bytecode_analysis::step(code, pc).max(1);
    }
    true
}

/// One instruction of a covered nested body. Forward branches only (a back
/// edge polls, and the rotated-loop walk is not checked here); a field access
/// (its base must be proven non-null, which the builder checks node by node);
/// an invoke only into a nested row admitted the same way, or one the
/// resolver elided (`Object.<init>`); otherwise the covered-range opcode rule.
fn opcode_admitted(site: &crate::InlineSite, len: usize, pc: usize, depth: usize) -> bool {
    let code = &site.callee_code;
    match code[pc] {
        0x99..=0xa7 | 0xc6 | 0xc7 => crate::bytecode_analysis::offset_branch_target(code, pc)
            .is_some_and(|t| t > pc && t < len),
        0xac..=0xb1 | 0xb4 | 0xb5 => true,
        0xb6..=0xb9 => {
            site.elided_invoke_pcs.contains(&pc)
                || site
                    .nested_sites
                    .iter()
                    .any(|n| n.callee_pc == pc && row_admitted(n, depth + 1))
        }
        op => {
            splice_handler_range_opcode_admitted(op)
                || splice_handler_range_division_admitted(code, len, pc)
        }
    }
}

/// Does the chain of open splices, outermost first as `(base, invoke_pc)`,
/// hold a body opened at an `invoke` its parent covers (`covers(parent base,
/// invoke pc)`)? Then everything walked in the innermost body lies, at some
/// depth, under that covered `invoke`.
pub(super) fn covered_nested_chain(
    frames: &[(usize, usize)],
    covers: impl Fn(usize, usize) -> bool,
) -> bool {
    frames.windows(2).any(|w| covers(w[0].0, w[1].1))
}

/// The `Op::Load` / `Op::Store` / `Op::New` nodes from `mark` on: the ones a
/// passed `splice_handler_nodes_armed` check proved (it admits no other such
/// node).
pub(super) fn proven_exit_nodes_from(nodes: &[Node], mark: usize) -> Vec<NodeId> {
    nodes
        .iter()
        .enumerate()
        .skip(mark)
        .filter(|(_, n)| matches!(n.op, Op::Load(_) | Op::Store(_) | Op::New { .. }))
        // Cast: node ids index `graph.nodes`, bounded by the builder.
        .map(|(id, _)| id as NodeId)
        .collect()
}

/// The lowering half of `..._LOWER_PROVEN`: did the builder prove node `id`
/// (stamped inside a covered range) delivers nothing a covering handler
/// catches? Read by `ir_lower` `refuse_unarmed_splice_handler_exits`.
pub fn splice_handler_exit_proven(graph: &Graph, id: NodeId) -> bool {
    ir_splice_handler_lower_proven_enabled() && graph.splice_handler_proven_exits.contains(&id)
}

impl IrBuilder {
    /// The builder half of `..._COVERED_NESTED_GUARDED`, asked once per
    /// instruction before it is built: is the innermost open splice (at any
    /// depth) under a covered `invoke` of an enclosing spliced body? Then the
    /// instruction is checked like one in the covered range itself, and the
    /// body's span goes on `Graph::splice_handler_covered` (once) for the
    /// lowering's check.
    pub(super) fn enter_covered_nested_instruction(&mut self) -> bool {
        if self.splice.len() < 2 || !ir_splice_covered_nested_guarded_enabled() {
            return false;
        }
        let frames: Vec<(usize, usize)> = self.splice.iter().map(|f| (f.base, f.invoke_pc)).collect();
        let rows = &self.splice_handler_rows;
        let covered = covered_nested_chain(&frames, |base, pc| {
            rows.get(&base)
                .is_some_and(|rows| rows.iter().any(|r| r.start_pc <= pc && pc < r.end_pc))
        });
        if !covered {
            return false;
        }
        if let Some(f) = self.splice.last() {
            let span = (f.base, f.end);
            if !self.graph.splice_handler_covered.contains(&span) {
                self.graph.splice_handler_covered.push(span);
            }
        }
        true
    }

    /// `..._COVERED_NEW_NON_ERROR_CATCH`: may an `Op::New` built by the
    /// instruction at combined pc `at` stand in a covered range? Only when
    /// `at` lies in the innermost open body and every row covering it -- that
    /// body's rows at `at`, and each enclosing body's rows at the `invoke` that
    /// opened the next -- names a catch type that cannot catch an `Error`.
    pub(super) fn splice_handler_new_admitted(&self, at: usize) -> bool {
        if !ir_splice_covered_new_non_error_catch_enabled() {
            return false;
        }
        let Some(inner) = self.splice.last() else {
            return false;
        };
        if at < inner.base || at >= inner.end {
            return false;
        }
        let mut names: Vec<&'static str> = Vec::new();
        let mut collect = |base: usize, pc: usize| {
            if let Some(rows) = self.splice_handler_rows.get(&base) {
                names.extend(
                    rows.iter()
                        .filter(|r| r.start_pc <= pc && pc < r.end_pc)
                        .map(|r| r.catch_name),
                );
            }
        };
        collect(inner.base, at);
        for w in self.splice.windows(2) {
            collect(w[0].base, w[1].invoke_pc);
        }
        !names.is_empty() && names.iter().all(|n| catch_type_cannot_catch_error(n))
    }

    /// `..._LOWER_PROVEN`: after the instruction whose nodes start at `mark`
    /// passed `splice_handler_nodes_armed`, record its field accesses and
    /// allocations (each one proven there) for the lowering.
    pub(super) fn note_splice_handler_proven_exits(&mut self, mark: usize) {
        if !ir_splice_handler_lower_proven_enabled() {
            return;
        }
        let ids = proven_exit_nodes_from(&self.graph.nodes, mark);
        self.graph.splice_handler_proven_exits.extend(ids);
    }
}
