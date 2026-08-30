# `JpaLargeBlobTest.jpaBlobStream` — `java.util.Random` state leaks ~39 bytes of NATIVE memory per instance, and the blob loop costs 5–6 native calls per byte

## Status

**RETIRED 2026-08-30.** Both of this page's own findings are fixed. The test it
was opened for still fails, and that residual moved to
`known-issues/hibernate/jpalargeblobtest-per-native-call-floor-20260830.md`
— it is the VM's per-native-call floor, which is not a property of `Random`,
blobs or H2 and wants its own owner.


**Both of this page's own findings are CLOSED, 2026-08-30. The test still
fails, and what is left is not this page's.** Two independent mechanisms, both
CratonVM-specific, both reproducible in seconds by a standalone probe with no
database.

* **Mechanism 1, the native-memory leak — FIXED.** ~39 bytes per `Random` ever
  constructed, retained forever off-heap. 16M instances: 2163.9 MB → 1354.6 MB,
  flat across a 16x range.
* **The `new Random()` entropy draw — FIXED.** It was a CSPRNG syscall per
  construction, i.e. per byte of this fixture, and the spec says
  `seedUniquifier() ^ System.nanoTime()`. 982.2 → 546.1 ns/op.
* **Mechanism 2, the per-call cost — NOT A FINDING OF THIS PAGE.** It is the
  VM's ~300 ns native-call floor times five calls per byte. See
  [the arithmetic](#what-would-make-the-test-pass--the-arithmetic-closed-out).

Net on the real test: **312 s → 232 s (1.35x)**, still over `@Timeout(120)`.

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

## What would make the test pass — the arithmetic, closed out

The budget is 120 s for 100M bytes: **≤1200 ns/byte**.

**Where it stands after the two fixes on this page.** The real test, run
2026-08-30: `test_ms=231841` — **232 s, down from 312 s (1.35x)** — and it still
FAILS `@Timeout(120)`.

232 s / 100M bytes = **2318 ns/byte** end-to-end, against a 1200 budget. The
decomposition (`probes/BlobStreamCost.java`, this host, quiet, after both
fixes) splits it:

| component | ns/byte | how it is isolated |
|---|---:|---|
| the fixture's `read()` | **1468.5** | `stream boxed+new Random` |
| — of which BOXING | **812.2** | minus `stream prim +new Random` (656.3) |
| — of which `new Random()`+`nextInt` | **551.2** | minus `stream prim +shared Random` (105.1) |
| — of which loop + virtual dispatch | 105.1 | `stream prim +shared Random` |
| H2's blob write (the remainder) | ~850 | 2318 − 1468.5 |

Real HotSpot's whole `read()` is **34.0 ns/byte**.

**So neither fix on this page could ever have cleared it, and nor will one
more.** Both remaining components are the same thing — this VM's per-native-call
floor of roughly 300 ns, times the calls the fixture makes per byte:

* **boxing, 812 ns = ~3 calls.** `count > 0` is `Long.longValue`, `count--` is
  `Long.longValue` + `Long.valueOf`.
* **Random, 551 ns = ~2 calls.** `Random.<init>` and `Random.nextInt`.

Removing the boxing alone lands at 2318 − 812 = **1506 ns/byte** — still over.
Removing boxing AND the Random calls lands at **~955 ns/byte**, which is under
1200 and is the first arrangement that passes.

**That is a JIT-intrinsics project, not a finishing touch on this page.**
`jit/src/lib.rs::try_resolve_intrinsic` already has the shape (a per-family
match returning a `JitIntrinsic` the x64 ladder emits inline, as `Math.sqrt` and
`Math.min/max` do) and reserved empty regions for other families.
`Long.longValue` is a field load and would be a small addition; `Long.valueOf`
needs an allocation fast path with the JDK's −128..127 cache, which is bigger.
Both are VM-wide wins far beyond this test — the boxed-`Long` counter alone is
140 ns/op here against HotSpot's 3.6.

**This page has nothing left of its own to say about that.** Its two findings
are closed; what remains is the per-call floor, which is a property of the VM
and wants its own page and its own owner.

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
- ~~Decide whether `java.util.Random` needs a native at all in real-JDK mode.~~
  **ANSWERED 2026-08-30: it does. Retiring the shadow is 2.6x-7.7x SLOWER, and
  the reason is worth keeping** — see
  [Retiring the Random shadow](#retiring-the-random-shadow-built-measured-and-left-off)
  below. The item's premise ("pure Java, keeps its state in the object, and is
  JIT-compilable") is true and still leads to the wrong answer. Also: the A/B was
  never blocked. `CRATONVM_ENFORCE_NATIVE_SHADOW` is scoped to `--jdk-only`, but
  a probe that reimplements the same LCG in Java measures the same question on
  today's binary with no VM change at all, and gating the registration is ~20
  lines.
- ~~Replace `set_entropy_seed`'s per-construction OS entropy draw with the JDK's
  `seedUniquifier() ^ System.nanoTime()`.~~ **DONE 2026-08-30**, and it was
  worth more than this line credited: 982.2 → 546.1 ns/op, which is 1.41x on the
  whole fixture `read()`. The page called it "not the bottleneck (falsified
  above)" because seeded and unseeded constructors cost the same — they did,
  at 570 vs 620 ns, a 50 ns gap that read as noise. Re-measured quiet the gap
  was 150 ns (832 vs 982), and it is now 9 ns (537 vs 546). **A falsification
  measured once, on a contended host, at the resolution of the thing being
  falsified, is not a falsification.**
- `new StringBuilder()` measured **1076.8 ns/op** on CratonVM against HotSpot's
  0.1 in the same harness. Not on this test's path and not investigated — noted
  because it is a far broader surface than `Random` and the number is large
  enough to be worth its own look.

## Retiring the Random shadow: built, measured, and left OFF

`CRATONVM_JDK_RANDOM=1` (opt-in, default OFF) skips the `java/util/Random`
native registrations on a real JDK so the JDK's own bytecode serves the class.
It is correct and it is slower, so it ships off.

### Why the obvious argument is wrong

The real `java.util.Random`'s state is a `private final AtomicLong seed`, and
every draw runs

```java
do { oldseed = seed.get(); nextseed = ...; } while (!seed.compareAndSet(oldseed, nextseed));
```

**`AtomicLong.get` and `AtomicLong.compareAndSet` are themselves natives in this
VM** — confirmed from `--dump-native-registry`, 23 invocations each across a
23-draw run. So the JDK path costs **two native calls per draw where the shadow
costs one**, plus the Java frames around them.

MEASURED, one binary, `probes/RandomShadowCost.java`:

| arm | shadow (default) | JDK bytecode |
|---|---:|---:|
| `new Random(i).nextInt()` | **663.3 ns/op** | 1655.6–1689.9 ns/op |
| `shared Random.nextInt()` | **109.0 ns/op** | 826.4–827.3 ns/op |

**The estimate that motivated this was wrong, and the error is reusable.** The
probe first priced the shadow against a hand-written `MyRandom` — the same LCG
over a plain `long` field — at 122 ns/op, and predicted retiring the shadow
would buy 6.8x. `MyRandom` is not `java.util.Random`: it has no `AtomicLong`, so
it measured a *third* implementation that does not exist in either arm. A proxy
for "the JDK's version" has to contain the part that makes the JDK's version
expensive.

This becomes the right default the moment `AtomicLong` stops being native, or
`Random` gets a JIT intrinsic. Re-run the probe then; do not trust this table.

### A latent defect it did uncover

There are **three** implementations of `java.util.Random` in this tree, and the
first attempt at the retirement made the wrong one live: `RandomSpec` printed
eight rows of **zeros**.

1. `native-collections/src/lib.rs::register_random_natives` — a **synthetic**
   2-field shape that keeps the seed in FIELD 0 of the receiver, registered as
   `Bridge`;
2. `native-builtins/src/securerandom.rs` — the spec-exact LCG over the identity
   side table, registered as `Intrinsic` **afterwards**;
3. the real JDK bytecode, never reached in compatible mode.

On a real JDK, field 0 of `java.util.Random` is the `AtomicLong` *reference*,
not a long — so (1) reads and writes the wrong thing and every draw is 0. It was
harmless only because (2) overwrites the same ten triples and registration is
LAST-WRITE-WINS. Gating only (2) uncovered (1).

`--dump-native-registry` named it in one run (`kind: "bridge"`,
`registered_by: native-collections/src/lib.rs`, `invocations: 7`) while
`probes/AtomicLongSpec.java` proved the machinery underneath was byte-identical
to HotSpot — including the exact `Random.next(int)` CAS loop returning
`-1170105035`. Both sites now take the same gate, so the two can no longer
disagree about which JDK they are serving.

### Correctness of the opt-in arm

All three flag states — unset, `=1`, `=0` — are byte-identical to real HotSpot
on `probes/RandomSpec.java` (8 rows), `probes/SecureRandomSpec.java` (11 rows,
including SHA1PRNG's exact seeded bytes and the non-replay properties), and
`probes/RandomLiveAcrossGc.java`. `SecureRandom extends Random`, so its
superclass construction changes under the flag; that is what
`SecureRandomSpec.java` exists to check.

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
