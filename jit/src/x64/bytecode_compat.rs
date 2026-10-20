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
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_DUPX"))
}

/// DBG bisection (spring-bug-11): disable ONLY the dup_x1 (0x5A) codegen arm,
/// to tell whether the Groovy SIGSEGV is in dup_x1 vs dup_x2 vs a co-located op.
pub(super) fn dup_x1_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_DUP_X1"))
}

/// DBG bisection (spring-bug-11): disable ONLY the dup_x2 (0x5B) codegen arm.
pub(super) fn dup_x2_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_DUP_X2"))
}

/// Disable ONLY the dup2_x2 (0x5E) codegen arm, restoring the historical
/// "reach the catch-all and stay interpreted" behaviour. The arm picks its
/// shuffle shape from the `stack_kinds` width analysis rather than from a
/// peephole, so it wants its own bisection lever independent of `dup-x1`/
/// `dup-x2`.
pub(super) fn dup2_x2_codegen_disabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_DUP2_X2"))
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
        !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD")
    })
}

/// Trace what the dup_x1 rotate did to the model: the three slots and their
/// oop marks, before and after. `CRATONVM_DBG_DUPX_METHODS` says WHICH compiled
/// methods carry the opcode; this says what happened inside each one.
pub(super) fn dupx_trace() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DUPX_TRACE"))
}

/// DBG bisection (spring-bug-11): immediately `canonicalize_stack()` after a
/// dup_x1/dup_x2 rotate, eliminating the non-canonical rotated frame offsets in
/// place. If this clears the crash, the defect is a downstream consumer of the
/// non-canonical offsets (canonicalize parallel-move / a merge that assumes
/// canonical layout); if it does NOT, the rotate model itself or a co-located
/// op is to blame.
pub(super) fn dupx_eager_canon() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DUPX_EAGER_CANON"))
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
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SP_IC_SITES"))
}

/// The deepest `multianewarray` the single-pass backend compiles.
///
/// Not a JVM limit — JVMS §4.4.1 caps an array type at 255 dimensions — but a
/// budget: the emitter writes the dimension counts into that many frame
/// scratch words before the helper call, out of a finite spill reserve. Java
/// source beyond four dimensions is vanishingly rare and the two shapes that
/// motivated lifting the old `ndims != 2` gate at all are three-dimensional
/// (`RomulusEngine`'s `new byte[3][4][4]`).
///
/// Kept in step with `MULTIANEWARRAY_INLINE_DIMS` in `vm/src/jit/helpers.rs`,
/// which is the matching no-heap fast path on the other side of the call.
pub const MAX_JIT_MULTIANEWARRAY_DIMS: usize = 8;

/// Scan a method's bytecode for single-pass-backend compatibility, recording
/// WHY it refused when it does.
///
/// A `None` here is permanent: `try_compile_inner` and the OSR door both
/// bail-list the method on it, so a method this refuses runs interpreted for
/// the life of the process — through every door, not just the one that asked.
/// That makes the reason load-bearing, and until this wrapper existed there
/// was none: the 40 `return None` arms inside [`jit_scan_inner`] recorded
/// nothing, so `jit_bridge`'s osr-DENY line — whose own comment says "NAME the
/// opcode" — printed `<unrecorded> @pc=0 op=0x00` for every one of them.
///
/// MEASURED: `org.bouncycastle.crypto.engines.RomulusEngine.skinny_128_384_plus_enc`
/// was 63% of `RomulusTest`'s samples and 138x HotSpot's time for that test,
/// because it allocates `new byte[3][4][4]` and the `multianewarray` arm below
/// refused any `ndims != 2`. Finding that took a bisect of the scanner; with
/// this wrapper the osr-DENY line names `@pc=10 op=0xc5` and points straight
/// at it. (That arm now admits any arity up to
/// [`MAX_JIT_MULTIANEWARRAY_DIMS`]; the wrapper is what made the cause
/// legible, and is why it stays.)
///
/// The wrapper costs one `Cell` store on the refusal path and nothing on the
/// success path.
pub fn jit_scan(code: &[u8], code_len: usize, descriptor: &str) -> Option<JitScanResult> {
    // Interpreter round i6 wave 116, lane L6 (page
    // `i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock`):
    // a body whose monitor bytecode is unpaired is never compiled, as HotSpot
    // refuses a method without balanced monitors (`ciMethod::
    // has_balanced_monitors`). Asked here because every compile door scans
    // with this function and bail-lists on its `None`.
    if unpaired_monitor_refusal_enabled() && monitor_bytecode_unpaired(code, code_len) {
        crate::note_jit_bail_site("jit_scan/unpaired-monitor-bytecode");
        return None;
    }
    // Interpreter round i6 wave 117, lane L2 (the open item of the page above):
    // and a body whose enters and exits are both present but provably not
    // structured ([`monitor_bytecode_unstructured`]).
    if unstructured_monitor_refusal_enabled() && monitor_bytecode_unstructured(code, code_len) {
        crate::note_jit_bail_site("jit_scan/unstructured-monitor-bytecode");
        return None;
    }
    // `usize::MAX` distinguishes "refused before the opcode loop started"
    // (the descriptor and length checks) from "refused at this bytecode".
    let mut refused_at: (usize, u8) = (usize::MAX, 0);
    let scanned = jit_scan_inner(code, code_len, descriptor, &mut refused_at);
    if scanned.is_none() {
        if refused_at.0 == usize::MAX {
            crate::note_jit_bail_site("jit_scan/pre-scan");
        } else {
            crate::note_jit_bail_site_at("jit_scan/unsupported-opcode", refused_at.0, refused_at.1);
        }
    }
    scanned
}

/// `CRATONVM_JIT_UNPAIRED_MONITOR_REFUSE` (interpreter round i6 wave 116,
/// lane L6; HotSpot parity, **default ON**, `=0` kill switch). Read on the
/// compile path only.
fn unpaired_monitor_refusal_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNPAIRED_MONITOR_REFUSE")
}

/// Does `code` hold a `monitorexit` without any `monitorenter`, or a
/// `monitorenter` without any `monitorexit`?
///
/// Compiled code releases a `monitorexit`'s object through the monitor table
/// (`jit_monitor_exit`), whichever frame took it, so a compiled body that
/// exits a monitor it never entered releases its CALLER's lock. The caller's
/// own `monitorexit` then throws inside javac's catch-any handler, which
/// covers that `monitorexit` itself, and the caller spins forever
/// (`L6W115MonitorExitOfACallersLock`, row `hot`). HotSpot never compiles
/// such a body: it runs interpreted and throws `IllegalMonitorStateException`.
///
/// Counts, not a dataflow: javac pairs every `monitorenter` with at least two
/// `monitorexit`s, and never emits either alone, so no javac body is refused.
/// A walk that loses instruction sync answers `false` (the scanner proper
/// refuses such a body anyway).
pub fn monitor_bytecode_unpaired(code: &[u8], code_len: usize) -> bool {
    let len = code_len.min(code.len());
    let (mut enters, mut exits) = (false, false);
    let mut pc = 0usize;
    while pc < len {
        match code[pc] {
            0xc2 => enters = true,
            0xc3 => exits = true,
            _ => {}
        }
        let step = crate::bytecode_analysis::step(code, pc);
        if step == 0 {
            return false;
        }
        pc += step;
    }
    enters != exits
}

/// `CRATONVM_JIT_UNSTRUCTURED_MONITOR_REFUSE` (interpreter round i6 wave 117,
/// lane L2; HotSpot parity, **default ON**, `=0` kill switch): refuse a body
/// [`monitor_bytecode_unstructured`] proves unbalanced. Read on the compile
/// path only, and only for a body with monitor bytecode.
fn unstructured_monitor_refusal_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSTRUCTURED_MONITOR_REFUSE")
}

/// The object a `monitorenter` / `monitorexit` at `pc` acts on, as far as the
/// two javac shapes name it: the local an `aload` right before it read
/// (`aload_m; monitorexit`, and `astore_m; aload_m; monitorenter`), or the
/// local a `dup; astore_m` right before it stored (javac's
/// `<expr>; dup; astore_m; monitorenter`). `None` (unknown) for anything
/// else, and when `pc` is a branch target (the textual predecessor is then not
/// the only one).
fn monitor_operand_local(code: &[u8], pc: usize, prev: &[usize], targets: &[bool]) -> Option<u16> {
    const NONE: usize = usize::MAX;
    if targets.get(pc).copied().unwrap_or(true) {
        return None;
    }
    let p = *prev.get(pc)?;
    if p == NONE {
        return None;
    }
    if let Some(local) = load_local(code, p, 0x19, 0x2a) {
        return Some(local);
    }
    let stored = load_local(code, p, 0x3a, 0x4b)?;
    if targets.get(p).copied().unwrap_or(true) {
        return None;
    }
    let pp = *prev.get(p)?;
    (pp != NONE && code.get(pp) == Some(&0x59)).then_some(stored)
}

/// The local index of the `aload` (`base` 0x19, `short` 0x2a) or `astore`
/// (0x3a, 0x4b) at `pc`, in all three encodings (short, indexed, `wide`).
fn load_local(code: &[u8], pc: usize, base: u8, short: u8) -> Option<u16> {
    let op = *code.get(pc)?;
    if (short..short + 4).contains(&op) {
        return Some(u16::from(op - short));
    }
    if op == base {
        return code.get(pc + 1).map(|&i| u16::from(i));
    }
    if op == 0xc4 && code.get(pc + 1) == Some(&base) {
        return Some(u16::from_be_bytes([*code.get(pc + 2)?, *code.get(pc + 3)?]));
    }
    None
}

/// One held monitor of the abstract state: the local the javac shape stored
/// its object in, or unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeldLock {
    Local(u16),
    Unknown,
}

/// Does `code` hold monitor bytecode that is provably NOT structured, which
/// HotSpot refuses to compile (`ciMethod::has_balanced_monitors`) and the
/// single-pass backend must not compile either (`jit_monitor_exit` releases
/// an object through the monitor table, whichever frame took it)?
/// Interpreter round i6 wave 117, lane L2; the open item of
/// `i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock`.
///
/// A monitor-DEPTH dataflow over the normal control flow from pc 0 (exception
/// edges are not followed: `jit_scan` has no exception table, and a handler
/// is only reached through one), with each held monitor named by the local
/// its javac shape keeps the object in ([`monitor_operand_local`]) and no
/// other slot tracking. Proven unbalanced, answering `true`:
///
/// * a `monitorexit` with no monitor held on some path;
/// * a `monitorexit` whose named local differs from the named local of the
///   innermost held monitor (`aload_0; monitorenter; aload_1; monitorexit`);
/// * a `*return` with a monitor held;
/// * two paths reaching one pc with different depths.
///
/// Everything else answers `false`: an operand the two shapes do not name
/// (unknown matches anything), a local stored to while it names a held
/// monitor (that monitor becomes unknown), a subroutine (`jsr` / `ret`), an
/// undecodable successor. So a javac body is never refused: javac keeps the
/// lock in a dedicated local (`dup; astore_m; monitorenter`), exits through
/// `aload_m; monitorexit` on every normal path, and leaves only its catch-any
/// handler (not walked) to exit on the exceptional ones.
pub fn monitor_bytecode_unstructured(code: &[u8], code_len: usize) -> bool {
    use crate::bytecode_analysis as ba;
    const NONE: usize = usize::MAX;
    let len = code_len.min(code.len());
    // The linear walk: instruction starts, each one's textual predecessor,
    // and whether there is any monitor bytecode at all.
    let mut prev = vec![NONE; len];
    let mut any_monitor = false;
    let mut pc = 0usize;
    let mut last = NONE;
    while pc < len {
        prev[pc] = last;
        match code[pc] {
            0xc2 | 0xc3 => any_monitor = true,
            op if ba::is_subroutine_op(op) || ba::is_wide_ret(code, pc) => return false,
            _ => {}
        }
        let step = ba::step(code, pc);
        if step == 0 {
            return false;
        }
        last = pc;
        pc += step;
    }
    if !any_monitor {
        return false;
    }
    let targets = ba::branch_target_map(code, len);
    let mut states: Vec<Option<Vec<HeldLock>>> = vec![None; len];
    states[0] = Some(Vec::new());
    let mut work = vec![0usize];
    let mut succs = Vec::new();
    while let Some(pc) = work.pop() {
        let Some(mut held) = states[pc].clone() else {
            continue;
        };
        let op = code[pc];
        match op {
            0xc2 => {
                let named = monitor_operand_local(code, pc, &prev, &targets)
                    .map_or(HeldLock::Unknown, HeldLock::Local);
                held.push(named);
            }
            0xc3 => {
                let Some(top) = held.pop() else {
                    return true;
                };
                let named = monitor_operand_local(code, pc, &prev, &targets);
                if let (HeldLock::Local(entered), Some(exited)) = (top, named) {
                    if entered != exited {
                        return true;
                    }
                }
            }
            0xac..=0xb1 if !held.is_empty() => return true,
            _ => {
                // A store to a local that names a held monitor: from here on
                // that monitor's object is no longer known by the local.
                if let Some(stored) = load_local(code, pc, 0x3a, 0x4b) {
                    for lock in held.iter_mut() {
                        if *lock == HeldLock::Local(stored) {
                            *lock = HeldLock::Unknown;
                        }
                    }
                }
            }
        }
        succs.clear();
        if !ba::normal_successors(code, len, pc, &mut succs) {
            return false;
        }
        for &next in &succs {
            if next >= len {
                continue;
            }
            let Some(seen) = states[next].as_mut() else {
                states[next] = Some(held.clone());
                work.push(next);
                continue;
            };
            if seen.len() != held.len() {
                return true;
            }
            let mut changed = false;
            for (a, b) in seen.iter_mut().zip(&held) {
                if *a != *b && *a != HeldLock::Unknown {
                    *a = HeldLock::Unknown;
                    changed = true;
                }
            }
            if changed {
                work.push(next);
            }
        }
    }
    false
}

#[cfg(test)]
mod i117_l2_unstructured_monitor_tests {
    use super::monitor_bytecode_unstructured as unstructured;

    #[test]
    fn javac_synchronized_block_is_structured() {
        // synchronized (o) { o.hashCode(); }   (o in local 0, lock in local 1)
        //  0 aload_0; 1 dup; 2 astore_1; 3 monitorenter; 4 aload_0;
        //  5 invokevirtual #7; 8 pop; 9 aload_1; 10 monitorexit; 11 goto +8 (19);
        // 14 astore_2; 15 aload_1; 16 monitorexit; 17 aload_2; 18 athrow; 19 return
        let code = [
            0x2a, 0x59, 0x4c, 0xc2, 0x2a, 0xb6, 0x00, 0x07, 0x57, 0x2b, 0xc3, 0xa7, 0x00, 0x08,
            0x4d, 0x2b, 0xc3, 0x2c, 0xbf, 0xb1,
        ];
        assert!(!unstructured(&code, code.len()));
    }

    #[test]
    fn javac_return_inside_and_nested_blocks_are_structured() {
        // synchronized (a) { synchronized (b) { return 1; } }
        //  0 aload_0; dup; astore_2; monitorenter        (lock a in 2)
        //  4 aload_1; dup; astore_3; monitorenter        (lock b in 3)
        //  8 iconst_1; aload_3; monitorexit; aload_2; monitorexit; ireturn
        // 14 handler: astore 4; aload_3; monitorexit; aload 4; athrow   (not walked)
        let code = [
            0x2a, 0x59, 0x4d, 0xc2, 0x2b, 0x59, 0x4e, 0xc2, 0x04, 0x2d, 0xc3, 0x2c, 0xc3, 0xac,
            0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf,
        ];
        assert!(!unstructured(&code, code.len()));
    }

    #[test]
    fn javac_block_in_a_loop_is_structured() {
        // for (;;) { if (c) break; synchronized (o) { } }   o in 0, c in 1, lock in 2
        //  0 iload_1; ifne +12 (13); aload_0; dup; astore_2; monitorenter;
        //  8 aload_2; monitorexit; goto -10 (0); 13 return
        let code = [
            0x1b, 0x9a, 0x00, 0x0c, 0x2a, 0x59, 0x4d, 0xc2, 0x2c, 0xc3, 0xa7, 0xff, 0xf6, 0xb1,
        ];
        assert!(!unstructured(&code, code.len()));
    }

    #[test]
    fn an_exit_of_another_object_is_unstructured() {
        // aload_0; monitorenter; aload_1; monitorexit; return
        assert!(unstructured(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], 5));
    }

    #[test]
    fn a_lock_held_at_return_is_unstructured() {
        // aload_0; monitorenter; aload_0; monitorexit; aload_0; monitorenter; return
        assert!(unstructured(&[0x2a, 0xc2, 0x2a, 0xc3, 0x2a, 0xc2, 0xb1], 7));
    }

    #[test]
    fn an_exit_with_nothing_held_is_unstructured() {
        // aload_0; monitorenter; aload_0; monitorexit; aload_0; monitorexit; return
        assert!(unstructured(&[0x2a, 0xc2, 0x2a, 0xc3, 0x2a, 0xc3, 0xb1], 7));
    }

    #[test]
    fn a_depth_that_depends_on_the_path_is_unstructured() {
        // iload_1; ifeq +6 (7); aload_0; monitorenter; nop; 7 aload_0; monitorexit; return
        let code = [0x1b, 0x99, 0x00, 0x06, 0x2a, 0xc2, 0x00, 0x2a, 0xc3, 0xb1];
        assert!(unstructured(&code, code.len()));
    }

    #[test]
    fn unknown_operands_and_no_monitors_are_not_refused() {
        // getstatic #1; monitorenter; getstatic #1; monitorexit; return
        let code = [0xb2, 0x00, 0x01, 0xc2, 0xb2, 0x00, 0x01, 0xc3, 0xb1];
        assert!(!unstructured(&code, code.len()));
        assert!(!unstructured(&[0x1a, 0xac], 2));
        // An exit of a local stored over while held: unknown, not refused.
        // aload_0; astore_1; aload_1; monitorenter; aload_0; astore_1; aload_1; monitorexit; return
        let code = [0x2a, 0x4c, 0x2b, 0xc2, 0x2a, 0x4c, 0x2b, 0xc3, 0xb1];
        assert!(!unstructured(&code, code.len()));
    }
}

#[cfg(test)]
mod i116_l6_unpaired_monitor_tests {
    use super::monitor_bytecode_unpaired;

    #[test]
    fn only_an_enter_or_an_exit_alone_is_unpaired() {
        // aload_0; monitorexit; return -- the probe's `Exit.exit`.
        assert!(monitor_bytecode_unpaired(&[0x2a, 0xc3, 0xb1], 3));
        // aload_0; monitorenter; return.
        assert!(monitor_bytecode_unpaired(&[0x2a, 0xc2, 0xb1], 3));
        // aload_0; monitorenter; aload_0; monitorexit; return.
        assert!(!monitor_bytecode_unpaired(&[0x2a, 0xc2, 0x2a, 0xc3, 0xb1], 5));
        // No monitor bytecode at all.
        assert!(!monitor_bytecode_unpaired(&[0x1a, 0xac], 2));
        // javac's `synchronized (o) { o.hashCode(); }` shape: one enter, two exits.
        let javac = [
            0x2a, 0x59, 0x4c, 0xc2, 0x2a, 0xb6, 0x00, 0x07, 0x57, 0x2b, 0xc3, 0xa7, 0x00, 0x08,
            0x4d, 0x2b, 0xc3, 0x2c, 0xbf, 0xb1,
        ];
        assert!(!monitor_bytecode_unpaired(&javac, javac.len()));
        // A `0xc3` operand byte (sipush 0x00c3) is not an instruction.
        assert!(!monitor_bytecode_unpaired(&[0x11, 0x00, 0xc3, 0xac], 4));
    }
}

/// The scanner proper. `refused_at` is updated to the `(pc, opcode)` under
/// inspection at the top of every iteration, so whichever `return None` arm
/// fires, the caller above knows which bytecode it was.
fn jit_scan_inner(
    code: &[u8],
    code_len: usize,
    descriptor: &str,
    refused_at: &mut (usize, u8),
) -> Option<JitScanResult> {
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
    let mut has_putstatic = false;
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
                                                                  // (`jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md`)
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
        // Whichever `return None` arm below fires, this is the bytecode it saw.
        *refused_at = (pc, op);
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
            // irem, lrem, frem, drem. Both tiers lower frem/drem (0x72/0x73) to
            // a CALL to the jit_frem/jit_drem fmod helper.
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
            // in its own comment: `bytecode_analysis::step` and `bytecode_analysis::step`
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
                        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
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
            // goto_w — `goto` with a 32-bit offset. javac emits it for a
            // branch spanning more than 32 KiB and ASM-based rewriters emit it
            // whenever injected code pushes a branch past that span. Lowered
            // by `walk_control`'s `0xc8` arm; the walk's branch-target map and
            // every analysis on the single-pass path decode it through
            // `bytecode_analysis` (the ones that do not model it refuse or stay
            // conservative: `detect_loops` does not call a `goto_w` back edge a
            // loop, the BCE edge scans and the loop rewriter refuse the method).
            // Before this arm, one `goto_w` anywhere kept the whole method
            // interpreted for the life of the process
            // (`single-pass-refuses-every-method-containing-goto-w-20260918.md`).
            0xc8 => {
                if pc + 4 >= code_len {
                    return None;
                }
                pc += 5;
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
                has_putstatic = true;
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
            // multianewarray — multi-dimensional array allocation, any arity
            // up to [`MAX_JIT_MULTIANEWARRAY_DIMS`].
            0xc5 => {
                if pc + 3 >= code_len {
                    return None;
                }
                let cp_idx = ((code[pc + 1] as u16) << 8) | (code[pc + 2] as u16); // Widening: always safe
                let ndims = code[pc + 3];
                // `ndims == 0` is not a legal classfile (JVMS §6.5
                // multianewarray: "dimensions must be greater than or equal to
                // 1"); the interpreter raises an internal error on it. Refusing
                // it here keeps that shape out of the emitter, which would
                // otherwise pop nothing and push an array.
                //
                // The upper bound is this backend's, not the JVM's: the
                // emitter pins the dimensions into `ndims` frame scratch words
                // before the call, and the spill reserve is finite. Admitting
                // a method the emitter will then refuse costs a PERMANENT
                // whole-method bail (see this function's doc comment), so the
                // scanner declines the same shapes the emitter would.
                if ndims == 0 || ndims as usize > MAX_JIT_MULTIANEWARRAY_DIMS {
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
            // tableswitch — accept in the scanner. Both tiers pick a SHAPE for
            // it further downstream (`x64/op_control.rs`'s `0xaa` arm in the
            // single-pass backend, `ir_lower`'s `Op::Switch` arm in the
            // optimizing one): a jump table, a binary search, or a compare
            // chain. This screen only has to agree about where the instruction
            // ENDS.
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
            // lookupswitch — accepted on the same terms as `tableswitch`
            // above, and lowered by the same three-shape choice.
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
            // RBC.6 — athrow. Accepted; `has_athrow` is recorded for the
            // gates that still ask it: the single-pass OSR door withdraws
            // an athrow method's entries inside a protected range, and
            // `CRATONVM_JIT_OSR_ATHROW=0` restores the blanket OSR refusal
            // (see the field's doc). It no longer keeps a method with local
            // handlers from compiling, and the OSR lift is default-on: an
            // exception the OSR'd frame cannot catch unwinds from the
            // caller, with no stale back-edge resume. `analyze_escapes`
            // already treats 0xbf as a full-escape barrier, so scalar
            // replacement stays sound.
            0xbf => {
                has_athrow = true;
                pc += 1;
            }
            // Anything else: not JIT-compatible
            _ => {
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
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
        has_putstatic,
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
    /// RBC.6 — the method contains `athrow` (0xbf). OSR-eligible unless
    /// `CRATONVM_JIT_OSR_ATHROW=0`; with a non-empty exception table the OSR
    /// door withdraws the artifact's entries inside a protected range
    /// (`jit_bridge::withdraw_osr_entries_inside_protected_ranges`). A
    /// method combining this with a non-empty local exception table is now
    /// compilable — see `docs/feature-designs/jit-local-exception-handlers.md`
    /// — subject to `local_handler_reads_unsafe_local`'s check on
    /// `local_slot_ops` below.
    pub has_athrow: bool,
    /// The method contains primitive `newarray` (0xbc).
    pub has_newarray: bool,
    /// Whether the method contains a `putstatic` (0xb3). `static_field_ops`
    /// lists reads and writes together, and only a write is refused by
    /// `ir::ir_compatible`.
    pub has_putstatic: bool,
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
