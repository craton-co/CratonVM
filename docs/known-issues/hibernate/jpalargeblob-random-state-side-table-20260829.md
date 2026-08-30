# `JpaLargeBlobTest.jpaBlobStream` — `java.util.Random` state leaks ~39 bytes of NATIVE memory per instance, and the blob loop costs 5–6 native calls per byte

## Status

**Mechanism 1 (the native-memory leak) is FIXED, 2026-08-30. Mechanism 2 (the
per-call cost) is OPEN.** Two independent mechanisms, both CratonVM-specific,
both reproducible in seconds by a standalone probe with no database.

The leak fix is `cratonvm_types::identity_side_tables`, an eviction channel the
ZGC sweep drives; see [The fix](#the-fix-eviction-driven-from-the-sweep) below
for what it does and the two things this page got wrong about where it had to
go. It is behind `CRATONVM_IDENTITY_HASH_EVICT` (ON by default, `=0` reverts),
so both arms are one binary. Opened 2026-08-29 out of
`h2-complete-suite-misc-residuals-20260829.md`, which had it as "could be a
genuine throughput gap … or could be H2-specific BLOB handling. Not measured or
distinguished." It is neither H2-specific nor a single gap.

Measured on CratonVM `dev@4080c8706`, JDK 25.0.3+9-LTS Temurin, Windows.

## The failure, and what it actually is

```
org.hibernate.orm.test.lob.JpaLargeBlobTest.jpaBlobStream
java.util.concurrent.TimeoutException: jpaBlobStream(...) timed out after 120 seconds
```

The test is not hung. `@Timeout(120)` is a METHOD-level JUnit annotation, which
no runner property widens, and JUnit observes it after the fact for
non-interruptible work — so the run is reported as a timeout while the work runs
to completion:

| VM | wall |
|---|---|
| real HotSpot | `test_ms=7078` |
| CratonVM | `test_ms=312445` — **44x**, and it does finish |

## What the fixture actually asks for

`LobEntity.BLOB_LENGTH` is **100,000,000**, and the stream the test hands
Hibernate produces one byte per call:

```java
private Long count = (long) 200 * 1024 * 1024;
@Override public int read() {
    read = true;
    if (count > 0) { count--; return new Random().nextInt(); }
    return -1;
}
```

So the workload is 100 million iterations of: a virtual `read()`, a boxed-`Long`
compare, a boxed-`Long` decrement, a `new Random()`, and a `nextInt()`. Per-byte
cost is the whole story, and it is measurable without a database.

## The decomposition

`iters` differ per VM so every arm finishes; ns/op is what compares. One
execution per shape, best of three, after warm-up.

| arm | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| empty loop | 0.3 | 0.8 | 3x |
| `System.nanoTime()` | 24.3 | 65.4 | 2.7x |
| shared `Random.nextInt()` | 9.2 | 85.6 | 9x |
| `new Random(seed).nextInt()` | 10.1 | **570.2** | 56x |
| `new Random().nextInt()` | 35.2 | **619.9** | 18x |
| boxed `Long` counter (`count--`) | 3.7 | **143.9** | 39x |
| primitive `long` counter | 0.3 | 1.5 | 5x |
| the fixture's `read()`, per byte | 43.6 | **1670.6** | **38x** |
| `new Object()` | 0.0 | 70.3 | — |

The last row of the model checks out against the real run: 1670 ns × 100M =
167 s of stream cost, and the measured end-to-end is 312 s with H2's blob write
on top. HotSpot: 43.6 ns × 100M = 4.4 s against a measured 7 s.

**A hypothesis this falsified.** `new Random()`'s no-arg constructor in this VM
draws OS entropy — `BCryptGenRandom` on Windows, and an `open`+`read` of
`/dev/urandom` on Linux — per construction
(`securerandom.rs::set_entropy_seed`). That looked like the answer and is not:
`new Random(seed)` skips the entropy draw entirely and costs **the same**
(570 vs 620 ns). The entropy draw is still a spec divergence worth its own fix —
`java.util.Random`'s no-arg constructor is specified as
`seedUniquifier() ^ System.nanoTime()`, and drawing from a CSPRNG for a
generator explicitly documented as not cryptographically secure buys nothing —
but it is not what makes this test slow.

## Mechanism 1 — a native-memory leak, ~39 bytes per `Random` ever constructed

`java.util.Random`'s generator state does not live in the object. It lives in
two process-wide tables in `native-builtins/src/securerandom.rs`:

```rust
static SEED_TABLE: RwLock<Option<FxHashMap<i32, u64>>> = RwLock::new(None);
static GAUSSIAN_TABLE: RwLock<Option<FxHashMap<i32, f64>>> = RwLock::new(None);
```

keyed by `identity_hash_code`. **Neither has a removal path tied to the object's
death.** `set_seed` inserts, `lcg_next` mutates, and nothing ever evicts — the
GC cannot, because it holds address→hash and these are keyed by hash alone.

The contrast is one file away and makes the omission legible:
`gc/src/compact_header.rs`'s `HashCodeTable::update_after_gc` DOES drop a dead
object's entry, with a comment saying why — *"Keeping it would hand the next
object allocated at this address a stranger's identity hash."* The identity-hash
table is pruned; the two tables keyed by its output are not.

MEASURED (`RandomLeak.java`: allocate N `Random`s, drop every one, full GC each
round, and read both the Java heap and the process's private bytes):

| `Random`s constructed | Java heap used | peak process private |
|---:|---:|---:|
| 500,000 | 1,680 KB | 1,322 MB |
| 2,000,000 | 1,680 KB | 1,428 MB |
| 4,000,000 | 1,680 KB | 1,545 MB |
| 8,000,000 | 1,680 KB | 1,613 MB |

The Java heap is **flat to the kilobyte** across 8 million dead `Random`s — they
are collected, correctly — while process private bytes climb 291 MB over the
last 7.5 million, i.e. **~39 bytes retained per instance, forever, off-heap**.

For this test that is 100,000,000 × 39 B ≈ **3.9 GB of native memory**. The real
run reached **5.28 GB private** under `--Xmx 3000m`: the Java heap is capped and
honoured, and the growth is entirely outside it — invisible to `-Xmx`, to
`Runtime.freeMemory`, and to every Java-side heap metric an operator would
check.

This is the part that is a defect rather than a characteristic. Any long-lived
service that calls `new Random()` per request grows without bound and cannot be
made to stop by tuning the heap.

**The same shape lives elsewhere.** `securerandom.rs`'s own comment nominates
`lang_invoke.rs`'s `VH_META_TABLE` as "the canonical example of this pattern",
and that table is keyed the same way with no eviction either. A fix wants to be
a mechanism — eviction driven from the same place `HashCodeTable` prunes, which
is the one site that knows which hashes just died — not a third private copy.
Sizing the family is the first task, not the last.

## Mechanism 2 — 5–6 native calls per byte

`--dump-native-registry` on a run of the probe:

```
java/util/Random.<init>()V     inv=10622    by=native-builtins/src/securerandom.rs:1731
java/util/Random.nextInt()I    inv=248000   by=native-builtins/src/securerandom.rs:1734
java/lang/Long.valueOf(J)      registered   by=native-builtins/src/lang_math.rs:392
java/lang/Long.longValue()J    registered   by=native-builtins/src/lang_math.rs:398
```

So one iteration of the fixture's `read()` is: `Random.<init>`, `Random.nextInt`,
`Long.longValue` twice (the `> 0` compare and the decrement), and `Long.valueOf`
once — five native calls, each carrying this VM's per-call floor plus an
identity-hash lookup and a global `RwLock` write for the two `Random` ones. The
arms above price the boxing half on its own: a boxed `Long` decrement is 143.9 ns
against a primitive `long`'s 1.5.

HotSpot inlines all five to approximately nothing, which is why its per-byte
figure (43.6 ns) is barely above its `new Random().nextInt()` figure (35.2).

## The obvious fix was built, measured, and REVERTED — read this before rebuilding it

`java.util.Random.seed` is a `private final AtomicLong` in every real JDK, and
this VM's `AtomicLong` keeps its value in **field 0 of the object** with no side
table of its own (`util_concurrent_ext::native_atomic_long_get` / `_set`). So
the state has an obvious per-object home that dies with the instance, and moving
it there is a change to one file. It was written: `seed_slot` /`cell_get` /
`cell_put` / `cell_install` in `securerandom.rs`, resolving the slot through
`resolve_field_index_by_class_id` off the receiver's own `ClassId`, falling back
to the table for a receiver whose class declares no `seed` slot (the fabricated
synthetic-JDK shape).

It works, and it is **not an improvement**. Measured on the built binary, same
harness, same host:

| arm | table (today) | per-object, resolve every call | per-object + slot memo |
|---|---:|---:|---:|
| shared `Random.nextInt()` | **85.6** | 312.9 | 213.6 |
| `new Random(seed).nextInt()` | **570.2** | 836.9 | 862.7 |
| the fixture's `read()`, per byte | **1670.6** | 1690.2 | 1728.6 |

All eight rows of `probes/RandomSpec.java` stay byte-identical to real HotSpot
throughout, so it is correct — it is just slower, and slower on the very test
this page is about.

**Two things it ran into, both worth knowing before anyone rebuilds it:**

1. **`resolve_field_index_by_class_id` costs ~115 ns** — it takes the
   class-manager READ LOCK and walks the hierarchy comparing field names. That
   is more than the whole `nextInt` it was added to. A one-slot thread-local
   memo keyed by `(vm_identity, ClassId)` — the shape
   `typecheck::reference_array_component_is_object` already uses, VM identity in
   the key because `ClassId`s are per-VM dense indices — recovers about a third
   of it and no more. What is left is four `NativeContext` field accesses per
   `nextInt` (`obj[seed]`, `cell[0]`, then the write) against the table path's
   one identity hash plus one lock.
2. **It would weaken `java.util.Random`'s documented thread-safety.** The table
   path does its read-modify-write under the table's write lock, so it is atomic
   per operation. A plain read-then-write through two field accesses is not, so
   two threads sharing a `Random` could draw the same value. The JDK's own
   implementation is safe because it CASes the `AtomicLong` in a retry loop —
   which this would also have to do, making it slower again.

So the per-object move is not the repair. **The repair is eviction** — and the
rest of this paragraph, as written on 2026-08-29, named the wrong site for it:

> the one place that knows which hashes have just died is
> `HashCodeTable::update_after_gc`, which already drops exactly those entries
> for its own table. A hook there … fixes `SEED_TABLE`, `GAUSSIAN_TABLE` and
> `VH_META_TABLE` at once.

Both halves of that are false, and finding out why is most of the work:

* **`HashCodeTable` has no production consumer.** Its own doc comment says so
  ("it is exported from `lib.rs` and referenced only by tests"). The identity
  hash this VM hands out lives in the object's **mark word**, installed lazily
  by one CAS in `ObjectHeader::mark_word_identity_hash`. There is no table of
  hashes to prune, so there is no `update_after_gc` to hook — the collector has
  to read the dying object's header before it destroys it.
* **`VH_META_TABLE` is not fixed by eviction and was not wired.** `vh_meta_put`
  registers every VarHandle as a PERMANENT GC root (B-J), so no VarHandle is
  ever reclaimed and an evictor there could never fire. Its entries do
  accumulate, but the root is what retains them and the root is load-bearing —
  that is a question about VarHandle lifetime, not one an eviction hook can
  answer. Registering it anyway would have bought a no-op that turned the
  sweep's collection cost on for nothing.

The same false citation — "`identity_hash_code` is preserved across compaction
by `HashCodeTable::update_after_gc`" — sat in the doc comments of both
`securerandom.rs::obj_key` and `lang_invoke.rs`'s table, which is how a missing
eviction path came to look like somebody else's already-solved problem. Both are
corrected.

## The fix: eviction driven from the sweep

`cratonvm_types::identity_side_tables` is a registry of eviction callbacks. It
lives in `types` because `gc` and `native-builtins` depend on that crate and on
neither each other — the same reason `loader_pin` lives there. `securerandom.rs`
registers one evictor covering `SEED_TABLE`, `GAUSSIAN_TABLE` and
`SHA1PRNG_TABLE`; the ZGC sweep reports the identity hashes of the objects it
just reclaimed, once per cycle.

**Where the hashes are read is the whole design.** They are taken in the sweep's
dead arm, immediately before `write_bytes` zeroes the header — not from the
`dead` vector afterwards. Two reasons, and the second is a correctness one:

1. The sweep zeroes the header, so afterwards the mark word is gone.
2. **The sweep runs BEFORE compaction** in a cycle (`zgc.rs`: sweep at the
   "Sweep phase" marker, "STW compaction complete" ~600 lines later,
   `prune_dead` after that). A dead base is very often a *survivor's* new base
   once the slide has run — this is the same collision `prune_dead`'s ordering
   comment is about — so reading a header at a `dead[i]` address after
   compaction could hand a **live** object's hash to the evictors and silently
   re-seed it. Reading in the sweep makes that unreachable by construction
   rather than by screening.

`ObjectHeader::neutral_hash` returning `0` for a never-hashed object is a free
and exact filter: an object whose identity hash was never requested cannot be a
key in any of these tables. A hash *displaced* by monitor inflation also reads
`0` and is not recovered — that shape (a `synchronized` block on the very object
whose native state is side-tabled) still leaks, which is a strict improvement on
leaking all of them.

### Measured

One binary, two arms, `CRATONVM_IDENTITY_HASH_EVICT=0|1`, `probes/RandomLeak.java`
at `--Xmx 1000m`, peak process private bytes sampled from outside:

| `Random`s constructed | evict=0 (before) | evict=1 (after) |
|---:|---:|---:|
| 1,000,000 | 1,374.6 MB | **1,354.5 MB** |
| 4,000,000 | 1,549.0 MB | **1,354.5 MB** |
| 8,000,000 | 1,752.8 MB | **1,354.6 MB** |
| 16,000,000 | 2,163.6 MB | **1,354.6 MB** |

The fixed arm is **flat to 0.1 MB across a 16x range**; the leaking arm grows
~52 bytes per instance. The 16M row reproduces to a tenth of a megabyte across
three runs (2,163.6 / 2,163.8 / 2,163.7 against 1,354.6 / 1,356.6 / 1,354.5).

**It is not meaningfully faster, and an earlier draft of this page said it was.**
The first sweep measured 34.6 s → 24.0 s at 16M and read it as the leak's second
cost — the grown tables being the ones every `nextInt` then probes. That was a
measurement of LOAD: it ran while a `cargo test` of `native-builtins` had this
box at 16 concurrent `rustc` processes. Re-run on a quiet host the same arms are
19.1 s → 18.5 s and 20.4 s → 19.4 s, i.e. 3–5%, and `probes/BlobStreamCost.java`
at `iters=300000` puts the fixture's own `read()` shape at 2,075.6 → 2,055.1
ns/op, which is nothing. The memory result is unaffected — it is the same to a
tenth of a megabyte under both load regimes, which is exactly the difference
between a structural fact and a timing.

Engagement, from `CRATONVM_GC_STATS=1` on `probes/RandomLiveAcrossGc.java`:
`zgc-sweep-cost: dead_objects=1200428`, so the reporting path really ran.
`compaction_cycles=0` on that probe — the compactor does not engage on so small
a live set, which is why the ordering argument above is stated structurally and
not as a passed test.

### Correctness

`probes/RandomSpec.java` — all eight spec rows byte-identical to real HotSpot in
**both** arms.

`probes/RandomLiveAcrossGc.java` is new, and is the regression test for the
hazard eviction introduces: 200 seeded `Random`s held live while 1.2M hashed
`Random`s die around them and `System.gc()` runs every round, with a second
witness for `haveNextNextGaussian`'s separate table. Every value is a pure
function of the fixed seeds, so an evicted live entry — which re-seeds from OS
entropy and still returns a number — shows up as a diverged checksum rather than
as an error. Byte-identical to real HotSpot, 3/3 runs per arm.

## What would make the test pass

The budget is 120 s for 100M bytes: **≤1200 ns/byte**, against 1670 today. A
1.4x improvement clears it and a 2x is comfortable — so this does not need the
whole gap closed.

**The leak fix does not clear it.** Freeing the memory does not make the path
cheaper: the per-byte cost is mechanism 2, five native calls per iteration, and
that is untouched. `JpaLargeBlobTest.jpaBlobStream` is still expected to exceed
its `@Timeout(120)`. What the fix removes is the ~3.9 GB of unreclaimable native
memory the test dragged along with it, which was the part that was a defect
rather than a slowness.

## Not yet done

- ~~Size the side-table family before designing the eviction hook at
  `HashCodeTable::update_after_gc`.~~ **Done, and the named hook site was wrong
  — see [The fix](#the-fix-eviction-driven-from-the-sweep).** What remains of
  the census: a `grep` for `static … <i32, …>` across the tree finds ~36 maps
  keyed by an `i32`, but most are keyed by an fd, a port or a handle rather than
  by an identity hash. `securerandom.rs`'s three are wired; `VH_META_TABLE` is
  deliberately not (its objects are permanent GC roots and can never be
  reported). The rest have not been classified one by one, and each one that
  *is* identity-hash-keyed is a one-line registration now that the channel
  exists.
- **Only ZGC reports its dead.** `MonitorCleanup::prune_dead` has exactly one
  caller (`zgc.rs`), so `gen_heap` and `g1` have no dead-object notification at
  all — they do not prune monitors either, which is a pre-existing gap of the
  same shape rather than one this change introduced. ZGC is the default
  collector, so the production path is covered; running under `g1` or the
  generational heap still leaks. Giving those two a dead-object channel is the
  follow-up, and it fixes their monitor pruning at the same time.
- Decide whether `java.util.Random` needs a native at all in real-JDK mode. The
  JDK's own implementation is pure Java, keeps its state in the object, and is
  JIT-compilable. **MEASURED: the full spec surface is byte-identical** between
  the native shadow and real HotSpot — seeded `nextInt`/`nextLong`/`nextDouble`/
  `nextInt(bound)`/`nextBoolean`/`nextGaussian`/`nextBytes` and unseeded
  distinctness, all eight rows. What blocks the A/B is that
  `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Random` **does not engage in
  compatible mode** — the dial is scoped to `--jdk-only`'s step-1 dispatch — so
  retiring the shadow has to be measured by changing the registration, not by a
  flag. Synthetic-JDK mode still needs these natives; the registrations' own
  comment records them being dropped once and seeded `Random` returning
  all-zero output.
- Replace `set_entropy_seed`'s per-construction OS entropy draw with the JDK's
  `seedUniquifier() ^ System.nanoTime()`. Not the bottleneck (falsified above),
  but it is a syscall per `new Random()` and it is not what the spec says.
- `new StringBuilder()` measured **1076.8 ns/op** on CratonVM against HotSpot's
  0.1 in the same harness. Not on this test's path and not investigated — noted
  because it is a far broader surface than `Random` and the number is large
  enough to be worth its own look.

## Repro — no database needed

```bash
# the decomposition table
javac BlobStreamCost2.java
java -Diters=2000000 -cp . BlobStreamCost2                       # HotSpot
cratonvm --java-home <jdk25> -Diters=300000 -cp . BlobStreamCost2

# the leak: Java heap flat, process private bytes climbing
javac RandomLeak.java
cratonvm --java-home <jdk25> --Xmx 1000m -Dchunk=500000 -Drounds=16 -cp . RandomLeak
# watch the process's private bytes from outside while it runs
```

The full test, for the end-to-end number, needs the H2 suite fixture:

```bash
cd apps/hib-suite-runner
cratonvm --java-home <jdk25> --Xmx 3000m @common.args \
  MethodRunner org.hibernate.orm.test.lob.JpaLargeBlobTest jpaBlobStream
```
