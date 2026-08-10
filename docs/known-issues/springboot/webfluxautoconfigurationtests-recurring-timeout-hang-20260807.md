# `WebFluxAutoConfigurationTests` — recurring full-timeout HANG: **not a hang**. Measured: no stall, the JIT buys 17%, and lambda SAM dispatch costs ~220x a named-class call

**Status: root-caused as a throughput defect, 2026-08-09. The class does not
stall** — it completes all 70 tests, every time it is given enough budget. Its
cost is flat per test method, and half of it sits under Spring's annotation
machinery. The reason the JIT cannot rescue it is a specific, reproducible VM
defect: **invoking a lambda's SAM costs ~4,200 ns against ~18 ns for the
identical interface call on a named class**, so a lambda-dense workload runs in
interpreter-side Rust dispatch machinery that compiled code never enters.

The remaining work is that dispatch defect, which is **not specific to this
class** — it is suite-wide. See [What is left](#what-is-left).

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

## Root cause: lambda SAM dispatch is ~220x a named-class interface call

A diffuse profile over lambda-dense code plus a JIT that buys 17% points at the
call mechanism rather than the call*ees*. Measured directly (`LambdaProbe2`),
with each arm on its **own monomorphic call site** and every arm measured in
**both orders** so neither call-site polymorphism nor warm-up can explain the
result:

| Call shape (empty body) | HotSpot | CratonVM |
|---|---:|---:|
| `invokevirtual`, final class | 2 ns | 15-18 ns |
| `invokeinterface`, named class | 1-2 ns | 18-20 ns |
| `invokeinterface`, anonymous class | 0-3 ns | 14-20 ns |
| **`invokeinterface`, lambda** | **2-10 ns** | **4169-4228 ns** |

CratonVM's ordinary dispatch is healthy — 15-20 ns for virtual, interface, and
anonymous-class calls alike, all within a factor of 10 of HotSpot. The lambda
row is **207x-226x** the named-class row *in the same process*, and HotSpot has
no such penalty. Because the ratio is measured inside one run, host load cannot
produce it.

A second probe separates dispatch from body compilation: with a 200-iteration
integer loop as the body, the named-class arm costs 413 ns of body and the
lambda arm 1311 ns — a 3.2x body penalty on top of a ~250x *dispatch* penalty.
The overwhelming term is fixed per-call dispatch overhead, not an uncompiled
body.

This matters for Spring specifically because Spring's context startup is
lambda-dense — `ConcurrentReferenceHashMap.computeIfAbsent`, the
`Supplier`/`Function` callbacks throughout `AbstractBeanFactory.doGetBean`,
`AutowiredAnnotationBeanPostProcessor.lambda$buildAutowiringMetadata$1`,
`InitDestroyAnnotationBeanPostProcessor.lambda$buildLifecycleMetadata$0`,
`PropertiesPropertySource.lambda$getPropertyNames$0` — all of which appear by
name in this class's own profile.

`try_lambda_dispatch` (`vm/src/runtime/interpreter/lambda.rs`) is where the
overhead lives. It is not on the compiled path at all: a `--stack-sample-ms 200`
run of the lambda probe serviced **2** sample requests over a run of tens of
seconds, i.e. the thread is essentially never at an interpreter safepoint —
it is inside Rust dispatch machinery. Reading that function offers several
candidate terms (the `LambdaCallSite` clone under the `lambda_proxies` lock,
`coerce_lambda_args`, the by-name `invoke_shared` class resolution taken once
per call, and the target invoke itself), and source-reading cannot rank them —
so `CRATONVM_DBG=lambda-prof` now measures the total and the parts in the same
run.

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

## What is left

The lambda dispatch defect, which is **suite-wide, not this class's**. Every
timing row above is a consequence of it; fixing it is not a `WebFluxAutoConfigurationTests`
change and should not be tracked under this class's name.

Reproduction is a single self-contained probe, no Spring involved: compile
`LambdaProbe2` (four call shapes, monomorphic sites, both orders) and run it on
both VMs. The lambda row is the defect; every other row is the control.

Next step for whoever picks it up: run any lambda-dense workload with
`CRATONVM_DBG=lambda-prof`, which prints `total / lookup / prep / target / other`
ns-per-dispatch every 200k dispatches, and fix the term it names. Do **not**
change `try_lambda_dispatch` on the strength of reading it — that function
carries a long list of named correctness regressions in its own comments
(`ProcessInfoTests.memoryInfoIsAvailable`, `NoSuchMethodFailureAnalyzerTests`,
Flink's `getRawValueFromOption`, Spring Data's duplicate `lambda$new$2`, the
Scala `JFunction` ping-pong), each caused by a change to how it picks a target.

## Affected classes

- `module/spring-boot-webflux` — `org.springframework.boot.webflux.autoconfigure.WebFluxAutoConfigurationTests`
  (70 tests, 70 Spring contexts; nothing about the class itself is unusual — it
  is simply large enough to make the per-lambda cost visible against a fixed
  300s budget)
