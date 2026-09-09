# `java.lang.System.props` is null, and it is the first thing "remove them all" hits

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
VM's shim for that call (`get_or_build_jla_shim`, a synthetic
`java/lang/System$1`) implements only the handful of `JavaLangAccess` methods
boot needs. So this one is NOT a one-field fix: publishing the static is easy
and would only trade the NPE for an `AbstractMethodError` on
`uncheckedEncodeASCII`, which has to be implemented first. **Recorded as the
next move rather than attempted here.**


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

## 6. What this does NOT claim

* Not that the `java/lang/System` property natives are retirable. They are not
  in any table, and retiring them needs a problem this change does not solve:
  real `System.setProperty` bytecode writes `props.map` and the VM's own Rust
  property store — which `NativeContext::get_system_property` serves to the rest
  of the VM — would not hear about it. The store has to become a cache of the
  object rather than the authority first.
* Not that `initPhase1` is now faithful. It publishes one more field of the
  several the real method sets.
* Nothing about `--real-jdk`, which this change deliberately does not reach.
