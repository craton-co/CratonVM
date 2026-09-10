// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GPU-offload JIT admission gate.
//!
//! Follow-up item 2 in
//! `gpu-offload-followups-20260711.md` ("JIT-compiled
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
//! This gate resolves that by refusing to compile such a method, which
//! is sound and enormously broad — it fires on any method writing a
//! primitive array, kernel-adjacent or not, so attaching a GPU
//! de-optimises the CPU half of a mixed workload. This module's own
//! note concedes it is "the common case for the *producer* method
//! rather than the caller", i.e. it fires far more often than the
//! invokestatic reason it shares this file with.
//!
//! The other direction is equally sound and available:
//! [`ArrayWriterPolicy::AllowJit`] compiles the method and gives up the
//! residency cache instead, via
//! [`crate::runtime::offload::input_cache::disable_for_jit_array_writer`].
//!
//! **AUDIT 2026-09-02: it was tried as the default, and measured, and
//! the measurement sent it back.** On an RTX 2060 against a binary from
//! the same tree without the change, `GpuWarm f 2^22 5` went from
//! `warm_ms=2` to `warm_ms=10`, and `CRATONVM_GPU_TRACE_BYTES=1` showed
//! exactly why: 48 MB uploaded once and then zero, against 48 MB on
//! every single submit. The residency cache exists for a workload that
//! re-submits the same arrays, and on one it is worth 5x — while
//! nothing in that run made the CPU-side benefit visible, because there
//! was no CPU-side work left to speed up.
//!
//! So the trade stays where it was, and the inversion is a flag with the
//! numbers attached. See [`ArrayWriterPolicy`] for which shape each
//! answer is for.
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
//! - ~~**Forward references.**~~ **Closed 2026-09-06.** [`compute`] can
//!   only judge a call target if the target's declaring class is
//!   *already loaded* at the moment the caller is scanned, and it still
//!   cannot — that half is unchanged, and deliberately so: treating "not
//!   loaded" as "assume eligible" would ban huge swaths of an ordinary
//!   VM boot from the JIT.
//!
//!   What is closed is what the skip COST. This bullet used to end
//!   "offload can still be silently dropped for that specific caller",
//!   and that was an understatement: the drop was PERMANENT and it was
//!   not limited to that caller. An unjudged target is never registered
//!   with [`cratonvm_jit::offload_hook`], and
//!   `helpers.rs::try_compiled_offload` memoized `NotKernel` per call
//!   site for the life of the process — so the very first execution of
//!   the site decided, using a registry that could not yet know, and the
//!   class that same dispatch was about to load could not change the
//!   answer. A caller that got hot before its kernel branch was first
//!   taken lost the device for the rest of the run.
//!
//!   The registry now learns instead. `try_compiled_offload` asks
//!   [`target_is_dispatchable_kernel`] on a registry miss — the same
//!   question [`judge_target`] asks here — screened first by
//!   [`descriptor_could_ever_dispatch`] so the ~all-of-them sites that
//!   cannot be kernels never pay for it, and re-asked rather than
//!   memoized while the class is still unloaded. A kernel it finds is
//!   `note_kernel`ed, which also repairs every LATER compile: the
//!   compile-time doors in `jit/src/lib.rs` and
//!   `jit/src/x64/bytecode_walk.rs` read the same registry to decide
//!   whether to bind directly or inline, and a stale cached `false` in
//!   [`GATE_CACHE`] stops mattering once the target is in the registry.
//!
//!   Measured on `test_classes/gpu/GpuForwardRef.java`, whose two arms
//!   differ only in whether the caller gets hot before or after the
//!   kernel's class is loaded. See
//!   `internal/gpu/compiled-caller-gate-refused-ldc-kernels-FIXED-20260905.md`.
//!
//!   The one case still open is narrower than the old bullet and needs
//!   the site to be recompiled BETWEEN the first and second execution of
//!   a forward-referenced site — the window in which the class exists,
//!   the registry does not yet know, and a fresh compile can bind
//!   directly.
//! - ~~**Hint-loosened kernels.**~~ **Closed 2026-09-05.** [`compute`]
//!   now calls [`jit_cuda::analyzer::analyze_with_annotations_and_pool`]
//!   with the target's own annotations and constant pool — the same call
//!   the dispatcher makes, which is the only way the two can agree.
//!
//!   This bullet used to say the annotation-free verdict was
//!   "intentionally conservative in the direction that costs offload
//!   throughput, not correctness — the worst case is a JIT-compiled
//!   caller that stops offloading". That worst case was then measured at
//!   **10.8x** on the constant-pool half of the same disagreement (see
//!   `internal/gpu/compiled-caller-gate-refused-ldc-kernels-FIXED-20260905.md`),
//!   which is what retired the argument: "costs throughput, not
//!   correctness" is not a reason to keep a gate asking a different
//!   question from the dispatcher it models.
//!
//!   It also ran the other way. A `@GpuExclude` target could be judged
//!   `Eligible` here and REGISTERED with the offload hook, arming it for
//!   a method `lookup_or_compile` short-circuits and never launches —
//!   and under [`CallerGateMode::Block`] denying its caller compilation
//!   for an offload that could not happen.
//! - **`invokedynamic`-mediated calls** (method references, lambdas)
//!   are not scanned — only literal `invokestatic` bytecodes. A caller
//!   that reaches an eligible kernel through a `MethodHandle` is not
//!   covered.
//!
//!   AUDIT 2026-09-06: this one is NOT a disagreement, and it is worth
//!   saying so next to two that were. Both dispatcher doors are
//!   `invokestatic`-only — `interpreter/dispatch_static.rs` is the
//!   interpreter's `execute_invokestatic` arm, and
//!   `jit/helpers.rs::jit_invoke_dispatch` guards its offload attempt on
//!   `info.invoke_kind == 3`. Neither fires for an `invokedynamic`, so a
//!   `MethodHandle`-mediated kernel does not offload from the
//!   interpreter either, and the gate agreeing that it is out of scope
//!   is the two asking the SAME question. It is a feature the offload
//!   path does not have, not a gate that models the wrong dispatcher.
//!
//! # Integration status
//!
//! Wired. [`caller_blocks_jit_by_name`] is consulted at every JIT/OSR
//! admission site — one in `interpreter.rs` and four in
//! `interpreter/jit_bridge.rs` (first-call eager compile, the
//! invocation-count promotion, the callee-compile closure inside it, the
//! callee-dispatcher compile path, and `compile_osr_artifact`, the OSR
//! path this module exists for). Find them with
//! `grep -rn caller_blocks_jit_by_name vm/src`. This paragraph said "not
//! yet wired" for several weeks after it was.

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
    cratonvm_types::gpu_jit_gate_census::note_verdict(verdict);
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
/// `invokedynamic`-mediated calls). Annotation-loosened kernels were on
/// that list until 2026-09-05 and are now handled.
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

    // ARM THE HOOK, without registering anything.
    //
    // AUDIT 2026-09-06. `jit_invoke_dispatch` consults the offload hook only
    // `if any_kernels()`, and that used to become true only when a caller scan
    // below had already found a kernel. A program whose only kernel is
    // FORWARD-REFERENCED registers nothing here, so the flag stayed false, so
    // the compiled dispatch helper never looked, so nothing ever registered it
    // — the empty case sealed itself shut. Arming on "there is a device"
    // rather than "we found something" lets the helper's per-site resolution
    // ask [`target_is_dispatchable_kernel`] about the targets this scan could
    // not judge. A run without `--gpu` never gets here and still pays one
    // relaxed bool per compiled static dispatch.
    cratonvm_jit::offload_hook::arm(shared.config.gpu_min_work);

    // AUDIT 2026-09-02: with `--nojit` there is nothing to admit, so
    // there is nothing to decide — and nothing to trade away either.
    // This is not merely an optimisation: `compute` is still consulted
    // on that path, and under `ArrayWriterPolicy::AllowJit` it was
    // giving up the residency cache to buy compilation that could never
    // happen. Measured on GpuWarm at 2^22, `--gpu --nojit` re-uploaded
    // 48 MB on every submit instead of zero.
    if crate::runtime::env_cache::disable_jit() {
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

    // Phase 10 #2, JIT half. Two sound answers; which one is a POLICY,
    // and the default is the measured one — see
    // [`ArrayWriterPolicy`].
    //
    // A method writing an int/long/float/double array cannot run
    // compiled while the input-residency cache is live, because the IR
    // pipeline's inline `MOVSS`/`MOVSD` store has no hook to invalidate
    // it from. Either the method stays interpreted, or the cache stands
    // down. Both are correct; they cost different things.
    if method_writes_primitive_array(&code_attr.code) {
        match array_writer_policy() {
            ArrayWriterPolicy::Barrier => {
                // Nothing to trade any more: the compiled tiers mark what
                // they wrote and `input_cache::drain_locked`
                // evicts it before the next read. The method compiles AND
                // the cache stays coherent. See `cratonvm_jit::gpu_barrier`.
                cratonvm_types::gpu_jit_gate_census::note_released_array_writer();
                // Fall through to the invokestatic scan, which is a
                // separate reason and still applies.
            }
            ArrayWriterPolicy::KeepCache => {
                cratonvm_types::gpu_jit_gate_census::note_blocked_name(
                    format!("{}.{}{}", class.name, method.name, method.descriptor),
                    cratonvm_types::gpu_jit_gate_census::BlockReason::WritesPrimitiveArray,
                );
                return true;
            }
            ArrayWriterPolicy::AllowJit => {
                crate::runtime::offload::input_cache::disable_for_jit_array_writer();
                // Fall through to the invokestatic scan: this method may
                // ALSO call an offload-eligible kernel, which is the
                // other, narrower reason to refuse it, and that one
                // still holds.
            }
        }
    }

    // `CallerGateMode::Off` skips the scan entirely. See the enum.
    if caller_gate_mode() == CallerGateMode::Off {
        return false;
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

        // The whole judgement -- resolve, analyze, and then the
        // dispatcher's own gates -- now lives in [`judge_target`], because
        // the compiled dispatch helper has to ask the IDENTICAL question
        // at run time about a target this scan could not judge. See that
        // function for the audit trail this comment used to carry.
        match judge_target(
            &cm,
            shared.config.gpu_min_work,
            target_class_name,
            target_method_name,
            target_descriptor,
        ) {
            // Known limitation, NARROWED 2026-09-06: if the target's
            // declaring class is not yet loaded we cannot judge
            // eligibility here. We do NOT treat "not loaded" as "assume
            // eligible" -- that would make an ordinary VM boot (where most
            // classes referenced by a freshly-loaded caller are not loaded
            // yet) ban huge swaths of unrelated code from the JIT.
            //
            // What changed is what the skip COSTS. It used to be terminal:
            // the target was never registered, so the compiled call site
            // was bound directly or inlined and offload ended for the life
            // of the process. `try_compiled_offload` now asks
            // [`target_is_dispatchable_kernel`] once per site, at a moment
            // when the class does exist, and registers what it finds -- so
            // this `continue` costs the FIRST call through the site and
            // nothing after it.
            TargetVerdict::Unresolved => {
                cratonvm_types::gpu_jit_gate_census::note_target_unresolved();
                continue;
            }
            // Counted, and named. These refusals used to be a bare
            // `continue`: the counters exist because a narrowing that shows
            // up only as an ABSENCE cannot be told from one that never
            // fired, and this one had no counter at all. That is how the
            // pool-free `analyze` went unnoticed -- a run whose kernels had
            // all silently stopped registering printed a census identical
            // to a healthy one.
            //
            // Split, because a census that folds "never a candidate" into
            // "refused" is one nobody reads: one run of `GpuIntensitySweep`
            // walks past ~154 signature-refused JDK targets, and naming
            // those would bury the handful that matter.
            TargetVerdict::NeverCandidate => {
                cratonvm_types::gpu_jit_gate_census::note_target_never_candidate();
                continue;
            }
            TargetVerdict::BodyRefused(reason) => {
                cratonvm_types::gpu_jit_gate_census::note_target_body_refused(format!(
                    "{target_class_name}.{target_method_name}{target_descriptor} — {reason:?}"
                ));
                continue;
            }
            TargetVerdict::Undispatchable => {
                cratonvm_types::gpu_jit_gate_census::note_released_undispatchable(format!(
                    "{target_class_name}.{target_method_name}{target_descriptor}"
                ));
                continue;
            }
            TargetVerdict::Kernel => {}
        }

        // A kernel the dispatcher really can launch. What that costs
        // the caller is now a POLICY -- see [`CallerGateMode`].
        match caller_gate_mode() {
            CallerGateMode::Block => {
                cratonvm_types::gpu_jit_gate_census::note_blocked_name(
                    format!(
                        "{}.{}{} -> calls {}.{}{}",
                        class.name,
                        method.name,
                        method.descriptor,
                        target_class_name,
                        target_method_name,
                        target_descriptor
                    ),
                    cratonvm_types::gpu_jit_gate_census::BlockReason::CallsEligibleKernel,
                );
                return true;
            }
            CallerGateMode::CompiledHook => {
                // Register the TARGET, not the caller. The compiler then
                // keeps every site aimed at it on the dispatch helper, and
                // the helper consults the offload hook -- so the caller
                // compiles AND the site still offloads.
                //
                // No `return`: a method can call several kernels, and each
                // one has to be registered or the sites aimed at the
                // unregistered ones get bound directly and go dark.
                cratonvm_jit::offload_hook::note_kernel(
                    target_class_name,
                    target_method_name,
                    target_descriptor,
                );
            }
            CallerGateMode::Off => {}
        }
    }
    false
}

/// Could [`crate::runtime::offload::try_dispatch`] EVER offload a call to
/// this target — not "is its bytecode lowerable", which is what
/// [`jit_cuda::analyzer::analyze`] answers.
///
/// # Why the analyzer's verdict is the wrong question on its own
///
/// AUDIT 2026-09-04. The gate blocked any caller of an `Eligible`
/// `invokestatic`, and `Eligible` is a statement about the callee's
/// BYTECODE: no exception handlers, supported parameter types, no
/// forbidden opcodes. `java/lang/Math.min(II)I` passes all of that. So
/// does `Math.max(II)I`, `FloatOps.sq(F)F`,
/// `Currency$SpecialCaseEntry.toIndex`, and
/// `DirectMethodHandleDesc$Kind.tableIndex`. None of them can be
/// offloaded, and none of them ever could be — but every caller of any
/// of them was denied JIT admission for the life of the process.
///
/// Measured on kfusion under `--gpu`: 10 methods blocked for this
/// reason, and the targets included both `Math` intrinsics — which means
/// `TornadoMath.clamp` and, transitively, every method calling it lost
/// compilation to protect an offload that could not happen.
///
/// # The two gates, mirrored exactly
///
/// `try_dispatch` applies these to every `LookupOutcome::Hit` before it
/// will launch anything:
///
/// 1. **Return shape.** Only a `)V` map, or a proven reduction returning
///    `)I`/`)J`, is transparently dispatched; anything else takes
///    `DispatchOutcome::FallThrough`. That is the PERMANENT arm — not
///    `FallThroughKeepHooked` — so a `)I` non-reduction target is not
///    merely declined today, it is declined on every call forever, and
///    the hook the gate is protecting has nothing to protect.
/// 2. **`--gpu-min-work`.** The per-call work estimate is
///    `largest_primitive_array_len(args)`, and a descriptor with no
///    array parameter makes that `0` on every call, for every argument
///    value. With `gpu_min_work > 0` (default 4096) such a target can
///    never clear the threshold. `ParamKind::from_field` admits only
///    primitives and primitive arrays, so "the descriptor's parameter
///    list contains `[`" is exactly "an argument can be an array".
///
/// Both are properties of the METHOD, decidable here, and both are read
/// off the same values `try_dispatch` reads. This is deliberately not a
/// heuristic: a target that fails either test cannot reach a launch, so
/// refusing to compile its callers buys nothing and costs the caller.
///
/// What this does NOT relax: a `)V` kernel taking arrays still blocks its
/// callers, which is the case the gate was built for and the one the
/// module comment argues.
/// What the gate concluded about ONE `invokestatic` target.
///
/// Split out 2026-09-06 so `compute` and the compiled dispatch helper ask the
/// SAME question -- which is the whole lesson of
/// `internal/gpu/compiled-caller-gate-refused-ldc-kernels-FIXED-20260905.md`,
/// where a gate calling a different analyzer entry point than the dispatcher
/// it modelled cost 10.8x on a kernel it silently refused to register.
#[derive(Debug)]
enum TargetVerdict {
    /// The target's declaring class is not loaded, so nothing can be said.
    /// NOT "ineligible" -- the caller has to decide what an unknown costs.
    Unresolved,
    /// Signature-shaped refusal: never a candidate at all.
    NeverCandidate,
    /// Kernel-shaped, and the analyzer refused the BODY. The actionable half.
    BodyRefused(jit_cuda::analyzer::Reason),
    /// The analyzer says `Eligible`, but the dispatcher could never launch it.
    Undispatchable,
    /// A kernel the dispatcher really can launch.
    Kernel,
}

/// Judge one call target exactly the way `OffloadCache::lookup_or_compile`
/// would.
///
/// AUDIT 2026-09-05. This used the CP-free `analyze`, which rejects
/// `ldc`/`ldc_w`/`ldc2_w` unconditionally because it has no pool to resolve
/// them against, while `lookup_or_compile` calls the CP-aware variant that
/// admits a numeric literal. So the two disagreed, silently, about any kernel
/// containing a constant-pool constant.
///
/// `bench-gpu/GpuFloatDivChain.divChain` is one such kernel: its
/// `x = x / d + 1.0000001` is an `ldc2_w`, so the gate saw INELIGIBLE while the
/// interpreter saw `Eligible` and offloaded. Its int twin's `+ 12345` is a
/// `sipush` with no pool entry, so that one agreed and worked. Measured at
/// N=2^24: 9,276 ms against the int twin's 8 ms, and 28 ms with
/// `CRATONVM_JIT_OSR=0` (which keeps the caller interpreted, where the CP-aware
/// verdict is the one that runs).
///
/// NOT an FP-only disagreement, though both symptoms that found it were. The
/// boundary is the CONSTANT POOL, not the type: an `int[]` kernel whose
/// constant is above `sipush` range needs an `ldc` and went dark too, while a
/// `long[]` kernel using only `lconst_1` never touched the pool and offloaded
/// normally. Both measured on `test_classes/gpu/GpuLdcSplit.java`, which exists
/// to break that correlation.
///
/// ANNOTATIONS TOO, for the same reason as the pool: the dispatcher reads them
/// (`read_method_annotations`, then `analyze_with_annotations_and_pool`), so a
/// gate that does not is asking a different question and will disagree. That
/// disagreement ran BOTH ways -- a kernel eligible only via
/// `@GpuKernel`/`AdmissionHint` was invisible here and its compiled sites went
/// dark, and a `@GpuExclude` method could be judged `Eligible` and REGISTERED,
/// arming the hook for a target the dispatcher short-circuits and never
/// launches.
///
/// Decoding costs one attribute pass per judged target, next to a full bytecode
/// scan that was already being paid.
fn judge_target(
    cm: &crate::classloading::ClassManager,
    gpu_min_work: u32,
    target_class_name: &str,
    target_method_name: &str,
    target_descriptor: &str,
) -> TargetVerdict {
    let Some(target_class_id) = cm.get_loaded_class_id(target_class_name) else {
        return TargetVerdict::Unresolved;
    };
    let Some(target_class) = cm.get_class(target_class_id) else {
        return TargetVerdict::Unresolved;
    };
    let Some(target_method) = target_class
        .methods
        .iter()
        .find(|m| &*m.name == target_method_name && &*m.descriptor == target_descriptor)
    else {
        // The class is loaded and has no such method. That is a resolution
        // failure the dispatcher would hit too, not a "come back later".
        return TargetVerdict::NeverCandidate;
    };

    let target_attrs = crate::runtime::offload::decode_method_attrs(
        &target_method.attributes,
        &target_class.constant_pool,
    );
    let target_annotations =
        jit_cuda::annotations::read_method_annotations(&target_attrs, &target_class.constant_pool);
    let sig = match jit_cuda::analyzer::analyze_with_annotations_and_pool(
        target_method,
        &target_annotations,
        &target_class.constant_pool,
    ) {
        jit_cuda::OffloadVerdict::Eligible(sig) => sig,
        jit_cuda::OffloadVerdict::Rejected(reason) => {
            use jit_cuda::analyzer::Reason;
            let never_a_candidate = matches!(
                reason,
                Reason::NonStatic
                    | Reason::Synchronized
                    | Reason::NativeOrAbstract
                    | Reason::NoCode
                    | Reason::BadDescriptor
                    | Reason::UnsupportedParamType
                    | Reason::UnsupportedReturnType
                    | Reason::GpuExcluded
            );
            return if never_a_candidate {
                TargetVerdict::NeverCandidate
            } else {
                TargetVerdict::BodyRefused(reason)
            };
        }
    };

    // ...and then the DISPATCHER's own gates. `Eligible` answers "could this
    // bytecode be lowered to PTX", which is not the question this gate is
    // asking. See [`target_can_ever_dispatch`].
    if !target_can_ever_dispatch(&sig, target_descriptor, gpu_min_work) {
        return TargetVerdict::Undispatchable;
    }
    TargetVerdict::Kernel
}

/// Could a target with THIS descriptor ever be a dispatchable kernel?
///
/// A necessary condition, read off the descriptor alone -- no class loading, no
/// bytecode scan, no allocation. It is the descriptor-only half of
/// [`target_can_ever_dispatch`]: `try_dispatch` launches only `)V`, or `)I`/`)J`
/// for a reduction, and with `--gpu-min-work` above zero the work estimate is
/// structurally zero unless something in the parameter list can be an array.
///
/// Exists so `try_compiled_offload` can reject the overwhelming majority of
/// compiled static call sites -- every `String` returner, every no-array
/// helper -- before paying for [`target_is_dispatchable_kernel`]'s analyzer
/// pass. Screening in the WRONG direction here would be a lost offload, so it
/// tests only conditions the dispatcher itself insists on.
pub fn descriptor_could_ever_dispatch(descriptor: &str, gpu_min_work: u32) -> bool {
    let returns_kernel_shape =
        descriptor.ends_with(")V") || descriptor.ends_with(")I") || descriptor.ends_with(")J");
    if !returns_kernel_shape {
        return false;
    }
    if gpu_min_work > 0 && !descriptor_has_array_parameter(descriptor) {
        return false;
    }
    true
}

/// Is this ONE target a kernel the dispatcher can launch? `None` when its
/// declaring class is not loaded, so the question cannot be answered yet.
///
/// The compiled dispatch helper's per-site resolution calls this on a registry
/// miss. `offload_hook`'s registry is populated as a side effect of scanning
/// CALLERS, and a caller scanned before its callee's class existed registers
/// nothing -- the "forward references" limitation in this module's docs. That
/// limitation used to be terminal, because `helpers.rs::try_compiled_offload`
/// memoized `NotKernel` per site for the life of the process: the first
/// execution of the site decided, and the class that same dispatch was about to
/// load could not change the answer.
///
/// Asking here closes it. By the time a site executes a second time the class
/// the first dispatch resolved is loaded, so this returns a real answer, and
/// the caller registers what it finds -- which also fixes every LATER compile,
/// since the compile-time doors in `jit/src/lib.rs` and
/// `jit/src/x64/bytecode_walk.rs` read the same registry to decide whether to
/// bind directly or inline.
pub fn target_is_dispatchable_kernel(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<bool> {
    let cm = shared.classes.class_manager.read();
    match judge_target(
        &cm,
        shared.config.gpu_min_work,
        class_name,
        method_name,
        descriptor,
    ) {
        TargetVerdict::Unresolved => None,
        TargetVerdict::Kernel => Some(true),
        _ => Some(false),
    }
}

fn target_can_ever_dispatch(
    sig: &jit_cuda::KernelSignature,
    descriptor: &str,
    gpu_min_work: u32,
) -> bool {
    // `CRATONVM_GPU_JIT_GATE_DISPATCHABLE=0` says "any Eligible target
    // blocks its callers", which is what this gate did until
    // 2026-09-04. Kept as a switch and not just as history: it is the
    // control arm for measuring what the narrowing is worth, and the two
    // narrowings in this file have to be separable or a measurement
    // cannot attribute a difference to either.
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_GPU_JIT_GATE_DISPATCHABLE")
            .ok()
            .as_deref()
            != Some("0")
    });
    if !on {
        return true;
    }
    // Gate 1 — `try_dispatch`'s `is_void || is_int_reduction ||
    // is_long_reduction`, verbatim.
    let is_void = descriptor.ends_with(")V");
    let is_int_reduction = sig.is_reduction && descriptor.ends_with(")I");
    let is_long_reduction = sig.is_reduction && descriptor.ends_with(")J");
    if !is_void && !is_int_reduction && !is_long_reduction {
        return false;
    }
    // Gate 2 — `--gpu-min-work` against a work estimate that is
    // structurally zero when nothing in the parameter list can be an
    // array. `gpu_min_work == 0` disables the threshold, so this half
    // must not fire then.
    if gpu_min_work > 0 && !descriptor_has_array_parameter(descriptor) {
        return false;
    }
    true
}

/// Does `descriptor`'s PARAMETER list contain an array type?
///
/// Only the parameters: `([I)I` has one, `(II)[I` has none — the return
/// type is not an argument and contributes nothing to
/// `largest_primitive_array_len`. Written as a scan of the substring
/// between the parentheses rather than a full descriptor parse because
/// that is the entire question, and a malformed descriptor (no `)`) is
/// answered `false`, matching [`compute`]'s fail-open-on-can't-tell
/// stance everywhere else in this file.
fn descriptor_has_array_parameter(descriptor: &str) -> bool {
    let Some(open) = descriptor.find('(') else {
        return false;
    };
    let Some(close) = descriptor.find(')') else {
        return false;
    };
    if close < open {
        return false;
    }
    descriptor[open + 1..close].contains('[')
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
///   `fastore` (0x51), `dastore` (0x52), `bastore` (0x54) and
///   `sastore` (0x56).
///
/// The last two were added 2026-09-02, with the offload path that made
/// `short[]`/`byte[]` cacheable in the first place. While those widths
/// could not be marshalled, nothing cached them and a compiled writer
/// could not stale anything; the four-opcode set was exactly right. The
/// moment they became offloadable it was under-inclusive, and the
/// resulting divergence is reproducible: `GpuJitWriterStale` at
/// n=8192/2000 rounds diverges from HotSpot on the short and byte
/// checksums while the int one — whose writer this scan does refuse —
/// stays correct.
///
/// `castore` (0x55) joined the set on 2026-09-03, when `char[]` became
/// marshallable and therefore cacheable. It was correctly excluded
/// before that -- no `char[]` was ever cached, so refusing its writers
/// would have cost compilation to protect nothing -- and its inclusion
/// now is the same obligation that `bastore`/`sastore` acquired when
/// short[]/byte[] became offloadable. `bastore` still covers
/// `boolean[]` as well as `byte[]`, which IS an over-approximation, but
/// the opcode cannot distinguish them and over-refusing is the safe
/// direction.
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
        // iastore / lastore / fastore / dastore, plus bastore (0x54),
        // castore (0x55) and sastore (0x56) since `short[]`/`byte[]`
        // and then `char[]` became cacheable.
        // Reached only on a real instruction boundary, so an operand
        // byte that happens to equal one of these cannot false-positive.
        if (0x4f..=0x52).contains(&op) || op == 0x54 || op == 0x56 || op == 0x55 {
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
/// Which side of the array-writer trade this process takes.
///
/// # The measurement
///
/// Both answers are sound. The default is the one that was measured, on
/// an RTX 2060 against a binary built from the same tree without the
/// change, `GpuWarm f 2^22 5`, `CRATONVM_GPU_TRACE_BYTES=1`:
///
/// | | warm_ms | H2D per submit |
/// |---|---:|---:|
/// | `KeepCache` (default) | 2 | 48 MB once, then 0 |
/// | `AllowJit` | 10 | 48 MB, every submit |
///
/// The residency cache exists precisely for a workload that re-submits
/// the same arrays, and on one it is worth 5x. Nothing in that run made
/// the CPU-side benefit of compiling the array writer visible, because
/// there was no CPU-side work left to speed up.
///
/// That does not make `AllowJit` wrong — it makes it wrong AS A DEFAULT.
/// The case it is for is the opposite shape: a mixed workload whose CPU
/// half fills, transforms and post-processes arrays around a kernel that
/// runs once. There, blocking the JIT de-optimises code that has nothing
/// to do with the device, to keep coherent a cache with nothing in it.
/// This is the knob for measuring which shape you have.
///
/// `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` selects it; anything else, or
/// unset, keeps the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayWriterPolicy {
    /// Compile the method AND keep the cache: the compiled store marks
    /// its bucket and `input_cache::drain_locked` evicts before
    /// the next read. The default whenever
    /// [`cratonvm_jit::gpu_barrier::is_armed`], i.e. on an x86_64
    /// `--gpu` run without the kill switch.
    ///
    /// This is not a third point on the trade the other two variants
    /// split -- it dissolves the trade. Both of those exist because a
    /// compiled array store could not evict; now it can.
    Barrier,
    /// Keep the input-residency cache; leave a method that writes a
    /// primitive array interpreted. The pre-2026-09-02 behaviour, and
    /// the default on any target the barrier cannot be armed for
    /// (non-x86_64) or where it was switched off.
    KeepCache,
    /// Compile the method; give up the residency cache for the rest of
    /// the process. See
    /// [`crate::runtime::offload::input_cache::disable_for_jit_array_writer`].
    AllowJit,
}

/// What a caller of a real, dispatchable GPU kernel costs.
///
/// # The measurement that moved the default
///
/// `GpuHookOverheadBench` under `--gpu`, one binary, this switch as the
/// only difference. `base_ns_per_call` is a loop over an **ineligible**
/// target, so the offload hook is not in it at all:
///
/// | | `base_ns_per_call` |
/// |---|---:|
/// | [`CallerGateMode::Block`] (the default) | 407.9 |
/// | [`CallerGateMode::Off`] | 10.2 |
/// | no `--gpu` | 9.0 |
///
/// 40x, and it is charged to every line of the enclosing method rather
/// than to the kernel call it was refused for. The refusal is a whole
/// method's compilation spent protecting one call site.
///
/// [`CallerGateMode::CompiledHook`] is what that measurement argues for,
/// and it is the default since 2026-09-05 -- 9,005 kernel launches from
/// compiled callers on the same bench, all three scenarios 27-42x
/// better than `Block`:
///
/// | | `base` | `small` | `big` |
/// |---|---:|---:|---:|
/// | `Block` | 358.5 | 13534.8 | 127257.4 |
/// | `CompiledHook` | 13.2 | 358.5 | 79983.0 |
/// | no `--gpu` | 15.4 | 28.5 | 2030.7 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallerGateMode {
    /// Compile the caller, and let the COMPILED dispatch helper consult
    /// the offload hook: the caller runs compiled and the site still
    /// offloads, so there is no trade left to make.
    ///
    /// Compile the caller, and let the COMPILED dispatch helper consult
    /// the offload hook: the caller runs compiled and the site still
    /// offloads, so there is no trade left to make. **The default since
    /// 2026-09-05.**
    ///
    /// It was opt-in for one day, on one scenario:
    /// `bench-gpu/runtime-stress.sh`'s `cache_coherence` failed under
    /// it. That was never this feature. It was a JIT miscompilation --
    /// `wide iinc` was invisible to `find_modified_locals`, so LICM
    /// hoisted a loop's induction variable -- and this mode was simply
    /// the first thing that ever compiled the method, because
    /// [`CallerGateMode::Block`] had kept every scenario in that file
    /// interpreted. Fixed 2026-09-05; see the retired
    /// `osr-miscompiles-cachecoherence-20260904` write-up, named rather
    /// than linked because it retired to the internal tree.
    ///
    /// With it fixed, all seven `runtime-stress.sh` scenarios pass under
    /// this mode, as do `jit-writer-stale.sh` and the rest.
    CompiledHook,
    /// Refuse the caller JIT admission. The pre-2026-09-05 behaviour,
    /// kept as the control arm and as the way back.
    /// `CRATONVM_GPU_JIT_GATE_CALLERS=block`.
    Block,
    /// Compile the caller and consult nothing: offload silently ends at
    /// every compiled site. NOT a production setting --
    /// `CRATONVM_GPU_JIT_GATE_CALLERS=0` -- it exists because the cost of
    /// the refusal cannot be attributed by comparing `--gpu` against no
    /// `--gpu`, which differ in everything else the device touches.
    Off,
}

fn caller_gate_mode() -> CallerGateMode {
    static M: std::sync::OnceLock<CallerGateMode> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_GPU_JIT_GATE_CALLERS")
            .ok()
            .as_deref()
        {
            Some("0") => CallerGateMode::Off,
            Some("block") => CallerGateMode::Block,
            _ => CallerGateMode::CompiledHook,
        }
    })
}

/// See [`ArrayWriterPolicy`].
///
/// `CRATONVM_GPU_JIT_ARRAY_WRITERS` selects explicitly: `allow` gives up
/// the cache, `refuse` restores the old refuse-to-compile behaviour.
/// Unset, the answer follows the barrier -- [`ArrayWriterPolicy::Barrier`]
/// when it is armed, and the historical `KeepCache` when it is not, which
/// is what keeps a non-x86_64 target and a killed-switch run correct
/// rather than merely unchanged.
///
/// The env read is memoized; the armed check is NOT. Arming happens once
/// during VM construction, but memoizing the pair would let a caller that
/// asked before that (a unit test, a second VM in one process) freeze the
/// wrong answer for the process.
fn array_writer_policy() -> ArrayWriterPolicy {
    use std::sync::OnceLock;
    static OVERRIDE: OnceLock<Option<ArrayWriterPolicy>> = OnceLock::new();
    let explicit = *OVERRIDE.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_GPU_JIT_ARRAY_WRITERS")
            .ok()
            .as_deref()
        {
            Some("allow") => Some(ArrayWriterPolicy::AllowJit),
            Some("refuse") => Some(ArrayWriterPolicy::KeepCache),
            _ => None,
        }
    });
    if let Some(p) = explicit {
        return p;
    }
    if cratonvm_jit::gpu_barrier::is_armed() {
        ArrayWriterPolicy::Barrier
    } else {
        ArrayWriterPolicy::KeepCache
    }
}

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
    // `descriptor_could_ever_dispatch` — the screen the compiled dispatch
    // helper puts in front of the gate query, so that asking about a
    // forward-referenced target does not cost every compiled static call
    // site in the program an analyzer pass.
    //
    // It is a NECESSARY condition, so the direction that matters is the
    // false NEGATIVE: a descriptor it rejects never gets asked about, and
    // a kernel rejected here is a kernel that stays dark. Each case below
    // is one the dispatcher itself insists on, checked against
    // `target_can_ever_dispatch`'s two gates.
    // ------------------------------------------------------------------

    #[test]
    fn the_descriptor_screen_admits_the_three_shapes_try_dispatch_launches() {
        // void, the ordinary kernel shape.
        assert!(descriptor_could_ever_dispatch("([I[I)V", 1));
        // `)I` and `)J`, the reduction shapes.
        assert!(descriptor_could_ever_dispatch("([I[I)I", 1));
        assert!(descriptor_could_ever_dispatch("([J)J", 1));
    }

    #[test]
    fn the_descriptor_screen_rejects_a_return_type_try_dispatch_cannot_hand_back() {
        // `try_dispatch` has an arm for void, `)I` and `)J` and nothing
        // else, so a `float`/`double`/reference return can never dispatch
        // however eligible the body is.
        assert!(!descriptor_could_ever_dispatch("([F[F)F", 1));
        assert!(!descriptor_could_ever_dispatch("([D)D", 1));
        assert!(!descriptor_could_ever_dispatch(
            "([I)Ljava/lang/String;",
            1
        ));
    }

    #[test]
    fn the_descriptor_screen_needs_an_array_only_while_min_work_is_armed() {
        // With `--gpu-min-work` above zero the work estimate is
        // structurally zero unless a parameter can be an array, so a
        // scalar-only signature can never clear the threshold...
        assert!(!descriptor_could_ever_dispatch("(II)V", 1));
        // ...but `--gpu-min-work 0` disables the threshold, and then the
        // same signature is admissible. Screening it out at zero would
        // hide a kernel the dispatcher would have launched.
        assert!(descriptor_could_ever_dispatch("(II)V", 0));
        // A reference array still counts as an array here: this screen is
        // a necessary condition, and the element-type refusal belongs to
        // the analyzer, which the caller asks next.
        assert!(descriptor_could_ever_dispatch(
            "([Ljava/lang/Object;)V",
            1
        ));
    }

    #[test]
    fn the_descriptor_screen_survives_a_descriptor_it_cannot_parse() {
        // `descriptor_has_array_parameter` returns false rather than
        // panicking on a malformed descriptor, and the screen must not
        // turn that into an admission.
        assert!(!descriptor_could_ever_dispatch("", 1));
        assert!(!descriptor_could_ever_dispatch("not a descriptor", 1));
        // No parameter list at all, but the right return shape: still no
        // array, so still refused while min-work is armed.
        assert!(!descriptor_could_ever_dispatch("()V", 1));
    }

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

    // ------------------------------------------------------------------
    // The compiled-tier barrier, decoded rather than re-asserted.
    //
    // `gpu_barrier`'s own tests check the encoder against the same
    // arithmetic that produced it, which cannot catch a wrong ModRM or a
    // REX bit naming the wrong register -- the sequence would still be
    // 51 bytes with the jumps landing correctly and would still pass.
    // `iced-x86` is an independent decoder, and it lives in this crate,
    // so the check lives here.
    // ------------------------------------------------------------------

    /// Decode the emitted barrier and assert it is the eleven
    /// instructions the design says it is, on the registers it says.
    ///
    /// A wrong register here is not a crash, it is silent corruption in
    /// one direction (clobbering a live value) or a silently dead
    /// barrier in the other (testing a bit of the wrong word, so a
    /// compiled write never marks its bucket and the device keeps
    /// serving stale data).
    #[test]
    fn the_emitted_barrier_decodes_to_the_documented_sequence() {
        use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};

        const BASE: u64 = 0x0000_7FF0_0010_0000;
        const FILTER: usize = 0x0000_7FF0_0020_0000;
        const DIRTY: usize = 0x0000_7FF0_0020_0040;

        // Encode from an explicit pair. This used to arm the process
        // globals and restore them, with a comment saying save-and-restore
        // kept the rest of the binary safe. It does not: cargo runs these
        // tests as parallel THREADS, so this and `the_barrier_never_writes_rax`
        // interleave -- and `armed_before` was itself the race, asserting
        // "no test in this binary should have armed the barrier" while the
        // sibling test had. Meanwhile `array_writer_policy` below reads
        // `is_armed()` in PRODUCTION, so an arming window here silently
        // moves the policy any concurrent test observes.
        //
        // `barrier_bytes_for` is the same encoder without the globals; see
        // `gpu_barrier::arm`'s doc, and `jit/tests/gpu_barrier_arming.rs`
        // for what covers the publishing itself.
        let bytes =
            cratonvm_jit::gpu_barrier::barrier_bytes_for(FILTER, DIRTY).expect("armed pair");

        let mut decoder = Decoder::with_ip(64, &bytes, BASE, DecoderOptions::NONE);
        let decoded: Vec<_> = decoder.iter().collect();

        // Eleven instructions, and the decoder must consume the sequence
        // exactly: a trailing partial instruction would mean the length
        // constant and the encoding disagree.
        assert_eq!(decoded.len(), 11, "{decoded:?}");
        let consumed: usize = decoded.iter().map(|i| i.len()).sum();
        assert_eq!(consumed, cratonvm_jit::gpu_barrier::BARRIER_LEN);
        let end = BASE + cratonvm_jit::gpu_barrier::BARRIER_LEN as u64;

        // MOV R11, &ADDR_FILTER
        assert_eq!(decoded[0].mnemonic(), Mnemonic::Mov);
        assert_eq!(decoded[0].op0_register(), Register::R11);
        assert_eq!(decoded[0].immediate64(), FILTER as u64);

        // CMP QWORD [R11], 0 -- the whole fast path rests on this reading
        // the FILTER WORD, not the pointer to it.
        assert_eq!(decoded[1].mnemonic(), Mnemonic::Cmp);
        assert_eq!(decoded[1].op0_kind(), OpKind::Memory);
        assert_eq!(decoded[1].memory_base(), Register::R11);
        assert_eq!(decoded[1].memory_displacement64(), 0);
        assert_eq!(decoded[1].immediate32(), 0);

        // JZ .skip
        assert_eq!(decoded[2].mnemonic(), Mnemonic::Je);
        assert_eq!(decoded[2].near_branch_target(), end);

        // MOV R11, [R11] -- the filter word itself
        assert_eq!(decoded[3].mnemonic(), Mnemonic::Mov);
        assert_eq!(decoded[3].op0_register(), Register::R11);
        assert_eq!(decoded[3].memory_base(), Register::R11);

        // MOV R10, RAX -- RAX is the array pointer at every store site.
        assert_eq!(decoded[4].mnemonic(), Mnemonic::Mov);
        assert_eq!(decoded[4].op0_register(), Register::R10);
        assert_eq!(decoded[4].op1_register(), Register::RAX);

        // SHR R10, 3 ; AND R10, 63 -- the `(addr >> 3) & 63` bucket,
        // identical to `input_cache::addr_bit`.
        assert_eq!(decoded[5].mnemonic(), Mnemonic::Shr);
        assert_eq!(decoded[5].op0_register(), Register::R10);
        assert_eq!(decoded[5].immediate8(), 3);
        assert_eq!(decoded[6].mnemonic(), Mnemonic::And);
        assert_eq!(decoded[6].op0_register(), Register::R10);
        assert_eq!(decoded[6].immediate8to64(), 63);

        // BT R11, R10 -- bit R10 of the filter word. Register
        // destination, so the offset is mod 64 and the AND above is
        // belt-and-braces rather than load-bearing.
        assert_eq!(decoded[7].mnemonic(), Mnemonic::Bt);
        assert_eq!(decoded[7].op0_register(), Register::R11);
        assert_eq!(decoded[7].op1_register(), Register::R10);

        // JNC .skip
        assert_eq!(decoded[8].mnemonic(), Mnemonic::Jae);
        assert_eq!(decoded[8].near_branch_target(), end);

        // MOV R11, &DIRTY ; MOV BYTE [R11 + R10*1], 1
        assert_eq!(decoded[9].mnemonic(), Mnemonic::Mov);
        assert_eq!(decoded[9].op0_register(), Register::R11);
        assert_eq!(decoded[9].immediate64(), DIRTY as u64);
        assert_eq!(decoded[10].mnemonic(), Mnemonic::Mov);
        assert_eq!(decoded[10].op0_kind(), OpKind::Memory);
        assert_eq!(decoded[10].memory_base(), Register::R11);
        assert_eq!(decoded[10].memory_index(), Register::R10);
        assert_eq!(decoded[10].memory_index_scale(), 1);
        assert_eq!(decoded[10].memory_displacement64(), 0);
        assert_eq!(decoded[10].immediate8(), 1);

        // Nothing outside R10, R11 and the flags is written. This is the
        // property that lets the sequence be dropped after a store in
        // both backends without spilling anything first.
        for insn in &decoded {
            for i in 0..insn.op_count() {
                if insn.op_kind(i) == OpKind::Register {
                    let r = insn.op_register(i);
                    assert!(
                        matches!(r, Register::R10 | Register::R11 | Register::RAX),
                        "barrier touches {r:?}, which no store site guarantees is dead"
                    );
                }
            }
        }
    }

    /// RAX is read and never written: the array pointer is still needed
    /// by nothing here, but writing it would corrupt a backend that
    /// keeps using it (the single-pass `fastore` arm re-reads nothing,
    /// but the contract is what the next store site will rely on).
    #[test]
    fn the_barrier_never_writes_rax() {
        use iced_x86::{Decoder, DecoderOptions, OpKind, Register};

        // No arming: see the sibling test above for why this binary must
        // not publish to the process globals.
        let bytes =
            cratonvm_jit::gpu_barrier::barrier_bytes_for(0x1000, 0x2000).expect("armed pair");

        let mut decoder = Decoder::with_ip(64, &bytes, 0x1_0000, DecoderOptions::NONE);
        for insn in decoder.iter() {
            if insn.op_count() > 0 && insn.op0_kind() == OpKind::Register {
                assert_ne!(
                    insn.op0_register(),
                    Register::RAX,
                    "the barrier must not clobber the array pointer"
                );
            }
        }
    }

    #[test]
    fn scan_empty_code_finds_nothing() {
        assert!(scan_invokestatic_cp_indices(&[]).is_empty());
    }

    // ------------------------------------------------------------------
    // `method_writes_primitive_array` — the JIT half of Phase 10 #2.
    // ------------------------------------------------------------------

        /// The residency cache stands down for the JIT, and stays down.
    ///
    /// AUDIT 2026-09-02. This pins the mechanism behind
    /// [`ArrayWriterPolicy::AllowJit`]: when that policy is selected,
    /// admitting a method that writes a primitive array disables the
    /// cache instead of refusing the method. The DEFAULT does not reach
    /// it — see the module docs for the measurement that put it behind a
    /// flag — so this test calls the mechanism directly rather than
    /// going through `compute`.
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
            // Added 2026-09-02 with the offload path that made
            // `short[]`/`byte[]` marshallable, and therefore cacheable.
            // Until then this scan was right to ignore them.
            (0x54, "bastore"),
            (0x56, "sastore"),
            // Added 2026-09-03 with char[] marshalling.
            (0x55, "castore"),
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
    fn array_store_scan_ignores_reference_stores() {
        // What is left out, and why each one.
        //
        // `aastore` (0x53) stores references; the input cache holds only
        // primitive buffers.
        //
        // `castore` (0x55) USED to be here for the same reason, and moved
        // to the blocking set on 2026-09-03 when `char[]` became
        // marshallable. That is the whole pattern: this list is not a
        // fixed fact about opcodes, it is a shadow of
        // `is_marshallable_array_element`, and it has to move whenever
        // that does.
        //
        // This test used to also assert 0x54 and 0x56, on the reasoning
        // that "bastore/castore/sastore never reach the input cache,
        // which only holds int/long/float/double buffers -- bastore has
        // a helper hook anyway". Both halves stopped being true on
        // 2026-09-02: `short[]`/`byte[]` became marshallable and so
        // cacheable, and `jit_bastore` in fact had NO invalidation hook
        // (only `jit_iastore` did). A JIT-compiled `short[]`/`byte[]`
        // writer then left the residency cache stale, reproducibly --
        // `test_classes/gpu/GpuJitWriterStale.java` diverges from
        // HotSpot on the short and byte checksums at n=8192 over 2000
        // rounds while the int one, whose writer this scan does refuse,
        // stays correct.
        for op in [0x53u8] {
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
