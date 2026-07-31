# `PriorityBlockingQueue`'s native `offer()` didn't refresh its backing-array/element `ObjectRef`s across a re-entrant `compareTo()` call — corrupted concurrent H2 MVStore compaction — **FIXED**

## Status
**FIXED** (2026-07-31) — root cause confirmed by an isolated, deterministic
reproduction *and* by an A/B on the reporting H2 test, fixed, and gated by a new
regression-suite class. A second, independent defect in the same natives (a
stop-the-world wedge) was found while reproducing this one and fixed in the same
change; on JIT-on runs it is what both affected classes hit first.

Original report: found while investigating H2 suite regressions in the
twelfth-pass `TestUpgrade` session's follow-up full-suite run (2026-07-30/31),
by code inspection against the already-fixed sibling `tm_binary_search`.

Fix commit: `fix/pbq-stale-objectref-20260731`, branched from `dev` @ `a31a8a93fe`.

## What was wrong

### Defect 1 — stale `ObjectRef`s across the binary search (the reported bug)

`native-collections/src/lib.rs`'s `pbq_offer_locked` (backing
`PriorityBlockingQueue.offer(Object)`, which real bytecode `add(E)` calls
through) did a sorted-array binary search, capturing the backing array `arr`
once before the loop and reusing both it and the argument `elem` after every
comparison:

```rust
let arr = ...;                                   // read ONCE, before the loop
while low < high {
    let mid_elem = ctx.get_array_element(arr, mid);
    let cmp = tree_compare(ctx, &comparator, mid_elem, elem)?;   // real compareTo
    ...
}
for i in (low..size).rev() { ... }               // still the pre-loop `arr`
ctx.set_array_element(arr, low, elem);           // and the pre-loop `elem`
```

`tree_compare` → `natural_compare` → `compare_via_compare_to` dispatches the
element's **real, interpreted** `compareTo()` whenever the element isn't a
`String` or a homogeneous primitive wrapper — exactly
`FileStore$RemovedPageInfo`'s case. That is a full re-entry into the VM: it can
allocate and trigger a young collection, which relocates (or promotes) both the
backing array and the element. Neither was pinned or re-read.

Two observable shapes, both reproduced:

* the **next** iteration's `get_array_element(arr, mid)` decodes a relocated
  address and aborts the VM with `gen_heap.rs` `assertion left == right failed,
  left: Object, right: Array`; and
* the final `set_array_element(arr, low, elem)` stores a stale `elem`, which a
  later `offer()`/`poll()` reads back as whatever now occupies that address —
  the originally reported

```
Caused by: java/lang/ClassCastException: java.lang.Object cannot be cast to
  org.h2.mvstore.FileStore$RemovedPageInfo
	at org/h2/mvstore/FileStore$RemovedPageInfo.compareTo(FileStore.java:2172)
	at java/util/concurrent/PriorityBlockingQueue.add(PriorityBlockingQueue.java:450)
	at org/h2/mvstore/FileStore.accountForRemovedPage(FileStore.java:2105)
```

The same hazard was present in `pbq_ensure_capacity` (`alloc_ref_array` can
collect, and the copy loop then read out of the *old*, possibly-moved array and
wrote the field through a possibly-moved `this`) and in `native_pbq_init`.

This is the **same bug family** already fixed in the sibling `TreeMap`/`TreeSet`
array-mode binary search (`tm_binary_search`/`ts_binary_search`, same file, see
their doc comments and
`docs/internal/fixed-suite-bugs/wildfly/wildfly-parallel-boot-stale-objectref-residual.md`);
`pbq_offer_locked` had never been given the same treatment.

### Defect 2 — the PBQ natives wedged the whole VM at a stop-the-world barrier

`native_pbq_offer`/`poll`/`take` acquired the queue monitor with the plain
`ctx.monitor_enter`, which deliberately blocks as a **counted** mutator (see its
long doc comment in `vm/src/vm/vm_exec.rs`: the wholesale switch to
`monitor_enter_blocking` was reverted precisely because most native callers keep
using raw `ObjectRef`s across the wait without pinning). A writer parked on the
queue monitor therefore never arrived at a concurrent stop-the-world barrier, and
the collection never completed:

```
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still
  waiting for cooperative mutators rounds=64 pending=3 taken=0
```

Same shape as the WildFly `parallel-extension-add` boot hang that motivated
`ChmMonitorGuard::acquire_gc_safe`
(`docs/internal/fixed-suite-bugs/stw-crossthread-jit-takeover-hang-cluster.md`).
With the JIT on, this — not the `ClassCastException` — is what both affected H2
classes hit first: each hung for the full 1500 s timeout without ever reaching
the corruption.

## The fix

`native-collections/src/lib.rs`:

* `pbq_offer_locked` — pins `this`, `arr` and `elem` and re-reads all three
  through their pins after **every** `tree_compare` call (including on the error
  path), mirroring `tm_binary_search` exactly. The post-loop shift/insert and the
  trailing `set_field(PBQ_FIELD_SIZE)` now use the refreshed references.
* `pbq_ensure_capacity` — pins `this` and the old array across `alloc_ref_array`,
  re-reads both, and returns the forwarded `this` (`#[must_use]`) so the caller
  cannot reuse its pre-call copy.
* `native_pbq_init` — pins `this` across the initial `alloc_ref_array`.
* `native_pbq_offer`/`native_pbq_poll`/`native_pbq_take` — pin `this` (and
  `elem`/the dequeued head) for the whole critical section, and acquire the
  monitor with `ctx.monitor_enter_gc_safe`, the sanctioned escape hatch for
  callers that pin-and-refresh. `monitor_exit` is now issued on the *current*
  address (`Monitors::remap_after_gc` moves the table entry, so exiting on the
  stale address would have been wrong). `native_pbq_take` also re-reads `this`
  after every `monitor_wait` iteration.

`native_pbq_peek` and `pbq_poll_locked` need no change: neither allocates nor
re-enters the interpreter between capturing a reference and using it.
`pbq_seed_real_lock` was already correct and is unchanged.

## Reproduction / regression gate

`regression-suite/src/RPriorityQueueGc.java` is the permanent gate. Its
`compareTo` allocates deliberately so the collection lands *inside* the native,
which makes both failure shapes deterministic instead of load-dependent. The
suite runs it under `--nojit --Xmx 64m` via the new `cv_extra_args` hook in
`regression-suite/run.sh` — **with a live JIT frame on the stack the young
generation falls back to a non-moving sweep** (`[moving-young] fallback:
reason=unregistered-jit-frame-on-stack`), under which a stale reference still
resolves and the defect hides entirely. Verified as a real gate:

| binary | `ONLY=RPriorityQueueGc bash regression-suite/run.sh` |
| --- | --- |
| `origin/dev` @ `a31a8a93fe` | **FAIL** `rc=1: native method panic` (gen_heap Object/Array) |
| with this fix | **PASS** (output matches HotSpot) |

## H2 verification

Azure host `20.83.144.174`, worktree `/data/data/wt-h2-pbq-staleref-20260731`,
debug binaries `cvm-pbq-base-20260731` (pure `origin/dev` @ `a31a8a93fe`) and
`cvm-pbq-fix2-20260731`. One process per class, `--Xmx 1g`, 1500 s cap.

**JIT on** — defect 2 dominates; the corruption is never reached:

| class | `origin/dev` | with this fix |
| --- | --- | --- |
| `org.h2.test.db.TestCompatibility` | **HUNG** 1500 s (`rc=124`), `STW … rounds=64 pending=1 taken=0` | completes in 191 s |
| `org.h2.test.db.TestMultiThread` | **HUNG** 1500 s (`rc=124`), same wedge | completes in 643 s |

**`--nojit`** — no JIT-takeover STW, so defect 1 is exposed directly. This is the
clean A/B for the reported symptom:

| class | `origin/dev` | with this fix |
| --- | --- | --- |
| `org.h2.test.db.TestCompatibility` | `rc=1` after 1014 s — **`ClassCastException: java.lang.Object cannot be cast to org.h2.mvstore.FileStore$RemovedPageInfo`**, raised exactly as reported from `FileStore.accountForRemovedPage` → `MVStore.panic` | **`rc=0`, PASSES**, 714 s, zero `RemovedPageInfo` casts |

`TestCompatibility` therefore goes from *fails with the reported corruption* to
*passes outright*.

### Remaining `TestMultiThread` failures are a different, already-tracked bug

`TestMultiThread` no longer hangs and never shows a `RemovedPageInfo` cast or
"The database has been closed" — the symptom the original report flagged as a
possible downstream effect is gone. It still exits non-zero, on two causes
neither of which is this bug:

* `ExecutionException` wrapping `CloneNotSupportedException`, raised from
  `org.h2.mvstore.tx.VersionedBitSet.<init>` (`VersionedBitSet extends BitSet`
  and clones itself) via a nonsensical `java.lang.Thread.clone` frame. This is
  the byte-for-byte same mechanism already tracked as
  `docs/known-issues/h2/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame.md`;
  `TestCompatibility` (JIT on) and `TestMultiThread` are two further carriers,
  and `VersionedBitSet` is a much smaller reproduction than `TestTempTables`'
  `testLotsOfTables`. Noted on that doc.
* On one run, H2's own `job.get(5, TimeUnit.MINUTES)` expired
  (`TimeoutException`). HotSpot runs the whole class in **4.5 s** on the same
  host, so there is a real throughput gap here — but it is a throughput gap, not
  a correctness one, and it is not this bug.

## Audit of the rest of the family

Every array-mode binary search in `native-collections/src/lib.rs` that can
dispatch a real `compareTo`/`Comparator` was re-checked:

* `tm_binary_search`, `ts_binary_search` — already hardened (2026-07-13).
* `pbq_offer_locked` — fixed here.
* `cslm_binary_search`/`cslm_ensure_capacity` (`ConcurrentSkipListMap`) — has the
  identical unhardened shape, but it is **dead code**: the registration is
  deliberately disabled (`let _ = register_concurrent_skip_list_map_natives;`;
  the real JDK bytecode is used instead). Confirmed empirically — a
  `ConcurrentSkipListMap` port of the probe passes unchanged on the *pre-fix*
  binary. Left alone; if that registration is ever re-enabled it must be
  hardened first.

Not audited here, and worth a separate look: the `TreeMap`/`TreeSet` range
helpers (`native_tm_head_map`/`tail_map`/`sub_map` and their `TreeSet` twins)
pin their `result`/`comparator`/boundary key correctly, but iterate a bare
`Vec<(Value, Value)>` returned by `tm_collect_pairs` whose entries are never
refreshed across the `tree_compare`/`native_tm_put` calls in the loop body.

## Verification summary

* `cargo test -p cratonvm-native-collections` — 156 tests, 0 failures.
* `cargo fmt -p cratonvm-native-collections -- --check` clean; `cargo clippy -p
  cratonvm-native-collections` clean.
* Full `regression-suite/run.sh` on the fixed binary: 18 passed, 1 failed
  (`RSerial`) — `RSerial` fails identically on the unmodified `origin/dev`
  binary, i.e. pre-existing and unrelated.
