# `BuiltinClassLoader` could not link, and `Class.getName` was never tagged

**2026-09-10.** The two structural blockers the loader-and-bootstrap lane
recorded rather than attempted, both closed, plus the triage that lane owed the
other eight.

This is the record that retires the `lane-7-loader-and-bootstrap` page —
retired to the internal tree, so it is named here rather than linked. Everything
durable on that page is here or in the code; the page itself is a campaign
record now.

---

## 1. Two natives disagreed, and `BuiltinClassLoader` could not link

Under `CRATONVM_ENFORCE_NATIVE_SHADOW=all`, ten `--jdk-only` corpus vectors died
with a line that names the consumer, not the cause:

```text
NoClassDefFoundError: jdk/internal/loader/BuiltinClassLoader
```

The class is in the image. `--verbose:class` says nothing, and neither does the
`NoClassDefFoundError` stack: `ensure_class_initialized_shared` raises a *fresh*
one at every later use, so the trace names whoever touched the class and never
the `<clinit>` that failed. `CRATONVM_DBG_CLINIT_FAIL=1` is the lever that
exists for exactly this, and one run was enough:

```text
[DBG_CLINIT_FAIL] <clinit> failure NOT swallowed ... class=jdk/internal/loader/BuiltinClassLoader
                  exc=java/lang/InternalError
```

### The `InternalError` is one `if`, and the image says which

`javap -c` on the image rather than a guess — JDK 25
`BuiltinClassLoader.<clinit>`:

```text
16: invokestatic  #509   // ClassLoader.registerAsParallelCapable:()Z
19: ifne          33
22: new           #65    // class java/lang/InternalError
26: ldc_w         #514   // String Unable to register as parallel capable
32: athrow
```

`ClassLoader.registerAsParallelCapable()` is
`ParallelLoaders.register(Reflection.getCallerClass())`, and that is

```java
if (loaderTypes.contains(c.getSuperclass())) { loaderTypes.add(c); return true; }
return false;
```

so registration is a **chain**: `ParallelLoaders.<clinit>` seeds `ClassLoader`,
`SecureClassLoader` registers under it, `BuiltinClassLoader` under
`SecureClassLoader`. `BuiltinClassLoader.getSuperclass()` is
`java.security.SecureClassLoader`, not `ClassLoader` — read, not assumed.

### Which link, measured without reflection

`--add-opens java.base/java.lang=ALL-UNNAMED` does not survive `--jdk-only`: a
probe that reflects on `ParallelLoaders.loaderTypes` gets
`InaccessibleObjectException` and measures nothing.

`apps/probes/L7ParallelCapableProbe.java` asks the same question with no
reflection at all. A subclass of `X` whose `<clinit>` calls
`registerAsParallelCapable()` gets `true` exactly when `X` is already in
`loaderTypes`, so three subclasses read three links:

```text
                                            HotSpot  armed  unarmed
  Direct       extends ClassLoader           true     true   true
  UnderSecure  extends SecureClassLoader     true     FALSE  true
  UnderUrl     extends URLClassLoader        true     FALSE  true
```

Row 1 is the control, and it is the important one: the real
`registerAsParallelCapable` bytecode and `Reflection.getCallerClass()` both work
here. What is broken is that `java.security.SecureClassLoader` never registered.

### The two registrations, and why neither alone is the bug

```text
java/lang/ClassLoader.registerAsParallelCapable()Z   classloader_real.rs:623 (+2 more registrations)
java/security/SecureClassLoader.<clinit>()V          classloader_real.rs:830
```

* the first returns a constant `Int(1)`. Its comment says why: *"CratonVM does
  not serialize loading on a per-class-name lock in the first place, so every
  loader behaves as parallel-capable and `true` is the accurate answer"*.
  Accurate as an ANSWER — but it never touches `loaderTypes`.
* the second is a **no-op**, added as S111r9 to route around a Spring Boot
  fat-jar launcher corruption in `--real-jdk`.

Unarmed, both natives run and the pair is self-consistent: nobody reads
`loaderTypes` and every caller gets the constant `true`. Armed,
`BuiltinClassLoader.<clinit>` is real bytecode reading the real set while
`SecureClassLoader.<clinit>` is *still the native* — because it runs inside the
built-in loader allocation chain, i.e. **from inside a native, where the dial
cannot see it.** That is the lane's own trap, and it has a consequence worth
stating plainly: **no arm of `CRATONVM_ENFORCE_NATIVE_SHADOW` could ever have
reached this row.** Only a registration-time refusal does.

The state is the corrupt MIXTURE `scripts/jdk-only-blast-radius.sh` caveat 4
describes — uniform-native works, uniform-bytecode works, half and half does
not — and not a partial retirement.

### The fix: `RETIRED_SHADOW_L7_TRIPLES`, two rows, retired as a pair

Both are bucket A (the image method declares `Code`), so yielding has somewhere
to go, and `Compatible` is untouched — a `SyntheticStub` registers and
dispatches normally there, which is what keeps S111r9 intact for the
`--real-jdk` launchers it was written for.

**All or none.** Retiring only `SecureClassLoader.<clinit>` runs its real
bytecode into the constant-`true` native and registers nothing; retiring only
`registerAsParallelCapable` leaves the `<clinit>` a no-op, so nothing calls it.
Either half alone leaves `BuiltinClassLoader.<clinit>` throwing — the same shape
as the `LogRecord` source pair two waves earlier.

The native's own comment said a faithful implementation was *"not implementable
... `NativeContext` exposes no caller-class / stack-walk accessor"*.
`NativeContext::frame_class_ids` has existed since the
`latestUserDefinedLoader` work, so that sentence is stale — worth recording
because a comment saying "we lack a capability" outlives the day it is built.
It is not why retirement was chosen, though: a native mirroring
`ParallelLoaders` would keep a second copy of a JDK set in step with the JDK's
own, and §1.4's remedy for a shadow over concrete bytecode is to yield to it.

## 2. `Class.getName` was recorded as tagged, and was not

`lane-0-integration-and-gates.md` §7 and `lane-6-net-security.md` §2 both say
`Class.getName` is a reviewed `Intrinsic` and that the `ServiceLoader`
*"module java.base does not declare `uses`"* family is closed by it. **On `dev`
it was still an ambient `Bridge`**, and three independent instruments agree:

```text
--dump-native-registry --explain-jdk-only   kind=bridge  kind_stated=false
scripts/baselines/jdk-only-kind-map-25-linux.tsv   java/lang/Class getName ()Ljava/lang/String; 0 bridge 0 1
git log --all -S ClassNameSweep             the only hit is the doc that describes the probe
```

`apps/probes/ClassNameSweep.java` did not exist in any commit. This is the
lane's own §4 rule reading back on itself: a triage page is stale the day after
it is written, and prose counts are never re-frozen.

### Why yielding is wrong here, and how

`getName`'s real body is
`String name = this.name; return name != null ? name : initClassName();`, and
`java.lang.Class.name` in this VM is an **overlay**: the mirror allocator
(`vm/src/vm/vm_object.rs`, the `slots.name` store) writes the VM's INTERNAL name
into it, and `lang_class::mirror_class_name_strict` and its callers read it back
in that form deliberately — that strict reader is the fix for ByteBuddy's
hierarchy walker, which the reverse map's Object aliasing broke. So yielding
hands every caller `java/lang/Object`, which is worse than a null because
nothing throws.

`apps/probes/ClassNameSweep.java`, 85 rows over every receiver shape whose rules
differ and every accessor the JDK derives from `getName`:

```text
                                                 rows differing, of 85
  HotSpot vs --jdk-only unarmed                     0
  HotSpot vs CRATONVM_ENFORCE_NATIVE_SHADOW=all     9      <- before the tag
  HotSpot vs the same arm, after the tag            1
```

and the nine are not cosmetic:

```text
 73 forName(Object.getName())==Object      ClassNotFoundException: java/lang/Object
 74 forName(String[].getName())==String[]  ClassNotFoundException: [Ljava/lang/String;
  6 Object.toString                        class java/lang/Object
 79 no slash: Object                       false
```

**They also explain why a dial arm alone would have called this family clean.**
Rows reached through a METHOD REFERENCE (`c::getName`) kept the native and
answered correctly; the same call written inline in a lambda body yielded and
answered `java/lang/Object`. Rows 1-5 of the sweep — `getName`, `getTypeName`,
`getCanonicalName`, `getSimpleName`, `getPackageName` on `Object`, all method
references — MATCH under the dial. A probe that asked only that route reports
the family fine. One tag removes the split by making the row exempt at every
door.

**The row the tag does not repair is recorded, not frozen.**
`Class.forName(Nested.class.getName())` still throws `ClassNotFoundException:
ClassNameSweep$Nested` under the dial while the same round-trip works for
`java.lang.Object` and for `String[]`. That is a `Class.forName` defect on a
nested application class, not a `getName` one — `getName` now returns the name
HotSpot returns — and it is L0's row. The sweep is checked in, so it goes red
the day it is fixed or the day this answer drifts.

`Class.getName` is now `register_with_kind(..., NativeKind::Intrinsic)`, with
the numbers in the registrar's rationale and the kind-map row amended
`bridge 0` -> `intrinsic 1`.

### The instrument this defect had already broken

The first version of `ClassNameSweep` printed **zero rows** in the armed arm.
`System.out.printf` reaches `java.util.Formatter` -> `DecimalFormatSymbols` ->
`LocaleProviderAdapter`, which throws
`ServiceConfigurationError: Locale provider adapter "CLDR" cannot be
instantiated`. A zero-row arm is a mute instrument, not a result — the probe now
pads by hand and stays on `System.out.println`, and any probe written for the
armed arm must do the same.

### The tag also exposed a blind spot in the drift gate, and `dev` was already red

Landing the tag turned `registrar_drift.rs`'s
`the_drift_baseline_has_no_stale_rows` red, reporting that
`(register_synthetic_overrides, java/lang/Class.getName)` "no longer drifts".
It does still drift. **The scanner cannot see `register_with_kind(`.**

That file has two sweeps. The call-site sweep matches
`name.starts_with("register")`, which includes `register_with_kind`. The
enclosing-fn sweep required the byte after `register` to be `(` — its own
comment listed `register_with_kind(` among the things it skips. So the moment a
triple is TAGGED, its shipping registration becomes unreadable to the second
sweep and the pair reads as resolved: the gate reports the loss as GOOD NEWS,
which is the exact failure mode that file's header warns about, arriving through
the parser instead of through a refactor.

**It had already happened.** The same run named a SECOND stale row,
`(register_p59_module, java/lang/Class.getModule)` — tagged `Intrinsic` on
2026-09-09 by `0b2791ac7`. `git show origin/dev:native-builtins/src/lib.rs`
confirms the shipping registration there is `register_with_kind`, so this gate
has been red on `dev` since that commit, for a reason nobody could act on.
Reproducing it with a second row is how the parser bug was found rather than the
row baselined away.

Fixed: the second sweep now accepts the `_with_kind` suffix (745 call sites
across the six scanned crates are spelled that way). With it, both stale rows
come back and **54 further pairs appear**, all of the same shape — a
synthetic-only pass plus `register_essential_natives_with_shims` — and all of
them real drift the scanner had been unable to read. `DRIFT_TRIPLES` is re-taken
from the test's own PASTE-READY output, 1224 -> 1275 triples and 1357 -> 1411
pairs. **Nothing was deleted from that baseline**; it grew by what the scanner
could not see. 7 of 7 green.

## 3. The link the fix exposed: `java.lang.ClassLoader.scl` is null

The `BuiltinClassLoader` fix made `apps/probes/L7UnnamedModuleSweep.java` go
from 10 of 10 matching to SIX of its ten rows throwing, in the ARMED arm only —
which is why the wave was measured with its probes before its corpus:

```text
1 scl.getUnnamedModule()==null THREW NullPointerException:
    Cannot invoke "java.lang.ClassLoader.getUnnamedModule()" because "<local0>" is null
```

`ClassLoader.getSystemClassLoader()` answered **null**. That is the fourth
member of the published-static cluster, arriving exactly the way the other
three did.

The real `System.initPhase3()` calls `ClassLoader.initSystemClassLoader()`,
which assigns the static `ClassLoader.scl`. This VM has no `initPhase3` body in
real-JDK mode — the registration is `#[cfg(feature = "synthetic-jdk")]` and the
body is a documented no-op — and `scl` is stamped only as the LAST step of
`classloader::get_or_create_app_loader`, which is lazy. The real
`getSystemClassLoader()` bytecode is `switch (VM.initLevel()) { … } return scl;`,
so it read a null whenever it ran before the first native that needed an app
loader.

**It was invisible while the loader hierarchy could not link**, because the
vectors that would have reached it died earlier. A fix that unblocks a chain
exposes the next link, and reporting the delta without the new link would be the
same arithmetic the lane page's §4 warns about, one increment on.

The remedy is the cluster's own rule — *a published static must beat its
reader* — so `publish_system_class_loader` forces the loader at the end of the
`--jdk-only` `initPhase1` arm, beside `System.props`,
`SharedSecrets.javaLangAccess` and `VM.savedProps`. Nothing decides the VALUE
there: the stamp stays where it always was, at the end of
`get_or_create_app_loader`, so forcing it early cannot make two writers
disagree. `--real-jdk` is untouched, on the precedent
`publish_real_system_props` set.

With it, `L7UnnamedModuleSweep` is back to 10 of 10 on all three arms and
`apps/probes/L7ParallelCapableProbe.java`'s companion `L7Scl2` reads
`getSystemClassLoader() == null -> false` on all six of its rows armed, where
before the publish every one of them read `true`.

## 4. The null `java.lang.Module` family is CLOSED

The lane page's target 2 — `ClassLoader.getUnnamedModule()` answering null
through `ClassLoader.postDefineClass` -> `NamedPackage.<init>` — was closed on
2026-09-09 by lane 0's `Class.getModule` -> reviewed `Intrinsic`, before this
lane opened. Re-measured rather than assumed, because a page's own §4 says to:

`apps/probes/L7UnnamedModuleSweep.java`, 10 rows over the built-in application
loader, a user-defined loader, a named-module control, the
`Class.getModule() == loader.getUnnamedModule()` identity contract, and
`isNamed`/`getName`/`getClassLoader`:

```text
  HotSpot / --jdk-only unarmed / --jdk-only armed:  10 of 10 IDENTICAL
```

No field publish is needed and none was written. One residual stands, and it is
lane 0's, not this one's: `Module.getLayer()` on the *unnamed* module answers
non-null where HotSpot answers null. It is row 32 of
`apps/probes/ClassModuleSweep.java`, recorded and not frozen.

## 5. The armed corpus, re-classified and routed

The lane page's §4 owed the other eight lanes a table. Two things had to be
rebuilt before it could be written, and both are worth stating because they are
why the previous classification could not be re-derived from the harness.

**1. `run.sh`'s per-vector `why` is not the exception.** It is the last stderr
line matching a fixed alternation, capped at 90 characters, and for a large
share of the armed arm that line is the dial's own `[DIAL_DOOR_CENSUS]` banner.
The taxonomy here comes from a re-run of each failing vector with **run.sh's own
command line** — the three per-class hooks come from `harness-guard.sh`, the
single definition `run.sh` itself sources, so `--module-path` and the service
vectors' `-cp` additions are not silently dropped — keeping the raw stderr.

**2. Routing on the failing frame is a tautology.** An `AssertionError` is
thrown in the vector's own `check()`, so the innermost non-constructor frame is
the vector, and every assertion would route to whoever owns `java/lang`. The
owner column comes from each vector's own class javadoc instead, mapped to lane
0 §2 by hand.

### The taxonomy, re-measured

```text
  BEFORE (107 failing)              AFTER (90 failing)
  44  AssertionError              32  AssertionError
  18  NullPointerException        21  NullPointerException
  13  <no exception line>         14  <no exception line>
  6  NoClassDefFoundError         6  IllegalArgumentException
  6  IllegalArgumentException     3  InternalError
  5  ServiceConfigurationError    3  AbstractMethodError
  3  ExceptionInInitializerError  2  ExceptionInInitializerError
  2  InternalError                2  UnsatisfiedLinkError
  2  AbstractMethodError          1  ArithmeticException
  2  RuntimeException             1  ClassNotFoundException
  1  ArithmeticException          1  FileNotFoundException
  1  ClassNotFoundException       1  ClassCastException
  1  FileNotFoundException        1  RuntimeException
  1  UnsatisfiedLinkError         1  SocketException
  1  SocketException              1  InaccessibleObjectException
  1  InaccessibleObjectException  
```

### Who owns them

```text
    23  VM-INTERNAL
    18  L4
    14  L3
    11  L5
     9  L2
     9  L6
     8  L1
     7  L7
     6  FROZEN
     2  L0
```

**Two of those buckets are not lanes, and the lane page's framing had no row for
either.** `VM-INTERNAL` is a vector that exercises the JIT, the collector or
class unloading: it fails under the dial because the dial changes dispatch
VM-wide, not because a §1.4 shadow in some lane's prefix answered wrongly.
Handing `RJitTreeSubMapIter` to L1 because a `TreeMap` appears in its assertion
would hand L1 work it cannot do. `FROZEN` is lane 0 §2's own 316 unowned rows,
which by construction have no owner to route to.

### The table

The 90 rows the armed arm still fails on, after this wave:

| vector | owner | subject | exception (armed) | failing site |
|---|---|---|---|---|
| `RBufferPoolCount` | **FROZEN** | direct buffer pool through both routes (java.lang.management) | `ExceptionInInitializerError` | `sun/management/ManagementFactoryHelper.isPlatformLoggingMXBeanAvailable` |
| `RJdkAwtHeadless` | **FROZEN** | headless AWT image and Graphics2D | `UnsatisfiedLinkError: Can't load library: /data/jdkimages/jdk25-linux/jdk-25.0.4+7/lib` | `jdk/internal/loader/NativeLibraries$NativeLibraryImpl.open` |
| `RJdkJmx` | **FROZEN** | JMX ManagementFactory, ObjectName, MBean | `NullPointerException: Cannot invoke "java.lang.reflect.Constructor.newInstanceWithCall` | `java/lang/reflect/ReflectAccess.newInstance` |
| `RJdkProcess` | **FROZEN** | ProcessHandle current process, parent, info | `AssertionError: the sleeper must be alive` | `RJdkProcess.check` |
| `RJdkProcessStreams` | **FROZEN** | Process child streams | `<no exception line>` | `` |
| `RJdkSqlPackage` | **FROZEN** | java.sql must be loadable | `InternalError: RJdkSqlPackage$Holder::ref cannot be accessed reflectively befor` | `jdk/internal/reflect/MethodHandleAccessorFactory.newFieldAccessor` |
| `RJdkFieldModule` | **L0** | the JPMS half of Field.get/Field.set | `AssertionError: java.base must export java.lang` | `RJdkFieldModule.check` |
| `RJdkModule` | **L0** | named-module application, module path, reads, exports | `NullPointerException: Cannot invoke "java.lang.ModuleLayer.findModule(String)" because` | `RJdkModule.svc` |
| `RJdkCollections` | **L1** | the collections graph | `AssertionError: TreeMap order` | `RJdkCollections.check` |
| `RJdkLogging` | **L1** | java.util.logging after the 84-shadow retirement | `ClassCastException: class java.lang.Class cannot be cast to class java.lang.invoke.R` | `java/lang/invoke/MethodHandleImpl$1.getDeclaringClass` |
| `RJdkOptionalShape` | **L1** | java.util.Optional layout and its nine natives | `IllegalArgumentException: No group with name <VNUM>` | `java/util/regex/Matcher.getMatchedGroupIndex` |
| `RJdkViews` | **L1** | collection views | `AssertionError: descendingMap key order: [d, c, b, a]` | `RJdkViews.check` |
| `RSimpleDateFormatZone` | **L1** | SimpleDateFormat over an app-built SimpleTimeZone | `AssertionError: getTimeZone("America/New_York").getOffset(JUL) must be -14400000` | `RSimpleDateFormatZone.check` |
| `RSimpleTimeZoneRaw` | **L1** | SimpleTimeZone ID is an opaque label | `<no exception line>` | `` |
| `RBigIntMontgomery` | **L2** | BigInteger Montgomery multiply | `ArithmeticException: BigInteger not invertible.` | `java/math/MutableBigInteger.mutableModInverse` |
| `RJdkFailure` | **L2** | failure semantics, missing class/method/field | `NullPointerException: Cannot invoke "java.net.URLStreamHandler.openConnection(java.net` | `java/net/URL.openConnection` |
| `RJdkRecords` | **L2** | records and sealed classes | `AssertionError: generic component type: java.lang.Object` | `RJdkRecords.check` |
| `RAnnotationProxyGate` | **L3** | annotation-proxy dispatch gate | `NullPointerException: Cannot invoke "RAnnotationProxyGate$Tag.annotationType()" becaus` | `RAnnotationProxyGate.main` |
| `RCanAccessRules` | **L3** | AccessibleObject.canAccess rules | `InternalError: RCanAccessRules$PrivateEnum::GET_TYPE cannot be accessed reflect` | `jdk/internal/reflect/MethodHandleAccessorFactory.newFieldAccessor` |
| `RJdkHandles` | **L3** | MethodHandle / VarHandle lookup | `<no exception line>` | `` |
| `RJdkLambdas` | **L3** | lambdas and method references, invokedynamic | `AssertionError: identity implementation class must be synthetic (generated)` | `RJdkLambdas.check` |
| `RJdkLookupIn` | **L3** | MethodHandles.Lookup.in / dropLookupMode | `AssertionError: RJdkLookupIn: in(other module) keeps PUBLIC only: got 0 want 1` | `RJdkLookupIn.check` |
| `RJdkProxy` | **L3** | dynamic proxies | `IllegalArgumentException: 'proxy' is not a proxy instance` | `java/lang/reflect/Proxy.invokeDefault` |
| `RJdkProxyIface` | **L3** | MethodHandleProxies.asInterfaceInstance | `AssertionError: 9 of 9 steps failed: [invokesTheTarget, voidSam, identityAndClas` | `RJdkProxyIface.main` |
| `RJdkReflBox` | **L3** | reflective boxing identity | `RuntimeException: 29 reflective boxing identity checks failed` | `RJdkReflBox.main` |
| `RJdkReflect` | **L3** | reflection and serialization accessors | `AssertionError: generic field type` | `RJdkReflect.check` |
| `RJdkVarHandleModeSupport` | **L3** | which access modes a VarHandle supports by type | `<no exception line>` | `` |
| `RJdkVarHandleNullCoord` | **L3** | every VarHandle access mode with a null coordinate | `<no exception line>` | `` |
| `RReflect` | **L3** | reflection and runtime annotations | `AssertionError: getAnnotation present` | `RReflect.check` |
| `RVarHandleAccess` | **L3** | VarHandle access modes | `AssertionError: get boolean: expected true got false` | `RVarHandleAccess.eq` |
| `RChannelInterrupt` | **L4** | nio null interruptor, channel half | `IllegalArgumentException: Unsafe.setMemory: address 0x0 is not in any live arena` | `jdk/internal/misc/Unsafe.setMemory` |
| `RChannelToString` | **L4** | SocketChannelImpl.toString | `NullPointerException: Cannot invoke "java.net.InetSocketAddress.getPort()"` | `RChannelToString.main` |
| `RDirectBufferElem` | **L4** | direct ByteBuffer element access | `IllegalArgumentException: Unsafe.setMemory: address 0x0 is not in any live arena` | `jdk/internal/misc/Unsafe.setMemory` |
| `RFileChannelFastIo` | **L4** | FileChannel fast I/O path | `IllegalArgumentException: Unsafe.setMemory: address 0x0 is not in any live arena` | `jdk/internal/misc/Unsafe.setMemory` |
| `RFileTimes` | **L4** | java.nio.file attribute times | `FileNotFoundException: sun.nio.fs.UnixPath@6a14b45c/plain.txt (No such file or director` | `java/io/FileOutputStream.open` |
| `RForeignLayoutCollections` | **L4** | natives registered on FFM layout collections | `AssertionError: RForeignLayoutCollections: map.get.k1 = null, want v1` | `RForeignLayoutCollections.check` |
| `RForeignLayoutJdkInterfaces` | **L4** | natives registered on JDK interfaces, foreign implementor | `NullPointerException: Cannot invoke "java.lang.Class.descriptorString()" because "this` | `java/lang/Class.descriptorString` |
| `RFsSingleton` | **L4** | FileSystems.getDefault singleton | `NullPointerException: Cannot invoke "java.net.URLStreamHandler.openConnection(java.net` | `java/net/URL.openConnection` |
| `RJdkAsyncChannel` | **L4** | AsynchronousFileChannel and its Future | `<no exception line>` | `` |
| `RJdkByteOrder` | **L4** | ByteBuffer.order and everything that reads it | `IllegalArgumentException: Unsafe.setMemory: address 0x0 is not in any live arena` | `jdk/internal/misc/Unsafe.setMemory` |
| `RJdkForeign` | **L4** | the Panama / FFM surface, Linker, downcalls | `AssertionError: 7 of 7 steps failed: [layouts, downcall, downcallInt, handleIsAR` | `RJdkForeign.main` |
| `RJdkNio` | **L4** | files and NIO, random access, mapping, channels | `NullPointerException: Cannot invoke "sun.nio.fs.UnixDirectoryStream.iterator(java.nio.` | `sun/nio/fs/UnixSecureDirectoryStream.iterator` |
| `RJdkWatchService` | **L4** | java.nio.file.WatchService | `NullPointerException: Cannot read field "poller" because the return value of "sun.nio.` | `sun/nio/fs/LinuxWatchService$LinuxWatchKey.cancel` |
| `RSegmentBulkCopy` | **L4** | MemorySegment bulk copy | `ExceptionInInitializerError` | `jdk/internal/foreign/Utils.<clinit>` |
| `RSerial` | **L4** | java.io serialization round-trip | `NullPointerException: Cannot invoke "java.lang.Class.descriptorString()" because "this` | `java/lang/Class.descriptorString` |
| `RSocketChannelInterrupt` | **L4** | nio null interruptor, socket half | `NullPointerException: Cannot invoke "java.net.InetSocketAddress.getPort()" because the` | `RSocketChannelInterrupt.main` |
| `RSocketFastIo` | **L4** | socket fast I/O path | `NullPointerException: Cannot invoke "java.net.InetSocketAddress.getPort()" because the` | `RSocketFastIo.main` |
| `RAtomicArray` | **L5** | java.util.concurrent.atomic arrays | `AssertionError: RAtomicArray: guarded increments lost: expected 80000 got 0` | `RAtomicArray.check` |
| `RBlockingQueue` | **L5** | blocking-queue natives over real JDK classes | `<no exception line>` | `` |
| `RChmKeySetView` | **L5** | ConcurrentHashMap.newKeySet | `AssertionError: churn round 0: size=1 (expected 0)` | `RChmKeySetView.check` |
| `RExecutorShutdown` | **L5** | executor shutdown interrupts a running task | `AssertionError: task never started` | `RExecutorShutdown.check` |
| `RJdkAqs` | **L5** | AQS reentrant locks, read/write locks, conditions | `AssertionError: contended counter: 0` | `RJdkAqs.check` |
| `RJdkExecutors` | **L5** | executors, fixed pool, scheduled, shutdown | `<no exception line>` | `` |
| `RJdkJni` | **L5** | JNI native library load, registered and symbol-bound | `NullPointerException: Cannot invoke "jdk.internal.loader.NativeLibraries.loadLibrary(j` | `java/lang/ClassLoader.loadLibrary` |
| `RJdkPhaser` | **L5** | java.util.concurrent.Phaser | `<no exception line>` | `` |
| `RShutdownHooks` | **L5** | Runtime.addShutdownHook | `<no exception line>` | `` |
| `RUnsafeArrayBase` | **L5** | sun.misc.Unsafe get/put with an array base | `InaccessibleObjectException: Unable to make field private static final sun.misc.Unsafe sun.mi` | `java/lang/reflect/AccessibleObject.throwInaccessibleObjectException` |
| `RChaCha20Cipher` | **L6** | javax.crypto.Cipher ChaCha20 and the AEAD form | `NullPointerException: Cannot enter synchronized block because "this.lock" is null` | `javax/crypto/Cipher.chooseProvider` |
| `RCrypto` | **L6** | JCA digests, HMAC, AES-GCM, asymmetric | `AbstractMethodError: method java/security/MessageDigestSpi.engineUpdate([BII)V has no` | `java/security/MessageDigest.update` |
| `RJdkNet` | **L6** | networking, loopback TCP/UDP, DNS, socket options | `UnsatisfiedLinkError: java/net/Inet6AddressImpl.lookupAllHostAddr(Ljava/lang/String;I)` | `java/net/Inet6AddressImpl.lookupAllHostAddr` |
| `RJdkSecurity` | **L6** | SecureRandom, message digests | `AbstractMethodError: method java/security/MessageDigestSpi.engineUpdate([BII)V has no` | `java/security/MessageDigest.update` |
| `RJdkX509Intercept` | **L6** | the seventeen natives on the abstract X509 carrier | `NullPointerException: Cannot invoke "java.security.KeyFactorySpi.engineGeneratePublic(` | `java/security/KeyFactory.generatePublic` |
| `RPermissionInit` | **L6** | the permission family constructors | `InternalError: java.security.Permission::serialVersionUID cannot be accessed re` | `jdk/internal/reflect/MethodHandleAccessorFactory.newFieldAccessor` |
| `RSslEndpointIdentification` | **L6** | SSLEngine endpoint identification, CVE-2018-8034 | `NullPointerException: Cannot invoke "java.security.KeyPair.getPublic()"` | `RSslEndpointIdentification.selfSigned` |
| `RSslLiveSession` | **L6** | JSSE session that has genuinely negotiated | `<no exception line>` | `` |
| `RSslNullSession` | **L6** | JSSE session that has negotiated nothing | `SocketException: Unconnected sockets not implemented` | `javax/net/SocketFactory.createSocket` |
| `RBuiltinLoaderPackages` | **L7** | getDefinedPackage on the three built-in loaders | `AssertionError: RBuiltinLoaderPackages: the walk finds java.sql too: getPackage ` | `RBuiltinLoaderPackages.ck` |
| `RJdkDefineClass` | **L7** | ClassLoader.defineClass1 / defineClass2 | `NullPointerException: Cannot invoke "java.net.URLStreamHandler.toExternalForm(java.net` | `java/net/URL.toExternalForm` |
| `RJdkHidden` | **L7** | hidden classes | `NullPointerException: Cannot invoke "java.net.URLStreamHandler.openConnection(java.net` | `java/net/URL.openConnection` |
| `RJdkServices` | **L7** | ServiceLoader class-path provider discovery | `NullPointerException: Cannot invoke "java.net.URLStreamHandler.openConnection(java.net` | `java/net/URL.openConnection` |
| `RLangPackages` | **L7** | the ten classloader-package natives | `AssertionError: self.getPackage!=null: the unnamed package has a Package object` | `RLangPackages.ck` |
| `RLoaderIdentity` | **L7** | which loader DEFINES a class | `AssertionError: RLoaderIdentity: java.sql.Connection is platform-defined: got jd` | `RLoaderIdentity.ck` |
| `RServiceLoaderDoubleSource` | **L7** | one provider reaching ServiceLoader through two sources | `NullPointerException: Cannot invoke "java.net.URLStreamHandler.toExternalForm(java.net` | `java/net/URL.toExternalForm` |
| `RClassUnloadSweep` | **VM-INTERNAL** | class unloading on the System.gc() path | `<no exception line>` | `` |
| `RClassUnloadSweepGen` | **VM-INTERNAL** | the same probe under the generational young sweep | `<no exception line>` | `` |
| `RFieldSiteCache` | **VM-INTERNAL** | field-site inline cache | `ClassNotFoundException: RFieldSiteCache$Epoch0` | `java/lang/Class.forName` |
| `RJdkIntrinsics2` | **VM-INTERNAL** | NativeKind::Intrinsic census, second generation | `AbstractMethodError: method java/security/MessageDigestSpi.engineUpdate([BII)V has no` | `java/security/MessageDigest.update` |
| `RJdkIntrinsics3` | **VM-INTERNAL** | NativeKind::Intrinsic census, third generation | `AssertionError: bigint: shiftLeft(1000) then shiftRight(1000) must be the identi` | `RJdkIntrinsics3.check` |
| `RJdkStrict` | **VM-INTERNAL** | the MODE-DIVERGENT probes | `AssertionError: the identity lambda class must be synthetic` | `RJdkStrict.check` |
| `RJitMapTierDiff` | **VM-INTERNAL** | map operation interpreted vs compiled | `AssertionError: 2 divergence(s)` | `RJitMapTierDiff.main` |
| `RJitMultiArrayClass` | **VM-INTERNAL** | multianewarray runtime array class | `AssertionError: 3 divergence(s)` | `RJitMultiArrayClass.main` |
| `RJitStackTraceLines` | **VM-INTERNAL** | a stack trace must not degrade when methods tier up | `AssertionError: cold: innermost frame is fillInStackTrace, expected leaf` | `RJitStackTraceLines.check` |
| `RJitTreeSubMapIter` | **VM-INTERNAL** | compiled tailMap entrySet iteration | `AssertionError: compiled tailMap entrySet iteration diverged on 3497 of 4000 rou` | `RJitTreeSubMapIter.check` |
| `RJitVarHandleRefRead` | **VM-INTERNAL** | VarHandle reference-read thin bind, hot | `NullPointerException: Cannot read field "v" because "<local4>" is null` | `RJitVarHandleRefRead.hotStrictRead` |
| `RMapGcStress` | **VM-INTERNAL** | map natives walking a bucket chain under GC | `AssertionError: HashMap/filled: iterated 0 != 3000` | `RMapGcStress.check` |
| `RMapResizeGc` | **VM-INTERNAL** | HashMap/CHM resize under GC pressure | `AssertionError: HashMap: iterated 0 != 20000` | `RMapResizeGc.check` |
| `ROverlaySystemGcStress` | **VM-INTERNAL** | in-place old-gen sweep | `AssertionError: lhs iteration yielded 0 of 24` | `ROverlaySystemGcStress.check` |
| `RPriorityQueueGc` | **VM-INTERNAL** | PriorityQueue under GC | `<no exception line>` | `` |
| `RSyncMethodJit` | **VM-INTERNAL** | ACC_SYNCHRONIZED after the JIT | `AssertionError: instance monitor lost updates: 2 != 480000` | `RSyncMethodJit.check` |
| `RTreeRangeGc` | **VM-INTERNAL** | TreeMap range views under GC | `AssertionError: RTreeRangeGc: tailSet: 0 elements, expected 300` | `RTreeRangeGc.check` |

## 6. The 20 loader rows, classified

The lane's own retirement scope is the `jdk/internal/loader/` half of its prefix
set: 20 bucket-A/B rows over 6 classes, re-derived from
`--dump-native-registry --explain-jdk-only` on this tree and reconciling exactly
with lane 0 §2.

**One of the 20 is ever dispatched by the corpus.** Unioned over the 132
per-vector `--jdk-only-report` files of the unarmed arm — the only honest
denominator, because the per-vector sinks are bounded:

```text
  jdk/internal/loader/BootLoader.loadLibrary(Ljava/lang/String;)V   native-won in 19 vectors
  the other 19 rows                                                 recorded in NO vector
```

That is precondition 4 of the retirement protocol failing for 19 of 20:
**a dispatch observed by the instrument that produced the improvement**. Arming
or retiring a row this corpus never dispatches changes nothing here, and the
green that follows carries no information — which is exactly the trap 146 of
Phase 2's 236 "retire-safe" candidates fell into.

So the 20 are classified, not retired, and the classification names the
instrument rather than the rows:

| rows | class | verdict |
|---|---|---|
| 9 | `jdk/internal/loader/URLClassPath` | bucket A, never dispatched by this corpus — precondition 4 unmet |
| 5 | `jdk/internal/loader/AbstractClassLoaderValue` | bucket A, no violation row in any vector — see the floor caveat below |
| 1 | `jdk/internal/loader/BootLoader.loadLibrary` | bucket A, dispatched in 19 vectors — the only row this corpus can price |
| 1 | `jdk/internal/loader/BootLoader.findResourceAsStream` | bucket A, never dispatched |
| 2 | `jdk/internal/loader/BuiltinClassLoader` | bucket A/B, never dispatched |
| 2 | `ClassLoaders$AppClassLoader` / `$PlatformClassLoader` `getResourceAsStream` | bucket B, inherited from `ClassLoader`, never dispatched |

**The union is a FLOOR, not a total, and one row says so.**
`AbstractClassLoaderValue.get` records `invocations=71` in a single-vector
`--dump-native-registry` run while the corpus's `native-shadows-bytecode` union
lists it nowhere. A row absent from the union is "this instrument did not see
it", never "it is not dispatched".

The lane page's instruction was to do these last, because *retiring a loader
native while the loader hierarchy does not link changes which failure you see
without changing whether it fails*. The hierarchy links now; what the rows still
lack is an instrument that reaches them. Naming that is the result.

## 7. Measured

One binary per arm on azure vm1, JDK 25.0.4+7. The control is the same worktree
at `3af66475b` without this wave, built first and kept beside the candidate;
every number below is a pair taken with those two binaries.

### The corpus

```text
  SUITE=all, CRATONVM_ARGS=--jdk-only          TIMEOUT   before      after
    unarmed                                       600    132 / 0     132 / 0
    CRATONVM_ENFORCE_NATIVE_SHADOW=all            180     25 / 107    42 / 90
```

**Unarmed is verdict-neutral**, which is the acceptance criterion; 132/132 is
also what the lane page recorded, so the control reproduces.

The armed arm is scored at 180 seconds and the control was re-run at 180 too —
a HANG cell is a claim about your timeout, and both binaries were held to the
same one. (The unarmed arm keeps 600 because lane 0 §5 records `RMapGcStress`
timing out below that and the corpus then silently reporting 131 vectors.)

**17 vectors flipped to PASS. NONE flipped the other way.**

```text
  RArrayStoreInterfaces  RArrayStoreLibrary  RArrayStoreTiers  RCanAccessReceiver
  RExceptions  RForNameGcStress  RImmutableFactoryTypes  RJdkBridge1  RJdkEnvMap
  RJdkForkJoin  RJdkFormatLocale  RJdkHello  RJdkStringCodePoints
  RJitArraycopyRefDeopt  RNioNoFollow  RStringBuilderContent  RStrings
```

**CORRECTION, 2026-09-11: `RJdkForkJoin` is in that list and should not be read
as closed by this wave.** Lane 5 root-caused the same vector independently and
found a real defect underneath it — `unsafe_natives_ext.rs` split every
`Unsafe.getAndSet*` on the RECEIVER'S SHAPE, and while the field arm was a
`compare_and_swap_field` retry loop the ARRAY arm was a bare
`get_array_element` + `set_array_element` pair, so a queued task could be
claimed twice and EXECUTED twice. See
[`lane-5-concurrent-thread-unsafe-RETIRED-20260910`](../../internal/retired/lane-5-concurrent-thread-unsafe-RETIRED-20260910.md) §2.

A divide-and-conquer sum is idempotent, so every ordinary ForkJoin assertion
passes over that race; `RJdkForkJoin.countedCompleter` is the one assertion in
the corpus that counts SIDE EFFECTS rather than reducing values, and it is
therefore RACY rather than deterministic. This wave removed the loader failure
that was this vector's FIRST failure and then observed a pass — which is a pass
of a racy assertion, not evidence that the race was gone. The fix for it is
lane 5's, landed separately.

The general form is the one this record argues for everywhere else: a
first-failure count cannot score a fix in a chain, and a single PASS of a
non-deterministic assertion is not a measurement. The other sixteen rows are
unaffected — none of them is a race — but the honest statement of +17 is "17
first failures removed", which is what the section below reports.

### The number that keeps +17 honest

The lane page's own rule is to report *"N first-failures removed, M new blockers
named"*, never just the delta. Re-running all 107 before-failures and all 90
after-failures with `run.sh`'s own command line and keeping the raw stderr:

```text
  107 failing before, 90 after
   17 stopped failing
   29 still fail, and FAIL SOMEWHERE ELSE   <- first failure removed, new blocker named
   61 fail at exactly the same place
```

So the wave removed **46** first failures and named **29** new blockers. Two
whole families are gone:

```text
                                 before  after
  NoClassDefFoundError              6      0     <- the BuiltinClassLoader family
  ServiceConfigurationError         5      0     <- the CLDR / ServiceLoader family
  AssertionError                   44     32
  NullPointerException             18     21     <- new blockers, further along
```

Four of the 29 moved rows are the `getName` tag showing its work without
changing a verdict — same assertion, correct name:

```text
  RJdkRecords      generic component type: java/lang/Object  ->  java.lang.Object
  RPermissionInit  java/security/Permission::serialVersionUID -> java.security.Permission::…
  RUnsafeArrayBase sun/misc/Unsafe                           ->  sun.misc.Unsafe
  RFileTimes       sun/nio/fs/UnixPath@…                     ->  sun.nio.fs.UnixPath@…
```

### The probes

```text
  apps/probes/L7ParallelCapableProbe.java   rows 2 and 3   FALSE -> true (armed)
  apps/probes/L7LoaderBootstrapProbe.java   rows 4 and 5   threw -> ok; rows 3-5 now
                                                           byte-identical to HotSpot
  apps/probes/L7UnnamedModuleSweep.java     10 of 10 on all three arms, before and after
  apps/probes/ClassNameSweep.java           85 rows: 0 differ unarmed,
                                                     9 differ armed -> 1 after the tag
  L7Scl2 (getSystemClassLoader, x6)         null on all six armed -> non-null on all six
```

### The whole probe tree, A/B between the two binaries

`scripts/jdk-only-phase2-battery.sh` is the wrong instrument here and the reason
is worth recording: it compares base-vs-armed for ONE binary, which is right for
a dial scope and useless for a `retired_shadow.rs` entry, because a
registration-time refusal is not reachable through
`CRATONVM_ENFORCE_NATIVE_SHADOW` at all — both of its columns would come from a
binary that already carries the change. The control for a retirement is the same
tree without it.

```text
  121 probes measured (4 excluded: javac needs --add-exports)
    1 moved, 0 worse
```

The one mover is `VtHandoffProbe`, and **its own control moved**: in the first
of the two runs the CONTROL binary scored `d(hs,A)=14` and the candidate 0; in
the second the control scored 0 and the candidate 10. Same binary, same probe,
opposite verdict — that is the known-flaky virtual-thread handoff counter the
operations page names, measuring its own noise floor rather than this wave.

### The retirement mechanics

```text
  4 refusals, 0 survivors
```

per `--jdk-only-report`'s `synthetic-native-registered` rows — the §6 trap
discharged. Three of the four are the one `registerAsParallelCapable` triple,
which is why this had to be a table entry rather than an edit to the site that
owns the slot.

### The gate cells

```text
  BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT   1883 -> 1887   (+4)
  BASELINE_SYNTHETIC_STUBS_MANAGEMENT      1894 -> 1898   (+4)
  BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK   1883 -> 1887   (+4)
  BASELINE_INTRINSICS                      read in the same runs, unchanged
```

CASE ONE, a relabel: total registrations did not move and this wave adds **zero
`register(` call sites**. All four rows are `Bridge -> SyntheticStub`, i.e. they
moved out of a kind that runs a native and into the kind `--jdk-only` refuses.

`native-builtins/tests/registrar_drift.rs` is the other gate cell this wave
touches, and it was **already red on `dev`** — see §2's tail. Fixed and
re-taken; 7 of 7 green.
