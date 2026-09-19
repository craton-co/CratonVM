# The `ObjectRef` concurrency contract

*Supersedes the "sound by accident of the
single-threaded scheduler" rationale that used to sit above
`unsafe impl Send for ObjectRef` in `types/src/value.rs`.*

`ARCHITECTURE.md` §"Key Design Decisions" item 6 flags that the `unsafe impl
Send/Sync for ObjectRef` argument rested on an obsolete execution model
(one OS thread, cooperative scheduling). That premise is false: `Thread.start`
spawns a real OS thread per Java thread. This document is the re-derivation
the flag asks for.

**Verdict up front: the two `unsafe impl`s are sound**, and were never
load-bearing in the way the old comment implied. They are sound for a much
narrower and more boring reason than the old comment claimed, and the
obligations the old comment listed under them do not belong to the impls at
all — they belong to the *dereference sites* scattered across `vm/`, `gc/`
and `jit/`. §6 states the actual proof; §7 lists the obligations that are
real, load-bearing, and **not** enforced by types or assertions.

No unsound schedule was found. Several *stale-reference* hazards were found
(§8) — those are liveness/root-coverage bugs at deref sites, not defects in
the auto-trait impls.

---

## 1. The real threading model

| Layer | Reality | Evidence |
|---|---|---|
| Java platform thread | One real OS thread each | `Thread.start` → `std::thread::Builder` in `thread_start`, `vm/src/vm/vm_exec.rs`; `ARCHITECTURE.md:544` |
| Java virtual thread | Multiplexed over carrier OS threads; frames live in a boxed `JvmThread` that is parked and remounted | `vm/src/threading/virtual_threads.rs`; single remount site `resume_virtual_continuation`, `vm/src/vm/vm_exec.rs:2705` |
| `threading/virtual_scheduler.rs` | **Vestigial.** Bounds nothing, has no live callers | module header, `vm/src/threading/virtual_scheduler.rs:5-41` |
| GC-collector threads | Concurrent marker (G1) and concurrent old-gen sweep run *while mutators run* | `gc/src/g1_concurrent.rs:12-18`; `gc/src/concurrent_mark.rs:7-20` |
| Stop-the-world | Cooperative flag poll + condvar barrier, plus forced OS-level takeover of in-JIT peers | `vm/src/threading/gc_barrier.rs`; `vm/src/jit/xt_root_scan.rs` |

So: genuine preemptive OS parallelism, plus at least one GC thread that
reads (and, during concurrent sweep, frees) heap objects without any
handshake with mutators.

### 1.1 Mutator states the STW census distinguishes

`GcBarrier::request_stw_counted_locked` (`gc_barrier.rs:209`) computes
`expected = alive - 1 - blocked`, and records the exact excluded identity set
in `excluded_blocked` under the same lock every arrival reads
(`gc_barrier.rs:221-226`, `arrive_and_wait_auto`). The four states:

1. **Running interpreter** — counted. Polls `stw_requested` at allocation
   sites and backward branches, publishes a fresh root snapshot, parks
   (`interpreter.rs::safepoint_check`, `interpreter.rs:4298-4341`).
2. **Running a non-blocking native** — counted, and *deliberately waited
   for*: "a *running* native still holds raw `ObjectRef`s in Rust locals and
   must be waited for so the copying collector does not relocate objects
   under it" (`gc_barrier.rs:44-47`). It arrives when it returns to the
   interpreter. This is a liveness cost, not a soundness hole.
3. **Parked in a blocking native** (`Object.wait`, `park`, `sleep`,
   `select`, `ReferenceQueue.remove`, virtual-thread unmount) — excluded.
   Covered by the deposited root snapshot plus a wake-time fixup (§4.2).
4. **Executing JIT code** — never polls. Handled by forced takeover: the
   initiator `SuspendThread`s peers, keeps frozen only those whose `Rip` is
   inside a registered JIT range (therefore holding no Rust lock),
   conservatively scans their registers and stacks, and calls
   `reduce_expected` so `wait_for_all` cannot hang on them
   (`xt_root_scan.rs:22-45`, `gc_barrier.rs:523-540`).

---

## 2. What `ObjectRef` actually is

```rust
pub struct ObjectRef { ptr: NonNull<u8> }   // types/src/value.rs:61-64
```

* A **bare raw pointer to the object header**, not a handle, not a tagged
  index, not a GC-managed smart pointer. `NonNull<u8>` is used purely for the
  niche so `Option<ObjectRef>` stays pointer-sized (`value.rs:56-60`,
  asserted at `value.rs:1239-1242`).
* `Copy + Clone + PartialEq + Eq + Debug`. **No `Drop`. No interior
  mutability. No lifetime, no ownership, no refcount.**
* `Hash` is `self.ptr as usize` (`value.rs:70-74`) — address identity.
* Construction goes through `from_raw` / `from_raw_nonnull`
  (`value.rs:446`, `value.rs:476`), which are `unsafe`, `debug_assert!`
  non-null + 8-byte-aligned, and record the address in the process-global
  provenance bitmap (`record_object_ref_payload`, `value.rs:253`).
* The **pointee is owned by the collector**, never by the `ObjectRef`. Every
  backend allocates into its own arena (`gc/src/gen_heap.rs`, `gc/src/g1.rs`,
  `gc/src/zgc.rs`); the arena, not the reference, decides lifetime.

The crucial consequence, and the whole reason the old rationale went wrong:
**an `ObjectRef` is a *value*, not a *capability*.** Holding one asserts
nothing about the pointee. Every guarantee people attribute to `ObjectRef`
actually lives at the sites that dereference it.

---

## 3. What mutates the pointee, and under what lock

| Mutation | Path | Synchronization |
|---|---|---|
| Plain field store | `VmHeap::set_field` → `gen_heap::set_field` → `write_slot` | **No lock.** Two relaxed `AtomicU64` word writes (`cratonvm_types::write_value_atomic`, `value.rs:1268-1290`; `gen_heap.rs:11595-11604`). Per-word atomic, so no torn pointer; not sequentially consistent. |
| Plain field read | `get_field` → `read_slot` | Same, `read_value_checked_atomic` (`gen_heap.rs:11548-11559`). |
| Volatile field | `get_field_volatile` / `set_field_volatile` | Striped mutex `collector::volatile_stripe_lock` + `SeqCst` fences (`gen_heap.rs:2846-2867`). |
| Reference store | any of the above | SATB pre-barrier + G1 remembered-set post-barrier inside the accessor (`docs/GC.md` §"Write barriers"). |
| Object header (mark bits, forwarding) | collector | STW for relocation; `AcqRel` bitmap ops during concurrent mark. |
| Relocation of the object itself | collector, STW only (§4) | `StopTheWorldToken` witness (`gc/src/collector.rs:120-161`). |
| Concurrent marker reads slots | `concurrent_mark.rs`, `g1.rs` | `read_value_atomic`; correctness carried by SATB, not by ordering (`value.rs:1264-1266`). |

**Correction to the old comment.** It claimed field access "hold[s] the
appropriate monitor lock or use[s] atomic operations for volatile fields.
Direct pointer mutation is never performed outside the GC." The first half is
false for the overwhelmingly common case: non-volatile `getfield`/`putfield`
takes **no lock at all**. What actually prevents a torn/spliced pointer is
per-word atomicity in `read_value_atomic`/`write_value_atomic`, added by the
"PLAIN-SLOT TEARING FIX" (`gc/src/heap.rs:1542-1574`). The second
half is also false: the JIT's inline `jit_putfield_*` fast paths write slots
directly (`value.rs:1255-1259`) — which is exactly why the atomic helpers had
to exist on both sides.

Java-level data races on a plain field are therefore *Java* races (allowed by
the JMM, produce a stale or new value) and not *Rust* UB.

---

## 4. Can the pointee move under a live `ObjectRef` copy on another thread?

**Yes — relocation happens, and it is made safe by stop-the-world plus an
exhaustive per-holder rewrite, not by pinning.**

### 4.1 Relocation is STW-only

Every moving entry point takes `&StopTheWorldToken`, a zero-sized,
`unsafe`-to-construct witness (`gc/src/collector.rs:120-161`). The only
production constructor is in `interpreter.rs`, immediately after
`gc_barrier.wait_for_all()` returns and after the in-JIT peers have been
frozen (`interpreter.rs:1436-1457`). Concurrent phases never move: G1's
concurrent mark explicitly "requires no STW token" and only marks
(`g1_concurrent.rs:12-18`); the generational concurrent cycle is
mark/remark/sweep with no compaction (`concurrent_mark.rs:7-20`).

### 4.2 Every holder of an address is rewritten

A moving collection produces a `pointer_map: HashMap<usize, usize>`, and the
protocol rewrites, per holder class:

| Holder | Rewrite | Site |
|---|---|---|
| Initiator's frames + ~21 shared root families | `update_all_roots` | `vm/src/memory/roots.rs::collect_roots` ↔ `vm/src/memory/gc.rs::update_all_roots` |
| Cooperatively parked mutators | `apply_pointer_map_to_thread` after `arrive_and_wait_auto` returns the map | `interpreter.rs:4341-4356`, `:4394` |
| Threads parked in a blocking native | initiator folds the map into a per-thread chained `fixup` + `slot_origins`; the thread applies it on wake in `check_post_block_gc_refs` | `thread_registry.rs::fold_pointer_map_into_blocked` (called from `memory/gc.rs:1059`), `vm_exec.rs:3636` |
| Parked virtual-thread continuations | same blocked-region protocol — the registry entry shares the continuation's `Arc<GcBlockState>` / `root_snapshot` (`spawn_thread`'s `is_virtual` arm), and the remount drains it | `vm_exec.rs:2753-2790`; the virtual-thread resume GC-fixup record |
| JNI local handles | `update_local_refs_after_gc` on the resuming thread | `interpreter.rs:4405` |
| JNI array-critical pins | root splice + `pinned::update_after_gc` re-keys the pin table | `gc/src/pinned.rs:131-170` |
| Frozen in-JIT peers | **not rewritten** — cannot be. The collection must therefore not move: each contributing pass calls `gc_quiescence::mark_moving_young_coverage_incomplete_because`, and under G1 every region such a peer can address is pinned out of the collection set | `xt_root_scan.rs:46-70`; `interpreter.rs:1444` |
| Optional native subsystems | `remap` callback in the external-root provider contract | `gc/src/external_roots.rs:23-32,102-106` |

### 4.3 What "pinning" does and does not mean here

There are three distinct mechanisms and they are easy to conflate:

* `gc/src/pinned.rs` — process-global refcounted **keep-alive** set for JNI
  array checkout. Its own header is explicit: it keeps the object *alive*, it
  does **not** keep it *in place*; data-movement safety comes from JNI handing
  native code a **copy** of the array body, never a heap pointer
  (`pinned.rs:14-36,45-54`).
* G1 region pinning for frozen in-JIT peers — genuine no-relocation, because
  conservative roots are unrewritable (`interpreter.rs:1444`).
* `Heap::pin_ref` / `enter_gpu_critical` — `#[cfg(feature = "gpu-offload")]`
  only (`gc/src/heap.rs:1275-1310`), and `SafepointToken` is deliberately
  `!Send`/`!Sync` (`gc/src/safepoint.rs:57-67`).

There is **no general-purpose "pin this `ObjectRef` so it cannot move"
primitive** available to ordinary VM code.

---

## 5. Per-operation contract

"Phase" values: **STW** = inside a stop-the-world pause; **CONC-MARK** =
concurrent marking or concurrent old-gen sweep in progress, mutators running;
**MUTATE** = no collection in progress.

| Operation | Permitted GC phase(s) | Root / pinning requirement | Required atomicity / ordering | Lifetime guarantee | What makes it sound today | Residual risk |
|---|---|---|---|---|---|---|
| **Copy / move an `ObjectRef` between threads** (`Send`) | any | none | none — plain register/word copy | none: the value is just an address | `ObjectRef` is `Copy`, `Drop`-free, has no interior mutability, and owns nothing. Sending it is bit-identical to sending a `usize`. | The *receiver* inherits every deref obligation below, with no type-level hint that it has. |
| **Share `&ObjectRef` across threads** (`Sync`) | any | none | none | none | The only field is read-only after construction; `&ObjectRef` grants read-only access to an immutable pointer value. No data race is expressible. | none |
| **Compare / hash** (`==`, `Hash`) | any | none | none | none | Address comparison of two `usize`s. | **Identity is not stable across a moving collection.** A pre-GC and post-GC `ObjectRef` for the same object compare unequal. Any `HashMap<ObjectRef, _>` needs a paired remap (cf. `pinned::update_after_gc`). Nothing enforces this. |
| **Construct** (`from_raw`, `from_raw_nonnull`) | MUTATE, STW (collector rebuilding refs) | caller must already hold a valid heap address | provenance `fetch_or` is `Relaxed` (`value.rs:302-304`) | none | Non-null + 8-byte alignment are `debug_assert!`-only; callers null-check upstream and route null to `Value::Object(None)` (`value.rs:430-435`). | Relaxed provenance recording: a missing edge makes a remote decode reject a genuine reference (conservative degrade to `Object(None)`), never a fabricated pointer — the typed decode paths are the load-bearing defence (`value.rs:298-301`). |
| **`as_ptr` / `as_nonnull`** | any | none | none | none — returns the address only | Pure accessor. | Every caller that then dereferences takes on the row below. |
| **Read a field** (`get_field`, `get_field_as`) | MUTATE, CONC-MARK; **never** while a moving STW is mid-collection on another thread | the ref must be in the root set for *this* thread (frame slot, `native_pin_roots`, `handle_slots`, root snapshot, JNI local/global) | per-word relaxed atomic (`read_value_checked_atomic`) | valid until this thread's next safepoint/allocation/blocking transition | STW excludes concurrent relocation; a running mutator cannot be mid-collection with another thread. Header `num_slots` is range- and bounds-checked at runtime, degrading OOB reads to `Object(None)` rather than SIGSEGV (`gen_heap.rs:2310-2360`). | If the ref was *not* rooted, a concurrent old-gen sweep may already have freed it — this is the stale-reference family, not a `Send`/`Sync` defect. |
| **Write a field** (`set_field`, `set_field_as`) | MUTATE only | receiver **and** stored value must both be rooted | per-word relaxed atomic + SATB pre-barrier + G1 post-barrier | until next safepoint | Barriers are inside the accessor, so interpreter, native and JIT-helper stores are covered by construction (`docs/GC.md`). | JIT inline fast paths write the slot directly and must bail to the helper unless the backend publishes region bounds — only Generational does. |
| **Volatile field access** | MUTATE, CONC-MARK | as above | striped mutex + `SeqCst` fences | until next safepoint | `volatile_stripe_lock` makes the 16-byte `Value` appear atomic; fences give the JMM edge (`gen_heap.rs:2827-2867`). | Stripe collisions serialize unrelated fields (perf only). |
| **Deref the header** (`get_header`, JIT inline reads) | MUTATE, CONC-MARK | must be rooted | none | until next safepoint | `is_object_address` / `plausible_heap_pointer` gate the conservative paths (`value.rs:710-714`). | A stale ref that passes `plausible_heap_pointer` still derefs freed memory. |
| **Hold across an interpreter safepoint** | — | must live in a *rewritable* slot: frame local, operand stack, `native_pin_roots`, `handle_slots`, or a remapped shared table | — | rewritten in place by `apply_pointer_map_to_thread` | The thread re-reads the slot after resume; the raw `ObjectRef` copy it held in a Rust local before the poll is **stale**. | A Rust local holding an `ObjectRef` across `safepoint_check` is not rewritten. Nothing detects this. |
| **Hold across a blocking native** | — | must be in the deposited snapshot **and** reachable by the wake-side fixup; native-local copies must be handed to `end_blocking_region_refs(&mut [Value])` | — | rewritten on wake via `fixup` chain + `slot_origins` | `deposit_root_snapshot` publishes frames, `monitor_on_exit`, conservative locals; `check_post_block_gc_refs` is the sole consumer (`vm_exec.rs:3636`, `:11341`). | A native that keeps an `ObjectRef` in a Rust local and calls plain `end_blocking_region()` resumes with a stale address. The API cannot express "also fix up this `ObjectRef`" — only `&mut [Value]`. |
| **Hold across a virtual-thread unmount/remount** | — | same as blocking native | — | drained at the single remount site | `resume_virtual_continuation` publishes identity first, then drains via `check_post_block_gc` (`vm_exec.rs:2731-2790`). | Continuation `FrozenFrame { locals: Vec<u64> }` (`virtual_threads.rs:57-63`) carries **untagged** words; grep shows no GC scan or remap of `FrozenFrame` anywhere in `gc/` or `vm/src/memory/`. Safe today only because the live protocol keeps frames in the boxed `JvmThread`, not in `FrozenFrame`. |
| **Hold in a JIT frame / register** | — | conservative scan by the frame's own thread, or by the initiator after OS-level takeover | — | **not rewritable** | The collection is forced non-moving (or the peer's regions are pinned) for exactly this reason (`xt_root_scan.rs:46-70`). | This is the single largest coupling in the design: a future collector that moves while any JIT frame is live breaks it. `mark_moving_young_coverage_incomplete_because` is the obligation call, and it is a convention, not a type. |
| **Hold in an optional native subsystem** | — | must register an `ExternalRootProvider` with both `scan` and `remap` | — | provider-supplied | Registration is name-keyed and rejects callback mismatch (`external_roots.rs:53-64`). | Registration is opt-in; a subsystem that forgets simply loses its roots. |

---

## 6. Why `unsafe impl Send`/`Sync for ObjectRef` is sound

The old comment argued that `Send`/`Sync` hold *because* the collector, the
field-access protocol and compaction behave correctly. That is the wrong
proof obligation, and it is why it went stale: it coupled two auto-trait
impls to the entire GC design.

The correct derivation is much smaller.

`Send` and `Sync` are statements about **this type's own values**, not about
what a program may legally do with the address inside:

1. `ObjectRef` is `#[derive(Copy)]` with a single `NonNull<u8>` field, no
   `Drop` impl, and no interior mutability (`value.rs:61-64`). Sending it
   transfers a bit pattern; nothing is deallocated, unshared or invalidated
   by the transfer.
2. `Sync` requires `&ObjectRef` to be safe to share. The only field is
   written once at construction and never mutated, so `&ObjectRef` grants
   read-only access to an immutable word. No data race on `ObjectRef` itself
   is expressible.
3. `NonNull<u8>` is `!Send + !Sync` only because the standard library must be
   conservative about *ownership* semantics it cannot see. `ObjectRef` has no
   ownership semantics — the collector owns the pointee — so the conservative
   default does not apply. The correct comparison is `usize`, which is
   `Send + Sync`, not `Box<u8>`.
4. Multi-OS-thread execution changes nothing in (1)–(3). It changes a great
   deal for *dereferencing*, which is a property of the deref sites and is
   already `unsafe` at each of them.

Consequences worth stating explicitly, because the old comment implied the
opposite:

* Removing these impls would **not** make the VM safe. It would make it not
  compile (`ObjectRef` crosses threads in `root_snapshot: Arc<Mutex<Vec<ObjectRef>>>`,
  `ExternalRootProvider` callbacks, the pointer maps, the JNI global table,
  …) while leaving every real hazard untouched.
* Conversely, keeping them grants no licence. `ObjectRef` is exactly as safe
  to deref as the invariants in §5 make it, on any thread, including the one
  that created it.

The soundness burden that *does* exist is enumerated in §7. It is a
VM-architecture burden, not a `types` crate burden.

---

## 7. Invariants that are NOT enforced by types or assertions

These are the follow-up work items. Each is a real obligation that today
rests on convention, review and comments.

1. **`ObjectRef` implies nothing about validity.** There is no `Handle`,
   no lifetime, no phantom "rooted" marker. A stale `ObjectRef` and a live
   one are the same type. *Candidate fix:* a `RootedRef<'scope>` newtype for
   the paths that already have a scope (`handle_slots` is the natural
   anchor).
2. **A Rust local holding an `ObjectRef` across a safepoint, a blocking
   region, or an allocation is not rewritten.** `end_blocking_region_refs`
   accepts only `&mut [Value]`; there is no `&mut [ObjectRef]` overload and
   nothing checks that a native which blocks passed its locals in.
3. **`Hash`/`Eq` are address-based and unstable across a moving GC.** Any
   new `HashMap<ObjectRef, _>` silently breaks unless a matching entry is
   added to `update_all_roots`. Nothing enumerates the tables that need it.
4. **Conservative roots must forbid relocation.** Enforced only by each
   contributing pass remembering to call
   `gc_quiescence::mark_moving_young_coverage_incomplete_because`
   (`xt_root_scan.rs:56-63`). A new conservative root source that forgets
   silently permits a moving collection over unrewritable frames.
5. **The blocked-region flag must be raised only after the snapshot is
   complete, and cleared only under the barrier lock.** Both are prose
   contracts (`vm_exec.rs:3243-3253`, `gc_barrier.rs:427-460`). The debug
   tripwire `CRATONVM_DBG_BLOCKED_ACCESS` detects violations only when armed
   (`interpreter.rs:4231-4241`).
6. **`external_roots` providers must supply `remap`, not just `scan`.** The
   struct requires the field, but nothing checks the callback is not a no-op
   — `remap` is a plain `fn(&HashMap<usize, usize>)`.
7. **Provenance bitmap ordering.** `record_object_ref_payload_slow` uses
   `Relaxed` for both the load and the `fetch_or` (`value.rs:302-304`),
   justified by "any cross-thread transfer of the pointer carries its own
   synchronizes-with edge". That claim is unproven; the fail-safe direction
   is documented and benign (reject a genuine ref → typed decode handles it),
   but it is asserted, not tested.
8. **`FrozenFrame.locals: Vec<u64>` is untagged and unscanned.**
   (`virtual_threads.rs:57-63`.) Not currently a live hazard, because the
   parked-continuation protocol keeps frames in the boxed `JvmThread`; it
   becomes one the moment anything starts persisting object refs there.
9. **`CRATONVM_ASSERT_SINGLE_OS_THREAD` is now a false-alarm generator.**
   Any multithreaded Java program trips it by design. It survives only as a
   bisection aid ("is this workload really single-threaded?"). Reading a trip
   as a defect is a documented trap (`value.rs:99-118`).

---

## 8. Documentation ↔ code discrepancies found

Where the two disagreed, the code won.

| Claim | Source | Reality |
|---|---|---|
| "All field reads/writes … hold the appropriate monitor lock or use atomic operations for volatile fields" | old `value.rs` SAFETY comment, point (2) | Plain field access takes **no lock**. Safety comes from per-word `AtomicU64` access (`gen_heap.rs:11548-11604`). Only *volatile* access takes the stripe lock. |
| "Direct pointer mutation is never performed outside the GC" | old `value.rs` SAFETY comment, point (2) | JIT `jit_putfield_*` helpers write slots directly; that is precisely why `write_value_atomic` exists on both sides (`value.rs:1255-1259`). |
| "GC stop-the-world pauses ensure no thread observes a half-moved object" | old `value.rs` SAFETY comment, point (3) | True but incomplete: STW alone does not save a thread whose *stale copy* of the address is not rewritten. The real mechanism is STW **plus** exhaustive per-holder rewrite, **plus** forbidding relocation entirely when a holder is unrewritable (§4.2, §4.3). |
| "The collector will not free an object while any root … holds an ObjectRef to it" | old `value.rs` SAFETY comment, point (1) | True *if* the ref is in the root set. The root set is ~21 hand-maintained families (`memory/roots.rs`); the entire stale-reference bug family in `docs/known-issues/` is what happens when one is missed. |
| `Heap::pin_ref` presented as a general GC-coordination primitive | `ARCHITECTURE.md:476-482` | `#[cfg(feature = "gpu-offload")]` only, and on the semi-space `Heap`, which is not the default backend (`gc/src/heap.rs:1275-1310`). |
| Pinning implies "will not relocate" | `gc/src/heap.rs:1288-1289` doc comment on `pin_ref` ("Pin `obj` so the GC walks it as an additional root") vs `gc/src/pinned.rs:31-36` | `pinned.rs` is explicit that pinning is keep-alive only; relocating a pinned array is safe because JNI hands out a copy. The two "pin" vocabularies mean different things. |
| `virtual_scheduler` bounds virtual-thread concurrency | module name / `thread_realm.rs` field | Vestigial, no live callers, bounds nothing (`virtual_scheduler.rs:5-41`). |

---

## 9. How to falsify this proof

The claims above are checkable. In rough order of cost/benefit:

### 9.1 ThreadSanitizer over the mutator/collector boundary

Build the workspace with `-Zsanitizer=thread` (nightly, `x86_64-unknown-linux-gnu`)
and run a churn workload with several platform threads plus a moving young
generation. What to look for:

* Races on 16-byte `Value` slots that survive `read_value_atomic` /
  `write_value_atomic` — would falsify §3.
* Races between the concurrent marker's slot reads and mutator stores that
  the SATB barrier is claimed to cover — would falsify §5's CONC-MARK column.
* Races on `PROVENANCE_L1` leaves — would falsify §7 item 7.

Expected noise: the conservative JIT stack scan reads mutator stacks by
design; suppress `xt_root_scan` and `conservative_roots` frames rather than
chasing them. TSan cannot see JIT-generated code at all, so this experiment
covers the interpreter/native half only.

### 9.2 A no-op-provenance differential

Compile a build in which `object_ref_payload_is_known` returns `true`
unconditionally and one in which the `fetch_or` is `SeqCst`. If behaviour
differs on any suite, the "self-carrying happens-before edge" claim (§7
item 7) is wrong. If neither differs across many runs, the `Relaxed` choice
is at least not currently observable.

### 9.3 Model-check the STW census

`request_stw_counted_locked` / `enter_blocked` / `mark_blocked_region_leave` /
`leave_blocked_region_flagged` / `arrive_and_wait_auto` form a small state
machine over `(alive, blocked, expected, arrived, excluded_blocked,
gc_generation)`. Extract it into a standalone `loom` (or TLA+) model and
check two properties:

* **Safety** — `wait_for_all` never returns while a thread that was counted
  in `expected` is still running bytecode. (The GCAUDIT-0711 finding-1a
  comment, `gc_barrier.rs:69-91`, says this exact violation produced the
  MTChurn lost-increment / BinaryTrees wrong-total corruption family.)
* **Liveness** — no schedule leaves `wait_for_all` blocked forever.

This is the highest-value experiment: it is the one place where a wrong
answer is *memory* corruption rather than a Java-visible race, and the
current argument is a dense prose comment.

### 9.4 A stale-reference canary in `from_raw`

Under `#[cfg(debug_assertions)]`, cross-check every constructed `ObjectRef`
against the heap's `is_object_address`. This cannot live in `types` (no heap
visibility) but can be installed as a function pointer from `gc`. A trip
names the construction site of a stale reference directly, instead of the
deref site that eventually SIGSEGVs. Compare with the existing
`CRATONVM_DBG_STALE_OBJREF` producer-side check (`gen_heap.rs:2588-2614`),
which only covers `set_field`.

### 9.5 Adversarial schedule injection

Add a debug-only `CRATONVM_STW_JITTER` that sleeps a randomized interval at
each of: after `request_stw` and before `wait_for_all`; between
`fold_pointer_map_into_blocked` and `complete_gc`; between
`leave_blocked_region_flagged` and the fixup apply. The blocked-region
windows this widens are exactly the ones the finding-1(a/c) comments
identify as historically corrupting.

---

## 10. Reconciliation notes

* `ARCHITECTURE.md:544-551` should be updated to point here once this
  document lands; its warning ("should be read with that in mind") is now
  discharged. That file is outside this change's ownership.
* The `CRATONVM_ASSERT_SINGLE_OS_THREAD` tripwire block and the
  `single_thread_guard_violation` panic message in `types/src/value.rs` used
  to assert that `Send`/`Sync` are "sound only under single-OS-thread Java
  execution". That is false per §6; both have been reworded to say the
  tripwire is a diagnostic, not a soundness guard. **The panic string
  changed** — anything grepping for the old wording ("requires re-deriving
  Send/Sync") needs updating.
* `as_ptr` / `as_nonnull` now carry a `#[cfg(debug_assertions)]` re-check of
  the non-null and 8-byte-alignment construction invariants. The alignment
  check can convert a silently-degrading corruption into a debug-build panic;
  see the note on `debug_check_deref_invariants` for the bypass path
  (`read_value_checked_atomic` validates the discriminant, not the payload)
  and for how to stand it down.
* New additive API: `ObjectRef::is_plausible_heap_pointer`, the reporting
  (non-panicking) form of the range invariant that deliberately is *not*
  asserted at deref.
* `` is scheduled for removal from history; the two facts this
  document takes from `vt-resume-gc-fixup.md` (the three-sided blocked-region
  protocol, and the single remount site) are restated in §4.2 so this
  document does not depend on it.
