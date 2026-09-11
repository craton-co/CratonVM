# The type-variable scope walk never climbed — its loop body shadowed the binding it advanced, and 20 Spring Framework classes were the bill

| | |
|---|---|
| **Status** | **FIXED 2026-09-10.** Landed on `dev` as `bb92afbb4` out of the Spring **Boot** lane (`claude/sb-sixfail-20260910`); found independently and at the same time from the Spring **Framework** lane (`claude/spring-residuals-20260910`), which contributes this record and the verification below. |
| **Scope** | `native-builtins/src/generics.rs`, the `TypeSig::TypeVar` arm — every type-variable USE resolved through core reflection. |
| **Population** | 20 Spring Framework classes failing with `IllegalArgumentException: Could not create type` or an `AssertionFailedError`. |
| **Host** | Azure `20.80.105.49`, real JDK 25 (`/data/toolchain/jdk-25`), binary `cratonvm-springfix-20260910`. |

## Found twice the same day, from two different suites

Two sessions arrived at this line within hours of each other, from workloads
with nothing in common but ByteBuddy:

| lane | symptom | the type variable | its wrong declaration |
|---|---|---|---|
| Spring **Boot** (`bb92afbb4`, landed) | `Mockito.mock(MockRepository.class)` → `Cannot resolve T` | `T` in `QuerydslPredicateExecutor`'s `<S extends T, R> R findBy(...)` | the method `findBy` instead of the interface |
| Spring **Framework** (this page) | AssertJ `SoftAssertions` → `Could not create type` … `Cannot resolve ACTUAL` | `SELF`/`ACTUAL` in `AbstractObjectAssert`'s `<T> SELF returns(T, Function<ACTUAL,T>)` | the method `returns` instead of the class |

Both reduce to the same sentence: **a method-level signature that names its
enclosing type's variable**. That shape is common enough to appear in two
unrelated libraries on the same afternoon, which is the best available measure
of the blast radius — and a reason to treat the 20 Spring Framework classes and
the 3 Spring Boot GraphQL classes below as a lower bound on what it cost.

The two fixes are structurally the same rewrite (`pin_base`, a `resolved`
binding, re-pin on each climb, one `unpin_native_roots` at the foot). `dev`
carries the Spring Boot one; this branch's own copy was dropped into it on
merge. The verification in this page was then re-run against the **landed**
code, not against the copy it replaced.

## The one line

`generics.rs` resolves a type-variable *use* (`SELF` inside
`AbstractObjectAssert.returns`'s signature) by walking the enclosing generic
declarations — the method, then its declaring class, then the outer classes —
and returning the real `TypeVariable` that declaration hands out from
`getTypeParameters()`. The walk was written correctly and commented at length.
It never ran past its first step:

```rust
let mut scope = decl;
let scope_pin = ctx.pin_native_root(scope);
for _ in 0..16 {
    let mut scope = ctx.read_native_pin(scope_pin, scope);   // <-- SHADOW
    if let Some(real) = resolve_declared_type_variable(ctx, scope, name) { ... }
    // ...
    match next {
        Some(enclosing) => scope = enclosing,                // writes the SHADOW
        None => break,
    }
}
```

The `let mut scope` inside the body shadows the binding it was meant to
refresh. `scope = enclosing` at the foot writes the shadow, the shadow dies
with the iteration, and the next pass re-reads the same starting `decl` off
the pin. Sixteen passes, one scope. Every variable belonging to an enclosing
scope fell through to the synthetic stand-in below the loop — whose
`getGenericDeclaration()` is the *immediate* declaration and whose bound
defaults to `Object`.

**Why nothing complained:** the crate's lint set passes `--allow=unused_mut`,
so the now-dead `mut` on the outer binding said nothing, and no other code in
the function reads the outer `scope` after the loop, so there was no type error
either. The comment block above the loop describes, in detail and correctly,
behaviour the code did not have.

## What it looked like from the outside

`org.assertj.core.api.AbstractObjectAssert` declares

```java
public <T> SELF returns(T expected, Function<ACTUAL, T> from)
```

`T` is declared by the method; `SELF` and `ACTUAL` by the class. Asking the two
VMs the same question (probe `TvDecl`, below):

| type variable | HotSpot 25 `getGenericDeclaration()` | CratonVM, before |
|---|---|---|
| `SELF` (return type) | `class …AbstractObjectAssert` | **the method** `…returns(Object,Function)` |
| `T` (parameter 0) | the method | the method |
| `ACTUAL` (inside `Function<ACTUAL,T>`) | `class …AbstractObjectAssert` | **the method** |

ByteBuddy's `TypeDescription.Generic.Visitor.Substitutor.ForTypeVariableBinding`
binds a variable only when the variable's declaration equals the parameterized
type's erasure. With a `Method` as the declaration it takes the `onMethod`
branch and leaves the variable alone, so `SELF`/`ACTUAL` stayed unsubstituted
all the way into bridge generation:

```text
java.lang.IllegalArgumentException: Could not create type
  at net.bytebuddy.TypeCache.findOrInsert(TypeCache.java:174)
  at org.assertj.core.api.SoftProxies.createSoftAssertionProxyClass(SoftProxies.java:143)
  at org.assertj.core.api.AbstractSoftAssertions.proxy(AbstractSoftAssertions.java:52)
  at org.springframework.test.context.cache.ContextCacheTestUtils
        .lambda$assertContextCacheStatistics$0(ContextCacheTestUtils.java:51)
Caused by: java.lang.IllegalArgumentException:
  Cannot resolve ACTUAL from public ? org.assertj.core.api.IntegerAssert$ByteBuddy$AjCURWPq.returns(?)
```

Every Spring test that reaches AssertJ's `assertSoftly` died there.
`ContextCacheTestUtils.assertContextCacheStatistics` is one such call, and it
is what the whole `test.context.cache` package asserts with.

## The measurement that named the reflection layer

ByteBuddy can describe a type two ways: `TypeDescription.ForLoadedType` (core
reflection) and `TypePool` (it parses the class file itself). Same process,
same bytes, same library version, one probe:

```text
===== HOTSPOT =====
LOADED  rep=returns(…)Lorg/assertj/core/api/IntegerAssert;  ret=class …IntegerAssert  params=[T, Function<java.lang.Integer, T>]
POOL    rep=returns(…)Lorg/assertj/core/api/IntegerAssert;  ret=class …IntegerAssert  params=[T, Function<java.lang.Integer, T>]
===== CRATONVM (before) =====
LOADED  rep=returns(…)Ljava/lang/Object;                    ret=SELF                  params=[T, Function<ACTUAL, T>]
POOL    rep=returns(…)Lorg/assertj/core/api/IntegerAssert;  ret=class …IntegerAssert  params=[T, Function<java.lang.Integer, T>]
```

`POOL` agrees on both VMs; only `LOADED` diverges. That pairing excluded
ByteBuddy, the class file and the library version and pointed at core
reflection — before any VM source had been read.

It also killed the plausible wrong answer. `Class.getDeclaredMethods()` returns
the same *set* on both VMs but in a different *order* (HotSpot groups by name
symbol and here emits the bridges first; CratonVM emits each non-bridge before
its bridge; the class file has all bridges last — three different orders), and
it is tempting to blame the order for which of two same-erasure methods
ByteBuddy elects. The `POOL` row **is** class-file order — non-bridge first,
the same shape as CratonVM's — and it produces the correct answer on both VMs.
Order is not the discriminator. Chasing it would also have been unfixable:
HotSpot's intra-name order comes out of an unstable quicksort keyed on symbol
addresses, so there is no rule there to match.

## What it fixes

Rerunning the 32 non-`OK` classes of the 2026-09-10 15:26 full-suite ZGC
8-shard sweep (2 848 classes) under the fixed binary, one class at a time:

| status | before (the sweep) | after |
|---|---:|---:|
| `OK` | 0 | **29** |
| `FAIL` | 22 | 1 |
| `TIMEOUT` | 1 | 2 |
| `LOADERR` | 9 | 0 |

The 20 classes this defect accounts for:

```text
beans.BeanUtilsTests
beans.PropertyDescriptorUtilsPropertyResolutionTests
core.BridgeMethodResolverTests
core.ResolvableTypeTests
scheduling.annotation.ScheduledAnnotationBeanPostProcessorTests
test.context.cache.ClassLevelDirtiesContextTestNGTests
test.context.cache.ClassLevelDirtiesContextTests
test.context.cache.ContextCachePauseModeTests
test.context.cache.ContextCacheTests
test.context.cache.ContextFailureThresholdTests
test.context.cache.LruContextCacheTests
test.context.cache.SpringExtensionContextCacheTests
test.context.config.interfaces.DirtiesContextInterfaceTests
test.context.junit4.ExpectedExceptionSpringRunnerTests
test.context.junit4.FailingBeforeAndAfterMethodsSpringRunnerTests
test.context.junit4.RepeatedSpringRunnerTests
test.context.junit4.TimedSpringRunnerTests
test.context.junit4.rules.FailingBeforeAndAfterMethodsSpringRuleTests
test.context.junit4.rules.RepeatedSpringRuleTests
test.context.junit4.rules.TimedSpringRuleTests
```

Seventeen are the AssertJ `SoftAssertions` shape. The other three reach the
same defect without ByteBuddy anywhere — Spring's own generic machinery reading
the wrong declaring scope:

| test | before | after |
|---|---|---|
| `BridgeMethodResolverTests.interfaceHierarchy` | resolved to `BaseInterface.test(S)`, bound printed as `<S extends T>` — `T` never resolved | `FooInterface.test(S)`, bound `<S extends FooEntity>` |
| `BridgeMethodResolverTests.classHierarchy` | `BaseClass.test(S)` | `FooClass.test(S)` |
| `BridgeMethodResolverTests.spr3357` | picked the bridge `doSomething(DomainObjectSuper, Object)` | `<T> doSomething(DomainObjectExtendsSuper, T)` |
| `ResolvableTypeTests.resolveFromOuterClass` | `null` | `java.lang.Integer` |

`resolveFromOuterClass` states the defect in its own name: a type variable
declared by an **outer** class is precisely what the walk existed to find, and
precisely what a walk that cannot take a second step can never reach.

## Verification against the landed code

Everything above was taken with this branch's own copy of the fix. After
`bb92afbb4` landed, all of it was re-run against **dev's** version
(`cratonvm-springdev-20260911`, built from the merge):

```console
$ cratonvm-springdev-20260911 -cp "$CP" SoftProbe
SOFT_OK

$ diff tv-hs.txt tv-dev.txt | grep -c genericDeclaration
0                       # every declaring scope now matches HotSpot

$ ./run-suite.sh run --list verify29.tsv --tag verify29-dev
classes: OK=29
test-methods: found=474 passed=473 failed=0     # the 1 non-pass is a skip
```

29 classes: the 20 this defect accounts for plus the 9 restored
`aop.target` fixture classes. All `OK`, no failed methods.

## Fixed in passing

Once the walk actually moves, its pin discipline has to move with it. Each new
scope is pinned as it is entered; `scope` is re-read from the pin after every
call that can collect **and before it is compared with the reference that call
returned** (otherwise a moved `scope` compares unequal to itself, the
`enclosing != scope` guard passes, and the walk climbs into its own starting
point); the pin range is released from `base_pin` at the foot of the walk
instead of being leaked one handle per call.

## What is left, and why none of it is this

Three classes still do not read `OK`, each with its own record:

* `aot.nativex.FileNativeConfigurationWriterTests` — `FAIL 3/9`. Fails
  **identically on stock HotSpot 25** on the same classpath and the same
  harness. See `not-cratonvm-bugs-consolidated.md` beside this page.
* `beans.factory.aot.BeanRegistrationsAotContributionTests` — `TIMEOUT`.
  Correct, but ~100× slower than HotSpot: ~1 067 s against the runner's 180 s
  per-class cap. Open:
  `docs/known-issues/spring/beanregistrations-verylarge-throughput-20260907.md`.
* `test.context.aot.AotIntegrationTests` — `TIMEOUT` at the 180 s cap.
  See `docs/known-issues/spring/aotintegrationtests-runs-a-nested-suite-and-exceeds-the-per-class-cap-20260910.md`.

The 9 `LOADERR` classes were never a VM question at all — 40 fixture files
deleted from the Spring checkout's working tree. Separate page:
`the-nine-loaderr-classes-were-forty-fixture-files-deleted-from-the-working-tree-20260910.md`.

## Repro

```bash
# minimal: one soft assertion, ~10 s
cd /data/springres-work
CP="/data/springres-work:$(cat cp-spring-test.txt)"
/data/toolchain/jdk-25/bin/java -cp "$CP" SoftProbe          # SOFT_OK
<cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" SoftProbe
#   before: SOFT_FAIL java.lang.IllegalArgumentException: Could not create type
#   after:  SOFT_OK
```

```bash
# the reflection question itself, one block per type variable
/data/toolchain/jdk-25/bin/java -cp "$CP" TvDecl > tv-hs.txt
<cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" TvDecl \
  | grep -v '^\[cratonvm\]' > tv-cv.txt
diff tv-hs.txt tv-cv.txt
#   before: genericDeclaration lines differ for SELF and ACTUAL
#   after:  only the TypeVariable CARRIER CLASS differs (see below)
```

```bash
# the suite rows
cd apps/spring-suite-runner
CRATONVM_BIN=<binary> JDK25=/data/toolchain/jdk-25 SPRING=<spring checkout> \
  ./run-suite.sh run --list residual32.tsv --tag resid32
```

The five probes — `SoftProbe`, `TvDecl`, `GraphProbe`, `PoolProbe`, `BbChain` —
live in `/data/springres-work` on the Azure host. `GraphProbe` prints
ByteBuddy's `MethodGraph` node for `returns` (identical to HotSpot after the
fix); `PoolProbe` is the `LOADED` vs `POOL` pairing above; `BbChain` walks
ByteBuddy's generic superclass chain (identical on both VMs before *and* after,
which is what ruled out the substitution machinery itself).

## The carrier class is a separate, still-open cosmetic gap

After the fix `diff tv-hs.txt tv-cv.txt` still shows one difference, on every
line:

```text
< SELF [sun.reflect.generics.reflectiveObjects.TypeVariableImpl]     # HotSpot
> SELF [java.lang.reflect.TypeVariable]                              # CratonVM
```

CratonVM mints instances of the *interfaces* `TypeVariable`,
`ParameterizedType`, `WildcardType` and `GenericArrayType`; the VM's own
JVMS 6.5 uninstantiable-receiver census reports all four at shutdown, naming
`generics.rs` as the requester. Nothing in this investigation turned out to
depend on it — `instanceof` answers correctly and every accessor agreed with
HotSpot once the walk was fixed — but it is the reason a
`getClass().getName()` comparison against these objects cannot be used as an
oracle, and it is why the `TvDecl` diff is not expected to be empty.
