# `int.class.getClassLoader()` answered the app loader — plus two deeper gaps left open

**Status: the primitive-loader defect is FIXED 2026-08-27.** The other two
findings are **OPEN and scoped** in §3 and §4.

## 1. The batch

The last named large families on the bridge-kind retirement surface —
`ClassLoader` (27 rows), `Module` (23), `MethodHandles`/`MethodHandle`/
`MethodType` (34), `ForkJoinTask` (23), `ForkJoinPool` (19) — plus
`java/lang/System$1` (28), the `JavaLangAccess` implementation, which is not
callable from Java and is reached INDIRECTLY through the `Module` and
`ClassLoader` calls that route through it.

`probes/LoaderModuleSweep.java`, 95 lines. ForkJoin work is joined before it is
read and every task is a pure function of its input, so no result depends on
scheduling or worker count.

```text
compatible   3 differing lines
--jdk-only   3 differing lines     <- identical, so none is a mode defect
```

## 2. FIXED — a primitive class had a defining loader

```text
int.class.getClassLoader()
  HotSpot    null
  CratonVM   jdk.internal.loader.ClassLoaders$AppClassLoader
```

`Class.getClassLoader()` is specified to answer null for a primitive, exactly as
for a bootstrap-loaded class. `native_class_get_class_loader` consulted the
mirror's `classLoader` field and then the reverse map, and a primitive mirror is
minted by the VM and can pick up whichever loader created it. The primitive
check now runs FIRST, ahead of both.

**Why the direction matters:** a non-null loader on a primitive makes a caller
believe the type is application-defined, and the usual next move — `loader
.loadClass(name)`, or a loader-keyed cache — then keys `int` under the wrong
loader. `String.class` and `Object.class` were already correctly null; only the
primitives were wrong.

## 3. OPEN — `Module.getPackages()` for `java.base` is short

```text
java.base.getPackages().size() > 100
  HotSpot    true          CratonVM   false
```

Everything else about `Module` matched: `isNamed`, `getName`, `toString` shape,
`canRead` in both directions, `isExported` for an exported package, an internal
one, and to the unnamed module, `isOpen`, `getDescriptor` and its `name`/
`isAutomatic`/`isOpen`, the boot layer's `findModule` hit and miss, and the
unnamed module's null descriptor. **The predicates are right; the package SET is
incomplete.**

Not fixed here: the package set comes from the module's own descriptor data, and
filling it needs the real image's `module-info` read rather than a synthesised
list. Recorded because the failure is quiet — a caller enumerating packages for
a scan gets a short answer with no error, which is the same shape as the
already-recorded `getDefinedPackage` gap.

## 4. OPEN — `invokeExact` does not enforce its exact signature

```text
MethodHandle max = lookup.findStatic(Math.class, "max", (int,int)int);
max.invokeExact(1L, 2L)
  HotSpot    WrongMethodTypeException
  CratonVM   accepted
```

`invokeExact` is signature-polymorphic and, unlike `invoke`, performs NO
conversion: the call site's descriptor must match the handle's type exactly or
it must throw. Every other `MethodHandle` row in this probe matched —
`findStatic`/`findVirtual`/`findGetter`, `bindTo`, `asType`, `constant`,
`identity`, `dropArguments`, `insertArguments`, `arrayElementGetter`, the whole
`MethodType` surface, and three of the four refusals.

Not fixed here, and deliberately so: enforcing it means the call-site descriptor
has to reach the handle's dispatch, which is the `methodhandle-invoke-arrives-at
-the-stackless-interpreter-door` path — a change to how `invokeExact` is
dispatched rather than to a native body. That is its own piece of work with its
own blast radius, and the recorded `MethodHandle` findings
(`cratonvm-methodhandle-adapters-mutate-the-receiver`,
`an-adapter-that-dispatches-right-and-reports-the-wrong-type`) say this area
punishes partial changes.

**What it costs meanwhile:** a caller that gets the signature wrong gets a
silently coerced call instead of a diagnostic, and `invokeExact`'s entire reason
for existing over `invoke` is that it does not coerce.

## 5. A fourth harness artefact

The first run showed FOUR differences. One was mine: `Module.toString()` on an
UNNAMED module renders as `unnamed module @<identityHashCode>`, so printing it
put a VM-chosen identity in the diff — in a probe whose own header claimed it
printed no identity hashes. Now prints the shape (`startsWith("unnamed module
@")`) rather than the value.

That is four harness artefacts across this survey — stdout encoding, asymmetric
stderr capture, an assertion that hid the value, and now an identity in a
`toString`. All four produced confident-looking differences with no VM behaviour
behind them. **The recurring cause is printing something whose value the two VMs
are entitled to choose independently.**

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out LoaderModuleSweep
```
