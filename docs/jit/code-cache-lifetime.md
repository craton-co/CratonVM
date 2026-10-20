# JIT code-cache lifetime and ownership

Who owns compiled code and its side tables, what keeps each baked address
alive, in what order a body may be reclaimed, and what stops a completed
compilation of retired bytecode from being installed.

**Companion documents.** `docs/jit/code-cache-lifecycle.md` covers the
*retirement protocol* (quiescence, W^X, the counter inventory) on the
`vm/src/jit/` side. This document covers the *jit-crate* side: the census of
every raw address the compiler bakes, the arena that backs the inline-cache
slots, the reclamation order in `jit/src/lib.rs`, and the installation gate.
Where they overlap, that one is authoritative on the sweep and this one on the
producers.

**Why the two exist separately.** This branch has already produced, in this
area alone: two use-after-frees in code reclamation; a JIT-baked direct call
target that had to be pinned or refused; JIT code unmapped while executing
(which needed its own diagnosis recipe, `pc == addr`); and an unguarded
JIT-to-JIT call that misattributed the innermost frame. The defect density is
high and the failure mode is always the same — a wild jump, far from the cause.
A census that lives next to the code is the only thing that makes the next one
findable by reading rather than by crashing.

---

## 1. The baked-address census

"Baked" means: written into machine code as an immediate, or written into a
side table that machine code loads from without any further validation. Both
have the same failure mode, so both are listed.

### 1.1 Baked as an immediate

| Address | Baked where | What pins the target | If freed while a frame is inside |
|---|---|---|---|
| Compiled callee entry (raw JIT→JIT `CALL`) | `x64.rs` direct-call ladder; `ir_lower.rs` `Op::Call` direct arm | `CompiledMethod::_direct_callee_roots` — one `Arc<CompiledMethod>` per entry, resolved at publication by `JitCache::prepare_for_publication`. **Publication is refused** if any entry fails to resolve (`strict_callee_roots_enabled`, default-ON) | Cannot happen: the caller's own `Arc` transitively owns the callee's buffer for the caller's whole life |
| Runtime helper (`new`, monitor, dispatch, frame-record, callee-deopt service) | `runtime_lowering.rs`; `x64.rs`; `ir_lower.rs` | Nothing needs to: `JitRuntimeHelpers` are `extern "C"` items with VM lifetime | n/a |
| Thin direct-native helper (`INTEGER_VALUE_OF_DIRECT_FN` and eight siblings) | `try_compile_inner`, via `direct_native_helper` | Same — a VM-installed function address, never unmapped. Deliberately **not** in `_direct_callee_entries`: it has no `Arc` owner, so listing it would make `prepare_for_publication` refuse every artifact that uses one | n/a |
| Intrinsic sentinel (`JitIntrinsic::*::as_entry()`) | `try_compile_inner` | Not an address at all — `usize::MAX - n`, matched by the codegen ladder, never called | n/a |
| `*const JitInvokeInfo` | dispatch-helper argument marshalling in both backends | The artifact's own `_jit_invoke_infos: Vec<Box<JitInvokeInfo>>` | n/a — dies with the code that names it |
| `*const JitMICSlot`, `*const JitPICSlot` | inline-cache cascades in `x64.rs`, `ir_lower.rs`; `runtime_lowering::emit_hashed_vtable_stub` | The artifact's own `_jit_mic_slots` / `_jit_pic_slots: Vec<Box<..>>` | n/a — same lifetime as the code |
| `*const str` inside a `JitInvokeInfo` | built in `try_compile_inner` / the VM's OSR compile | `_jit_strings: Vec<Box<str>>` on the same artifact | n/a |
| `*const DeoptimizationPoint` | frame-deopt stubs (`deopt_real` only) | `_deopt_point_boxes: Vec<Box<..>>`, plus the `DeoptEpochGuard` version check that runs **before** the box is dereferenced | Guard refuses the resume and falls back to a whole-method re-run |
| `*const DeoptEpochGuard` | 4th arg of every frame-deopt stub | Deliberately leaked, process lifetime | n/a |
| Object-layout / trip-count / stride constants | everywhere | Immediates in the artifact's own buffer | n/a |

### 1.2 Loaded from a side table, then called

This is the class that actually bites, because the value can change or be
withdrawn while the code that reads it is running.

| Value | Read by | Retained by | Withdrawal path |
|---|---|---|---|
| `JitMICSlot::cached_entry_ptr` | `MOV R11,[slot+8]; CALL R11` | `JitMICSlot::compiled_owner` | `clear_compiled_entry` zeroes the entry (Release) **then** releases the owner through `defer_jit_owner` |
| `JitPICSlot::entry_ptrs[i]` | 4-way inline cascade | `compiled_owners[i]` | `invalidate_targets` / `clear_entries`, same order |
| `JitPICSlot::mega_entry_ptrs[i]` | `emit_hashed_vtable_stub`'s two-way probe | `mega_compiled_owners[i]` | same order; the emitted probe `TEST R11,R11` before calling, so a zeroed way degrades to the resolving helper |
| `DispatchCache::entry` (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`, `vm/src/jit/helpers.rs`) | `try_mic_rust_cached_entry` and both dispatch-helper arms, which read the raw address out of the map **without cloning the owner** | `DispatchCache::_owner: Option<RetainedCode>` | `HashMap::clear` in `flush_raw_entry_dispatch_caches` / `flush_class_identity_dispatch_memos`, plus replacement and thread exit — all via `RetainedCode::drop` |
| `CachedInvokeTarget::Jit::compiled` (`JvmThread::invoke_cache`) | the interpreter's cached invokestatic/invokevirtual arms | itself, as a `RetainedCode` | `get`'s staleness auto-evict, `put` replacement, `evict`, `clear`, thread exit |

The last two are the ones the `jit`-crate inventory above cannot see, and they
are the ones that have broken before. They are worse than the inline-cache
slots in one specific way: **a per-thread cache is evicted by the thread
dispatching through it**, and both eviction paths run *from* the dispatch
helper, i.e. from inside compiled code. So the eviction can run while a frame of
the evicted body is on that same thread's stack. `RetainedCode` is what makes
their release go through the retirement queue rather than through a bare `Arc`
drop; see
`jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`.

Every install into all three funnels through `jit_entry_publishable`, which
refuses an address that is a live `jit_entry_owners` key with a dead `Weak`, or
that lies inside a live JIT code region, when no owner could be retained. Those
two shapes mean "this WAS a compiled body and we failed to keep it", which is
exactly the stale-entry crash. An address matching neither is a native
trampoline or a unit-test sentinel and is admitted (and counted in
`unowned_ic_entry_publishes`).

**Fixed this pass.** `JitPICSlot::seed_from_mic` read the MIC's entry pointer
*before* its owner, and did not re-run `jit_entry_publishable` on the result.
`clear_compiled_entry` zeroes the entry and only then takes the owner, so the
old read order admitted the interleaving that copies a live raw address into
the PIC together with an owner that had already been released — a callable
pointer with no keep-alive, in a slot generated code `CALL`s without further
validation. Reading owner-then-entry makes that interleaving impossible (if the
owner read lands after the take, the entry read, which follows it, necessarily
lands after the zeroing), and the `jit_entry_publishable` re-check stops the
promotion from laundering a refusal the MIC itself would make today.
`seeding_a_pic_refuses_a_mic_entry_that_lost_its_owner` pins it, with
`seeding_a_pic_carries_a_well_formed_mic_entry_forward` as the control.

Reachability: `seed_from_mic`'s two in-tree callers are both in
`try_compile_inner` and operate on a freshly-constructed local MIC with no
concurrency and no entry pointer, so the defect was latent rather than live.
`promote_mic_to_pic` is public and does exactly the racy thing.

---

## 2. The inline-cache slot arenas

**Claim to check:** *"a `Vec` reallocation invalidates every outstanding pointer
into it — check whether the arena can grow after pointers are handed out."*

**Verified by reading: it cannot bite, because every arena is a `Vec` of `Box`,
never a `Vec` of the payload.** Growth moves the `Box` handles; the pointees do
not move. The relevant declarations:

* `CompiledMethod::_jit_mic_slots: Vec<Box<JitMICSlot>>`
* `CompiledMethod::_jit_pic_slots: Vec<Box<JitPICSlot>>`
* `CompiledMethod::_jit_invoke_infos: Vec<Box<JitInvokeInfo>>`
* `CompiledMethod::_jit_strings: Vec<Box<str>>`
* `CompiledMethod::_deopt_point_boxes: Vec<Box<DeoptimizationPoint>>`

(All in `pub struct CompiledMethod` in `jit/src/lib.rs`; line numbers are not
given because that file moves by hundreds of lines a week.)

(`JitCache::string_arena` / `invoke_info_arena` also used to be listed here.
They had no callers and were deleted on 2026-09-12.)

The compile-time staging vectors have the same shape: `owned_mic_slots:
Vec<Box<JitMICSlot>>` / `owned_pic_slots` in `try_compile_inner`,
`ir_mic_boxes` / `ir_pic_boxes` on the IR path, `cloned_mic_slots` /
`cloned_pic_slots` on the `x64::Compiler` (transferred at
`x64.rs:25505`). A raw pointer is taken from `&*boxed` *before* the box is
pushed, and the push cannot move it.

**Who can hold a slot pointer, and for how long.**

1. **The artifact's own emitted code.** Same lifetime by construction.
2. **A dispatch helper, for the duration of one call.** `vm/src/jit/helpers.rs`
   receives `mic_ptr` / `pic_ptr` as `i64` arguments and dereferences them
   inside the call; a workspace grep finds no site that stores one. The caller
   is a frame *inside* the owning artifact, so the artifact cannot be reclaimed
   underneath it — see §3.
3. **Nothing else.**

**The replicated-loop case.** `x64/licm.rs`'s side-table replication duplicates
a pc-keyed entry once per loop-body image and CLONES the payload, so several
output PCs share ONE slot. That is correct and it does not weaken the lifetime
argument: all copies live in the same artifact, and the slot is owned by that
artifact's `_jit_pic_slots`. There is no configuration in which one copy
outlives another.

**What is *not* proven here.** Every per-artifact arena above is dropped with
its artifact. The process-wide intern tables are not: `intern_typecheck_target`
leaks one string per distinct `(class name, resolved ClassId)` and
`TYPECHECK_TARGET_BY_SITE` one row per such string, because the emitted code
and the runtime helper recover the resolved id from the name pointer. Interning
by name only would need the per-site id carried in the artifact and passed by
the emitters. That is a bounded leak, not a safety defect.

---

## 3. Reclamation ordering

### 3.1 The order, as implemented

`CompiledMethod::drop`'s body runs, in this order, **before** any field is
dropped (and therefore before the mapping is released — field drop follows the
`Drop` impl body):

1. `report_stale_ic_holders(entry)` — diagnostic, `CRATONVM_DBG_JIT_STALE_IC`.
2. `unregister_jit_code_range(entry)` — withdraw from the GC's range registry.
3. `release_compile_id(compile_id)` — withdraw the compile-id binding the
   prologue publishes beside its RBP, so a mirror still holding the id
   resolves to `None` rather than to a freed artifact. The id is reissued only
   after grace.
4. `unregister_jit_method_name(entry)` — see §3.4.
5. `implicit_null::unregister_range(entry, len)` — an implicit null-check
   entry that outlived its buffer would match a PC of whatever
   `alloc_executable` hands out next, and the signal handler would resume
   there.
6. Remove the `jit_entry_owners` key, *only* if its `Weak` no longer upgrades.
   The condition is what makes address reuse safe: a key that resolves to a
   live artifact belongs to a newer body and must not be removed.
7. Purge `osr_trampoline_cache` entries targeting `[entry, entry+len)`.
8. Field drop, in DECLARATION order. `_buffer` is the struct's first field,
   so `ExecutableBuffer::drop` — which withdraws the debugger/profiler symbols
   (`code_events::retire`), deregisters the region and unmaps — runs FIRST,
   and the side tables go after it: the strings, the invoke-info and slot
   boxes, `_direct_callee_roots` (which may cascade into another artifact's
   drop), the deopt boxes. That order is safe because steps 1–7 already
   withdrew every lookup that could lead a reader from an address to those
   tables, and nothing executes the body by the time its last owner drops.
   (An earlier revision of this list said the unmap came last; it never did.)

### 3.2 Are all frames guaranteed out of it?

Not by this `Drop` — by the queue in front of it. Every retirement site hands
the artifact to `defer_jit_owner`, which drops it immediately only when
`ACTIVE_JIT_EXECUTIONS.is_zero()` and otherwise queues it, stamped with a
retirement generation, until `drain_deferred_jit_owners` can show every thread
is outside it: the striped sum reads zero, or per-thread evidence (depth zero,
back at depth zero since the stamp, or parked in a blocking transition with a
stack summary that does not name the body). See
`docs/jit/code-cache-lifecycle.md` §"The quiescence signals". The sites are:
`JitCache::put` and `put_osr` (superseded body), `invalidate_matching` (both
maps) and `clear_all` (all through `retire_withdrawn_body`), the inline-cache
withdrawal paths (`JitMICSlot::update` replacing an owner and
`clear_compiled_entry`; `JitPICSlot`'s retired ways and megamorphic entries,
`clear_entries`, `invalidate_targets`; a PIC way is written once and never
evicted), and every `cratonvm_jit::RetainedCode` drop, which is how the two
`vm/`-side per-thread dispatch caches in §1.2 release their keep-alive.

The queue is the backstop; the primary argument is ownership. A thread inside a
compiled body always holds an owning `Arc<CompiledMethod>` for it, so a
reference count reaching zero is itself a proof that no thread is inside.
`published_code_free_audit().1` counts every release that reached the OS without
the queue's authorisation and must be zero.

`drain_deferred_jit_owners` (formerly `drain_deferred_jit_owners_if_quiescent`)
takes the queue lock **before** reading the quiescence counter or the
per-thread records, and holds it across both. That ordering is the
correctness argument, not a performance choice: reading `is_zero()` first would
let a thread observe zero at `t0`, be descheduled while a peer enters JIT at
`t1` and a third unpublishes-and-queues a body at `t2 > t1`, then resume and
free that body on a witness that predates its own unpublication.
`docs/jit/code-cache-lifecycle.md` §"Reconciliation 1" used to describe that
window as still open; it was closed, the reasoning is in the function's
comment, and that section now says so (§6.5).

`defer_jit_owner`'s own fast path reads `is_zero()` without the queue lock.
That is sound *only because* every caller has already unpublished the body
before calling, so each thread's stripe-read witness postdates the
unpublication and a thread outside compiled code at its witness instant can
only re-enter through a surface that has been cleared. Nothing in the signature
enforces the precondition; it is stated in the function's comment.

### 3.3 Are the oop maps and deopt tables still reachable by an in-flight collection?

The dangerous shape is a collector holding a stale `(range → CompiledMethod*)`
pair across the artifact's drop. Reading the registry:

* `jit_code_ranges_snapshot()` and `snapshot_code_ranges_into()` return
  `(start, end)` pairs only. **No owner pointer escapes into a cached
  snapshot**, so the whole-stack conservative sweep — the one that caches
  across a whole scan and re-validates via `jit_code_ranges_generation()` —
  cannot hold a dangling metadata pointer. The worst a stale range does is
  classify a freed address as in-JIT, which is the conservative direction.
* `lookup_jit_code_range(addr)` **does** return the `Arc<CompiledMethod>` inner
  address, as a bare `usize` with no keep-alive. It is a fresh lock-free load
  each call, and step 2 above withdraws the range before the mapping is
  released, so the lookup itself is sound. The residual is the window between
  the lookup returning and the caller dereferencing: today it is closed only
  indirectly, by `defer_jit_owner` declining to reclaim while any thread is in
  JIT — which holds for a root scan of a *running* JIT frame, but is not an
  invariant the signature states or a caller can check.

**Added this pass:** `pin_jit_code_range_owner(addr) -> Option<Arc<CompiledMethod>>`
— the same lookup with the keep-alive attached. `None` is the safe answer for
"no live body covers this address"; the alternative was a raw address.

**Update.** The "closed only indirectly, by `defer_jit_owner`" residual
above was not merely untidy — it was the bug. `try_call_compiled_entry_reentrant`
(the JIT→JIT dispatch helper) dereferenced the bare `usize` and entered the
callee holding no reference at all, on a `SAFETY` comment claiming the registry
owned the artifact. That made JIT→JIT dispatch the one way into a compiled body
that holds no owning reference to it, and it is the mechanism behind
`jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`.

It now pins. To make that affordable per dispatch, `JitCodeRange` carries a
`Weak<CompiledMethod>` and `pin_jit_code_range_owner` upgrades it lock-free
(one atomic increment); the `jit_entry_owners` mutex is now only the fallback
for ranges registered without an owner. The per-stack-word sweep still wants
`snapshot_code_ranges_into` and its zero-owner-pointer guarantee. **The
remaining vm-side call sites — see §6.2.**

### 3.4 The name registry was leaking, and lying

`register_jit_method_name` had no withdrawal. The `JIT_NAME_RANGES` `Vec` grew
for the process lifetime, and — the half that matters — kept naming an address
after the body was unmapped. Because `lookup_jit_method_name` returned the
*first* covering range, a crash inside a **new** body at a recycled address was
reported under the **dead** body's name. A crash report that names the wrong
method is worse than one that names none, because it is believed; that is
precisely the failure this registry exists to prevent.

Fixed on both axes: `unregister_jit_method_name` is called from
`CompiledMethod::drop`, and both lookups now search from the end so the newest
registration for an address wins even if a withdrawal is ever missed. The
registry is populated only under `CRATONVM_DBG_JIT_NAMES`, so this is
diagnostic-quality, not a runtime safety fix — but the diagnostic is what
someone reads at 2 a.m. after a `pc == addr` fault.

### 3.5 What was already right

* `put` / `put_osr` register the code range and the `jit_entry_owners` weak
  **before** publishing the shard snapshot, so a peer that resolves an entry can
  immediately be stack-walked.
* `invalidate_matching` computes the transitive reverse closure over
  `_direct_callee_entries` *before* publishing any new snapshot, retargets every
  inline cache, and only then withdraws ownership. A reader holding an old
  caller snapshot either misses after the release publication or keeps the
  callee alive through the slot's strong owner.
* `invalidate_matching`'s empty-set early return is correct by construction and
  is load-bearing for throughput (~31% of CPU on an AOT-codegen workload).

### 3.6 Pins taken for a body that is then not published (round 9)

`prepare_for_publication` pins every baked callee (`Weak::upgrade` on the
`jit_entry_owners` row) BEFORE it knows whether the caller will publish. On a
refusal — a callee already retired by an invalidation, a callee whose address
was rebound to a different artifact, or the strict-roots rule refusing an
artifact with an unpinnable callee — those pins used to be dropped as bare
`Arc`s with the discarded artifact. A pin on a retired callee is exactly the
reference that can be that body's LAST owner (the retirement queue may drain
its own reference while the pin is held), and a bare last drop returns a
published mapping outside any authorised reclamation. It is not a
use-after-free — the callee was unpublished before its queue entry was
released — but it is counted by `published_code_free_audit().1`, which must
stay zero, and a false positive there is indistinguishable from the real
thing.

They now go through `release_unpublished_pins`, i.e. `RetainedCode`'s drop:
a plain drop when another owner remains, the retirement queue when the pin is
the last. The owner map is also taken ONCE per publication instead of once per
baked call, and no `Arc` is dropped while it is held (a last drop runs
`CompiledMethod::drop`, which takes the same non-reentrant lock).

### 3.7 The shard maps hold `RetainedCode` (round 9 wave 2)

`JitCacheMap` stores `(JitKey, RetainedCode)`, not `(JitKey, Arc<CompiledMethod>)`.
A reader (`get`, `get_osr`, `snapshot_entry_counts`, `sweep_cold_bodies`) holds
an `arc_swap` guard on a shard map for the length of its lookup, and a
publisher that supersedes or withdraws a body meanwhile leaves the OLD map —
alive only through that guard — owning a reference to the withdrawn body. The
publisher's `retire_withdrawn_body` then drops a non-last reference, and the
reader's guard drop was the body's LAST one: a published mapping released
outside the retirement queue, i.e. a false positive in
`published_code_free_audit().1`. `invalidate_matching_collecting` had already
fixed its OWN guard (`drop(current)` before retiring); other threads' guards
were out of its reach, and `put`/`put_osr`/`clear_all` had the same shape.

With `RetainedCode` in the map, whichever thread drops the last map reference
routes the body through `defer_jit_owner` by construction. The public API is
unchanged: `get`/`get_osr` still hand out `Arc<CompiledMethod>` clones (a
caller that holds one and may drop it last should wrap it in `RetainedCode`
too, which is the pre-existing rule for every holder). Regression test:
`jit/tests/r9w2_runtime2_cache_reader_guard.rs`.

### 3.8 A refused RW->RX flip declines the compile (round 9 wave 2)

`ExecutableBuffer::finalize` aborts the process when `mprotect` /
`VirtualProtect` refuses the flip. `ExecutableBuffer::try_finalize` and
`CompiledMethod::try_new` / `try_new_with_context` return the failure instead
(counted in `code_buffer_protect_failures()`, first one reported on stderr);
the refused buffer is still RW and unpublished, so dropping it is an ordinary
unmap (an arena block is made writable again by the arena's own release). The
backends' constructor calls switch to the `try_` forms in their own lanes —
see `docs/internal/jit-review-r9/NOTES-w2-runtime2.md`. Until they do, the
abort remains the behaviour of a backend that calls `CompiledMethod::new`.

### 3.9 The last `RetainedCode` drop, after the merge with dev (2026-09-19)

Round 9 wave 2 made `RetainedCode::drop` decide "am I the last handle?" with a
per-body `retained_holders` count. The non-last path then released through
`Arc::into_inner` and re-wrapped the value into a new `Arc`. Dev replaced that in
parallel, and the merge took dev's version:
- **Why the re-wrap is wrong:** it frees the ORIGINAL allocation at once, while the
  code-range registry still holds that allocation's raw address
  (`register_jit_code_range(entry, len, Arc::as_ptr(..))`).
- **What `drop` does now:** it drops plainly when `Arc::strong_count > 1`, and routes
  through `defer_jit_owner` otherwise. The `retained_holders` field is gone.
- **Why that is safe:** two concurrent last-two drops can still both drop plainly, but
  `ExecutableBuffer::drop` rescues a published mapping released outside the queue INTO
  the queue (`defer_orphaned_mapping`). The late unmap cannot become a use-after-free.
- **What the audit shows:** `published_code_free_audit().1` still counts each rescue, so
  it is no longer expected to read zero. `jit/tests/r9w2_runtime2_cache_reader_guard.rs`
  bounds it: fewer than 50 per 500 supersedes.
- **To get back to zero:** an exact last-holder decision that keeps the original
  allocation alive.
- **Pinning:** `_direct_callee_roots` is now `Vec<RetainedCode>`. The publication pin
  (`prepare_for_publication`) keeps round 9's single `jit_entry_owners` lock pass, and a
  refused pin is released through the queue (`release_unpublished_pins`).

### 3.10 Retire cells for in-loop direct calls, default ON (round 9 waves 9-11)

A running compiled frame keeps its baked direct calls for as long as it runs. When
a callee is withdrawn (deopt eviction, `MakeNotCompilable`, sweep), a caller that
is still looping, typically an OSR'd `main`, used to re-enter the withdrawn body
on every iteration.

- **What a retire cell is:** a baked `invokestatic` direct call inside a loop calls through
  a cell (`retirable_direct_call` in `jit/src/lib.rs`, guard id `JIT_RETIRE_CELL_GUARD`).
- **What withdrawal does:** it clears the cell, so the next call takes
  `jit_invoke_dispatch` and reaches the newest body or the interpreter.
- **Context-free callees:** since wave 10 they get a cell too. The cell carries the callee's
  own ABI flag, because `admits` compares it.
- **Default and measurement:** ON since wave 11, with `CRATONVM_JIT_RETIRE_CELL=0` to opt
  out. `TryLoop throw` went from 8 500 to 2 400 ms. The rest of that probe's cost was the
  OSR guard-exit charging in `docs/jit/on-stack-replacement.md` §8.
- **The mutual-recursion counterpart:** cycle-edge cells (`CRATONVM_JIT_CYCLE_EDGE_CELL`,
  default ON since wave 7).

---

## 4. Redefinition: the installation answer

**The question.** A separate lane established that the compilation broker has no
invalidation for a redefined class's queued or in-flight requests — no epoch
stamp anywhere, so a completion cannot notice the bytecode it compiled was
replaced. What stops a completed compilation of stale bytecode from being
installed and called?

**The answer before this pass: nothing.** Verified by reading:

* `vm/src/vm/vm_exec.rs::redefine_class` calls `cratonvm_jit::bump_redefine_epoch()`
  and then `jit_cache.write().clear_all()`. Both act on state that exists *now*.
* `vm/src/vm/vm_init.rs::jit_invalidate_adapter` calls `clear_all()` on every
  live VM after a synthetic-stub **layout** upgrade — and does not touch the
  redefine epoch at all.
* `JitCache::put` and `put_osr` had exactly two admission checks:
  `prepare_for_publication` (baked callee roots) and, for `put_osr`, a
  `debug_assert!(compiled.compiled_via_osr)`. Neither has anything to do with
  the age of the compilation's inputs.
* `CompiledMethod::compilation_epoch` is **not** this. It is the deopt-osr
  per-method speculation epoch, stamped at install by
  `interpreter.rs::stamp_compilation_epoch` only under `CRATONVM_DEOPT_REAL`,
  and it gates *deopt resume*, not installation. It is `0` on every production
  artifact.

So a compile that read the pre-redefinition bytecode, finished after the flush,
and published was accepted unconditionally and called. For a JVMTI redefine that
means running the bytecode the agent replaced. For a layout upgrade it is worse:
the body's baked field offsets no longer describe the class.

**The answer now.**

* `CompiledMethod::install_epoch` — the value of the process-wide
  `JIT_INSTALL_EPOCH` when this artifact's **compilation began**.
* `open_compile_epoch_witness()` — a re-entrant RAII scope opened at the top of
  `try_compile_with_invokespecial_resolver`, before any constant-pool resolver
  runs. Both `CompiledMethod` constructors read it. A nested compile (the
  `callee_compiler` recursion) keeps the outermost — oldest — epoch, which can
  only refuse more.
* `bump_redefine_epoch()` and `JitCache::clear_all()` advance
  `JIT_INSTALL_EPOCH`.
* `JitCache::flush_barrier` — raised by `clear_all` to the post-bump value,
  **under `mutation`**, the same lock both publication paths hold. So a
  publication either completes entirely before the flush (and is then wiped by
  it) or observes the raised barrier. There is no third outcome.
* `JitCache::publication_epoch_is_current` refuses any artifact stamped below
  the barrier, counting `stale_install_epoch_refusals()`.
  `CRATONVM_JIT_STRICT_INSTALL_EPOCH=0` restores publish-anyway for bisection.

**Cost of a refusal:** one wasted compile. The method stays interpreted and
recompiles on a later invocation, against the bytecode that is actually
installed — the identical shape and identical cost as
`strict_callee_roots_enabled`'s refusal. Every `put` call site already reads the
published artifact back out of the cache with `get(..)?` rather than reusing the
address it held before the `put`, so a refusal degrades to "no compiled body"
and the interpreter continues. That readback was introduced for the concurrent-
publish race; it is what makes a second refusal reason free.

**Why the barrier is per-cache and the counter is global.** A global watermark
would let one VM's redefinition refuse an unrelated VM's publication — and, in
the lib test binary, would let any `clear_all` test randomly fail any concurrent
`put` test. `jit_invalidate_adapter` already fans `clear_all` across every live
VM, so a real flush still arms every cache.
`flushing_one_cache_does_not_refuse_another_caches_publication` pins it.

---

## 5. Other findings

**`_direct_callee_entries` was assigned, not extended, on the IR path.**
`try_compile_inner`'s IR arm did `compiled._direct_callee_entries = take(..)`
three lines below a comment explaining, for the sibling slot transfer, that it
must be `extend` and never assign for exactly this reason. An assignment
silently discards anything the lowerer recorded on the artifact — and a
discarded entry is one `prepare_for_publication` will not pin, so the body would
be published with a baked `CALL` to an address nothing keeps mapped. The
lowerer records none today, which is why the assignment looked safe. Changed to
`append` + sort + dedup.

**`emit_hashed_vtable_stub` accepted a null slot base.** It bakes `pic` as
`MOV R10, imm64` and immediately dereferences it (`CMP EDX,[R10+RCX*4+96]`), so
a zero would fault inside generated code on the first megamorphic dispatch — at
a PC that names the method and an address that names nothing. Both callers
filter a null slot today; the stub now refuses one itself, emitting nothing and
leaving the site on its resolving helper. Fail-closed, and a third caller cannot
reintroduce the crash by omission.

**Not changed, deliberately.**

* `install_megamorphic` returns from the whole function when
  `jit_entry_publishable` refuses a way, instead of trying the second way. That
  costs one megamorphic way, never correctness.
* The `INSTALLING_CLASS_ID` (`u32::MAX`) reservation is not released if the
  installing thread unwinds between reserve and publish. The way is then
  permanently dead — generated code compares a real class id against `u32::MAX`
  and always misses, taking the helper. Wedged, not unsafe.
* `invalidate_matching` (`invalidate_for_class`, `invalidate_for_class_change`,
  `invalidate_unloaded_class`) does **not** arm the install barrier. It runs on
  every class define, so arming it there would refuse essentially every
  concurrent compile during bootstrap. Closing the same hole at that granularity
  needs a per-method barrier, not a per-cache one — see §6.
* ~~`CompiledMethod::_deopt_point_boxes`' doc comment claims `Drop` "leaks
  them"~~ — the comment has since been corrected: it now says they are dropped
  with the artifact, which the retirement queue frees only once no thread can
  still be executing it.

**`loop_analysis.rs`** holds none of these shapes at all: every value is a
bytecode quantity (bcis, CP indices, local masks, constants), no function takes
or returns a raw pointer, and what reaches emitted code is immediates in the
artifact's own buffer. The conclusion is recorded in its module doc so it does
not have to be re-derived.

---

## 6. What remains — cross-file changes this lane could not make

Each is a small, precisely-located edit outside `jit/src/{lib,loop_analysis,runtime_lowering}.rs`.

### 6.1 Widen the OSR compile's epoch window (one line, `vm/`)

`vm/src/runtime/interpreter/invoke.rs` (~line 15179) builds its OSR body by
calling `crate::jit::x64::compile_with_param_slots` directly rather than
`jit::try_compile`, so no compile witness is open and the artifact is stamped at
**buffer finalize** instead of at compile start. That is still a real gate — a
flush after finalize refuses it — but it misses the wide part of the window.

Fix: add, at the top of that compile block (before the constant-pool resolvers
run),

```rust
let _compile_epoch = cratonvm_jit::open_compile_epoch_witness();
```

and keep it alive until after `jit_cache.put_osr(..)`. No other change.

### 6.2 Route the GC's frame-metadata lookups through the pinning form (`vm/`)

Every caller of `cratonvm_jit::lookup_jit_code_range` that dereferences the
returned `usize` as a `*const CompiledMethod` should call
`cratonvm_jit::pin_jit_code_range_owner` instead and hold the returned `Arc`
across the use. Callers that only need "is this address in JIT code" should
keep using `snapshot_code_ranges_into`, which hands out no owner pointer at
all.

**Done:** `try_call_compiled_entry_reentrant` — the one that was not
a root scan but an *entry into the body*, and therefore the one where the
missing keep-alive was a use-after-free rather than a stale read. The pin is now
lock-free, so the cost objection that kept this on the "should" list no longer
applies to the rest.

**Still open:** the precise-map frame walk (`remap_active_jit_frames`) and the
cross-thread STW root scan (`vm/src/jit/xt_root_scan.rs`). Note that
`xt_root_scan` must *not* take a lock while a peer is frozen (see the comment at
its call site); the `Weak::upgrade` form is lock-free and therefore now
admissible there, which it was not before.

Do **not** change the per-stack-word conservative sweep: it is the one that
caches a snapshot across a whole scan, and it already takes the safe form.

### 6.3 Give the compilation broker an epoch (`jit/src/tiered.rs`, another lane's file)

The barrier added here catches a stale compilation at *installation*. It does
not stop the work being done. A queued `CompilationTask` should carry
`jit_install_epoch()` from the moment it is enqueued, and the worker should drop
a task whose epoch is below the current one before compiling it. That is a pure
throughput improvement over the current behaviour and belongs with whoever owns
the broker.

### 6.4 A per-method barrier for targeted invalidation (design, not a patch)

`invalidate_for_class*` deliberately does not arm the cache-wide barrier (§5).
Closing that hole needs the barrier keyed by method identity — the natural home
is the same `SharedVm::method_epochs` map `stamp_compilation_epoch` already
uses, extended so `put` can consult it. Worth doing only once there is evidence
of an in-flight compile surviving a CHA invalidation; nothing observed.

### 6.5 ~~Retire `docs/jit/code-cache-lifecycle.md` §"Reconciliation 1"~~ — DONE

Its "`drain_deferred_jit_owners_if_quiescent` has the ordering backwards"
finding was fixed; the lock is taken first and the argument is in the function's
comment. Leaving a fixed defect described as open is how a later session spends
a day re-fixing it. That section now says so, and its §3 — which asked for a
non-zero `active_jit_executions_at_free` to be treated as the protocol
violation — was corrected at the same time: that test is wrong in both
directions (73 of 90 real violations in one measured run reported zero).

---

## 7. Tests

`jit/src/lib.rs`, `mod code_cache_lifetime_tests`:

| Test | Pins |
|---|---|
| `a_compilation_that_predates_a_cache_flush_is_not_installed` | the P0: an artifact compiled before a flush must not publish |
| `a_compilation_that_starts_after_a_flush_installs_normally` | the control — the gate is not "refuse everything" |
| `the_compile_witness_stamps_the_start_epoch_not_the_finalize_epoch` | the stamp is compile-START, and outside a scope it tracks the live epoch |
| `a_nested_compile_keeps_the_outer_epoch` | `callee_compiler` re-entrancy |
| `flushing_one_cache_does_not_refuse_another_caches_publication` | the barrier's per-cache scope |
| `seeding_a_pic_refuses_a_mic_entry_that_lost_its_owner` | §1.2's ordering fix |
| `seeding_a_pic_carries_a_well_formed_mic_entry_forward` | its control |
| `a_retired_body_withdraws_its_crash_report_name` | §3.4 |
| `the_newest_registration_for_an_address_wins` | §3.4's belt-and-braces half |
| `pinning_a_code_range_owner_retains_the_artifact` | §3.3 — the pin survives the cache releasing the body |
| `pinning_an_unowned_code_range_yields_none` | and refuses when nothing can retain it |
| `pinning_an_unmapped_address_yields_none` | |

`jit/src/runtime_lowering.rs`: `a_null_pic_slot_emits_no_stub` and
`a_real_pic_slot_is_baked_as_the_probe_base`.

Every one is written to survive the rest of the binary running concurrently:
nothing asserts an absolute value of a process-global counter, nothing mutates a
process-global override, and no test compares two separately-read global reads
(the epoch tests compare two artifacts against each other instead). There are no
wall-clock bounds anywhere.

---

## Related

* `docs/jit/code-cache-lifecycle.md` — the vm-side retirement protocol, W^X,
  and the counter inventory. Its §"Reconciliation 1" is marked DONE; see §6.5.
* `docs/jit/compilation-broker.md` — the queue that needs §6.3.
* `docs/jit/deopt-metadata.md`, `docs/jit/deopt-frame-state-interning.md` — the
  boxes in §1.1's last three rows.
* `docs/threading/thread-transition-states.md` §7.2/§10.3/§10.4 — why
  `any_thread_in_jit()` over-approximates, and why narrowing it to `Rip`-based
  classification would start freeing live bodies.
* `jit/src/platform.rs` — the mapping primitives `ExecutableBuffer` wraps.
