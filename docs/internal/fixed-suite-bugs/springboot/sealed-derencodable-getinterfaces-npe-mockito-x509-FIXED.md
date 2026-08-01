# Mocking `X509Certificate` crashes: `Class.getInterfaces()` NPEs (`"rd" is null`) while ByteBuddy walks the sealed `DEREncodable` hierarchy — FIXED

**Status: fixed 2026-08-01** (found 2026-07-31).
Branch `fix/springboot-derencodable-sealed-20260801`, worktree
`C:\craton\CratonVM-derencodable-20260801`, binary
`cratonvm-derenc-fix.exe`.

## Original symptom

Every Mockito `mock(X509Certificate.class)` (directly, or transitively via a
parameterized test's argument-source constructing a mock certificate) failed
with:

```
org.mockito.exceptions.base.MockitoException:
Mockito cannot mock this class: class java.security.cert.X509Certificate.
...
Underlying exception : org.mockito.exceptions.base.MockitoException: Could not modify all classes
[class java.lang.Object, class java.security.cert.Certificate, interface java.security.cert.X509Extension,
 interface java.io.Serializable, interface java.security.DEREncodable, class java.security.cert.X509Certificate]
     net.bytebuddy.TypeCache.findOrInsert(...)
   Caused by: java.lang.IllegalStateException: Byte Buddy could not instrument all classes within the mock's type hierarchy
     org.mockito.internal.creation.bytebuddy.InlineBytecodeGenerator.triggerRetransformation(...)
   Caused by: java.lang.NullPointerException: Cannot read field "interfaces" because "rd" is null
     java.lang.Class.getInterfaces(Class.java:1217)
     java.lang.Class.isDirectSubType(Class.java:4082)
     java.lang.Class.lambda$getPermittedSubclasses$0(Class.java:4071)
     java.lang.Class.getPermittedSubclasses(Class.java:4071)
     java.lang.Class.isSealed(Class.java:4112)
     net.bytebuddy.utility.Invoker$Dispatcher.invoke(Unknown Source)
     ...
     net.bytebuddy.description.type.TypeDescription$ForLoadedType.isSealed(TypeDescription.java:9249)
     net.bytebuddy.dynamic.scaffold.InstrumentedType$Factory$Default$1.represent(InstrumentedType.java:476)
     net.bytebuddy.ByteBuddy.redefine(ByteBuddy.java:1001)
```

Reproduced on the pre-fix binary, exactly as originally reported:

| Module | Class | Before | After |
|---|---|---|---|
| `core/spring-boot` | `org.springframework.boot.ssl.pem.PemSslStoreTests` | `tests=5 failed=3` | **`tests=5 failed=0`** |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.ssl.CertificateMatcherTests` | `tests=0 failed=0 containersFailed=4` | **`tests=24 failed=0 containersFailed=0`** |

## Root cause

Two independent defects, chained. The first produces a bad value; the second
decides how catastrophically that value fails.

### 1. A bootstrap-loaded sealed class resolved almost none of its permitted subclasses

JDK 25 introduced `java.security.DEREncodable` as a genuine **sealed**
interface (the PEM-encoding JEP) that `X509Certificate` implements. Real
HotSpot 25.0.3:

```
DEREncodable.class.getPermittedSubclasses() ==
  [AsymmetricKey, KeyPair, PKCS8EncodedKeySpec, X509EncodedKeySpec,
   EncryptedPrivateKeyInfo, X509Certificate, X509CRL, PEMRecord]
```

`native_class_get_permitted_subclasses`
(`native-builtins/src/lang_class.rs`) reads those eight names straight from the
classfile's `PermittedSubclasses` attribute — always correct — and resolves
each through `resolve_nestmate_via_defining_loader`. The
WEBCLIENTEXT-SEALED-20260717 fix made that helper resolve **actively**, by
driving the sealed class's own `ClassLoader.loadClass`. But that only applies
inside the `loader_obj` arm, and a class defined by the **bootstrap** loader
has no Java-level `ClassLoader` object at all —
`native_class_get_class_loader` returns null for every `java/`, `javax/`,
`jdk/`, `sun/`, `com/sun/` name. So every JDK-owned sealed class fell straight
through to `ctx.class_id_by_name`, a **passive** cache probe that never
triggers classloading, and reported only the permitted subclasses some earlier
code happened to have loaded already.

Measured on the pre-fix binary (`probes/SealedDerEncodableProbe.java`, the raw
native read reflectively, before the public wrapper's filter):

```
RAW0 java.security.DEREncodable n=8
  [<NULL-SLOT>, <NULL-SLOT>, <NULL-SLOT>, <NULL-SLOT>, <NULL-SLOT>,
   java.security.cert.X509Certificate, <NULL-SLOT>, <NULL-SLOT>]
```

One entry out of eight, purely because `X509Certificate` was already loaded.
Re-running the same probe *after* touching all eight classes returned all
eight — the passive-lookup signature.

### 2. A warmed inline cache ran an instance callee with `this == null`

Real JDK bytecode then dereferences every element without a null guard,
because HotSpot's own `getPermittedSubclasses0` cannot produce a null element:

```java
subClasses = Arrays.stream(subClasses).filter(this::isDirectSubType)...

private boolean isDirectSubType(Class<?> c) {
    if (isInterface()) {
        for (Class<?> i : c.getInterfaces(/* cloneArray */ false)) {   // Class.java:4082
```

`c.getInterfaces(boolean)` is **private** — an `invokespecial`. JVMS §6.5
requires `invokevirtual`/`invokespecial`/`invokeinterface` to raise
`NullPointerException` when `objectref` is null, *before* the callee frame
exists. `execute_invokevirtual_cached`'s `VirtualBytecode` arm has always
deferred a `Value::Object(None)` receiver to the slow path (which owns both
the canonical NPE and the deliberate null-tolerant shims). Its `Bytecode` arm
— which serves `invokespecial`, i.e. every private/super call — and its
`Native` arm never got the same guard: they popped the null straight into
`args[0]` and pushed the callee frame (or invoked the registered native)
anyway.

Measured on the pre-fix binary (`probes/NullReceiverInvokeProbe.java`): the
**first** `Impl.callPrivateOn(null)` throws NPE correctly (slow path), and
after 50 000 warming calls the **same site** returns `3` — the private
method's body, executed with a null `this`.

Chaining the two explains the reported stack frame-for-frame:

1. `getPermittedSubclasses0()` hands back an array with seven holes.
2. `isDirectSubType(null)` reaches `c.getInterfaces(false)`. The eight
   iterations of the `anyMatch` lambda warm that call site, so by the time a
   hole arrives the cached `Bytecode` arm answers it.
3. The frame for `getInterfaces(boolean)` is pushed with `this == null`.
4. Its first act is `reflectionData()`, a **registered native**
   (`native_class_reflection_data`, part of the WildFly
   `Class$ReflectionData` livelock fix). Handed a null `args[0]`, it returns
   `Value::Object(None)` — a null RETURN, where the JDK contract is that
   `reflectionData()` never returns null.
5. `Class<?>[] interfaces = rd.interfaces;` — `Class.java:1217` exactly —
   throws `NullPointerException: Cannot read field "interfaces" because "rd"
   is null`.

That also explains the milder symptom the original report noted: calling
`DEREncodable.class.getPermittedSubclasses()` *directly*, in a cold process,
returned an empty array `[]` rather than crashing. On the cold path the same
null receiver reaches the slow dispatcher's deliberate null-tolerant shim for
`java/lang/Class.getInterfaces` (added for Spring's
`GenericConversionService$Converters.getClassHierarchy`), which answers an
empty `Class[]`; every entry then fails `isDirectSubType` and the wrapper
filters the whole array away. Cold and warm answers for the same call
disagreed about whether the program had already failed — that divergence, not
a mangled `args` slice, is the real second defect. (The original report's
hypothesis that `native_class_reflection_data` "can't identify a receiver
object in the call's `args` slice" was right about *which line returns null*
and wrong about *why*: the receiver slot genuinely held null, because the
caller put it there.)

## Fix

`native-builtins/src/lang_class.rs`:

- `resolve_nestmate_via_defining_loader` now falls back to an **active**
  `ctx.load_class(name)` after the passive `class_id_by_name` probe misses,
  guarded by `would_fabricate_synthetic_stub` so a name with no real class
  file behind it still answers `None` instead of minting a stub. This is the
  same active resolution the loader arm already got from
  `ClassLoader.loadClass`, and it is equally re-entrancy-safe (an ordinary
  native-method call boundary, exactly like `Class.forName`'s native).
- `native_class_get_permitted_subclasses` now **compacts** its result: a name
  that genuinely cannot be resolved is dropped rather than left as a hole, so
  the JDK's "every element is a real `Class`" contract holds by construction.
- Both that native and `native_class_get_nest_members` now accumulate
  `ClassId`s (GC-immune indices) rather than `ObjectRef`s across the
  resolution loop, and pin the destination array across `get_class_mirror`.
  Resolution can now allocate and therefore collect, so the previous
  `Vec<ObjectRef>` was the Family-1 stale-native-local shape.

`vm/src/runtime/interpreter/invoke.rs`:

- `execute_invokevirtual_cached`'s `Bytecode` arm (skipped for `is_static`)
  and `Native` arm now return `CachedCallResult::CacheMiss` when the receiver
  slot holds `Value::Object(None)`, matching the `VirtualBytecode` arm. The
  slow path then decides — canonical NPE, or one of its deliberate
  null-tolerant shims — so a warm call site and a cold one always agree. This
  function only ever serves instance invokes (`invokestatic` goes to
  `execute_invokestatic_cached`), so the receiver is always at
  `peek_at(num_params)`.

## Validation

Binaries: `cratonvm-derenc-base.exe` (pre-fix) and `cratonvm-derenc-fix.exe`
(post-fix), both release builds of the same worktree. JDK 25.0.3 at
`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`.

**Sealed metadata vs. real HotSpot** — `probes/SealedDerEncodableProbe.java`.
Post-fix `getPermittedSubclasses0()` on `java.security.DEREncodable`, with
nothing preloaded, returns all 8 names in HotSpot's order, and the public
wrapper survives its own `isDirectSubType` filter at `n=8`. Byte-for-byte
identical to the real-HotSpot run of the same probe.

**Null-receiver conformance** — `probes/NullReceiverInvokeProbe.java`. After
50 000 warming calls, `invokespecial` / `invokevirtual` / `invokeinterface` on
a null receiver all throw `NullPointerException`, JIT on **and** `--nojit`
(pre-fix, warm `invokespecial` returned the callee's value).

**The two affected Spring Boot classes**, via
`apps/spring-boot-suite-runner/run-single-class.ps1`:

| Class | JIT on | `--nojit` |
|---|---|---|
| `PemSslStoreTests` | `tests=5 failed=0 aborted=0 skipped=0 containersFailed=0` | same |
| `CertificateMatcherTests` | `tests=24 failed=0 aborted=0 skipped=0 containersFailed=0` | same |

`CertificateMatcherTests`'s 24 tests span RSA, DSA, Ed25519, Ed448, P-256 and
P-521 key generation, so this also reconfirms the earlier
`springboot-certificatematchertests-dsa-keypairgenerator-FIXED.md` fix, which
this bug had been masking (`asCertificate` calls
`Mockito.mock(X509Certificate.class)` before any DSA key pair is touched).

**Spring Boot suite regression, A/B on the same 247-class slice** (every 8th
class of the 1975-class `all-tests.tsv`, so every module is represented),
`-Parallel 4 -TimeoutSec 300`, pre-fix binary vs. post-fix binary:

| | PASS | FAIL | EMPTY | HANG |
|---|---:|---:|---:|---:|
| Arm A (`cratonvm-derenc-base`) | 234 | 7 | 4 | 2 |
| Arm B (`cratonvm-derenc-fix`) | **235** | 8 | 4 | 0 |

**Zero PASS → non-PASS.** The only two status changes are the two classes that
`HANG`ed in arm A — the shared host was heavily contended during that arm:

- `DataJdbcRepositoriesAutoConfigurationTests` `HANG → PASS`
- `GraphQlWebMvcAutoConfigurationTests` `HANG → FAIL`

Re-run in isolation, both binaries answer that second one identically
(`tests=17 failed=11 containersFailed=0`), so it is a pre-existing failure
surfacing through a load-induced timeout, not a regression. This matters
because fix (2) sits on the interpreter's cached-invoke path, which every
`invokespecial` in the VM traverses.

**Rust regression tests** (new, both in `vm/tests/`):

- `sealed_bootstrap_permitted_subclasses.rs` — asserts
  `getPermittedSubclasses0()` on the bootstrap-loaded `DEREncodable` has
  `nulls=0` and `n=8`, reading the raw native reflectively so a regression
  cannot be laundered into an empty array by the public wrapper. Skips on a
  pre-25 JDK.
- `null_receiver_cached_invoke.rs` — asserts all three instance-invoke opcodes
  NPE on a null receiver **after** warming, JIT on and off. A cold-only test
  passed throughout the entire lifetime of the bug.

Both were mutation-checked against the pre-fix binary
(`CRATONVM_BIN=cratonvm-derenc-base.exe`), and both fail there —
`sealed_bootstrap` on `RAW0 n=8 nulls=8`, `null_receiver_cached` on both the
interpreted and JIT variants. That check is not ceremony: the first draft of
the nest-member assertions shadowed two locals, javac errored, `compile_probe`
returned `None`, and the test reported `ok` in 0.8 s against a binary that
reproduces the bug perfectly. `compile_probe` now panics on a compile error
and only skips when `javac` is genuinely absent.

## Notes for the next reader

- The slow dispatcher's null-tolerant shim for `java/lang/Class.getInterfaces`
  (`vm/src/runtime/interpreter/invoke.rs`, the `Round 63` block) is still
  there and still returns an empty `Class[]` for `null.getInterfaces()`. It is
  a deliberate, load-bearing Spring accommodation and is left untouched; the
  fix above makes the *cached* path route to it consistently instead of
  diverging from it. `probes/NullReceiverInvokeProbe.java` reports that one
  site as `NO-THROW <-- WRONG`, which is expected.
- The `getPermittedSubclasses` and `getNestMembers` natives share
  `resolve_nestmate_via_defining_loader`, so the bootstrap-loader fix applies
  to nest members too. `Class.getNestMembers()` on a JDK-owned nest host had
  the same passive-lookup hole, and it was worse there — measured with
  `probes/NestMembersBootstrapProbe.java`, in a process that has not touched
  any nest member:

  | Nest host | Pre-fix | Post-fix | Real HotSpot 25.0.3 |
  |---|---:|---:|---:|
  | `java.lang.Character` | 1 | **5** | 5 |
  | `java.lang.ProcessBuilder` | 1 | **12** | 12 |
  | `java.util.concurrent.ConcurrentHashMap` | 16 | **54** | 54 |
  | `java.util.Map` | 2 | 2 | 2 |
  | `java.security.KeyPair` | 1 | 1 | 1 |

  Post-fix matches HotSpot on all five. `getNestMembers()` backs
  `Lookup.defineHiddenClass` nestmate checks and private-member access
  reflection, so this was a silent correctness gap of its own, not just a
  cosmetic count.
