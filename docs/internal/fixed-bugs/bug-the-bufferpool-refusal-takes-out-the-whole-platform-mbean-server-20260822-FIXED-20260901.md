# The `BufferPool` refusal is correctly loud and wrongly SCOPED — strict mode loses the whole platform MBean server

**Status: CLOSED 2026-09-01.** The `<clinit>` blast radius was fixed
2026-08-24, and NOT by the fix this record asked for. What this record's
closing sentence claimed about the OTHER half — "the direct query now reads
real `Bits.BUFFER_POOL` state and nothing answers zero" — was still false on
2026-09-01, in both directions: `getPlatformMXBeans(BufferPoolMXBean.class)`
answered an EMPTY LIST under `--jdk-only`, and the platform MBean server's
`java.nio:type=BufferPool,name=direct` answered a CONSTANT ZERO in **both**
modes. Measured and fixed in the last section of this record.
Opened 2026-08-22.

```text
CRATONVM_ARGS=--jdk-only  regression-suite  111 passed, 0 failed
  RJdkJmx        PASS
```

Measured on `e910c5bb0`, release build 2026-08-24 15:02, against HotSpot
25.0.3+9. This record asked for the refusal to be SCOPED per call path. What
actually closed it was **retiring `JavaNioAccess.getDirectBufferPool`**
(`ba798eca7`): `javap -c` shows the real method is two instructions returning
`Bits.BUFFER_POOL`, so `VM$BufferPoolsHolder.<clinit>` now runs the JDK's own
bytecode and never reaches a CratonVM fabrication at all.

The refusal is now unreachable from every caller, which is worth stating
precisely because it is stronger than "the vector is green":
`alloc_buffer_pool` is called from exactly one place, `alloc_all_buffer_pools`,
and that function pre-checks `try_ensure_synthetic_class` and returns an empty
`Vec` on refusal (`697754cb6`) before any per-pool call can throw. The
single-pool caller this record's trace went through no longer exists.

**So the trade this record framed was never taken, and does not need to be.**
The argument in §"What would fix it" -- keep the throwable on the direct query,
return empty on the SharedSecrets path -- was a choice between a loud refusal
and a constant-zero reading. Retiring the shim removed the fabrication instead,
so the direct query now reads real `Bits.BUFFER_POOL` state and nothing
answers zero. Left below unedited: the blast-radius measurement is what made
the scope of the problem legible, and it is still the reason a refusal thrown
from inside a `<clinit>` is not scoped to the feature it refuses.


## What is failing

`RJdkJmx`, **`--jdk-only` only**, deterministic:

```text
r16 (2026-08-22 14:29)  strict  PASS 2/2   107/107 in the arm
r17 (2026-08-22 17:27)  strict  FAIL 3/3   106/107
pristine origin/dev 938688f00  strict  FAIL 3/3   <- built and run, not inferred
```

```text
NoClassDefFoundError: cratonvm/internal/BufferPool
  at jdk/internal/misc/VM$BufferPoolsHolder.<clinit>(VM.java:468)
  at jdk/internal/misc/VM.getBufferPools(VM.java:480)
  at sun/management/ManagementFactoryHelper.getBufferPoolMXBeans(…:341)
  at java/lang/management/DefaultPlatformMBeanProvider$10.nameToMBeanMap(…:413)
  at java/lang/management/ManagementFactory.getPlatformMBeanServer(…:473)
  at RJdkJmx.registerAndInvoke(RJdkJmx.java:133)
```

Mode-split, same binary: **strict `rc=1`, compatible `rc=0` `PASS (67 checks)`.**

## This is NOT the retirement round, and it is not a fabrication bug

`9d3f78943` *fix(jmx,reflect): a "direct" BufferPoolMXBean that exists, and
counts* introduced `cratonvm/internal/BufferPool`, and **its author anticipated
strict mode refusing it**:

```rust
None => match ctx.try_ensure_synthetic_class(CRATON_BUFFER_POOL_CLASS, …) {
    Ok(id) => id,
    // Strict mode refused the fabrication. There is no real class to
    // stand in … so the refusal stands as a catchable throwable rather
    // than a silently empty list.
    Err(err) => return Err(refusal_to_java_failure(ctx, err)),
}
```

The fix itself is good and the argument for a loud refusal is right: the bean it
replaced was stamped with the `BufferPoolMXBean` INTERFACE and its accessors
were stateless lambdas returning `0`, and a pool bean that always reports zero
is worse than no pool bean — it reports a perfect cache no matter what the VM is
doing.

**The defect is the SCOPE of the refusal, not the decision to refuse.**

## The blast radius, measured

`probes/JmxBlast.java`, three independent JMX entry points, HotSpot 25.0.3 as
the oracle:

| call | HotSpot | CratonVM `--jdk-only` |
| --- | --- | --- |
| `ManagementFactory.getPlatformMBeanServer()` | OK | **`NoClassDefFoundError`** |
| `getRuntimeMXBean().getName()` | OK | OK |
| `getMemoryMXBean().getHeapMemoryUsage()` | OK | OK |

So it is not "buffer pools are unavailable in strict mode". It is **the platform
MBean server is unavailable in strict mode**, and `getPlatformMBeanServer()` is
the standard entry point for ALL JMX registration. `RJdkJmx` never asks about
buffer pools at all — it registers an MBean, and that call eagerly initialises
`VM$BufferPoolsHolder`.

**A refusal thrown from inside a `<clinit>` is not scoped to the feature it
refuses.** `VM$BufferPoolsHolder` is a holder-idiom static initialiser: it runs
once, on first touch, on a path that unrelated JMX work traverses, and a
throwable escaping it poisons the holder for the life of the process. The two
MXBeans that still work are the ones that do not go through the MBean server.

## What would fix it, and the trade the owning lane already priced

The choice is not "loud refusal vs silent empty list" everywhere — it is
per-call-path:

* the **direct** query (`getPlatformMXBeans(BufferPoolMXBean.class)`) is where a
  silent empty list is genuinely harmful, because a caller reads a delta of two
  `getCount()` readings and a constant zero lies. Keep the throwable there.
* the **SharedSecrets** path that `VM$BufferPoolsHolder.<clinit>` traverses is
  not asking for the feature; it is populating a holder on the way to something
  else. Refusing there converts "one MXBean is unavailable" into "JMX is
  unavailable", which no caller can catch usefully because it fires inside a
  class initialiser they did not write.

Returning an empty list on the second path costs exactly the harm the commit
message argues against — a constant-zero reading — for callers that never
reached the first path. That is a real cost and it is the lane's call, not this
record's. What this record supplies is the number that was missing: the current
scope is **every JMX consumer**, measured, not one MXBean.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp regression-suite/build RJdkJmx
```

Expect `rc=1` **as of 2026-08-22**; it has passed in both modes since
2026-08-24. `probes/JmxBlast.java` isolates the entry points above and, since
2026-09-01, also reads a `getCount()` delta across a 1 MiB `allocateDirect` —
which is the only check that can tell a working pool bean from one that answers
zero, or from no bean at all.

## 2026-09-01 — the refusal stopped THROWING and started answering NOTHING, and the other route was answering ZERO

The header above says the `<clinit>` blast radius was closed on 2026-08-24 by
retiring `JavaNioAccess.getDirectBufferPool`, and that "the direct query now
reads real `Bits.BUFFER_POOL` state and nothing answers zero". The first clause
holds and is what made everything below possible. The second was **false in
both directions**, and the measurement that found it is the one this record
never ran: ask the pool the two different ways an application can ask.

There are two routes and in this VM they were not the same code.

* **A.** `ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)` — the
  one this record's blast-radius table used. Intercepted by a CratonVM native
  (`jmx.rs`) and answered from `alloc_all_buffer_pools`.
* **B.** the platform MBean server, `java.nio:type=BufferPool,name=direct`.
  `DefaultPlatformMBeanProvider` reaches it through
  `ManagementFactoryHelper.getBufferPoolMXBeans()`, which wraps the JDK's own
  `VM$BufferPool` objects. **This is the route JConsole, `jcmd`, and every JMX
  exporter use**, and no CratonVM native is on it.

MEASURED 2026-09-01 on `2d866cb28` (`origin/dev`), `probes/PoolRoutes.java`,
one 1 MiB `ByteBuffer.allocateDirect` between the two readings:

| route | HotSpot 25.0.3+9 | CratonVM | CratonVM `--jdk-only` |
| --- | --- | --- | --- |
| A — `getPlatformMXBeans` list | 3 pools | 3 pools | **0 pools** |
| A — `direct.getCount()` | 0 → 1 | 0 → 1 | **no bean to ask** |
| B — MBean registered | yes | yes | yes |
| B — `Count` attribute | 0 → 1 | **0 → 0** | **0 → 0** |

`probes/JmxBlast.java` scores the same run `7/7 · 7/7 · 4/7`.

**Two defects, and the record's own sentence names both.**

### 1. Route A answered an EMPTY LIST under `--jdk-only`

So the trade this record framed **was** taken after all — not as the loud
refusal the commit chose, and not as the per-call-path scoping this record
asked for, but silently, as the empty list. `alloc_all_buffer_pools`
pre-checks `try_ensure_synthetic_class` and returns an empty `Vec` on refusal
(`697754cb6`); that is what stopped the throwable escaping the `<clinit>`, and
it is also what left strict mode with no buffer-pool bean at all.

**The refusal itself is right and is not changed.** `--jdk-only` forbids
compatibility stand-ins; `cratonvm/internal/BufferPool` is one; refusing it is
the policy working. What was wrong is that the refusal was the END of the
answer. Strict mode is the mode that HAS a real class library, so it is the
mode with a real answer to hand:
`sun.management.ManagementFactoryHelper.getBufferPoolMXBeans()`.

`getPlatformMXBeans(BufferPoolMXBean.class)` now delegates there when — and
only when — the carrier was refused. Compatible mode never reaches the branch,
and a synthetic image, which has no `ManagementFactoryHelper` either, falls
through to the empty list it had before.

The delegated list is **reordered**. `VM.getBufferPools()` publishes
`[direct, mapped, sync]`; HotSpot's `getPlatformMXBeans`, which goes through
`DefaultPlatformMBeanProvider`'s name-keyed map instead, answers
`[mapped, direct, mapped - 'non-volatile memory']`. This record's own
`BUFFER_POOL_NAMES` says why that matters — *"the order is part of the
observable answer — `getPlatformMXBeans` returns a `List` and callers index
it"* — so the same table drives the reordering.

**And it is only reachable because of `ba798eca7`.** With
`JavaNioAccess.getDirectBufferPool` retired, `VM$BufferPoolsHolder.<clinit>`
runs the JDK's own bytecode instead of a CratonVM fabrication — which is what
stops it throwing AND what makes the JDK's own pool list a thing that can be
asked for. Delegating to it before that commit would have re-entered the
fabrication that took out the MBean server.

### 2. Route B answered a CONSTANT ZERO — in BOTH modes, and it always had

This is the one that matters more, and this record's own argument is the reason:

> a pool bean that always reports zero is worse than no pool bean — it reports
> a perfect cache no matter what the VM is doing

The three `java.nio:type=BufferPool` MBeans are registered in every arm above.
Their `Count` / `MemoryUsed` / `TotalCapacity` come from `java.nio.Bits$1`,
whose three `getstatic`s read `Bits.COUNT` / `RESERVED_MEMORY` /
`TOTAL_CAPACITY` — `AtomicLong`s that only `Bits.reserveMemory` maintains. This
VM never calls `Bits.reserveMemory`: `ByteBuffer.allocateDirect` is
force-overridden in EVERY mode (`native_override.rs`) by an allocator that
keeps its own counters (`native-io`'s `direct_buffer::bits()`). So the JDK's
three are structurally zero and nothing was ever going to move them.

That means the harm this record argued against was live on the standard JMX
route the whole time, including in compatible mode where route A looked
healthy — and route A looking healthy is exactly why nobody asked.

**The fix.** The three COUNTERS on `java/nio/Bits$1` are forced to the natives
that already answer them for CratonVM's own carrier
(`buffer_pool_get_count` / `_memory_used` / `_total_capacity`, which read
`direct_buffer_pool_stats()`). `buffer_pool_kind` answers DIRECT for a receiver
with no kind slot, which is what a `Bits$1` receiver is, so the existing bodies
were already right for it. `getName()` is NOT forced: it is two instructions
returning `"direct"` and the JDK's own answer is correct.

That is a read-side hook and costs nothing on the allocation path. The
alternative — routing `allocateDirect` through the JDK's `Bits.reserveMemory`
so the counters are genuinely the JDK's — is a change to the direct-memory
allocator and its `MaxDirectMemorySize` enforcement, in every mode, on a hot
path, and it is not what either of these two records is about.

### After

```text
                                        HotSpot   CratonVM   CratonVM --jdk-only
JmxBlast                                  7/7       7/7          7/7
RBufferPoolCount  routeA pools     [mapped, direct, mapped - 'non-volatile memory']  (all three)
                  routeA moved            true      true         true
                  routeB moved            true      true         true
                  routes agree            true      true         true
RJdkJmx                                   PASS      PASS         PASS
```

`regression-suite/src/RBufferPoolCount.java` is the ratchet, scheduled in
`CORE_CLASSES`. It asserts the DELTA and the AGREEMENT of the two routes, never
an absolute byte count — HotSpot's `getMemoryUsed` carries page-alignment slop
that CratonVM's logical reservation does not — and the suite diffs it against
HotSpot in the same environment.

### What is NOT claimed

* The two mapped pools still read zero. Nothing in this VM accounts for
  `FileChannel.map` separately, HotSpot reads zero for them too on a process
  that has mapped nothing, and inventing a number would be worse than the
  measured zero. Unchanged, and stated in `BUFFER_POOL_KIND_MAPPED` already.
* `getTotalCapacity()` and `getMemoryUsed()` are the same number here, where
  HotSpot's differ by page-alignment slop. Also unchanged and already stated.
* Forcing three methods on `java/nio/Bits$1` names an anonymous class, and an
  anonymous class can be renumbered by a JDK upgrade. That is why the ratchet
  asserts the counter MOVES rather than asserting the hook fired: a rename
  turns `RBufferPoolCount` red with "routeB count moved = false", which names
  the defect directly.
