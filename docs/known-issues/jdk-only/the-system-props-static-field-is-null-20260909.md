# The null static fields "remove them all" hits, starting with `java.lang.System.props`

**2026-09-09.** The sequel to
[`the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909.md`](the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909.md),
one level up, found by that page's own §9 and fixed the same way.

---

## 1. The defect

`System.getProperty(String)` in the real JDK is one line:

```java
    public static String getProperty(String key) {
        checkKey(key);
        return props.getProperty(key);          // System.java:744
    }
```

`props` is a `private static Properties`, and the real `System.initPhase1()`
populates it. This VM registers a NATIVE `initPhase1` (real-JDK mode, at
`native-builtins/src/lib.rs`) that wires `System.out`/`err`/`in` and
`lineSeparator` and does not touch `props`. Its own doc comment lists what the
real method does and puts *"Sets up the system properties map (`System.props`)"*
first — the body does everything but that.

While the `System.getProperty` NATIVE answers, nothing is wrong. The moment any
real `System` bytecode runs, the field is null:

```text
NullPointerException: Cannot invoke "java.util.Properties.getProperty(String)"
  because "java.lang.System.props" is null
    at java/lang/System.getProperty(System.java:744)
    at RJdkHello.systemStreams(RJdkHello.java:42)
```

That is the **first line of the first vector** of the `--jdk-only` corpus under
`CRATONVM_ENFORCE_NATIVE_SHADOW=all`.

## 2. It is the same defect as the `Properties.map` one, one level up

The `Properties` cluster's root comment in `native-builtins/src/lib.rs` said the
first move was to make `System.getProperties()` return a real `Properties` —
*"real `<init>`, real `map`"*. That landed as `replace_real_map`. This is the
next field in the same chain, and the shape is identical: **a JDK object model
this VM keeps in Rust, with the JDK's own field left null because nothing had
ever read it.**

## 3. The fix

`native-builtins/src/lib.rs` gains `publish_real_system_props`, called from a
`--jdk-only` arm of the `initPhase1` registration
(`native_system_init_phase1_jdk_only`) and again from the `--jdk-only`
`getProperties` body.

* **Registration-time branch, one registration, two bodies** — the same shape
  the `getProperties` pair uses, and for the same reason: `NativeCallback` is a
  bare `fn` pointer that captures nothing and `NativeContext` exposes no policy
  accessor.
* **`--real-jdk` keeps the null field on purpose.** The singleton only acquires
  a real `map` on the `--jdk-only` path, so stamping it in compatible mode would
  publish a `Properties` real bytecode still cannot read — the NPE moved one
  frame, which is what this cluster produces every time a receiver is made
  half-real.
* **It publishes a POPULATED receiver or nothing.** `replace_real_map` returns
  the count it wrote and keeps the invariant that the map holds the whole
  snapshot or is ABSENT; a zero return leaves the field exactly as it was.
  Publishing an EMPTY one would be worse than the null: `getProperty` stops
  throwing and starts answering `null`, which is the 2026-07-14
  `InternalError: null property: java.home` regression from a third direction.
* **Failure at `initPhase1` is safe.** That method runs before most of the Java
  world exists; if the backing `ConcurrentHashMap` cannot be constructed yet
  nothing is written, which is exactly today's behaviour, and the `getProperties`
  body republishes on every call.

## 4. Measured

`apps/probes/SysPropsStaticProbe.java` is new. **It requires
`--add-opens java.base/java.lang=ALL-UNNAMED` and is MUTE without it** — run
plain, every row on both VMs is an `InaccessibleObjectException` and the probe
compares two exception strings, which do not even match, so it reads as eight
diffs that say nothing about the field. That is why it is not in the standard
sweep, which passes no extra launcher flags.

```text
                                        HotSpot   before   after
props is non-null                        true     false    true
props is a Properties            java.util.Properties  NPE  java.util.Properties
props size > 10                          true      NPE     true
props java.version present               true      NPE     true
props agrees with getProperty            true      NPE     true
props is the getProperties object        true     false    true
a write through the API is visible       yes       NPE     yes
a clear through the API is visible       null      NPE     null
```

`after` is byte-identical to HotSpot on all eight. Rows 6 to 8 are the ones that
matter beyond "not null": the field IS the object `getProperties()` hands out,
so a retirement that starts reading it sees the same map callers mutate rather
than a snapshot taken once at boot.

## 5. What it moves, and the next domino

`RJdkHello` under `CRATONVM_ENFORCE_NATIVE_SHADOW=all` stops failing on
`System.props` and fails one step later:

```text
CoderMalfunctionError: NullPointerException: Cannot invoke
  "jdk.internal.access.JavaLangAccess.uncheckedEncodeASCII(char[],int,byte[],int,int)"
  because "sun.nio.cs.UTF_8.JLA" is null
    at java/io/PrintStream.write(PrintStream.java:665)
```

Same family, third field. `sun.nio.cs.UTF_8.JLA` is
`SharedSecrets.getJavaLangAccess()` captured in a static initialiser, and this
VM's shim for that call (`get_or_build_jla_shim`) allocates a
`java/lang/System$1` and registers three of that interface's methods as natives.

**This section originally read that publishing the static would therefore only
trade the NPE for an `AbstractMethodError` on `uncheckedEncodeASCII`, and that
the method had to be implemented first. That was wrong** -- `java.lang.System$1`
is a REAL image class carrying bytecode for all 88 members, so there was nothing
to implement. §7 has the correction and what it bought. The wrong guess is left
standing here because one `javap` of the image settled it, and reading the image
before pricing the work is the transferable part.


### The corpus count does not move, and that is the finding

```text
CRATONVM_ENFORCE_NATIVE_SHADOW=all, whole corpus
  before   5 passed, 127 failed
  after    5 passed, 127 failed
```

**A first-failure metric cannot score a fix in a CHAIN.** Every vector reports
the first thing that kills it, so removing failure N just exposes failure N+1
and the count is unchanged. `RJdkHello` is the proof: it moved off
`System.props` and onto `UTF_8.JLA`, one row of progress that the total cannot
express. Anyone using the `all` pass count to steer this cluster will read real
work as zero.

What DOES steer it is the distribution of first failures. Sixteen vectors,
sampled across the corpus, on the binary this page produces:

```text
  9   a null static holding a jdk.internal access object
        sun.nio.cs.UTF_8.JLA                  RCollections RJdkCollections
                                              RJdkRecords RImmutableFactoryTypes
        jdk.internal.constant.ConstantUtils.JLA  RAtomicArray RBlockingQueue
                                                 RVarHandleAccess
        SharedSecrets.langReflectAccess       RStrings RJdkNio
  4   AssertionError -- a real behavioural difference, not a null field
        RExceptions RReflect RJdkViews RJdkLambdas
  2   AbstractMethodError                     RCrypto RJdkSecurity
  1   NullPointerException, other             RJdkModule
```

**Nine of sixteen are one family**, and it is this page's family: a JDK-internal
object this VM keeps in Rust, captured by real bytecode into a static the VM
never fills. `System.props` was the same shape and so is every row of that first
group.

It is not the same FIX, and the difference is the whole reason this page stops
here. `System.props` needed a `Properties` the VM already had. The `JLA` statics
need `jdk.internal.access.JavaLangAccess`, and this VM's stand-in for it
(`get_or_build_jla_shim`, a synthetic `java/lang/System$1`) implements only the
handful of methods boot calls. Publishing the static without implementing
`uncheckedEncodeASCII` and its siblings trades the NPE for an
`AbstractMethodError` one frame later -- the half-real receiver this cluster
keeps producing, and the reason the first cut of `replace_real_map` was worse
than the bug it fixed.

So the measured next move is: **implement the `JavaLangAccess` surface real
bytecode actually calls, then publish the statics** -- in that order, and with
the four `AssertionError` vectors kept separate, because those are behavioural
differences that no null field explains.

## 7. The `SharedSecrets` access objects, and 5 of 132 becoming 24

§5 named `JavaLangAccess` as the measured next move and said publishing the
static alone would only trade the NPE for an `AbstractMethodError`, because this
VM's stand-in implements a handful of the interface. **That was wrong, and
checking the image is what showed it:**

```text
$ javap -p 'java.lang.System$1'
class java.lang.System$1 implements jdk.internal.access.JavaLangAccess {
  ...  88 members, all with bytecode
  public int uncheckedEncodeASCII(char[], int, byte[], int, int);
```

`java.lang.System$1` is a class the JDK image actually contains, and
`java.lang.reflect.ReflectAccess` is the same for `JavaLangReflectAccess`.
`try_alloc_concurrent_synthetic` resolves the real class id when the image has
it, so the object this VM was already allocating for its `getJavaLangAccess`
shim is a GENUINE `System$1` whose every method has a real body. There was
nothing to implement. The object worked and was simply never handed to the field
the JDK reads it from.

The three natives registered on `System$1` (`currentCarrierThread`,
`currentThread0`, `layers`) are not evidence against that: they exist to WIN in
compatible mode, where the kind is discarded and a registered native always
beats bytecode. Under `--jdk-only` they yield — and yielding is correct once
there is something to yield to.

### Ordering is load-bearing, and it cost a build to learn

Published AFTER the `initPhase1` body, the fix half-worked:

```text
                              publish after body   publish before body
jdk.internal.constant.ConstantUtils.JLA   cleared          cleared
sun.nio.cs.UTF_8.JLA                      STILL NULL       cleared
```

`native_system_init_phase1` installs charsets on the system streams, which
initialises `sun.nio.cs.UTF_8`, whose `<clinit>` **captures**
`SharedSecrets.getJavaLangAccess()` into a static of its own. Set the field
after that and `UTF_8.JLA` still holds the null it captured on the way past;
`ConstantUtils` initialises later and so was already fine. A field the JDK
copies has to be right before the copy is taken, and only the failing half of
the pair says which side of the body you are on.

### The publisher is a table, and every entry names its vector

`SHARED_SECRETS_TO_PUBLISH` in `native-builtins/src/lib.rs`. `SharedSecrets` has
around thirty of these fields; two are here, each annotated with the corpus
vector that named it, so the list stays a record of what was needed rather than
a guess at what might be.

### Measured

```text
probe tree, unarmed --jdk-only, control = the same tree without this change
  115 probes, ZERO moved
--jdk-only corpus     132 passed, 0 failed
SUITE=all             132 passed, 0 failed
```

And the arm this whole page exists for:

```text
CRATONVM_ENFORCE_NATIVE_SHADOW=all, whole corpus
  before the props fix       5 passed, 127 failed
  after the props fix        5 passed, 127 failed
  after SharedSecrets       24 passed, 108 failed
```

§5 said a first-failure count cannot score a fix in a chain. It also cannot hide
one once the chain is BROKEN: nineteen vectors did not need anything else.

### What the survivors fail on now, and why that is the result

```text
RCollections, RImmutableFactoryTypes   PASS
RJdkHello, RStrings    ServiceConfigurationError: Locale provider adapter "CLDR"
RJdkCollections        AssertionError: TreeMap order
RJdkRecords            AssertionError: generic component type: java/lang/Object
RAtomicArray           AssertionError: guarded increments lost: expected 80000 got 1..
RJdkNio                IllegalStateException: Directory stream is closed
```

Not one is a null field. Every sampled vector has moved from *"the VM never
filled a JDK field"* to a **real semantic gap** — a missing locale provider, a
map that iterates in the wrong order, atomics that lose increments. Those are
implementation work, individually harder and individually smaller, and they are
what the `--jdk-only` goal actually consists of once the object-model
scaffolding stops being the answer to every question.

## 9. All 108 classified, and `VM.savedProps` is the third field

§7 left 108 failures and a claim that they were "real semantic gaps". That was
an inference from six sampled vectors. **Every one of the 108 was then run
directly and classified by the exception the VM actually reported**, because the
harness's per-vector message is the last line of stderr and for 44 of them that
line is the dial's own door census -- a message that says nothing about the
cause.

```text
 41  AssertionError                        a real behavioural difference
 22  NullPointerException                  of which 12 are a null java.lang.Module
 11  IllegalStateException                 ALL of them "Not yet initialized"
 10  <no exception line>                   an output diff, not a crash
  6  IllegalArgumentException
  4  InternalError
  3  ServiceConfigurationError
  2  each: UnsatisfiedLinkError, RuntimeException, IllegalStateException,
        AbstractMethodError
  1  each: SocketException, ClassNotFoundException, ArithmeticException,
        FileNotFoundException
```

So §7 was right that the null-field era was over as the DOMINANT cause and
wrong that it was over: 23 of 108 are still one, in two families of 11 and 12.

### `jdk.internal.misc.VM.savedProps`, and it throws

The eleven are one field, and it is the same shape as `System.props` and the
`SharedSecrets` pair -- with one difference that makes it worse:

```java
    public static String getSavedProperty(String key) {
        if (savedProps == null)
            throw new IllegalStateException("Not yet initialized");
```

The other two answer `null`. This one THROWS, so its absence is not a wrong
answer somewhere downstream, it is an `ExceptionInInitializerError` out of
whichever `<clinit>` asks first:

```text
<clinit> failed, class jdk/internal/loader/ClassLoaders
  caused by IllegalStateException: Not yet initialized
    at jdk/internal/misc/VM.getSavedProperty(VM.java:211)
    at jdk/internal/loader/ClassLoaders.<clinit>(ClassLoaders.java:66)
```

`publish_vm_saved_props` fills it with a real `java/util/HashMap`, built by
`new_object_initialized` and filled through its own `put` bytecode -- real code
calls `get` on this, so a carrier will not do. Same absent-or-complete
invariant as the rest of the cluster: a partially filled map would stop
throwing and start answering `null`, and `getSavedProperty("java.home")`
answering null is the 2026-07-14 `InternalError: null property: java.home`
regression yet again.

### Where the eleven go next, and why this increment stops here

```text
RJdkForkJoin      AssertionError: CountedCompleter leaves: 128     <- a real gap
RLangPackages     NoClassDefFoundError: jdk/internal/loader/BuiltinClassLoader
RBufferPoolCount  NoClassDefFoundError: jdk/internal/loader/BuiltinClassLoader
RLoaderIdentity   NoClassDefFoundError: jdk/internal/loader/BuiltinClassLoader
```

`ClassLoaders.<clinit>` now gets eleven lines further -- from line 66 to line
77 -- and dies constructing the builtin loader hierarchy. That is not another
null field: `jdk/internal/loader/BuiltinClassLoader` is in the image and this VM
cannot load or link it. **Structural class-loading work, and outside what a
field publish can reach**, so it is recorded rather than attempted.

The remaining 12-vector family is a null `java.lang.Module` --
`ClassLoader.getUnnamedModule()` answering null through
`ClassLoader.postDefineClass` -> `NamedPackage.<init>`. That one may still be
field-shaped (`ClassLoader.unnamedModule`, set by the real `ClassLoader`
constructor) and is the next thing to price, with the same warning §5 earned:
read the image before assuming the carrier is missing.

### Measured, and the count barely moves

```text
probe tree, control = the same tree without this change
  115 probes, 1 moved: JdkOnlyPlatformProbe +2, the known-flaky handoff count
  (delta 0, -2, 0, 0, +2 across the five A/Bs of this lane -- see
   ../../contributing/jdk-only-lane-operations.md)
--jdk-only corpus     132 passed, 0 failed
SUITE=all             132 passed, 0 failed
CRATONVM_ENFORCE_NATIVE_SHADOW=all   24 passed -> 25 passed of 132
```

**Eleven vectors cleared their first failure and the total moved by one.** Both
facts are real and neither is the other's correction: ten of the eleven walked
into the `BuiltinClassLoader` link failure above, which no field publish
reaches. §5's rule holds in the direction that flatters nobody -- a first-failure
count cannot score a fix in a chain, and that is as true of a fix worth having
as of one that is not.

The number to read for this increment is 11 first-failures removed and one new
structural blocker NAMED, not +1.

## 11. `Class.getModule` is a shadow the contract cannot remedy

The 12-vector `Module` family, and the only increment of this lane that changes
a KIND rather than filling a field.

### The letter and the substance disagree

`Class.getModule()` is not `ACC_NATIVE`. Its body is `return module;`, so a
native registered in front of it shadows real bytecode, §1.4 applies by the
letter, and §1.4's remedy is to yield. **The remedy cannot work.**
`java.lang.Class.module` is `private transient Module` and **no Java code
writes it** -- a real JVM populates it at class-definition time through
`Module.defineModule0`. Yield and the field answers null:

```text
NullPointerException: Cannot invoke "java.lang.Module.isNamed()"
  because "module" is null
    at java/lang/ClassLoader.postDefineClass(ClassLoader.java:871)
    at java/lang/NamedPackage.<init>(NamedPackage.java:48)
```

Twelve of the 108 are that null, spelled `module`, `callerModule` or
`thisModule` a frame or two along.

So this is a native doing a VM's job, which is the case §1.4's
reviewed-`Intrinsic` exception exists for. The tag was `bridge` with
`kind_stated: false` -- **ambient, never chosen** -- so this states a decision
rather than overturning one.

### The review, because `Intrinsic` is a claim

An `Intrinsic` claims semantics-preserving, and the only way to earn that is to
compare answers with the oracle. `apps/probes/ClassModuleSweep.java` is 32 rows
against HotSpot 25.0.3+9: module names for `java.base`, a platform module and
the unnamed module; primitives, arrays of primitives and of references, nested,
anonymous and lambda classes; Module IDENTITY within one VM, which the JDK
depends on because `Module` does not override `equals`; and
`isNamed`/`getName`/`getClassLoader`/`getDescriptor`/`isOpen`/`isExported`/
`canRead`/`getLayer`.

```text
31 of 32 rows byte-identical to HotSpot.
```

The one deviation is recorded and NOT fixed here:

```text
32 layer of unnamed is null    HotSpot true, this VM false
```

`Module.getLayer()` on the UNNAMED module should be null. That is a defect in
`getLayer`, not in `getModule`, and the tag does not freeze it: the sweep is
checked in, so the row goes red the day it is fixed or the day this answer
drifts. Nothing else on `java/lang/Module` moves -- the claim is about one
triple whose backing field no Java code can fill.

### The mechanism was verified, not assumed

The dial declines exactly one kind:

```rust
let strict_bridge = policy.is_jdk_only() && kind == NativeKind::Bridge;
```

`Intrinsic` is exempt at all nine doors, and the shadow census exempts it by
construction. So the re-tag both survives the `all` arm and truthfully leaves
the census -- it stops being counted as a shadow because it stops being one.

### Measured

```text
probe tree, control = the same tree without this change
  115 probes, 1 moved: VtHandoffProbe -4, the OTHER known-flaky handoff
  counter named in ../../contributing/jdk-only-lane-operations.md
--jdk-only corpus     132 passed, 0 failed
SUITE=all             132 passed, 0 failed
```

and the family itself, under `CRATONVM_ENFORCE_NATIVE_SHADOW=all`:

```text
RLoaderChurnDefine   Module null  ->  PASS
RJdkDefineClass      Module null  ->  URLStreamHandler NPE
RJdkHidden           Module null  ->  URLStreamHandler NPE
RFsSingleton         Module null  ->  ServiceConfigurationError, FileSystemProvider
RJdkNet              Module null  ->  ServiceConfigurationError, InetAddressResolver
RJdkServices         Module null  ->  NoClassDefFoundError, BuiltinClassLoader
```

`scripts/baselines/jdk-only-kind-map-25-linux.tsv` gains one hand-amended row,
`bridge -> intrinsic` with `kind_stated 0 -> 1`, carrying this rationale. The
gate's own header allows exactly that movement and calls it adjudication.

```text
CRATONVM_ENFORCE_NATIVE_SHADOW=all, whole corpus   25 passed -> 26 passed
```

Twelve first-failures removed, one net pass. Same arithmetic as §9 and the same
reason: five of the twelve walked into the `URLStreamHandler`,
`FileSystemProvider`/`InetAddressResolver` and `BuiltinClassLoader` gaps listed
above. **The count is not the deliverable in a chain** -- what this increment
bought is twelve vectors moved off a null field that the contract's own remedy
could never have filled, and the shadow census losing a row it should never have
carried.

### Where the lane leaves the arm

```text
   5 passed   the wave that opened this record
  24 passed   + SharedSecrets
  25 passed   + VM.savedProps
  26 passed   + the Class.getModule review
```

and 106 remaining, whose composition is the useful part: 41 `AssertionError`s
that are individual semantic gaps, the `BuiltinClassLoader` link failure, the
service-provider families (`FileSystemProvider`, `InetAddressResolver`, CLDR),
`URLStreamHandler`, and a long singleton tail. Every one of those is
implementation work with a name, which is not where this record started.

## 12. What this does NOT claim

* Not that the `java/lang/System` property natives are retirable. They are not
  in any table, and retiring them needs a problem this change does not solve:
  real `System.setProperty` bytecode writes `props.map` and the VM's own Rust
  property store — which `NativeContext::get_system_property` serves to the rest
  of the VM — would not hear about it. The store has to become a cache of the
  object rather than the authority first.
* Not that `initPhase1` is now faithful. It publishes one more field of the
  several the real method sets.
* Nothing about `--real-jdk`, which this change deliberately does not reach.
