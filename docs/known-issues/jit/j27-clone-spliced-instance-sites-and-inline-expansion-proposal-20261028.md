# Proposal: spliced instance `clone()` sites, and an inline clone expansion (j27 wave 3, 2026-10-28)

> **j27 wave 8 (lane W8SPL, 2026-10-28): REVIEWED, still PARTLY (item 2 instances NOT FIXED), PENDING HOST VERIFICATION (no code change).** Re-read the array expansion (`jit/src/ir_lower/clone_inline.rs`): the receiver screen (null, exact kind-tags byte, JVMTI allocation listeners, SATB pre gate for reference payloads) runs before the bump; the copy gets a FRESH header from the bump with the receiver's run-time class word (no identity hash or GC bits copied); the whole-word payload copy stays inside both objects because `ARRAY_DATA_OFFSET % 8 == 0` is a const assertion and allocations are 8-aligned; the receiver is re-read from its frame home after the bump, which has no GC point; reference payloads only on Generational with the copy young or no old object present, so no card mark is owed. No defect found. Instances (`CloneInstance`: per-class field copy with each collector's post barrier, an EA-visible or guarded exact class) are still not done: that is a new IR op plus lowering per collector and cannot be judged without a build and `ObjectCloneCost` timings, so it stays the open item of this page.

> **j27 wave 4 (lane CLONEIR, 2026-10-28): item 2 PARTLY (arrays FIXED IN CODE, opt-in; instances NOT
> FIXED), PENDING HOST VERIFICATION.** New switch `CRATONVM_JIT_IR_CLONE_INLINE` (opt-in: unexecuted in a VM;
> not registered yet). New module `jit/src/ir_lower/clone_inline.rs`: a guarded PREFIX of an optimizing-tier
> `Op::Call` that is a virtual `[..].clone()` already bound to a thin clone helper (`object_clone_virtual`, or
> `byte_array_clone` for `[B`), so it only replaces `array_clone_serve`'s served arm. The hit path makes no call
> and has no GC point: receiver non-null with the site's exact kind-tags byte; no JVMTI `VMObjectAlloc` /
> `SampledObjectAlloc` listener (new DATA word `DirectHelperTable::jvmti_alloc_events_word`, the VM's C JVMTI
> event word, published only in `experimental-debug` builds; ABI 29, 201 words); the TLAB bump of a compiled
> `newarray`/`anewarray` with the receiver's class word and length
> (`runtime_lowering::emit_inline_tlab_array_runtime_class_fast_path_ir`, the `Array.newArray` prefix's emitter
> opened to primitive atypes); for a reference array the SATB pre gate idle and the copy young or no old
> object (the gated `aastore`'s gates); then the payload in whole aligned 8-byte words (no torn element). Every
> miss declines to the unchanged helper call, which owns the NPE, every barriered copy and arrays past the
> 32 KiB inline cap. Collectors: primitive arrays on Generational, G1 and ZGC; reference arrays (`int[][]`,
> `String[]`, ...) on Generational only (G1 has no published store plan, ZGC keeps its own barriers): they keep
> the helper there. Known limit: a reference copy that turns out old (only with old objects present) declines
> AFTER the bump, leaving a zeroed orphan, as the helper's own copy refusal does. `CloneNotSupportedException`
> cannot arise (array sites only). Instances (`CloneInstance`, field copy with barriers, EA) are NOT done: an
> instance `clone()` keeps the thin helper. Unit tests (`cratonvm-jit`,
> `ir_lower::clone_inline::j27_clone_inline_tests::*`: the site screen; every primitive type cloned by the
> EXECUTED emitter against a hand-built TLAB, lengths 0..9; every decline leaves the TLAB untouched, JVMTI bits
> included; the reference copy under each gate). Probe `tools/probes/jit/j27/J27CloneIrRows.java` (every
> primitive array, `int[][]`, `String[]` under churn, an `Object[]` site whose receiver class changes, a
> 40 KB array, a null receiver, an instance with reference fields, a non-`Cloneable` receiver, an overriding
> subclass first seen mid-run; expected HotSpot 25 facts in its header). Host verify: run the probe under
> HotSpot 25 and under CratonVM `--jdk-only` and `--compatible`, each with `CRATONVM_JIT_IR_CLONE_INLINE` unset
> and `=1`, on Generational, G1 and ZGC; the facts must equal the header every time; with `=1
> CRATONVM_DBG_JITC=1` the array rows print `clone-inline (ir)` (no line for an instance row); then time
> `tools/probes/spring/ObjectCloneCost.java` and `J27CloneRows.java -Dj27.timing=true` with the switch off/on
> before proposing a default flip. Also build with `--features experimental-debug` (the VM's const assertion of
> the JVMTI mask lives behind it).

> **j27 wave 4 (lane CLONE2, 2026-10-28): PARTLY (items 1 and 3 FIXED IN CODE; item 2 not done), PENDING HOST VERIFICATION.**
> Item 1: the second option below ("pass the seeds"), done as a post-splice pass rather than a closure change, the
> mirror of the `ThreadLocal` unbind pass: `clone_thin_bind::bind_served_spliced_clone_calls` reads each surviving
> virtual call's interned `JitInvokeInfo` (`invoke_kind` 0, `clone()Ljava/lang/Object;` only -- never an
> `invokespecial`) and its seeded callee counts, asks `virtual_clone_helper` (door `splice-seed`), and moves a bound
> row from the inline-cache list to `direct_calls`. Called in `jit/src/lib.rs` right after
> `unbind_refused_spliced_thin_calls`, behind `ir_direct_calls_enabled()` (the splice closure's own gate). Switch
> `CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND`, default ON (same evidence and decline as the default-ON own-site bind;
> a wrong bind is a slower call, never a wrong answer), `=0` kill. Probe `tools/probes/jit/j27/J27Clone2Rows.java`
> (spliced callee, a served and an overriding class first seen after the caller compiled,
> `CloneNotSupportedException` thrown inside a spliced body; facts in its header). Unit test `cratonvm-jit`
> `clone_thin_bind::j27_clone_virtual_thin_tests::j27_a_spliced_instance_clone_binds_from_the_callee_profile`.
> Host verify: the parent page's wave-4 block, (a)-(c).
> **Item 3 (wave 4, CLONE2 follow-up): FIXED IN CODE for the instance path, no switch (bug fix).** The copy's
> refusals that do not depend on the object are now named heap questions, asked before the bump:
> `VmHeap::compact_object_copy_admits_class` (per class: owned compact layout, at most 64 fields / 1024 bytes, every
> slot resolved; memoised as `OBJECT_CLONE_COMPACT_COPY_BIT` in the clone verdict, required by both
> `object_clone_is_plain` and `object_clone_virtual_serves`, so the compiler does not bind such a class either) and
> `VmHeap::compact_object_copy_ready` (per call: field diagnostics, ZGC's punned-cell watch). Both read the one plan
> builder `copy_compact_object_fields` itself uses (`compact_copy_plan`, `gc/src/vm_heap.rs`). Only a per-object
> refusal (a receiver whose body is not compact) can still orphan. The array path is unchanged: its only refusal
> after the bump is an auto-box payload in the per-element reference copy (per object, rare). Tests: `cratonvm-gc`
> `vm_heap::compact_clone_copy_tests::*` (the class answer agrees with the copy on every backend), `cratonvm-vm`
> `jit::helpers::j27_clone_copy_tests::j27_a_class_the_compact_copy_refuses_is_not_served`. Side effect to confirm:
> a Cloneable class with NO instance fields has no compact layout, so it was already refused by the copy (orphan +
> native every call) and now declines straight to the native.

**Status:** PARTLY, PENDING HOST VERIFICATION: items 1 and 3 FIXED IN CODE (j27 wave 4, lane CLONE2); item 2
PARTLY (arrays FIXED IN CODE, opt-in `CRATONVM_JIT_IR_CLONE_INLINE`; instances NOT FIXED; j27 wave 4, lane
CLONEIR). Proposed by lane CLONE.
**Area:** `jit/src/clone_thin_bind.rs`, `jit/src/lib.rs` (`spliced_thin_helper`), `vm/src/jit/helpers/fast_native.rs`
(`jit_object_clone_virtual_direct`).

## 1. Spliced bodies bind array clones only

`spliced_thin_helper` has no receiver counts for a spliced body's surviving call, so
`clone_thin_bind::virtual_clone_helper` is asked with `counts = None` there and binds only an array class. An
instance `x.clone()` inside a spliced callee keeps its inline cache. The callee's receiver counts already ride beside
the row, re-keyed at its combined pc (`IrInlineTables::spliced_receiver_seeds`, which
`thread_local_thin_bind::unbind_refused_spliced_thin_calls` reads). What would fix it: bind instance rows in the
splice too, then UNBIND each whose seeded counts give no served dominant class, exactly as the ThreadLocal pass
does (or pass the seeds into `spliced_thin_helper`).

## 2. HotSpot expands the clone inline; we still call a helper

C2's `inline_native_clone` emits the allocation and the copy inline (with an `ArrayCopy` node for arrays). The
thin helper removes the native door and the inline-cache slow path, but each clone is still a call, a plausibility
screen, a TLAB bump in Rust and a copy. What would close the remaining gap: an IR `CloneArray` / `CloneInstance`
op for a receiver of exactly known class (EA-visible allocation site, or a guarded profile class), lowered to the
inline TLAB bump (`runtime_lowering` already emits one for `new`) plus a word copy with each collector's post
barrier, falling back to this helper. Measure first with `tools/probes/spring/ObjectCloneCost.java` and the
`time` lines of `tools/probes/jit/j27/J27CloneRows.java` (`-Dj27.timing=true`).

## 3. Declined allocations

`array_clone_serve` and `object_clone_serve` allocate before they copy; when the copy then refuses (a reference
array whose payload holds an auto-box wrapper, a compact-copy plan refusal) a zeroed, unreferenced object is left in
the TLAB and the native allocates again. Rare by construction; a pre-check would make it never.
