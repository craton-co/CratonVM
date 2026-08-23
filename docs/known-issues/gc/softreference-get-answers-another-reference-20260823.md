# `SoftReference.get()` answers another Reference, and `MethodTypeForm` casts it

## Status

**OPEN.** Reproduces on unmodified `origin/dev` (`0b0207bf4`) and on
`fix/bcjava-pqc-and-cipherstream-20260822`, with the **same count on both**, so
it is neither caused nor fixed by anything on that branch. Found by running
`pqc.jcajce.provider.test.AllTests` to completion for the first time — no 900 s
cap could reach it.

## The symptom

```text
java.lang.ClassCastException: class java.io.ClassCache$CacheRef cannot be cast
to class java.lang.invoke.LambdaForm
    at java.lang.invoke.MethodTypeForm.cachedLambdaForm(MethodTypeForm.java:130)
    at java.lang.invoke.DirectMethodHandle.preparedLambdaForm(DirectMethodHandle.java:228)
    at java.lang.invoke.DirectMethodHandle.makeAllocator(DirectMethodHandle.java:140)
    at java.lang.invoke.MethodHandles$Lookup.serializableConstructor(MethodHandles.java:3365)
    at jdk.internal.reflect.MethodHandleAccessorFactory.newSerializableConstructorAccessor(...)
```

`MethodTypeForm.java:130` is the whole of the method:

```java
public LambdaForm cachedLambdaForm(int which) {
    SoftReference<LambdaForm> entry = lambdaForms[which];
    return (entry != null) ? entry.get() : null;      // <- line 130
}
```

so the object that failed the cast is what `SoftReference.get()` returned.
`get()` reads the `referent` field. **A `SoftReference`'s referent is another
`Reference` object** — a `java.io.ClassCache$CacheRef`, which is
`ObjectStreamClass`'s soft-reference subclass and has nothing to do with
`MethodTypeForm`.

There is a second face with the same cause. On a full
`pqc.jcajce.provider.test.AllTests` run the same defect also appears as

```text
java.lang.ClassCastException: class <something> cannot be cast to
class java.io.ObjectStreamClass
```

— `ObjectStreamClass.lookup` reading its own `ClassCache`. One run produced 22
of those and 30 of the `LambdaForm` ones. Both are a `Reference` whose referent
is not what was stored, reached through the two soft-reference caches that
Java serialization warms up first.

## Reproduce

Standalone classes do not do it. It needs several test classes in one JVM, and
about 25 minutes:

```java
public class PqcSubset {
    public static Test suite() {
        TestSuite s = new TestSuite("pqc subset");
        s.addTestSuite(XMSSTest.class);        // long-running, allocation-heavy
        s.addTestSuite(XMSSMTTest.class);
        s.addTestSuite(SLHDSATest.class);      // serializes keys
        s.addTestSuite(FalconTest.class);      // serializes keys
        return s;
    }
}
```

```bash
cratonvm --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
    -Dtest.java.version.prefix=25 \
    -c "<subset-dir>:$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner PqcSubset
```

Measured within one hour on one host, 61 tests:

| binary | result | `ClassCache$CacheRef` lines |
|---|---|---|
| `dev` (`0b0207bf4`) | `Failures: 2, Errors: 12` | **21** |
| `fix/bcjava-pqc-and-cipherstream-20260822` | `Failures: 0, Errors: 6` | **21** |

The six errors that survive on the fixed binary are exactly this defect. The
other eight are separate, fixed defects (a `Signature.getInstance` wrapping bug
and a `KeyPairGenerator` algorithm-name bug); they are listed here only because
the counts would otherwise look like a regression.

`SLHDSATest` and `FalconTest` each pass **on their own**, on both binaries, at
`-Xmx 1g`, `128m` and `64m`. Whatever primes this happens in the classes that
run before them.

## It is the default collector, not the workload

The same binary, the same subset, the same host:

| collector | result | `ClassCache$CacheRef` lines |
|---|---|---|
| default (ZGC) | `Failures: 0, Errors: 6` | 6 |
| **`--XX:UseGc G1`** | **`OK (61 tests)`** | **0** |
| **`--XX:UseGc Generational`** | **`OK (61 tests)`** | **0** |

Both of the other collectors run the whole subset clean. That places the defect
squarely in the DEFAULT collector's reference processing, and it is why either
`--XX:UseGc` spelling is a usable workaround for anyone who needs these classes
green today.

## Where to look

`SoftReference.get()` is not intercepted — `java/lang/ref/Reference` registers
`clear0`, `refersTo0` and the pending-list pair, but not `get`, so this is real
bytecode reading field 0. The referent field was therefore **written wrong by
the VM**, and `process_references_after_gc` is the only thing that writes it.

Two write paths in that function put a `Reference` into a Reference's fields,
and both have a history:

* **the ReferenceQueue linkage.** The protocol pushes the reference onto the
  queue's head and stores the old head in the reference's `next` field. It used
  to store it in the REFERENT field, which is precisely this symptom — an
  enqueued-but-unpolled reference answering `get()` with the next Reference in
  the queue. That was fixed: `gc_reference_next_slot` now resolves `next` by
  name on `java/lang/ref/Reference`, and only a legacy 2-field synthetic falls
  back to slot 0. Worth re-checking that no real-JDK `SoftReference` subclass
  reaches that fallback.
* **the referent remap.** After a moving collection the processor rewrites each
  surviving referent through `pointer_map`
  (`gc_and_alloc.rs`, `let referent_new = match pointer_map.get(&referent_old)`).
  A wrong or stale entry there writes a wrong OBJECT into the referent slot, and
  the wrong object would be whatever now occupies that address — which is how a
  `CacheRef` could land in a `MethodTypeForm` slot.

The distinguishing question is cheap: does the referent become wrong at a
collection, or at an enqueue? Stamping the referent slot's writer (the two sites
above already have `straystack_enabled()` tracing) and re-running the repro
answers it in one run.

## What is not claimed

**Not that this is new.** Nothing here bisects it. It reproduces on `dev` today;
how long it has been there is unmeasured.

**Not that six is the population.** Six is what this four-class subset produces.
The full `pqc.jcajce.provider.test.AllTests` produced 1, 24, 45 and 104 of these
lines on four runs whose binaries differ only by unrelated JCA fixes — the count
is a property of the run, not of the build, and no single run's count is a rate.
(The `dev` binary's own runs bracket that range: zero on one full-suite run, one
on the next.)

**Not that it is confined to `MethodTypeForm`.** That is simply the first
consumer to cast the result. Any `get()` on an affected reference returns a
wrong object; a caller that does not cast would use it silently.
