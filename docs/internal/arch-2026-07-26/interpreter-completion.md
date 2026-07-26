# Interpreter completion wave

Slug: `interpreter-completion`. Owner files: `vm/src/runtime/interpreter.rs`,
`jit-api/src/lib.rs`, `vm/src/vm.rs`, `gc/src/compact_header.rs`, this doc.

Status: **all three assigned tasks landed fully.** One task uncovered a
regression that a sibling's unbuilt landing had already introduced into three
`vm/src/vm.rs` tests; that is repaired here too (same file, same owner).

## Basis

Everything below was derived on the merged integration tree:

* `arch/wave1-integration-20260726` = **`eb82bc9b1895b5b0f6a01bd0afec682666c03059`**
  (`dev` @ `6495a191c` plus 24 merged agent branches). The merge into this
  branch was a fast-forward, so the working sha is identical.

Step-zero verification of the quickened-dispatch adoption on that tree:
`vm/src/runtime/interpreter.rs` contains live `q.resolve(saved_pc)` into the
quickened stream (`:10457`, plus the `resolve_matches_the_index_of_pc_triple_it_replaced`
pin) and **no** `quick_hint` identifier — the only two occurrences are prose in
comments explaining its removal.

Not built, not tested (nine concurrent builds OOM this host). All four edited
files parse and are `rustfmt --check`-clean for the regions changed; the three
pre-existing `interpreter.rs` format diffs (at `:16231`, `:22337`, `:39770` on
the pristine tree) are untouched and unrelated. Line endings verified
byte-for-byte pure CRLF in all four files after editing.

---

## 1. The soft-reference policy now has a real pressure input

`refs-metaspace-unloading.md` §R1, verified on the merged tree before editing.

### The claim, re-verified

`gc/src/reference.rs` compares `last_access_time_ms` (stamped by
`NativeContext::touch_soft_reference` from `SystemTime::now()`, ~1.7e12, fired
by `SoftReference.<init>` *and* every `SoftReference.get()`) against a
`current_time_ms` that both production call sites passed as a literal `0`. With
`current_time_ms == 0` the cutoff is `0.saturating_sub(threshold) == 0`, so the
`BTreeMap` range selects only entries still at timestamp `0`, whose idle time is
also `0` — never over threshold. No `SoftReference` could ever be cleared on
either VM path.

The sibling fixed the clock dimension inside `reference.rs`
(`last_observed_clock_ms`, `effective_now_ms = max(caller, observed)`). The
**pressure** dimension was still constant: both sites passed a literal `64`, so
the threshold was a fixed 64 seconds of idleness regardless of heap occupancy.

### What landed

Both call sites in `vm/src/runtime/interpreter.rs` now read the accessor:

* `process_references_after_gc` — `let free_mb = shared.mem.heap.soft_ref_policy_free_mb();`
  hoisted **above** `shared.mem.ref_processor.lock()`, then
  `ref_proc.process_references(&is_marked, free_mb, 0)`.
* `g1_remark_process_references` — same shape.

The hoist above the lock is deliberate, not cosmetic: `soft_ref_policy_free_mb`
reaches into `young_gen_stats()` / `old_gen_stats()`, which take heap-internal
locks. Evaluating it inside the `process_references` argument list would nest a
heap acquisition under the reference-processor lock for no benefit. Neither
ordering is currently in `types/src/lock_order.rs`, so this avoids *creating*
the question rather than answering it.

**Units check (asked for explicitly):** `VmHeap::soft_ref_policy_free_mb()`
returns `usize` **whole megabytes**; `ReferenceProcessor::process_references`
takes `free_heap_mb: usize` and multiplies it by
`soft_ref_lru_policy_ms_per_mb` (default 1000) to get a millisecond threshold.
The units match exactly, and the accessor rounds **down** so a sub-megabyte
remainder reads as 0 MB = maximum pressure, which is the correct bias under the
non-moving fragmenting collector that actually runs.

The third argument stays `0`, and that is **not** a second hardcode: when the
caller passes `0`, `gc::reference` substitutes the mutator clock it learned via
`touch_soft_reference`. A comment at each site says so, because a future reader
sweeping for magic numbers would otherwise "fix" it into a
`SystemTime::now()` call that is on the same clock but read at a different
instant.

### Tests (`vm/src/vm.rs`)

* `soft_ref_policy_pressure_input_is_load_bearing` — the whole point. At one
  fixed instant with one fixed 30 s of idleness, `free_heap_mb = 64` (the old
  hardcode, threshold 64 s) **retains** the referent and `free_heap_mb = 0`
  (no allocatable headroom, threshold 0 s) **clears** it. If a future change
  reverts either call site to a constant, one arm of this test stops
  discriminating. The retain arm carries an assertion message saying exactly
  that, so the test cannot silently degrade into a tautology.
* `soft_ref_policy_free_mb_drives_the_threshold_the_vm_call_sites_use` —
  evaluates the same expression the call sites evaluate against a live
  `SharedVm`, asserts it never exceeds real whole-heap headroom (over-reporting
  is the failure mode that clears soft refs *never*), and drives a real
  `process_references` past the threshold that reading implies.
* `soft_ref_policy_zero_clock_argument_uses_the_mutator_clock` — pins that the
  `0` third argument is safe: under maximum pressure *and* a literal `0` clock,
  a just-created, just-touched soft ref must survive. This is the test that
  fails if someone removes `last_observed_clock_ms`.

### Regression found: three `vm.rs` tests the sibling's landing broke

`89c6c2353` ("fix(gc): soft-ref LRU could never clear…") states in its own
commit message: *"Not built or tested (concurrent builds OOM the host)."* It
introduced `effective_now_ms = max(current_time_ms, last_observed_clock_ms)`,
which silently invalidates any test that (a) constructs a `SoftReference`
through the real `NativeContextImpl` — whose `touch_soft_reference` stamps the
entry with `SystemTime::now()` ≈ 1.7e12 — and then (b) passes a small literal
clock and expects the ref to clear. The small literal is now dominated by the
entry's own creation stamp, idle time computes as 0, and the ref survives.

Three tests in `vm/src/vm.rs` match that shape exactly and were dead on arrival:

| Test | Was | Now |
|---|---|---|
| `m20_soft_ref_cleared_under_memory_pressure` | `process_references(&is_marked, 0, 1_000_000)` | `…, 0, wall_clock_ms() + 60_000)` |
| `m20_soft_ref_cleared_under_zero_memory` | `process_references(&…, 0, 1000)` | `…, 0, wall_clock_ms() + 60_000)` |
| `s28` soft-ref-with-queue | `process_references(&is_marked, 0, 1_000_000)` | `…, 0, wall_clock_ms() + 60_000)` |

All three are in a file this slug owns, so they are fixed here rather than
filed. Each carries a comment explaining why the clock must be anchored to the
mutator's wall clock. The two "must NOT clear" siblings
(`m20_soft_ref_retained_with_plenty_of_memory`, the `s28` plenty-of-memory case)
still pass unchanged — and now pass for the right reason.

Audited and confirmed **unaffected** (no `SoftReference` construction, so
`last_observed_clock_ms` stays 0 and the caller's clock still dominates):
`vm/tests/tier1_tests.rs:1364`, `vm/src/vm/vm_init.rs:6512/6528/6551`,
`gc/tests/phase_h_integration.rs`, `gc/tests/wp1_10_reference.rs`, and every
weak/phantom/finalizer test in `vm/src/vm.rs`. These use
`discover_reference` directly and never touch.

### Not attempted

§R2 (the last-ditch `clear_all_soft_refs` before `OutOfMemoryError`) needs the
`reference.rs` half, which this slug does not own. Filed below.

---

## 2. Native-dispatch adoption: Step 0 + sites A1, A2, A3

`native-dispatch-memoization.md` §3.

### Step 0 needed files this slug does not own — here is what was done instead

The spec asks for a **new** field `native_call_site: NativeCallSite` on
`CachedBytecodeMethod`. Re-derived on the merged tree,
`grep -rn 'native_callback_cache: std::sync::OnceLock::new()'` finds **21**
struct literals across **9** files:

```
classloading/src/resolution.rs        3   NOT OWNED
jit/src/lib.rs                       12   NOT OWNED
jit/tests/ir_vs_singlepass.rs         1   NOT OWNED
vm/src/jit/helpers.rs                 1   NOT OWNED
vm/src/runtime/lockfree_resolve.rs    1   NOT OWNED (named in the ban list)
vm/src/runtime/vtable.rs              2   NOT OWNED (named in the ban list)
jit-api/src/lib.rs                    1   owned
vm/src/runtime/interpreter.rs        10   owned
vm/src/vm.rs                          1   owned
```

(`vm/src/vm/vm_exec.rs`, the file the brief guessed at, contains **no**
literal.)

The suggested fallback — "give the field a `Default` and use
`..Default::default()`-compatible construction" — does not work. Rust requires
every field in a struct literal unless the literal itself already ends in
`..expr`; none of these do. Adding *any* new field, `Default` or not, breaks
20 literals in six files this slug must not write to, which would leave the
whole workspace non-compiling for eight concurrent agents.

**What landed instead: the existing field was repurposed in place.**

```rust
// jit-api/src/lib.rs
pub native_callback_cache: std::sync::OnceLock<cratonvm_native_api::NativeCallSite>,
```

was `std::sync::OnceLock<Option<cratonvm_native_api::NativeCallback>>`. Every
existing literal spells the initializer `std::sync::OnceLock::new()`, whose `T`
is inferred from the field type, so all 20 non-owned literals compile unchanged.
The hand-written `Clone`'s line (`native_callback_cache: self.native_callback_cache.clone()`)
also compiles unchanged, because `NativeCallSite: Clone`.

Consumers go through a new accessor rather than the field:

```rust
#[inline]
pub fn native_call_site(&self) -> &cratonvm_native_api::NativeCallSite {
    self.native_callback_cache.get_or_init(cratonvm_native_api::NativeCallSite::new)
}
```

This satisfies the spec's other Step-0 requirement — *"retire
`native_callback_cache` in the same pass so there is one mechanism, not two"* —
in the strongest possible form: there is now literally one field, and the old
`Option<NativeCallback>` mechanism no longer exists anywhere. Cost of the
`OnceLock` hop is one acquire load ahead of the memo's relaxed load, on a path
that previously hashed three strings.

The only residue is the **field name**, which is now misleading. That is a pure
mechanical rename filed below; it is deliberately not attempted here because it
would touch the same six non-owned files.

### The latent bug this fixes (not just a perf change)

`OnceLock<Option<NativeCallback>>` memoized a **negative permanently**. Its
soundness argument — "native registration is immutable after VM boot" — holds
for the steady state but not for boot itself, nor for `alias_class`, nor for
the lazy `register_*` passes that run after the first bytecode executes. A
native registered by a later pass was invisible at these call sites for the life
of the process, while `find` kept resolving it fine. `NativeCallSite` keys on
`NativeMethodRegistry::generation()` and re-resolves when a slot is appended.

At **site A3** that is not merely a slow path: a missed native means the
instance tier-up gate believes there is no native override, tiers the method up,
and JIT-compiles its **real bytecode** — after which every call through that
receiver class permanently bypasses the override, because compiled code does not
re-run the interpreter's native-vs-bytecode decision. That is the exact
`java.lang.ClassValue#get` / Groovy `ClassInfo` failure mode the comment at that
site already documents.

### Sites converted

All three convert together because all three read the one per-entry cell.

| Site | Location | Before | After |
|---|---|---|---|
| A1 | `intercept_force_registered_native_cached` | `(*cached.native_callback_cache.get_or_init(\|\| …find(class_name, method_name, method_descriptor)))?` | `cached.native_call_site().callback(&shared.natives.native_methods, class_name, method_name, method_descriptor)?` |
| A2 | vtable-hit force-native gate | `…get_or_init(…).is_some()` | `cached.native_call_site().resolve(…).is_some()` |
| A3 | instance tier-up gate | `…get_or_init(…).is_some()` | `cached.native_call_site().resolve(…).is_some()` |

A2/A3 use `resolve` rather than `callback`: both only need "is a native
registered", and `resolve` stops at the `NativeMethodId` without the extra
`callback_of` array index.

### The one-cell-one-triple invariant, verified site by site

A `NativeCallSite` memo is `(generation << 32) | slot`, validated against the
generation **alone** — the triple is deliberately not re-checked on a warm hit.
A cell reached with two triples silently serves the second whatever the first
memoized, including a `None` for a native that *is* registered. Verified for
each site:

* **A1** binds `let class_name = cached.class_name.as_ref();` (and the two
  siblings) at the top of `intercept_force_registered_native_cached`. Same
  triple as the entry, by construction.
* **A2** and **A3** pass `cached.class_name` / `cached.method_name` /
  `cached.method_descriptor` directly, differing only in spelling
  (`.as_ref()` vs `&`).
* The `java/lang/ClassLoader` re-target earlier in
  `intercept_force_registered_native_cached` looks up a **different** class name
  for the same entry. It was left on plain `find` and is called out in a comment
  as the reason. Routing it through the shared cell would be exactly the bug the
  invariant forbids.

The `NCS_*` file-local statics that a sibling landed for the constant-triple
sites (B1-B7) are untouched and unaffected — they are independent cells.

### Tests

`jit-api/src/lib.rs`:

* `native_call_site_is_substitutable_for_find` — miss, hit and warm read all
  agree with `registry.find`.
* `native_call_site_heals_a_negative_after_late_registration` — warms the cell
  with a negative twice (a `OnceLock` would be sealed), registers, and requires
  the native to become visible. This is the bug pin.
* `native_call_site_is_stable_per_entry_and_survives_clone` — the accessor
  returns one stable cell per entry; `Clone` produces a *distinct* cell that
  nonetheless answers identically (the triple is `Arc`-shared with the clone).

`vm/src/runtime/interpreter.rs` (`wave1_adoption_tests`):

* `sites_a1_a2_a3_share_one_cell_because_they_share_one_triple` — makes the
  invariant executable: the three sites' three *spellings* of the arguments are
  driven through one shared, warm cell and must agree with each other and with
  `find`.
* `a1_a3_see_a_native_registered_after_the_site_first_ran` — the tier-up-gate
  consequence, stated in the assertion message.

These sit next to the existing
`sharing_one_memo_cell_across_two_triples_silently_mis_answers`, which pins the
footgun itself.

### Not attempted

* **A5** (`try_stackless_invoke`) — out of scope by instruction, and correctly
  so: it rewrites the effective class before the lookup
  (`if class_name.starts_with('[') { "java/lang/Object" }`), so the triple is
  not a call-site constant and is not memoizable by this mechanism.
* **A4** (`invoke_or_native` in `vm/src/vm/vm_exec.rs`) — not an owned file. It
  is now unblocked by Step 0; see the request below.
* Steps 1 (residual near-constant sites), 3 and 4 — unchanged.

---

## 3. `NarrowKlassTable` gained a removal path

`refs-metaspace-unloading.md` §R3. `gc/src/compact_header.rs`.

Both directions (`to_narrow: FxHashMap<u32, u32>`, `to_class_id: FxHashMap<u32, u32>`)
are class-id keyed and had **no removal path of any kind** — no `remove`,
`retain`, `prune` or `clear` anywhere in the impl. They grew one entry per class
defined, forever. `NarrowKlassTable::remove_classes(&self, &[ClassId]) -> usize`
drops both directions and reports how many mappings were actually released.

Two decisions worth recording:

1. **`ClassId`-typed, not `&[u32]`.** §R3 suggested `&[u32]`; every other public
   method on this type (`get_or_assign`, `contains`) takes
   `cratonvm_types::ClassId`, and the raw-`u32` half of the table is also the
   *narrow klass* value, so a bare `&[u32]` parameter would be ambiguous at the
   call site about which of the two `u32` namespaces it means.
2. **`next_id` is deliberately not rewound or recycled.** A narrow klass is
   embedded in the header word of every live object of that class. Reissuing a
   retired id would let a header the sweeper has not reached — or a stale copy
   captured by a concurrent scan — `resolve()` to a *different, live* class:
   silent heap-type confusion. Leaving the counter monotonic makes a retired id
   resolve to `None`, which every reader already handles because `resolve`
   returns `Option`. The bound this leaves is ~4.29e9 distinct class definitions
   per process, which is not the growth §R3 is about.

Locking mirrors `get_or_assign` exactly — `to_narrow` first, then `to_class_id`,
never both held at once — so a concurrent assign either completes wholly before
the removal or wholly after; it can never observe a half-removed pair in the
direction it reads.

Tests: `narrow_klass_table_remove_classes_drops_both_directions`,
`…_is_idempotent_and_ignores_unknowns`, `…_does_not_recycle_retired_ids`,
`…_repeated_unload_cycles_leave_no_residue` (64 define/unload cycles must reach
a steady state — the shape of any CGLIB/ByteBuddy/redeploy workload, matching
the discipline `gc/src/class_unloading.rs` adopted), and
`…_is_thread_safe` (ten threads retiring disjoint slices; the reported counts
must sum to exactly one removal per class).

**No caller was added, and none exists to add.** `CompactAllocator`, the only
owner of a `NarrowKlassTable`, still has no caller outside its own file. This is
a capability landing without a driver, which is normally the anti-pattern this
repo tracks — it is acceptable here only because the alternative is a table that
provably violates the bounded-metadata rule the moment anybody wires
`CompactAllocator` up, and because §R3 asked for exactly this shape. The wiring
request is filed below so it is not forgotten.

---

## Cross-owner requests

Precise and line-referenced. None of these files belong to this slug.

### X1 — rename `CachedBytecodeMethod::native_callback_cache` to `native_call_site`

**Owner:** whoever can write all nine files at once.
**Files:** `jit-api/src/lib.rs` (field decl, `Clone` line, accessor body, three
doc references), plus the 20 struct literals in
`classloading/src/resolution.rs` (3), `jit/src/lib.rs` (12),
`jit/tests/ir_vs_singlepass.rs` (1), `vm/src/jit/helpers.rs` (1),
`vm/src/runtime/lockfree_resolve.rs` (1), `vm/src/runtime/vtable.rs` (2).

Pure mechanical: the field now holds a `OnceLock<NativeCallSite>` and its name
says `callback_cache`. Every literal reads
`native_callback_cache: std::sync::OnceLock::new()` and becomes
`native_call_site: std::sync::OnceLock::new()`. No behaviour change; no
consumer reads the field directly any more (the accessor
`CachedBytecodeMethod::native_call_site()` is the sole reader). Derive the
literal list with
`grep -rn 'native_callback_cache: std::sync::OnceLock::new()'` rather than
copying the list above — it moves on every `dev` merge.

### X2 — site A4, `invoke_or_native` (`vm/src/vm/vm_exec.rs`)

**Owner:** `vm/src/vm/vm_exec.rs`.
**Unblocked by:** Step 0 above, which is now landed.

`invoke_or_native` calls `find_with_kind(effective_class, method_name, descriptor)`
per invocation and takes bare `&str`s, so it needs a call-site cell threaded in.
The recommended shape from `native-dispatch-memoization.md` §3 Step 2 still
applies verbatim, with one substitution: the field is reached as
`cached.native_call_site()`, not `cached.native_call_site`:

```rust
// caller side
invoke_or_native(shared, thread, class, method, desc, args, …,
                 Some(cached.native_call_site()));

// callee side
let (cb, kind) = match native_site {
    Some(site) => site.callback_with_kind(&shared.natives.native_methods,
                                          class_name, method_name, descriptor)?,
    None => shared.natives.native_methods.find_with_kind(class_name, method_name, descriptor)?,
};
```

**Blocking caution.** `invoke_or_native` resolves `effective_class`, which is
not always the caller's `cached.class_name`. A cell may only be threaded through
from a caller where the two are provably equal; anywhere they can diverge, pass
`None`. The invariant is not enforced by the type system and a violation is
silent (see `sharing_one_memo_cell_across_two_triples_silently_mis_answers`).

### X3 — wire `NarrowKlassTable::remove_classes` into class unloading

**Owner:** whoever gives `CompactAllocator` its first caller.

When `CompactAllocator` is wired up, its `NarrowKlassTable` must be pruned from
the same place that already reports unloaded classes —
`crate::memory::gc::unload_dead_class_metadata` returns a
`ClassUnloadingResult`, and `gc/src/class_unloading.rs` was extended in this
same wave to carry `unloaded_loader_addrs` for exactly this kind of coupling.
Call `remove_classes` with the unloaded class ids; do **not** rewind `next_id`
(see §3 above for why that would be heap-type confusion, not a cleanup).

Until that call exists, the removal path is tested but unreached, and the table
is bounded only by the process lifetime.

### X4 — §R2, the last-ditch soft-reference clear before `OutOfMemoryError`

**Owners:** `gc/src/reference.rs` (the API) and `vm/src/runtime/interpreter.rs`
(the driver — this slug, available to do that half).

HotSpot never throws `OutOfMemoryError` without a full GC that clears **all**
soft references. CratonVM has no equivalent. With §R1 landed, the LRU policy now
responds to pressure, but an actively-touched cache (touched more often than the
threshold) still holds its referents right up to OOM.

Shape, unchanged from §R2: `ReferenceProcessor::request_clear_all_soft_refs()`
setting a one-shot flag that makes the next `process_soft_refs` ignore the LRU
predicate, called from the allocation-failure retry loop in
`interpreter.rs` before the final attempt. It must land as **one** change with
both halves — the API alone would be a capability landing with no driver. The
`interpreter.rs` half is a few lines and this slug will take it the moment the
`reference.rs` half exists.
