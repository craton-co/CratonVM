# `QuartzEndpointWebIntegrationTests` issues ~24,000 HTTP requests where `--nojit` issues ~22 — a JIT-only spin loop, OOM-killed as a side effect

**Status: OPEN, reproduced 2026-08-18 on `dev` `24a5d4528` (Azure Linux). Not
fixed. The root cause is NOT isolated, but it is now correctly framed: this is a
poll loop that never observes its condition under the JIT, not a memory leak.
Four hypotheses tested and refuted, three JIT kill switches tried and cleared —
all recorded below so nobody re-runs them.**

Found re-measuring the four classes on
retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md. That
page records this class as green under the shipped default. It is not, on Linux,
on current dev — and the failure is not that page's mechanism: there are **zero**
`[moving-young]` fallback lines in the run.

## What it actually is

The class runs its request against the Quartz actuator endpoint over and over,
forever. Counted by instrumenting the throwable/frame capture
(`CRATONVM_DBG_STTRACE=1`) and tallying `QuartzEndpoint.triggerQuartzJob`
frames:

| arm | `triggerQuartzJob` frames | window | outcome |
|---|---:|---|---|
| `--nojit` — **the whole passing run** | **66** | ~110 s | ✓45/45 |
| JIT on | **47,362** | 22 s | OOM-killed |

23,681 separate captured traces carry exactly **three** `triggerQuartzJob`
frames (`QuartzEndpointWebExtension:97` → `QuartzEndpoint:227` →
`QuartzEndpoint:231`) and none carry more, so this is **not** runaway recursion —
it is ~24,000 *separate, complete HTTP requests* through a three-deep call nest
that should run about 45 times.

The logs are quiet: 4 `IllegalStateException`s in the whole run and no repeated
exception. The requests are not failing and being retried by an error handler;
something is polling for a condition it never sees. `--nojit` sees it.

**The 22 GB is a symptom, not the bug.** Each iteration captures a stack trace
(up to 240 frames), so RSS climbs ~350 MB/s until the kernel OOM-killer takes
the process — 21.9 GB unconstrained, ~24 s under an 8 GB cgroup cap. That is why
`--Xmx` has no effect (the Java heap reads ~1 MB throughout) and why all three
collectors behave identically. Stop the loop and the memory goes with it.

**Run it under a cap.** This is a shared box and an unconstrained arm will OOM
other people's work:
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0 …`

## Measured

One class per process, `--Xmx 2g`, real JDK 25 backend, the three load-bearing
suite env vars, RSS sampled every 2 s.

| arm | peak RSS | outcome |
|---|---:|---|
| HotSpot 25 | **385 MB** | ✓45/45 in 8 s |
| CratonVM, ZGC (shipped default) | 7,867 MB | OOM-killed @ ~24 s |
| CratonVM, `--XX:UseGc G1` | 7,491 MB | OOM-killed @ ~24 s |
| CratonVM, `-XX:+UseGenerationalGC` | 7,998 MB | OOM-killed @ ~24 s |
| CratonVM, `--nojit` | 2,741 MB | **✓45/45** in ~110 s |

Collector-independent, so not a GC bug. `--nojit` clears it, so it is the JIT —
which is the same thesis the retired page reached for these classes by a
different route.

## 2026-08-18, later: the exception traffic is NOT the signal, and a warning about the instrument

An earlier pass on this page reported "18,069 `NullPointerException`s, 18,066 of
them thrown at `TomcatEmbeddedWebappClassLoader.loadClass`". **That number is an
artefact and both halves of it are wrong.** It came from pairing a debug line
that printed the throwable's class with a *separate* debug line that printed
frames. Several threads capture concurrently, `eprintln!` interleaves, and
pairing "the last class line" with "the next frame line" attributes frames to
the wrong throwable. The same defect in the opposite direction made the frame
list read outermost-first, so "the deepest frame" was the thread entry point.

With class and frames emitted on ONE line (`STTRACE_DBG_TOP`, three commits on
this branch), the real distribution over 25 s is:

| throwable | count | where |
|---|---:|---|
| `NoSuchMethodException` | 1,324 | Jersey `AnnotatedMethod.findAnnotatedMethod`, Spring `DisposableBeanAdapter.inferDestroyMethodsIfNecessary` |
| `ClassNotFoundException` | 85 | ordinary optional-dependency probing |
| `NoSuchBeanDefinitionException` | 70 | ordinary Spring wiring |
| `NullPointerException` | **2** | `ModuleDescriptor.modsHashCode` during a `<clinit>` |

Every one of those is **ordinary framework reflection** that HotSpot also
performs. There is no exception storm and nothing is failing in a loop. The
stack-trace capture cost is real (it is what turns the spin into 22 GB of RSS)
but it is a consequence of the iteration count, not a cause.

**A correct-looking number from a wrongly-paired log is worse than no number.**
Both mis-pairings produced plausible, confident, specific answers — a named
class and a named throw site — and sent the investigation at
`getClassLoadingLock`, which a 20-line probe then cleared on all three arms
(`ClassLoadingLockProbe`: 400,000 iterations, zero null locks on HotSpot,
CratonVM+JIT and CratonVM `--nojit`).

### What the watchdog dump adds

`--stack-dump-on-timeout 20` while spinning: all four `reactor-http-nio-N`
threads — the `WebTestClient`'s own Netty event loops — are `blocked=true` in
`NioIoHandler.select`, i.e. **the client is idle, waiting for I/O**, while the
server side keeps executing. Whatever is iterating is on the server, not a
client retry loop.

## NAMED, 2026-08-18: `quartzTriggerJobWithUnknownJobKey`, the WebMvc variant only

`@WebEndpointTest` is a parameterized template — 15 methods x 3 web-server
variants = the 45 tests — so `DiscoverySelectors.selectMethod(fqcn, name)`
cannot address one (it returns `tests=0 containersFailed=1` for every method).
`probes/SbRunnerTrace.java` (added with this; the fixture copy lives in `sb-runner/`) registers a
`TestExecutionListener` that prints every test as it starts and finishes,
flushed per line so a kill still leaves the last `@@START` behind:

```
@@DONE  SUCCESSFUL WebFlux
@@START Jersey  | …[test-template:quartzTriggerJobWithUnknownJobKey(WebTestClient)]/[test-template-invocation:#1]
@@DONE  SUCCESSFUL Jersey
@@START WebMvc  | …[test-template:quartzTriggerJobWithUnknownJobKey(WebTestClient)]/[test-template-invocation:#2]
                                    <- never finishes; 5 started, 4 finished
```

HotSpot runs all 45. **CratonVM hangs on the WebMvc invocation of
`quartzTriggerJobWithUnknownJobKey`, and the WebFlux and Jersey invocations of
the SAME method pass.**

The test is four lines, and so is the server path it drives:

```java
client.post().uri("/actuator/quartz/jobs/samples/does-not-exist")
      .contentType(MediaType.APPLICATION_JSON).bodyValue(Map.of("state","running"))
      .exchange().expectStatus().isNotFound();
```
```java
// QuartzEndpointWebExtension:97 -> QuartzEndpoint:227 -> :231
JobDetail jobDetail = this.scheduler.getJobDetail(jobKey);   // mock -> null
if (jobDetail == null) { return null; }                      // -> handleNull -> 404
```

So the request that spins is: **a POST carrying a JSON body, whose handler
returns 404 without ever reading that body, under Spring MVC.**

### What the counts say about the loop

From the same run: `DispatcherServlet.doService` **23,683**,
`RequestMappingHandlerAdapter.invokeHandlerMethod` **23,683**,
`Http11Processor.service` **24,870**, `NioEndpoint$SocketProcessor.doRun`
**24,871** — while the client's four `reactor-http-nio` event loops sit
`blocked=true` in `NioIoHandler.select`. One client request, ~23,700 complete
server-side dispatches. Tomcat is being handed the same socket over and over and
parsing a request from it each time.

**It is not an error dispatch.** `ErrorPageFilter`, `RequestDispatcher`,
`ApplicationDispatcher`, `forward`, `BasicErrorController` and `/error` all
appear **zero** times in the captured frames.

**And it is not raw selector readiness.** `probes/SelectorReadinessProbe.java`
(one client, one 5-byte write, then silence) reports
`readableWakeups=1 reads=1 bytesTotal=5 zeroByteReads=0` identically on HotSpot,
CratonVM+JIT and CratonVM `--nojit`. A drained non-blocking socket stops being
reported readable, correctly, on every arm.

### And the spinning thread is `main`, not a server thread

`--stack-dump-on-timeout 20` reports `tid=0 name="main"` as
`deposit=STALE — thread is RUNNING (blocked=false)`, while every
`reactor-http-nio` client loop is `deposit=live` and parked in
`NioIoHandler.select`. So the thread burning CPU is the **test** thread, and the
~23,700 server dispatches are being *driven* by it, not spontaneously generated
by Tomcat.

That matters because it redirects the search: the loop is on the JUnit/
`WebTestClient` side of `exchange()`, not inside the servlet container. The
watchdog cannot show where — a RUNNING thread's printed chain is its last
*blocking* site, which is stale by definition, and `--stack-sample-ms 25`
produced no sample records on this build, so the sampler needs checking before
it can be relied on here.

The untested hypothesis that fits what is left: an **unconsumed request body**.
Tomcat must swallow the bytes a handler never read before it can treat the
connection as ready for the next request; if that drain does not advance, the
leftover body is re-parsed as a new request forever. It explains the
WebMvc-only failure (Jersey and WebFlux consume the entity), the body-carrying
POST, and the 404-without-reading-the-body path. Next step: drive that exact
shape — POST with `Content-Length`, handler returns without reading — against a
socket and check whether the leftover bytes are drained.

## The shape to look for

A spin/poll loop whose condition is written by one thread and read by another:
the request runs on `http-nio-auto-N`, the assertion waits on `main`. The
classic compiled-code failure for that shape is a **non-volatile field read
hoisted out of the loop** (or otherwise cached in a register), so the waiter
never observes the writer's store. That is consistent with every observation
here — JIT-only, collector-independent, no exception, unbounded iterations —
but it is **not confirmed**, and the loop that spins has not been identified in
the Java source yet.

Finding which of the 45 tests spins is harder than it looks and one route is
already closed: `@WebEndpointTest` is a parameterized *template* (15 methods x 3
web-server variants = 45 tests), and `DiscoverySelectors.selectMethod(fqcn,
name)` does not address a template — `SbRunnerMethod` returns
`tests=0 containersFailed=1` for every one of them. The next pass needs either
a `MethodSource`-aware selector (the parameter type must be in the selector) or
a JUnit `TestExecutionListener` that prints each test's start and finish, so the
last one to start can be named.

With the test named, the remaining question is what its server-side handler
iterates on — and the client being idle in `select` says the answer is on the
server.

## Refuted — do not re-run these

Each hypothesis was tested with a standalone probe reproducing the suspected
shape in isolation. All stay flat, so none is the mechanism.

1. **"The throwable stack-trace side table is never pruned."** The natural
   suspect once the trace-capture symbols showed up in the profile: the frames
   are parked in a VM-wide identity-hash-keyed store that only a collection
   prunes, and Throwables are tiny so the heap-occupancy trigger never fires.
   `ThrowNoGcProbe` — 50,000 throws at depth 200 with **no** `System.gc()` —
   holds flat at **317 MB**. The store is swept.
2. **"Classloader churn leaks metadata."** `LoaderChurnProbe` — 240 throwaway
   `URLClassLoader`s over the same 186 real jars — grows 464 → 508 MB, ~0.2 MB
   per loader. Real, small, two orders of magnitude short.
3. **"A JIT recompile loop."** `CRATONVM_DBG_JITC=1` over 30 s: **1,358 compiles
   of 1,280 distinct methods**, most-recompiled method 25 times. Ordinary
   warm-up volume.
4. **"Per-thread VM state is retained at thread exit."** Real but **bounded**:
   `ThreadOnlyProbe` shows ~85 KB per exited-and-joined thread (HotSpot flat at
   48 MB over 2,000 threads, CratonVM 268 → 435 MB) — but it **plateaus**, with
   18,000 threads levelling in a 500-800 MB band and falling back. Worth its own
   look; not 5 GB.

Three JIT kill switches were tried and none clears it (all still spin past a
200 s cap): `CRATONVM_DISABLE_AALOAD_LICM`, `CRATONVM_DISABLE_ARITH_LICM`,
`CRATONVM_JIT_GETFIELD_HELPER`.

Two dead ends on instrumentation, recorded to save the next person the time:
`heaptrack` cannot see this — the binary uses **mimalloc** as its global
allocator and mimalloc goes to `mmap` directly, so libc-malloc interception
records nothing. And a `perf record -e page-faults` profile attributes 21.6% to
mimalloc internals on the request thread with **no resolvable Rust caller**;
frame-pointer unwinding does not reach past them. Sampling says where allocation
happens, which was the wrong question anyway.

## Reproduction

```bash
# fixture: apps/spring-boot, built test classes + per-module cratonvm-test-cp.txt
# /data/sb4.sh and /data/sbrss.sh on the Azure Linux box wrap the exact launch
# run-spring-boot-suite.ps1 uses (three env vars, --stack-dump-on-timeout 0,
# --add-opens=java.base/java.net).

MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz \
  org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests q 400

# the passing control
MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz <same class> q 400 --nojit

# count the loop rather than watching the memory — this is the load-bearing number
CRATONVM_DBG_STTRACE=1 <cratonvm> … 2>&1 | grep -c 'QuartzEndpoint.triggerQuartzJob'
```

## Related

- retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md —
  the page this was found from. Same class, different mechanism: that one is a
  `[moving-young]` fallback spiral under Generational, and this run logs none.
  Its thesis that the JIT is what breaks these classes survives.
- `docs/known-issues/gc/generational-young-relocation-nulls-live-string-references-20260818.md`
  — the other finding from the same re-measurement.
