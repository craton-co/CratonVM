# CratonVM Lock Order (T1.8.4)

This document defines the **global lock acquisition order** for every
mutex and rwlock in `cratonvm-vm` and its dependencies. Acquiring locks
out of order is a deadlock waiting to happen and *will* be caught by
the lock-order regression tests in `vm/tests/lock_order_tests.rs`.

The runtime enforcement counterpart lives in
[`vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs):
`LockLevel`, `OrderedMutex`, and `OrderedRwLock`. The numeric
discriminants of `LockLevel` mirror the L0–L10 column in the table
below exactly — if you change one, change the other in the same
commit.

> **Runtime enforcement status (V11).** The `OrderedMutex` /
> `OrderedRwLock` wrappers maintain a per-thread set of currently-held
> levels and, in **debug builds only**, `assert!` on every acquire that
> the new level is *strictly less* than the lowest level already held
> (the descending rule). In release builds the tracking module is
> compiled out and the wrappers are zero-cost. **This check only sees a
> lock once that lock is actually wrapped.** See
> [Which locks are actually enforced](#which-locks-are-actually-enforced)
> below for the precise, current list — most locks in the table are
> still raw `parking_lot`/`std::sync` and are therefore **not** observed
> by the runtime checker yet (they remain enforced only by review and
> the `vm/tests/lock_order_tests.rs` fuzzer).

## Why this matters

A JVM has many subsystems that occasionally need to coordinate:
- The interpreter calls into the class manager.
- The class manager calls into the heap.
- The heap may stop the world.
- A safepoint may walk every thread's monitor stack.
- A monitor wait may release the heap lock to allow allocation.

Without a documented hierarchy these calls eventually nest in
contradictory orders and deadlock under load. With a documented
hierarchy each lock acquisition is locally checkable: "am I about to
acquire a lock at level N while holding one at level M ≥ N?".

## The hierarchy

Locks are grouped into **levels**. A thread holding a lock at level N
may *only* acquire locks at levels strictly less than N (i.e. lower
numbers). Re-entrant acquisitions of the same lock by the same thread
are always allowed (Rust's `parking_lot::Mutex` is non-reentrant; we
use `parking_lot::ReentrantMutex` where reentrancy is required).

Lower level = acquired first = held longer.

| Level | Lock | Type | Owner module |
|------:|------|------|--------------|
| **L10** | `SharedVm::class_manager` (RwLock) | rw | `vm/src/vm/vm_init.rs` |
| **L9**  | `SharedVm::native_methods` (Mutex via append-only) | append-only | `vm/src/vm/vm_init.rs` |
| **L8**  | `SharedVm::heap` interior locks | mixed | `gc/src/heap.rs` |
| **L7**  | `SharedVm::ref_processor` (Mutex) | mut | `vm/src/vm/vm_init.rs` |
| **L6**  | `SharedVm::monitors` (per-object Monitors via `Arc<Monitor>`) | mut | `vm/src/threading/monitor.rs` |
| **L5**  | `SharedVm::thread_registry` (Mutex) | mut | `vm/src/threading/thread_registry.rs` |
| **L4**  | `SharedVm::flight_recorder` (Mutex) | mut | `vm/src/jfr/recorder.rs` |
| **L3**  | `SharedVm::cleaner_thread.pending_actions` (Mutex) | mut | `gc/src/reference.rs` |
| **L2**  | `SharedVm::native_memory` (NativeMemoryTable Mutex) | mut | `native-api/src/ffi.rs` |
| **L1**  | `JvmThread`-local state | thread-owned | `vm/src/threading/jvm_thread.rs` |
| **L0**  | Per-call scratch (`Vec`s, `HashMap`s constructed inside a single function call) | local | — |

### Sub-hierarchies within a subsystem

Some subsystems contain multiple cooperating locks of their own. The
canonical order inside the subsystem is documented here; treat them as
all being at the subsystem's table-level when reasoning about the
global hierarchy.

| Sub-level | Lock | Type | Owner module | Rule |
|----------:|------|------|--------------|------|
| **L4.a** (outer) | `ProfileStore::methods` (RwLock) | rw | `jit/src/profile.rs` | acquire first |
| **L4.b** (inner) | `ProfileStore::name_index` (RwLock) | rw | `jit/src/profile.rs` | only acquire while holding `methods`, or alone |

Rationale (round-7 CRIT-2): `get_or_insert_borrowed` previously took
`name_index.read()` first on the fast path and `methods.write()` →
`name_index.write()` on the slow path, an AB/BA inversion under
contention. The fix imposes the canonical `methods > name_index`
ordering: the fast path probes `name_index` then drops it before
touching `methods`, and the slow path acquires `methods.write()`
*before* `name_index.write()`. `snapshot_all` further takes a
two-phase snapshot so the per-slot `Mutex<MethodProfile>` is never
held under `methods.read()`.

### Reading the table

- A thread holding `class_manager` (L10) may acquire `heap` (L8),
  `ref_processor` (L7), `monitors` (L6), and so on — descending.
- A thread holding `monitors` (L6) **must not** acquire
  `class_manager` (L10) or `heap` (L8). If it needs class info it must
  drop the monitor lock first.
- `JvmThread` (L1) state is per-thread and never contended; it can be
  read or mutated freely by the owning thread without affecting any
  global lock.

## Which locks are actually enforced

The hierarchy above is the *design*. The runtime checker in
`lock_order.rs` only observes locks that have been physically wrapped
in `OrderedMutex` / `OrderedRwLock`. The honest, current status:

| Level | Lock | Runtime-checked? | Notes |
|------:|------|------------------|-------|
| L10 | `class_manager` (RwLock) | **No** — aspirational | `parking_lot::RwLock` read/written from ~19 modules incl. JNI/FFI surfaces. Wrapping it would change the guard API at every call site (incl. files outside this pass's ownership). Enforced only by review + fuzzer. |
| L8 | `heap` interior locks | **No** — aspirational | Defined in the separate `gc` crate (`gc/src/vm_heap.rs`), which cannot depend on `vm::runtime::lock_order` without a circular crate dependency. |
| L6 | `monitors` registry | **Yes (V11)** — debug builds | Both internal maps of `MonitorTable` (`monitors` and `cas_locks` in `vm/src/threading/monitor.rs`) are now `OrderedMutex` at `LockLevel::Monitors`. Acquiring either while holding *any* equal-or-lower-level wrapped lock trips a debug `assert!`. The inner per-object `Arc<Mutex<()>>` CAS locks stay raw `parking_lot` (L6-internal sub-locks, no global ordering constraint). |
| L9, L7, L5–L0 | all others | **No** — aspirational | Not yet wrapped. |

So today exactly **one** of the three top locks named for this pass is
runtime-enforced: **`monitors` (L6)**. `class_manager` (L10) and
`heap` (L8) remain aspirational for the reasons in the table — they
could not be wired without either touching files owned by other agents
or introducing a circular crate dependency.

A side effect of wiring `monitors`: because both the `monitors` and
`cas_locks` registries sit at the *same* level (L6), the checker forbids
holding one while taking the other. `MonitorTable::remap_after_gc` was
restructured to scope each registry guard into its own critical section
(the two maps are independent, so this is behaviour-preserving).

## Concrete patterns

### Allowed: interpreter calls into the heap

The interpreter holds no global locks of its own; when it needs to
allocate it calls into `heap.alloc_object` which acquires the heap
allocator at L8. No inversion possible.

```text
interpreter (L0/L1)
  → heap.alloc_object       // acquires L8
    → ref_processor.discover_reference  // acquires L7 (< 8) ✓
```

### Allowed: GC stops the world

A GC pause holds the heap lock at L8 and then walks every thread's
monitor stack at L6. Both are below L10, so the GC can also resolve
classes if needed.

```text
heap.collect_garbage (L8)
  → monitors.cleanup_dead_entries (L6) ✓
  → thread_registry.collect_all_root_snapshots (L5) ✓
```

### Forbidden: monitor → class manager

The bug we never want: a synchronized method calls into the JIT, which
needs to resolve a class, which acquires `class_manager`. If another
thread is loading a class while holding `class_manager` and tries to
enter the same monitor, both threads spin forever.

```text
Thread A: holds Monitor M (L6) → wants class_manager (L10)  ✗ INVERSION
Thread B: holds class_manager (L10) → wants Monitor M (L6)  ✗
```

The rule that prevents this: **never call `Lookup.findClass` /
`load_class` while holding a monitor**. The interpreter unwinds the
monitor before calling into the class manager, then re-enters once
class loading completes.

### Forbidden: ref_processor → heap

The cleaner thread drains `cleaner_actions` while holding L3, then
runs each action via `invoke_shared`. If the action allocates, that
acquires L8 — fine, descending. But if the action then tries to
trigger a GC (L8 → wants to scan refs at L7 → wants the cleaner
state at L3) we'd inverse.

The rule: **cleaner actions must complete on a thread that does not
own the cleaner-thread queue**. We enforce this by draining the
queue first (release L3), then iterating the drained vector
(unlocked) and invoking each action.

## Adding a new lock

Before introducing a new `Mutex` or `RwLock` in `vm/`, `gc/`,
`native-builtins/`, `classloading/`, or `jit/`:

1. Pick a level. If unsure, pick the highest unused level above the
   subsystems you'll call into.
2. Document it in the table above.
3. Add a regression test in `vm/tests/lock_order_tests.rs` that
   acquires your lock and tries to acquire an existing one at the
   declared higher level — expect deadlock-detection panic.
4. Get the change reviewed.

## Enforcement

`vm/tests/lock_order_tests.rs` runs a small fuzzer that hammers
multiple threads against the published lock order. Any actual
inversion shows up as a deadlock (caught by the test's wall-clock
timeout) and bisects to the offending change.

## T1.8.3 audit — RwLock across FFI

As of 2026-04-15 the following sites hold `SharedVm::class_manager`
as a writer across non-trivial internal work. None hold across an
FFI boundary or a native method call — all are pure Rust class-table
manipulations with bounded runtime:

- `vm_exec.rs:1559-1567` — `module_registry.add_{reads,exports,opens}`
  each chain the lock into a single expression that releases before
  the function returns.
- `vm_exec.rs:1583,1601,1701,1725,1763` — `let mut cm = …write();`
  blocks are scoped to the local function; audit confirmed none of
  them call into `libloading`, `libffi`, `libc`, or a native method
  registration while holding the lock.
- `vm_exec.rs:2216,2350` — short-lived `method_class_id` lookups.

**Invariant added:** never call `load_class` / `libloading::Library::new`
/ `native_methods.register` / `invoke_native` while holding any
`write()` handle. The brief-load regression test
`t1_vm_under_brief_load_does_not_deadlock` in
`vm/tests/tier1_tests.rs` drives parallel class-access + monitor
traffic and serves as the smoke gate.

## T1.8.5 audit — `unsafe` block SAFETY comments

The repo has ~448 `unsafe { … }` blocks across `vm/`, `jit/`, and
`gc/`. Exhaustive back-fill of SAFETY comments is tracked as a
follow-up; T1.8.5 landed the enforcement mechanism:

- `#![warn(clippy::undocumented_unsafe_blocks)]` is now active at
  the top of `vm/src/runtime/interpreter.rs` (the hottest GC /
  exception / safepoint path).
- Every T1.1.a-added `unsafe` block inside
  `vm/src/jit/conservative_roots.rs::scan_oop_slots` and
  `scan_one_frame_precise` already carries a `// SAFETY:` comment.
- The NEW-18 `panama_libffi` module carries SAFETY comments on every
  libffi-facing `unsafe` (thread-local ActiveContextGuard).
- New unsafe blocks added anywhere under the gate generate a clippy
  warning in CI, providing the forward-looking enforcement.

## See also

- [Roadmap T1.6 — Threading & JMM](roadmap-100.md)
- [Roadmap T1.8 — Native API consistency](roadmap-100.md)
- HotSpot's own lock-order documentation in
  `src/hotspot/share/runtime/mutexLocker.hpp` (informational —
  CratonVM uses different locks but the principle is the same).
