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

## 8. What this does NOT claim

* Not that the `java/lang/System` property natives are retirable. They are not
  in any table, and retiring them needs a problem this change does not solve:
  real `System.setProperty` bytecode writes `props.map` and the VM's own Rust
  property store — which `NativeContext::get_system_property` serves to the rest
  of the VM — would not hear about it. The store has to become a cache of the
  object rather than the authority first.
* Not that `initPhase1` is now faithful. It publishes one more field of the
  several the real method sets.
* Nothing about `--real-jdk`, which this change deliberately does not reach.
