// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::Arc;
use crate::attribute::ExceptionTableEntry;
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
mod descriptor_contract_tests;

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
/// `cross-owner-closeout.md` §6.
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
        None::<std::ffi::OsString>.is_some()
    })
}

/// The per-call return-tag scan [`CachedBytecodeMethod::return_tag`] replaces,
/// kept here so the kill switch can restore the old cost, and the one
/// definition [`DescriptorFacts::of`] memoizes. Byte-for-byte the same answer
/// as `cratonvm_jit::return_type`, which this crate cannot name (the
/// dependency runs the other way).
///
/// The parameter list is WALKED, not scanned for the first `)` (round 11 wave
/// 4). JVMS 4.2.2 bars only `. ; [ /` from a class name, so `(LA)B;)Z` is a
/// legal descriptor of a method taking one `LA)B;` and returning `boolean`.
/// The first-`)` scan answered `B` for it, so `ireturn` narrowed the result as
/// a `byte` in the interpreter while both compiled tiers (which read
/// `cratonvm_jit::return_type`, fixed for this in round 11) narrowed it as a
/// `boolean`. The walk mirrors `cratonvm_jit::DescriptorParamIter` token for
/// token, malformed input included, and the byte after the `)` that stops it
/// is the tag; `b'V'` when there is none.
#[inline]
fn scan_return_tag(descriptor: &str) -> u8 {
    descriptor_param_list_end(descriptor)
        .and_then(|close| descriptor.as_bytes().get(close + 1).copied())
        .unwrap_or(b'V')
}

/// The byte index of the `)` that CLOSES a method descriptor's parameter
/// list, found by walking the parameter tokens; `None` when the walk runs off
/// the end (no closing `)`, or an unterminated `L...;`).
///
/// The one place to cut a descriptor into parameters and return type. A class
/// name may contain `)` (JVMS 4.2.2 bars only `. ; [ /`), so `(LA)B;)Z` takes
/// one `LA)B;` and returns `boolean`, and a `find(')')` cuts it inside the
/// class name (round 11 wave 5: `r11w4-lower-first-paren-descriptor-scans-outside-the-lane`).
/// The walk skips a leading `(` when there is one, and mirrors
/// `cratonvm_jit::DescriptorParamIter` token for token, malformed input
/// included.
#[inline]
pub fn descriptor_param_list_end(descriptor: &str) -> Option<usize> {
    let bytes = descriptor.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'('));
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < bytes.len() {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    (bytes.get(i) == Some(&b')')).then_some(i)
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
        // Return tag: the byte after the `)` that closes the parameter list,
        // found by walking the parameters -- NOT the first `)`, which can sit
        // inside a class name (see `scan_return_tag`). Identical to
        // `cratonvm_jit::return_type`, including its `b'V'` answer for a
        // descriptor with no ')' or nothing after it.
        let ret_tag = scan_return_tag(descriptor);
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
    /// `docs/feature-designs/gpu-offload.md`).
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
    /// JDK-ONLY-WAVE2 (`docs/book/src/user-guide/real-jdk-mode.md` §7): this cell
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
    /// Is this method's frame hidden from a `Throwable`'s stack trace, as
    /// HotSpot's `Method::is_hidden` (a method of a hidden class, or one
    /// annotated `@jdk.internal.vm.annotation.Hidden`)? Decided by the stack
    /// walker the first time the frame is captured (interpreter round i1
    /// wave 34); `stackwalker::backtrace_frame_is_hidden`.
    pub hidden_frame: std::sync::OnceLock<bool>,
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
    /// See `docs/feature-designs/real-jdk-mode.md`.
    ///
    /// The *argument*- and *receiver*-dependent halves of those arms are NOT
    /// memoized and must not be: a null second argument, an Objenesis-shaped
    /// receiver and a redefined class are per-call state. This cell answers
    /// only "could this triple ever reach that arm", so a set bit still runs
    /// the original test in full, and a clear bit skips a test whose
    /// name-keyed half could not have matched anyway.
    pub intercept_shape_cache: std::sync::OnceLock<u8>,
    /// Interpreter-side invocation counter for the tier-up decision on the
    /// interpreter's fast doors. Bumped with a relaxed load and store per
    /// call — a racy, lossy count like HotSpot's interpreter counters (see
    /// `invoke_fast::DOOR_COUNT_IS_PLAIN` in the vm crate); folded into
    /// `ProfileStore::increment_invocation` in batches so the census and
    /// `hot_but_stuck_in_interpreter` still see every counted call (see
    /// `invoke_fast::count_door_invocation`).
    pub interp_invocations: std::sync::atomic::AtomicU32,
    /// The tiered manager's "nothing left to decide" stamp for this call
    /// site's method, or `0`.
    ///
    /// Written from `TieredCompilationManager::on_method_invocation_settling`
    /// and checked with `TieredCompilationManager::tiering_settled` before the
    /// interpreter's tier-up hooks call the manager again. A method that will
    /// never compile (declined by policy, out of retries, already at C2) used
    /// to take the manager's global `methods` mutex and build a fresh key at
    /// every retry stride for the life of the process. The stamp is a manager
    /// generation, so a deopt, an unload, a redefinition or a policy change
    /// expires every stamp at once and the hook resumes asking.
    pub tiering_settled: std::sync::atomic::AtomicU32,
    /// Per-method branch-profile arm, cached on this entry so the interpreter's
    /// conditional-branch arms can test it with one relaxed load.
    ///
    /// Set by the VM's first-nomination door
    /// (`jit_bridge::offer_invocation_to_tiered_manager`) when the tiered
    /// manager queues this method's first compile while it is still
    /// interpreted: the door arms the method's own lock-free counters
    /// (`ProfileStore::arm_branch_counters_borrowed`) and sets this flag, and
    /// from then on the interpreter records this method's branches even with
    /// the process-wide PGO gate off. See
    /// `docs/feature-designs/jit-osr-entry-metadata.md`.
    ///
    /// Sticky (nothing clears it) and advisory: a `false` on an entry of a
    /// method armed through another entry only loses observations, never
    /// changes what the program computes. Every struct literal initialises it
    /// to `false`; [`Clone`] carries it forward, since the clone names the
    /// same method.
    pub branch_profile_armed: std::sync::atomic::AtomicBool,
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
    /// docs/feature-designs/gpu-offload.md.
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
    /// process. `cratonvm_types::NativeCallSite` keys its memo on the
    /// registry generation instead, so a stale negative self-heals at the cost
    /// of one `u32` compare -- the same argument
    /// [`Self::jit_probe_generation`] below already relies on. See
    /// `native-dispatch-memoization.md` §3
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
    /// `interpreter-completion.md`.
    ///
    /// # Real-JDK: this cell does NOT lose the `NativeKind`
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
    pub native_callback_cache: std::sync::OnceLock<cratonvm_types::NativeCallSite>,
    /// T2.5 — memoized JIT invocation-counter key for this method, i.e. the
    /// packed `(declaring_class_id << 64) | fingerprint(method_name, descriptor)`
    /// u128 that the interpreter uses to index
    /// `ProfileStore::increment_invocation` (see [`invoc_key_parts`]).
    ///
    /// The interpreter's `Bytecode` / `VirtualBytecode` dispatch arms recomputed
    /// this key on EVERY interpreted invocation of a not-yet-compiled method by
    /// running a 31-multiplier byte loop over both the method name and the full
    /// descriptor — pure per-call overhead on the hottest interpreter path, for
    /// a value that is a pure function of three immutable fields of this entry.
    /// Memoized here for exactly the same reason (and by exactly the same
    /// argument) as [`Self::force_native_cache`] and
    /// [`Self::native_callback_cache`] above. Read via [`Self::invoc_key`].
    pub invoc_key: std::sync::OnceLock<u128>,
    /// T2.2 — epoch memo for "this method has no published JIT body".
    ///
    /// Holds the value of the owning VM's `JitCache::generation()` as of the last
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
    /// *every* JIT-cache publication and invalidation bumps that cache's
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
    /// executed. `crate::QuickenedCode` does that decode once and
    /// hands out borrowed `&Instruction` records thereafter.
    ///
    /// `None` means "this method could not be pre-decoded" (a linear walk
    /// from pc 0 hit a decode error); such methods keep using the original
    /// on-demand decode, so quickening can never change behaviour.
    ///
    /// The stream itself is interned process-wide on the identity of the
    /// bytecode allocation (`crate::quickened::intern`), so the
    /// several `CachedBytecodeMethod`s that a hot method accumulates across
    /// call sites all share one copy rather than each building their own.
    pub quickened: std::sync::OnceLock<Option<std::sync::Arc<crate::QuickenedCode>>>,
    /// The constant-pool generation `code` indexes: the class-redefinition
    /// count (`cratonvm_classloading::class_redefinition_count`) its producer
    /// read under the same class-manager guard it copied `code` under, or
    /// [`Self::POOL_GENERATION_UNKNOWN`] for a producer that does not know it
    /// (every [`Self::from_parts`] entry until [`Self::with_pool_generation`]
    /// names it).
    ///
    /// Interpreter round i1 wave 43, lane L2
    /// (`interpreter-L3-proposal-frames-carry-the-pool-generation-their-door-validated-FIXED-20261011`):
    /// a frame pushed from this entry while its class's redefinition swapped
    /// the pool runs THIS body, whichever body its stamp suggests; the
    /// conversion of such a frame reads the generation here instead of
    /// guessing from bytes (`obsolete_frames::convert_frames`). Read only on
    /// that path; no call path loads it.
    pub pool_generation: u64,
    /// The interpreter's per-method memo of fused array increment shape
    /// declines (interpreter round i3 wave 75, lane L7,
    /// `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`; read and written through
    /// `vm::runtime::interpreter::field_fast` only): one bit per byte of
    /// [`Self::code`], set at the pc of a fused array increment entry whose
    /// group the decoder refused on its bytes (`a[j] += SF`, `a[j] += b[k]`),
    /// so the dispatch loop no longer calls the helper there. Allocated on
    /// the first such decline through this entry; empty for every entry of
    /// a method that never declines, and never read with the switch off.
    ///
    /// Advisory: a set bit only makes the interpreter run that group
    /// unfused, which answers exactly as the fused group would, so a lost
    /// write (a racing `fetch_or` is not lost, but a clone starts empty) or a
    /// bit set by another VM sharing this entry costs speed, never an answer.
    ///
    /// Since interpreter round i3 wave 77 (lane L7) the cell holds an
    /// [`ArrayIncMemo`]: the same bitmap (`declined`) plus a lazily allocated
    /// per-pc miss counter for the deferred operands
    /// (`CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`).
    pub array_inc_declined: std::sync::OnceLock<ArrayIncMemo>,
    /// The interpreter's special-door constructor verdict for this entry's
    /// method (`vm::runtime::interpreter::invoke_fast::CtorVerdict`, with the
    /// VM identity and the process redefinition count it was decided under),
    /// filled once under `CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY` (interpreter
    /// round i6 wave 119, lane L7; stage 3 of
    /// `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`).
    /// Opaque here (`Any`): the cell's type is the interpreter's
    /// (`invoke_fast::EntryCtorVerdictCell`, installed once and refilled
    /// since wave 120). A stale stamp is a miss and a refill, never a wrong
    /// verdict; a clone starts empty.
    pub ctor_verdict_memo: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
}

/// What [`CachedBytecodeMethod::array_inc_declined`] holds once a fused array
/// increment entry of the method first declined (interpreter round i3 waves
/// 75 and 77, lane L7; read and written through
/// `vm::runtime::interpreter::field_fast` only).
pub struct ArrayIncMemo {
    /// One bit per byte of the method's code: set at the pc of an entry whose
    /// group is refused for good (wave 75's shape declines, and since wave 77
    /// a deferred operand that missed its budget).
    pub declined: Box<[std::sync::atomic::AtomicU64]>,
    /// One saturating byte per byte of the method's code: how often the
    /// entry at that pc failed to read its deferred operand (a cold static
    /// site, a null or short operand array). Allocated at the first such
    /// miss under `CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`; never
    /// touched by a hit.
    pub operand_misses: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU8]>>,
}

impl ArrayIncMemo {
    /// An empty memo for code of `code_len` bytes (every bit clear, no miss
    /// counter yet).
    pub fn new(code_len: usize) -> Self {
        Self {
            declined: (0..=code_len / 64)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            operand_misses: std::sync::OnceLock::new(),
        }
    }
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
            hidden_frame: self.hidden_frame.clone(),
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
            interp_invocations: std::sync::atomic::AtomicU32::new(
                self.interp_invocations
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            // Carried forward for the reason `jit_probe_generation` is: the
            // stamp is generation-keyed, so an inherited stale one simply fails
            // `tiering_settled` and the clone asks the manager again.
            tiering_settled: std::sync::atomic::AtomicU32::new(
                self.tiering_settled
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            // The arm is a property of the method, which the clone shares.
            branch_profile_armed: std::sync::atomic::AtomicBool::new(
                self.branch_profile_armed
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
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
            // The clone shares `code`, so it indexes the same pool.
            pool_generation: self.pool_generation,
            // Advisory (see the field): the clone starts empty and relearns
            // its declines at one helper call each.
            array_inc_declined: std::sync::OnceLock::new(),
            ctor_verdict_memo: std::sync::OnceLock::new(),
        }
    }
}

/// The identity and `Code`-attribute half of a [`CachedBytecodeMethod`] — every
/// field a producer actually decides. [`CachedBytecodeMethod::from_parts`]
/// adds the memo cells, all empty.
///
/// The memo half is ten cells that every producer initialises identically, and
/// the ~25 struct literals that spelled them out meant each new cell was a
/// ~25-site edit across several owners (interpreter round i1, wave 3, L7).
/// Named fields rather than a positional `new` because six of them are
/// adjacent `u16`/`bool` pairs a positional call could swap silently.
pub struct CachedMethodParts {
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
}

impl CachedBytecodeMethod {
    /// A fresh entry for `parts`, every memo cell empty — exactly what the
    /// struct literals it replaces wrote out by hand.
    pub fn from_parts(parts: CachedMethodParts) -> Self {
        let CachedMethodParts {
            declaring_class_id,
            class_name,
            method_name,
            method_descriptor,
            source_file,
            code,
            exception_table,
            max_stack,
            max_locals,
            num_params,
            is_synchronized,
            is_static,
        } = parts;
        Self {
            declaring_class_id,
            class_name,
            method_name,
            method_descriptor,
            source_file,
            code,
            exception_table,
            max_stack,
            max_locals,
            num_params,
            is_synchronized,
            is_static,
            force_native_cache: std::sync::OnceLock::new(),
            hidden_frame: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
            array_inc_declined: std::sync::OnceLock::new(),
            ctor_verdict_memo: std::sync::OnceLock::new(),
            pool_generation: Self::POOL_GENERATION_UNKNOWN,
        }
    }

    /// [`Self::pool_generation`] of an entry whose producer did not say which
    /// constant-pool generation its code indexes.
    pub const POOL_GENERATION_UNKNOWN: u64 = u64::MAX;

    /// This entry, naming the constant-pool generation its code indexes:
    /// `generation` must be the class-redefinition count read under the
    /// class-manager guard the code was copied under (interpreter round i1
    /// wave 43, lane L2; see [`Self::pool_generation`]).
    #[must_use]
    pub fn with_pool_generation(mut self, generation: u64) -> Self {
        self.pool_generation = generation;
        self
    }

    /// [`Self::pool_generation`], or `None` when the producer did not know it.
    #[inline]
    pub fn known_pool_generation(&self) -> Option<u64> {
        (self.pool_generation != Self::POOL_GENERATION_UNKNOWN).then_some(self.pool_generation)
    }

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
    pub fn native_call_site(&self) -> &cratonvm_types::NativeCallSite {
        self.native_callback_cache
            .get_or_init(cratonvm_types::NativeCallSite::new)
    }

    /// The memoized JIT invocation-counter key for this method — see
    /// [`Self::invoc_key`]. Computed on first use, then read straight out of the
    /// `OnceLock`.
    ///
    /// Every door that counts this method must compute the same key, because
    /// they all key the *same* `ProfileStore` invocation counters: a door with a
    /// different key silently counts a method's warmup in a counter nobody
    /// else reads. [`invoc_key_parts`] is the one definition.
    #[inline]
    pub fn invoc_key(&self) -> u128 {
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
    /// `current_generation` must come from the owning VM's
    /// `JitCache::generation()` (an `Acquire` load). Per cache, so another VM's
    /// compilations do not invalidate this VM's memo. A `true` answer means the caller may skip the
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

    /// JIT round 19 wave 7 (lane handler7, SYNCDOOR4-3): the raw memo word
    /// [`Self::jit_probe_generation`] holds: a bare generation (a miss, see
    /// [`Self::record_jit_probe_miss`]) or a generation tagged
    /// [`JIT_PROBE_FOUND_TAG`] (a hit, see [`Self::record_jit_probe_found`]).
    #[inline]
    pub fn jit_probe_word(&self) -> u64 {
        self.jit_probe_generation
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// SYNCDOOR4-3: record that a `JitCache::get` for this method found a body
    /// a by-name tail enters (not a wrapped entry) while the cache was at
    /// `generation`, tagged [`JIT_PROBE_FOUND_TAG`] so no miss reader
    /// ([`Self::jit_probe_is_current`]) can mistake it for a miss, and also
    /// [`JIT_PROBE_STAND_DOWN_TAG`] when the door that found it may stand down
    /// on the memo alone next time (`CRATONVM_JIT_BC_CALLEE_STAND_DOWN_MEMO`,
    /// read once per generation by the recorder). Sound for the reason the
    /// miss memo is: every publication and invalidation bumps the generation,
    /// so while it is current the body found is still the one published.
    #[inline]
    pub fn record_jit_probe_found(&self, generation: u64, stand_down: bool) {
        debug_assert!(generation & (JIT_PROBE_FOUND_TAG | JIT_PROBE_STAND_DOWN_TAG) == 0);
        let tags = if stand_down {
            JIT_PROBE_FOUND_TAG | JIT_PROBE_STAND_DOWN_TAG
        } else {
            JIT_PROBE_FOUND_TAG
        };
        self.jit_probe_generation
            .store(generation | tags, std::sync::atomic::Ordering::Relaxed);
    }
}

/// JIT round 19 wave 7 (lane handler7, SYNCDOOR4-3): the tag
/// [`CachedBytecodeMethod::record_jit_probe_found`] sets on the memo word for a
/// HIT. A JIT-cache generation starts at 1 and counts publications, so it never
/// reaches bit 62; a tagged word therefore never equals a live generation and
/// every miss reader ([`CachedBytecodeMethod::jit_probe_is_current`]) still
/// asks the cache, exactly as before the tag existed.
pub const JIT_PROBE_FOUND_TAG: u64 = 1 << 63;

/// SYNCDOOR4-3: with [`JIT_PROBE_FOUND_TAG`], the hit was recorded under
/// `CRATONVM_JIT_BC_CALLEE_STAND_DOWN_MEMO`: the bytecode-callee doors stand
/// down on the memo alone while its generation is current.
pub const JIT_PROBE_STAND_DOWN_TAG: u64 = 1 << 62;

/// The canonical `ProfileStore` invocation-counter key for a method.
///
/// Single source of the hash, because several sites key the SAME counters and a
/// divergent hash would silently reset every method's warmup count rather than
/// fail: [`CachedBytecodeMethod::invoc_key`] memoizes this, and the interpreter's
/// back-edge tier-up path recomputes it from a live frame (where no
/// `CachedBytecodeMethod` is in hand). Keep them bit-identical.
///
/// `(declaring_class_id << 64) | fingerprint`, where the fingerprint is a
/// 64-bit FNV-1a over the name, a `0xFF` separator and the descriptor, passed
/// through the splitmix64 finalizer so the low bits the store shards on are
/// well mixed. The key used to be a 32-bit `31 * h` fold of the concatenated
/// strings: overloads could share a counter (short strings collide by
/// construction -- `"Aa"` and `"BB"` fold alike), and with no separator a name
/// and descriptor could trade characters. `0xFF` never occurs in modified
/// UTF-8, so the boundary is unambiguous. The class id stays whole in the high
/// half because `ProfileStore::invalidate_class` sweeps by it.
#[inline]
pub fn invoc_key_parts(
    declaring_class_id: u32,
    method_name: &str,
    method_descriptor: &str,
) -> u128 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in method_name.as_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h = (h ^ 0xFF).wrapping_mul(FNV_PRIME);
    for &b in method_descriptor.as_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    (u128::from(declaring_class_id) << 64) | u128::from(h)
}

