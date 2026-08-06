# Infinispan `ConfigurationBuilder` retransform rejected: "stack overflow during verification"

**Status: FIXED / CLOSED 2026-08-06.** Was
`docs/known-issues/springboot/infinispan-configurationbuilder-retransform-verify-20260805.md`,
split out on 2026-08-05 from
`cacheautoconfigurationtests-configclass-parse-nosuchmethod-gc-20260805`
(retired as
[`cacheautoconfigurationtests-configclass-parse-nosuchmethod-FIXED.md`](cacheautoconfigurationtests-configclass-parse-nosuchmethod-FIXED.md)).

`module/spring-boot-cache` ·
`org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`
(`infinispanAsJCacheWithConfig`, `infinispanAsJCacheWithCaches`)

## The symptom, once, on Azure Linux (`dev @1078f6f05c`, 2026-08-05)

```
WARN retransformClasses0: UnsupportedClassRedefinitionError {
  class_name: "org/infinispan/configuration/cache/ConfigurationBuilder",
  message: "new bytes failed bytecode verification: verification error in
  org/infinispan/configuration/cache/ConfigurationBuilder.simpleCache:
  at bytecode offset 27: stack overflow during verification" }
```

The open page left two hypotheses — **(1)** our verifier over-counts operand
stack depth for something ByteBuddy emits, or **(2)** the bytes were damaged
before verification — and prescribed re-running the Azure full suite with
`CRATONVM_DBG_REDEFINE_DUMP` set, hoping for a second sighting.

That prescription was not needed. **The reported offset alone decides it**, and
the second sighting never came.

## Hypothesis 1 is dead: the offset names a `max_stack` ByteBuddy cannot emit

Offset 27 in the woven `simpleCache` is a one-slot push onto a 3-deep stack
(`byte-buddy 1.18.8` / `mockito-core 5.23.0` / `infinispan-core 16.1.4` — the
versions on both the Azure and the Windows checkout; the woven prologue is
byte-identical on both hosts). `VerificationFrame::push` rejects when
`stack.len() >= max_stack`, so:

* an overflow **at** 27 requires `max_stack <= 4`, and
* **no** overflow at 24 (a push onto a 3-deep stack) requires `max_stack >= 4`.

So the `Code` attribute the verifier read declared **exactly `max_stack = 4`**.
Both `simpleCache` overloads give the same answer — the woven prologue is the
same shape for each — so it does not matter which one the message named.

ByteBuddy cannot produce that. Its advice weaving declares
`max(original_max_stack, advice_requirement)`; measured directly by patching the
original `ConfigurationBuilder.class` and reading the woven dump back
(`probes/patch_max_stack.py`, `-Dnet.bytebuddy.dump=<dir>`):

| original `simpleCache()Z` `max_stack` | woven `max_stack` |
|---:|---:|
| 2 (as shipped) | 6 |
| 3 | 6 |
| 5 | 6 |
| 9 | 9 |

The floor for that woven body is **6** (**9** for the `(Z)` overload). No base
class file, valid or not, makes ByteBuddy emit 4. **The class file that reached
`redefine_class` was therefore not the class file ByteBuddy produced.** That is
hypothesis 2, and it is the same family as the sibling face the page recorded at
the neighbouring bisect anchor `ffd559231` — an `ArrayIndexOutOfBoundsException`
raised *inside* Mockito's inline mock creation, i.e. ASM reading a buffer that
was not the class file it expected.

The damage window is the one `383e7f5cf` closed on 2026-08-05, **after** the run
that produced this line: a per-site memo keyed on a recycled `JitInvokeInfo`
address let one call site serve another's dispatch, corrupting arbitrary
reference values. The parent page's own failures on this exact test class went
51-57 → 0 across that commit.

## The verifier was independently cleared

Hypothesis 1 was not merely argued away, it was measured. Every class on the
`module/spring-boot-cache` test classpath — **76 557 classes**, the full
transitive closure of 60-odd jars — was loaded under both VMs:

| arm | loaded | `NoClassDefFoundError` | verifier rejections |
|---|---:|---:|---:|
| HotSpot 25 | 75 796 | 760 | 0 |
| CratonVM | 75 796 | 760 | 0 |

The two failure sets are identical class-for-class. And the sweep is not
vacuous: with `simpleCache()Z`'s `max_stack` patched from 6 down to 4, CratonVM
rejects the same class file — the positive control that proves Pass 3 ran, and
incidentally confirms the arithmetic above.

A narrower sweep drove **636 real ByteBuddy retransformations** through
`redefine_class` in one process (`probes/MockManyProbe.java`, mocking every
concrete public class in the infinispan / spring-core / spring-context /
caffeine jars). Zero rejections, zero `CRATONVM_DBG_REDEFINE_DUMP` output.

## Reproduction, on both halves

The page's real cost was that its only reproduction was a whole
`CacheAutoConfigurationTests` run under the suite runner.
`probes/InfinispanMockRetransformProbe.java` is the same trigger in two seconds:
load `ConfigurationBuilder`, `mock()` it, call both `simpleCache` overloads.

| host | build | runs | result |
|---|---|---:|---|
| Windows 11 | `dev` @ `ce462d315` | 1 | clean |
| **Azure Linux** | `dev` @ `ce462d315` | **5** | **clean, no redefine dumps** |

The page said, correctly, not to close on a Windows-only run
(`feedback_a_windows_only_verification_leaves_the_linux_half_uncovered`). The
Linux half is above.

**The confound the page told us to rule out first is ruled out.** That Azure
checkout carries ~12 788 CRLF-corrupted fixture files, but the artifact that
matters here is a binary jar: `infinispan-core-16.1.4.jar` is `b58a3396…` on
both hosts and the `ConfigurationBuilder.class` inside it is `60f684cb…`,
21 585 bytes, byte-identical.

## Three real defects found in the same machinery, and fixed

The investigation was not empty-handed. Everything below is in the JVMTI
retransformation path this page's symptom passes through, and each is
independently a way to produce a rejection or a crash that reads like the one
filed here.

### 1. A redefine was verified more strictly than the definition it replaced

`define_class_with_options` **defers the Pass-3 type-state verdict** for any
class whose defining loader is `UserDefined` while `loader_aware_resolution()`
is on (the default): that hierarchy adapter cannot preserve both loader
identities through every pre-definition edge and produces false
areturn/checkcast rejections for otherwise valid forked bytecode. It enforces
the hierarchy-independent structural half (JVMS §4.9.1) and moves on.

`redefine_class` ran the **full** verifier regardless. So every application
class in a Spring / WildFly / H2 / Elasticsearch run — all of them defined by
user loaders — was excused from Pass 3 when it loaded and then held to it the
moment a JVMTI agent retransformed it. Mockito's inline mock maker retransforms
every class it mocks, so retransform time was the *one* place the false
rejections the deferral exists to avoid could surface, and the face is exactly
the one filed on this page:

```
UnsupportedClassRedefinitionError { class_name: "Foo",
  message: "new bytes failed bytecode verification: verification error in
  Foo.foo: at bytecode offset 0: stack overflow during verification: ..." }
```

That is `classloading/tests/redefine_verify_policy.rs` running against the
pre-fix code — same error type, same message, same phrase, no corrupted bytes
anywhere. It goes green on the fix, with two controls: an application-loader
class is still fully verified, and structurally broken bytes are still rejected
under a user loader.

**This is a second, independent mechanism for this page's exact symptom.** It
cannot be *the* mechanism for the Azure line — that one needs `max_stack = 4`,
which the deferral does not create — but it is the mechanism that would have
produced the next one.

### 2. Transformers were handed a zero-length class file

`retransformClasses0` ran the whole transformer chain even when it had **no
base bytes at all**, passing every registered transformer an empty `byte[]` as
the class file. `original_class_bytes`'s own doc comment claimed "an empty
result makes `native_retransform_classes0` skip the class, which is the safe
outcome" — only the *restore* step was skipped; the chain ran anyway. A comment
outliving its defect.

ASM's `ClassReader` reads the header off that array unconditionally, so Mockito
re-throws the `ArrayIndexOutOfBoundsException` from inside mock creation, where
it reads as a broken agent rather than the missing retransformation base it
actually is — **the sibling face this page recorded at `ffd559231`**. Now
guarded at the funnel (`run_transformer_chain`) as well as at the caller.

### 3. The classpath fallback could seed a retransform with a different build

`class_bytes_cache` is FIFO-evicted at a 16 MiB soft cap, and any real
application blows through that during startup — so by the time an agent
retransforms a class, the base is usually re-read as `<name>.class` off the
classpath: bootstrap, then extension, then application, **never the class's
defining loader**. Matching `this_class` was the only check, and it cannot tell
two *builds of the same class* apart. Spring Boot's test infrastructure produces
that state routinely, defining classes through `ModifiedClassPathClassLoader` /
`FilteredClassLoader` / per-test `URLClassLoader`s while a different build of
the same coordinate sits on the application classpath.

The VM now keeps a never-evicted `(length, hash)` of the bytes each class was
defined from — 16 bytes per class — and refuses a fallback that does not match,
rather than weaving build A's method bodies and installing them over live
build B.

## Verdict

`CacheAutoConfigurationTests`, all 59 tests, on the branch that carries the
three fixes below:

| arm | status | failed | seconds |
|---|---|---:|---:|
| CratonVM, Windows 11 | **PASS** | 0 | 272.9 |
| CratonVM, **Azure Linux** | **PASS** | 0 | 203.7 |

Zero `UnsupportedClassRedefinitionError`s, zero "stack overflow during
verification", zero `CRATONVM_DBG_REDEFINE_DUMP` output and zero
retransformation-base refusals in either stderr log.

Reproduce:

```bash
pwsh -NoProfile -File apps/spring-boot-suite-runner/run-spring-boot-suite.ps1 -Vm craton -Exe <exe> -JdkHome <jdk25> -SpringBootRoot <spring-boot> -ClassList apps/spring-boot-suite-runner/.suite/infverify-cache-20260806.tsv -RunName infverify-cache -Parallel 1 -TimeoutSec 900
```

## What to keep

* `probes/InfinispanMockRetransformProbe.java` — the two-second trigger.
* `probes/MockManyProbe.java` — N retransformations through `redefine_class` in
  one process, for the next time a woven-bytes question needs volume.
* `probes/LoadAndVerifyProbe.java` — `@list-file` of class names; `define_class`
  runs the same `verifier::verify_class` `redefine_class` runs, so a rejection
  reproduces from captured bytes with no agent and no suite.
* `probes/patch_max_stack.py` — the positive control. **A verifier sweep that
  reports "everything verified" is indistinguishable from one whose verifier
  never ran**; hand it a class that must be rejected before believing a green.

## Found in passing, filed separately

`mock()`-ing `org.springframework.core.NamedThreadLocal` fails on CratonVM with
`NullPointerException: … the return value of "java.lang.ThreadLocal.get()" is
null`, and then **every subsequent `mock()` in the process fails identically**
(89 of 728 in the sweep). HotSpot 25 mocks all of them cleanly. Mockito
instruments the target's whole superclass chain, so this retransforms
`java.lang.ThreadLocal` itself — the hypothesis to test first is that the
retransform installs woven bytecode over a method this VM serves with a Rust
native, losing the implementation. Not this page's defect.
