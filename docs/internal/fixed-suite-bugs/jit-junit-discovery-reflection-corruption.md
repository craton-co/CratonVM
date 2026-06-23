\---

name: jit-junit-discovery-reflection-corruption

description: FIXED — reflection mirror arrays (Field\[]/Method\[]/annotation arrays) weren't GC-rooted while built; a mid-build GC corrupted them to bare java/lang/Object. THE WildFly B-on cluster (annotationType/AbstractMethodError/ClassCast). NOT JIT-specific.

metadata:

&#x20; node\_type: memory

&#x20; type: project

&#x20; originSessionId: 235abc15-3524-4d58-83a6-5a6b5f702895

\---



The genuinely-CratonVM-specific WildFly B-on JUnit-platform cluster (`annotationType

must not be null` ×5, `AbstractMethodError TestEngine.getId/TestDescriptor.getParent/

Executable.hasGenericInformation` ×3, `ClassCast Object→TestExecutionResult$Status` ×1 —

the "\~10 classes / 0.6% non-reproducible" set) was NOT a JIT codegen bug and NOT the

exception-handler bug \[\[jit-exception-handler-noargs-this-null]]. It was a \*\*GC-root gap

in the reflection native array builders\*\*.



\*\*Root cause:\*\* `native-builtins/src/lang\_class.rs` builders like `getDeclaredFields0` did

`let arr = ctx.new\_ref\_array(...); for meta { let o = create\_field\_object(ctx, meta); /\*ALLOCATES\*/ ctx.set\_array\_element(arr, i, o) }`. `arr` lives only in a Rust local — invisible

to the GC root scan. `create\_field\_object`/`create\_method\_object`/`create\_constructor\_object`/

`create\_annotation\_proxy`/`descriptor\_to\_class\_mirror` allocate, so a young GC mid-loop

relocates/reclaims `arr`; the stale `ObjectRef` then resolves to a reused, usually

`java/lang/Object`, slot (the `pin\_native\_root` doc describes this exact failure). Plain

`new String\[]` and `Array.newInstance` are fine — only fill-WITH-allocation builders break.



\*\*Not JIT-specific:\*\* reproduces with `CRATONVM\_DISABLE\_JIT=1` under

`CRATONVM\_DBG\_GC\_STRESS=65536` (force young GC every 64 KB). B-on merely EXPOSES it in

production — JIT-compiling the reflection-heavy discovery path raises GC pressure during

the mid-build window. (That's why it looked "B-gated": B-off ran GC rarely enough to miss it.)



\*\*Repro (deterministic standalone!):\*\* `wildfly-suite/repro/MinRepro.java` Case A

(`getDeclaredFields()` held across an alloc) crashes `ClassCast Object→\[Field` under

GC\_STRESS; Case B (`new String\[]`) and `ArrRepro` (`Array.newInstance`) pass — pinpointing

fill-with-alloc. Also ReflRepro/JUnitReflRepro. The standalone repro was found via GC\_STRESS

after the WildFly cluster (warmed clustering batch, B-on THRESH=2) gave the same signatures.



\*\*Fix (FIXED on dev, 2026-06-16):\*\* `build\_mirror\_array`/`build\_mirror\_array\_comp` helper —

allocate the array, `pin\_native\_root` it across the fill loop, `read\_native\_pin` the

forwarded ref before each store. Applied to getDeclaredFields0/Methods0/Constructors0,

create\_method/constructor\_object param+exception arrays, build\_annotation\_array/

build\_class\_annotation\_array, getInterfaces (x2), getAnnotationsByType (x2). Verified:

the PRIMARY cluster repro `MinRepro` (getDeclaredFields ClassCast) + `ArrRepro` clean

(== HotSpot) + WildFly clustering cluster → benign no-container FAIL.

\*\*⚠ RESIDUAL GAP CONFIRMED (2026-06-16, merged dev binary `3f1cbdd3` = dev incl. fix

`fda29dcf`):\*\* `ReflRepro` (getDeclaredFields + getDeclaredMethods + `f.getAnnotation`

hot loop) STILL crashes \*\*deterministically 4/4\*\* under `CRATONVM\_DBG\_GC\_STRESS=65536`

— rc=132 SIGILL, signature `inline-alloc forgot to set kind=Array` /

`implausible object size … class\_id=0` (a corrupt array HEADER, distinct from the

mirror-array ClassCast). So the earlier "all repros clean" was over-optimistic — the

residual same-pattern sites below are NOT just lower-freq, they're reproducibly broken

(ReflRepro's `getAnnotation` → `create\_annotation\_proxy` name/value arrays and/or the

getDeclared\* filter/Vec-build path). The follow-up sweep is REQUIRED, not optional.

\*\*↳ CORRECTION (2026-06-16, deeper dig): ReflRepro's residual crash is NOT the

reflection builders.\*\* Investigated with a debug-symbol build + `CRATONVM\_DBG\_OOBFIELD`:

(1) the `set\_field out-of-bounds on a 0-slot java/lang/Object` that looks like a

reclaimed mirror is a \*\*benign VM-STARTUP artifact\*\* — backtrace =

`ensure\_system\_stdin\_object` → `initialize\_class\_shared` → `Vm::invoke` (one-shot,

dropped by the guard), NOT the reflection loop. (2) Across the whole ReflRepro run

there are \*\*ZERO loop-time OOB writes\*\*, so create\_field/method/constructor/

annotation-proxy objects are NOT being reclaimed-and-written — i.e. a

build\_mirror\_array-style pin sweep of those builders (which I implemented and then

REVERTED, golden-clean but no effect on ReflRepro) does NOT fix this crash. (3) The

actual fatal crash is the \*\*non-moving young sweep walker hitting a bad ARRAY header\*\*

(`implausible object size …`, `kind=Object but array\_length=N / inline-alloc forgot

to set kind=Array`) with \*\*UTF-16 string bytes\*\* (`73 00 61 00` = "s␀a␀") right around

the bad header — i.e. a \*\*String/char\[] (StringBuilder / Class.getName) array\*\*,

interpreter-allocated, whose size/header the walker mis-parses under

`CRATONVM\_DBG\_GC\_STRESS=65536`. So ReflRepro is a \*\*core young-heap array/String

allocation↔sweep bug\*\*, DISTINCT from bug-06's reflection mirror-array rooting — it

needs GC-walker/object-layout debugging (why an array header reads as kind=Object /

implausible size), not more lang\_class.rs pinning. The bug-06 residual reflection-

builder sweep (getParameterAnnotations / annotation-proxy / getFields-Vec) may still

be worth doing as hardening, but it is NOT what makes ReflRepro crash.

Residual same-pattern sites (not in the primary cluster): collect\_public\_fields/methods

(getFields/getMethods Vec-build), getParameterAnnotations, create\_annotation\_proxy

name/value arrays, enum/type-var builders — tracked in bug-06 for a follow-up sweep.

Docs: `CratonVM-wildfly/docs/wildfly-suite-bugs/bug-06-\*.md`.

\*\*Lesson:\*\* any native that holds an `ObjectRef` across an allocating call must

`pin\_native\_root` it (the codebase's HIB-CV-18 "GC-safe array fill" pattern).

---

## Spring-suite full-run field evidence — the residual race is live at suite scale (2026-06-16)

Running the **complete** Spring Framework suite (`apps/spring-framework`, ~2930 classes, JUnit
Platform 6.1, 4-way batched, boot JDK 25) on dev `8e8e47d9` reproduced this race **pervasively** in
production — independent confirmation that the residual young-sweep / GC-register-invisibility gap
(this doc + [[SB-SUITE-CRASH-04-jit-inline-new-heap-corruption]] + the `precise-jit-stack-maps-*`
testcases) is still active without `GC_STRESS`. Full per-finding reports + repros:
`spring-suite/crash-reports-2026-06-16/` (`bug-04`, `bug-05`, `crash-03`, `INDEX.md`, `FAIL-ANALYSIS.md`).

**Diagnostic signature (use this to classify any suite failure):** a class that **fails or crashes in
the batched run but passes 1-per-JVM in isolation** is *this race*, NOT a deterministic bug. In the
2026-06-16 run, **every deterministic crash was a separate, fixable bug** (`crash-01` ArrayList(int)
OOM-abort, `crash-03` Netty `PooledByteBuf` `Unsafe.Buffer.address` SIGSEGV — both reproduced 1/JVM and
are now fixed); **everything below is load-dependent and is this one race.**

**Manifestations observed (one race, different victims):**
1. **Batch-only SIGSEGV at a *varying* victim class.** spring-webflux batches SIGSEGV'd (rc=139) at a
   *different* class on each run (`DefaultWebClientTests`, `DefaultRenderingResponseTests`,
   `DispatcherHandlerIntegrationTests`, `DefaultClientRequestBuilderTests`); none reproduces in
   isolation. The **varying victim is the hallmark** — the collector reclaims whatever live object is
   transiently unrooted at the GC point under multithreaded JUnit execution.
2. **Live String constant → bare `java.lang.Object`** (`bug-04`). KRun's interned `"OK"`/`"FAIL"`
   status literals read back as `java.lang.Object@<hash>` in batch (only **two** distinct hashes per
   JVM = the two constants), but correct in isolation. Same reclaim-and-reuse, hitting interned
   constants instead of mirror arrays.
3. **`ClassCastException: java/lang/Object cannot be cast to Class / CharSequence /
   TestExecutionSummary$Failure`** — FAIL-cluster instances of a live ref reclaimed→reused as `Object`
   (same `Object→...$Status` family this doc opened with, now seen across spring-beans/web/JUnit-harness).
4. **Part of the generics-reflection CCE cluster** (`bug-05`): `DirectFieldAccessorTests` passes 93/93
   alone but FAILs the `FieldTypeSignature`→`Type[]` CCE in batch — a *load-dependent* second cause,
   distinct from the deterministic `Object[]`-vs-`Type[]` half of bug-05 (that half is fixed).

**Leverage:** this single race is the dominant source of the run's *non-deterministic* FAIL/CRASH/
LOADERR/TIMEOUT noise under 4-way load (it inflates those counts well above the true per-class bug
count). It is the **highest-leverage remaining bug** in the Spring suite — one GC fix would clear a
large slice of the apparent failures at once. The fix is architectural (the young-heap array/String
header-walker mis-parse + the unrooted-live-ref-under-multithreaded-GC gap already analysed above);
this entry is the suite-scale field evidence that it has NOT regressed away on current dev.

