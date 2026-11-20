// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Sea-of-Nodes Intermediate Representation for the JIT compiler.
//!
//! The IR sits between bytecode and x86-64 machine code, decoupling
//! optimization from instruction selection.  Nodes live in a flat
//! arena (`Graph`); edges are `NodeId` indices.
//!
//! ## Pipeline
//!
//! ```text
//! bytecode → IrBuilder → Graph → optimize → schedule → lower → x64
//! ```
//!
//! The IR is built incrementally: methods that pass `ir_compatible()`
//! are compiled through this pipeline; others fall back to the
//! single-pass `x64::compile`.

use crate::JitInvokeInfo;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

// JIT j27 wave 2 (lane INTRINS): call sites expanded into plain IR arithmetic
// (`Integer`/`Long.reverse`, `Math.copySign`, `Math`/`StrictMath.signum`).
mod pure_expand;
pub use pure_expand::{try_ir_pure_expansion, PureExpansion};
// j27 wave 6 (lane T1BCE): `Integer` / `Long` `compress` / `expand` as BMI2
// `PEXT` / `PDEP` scalar rows (`ir_lower` reads the encodings).
pub(crate) mod bit_compress;
// j27 wave 4 (lane LATIN1): `Character.isX` over Latin-1 as the JDK table.
mod latin1_char_class;
pub use latin1_char_class::{try_ir_latin1_char_class, Latin1CharClass};
// j27 wave 4 (lane SMALLFIX): IR2-5 switch reads, `StrictMath.abs(F/D)`.
#[cfg(test)]
mod j27_smallfix_tests;
// j27 wave 5 (lane RECORDS, RECORDS21-3): the record fold's String-hash and
// same-object shortcuts ahead of its `Objects` calls.
mod record_fold_shortcuts;
pub(crate) use record_fold_shortcuts::record_fold_admit_string_hash;
#[cfg(test)]
use record_fold_shortcuts::{
    ir_indy_record_fold_ref_identity_enabled, ir_indy_record_fold_string_hash_enabled,
};
#[cfg(test)]
mod record_fold_shortcuts_tests;
// j27 wave 5 (lane PROXY): the exact class-set guard of an interface site
// whose classes all select one inherited body (`AbstractPoolEntry.index()`).
mod cha_class_set;
pub(crate) use cha_class_set::cha_class_set_seed_class;
pub use cha_class_set::{ir_cha_interface_class_set_enabled, CHA_CLASS_SET_MAX};
#[cfg(test)]
mod cha_class_set_tests;
// j27 wave 7 (lane T1L1, CF23-1): a nested body kept spliced inside a spliced
// callee's covered `try`, and the lowering's agreement with the builder's
// proofs there.
mod splice_covered_nested;
pub use splice_covered_nested::{
    catch_type_cannot_catch_error, ir_splice_covered_nested_guarded_enabled,
    ir_splice_covered_new_non_error_catch_enabled, ir_splice_handler_lower_proven_enabled,
    splice_handler_exit_proven,
};
pub(crate) use splice_covered_nested::{
    covered_nested_row_admitted, covered_nested_row_admitted_at_depth,
    covered_nested_row_admitted_with_policy,
};
#[cfg(test)]
mod frozen_fold_tests;
mod sparse_switch;
#[cfg(test)]
mod sparse_switch_tests;
#[cfg(test)]
mod splice_covered_nested_tests;

#[cfg(test)]
mod r15w1_sync_self_lock_builder_tests;

#[cfg(test)]
mod r15w3_sync2_static_self_lock_builder_tests;
/// Is the reachable normal-flow CFG of `code[..code_len]` REDUCIBLE — does
/// every edge that retreats in a depth-first walk from pc 0 land on an
/// instruction that dominates its source? (round 11 wave 3)
///
/// The builder turns every such target into a loop header: φs seeded from the
/// forward entries, the retreating edges back-patched in. That is valid SSA
/// whatever the CFG. On an irreducible one — a cycle entered at two points,
/// `if (c) goto X; H: …; X: …; goto H` in bytecode javac never emits but
/// Kotlin coroutine state machines, obfuscators and some weavers do — the
/// "header" does not dominate its "latch". Rounds 11 wave 3-4 refused such a
/// method in [`IrBuilder::build`], because `ir_optimize::loop_headers` then
/// classified back edges by forward reachability alone and a LICM hoist to
/// the one classified pre-header ran on only one of the cycle's entries.
///
/// Test-only since round 11 wave 5: the loop passes check dominance
/// themselves (`ir_optimize::back_edges_dominated_by`), the refusal is gone,
/// and this stays as the tests' proof that an irreducible fixture really is
/// one.
///
/// Instruction-granular, one DFS and one Cooper–Harvey–Kennedy dominator pass
/// over the reachable instructions. Total: an index this cannot resolve
/// answers "reducible".
#[cfg(test)]
fn cfg_is_reducible(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    code_len: usize,
) -> bool {
    const NONE: usize = usize::MAX;
    let pcs: Vec<usize> = verified
        .instructions()
        .iter()
        .map(|insn| insn.pc as usize)
        .filter(|&pc| pc < code_len && reachable.contains(&pc))
        .collect();
    let n = pcs.len();
    if n == 0 || pcs.first() != Some(&0) {
        return true;
    }
    let index_of: HashMap<usize, usize> = pcs.iter().enumerate().map(|(i, &pc)| (pc, i)).collect();
    let succ: Vec<Vec<usize>> = pcs
        .iter()
        .map(|&pc| {
            verified
                .successors(pc)
                .into_iter()
                .filter(|&s| s < code_len)
                .filter_map(|s| index_of.get(&s).copied())
                .collect()
        })
        .collect();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (u, targets) in succ.iter().enumerate() {
        for &v in targets {
            if let Some(list) = preds.get_mut(v) {
                list.push(u);
            }
        }
    }

    // Reverse postorder from the entry (index 0 is pc 0).
    let mut seen = vec![false; n];
    let mut postorder: Vec<usize> = Vec::with_capacity(n);
    let mut dfs: Vec<(usize, usize)> = vec![(0, 0)];
    seen[0] = true;
    while let Some(&(b, i)) = dfs.last() {
        match succ.get(b).and_then(|t| t.get(i)).copied() {
            Some(s) => {
                if let Some(top) = dfs.last_mut() {
                    top.1 = i + 1;
                }
                if let Some(flag) = seen.get_mut(s) {
                    if !*flag {
                        *flag = true;
                        dfs.push((s, 0));
                    }
                }
            }
            None => {
                dfs.pop();
                postorder.push(b);
            }
        }
    }
    let rpo: Vec<usize> = postorder.iter().rev().copied().collect();
    let mut rpo_num = vec![NONE; n];
    for (k, &b) in rpo.iter().enumerate() {
        rpo_num[b] = k;
    }

    // Cooper–Harvey–Kennedy. An idom is always earlier in `rpo` than the
    // block it dominates, so every walk up an `idom` chain below strictly
    // decreases `rpo_num` and ends at the entry.
    let mut idom = vec![NONE; n];
    idom[0] = 0;
    let intersect = |idom: &[usize], mut a: usize, mut b: usize| -> usize {
        for _ in 0..=2 * n {
            if a == b {
                return a;
            }
            let (ra, rb) = (rpo_num[a], rpo_num[b]);
            if ra > rb {
                a = idom[a];
            } else {
                b = idom[b];
            }
        }
        0
    };
    let mut changed = true;
    let mut rounds = 0usize;
    while changed && rounds <= n {
        changed = false;
        rounds += 1;
        for &b in rpo.iter().skip(1) {
            let mut new_idom = NONE;
            for &p in &preds[b] {
                if idom[p] == NONE {
                    continue;
                }
                new_idom = if new_idom == NONE {
                    p
                } else {
                    intersect(&idom, p, new_idom)
                };
            }
            if new_idom != NONE && idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
        }
    }

    // Every retreating edge `u -> v` (in this DFS) must have `v` dominating
    // `u`; that holds for one DFS exactly when it holds for all of them.
    for (u, targets) in succ.iter().enumerate() {
        for &v in targets {
            if rpo_num[v] == NONE || rpo_num[u] == NONE || rpo_num[v] > rpo_num[u] {
                continue;
            }
            let mut x = u;
            let mut dominated = false;
            for _ in 0..=n {
                if x == v {
                    dominated = true;
                    break;
                }
                if x == 0 || idom[x] == NONE {
                    break;
                }
                x = idom[x];
            }
            if !dominated {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod zt31_scoped_fold_aligned_tests;
/// The admission half of [`ir_splice_callee_handlers_enabled`] (nio21 wave 4,
/// lane L2): may the body `code[..code_len]`, whose exception table is `rows`
/// (`(start, end, handler)`, table order, every catch type already checked by
/// the caller), be spliced with its handlers entered from call-site edges?
///
/// Admitted only when every row is the javac shape of
/// `try { calls } catch (X e) { throw wrap(e); }` -- the
/// `CharsetDecoder.decode(BB,CB,Z)` / `CharsetEncoder.encode(CB,BB,Z)` shape:
///
/// * the range is non-empty, every instruction in it is normally reachable and
///   [`splice_handler_range_opcode_admitted`] (so the ONLY exits inside it are
///   calls, which the edges cover, and guards, which deopt into the callee);
/// * the handler lies at or after the range's end and is not normally
///   reachable; its code (the "run": the instructions from it up to the next
///   normally reachable one) is straight-line, ends in its only `athrow`, has
///   a normally reachable instruction after it (or ends the body, under
///   [`ir_splice_tail_handler_enabled`]), and reads no local it did not
///   store first. The last two make the handler independent of the try
///   range's locals, so no liveness question about an exceptional edge can
///   arise in a deopt frame inside the range;
/// * at most [`IR_CALL_THROW_MAX_CANDIDATES`] rows.
///
/// The returned copy masks every run to `nop`s: the builder never walks it
/// (an entered handler deopts at its first instruction and the rest is dead),
/// and no planner or resolver table sees the handler's `new` /
/// `invokespecial` / `athrow`. The interpreter, which a deopt resumes into,
/// runs the method's own code and table.
///
/// Test-only since JIT round 22 wave 7 (lane review): the VM's resolver calls
/// [`splice_callee_handler_plan_with`].
#[cfg(test)]
pub fn splice_callee_handler_plan(
    code: &[u8],
    code_len: usize,
    rows: &[(usize, usize, usize)],
) -> Option<SpliceCalleeHandlerPlan> {
    splice_callee_handler_plan_with(code, code_len, rows, false)
}

#[cfg(test)]
mod nio21w1_l2_local_throw_edge_tests;

/// JIT round 20 wave 11 (lane pIR; L2-3 / L2W2-4): the superclass catch
/// decisions of the local throw edges.
#[cfg(test)]
mod r20w11_pir_superclass_catch_tests;

#[cfg(test)]
mod r21_keepset_wide_handler_tests;

#[cfg(test)]
mod j27_decloop_tail_handler_tests;

#[cfg(test)]
mod nio21w3_l2_call_throw_edge_tests;
/// Mask every cold throw arm of `code[..code_len]` in place, for the
/// optimizing tier's splice (round 16 wave 7, lane arraylist): the arm becomes
/// `aconst_null; nop...; athrow` -- the same length, the same branch structure,
/// and still verifiable -- so no planner walk, resolver table or builder arm
/// sees the exception construction it never runs (the `new`, the constructor
/// `invokespecial`, a message `ldc` or concatenation). An arm that is a lone
/// `athrow` (its exception computed before the branch) is left as it is.
///
/// Returns the masked arms' `athrow` pcs, or `None` -- with `code` untouched
/// -- when [`cold_throw_arms`] does. Only the COPY that is spliced may be
/// masked: the interpreter, which runs every arm, reads the method's own code.
///
/// Test-only since JIT round 22 wave 7 (lane review): the VM's resolver calls
/// [`mask_cold_throw_arms_keeping`].
#[cfg(test)]
pub fn mask_cold_throw_arms(code: &mut [u8], code_len: usize) -> Option<Vec<usize>> {
    mask_cold_throw_arms_keeping(code, code_len, false)
}

#[cfg(test)]
mod r19w4_throws4_recursion_base_arm_tests;
/// Round rx27 wave 1 (lane matcher): the cold handler arm
/// ([`cold_handler_arm_mask`]) on JDK 25's `Pattern.matcher` and on shapes
/// it must refuse, and the builder's half (the masked arm is a guard at its
/// branch; no monitor op is built).
#[cfg(test)]
mod rx27_cold_handler_arm_tests;

/// JIT r23 wave 9, lane L6 (`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT`): the
/// resolver's profile rule for an unresolved String `ldc`, on JDK 25's
/// `StringLatin1.newString([BII)`.
#[cfg(test)]
mod r23w9_l6_unresolved_ldc_exit_tests;

#[cfg(test)]
mod nio20_l1_fence_finally_tests;
/// CF-W9-3: has the fresh allocation `recv` stayed unpublished on every
/// memory path from it to the current memory state? Walks the memory
/// graph back from `mem` (a memory φ fans out to its inputs) and
/// refuses:
/// * any memory operation that takes `recv` as anything but the BASE of a
///   load or store of its own fields -- a store of `recv` into another
///   object, a call that receives it -- except the empty
///   `java/lang/Object.<init>()V` its constructor chain may keep as a call;
/// * any node it does not know to be a memory step (the start of the
///   method included: a path that never meets `recv` is not "after" it);
/// * more than `MAX_STEPS` distinct nodes.
#[cfg(test)]
pub(crate) fn vh_fresh_receiver_unpublished_in(graph: &Graph, mem: NodeId, recv: NodeId) -> bool {
    vh_fresh_receiver_publication(graph, mem, recv).is_none()
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod ir_core_tests;

/// The JVMS stack shuffles and `wide` forms the builder used to refuse at its
/// catch-all after `ir_compatible` had admitted the method.
#[cfg(test)]
mod stack_shuffle_and_wide_tests;

/// `Op::Switch` — the density rules, the dense normalisation, and the edge set
/// the JVMS requires the builder to preserve.
#[cfg(test)]
mod switch_tests;

// ── Round 9 wave 3 (lane irmid3): bounded self-recursive splicing and the
// splice resume map ─────────────────────────────────────────────────────────
#[cfg(test)]
mod r9w3_irmid3_tests;

#[cfg(test)]
mod r14w4_calls2_string_access_tests;

#[cfg(test)]
mod r11w2_ircore_tests;

// ── Round 12 wave 2 (lane irbuild): the loop-closed replay extent ─────────
#[cfg(test)]
mod r12w2_irbuild_tests;

/// Round 13 wave 11 (lane chain5): proposal I8-2 -- the receiver of a GUARDED
/// splice is exact inside its own body, so a nested exact-receiver site on it
/// (`put` -> `putVal` -> `newNode` for a map held in a field) splices on the
/// hit edge -- and that fact must not outlive the hit edge.
#[cfg(test)]
mod r13w11_chain5_guarded_exact_tests;

/// Round 12 wave 5 (lane iropt4): the three relaxations of the splice call
/// fence (`SpliceFenceRelax`), each with its must-refuse twin and its kill
/// switch. Every positive case here was refused before this wave; the
/// `HashMap.get` shape is `get_shape_builds_through_a_null_fact`, the
/// `HashMap.put` shape is `a_call_on_the_splice_receiver_after_a_call_builds`.

/// Round 13 wave 6 (lane chain2): the scope class table an optimizing
/// artifact keeps for the VM's chain builder (`splice_scope_class_ids`,
/// `r13w5-resume-chain-scope-class-ids-patch`).
#[cfg(test)]
mod r13w6_chain2_scope_class_tests;

// ── Round 14 wave 5 (lane sync5): SS8-3, SS8-1 b, and the loop-φ fold ──

// ── Round 14 wave 6 (lane sync6): SS8-1 a, a same-receiver nested body ──

// ── Round 14 wave 7 (lane sync7): SS8-5 and RV6-1, `holdsLock(C.class)` ──

// ── JIT r20 wave 19 (lane zLdc): THROW20-4 `date`, spliced String / Class `ldc` ──

/// JIT round 19 wave 2, lane eager2 (EAGER19-1): a deferred `new` the VM
/// admitted to the uninitialized-class trap builds as a guard at its bci
/// instead of bailing the method; one it did not admit keeps today's path.
#[cfg(test)]
mod r19w2_eager2_uninit_new_trap_tests;

/// JIT round 19 wave 3, lane eager3 (EAGER2-3 / EAGER2-4): a `new` the VM
/// admitted to the class-initialized GUARD builds as `Guard(ClassInitTest)`
/// in front of the resolved `Op::New`; the guard's cause is recorded by pc;
/// a guard site whose plant is refused keeps the row-less path.
#[cfg(test)]
mod r19w3_eager3_uninit_new_guard_tests;

/// JIT round 19 wave 4 (lane splice4): the SPLICE3-1..3 switches are inert
/// without their base switches, and SPLICE3-3's rate rule.
#[cfg(test)]
mod r19_splice4_tests;

#[cfg(test)]
mod nio21w7_byte_array_field_tests;

/// nio21 wave 18 (lane L3): an `athrow` reached with a `monitorenter` of this
/// body held (`IrBuilder::trap_athrow_in_monitor`).

/// nio21 wave 19 (lane L3, from lane L4's `ConcurrentHashMap.putVal` /
/// `replaceNode` finding): a lock local carried through a loop reaches its
/// `monitorexit` through a join whose other input is the local itself.

/// nio21 wave 19 (lane L3): `ThreadLocal.get()`'s hit path inline
/// (`IrBuilder::try_thread_local_get_intrinsic`).

/// Interpreter round i4 wave 94, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`):
/// a row-less field site the VM admitted builds as an uncommon trap at its
/// bci (`TrapCause::UnresolvedField`); one not admitted bails as before.
#[cfg(test)]
mod i94_l2_field_site_trap_tests;

/// JIT round 23 wave 1, lane L2 (INLINE22-4, `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`):
/// a lambda SAM forwarder's body is frameless -- no stack-trace level, no deopt
/// chain scope -- and the body it calls is parked directly in the `invoke` the
/// forwarder replaced.
#[cfg(test)]
mod r23w1_lambda_forwarder_tests;

/// JIT round 23 wave 4 (lane L1; `CRATONVM_JIT_IR_SPLICE_HANDLERS_POSTING_MODE`):
/// the JVMTI conjunct of the spliced-handler gate.

#[cfg(test)]
mod r23w4b_l2_ldc_condy_slot_ir_tests;

/// JIT round 23 wave 5 (lane L1; `CRATONVM_JIT_IR_SPLICE_ATHROW_EXIT`).

#[cfg(test)]
mod r23_l8_splice_operand_expression_tests;

/// JIT round 21 (lane tlocal, `CRATONVM_JIT_IR_THREAD_LOCAL_PROBE_WALK`): the
/// `ThreadLocal.get()` expansion follows the linear-probe chain past the home
/// slot, a cleared key is a miss (the JDK's expunge runs in the call), and
/// `=0` probes the home slot only.

/// JIT round 21 (lane lambdatrap, TLOCAL21-2): a non-capturing lambda row
/// added with `add_indy_bridge_sites` builds as the single-pass tier's bridged
/// call (`Op::IndyBridge`), not as an uncommon trap, and the add keeps a row
/// already recorded.
#[cfg(test)]
mod jr21_lambdatrap_published_word_bridge_tests;

/// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3;
/// `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL`): a SAM call planned as its lambda's
/// impl (`IrBuilder::lambda_impl_opcode`) is the impl's plain static call with
/// the captures in place of the proxy, at the `invokeinterface`'s bci, whose
/// snapshot still holds the proxy; a receiver that is not the planned
/// lambda's value refuses the method.
#[cfg(test)]
mod jr21_lambda2_impl_call_tests;

/// JIT round 21 (lane tl2): the `ThreadLocal` expansion for an
/// `InheritableThreadLocal`-typed site (`CRATONVM_JIT_IR_INHERITABLE_THREAD_LOCAL`)
/// and its admission inside a splice (`CRATONVM_JIT_IR_THREAD_LOCAL_IN_SPLICE`).
#[cfg(test)]
mod jr21_tl2_thread_local_tests;

/// JIT round 21 (lane lambda3; `CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES`):
/// with the switch's builder half on, a CAPTURING planned SAM call the build
/// would not splice refuses the method (the door rebuilds it with the
/// interface call); a non-capturing one is built as before.
#[cfg(test)]
mod jr21_lambda3_builder_tests;

/// JIT round 21 (lane tl3, `CRATONVM_JIT_IR_REFERS_TO_INLINE`):
/// `Reference.refersTo` and its spliced `refersToImpl` / `refersTo0` as an
/// inline identity compare of the referent (`try_reference_refers_to_intrinsic`).

/// JIT round 21 lane mcs (page
/// `jr21-mcs-guarded-indy-call-lost-its-direct-row-20261014.md`): the guarded
/// `MutableCallSite` site's planned static body spliced on the guard's hit edge
/// (`CRATONVM_JIT_INDY_MUTABLE_GUARD_SPLICE`), and the call form it falls back to.

/// JIT round 21 lane refs2 (`CRATONVM_JIT_IR_REFERS_TO_OWN_LAYOUT`; page
/// `jr21-refs2-soft-phantom-refersto-and-the-layout-wait-20261014.md`): the
/// `refersTo` expansion builds from the referent-only index when the compile
/// has no `ThreadLocal` layout, and the table reader declines with a switch off.
#[cfg(test)]
mod jr21_refs2_refers_to_own_layout_tests;

/// tl21 (TLOCAL21-4b): a `ThreadLocal`-declared site whose profile saw ONE
/// inheritable-map class alone (the VM's kind answer is "inheritable") is the
/// trap form on `Thread.inheritableThreadLocals`; every other shape keeps its
/// previous form. Derived from `ThreadLocal.getMap` / `InheritableThreadLocal.getMap`
/// (JDK 21 `javap`), not measured.

/// JIT round 21 (lane tlsub): TL321-4 (a profiled `ThreadLocal` subclass the
/// VM vouches for is tested beside the layout's classes) and TL321-1 (a site
/// whose profile saw one admitted class alone builds every miss as a guard at
/// the invoke, with no call), with their switches and the de-spec withdrawal.

/// JIT round 21 (lane staticfwd): OI-4, a plain primitive `putstatic`'s value
/// answers a later `getstatic` of the same static
/// (`CRATONVM_JIT_IR_STATIC_STORE_FORWARD`), and FG16-3, a plain `putstatic`
/// inside a splice is committed behind the replay fence
/// (`CRATONVM_JIT_IR_SPLICE_PUTSTATIC`).

#[cfg(test)]
mod jr21_records_fold_tests;

/// t1 wave 8, lane LAMBDATRAP
/// (`CRATONVM_JIT_IR_INDY_PUBLISHED_WORD_BRIDGE_OVER_REFUSAL_ONLY`): under the
/// refusal-only bridge plan (default ON for a loop method whose indys lie
/// outside its loops) a published-word row keeps its `Op::IndyBridge` where
/// the default arm would plant a trap; a plain bridge row there still keeps
/// the trap, and `=0` restores the demotion.
#[cfg(test)]
mod t1_lambdatrap_published_word_over_refusal_only_tests;

/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a constant filter
/// species' site is two plain calls -- the PRE call of the row keyed by the
/// site's `getstatic` pc (and at that bytecode pc), then the MAIN call the
/// invoke arm builds from the site's own row -- and refuses the method when
/// its rows do not fit.
#[cfg(test)]
mod jr21_mhfilter_builder_tests;
#[cfg(test)]
mod mh_convert_member_splice_tests;
#[cfg(test)]
mod mh_folded_member_splice_tests;
#[cfg(test)]
mod splice_operand_varargs_tests;

/// JIT tier2 wave 4, lane g1vh21 (G1VHSOFT21-2): the `Reference.get()`
/// expansion at a `Reference`-typed site builds the `WeakReference` body
/// behind an exact-class test, and nothing else.
///
/// Run: `cargo test -p cratonvm-jit g1vh21`
#[cfg(test)]
mod g1vh21_reference_typed_get_tests;

/// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`): the
/// `Reference.get` expansion's layout words.
#[cfg(test)]
mod jr21_g1vhsoft_reference_get_layout_tests;
#[cfg(test)]
mod static_guess_mh_tests;

mod node_id;
#[allow(unused_imports)]
pub use node_id::*;
#[allow(unused_imports)]
use node_id::*;
mod safepoints;
#[allow(unused_imports)]
pub use safepoints::*;
#[allow(unused_imports)]
use safepoints::*;
mod loop_phis;
#[allow(unused_imports)]
pub use loop_phis::*;
#[allow(unused_imports)]
use loop_phis::*;
mod set_osr_requested_header;
#[allow(unused_imports)]
pub use set_osr_requested_header::*;
#[allow(unused_imports)]
use set_osr_requested_header::*;
mod splice_op_cannot_raise;
#[allow(unused_imports)]
pub use splice_op_cannot_raise::*;
#[allow(unused_imports)]
use splice_op_cannot_raise::*;
mod narrow_int_return;
#[allow(unused_imports)]
pub use narrow_int_return::*;
#[allow(unused_imports)]
use narrow_int_return::*;
mod is_jdk_throwable_init_candidate;
#[allow(unused_imports)]
pub use is_jdk_throwable_init_candidate::*;
#[allow(unused_imports)]
use is_jdk_throwable_init_candidate::*;
mod attach_sb_virtual_builders;
#[allow(unused_imports)]
pub use attach_sb_virtual_builders::*;
#[allow(unused_imports)]
use attach_sb_virtual_builders::*;
mod field_load_memo_mode;
#[allow(unused_imports)]
pub use field_load_memo_mode::*;
#[allow(unused_imports)]
use field_load_memo_mode::*;
mod ir_splice_store_fence_forward_branch;
#[allow(unused_imports)]
pub use ir_splice_store_fence_forward_branch::*;
#[allow(unused_imports)]
use ir_splice_store_fence_forward_branch::*;
mod ir_trap_census;
#[allow(unused_imports)]
pub use ir_trap_census::*;
#[allow(unused_imports)]
use ir_trap_census::*;
mod root_ref_facts;
pub use root_ref_facts::ConstRefMeta;

#[cfg(test)]
mod t1_static_initialized_spliced_tests;

#[cfg(test)]
mod nio_charset_layout_tests;

#[cfg(test)]
mod opcost_exc_tests;
