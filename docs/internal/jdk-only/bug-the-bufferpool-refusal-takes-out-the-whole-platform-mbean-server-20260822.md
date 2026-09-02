# The `BufferPool` refusal is correctly loud and wrongly SCOPED — strict mode loses the whole platform MBean server

**Status: FIXED 2026-08-24, and NOT by the fix this record asked for.**
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

Expect `rc=1`. Drop `--jdk-only` for `PASS RJdkJmx (67 checks)`.
`probes/JmxBlast.java` isolates the three entry points above.
