# The deopt stashes are thread-local heap references

**Status: BOTH halves are landed.** The `jit/` half is the visitors + tests in
`jit/src/deopt.rs`; the `vm/` half is the four call sites this page listed under
"Outstanding", and they are wired — the scan in `memory/roots.rs` §10
(`for_each_stashed_deopt_object`) and `vm/vm_exec.rs`'s pre-park deposit, the
remap in `memory/gc.rs` (`remap_stashed_deopt_objects`) and its `vm_exec.rs`
sibling. `jit/src/ir_verify.rs`: swept, nothing to fix.

**This status line was stale, and the staleness had a cost.** While it read
"not wired", `route_implicit_exc_through_callee` carried a
`clear_exceptional_frame()` immediately before `create_exception_object`,
justified in its own comment by "a `ReconstructedFrame` is not a GC root". That
defence outlived its cause: it discarded the reason-9 frame the compiled body
had just published, so RBC.6's `getfield`/`putfield` admission let a
NullPointerException escape a handler that catches it, for every compiled
`try`-wrapped field access in the tree. Fixed by keeping the frame — see
`fixed-bugs/rbc6-getfield-putfield-npe-escape-FIXED-20260818.md`. A status
line is load-bearing; this one was read as permission to defend against a
hazard that was already closed.

This is the `jit/`-crate sibling of
`fixed-bugs/jit-signals-root-gap.md`, which moved the JIT's pending
throwable off a `thread_local!` and onto `JvmThread`. Same defect, same
consequence, different crate — and, because `jit/` cannot depend on `vm/`, a
different fix shape.

## The premise, re-verified at the line

`LAST_DEOPT` (`jit/src/deopt.rs`, the `thread_local!` above `take_last_deopt`)
and `LAST_EXCEPTIONAL` (the `thread_local!` above `take_exceptional_frame`) are
`RefCell<Option<ReconstructedFrame>>`. A `ReconstructedFrame`'s slots include
`FrameValue::Object(u64)` — a **raw Java heap address**, `0` for null
(`FrameValue::Object`'s doc, `jit/src/deopt.rs`).

Verified by reading, at the cited lines:

| claim | evidence |
| --- | --- |
| the stashes hold raw addresses | `resolve_value`, `jit/src/deopt.rs:1497` (`RegisterRef` → `Object(gpr)`) and `:1518` (`StackSlotRef` → `Object(*slot)`) — the only two non-test producers of `FrameValue::Object` in the crate |
| the addresses exist **only** at runtime, never in metadata | `DeoptVerifier` already rejects a non-zero `FrameValue::Object` in an emitted `FrameState` as `DeoptMetadataError::BakedObjectAddress` (`jit/src/deopt.rs:4685`). So `DeoptimizationPoint`, `FrameStateInterner` and the leaked deopt-point boxes are address-free by construction — the two thread-locals are the whole exposure |
| who writes them | `x64_deopt_entry` (`jit/src/deopt.rs:2370` exceptional, `:2382` ordinary, `:2342` the superseded-artifact sentinel) and `ir_deopt_entry` (`:2270`), plus the re-stash helpers `restash_last_deopt` / `restash_exceptional_frame` |
| who reads them | `vm/src/runtime/interpreter/invoke.rs:15789`, `:20173`, `:20598`; `vm/src/jit/helpers.rs:2885`; `vm/src/runtime/interpreter.rs:12841`, `:12991` |
| a thread-local is invisible to a *peer* collector | `VM_ROOT_SOURCES` callbacks run on the collecting thread; the per-thread halves both take the **current** thread — `collect_roots(shared, thread)` (`vm/src/memory/roots.rs:65`), `update_all_roots(shared, thread, ..)` (`vm/src/memory/gc.rs:365`) |
| an allocation is a safepoint | `vm/src/threading/gc_barrier.rs:6-9`: threads stop "at their next safepoint (allocation site or backward branch)" |
| the maintainers already suspect it | `vm/src/runtime/interpreter.rs:12862-12863` drops a foreign exceptional frame because re-stashing it would "route with a stale bci and stale **(possibly collected) object pointers**" |

`resolve_value`'s own comment (`:1515-1517`) says the read "keeps the oop
current — no GC has run since the guard captured it". That is true of the
*capture*. Nothing kept it current afterwards.

## Can a collection run inside the window? Yes — three of them, named

The collector here is cooperative: a peer's GC cannot proceed past a thread
that is running Rust VM code without polling. So "is there a window" reduces to
"does **this** thread allocate, poll, or park between the write and the read".
It does.

### W1 — the shortest, and the one that needs no leak: an allocating drain arm

`emit_post_invoke_exception_check` (`jit/src/x64.rs:12227-12281`) routes **every**
`i64::MIN` return at a bci inside a protected range into the reason-9 deopt
stub (the non-`J`/`D` arm at `:12272-12277` does so on the raw `CMP` alone). The
stub calls `x64_deopt_entry`, which publishes a `PendingException` frame to
`LAST_EXCEPTIONAL` (`jit/src/deopt.rs:2362-2371`) and returns the sentinel.

The pending signal at that instant need not be a throwable: `jit_dispatch_threw`
(`vm/src/jit/helpers.rs:1026-1028`) treats a bare `npe` / `aioobe` / `deopt`
flag as a genuine sentinel. So the compiled method can return with
`sig.exception == None` and `sig.npe == Some`. The interpreter's sink then, in
order:

1. `vm/src/runtime/interpreter/invoke.rs:19983` — `sig.exception` is `None`, the
   short non-allocating early return does **not** fire;
2. `:20056` `throw_runtime_error(.. NullPointerException ..)` — **allocates a
   Java object**. (Siblings: `:20096` `create_exception_object` for AIOOBE,
   `:20131` `throw_runtime_error` for `ArithmeticException`.)
3. `:20066` `route_jit_signal_exception` →
   `vm/src/runtime/interpreter.rs:12841` `take_exceptional_frame()` →
   `:12851` `ir_deopt_locals(&rframe.locals)` — the handler frame's locals are
   built from the addresses captured **before** step 2.

Step 2 is a safepoint. A moving collection there hands step 3 from-space
pointers; a non-moving one hands it reclaimed memory. Neither is detectable at
the point of failure.

The same three arms sit between a stashed `LAST_DEOPT` frame and its drain at
`:20173`, and they `return` without draining it — so a `LAST_DEOPT` frame
standing when one of them fires is not merely exposed, it is leaked (W3).

### W2 — a whole interpreter re-execution, by design

`route_implicit_exc_through_callee` (`vm/src/jit/helpers.rs:2402-2432`) resumes a
compiled callee at its own handler via `try_run_callee_handler`. That function
is already annotated (`:2171-2181`) with the fact that `resolve_callee_cached`
"can load the callee's class (a user `ClassLoader.loadClass`, hence allocation,
hence a collection)", and it runs `run_jit_callee_handler`
(`vm/src/runtime/interpreter.rs:13202`), which refills the operand pools,
acquires a monitor, pushes a frame and executes arbitrary Java.

The sibling lane pinned `exc` across exactly that span. The **exceptional frame
the same callee published** is not pinned, is not consumed by that path, and —
on the `Some(v)` success return at `:2432` — is never cleared either
(`clear_exceptional_frame()` at `:2447` is on the fall-through only). It
survives the whole re-execution and then leaks.

### W3 — the leak window, which is unbounded

A frame the identity check refuses is **deliberately** left stashed so an outer
sink can claim it (`jit/src/deopt.rs`, `peek_last_deopt_identity`'s doc: "A
non-matching frame must stay stashed so it propagates"). Combined with the
non-draining early returns in W1, a stashed frame can outlive its owner by an
arbitrary amount of Java execution. The eventual claimant's identity check
(`deopt_frame_matches_method`) compares class/method/descriptor only, so a
*stale frame of the same method* passes it and is used. That is precisely the
failure `vm/src/runtime/interpreter.rs:12862` describes, still reachable for the
same-method case.

**Conclusion: the finding is real.** It is not "one non-allocating operation
with no safepoint"; W1 alone is an unconditional Java allocation between the
write and the read.

## The fix, and why this shape

Three options were available:

* **Do not store a heap address.** Rejected: the consumer's whole job is to
  rebuild an interpreter frame from those object identities. There is nothing
  else to store.
* **Bound the window.** Rejected: every operation in all three windows lives in
  `vm/`, outside this lane's file set, and W3 has no bound to give.
* **Make the stash reachable, and say precisely what `vm/` must call.** Taken.

`jit/` cannot depend on `vm/`, so the storage cannot move onto `JvmThread`. But
"thread-local storage is unreachable from a collecting thread" is a statement
about *peers*: a thread-local is perfectly reachable **from the thread that owns
it**, and this VM already does all of its per-thread root work on the owning
thread — `collect_roots(shared, thread)`, `update_all_roots(shared, thread, ..)`,
`NativeContext::deposit_root_snapshot`, `check_post_block_gc`. Note also that
the `JvmThread`-field fix has the same peer limitation today (that document's
Outstanding §2); its advantage is future cross-thread scanning, not present
reach.

So `jit/src/deopt.rs` now exports the two halves of the GC contract as
on-thread visitors:

```rust
pub fn for_each_stashed_deopt_object(f: impl FnMut(u64));            // scan
pub fn remap_stashed_deopt_objects(f: impl FnMut(u64) -> Option<u64>); // remap
pub fn stashed_deopt_object_count() -> usize;                        // diagnostic
```

plus `ReconstructedFrame::for_each_object_address{,_mut}`, which walk **all four**
containers that can hold an address — `locals`, `stack`, `monitors[].object`,
and the inlined `caller_frames` chain — and descend into the `field_values` of
any scalar-replaced object nested in them (`resolve_value`'s
`FrameValue::VirtualObject` arm resolves those fields to concrete `Object`s, so
a walk that skipped them would leave a virtual object's referents unrooted). A
`VirtualObjectRef` is an intra-frame id edge, not an address, and is the cycle
terminator that bounds the walk. Null (`0`) is never offered: every value a
caller receives is a live heap reference.

### Fail-closed guard

A remap without a matching scan is worse than either failure alone: it
faithfully rewrites a reference to a slot the collector was free to reclaim.
That is the exact half-wiring the sibling document shipped with. So
`remap_stashed_deopt_objects` carries a `debug_assert!` that this thread has
offered its stashes to a root scan at least once whenever there is anything to
remap, naming this file in the message. Test:
`remapping_a_populated_stash_without_a_scan_trips_the_wiring_check` (which runs
on a spawned thread, because both the stashes and the counter are thread-local
and `--test-threads=1` would otherwise make it vacuous).

## Census — `jit/src/deopt.rs` and `jit/src/ir_verify.rs`

Every `thread_local!`, every `static`, and every structure that outlives a call.
"Heap ref?" means: can it hold a `FrameValue::Object`, an `ObjectRef`, or a raw
Java heap address?

| holder | what it holds | who writes it | window | can a collection run in it? | action |
| --- | --- | --- | --- | --- | --- |
| `deopt.rs` `LAST_DEOPT` | `ReconstructedFrame` — raw addresses in `locals` / `stack` / `monitors` / `caller_frames` / virtual-object fields | `ir_deopt_entry:2270`, `x64_deopt_entry:2342`,`:2382`, `restash_last_deopt` | deopt stub → VM sink (`invoke.rs:15789`/`:20173`/`:20598`, `helpers.rs:2885`); **unbounded** when an early-returning arm consumes the sentinel without draining | **yes** — W1 (`throw_runtime_error` / `create_exception_object` at `invoke.rs:20056`/`:20096`/`:20131`), W3 (unbounded) | **visitors added**; `vm/` must call them (Outstanding) |
| `deopt.rs` `LAST_EXCEPTIONAL` | same | `x64_deopt_entry:2370`, `restash_exceptional_frame` | reason-9/10 stub → `route_jit_signal_exception` (`interpreter.rs:12841`) or `drop_own_exceptional_frame` (`:12991`); leaks on the `try_run_callee_handler` success return (`helpers.rs:2432`) | **yes** — W1 and W2 (`resolve_callee_cached` class load + `run_jit_callee_handler` full interpreter re-execution) | **visitors added**; `vm/` must call them (Outstanding) |
| `deopt.rs` `STASH_ROOT_SCANS` (new) | `Cell<u64>` counter | `for_each_stashed_deopt_object` | — | n/a | none — debug wiring check only |
| `deopt.rs` `DESPEC_SET` (`:553`) | `RwLock<FxHashSet<(String, u32)>>` — `(method_key, bci)` | `despec_insert` | process lifetime | n/a | **none** — no heap ref. Strings and bcis |
| `deopt.rs` `DeoptEpochGuard` (`:636-641`) | `AtomicU64` + `AtomicPtr<AtomicU64>` into `SharedVm::method_epochs`; leaked per artifact | `emit_deopt_stubs` | process lifetime | n/a | **none** — a VM-side counter address, not a heap address |
| `deopt.rs` `DeoptimizationPoint` / `FrameState` / `FrameStateInterner` / `DeoptimizationLog` / `InvalidationManager` | method keys, bcis, slot *descriptions* (`StackSlotRef(off)`, `RegisterRef(r)`), class ids, `Arc<[FrameValue]>` chunks | the compiler frontends | process lifetime (boxes are leaked; see `CompiledMethod`'s Drop) | n/a | **none** — a baked address here is already a hard error (`BakedObjectAddress`, `:4685`), and no non-test producer emits one |
| `ir_verify.rs` `ENABLED` (`:315`), `DISABLED` (`:329`) | `OnceLock<bool>` env-var caches | `verify_enabled` / `pre_lower_verify_disabled` | process lifetime | n/a | **none** |
| `ir_verify.rs` everything else | a checker over a borrowed `&Graph`; `VerifyOptions` (flags), `Violations` (a call-local `Vec<String>`) | — | one `verify_graph` call | n/a | **none** — the file declares no `thread_local!`, no mutable `static`, and never names `FrameValue` outside a doc comment |

## Tests (`jit/src/deopt.rs`, `mod deopt_stash_root_tests`)

* `every_object_slot_family_is_offered_to_a_root_scan` — one distinct address in
  every container that can hold one (locals, stack, monitor, virtual-object
  field, nested virtual object, caller frame), asserted as an exact set. This is
  the test that fails if a new `FrameValue` variant starts carrying an address.
* `the_exceptional_stash_is_scanned_too` and
  `both_stashes_are_scanned_when_both_are_occupied` — a scan of only `LAST_DEOPT`
  would close the wrong half; W1 is the exceptional stash's window.
* `a_moving_collection_rewrites_every_stashed_object_slot` — scan, then relocate
  every object, then drain and assert the frame reads post-move addresses, with
  the monitor and caller-frame containers spot-checked by hand so a walk that
  "visits" without writing back cannot pass.
* `remap_leaves_addresses_the_map_does_not_mention_alone`.
* `null_object_slots_are_not_offered_as_roots`,
  `nothing_is_offered_when_no_frame_is_stashed`.
* `non_reference_slots_are_never_offered_or_rewritten` — `Int`/`Long`/`Float`/
  `Double` bit patterns that look like pointers, an unresolved `StackSlotRef`
  (a frame *offset*), `RegisterRef`, `VirtualObjectRef`, `Undefined`,
  `Unsupported`, `MaterializationRequired` all survive a remap byte-identical.
* `scanning_and_remapping_leave_the_stash_in_place` — neither half consumes the
  frame or perturbs its identity.
* `remapping_an_empty_stash_without_a_scan_is_not_an_error` and
  `remapping_a_populated_stash_without_a_scan_trips_the_wiring_check`.

No fixed wall-clock bounds anywhere; nothing sleeps.

## Outstanding — the `vm/` half (four call sites, none in this lane's file set)

Until these land, nothing above changes runtime behaviour: the visitors are
inert and the two stashes remain unrooted and un-remapped. Both halves must land
**together**.

1. **Scan, collecting thread** — `vm/src/memory/roots.rs::collect_roots`, §10,
   immediately after the `thread.jit_pending_exception` push (`:547-549`):

   ```rust
   // The JIT's stashed deopt / exceptional frames. They live in `jit/`
   // thread-locals (that crate cannot depend on `vm/`), so they are reached
   // through an on-thread visitor rather than a field. Paired with the remap
   // in `gc.rs`. See `docs/jit/deopt-thread-local-roots.md`.
   cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| {
       if let Some(obj) = shared.mem.heap.is_object_address(addr as usize) {
           roots.push(obj);
       }
   });
   ```

   (`is_object_address` rather than a raw `ObjectRef` construction, matching the
   `pinned_addrs` block at `:526-532` — the stash can name an address the heap
   no longer owns if a *previous* collection already ran unrooted.)

2. **Remap, collecting thread** — `vm/src/memory/gc.rs::remap_thread_object_slots`
   (§10), alongside the three `JvmThread` slots:

   ```rust
   cratonvm_jit::deopt::remap_stashed_deopt_objects(|addr| {
       pointer_map.get(&(addr as usize)).map(|&to| to as u64)
   });
   ```

3. **Scan, a thread about to park** —
   `vm/src/vm/vm_exec.rs::deposit_root_snapshot_inner`: add the same push, so a
   peer that enters a blocking region mid-window (W2 reaches
   `run_jit_callee_handler`, which can block) deposits its stashed oops. Without
   this, `GcBarrier` excludes the parked thread from `expected` and collects
   without ever seeing them.

4. **Remap, that thread on wake** — `vm/src/vm/vm_exec.rs::check_post_block_gc`:
   the matching `remap_stashed_deopt_objects` against the pointer map it already
   applies to its own frames.

Wiring 2 or 4 without 1 or 3 is the half-wiring the debug assertion refuses.

## Also found, outside this lane's files (not changed)

* **`vm/src/jit/helpers.rs:2432`** — the `try_run_callee_handler` success return
  leaks the callee's exceptional frame. `clear_exceptional_frame()` at `:2447`
  covers only the fall-through. Even with the visitors wired this leaves a stale
  frame that a later same-method drain will claim
  (`deopt_frame_matches_method` compares names only). Bounding it — clearing the
  frame on the success path, as the fall-through already does — is a strictly
  better fix than rooting it for an unbounded time.
* **`vm/src/runtime/interpreter/invoke.rs:20055`, `:20094`, `:20130`** — the
  three arms `return` without draining `LAST_DEOPT`, which is what makes W3
  unbounded. A `let _ = take_last_deopt();` before each `return` would bound it.

## Rule of thumb

A `thread_local!` is invisible to a **peer** collector, and therefore to
`VM_ROOT_SOURCES`. It is *not* invisible to the thread that owns it, and this
VM's root scan and remap are both per-thread and on-thread. So a heap reference
in a `jit/` thread-local is fixable without a `JvmThread` field — by an
on-thread visitor called from all four per-thread root points. What is never
optional is that the scan half and the remap half land in the same change.
