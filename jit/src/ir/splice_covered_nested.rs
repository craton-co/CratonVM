// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Nested splices inside an enclosing callee's covered `try` range.
//!
//! The planner screens every descendant under a covered invoke. Admitted
//! bodies have no handler table, synchronization or indy rows, and only
//! forward branches. An invoke either opens another admitted body or has a
//! surviving call form. Guarded rows use one class test and arm the ordinary
//! miss call with the enclosing scope's handler edges.
//!
//! Every instruction of an admitted body is checked by
//! `IrBuilder::splice_handler_nodes_armed`, and the body's relocated span joins
//! `Graph::splice_handler_covered` for an independent lowering check. Calls
//! inherit the table covering the invoke that opened the nested chain, with
//! that scope's parked locals and the call's resulting memory. Handler entry
//! deopts into that scope after the completed call. An unarmed exit still
//! refuses the compile, including chains with multiple covering scopes.
//!
//! The builder records its proven field accesses and active-class static reads
//! for lowering. Covered array lengths carry a null guard and require their
//! own precise chain snapshot; a null resumes at the original bytecode rather
//! than skipping a catch or replaying a completed prefix. Allocation under
//! whitelisted JDK Exception handlers is admitted only when no enclosing table
//! can catch the Error it may raise. No process-global compatibility state.
//!
//! See the covered-nested retirement report and the CF23-1 section of
//! `docs/feature-designs/jit-ir-optimizations.md`.

use super::{
    splice_handler_range_division_admitted, splice_handler_range_opcode_admitted, Graph, IrBuilder,
    Node, NodeId, Op,
};

/// `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED` (j27 wave 7, lane T1L1;
/// perf, default ON, `=0` disables): see the module doc. Acts only under
/// the callee-handler family (a body with handler rows is spliced only there).
pub fn ir_splice_covered_nested_guarded_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN` (j27 wave 7, lane T1L1;
/// perf, default ON, `=0` disables): the lowering's covered-range check admits the
/// field accesses and allocations the builder proved cannot deliver anything
/// a covering handler catches. See the module doc.
pub fn ir_splice_handler_lower_proven_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH` (j27 wave 7, lane
/// T1L1; perf, default ON, `=0` disables): an `Op::New` inside a covered range whose
/// covering rows cannot catch an `Error`. See the module doc.
pub fn ir_splice_covered_new_non_error_catch_enabled() -> bool {
    true
}

/// How deep a chain of covered nested bodies the planner admits. Two nested
/// eight nested bodies preserve useful constructor and decoder chains while
/// bounding their total bytecode walk.
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
pub(crate) fn handler_is_bare_athrow(code: &[u8], handler_pc: usize) -> bool {
    code.get(handler_pc) == Some(&0xbf)
        || matches!(
            code.get(handler_pc..handler_pc + 3),
            Some(&[st, ld, 0xbf]) if (0x4b..=0x4e).contains(&st) && ld == st - 0x4b + 0x2a
        )
        || matches!(
            code.get(handler_pc..handler_pc + 5),
            Some(&[0x3a, i1, 0x19, i2, 0xbf]) if i1 == i2
        )
}

pub(super) fn rows_cannot_catch_error(rows: &[&super::IrSpliceHandlerRow], code: &[u8]) -> bool {
    for r in rows {
        if catch_type_cannot_catch_error(r.catch_name) {
            continue;
        }
        if r.catch_name == "java/lang/Error"
            && (r.bare_athrow || handler_is_bare_athrow(code, r.handler_pc))
        {
            return true;
        }
        return false;
    }
    true
}

pub fn catch_type_cannot_catch_error(name: &str) -> bool {
    NON_ERROR_CATCH_TYPES.iter().any(|w| {
        w.len() == name.len()
            && w.bytes()
                .zip(name.bytes())
                .all(|(a, b)| a == b || (a == b'/' && b == b'.'))
    })
}

/// The planner half of `..._COVERED_NESTED_GUARDED`: may the nested row `n`,
/// at an `invoke` inside a covered `try` of its enclosing spliced body, stay
/// spliced? Read by `lib.rs` `ir_splice_strip_handler_covered_nested`.
pub(crate) fn covered_nested_row_admitted(n: &crate::NestedInlineSite) -> bool {
    covered_nested_row_admitted_at_depth(n, 0)
}

pub(crate) fn covered_nested_row_admitted_at_depth(
    n: &crate::NestedInlineSite,
    depth: usize,
) -> bool {
    covered_nested_row_admitted_with_policy(n, depth, false)
}

pub(crate) fn covered_nested_row_admitted_with_policy(
    n: &crate::NestedInlineSite,
    depth: usize,
    non_error_catch: bool,
) -> bool {
    ir_splice_covered_nested_guarded_enabled() && row_admitted(n, depth, non_error_catch)
}

fn row_admitted(n: &crate::NestedInlineSite, depth: usize, non_error_catch: bool) -> bool {
    depth < COVERED_NESTED_MAX_DEPTH && body_admitted(&n.site, depth, non_error_catch)
}

fn body_admitted(site: &crate::InlineSite, depth: usize, non_error_catch: bool) -> bool {
    let handlers_ok = (site.ir_handler_rows.is_empty()
        && site.callee_original_handler_rows.is_empty())
        || (super::ir_splice_callee_handlers_wide_admitted()
            && !site.ir_handler_rows.is_empty()
            && site.ir_handler_rows.len() == site.callee_original_handler_rows.len());
    if site.callee_is_synchronized
        || site.class_id == 0
        || !handlers_ok
        || !site.ir_indy_rows.is_empty()
    {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!("[cratonvm-jitc] ir-covered-nested-refused {}.{}{} class={} sync={} handlers={} indy={}",
                site.class_name, site.method_name, site.descriptor, site.class_id,
                site.callee_is_synchronized, site.ir_handler_rows.len(), site.ir_indy_rows.len());
        }
        return false;
    }
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    if len == 0 {
        return false;
    }
    let mut pc = 0usize;
    while pc < len {
        if !opcode_admitted(site, len, pc, depth, non_error_catch) {
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-covered-nested-refused {}.{}{} pc={pc} op=0x{:02x}",
                    site.class_name, site.method_name, site.descriptor, code[pc]
                );
            }
            return false;
        }
        pc += crate::bytecode_analysis::step(code, pc).max(1);
    }
    true
}

/// One instruction of a covered nested body. Forward branches only (a back
/// edge polls, and the rotated-loop walk is not checked here); a field access
/// (its base must be proven non-null, which the builder checks node by node);
/// an invoke into another admitted row, a surviving call, or an elided
/// `Object.<init>`; an active-class static read or a guarded array length;
/// otherwise the covered-range opcode rule.
fn opcode_admitted(
    site: &crate::InlineSite,
    len: usize,
    pc: usize,
    depth: usize,
    non_error_catch: bool,
) -> bool {
    let code = &site.callee_code;
    match code[pc] {
        0x99..=0xa7 | 0xc6 | 0xc7 => crate::bytecode_analysis::offset_branch_target(code, pc)
            .is_some_and(|t| t > pc && t < len),
        0x12 | 0x13 => {
            super::ir_splice_unresolved_ldc_exit_enabled()
                || site.ldc_info.iter().any(|&(at, _)| at == pc)
                || site.ir_ldc_class_info.iter().any(|&(at, _)| at == pc)
        }
        0xaa | 0xab => {
            let mut any = false;
            let mut all_forward = true;
            crate::bytecode_analysis::for_each_switch_target_lenient(code, len, pc, |t| {
                any = true;
                if !(t > pc && t < len) {
                    all_forward = false;
                }
            });
            any && all_forward
        }
        0x2e..=0x35 => super::ir_splice_handler_lower_proven_enabled(),
        0xbb if non_error_catch && super::ir_splice_covered_new_non_error_catch_enabled() => true,
        0xac..=0xb1 | 0xb4 | 0xb5 | 0xbe => true,
        0xbf => super::ir_splice_athrow_exit_enabled(),
        0xb2 => site
            .static_field_info
            .iter()
            .any(|&(at, _, _, _, volatile)| at == pc && !volatile),
        0xb6..=0xb9 => {
            site.elided_invoke_pcs.contains(&pc)
                || crate::ir_splice_nested_call_survives(site, pc)
                || site
                    .nested_sites
                    .iter()
                    .any(|n| n.callee_pc == pc && row_admitted(n, depth + 1, non_error_catch))
        }
        op => {
            splice_handler_range_opcode_admitted(op)
                || (super::ir_splice_callee_handlers_wide_admitted()
                    && super::splice_handler_wide_range_opcode_admitted(op))
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

/// The field/static/array accesses and allocations from `mark` on: the ones a
/// passed `splice_handler_nodes_armed` check proved (it admits no other such
/// node).
pub(super) fn proven_exit_nodes_from(nodes: &[Node], mark: usize) -> Vec<NodeId> {
    nodes
        .iter()
        .enumerate()
        .skip(mark)
        .filter(|(_, n)| {
            matches!(
                n.op,
                Op::Load(_)
                    | Op::Store(_)
                    | Op::LoadStatic { .. }
                    | Op::New { .. }
                    | Op::NewArray { .. }
            )
                || (n.bytecode_pc.is_some()
                    && matches!(n.op, Op::ArrayLoad(_) | Op::ArrayLength | Op::ArrayStore(_)))
        })
        // Cast: node ids index `graph.nodes`, bounded by the builder.
        .map(|(id, _)| id as NodeId)
        .collect()
}

/// The lowering half of `..._LOWER_PROVEN`: did the builder prove node `id`
/// (stamped inside a covered range) delivers nothing a covering handler
/// catches? Read by `ir_lower` `refuse_unarmed_splice_handler_exits`.
pub fn splice_handler_exit_proven(graph: &Graph, id: NodeId) -> bool {
    if !ir_splice_handler_lower_proven_enabled() {
        return false;
    }
    if graph.splice_handler_proven_exits.contains(&id) {
        return true;
    }
    if graph
        .nodes
        .get(id as usize)
        .and_then(|n| n.bytecode_pc)
        .is_some_and(|pc| graph.splice_handler_proven_pcs.contains(&pc))
    {
        return true;
    }
    let Some(node) = graph.nodes.get(id as usize) else {
        return false;
    };
    match &node.op {
        Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } => {
            // A row is added only when the declaring class is already
            // initialized and this getstatic can use the direct, no-helper
            // load. Without that compile-time proof it could run <clinit>,
            // so the covered handler must keep rejecting it.
            graph.plain_static_reads.contains(&(*class_id, *field_index))
                && crate::ir::ir_plain_static_read_enabled()
                && crate::x64::inline_getstatic_enabled()
        }
        // These reads emit null / bounds failures as deopts at the original
        // bytecode PC. Interpretation then runs in the callee frame, so its
        // own exception table decides whether the failure is caught.
        Op::ArrayLoad(_) | Op::ArrayLength => node.bytecode_pc.is_some(),
        _ => false,
    }
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
        let frames: Vec<(usize, usize)> =
            self.splice.iter().map(|f| (f.base, f.invoke_pc)).collect();
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
    pub(super) fn splice_handler_new_admitted(&self, at: usize, code: &[u8]) -> bool {
        if !ir_splice_covered_new_non_error_catch_enabled() {
            return false;
        }
        let Some(inner) = self.splice.last() else {
            return false;
        };
        if at < inner.base || at >= inner.end {
            return false;
        }
        let mut any_rows = false;
        let mut ok = true;
        let mut check_frame = |base: usize, pc: usize| {
            let Some(rows) = self.splice_handler_rows.get(&base) else {
                return;
            };
            let covering: Vec<&super::IrSpliceHandlerRow> = rows
                .iter()
                .filter(|r| r.start_pc <= pc && pc < r.end_pc)
                .collect();
            if covering.is_empty() {
                return;
            }
            any_rows = true;
            if !rows_cannot_catch_error(&covering, code) {
                ok = false;
            }
        };
        check_frame(inner.base, at);
        for w in self.splice.windows(2) {
            check_frame(w[0].base, w[1].invoke_pc);
        }
        any_rows && ok
    }

    /// `..._LOWER_PROVEN`: after the instruction whose nodes start at `mark`
    /// passed `splice_handler_nodes_armed`, record its proven field/static/
    /// array accesses and allocations for the lowering.
    pub(super) fn note_splice_handler_proven_exits(&mut self, mark: usize) {
        if !ir_splice_handler_lower_proven_enabled() {
            return;
        }
        let ids = proven_exit_nodes_from(&self.graph.nodes, mark);
        for &id in &ids {
            if let Some(bci) = self.graph.node_opt(id).and_then(|n| n.bytecode_pc) {
                self.graph.splice_handler_proven_pcs.push(bci);
            }
        }
        self.graph.splice_handler_proven_exits.extend(ids);
    }
}
