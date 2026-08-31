# `JpaLargeBlobTest.jpaBlobStream` — 232 s against a 120 s timeout, and all of it is the per-native-call floor

## Status

**OPEN, fully decomposed, nothing left to diagnose.** This is the residual of
`fixed-suite-bugs/hibernate/jpalargeblob-random-state-side-table-FIXED-20260830.md`, which is retired: both of
that page's own findings are fixed, the test got 1.35x faster, and it still
fails. What is left is not a defect in `Random`, in blobs, or in H2 — it is this
VM's per-native-call cost, and it needs a JIT change nobody has scheduled.

Measured 2026-08-30 on CratonVM `dev@0739d6388`, JDK 25.0.3+9-LTS Temurin,
Windows, quiet host.

## The number

```
org.hibernate.orm.test.lob.JpaLargeBlobTest#jpaBlobStream
  ok=0 failed=1  test_ms=231841
```

232 s for a 100,000,000-byte stream = **2318 ns/byte**. `@Timeout(120)` is a
method-level annotation no runner property widens, so the budget is
**≤1200 ns/byte**. Real HotSpot runs the same test in ~7 s.

Down from **312 s** before the two fixes on the retired page (a leak, and a
CSPRNG syscall per `new Random()`).

## Where every nanosecond goes

`probes/BlobStreamCost.java`, same host, after both fixes. The fixture's
`read()` returns one byte per call and does: a virtual dispatch, a boxed-`Long`
compare, a boxed-`Long` decrement, a `new Random()`, and a `nextInt()`.

| component | ns/byte | isolated by |
|---|---:|---|
| the fixture's `read()` | **1468.5** | `stream boxed+new Random` |
| — BOXING | **812.2** | minus `stream prim +new Random` (656.3) |
| — `new Random()` + `nextInt` | **551.2** | minus `stream prim +shared Random` (105.1) |
| — loop + virtual dispatch | 105.1 | `stream prim +shared Random` |
| H2's blob write | ~850 | 2318 − 1468.5 |

HotSpot's whole `read()`: **34.0 ns/byte**.

**Both large components are the same thing.** This VM's floor for a native call
is roughly 300 ns, and the fixture makes five per byte:

* **boxing, 812 ns ≈ 3 calls** — `count > 0` is `Long.longValue`; `count--` is
  `Long.longValue` + `Long.valueOf`;
* **Random, 551 ns ≈ 2 calls** — `Random.<init>` and `Random.nextInt`.

## What would actually clear it

| change | resulting ns/byte | passes? |
|---|---:|---|
| nothing (today) | 2318 | no |
| intrinsify boxing only | ~1506 | no |
| intrinsify boxing **and** the `Random` calls | **~955** | **yes** |

So it needs both, and it is a JIT-intrinsics project rather than a fix to any
one library method.

`jit/src/lib.rs::try_resolve_intrinsic` already has the right shape — a
per-family match returning a `JitIntrinsic` that the x64 ladder emits inline,
as `Math.sqrt` and `Math.min/max` already do — and carries reserved empty
regions for exactly this kind of addition.

* `Long.longValue()J` is a field load behind a null check. Small.
* `Long.valueOf(J)` needs an allocation fast path plus the JDK's mandated
  −128..127 cache. Bigger, and the correctness surface is the cache identity
  (`Long.valueOf(127) == Long.valueOf(127)` must hold).
* `Random.nextInt` would need the generator state reachable from compiled code;
  note that **retiring the `Random` native shadow does NOT help** — it was built
  and measured at 2.6x–7.7x slower, because the JDK's own `Random` keys on an
  `AtomicLong` whose `get`/`compareAndSet` are themselves natives here. See the
  retired page.

**This is worth far more than this test.** A boxed-`Long` counter costs 140
ns/op here against HotSpot's 3.6 — that is every `Long`, `Integer` and
`Character` in every collection and every autoboxed loop in the VM.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm --java-home <jdk25> --Xmx 3000m @common.args \
  MethodRunner org.hibernate.orm.test.lob.JpaLargeBlobTest jpaBlobStream

# the decomposition, no database needed
javac -d . probes/BlobStreamCost.java
cratonvm --java-home <jdk25> -Diters=300000 -cp . BlobStreamCost
java -Diters=2000000 -cp . BlobStreamCost      # HotSpot, for the control column
```

## Related

* `fixed-suite-bugs/hibernate/jpalargeblob-random-state-side-table-FIXED-20260830.md`
  — the page this came out of: the native-memory leak, the entropy-draw spec
  divergence, the Random-shadow retirement that was built and left off, and the
  measurement mistakes made along the way.
