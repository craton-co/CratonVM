# `WebFluxAutoConfigurationTests` — recurring full-timeout HANG: **not a hang**, and not one dominant term

**Status: RETIRED — 2026-08-10** (branch `docs/retire-webflux-throughput-20260810`).
Supersedes `docs/known-issues/springboot/webfluxautoconfigurationtests-recurring-timeout-hang-20260807.md`.

Retired because **every question this page asked has been answered and every
lead it generated has been closed by measurement** — not because the class is
fast. It is still ~10-30x HotSpot and can still exhaust a 300s budget under
load; what is retired is the *investigation*, which had become a generator of
plausible mechanisms that measurement kept refuting.

| lead | headline | measured worth |
|---|---|---|
| compile refusals | "69 hot methods refused" | 63 were mislabelled policy verdicts; **7** real ([FIXED](#1-the-jit-is-asked-and-refuses-on-exactly-the-hot-annotation-methods)) |
| native-shadow seal | "1,279 sealed > 1,155 at C2" | removing the seal entirely: **0** |
| annotation/proxy path | "~1000x HotSpot per call" | **~1%** of the run |
| lambda SAM dispatch | "220x a named-class call" | fixed, ~4x on the mechanism, **0%** here |
| OSR-only loops | "7-16x" | **180x**, split out to `../../fixed-bugs/osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md` |

**If this class goes red again, it is a budget/throughput event, not a new
defect** — start from the tables below rather than re-triaging. The one live
item to come out of all this is the OSR page above, and it is a measurement
hazard rather than this class's problem.

**What is left is breadth, not a mechanism**: ordinary Java throughput across
Spring's own code. Three independent ceiling measurements failed to contradict
that, and the sampling profile said it from the start (largest single leaf 3.9%).

---

The class does **not** stall — it completes all 70 tests every time it is given
enough budget, its cost is flat per test method, and 99.1% of stack-sample
requests are serviced across a full run. It is ~10-30x HotSpot on a workload
that is 70 full Spring context startups, which is what makes a fixed 300s budget
a coin flip.

One concrete VM defect was found and fixed while investigating this
(lambda SAM dispatch, ~4x), and it **did not move this class at all** — see
[A fix that did not help](#a-fix-that-did-not-help-recorded-so-nobody-re-runs-it).
That is the most useful thing on this page: the cost here is diffuse, and the
next person should not expect a single term to explain it.

## Answers to the two questions this page asked

> *"whether this is purely a throughput/margin problem … or a genuine
> intermittent stall specific to this class's context setup"*

**Throughput. There is no stall.** A `--stack-sample-ms 500` run over the whole
409s serviced **811 of 818** possible sample requests — 99.1%. A stall shows up
in that number as a gap, because a thread that is stuck never reaches the
interpreter's sample hook. There is no gap anywhere in the run, including in
the "final ~3 minutes of silence" the original triage flagged: silence in the
log is the *absence of a test that logs*, not the absence of progress.

Cost is also **flat per test method** — attributing every sample to the
enclosing `@Test` frame gives a top method of 25 samples (12.5s) against a
typical 13 (6.5s) across ~70 methods. Nothing is concentrated.

> *"get a HotSpot baseline timing for this specific class (none was found in
> this session's history search) to compute a throughput ratio"*

| VM | Seconds | Tests |
|---|---:|---|
| HotSpot 25.0.3.9 | **12.14** | 70/70 |
| HotSpot 25.0.3.9 (repeat, hours later) | **12.14** | 70/70 |
| HotSpot 25.0.3.9 (repeat) | **12.15** | 70/70 |
| CratonVM, JIT | 398.9 | 70/70 |
| CratonVM, `--nojit` | 478.0 | 70/70 |

Read the CratonVM rows with care: this box was shared and heavily loaded
throughout (other sessions running full suites), and HotSpot is visibly immune
to that — three runs across several hours agree to 0.01s — while CratonVM is
not. Against this page's own recorded clean-run range of 129-278s the ratio is
**10.6-22.9x**; the loaded measurements put it at 32.9x. Either way the class is
an order of magnitude off, which is what makes a fixed 300s budget a coin flip.

**The JIT is worth 17%.** 398.9s with it, 478.0s without, on a workload that is
70 full Spring application-context startups. That single number is the finding:
whatever this class spends its time on is not something compiling Java bodies
can help with.

## Where the time actually goes

811 samples, attributed *inclusively* (a bucket counts a sample if any frame on
the stack matches):

| Bucket | Share |
|---|---:|
| `springframework/beans/factory` | 57.5% |
| `springframework/core/annotation` + `core/type` | 50.7% |
| `springframework/context/annotation` | 38.7% |
| `springframework/boot/context/properties` | 17.1% |
| `java/lang/reflect` + `jdk/internal/reflect` + `jdk/proxy*` | 12.6% |
| class-file parsing (`jdk/internal/classfile`, ASM) | 6.4% |
| **class loading** (`loadClass`/`forName`/`defineClass`) | **0.1%** |

Class loading is not the cost — worth stating plainly, because it is the usual
suspect for a slow Spring startup and it is refuted here. The *self*-time
histogram is correspondingly diffuse: the largest single leaf is
`ConcurrentReferenceHashMap$Segment.getReference` at 3.9%, and the top 20 leaves
are all Spring annotation-merging or bean-factory methods. There is no hotspot
to fix in the ordinary sense.

## A fix that did not help, recorded so nobody re-runs it

A diffuse profile over lambda-dense code plus a JIT worth 17% looks exactly like
"the call mechanism, not the callees". That hypothesis was followed all the way
to a real, fixed defect — and the defect turned out **not** to be this class's
problem.

Measured directly (`LambdaProbe2`), with each arm on its **own monomorphic call
site** and every arm measured in **both orders** so neither call-site
polymorphism nor warm-up can explain the result:

| Call shape (empty body) | HotSpot | CratonVM |
|---|---:|---:|
| `invokevirtual`, final class | 2 ns | 15-18 ns |
| `invokeinterface`, named class | 1-2 ns | 18-20 ns |
| `invokeinterface`, anonymous class | 0-3 ns | 14-20 ns |
| **`invokeinterface`, lambda** | **2-10 ns** | **4169-4228 ns** |

CratonVM's ordinary dispatch is healthy — 15-20 ns for virtual, interface, and
anonymous-class calls alike, all within a factor of 10 of HotSpot. The lambda
row was **207x-226x** the named-class row *in the same process*, and HotSpot has
no such penalty. Because the ratio is measured inside one run, host load cannot
produce it.

`CRATONVM_DBG=lambda-prof` (added for this) then priced the phases of
`try_lambda_dispatch` in the same run: of ~3,600 ns per dispatch of an **empty**
lambda, **~2,820 ns was the target invoke** — and with a no-op body, that is all
resolution — against ~145 ns for the proxy-table lookup and ~290 ns for argument
coercion. The cause was `try_invoke_cached_lambda_impl`'s
`if method.is_static() … return Ok(None)`: javac compiles a **non-capturing**
lambda body to a private *static* synthetic method, so the single most common
lambda shape in Java was refused by the very fast path built for lambda impls
and fell back to a by-name invoke on every call.

**Fixed** — statics are now cacheable (the frame builder was already
receiver-agnostic), plus a per-proxy memo for the four `class_manager.write()
.load_class(name)` sites that were taking the VM-wide class-manager **write**
lock once per dispatch. Result on the probe: **2080-3260 ns → 638-674 ns**, a
3.5-5x improvement, with the run-to-run spread collapsing too.

**And it changed this class by nothing.** Interleaved, HotSpot-bracketed:

| Arm | Seconds |
|---|---:|
| HotSpot | 10.12 |
| CratonVM, before the lambda fix | 323.1 |
| CratonVM, after | 312.7 |
| CratonVM, after (repeat) | 327.9 |
| HotSpot | 10.12 |

70/70 in every arm; the spread is noise. So lambda dispatch is a genuine ~4x VM
defect **and** it is not this workload's dominant term — the inference from
"diffuse profile + JIT worth 17%" to "the call mechanism" does not survive its
own A/B. Recorded because the reasoning is seductive and the experiment is
expensive to repeat.

The lambda work still lands on its own merits (2460/2460 `vm --lib`, six Spring
classes byte-identical before and after), but it is not a fix for this page.

## What this is not

- **Not a stall or a deadlock.** 99.1% sample coverage; all 70 tests pass.
- **Not GC.** The `[moving-young] fallback: reason=cross-thread-jit-peer` line
  the original triage noticed is a handled quiescence fallback that appears in
  clean runs throughout this suite; the profile puts no meaningful share in
  collection.
- **Not class loading**, at 0.1% of samples.
- **Not the load-time class-transform rescan** fixed on 2026-08-09
  (`fixed-suite-bugs/springboot/devtoolsembeddeddatasourceautoconfigurationtests-load-time-transform-rescan-FIXED.md`).
  That defect only reaches classes loaded through a *user* loader; this class
  carries no `@ClassPathExclusions` and runs on the application loader, where
  that hook's already-defined early-out fires normally. Verified: the class
  behaves identically before and after that fix.
- **Not "the same shape as `ZipContentTests`"**, which this page grouped itself
  with. That one is a disk-capacity failure (it needs ~15 GB free), not a
  throughput ceiling.

## Where the gap actually is (2026-08-10 measurement pass)

Three measurements, each answering one of the questions the section above left
open. None of them is "the class is doing something wrong".

### 1. The JIT is asked, and refuses, on exactly the hot annotation methods

`CRATONVM_DBG=jit-method-stats` on a full run (202s on a quiet box):

```
1571 distinct methods tracked, 1570 ever invoked, 22,960,010 total invocations
still-interpreted=80  c1=335  c2=1156
compiles: c1=1552 c2=1158 osr=1 deopts=220 c2_bailouts=136 total_compile_time_ms=166
hot_but_stuck_in_interpreter=79 (ineligible-by-policy=0, compile-failures=69)
```

So the JIT is not failing to *reach* this workload — 1156 methods make C2, and it
spends 166 ms doing it. But **69 hot methods asked the compiler and were
refused**, then hit `tier_fail_count=3` and were bail-listed permanently. The
list is the annotation machinery the sampling profile already pointed at:

| invocations | method |
|---:|---|
| 269,876 | `BeanFactoryUtils.transformedBeanName` |
| 93,108 | `AnnotationTypeMappings.forAnnotationType` |
| 39,412 | `TypeMappedAnnotations$IsPresent.doWithAnnotations` |
| 31,988 | `AttributeMethods.forAnnotationType` |
| 21,684 | `TypeMappedAnnotation.createIfPossible` |
| 7,860 | `AnnotatedElementUtils.findMergedAnnotation` |

**26 of the top 30 reported `reason=unrecorded`.** That is now resolved, and the
answer was not the one this page assumed — see the next section.

**Update 2026-08-10 — 63 of the 69 "compile failures" were never compiled at
all.** They were **policy verdicts wearing a codegen failure's label**.

`try_jit_compile_wrapped_entry` returns `None` for several unrelated reasons,
and its own comment lists them: *"skip-listed, resolver miss, code-cache cap,
concurrent redefine, ..."*. Two of those are permanent policy, not a codegen
attempt that failed — but the `CompileOutcome` hardcoded
`declined_permanently: false`, so `CompilerCore::complete_task` spent
`tier_fail_count` on every one of them. Three futile background compile tasks
per method, then the method is retired and reported as `compile-failed
reason=unrecorded` — unrecorded *precisely because nothing ever ran to record a
bail site*. `MethodState::ineligible` is the field that exists for this case,
and `complete_task` already honours it; it was simply never told.

Fixed by reporting a `None` as `declined_permanently` when the method is in the
skip-set or on the permanent bail-list. Same class, same workload:

| | before | after |
|---|---:|---:|
| `hot_but_stuck_in_interpreter` | 79 | 81 |
| ...of which **ineligible-by-policy** | **0** | **63** |
| ...of which **compile-failures** | **69** | **7** |
| C1 compiles | 1552 | 1571 |

So there were never 69 compiler bugs here — there are **7**, and those do carry
real bail sites (`rbc6-handler-reads-unsafe-local` and friends). The other 63
are the skip-seal policy, which is the finding below. Wall-clock is unchanged
(240.97s → 240.79s), as it should be: this corrects a label and stops wasted
compile tasks, it does not make anything faster.

A stage-fallback was added at the same time (`no-site-after-<stage>`, stamped by
each pipeline phase) so that a *genuine* silent refusal can never report
`unrecorded` again. It did not fire on this workload — which is itself what
proved no compile was running for those 63.

### 1b. 2,498 methods are sealed out of compilation before any attempt

The seal census, now split by reason (it was one label,
`static-policy-or-native-shadow`, covering four different verdicts):

```
JIT skip-seal census: 2498 method(s) sealed before any compile
  | calls-native-shadowed-method=1279  clinit=1219
```

**1,279 methods are excluded from the JIT because they call a method that has a
native shadow** — more than the 1,155 that reach C2 in the same run. Nothing
from the static policy table, nothing from the `ForkJoinTask` rule. This is the
largest single population touching the gap and it was invisible before today.

### 1c. …and it is worth nothing. Measured, 2026-08-10.

That headline — "more sealed than compiled" — is exactly the kind that gets a
large compiler change funded. It was tested first, in two steps, and it does not
survive either.

**Step 1: which arm fires?** The predicate has three, and they do not have equal
standing: `direct` and `inherited` are precise facts about the call, while
`interface_blind_possible_shadow` is a class-blind *"does ANY registered native
have this (name, descriptor)"* probe whose own comment concedes it "can only ever
ADD conservatism". The obvious suspicion was that the class-blind arm was
over-matching common signatures (`get()`, `size()`, `equals`) in Spring code.

```
JIT native-shadow verdicts by arm: direct=1015  interface-blind=169  inherited=81
```

It is not. The class-blind arm is 13%. Suppressing it
(`CRATONVM_JIT=-native-shadow-interface-blind`) frees 126 methods and moves 9
more to C2 — and moves wall clock not at all: 228.6s / 269.6s with it against
256.6s / 266.8s without, interleaved and HotSpot-bracketed. The seal is dominated
by **precise, correct** hits: Spring really does call natively-shadowed JDK
methods nearly everywhere.

**Step 2: price the ceiling.** If narrowing the detection is not the lever, the
only remaining one is the *granularity* — the seal disqualifies the whole caller
rather than just refusing the direct-call optimisation at that one site — and
rebuilding it per-site is a large, correctness-critical compiler change. So
rather than build it, price its best case by removing the seal entirely
(`CRATONVM_JIT=-native-shadow-caller-seal`, a measurement-only lever; it
disables a correctness guard and must never ship):

| | seal on | seal off |
|---|---:|---:|
| sealed for `calls-native-shadowed-method` | 1281 | **17** |
| methods reaching C2 | 1156 | **1212** |
| wall clock | 245.97s | **253.92s / 249.73s** |
| tests | 70/70 | 70/70 |

**Freeing 1,264 methods to compile buys nothing** — if anything it is marginally
slower, since the extra compiles are not free. The ceiling on this lead is zero,
so the per-site rebuild should not be attempted for this workload's sake.

Why zero is consistent with everything else on this page: the profile is diffuse,
the JIT already reaches 1,156 methods in 166 ms, and compiling 56 more changes
nothing because the time is not in the methods the seal was holding back. The
largest measured single term remains the annotation-proxy entry path (~94% of a
~14 us attribute read), which no amount of tier-up touches.

`SealProbe` remains as the shape probe: a JDK-leaf call costs ~50 ns/iter against
8 for pure integer work, and `String.charAt` ~460. That per-call cost is real —
it is simply not paid for by compiling the *caller*.

### 2. Annotation *attribute reads* cost ~10.5 us — and 94% of that is not where it looks

`ReflProbe2` (each accessor in its own small, tiering method — see the artifact
warning below), CratonVM against HotSpot, net of each VM's own control:

| accessor | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `Marker.value()` (annotation attribute) | ~10 ns | **~10,400 ns** | **~1000x** |
| `Marker.count()` (annotation attribute) | ~17 ns | **~11,200 ns** | **~660x** |
| `Class.getDeclaredMethods()` | 57 ns | 4,830 ns | 85x |
| `Annotation.annotationType()` | 9 ns | 1,350 ns | 150x |
| `Method.getDeclaredAnnotations()` | 16 ns | 1,610 ns | 100x |
| `Class.getDeclaredAnnotations()` | 23 ns | 1,150 ns | 50x |
| `Method.getModifiers()` | ~0 ns | 475 ns | — |

Attribute reads are 7x worse than any neighbouring accessor, which makes them
the one outlier worth chasing on this surface. But `CRATONVM_DBG=ann-proxy-prof`
(added for this) shows `annotation_proxy_dispatch_impl` — the element walk that
looks like the obvious culprit — costs only **~750 ns/call of the ~14,000 ns**.
**~94% is the entry path** from the Java call site through the generated
`$ProxyN` body and the native proxy dispatch, *before* the dispatcher runs.
Inside the dispatcher the split was `flagread=155 namecmp=200-290 rest=260`; the
flag read (an environment lookup per element access, for a value that cannot
change after startup) is now latched — but that is a 2% fix on a 6% component,
and A/B'ing it confirmed exactly that: 15798/15993 ns before against
15665/14887 ns after, i.e. nothing outside the noise. It stays because a
per-call `getenv` on a dispatch path is wrong regardless of payoff, not because
it bought anything.

### 3. A measurement artifact worth keeping: OSR-only loops run ~10x slower

The first version of the reflection probe put every loop inline in `main` — a
method called once, so its loops are reachable only by OSR. `FloorProbe`, three
identical loops in each shape:

| | small method, called often | inline in `main`, run once |
|---|---:|---:|
| loop arithmetic only | 8 ns/iter | **131 ns/iter** |
| + static call | 34 ns/iter | **314 ns/iter** |
| + static field read-modify-write | 105 ns/iter | **745 ns/iter** |

HotSpot: 2/0/0 against 2/1/1 — no difference at all. So on CratonVM the same
loop is **7-16x slower** when it can only be reached through OSR, and the first
probe's ~700 ns "floor" was that, not reflection. Every ratio it produced was
wrong and has been recomputed above. This is a real finding in its own right and
does not belong to this class; it is filed here only because it is where it was
found.

## 4. The annotation-proxy entry path — also worth ~1%. Measured 2026-08-10.

The remaining lead was the ~94% of a ~14 us attribute read that sits *before*
`annotation_proxy_dispatch_impl`. Two measurements settle it.

**Where the entry cost splits.** `ProxyProbe` compares a plain
`java.lang.reflect.Proxy` with a trivial hand-written `InvocationHandler`
against an annotation proxy, arm-A shape, net of each VM's own control:

| | HotSpot | CratonVM |
|---|---:|---:|
| direct interface call (control) | 8 ns | 760 ns |
| **plain** JDK proxy `.value()` | 10 ns | 6,126 ns |
| **annotation** proxy `.value()` | 14 ns | 16,175 ns |

So ~35% of the annotation cost is **generic dynamic-proxy dispatch** — shared
with every JDK proxy, including Spring AOP's — and ~65% is annotation-specific.
Both are ~500-1000x HotSpot. The generated `$ProxyN` body allocates an
`Object[]` per call (`ICONST_0; ANEWARRAY`) and the native
`Proxy$Dispatch.invokeProxy` does a `class_name_of_id` String allocation, a
by-name field lookup on the `Method`, and a String materialisation of the member
name — all per call.

**And it does not matter here**, because the call volume is small:

```
[PROXY-DISPATCH-PROF] native invokeProxy calls=300000 total=9532ns/call cumulative=2859ms
```

~300-400k dispatches at ~9.5 us = **~2.9-3.8 s of a 357 s run, ~1%**. The
interpreter-side hook adds ~0.3-0.5 s. Making the entire annotation and proxy
machinery *infinitely fast* would save about three seconds.

**A measurement caveat worth keeping.** The first version of this count
instrumented only `annotation_proxy_dispatch_impl` and reported ~100k
dispatches — implying <0.5 s and a tidy conclusion. That was wrong by ~10x:
proxy dispatch is a **two-branch funnel** (the interpreter hook *and* the native
`invokeProxy` entry, which deliberately does not route through the other), and
counting one branch under-reports by exactly the factor that decides the answer.
Both branches are now counted under the same flag.

## Conclusion: there is no single term, and three leads have proved it

| lead | headline | measured worth |
|---|---|---|
| compile refusals | "69 hot methods refused" | 63 were mislabelled policy; 7 real |
| native-shadow seal | "1,279 sealed > 1,155 at C2" | removing it entirely: **0** |
| annotation/proxy path | "~1000x HotSpot per call" | **~1%** of the run |

Every per-call gap above is real and large. None of them is where the 20x lives,
because none of them happens often enough. What is left is ordinary Java
throughput across Spring's own code — the sampling profile says exactly that
(largest single leaf 3.9%; 50.7% under `springframework/core/annotation`, which
is Spring's *own Java classes*, not VM annotation calls) and three independent
ceiling measurements have now failed to contradict it.

**Anyone picking this up should stop looking for a mechanism and start on
breadth**: the interpreter and the compiled-code quality across ordinary
application bytecode. And they should keep pricing ceilings first — it has cost
one build each time and saved three large changes.

## What is left

A ~10-30x gap on Spring context startup with **no single dominant term**. What
is now known, and what the next attempt should not repeat:

- The profile is genuinely diffuse (largest leaf 3.9%), so leaf-chasing will not
  pay. The useful shape is the inclusive table above: annotation merging and
  bean-factory metadata, not I/O, not class loading, not GC.
- "It must be the call mechanism" was tested and refuted (above). Whatever else
  gets hypothesised, **A/B it on this class before believing it** — the
  microbenchmark that motivates a change and the workload that has to improve
  are different measurements, and here they disagreed by 4x versus 0%.
- Both previously-unmeasured angles are now measured (see the section above).
  The three leads they produced, in the order worth taking them:
  1. ~~Name the 26 `reason=unrecorded` compile refusals.~~ **DONE 2026-08-10** —
     63 of the 69 were policy verdicts mislabelled as codegen failures; 7 real
     ones remain, each with a named bail site.
  2. ~~The skip seal: 1,279 methods excluded for `calls-native-shadowed-method`.~~
     **CLOSED 2026-08-10, ceiling measured at zero** (§1c). Removing the seal
     entirely frees 1,264 methods, moves 56 more to C2, and does not improve wall
     clock. Do not rebuild it per-site for this workload's sake.
  3. ~~The annotation-proxy entry path.~~ **CLOSED 2026-08-10 at ~1%** (§4).
     ~300-400k dispatches at ~9.5 us = ~3 s of a 357 s run.
  4. ~~OSR-only loops at 7-16x.~~ **Split out 2026-08-10 to
     [`internal/fixed-bugs/osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md`](../../fixed-bugs/osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md)**, where it is
     reproduced at **180x** (1 ns/iter for a loop in a called method against
     180 ns/iter for the identical loop inline in `main`) and the refusal is
     named: `osr-entry-unresumable-exit`, from a deopt point in the method's own
     prologue that the OSR entry cannot reach. It is not this class's problem —
     WebFlux has no loop hot enough for OSR to matter — but it is a **measurement
     integrity** problem for every probe written with its loop in `main`, which
     is how two probes in this very investigation ended up measuring the
     interpreter.

  The pattern across all three closed leads: every headline number ("69 refused
  compiles", "more sealed than compiled", "1000x per call") looked like a cause
  and none was. Price the ceiling with a lever or a counter before building the
  fix — three times now that has cost one build and saved a large one.
- Do **not** change `try_lambda_dispatch` on the strength of reading it. That
  function carries a long list of named correctness regressions in its own
  comments (`ProcessInfoTests.memoryInfoIsAvailable`,
  `NoSuchMethodFailureAnalyzerTests`, Flink's `getRawValueFromOption`, Spring
  Data's duplicate `lambda$new$2`, the Scala `JFunction` ping-pong), each caused
  by a change to how it picks a target.

Tooling left behind for whoever picks this up: `CRATONVM_DBG=lambda-prof` prints
`total / lookup / prep / target / other` ns-per-dispatch every 200k dispatches,
and `--stack-sample-ms N` plus the sample-coverage count is what settled the
stall question in one run.

## Affected classes

- `module/spring-boot-webflux` — `org.springframework.boot.webflux.autoconfigure.WebFluxAutoConfigurationTests`
  (70 tests, 70 Spring contexts; nothing about the class itself is unusual — it
  is simply large enough to make the per-lambda cost visible against a fixed
  300s budget)
