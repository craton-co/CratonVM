# The NIO socket transfer path — per-call costs removed, 2026-09-04

| | |
|---|---|
| **Status** | **IMPLEMENTED; the removed work is PRICED, the end-to-end effect is NOT.** Every cut below is in the tree behind its own kill switch, with a census that says whether it engaged. No throughput number has been taken, and none is claimed here |
| **Opened** | 2026-09-04, from a source read of the socket path prompted by the HTTP clusters' gap against HotSpot |
| **Touches** | `native-io/src/socket_fast_io.rs` (new), `native-io/src/socket_channel.rs`, `native-io/src/nio_selector.rs`, `native-api/src/registry.rs`, `vm/src/vm/vm_exec.rs`, `vm/src/vm/vm_init.rs` |
| **Acceptance vector** | `regression-suite/src/RSocketFastIo.java` — 43 checks; passes on Temurin 25.0.3+9 and on CratonVM with byte-identical observables, in all four switch arms |

## What was found

Six per-call costs, all on the path every HTTP byte crosses. They are listed in
the order they are expected to hurt, which is a **hypothesis about magnitude
derived from which costs scale with traffic and connection count** — not from a
profile. Overturning that order is a good outcome for the measurement below.

### F1 — every transfer allocated and zeroed the destination's remaining capacity

`sc_read` computed `len = limit − position` and did `vec![0u8; len]` *before*
asking the kernel how many bytes were waiting. An event loop reading a 120-byte
keep-alive request into netty's 64 KiB receive buffer allocated and zeroed
64 KiB to move 120 bytes. The cost scaled with the buffer the application
**offered**, not with the traffic it **got** — backwards for the small-message,
high-frequency shape HTTP has. The same shape was in `sc_write`,
`sc_write_gathering`, `sc_read_scattering`, `sc_blocking_read` and
`sc_blocking_write_fully`.

### F2 — direct buffers take the slowest path, not the fastest

`ByteBuffer.allocateDirect` memory in this VM is an `Unsafe`-arena **handle**,
not a real pointer (`direct_buffer.rs`'s own measured note:
`CRATONVM_DBG_DBB_ELEM=1` reports `raw-pointer=0 arena-handle=N`). So a direct
transfer takes `copy_to_native_memory`'s tagged-handle branch — an `RwLock` read
plus a `BTreeMap` range probe — on top of the bounce copy. Netty, Tomcat NIO,
Jetty and Undertow all default to direct buffers because on HotSpot that is the
zero-copy path; here it was the most expensive one.

### F3 — the selector's per-tick cost was O(all registered keys) under a global lock

After every `select()`, `apply_ready_ops` snapshotted **every** key of the
selector, took the write half of `sk_table()` — one `RwLock` shared by every
selector in the process — and recomputed `identity_hash_code` per key. A server
holding 5 000 idle keep-alive connections paid 5 000 hash computations and map
probes to report one ready socket, with every other event loop blocked on that
lock meanwhile. The cost was driven by connection count, which is the axis an
HTTP throughput test scales.

The recomputed hash was pure waste: `KeyState::key_hash` already stores it, and
exists precisely because it is GC-stable where the `ObjectRef` is not.

### F4 — publishing ready keys re-enters the interpreter once per key, permanently

`populate_selected_keys_field` calls `ctx.invoke_virtual(set, "add", …)` per
ready key. Per
[[native-reentry-hides-the-callee-from-tierup]], a callee reached that way from
a Rust native has no bytecode call site, so no inline-cache entry and no
invocation counter: netty's `SelectedSelectionKeySet.add` runs interpreted for
the life of the process and **no tier-up lever can reach it**.

### F5 — gathering writes allocated per source and copied the payload twice

`sc_write_gathering` allocated one `Vec` per source buffer, then concatenated
all of them into one more `Vec` before a single `send`: `N + 1` allocations and
two full passes for what the kernel receives as one call. This is netty's normal
write path — header buffer plus body buffer, for every HTTP response.

### F6 — `ByteBuffer` fields were resolved by name on every transfer

`buffer_access` read `position`, `limit`, `hb` and `offset` through
`get_field_by_name`, and `buffer_advance` did one more get and one set the same
way. Each is `resolve_field_index_in_hierarchy` under the class-manager read
lock — a string hash and a hierarchy walk — six to eight times per transfer,
re-deriving a constant. Same defect and same fix as `al_slots_for` in
`native-collections`, which went 1014 → 337 ns when memoized.

## What was changed

`native-io/src/socket_fast_io.rs` is new and holds the three shared pieces: the
reusable transfer buffer, the per-`ClassId` `ByteBuffer` layout cache, and the
census.

| | change | switch |
|---|---|---|
| F1 | `Scratch` — a per-thread buffer held at its high-water mark, returned on `Drop` so the seven early-return arms of `sc_read` cannot leak it. Steady state allocates nothing and zeroes nothing | `CRATONVM_SC_SCRATCH=0` |
| F2 | Census only — see "What was deliberately not done" | — |
| F3 | `KeyState::mirrored_ready` records what was last pushed to `sk_table`. The walk moved under the selector's own (uncontended, already-held) mutex; the global lock is taken only when readiness actually moved, and never on a quiet tick. The stored `key_hash` replaces the recomputed one | `CRATONVM_SEL_READY_CACHE=0` |
| F4 | `append_selected_keys_fast` writes `keys[size++] = key` directly for netty's `SelectedSelectionKeySet`, guarded on the exact class name and on the array having room | `CRATONVM_SEL_FAST_KEYS=0` |
| F5 | One scratch sized to the total, each source copied straight to its offset. `N + 1` allocations and one full copy pass removed | `CRATONVM_SC_SCRATCH=0` |
| F6 | `BbSlots` memoizes the slot indices per receiver `ClassId`, with a negative entry for classes that do not resolve | `CRATONVM_SC_BB_SLOTS=0` |

Census: `CRATONVM_SC_IO_STATS=1`, printed from `lang_system.rs`'s exit path
alongside the `FileChannel` one — **not** from `vm-cli`, because a JUnit runner
leaves through `System.exit` and never reaches `vm-cli`'s normal-return arm.

### A defect this work introduced, and how it was found

Both new caches — the `ByteBuffer` layout cache (F6) and the netty key-set
layout cache (F4) — were first written as process-global maps keyed by
`ClassId`. **That is wrong**, and silently so.

A `ClassId` is an INDEX into its own VM's class store
(`ClassId::new(self.classes.len())` in `classloading/src/class.rs`), and
several `SharedVm`s can live in one process — which is exactly why
`jit_invalidate_adapter` has to fan out over `live_hook_vms()` instead of
addressing one VM. Keyed on `ClassId` alone, one VM's `HeapByteBuffer` layout
would answer another VM's lookup for whatever class happened to share that
index, and the result is a wrong-but-in-bounds slot read: nothing raised, just
a channel that desynchronises.

Both are now keyed by `(vm_identity, ClassId)`.
`NativeContext::vm_identity`'s own doc already required this — *"Native side
caches ... must scope entries to this value; process-global object caches are
otherwise stale across VM lifetimes"* — so the rule existed and this code
broke it.

**How it surfaced is the part worth keeping.** It was not found by review. The
F4 unit tests failed, then PASSED when a debug `eprintln!` block was added
ahead of the call — an ordering-dependent result, which is the signature of
shared global state rather than a logic error. Each `MockNativeContext`
restarts its class table at `ClassId(1)`, so two unrelated tests describing
two different classes collided on one cache entry: the multi-VM embedding case
in miniature, reproduced by accident.

Two test-support defects had to be fixed before those tests could say anything
at all, and both are the same species — **a mock that answers a question it
does not model**:

* `MockNativeContext::resolve_field_index_by_class_id` was an unconditional
  `None` stub. Any fast path gated on a resolvable layout therefore REFUSED
  inside every unit test, so a test asserting the right outcome was asserting
  the SLOW path's answer while claiming to cover the fast one. Now backed by a
  `declare_field` table, still `None` for undescribed classes, so no existing
  test changes behaviour.
* `vm_identity` was the trait default `0` for every mock, making every test
  share one cache scope. Each mock now reports a unique identity, which is what
  makes cache scoping testable at all.

### The three guards that make these safe rather than fast

* **The scratch is not zeroed between transfers.** That zeroing is the cost
  being removed, so the buffer still holds the previous transfer's bytes.
  Every consumer honours the kernel's count exactly, and `Scratch::prefix` is
  the only way the socket paths look at the contents. `RSocketFastIo` section 2
  is written to catch a regression here and is the reason it pre-fills its
  destination with a sentinel: a path that copied its whole scratch region
  would deliver the previous message's tail and **throw nothing**.
* **The slot cache is invalidated on layout change.** `jit_invalidate_adapter`
  in `vm_init.rs` already fires on JVMTI redefine and on
  `upgrade_synthetic_class` / `recompute_subclass_layouts` — the events that can
  move a field under a live `ClassId` — and now clears the cache too. Without
  it the failure is the silent one: reading `position` from a slot that now
  holds something else.
* **The netty key-set fast path refuses rather than grows.** It requires the
  array to have room (`size + n < keys.length`, strictly, preserving netty's own
  post-`add` invariant) and falls through to `invoke_virtual` otherwise, rather
  than reimplementing `increaseCapacity`. It matches on the exact class name
  because a subclass may override `add` and only the generic path honours an
  override.

## What was deliberately not done, and why

**Zero-copy into a heap `ByteBuffer` — refused, needs pinning that does not
exist.** `array_data_ptr` yields a flat base pointer, so handing the kernel the
Java array looks available. It is not: the transfer happens inside
`begin_blocking_region`, during which a concurrent collection may relocate the
array. `pin_native_root` repairs a *reference* across such a move; it does not
stop the object moving, so a raw data pointer taken before the syscall is
invalid after it. Removing this copy needs genuine address pinning — HotSpot's
`GetPrimitiveArrayCritical` equivalent — which this VM does not have. **The
bounce is structural until that exists.** What F1 removes is the allocation and
the zeroing, not the copy.

**Zero-copy into a direct `ByteBuffer` — not built, because the evidence says it
is unreachable.** Off-heap memory cannot move, so this one is removable in
principle — but only for a REAL pointer, and `allocateDirect` hands back arena
handles. Rather than write an unsafe fast path for a population that may not
exist, the two are now COUNTED (`direct-raw` vs `direct-arena` in the census) so
a live HTTP workload answers the question before anyone writes the code. If the
census reports a meaningful `direct-raw` share, that is the trigger to build it.

**`writev` / `WSASend` — not built, because it would not remove a copy.** The
plan that opened this work named vectored I/O as the F5 fix. Working it through:
the sources are Java-heap or arena-handle buffers, so each has to be copied out
before the kernel can see it either way. Single-scratch and `writev` therefore
both make exactly ONE copy, and single-scratch is simpler and platform-neutral.
`writev` only wins when the sources are already real off-heap pointers — the
same precondition as the paragraph above, and it should be built together with
that, or not at all.

## What is measured, and what is not

### The removed work, priced directly

`cargo test --release -p cratonvm-native-io --lib -- --ignored --nocapture scratch_cost`,
on the merged tree, Windows 11, quiet host, minimum of three passes per row
(the minimum measures the code; the mean measures the load):

| offered capacity | alloc + zero | reuse | saved per call |
|---|---:|---:|---:|
| 128 B | 70 ns | 25 ns | 45 ns |
| 1 KiB | 76 ns | 25 ns | 51 ns |
| 8 KiB | 190 ns | 25 ns | 165 ns |
| 64 KiB | 1 904 ns | 23 ns | **1 881 ns** |

**The shape is the result, not the absolute numbers.** The old path's cost rises
27x across the sweep because it is a function of the capacity the application
OFFERED; the new path is flat at ~25 ns because it is a function of nothing.
That is the defect and the fix stated in one table. 1 904 ns to zero 64 KiB is
~34 GB/s effective, which is what an L2-resident `memset` should cost — the
number is physically sensible rather than an artifact.

The residual ~25 ns is `Scratch::new`'s thread-local take/put and its length
check. It does not scale, so it does not participate in the defect.

### What this does NOT establish

This prices the work that was removed. It does NOT say what fraction of a real
`read()` that is, because the surrounding syscall cost is untouched and is not
measured here. At netty's 64 KiB buffer the saving is 1.88 us per read; whether
that is 3% or 50% of a read depends on the syscall's own cost in the workload,
and **this host cannot supply that figure** — see below. Multiplying 1.88 us by
a call count from the census gives a CEILING on the saving, not a prediction.

### The end-to-end probe, and why its number is not quoted

`probes/SocketTransferCostProbe.java` exists and is committed, and its result
is that **it cannot resolve this effect on this host**. Recorded because the
next person will otherwise build it again:

* one loopback write+read pair costs ~65 us here, and rounds drift 2x under
  ambient load (one round read 130 us against another's 67 us);
* the first design — sweep the capacity, compare rows — put a few-us effect
  against a ~15% row-to-row spread;
* the second — interleave both capacities inside one loop so common-mode noise
  cancels, and report the difference — is a better instrument and still not
  enough. **The HotSpot control, which has no per-call allocation and must
  therefore show no capacity dependence at all, reported deltas from -4 350 to
  +6 168 ns.**

A quantity smaller than its own control's spread is not measured. The control
is what makes that a finding rather than a disappointment: without it, this
page would be quoting a CratonVM delta of the same magnitude and calling it a
result.

A quieter host, or a workload whose reads are served from an already-full
receive buffer rather than a ping-pong round trip, would resolve it. The Azure
hosts are the obvious candidates.

## The A/B recipe, for when a real workload is available

This page claims no speedup, and the code should not be described as having one
until the following exists. The discipline is the `FileChannel` fast path's,
because that is what worked:

```bash
# one binary, both arms, ABBA-interleaved, three rounds, quiet host
CRATONVM_SC_SCRATCH=0 CRATONVM_SC_BB_SLOTS=0   CRATONVM_SEL_READY_CACHE=0 CRATONVM_SEL_FAST_KEYS=0    # the "off" arm
CRATONVM_SC_IO_STATS=1                                    # the census, both arms
```

Four switches, not three, and they are deliberately separable: F3 and F4 are
independent cuts to the same function, and F3's failure mode — a latched cache
suppressing a real readiness change — presents as a reactor that stops being
told about a socket rather than as an error. An A/B that could only turn both
off together could not say which one did it. This gap was found by running the
"off" arm and noticing `select ticks clean=2 dirty=2` was IDENTICAL in both
arms, which is what an engagement census is for.

* **Quote the ratio to a plain-Java control, not the wall clock.** Absolute
  ns/op on the Azure host moves ~1.8x between load 2.5 and load 10, while the
  ratio held to within 0.3x across that swing. The control that matters here is
  the one `cratonvm-shims-cost-per-call-not-per-byte` used: the same echo loop
  over a hand-written socket wrapper on the same VM, which separates "the socket
  natives are slow" from "the interpreter is slow" in one column. Without it,
  this work can close the whole socket-path gap and the number barely moves
  because the per-call interpreter floor was always the larger half.
* **Read the census before reading the timing.** `scratch hit=0` means the path
  never engaged and the timing is measuring something else.
  `direct-arena` ≫ `direct-raw` confirms F2's premise; the reverse refutes it
  and re-opens the zero-copy direct work.
* **F3's acceptance test is a curve, not a delta.** Same request rate,
  connection count 10 → 1 000 → 10 000, ns per request. The old cost rose with
  connections at constant request rate; if the curve does not flatten, F3 did
  not do what it claims. `select ticks clean=N dirty=M` is the direct reading:
  `clean` ticks took the global lock zero times.
* **F4 needs netty to engage at all.** `selkeys fast=0 generic=N` under the
  regression suite is the correct and expected reading — the suite has no netty
  on its classpath. Only a netty run can move that row.

## Acceptance evidence

`regression-suite/src/RSocketFastIo.java`, registered in `run.sh`'s
`CORE_CLASSES`. 43 checks, every one comparing an exact byte or an exact
position rather than "no exception", because that is the failure mode these
paths have. It covers the heap and direct destinations, the sentinel tail past
the byte count, a window inside a larger buffer (position and limit both away
from the array bounds, so a mis-cached slot lands bytes in the wrong place
instead of failing), a gathering write with an empty source in the middle, a
scattering read, the non-blocking zero-not-EOF convention, a zero-room
destination, and — twice, which is the point — selector readiness, so a
readiness-mirror cache that latched would pass round one and fail round two.

Passing on Temurin 25.0.3+9: `PASS RSocketFastIo (43 checks)`.

## Related

* `performance/filechannel-heap-read-glue-depth-FIXED-20260823.md` — the same
  playbook on the file half, and the source of the kill-switch-plus-census
  discipline used here.
* `known-issues/netty/*` — the throughput walls this work is aimed at.
* The residual after all of this is the general per-call and per-bytecode price,
  which `jit-entries-per-call-cost-is-the-call-dense-wall` owns. If the census
  says the fast paths engaged and the gap is still large, that is where the rest
  of it lives — not here.
