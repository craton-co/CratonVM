# `BatchDataMongoAutoConfigurationTests`: not a classloading bug at all — a recycled `JitInvokeInfo` address

**Status: RESOLVED — 2026-08-05.** The `NoClassDefFoundError` was one face of
the recycled-`JitInvokeInfo` dispatch defect closed by `383e7f5cf`. Verified
13/13 over 14 runs with the JIT native site cache — the loudest consumer of
the broken key — explicitly ON.

## The reported symptom was a red herring, and this records why

```
Caused by: java.lang.NoClassDefFoundError: org/springframework/data/mongodb/core/convert/MongoCustomConversions
```

for a class whose jar is demonstrably on the runtime classpath (HotSpot runs
the same fixture 13/0). The open note reasoned from the *shape* of the error —
"a `NoClassDefFoundError` rather than a `ClassNotFoundException` usually means
an earlier failed load was cached" — and sent the next reader at `<clinit>`
poisoning. That reasoning is right for HotSpot and does not transfer:

CratonVM raises `NoClassDefFoundError` **with the internal, slash-form class
name** from `ensure_class_initialized_shared`'s `InitializationError` arm
(`vm/src/vm/vm_util.rs`) — which really is the cached-failed-`<clinit>` path —
and it *also* raises it, with the same slash-form message, from
`runtime/exceptions.rs::raise_no_class_def_found` on ordinary resolution
failure. The two are indistinguishable in a log, so the message carries none
of the information the HotSpot version of it does.

`CRATONVM_DBG=clinit-fail` settled it in a single run: the only class that
ever reached `InitializationError` was
`org/apache/commons/logging/impl/Log4jApiLogFactory`, which is
commons-logging probing for an absent log4j-api and happens on HotSpot too.
`MongoCustomConversions` never entered `<clinit>`.

## What the evidence actually said

The failure has **no fixed face**. Same binary, same fixture, run to run:

- the reported `NoClassDefFoundError` for `MongoCustomConversions`;
- `NoSuchMethodError: java.util.Arrays$ArrayList.getEnumConstantsShared(...)`
  — an `invokeinterface` on `JavaLangAccess` whose receiver was not one;
- `java.lang.Class cannot be cast to
  org.springframework.core.annotation.MergedAnnotation`;
- NPEs inside ByteBuddy (`writeAssignment is null`,
  `LazyProjection.resolve()` returned null) and `MockitoException: cannot mock
  this class`;
- one SIGSEGV.

`--nojit` is 13/13 and HotSpot is 13/13. **JIT-only, self-inconsistent, and
wrong at the level of TYPE at the point of use** — a dispatch or memory defect.
No classloading cause was going to explain that set, which is the reading the
original page missed by taking the first exception at face value.

## Root cause

`383e7f5cf` — *"a recycled JitInvokeInfo address let one call site serve
another's dispatch"*. Every per-thread dispatch memo in `jit/helpers.rs` is
keyed on `JitSiteKey = (vm_identity, JitInvokeInfo pointer)`. Those boxes are
owned by `CompiledMethod::_jit_invoke_infos` and freed when the method drops,
after which the allocator can hand the same address to the next compile's
info — and the key then names a DIFFERENT call site while the memos still hold
the old site's answer. `NATIVE_SITE_CACHE` holding a resolved native is the
loudest form: the reused site CALLs the previous site's native and returns
whatever that returns.

That is exactly the shape seen here — a receiver of the wrong class, a
`Class` where a `MergedAnnotation` belongs, an interface call landing on a
method its receiver never declared.

### Why this class, and why 2026-08-05

The 2026-08-04/05 work on the JIT's per-call-site native cache
(`9405271bd`, `84e2cc79e`, then `836631dcc` widening it from the audited leaf
set to every registered native) did not introduce the aliasing — it made the
existing hazard vastly more likely to be *observed*, because it put a resolved
native behind that key at almost every call site instead of a handful.

Measured on this fixture (13 tests), one host, the cache switched at runtime:

| tree | site cache | runs | runs with ≥1 failure |
|---|---|---:|---:|
| before `383e7f5cf` | off | 14 | **0** |
| before `383e7f5cf` | leaves only (2026-08-04) | 12 | 3 (7, 1 and 1 failures) |
| before `383e7f5cf` | every registered native (2026-08-05) | 8 | **8** (9-12 failures, one SIGSEGV) |
| with `383e7f5cf` | every registered native | 14 | **0** |

The leaf-only row is the configuration the 2026-08-05 Azure full-suite binary
carried (`1078f6f05c` contains `84e2cc79e`, not `836631dcc`), and its 25 %
rate is why that run scored this class FAIL 10/13 where 2026-08-02 — before
any of it — scored PASS 13/13. The bottom row is the fix.

## What this branch changes

Nothing about the root cause: `383e7f5cf` had already landed on `dev` and is
merged in. Two things came out of chasing it and are worth keeping.

**1. `resolve_native_owner_for_receiver` resolved its superclass walk by
NAME.** It took the receiver's `ClassId`, converted it to a name, and called
`get_loaded_class_id(name)` to start walking. A name does not identify a class
once two loaders have defined it — `invoke_or_native`'s own tail says so in as
many words ("A virtual call's receiver IS the authoritative answer") — and
this very test class builds a second, child-first definition of the mongo
types through `FilteredClassLoader` in
`autoconfigurationBacksOffEntirelyIfSpringMongoDbAbsent`. Resolving the walk
against the other loader's copy reads another class's method table, so the
"does the dispatch class declare its own bytecode" and superclass rules answer
about a class the receiver is not an instance of — and the entry that installs
is then guarded by the REAL receiver's class id, so it keeps firing. The walk
now starts from the `ClassId`; the name is used only for the registry, which
is name-keyed by construction.

It is a real defect and it is not the one above: with it fixed alone, and
`383e7f5cf` absent, this class stayed red 11-12/13.

**2. `CRATONVM_JIT=-native-site-cache`**, a default-ON kill switch for the
native site cache. This path was the prime suspect for a full day and there
was no way to take it out of a run short of a ten-minute rebuild — the entire
table above was produced by hand-patching a bisect switch into the binary
three times. A path whose failure mode is "call some other call site's native"
should be removable in one flag. `native_site_cache_default_is_on_and_the_kill_switch_kills`
pins both halves: that the default is ON, and that the token's `off_key` is
the key the reader actually consults (a switch that silently does nothing is
worse than none — the next investigation would clear this path as a suspect
while it was still in the run).

## Hypotheses that were tested and are wrong

Recorded because each looked convincing and each cost a build:

- **a poisoned `<clinit>`** — `CRATONVM_DBG=clinit-fail` says no;
- **`native_pending_return` clobbering** — the funnel clears it on every
  native return, and it is the handoff root for an object an earlier native
  returned into a compiled frame. Preserving it across primitive-returning
  natives changed nothing;
- **the prevalidated funnel** — `safe_native_call` instead of
  `safe_native_call_prevalidated_objects` did not help (and crashed);
- **one call site with two implementations** — refusing any site whose target
  has a real bytecode body still left 5 of 6 runs red;
- **the JIT scan cache** (`CRATONVM_JIT=-scan-cache`) and **GC pressure**
  (`-Xmx12g`) — neither moved it.

`Enum.ordinal()` looked like a clean single-native culprit for a while
(admitting only `java/lang/Enum` reproduced 4-5 failures) and a probe showed
it answers correctly in a hot compiled loop. Under the aliasing explanation
that is expected: which native a site serves is not a property of the native.

## Reproducing

```
apps/spring-boot-suite-runner/run-single-class.ps1 \
  -Module module/spring-boot-batch-data-mongodb \
  -ClassName org.springframework.boot.batch.mongodb.autoconfigure.BatchDataMongoAutoConfigurationTests \
  -Exe <binary>
```

13/13. `CRATONVM_DBG=intrinsic-stats` prints `compiled site-cached native
dispatches (non-leaf)` — non-zero is what says the amplifier is in the run and
the green is not vacuous (397,712 on a 400k-iteration probe; zero with
`CRATONVM_JIT=-native-site-cache`).

## Affected classes

- `module/spring-boot-batch-data-mongodb` —
  `org.springframework.boot.batch.mongodb.autoconfigure.BatchDataMongoAutoConfigurationTests`
  (13/13, 14 runs, site cache on).

The 2026-08-05 Spring Boot cluster whose symptoms are the same family —
`mockito-bytebuddy-mock-creation-npe-cluster-20260805`,
`mockito-bytebuddy-classfile-metadata-cluster-20260805`,
`spring-boot-annotation-metadata-null-cluster-20260805`,
`classfile-annotation-metadata-corruption-20260805` — was all found on
pre-`383e7f5cf` binaries and is a strong candidate to close with it. Each was
recorded on the Azure host and none is re-measured here, so they stay open
until they are.
