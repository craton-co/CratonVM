// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compilability screen: which bytecode shapes this backend accepts.
//!
//! Moved verbatim out of `x64.rs`'s `Bytecode compatibility check`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

/// Scan bytecode and determine JIT compatibility.
///
/// Returns `Some(needs_heap)` if the method can be JIT-compiled, `None` otherwise.
/// `needs_heap` is true if the method uses array/object opcodes that require
/// a heap pointer as a hidden first argument.
/// Kill-switch for the dup_x1/dup_x2 codegen arms (`CRATONVM_JIT_NO_DUPX=1`
/// restores the historical "bail to interpreter" behaviour). Added for
/// regression bisection while the arms are fresh.
pub(super) fn dupx_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_DUPX").is_some())
}

/// DBG bisection (spring-bug-11): disable ONLY the dup_x1 (0x5A) codegen arm,
/// to tell whether the Groovy SIGSEGV is in dup_x1 vs dup_x2 vs a co-located op.
pub(super) fn dup_x1_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_DUP_X1").is_some())
}

/// DBG bisection (spring-bug-11): disable ONLY the dup_x2 (0x5B) codegen arm.
pub(super) fn dup_x2_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_DUP_X2").is_some())
}

/// Disable ONLY the dup2_x2 (0x5E) codegen arm, restoring the historical
/// "reach the catch-all and stay interpreted" behaviour. The arm picks its
/// shuffle shape from the `stack_kinds` width analysis rather than from a
/// peephole, so it wants its own bisection lever independent of `dup-x1`/
/// `dup-x2`.
pub(super) fn dup2_x2_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_DUP2_X2").is_some())
}

/// Kill-switch for the IR tier's trusted-oop receiver shortcut on a PRIMITIVE
/// inline `getfield` (`CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD=1` restores the
/// full six-comparison containment guard).
///
/// It has its own lever because it is the one thing that makes the inline
/// `getfield` reachable at all under a collector that publishes no region
/// bounds — i.e. ZGC, the default since 2026-08-10 — so "is this the cause"
/// has to be answerable in one run rather than by reading gates.
pub fn trusted_oop_receiver_getfield_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD").is_none()
    })
}

/// Trace what the dup_x1 rotate did to the model: the three slots and their
/// oop marks, before and after. `CRATONVM_DBG_DUPX_METHODS` says WHICH compiled
/// methods carry the opcode; this says what happened inside each one.
pub(super) fn dupx_trace() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DUPX_TRACE").is_some())
}

/// DBG bisection (spring-bug-11): immediately `canonicalize_stack()` after a
/// dup_x1/dup_x2 rotate, eliminating the non-canonical rotated frame offsets in
/// place. If this clears the crash, the defect is a downstream consumer of the
/// non-canonical offsets (canonicalize parallel-move / a merge that assumes
/// canonical layout); if it does NOT, the rotate model itself or a co-located
/// op is to blame.
pub(super) fn dupx_eager_canon() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_DUPX_EAGER_CANON").is_some()
    })
}

/// Parse a comma-separated env var into a substring list, `None` when unset or
/// empty. Used by the single-pass inline-cache bisect levers below.
fn csv_filter(var: &str) -> Option<Vec<String>> {
    let raw = cratonvm_types::flags::runtime_var(var).ok()?;
    let parts: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Per-SITE bisect for the single-pass inline virtual cache.
///
/// `CRATONVM_JIT_SP_INLINE_IC=0` turns the whole inline MIC/PIC cascade off,
/// which is a whole-program answer: it tells you the defect is on that edge but
/// not WHICH site. These two narrow it. Both match against the composed key
///
/// ```text
/// <caller method label>||<callee class>.<callee method>
/// ```
///
/// so one list can name a caller (`XMLAttributesImpl.addAttributeNS||`), a
/// callee (`||java/util/Map.get`), or a specific edge. `_ONLY` admits only
/// matching sites; `_DENY` refuses matching ones and is applied second.
fn sp_ic_only_filter() -> &'static Option<Vec<String>> {
    static CACHE: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| csv_filter("CRATONVM_JIT_SP_IC_ONLY"))
}

fn sp_ic_deny_filter() -> &'static Option<Vec<String>> {
    static CACHE: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| csv_filter("CRATONVM_JIT_SP_IC_DENY"))
}

/// Whether the inline cascade may be emitted for this one call site. Cheap when
/// neither filter is set (two `Option` discriminant checks), which is the
/// default and the only configuration that runs in production.
pub(super) fn sp_ic_site_allowed(caller: &str, callee_class: &str, callee_method: &str) -> bool {
    let only = sp_ic_only_filter();
    let deny = sp_ic_deny_filter();
    if only.is_none() && deny.is_none() {
        return true;
    }
    let key = format!("{caller}||{callee_class}.{callee_method}");
    if let Some(list) = only {
        if !list.iter().any(|p| key.contains(p.as_str())) {
            return false;
        }
    }
    if let Some(list) = deny {
        if list.iter().any(|p| key.contains(p.as_str())) {
            return false;
        }
    }
    true
}

/// Split the cascade's two shapes so a bisect can tell the 4-way PIC (which
/// evicts, and carries the megamorphic tail) from the 1-entry MIC.
/// `CRATONVM_JIT_SP_INLINE_PIC=0` demotes every site to the MIC shape;
/// `CRATONVM_JIT_SP_INLINE_MIC=0` leaves only the PIC shape.
pub(super) fn sp_inline_pic_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_SP_INLINE_PIC").as_deref(),
            Ok("0")
        )
    })
}

pub(super) fn sp_inline_mic_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_SP_INLINE_MIC").as_deref(),
            Ok("0")
        )
    })
}

/// The megamorphic hashed stub is emitted whenever a PIC slot exists and the
/// cascade is allowed — it survives `CRATONVM_JIT_SP_INLINE_PIC=0` and
/// `_MIC=0`, so those two levers cannot tell it from the 4-way cascade.
/// `CRATONVM_JIT_SP_INLINE_MEGA=0` isolates it.
pub(super) fn sp_inline_mega_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_SP_INLINE_MEGA").as_deref(),
            Ok("0")
        )
    })
}

/// List every site the cascade is emitted at, so a bisect has a candidate set
/// to feed back into `CRATONVM_JIT_SP_IC_ONLY` / `_DENY` instead of guessing
/// method names. One line per emitted site, at compile time — not per call.
pub(super) fn sp_ic_site_trace() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SP_IC_SITES").is_some())
}

pub fn jit_scan(code: &[u8], code_len: usize, descriptor: &str) -> Option<JitScanResult> {
    let mut needs_heap = false;
    let mut multianewarray_ops = Vec::new();
    let mut field_ops = Vec::new();
    let mut typecheck_ops = Vec::new();
    // cov-05: `checkcast` (0xc0) sites only, a subset of `typecheck_ops`
    // (which also carries `instanceof`, 0xc1). `ir_compatible` gates on this
    // one alone — see its doc comment — so a method with no `checkcast` but
    // at least one `instanceof` may still reach the optimizing pipeline.
    let mut checkcast_ops: Vec<(usize, u16)> = Vec::new();
    let mut static_field_ops = Vec::new();
    let mut invoke_ops: Vec<(usize, u16, u8)> = Vec::new(); // (pc, cp_index, opcode)
    let mut new_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `new` (0xbb)
    let mut anewarray_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `anewarray` (0xbd)
    let mut ldc_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `ldc`/`ldc_w`
    let mut ldc2w_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `ldc2_w` (0x14)
    let mut indy_ops: Vec<(usize, u16)> = Vec::new(); // (pc, cp_index) for `invokedynamic` (0xba)
    let mut has_athrow = false; // RBC.6 — method contains 0xbf
    let mut has_newarray = false; // Primitive array allocation (0xbc)
                                  // RBC.6 local-handler-safety fix — every `*load`/`*store`/`iinc`
                                  // instruction's (bytecode_pc, local_slot). Populated inline in the
                                  // existing, already-correct per-opcode arms below (zero new pc-
                                  // advancement logic — just recording a side effect), so it can never
                                  // diverge from this scanner's own opcode-width decoding. Consumed by
                                  // `local_handler_reads_unsafe_local` (jit/src/lib.rs) to conservatively
                                  // verify every exception-table handler in a method only ever reads a
                                  // local it (or something reachable before it in bytecode order,
                                  // starting from the handler's own entry pc) has itself written — see
                                  // that function's doc comment for why this check exists.
    let mut local_slot_ops: Vec<(usize, bool, u16)> = Vec::new(); // (pc, is_store, slot)
                                                                  // BUG-LQB-SCOPE (RELAXED, see docs/feature-designs/jit-local-exception-handlers.md
                                                                  // — documented alongside the RBC.6 relaxation it was found while
                                                                  // validating): this scanner used to track the earliest "committing side
                                                                  // effect" pc and refuse to compile ANY method where one preceded an
                                                                  // `invokedynamic` in raw bytecode-pc order, because at the time
                                                                  // (`752796a0a`, 2026-07-07) the trap's fallback was believed to be an
                                                                  // imprecise whole-method re-run that could double-execute that side
                                                                  // effect (the real, once-confirmed Liquibase `Scope` corruption). That
                                                                  // premise is stale: the SAME day, a concurrent fix
                                                                  // (`fixed-suite-bugs/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md`)
                                                                  // closed FOUR separate bugs in the reason-8 (`UnreachedCode`) precise-resume
                                                                  // machinery this trap already uses UNCONDITIONALLY (`emit_osr_exit_map_at_reason`
                                                                  // below, `emit_deopt_stubs`'s reason-8 routing, not gated behind
                                                                  // `deopt_real_enabled()`) — including the exact nested-compiled-callee
                                                                  // identity-mismatch case this gate was defending against
                                                                  // (`try_resume_trapped_callee` + baked `method_key` identity checks,
                                                                  // verified via a dedicated repro: 30000/30000 corrupted calls before,
                                                                  // 0 after). A live indy trap today precisely resumes at the trapping bci
                                                                  // with the correct locals/stack reconstructed — it does not re-run
                                                                  // anything before it, so there is nothing left to double-execute. Removed
                                                                  // this scan-time gate; the underlying runtime protection it was
                                                                  // duplicating (imprecisely, at compile time) already exists and is
                                                                  // strictly more precise.
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // nop
            0x00 => {
                pc += 1;
            }
            // aconst_null
            0x01 => {
                pc += 1;
            }
            // iconst_m1..iconst_5
            0x02..=0x08 => {
                pc += 1;
            }
            // lconst_0, lconst_1
            0x09 | 0x0a => {
                pc += 1;
            }
            // fconst_0, fconst_1, fconst_2
            0x0b..=0x0d => {
                pc += 1;
            }
            // dconst_0, dconst_1
            0x0e | 0x0f => {
                pc += 1;
            }
            // bipush
            0x10 => {
                if pc + 1 >= code_len {
                    return None;
                }
                pc += 2;
            }
            // sipush
            0x11 => {
                if pc + 2 >= code_len {
                    return None;
                }
                pc += 3;
            }
            // iload, lload, fload, dload, aload (wide index)
            0x15..=0x19 => {
                if pc + 1 >= code_len {
                    return None;
                }
                local_slot_ops.push((pc, false, code[pc + 1] as u16));
                pc += 2;
            }
            // iload_0..iload_3
            0x1a..=0x1d => {
                local_slot_ops.push((pc, false, (op - 0x1a) as u16));
                pc += 1;
            }
            // lload_0..lload_3
            0x1e..=0x21 => {
                local_slot_ops.push((pc, false, (op - 0x1e) as u16));
                pc += 1;
            }
            // fload_0..fload_3, dload_0..dload_3
            0x22..=0x29 => {
                local_slot_ops.push((pc, false, ((op - 0x22) % 4) as u16));
                pc += 1;
            }
            // aload_0..aload_3
            0x2a..=0x2d => {
                local_slot_ops.push((pc, false, (op - 0x2a) as u16));
                pc += 1;
            }
            // iaload, laload, faload, daload, aaload, baload, caload, saload
            0x2e..=0x35 => {
                pc += 1;
            }
            // istore, lstore, fstore, dstore, astore (wide index)
            0x36..=0x3a => {
                if pc + 1 >= code_len {
                    return None;
                }
                local_slot_ops.push((pc, true, code[pc + 1] as u16));
                pc += 2;
            }
            // istore_0..istore_3
            0x3b..=0x3e => {
                local_slot_ops.push((pc, true, (op - 0x3b) as u16));
                pc += 1;
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                local_slot_ops.push((pc, true, (op - 0x3f) as u16));
                pc += 1;
            }
            // fstore_0..fstore_3, dstore_0..dstore_3
            0x43..=0x4a => {
                local_slot_ops.push((pc, true, ((op - 0x43) % 4) as u16));
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                local_slot_ops.push((pc, true, (op - 0x4b) as u16));
                pc += 1;
            }
            // iastore, lastore, fastore, dastore, aastore, bastore, castore, sastore
            0x4f..=0x56 => {
                // `aastore` (0x53) emits the SATB pre-write + post-store write
                // barriers, both of which load the VM pointer from
                // `heap_local_offset`. Without `needs_heap` that slot is never
                // set up (offset 0 aliases local 0), so the barrier calls
                // `jit_satb_pre_write_barrier` with a stack address as `vm_ptr`
                // → `vm.heap` dereferences garbage → SIGSEGV (H2 TestUtils,
                // JIT-only; the primitive array stores do inline stores with no
                // heap-dependent helper).
                if op == 0x53 {
                    needs_heap = true;
                }
                pc += 1;
            }
            // pop
            0x57 => {
                pc += 1;
            }
            // dup
            0x59 => {
                pc += 1;
            }
            // swap
            0x5f => {
                pc += 1;
            }
            // iadd..ddiv (all int/long/float/double add/sub/mul/div)
            0x60..=0x6f => {
                pc += 1;
            }
            // irem, lrem, frem, drem. (frem/drem 0x72/0x73: only the optimizing
            // IR backend lowers them — via a CALL to the jit_frem/jit_drem fmod
            // helper. The single-pass backend has no codegen arm, so it bails
            // them through the `match op` catch-all (`return false`). Admitting
            // them at scan time lets the IR pipeline see the method instead of
            // rejecting it outright here; with the FP gate off the method still
            // bails to single-pass → interpreter, exactly as before.)
            0x70..=0x73 => {
                pc += 1;
            }
            // ineg, lneg, fneg, dneg
            0x74..=0x77 => {
                pc += 1;
            }
            // ishl, lshl
            0x78 | 0x79 => {
                pc += 1;
            }
            // ishr, lshr
            0x7a | 0x7b => {
                pc += 1;
            }
            // iushr, lushr
            0x7c | 0x7d => {
                pc += 1;
            }
            // iand, land
            0x7e | 0x7f => {
                pc += 1;
            }
            // ior, lor
            0x80 | 0x81 => {
                pc += 1;
            }
            // ixor, lxor
            0x82 | 0x83 => {
                pc += 1;
            }
            // iinc — reads then writes the same slot, in that order (the
            // read-half must see whatever safety state existed BEFORE this
            // instruction; the write-half is what makes the slot safe for
            // anything after).
            0x84 => {
                if pc + 2 >= code_len {
                    return None;
                }
                local_slot_ops.push((pc, false, code[pc + 1] as u16));
                local_slot_ops.push((pc, true, code[pc + 1] as u16));
                pc += 3;
            }
            // wide (JVMS §6.5) — the prefix that widens the FOLLOWING opcode's
            // local index to two bytes, and `wide iinc`'s constant to two more.
            //
            // Refused here until 2026-08-28, and a refusal in this scan is
            // PERMANENT for the whole method at EVERY compile door: the `None`
            // arms call `mark_jit_bail_listed`, and the OSR door consults the
            // same list. netty's `FastLz.compress` is 1617 bytes whose entire
            // job is one loop, so OSR is its only route to compiled code —
            // and it carries three `iinc_w` instructions, of which
            // `iinc_w 18, -255` is simply an increment too big for a signed
            // byte. That one prefix left 256 MiB of compression running in the
            // interpreter at **138x HotSpot** (`FastLz` encode, 1 MiB:
            // 5115 ms against 37 ms), with no diagnostic beyond a silent
            // `OSR-recompile reason=no-cached-artifact` repeating forever.
            //
            // Every downstream consumer was already built for this and says so
            // in its own comment: `bytecode_len_at` and regalloc's `bc_len`
            // twin size it 4/6 "as defense-in-depth ... if any is ever
            // accepted", `find_reference_locals` reads its `aload`/`astore`
            // forms, `classify_local_kinds` has tests for all three shapes, and
            // `oop_dataflow_transfer` transfers it. Only this scan and the
            // codegen's own walk were missing, and they are changed together.
            0xC4 => {
                if pc + 1 >= code_len {
                    return None;
                }
                let wop = code[pc + 1];
                // `wide iinc` is SIX bytes (prefix, opcode, 2-byte index,
                // 2-byte signed constant); every other widened form is four.
                let width = if wop == 0x84 { 6 } else { 4 };
                if pc + width > code_len {
                    return None;
                }
                let idx = u16::from_be_bytes([code[pc + 2], code[pc + 3]]);
                match wop {
                    // iload, lload, fload, dload, aload
                    0x15..=0x19 => local_slot_ops.push((pc, false, idx)),
                    // istore, lstore, fstore, dstore, astore
                    0x36..=0x3a => local_slot_ops.push((pc, true, idx)),
                    // iinc — reads then writes the same slot, in that order,
                    // exactly as the narrow arm above records it.
                    0x84 => {
                        local_slot_ops.push((pc, false, idx));
                        local_slot_ops.push((pc, true, idx));
                    }
                    // `wide ret` (0xa9) and anything else. `ret` is half of the
                    // obsolete `jsr`/`ret` pair this scan does not accept in its
                    // narrow form either, so refuse the method rather than
                    // widen the set this walk claims to model.
                    _ => {
                        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
                            eprintln!("[cratonvm-jitc] scan-bail wide op=0x{wop:02x} pc={pc}");
                        }
                        return None;
                    }
                }
                pc += width;
            }
            // i2l, i2f, i2d, l2i, l2f, l2d, f2i, f2l, f2d, d2i, d2l, d2f, i2b, i2c, i2s
            0x85..=0x93 => {
                pc += 1;
            }
            // lcmp, fcmpl, fcmpg, dcmpl, dcmpg
            0x94..=0x98 => {
                pc += 1;
            }
            // ifeq, ifne, iflt, ifge, ifgt, ifle
            0x99..=0x9e => {
                if pc + 2 >= code_len {
                    return None;
                }
                pc += 3;
            }
            // if_icmpeq..if_icmple
            0x9f..=0xa4 => {
                if pc + 2 >= code_len {
                    return None;
                }
                pc += 3;
            }
            // goto
            0xa7 => {
                if pc + 2 >= code_len {
                    return None;
                }
                pc += 3;
            }
            // ireturn, lreturn, freturn, dreturn
            0xac..=0xaf => {
                pc += 1;
            }
            // return (void)
            0xb1 => {
                pc += 1;
            }
            // invokestatic — track in invoke_ops for resolution (self-call or cross-method).
            // Always set needs_heap because cross-method dispatch via
            // jit_invoke_dispatch requires vm_ptr stored at heap_local_offset.
            0xb8 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                invoke_ops.push((pc, cp_idx, op));
                needs_heap = true;
                pc += 3;
            }
            // areturn — return object reference
            0xb0 => {
                pc += 1;
            }
            // getstatic — static field read (needs vm context)
            0xb2 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                static_field_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // putstatic — static field write (needs vm context)
            0xb3 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                static_field_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // getfield — object field read
            0xb4 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                field_ops.push((pc, cp_idx));
                // Default getfield codegen routes through the checked helper,
                // which needs the hidden SharedVm pointer. Reserving the
                // context slot is harmless when raw inlining is explicitly
                // enabled.
                needs_heap = true;
                pc += 3;
            }
            // putfield — object field write. A reference-typed putfield
            // lowers to a `jit_putfield_object(vm_ptr, ...)` helper call
            // (write barrier + SATB barrier), so the compiled method MUST
            // carry the hidden VM pointer. Without `needs_heap` the
            // codegen loads `vm_ptr` from `heap_local_offset == 0`, i.e.
            // `[rbp-0]` — the saved RBP — and hands that stack address to
            // `jit_putfield_object`, which then dereferences it as a
            // `SharedVm`: silent heap corruption that surfaced as a
            // delayed SIGSEGV (Tomcat's `Catalina.setParentClassLoader`,
            // a bare `aload_0; aload_1; putfield; return` with no other
            // heap op to set the flag).
            0xb5 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                field_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // newarray — needs heap for allocation
            0xbc => {
                if pc + 1 >= code_len {
                    return None;
                }
                has_newarray = true;
                needs_heap = true;
                pc += 2;
            }
            // arraylength
            0xbe => {
                pc += 1;
            }
            // checkcast — type check (pass-through or exception)
            0xc0 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                typecheck_ops.push((pc, cp_idx));
                checkcast_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // instanceof — type check (returns 0 or 1)
            0xc1 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                typecheck_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // multianewarray — multi-dimensional array allocation (2D only for now)
            0xc5 => {
                if pc + 3 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                let ndims = code[pc + 3];
                if ndims != 2 {
                    return None;
                }
                multianewarray_ops.push((pc, cp_idx, ndims));
                needs_heap = true;
                pc += 4;
            }
            // if_acmpeq, if_acmpne — reference comparison branches
            0xa5 | 0xa6 => {
                if pc + 2 >= code_len {
                    return None;
                }
                pc += 3;
            }
            // ifnull, ifnonnull — null check branches
            0xc6 | 0xc7 => {
                if pc + 2 >= code_len {
                    return None;
                }
                pc += 3;
            }
            // invokevirtual, invokespecial — method dispatch via helper
            0xb6 | 0xb7 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                invoke_ops.push((pc, cp_idx, op));
                needs_heap = true;
                pc += 3;
            }
            // invokeinterface — interface dispatch via helper (5 bytes: opcode, cp_hi, cp_lo, count, 0)
            0xb9 => {
                if pc + 4 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                invoke_ops.push((pc, cp_idx, op));
                needs_heap = true;
                pc += 5;
            }
            // invokedynamic — no longer a permanent scan-time veto (5 bytes:
            // opcode, cp_hi, cp_lo, 0, 0). The overwhelmingly common source of
            // an invokedynamic in otherwise-ordinary hot methods is
            // `assert cond : "msg" + var;` (javac lowers the message concat via
            // `StringConcatFactory`), which sits on the assertions-disabled dead
            // branch. Previously ANY invokedynamic anywhere in a method's
            // bytecode — reachable or not — permanently blacklisted the WHOLE
            // method from JIT compilation (see the removed RG.1 test), forcing
            // hot per-call-site methods that merely CONTAIN a dead assert into
            // the interpreter forever.
            //
            // This scanner is CP-blind by design and does not resolve the
            // target descriptor — it just records the site (pc, cp_index) so
            // the codegen (which DOES have CP access) can look up the
            // descriptor later. The codegen lowers the instruction to an
            // unconditional jump to the existing uncommon-trap deopt stub
            // (`DeoptReason::UnreachedCode`): if this exact program point is
            // ever actually reached at runtime (assertions enabled, or a
            // genuinely live indy), the method permanently reverts to
            // interpreter-only execution for the rest of the process — i.e.
            // today's status quo for that one method. In the common case
            // (assertions disabled, dead branch) the trap is never taken and
            // the surrounding hot method compiles and runs at full JIT speed.
            //
            // REGRESSION FIX (2026-07-04): the unconditional-deopt codegen for
            // this opcode (the `0xba` arm in `compile_bytecode`, and the shared
            // `emit_deopt_stubs` out-of-line trap it jumps to) calls
            // `jit_uncommon_trap(vm_ptr, reason, bci)`, and loads `vm_ptr` from
            // `self.heap_local_offset` -- EXACTLY the same hidden-VM-pointer
            // frame slot documented above at the `aastore` (0x53) arm. That
            // slot is only reserved when `needs_heap` is set; otherwise
            // `heap_local_offset` aliases local slot 0 (see the `aastore`
            // comment), and the trap helper receives whatever garbage/local
            // value happens to sit there instead of the real `&SharedVm`,
            // which it blindly dereferences -- SIGSEGV. A method containing an
            // invokedynamic but no OTHER heap-requiring opcode (the common
            // dead-`assert` case is often exactly this shape) left
            // `needs_heap` false, so the very trap this fix relies on as its
            // "safe fallback" was itself unsafe. Set `needs_heap = true` here,
            // mirroring every other opcode whose codegen depends on
            // `heap_local_offset` (invoke*, new, anewarray, aastore, ...).
            0xba => {
                if pc + 4 >= code_len {
                    return None;
                }
                // BUG-LQB-SCOPE gate removed — see the doc comment above
                // (where `first_committing_side_effect_pc` used to be
                // declared) for why it's no longer needed.
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                indy_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 5;
            }
            // tableswitch — accept in scanner, emit CMP chain in compiler
            0xaa => {
                pc += 1;
                while pc % 4 != 0 && pc < code_len {
                    pc += 1;
                }
                let header_end = pc.checked_add(12)?;
                if header_end > code_len {
                    return None;
                }
                let low =
                    i32::from_be_bytes([code[pc + 4], code[pc + 5], code[pc + 6], code[pc + 7]]);
                let high =
                    i32::from_be_bytes([code[pc + 8], code[pc + 9], code[pc + 10], code[pc + 11]]);
                // Checked `high - low + 1`: raw i32 arithmetic overflows on
                // attacker-controlled bounds. Bail (not JIT-eligible) on
                // overflow or an out-of-range count.
                let num_offsets = match checked_tableswitch_count(low, high) {
                    Some(n) => n,
                    None => return None,
                };
                let payload_bytes = num_offsets.checked_mul(4)?;
                let switch_end = header_end.checked_add(payload_bytes)?;
                if switch_end > code_len {
                    return None;
                }
                pc = switch_end;
            }
            // lookupswitch — accept in scanner, emit CMP chain in compiler
            0xab => {
                pc += 1;
                while pc % 4 != 0 && pc < code_len {
                    pc += 1;
                }
                let header_end = pc.checked_add(8)?;
                if header_end > code_len {
                    return None;
                }
                let npairs_raw =
                    i32::from_be_bytes([code[pc + 4], code[pc + 5], code[pc + 6], code[pc + 7]]);
                let npairs = match checked_lookupswitch_npairs(npairs_raw) {
                    Some(n) => n,
                    None => return None,
                };
                let payload_bytes = npairs.checked_mul(8)?;
                let switch_end = header_end.checked_add(payload_bytes)?;
                if switch_end > code_len {
                    return None;
                }
                pc = switch_end;
            }
            // ldc — load int/float/string constant from CP (1-byte index)
            0x12 => {
                if pc + 1 >= code_len {
                    return None;
                }
                let cp_idx = code[pc + 1] as u16; // Widening: always safe
                ldc_ops.push((pc, cp_idx));
                pc += 2;
            }
            // ldc_w — load int/float/string constant from CP (2-byte index)
            0x13 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                ldc_ops.push((pc, cp_idx));
                pc += 3;
            }
            // ldc2_w — load long/double constant from CP (2-byte index)
            0x14 => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                ldc2w_ops.push((pc, cp_idx));
                pc += 3;
            }
            // pop2 — discard top two slots
            0x58 => {
                pc += 1;
            }
            // dup_x1 — duplicate top and insert two below
            0x5a => {
                pc += 1;
            }
            // dup_x2 — duplicate top and insert three below
            0x5b => {
                pc += 1;
            }
            // dup2 — duplicate top two slots
            0x5c => {
                pc += 1;
            }
            // dup2_x1 — duplicate top two and insert three below
            0x5d => {
                pc += 1;
            }
            // dup2_x2 — duplicate top two and insert four below
            0x5e => {
                pc += 1;
            }
            // new — object allocation; tracked for code generation
            0xbb => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                new_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // anewarray — reference array allocation; tracked for code generation
            0xbd => {
                if pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                anewarray_ops.push((pc, cp_idx));
                needs_heap = true;
                pc += 3;
            }
            // monitorenter / monitorexit. Non-escaping locks are elided;
            // live locks call the VM's direct mark-word helper, which needs
            // the hidden VM context just like allocation/field helpers.
            0xC2 | 0xC3 => {
                needs_heap = true;
                pc += 1;
            }
            // RBC.6 — athrow. Accepted; `has_athrow` is recorded so callers
            // can gate compilation to methods WITHOUT local exception
            // handlers (the codegen lowers athrow to "stash pending
            // exception + return the i64::MIN deopt sentinel", which cannot
            // dispatch to an in-method handler) and so the OSR trigger can
            // decline (its bail path resumes at the back-edge, which could
            // re-run side effects). `analyze_escapes` already treats 0xbf
            // as a full-escape barrier, so scalar replacement stays sound.
            0xbf => {
                has_athrow = true;
                pc += 1;
            }
            // Anything else: not JIT-compatible
            _ => {
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
                    eprintln!("[cratonvm-jitc] scan-bail op=0x{:02x} pc={}", op, pc);
                }
                return None;
            }
        }
    }

    // Check the return type is int, long, float, double, object reference, or void
    let ret = crate::return_type(descriptor);
    if !matches!(
        ret,
        b'I' | b'J' | b'F' | b'D' | b'[' | b'L' | b'V' | b'B' | b'C' | b'S' | b'Z'
    ) {
        return None;
    }

    // DUP2 category handling now lives in the codegen (`compile_bytecode`'s
    // 0x5C arm via `dup2_top_cat2`). Unlike this CP-less scan, the codegen
    // resolves `getfield`/`getstatic`/`invoke*` descriptors, so it can tell a
    // FORM-2 (single category-2 long/double) `dup2` from a FORM-1 (two
    // category-1) one and emit each correctly — bailing to the interpreter only
    // when the top width is genuinely unprovable. The old `dup2_category_safe`
    // reject gate that sat here over-rejected FORM-1 methods (any
    // `<getfield/invoke>; dup2` whose result is category-1, which the CP-less
    // scan could not prove), de-JITing a bintrees18-hot method and regressing
    // it to a heap-walker desync + throughput cliff — so the blanket reject is
    // removed in favour of the precise codegen handling.

    // Run a *conservative* escape pre-pass here: `jit_scan` has no
    // constant-pool resolver, so it cannot tell a trivial `<init>()V`
    // apart from an arg-bearing constructor. We therefore pass an empty
    // shape map (every `invokespecial` escapes its operands). The
    // precise pass — which re-enables scalar replacement for the
    // `new; dup; invokespecial <init>()V` pattern — runs later in
    // `compile_bytecode`, where the resolved invoke descriptors are
    // available, and overwrites this set.
    let non_escaping_new = if new_ops.is_empty() {
        std::collections::HashSet::new()
    } else {
        analyze_escapes(code, code_len, &FxHashMap::default())
    };

    Some(JitScanResult {
        needs_heap,
        multianewarray_ops,
        field_ops,
        typecheck_ops,
        checkcast_ops,
        static_field_ops,
        invoke_ops,
        new_ops,
        anewarray_ops,
        non_escaping_new,
        ldc_ops,
        ldc2w_ops,
        indy_ops,
        has_athrow,
        has_newarray,
        local_slot_ops,
    })
}

/// Result of scanning bytecode for JIT compatibility.
pub struct JitScanResult {
    pub needs_heap: bool,
    /// For each multianewarray instruction: (bytecode_pc, cp_index, ndims)
    pub multianewarray_ops: Vec<(usize, u16, u8)>,
    /// For each getfield/putfield instruction: (bytecode_pc, cp_index)
    pub field_ops: Vec<(usize, u16)>,
    /// For each checkcast/instanceof instruction: (bytecode_pc, cp_index)
    pub typecheck_ops: Vec<(usize, u16)>,
    /// cov-05: `checkcast` (0xc0) sites only — a subset of `typecheck_ops`.
    /// `ir::ir_compatible` refuses the whole method on this alone; an
    /// `instanceof`-only method is not refused here, so by construction every
    /// pc still in `typecheck_ops` for an *admitted* method is an
    /// `instanceof` site.
    pub checkcast_ops: Vec<(usize, u16)>,
    /// For each getstatic/putstatic instruction: (bytecode_pc, cp_index)
    pub static_field_ops: Vec<(usize, u16)>,
    /// For each invoke instruction: (bytecode_pc, cp_index, opcode)
    pub invoke_ops: Vec<(usize, u16, u8)>,
    /// For each `new` (0xbb) instruction: (bytecode_pc, cp_index)
    pub new_ops: Vec<(usize, u16)>,
    /// For each `anewarray` (0xbd) instruction: (bytecode_pc, cp_index)
    pub anewarray_ops: Vec<(usize, u16)>,
    /// Set of `new` bytecode PCs whose objects are non-escaping (candidates for scalar replacement).
    pub non_escaping_new: std::collections::HashSet<usize>,
    /// For each `ldc`/`ldc_w` (0x12, 0x13) instruction: (bytecode_pc, cp_index)
    pub ldc_ops: Vec<(usize, u16)>,
    /// For each `ldc2_w` (0x14) instruction: (bytecode_pc, cp_index)
    pub ldc2w_ops: Vec<(usize, u16)>,
    /// For each `invokedynamic` (0xba) instruction: (bytecode_pc, cp_index).
    /// The codegen resolves each site's target descriptor (this CP-less scan
    /// cannot) and lowers the instruction to an unconditional deopt to the
    /// interpreter via `DeoptReason::UnreachedCode` — see the 0xba codegen arm.
    pub indy_ops: Vec<(usize, u16)>,
    /// RBC.6 — the method contains `athrow` (0xbf). Never OSR-eligible
    /// (declined unconditionally in the OSR trigger, since the OSR bail
    /// path resumes at the back-edge and could re-run side effects). A
    /// method combining this with a non-empty local exception table is now
    /// compilable — see `docs/feature-designs/jit-local-exception-handlers.md`
    /// — subject to `local_handler_reads_unsafe_local`'s check on
    /// `local_slot_ops` below.
    pub has_athrow: bool,
    /// The method contains primitive `newarray` (0xbc).
    pub has_newarray: bool,
    /// RBC.6 local-handler-safety fix — every `*load`/`*store`/`iinc`
    /// instruction's `(bytecode_pc, is_store, local_slot)`, in bytecode
    /// order. See the field's push sites in `jit_scan` and
    /// `local_handler_reads_unsafe_local` (jit/src/lib.rs) for how this is
    /// used to conservatively verify a method's local exception handler(s)
    /// never observe a local the JIT's params-only handler-frame
    /// reconstruction (`route_jit_exception_through_method`) cannot
    /// recover.
    pub local_slot_ops: Vec<(usize, bool, u16)>,
}

/// Check if a bytecode method can be JIT-compiled (backward-compatible wrapper).
pub fn is_jit_compatible(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    jit_scan(code, code_len, descriptor).is_some()
}
