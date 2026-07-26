# Cross-owner closeout

Arch pass `cross-owner-closeout`, 2026-07-26.
Base: `arch/wave1-integration-20260726` @ **`928ad62a9`**.

Every item here was found by an agent that did **not** own the file it needed
changed, and was written up as a cross-owner request in a sibling doc. This pass
owns those files and closes the requests. Each claim below was re-verified
against the merged tree before acting — several were stated against an older
`dev`, and this repo has a standing habit of in-tree comments that assert the
opposite of the code (two more instances found here; see §2 and §5).

This pass could not build or test (nine concurrent cargo builds OOM the host).
Everything touched is `rustfmt --check`-clean *for the lines this pass wrote*
(three pre-existing diffs remain in `native-api/src/registry.rs` and
`native-api/src/native_id.rs` from a rustfmt version skew in the landing pass;
they were left alone rather than reflowed into this diff). CRLF line endings
were verified byte-for-byte on every touched file after every edit.

## Files touched

Owned by this pass:

* `native-api/src/registry.rs`
* `vm/src/memory/gc.rs`
* `vm/src/runtime/stackwalker.rs`
* `vm/src/threading/thread_registry.rs`
* this document

Touched outside that set, deliberately and minimally — see §7 for why each was
unavoidable:

* `native-api/src/native_id.rs` (same crate, same landing as the `registry.rs`
  work; `NativeCallSite` lives here, not in `registry.rs`)
* `vm/src/runtime/lockfree_resolve.rs` (four-line additive entry point that
  CR-LR-1 asked for by name)
* `vm/tests/wp1_9_stackwalker.rs` (one struct literal; adding a field to a
  `pub struct` breaks every literal, and leaving the workspace uncompilable is
  not an option)

---

## 1. CR-SW-1 — `StackTraceEntry::method_index` — **closed**

`native-api/src/registry.rs`

```rust
pub method_index: Option<u32>,
```

### Why it was needed

`resolve_line_numbers_in_place` (landed in `stackwalker.rs` by the
`stackwalk-and-vtable` pass) had to fail closed on overload sets: given only
`(class_id, method_name, bci)` there is no way to pick which `run` a frame is
in, the members of an overload set have **different** `LineNumberTable`s, and
guessing prints a line from the wrong method body. Verified on the merged tree:
the fail-closed branch was still there and still the only behaviour.

### What landed

* **The field.** An index, not a descriptor: O(1) at both ends, no `Arc` bump
  per frame per capture, and it is exactly what `stackwalker.rs`'s existing
  method-slot memo already computes.
* **`find_method_index_memoized`** — the memo's primitive is now the *index*;
  `find_method_memoized` is a two-line wrapper over it. No behaviour change, and
  the memo's four correctness rules (never memoize a negative, verify on every
  hit, retain nothing, bounded with clear-on-overflow) are untouched.
* **`entry_from_frame` populates it.** Free: the one class lookup and one memo
  probe that used to serve only the line number now serve both. This replaced a
  call to `line_number_for_bci`, which did the same two things and threw the
  index away.
* **`resolve_line_numbers_in_place` is exact when the index is present.** It
  reads `class.methods[idx]` from the *live* store and re-checks that the
  method's name equals the entry's `method_name` before using it. If that fails
  — a redefinition reordered or removed methods — it drops through to the old
  unambiguous-name rule, which is itself fail-closed. So the function has two
  fail-closed tiers and no path that can report a wrong line.
* `capture_frames_no_lines` and `synthetic_entry` set `method_index: None`.

### Is overload resolution now exact?

**Yes, wherever the index is present**, which is every entry captured through
`entry_from_frame` (i.e. every `Throwable` frame). `deferred_resolution_with_index_matches_the_eager_answer_exactly`
asserts the property that matters: for each member of a three-way overload set,
deferred-with-index equals the eager answer.

It is **not** exact for `capture_frames_no_lines` entries, and cannot be made so
in that function: deriving an index needs the `ClassStore` borrow that the
deposit path deliberately does not take. Those fall back to the unambiguous-name
rule. See CR-CLO-2 for the change that would close that gap.

### Did the `Throwable` path regress?

**No, and it cannot have.** The `Throwable` path is
`capture_full_trace` → `entry_from_frame`, which still resolves **eagerly**, by
exact `(name, descriptor)`, exactly as before. `resolve_line_numbers_in_place`
was not wired onto it and its doc comment now says so and says why. The one
behavioural difference on that path is that `entry_from_frame` now also *writes*
the index it already computed.

The residual honesty, stated in a test
(`an_index_pointing_at_the_wrong_overload_is_still_rejected_by_name_only_if_names_differ`):
verification is by name, so a stale index landing on *another member of the same
overload set* would pass the check. That is why the index is written at capture
time and only ever read against the same live class, and it is a further reason
the `Throwable` path stays eager rather than deferring across a window in which
a redefinition could reorder an overload set.

### Tests added (`vm/src/runtime/stackwalker.rs`)

`deferred_resolution_is_exact_for_overloads_when_the_index_is_present`,
`deferred_resolution_with_index_matches_the_eager_answer_exactly`,
`a_stale_index_falls_back_rather_than_resolving_the_wrong_body`,
`a_stale_index_into_an_overload_set_fails_closed` (wrong name, and
out-of-range index), `an_index_pointing_at_the_wrong_overload_is_still_rejected_by_name_only_if_names_differ`,
`resolution_never_touches_an_entry_whose_class_is_gone_even_with_an_index`,
`synthetic_and_lockfree_entries_carry_no_method_index`. The pre-existing
`deferred_resolution_fails_closed_on_overloads` is retained unchanged and now
pins the *no-index* half of the contract.

---

## 2. CR-SW-2 — line numbers in thread dumps — **closed on the reader this pass owns**

`vm/src/threading/thread_registry.rs`

New: `ThreadRegistry::frame_trace_of_resolved(thread_id, &ClassStore)`, which is
`frame_trace_of` followed by `stackwalker::resolve_line_numbers_in_place`.
`frame_trace_of` is retained (it has an out-of-lane caller) and its doc now
states plainly that **every entry it returns has `line_number == -1`**.

Two properties worth naming:

* The registry lock is dropped before the resolution runs — the walk touches no
  registry state, and holding L5 across a `ClassStore` walk would invert the
  usual order.
* Resolution is non-destructive: it resolves the returned copy, not the
  published snapshot, so the deposit path's data stays exactly as deposited and
  a second reader is not affected. Covered by a test.

**Comment-vs-code discrepancy found.** `vm/src/vm/vm_exec.rs::thread_stack_trace`
(now ~line 7541) carries a comment reading *"Resolve line numbers from the BCI
now that we hold the ClassStore (so a dump still gets source lines without
paying for them at every deposit)"* — and then does not: it calls the unresolved
`frame_trace_of`. The comment three lines below it also says the published entry
"doesn't carry the ClassId/descriptor needed to resolve source lines", which is
no longer true (it has carried `class_id` for some time, and now carries
`method_index` on the eager path). Both are stale. The fix is one identifier;
see CR-CLO-1.

Tests added: `frame_trace_of_is_line_less_and_resolved_reader_fills_it_in`,
`resolved_reader_leaves_overloaded_and_native_frames_alone`,
`resolved_reader_on_an_unknown_thread_is_empty_not_a_panic`,
`resolved_reader_after_class_unload_keeps_the_frame_and_drops_only_the_line`.

To make those possible without duplicating ~50 lines of `Class` literal, the
`ClassStore` fixture builders in `stackwalker.rs` moved from inside `mod tests`
to a `#[cfg(test)] pub(crate) mod test_support` beside it. No production
surface; `mod tests` imports them and is otherwise unchanged.

---

## 3. CR-VT-1 — batch vtable unload — **closed**

`vm/src/memory/gc.rs` (~line 135). Re-verified on the merged tree:
`VtableManager::unload_classes(&[u64])` exists (`vtable.rs:838`) and
`unload_classes_matches_a_loop_of_unload_class` proves it equivalent to the
loop, so this is a pure swap.

```rust
let dead: Vec<u64> = unloaded.iter().map(|c| c.id.as_u32() as u64).collect();
let mut vtables = shared.classes.vtable_manager.write();
vtables.unload_classes(&dead);
```

`unload_class` calls `invalidate_class`, which sweeps every slot of every vtable
in the VM, so the old per-class loop was `O(unloaded × all_classes × slots)`
under the manager write lock — hundreds of millions of slot visits for a few
hundred unloaded classes in a large VM, at a moment when every dispatching
thread is blocked on that lock. The `dead` vector is built *before* the lock is
taken.

---

## 4. CR-LR-1 — `invalidate_all()`'s two dead write locks — **closed**

`vm/src/memory/gc.rs` (~line 130), plus a four-line additive entry point in
`vm/src/runtime/lockfree_resolve.rs`.

**Claim re-verified independently on the merged tree.** `global_methods` and
`global_fields` are written only by `SharedResolutionState::cache_method` /
`cache_field`; a workspace-wide search for those names outside
`lockfree_resolve.rs` returns only (a) a doc-comment mention in
`classloading/src/access_control.rs:34` and (b) same-named methods on the
*unrelated* `ResolutionCache` type in `classloading/src/resolution.rs`, exercised
only by that file's own tests. A search for `shared_resolution.` outside the
module returns exactly two things: this `gc.rs` call, and the interpreter's
`promoted_*` paths. So in a running VM `invalidate_all()` took three write locks
to clear two permanently-empty maps.

The cost is negligible (uncontended, cold path). The hazard is documentary: a
reader of `gc.rs` reasonably concludes from that call that all three caches are
live, which is what the whole `lockfree_resolve` module has been mis-read as
before. `invalidate_promoted()` clears exactly the live cache and says so.

Tests: `invalidate_promoted_matches_invalidate_all_for_the_live_cache` — same
observable effect on the live cache, the other two maps empty either way, and a
cleared key re-misses rather than being memoized as a negative.

`invalidate_all` is kept (unused in production now, but it is the honest
"clear everything I own" operation and its doc now records the emptiness).

---

## 5. `NativeCallSite`'s one-triple-per-cell invariant — **closed, debug-only**

`native-api/src/native_id.rs` (the request said `registry.rs`; `NativeCallSite`
actually lives in the sibling module of the same crate and same landing).

### The invariant, and whether the public API protects it

The memo word is `(generation << 32) | (slot + 1)`. On a warm hit the triple is
**not** re-checked — deliberately, since re-checking means re-hashing three
strings, which is the entire cost the type exists to remove. So one cell must
serve exactly one triple.

Assessment of the public API: it is *hard* to violate by accident but not
impossible, and a violation is **silently wrong** rather than loudly wrong.
Points in its favour — there is no way to inject a raw `NativeMethodId`; the
strings are passed on every call, so a site cannot drift without the code
changing; and every embedding in the adoption plan is structurally
single-triple (a `static` beside a constant-triple lookup, or a cell owned by
the `CachedBytecodeMethod` whose own triple is the one looked up). Against it —
the adoption plan explicitly recommends `static NativeCallSite` cells at
constant-triple call sites, and two nearby lookups sharing one `static`
compiles fine and silently returns the first site's answer for the second
site's triple. That is exactly the failure the pass's own
`a_memo_from_one_registry_is_not_honoured_by_another` test tripped over before
it was corrected.

### What landed

A `#[cfg(debug_assertions)] triple_witness: AtomicU64` on the cell: the first
query records a non-zero fold of the registry's own 128-bit digest of the
triple, and every later query `debug_assert!`s that it matches.

* **Zero release cost, by construction.** The field does not exist in a release
  build, and the check is only ever reached from inside a `debug_assert!`, whose
  expression is not evaluated in release — so the triple is never hashed there.
  A release `NativeCallSite` is still exactly one `AtomicU64`, as documented.
  This is not a release-path check and must never become one.
* `Clone` carries the witness forward: a cloned `CachedBytecodeMethod` describes
  the same method, so cloning must not launder a violation into a clean cell.
* `invalidate()` does **not** clear it: forgetting a memo does not turn a cell
  into a different call site.
* The type's doc now states the invariant, why it is a caller contract rather
  than something the API can enforce, and why the enforcement is debug-only.

Tests: `witness_accepts_the_same_triple_repeatedly`,
`witness_rejects_a_second_triple_on_the_same_cell`,
`sharing_one_cell_between_two_triples_panics_in_debug` (`#[should_panic]`, the
real path),
`a_dedicated_cell_per_triple_is_the_supported_shape` (the supported shape stays
correct cold and warm), `invalidate_does_not_clear_the_triple_witness`,
`clone_carries_the_triple_witness`.

---

## 6. Cross-owner requests generated by this pass

**None of these edits were made.**

### CR-CLO-1 — `vm/src/vm/vm_exec.rs` (~line 7554): adopt the resolving reader

In `thread_stack_trace`, the cross-thread arm ends:

```rust
self.shared.threads.thread_registry.frame_trace_of(tid)
```

Every frame it returns has `line_number == -1`. `ThreadRegistry` now offers:

```rust
let cm = self.shared.classes.class_manager.read();
self.shared
    .threads
    .thread_registry
    .frame_trace_of_resolved(tid, &cm.class_store)
```

(the `class_manager.read()` is the same borrow the *current-thread* arm ten
lines above already takes). Strictly additive: the resolver is fail-closed —
it fills an unknown with the line an eager capture would have produced, or
leaves `-1`. It never writes a wrong line and never touches an entry that
already has one, including the `-2` native sentinel.

While there, **two stale comments** at that site should go: the one claiming
lines are already resolved "now that we hold the ClassStore" (they are not), and
the one claiming the published entry "doesn't carry the ClassId/descriptor
needed" (it carries `class_id`, and the eager path now also carries
`method_index`).

Any other `frame_trace_of` consumer that holds a `ClassStore` wants the same
switch. The **depositor** at `vm_exec.rs:2493` must stay on
`capture_frames_no_lines` — it is lock-free by design.

### CR-CLO-2 — `vm/src/runtime/frame.rs`: carry the method index on `Frame`

`capture_frames_no_lines` cannot populate `StackTraceEntry::method_index`
because deriving one needs a `ClassStore` borrow the deposit path must not take.
Consequently thread dumps still lose overloaded frames' line numbers even after
CR-CLO-1.

`Frame` already caches `class_name_arc` / `method_name_arc` /
`method_descriptor_arc` / `source_file_arc`. If it also cached the method's
index within its declaring class's `Class::methods` — known at frame push, where
the method was just resolved — `capture_frames_no_lines` would carry it for one
`u32` copy per frame, no lock and no lookup, and deferred resolution would be
exact for thread dumps too. This is the last piece of a fully lazy trace path.

### CR-CLO-3 — the deferred `Throwable` path is now *possible*; it is not *taken*

With `method_index` present, routing `Throwable` capture through
`capture_frames_no_lines`-style deferral no longer costs fidelity on overloads.
This pass did **not** make that change, and recommends it be made only with a
measurement attached, for two reasons:

1. It changes the most fidelity-sensitive output the VM produces
   (`printStackTrace`) for a throughput win, which is precisely the trade
   `tco-breaks-stacktrace-fidelity` records going wrong.
2. Deferral opens a window between capture and read in which a redefinition can
   reorder an overload set. The index is verified by *name*, so a reorder within
   an overload set is the one case verification cannot catch. Eager capture has
   no such window. A deferring design should either capture the descriptor as
   well, or gate deferral on `any_class_redefined()`.

What is unambiguously safe and unclaimed today: the `Vec<StackTraceEntry>`
allocation, the clone in `capture_throwable_stack_trace`, and the VM-wide
`throwable_stacks` write lock — CR-2 and CR-4 in `exception-and-indy-path.md`.

### CR-CLO-4 — `jit-api/src/lib.rs` + interpreter: adopt `NativeCallSite`

Unchanged from `native-dispatch-memoization.md` §3, restated only to attach the
new constraint: **each converted call site needs its own cell.** The debug-only
witness added in §5 will now panic loudly in a debug build if two triples share
one, which is the intended way to discover a mistake during that adoption.

---

## 7. Files touched outside the exclusive set, and why

* **`native-api/src/native_id.rs`.** The request was written as "while you are
  in `registry.rs`", but `NativeCallSite` lives in `native_id.rs` — same crate,
  same landing, no other owner in this wave. Editing `registry.rs` instead was
  not an option; the type is not there.
* **`vm/src/runtime/lockfree_resolve.rs`.** CR-LR-1 asked for exactly this and
  its author offered it ("ping this slug and it is a three-line addition"). The
  addition is purely additive — one new method plus a doc note on
  `invalidate_all` — and the owning pass is merged, so no sibling holds the file
  in this wave. `gc.rs` alone cannot drop a lock taken inside another module.
* **`vm/tests/wp1_9_stackwalker.rs`.** One struct literal, one field. Adding a
  field to a `pub struct` breaks every literal of it in the workspace; a
  workspace-wide search found exactly three construction sites (this file plus
  `registry.rs` and `stackwalker.rs`, both owned here). Leaving the tree
  uncompilable was the alternative.

## 8. Not done

* **Wiring `resolve_line_numbers_in_place` onto the `Throwable` path.** See
  CR-CLO-3. Explicitly out of scope: the instruction was to make the resolver
  exact, not to regress a working path to enable a faster one.
* **`method_index` on `capture_frames_no_lines`.** Needs a `Frame` change;
  CR-CLO-2.
* **A release-path triple check on `NativeCallSite`.** Deliberately refused —
  it would reinstate the string hashing the mechanism exists to remove.
