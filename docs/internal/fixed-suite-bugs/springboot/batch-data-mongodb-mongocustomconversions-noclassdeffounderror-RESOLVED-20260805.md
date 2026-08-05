# `BatchDataMongoAutoConfigurationTests`: the JIT's per-call-site native cache gave one call site two implementations

**Status: RESOLVED — 2026-08-05.** Fixed by
`fix/springboot-mongocustomconversions-20260805`. The
`NoClassDefFoundError` was one face of a JIT dispatch defect, not a
classloading defect; the class is 13/13 over 18 runs with the fix.

## The reported symptom was a red herring, and said so itself

```
Caused by: java.lang.NoClassDefFoundError: org/springframework/data/mongodb/core/convert/MongoCustomConversions
```

for a class whose jar is demonstrably on the runtime classpath (HotSpot runs
the same fixture 13/0). The original note reasoned from the *shape* of the
error — "a `NoClassDefFoundError` rather than a `ClassNotFoundException`
usually means an earlier failed load was cached" — and pointed at
`<clinit>` poisoning. It is worth stating why that reasoning, which is
correct for HotSpot, did not apply here.

CratonVM raises `NoClassDefFoundError` **with the internal, slash-form class
name** from `ensure_class_initialized_shared`'s `InitializationError` arm
(`vm/src/vm/vm_util.rs`), which is indeed the cached-failed-`<clinit>` path.
But it also raises it, with the same slash-form message, from
`runtime/exceptions.rs::raise_no_class_def_found` on ordinary resolution
failures. The two are indistinguishable in a log. Running the fixture under
`CRATONVM_DBG=clinit-fail` settled it in one run: the only class that ever
reached `InitializationError` was
`org/apache/commons/logging/impl/Log4jApiLogFactory`, which is
commons-logging probing for an absent log4j-api and is expected on HotSpot
too. `MongoCustomConversions` never entered `<clinit>` at all.

## What it actually was

The failure is **JIT-only** and **not deterministic in its face**. The same
binary and fixture produced, run to run:

- `NoClassDefFoundError` for `MongoCustomConversions` (the reported face);
- `NoSuchMethodError: java.util.Arrays$ArrayList.getEnumConstantsShared(...)`
  — an `invokeinterface` on `JavaLangAccess` whose receiver was not a
  `JavaLangAccess` at all;
- `ClassCastException: java.lang.Class cannot be cast to
  org.springframework.core.annotation.MergedAnnotation`;
- NPEs inside ByteBuddy (`writeAssignment is null`,
  `LazyProjection.resolve()` null) and `MockitoException: cannot mock this
  class`;
- one SIGSEGV.

`--nojit` is 13/13. That combination — JIT-only, self-inconsistent, wrong
*type* at the point of use — is a dispatch or memory defect, and the search
for a classloading cause was never going to converge.

### The mechanism

2026-08-04 (`9405271bd`, `84e2cc79e`) gave compiled code a per-call-site
native resolution cache, and 2026-08-05 (`836631dcc`) widened it from the
audited leaf set to **every registered native**. Both hook in at the TOP of
`jit_invoke_dispatch` and `jit_invoke_virtual_mic` — ahead of the monomorphic
inline cache, ahead of the compile probes.

That placement is the defect. The MIC can bind **the same call site** to a
compiled or interpreted **bytecode body**; the site cache serves a registered
**native** for it. One call site, two implementations, and which one runs
depends on which warmed first — per site, per thread, per run. For any native
whose state does not live where the bytecode reads it, that is silent
corruption that cannot name its origin.

The commit message for `836631dcc` states the premise explicitly: the
site-cached call is "the same call `invoke_or_native` would have made after
its cascade". That is true of the *bail* path in
`jit_invoke_dispatch`, which does reach `invoke_or_native`. It is not true of
the MIC path, which is where compiled virtual and interface calls actually
arrive, and which never consults `invoke_or_native` at all.

### Measurement

One binary, one host, the mechanism switched at runtime, on
`module/spring-boot-batch-data-mongodb`'s `BatchDataMongoAutoConfigurationTests`
(13 tests; `--nojit` green; HotSpot `hsfull-after-20260804-s3` green):

| site cache | runs | runs with ≥1 failure | failures seen |
|---|---:|---:|---|
| off (pre-2026-08-04 route) | 14 | **0** | — |
| leaves only (2026-08-04) | 12 | 3 | 7, 1, 1 |
| every registered native (2026-08-05) | 8 | **8** | 9-12, plus one SIGSEGV |

The leaf-only arm is the configuration the 2026-08-05 Azure full-suite run
carried (`1078f6f05c` contains `84e2cc79e` but not `836631dcc`), and its
25 % failure rate is why that run recorded this class as FAIL 10/13 while
2026-08-02 — before any of it — recorded PASS 13/13.

## The fix

`CRATONVM_JIT=native-site-cache`, **default OFF**
(`jit::helpers::native_site_cache_enabled`). `try_jit_site_cached_native_dispatch`
declines before it does anything else, so compiled code returns to the route
it took through 2026-08-02. The path, its counters
(`CRATONVM_DBG=intrinsic-stats`) and its refusal tally all stay, so the
performance work is measurable and can be re-landed once the two routes are
made to agree.

A second, independent defect found on the way is fixed rather than gated:
`resolve_native_owner_for_receiver` took the receiver's `ClassId`, converted
it to a NAME, and then called `get_loaded_class_id(name)` to start its
superclass walk. A name does not identify a class once two loaders have
defined it — `invoke_or_native`'s own tail says so in as many words ("A
virtual call's receiver IS the authoritative answer") — and this very test
class builds a second definition of the mongo types through
`FilteredClassLoader`. The walk now starts from the receiver's `ClassId`.

### What did NOT fix it

Recorded because each looked convincing and each cost a build:

- the `ClassId` round-trip above (a real bug, kept, but this class stayed red
  11-12/13 with it);
- preserving `native_pending_return` across a primitive-returning native, so
  an earlier native's object return keeps its handoff root;
- the full funnel (`safe_native_call`) instead of
  `safe_native_call_prevalidated_objects`;
- refusing any site whose target has a real bytecode body (still 5 of 6 runs
  red);
- `CRATONVM_JIT=-scan-cache`, and `-Xmx12g` — so it is neither the JIT scan
  cache nor simple GC pressure.

## Cost

The performance this gives back is real and is documented in the commits
being gated: `docs/internal/aqs-thread-handoff-latency-RETIRED-20260805.md`
(an uncontended `ReentrantLock` pair, 2,687 → 1,229 ns) and the leaf work's
3.9-10.5x on leaf natives. It is given back deliberately. A path that makes
a 13-test class fail 8 runs out of 8 is not a 2.2x win.

## Re-opening it correctly

The counters answer "is it on and is it firing": with
`CRATONVM_JIT=native-site-cache CRATONVM_DBG=intrinsic-stats`, look for
`compiled leaf-native dispatches` and `compiled site-cached native dispatches
(non-leaf)`. The thing that has to be established before it goes back on by
default is the one the measurement above did not: **that no site this cache
serves can also be bound to a bytecode body by the inline cache.** A
`--nojit` run is not a control for this — it is green either way.

`vm/src/jit/helpers.rs::native_site_cache_is_off_unless_asked_for` pins the
default so a flip has to be deliberate.

## Reproducing

```
apps/spring-boot-suite-runner/run-single-class.ps1 \
  -Module module/spring-boot-batch-data-mongodb \
  -ClassName org.springframework.boot.batch.mongodb.autoconfigure.BatchDataMongoAutoConfigurationTests \
  -Exe <binary>
```

Green by default. `CRATONVM_JIT=native-site-cache` in the environment brings
the failures straight back (12/13 failed on the first run of the fixed
binary), which is what says the flag is what decides and not the weather.

## Affected classes

- `module/spring-boot-batch-data-mongodb` —
  `org.springframework.boot.batch.mongodb.autoconfigure.BatchDataMongoAutoConfigurationTests`
  (13/13 with the fix, 18 runs).

The 2026-08-05 Spring Boot cluster whose symptoms are the same family —
`mockito-bytebuddy-mock-creation-npe-cluster-20260805`,
`mockito-bytebuddy-classfile-metadata-cluster-20260805`,
`spring-boot-annotation-metadata-null-cluster-20260805`,
`classfile-annotation-metadata-corruption-20260805` — was found on the same
leaf-only binary and is a candidate to close with this, but each was recorded
on the Azure host and none is re-measured here. They stay open until they
are.
