// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compiler API types for CratonVM.
//!
//! Shared types used by the JIT compiler crate and the VM crate:
//! - [`CachedBytecodeMethod`] — method data needed for JIT compilation
//! - [`JitRuntimeHelpers`] — function pointer table for JIT runtime callbacks
//! - [`helpers_abi`] — the typed ABI description of that table: one
//!   `HelperFn*` signature alias per callable slot, the machine-readable
//!   [`HELPER_FIELDS`] descriptor list, null-checked accessors, and
//!   [`JitRuntimeHelpers::validate_with`]
//! - [`gpu_lowering::GpuLowering`] (under the `gpu-lowering` feature) —
//!   trait seam for emitting PTX from a resolved Java method.
//!
//! # SAFETY: the `JitRuntimeHelpers` ABI is frozen
//!
//! [`JitRuntimeHelpers`] is a `#[repr(C)]` table of bare `usize` words. The
//! backend bakes each slot's **absolute address** into RWX machine code and
//! hand-writes the argument-register setup at every call site; the byte offsets
//! are what `as_words`/`word_at` and any non-Rust producer depend on. Never
//! reorder, remove, retype, or insert a field; append only, and bump
//! [`JIT_HELPERS_ABI_VERSION`] when you do — [`helpers_abi::ABI_REVISIONS`]
//! makes that a compile error to forget. The full contract, the per-field
//! signatures, and the per-field nullability rules live in [`helpers_abi`];
//! `docs/jit/helper-abi.md` lists which invariants have tripwires, which
//! do not, and the procedure for adding a slot.
//!
//! ## `gpu-lowering` feature status
//!
//! The `gpu_lowering` module is an off-by-default extension point, compiled
//! only with `--features gpu-lowering`. Current workspace GPU lowering uses
//! `cratonvm-jit-cuda`'s concrete bytecode-to-PTX entry points directly; no
//! workspace crate currently enables this feature or implements
//! [`gpu_lowering::GpuLowering`].
//!
//! The trait remains in-tree as a narrow public seam for a future pluggable PTX
//! backend. If that integration is abandoned, remove the feature, this module,
//! and the related public GPU documentation together.

#[cfg(feature = "gpu-lowering")]
pub mod gpu_lowering;

pub mod helpers_abi;

pub use helpers_abi::{
    accessor_name_matches_field, helper_field, helper_field_offset, str_eq, HelperAbiError,
    HelperAbiRevision, HelperArgAbi, HelperFieldDesc, HelperFnSig, HelperKind, HelperRetAbi,
    ABI_REVISIONS, GOLDEN_HELPER_OFFSETS, HELPERS_NEEDING_WIN64_STACK_ARGS, HELPER_FIELDS,
    HELPER_FIELD_STRIDE, HELPER_FN_SIGS, JIT_HELPERS_ABI_ALIGN, JIT_HELPERS_ABI_SIZE,
    JIT_HELPERS_ABI_VERSION, MAX_PLAUSIBLE_OFFSET, NUM_HELPER_FIELDS, SYSV_INT_ARG_REGS,
    WIN64_INT_ARG_REGS,
};

use std::sync::Arc;

use cratonvm_reader::attribute::ExceptionTableEntry;
use cratonvm_types::ClassId;

/// Count Java parameters using CratonVM's compact one-slot-per-value calling
/// convention. `long` and `double` each occupy one compact argument slot.
///
/// This descriptor contract belongs in the backend-neutral JIT API because
/// class loading needs it while constructing cached call metadata. Keeping it
/// in the concrete compiler crate inverted the intended
/// `classloading -> jit-api <- jit` dependency direction.
pub fn count_param_slots(descriptor: &str) -> usize {
    let bytes = descriptor.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return 0;
    }
    let mut i = 1;
    let mut slots = 0;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' | b'J' | b'D' => {
                slots += 1;
                i += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += usize::from(i < bytes.len());
                slots += 1;
            }
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += usize::from(i < bytes.len());
                    } else {
                        i += 1;
                    }
                }
                slots += 1;
            }
            _ => i += 1,
        }
    }
    slots
}

#[cfg(test)]
mod descriptor_contract_tests {
    use super::count_param_slots;

    #[test]
    fn compact_parameter_count_is_backend_neutral() {
        assert_eq!(count_param_slots("()V"), 0);
        assert_eq!(count_param_slots("(IJDF)V"), 4);
        assert_eq!(
            count_param_slots("([[I[Ljava/lang/Object;Ljava/lang/String;)V"),
            3
        );
        assert_eq!(count_param_slots("not-a-method-descriptor"), 0);
    }
}

/// Cached bytecode method info — everything needed to create a Frame without
/// any lock acquisitions or string allocations.
///
/// `Clone` is implemented by hand rather than derived because
/// [`Self::jit_probe_generation`] is an `AtomicU64` (not `Clone`). The manual
/// impl snapshots its current value, which is semantically right: the field is
/// a *memo*, and copying a memo forward is always sound (at worst the clone
/// re-probes once).
///
/// # Adding a field to this struct is a four-crate change
///
/// There are **38 struct literals** of this type, in
/// `vm/src/runtime/interpreter.rs`, `vm/src/runtime/vtable.rs`,
/// `vm/src/jit/helpers.rs`, `vm/src/runtime/lockfree_resolve.rs`,
/// `vm/src/vm.rs`, `classloading/src/resolution.rs`, `jit/src/lib.rs`,
/// `jit/tests/ir_vs_singlepass.rs` and this file — spanning
/// `cratonvm-jit-api`, `cratonvm-jit`, `cratonvm-classloading` and
/// `cratonvm-vm`. Only five have a `..expr` functional-update tail (the
/// `..make_cached_method()` literals in this file's `mod tests`), and those are
/// the ones to watch: they keep compiling silently while every other literal
/// errors. A `Default` impl does not help — a struct literal must name every
/// field unless it ends in `..expr`.
///
/// Changing an existing field's **type** is far cheaper: all 38 literals spell
/// the four `OnceLock` fields as `std::sync::OnceLock::new()`, which re-infers,
/// so an `OnceLock<T>` → `OnceLock<U>` change touches only this file and the
/// handful of named readers. That is why [`Self::native_callback_cache`] still
/// carries its old name while holding a `NativeCallSite`.
///
/// # CR-CLO-2 — the `method_index` field this struct still wants
///
/// **Not landed.** Specified here because the pass that needed it
/// (`frame-index-and-crash-trace`, 2026-07-26) owned this file but none of the
/// eight *production* sites that would populate the field, so landing it would
/// have left the workspace non-compiling for eight concurrent agents while
/// still producing `None` everywhere.
///
/// *What.* `pub method_index: Option<u32>` — this method's slot in its
/// declaring class's `Class::methods` list. The value belongs here rather than
/// on `Frame` because this entry is built once per call site and `Arc`-shared
/// across every call through it, so the slot is resolved **once per method**
/// instead of once per frame push.
///
/// *Why.* `stackwalker::capture_frames_no_lines` — the thread-dump depositor
/// that runs at every blocking/safepoint deposit and therefore must not take a
/// `ClassStore` borrow — publishes `StackTraceEntry::method_index: None`, so
/// deferred resolution (`stackwalker::resolve_line_numbers_in_place`) falls
/// back to the unambiguous-name rule and leaves every **overloaded** frame with
/// no line number at all. A `u32` copied out of the frame costs no borrow, no
/// lock and no allocation.
///
/// *How to land it.* Add the field, then populate it at the six sites that
/// already hold a live `ClassStore` borrow plus the resolved `&ClassFileMethod`
/// and declaring `ClassId` — `vm/src/runtime/interpreter.rs` in
/// `try_invoke_cached_lambda_impl`, `populate_invoke_cache`,
/// `try_jit_upgrade_with_gate`, `try_jit_compile_callee_slow`,
/// `populate_virtual_invoke_cache`, and `vm/src/jit/helpers.rs` in
/// `try_resume_trapped_callee`. `vm/src/runtime/vtable.rs`'s
/// `vtable_install_adapter` has no store but does have
/// `VtableSlotDescriptor::method_index` already resolved at link time, so it
/// can pass it straight through. The one production site with neither is the
/// first-call tier-up deopt-resume path in `interpreter.rs::execute`, which
/// drops its `class_manager` guard before building the entry; `None` there is
/// correct and costs only that one path the fallback rule.
///
/// Then `Frame::new_pooled_cached` (`vm/src/runtime/frame.rs`) seeds
/// `Frame::method_index` from `cached.method_index` — one line, already
/// commented in place — and `capture_frames_no_lines` swaps its `None` for
/// `f.method_index()`.
///
/// *What it must NOT be used for.* It does not make deferring the `Throwable`
/// capture path safe. Index verification is by *name*, so a redefinition that
/// reorders an overload set across the capture/read window is the one case
/// verification cannot catch, and eager capture has no such window. See
/// `arch-2026-07-26/cross-owner-closeout.md` §6.
///
/// # Descriptor facts
///
/// [`Self::descriptor_facts`] memoizes everything the hot paths need out of
/// `method_descriptor` — see [`DescriptorFacts`] for what was being re-parsed
/// per call before it existed.

/// Everything the interpreter's hot paths need to know about a method
/// descriptor, tokenised once instead of on every call.
///
/// # Why this type exists
///
/// The descriptor of a resolved method never changes, yet two of the
/// interpreter's hottest operations re-parsed it per execution:
///
/// * **Argument decode.** `ParamTags::of(&cached.method_descriptor)` ran a
///   fresh byte scan of the descriptor on *every* invoke through the inline
///   cache. Its own comment records tuning the inline width against ~11 ns of
///   per-call fixed setup — the right measurement aimed at the wrong knob,
///   because the scan should not have been happening at runtime at all.
/// * **Reference return.** `areturn` called `cratonvm_jit::return_type`, a
///   linear scan for `')'`, on every reference return — which in
///   object-oriented bytecode is most returns.
///
/// Measured (`probes/Arity.java`, `--nojit`, min-of-9, arms interleaved both
/// ways): each additional `int` argument cost ~35 ns against HotSpot's
/// template interpreter at ~0.85 ns, and the zero-argument arm — which does
/// no per-argument work at all — still carried the scan.
///
/// # Layout
///
/// Eleven bytes, `Copy`, no allocation and no indirection. `param_tags` holds
/// the first [`DescriptorFacts::INLINE_PARAMS`] parameter tags in declaration
/// order, **excluding** the receiver, with `b'['` standing for any array type
/// (the same tokenisation `nth_param_tag_byte` performs). A descriptor with
/// more parameters than that sets `param_tags_overflow` and readers fall back
/// to the per-index rescan for the tail — the same fallback the pre-computed
/// form always had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorFacts {
    /// Parameter type tags, declaration order, receiver excluded.
    pub param_tags: [u8; Self::INLINE_PARAMS],
    /// How many entries of `param_tags` are meaningful.
    pub param_tag_len: u8,
    /// The descriptor declares more parameters than `param_tags` can hold.
    pub param_tags_overflow: bool,
    /// The byte after `')'`. `b'V'` for void, and for a malformed descriptor —
    /// which is exactly what the `cratonvm_jit::return_type` scan this
    /// replaces answered.
    pub ret_tag: u8,
}

/// Kill switch for every [`DescriptorFacts`] consumer
/// (`CRATONVM_JIT_NO_DESCRIPTOR_FACTS=1`, or
/// `CRATONVM_JIT=-descriptor-facts`). Set, `ParamTags` and the return tag go
/// back through the per-call descriptor scans they replaced, so the change can
/// be priced inside one binary — a cross-binary comparison is not an A/B on a
/// host whose run-to-run spread exceeds the effect.
#[inline]
pub fn descriptor_facts_disabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_DESCRIPTOR_FACTS").is_some()
    })
}

/// The per-call return-tag scan [`CachedBytecodeMethod::return_tag`] replaces,
/// kept here so the kill switch can restore the old cost exactly. Byte-for-byte
/// the same answer as `cratonvm_jit::return_type`, which this crate cannot
/// name (the dependency runs the other way).
#[inline]
fn scan_return_tag(descriptor: &str) -> u8 {
    let bytes = descriptor.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b')' && i + 1 < bytes.len() {
            return bytes[i + 1];
        }
    }
    b'V'
}

impl DescriptorFacts {
    /// Kept at 8 to match the inline width `ParamTags` was measured into: at
    /// 16, the fixed setup cost regressed zero-argument calls in 7 of 8 paired
    /// rounds. That measurement no longer binds — the tokenisation happens
    /// once per method now, not once per call — but the array is still copied
    /// out of the `OnceLock` on each read, so eight (which covers essentially
    /// every real method) keeps that copy inside one cache line alongside the
    /// three scalars.
    pub const INLINE_PARAMS: usize = 8;

    /// Tokenise `descriptor`. Pure; called once per method through
    /// [`CachedBytecodeMethod::descriptor_facts`].
    ///
    /// The parameter walk mirrors `nth_param_tag_byte` exactly, including its
    /// `b'['`-for-arrays tag; `vm`'s `param_tags_match_nth_param_tag_byte`
    /// test pins the two against each other.
    pub fn of(descriptor: &str) -> Self {
        let bytes = descriptor.as_bytes();
        let mut param_tags = [b'L'; Self::INLINE_PARAMS];
        let mut len = 0usize;
        let mut overflow = false;
        let mut i = 1; // skip '('
        while i < bytes.len() && bytes[i] != b')' {
            let tag = bytes[i]; // first byte of this token ('[' for arrays)
            while i < bytes.len() && bytes[i] == b'[' {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'L' => {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1; // consume ';'
                }
                _ => {
                    i += 1; // single-char primitive
                }
            }
            if len < Self::INLINE_PARAMS {
                param_tags[len] = tag;
                len += 1;
            } else {
                overflow = true;
            }
        }
        // Return tag: the byte after the FIRST ')'. Identical to
        // `cratonvm_jit::return_type`, including its `b'V'` answer for a
        // descriptor with no ')' or nothing after it.
        let mut ret_tag = b'V';
        for j in 0..bytes.len() {
            if bytes[j] == b')' && j + 1 < bytes.len() {
                ret_tag = bytes[j + 1];
                break;
            }
        }
        Self {
            param_tags,
            // Cast: bounded by `INLINE_PARAMS` (8) by the loop above.
            param_tag_len: len as u8,
            param_tags_overflow: overflow,
            ret_tag,
        }
    }
}

pub struct CachedBytecodeMethod {
    pub declaring_class_id: ClassId,
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub method_descriptor: Arc<str>,
    pub source_file: Option<Arc<str>>,
    pub code: Arc<[u8]>,
    pub exception_table: Arc<[ExceptionTableEntry]>,
    pub max_stack: u16,
    pub max_locals: u16,
    pub num_params: u16,
    pub is_synchronized: bool,
    pub is_static: bool,
    /// Perf (2026-07-15, `ClientHttpConnectorTests` interpreter-throughput
    /// investigation): memoizes the pure/deterministic part of
    /// `force_native_over_real_jdk_bytecode(class_name, method_name,
    /// method_descriptor)` (a ~1400-line sequential string-comparison
    /// special-case dispatcher in `vm/src/runtime/interpreter.rs`, already
    /// documented as consuming ~51% of all executed instructions on
    /// method-call-heavy workloads -- see
    /// `docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`).
    /// This entry is `Arc`-shared across every cache hit for its callsite,
    /// so populating it once here and reading it thereafter turns an
    /// O(~55 string comparisons) recheck on every cached
    /// `invokevirtual`/`invokestatic` dispatch into an O(1) read after the
    /// first hit. The redefine-dependent wrapper around this pure check
    /// (`should_force_registered_native_over_bytecode`) is NOT cached here
    /// -- it depends on mutable per-class redefine state that can change
    /// after this entry is populated, so it is still re-evaluated on every
    /// hit (cheap: a single generation-counter read plus a short allowlist).
    ///
    /// JDK-ONLY-WAVE2 (`docs/feature-designs/jdk-only-mode.md` §7): this cell
    /// memoizes the *answer* of a hard-coded class-name/method-name dispatcher
    /// (`force_native_over_real_jdk_bytecode`, `vm/src/runtime/interpreter/
    /// native_override.rs`) whose whole purpose is to make a registered native
    /// win over concrete real-JDK bytecode — the exact inversion §1 rule 4
    /// forbids under `JdkOnly`. The list is a wave-2 removal and must NOT be
    /// deleted this wave.
    ///
    /// **This marker used to prescribe making the memo policy-qualified —
    /// `OnceLock<(bool, u8)>`, re-derived on a mode mismatch — on the grounds
    /// that "a `true` memoized under `Compatible` is not a valid answer under
    /// `JdkOnly`". That prescription was wrong and was retracted 2026-08-04.
    /// Do not implement it.** `force_native_over_real_jdk_bytecode(class_name,
    /// method_name, method_descriptor)` takes those three arguments and nothing
    /// else; re-checked against the current tree 2026-08-06, its 2,230-line
    /// body reads no mode, no policy and no VM. The memo is mode-independent
    /// and sound, and qualifying it would buy nothing.
    ///
    /// Policy is applied *downstream* of this cell, at dispatch — which is
    /// where the real defect turned out to be. Chasing this marker instead of
    /// the dispatch is why seven call sites reached
    /// `intercept_force_registered_native{,_cached}` with no `dispatch_policy`,
    /// no `resolve_native_dispatch_wave1` and no `record_invocation` until
    /// 2026-08-04. They now route through `admit_forced_native`.
    ///
    /// What is left here is not a defect: when the list goes, so does this
    /// cell. Until then it turns ~55 string comparisons per cached dispatch
    /// into an O(1) read.
    pub force_native_cache: std::sync::OnceLock<bool>,
    /// Memoized [`DescriptorFacts`] for [`Self::method_descriptor`]. Read
    /// through [`Self::descriptor_facts`], never directly.
    ///
    /// A `OnceLock` rather than an eagerly-computed field so the ~50
    /// struct literals that build this type (production and test alike)
    /// keep one uniform, `const`-constructible initializer, exactly as the
    /// four memo cells above it do.
    pub descriptor_facts_cache: std::sync::OnceLock<DescriptorFacts>,
    /// Which of `intercept_force_registered_native_cached`'s three *special-case*
    /// arms this call site's triple can possibly reach, as `INTERCEPT_SHAPE_*`
    /// bits. Zero — the answer for almost every call site in a program — means
    /// none of them, and the hot path skips straight to
    /// [`Self::force_native_cache`].
    ///
    /// # Why this exists
    ///
    /// `force_native_cache` above memoizes the ~55-comparison
    /// `force_native_over_real_jdk_bytecode` gauntlet, but it sits **below**
    /// three earlier arms that were still evaluated from scratch on every
    /// cached-invoke hit:
    ///
    /// * a `ClassLoader` null-resource re-target, keyed on
    ///   `(method_name, method_descriptor)` against three pairs;
    /// * a `java/lang/Class` reflection re-target, keyed on the same pair
    ///   against four more;
    /// * `real_http_url_connection_native`, whose entire gate is `class_name`
    ///   against five literals.
    ///
    /// Every one of those keys is a **function of this entry's own triple**,
    /// which never changes — so they were per-call-site constants re-derived
    /// per call. `perf` on `probes/InvokeAttributionProbe.java` — whose only
    /// call is `int callee(int)`, matching none of them — put
    /// `intercept_force_registered_native_cached` at 1.67% and
    /// `real_http_url_connection_native` at **1.50%** of the interpreted-invoke
    /// arm, with a `memcpy` arm underneath (`str::eq` bottoms out in `memcmp`).
    /// See `known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`.
    ///
    /// The *argument*- and *receiver*-dependent halves of those arms are NOT
    /// memoized and must not be: a null second argument, an Objenesis-shaped
    /// receiver and a redefined class are per-call state. This cell answers
    /// only "could this triple ever reach that arm", so a set bit still runs
    /// the original test in full, and a clear bit skips a test whose
    /// name-keyed half could not have matched anyway.
    pub intercept_shape_cache: std::sync::OnceLock<u8>,
    /// Per-call-site native-dispatch memo. **Read it through
    /// [`Self::native_call_site`], never directly.**
    ///
    /// Perf follow-up (2026-07-19, TestResponsePerformance interpreter-
    /// throughput residual): `intercept_force_registered_native_cached` called
    /// `NativeMethodRegistry::find` (a hash-keyed lookup over all three of
    /// class/method/descriptor) on every single cached-invoke hit whose
    /// force-native decision was `true` -- confirmed via `perf` to be the
    /// #2 hottest symbol (~7% of samples) on that benchmark, second only to
    /// the interpreter's own frame-dispatch loop. See
    /// docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md.
    ///
    /// # Why this holds a `NativeCallSite` and not an `Option<NativeCallback>`
    ///
    /// It used to be `OnceLock<Option<NativeCallback>>`, whose soundness
    /// argument was "native registration is immutable after VM boot". That is
    /// true of the steady state but **not of boot itself**, and not of
    /// `alias_class` or of the lazy `register_*` passes that run after the
    /// first bytecode executes. A `None` memoized before those run stayed
    /// wrong forever: a native that *is* registered, and that dispatches fine
    /// through `find`, was invisible at this call site for the life of the
    /// process. `cratonvm_native_api::NativeCallSite` keys its memo on the
    /// registry generation instead, so a stale negative self-heals at the cost
    /// of one `u32` compare -- the same argument
    /// [`Self::jit_probe_generation`] below already relies on. See
    /// `arch-2026-07-26/native-dispatch-memoization.md` §3
    /// Steps 0 and 2 (sites A1-A3).
    ///
    /// # ONE CELL, ONE TRIPLE
    ///
    /// A `NativeCallSite` memo is `(generation << 32) | slot`, validated
    /// against the generation *alone* -- the triple is deliberately not
    /// re-checked on a warm hit, because re-hashing three strings is the exact
    /// cost the cell exists to remove. A cell reached with two different
    /// triples silently serves the second whatever the first memoized. This
    /// cell therefore answers for **this entry's own
    /// `(class_name, method_name, method_descriptor)` and nothing else**. A
    /// call site that needs a *different* triple (e.g. the
    /// `java/lang/ClassLoader` re-target inside
    /// `intercept_force_registered_native_cached`) must use its own cell or
    /// plain `find`. Pinned by
    /// `sharing_one_memo_cell_across_two_triples_silently_mis_answers` in
    /// `vm/src/runtime/interpreter.rs`.
    ///
    /// # Field name
    ///
    /// Still `native_callback_cache` only because ~20 struct literals live in
    /// crates the wave that changed this could not write to, and the literals
    /// all read `std::sync::OnceLock::new()`, which type-infers unchanged. The
    /// rename to `native_call_site` is a pure mechanical follow-up; see the
    /// cross-owner request in
    /// `arch-2026-07-26/interpreter-completion.md`.
    ///
    /// # JDK-only: this cell does NOT lose the `NativeKind`
    ///
    /// A cache that stores only a `NativeCallback` cannot re-check policy on a
    /// hit — it has thrown away the one fact (`NativeKind`) the decision needs.
    /// This cell is *not* that shape, and that is worth stating explicitly
    /// because its accessor name (`callback()`) makes it look like it is: the
    /// memo word is a `NativeMethodId`, not a function pointer, so the kind is
    /// still one `O(1)` array index away (`NativeMethodRegistry::kind_of_id`)
    /// and so is the census counter (`record_invocation`).
    ///
    /// The gap is therefore in the **callers**, not the cell. Use
    /// [`Self::native_dispatch`] below, which redeems the id into the
    /// `(callback, kind)` pair `vm_exec::resolve_dispatch` takes as its
    /// `native` argument and records the invocation on the way past. A caller
    /// that reaches for `native_call_site().callback(..)` instead has silently
    /// opted out of both the policy check and the census.
    pub native_callback_cache: std::sync::OnceLock<cratonvm_native_api::NativeCallSite>,
    /// T2.5 — memoized JIT invocation-counter key for this method, i.e. the
    /// packed `(declaring_class_id << 32) | hash(method_name ++ descriptor)`
    /// u64 that the interpreter uses to index
    /// `ProfileStore::increment_invocation`.
    ///
    /// The interpreter's `Bytecode` / `VirtualBytecode` dispatch arms recomputed
    /// this key on EVERY interpreted invocation of a not-yet-compiled method by
    /// running a 31-multiplier byte loop over both the method name and the full
    /// descriptor — pure per-call overhead on the hottest interpreter path, for
    /// a value that is a pure function of three immutable fields of this entry.
    /// Memoized here for exactly the same reason (and by exactly the same
    /// argument) as [`Self::force_native_cache`] and
    /// [`Self::native_callback_cache`] above. Read via [`Self::invoc_key`].
    pub invoc_key: std::sync::OnceLock<u64>,
    /// T2.2 — epoch memo for "this method has no published JIT body".
    ///
    /// Holds the value of `cratonvm_jit::jit_cache_generation()` as of the last
    /// time an interpreter dispatch arm probed the shared JIT cache for this
    /// method and found *nothing*. `0` means "never probed" (the live generation
    /// starts at 1 and only ever increases, so `0` can never compare equal to
    /// it).
    ///
    /// Why this exists: the interpreter's cached-invoke `Bytecode` arms had to
    /// call `JitCache::get(class, method, descriptor, class_id)` on every single
    /// interpreted call, purely to notice that a background compile had
    /// published a body for this method. That lookup hashes all three strings
    /// and then re-compares all three with full string equality — the exact
    /// re-resolution the per-call-site inline cache exists to avoid. Because
    /// *every* JIT-cache publication and invalidation bumps the global
    /// generation, comparing this snapshot against it is an equivalent test:
    /// equal ⇒ the cache content has not changed since we last looked and found
    /// nothing, so looking again cannot find anything. Steady state therefore
    /// costs two integer loads and a compare instead of three string hashes and
    /// three string comparisons.
    ///
    /// Correctness rests on the generation being bumped by every writer of the
    /// JIT cache; see `JitCache::put` / `put_osr` / `invalidate_matching` /
    /// `clear_all` in `jit/src/lib.rs`, which are the only mutators.
    pub jit_probe_generation: std::sync::atomic::AtomicU64,
    /// Perf (2026-07-25, bytecode quickening): memoizes this method's
    /// pre-decoded instruction stream, mirroring `force_native_cache` above.
    ///
    /// The interpreter's spec-correct dispatch path used to call
    /// `Instruction::decode` on *every* execution of *every* bytecode -- a
    /// full opcode match plus operand reads, and a heap allocation for the
    /// out-of-line payload of every `tableswitch` / `lookupswitch` that
    /// executed. `cratonvm_reader::QuickenedCode` does that decode once and
    /// hands out borrowed `&Instruction` records thereafter.
    ///
    /// `None` means "this method could not be pre-decoded" (a linear walk
    /// from pc 0 hit a decode error); such methods keep using the original
    /// on-demand decode, so quickening can never change behaviour.
    ///
    /// The stream itself is interned process-wide on the identity of the
    /// bytecode allocation (`cratonvm_reader::quickened::intern`), so the
    /// several `CachedBytecodeMethod`s that a hot method accumulates across
    /// call sites all share one copy rather than each building their own.
    pub quickened: std::sync::OnceLock<Option<std::sync::Arc<cratonvm_reader::QuickenedCode>>>,
}

impl Clone for CachedBytecodeMethod {
    fn clone(&self) -> Self {
        Self {
            declaring_class_id: self.declaring_class_id,
            class_name: Arc::clone(&self.class_name),
            method_name: Arc::clone(&self.method_name),
            method_descriptor: Arc::clone(&self.method_descriptor),
            source_file: self.source_file.clone(),
            code: Arc::clone(&self.code),
            exception_table: Arc::clone(&self.exception_table),
            max_stack: self.max_stack,
            max_locals: self.max_locals,
            num_params: self.num_params,
            is_synchronized: self.is_synchronized,
            is_static: self.is_static,
            force_native_cache: self.force_native_cache.clone(),
            // Same reasoning as `force_native_cache`: a pure function of a
            // field the clone `Arc`-shares with this one
            // (`method_descriptor`), so carrying the memo forward answers the
            // same question.
            descriptor_facts_cache: self.descriptor_facts_cache.clone(),
            // Same reasoning as `force_native_cache`: the cell is a pure
            // function of the triple, and the clone's triple is `Arc`-shared
            // with this one, so carrying the memo forward answers for the same
            // question.
            intercept_shape_cache: self.intercept_shape_cache.clone(),
            // `NativeCallSite: Clone` snapshots the memo word. Carrying it
            // forward is sound for the same reason `jit_probe_generation`'s
            // snapshot is: the memo is generation-keyed, so a clone that
            // inherits a stale word re-resolves on its first read after the
            // registry moves. The clone answers for the same triple, because
            // the triple fields are `Arc`-shared with it.
            native_callback_cache: self.native_callback_cache.clone(),
            invoc_key: self.invoc_key.clone(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(
                self.jit_probe_generation
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            // Cloning the memo is correct and desirable: the quickened stream is
            // a pure function of `code`, which is `Arc`-shared with the clone, so
            // the clone would derive an identical stream. Carrying it over just
            // avoids re-deriving it (and the intern-table probe) on first use.
            quickened: self.quickened.clone(),
        }
    }
}

impl CachedBytecodeMethod {
    /// Everything the hot paths need from this method's descriptor,
    /// tokenised on first use and shared by every later call through this
    /// entry.
    ///
    /// This entry is `Arc`-shared across every dispatch that hits its call
    /// site, so the scan happens once per METHOD rather than once per call.
    /// See [`DescriptorFacts`] for the two hot paths that were re-parsing
    /// the descriptor per execution before this existed.
    #[inline]
    pub fn descriptor_facts(&self) -> &DescriptorFacts {
        self.descriptor_facts_cache
            .get_or_init(|| DescriptorFacts::of(&self.method_descriptor))
    }

    /// The descriptor's return-type tag byte (`b'V'` for void).
    /// Equivalent to `cratonvm_jit::return_type(&self.method_descriptor)`,
    /// without the per-call scan.
    #[inline]
    pub fn return_tag(&self) -> u8 {
        if descriptor_facts_disabled() {
            return scan_return_tag(&self.method_descriptor);
        }
        self.descriptor_facts().ret_tag
    }

    /// This entry's native-dispatch memo cell — see
    /// [`Self::native_callback_cache`] for the full contract.
    ///
    /// The returned cell is a drop-in for
    /// `shared.natives.native_methods.find(class, method, desc)`:
    ///
    /// ```ignore
    /// let cb = cached.native_call_site().callback(
    ///     &shared.natives.native_methods,
    ///     cached.class_name.as_ref(),
    ///     cached.method_name.as_ref(),
    ///     cached.method_descriptor.as_ref(),
    /// );
    /// ```
    ///
    /// It must only ever be handed this entry's own triple — the memo is per
    /// call site, not per triple, and is not re-validated against the strings
    /// on a warm hit. The `OnceLock` wrapper exists purely so that the ~20
    /// `CachedBytecodeMethod` struct literals that spell this field
    /// `std::sync::OnceLock::new()` keep compiling; it costs one already-hot
    /// acquire load ahead of the memo's relaxed load.
    #[inline]
    pub fn native_call_site(&self) -> &cratonvm_native_api::NativeCallSite {
        self.native_callback_cache
            .get_or_init(cratonvm_native_api::NativeCallSite::new)
    }

    /// Resolve this call site's native as the `(callback, kind)` pair
    /// `vm/src/vm/vm_exec.rs::resolve_dispatch` takes as its `native` argument,
    /// **and count the dispatch**.
    ///
    /// This is the JDK-only-correct replacement for
    /// `native_call_site().callback(registry, class, method, desc)`
    /// (`docs/feature-designs/jdk-only-mode.md` §4, §7). The bare `callback()`
    /// form discards the `NativeKind`, and a caller holding only a callback
    /// cannot answer "may this run under `JdkOnly`?" — the whole strict-mode
    /// question. It also never touches `record_invocation`, so every dispatch
    /// through it is invisible to the census, and the CI gate asserts on a
    /// census figure (`synthetic_stub_invocations == 0`). An uncounted path is
    /// an unverifiable one.
    ///
    /// # Cost
    ///
    /// One extra `O(1)` slot index over `callback()` (`kind_of_id`) plus one
    /// relaxed increment (`record_invocation`). No hashing, no allocation, no
    /// string comparison: the memo already produced the `NativeMethodId`, and
    /// every remaining step is an array index off it. Safe on the hot path,
    /// which is why the counter is incremented here rather than at some
    /// coarser choke point.
    ///
    /// # Contract
    ///
    /// Same one-cell-one-triple rule as [`Self::native_call_site`]: this may
    /// only ever be asked about **this entry's own**
    /// `(class_name, method_name, method_descriptor)`, which it reads from
    /// `self` so a caller cannot get it wrong. A site that needs a different
    /// triple (the `java/lang/ClassLoader` re-target inside
    /// `intercept_force_registered_native_cached`) must use its own cell or
    /// plain `find` — see the ONE CELL, ONE TRIPLE section on
    /// [`Self::native_callback_cache`].
    ///
    /// Returns `None` when no native is registered for the triple, which is the
    /// same answer `callback()` gives and which `resolve_dispatch` reads as
    /// "no native — use the bytecode".
    #[inline]
    pub fn native_dispatch(
        &self,
        registry: &cratonvm_native_api::NativeMethodRegistry,
    ) -> Option<(
        cratonvm_native_api::NativeCallback,
        cratonvm_native_api::NativeKind,
    )> {
        let id = self.native_call_site().resolve(
            registry,
            self.class_name.as_ref(),
            self.method_name.as_ref(),
            self.method_descriptor.as_ref(),
        )?;
        let callback = registry.callback_of(id)?;
        let kind = registry.kind_of_id(id)?;
        // Counted at the point of *decision*, not of invocation, because the
        // caller may legitimately discard the pair when `resolve_dispatch`
        // answers `Bytecode` (§7 order rule 3: concrete bytecode beats a
        // registered bridge). That over-counts a bridge the policy then
        // declines to run. It is the right direction to err in for a gate whose
        // assertion is `synthetic_stub_invocations == 0`: it can only ever make
        // a stub visible, never hide one.
        registry.record_invocation(id);
        Some((callback, kind))
    }

    /// The memoized JIT invocation-counter key for this method — see
    /// [`Self::invoc_key`]. Computed on first use, then read straight out of the
    /// `OnceLock`.
    ///
    /// The hash must stay bit-identical to the two open-coded loops this
    /// replaced (`vm/src/runtime/interpreter.rs`, the invokestatic `Bytecode`
    /// arm and the instance tier-up path), because both keyed the *same*
    /// `ProfileStore` invocation counters: a different key would silently reset
    /// every method's warmup count.
    #[inline]
    pub fn invoc_key(&self) -> u64 {
        *self.invoc_key.get_or_init(|| {
            invoc_key_parts(
                self.declaring_class_id.as_u32(),
                &self.method_name,
                &self.method_descriptor,
            )
        })
    }

    /// T2.2 — has this method already been probed against the shared JIT cache
    /// at generation `current_generation` and found to have no compiled body?
    ///
    /// `current_generation` must come from `cratonvm_jit::jit_cache_generation()`
    /// (an `Acquire` load). A `true` answer means the caller may skip the
    /// string-keyed `JitCache::get` entirely.
    #[inline]
    pub fn jit_probe_is_current(&self, current_generation: u64) -> bool {
        self.jit_probe_generation
            .load(std::sync::atomic::Ordering::Relaxed)
            == current_generation
    }

    /// T2.2 — record that a `JitCache::get` for this method returned `None`
    /// while the cache was at `generation`.
    ///
    /// `Relaxed` is sufficient: the value is only ever used to skip a lookup
    /// that would have returned `None` anyway, and the *global* generation read
    /// that guards it is an `Acquire` load, so a reader that observes a newer
    /// generation also observes the publication that caused it.
    #[inline]
    pub fn record_jit_probe_miss(&self, generation: u64) {
        self.jit_probe_generation
            .store(generation, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The canonical `ProfileStore` invocation-counter key for a method.
///
/// Single source of the hash, because several sites key the SAME counters and a
/// divergent hash would silently reset every method's warmup count rather than
/// fail: [`CachedBytecodeMethod::invoc_key`] memoizes this, and the interpreter's
/// back-edge tier-up path recomputes it from a live frame (where no
/// `CachedBytecodeMethod` is in hand). Keep them bit-identical.
#[inline]
pub fn invoc_key_parts(declaring_class_id: u32, method_name: &str, method_descriptor: &str) -> u64 {
    let mut h = 0u32;
    for &b in method_name.as_bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u32); // Widening: hash computation
    }
    for &b in method_descriptor.as_bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u32); // Widening: hash computation
    }
    // Widening: class ID to u64 for hash key
    ((declaring_class_id as u64) << 32) | (h as u64)
}

/// JEP 358 (helpful NPE) — operation-kind codes carried out-of-band from a
/// JIT-originated `NullPointerException` so the interpreter's post-JIT drain
/// can attach the *action-only* JEP-358 message ("Cannot load from int array",
/// "Cannot read the array length", …).
///
/// **Single source of truth.** These `u8` codes are baked as `MOV` immediates
/// by the inline null-check stubs in `jit/src/x64.rs` (the JIT crate, which
/// cannot depend on the VM crate) AND mapped to their HotSpot `BytecodeUtils`
/// strings by `vm/src/runtime/exceptions.rs::helpful_npe` (which re-exports this
/// module as `jit_action`). Keeping the numeric vocabulary in `jit-api` — the
/// one crate both sides already depend on — removes the drift hazard of two
/// hand-kept copies.
///
/// Codes `0..=7` are the increment-4 originals (length + int/object/byte
/// load/store); `8..=17` extend the vocabulary to the remaining primitive
/// element kinds (long/float/double/char/short) so the inline-codegen null
/// path can name every array element type HotSpot does. Values are **stable**
/// (append-only): the JIT bakes them into RWX code, so never renumber.
///
/// `baload`/`bastore` cannot distinguish `byte[]` from `boolean[]` at the null
/// site (same opcode, null receiver), so both map to [`ALOAD_BYTE`]/
/// [`ASTORE_BYTE`] — matching the existing per-type JIT helpers. (HotSpot
/// spells this "byte/boolean"; the divergence is pre-existing and tracked
/// separately, not introduced here.)
pub mod npe_action {
    /// No action recorded — the interpreter emits an unmessaged NPE
    /// (today's default shape). Inline sites with no precise array kind
    /// (e.g. an intrinsic's array null-check) use this.
    pub const NONE: u8 = 0;
    /// `arraylength` on a null array.
    pub const ARRAY_LENGTH: u8 = 1;
    /// `iaload` on a null `int[]`.
    pub const ALOAD_INT: u8 = 2;
    /// `aaload` on a null reference array.
    pub const ALOAD_OBJECT: u8 = 3;
    /// `baload` on a null `byte[]`/`boolean[]`.
    pub const ALOAD_BYTE: u8 = 4;
    /// `iastore` into a null `int[]`.
    pub const ASTORE_INT: u8 = 5;
    /// `aastore` into a null reference array.
    pub const ASTORE_OBJECT: u8 = 6;
    /// `bastore` into a null `byte[]`/`boolean[]`.
    pub const ASTORE_BYTE: u8 = 7;
    /// `laload` on a null `long[]`.
    pub const ALOAD_LONG: u8 = 8;
    /// `faload` on a null `float[]`.
    pub const ALOAD_FLOAT: u8 = 9;
    /// `daload` on a null `double[]`.
    pub const ALOAD_DOUBLE: u8 = 10;
    /// `caload` on a null `char[]`.
    pub const ALOAD_CHAR: u8 = 11;
    /// `saload` on a null `short[]`.
    pub const ALOAD_SHORT: u8 = 12;
    /// `lastore` into a null `long[]`.
    pub const ASTORE_LONG: u8 = 13;
    /// `fastore` into a null `float[]`.
    pub const ASTORE_FLOAT: u8 = 14;
    /// `dastore` into a null `double[]`.
    pub const ASTORE_DOUBLE: u8 = 15;
    /// `castore` into a null `char[]`.
    pub const ASTORE_CHAR: u8 = 16;
    /// `sastore` into a null `short[]`.
    pub const ASTORE_SHORT: u8 = 17;
}
/// Bit the JIT may set in `jit_getfield`'s `field_index` argument to say
/// **"this receiver is already proven to be an oop"**.
///
/// When set, the helper skips its `is_object_address` heap-membership walk.
/// Everything else — the pending-NPE contract, the slot bounds check, the
/// compact/legacy layout split, the read and the reference decode — is
/// unchanged, so this carries no new colouring or layout exposure. It is
/// purely "skip one validation".
///
/// # Why a flag bit and not a second helper slot
///
/// `helpers_abi.rs` pins [`JitRuntimeHelpers`]'s field count, byte size and
/// golden offsets with const assertions plus an ABI version, precisely so the
/// offsets the JIT bakes cannot move. A one-bit argument flag needs none of
/// that. `field_index` is a small non-negative slot index — a class-file field
/// table is `u16`-sized — so bit 62 cannot collide with a real index.
///
/// # Why skipping the walk is sound
///
/// The walk is validation against a stale/garbage receiver from a miscompiled
/// frame. The emitter sets this only where the IR's type lattice types the base
/// node `IrType::Ref` — the same proof the PRIMITIVE trusted-oop arm already
/// relies on, and that arm goes further and performs a raw inline load off this
/// very receiver. `plausible_heap_pointer` still runs either way, so null and
/// unaligned/out-of-range bits are still refused.
pub const GETFIELD_RECEIVER_PROVEN_OOP: u64 = 1 << 62;

/// Bit the JIT sets in `jit_getfield`'s `field_index` argument to say **"the
/// value I am about to receive is a REFERENCE, and I will dereference it"**.
///
/// Without it the helper cannot know what the caller asked for. It reads a
/// `Value` out of the slot and returns the payload of whichever variant it
/// finds, so a reference field whose slot has been type-punned to a primitive
/// comes back as that primitive's bits — and compiled code, which emitted a
/// plain `MOV r32,[reg+4]` for the `arraylength` that follows, dereferences an
/// integer.
///
/// That is not hypothetical. `org.apache.derby.iapi.types.SQLChar.rawData` is
/// declared `[C` and was read back as `Int(1)`; the emitted code went
///
/// ```text
///   call  jit_getfield          ; -> rax = 1
///   mov   r10, 8000000000000000h
///   cmp   rax, r10              ; the ONLY value it checks for
///   je    <deopt>
///   mov   eax,[rax+4]           ; arraylength  ->  SIGSEGV at addr 0x5
/// ```
///
/// i64::MIN was the only rejected value, so every other primitive payload
/// became a wild pointer.
///
/// With this bit set, a primitive found in a slot the caller will dereference
/// is degraded to null rather than returned — the same "degrade rather than
/// hand the JIT bits it will later deref → SIGSEGV" policy
/// `jit_decode_ref_word` already applies to an implausible POINTER, extended
/// to the case where the slot does not hold a pointer at all.
///
/// Bit 61 for the same reason bit 62 was chosen: a class-file field table is
/// `u16`-sized, so a real slot index cannot reach either.
pub const GETFIELD_EXPECT_REFERENCE: u64 = 1 << 61;

/// Every bit in `jit_getfield`'s `field_index` argument that is a flag rather
/// than part of the index. Masked off in one place so a third flag cannot be
/// added without the strip site seeing it.
pub const GETFIELD_FLAG_BITS: u64 = GETFIELD_RECEIVER_PROVEN_OOP | GETFIELD_EXPECT_REFERENCE;

/// Build `jit_getfield`'s third argument.
///
/// Seven call sites across the two backends emit this argument, and before
/// this existed each of them wrote `field_index as u64` by hand. That is how
/// [`GETFIELD_EXPECT_REFERENCE`] would rot: a site added later, or one whose
/// author did not know a safety bit had appeared, silently opts out of it and
/// the hole reopens at exactly one `getfield` arm. One encoder means a new flag
/// reaches every site by construction.
pub const fn getfield_index_arg(
    field_index: u32,
    is_reference: bool,
    receiver_proven_oop: bool,
) -> u64 {
    let mut arg = field_index as u64;
    if is_reference {
        arg |= GETFIELD_EXPECT_REFERENCE;
        // The receiver proof is only consulted on the reference path, and only
        // ever as an optimisation — see `GETFIELD_RECEIVER_PROVEN_OOP`.
        if receiver_proven_oop {
            arg |= GETFIELD_RECEIVER_PROVEN_OOP;
        }
    }
    arg
}

/// Recover the slot index from [`getfield_index_arg`]'s result — the DECODER
/// twin of that encoder.
///
/// One encoder means a new flag reaches every emitter by construction. It does
/// nothing at all for the other side, and the other side is where the flags are
/// dangerous: a decoder that misses one indexes an object with a bit set at 61
/// or 62, which is not a bounds-check failure, it is `idx * SLOT_SIZE`
/// overflowing.
///
/// That is not hypothetical twice over. `jit/src/x64/tests.rs`'s `stub_getfield`
/// checked `field_index as u32` against the slot count and then indexed with
/// `field_index as usize`: the u32 truncation dropped the flag, the bounds
/// check passed, and the index walked off the object — "which aborted the whole
/// test binary the first time a reference load carried a flag" (2026-08-23).
/// `jit/tests/ir_vs_singlepass.rs`'s twin stub was not updated with it and did
/// the same thing four days later, aborting the process inside an
/// `extern "C"` fn — where a panic cannot unwind, so it took every test after
/// it in the file with it.
///
/// Both of those are decode sites that had to *know* to strip. This is the one
/// place that knows, so a third flag added to [`GETFIELD_FLAG_BITS`] reaches
/// them without anyone remembering.
#[must_use]
pub const fn getfield_index_of(arg: i64) -> i64 {
    (arg as u64 & !GETFIELD_FLAG_BITS) as i64
}

#[cfg(test)]
mod getfield_arg_tests {
    use super::*;

    /// [`getfield_index_of`] inverts [`getfield_index_arg`] for every flag
    /// combination, and the flag mask covers exactly the two flags.
    ///
    /// The pair exists because a DECODE site that misses a flag does not fail a
    /// bounds check — it overflows `idx * SLOT_SIZE`. Two test doubles have
    /// already done exactly that; see [`getfield_index_of`].
    #[test]
    fn the_index_argument_round_trips_through_every_flag_combination() {
        for slot in [0u32, 1, 7, u16::MAX as u32] {
            for is_ref in [false, true] {
                for proven in [false, true] {
                    let arg = getfield_index_arg(slot, is_ref, proven);
                    assert_eq!(
                        getfield_index_of(arg as i64),
                        i64::from(slot),
                        "slot {slot} (is_ref={is_ref}, proven={proven})"
                    );
                    assert_eq!(
                        arg & GETFIELD_EXPECT_REFERENCE != 0,
                        is_ref,
                        "the reference flag must ride only on the reference path"
                    );
                }
            }
        }
    }

    /// A real slot cannot reach either flag bit, which is the property that
    /// lets them share the argument at all.
    #[test]
    fn a_class_file_slot_index_cannot_reach_the_flag_bits() {
        // A class-file field table is `u16`-sized.
        let widest = getfield_index_arg(u16::MAX as u32, false, false);
        assert_eq!(widest & GETFIELD_FLAG_BITS, 0);
        assert_eq!(
            GETFIELD_FLAG_BITS,
            GETFIELD_RECEIVER_PROVEN_OOP | GETFIELD_EXPECT_REFERENCE,
            "a third flag must join the mask, or every decoder silently keeps it"
        );
    }
}

/// Function pointer table for JIT runtime callbacks.
///
/// The JIT compiler embeds these addresses into generated machine code as
/// absolute `CALL` targets. Each pointer is the address of an `extern "C"`
/// function implemented in the VM crate.
///
/// `#[repr(C)]` is mandatory: this struct is an explicit ABI boundary. The
/// JIT may read a field either by name or by computed byte offset; the
/// default `repr(Rust)` layout is unspecified and the compiler is free to
/// reorder fields, so a stable C layout is the only sound contract here.
///
/// `Default` (every field zero) exists for the integration tests in
/// `jit/tests`, which build this table literally and wire up only the helpers
/// they exercise. They spread `..Default::default()` so that adding a field
/// here cannot break all of them at once — it did, for `service_callee_deopt`
/// and `set_throw_bci`.
///
/// Zero is the right default only for the fields whose contract already reads
/// 0 as unwired (`get_current_thread`, `safepoint_flag_addr`, the TLAB and
/// card-table offsets, …), where the backend checks for it and falls back.
/// It is NOT inert for a field the backend reaches via `emit_call_absolute`:
/// there a 0 is a CALL to a null pointer. If you add a call-target helper,
/// expect the `jit/tests` tables to need it wired explicitly to their local
/// trap stub, exactly as `set_throw_bci` / `service_callee_deopt` are.
/// Production builds always go through `build_helpers`, which populates every
/// field explicitly.
///
/// # SAFETY: frozen binary layout
///
/// This struct is an ABI, not a data structure. The rules are absolute:
///
/// * **`#[repr(C)]` is mandatory.** The JIT reads slots as
///   `[helpers_ptr + disp32]` with the displacement computed at compile time.
///   `repr(Rust)` may reorder fields, which would silently re-point every
///   baked `CALL` at a different helper.
/// * **Every field is `usize`** (8 bytes; x86-64 only), so the byte offset of
///   field *N* is exactly `N * 8`. A non-`usize` field would introduce padding
///   and break that identity — see the `const _` assertions below.
/// * **Append only.** Never reorder, never remove, never retype, never insert
///   in the middle. New helpers go at the end, exactly as `set_throw_bci` and
///   `service_callee_deopt` did.
/// * **Bump [`JIT_HELPERS_ABI_VERSION`] on any shape change**, including a
///   pure append: once a field exists, a producer and a consumer built against
///   different revisions cannot be distinguished by size alone.
/// * **Nullability is per field**, recorded in [`HELPER_FIELDS`]. A `required`
///   slot is `CALL`ed unconditionally and must be non-zero; a non-required
///   slot reads `0` as the documented "not wired" sentinel and the backend
///   falls back (or emits nothing).
///
/// The raw `usize` type conveys none of that. Use the typed view in
/// [`helpers_abi`] — [`Self::validate_with`] to check a table, the
/// `<field>_fn` accessors to obtain a null-checked, correctly-typed function
/// pointer, and [`HELPER_FIELDS`] to describe the layout to tooling.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct JitRuntimeHelpers {
    pub newarray: usize,
    pub new_object: usize,
    pub anewarray_object: usize,
    pub baload: usize,
    pub bastore: usize,
    pub iaload: usize,
    pub iastore: usize,
    pub aaload: usize,
    pub aastore: usize,
    pub multianewarray_2d: usize,
    pub arraylength: usize,
    pub getfield: usize,
    pub putfield_int: usize,
    pub putfield_long: usize,
    pub putfield_float: usize,
    pub putfield_double: usize,
    pub putfield_object: usize,
    pub getstatic: usize,
    pub putstatic_int: usize,
    pub putstatic_long: usize,
    pub putstatic_float: usize,
    pub putstatic_double: usize,
    pub putstatic_object: usize,
    pub checkcast: usize,
    pub instanceof_check: usize,
    pub throw_aioobe: usize,
    /// Direct-throw helper for `ArithmeticException` ("/ by zero"), the
    /// div-by-zero sibling of [`throw_aioobe`]. Called from the `idiv`/`irem`/
    /// `ldiv`/`lrem` zero-divisor guard stub: sets a pending-arithmetic flag and
    /// returns the `i64::MIN` deopt sentinel so the interpreter throws the
    /// exception through the method's exception table WITHOUT re-running the
    /// method from entry. The prior path (`uncommon_trap` → re-run) double-
    /// executed any side effect preceding the trap (HotSpot does not).
    /// Signature: `extern "C" fn() -> i64`.
    pub throw_arithmetic: usize,
    pub invoke_dispatch: usize,
    pub invoke_virtual_mic: usize,
    /// Scalar lambda adapter for TDigest numeric kernels.
    pub lambda_int_to_double: usize,
    pub write_barrier: usize,
    /// Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
    ///
    /// Logs the *old* reference value before a reference store, so the
    /// concurrent marker maintains the snapshot-at-the-beginning invariant.
    /// Without this, JIT-compiled `aastore`/`putfield`/`putstatic`
    /// overwriting a still-live reference during concurrent marking would
    /// drop the only path to the overwritten target → missed mark →
    /// use-after-free on the next mixed evacuation.
    ///
    /// Signature: `extern "C" fn(vm_ptr: i64, old_ref: i64)`.
    /// The helper short-circuits cheaply (single Acquire load) when
    /// `SatbQueue::is_active() == false`, which is the steady-state when
    /// no concurrent mark cycle is in flight.
    pub satb_pre_write_barrier: usize,
    /// Uncommon trap handler: called from JIT code when a speculative
    /// optimization fails (wrong receiver type, unreached branch, etc.).
    /// Signature: extern "C" fn(vm_ptr: i64, reason: i64, bci: i64) -> i64
    /// Returns a deopt action code (0=reinterpret, 1=recompile, 2=blacklist).
    pub uncommon_trap: usize,
    /// T1.1.28 — `Math.fma(double, double, double)` fused multiply-add.
    /// Signature: `extern "C" fn(a: f64, b: f64, c: f64) -> f64`.
    /// Implemented via Rust's `f64::mul_add`, which emits `VFMADD231SD`
    /// when the target CPU supports FMA3 and otherwise performs a
    /// software-correct single-rounding fused operation.
    pub math_fma_double: usize,
    /// T1.1.28 — `Math.fma(float, float, float)`.
    /// Signature: `extern "C" fn(a: f32, b: f32, c: f32) -> f32`.
    pub math_fma_float: usize,

    // -----------------------------------------------------------------
    // Inline TLAB bump-pointer wiring (HIGH-6 JIT audit, object_allocation)
    //
    // These three `usize`s are NOT function pointers — they are byte
    // offsets the JIT bakes as immediates into the inline TLAB
    // fast-path emitted by `jit/src/x64.rs::new` (opcode 0xbb).
    //
    // The helper-table populator (`vm/src/jit/helpers.rs::build_helpers`)
    // computes them once at startup from `JvmThread::tlab_offset()` plus
    // `Tlab::CURSOR_OFFSET` / `Tlab::END_OFFSET`. The runtime asserts in
    // `Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`
    // guard the layout — change them in lockstep.
    // -----------------------------------------------------------------
    /// Byte offset of `Tlab::cursor` from `&JvmThread` (i.e. the full
    /// `JvmThread → tlab → cursor` chain). Emitted as `[thread+disp32]`
    /// in the inline bump fast path.
    pub tlab_cursor_offset_in_thread: usize,
    /// Byte offset of `Tlab::end` from `&JvmThread`.
    pub tlab_end_offset_in_thread: usize,
    /// Byte offset of `ObjectHeader.class_id` from the object base.
    /// Currently `0` (JIT contract enforced by `class_id_remains_at_offset_zero`
    /// in `types/src/heap_types.rs`), exposed here so the JIT does not
    /// hardcode the constant in a second place.
    pub class_id_offset_in_obj: usize,
    /// Address of the small `extern "C" fn() -> *mut JvmThread` helper
    /// that returns the current thread's `JvmThread` pointer via the
    /// `JIT_THREAD` thread-local. Used by the inline TLAB bump as the
    /// first step (one CALL is cheaper than a full `jit_new_object`
    /// dispatch). Set to `0` when not wired (JIT then falls back to the
    /// helper-call path).
    pub get_current_thread: usize,
    /// Address of `extern "C" fn(vm_ptr, obj_ptr, class_id, num_fields)
    /// -> i64`, the inline-TLAB completion helper. Called by the JIT
    /// after a successful inline bump-pointer to finish the object
    /// header (kind/hash/num_slots), apply primitive-typed defaults
    /// from class metadata, and register finalizable classes. Returns
    /// the object pointer unchanged.
    pub tlab_post_init: usize,
    /// Stage 3 (precise oop maps) — address of
    /// `extern "C" fn(rbp: usize)`, called once in the JIT prologue when
    /// `CRATONVM_PRECISE_JIT_MAPS` is on to register this frame's EXACT
    /// RBP with the GC root walker (the Rust-side `JitEntryGuard` only
    /// captures an approximate SP). `0` = not wired → the prologue skips
    /// the call and the walker uses the conservative path. Optional.
    pub frame_record: usize,
    /// Shadow-stack precise roots (`CRATONVM_SHADOW_STACK`) — byte offset of
    /// the `ShadowStack` field from `&JvmThread`. The JIT emits the inline
    /// push as `[thread + shadow_stack_offset_in_thread + ShadowStack::TOP_OFFSET]`.
    /// `0` when the mechanism is off → no shadow codegen is emitted.
    pub shadow_stack_offset_in_thread: usize,
    /// RBC.6 (athrow codegen) — `extern "C" fn(exc_ptr: i64) -> i64`.
    /// Stashes the thrown exception object as the pending JIT exception
    /// (or sets the pending-NPE flag when `exc_ptr == 0`, per JVMS athrow-
    /// on-null semantics) and returns the `i64::MIN` deopt sentinel. The
    /// 0xbf codegen arm calls this and immediately runs the method
    /// epilogue; the interpreter's JIT-return drains surface the
    /// exception. Appended at the END of the struct so all prior golden
    /// offsets stay stable.
    pub throw_exception: usize,
    /// JEP 358 (helpful NPE), inline-codegen path — `extern "C" fn(code: i64)`.
    /// Called by the per-action inline null-check failure stubs emitted in
    /// `jit/src/x64.rs::emit_null_check_store_stubs`. Sets the pending-NPE flag
    /// *with* the JEP-358 [`npe_action`] code passed in `code` (so the
    /// interpreter's post-JIT NPE drain attaches the right action-only message
    /// — "Cannot load from int array", …, gated behind
    /// `-XX:+ShowCodeDetailsInExceptionMessages`) AND the out-of-band deopt
    /// signal (each stub loads `i64::MIN` as the method return value). This
    /// replaces the prior single shared stub that called `bastore(0)` and
    /// therefore fabricated the byte-store action for every array opcode.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub jit_npe_with_action: usize,
    /// `i64::MIN`-sentinel disambiguation for `J`/`D` (long/double) call returns
    /// — `extern "C" fn() -> i64`.
    ///
    /// The dispatch helpers (`invoke_dispatch` / `invoke_virtual_mic`) signal a
    /// callee exception/deopt to a compiled caller by returning the `i64::MIN`
    /// sentinel in RAX. For an `int`/ref/void return that is unambiguous
    /// (`i64::MIN` is not a valid sign-extended `int`, oop, or void result), so
    /// the post-invoke check is a plain `CMP RAX, i64::MIN; JE bail`. But a
    /// callee that *legitimately* returns `Long.MIN_VALUE` (a `J`/`D` whose bits
    /// equal `i64::MIN`) returns the SAME value WITHOUT any pending-signal flag —
    /// so a `J`/`D` call site cannot tell the two apart from RAX alone.
    ///
    /// At a `J`/`D` call site the backend therefore emits, only on the (rare)
    /// `RAX == i64::MIN` branch, a `CALL` to this helper, which PEEKS (does not
    /// clear) every out-of-band signal — pending exception / NPE / AIOOBE /
    /// deopt flag, and a stashed IR-deopt frame — and returns `1` iff a genuine
    /// exception/deopt is pending (caller bails), `0` iff the `i64::MIN` is a
    /// real return value (caller keeps it). The common (`RAX != i64::MIN`) path
    /// is untouched, so int/ref/void call sites stay byte-identical.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub dispatch_threw: usize,
    /// IR FP tier (Slice A) — `frem` runtime helper, `extern "C" fn(f32, f32)
    /// -> f32`. JVM `frem` is the `fmod`-style truncated remainder (sign of the
    /// dividend, with the matching NaN/∞ rules) which has no single SSE
    /// instruction; the IR `Op::Rem` Float arm loads the two operands into
    /// XMM0/XMM1 and `CALL`s this helper (result in XMM0). The float ABI passes
    /// the two args in XMM0/XMM1 and returns in XMM0 on both Win64 and SysV,
    /// matching the IR's own XMM scratch convention exactly, so no register
    /// shuffling is needed. Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub jit_frem: usize,
    /// IR FP tier (Slice A) — `drem` runtime helper, `extern "C" fn(f64, f64)
    /// -> f64`. The double analogue of [`Self::jit_frem`]; the IR `Op::Rem`
    /// Double arm `CALL`s it with the operands in XMM0/XMM1 and reads the
    /// remainder from XMM0. Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub jit_drem: usize,
    /// Native-stack headroom guard for direct self-recursive calls —
    /// `extern "C" fn(vm_ptr: i64) -> i64`.
    ///
    /// Historically every non-tail self-recursive `invokestatic` was routed
    /// through `invoke_dispatch` solely so its thread-local depth guard could
    /// convert runaway compiled recursion into a catchable
    /// `StackOverflowError` (BUG-1). That made every recursive call pay the
    /// full dispatch-helper round trip (TLS scan-cache invalidation, SATB
    /// flush, dispatch-cache lookup, …) — ~10-30x the cost of the `CALL`
    /// itself, the dominant term in recursive workloads (fib, binarytrees).
    ///
    /// With this helper wired, the backend instead emits a direct rel32
    /// self-`CALL` preceded by one `CALL` to this guard. The guard compares
    /// the current native stack pointer against a per-thread floor (queried
    /// from the OS once per thread and cached): comfortably above the floor
    /// it returns `0` and the site proceeds to the direct self-call; at
    /// exhaustion it stashes a catchable `java/lang/StackOverflowError`
    /// (exactly like the dispatch depth guard) and returns the `i64::MIN`
    /// deopt sentinel, which the site routes through the existing post-invoke
    /// sentinel check. `0` = not wired → the jit crate keeps routing self
    /// calls through `invoke_dispatch` (historical behaviour). Appended at
    /// the END of the struct so all prior golden offsets stay stable.
    pub self_call_stack_guard: usize,
    /// Guarded inline `getfield` — address of the GC's process-global
    /// `JIT_REGION_BOUNDS` table (`[yf_base, yf_end, yt_base, yt_end,
    /// og_base, og_end]`, six consecutive `AtomicUsize` words), NOT a
    /// function pointer.
    ///
    /// When non-zero, the single-pass backend's default `getfield` arm emits
    /// an inline receiver guard — null/alignment bit-tests plus the same
    /// three-region `[base, end)` containment check `jit_getfield`'s
    /// `is_object_address` gate performs — and on success reads the field
    /// cell directly (a receiver inside a published region points at arena
    /// memory that stays mapped, so the raw load cannot fault). Guard
    /// failures branch to the checked `jit_getfield` helper, preserving its
    /// NPE/sentinel semantics for null and implausible receivers. `0` = not
    /// wired (G1/ZGC backends, or before the first publish) → every getfield
    /// keeps the checked-helper path. Appended at the END of the struct so
    /// all prior golden offsets stay stable.
    pub region_bounds_addr: usize,
    /// Inline self-recursion guard — address of the leaf
    /// `jit_native_stack_floor` helper (`extern "C" fn() -> i64`), which
    /// returns the current OS thread's native-stack floor (get-or-compute of
    /// the same TLS value `self_call_stack_guard` uses; touches no VM state,
    /// never GCs).
    ///
    /// When wired, a method with direct self-recursive call sites reserves a
    /// frame slot, calls this once in its prologue, and each self-call site
    /// emits `CMP RSP, [rbp - floor_slot]; JA <skip guard>` — replacing the
    /// per-recursion-level `self_call_stack_guard` helper CALL (plus its
    /// safepoint spill / shadow push+reload bracketing) on the common path.
    /// The helper CALL remains verbatim as the fallback and still owns the
    /// catchable StackOverflowError raise. OSR trampolines initialise the
    /// slot to `usize::MAX` (`RSP > MAX` is unsatisfiable), so OSR-entered
    /// frames always take the helper. `0` = not wired → sites keep the
    /// unconditional helper CALL. Appended at the END of the struct so all
    /// prior golden offsets stay stable.
    pub native_stack_floor_fn: usize,
    /// Materializes an interned Java String for a compiled `ldc` site.
    /// Signature: `extern "C" fn(vm_ptr, utf8_ptr, utf8_len) -> i64`.
    /// The helper performs the string-pool lookup on every execution so the
    /// returned reference remains valid after a relocating collection; JIT code
    /// must never bake a managed-object address as an immediate.
    pub ldc_string: usize,
    /// Cooperative JIT safepoint polling (`CRATONVM_JIT_SAFEPOINT_POLLS`, off
    /// by default) — address of the single STW-requested flag byte
    /// (`GcBarrier::stw_requested`, see
    /// `vm/src/threading/gc_barrier.rs::stw_requested_flag_addr`), NOT a
    /// function pointer. The JIT backend bakes this address as an absolute
    /// immediate (`MOV R11, imm64`) and emits `TEST byte ptr [R11], 0xFF; JNZ
    /// slow` at method entry (context methods only) and on every `goto`
    /// loop back-edge when the flag is wired. `0` = polling disabled
    /// entirely — the backend emits no poll code and GC continues to rely
    /// solely on the `SuspendThread`-based conservative scan
    /// (`vm/src/jit/xt_root_scan.rs`) for threads running JIT code. The
    /// `GcBarrier` this points at lives inside `Arc<SharedVm>`, so the
    /// address is stable for the life of the VM (see the stability contract
    /// on `stw_requested_flag_addr`). Appended at the END of the struct so
    /// all prior golden offsets stay stable.
    pub safepoint_flag_addr: usize,
    /// Cooperative JIT safepoint polling slow path — address of
    /// `extern "C" fn(vm_ptr: i64)`
    /// (`vm/src/jit/helpers.rs::jit_safepoint_slow_path`). Called only on a
    /// poll hit (the inline flag-byte check observed a nonzero value):
    /// the poll site first performs the existing pre-safepoint register
    /// spill (`x64.rs::emit_pre_safepoint_spill`) so the frame's oop map is
    /// valid, THEN calls this helper, which joins the same stop-the-world
    /// wait the interpreter's own poll hit uses
    /// (`vm/src/runtime/interpreter.rs::safepoint_check`) so GC observes
    /// this thread's roots exactly like an interpreter frame.
    /// `vm/src/jit/helpers.rs::build_helpers` wires this unconditionally
    /// (the function always exists); [`Self::safepoint_flag_addr`] is what
    /// actually gates whether the JIT ever emits a `CALL` to it, so this
    /// field being non-zero while that one is `0` is harmless (dead code,
    /// never reached). Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub safepoint_slow_path: usize,
    /// Stable address of the generational collector's atomic card-byte array.
    ///
    /// Together with `jit_card_old_base/end`, this enables the x64 backend to
    /// emit the post-store card mark inline. All three fields are zero for G1
    /// and ZGC, whose remembered-set protocols remain helper-owned.
    pub jit_card_table_addr: usize,
    /// Inclusive old-generation base covered by `jit_card_table_addr`.
    pub jit_card_old_base: usize,
    /// Exclusive old-generation end covered by `jit_card_table_addr`.
    pub jit_card_old_end: usize,
    /// `extern "C" fn(bci: i64)` — stamp the *currently executing compiled
    /// method's own* throw-site bci onto the pending-exception signal.
    ///
    /// Emitted on the cold side of every post-invoke exception check
    /// (`jit/src/x64.rs::emit_exception_check_stubs`). Without it the signal
    /// still carries the bci that the *callee's* compiled `athrow` lowering
    /// stashed, and the interpreter range-checks that foreign pc against THIS
    /// method's exception table — silently dropping a `finally` (a catch-all
    /// entry cannot be rescued by the type check the way a typed handler can).
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub set_throw_bci: usize,
    /// Service a compiled callee's `i64::MIN` deopt/exception sentinel at an
    /// INLINE (generated) call site.
    ///
    /// The MIC/PIC cascade calls a cached compiled entry directly, so no
    /// dispatch helper is on the stack to notice that the callee trapped. The
    /// callee's reconstructed frame would then sit in the thread's single stash
    /// slot until some unrelated sink consumed it, de-speculating the wrong
    /// method and failing to resume. Called ONLY on the rare
    /// `RAX == i64::MIN` branch after an inline call.
    ///
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    ///
    /// Signature: `extern "C" fn(vm_ptr: i64, info_ptr: i64, args_ptr: i64,
    /// num_args: i64) -> i64` — returns the resumed call result, or `i64::MIN`
    /// unchanged when the sentinel must keep propagating.
    pub service_callee_deopt: usize,
    /// Constant-pool-indexed `new` (0xbb) slow path — `extern "C"
    /// fn(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64`.
    ///
    /// The ordinary [`Self::new_object`] path takes an already-resolved
    /// `(class_id, num_fields)` pair, which the compiler can only supply when
    /// the `new`'s target class is ALREADY LOADED. A hot method whose only
    /// un-taken branch does `throw new SomeException(...)` therefore failed to
    /// compile at all — `resolve_jit_new_site` returned `None` and the whole
    /// compile bailed, permanently after `MAX_TIER_FAIL_RETRIES`
    /// (jit-compile-bail-unresolved-new-cold-class.md).
    ///
    /// This helper moves resolution to run time: the compiler bakes the
    /// *referencing* class id and the CP index, and the helper resolves +
    /// initialises the target exactly like the interpreter's 0xbb handler
    /// (same thread, same program point) before falling into
    /// `jit_new_object`'s body. Sound because it is what the interpreter
    /// already does; free on the hot path because a `new` whose class IS
    /// loaded at compile time still takes the inline-TLAB/`new_object` path.
    ///
    /// `0` = not wired (hand-built test tables) → the backend refuses the
    /// deferred site and bails the compile, i.e. the pre-fix behaviour.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub new_object_cp: usize,
    /// Constant-pool-indexed `anewarray` (0xbd) slow path — `extern "C"
    /// fn(vm_ptr: i64, holder_class_id: i64, cp_idx: i64, length: i64) -> i64`.
    ///
    /// The `anewarray` sibling of [`Self::new_object_cp`]: same root cause
    /// (the component class is not loaded yet, so the compile-time resolver
    /// cannot name a class id), same runtime-resolution answer. `0` = not
    /// wired → the deferred site bails the compile. Appended at the END of
    /// the struct so all prior golden offsets stay stable.
    pub anewarray_object_cp: usize,
    /// `monitorenter` for compiled code. Returns the possibly-REMAPPED
    /// object, which the caller must store back: a contended acquire parks
    /// the thread and the object can move while it is parked.
    ///
    /// Optional - a table leaving this 0 makes `ir_lower` refuse a graph
    /// containing monitor ops rather than emit nothing and drop the lock.
    pub monitor_enter: usize,
    /// `monitorexit`. See `monitor_enter`.
    pub monitor_exit: usize,
    /// Constant-pool-indexed `ldc <Class>` (0x12/0x13 whose CP entry is a
    /// `CONSTANT_Class`) — `extern "C" fn(vm_ptr: i64, holder_class_id: i64,
    /// cp_idx: i64) -> i64`. Returns the target class's mirror `ObjectRef`,
    /// or `0` after publishing a pending exception.
    ///
    /// CP-indexed rather than class-id-indexed for the same reason
    /// [`Self::new_object_cp`] is: resolving the target can run a user
    /// `ClassLoader.loadClass`, which must not happen inside the compiler, so
    /// the *referencing* class id and the CP index are baked and resolution
    /// happens at run time on the executing thread.
    ///
    /// Re-consulted on every execution, like [`Self::ldc_string`] and unlike
    /// a baked immediate: a mirror is a heap object that a moving collector
    /// can relocate between two invocations of the same compiled body.
    ///
    /// `0` = not wired (hand-built test tables) → the backend refuses a
    /// class-`ldc` site and bails the compile, which is the pre-fix
    /// behaviour. Appended at the END of the struct so all prior golden
    /// offsets stay stable.
    pub ldc_class_cp: usize,

    /// `aastore` element-type check — the JVMS §6.5 *aastore* covariance rule
    /// and NOTHING else: no null check, no bounds check, no barrier, no store.
    ///
    /// `extern "C" fn(vm_ptr: i64, array_ptr: i64, val: i64) -> i64`. Returns
    /// `0` when the store is legal and the `i64::MIN` deopt sentinel when it is
    /// not, having stashed a real `ArrayStoreException` through the JIT_THREAD
    /// TLS. The sentinel is a *defined* return value on purpose: a `-> ()`
    /// helper leaves RAX undefined, so an `emit_post_invoke_exception_check`
    /// after it would be testing garbage.
    ///
    /// The x64 emitter lowers `aastore` inline (null check, bounds check, SATB
    /// pre-write barrier, store, card mark) and so never reaches
    /// [`Self::aastore`]; this is the one piece of that helper the inline path
    /// cannot do for itself, because the answer needs the class manager. On a
    /// refusal the caller must skip the store, the SATB pre-write barrier and
    /// the card mark.
    ///
    /// Required, not optional: a `0` slot would leave the inline lowering
    /// storing with no check, which is the heap-type-confusion defect
    /// (`Object[] a = new String[1]; a[0] = anInteger;` leaving an `Integer`
    /// inside a `String[]`) this slot exists to close. Appended at the END of
    /// the struct so all prior golden offsets stay stable.
    pub aastore_type_check: usize,

    /// Guarded inline `getfield` READ side — address of the GC's process-global
    /// `JIT_READ_BOUNDS` table (`gc/src/gen_heap.rs`), six consecutive
    /// `AtomicUsize` words in the same `[b0, e0, b1, e1, b2, e2]` layout as
    /// [`Self::region_bounds_addr`], so the emitted containment sequence is
    /// byte-identical and only the baked address differs.
    ///
    /// **Why a second table rather than reusing the first.**
    /// [`Self::region_bounds_addr`] is doing two jobs whose answers diverge.
    /// Its documented job is "is this address mapped, so a raw load cannot
    /// fault" — a READ question. Its load-bearing job since G1-2
    /// (`audits/g1-audit.md` §8.1) is "may an inline reference STORE skip the
    /// collector's write barrier", and G1/ZGC answer that by leaving the table
    /// EMPTY: a JNI-pinned, CSet-excluded G1 region is reachable only through
    /// its remembered set, so an inline store that skips
    /// `post_write_barrier_rset` loses that edge. One table, two questions,
    /// opposite answers — filling it to fix loads would silently unblock the
    /// stores it exists to block.
    ///
    /// **Who publishes.** `GenerationalHeap` (its three arenas, refreshed at
    /// every GC start/end) and `G1Collector` (its single contiguous `Box`
    /// arena, `[arena_base, arena_end)`, published once at construction and
    /// cleared on drop — G1's N regions are carved from ONE allocation, so
    /// three slots are more than the one it needs). **ZGC deliberately does
    /// NOT publish**, which is what keeps this table from landing ahead of the
    /// ZGC JIT load barrier: a compact reference slot there holds
    /// `Z_COLORED_TAG | colour | offset`, not a pointer, and inlining its load
    /// is the use-after-free `feature-designs/zgc-jit-load-barrier.md` exists
    /// to stop. Not publishing is a strictly stronger discharge of that
    /// obligation than a per-field-kind gate would be, and it costs G1 nothing:
    /// the measured containment-failure split there is 100% reference / 0%
    /// primitive.
    ///
    /// `0` = not wired (hand-built test tables) → the READ path keeps the
    /// checked `jit_getfield` helper, which is the pre-fix behaviour. Appended
    /// at the END of the struct so all prior golden offsets stay stable.
    pub read_bounds_addr: usize,
    /// Compiled local exception handlers — address of
    /// `extern "C" fn(vm_ptr: i64, site_ptr: i64, out_exc: *mut i64) -> i64`
    /// (`vm/src/jit/helpers.rs::jit_local_handler_lookup`).
    ///
    /// Called from a per-throwing-bci stub the instant a fallible site returns
    /// the `i64::MIN` sentinel. It answers which of THIS method's own
    /// exception-table entries takes the pending throwable — index into the
    /// site's compile-time candidate list, or `-1` for "this frame does not
    /// catch it" — and on a hit stores the throwable into the frame slot the
    /// handler's operand stack starts at.
    ///
    /// `0` = not wired (hand-built test tables, or the feature switched off)
    /// → the backend arms no local-handler stubs at all and every caught
    /// exception takes the reason-9 deopt / shared-sentinel route out of
    /// compiled code, which is the behaviour that predates the feature.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub local_handler_lookup: usize,

    /// `ldc <String>` — the interned literal named at `cp_idx` in
    /// `holder_class_id`'s constant pool.
    ///
    /// `extern "C" fn(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64`.
    /// The CP-indexed twin of [`Self::ldc_class_cp`], and the SUPERSEDER of
    /// [`Self::ldc_string`], which bakes the literal's UTF-8 bytes instead and
    /// therefore has no key to answer from: JVMS §5.4.3 resolves a
    /// constant-pool entry once and records the result, and the record is
    /// keyed `(class, cp index)`. The bytes form re-derived the answer on every
    /// execution — the string pool's `RwLock`, a hash of the literal's whole
    /// content and a `memcmp` — measured at 18.4 ns against HotSpot's 0.2 ns
    /// (`probes/LdcConstCostProbe.java`).
    ///
    /// `0` = not wired (hand-built test tables) → the backend refuses a
    /// string-`ldc` site and bails the compile, exactly as an unwired
    /// [`Self::ldc_class_cp`] makes it refuse a class-`ldc` site. Appended at
    /// the END of the struct so all prior golden offsets stay stable.
    pub ldc_string_cp: usize,
    /// FFM element READ fast path — `MemorySegment.getAtIndex`.
    ///
    /// Signature:
    /// `extern "C" fn(seg: i64, index: i64, kind: i64, out: *mut i64) -> i64`,
    /// returning 1 when it handled the access (value written through `out`) and
    /// 0 to DECLINE, in which case the emitted code falls through to the
    /// unchanged native dispatch for the same site. Everything it declines
    /// therefore keeps today's behaviour, exceptions included.
    ///
    /// `kind` is a compile-time constant taken from the call site's descriptor,
    /// which names the `ValueLayout` subtype — see the `FFM_KIND_*` codes.
    ///
    /// These accessors are per-ELEMENT, and through the ordinary dispatch funnel
    /// they measure ~1158 ns/element against ~0.8 ns for a `short[]` element.
    ///
    /// `0` = not wired (hand-built test tables) → no FFM fast path is emitted
    /// and every site keeps its native dispatch. Appended at the END so all
    /// prior golden offsets stay stable.
    pub ffm_segment_get: usize,
    /// FFM element WRITE fast path — `MemorySegment.setAtIndex`. The twin of
    /// [`Self::ffm_segment_get`], same 1-handled / 0-declined contract.
    ///
    /// Signature:
    /// `extern "C" fn(seg: i64, index: i64, kind: i64, value: i64) -> i64`.
    /// `value` carries raw bits: the integral kinds in their low bytes, float
    /// and double as `to_bits()`.
    pub ffm_segment_set: usize,

    // ── Reference-store barrier plan ────────────────────────────────────
    //
    // Three addresses that let compiled code inline the collector's OWN
    // early-outs instead of paying a call to discover them.
    //
    // The whole design is one property: each word below names a PREFIX of the
    // helper's control flow, and compiled code reads the same word the helper
    // itself reads. The JIT never reimplements a barrier — when a gate says
    // "there may be work" it calls the same helper it calls today, so no
    // collector's remembered-set contract moves into the emitter. What the JIT
    // gains is the right to skip the CALL when a gate proves the helper would
    // have returned immediately.
    //
    // This is deliberately NOT `region_bounds_addr`'s question. That table
    // asks "is the receiver in a published young region", which is a
    // GENERATIONAL question G1 and ZGC do not answer — so under the default
    // collector its emptiness routed every reference store to the helper and
    // still emitted six containment compares that could never pass. See the
    // field docs on `region_bounds_addr` and `read_bounds_addr` for that
    // history.
    /// Address of a `u8` that is **zero exactly when the SATB pre-write
    /// barrier is a no-op for every old value**.
    ///
    /// For ZGC this is `ZgcRealHeap::mark_active`; `satb_pre_barrier`'s entire
    /// body on a non-marking run is a relaxed load of it and a return.
    ///
    /// **Why an inline test of it is sound and not a race.** The flag is only
    /// ever ARMED inside a stop-the-world pause (`start_concurrent_mark` takes
    /// a `StopTheWorldToken` and arms it at step 3), so no mutator can sit
    /// between this test and its store while the flag turns on: every mutator
    /// is parked, and observes the armed flag when it resumes. Disarming is the
    /// safe direction — a mutator that skips the barrier after the mark phase
    /// ended has nothing to contribute to a completed snapshot.
    ///
    /// `0` = the collector does not publish one ⇒ compiled code must keep the
    /// pre-barrier, i.e. route the store to the full helper. Appended at the
    /// END of the struct so all prior golden offsets stay stable.
    pub ref_store_pre_gate: usize,
    /// Address of a `u8` that is **zero exactly when the post-write barrier is
    /// a no-op for every `(receiver, value)` pair**.
    ///
    /// For ZGC this is `ZgcRealHeap::has_old_objects`: `note_ref_store` returns
    /// on it before doing anything else, because with no old object in the heap
    /// there is no old-to-young edge to remember.
    ///
    /// `0` = not published ⇒ compiled code must always run the post barrier
    /// (call `write_barrier`).
    pub ref_store_post_gate: usize,
    /// Address of a `u8` `F` such that a receiver whose `GC_FLAGS_BYTE_OFFSET`
    /// byte is **unsigned-less-than `F`** provably needs no post barrier.
    ///
    /// That byte holds `gc_age` in bits 4..7 and the GC flags in bits 0..3, so
    /// with `F = promotion_age << 4` one unsigned byte compare is an EXACT test
    /// of `gc_age < promotion_age`: the flags nibble is at most 15, which
    /// cannot carry `age << 4` up to `(age + 1) << 4`. That is precisely ZGC's
    /// `note_ref_store_slow` early-out — a store into a young object needs no
    /// card, because a young cycle traces every young object anyway.
    ///
    /// Published as an address rather than baked as an immediate because the
    /// promotion age is dynamic; compiled code re-reads it at every store.
    ///
    /// `0` = the collector cannot express its post-barrier condition this way
    /// (Generational keys on `GC_FLAG_OLD_GEN`, a mask test rather than a
    /// floor; G1 keys on region state the emitter cannot see) ⇒ compiled code
    /// skips this test and falls back to `ref_store_post_gate` alone.
    pub ref_store_post_young_floor: usize,
    /// F-08 — address of the GC's `JIT_G1_BARRIER` table (`gc/src/gen_heap.rs`),
    /// baked as an immediate by the inline G1 post-write barrier.
    ///
    /// NOT a function pointer, and NOT a fourth spelling of
    /// [`Self::region_bounds_addr`]. That table's EMPTINESS under G1 is what
    /// closes defect G1-2, and it must keep answering "no inline reference
    /// store may skip the collector's barrier" there; this one answers the
    /// different question of what geometry an inline barrier needs in order to
    /// BE the barrier — arena base, arena length, region mask, and the F-05
    /// card table's base and shift. See `gc/src/gen_heap.rs::JitG1BarrierTable`.
    ///
    /// `0`, or a table whose `arena_len` word is zero, = no G1 collector has
    /// published → the emitter emits no inline barrier and every compiled
    /// reference store keeps the `putfield_object` helper it takes today.
    /// Appended at the END of the struct so all prior golden offsets stay
    /// stable.
    pub g1_barrier_addr: usize,
    /// F-08 — G1's post-write barrier, called from the inline arm when its two
    /// inline filters (same region, null store) both fail to prove there is
    /// nothing to remember.
    ///
    /// `extern "C" fn(vm_ptr: i64, obj_ptr: i64, val_ptr: i64)`.
    ///
    /// Distinct from [`Self::write_barrier`], which routes through
    /// `VmHeap::write_barrier` and carries a `debug_assert!` requiring an SATB
    /// pre-barrier on the same thread when a mark cycle is active. That
    /// assertion is right for a general store and wrong for this caller: the
    /// inline arm stores only when the field's OLD value is NULL, which is
    /// exactly the case `satb_pre_barrier` returns from immediately, so no
    /// pre-barrier is fired and none is owed. A dedicated entry point says that
    /// once, here, instead of weakening an assertion that protects every other
    /// caller.
    ///
    /// `0` = not wired (hand-built test tables) → no inline G1 barrier is
    /// emitted. Appended at the END of the struct so all prior golden offsets
    /// stay stable.
    pub g1_post_write_barrier: usize,
}

/// Classifies each field of [`JitRuntimeHelpers`] for the validator.
///
/// Drives the `helper_fields!` macro below — the macro emits one
/// `FieldEntry` per struct field, and [`JitRuntimeHelpers::validate`]
/// branches on `kind` so the right contract is applied per slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// Mandatory `CALL` target. Zero (null) means a baked absolute call
    /// would fault — `validate()` rejects.
    RequiredPtr,
    /// Optional `CALL` target. Zero means "not wired, JIT falls back to
    /// the helper-call slow path"; non-zero must be a real entry point.
    /// `validate()` accepts both — non-zero is automatically non-null.
    OptionalPtr,
    /// Byte offset (immediate), not a pointer. Zero is a legitimate value
    /// (e.g. `class_id_offset_in_obj` is 0 by contract). `validate()`
    /// ignores it.
    Offset,
}

/// One macro-generated entry per field of [`JitRuntimeHelpers`].
///
/// Carrying `name`, `kind`, and `value` together lets [`JitRuntimeHelpers::null_pointers`]
/// and [`JitRuntimeHelpers::validate`] iterate the same single source of
/// truth — there is no longer a parallel `[usize; N]` / `[&str; N]` pair
/// that can drift.
#[derive(Clone, Copy, Debug)]
pub struct FieldEntry {
    pub name: &'static str,
    pub kind: FieldKind,
    pub value: usize,
}

// ---------------------------------------------------------------------
// Single source of truth for the `JitRuntimeHelpers` field list.
//
// `helper_fields!` expands to:
//   * `JitRuntimeHelpers::all_fields(&self) -> [FieldEntry; NUM_FIELDS]`
//   * `JitRuntimeHelpers::NUM_FIELDS: usize`
//
// To add a new field: add it to the `helper_fields!` invocation below
// and add the matching `pub <name>: usize,` to the struct. NUM_FIELDS
// is computed from the macro input — no separate constant to bump.
//
// `repr(C)` + every field being `usize` means the layout is sequential,
// so the golden-offset test in `mod tests` can validate the on-disk
// shape against documented byte offsets.
// ---------------------------------------------------------------------
macro_rules! helper_fields {
    ( $( ($name:ident, $kind:expr) ),* $(,)? ) => {
        impl JitRuntimeHelpers {
            /// Number of fields in the bulk-validation array.
            ///
            /// Computed by the `helper_fields!` macro from the field list
            /// — there is no separate hand-maintained constant to bump
            /// when a field is added. The `const _: () = assert!(...)`
            /// below additionally ties this to
            /// `size_of::<JitRuntimeHelpers>() / size_of::<usize>()`, so
            /// adding a field to the *struct* without also adding it to
            /// the macro invocation is a compile-time error.
            pub const NUM_FIELDS: usize = [ $( helper_fields!(@unit $name) ),* ].len();

            /// All fields, tagged with classification, for bulk validation.
            ///
            /// Macro-generated from the `helper_fields!` invocation so the
            /// field list lives in exactly one place. Length is pinned by
            /// the return type, kind is per-entry, and `null_pointers`
            /// and `validate` both consume this array — the previous
            /// drift hazard between `all_pointers` and `field_names` is
            /// gone.
            fn all_fields(&self) -> [FieldEntry; Self::NUM_FIELDS] {
                [
                    $(
                        FieldEntry {
                            name: stringify!($name),
                            kind: $kind,
                            value: self.$name,
                        },
                    )*
                ]
            }
        }
    };
    (@unit $name:ident) => { () };
}

helper_fields! {
    (newarray,                       FieldKind::RequiredPtr),
    (new_object,                     FieldKind::RequiredPtr),
    (anewarray_object,               FieldKind::RequiredPtr),
    (baload,                         FieldKind::RequiredPtr),
    (bastore,                        FieldKind::RequiredPtr),
    (iaload,                         FieldKind::RequiredPtr),
    (iastore,                        FieldKind::RequiredPtr),
    (aaload,                         FieldKind::RequiredPtr),
    (aastore,                        FieldKind::RequiredPtr),
    (multianewarray_2d,              FieldKind::RequiredPtr),
    (arraylength,                    FieldKind::RequiredPtr),
    (getfield,                       FieldKind::RequiredPtr),
    (putfield_int,                   FieldKind::RequiredPtr),
    (putfield_long,                  FieldKind::RequiredPtr),
    (putfield_float,                 FieldKind::RequiredPtr),
    (putfield_double,                FieldKind::RequiredPtr),
    (putfield_object,                FieldKind::RequiredPtr),
    (getstatic,                      FieldKind::RequiredPtr),
    (putstatic_int,                  FieldKind::RequiredPtr),
    (putstatic_long,                 FieldKind::RequiredPtr),
    (putstatic_float,                FieldKind::RequiredPtr),
    (putstatic_double,               FieldKind::RequiredPtr),
    (putstatic_object,               FieldKind::RequiredPtr),
    (checkcast,                      FieldKind::RequiredPtr),
    (instanceof_check,               FieldKind::RequiredPtr),
    (throw_aioobe,                   FieldKind::RequiredPtr),
    (throw_arithmetic,               FieldKind::RequiredPtr),
    (invoke_dispatch,                FieldKind::RequiredPtr),
    (invoke_virtual_mic,             FieldKind::RequiredPtr),
    (lambda_int_to_double,           FieldKind::RequiredPtr),
    (write_barrier,                  FieldKind::RequiredPtr),
    (satb_pre_write_barrier,         FieldKind::RequiredPtr),
    (uncommon_trap,                  FieldKind::RequiredPtr),
    (math_fma_double,                FieldKind::RequiredPtr),
    (math_fma_float,                 FieldKind::RequiredPtr),
    (tlab_cursor_offset_in_thread,   FieldKind::Offset),
    (tlab_end_offset_in_thread,      FieldKind::Offset),
    (class_id_offset_in_obj,         FieldKind::Offset),
    (get_current_thread,             FieldKind::OptionalPtr),
    (tlab_post_init,                 FieldKind::OptionalPtr),
    (frame_record,                   FieldKind::OptionalPtr),
    (shadow_stack_offset_in_thread,  FieldKind::Offset),
    (throw_exception,                FieldKind::RequiredPtr),
    (jit_npe_with_action,            FieldKind::RequiredPtr),
    (dispatch_threw,                 FieldKind::RequiredPtr),
    (jit_frem,                       FieldKind::RequiredPtr),
    (jit_drem,                       FieldKind::RequiredPtr),
    (self_call_stack_guard,          FieldKind::OptionalPtr),
    // NOT a pointer: address of the GC's JIT_REGION_BOUNDS table, baked as an
    // immediate by the guarded inline getfield. 0 = not wired (helper-only).
    (region_bounds_addr,             FieldKind::Offset),
    // Leaf floor-query helper for the inline self-recursion check.
    (native_stack_floor_fn,          FieldKind::OptionalPtr),
    (ldc_string,                     FieldKind::RequiredPtr),
    // Cooperative JIT safepoint polling (CRATONVM_JIT_SAFEPOINT_POLLS,
    // off by default). Address of the STW-requested flag byte, not a
    // pointer — 0 = polling disabled (matches the region_bounds_addr
    // "optional address" convention above).
    (safepoint_flag_addr,            FieldKind::Offset),
    // Slow-path helper called on a poll hit. build_helpers wires this
    // unconditionally; safepoint_flag_addr above is what actually gates
    // whether the JIT ever emits a CALL to it, so 0 here is only ever
    // "not wired" for a hand-built test helpers table.
    (safepoint_slow_path,            FieldKind::OptionalPtr),
    // Optional generational inline-card metadata. These are data addresses /
    // bounds rather than callable targets and are all zero for G1/ZGC.
    (jit_card_table_addr,             FieldKind::Offset),
    (jit_card_old_base,               FieldKind::Offset),
    (jit_card_old_end,                FieldKind::Offset),
    (set_throw_bci,                  FieldKind::RequiredPtr),
    // Optional: a hand-built test helpers table leaves it 0, and the codegen
    // then emits no sentinel check — the pre-existing behaviour.
    (service_callee_deopt,           FieldKind::OptionalPtr),
    // Optional: a hand-built test helpers table leaves these 0, and the
    // backend then refuses a deferred (not-yet-loaded) `new`/`anewarray`
    // site and bails the compile — the pre-existing behaviour.
    (new_object_cp,                  FieldKind::OptionalPtr),
    (anewarray_object_cp,            FieldKind::OptionalPtr),
    // Optional: 0 makes `ir_lower` refuse a graph containing monitor ops.
    (monitor_enter,                  FieldKind::OptionalPtr),
    (monitor_exit,                   FieldKind::OptionalPtr),
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site
    // and bail the compile — the pre-fix behaviour.
    (ldc_class_cp,                   FieldKind::OptionalPtr),
    // Required: the `0x53` lowering is inline and calls this for the JVMS §6.5
    // covariance check; 0 would mean a reference store with no check at all.
    (aastore_type_check,             FieldKind::RequiredPtr),
    // NOT a pointer: address of the GC's JIT_READ_BOUNDS table, baked as an
    // immediate by the guarded inline getfield READ path. Deliberately a
    // DIFFERENT table from region_bounds_addr above -- see the field doc.
    (read_bounds_addr,               FieldKind::Offset),
    // Optional: 0 makes the single-pass backend arm no local-handler stubs, so
    // every caught exception keeps leaving compiled code — the pre-feature
    // behaviour.
    (local_handler_lookup,           FieldKind::OptionalPtr),
    // Optional: 0 makes both backends refuse a string-`ldc` site and bail the
    // compile, the same way an unwired `ldc_class_cp` does for `ldc <Class>`.
    (ldc_string_cp,                  FieldKind::OptionalPtr),
    (ffm_segment_get,                FieldKind::OptionalPtr),
    (ffm_segment_set,                FieldKind::OptionalPtr),
    // NOT pointers-to-code: addresses of collector-owned gate BYTES. `Offset`
    // (validated as "may be 0") rather than a required pointer, because 0 is
    // the meaningful value "this collector publishes no plan" — every emitter
    // arm then keeps the full-helper path it has today. The dangerous
    // direction is a WRONG non-zero, which would elide a barrier; that is the
    // publisher's obligation, and it writes the address of a real `'static`
    // gate byte or nothing at all.
    (ref_store_pre_gate,             FieldKind::Offset),
    (ref_store_post_gate,            FieldKind::Offset),
    (ref_store_post_young_floor,     FieldKind::Offset),
    // NOT a pointer: address of the GC's JIT_G1_BARRIER table, baked as an
    // immediate by the inline G1 post-write barrier. 0 = not wired.
    (g1_barrier_addr,                FieldKind::Offset),
    (g1_post_write_barrier,          FieldKind::OptionalPtr),
}

// Compile-time integrity check: the macro-generated NUM_FIELDS must
// match `sizeof(JitRuntimeHelpers) / sizeof(usize)`. Every field of
// `JitRuntimeHelpers` is a `usize` and the struct is `#[repr(C)]` with
// no padding, so this ratio equals the field count. If a future
// contributor adds `pub foo: usize` to the struct without adding `foo`
// to the `helper_fields!` invocation, this assert fails — the silent
// "field omitted from validation" failure mode is closed at build time.
//
// Uses a `const _:` item rather than an inline `const { ... }` block to
// stay compatible with Rust 1.77 (inline const expressions stabilised
// in 1.79; the workspace MSRV is below that).
const _: () = assert!(
    JitRuntimeHelpers::NUM_FIELDS
        == core::mem::size_of::<JitRuntimeHelpers>() / core::mem::size_of::<usize>(),
    "JitRuntimeHelpers::NUM_FIELDS drifted from struct field count — add the new field \
     to the helper_fields! invocation in jit-api/src/lib.rs",
);

// Belt-and-suspenders: pin the exact expected count so a *removal* of a
// field also requires touching this line. Without this, deleting a
// struct field AND its macro entry simultaneously would still satisfy
// the ratio assert above and silently change the JIT ABI.
const _: () = assert!(
    JitRuntimeHelpers::NUM_FIELDS == 74,
    "JitRuntimeHelpers field count changed — bump the literal here and update \
     the golden-offset test in mod tests if the change is intentional",
);

// Compile-time enforcement of the golden ABI's per-field stride. The JIT
// targets x86-64 exclusively and the documented helper offsets are computed
// as `index * 8`, assuming an 8-byte `usize`. The `jit_runtime_helpers_field_size_is_eight`
// test validates this, but a #[test] only runs when tests are compiled and
// executed — a hypothetical 32-bit build would compile cleanly and silently
// emit wrong offsets (4-byte stride). Pinning the size here turns that into
// a build failure on any non-8-byte-usize target instead of a miscompile.
const _: () = assert!(
    core::mem::size_of::<usize>() == 8,
    "JIT golden ABI requires an 8-byte usize (x86-64); offsets are computed as \
     index * 8 and would be wrong on a non-64-bit target",
);

impl JitRuntimeHelpers {
    /// Validate that every mandatory helper pointer is non-null.
    ///
    /// A null (zero) function pointer is invalid — there is no function at
    /// address 0, so an absolute `CALL` baked from it would fault. Non-null
    /// is the *only* sound validation we can apply: function entry points
    /// have no guaranteed alignment. On x86-64 a function may legitimately
    /// start at an odd address, so a parity/alignment check would
    /// false-positive on a perfectly valid helper.
    ///
    /// Iteration is over [`Self::all_fields`], the macro-generated list,
    /// so this method automatically covers every field of the struct.
    /// Per-field semantics:
    ///
    /// * [`FieldKind::RequiredPtr`] — must be non-zero. Returns `Err`
    ///   listing the offending field names otherwise.
    /// * [`FieldKind::OptionalPtr`] — zero means "not wired" and is OK;
    ///   any non-zero value is automatically non-null (so no additional
    ///   check is needed, but the slot is still covered by the iteration
    ///   — future tightening can add e.g. address-range sanity here).
    /// * [`FieldKind::Offset`] — byte offset, not a pointer. Zero is a
    ///   legitimate value (`class_id_offset_in_obj` is 0 by contract).
    ///   Skipped.
    ///
    /// Returns `Ok(())` when every required pointer is non-null. Returns
    /// `Err(Vec<&'static str>)` with the names of the null required
    /// pointers — useful for surfacing a list of misses in a single
    /// error message rather than failing on the first one. (Round-9
    /// fix: the previous bool-returning, dead-loop implementation
    /// silently returned `true` on a null `tlab_post_init` because the
    /// hand-maintained bulk array did not include it. The validator now
    /// iterates the macro-generated `all_fields()` list — all 46 fields,
    /// 40 of which are required pointers — so no field can be silently
    /// uncovered.)
    pub fn validate(&self) -> Result<(), Vec<&'static str>> {
        let nulls = self.null_pointers();
        if nulls.is_empty() {
            Ok(())
        } else {
            Err(nulls)
        }
    }

    /// Return the names of mandatory pointer fields whose value is null.
    ///
    /// Only [`FieldKind::RequiredPtr`] fields are reported — an unset
    /// optional helper is *expected* to be zero (and the offset fields
    /// can legitimately be zero), so neither would be a bug.
    pub fn null_pointers(&self) -> Vec<&'static str> {
        self.all_fields()
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr && e.value == 0)
            .map(|e| e.name)
            .collect()
    }
}

// AUDIT 2026-05-16: JitRuntimeHelpersBuilder has been deleted. It was
// unused — the real construction site is `vm/src/jit/helpers.rs` (struct-
// literal init) and the only callers of the Builder were this crate's own
// tests. The stringly-typed `set(name: &str, addr: usize)` silently
// `eprintln!`-degraded on typos, providing no compile-time safety while
// duplicating the field list across five call sites. If a future
// caller wants a builder pattern, use the struct literal directly or
// generate it from a declarative macro keyed on `field_names()`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_cached_method() -> CachedBytecodeMethod {
        CachedBytecodeMethod {
            declaring_class_id: ClassId::new(1),
            class_name: Arc::from("java/lang/Object"),
            method_name: Arc::from("hashCode"),
            method_descriptor: Arc::from("()I"),
            source_file: Some(Arc::from("Object.java")),
            code: Arc::from(vec![0xB1u8].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        }
    }

    fn noop_native(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[cratonvm_types::Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }

    /// Step 0 of `arch-2026-07-26/native-dispatch-memoization.md`
    /// §3: the per-entry native-dispatch memo must be substitutable for
    /// `registry.find(class, method, descriptor)` — on a miss, on a hit, and
    /// warm.
    #[test]
    fn native_call_site_is_substitutable_for_find() {
        let cached = make_cached_method();
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();

        // Miss agrees with `find`.
        assert!(registry
            .find(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor
            )
            .is_none());
        assert!(cached
            .native_call_site()
            .callback(
                &registry,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .is_none());

        registry.register(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            noop_native,
        );

        let via_find = registry
            .find(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .expect("registered native must be findable");
        let via_cell = cached
            .native_call_site()
            .callback(
                &registry,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .expect("the cell must resolve what `find` resolves");
        assert_eq!(via_find as usize, via_cell as usize);

        // Warm read agrees with itself.
        let warm = cached
            .native_call_site()
            .callback(
                &registry,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .expect("warm read");
        assert_eq!(warm as usize, via_find as usize);
    }

    /// THE LATENT BUG THIS FIELD CHANGE FIXES. The old
    /// `OnceLock<Option<NativeCallback>>` memoized a *negative* permanently, so
    /// a native registered by `alias_class` or a lazy `register_*` pass — both
    /// of which run after the first bytecode executes — was invisible at this
    /// call site for the rest of the process, while `find` kept resolving it.
    /// The generation-keyed cell must self-heal.
    #[test]
    fn native_call_site_heals_a_negative_after_late_registration() {
        let cached = make_cached_method();
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();

        // Warm the cell with a negative, twice, so a `OnceLock`-shaped memo
        // would definitely be sealed.
        for _ in 0..2 {
            assert!(cached
                .native_call_site()
                .callback(
                    &registry,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                )
                .is_none());
        }

        registry.register(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            noop_native,
        );

        assert!(
            cached
                .native_call_site()
                .callback(
                    &registry,
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                )
                .is_some(),
            "a native registered after the call site first ran must become \
             visible once the registry generation moves"
        );
    }

    /// The cell is reached through `&self`, so the same `Arc`-shared entry that
    /// several call sites hold must hand back one stable cell — and `Clone`
    /// must carry a usable (not corrupt) memo forward, per the hand-written
    /// `Clone` impl's comment.
    #[test]
    fn native_call_site_is_stable_per_entry_and_survives_clone() {
        let cached = make_cached_method();
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        registry.register(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            noop_native,
        );

        let first = cached.native_call_site() as *const _;
        let second = cached.native_call_site() as *const _;
        assert_eq!(
            first, second,
            "`native_call_site()` must return the one cell this entry owns"
        );

        let warm = cached
            .native_call_site()
            .callback(
                &registry,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            )
            .expect("warm");

        let cloned = cached.clone();
        // The clone shares the triple (`Arc`), so the inherited memo answers
        // for the same triple and must agree.
        let via_clone = cloned
            .native_call_site()
            .callback(
                &registry,
                &cloned.class_name,
                &cloned.method_name,
                &cloned.method_descriptor,
            )
            .expect("clone must resolve the same native");
        assert_eq!(via_clone as usize, warm as usize);
        // ...but it is a distinct cell, so mutating one cannot corrupt the other.
        assert_ne!(
            cached.native_call_site() as *const _,
            cloned.native_call_site() as *const _
        );
    }

    /// T2.2 — the memo primitive, tested against literal generation values so
    /// it is completely independent of the process-global counter (and therefore
    /// race-free under a parallel `cargo test`). The end-to-end protocol against
    /// a real `JitCache` is tested in the `cratonvm-jit` crate.
    #[test]
    fn jit_probe_generation_memo_reports_current_only_for_the_recorded_value() {
        let cached = make_cached_method();
        // A fresh entry has never probed: 0 can never equal a live generation
        // (`JIT_CACHE_GENERATION` starts at 1 and only increases).
        assert!(!cached.jit_probe_is_current(1));
        assert!(!cached.jit_probe_is_current(u64::MAX));

        cached.record_jit_probe_miss(17);
        // Fast path engages only for the exact generation the probe ran at.
        assert!(cached.jit_probe_is_current(17));
        assert!(!cached.jit_probe_is_current(18));
        assert!(!cached.jit_probe_is_current(16));

        // A later probe at a newer generation replaces the memo.
        cached.record_jit_probe_miss(18);
        assert!(cached.jit_probe_is_current(18));
        assert!(!cached.jit_probe_is_current(17));
    }

    /// The manual `Clone` impl must carry the memos forward, not silently drop
    /// them (a dropped memo is only a perf regression, but a *wrong* one would
    /// be a correctness bug, so pin the exact values).
    #[test]
    fn clone_preserves_the_memo_fields() {
        let cached = make_cached_method();
        cached.record_jit_probe_miss(99);
        let key = cached.invoc_key();
        let _ = cached.force_native_cache.set(true);

        let copy = cached.clone();
        assert!(copy.jit_probe_is_current(99));
        assert_eq!(copy.invoc_key(), key);
        assert_eq!(copy.force_native_cache.get(), Some(&true));

        // The clone's atomic is independent of the original's.
        copy.record_jit_probe_miss(100);
        assert!(cached.jit_probe_is_current(99));
        assert!(copy.jit_probe_is_current(100));
    }

    fn make_helpers() -> JitRuntimeHelpers {
        JitRuntimeHelpers {
            newarray: 0x1000,
            new_object: 0x1008,
            anewarray_object: 0x1010,
            baload: 0x1018,
            bastore: 0x1020,
            iaload: 0x1028,
            iastore: 0x1030,
            aaload: 0x1038,
            aastore: 0x1040,
            multianewarray_2d: 0x1048,
            arraylength: 0x1050,
            getfield: 0x1058,
            putfield_int: 0x1060,
            putfield_long: 0x1068,
            putfield_float: 0x1070,
            putfield_double: 0x1078,
            putfield_object: 0x1080,
            getstatic: 0x1088,
            putstatic_int: 0x1090,
            putstatic_long: 0x1098,
            putstatic_float: 0x10A0,
            putstatic_double: 0x10A8,
            putstatic_object: 0x10B0,
            checkcast: 0x10B8,
            instanceof_check: 0x10C0,
            throw_aioobe: 0x10C8,
            throw_arithmetic: 0x1128,
            invoke_dispatch: 0x10D0,
            invoke_virtual_mic: 0x10D8,
            lambda_int_to_double: 0x10DC,
            write_barrier: 0x10E0,
            satb_pre_write_barrier: 0x1110,
            uncommon_trap: 0x10E8,
            math_fma_double: 0x10F0,
            math_fma_float: 0x10F8,
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 8,
            class_id_offset_in_obj: 0,
            get_current_thread: 0x1100,
            tlab_post_init: 0x1108,
            frame_record: 0x1110,
            shadow_stack_offset_in_thread: 0,
            throw_exception: 0x1118,
            jit_npe_with_action: 0x1120,
            dispatch_threw: 0x1128,
            jit_frem: 0x1130,
            jit_drem: 0x1138,
            self_call_stack_guard: 0x1140,
            region_bounds_addr: 0x1148,
            native_stack_floor_fn: 0x1150,
            ldc_string: 0x1158,
            safepoint_flag_addr: 0x1160,
            safepoint_slow_path: 0x1168,
            jit_card_table_addr: 0x1170,
            jit_card_old_base: 0x1178,
            jit_card_old_end: 0x1180,
            set_throw_bci: 0x1188,
            service_callee_deopt: 0x1190,
            new_object_cp: 0x1198,
            anewarray_object_cp: 0x11A0,
            monitor_enter: 0x11A8,
            monitor_exit: 0x11B0,
            ldc_class_cp: 0x11B8,
            aastore_type_check: 0x11C0,
            read_bounds_addr: 0x11C8,
            local_handler_lookup: 0x11D0,
            ldc_string_cp: 0x11D8,
            ffm_segment_get: 0x11E0,
            ffm_segment_set: 0x11E8,
            ref_store_pre_gate: 0x11F0,
            ref_store_post_gate: 0x11F8,
            ref_store_post_young_floor: 0x1200,
            g1_barrier_addr: 0x1208,
            g1_post_write_barrier: 0x1210,
        }
    }

    // --- CachedBytecodeMethod ---

    #[test]
    fn test_cached_method_construction() {
        let m = make_cached_method();
        assert_eq!(m.declaring_class_id, ClassId::new(1));
        assert_eq!(&*m.class_name, "java/lang/Object");
        assert_eq!(&*m.method_name, "hashCode");
        assert_eq!(&*m.method_descriptor, "()I");
        assert_eq!(m.max_stack, 2);
        assert_eq!(m.max_locals, 1);
        assert_eq!(m.num_params, 0);
    }

    #[test]
    fn test_cached_method_source_file_some() {
        let m = make_cached_method();
        assert!(m.source_file.is_some());
        assert_eq!(&*m.source_file.unwrap(), "Object.java");
    }

    #[test]
    fn test_cached_method_source_file_none() {
        let m = CachedBytecodeMethod {
            source_file: None,
            ..make_cached_method()
        };
        assert!(m.source_file.is_none());
    }

    #[test]
    fn test_cached_method_code_content() {
        let m = make_cached_method();
        assert_eq!(m.code.len(), 1);
        assert_eq!(m.code[0], 0xB1); // return opcode
    }

    #[test]
    fn test_cached_method_empty_exception_table() {
        let m = make_cached_method();
        assert!(m.exception_table.is_empty());
    }

    #[test]
    fn test_cached_method_with_exception_table() {
        let entry = ExceptionTableEntry {
            start_pc: 0,
            end_pc: 10,
            handler_pc: 20,
            catch_type: 5,
        };
        let m = CachedBytecodeMethod {
            exception_table: Arc::from(vec![entry].as_slice()),
            ..make_cached_method()
        };
        assert_eq!(m.exception_table.len(), 1);
        assert_eq!(m.exception_table[0].start_pc, 0);
        assert_eq!(m.exception_table[0].end_pc, 10);
        assert_eq!(m.exception_table[0].handler_pc, 20);
        assert_eq!(m.exception_table[0].catch_type, 5);
    }

    #[test]
    fn test_cached_method_clone() {
        let m1 = make_cached_method();
        let m2 = m1.clone();
        assert_eq!(&*m1.class_name, &*m2.class_name);
        assert_eq!(&*m1.method_name, &*m2.method_name);
        assert_eq!(m1.max_stack, m2.max_stack);
        assert_eq!(m1.max_locals, m2.max_locals);
        assert_eq!(m1.code.len(), m2.code.len());
    }

    #[test]
    fn test_cached_method_arc_sharing() {
        let m1 = make_cached_method();
        let m2 = m1.clone();
        // Arc::clone shares the same allocation
        assert!(Arc::ptr_eq(&m1.class_name, &m2.class_name));
        assert!(Arc::ptr_eq(&m1.code, &m2.code));
    }

    #[test]
    fn test_cached_method_large_code() {
        let code: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let m = CachedBytecodeMethod {
            code: Arc::from(code.as_slice()),
            max_stack: 100,
            max_locals: 50,
            ..make_cached_method()
        };
        assert_eq!(m.code.len(), 1024);
        assert_eq!(m.max_stack, 100);
        assert_eq!(m.max_locals, 50);
    }

    // --- JitRuntimeHelpers ---

    #[test]
    fn test_helpers_construction() {
        let h = make_helpers();
        assert_eq!(h.newarray, 0x1000);
        assert_eq!(h.new_object, 0x1008);
        assert_eq!(h.write_barrier, 0x10E0);
    }

    #[test]
    fn test_helpers_copy() {
        let h1 = make_helpers();
        let h2 = h1; // Copy
        assert_eq!(h1.newarray, h2.newarray);
        assert_eq!(h1.invoke_dispatch, h2.invoke_dispatch);
    }

    #[test]
    fn test_helpers_clone() {
        let h1 = make_helpers();
        let h2 = h1.clone();
        assert_eq!(h1.checkcast, h2.checkcast);
        assert_eq!(h1.instanceof_check, h2.instanceof_check);
    }

    #[test]
    fn test_helpers_all_fields_distinct() {
        let h = make_helpers();
        let ptrs = [
            h.newarray,
            h.new_object,
            h.anewarray_object,
            h.baload,
            h.bastore,
            h.iaload,
            h.iastore,
            h.aaload,
            h.aastore,
            h.multianewarray_2d,
            h.arraylength,
            h.getfield,
            h.putfield_int,
            h.putfield_long,
            h.putfield_float,
            h.putfield_double,
            h.putfield_object,
            h.getstatic,
            h.putstatic_int,
            h.putstatic_long,
            h.putstatic_float,
            h.putstatic_double,
            h.putstatic_object,
            h.checkcast,
            h.instanceof_check,
            h.throw_aioobe,
            h.invoke_dispatch,
            h.invoke_virtual_mic,
            h.lambda_int_to_double,
            h.write_barrier,
            h.satb_pre_write_barrier,
        ];
        // All addresses should be unique
        let mut set = std::collections::HashSet::new();
        for p in &ptrs {
            assert!(set.insert(p), "Duplicate pointer value: {:#x}", p);
        }
        assert_eq!(set.len(), 31);
    }

    #[test]
    fn test_helpers_zero_values() {
        let h = JitRuntimeHelpers {
            newarray: 0,
            new_object: 0,
            anewarray_object: 0,
            baload: 0,
            bastore: 0,
            iaload: 0,
            iastore: 0,
            aaload: 0,
            aastore: 0,
            multianewarray_2d: 0,
            arraylength: 0,
            getfield: 0,
            putfield_int: 0,
            putfield_long: 0,
            putfield_float: 0,
            putfield_double: 0,
            putfield_object: 0,
            getstatic: 0,
            putstatic_int: 0,
            putstatic_long: 0,
            putstatic_float: 0,
            putstatic_double: 0,
            putstatic_object: 0,
            checkcast: 0,
            instanceof_check: 0,
            throw_aioobe: 0,
            throw_arithmetic: 0,
            invoke_dispatch: 0,
            invoke_virtual_mic: 0,
            lambda_int_to_double: 0,
            write_barrier: 0,
            satb_pre_write_barrier: 0,
            uncommon_trap: 0,
            math_fma_double: 0,
            math_fma_float: 0,
            tlab_cursor_offset_in_thread: 0,
            tlab_end_offset_in_thread: 0,
            class_id_offset_in_obj: 0,
            get_current_thread: 0,
            tlab_post_init: 0,
            frame_record: 0,
            shadow_stack_offset_in_thread: 0,
            throw_exception: 0,
            jit_npe_with_action: 0,
            dispatch_threw: 0,
            jit_frem: 0,
            jit_drem: 0,
            self_call_stack_guard: 0,
            region_bounds_addr: 0,
            native_stack_floor_fn: 0,
            ldc_string: 0,
            safepoint_flag_addr: 0,
            safepoint_slow_path: 0,
            jit_card_table_addr: 0,
            jit_card_old_base: 0,
            jit_card_old_end: 0,
            set_throw_bci: 0,
            service_callee_deopt: 0,
            new_object_cp: 0,
            anewarray_object_cp: 0,
            monitor_enter: 0,
            monitor_exit: 0,
            ldc_class_cp: 0,
            aastore_type_check: 0,
            read_bounds_addr: 0,
            local_handler_lookup: 0,
            ldc_string_cp: 0,
            ffm_segment_get: 0,
            ffm_segment_set: 0,
            ref_store_pre_gate: 0,
            ref_store_post_gate: 0,
            ref_store_post_young_floor: 0,
            g1_barrier_addr: 0,
            g1_post_write_barrier: 0,
        };
        assert_eq!(h.newarray, 0);
        assert_eq!(h.write_barrier, 0);
    }

    #[test]
    fn test_helpers_field_access_all() {
        let h = make_helpers();
        // Verify every field is accessible and has the expected value
        assert_eq!(h.anewarray_object, 0x1010);
        assert_eq!(h.baload, 0x1018);
        assert_eq!(h.bastore, 0x1020);
        assert_eq!(h.iaload, 0x1028);
        assert_eq!(h.iastore, 0x1030);
        assert_eq!(h.aaload, 0x1038);
        assert_eq!(h.aastore, 0x1040);
        assert_eq!(h.multianewarray_2d, 0x1048);
        assert_eq!(h.arraylength, 0x1050);
        assert_eq!(h.getfield, 0x1058);
        assert_eq!(h.putfield_int, 0x1060);
        assert_eq!(h.putfield_long, 0x1068);
        assert_eq!(h.putfield_float, 0x1070);
        assert_eq!(h.putfield_double, 0x1078);
        assert_eq!(h.putfield_object, 0x1080);
        assert_eq!(h.getstatic, 0x1088);
        assert_eq!(h.putstatic_int, 0x1090);
        assert_eq!(h.putstatic_long, 0x1098);
        assert_eq!(h.putstatic_float, 0x10A0);
        assert_eq!(h.putstatic_double, 0x10A8);
        assert_eq!(h.putstatic_object, 0x10B0);
        assert_eq!(h.checkcast, 0x10B8);
        assert_eq!(h.instanceof_check, 0x10C0);
        assert_eq!(h.throw_aioobe, 0x10C8);
        assert_eq!(h.invoke_dispatch, 0x10D0);
        assert_eq!(h.invoke_virtual_mic, 0x10D8);
    }

    #[test]
    fn test_cached_method_multiple_exception_entries() {
        let entries = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 5,
                handler_pc: 10,
                catch_type: 1,
            },
            ExceptionTableEntry {
                start_pc: 5,
                end_pc: 15,
                handler_pc: 20,
                catch_type: 2,
            },
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 15,
                handler_pc: 30,
                catch_type: 0,
            }, // finally
        ];
        let m = CachedBytecodeMethod {
            exception_table: Arc::from(entries.as_slice()),
            ..make_cached_method()
        };
        assert_eq!(m.exception_table.len(), 3);
        assert_eq!(m.exception_table[2].catch_type, 0); // finally handler
    }

    #[test]
    fn test_cached_method_declaring_class_id_equality() {
        let m1 = make_cached_method();
        let m2 = CachedBytecodeMethod {
            declaring_class_id: ClassId::new(2),
            ..make_cached_method()
        };
        assert_eq!(m1.declaring_class_id, ClassId::new(1));
        assert_eq!(m2.declaring_class_id, ClassId::new(2));
        assert_ne!(m1.declaring_class_id, m2.declaring_class_id);
    }

    // --- JitRuntimeHelpers validation ---

    #[test]
    fn test_helpers_validate_all_nonzero() {
        let h = make_helpers();
        assert!(h.validate().is_ok());
    }

    #[test]
    fn test_helpers_validate_fails_on_zero() {
        let mut h = make_helpers();
        h.newarray = 0;
        let err = h.validate().expect_err("zero newarray must fail");
        assert!(err.contains(&"newarray"));
    }

    #[test]
    fn test_helpers_null_pointers_empty_when_valid() {
        let h = make_helpers();
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn test_helpers_null_pointers_reports_zeroed_fields() {
        let mut h = make_helpers();
        h.newarray = 0;
        h.write_barrier = 0;
        let nulls = h.null_pointers();
        assert!(nulls.contains(&"newarray"));
        assert!(nulls.contains(&"write_barrier"));
        assert_eq!(nulls.len(), 2);
    }

    // --- Builder tests deleted in 2026-05-16 audit: Builder itself
    //     was deleted (see comment above the deleted block in the
    //     module body). The remaining tests cover the runtime-helpers
    //     struct + validation directly.

    // --- JitRuntimeHelpers golden ABI offsets ---
    //
    // The JIT compiler bakes `JitRuntimeHelpers` field addresses into
    // generated RWX machine code as absolute CALL targets and as
    // `[helpers_ptr + disp32]` loads. A silent field reorder would
    // change the disp32 immediates while the JIT still emits the old
    // offsets — i.e. a CALL that used to dispatch `new_object` would
    // now dispatch `anewarray_object`, with no compile error. This
    // failure is undetectable at runtime until the wrong helper
    // corrupts the heap.
    //
    // The struct is `#[repr(C)]` and every field is `usize` (8 bytes
    // on x86-64, the only supported target). That gives a sequential
    // layout with no padding, so the documented offset of field N is
    // simply `N * 8`. The test below pins both:
    //
    //   1. The byte offset of every individual field (via
    //      `std::mem::offset_of!`, stable since 1.77), and
    //   2. The 0-based field index of every field (via a const N) so
    //      a swap of two fields with the same size — which preserves
    //      every offset individually — is also caught.
    //
    // If you add a new field to `JitRuntimeHelpers`, append a row
    // below and bump the `NUM_FIELDS` literal in the const-assert in
    // the module body. Do NOT renumber existing rows: the indices
    // here are part of the JIT ABI.

    /// Width of every field in `JitRuntimeHelpers` (every field is `usize`).
    /// On any 64-bit target Rust supports this is 8. We pin to 8 because the
    /// JIT only emits x86-64 code today; if/when 32-bit support lands, this
    /// test (and the JIT immediate-width assumptions) must be revisited.
    const FIELD_WIDTH: usize = 8;

    /// Documented JIT ABI offset of a field, given its 0-based index in
    /// the `#[repr(C)]` declaration order. Sequential because every
    /// field is `usize` (no inter-field padding).
    const fn expected_offset(index: usize) -> usize {
        index * FIELD_WIDTH
    }

    #[test]
    fn jit_runtime_helpers_field_size_is_eight() {
        // x86-64 only. If this fires, the per-field 8-byte stride
        // below is no longer the JIT ABI and the offset assertions
        // are stale.
        assert_eq!(std::mem::size_of::<usize>(), FIELD_WIDTH);
    }

    #[test]
    fn jit_runtime_helpers_struct_size_matches_field_count() {
        // `#[repr(C)]` + all `usize` fields ⇒ no padding ⇒ struct size
        // is exactly `NUM_FIELDS * 8`. Drift here means a field was
        // added/removed without updating `helper_fields!`, OR the
        // struct gained a non-`usize` field that violates the JIT ABI.
        assert_eq!(
            std::mem::size_of::<JitRuntimeHelpers>(),
            JitRuntimeHelpers::NUM_FIELDS * FIELD_WIDTH,
        );
        // And the macro-driven count is the canonical one for this ABI
        // revision -- 74 as of v11. v10 appended dev's three reference-store
        // barrier gates; F-08 appended the two G1 inline-barrier words after
        // them, so both sets of golden offsets below stay where they were.
        assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 74);
    }

    #[test]
    fn jit_runtime_helpers_repr_c_golden_offsets() {
        // Each row: (0-based ABI index, field name, offset_of! probe).
        // The expected byte offset is `index * 8`. The names are also
        // checked against `all_fields()` so a field rename (which the
        // offset_of! invocation would silently follow) is caught too.
        let probes: [(usize, &'static str, usize); JitRuntimeHelpers::NUM_FIELDS] = [
            (
                0,
                "newarray",
                std::mem::offset_of!(JitRuntimeHelpers, newarray),
            ),
            (
                1,
                "new_object",
                std::mem::offset_of!(JitRuntimeHelpers, new_object),
            ),
            (
                2,
                "anewarray_object",
                std::mem::offset_of!(JitRuntimeHelpers, anewarray_object),
            ),
            (3, "baload", std::mem::offset_of!(JitRuntimeHelpers, baload)),
            (
                4,
                "bastore",
                std::mem::offset_of!(JitRuntimeHelpers, bastore),
            ),
            (5, "iaload", std::mem::offset_of!(JitRuntimeHelpers, iaload)),
            (
                6,
                "iastore",
                std::mem::offset_of!(JitRuntimeHelpers, iastore),
            ),
            (7, "aaload", std::mem::offset_of!(JitRuntimeHelpers, aaload)),
            (
                8,
                "aastore",
                std::mem::offset_of!(JitRuntimeHelpers, aastore),
            ),
            (
                9,
                "multianewarray_2d",
                std::mem::offset_of!(JitRuntimeHelpers, multianewarray_2d),
            ),
            (
                10,
                "arraylength",
                std::mem::offset_of!(JitRuntimeHelpers, arraylength),
            ),
            (
                11,
                "getfield",
                std::mem::offset_of!(JitRuntimeHelpers, getfield),
            ),
            (
                12,
                "putfield_int",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_int),
            ),
            (
                13,
                "putfield_long",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_long),
            ),
            (
                14,
                "putfield_float",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_float),
            ),
            (
                15,
                "putfield_double",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_double),
            ),
            (
                16,
                "putfield_object",
                std::mem::offset_of!(JitRuntimeHelpers, putfield_object),
            ),
            (
                17,
                "getstatic",
                std::mem::offset_of!(JitRuntimeHelpers, getstatic),
            ),
            (
                18,
                "putstatic_int",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_int),
            ),
            (
                19,
                "putstatic_long",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_long),
            ),
            (
                20,
                "putstatic_float",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_float),
            ),
            (
                21,
                "putstatic_double",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_double),
            ),
            (
                22,
                "putstatic_object",
                std::mem::offset_of!(JitRuntimeHelpers, putstatic_object),
            ),
            (
                23,
                "checkcast",
                std::mem::offset_of!(JitRuntimeHelpers, checkcast),
            ),
            (
                24,
                "instanceof_check",
                std::mem::offset_of!(JitRuntimeHelpers, instanceof_check),
            ),
            (
                25,
                "throw_aioobe",
                std::mem::offset_of!(JitRuntimeHelpers, throw_aioobe),
            ),
            (
                26,
                "throw_arithmetic",
                std::mem::offset_of!(JitRuntimeHelpers, throw_arithmetic),
            ),
            (
                27,
                "invoke_dispatch",
                std::mem::offset_of!(JitRuntimeHelpers, invoke_dispatch),
            ),
            (
                28,
                "invoke_virtual_mic",
                std::mem::offset_of!(JitRuntimeHelpers, invoke_virtual_mic),
            ),
            (
                29,
                "lambda_int_to_double",
                std::mem::offset_of!(JitRuntimeHelpers, lambda_int_to_double),
            ),
            (
                30,
                "write_barrier",
                std::mem::offset_of!(JitRuntimeHelpers, write_barrier),
            ),
            (
                31,
                "satb_pre_write_barrier",
                std::mem::offset_of!(JitRuntimeHelpers, satb_pre_write_barrier),
            ),
            (
                32,
                "uncommon_trap",
                std::mem::offset_of!(JitRuntimeHelpers, uncommon_trap),
            ),
            (
                33,
                "math_fma_double",
                std::mem::offset_of!(JitRuntimeHelpers, math_fma_double),
            ),
            (
                34,
                "math_fma_float",
                std::mem::offset_of!(JitRuntimeHelpers, math_fma_float),
            ),
            (
                35,
                "tlab_cursor_offset_in_thread",
                std::mem::offset_of!(JitRuntimeHelpers, tlab_cursor_offset_in_thread),
            ),
            (
                36,
                "tlab_end_offset_in_thread",
                std::mem::offset_of!(JitRuntimeHelpers, tlab_end_offset_in_thread),
            ),
            (
                37,
                "class_id_offset_in_obj",
                std::mem::offset_of!(JitRuntimeHelpers, class_id_offset_in_obj),
            ),
            (
                38,
                "get_current_thread",
                std::mem::offset_of!(JitRuntimeHelpers, get_current_thread),
            ),
            (
                39,
                "tlab_post_init",
                std::mem::offset_of!(JitRuntimeHelpers, tlab_post_init),
            ),
            (
                40,
                "frame_record",
                std::mem::offset_of!(JitRuntimeHelpers, frame_record),
            ),
            (
                41,
                "shadow_stack_offset_in_thread",
                std::mem::offset_of!(JitRuntimeHelpers, shadow_stack_offset_in_thread),
            ),
            (
                42,
                "throw_exception",
                std::mem::offset_of!(JitRuntimeHelpers, throw_exception),
            ),
            (
                43,
                "jit_npe_with_action",
                std::mem::offset_of!(JitRuntimeHelpers, jit_npe_with_action),
            ),
            (
                44,
                "dispatch_threw",
                std::mem::offset_of!(JitRuntimeHelpers, dispatch_threw),
            ),
            (
                45,
                "jit_frem",
                std::mem::offset_of!(JitRuntimeHelpers, jit_frem),
            ),
            (
                46,
                "jit_drem",
                std::mem::offset_of!(JitRuntimeHelpers, jit_drem),
            ),
            (
                47,
                "self_call_stack_guard",
                std::mem::offset_of!(JitRuntimeHelpers, self_call_stack_guard),
            ),
            (
                48,
                "region_bounds_addr",
                std::mem::offset_of!(JitRuntimeHelpers, region_bounds_addr),
            ),
            (
                49,
                "native_stack_floor_fn",
                std::mem::offset_of!(JitRuntimeHelpers, native_stack_floor_fn),
            ),
            (
                50,
                "ldc_string",
                std::mem::offset_of!(JitRuntimeHelpers, ldc_string),
            ),
            (
                51,
                "safepoint_flag_addr",
                std::mem::offset_of!(JitRuntimeHelpers, safepoint_flag_addr),
            ),
            (
                52,
                "safepoint_slow_path",
                std::mem::offset_of!(JitRuntimeHelpers, safepoint_slow_path),
            ),
            (
                53,
                "jit_card_table_addr",
                std::mem::offset_of!(JitRuntimeHelpers, jit_card_table_addr),
            ),
            (
                54,
                "jit_card_old_base",
                std::mem::offset_of!(JitRuntimeHelpers, jit_card_old_base),
            ),
            (
                55,
                "jit_card_old_end",
                std::mem::offset_of!(JitRuntimeHelpers, jit_card_old_end),
            ),
            (
                56,
                "set_throw_bci",
                std::mem::offset_of!(JitRuntimeHelpers, set_throw_bci),
            ),
            (
                57,
                "service_callee_deopt",
                std::mem::offset_of!(JitRuntimeHelpers, service_callee_deopt),
            ),
            (
                58,
                "new_object_cp",
                std::mem::offset_of!(JitRuntimeHelpers, new_object_cp),
            ),
            (
                59,
                "anewarray_object_cp",
                std::mem::offset_of!(JitRuntimeHelpers, anewarray_object_cp),
            ),
            (
                60,
                "monitor_enter",
                std::mem::offset_of!(JitRuntimeHelpers, monitor_enter),
            ),
            (
                61,
                "monitor_exit",
                std::mem::offset_of!(JitRuntimeHelpers, monitor_exit),
            ),
            (
                62,
                "ldc_class_cp",
                std::mem::offset_of!(JitRuntimeHelpers, ldc_class_cp),
            ),
            (
                63,
                "aastore_type_check",
                std::mem::offset_of!(JitRuntimeHelpers, aastore_type_check),
            ),
            (
                64,
                "read_bounds_addr",
                std::mem::offset_of!(JitRuntimeHelpers, read_bounds_addr),
            ),
            (
                65,
                "local_handler_lookup",
                std::mem::offset_of!(JitRuntimeHelpers, local_handler_lookup),
            ),
            (
                66,
                "ldc_string_cp",
                std::mem::offset_of!(JitRuntimeHelpers, ldc_string_cp),
            ),
            (
                67,
                "ffm_segment_get",
                std::mem::offset_of!(JitRuntimeHelpers, ffm_segment_get),
            ),
            (
                68,
                "ffm_segment_set",
                std::mem::offset_of!(JitRuntimeHelpers, ffm_segment_set),
            ),
            (
                69,
                "ref_store_pre_gate",
                std::mem::offset_of!(JitRuntimeHelpers, ref_store_pre_gate),
            ),
            (
                70,
                "ref_store_post_gate",
                std::mem::offset_of!(JitRuntimeHelpers, ref_store_post_gate),
            ),
            (
                71,
                "ref_store_post_young_floor",
                std::mem::offset_of!(JitRuntimeHelpers, ref_store_post_young_floor),
            ),
            (
                72,
                "g1_barrier_addr",
                std::mem::offset_of!(JitRuntimeHelpers, g1_barrier_addr),
            ),
            (
                73,
                "g1_post_write_barrier",
                std::mem::offset_of!(JitRuntimeHelpers, g1_post_write_barrier),
            ),
        ];

        // (a) Each field is at its documented sequential byte offset.
        // A swap of two same-size fields would change the (index, name)
        // → offset_of! relationship and trip this.
        for (idx, name, actual) in probes.iter() {
            let expected = expected_offset(*idx);
            assert_eq!(
                *actual, expected,
                "ABI offset drift: field `{}` (index {}) is at byte offset {}, \
                 expected {}. The JIT bakes this offset into RWX code — fix the \
                 reorder, or if intentional, update both this table and \
                 `vm/src/jit/helpers.rs::build_helpers`.",
                name, idx, actual, expected,
            );
        }

        // (b) The macro-generated name list agrees with the golden
        // table, in the same order. This catches a field rename in
        // the struct that the contributor forgot to mirror into the
        // `helper_fields!` macro invocation (or vice versa).
        let h = make_helpers();
        let fields = h.all_fields();
        assert_eq!(fields.len(), probes.len());
        for (i, (_, expected_name, _)) in probes.iter().enumerate() {
            assert_eq!(
                fields[i].name, *expected_name,
                "macro-driven field order disagrees with golden ABI table at \
                 index {}: macro has `{}`, golden has `{}`",
                i, fields[i].name, expected_name,
            );
        }
    }

    #[test]
    fn jit_runtime_helpers_all_fields_classified() {
        // The macro must classify every field.
        // 43 RequiredPtr + 12 OptionalPtr + 10 Offset = 65. A new
        // field whose classification is omitted will fail to compile (the
        // macro requires both arms); this test pins the *counts* so a
        // reclassification (e.g. demoting a RequiredPtr to OptionalPtr) is
        // also a deliberate, reviewed change.
        let h = make_helpers();
        let f = h.all_fields();
        let req = f
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr)
            .count();
        let opt = f
            .iter()
            .filter(|e| e.kind == FieldKind::OptionalPtr)
            .count();
        let off = f.iter().filter(|e| e.kind == FieldKind::Offset).count();
        assert_eq!(req, 43, "required-pointer count drifted");
        assert_eq!(opt, 17, "optional-pointer count drifted");
        assert_eq!(off, 14, "offset-field count drifted");
        assert_eq!(req + opt + off, JitRuntimeHelpers::NUM_FIELDS);
    }

    #[test]
    fn jit_runtime_helpers_validate_ignores_offsets() {
        // `class_id_offset_in_obj` is 0 by contract; the validator
        // must NOT reject on that. (Regression for the round-9 fix —
        // the previous `validate()` looped over a hand-maintained array
        // that omitted all offset/optional fields, so this was true
        // by accident. The new validator iterates ALL 46 fields and
        // must still pass when offsets are zero.)
        let mut h = make_helpers();
        h.tlab_cursor_offset_in_thread = 0;
        h.tlab_end_offset_in_thread = 0;
        h.class_id_offset_in_obj = 0;
        assert!(h.validate().is_ok());
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn jit_runtime_helpers_validate_accepts_unwired_optionals() {
        // Optional helpers default to zero ("not wired") — must pass.
        let mut h = make_helpers();
        h.get_current_thread = 0;
        h.tlab_post_init = 0;
        assert!(h.validate().is_ok());
        // And they are NOT reported by null_pointers (they are not
        // mandatory-and-null, just optional-and-unset).
        assert!(h.null_pointers().is_empty());
    }

    #[test]
    fn jit_runtime_helpers_validate_rejects_each_required_null() {
        // Sweep every required-pointer field: zeroing it must trip
        // the validator AND report the field by name.
        let names: Vec<&'static str> = make_helpers()
            .all_fields()
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr)
            .map(|e| e.name)
            .collect();
        assert_eq!(names.len(), 43);
        for name in names {
            let mut h = make_helpers();
            // Zero the field by name via a match — the macro doesn't
            // give us per-field mutable accessors, so this is exhaustive
            // but verbose. The test exists precisely to ensure no
            // required field is silently uncovered.
            zero_field_by_name(&mut h, name);
            match h.validate() {
                Ok(()) => panic!(
                    "validator failed to reject null required field `{}` (returned Ok)",
                    name
                ),
                Err(err) => assert!(
                    err.contains(&name),
                    "validator's Err list did not include `{}`; got {:?}",
                    name,
                    err,
                ),
            }
            let nulls = h.null_pointers();
            assert!(
                nulls.contains(&name),
                "null_pointers() did not report `{}`; got {:?}",
                name,
                nulls,
            );
        }
    }

    #[test]
    fn jit_runtime_helpers_all_required_null_reports_every_name() {
        // Zero EVERY required pointer at once: `null_pointers()` must
        // return the complete set of 41 required-field names (and
        // `validate()` must reject). This complements the per-field
        // sweep above — it proves the validator does not stop at the
        // first miss and that the offset/optional fields (left non-zero
        // for offsets, zero for unwired optionals) never leak into the
        // report.
        let mut h = make_helpers();
        let required: Vec<&'static str> = h
            .all_fields()
            .iter()
            .filter(|e| e.kind == FieldKind::RequiredPtr)
            .map(|e| e.name)
            .collect();
        assert_eq!(required.len(), 43, "expected 43 required pointers");
        // throw_exception is the round-10 addition — pin it explicitly so
        // a regression that drops it from the required set is caught here
        // and not just by the count.
        assert!(
            required.contains(&"throw_exception"),
            "throw_exception must be a required (null-rejected) pointer",
        );
        // jit_npe_with_action is the JEP-358 inline-NPE-path addition — pin it
        // explicitly for the same reason.
        assert!(
            required.contains(&"jit_npe_with_action"),
            "jit_npe_with_action must be a required (null-rejected) pointer",
        );
        // dispatch_threw is the i64::MIN-sentinel J/D-return disambiguation
        // helper — pin it explicitly for the same reason.
        assert!(
            required.contains(&"dispatch_threw"),
            "dispatch_threw must be a required (null-rejected) pointer",
        );
        // jit_frem / jit_drem are the IR FP-tier (Slice A) fmod helpers — pin
        // them explicitly for the same reason.
        assert!(
            required.contains(&"jit_frem"),
            "jit_frem must be a required (null-rejected) pointer",
        );
        assert!(
            required.contains(&"jit_drem"),
            "jit_drem must be a required (null-rejected) pointer",
        );
        // ldc_string is the interned-String-materialization helper for
        // compiled `ldc` sites — pin it explicitly for the same reason.
        assert!(
            required.contains(&"ldc_string"),
            "ldc_string must be a required (null-rejected) pointer",
        );

        for name in &required {
            zero_field_by_name(&mut h, name);
        }

        let nulls = h.null_pointers();
        assert_eq!(
            nulls.len(),
            required.len(),
            "null_pointers() should report all {} required fields; got {:?}",
            required.len(),
            nulls,
        );
        for name in &required {
            assert!(
                nulls.contains(name),
                "null_pointers() omitted required field `{}`; got {:?}",
                name,
                nulls,
            );
        }
        // And validate() must fail with the same complete list.
        match h.validate() {
            Ok(()) => panic!("validate() returned Ok with all required pointers null"),
            Err(err) => assert_eq!(err.len(), required.len()),
        }
    }

    // Helper for `validate_rejects_each_required_null`: zero a single
    // field by string name. Kept in the test module so production
    // code doesn't grow a stringly-typed mutator.
    fn zero_field_by_name(h: &mut JitRuntimeHelpers, name: &str) {
        match name {
            "newarray" => h.newarray = 0,
            "new_object" => h.new_object = 0,
            "anewarray_object" => h.anewarray_object = 0,
            "baload" => h.baload = 0,
            "bastore" => h.bastore = 0,
            "iaload" => h.iaload = 0,
            "iastore" => h.iastore = 0,
            "aaload" => h.aaload = 0,
            "aastore" => h.aastore = 0,
            "multianewarray_2d" => h.multianewarray_2d = 0,
            "arraylength" => h.arraylength = 0,
            "getfield" => h.getfield = 0,
            "putfield_int" => h.putfield_int = 0,
            "putfield_long" => h.putfield_long = 0,
            "putfield_float" => h.putfield_float = 0,
            "putfield_double" => h.putfield_double = 0,
            "putfield_object" => h.putfield_object = 0,
            "getstatic" => h.getstatic = 0,
            "putstatic_int" => h.putstatic_int = 0,
            "putstatic_long" => h.putstatic_long = 0,
            "putstatic_float" => h.putstatic_float = 0,
            "putstatic_double" => h.putstatic_double = 0,
            "putstatic_object" => h.putstatic_object = 0,
            "checkcast" => h.checkcast = 0,
            "instanceof_check" => h.instanceof_check = 0,
            "throw_aioobe" => h.throw_aioobe = 0,
            "throw_arithmetic" => h.throw_arithmetic = 0,
            "invoke_dispatch" => h.invoke_dispatch = 0,
            "invoke_virtual_mic" => h.invoke_virtual_mic = 0,
            "lambda_int_to_double" => h.lambda_int_to_double = 0,
            "write_barrier" => h.write_barrier = 0,
            "satb_pre_write_barrier" => h.satb_pre_write_barrier = 0,
            "uncommon_trap" => h.uncommon_trap = 0,
            "math_fma_double" => h.math_fma_double = 0,
            "math_fma_float" => h.math_fma_float = 0,
            // round-10: throw_exception was added as a RequiredPtr after
            // the original 33-field sweep was written. Without this arm the
            // sweep panics on it and the required-null coverage is silently
            // incomplete (the exact failure mode this test guards against).
            "throw_exception" => h.throw_exception = 0,
            "jit_npe_with_action" => h.jit_npe_with_action = 0,
            "dispatch_threw" => h.dispatch_threw = 0,
            "jit_frem" => h.jit_frem = 0,
            "jit_drem" => h.jit_drem = 0,
            "ldc_string" => h.ldc_string = 0,
            "set_throw_bci" => h.set_throw_bci = 0,
            "aastore_type_check" => h.aastore_type_check = 0,
            "read_bounds_addr" => h.read_bounds_addr = 0,
            "local_handler_lookup" => h.local_handler_lookup = 0,
            other => panic!("unknown required-pointer field name in test: {}", other),
        }
    }
}
