# `RJdkJmx`: two different defects, one per arm — a missing `getObjectName()` and a snapshot iterator that cannot `remove()`

**Status:** the `--real-jdk` arm is FIXED in source 2026-08-06 (lane L4, JDK-only
wave 2). The `--jdk-only` arm is DIAGNOSED, NOT FIXED — its root cause is in
`native-collections/src/lib.rs`, which lane L4 does not own; the exact patch
shape is in *The `--jdk-only` arm* below. Neither is verified against a binary.

## The task's premise was wrong: the two arms do NOT fail identically

The lane brief said `RJdkJmx` "fails in BOTH `--real-jdk` and `--jdk-only`" with
the `NotCompliantMBeanException` trace. Diffing the captured logs says
otherwise, and the difference is the whole diagnosis:

| arm | last `CK` line reached | dies at | with |
| --- | --- | --- | --- |
| HotSpot 25 | `platform=[…] runtimeName=java.lang:type=Runtime` | — | `PASS RJdkJmx (49 checks)` |
| `--real-jdk` | `attrs=… ops=… notif=…` (end of `registerAndInvoke`) | `RJdkJmx.java:226`, in `platformBeans` | `AbstractMethodError: java/lang/management/PlatformManagedObject.getObjectName()Ljavax/management/ObjectName; has no Code attribute` |
| `--jdk-only` | `objectName=…` (end of `objectNames`) | `RJdkJmx.java:133`, first line of `registerAndInvoke` | `NotCompliantMBeanException: sun.management.GarbageCollectorImpl: remove`, caused by `UnsupportedOperationException: remove` at `java/util/Iterator.remove(Iterator.java:102)` |

So `--real-jdk` gets *two whole test methods further* than `--jdk-only`. They
are two independent defects and both must be fixed for the class to pass.

The brief's hypothesis (b) — "virtual dispatch picks the interface default over
the subclass override" — is **falsified**. There is no override to pick: the
receiver's class genuinely declares no `remove()` (see below). Hypothesis (a)
is the right one, and the strict-mode class refusals logged at startup
(`cratonvm/internal/Unmodifiable*`, `java/util/Enumeration$Impl`, …) are indeed
*not* the cause — a different, unlogged refusal is.

## The `--real-jdk` arm — `getObjectName()` was never registered

`RJdkJmx.platformBeans` line 226:

```java
check(rt.getObjectName().getCanonicalName().equals("java.lang:type=Runtime"), …);
```

`rt` comes from `ManagementFactory.getRuntimeMXBean()`, which
`native-builtins/src/jmx.rs:3453` intercepts with a `Bridge` native that returns
`alloc_runtime_mxbean(ctx)` — and that (`jmx.rs:3561`) is

```rust
alloc_concurrent_synthetic(ctx, "java/lang/management/RuntimeMXBean", 10)
```

i.e. an object stamped with the **interface**. Every method that can run on it
is a native registered against that interface name in `register_runtime_mxbean`
(`jmx.rs:3598`). `getObjectName()` is inherited from
`java.lang.management.PlatformManagedObject` and was not in that list, so the
`invokeinterface` resolved the abstract declaration, which has no Code
attribute, and threw. `jmx.rs:3723-3730` already documents this exact failure
family, for `getSystemProperties()`:

> The synthetic `RuntimeMXBean` is an interface object, so an unregistered
> method falls through to the abstract interface method and raises
> `AbstractMethodError: ... has no Code attribute`.

`javac` puts the **qualifying** type in the constant pool, not the declaring
one — verified on this host with JDK 25:

```
invokeinterface #19, 1  // InterfaceMethod java/lang/management/RuntimeMXBean.getObjectName:()Ljavax/management/ObjectName;
```

which is why the registration goes on each concrete MXBean interface.

The registration is reached even though the failure names `PlatformManagedObject`
(the *declaring* interface, which is where CP resolution ends up because
`RuntimeMXBean` does not redeclare the method). `interpreter.rs:1188-1240`, the
"Receiver-own-class native rescue (general)", runs before the
`AbstractMethodError` is raised: it walks the **receiver's runtime class** chain
— here `java/lang/management/RuntimeMXBean`, the class the object is stamped
with — for a registered native with the same name and descriptor, and it only
runs when that walk finds no bytecode, which is exactly this case. That is the
same rescue that already makes the sibling `getName()` / `getStartTime()`
natives reachable.

### What changed

`native-builtins/src/jmx.rs`, one new section immediately before
`// 2. RuntimeMXBean`, plus one call added at the end of
`register_jmx_natives`:

* `platform_mxbean_object_name_text(ctx, this)` — maps the receiver's stamped
  class name to its canonical platform `ObjectName` text.
* `native_platform_managed_object_name` — builds the `ObjectName` through the
  file's existing `object_name_new` (the same 1-field model `objectNames()`
  already exercises green in the `--real-jdk` log), or answers `null` for a
  class this file does not fabricate.
* `register_platform_managed_object_names(r)` — registers
  `getObjectName()Ljavax/management/ObjectName;` as `Bridge` on:

| stamped class | ObjectName |
| --- | --- |
| `java/lang/management/RuntimeMXBean` | `java.lang:type=Runtime` |
| `java/lang/management/ThreadMXBean` | `java.lang:type=Threading` |
| `java/lang/management/ClassLoadingMXBean` | `java.lang:type=ClassLoading` |
| `java/lang/management/OperatingSystemMXBean` | `java.lang:type=OperatingSystem` |
| `java/lang/management/CompilationMXBean` | `java.lang:type=Compilation` |
| `java/lang/management/PlatformLoggingMXBean` | `java.util.logging:type=Logging` |
| `jdk/management/VirtualThreadSchedulerMXBean` | `jdk.management:type=VirtualThreadScheduler` |
| `java/lang/management/GarbageCollectorMXBean` | `java.lang:type=GarbageCollector,name=` + slot 0 |

Exactly the eight interfaces an `alloc_*` helper passes to
`alloc_concurrent_synthetic`, and no more. `MemoryMXBean` is deliberately absent:
`alloc_memory_mxbean` (`jmx.rs:4014`) stamps the concrete
`sun/management/MemoryImpl`, whose real bytecode already answers this.

`Bridge`, not `SyntheticStub`: these stand in for a method the real JDK
declares on a real interface, and the Compatible path needs them, so they must
survive `--jdk-only` registration.

**Deliberately NOT registered on `java/lang/management/PlatformManagedObject`.**
An interface native outranks exact-class registrations, so a native on the root
interface would intercept receivers stamped with the concrete
`sun.management.*Impl` classes — `alloc_memory_mxbean` returns a
`sun/management/MemoryImpl`, and `alloc_garbage_collector_impl` a
`sun/management/GarbageCollectorImpl` — whose real bytecode
(`Util.newObjectName(...)`) already answers correctly. The `javac` evidence
above says the root interface is never the CP class anyway.

### Why check 256 should agree once 226 passes

`platformBeans` line 256 asserts `proxy.getStartTime() == rt.getStartTime()`
across two different objects: the fabricated bean (`jmx.rs:3588`, slot 7 =
`vm_start_epoch_ms()`) and the real `sun.management.RuntimeImpl` behind the
platform server, which reads `VMManagementImpl.getStartupTime()` — registered
at `jmx.rs:1463` as the *same* `vm_start_epoch_ms()`. One source, so they
cannot drift.

## The `--jdk-only` arm — a real `Arrays$ArrayItr` has no `remove()`

Read the strict trace innermost-frame-last:

```
Caused by: java/lang/UnsupportedOperationException: remove
    …
    at com/sun/jmx/mbeanserver/Introspector.getMXBeanInterface(Introspector.java:348)
    at com/sun/jmx/mbeanserver/MXBeanSupport.findMXBeanInterface(MXBeanSupport.java:95)
    at java/util/Iterator.remove(Iterator.java:102)
```

`Iterator.java:102` is the body of the `Iterator.remove()` **default method**,
`throw new UnsupportedOperationException("remove")`. The message `"remove"` is
the fingerprint: `Collections.unmodifiable*` iterators and `ImmutableCollections`
both throw a *message-less* UOE, so the default method is the only producer.

JDK 25 `MXBeanSupport.java:79-104`:

```java
final Set<Class<?>> candidates = newSet();          // Util.newSet() -> new HashSet<>()
…
reduce:
while (candidates.size() > 1) {
    for (Class<?> intf : candidates) {
        for (Iterator<Class<?>> it = candidates.iterator(); it.hasNext(); ) {
            final Class<?> intf2 = it.next();
            if (intf != intf2 && intf2.isAssignableFrom(intf)) {
                it.remove();                        // <- line 95
                continue reduce;
```

`sun.management.GarbageCollectorImpl` implements two MXBean interfaces
(`GarbageCollectorMXBean` extends `MemoryManagerMXBean`), so the reduce loop
runs and `it.remove()` is reached. The collection is a plain
`java.util.HashSet`, so the iterator comes from
`native-collections/src/lib.rs:11808` → `native_hs_iterator`.

That function (`native-collections/src/lib.rs:12707`) does:

```rust
let itr = match try_alloc_synthetic(ctx, "java/util/HashMap$KeyItr", MAP_KEY_ITR_NUM_FIELDS) {
    Ok(itr) => itr,
    Err(_refused) => { … return real_snapshot_iterator(ctx, keys_arr, total); }
};
```

Under `--jdk-only` the fabrication of `java/util/HashMap$KeyItr` is refused (no
JDK declares that name — the real one is `HashMap$KeyIterator`) and
`real_snapshot_iterator` (`lib.rs:1996`) hands back a real
`java.util.Arrays$ArrayItr` instead. JDK 25 `Arrays.java:4288-4310` declares
that class with **`cursor`, `a`, `hasNext()`, `next()` and nothing else** — no
`remove()` — so `it.remove()` reaches the interface default and throws. That
function's own doc comment says so in advance:

> One behaviour genuinely differs, and it is loud rather than silent:
> `remove()` on the fabricated iterator writes through to the backing
> collection …, while a fixed-size list's iterator raises
> `UnsupportedOperationException` from real JDK bytecode.

The `Compatible` arm survives because two things line up there and neither does
in strict: the fabrication succeeds, producing `HashMap$KeyItr` with a backing
pointer in `MAP_KEY_ITR_FIELD_BACKING`; and
`force_native_over_real_jdk_bytecode` (`vm/src/runtime/interpreter/native_override.rs:4470`)
routes `("java/util/Iterator", "remove", "()V")` to `native_itr_remove_noop`
(`native-collections/src/lib.rs:49475`), which dispatches on the receiver's
class name and lands on `native_map_key_itr_remove`.

Both halves fail in strict, and the second one fails *even if the first is
fixed*:

* `native_itr_remove_noop`'s match has arms for `HashMap$KeyItr`,
  `ArrayList$Itr`, `ArrayList$ListItr`, `TreeSet$Itr` and `LinkedList$Itr`.
  `java/util/Arrays$ArrayItr` falls to `_ => {}` and throws the same UOE.
* The registration is `Bridge` (`register_iterator_protocol_natives`,
  `lib.rs:49342-49355`), and `resolve_native_dispatch_wave1`
  (`vm/src/vm/vm_exec.rs:824`) sends a `Bridge` to the real bytecode whenever
  bytecode is available. The force-native call sites pass
  `bytecode_available: true` by construction (`vm_exec.rs:23739`,
  `native_override.rs:5387`), and the `Iterator.remove()` default method *is*
  available bytecode — so under `--jdk-only` the interception is declined and
  the throwing default runs. This is `[ST drop@register]` in a new place: it is
  not the registration that is dropped, it is the *win*.

### The patch this needs (not applied — file is not lane L4's)

Any real fix has to keep `it.remove()` writing through to the `HashSet`,
because `findMXBeanInterface`'s loop is `while (candidates.size() > 1)`: an
iterator whose `remove()` only mutates a snapshot leaves `size()` unchanged and
turns the reduce loop into a livelock or an
`IllegalArgumentException("implements more than one MXBean interface")`. That
rules out the cheap variants (wrapping the snapshot in a mutable `ArrayList`
and returning *its* real `ArrayList$Itr`).

The smallest shape that works is three coordinated edits, all in
`native-collections/src/lib.rs` unless noted:

1. Give the strict stand-in a backing pointer. `Arrays$ArrayItr` has exactly
   two fields and no room, so `real_snapshot_iterator` needs a GC-rooted side
   table keyed by the iterator `ObjectRef` holding `(backing set, last
   returned index)` — the same state `MAP_KEY_ITR_FIELD_BACKING` /
   `MAP_KEY_ITR_FIELD_LAST_RET` carry today.
2. Add a `"java/util/Arrays$ArrayItr"` arm to `native_itr_remove_noop`
   (`lib.rs:49486-49500`) that reads that side table and calls the existing
   `native_map_key_itr_remove` logic against the backing set.
3. Make the win survive strict mode: `register_iterator_protocol_natives`
   (`lib.rs:49355`) must register `("java/util/Iterator", "remove", "()V")` as
   `NativeKind::Intrinsic` rather than inheriting the block's `Bridge`, or
   `resolve_native_dispatch_wave1` will keep handing the call to the throwing
   default. This is a §1.4 reviewed exception and needs the justification
   written down: CratonVM allocates `java.util.HashSet` in its own compact
   layout, so the real `HashMap$KeyIterator` the JDK bytecode would produce
   cannot exist, and the "real bytecode" strict mode would prefer is a method
   that unconditionally throws.

The alternative — and the one that actually retires this class of bug rather
than patching it — is to stop giving `java.util.HashSet` a CratonVM layout
under `--jdk-only`, at which point `iterator()` is not intercepted at all and
the real `HashMap$HashIterator.remove()` runs. That is a lane-scale change to
`native-collections`, not a patch.

## Baseline: the bridge ratchet MUST be re-frozen

`scripts/baselines/jdk-only-bridge-ratchet.json` freezes
`bridge_without_acc_native` at **9528** with `"slack": 0`, and
`scripts/jdk-only-bridge-ratchet.py:314` fails on `observed > frozen + slack`.

The eight new rows bind methods the image declares **abstract on an interface**,
not `ACC_NATIVE`, so they land in `bridge.abstract_method` (1315 → 1323) and
therefore in `bridge_without_acc_native` (9528 → **9536**); `bridge.rows` goes
10304 → 10312 and `total_rows` 11876 → 11884. `bridge_shadows_bytecode` (4581)
does **not** move — an abstract declaration has no bytecode to shadow.

This is adjudicated work, not a regression: the alternative to a bridge here is
the `AbstractMethodError` this document exists to remove, because the receiver
is an object CratonVM stamps with the interface itself and no bytecode
implementation of `getObjectName()` can ever exist on it. Re-take the census on
a JDK-bearing host (`regression-suite/bridge-ratchet.sh`, then
`--update-baseline`) and carry the reason into the block's `note`. Choosing
`Intrinsic` to keep the number flat would be gaming the gate — `Intrinsic` is
the §1.4 "may shadow real bytecode" exception, and these rows shadow nothing.

Other baselines are untouched: `native-builtins/tests/stub_ratchet.rs` counts
`SyntheticStub` only, and `jdk-only-kind-map-25-linux.tsv` scores only rows
present in both baseline and census, so new rows pass and are reported.

## How to verify, once a binary exists

```sh
cargo build --release -p cratonvm-cli

javac -d regression-suite/build regression-suite/src/RJdkJmx.java
java -cp regression-suite/build RJdkJmx                          # HotSpot 25 oracle
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkJmx
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkJmx
```

* The `--real-jdk` arm must stop dying at `RJdkJmx.java:226` and print
  `CK RJdkJmx platform=[ClassLoadingMXBean, MemoryMXBean, OperatingSystemMXBean,
  RuntimeMXBean, ThreadMXBean] runtimeName=java.lang:type=Runtime`, byte-identical
  to HotSpot. That line is the direct read-back of `getObjectName()`.
* The `--jdk-only` arm will still die at `RJdkJmx.java:133` until the
  `native-collections` patch above lands. A strict run that reaches line 226 and
  fails there is the signal that only the collections half remains; a strict run
  that reaches `PASS` means both halves are done.
* Both arms must end at `PASS RJdkJmx (49 checks)`.

## The single observation that would falsify this

For the `--real-jdk` half: a `--real-jdk` run that still throws
`AbstractMethodError` on `getObjectName` after this change, naming
`java/lang/management/PlatformManagedObject` — that would mean the receiver-walk
rescue at `interpreter.rs:1188` did not fire for this receiver, and the
registration set would have to move to `PlatformManagedObject` (accepting the
shadowing risk called out above).

For the `--jdk-only` half: a strict run with `CRATONVM_HS_ITR_DBG=1` (the
`dbg_hs_itr()` switch at `native-collections/src/lib.rs:1806`) showing
`native_hs_iterator` was never called on this path — that would mean the
receiver is some other iterator with no `remove()` and the chain above names
the wrong producer, though the fix shape would be unchanged.
