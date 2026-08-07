# `org/h2/` JIT ban (HIB-LONGTAIL.1) and its residuals — CLOSED 2026-08-05

**Status: ✅ CLOSED.** Every question this page and its companion
(`h2-jitban-longtail1-SUPERSEDED-20260728-ab-record.md`) existed to answer is
settled:

| item | verdict |
|---|---|
| the `org/h2/` package ban itself | **gone** — there is no such ban in the tree any more |
| residuals 1-3 (`TestPageStoreCoverage`, `TestReopen`, `TestRunscript`) | fixed 2026-07-28, re-verified here |
| `TestMetaData` / `new TreeMap<>(cmp)` losing its comparator | fixed 2026-07-31, re-verified here |
| `TestMemoryEstimator` "new sole blocker" | **withdrawn** — it rested on a null A/B |
| residual 4 — `nioMemLZF:` per-element throughput | **closed**, ~2x (101 → ~50 ms/op) — and *not* by this page; details below |

What is NOT closed, and why it is not this page's business, is at the bottom.

## The ban does not exist

`vm/src/jit/skip_list.rs` is gone; nothing in `jit/` or `vm/` refuses to compile
`org/h2/`. `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/` is a no-op — measured with
`CRATONVM_DBG_JIT_COMPILED=1`, the default build compiles 27 `org/h2/…` methods
and the flag arm compiles 26. Two identical configurations.

Every A/B in the earlier revisions that used that flag as its only independent
variable therefore measured nothing, including the "+7 PASS" the last revision's
headline rested on and the "`TestMemoryEstimator` is the sole blocker"
storyline built on top of it. Both are withdrawn.

**The lesson generalises**: before A/B-ing any JIT ban, count actual compiles in
both arms. A ban's comment outlives the ban.

## Re-verification

`run-h2-suite.sh`, 400 s cap, one class at a time, against a control build of
the same `dev` without this work:

| class | control | this work |
|---|---|---|
| `TestPageStoreCoverage` | PASS | PASS |
| `TestReopen` | PASS | PASS |
| `TestRunscript` | PASS | PASS |
| `TestMetaData` | PASS | PASS |
| `TestMemoryEstimator` | PASS | PASS |
| `TestStreamStore` / `TestNestedJoins` / `TestCompatibilityOracle` / `TestCompatibilitySQLServer` | PASS | PASS |

Measured 2026-08-02 on an older `dev`, all four of the first group CRASHed in
both arms — that was
`docs/internal/unresumable-unconditional-trap-mvmap-FIXED-20260802.md`, an
unrelated JIT regression that landed after this page was last touched and was
fixed on 2026-08-03. They were confirmed even then to be that regression and not
this page's residuals: all four turned PASS under its documented
`CRATONVM_JIT_DENY=MVMap.evaluateMemoryForKey,MVMap.evaluateMemoryForValue`
workaround.

`probes/TreeMapCmpProbe.java` — the 60-line pure-JDK witness for the
`TestMetaData` root cause — passes 40000/40000.

## Residual 4 — `nioMemLZF:` per-element throughput

### What it was

`org.h2.store.fs.niomem.FileNioMemData` stores pages as
`ByteBuffer.allocateDirect`, so `org.h2.compress.CompressLZF` uses its
`(ByteBuffer, …)` overloads — which move **one byte per call** through four
accessors (`get()`, `get(int)`, `put(byte)`, `put(int, byte)`). A 64 KB page is
~65,000 dispatches per pass, and `TestFileSystem.testConcurrent` does 10,000
operations against each of two `nioMemLZF:` prefixes.

An earlier revision replaced the JDK's five-nested-invocation bytecode chains
with natives (`native-io/src/direct_buffer.rs`) and got 390 → 91 ms/op. It then
concluded that the remaining ~200x was irreducible interpreter cost and that
only a JIT intrinsic could close it. That conclusion was wrong about where the
time went.

### Where the time actually went

`probes/DbbElemProbe.java` measures the four accessors alone — no filesystem, no
compressor, no lock:

| | HotSpot | CratonVM, 2026-08-02 |
|---|---|---|
| `get(int)` | 0.4 ns | **647 ns** |
| `put(int, byte)` | 0.6 ns | **684 ns** |

`perf record` over `probes/LzfProbe.java` attributed ~85% of that to dispatch,
not to the access: a full compile probe on every call for a callee that is a
registered native and therefore can never have a compiled entry; the native
registry's `(class, name, descriptor)` hash; the `safe_native_call` funnel; three
or four `ctx.get_field` calls each re-resolving the field's declared descriptor
and re-forwarding the reference; and the `Unsafe` arena's `RwLock` + `BTreeMap`
probe.

### What closed it — and who closed it

**Not this page.** The first two costs were removed independently and generally,
by `perf/leaf-native-jit-bypass-20260804` (the native funnel's fixed cost turned
out to be two `Arc` refcounts in `thread_state::with_cell`) and
`perf/aqs-unattributed-20260805` (the JIT now site-caches *every* native reached
from compiled code, not just the leaves). Together they took the accessors from
647 to ~294 ns and `nioMemLZF:` from ~101 to ~50 ms/op.

That is worth stating plainly because this session first re-derived the same two
fixes independently, under different names, and measured a 5x against the
2026-08-02 `dev` — before those merges existed. Rebasing onto current `dev` and
re-measuring **interleaved** showed the two mechanisms were the same work:

| interleaved `DbbElemProbe`, three alternating rounds | `get(int)` | `put(int, byte)` |
|---|---|---|
| current `dev` | 249.8 / 283.0 / 269.0 ns | 322.6 / 318.4 / 299.6 ns |
| + the remaining changes here | 295.8 / 293.9 / 264.8 ns | 321.9 / 320.6 / 263.2 ns |

Indistinguishable. So the duplicate mechanism was dropped rather than landed
twice, and with it went the two further changes it had been carrying — batched
descriptor-hinted field reads and a raw-block rewrite of the `Unsafe` arena.
The arena one in particular was a real correctness risk (raw pointers, byte
writes under a read lock, `unsafe impl Sync`) in a security-sensitive component,
and it had **no measured benefit once the general fixes were in**. This page's
own earlier revision states the rule it is being held to: *a change that carries
a correctness risk for zero measured throughput does not land.*

What is kept from that line of work is only what stands on its own:

* `NativeHeapAccess::get_field_typed` / `get_fields_typed` — the descriptors of
  `Buffer.{address,limit,position}` and `ByteBuffer.isReadOnly` are fixed by the
  JDK's own declarations, so the per-read metadata lookup inside `get_field` is
  answering a question the caller already knows, and batching collapses four
  reference-forwarding reads into one. Additive, defaulted, no measured delta.
* `copy_to_native_memory` no longer runs an *uncached* `getenv` for
  `CRATONVM_DBG_HEAPCOPY` once per byte written — a bug-shaped inefficiency, and
  the same read-once treatment `blockgc_dbg` beside it already had.
* `CRATONVM_DBG_DBB_ELEM`, the per-accessor census, which is what established
  the two facts in *What is left* below.

`memLZF:`, `nioMemFS:` and `memFS:` are sub-millisecond and unchanged.
`regression-suite` is 22/22 including `RDirectBufferElem`, the 444-check witness
that these accessors are byte-identical to the bytecode they replace.

The decisive consequence, measured 2026-08-02: `TestFileSystem` no longer stops
at `nioMemLZF:`. It cleared `memFS:`, `memLZF:`, `nioMemFS:`, `nioMemLZF:1:`,
`nioMemLZF:12:`, `rec:memFS:`, `testUserHome()` and `cache:` and ran on into
`nioMapped:` at 784 s — see the hand-off below.

**That is not reproducible on `dev` as of 2026-08-05**, for a reason unrelated to
anything here: the class now dies in ~1 second on the *first*, plain-disk
filesystem with `IOException: pread0/pwrite0: bad addr/len/pos`, identically on
`dev` and on this branch, where the 2026-08-02 binary did not. Filed as the
`bug-h2-testfilesystem-pread0-bad-addr-len-pos` write-up, which closed
2026-08-07 as not reproducible — including at this very commit, so what that
2026-08-05 arm measured is not settled.
So `TestFileSystem` cannot presently serve as this page's acceptance test; the
`DbbElemProbe` / `LzfProbe` / `DirectReclaimProbe` numbers above and the
per-class table earlier are what the verdict rests on.

### What is left of residual 4

The gap to HotSpot is still ~2 orders of magnitude, and it is the dispatch that
remains: the funnel, the MIC helper's prologue, the `Value` marshalling and the
arena's read lock. Two facts bound what can be done about it, and both were
measured here rather than assumed:

* **These four cannot be registered as LEAF natives**, which is why the general
  leaf bypass does not reach them. Their deliberate bail to real bytecode
  re-enters Java, and `NativeMethodRegistry::set_leaf`'s contract forbids that.
  Claiming leaf anyway — measured as a throwaway, then reverted — buys ~15%
  (46 → 39 ms/op interleaved), so the prize is real but small and does not
  justify breaking the contract. **Closing the bail instead is the cheap next
  step**: raise the JDK's own `IndexOutOfBoundsException` from the native rather
  than deferring to bytecode for it, and the leaf claim becomes honest.
* **`ByteBuffer.allocateDirect` memory is an `Unsafe`-arena handle, not a real
  pointer.** `CRATONVM_DBG_DBB_ELEM=1` reports `raw-pointer=0
  arena-handle=2000000` for `DbbElemProbe`. So the structural answer the
  previous revision named — a JIT intrinsic lowering the element access inline
  with no call at all — cannot be emitted as things stand: inline code cannot do
  a locked map probe to resolve a handle.

Both are carried in the block comment above `DbbElemFields` in
`native-io/src/direct_buffer.rs`, which is where whoever picks it up will be.

## The bug residual 4's slowness was hiding: direct buffers were never reclaimed

With the throughput fixed, `TestFileSystem` reached
`OutOfMemoryError: Direct buffer memory: tried 65536, used 1073737362, max
1073741824` after 614 s. `probes/DirectReclaimProbe.java` reduces it to fifteen
lines: allocate and drop 64 KiB direct buffers in a loop. HotSpot churns 2.5 GiB
through it; CratonVM died after **exactly 16384** buffers — 1024 MiB at
`-Xmx 1g`, i.e. *nothing* had ever been reclaimed, with or without explicit
`System.gc()`, with or without the JIT. It still reproduces on unmodified `dev`.

`java.nio.DirectByteBuffer`'s entire reclamation path is
`Cleaner.create(this, new Deallocator(base, size, cap))`, and
`jdk.internal.ref.Cleaner extends PhantomReference`. Its constructor is
`super(referent, dummyQueue)`, so it arrived at the reference processor as an
ordinary phantom and was enqueued — on a private queue nothing ever polls. In
the real JDK the difference is made by `ReferenceHandler`, which special-cases
`instanceof Cleaner` and calls `clean()` directly instead of enqueueing.

Fixed by mirroring exactly that:

* `native_phantom_ref_init` recognises the class and discovers it through a new
  wire encoding (4) meaning "phantom that RUNS instead of enqueueing";
* `ReferenceEntry::runs_cleaner` keeps it in `phantom_refs` — **not** in
  `ReferenceType::Cleaner` — because only phantom entries get their referent
  nulled before marking, and a `Cleaner` is reachable forever from its own
  class's static list, so without that nulling its referent can never die. (That
  is a second, independent leak, and it is why the obvious one-line version of
  this fix does not work: it was tried and measured first.)
* `run_cleaner_actions` dispatches `clean()` for that class rather than decoding
  the synthetic `Cleanable` shape, whose slot 0 is an action and not a referent.
* `Bits.reserveMemory` gained the JDK's reclaim-and-retry contract: force a
  collection and retry before throwing. Direct memory is invisible to the heap's
  own occupancy trigger, so nothing else has any reason to collect.

`DirectReclaimProbe` now churns 2.5 GiB clean.

## Hand-off: `nioMapped:` cannot GC its mapped buffer

`TestFileSystem` now fails much later, in `testPositionedReadWrite` on
`nioMapped:`, with

```
java.io.IOException: Timeout (10000 ms) reached while trying to GC mapped buffer
        at org.h2.store.fs.niomapped.FileNioMapped.unMap(FileNioMapped.java:68)
```

H2 nulls its own `mapped` field, weakly references the buffer and spins on
`System.gc()` until the reference clears (the JDK-4724038 workaround). It never
clears. This is a **different, pre-existing** bug that this class simply never
reached before, and it is not about the JIT ban or about `nioMemLZF:`. It does
NOT reproduce in isolation — `probes/WeakGcProbe.java` collects a plain object,
a direct buffer and a `MappedByteBuffer` on the first `System.gc()`, and
`apps/h2database-suite-runner/probes/NioMappedProbe.java` drives H2's own
`unMap()` path clean on both this build and an unmodified `dev` one. Tracked in the `bug-h2-niomapped-unmap-gc-timeout` write-up, which closed
2026-08-07: the retention was the conservative JIT root scan marking the whole
native stack on the residue of a compiled frame that had already returned, and
it reproduces in 12 seconds, not 784.

## Reproducing

```bash
# the four accessors alone
javac -d /tmp/probe probes/DbbElemProbe.java
<binary> --java-home <jdk25> -c /tmp/probe DbbElemProbe 300000
#   CRATONVM_DBG_DBB_ELEM=1 also reports served/bailed per accessor and
#   whether the addresses are real pointers or arena handles.

# nioMemLZF: end to end
H2CP=<h2>/target/classes
javac -cp $H2CP -d /tmp/probe apps/h2database-suite-runner/probes/LzfProbe.java
TMPDIR=/data/tmp <binary> --java-home <jdk25> -Dprobe.reader=false \
  -c $H2CP:/tmp/probe LzfProbe 'nioMemLZF:1:/probe' 100

# direct-buffer reclamation
javac -d /tmp/probe probes/DirectReclaimProbe.java
<binary> --java-home <jdk25> --Xmx 1g -c /tmp/probe DirectReclaimProbe 40000 65536 0
```

`TMPDIR=/data/tmp` is required on the Azure host: `/` is full and the runner's
internal `mktemp` silently produces empty results otherwise.

## Related

- `native-io/src/direct_buffer.rs` — the four accessors and the standing note on
  what would close the rest.
- `native-api/src/registry.rs` — `get_field_typed` / `get_fields_typed`.
- `gc/src/reference.rs` — `ReferenceEntry::runs_cleaner`.
- `vm/src/runtime/interpreter/gc_and_alloc.rs` — `run_cleaner_actions`'
  `jdk.internal.ref.Cleaner` arm.
- `regression-suite/src/RDirectBufferElem.java` — the accessor witness.
- `h2-jitban-longtail1-SUPERSEDED-20260728-ab-record.md` — the companion page,
  kept for the A/B numbers it recorded.
