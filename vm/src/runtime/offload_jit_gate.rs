// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GPU-offload JIT admission gate.
//!
//! Follow-up item 2 in
//! `fixed-suite-bugs/gpu-offload-followups-20260711.md` ("JIT-compiled
//! callers bypass the offload hook"): the transparent GPU-offload hook
//! ([`crate::runtime::offload::try_dispatch`]) only fires from the
//! *interpreter's* `execute_invokestatic` slow path. If the **caller**
//! method containing the eligible `invokestatic` is itself promoted to
//! JIT-compiled (or OSR-compiled) code, dispatch moves into JIT-emitted
//! code and the hook is never consulted again — offload silently stops
//! for that call site, permanently, for the life of the process.
//!
//! The conservative fix implemented here: while GPU offload is active
//! (`--gpu`, a usable CUDA device), refuse to admit a caller method to
//! the JIT/OSR pipeline at all when its bytecode contains an
//! `invokestatic` (0xB8) whose resolved target is GPU-offload-eligible.
//! The offloaded kernel dominates the runtime of any such loop, so
//! leaving the interpreted caller loop interpreted is an acceptable
//! cost — the alternative (a JIT-compiled caller that silently never
//! offloads again) is strictly worse.
//!
//! # Inline array stores: the cache stands down, not the JIT
//!
//! A method that stores into an `int[]`/`long[]`/`float[]`/`double[]`
//! also cannot run compiled while `offload::input_cache` is live. That
//! has nothing to do with where a kernel is called from: the cache
//! mirrors a Java array in device memory across submissions, so a host
//! write must evict the entry, and while the interpreter's `*astore`
//! arms and the `jit_iastore`/`jit_bastore` helpers all call
//! `input_cache::invalidate`, the JIT's IR pipeline lowers
//! `Op::ArrayStore` to a raw inline `MOVSS`/`MOVSD` with no helper to
//! hook.
//!
//! Until 2026-09-02 this gate resolved that by refusing to compile such
//! a method. It is sound, and it is enormously broad — it fired on any
//! method writing a primitive array, kernel-adjacent or not, so
//! attaching a GPU de-optimised the CPU half of every mixed workload.
//! This module's own note conceded it was "the common case for the
//! *producer* method rather than the caller", i.e. it fired far more
//! often than the invokestatic reason it shares this file with.
//!
//! The trade is now inverted: admitting such a method calls
//! [`crate::runtime::offload::input_cache::disable_for_jit_array_writer`],
//! which drops the cache and refuses further entries, and the method is
//! compiled. Both directions are sound; this one puts the cost on the
//! path that benefits from the cache (one H2D copy per submit for an
//! array re-submitted unchanged) instead of on unrelated CPU code.
//!
//! It is also decided lazily rather than up front. A program that never
//! JIT-compiles an array writer keeps the cache exactly as before; one
//! that does keeps its native code and loses the cache from that moment.
//! Neither ever pays both. See that function for why admission is early
//! enough to be safe.
//!
//! The FIRST reason still refuses: a method containing an `invokestatic`
//! to an offload-eligible target is still kept interpreted, because the
//! hook that dispatches it only fires from the interpreter. That one is
//! narrow and its cost is argued above.
//!
//! # Entirely `gpu-offload`-gated
//!
//! Like [`crate::runtime::offload`], this whole module only exists when
//! the `gpu-offload` Cargo feature is enabled — see the `#![cfg(...)]`
//! below. On a CPU-only build the module is not compiled at all: zero
//! size, zero symbols, zero effect. On a `gpu-offload` build running
//! with `--gpu` off (or with `--gpu` on but no usable CUDA driver),
//! [`caller_blocks_jit`] costs exactly one `bool` read
//! (`shared.config.gpu_offload_enabled`) before returning `false` — the
//! `RwLock`-guarded cache and the offload registry are never touched.
//!
//! # Design
//!
//! - [`caller_blocks_jit`] is the primary entry point, matching the
//!   `(SharedVm, ClassId, method_index)` shape the JIT admission sites
//!   already resolve their target method with (mirrors the cache key
//!   [`crate::runtime::offload::OffloadCache`] uses for the exact same
//!   reason: `ClassId` + method-index-in-class is stable for the
//!   lifetime of a loaded class and is cheaper to hash than an
//!   interned-string triple).
//! - [`caller_blocks_jit_by_name`] is a convenience wrapper for call
//!   sites that only have `(class_name, method_name, descriptor)` in
//!   scope (every JIT admission site in `interpreter.rs` — see
//!   `should_skip_jit_with_init`'s callers — resolves the method by
//!   name, not by index). It resolves the index once via a linear scan
//!   of `class.methods` (the same pattern
//!   [`crate::runtime::offload::try_dispatch`] uses) and delegates.
//! - The verdict for a given `(ClassId, method_index)` is cached for
//!   the life of the process in [`GATE_CACHE`]. **Class redefinition is
//!   out of scope**: if a class is redefined after its caller methods
//!   were already scanned, a stale cached verdict is not invalidated.
//!   This mirrors the existing GPU-offload subsystem's stance (the
//!   `OffloadCache` kernel/blacklist maps have the same lifetime
//!   contract) and is consistent with `--gpu` being a
//!   startup/benchmarking flag, not a hot-reload-friendly one.
//!
//! # Known limitations (documented, not fixed here)
//!
//! - **Forward references.** [`compute`] can only judge a call target
//!   eligible if the target's declaring class is *already loaded* at
//!   the moment the caller is scanned. A caller compiled/scanned before
//!   its callee's class has ever been loaded will not see the callee as
//!   eligible and will NOT be blocked from JIT admission — offload can
//!   still be silently dropped for that specific caller. Closing this
//!   gap needs either a reverse (callee → known callers) index rebuilt
//!   on every class load, or re-running this gate at every promotion
//!   attempt rather than caching permanently; both are out of scope for
//!   the conservative first fix.
//! - **Hint-loosened kernels.** [`compute`] calls
//!   [`jit_cuda::analyzer::analyze`] (the strict, annotation-free
//!   verdict — `MethodAnnotations::default()`), matching the task's
//!   guidance that a plain `Eligible` verdict is sufficient for this
//!   gate. A kernel that is only eligible because of a
//!   `@GpuKernel`/`AdmissionHint` annotation (see
//!   `jit_cuda::analyzer::analyze_with_annotations`) is invisible to
//!   this scan and will not block its caller's JIT admission. This is
//!   intentionally conservative in the direction that costs offload
//!   throughput, not correctness — the worst case is a JIT-compiled
//!   caller that stops offloading, exactly the pre-existing bug this
//!   module fixes for the common (unannotated) case.
//! - **`invokedynamic`-mediated calls** (method references, lambdas)
//!   are not scanned — only literal `invokestatic` bytecodes. A caller
//!   that reaches an eligible kernel through a `MethodHandle` is not
//!   covered.
//!
//! # Integration status
//!
//! This module is fully implemented and unit-tested but is **not yet
//! wired into the JIT admission path**. Every JIT/OSR admission call
//! site that would need a one-line addition
//! (`crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(shared,
//! class_id, class_name, method_name, descriptor)` ORed into the
//! existing skip/deny check) lives inside `vm/src/runtime/interpreter.rs`,
//! which — in the multi-agent session that authored this module — is
//! owned by a different, concurrently-editing agent and was read-only
//! for this change. The four sites (all funnel through
//! `crate::jit::skip_list::should_skip_jit_with_init`, confirming that
//! function's own doc comment: "the single source of truth"):
//!
//! 1. First-call eager compile path (`fn execute`, the `static_skip_reason`
//!    check around the `should_skip_jit_with_init` call near line 4321).
//! 2. `try_jit_upgrade_with_gate` — caller-invocation-count promotion,
//!    `should_skip_jit_with_init` call near line 27156.
//! 3. The recursive callee-compile closure inside the caller-counter
//!    promotion path, `should_skip_jit_with_init` call near line 27510.
//! 4. `try_jit_compile_callee_slow` — the callee-dispatcher compile path
//!    used by JIT helper callbacks, `should_skip_jit_with_init` call near
//!    line 28268.
//! 5. `compile_osr_artifact` — **the OSR path this whole module exists
//!    for** ("OSR of a hot loop that contains the invokestatic"),
//!    `should_skip_jit_with_init` call near line 25586.
//!
//! Every one of those five call sites already has `shared: &SharedVm`,
//! a resolved `class_id`/`declaring_class_id`, and the method's
//! `class_name`/`method_name`/`descriptor` in scope, so wiring in
//! [`caller_blocks_jit_by_name`] at each is mechanically a one-line
//! addition (`|| crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(...)`
//! next to that site's existing skip check) — no further design work
//! needed, just ownership of the file.

#![cfg(feature = "gpu-offload")]

use std::sync::OnceLock;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::classloading::ClassId;
use crate::vm::SharedVm;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// `(vm_identity, ClassId, method_index_in_class)`.
///
/// PER-VM STATE (P0, `docs/architecture/per-vm-state.md`). The `vm_identity`
/// component is load-bearing, not decorative: `ClassId`s are allocated per-VM
/// (`ClassStore::next_id` returns `self.classes.len()`), so with two VMs live
/// `(ClassId(7), 3)` names two unrelated methods. Without the VM key, the
/// second VM would read the first VM's verdict and either deny JIT admission
/// to a method with no offloadable `invokestatic` (a silent throughput cliff)
/// or — the dangerous direction — admit a method the analyzer would have
/// blocked. The verdict is a function of the method's bytecode, which lives in
/// a specific VM's class store, so the VM has to be part of the key.
type GateKey = (usize, ClassId, u16);

/// Process-lifetime cache of admission verdicts, keyed by [`GateKey`] — the
/// same stable `(ClassId, method_index)` pair
/// [`crate::runtime::offload::OffloadCache`] uses, prefixed with the owning
/// VM. See the module docs' "Design" section for why this is never
/// invalidated.
static GATE_CACHE: OnceLock<RwLock<FxHashMap<GateKey, bool>>> = OnceLock::new();

fn cache() -> &'static RwLock<FxHashMap<GateKey, bool>> {
    GATE_CACHE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// Returns `true` iff `class_id`'s method at `method_index` must be
/// denied JIT/OSR admission because GPU offload is active and the
/// method's bytecode contains an `invokestatic` whose resolved target
/// the GPU analyzer considers offload-eligible.
///
/// Cheapest-check-first: `shared.config.gpu_offload_enabled` is read
/// before touching the cache lock or the offload registry, so a
/// `--gpu`-off run pays exactly one `bool` read per call — the task's
/// "one boolean check" budget.
pub fn caller_blocks_jit(shared: &SharedVm, class_id: ClassId, method_index: u16) -> bool {
    if !shared.config.gpu_offload_enabled {
        return false;
    }
    let key: GateKey = (shared.vm_identity, class_id, method_index);
    if let Some(&verdict) = cache().read().get(&key) {
        return verdict;
    }
    let verdict = compute(shared, class_id, method_index);
    cache().write().insert(key, verdict);
    verdict
}

/// Convenience wrapper for call sites that resolve their method by
/// `(class_name, method_name, descriptor)` rather than by index — i.e.
/// every JIT admission site in `interpreter.rs` (see module docs). Looks
/// up the method's index within `class_id` via a linear scan (the same
/// pattern `offload::try_dispatch` uses to key its own cache) and
/// delegates to [`caller_blocks_jit`]. Returns `false` (never blocks)
/// if the class or method cannot be resolved — an admission gate must
/// fail open on "can't tell", not deny compilation of methods it
/// couldn't even identify.
pub fn caller_blocks_jit_by_name(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    if !shared.config.gpu_offload_enabled {
        return false;
    }
    let method_index = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return false;
        };
        let Some(idx) = class
            .methods
            .iter()
            .position(|m| &*m.name == method_name && &*m.descriptor == method_descriptor)
        else {
            return false;
        };
        idx as u16
    };
    caller_blocks_jit(shared, class_id, method_index)
}

/// The actual analysis behind [`caller_blocks_jit`]'s cache miss path.
/// See the module docs' "Known limitations" section for what this
/// deliberately does not handle (forward class references,
/// annotation-loosened kernels, `invokedynamic`-mediated calls).
fn compute(shared: &SharedVm, class_id: ClassId, method_index: u16) -> bool {
    // Mirrors the exact "no device -> Skip" short-circuit
    // `OffloadCache::lookup_or_compile` uses, via the same public
    // `has_device()` accessor the interpreter hook consults.
    let registry = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);
    if !registry.has_device() {
        return false;
    }

    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    let Some(method) = class.methods.get(method_index as usize) else {
        return false;
    };
    let Some(code_attr) = method.code() else {
        return false;
    };

    // Phase 10 #2, JIT half — INVERTED, AUDIT 2026-09-02.
    //
    // A method writing an int/long/float/double array used to be refused
    // outright, because the IR pipeline's inline `MOVSS`/`MOVSD` store
    // has no hook to invalidate the input-residency cache from. That is
    // sound and enormously broad: the reason has nothing to do with
    // whether the method has ever seen a kernel, so `--gpu` de-optimised
    // the CPU half of every mixed workload — a ray tracer's setup loops,
    // an inference pipeline's array fills — to keep coherent a cache
    // most of those methods will never touch. It was also, by the
    // module's own note, "the common case for the *producer* method
    // rather than the caller", i.e. it fired far more often than the
    // reason it shares this function with.
    //
    // Both sides of the trade are sound; the question is which pays.
    // Now the CACHE stands down instead: admitting this method disables
    // the residency cache and drops what it holds, and the method gets
    // compiled. Ordering is the argument — this runs at admission,
    // strictly before the compiled code can execute a store, so there is
    // no window in which a compiled store meets a live entry.
    //
    // A program that never JIT-compiles an array writer keeps the cache
    // exactly as before; one that does keeps its native code and pays
    // one H2D copy per submit for arrays it re-submits unchanged.
    // Neither pays both, and nothing had to be decided up front.
    if method_writes_primitive_array(&code_attr.code) {
        crate::runtime::offload::input_cache::disable_for_jit_array_writer();
        // Fall through to the invokestatic scan: this method may ALSO
        // contain a call to an offload-eligible kernel, which is the
        // other, narrower reason to refuse it, and that one still holds.
    }

    let cp_indices = scan_invokestatic_cp_indices(&code_attr.code);
    if cp_indices.is_empty() {
        return false;
    }

    for cp_index in cp_indices {
        let Some((target_class_name, target_method_name, target_descriptor)) =
            resolve_method_ref(&class.constant_pool, cp_index)
        else {
            continue;
        };

        // Known limitation (documented in the module docs): if the
        // target's declaring class is not yet loaded we cannot judge
        // eligibility and simply don't count this call site. We do NOT
        // treat "not loaded" as "assume eligible" — that would make an
        // ordinary VM boot (where most classes referenced by a
        // freshly-loaded caller are not loaded yet) ban huge swaths of
        // unrelated code from the JIT.
        let Some(target_class_id) = cm.get_loaded_class_id(target_class_name) else {
            continue;
        };
        let Some(target_class) = cm.get_class(target_class_id) else {
            continue;
        };
        let Some(target_method) = target_class
            .methods
            .iter()
            .find(|m| &*m.name == target_method_name && &*m.descriptor == target_descriptor)
        else {
            continue;
        };

        // Plain, annotation-free verdict — see "Known limitations" for
        // why hint-loosened kernels are intentionally not covered.
        if matches!(
            jit_cuda::analyzer::analyze(target_method),
            jit_cuda::OffloadVerdict::Eligible(_)
        ) {
            return true;
        }
    }
    false
}

/// Walk `code` instruction-by-instruction — correctly skipping every
/// opcode's operand bytes, including the variable-length `tableswitch`
/// / `lookupswitch` / `wide` forms — and collect the constant-pool
/// index operand of every `invokestatic` (0xB8) instruction found.
///
/// A naive byte-scan for `0xB8` would false-positive on any operand
/// byte that happens to equal `0xB8` (e.g. a `bipush 0xB8` immediate,
/// or a jump-table entry byte), so this mirrors the same
/// instruction-length table used elsewhere in the JIT admission layer
/// (`crate::jit::skip_list::classify_init_complexity`) — written
/// independently here rather than shared, since that function lives in
/// a file this module does not own and answers a different question
/// (constructor/initializer triviality, not call-target scanning).
///
/// Conservative on any parse anomaly (truncated operand, malformed
/// switch table): stops the walk and returns whatever was already
/// collected rather than panicking or misinterpreting subsequent bytes.
fn scan_invokestatic_cp_indices(code: &[u8]) -> Vec<u16> {
    scan_code(code).0
}

/// Walk `code` once, collecting both facts the gate needs:
///
/// * the constant-pool index of every `invokestatic`, and
/// * whether the method stores into a primitive array of a type the GPU
///   input cache can hold — `iastore` (0x4f), `lastore` (0x50),
///   `fastore` (0x51), `dastore` (0x52).
///
/// The second is what closes the JIT half of Phase 10 #2. See
/// [`method_writes_primitive_array`].
fn scan_code(code: &[u8]) -> (Vec<u16>, bool) {
    let mut out = Vec::new();
    let mut writes_array = false;
    let mut pc = 0usize;
    while pc < code.len() {
        let op = code[pc];
        if op == 0xb8 {
            if pc + 2 < code.len() {
                out.push(u16::from_be_bytes([code[pc + 1], code[pc + 2]]));
            } else {
                break;
            }
        }
        // iastore / lastore / fastore / dastore. Reached only on a real
        // instruction boundary, so an operand byte that happens to equal
        // one of these cannot false-positive.
        if (0x4f..=0x52).contains(&op) {
            writes_array = true;
        }
        let len = match op {
            0x00..=0x0f => 1,
            0x10 => 2,        // bipush
            0x11 => 3,        // sipush
            0x12 => 2,        // ldc
            0x13 | 0x14 => 3, // ldc_w / ldc2_w
            0x15..=0x19 => 2, // iload..aload
            0x1a..=0x35 => 1, // iload_n..saload
            0x36..=0x3a => 2, // istore..astore
            0x3b..=0x56 => 1, // istore_n..sastore
            0x57..=0x5f => 1, // pop..swap
            0x60..=0x83 => 1, // arithmetic
            0x84 => 3,        // iinc
            0x85..=0x93 => 1, // conversions
            0x94..=0x98 => 1, // lcmp/fcmpl/fcmpg/dcmpl/dcmpg
            0x99..=0xa6 => 3, // ifeq..if_acmpne
            0xa7 => 3,        // goto
            0xa8 => 3,        // jsr
            0xa9 => 2,        // ret
            0xaa => {
                // tableswitch — pad to 4-byte boundary, then 12-byte
                // header (default, low, high), then (high-low+1) jumps.
                let pad = (4 - ((pc + 1) % 4)) % 4;
                let table = pc + 1 + pad;
                if table + 12 > code.len() {
                    break;
                }
                let low = i32::from_be_bytes([
                    code[table + 4],
                    code[table + 5],
                    code[table + 6],
                    code[table + 7],
                ]);
                let high = i32::from_be_bytes([
                    code[table + 8],
                    code[table + 9],
                    code[table + 10],
                    code[table + 11],
                ]);
                let n = (high as i64 - low as i64 + 1).max(0) as usize;
                1 + pad + 12 + n * 4
            }
            0xab => {
                // lookupswitch — pad to 4-byte boundary, then 8-byte
                // header (default, npairs), then npairs * 8-byte pairs.
                let pad = (4 - ((pc + 1) % 4)) % 4;
                let table = pc + 1 + pad;
                if table + 8 > code.len() {
                    break;
                }
                let npairs = u32::from_be_bytes([
                    code[table + 4],
                    code[table + 5],
                    code[table + 6],
                    code[table + 7],
                ]) as usize;
                1 + pad + 8 + npairs * 8
            }
            0xac..=0xb1 => 1, // ireturn..return
            0xb2..=0xb6 => 3, // getstatic, putstatic, getfield, putfield, invokevirtual
            0xb7 | 0xb8 => 3, // invokespecial, invokestatic
            0xb9 => 5,        // invokeinterface (index2, count, 0)
            0xba => 5,        // invokedynamic (index2, 0, 0)
            0xbb => 3,        // new
            0xbc => 2,        // newarray
            0xbd => 3,        // anewarray
            0xbe..=0xbf => 1, // arraylength, athrow
            0xc0..=0xc1 => 3, // checkcast, instanceof
            0xc2..=0xc3 => 1, // monitorenter, monitorexit
            0xc4 => {
                // wide prefix: next opcode is widened.
                if pc + 1 >= code.len() {
                    break;
                }
                if code[pc + 1] == 0x84 {
                    6 // wide iinc: c4 84 idx2 const2
                } else {
                    4 // wide iload..astore: c4 op idx2
                }
            }
            0xc5 => 4,        // multianewarray
            0xc6 | 0xc7 => 3, // ifnull, ifnonnull
            0xc8 | 0xc9 => 5, // goto_w, jsr_w
            _ => 1,
        };
        if len == 0 {
            break;
        }
        pc += len;
    }
    (out, writes_array)
}

/// Does `code` store into an `int[]`/`long[]`/`float[]`/`double[]`?
///
/// # Why the gate cares
///
/// The GPU input-residency cache (`offload::input_cache`) must be
/// dropped whenever the host writes an array the device is mirroring.
/// The interpreter's `*astore` arms and the `jit_iastore` / `jit_bastore`
/// helpers all call `input_cache::invalidate`, but the JIT's IR pipeline
/// lowers `Op::ArrayStore` to a **raw inline `MOVSS`/`MOVSD`**
/// (`jit/src/ir_lower.rs`) with no helper call at all. There is no
/// callback to hook, and emitting one per element store would put a
/// branch and a potential call in the middle of every compiled array
/// write.
///
/// So while offload is active, such a method is simply not admitted to
/// the JIT — the same conservative trade this module already makes for
/// eligible callers (see the module comment): a correct interpreted loop
/// beats a compiled one that silently feeds the kernel stale data.
///
/// This costs nothing on a CPU-only build (module not compiled), and
/// nothing on a `gpu-offload` build running without a usable `--gpu`
/// device, because [`caller_blocks_jit`] checks that first.
/// Whether `code` contains `iastore` / `lastore` / `fastore` / `dastore`
/// — a store into an array shape the GPU input-residency cache can hold.
///
/// A `true` here no longer refuses JIT admission. It means the residency
/// cache must stand down before this method runs compiled; see the
/// "Inline array stores" section of the module docs and
/// [`crate::runtime::offload::input_cache::disable_for_jit_array_writer`].
///
/// `aastore` (0x53) and the sub-word stores `bastore`/`castore`/`sastore`
/// (0x54..=0x56) are deliberately absent: the cache holds only
/// `int[]`/`long[]`/`float[]`/`double[]`, so a store to anything else
/// cannot invalidate an entry that could exist.
fn method_writes_primitive_array(code: &[u8]) -> bool {
    scan_code(code).1
}

/// Resolve a `MethodReference` / `InterfaceMethodReference` constant-pool
/// entry into its declaring class name, method name, and descriptor.
/// `invokestatic` never targets any other constant-pool entry kind (an
/// interface's static method, added in Java 8, is the
/// `InterfaceMethodReference` case), so this covers every shape a real
/// class file can produce at an `invokestatic` operand. Returns `None`
/// for a malformed or out-of-range reference — the caller treats that
/// call site as "can't tell" and moves on, matching [`compute`]'s
/// fail-open stance.
fn resolve_method_ref(cp: &ConstantPool, index: u16) -> Option<(&str, &str, &str)> {
    let (class_index, name_and_type_index) = match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let class_name = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(name_and_type_index)?;
    Some((class_name, name, descriptor))
}

#[cfg(test)]
mod tests {
    use crate::runtime::offload::input_cache;
    use super::*;

    // ------------------------------------------------------------------
    // `scan_invokestatic_cp_indices` — the novel logic this module
    // introduces. `caller_blocks_jit`/`compute` are not exercised here:
    // both need a real `SharedVm` with a populated `ClassManager`, which
    // (per `crate::runtime::offload`'s own test-module comment) needs
    // "~30 fields of unrelated bookkeeping" to construct — exactly the
    // "existing test fixtures make that hard" case the task anticipated.
    // The pure bytecode scanner and constant-pool resolver below are the
    // parts that can go wrong independent of any VM state, so they get
    // the thorough hand-rolled-bytecode coverage.
    // ------------------------------------------------------------------

    #[test]
    fn scan_empty_code_finds_nothing() {
        assert!(scan_invokestatic_cp_indices(&[]).is_empty());
    }

    // ------------------------------------------------------------------
    // `method_writes_primitive_array` — the JIT half of Phase 10 #2.
    // ------------------------------------------------------------------

        /// The residency cache stands down for the JIT, and stays down.
    ///
    /// AUDIT 2026-09-02. This pins the inverted trade described in
    /// `offload_jit_gate`'s module docs: admitting a method that writes a
    /// primitive array disables the cache instead of refusing the method.
    ///
    /// # What this can and cannot reach without a device
    ///
    /// Every real cache entry owns an `Arc<DeviceBuffer<T>>`, and a
    /// `DeviceBuffer` cannot be constructed without a CUDA driver — a
    /// stub build's constructors all return `NoDriver`. So the parts a
    /// unit test can observe are the switch, the teardown of whatever
    /// the table held, and the address filter that guards `invalidate`'s
    /// hot path. The refusal of FUTURE entries is a single early return
    /// at the top of `insert`, which is the only function that inserts;
    /// it is checked here by asserting the predicate that return reads,
    /// and end-to-end by the hardware gate in `gpu-selfhosted.yml`.
    ///
    /// The switch is process-wide and one-way by design, which is also
    /// why this is one test rather than three: a later test could not
    /// observe the "before" state.
    #[test]
    fn disabling_the_input_cache_drops_what_it_holds_and_refuses_more() {
        assert!(
            input_cache::is_enabled(),
            "the cache must start enabled, or the rest of this proves nothing"
        );
        assert!(
            input_cache::table_len_for_test() == 0,
            "no test in this binary should have populated the cache"
        );

        input_cache::disable_for_jit_array_writer();

        assert!(
            !input_cache::is_enabled(),
            "the switch did not flip; `insert` would keep accepting entries \
             that nothing can invalidate once compiled code writes the array"
        );
        assert_eq!(
            input_cache::table_len_for_test(),
            0,
            "every entry must be gone: one cached a moment ago mirrors an \
             array the about-to-run compiled code may write"
        );
        assert_eq!(
            input_cache::addr_filter_for_test(),
            0,
            "the address filter must be rebuilt from the emptied table, or \
             `invalidate` keeps paying for a lock and a failed lookup on \
             every array store in the VM"
        );

        // Idempotent: the gate calls this once per admitted method, which
        // for a large program is thousands of times.
        input_cache::disable_for_jit_array_writer();
        assert!(!input_cache::is_enabled());
        assert_eq!(input_cache::table_len_for_test(), 0);
    }

    #[test]
    fn array_store_scan_finds_each_cached_element_type() {
        for (op, name) in [
            (0x4fu8, "iastore"),
            (0x50, "lastore"),
            (0x51, "fastore"),
            (0x52, "dastore"),
        ] {
            // aload_0; iconst_0; iconst_1; <astore>; return
            let code = [0x2a, 0x03, 0x04, op, 0xb1];
            assert!(
                method_writes_primitive_array(&code),
                "{name} must block JIT admission while offload is live"
            );
        }
    }

    #[test]
    fn array_store_scan_ignores_reference_and_subword_stores() {
        // aastore/bastore/castore/sastore never reach the input cache,
        // which only holds int/long/float/double buffers. bastore has a
        // helper hook anyway.
        for op in [0x53u8, 0x54, 0x55, 0x56] {
            let code = [0x2a, 0x03, 0x04, op, 0xb1];
            assert!(!method_writes_primitive_array(&code), "opcode {op:#x}");
        }
    }

    #[test]
    fn array_store_scan_is_not_fooled_by_an_operand_byte() {
        // `bipush 0x4f` — the 0x4f is an immediate, not an iastore. A
        // naive byte scan would block every method containing the
        // constant 79, so this is the property that keeps the gate from
        // disabling the JIT across the whole program.
        let code = [0x10, 0x4f, 0xb1];
        assert!(!method_writes_primitive_array(&code));
    }

    #[test]
    fn array_store_scan_sees_a_store_after_a_switch_table() {
        // A lookupswitch body full of arbitrary bytes, then a real
        // iastore: the walk must resync on the instruction boundary.
        let mut code = vec![0x2a, 0xab, 0x00, 0x00]; // aload_0; lookupswitch; pad to 4
        code.extend_from_slice(&[0, 0, 0, 8]); // default
        code.extend_from_slice(&[0, 0, 0, 0]); // npairs = 0
        code.push(0x4f); // iastore
        assert!(method_writes_primitive_array(&code));
    }

    #[test]
    fn scan_code_reports_both_facts_from_one_walk() {
        // aload_0; iconst_0; iconst_1; iastore; invokestatic #7; return
        let code = [0x2a, 0x03, 0x04, 0x4f, 0xb8, 0x00, 0x07, 0xb1];
        let (indices, writes) = scan_code(&code);
        assert_eq!(indices, vec![7]);
        assert!(writes);
    }

    #[test]
    fn scan_finds_single_invokestatic() {
        // invokestatic #7, then return.
        let code = [0xb8, 0x00, 0x07, 0xb1];
        assert_eq!(scan_invokestatic_cp_indices(&code), vec![7]);
    }

    #[test]
    fn scan_finds_multiple_invokestatic_in_order() {
        // invokestatic #1; pop; invokestatic #300; return.
        let code = [0xb8, 0x00, 0x01, 0x57, 0xb8, 0x01, 0x2c, 0xb1];
        assert_eq!(scan_invokestatic_cp_indices(&code), vec![1, 300]);
    }

    #[test]
    fn scan_ignores_stray_0xb8_inside_an_operand() {
        // bipush 0xB8 (i.e. -72 as a signed byte immediate) must NOT be
        // misread as an invokestatic at the operand byte's offset. Real
        // invokestatic (#42) follows.
        let code = [
            0x10, 0xb8, // bipush -72
            0xb8, 0x00, 0x2a, // invokestatic #42
            0xb1, // return
        ];
        assert_eq!(scan_invokestatic_cp_indices(&code), vec![42]);
    }

    #[test]
    fn scan_skips_tableswitch_body_correctly() {
        // A tableswitch whose padding/jump-offset bytes are chosen to
        // contain 0xB8 byte values, followed by a real invokestatic.
        // pc=0: tableswitch opcode.
        let mut code = vec![0xaa];
        // pad to next 4-byte boundary from pc=1: (4 - (1 % 4)) % 4 = 3.
        code.extend_from_slice(&[0xb8, 0xb8, 0xb8]); // padding bytes (garbage)
        code.extend_from_slice(&0i32.to_be_bytes()); // default offset
        code.extend_from_slice(&0i32.to_be_bytes()); // low = 0
        code.extend_from_slice(&1i32.to_be_bytes()); // high = 1
        code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // jump entry 0 (garbage, looks like invokestatic bytes)
        code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // jump entry 1 (garbage)
                                                              // Now a real invokestatic #99.
        code.extend_from_slice(&[0xb8, 0x00, 0x63]);
        assert_eq!(scan_invokestatic_cp_indices(&code), vec![99]);
    }

    #[test]
    fn scan_skips_lookupswitch_body_correctly() {
        // pc=0: lookupswitch opcode.
        let mut code = vec![0xab];
        code.extend_from_slice(&[0xb8, 0xb8, 0xb8]); // padding (garbage)
        code.extend_from_slice(&0i32.to_be_bytes()); // default offset
        code.extend_from_slice(&1i32.to_be_bytes()); // npairs = 1
        code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // match (garbage)
        code.extend_from_slice(&0xb8b8b8b8u32.to_be_bytes()); // offset (garbage)
        code.extend_from_slice(&[0xb8, 0x00, 0x05]); // real invokestatic #5
        assert_eq!(scan_invokestatic_cp_indices(&code), vec![5]);
    }

    #[test]
    fn scan_skips_wide_iinc_correctly() {
        // wide iinc idx=0x0001 const=0x00b8 (6 bytes total), then a real
        // invokestatic. The wide operand bytes deliberately contain 0xB8.
        let code = [
            0xc4, 0x84, 0x00, 0x01, 0x00, 0xb8, // wide iinc
            0xb8, 0x00, 0x0a, // invokestatic #10
        ];
        assert_eq!(scan_invokestatic_cp_indices(&code), vec![10]);
    }

    #[test]
    fn scan_truncated_invokestatic_operand_does_not_panic() {
        // invokestatic with only one operand byte present.
        let code = [0xb8, 0x00];
        assert!(scan_invokestatic_cp_indices(&code).is_empty());
    }

    #[test]
    fn scan_no_invokestatic_present() {
        let code = [0x2a, 0xb1]; // aload_0; return
        assert!(scan_invokestatic_cp_indices(&code).is_empty());
    }

    // ------------------------------------------------------------------
    // `resolve_method_ref`
    // ------------------------------------------------------------------

    /// Build a tiny constant pool:
    ///   1: Utf8 "com/example/Kernel"
    ///   2: ClassReference -> 1
    ///   3: Utf8 "vectorAdd"
    ///   4: Utf8 "([I[I[I)V"
    ///   5: NameAndType(3, 4)
    ///   6: MethodReference(2, 5)
    ///   7: InterfaceMethodReference(2, 5)
    ///   8: Integer(42) — a non-method-ref entry for the negative test.
    fn sample_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone, // 0 (unused)
            ConstantPoolEntry::Utf8(std::sync::Arc::from("com/example/Kernel")), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8(std::sync::Arc::from("vectorAdd")), // 3
            ConstantPoolEntry::Utf8(std::sync::Arc::from("([I[I[I)V")), // 4
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            ConstantPoolEntry::InterfaceMethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 7
            ConstantPoolEntry::Integer(42), // 8
        ])
    }

    #[test]
    fn resolve_method_reference() {
        let cp = sample_cp();
        let resolved = resolve_method_ref(&cp, 6).expect("MethodReference must resolve");
        assert_eq!(resolved, ("com/example/Kernel", "vectorAdd", "([I[I[I)V"));
    }

    #[test]
    fn resolve_interface_method_reference() {
        let cp = sample_cp();
        let resolved = resolve_method_ref(&cp, 7).expect("InterfaceMethodReference must resolve");
        assert_eq!(resolved, ("com/example/Kernel", "vectorAdd", "([I[I[I)V"));
    }

    #[test]
    fn resolve_non_method_ref_entry_returns_none() {
        let cp = sample_cp();
        assert!(resolve_method_ref(&cp, 8).is_none());
    }

    #[test]
    fn resolve_out_of_range_index_returns_none() {
        let cp = sample_cp();
        assert!(resolve_method_ref(&cp, 999).is_none());
    }
}
