# `JpaLargeBlobTest.jpaBlobStream` — `java.util.Random` state leaks ~39 bytes of NATIVE memory per instance, and the blob loop costs 5–6 native calls per byte

## Status

**OPEN, root-caused, measured, not fixed.** Two independent mechanisms, both
CratonVM-specific, both reproducible in seconds by a standalone probe with no
database. Opened 2026-08-29 out of
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

## What would make the test pass

The budget is 120 s for 100M bytes: **≤1200 ns/byte**, against 1670 today. A
1.4x improvement clears it and a 2x is comfortable — so this does not need the
whole gap closed. Either mechanism, addressed, is likely enough on its own.

## Not yet done

- Size the side-table family (`SEED_TABLE`, `GAUSSIAN_TABLE`, `VH_META_TABLE`,
  and whatever else keys per-object native state by identity hash) before
  designing the eviction hook. A defect with several copies comes back.
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
