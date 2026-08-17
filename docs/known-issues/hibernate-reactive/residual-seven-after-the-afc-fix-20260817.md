# The hibernate-reactive residual seven: two are this box, five are lambda dispatch

**Status: OPEN as ONE perf defect; two of the seven are NOT CratonVM defects.**
Filed 2026-08-17 on `dev` after
`asynchronousfilechannel-close-waits-for-the-read-it-cancels-20260816-FIXED`
took the Windows hibernate-reactive FAIL bucket from 238 classes to seven.

Nothing here is a hang, a deadlock, or a wrong answer. The five CratonVM-owned
classes **produce correct results** and are reported as FAIL/HANG only because
they exceed a timeout, and the reason they do has a single, measured cause:
**invoking a lambda / functional-interface method costs ~1.7–2.1 µs, 8–10x
HotSpot's INTERPRETER, while a plain static call on the same VM is 2–4x FASTER
than HotSpot's interpreter.** hibernate-reactive's reactive pipeline is
essentially nothing but functional-interface invocations.

## The split

| class | verdict |
|---|---|
| `ORMReactivePersistenceTest` | **this box** — HotSpot fails identically |
| `it.quarkus.qe.database.DatabaseHibernateReactiveTest` | **this box** — HotSpot fails identically |
| `MultithreadedInsertionTest` | lambda dispatch — PASSES with a raised timeout |
| `MultithreadedIdentityGenerationTest` | lambda dispatch — PASSES with a raised timeout |
| `MultithreadedInsertionWithLazyConnectionTest` | lambda dispatch — 1 of 2 methods; the other exceeds the FIXTURE's own budget |
| `it.LocalContextTest` | lambda dispatch — exceeds the fixture's own budget |
| `techempower.TechEmpowerTest` | lambda dispatch — exceeds the fixture's own budget |

## 1. Two are the host, not the VM

Both were run under stock HotSpot JDK 25 on the same box, with the same Docker
daemon, minutes apart. **Both fail there too, with byte-identical messages.**

### `ORMReactivePersistenceTest` — the box's time zone

```
org.hibernate.service.spi.ServiceException: Unable to create requested service
  [org.hibernate.engine.jdbc.env.spi.JdbcEnvironment] due to:
  Error calling Driver.connect()
  [FATAL: invalid value for parameter "TimeZone": "America/Buenos_Aires"]
```

The PostgreSQL JDBC driver puts `TimeZone.getDefault().getID()` in its startup
packet, so an ID the *server's* tzdata will not accept kills the connection
before any query runs. `probes/DefaultLocaleTimeZoneProbe.java` run under both
VMs returns ten identical values:

```
timezone.id=America/Buenos_Aires          <- IDENTICAL under HotSpot and CratonVM
zoneid.systemDefault=America/Buenos_Aires
prop.user.timezone=America/Buenos_Aires
locale.default=ru_RU
```

`America/Buenos_Aires` is the pre-2009 spelling; current tzdata carries it only
in the `backward` compatibility file, which the `postgres:18.4` image does not
install. HotSpot's own Windows→IANA table produces it, so this is not a
CratonVM mapping defect — it is a Windows host set to Buenos Aires talking to a
container built without `backward`.

### `DatabaseHibernateReactiveTest` — the box's display language

The test asserts the English Bean Validation message and the box is `ru_RU`, so
hibernate-validator resolves `ValidationMessages_ru.properties`:

```
interpolatedMessage='не должно равняться null'   expected: "must not be null"
```

Again identical under HotSpot. Both classes pass on the Azure Linux host, which
is UTC/`en`.

**Neither belongs in a CratonVM residual.** They are recorded here so the next
reader does not spend a session on them, and so a Windows run can be read
correctly: this box fails them under any JVM.

## 2. Five are one defect: functional-interface dispatch

### 2.1 They are not hangs

Raising only the JUnit timeout is enough for two of them, and the other three
get much further than the suite ever let them:

| class | HotSpot | CratonVM, timeout raised |
|---|---|---|
| `MultithreadedInsertionTest` | 18.5 s | **PASS 219.4 s** (11.9x) |
| `MultithreadedIdentityGenerationTest` | 13.5 s | **PASS 285.7 s** (21.2x) |
| `MultithreadedInsertionWithLazyConnectionTest` | 49.7 s | 1 of 2 methods PASS, 942.9 s |
| `it.LocalContextTest` | 10.0 s | 622.4 s, still cut off (>62x) |
| `techempower.TechEmpowerTest` | 12.9 s | 323.7 s, still cut off (>25x) |

The last three stop on the **Vert.x** deadline, not the JUnit one:

```
java.util.concurrent.TimeoutException: The test execution timed out. Make sure
your asynchronous code includes calls to either VertxTestContext#completeNow()…
```

That deadline is a constant in each fixture's own source — `@Timeout(value =
10, timeUnit = MINUTES)` for `LocalContextTest` and
`MultithreadedInsertionWithLazyConnectionTest`, `5` for `TechEmpowerTest` — and
`io.vertx.junit5` exposes no system property for it, so **no VM flag or runner
override can reach them.** Patching an upstream stress test's own budget to
make a VM look better is not a trade worth making; they stay reported as FAIL,
with this page as the reason.

All five are VOLUME-driven, which is why they are the ones that show it:

| class | what it repeats |
|---|---|
| `MultithreadedInsertionTest` | 12 threads x 2000 entities = 24 000 inserts |
| `MultithreadedIdentityGenerationTest` | same shape, id generation only |
| `MultithreadedInsertionWithLazyConnectionTest` | same, x2 methods |
| `it.LocalContextTest` | `for (i < REQUEST_NUMBER)` HTTP round trips |
| `techempower.TechEmpowerTest` | `REQUEST_NUMBER = 500` round trips |

### 2.2 The profile says composition, and composition means lambdas

The in-VM watchdog samples every registered thread repeatedly, so a dump is a
poor-man's profile. 15 432 samples from one 90 s window of
`MultithreadedInsertionTest`, leaves attributed to the CALLER when the leaf is
an invoke (`pc=0 last_pc=0`):

```
 11.7%  AsyncTrampoline$TrampolineInternal.unroll     (hibernate-reactive)
 11.5%  CompletableFuture.thenCompose
  8.4%  CompletableFuture.uniComposeStage
  5.6%  CompletableFuture.whenComplete
  5.0%  CompletableFuture.uniWhenComplete
  4.1%  io.vertx.core.Future.lambda$toCompletionStage$5
  3.2%  CompletableFuture$UniCompose.tryFire
  2.8%  CompletionStages$ArrayLoop.next
```

Flat, no single hot body, ~55% in `CompletableFuture` composition plus
hibernate-reactive's own trampoline. Five threads RUNNING at every sample
(`deposit=STALE`), 44–62 interpreted frames deep with "25 active JIT call(s) on
the native stack" — forward progress through the persist pipeline, not a wait.

### 2.3 The measurement — read every row against `-Xint`

`probes/CompletionStageChainProbe.java`, 200 000 ops per shape,
ABBA-interleaved on a quiet box. The project's own yardstick is that **2.5x
versus HotSpot `-Xint` is the statement about this VM** (see
`perf/vm-per-call-dispatch-cost-20260813.md`); the C2 column is
a statement about not having an optimising compiler.

| shape | HotSpot C2 | HotSpot `-Xint` | CratonVM | **vs `-Xint`** |
|---|---|---|---|---|
| `box` — `Integer.valueOf` | 13–18 ns | 73 ns | 157–165 ns | **2.2x** |
| `alloc` — `completedFuture` | 23–35 ns | 453 ns | 2 538–2 843 ns | 5.9x |
| `apply` — one `thenApply` | 36–61 ns | 937 ns | 20 406–40 806 ns | **22–44x** |
| `compose` — one `thenCompose` | 73–82 ns | 1 313 ns | 22 810–60 140 ns | 17–46x |
| `when` — one `whenComplete` | 47–55 ns | 1 303 ns | 20 272–40 196 ns | 16–31x |
| `trampoline` — compose+when | 74 ns | 2 033 ns | 38 043–74 275 ns | 19–37x |

`box` sits exactly at the documented VM-wide baseline. Composition is an order
of magnitude past it. **This is not the known per-call ceiling.**

And the JIT is not buying anything here — `--nojit` is the same speed or
*faster* on every composition shape (`apply` 16 527 ns with the JIT off against
20 406–40 806 ns with it on), where it is worth 3–4x on `box` and `alloc`.

### 2.4 Which primitive — `probes/CompositionPrimitivesProbe.java`

A composition step touches four things a plain allocation does not. Measured
separately, 500 000 ops each, ABBA-interleaved, same box:

| primitive | HotSpot `-Xint` | CratonVM JIT | CratonVM `--nojit` | **vs `-Xint`** |
|---|---|---|---|---|
| `plainCall` — static `int` call | 18.8–32.5 ns | **7.2–9.7 ns** | 357.5 ns | **0.3–0.4x (FASTER)** |
| `volatileRW` — volatile ref r/w | 19.5 ns | 104.7 ns | — | 5.4x |
| `cas` — `AtomicReference.compareAndSet` | 450.3 ns | 540.6 ns | — | **1.2x** |
| `lambda` — `Function.apply`, one target | 209–236 ns | **1 701–2 074 ns** | 3 263 ns | **7.6–9.3x** |
| `iface` — `Function.apply`, two targets | 216–230 ns | **1 586–1 856 ns** | 3 666 ns | 7.4–8.4x |

**That is the whole answer, and the probe carries its own control.**
`plainCall` is the *same loop shape*, gets the *same* OSR treatment, and comes
out 2–4x faster than HotSpot's interpreter and 40x faster than this VM's own
`--nojit`. So the JIT works, the loop shape is not an artifact, and CAS and
volatile access are fine. Only the functional-interface invocation is slow, and
the JIT recovers just 1.9x on it (3 263 → 1 701 ns) against the 40x it gets on
a plain static call.

Put the two together: on HotSpot's interpreter a lambda call is ~11–13x a
static call; on CratonVM it is **~200x**. `CompletableFuture` composition,
hibernate-reactive's `AsyncTrampoline`, and every vert.x `Handler` are made of
nothing else, which is exactly why these five classes and no others are left.

## 3. The profile, and why this page does not prescribe a fix

Reproduced on Azure Linux, so the finding is not a Windows artifact — and there
the internal spread is starker still, because that host's HotSpot interpreter is
slower while CratonVM's JIT'd static call is not:

| | HotSpot `-Xint` | CratonVM | spread vs its own `plainCall` |
|---|---|---|---|
| `plainCall` | 54.1 ns | **6.4 ns** | 1x |
| `lambda` | 726.1 ns | 1 907.2 ns | **298x** |
| `iface` | 893.0 ns | 1 970.8 ns | 308x |
| `cas` | 1 696.9 ns | 513.1 ns | — (CratonVM faster) |

HotSpot's interpreter puts a lambda call at 13x a static call. CratonVM puts it
at ~300x. That ratio, not the absolute, is the defect.

`perf record -F 999 --call-graph=dwarf` over the probe (2 202 samples, self
time, `--percent-limit 0.8`):

```
 5.86%  vm_exec::safe_native_call_impl
 4.54%  interpreter::execute_frame_from_index
 4.45%  VmHeap::is_object_address
 4.41%  interpreter::lambda::try_lambda_dispatch
 4.13%  interpreter::lambda::coerce_lambda_args
 3.81%  ZgcRealHeap::alloc_raw_tlab
 3.54%  ZObjectStarts::contains
 2.45%  jit::helpers::try_jit_site_cached_native_dispatch
 2.23%  jit::helpers::forward_jit_reference_args
 2.18%  jit::helpers::jit_invoke_virtual_mic
 …
 1.32%  jit::helpers::try_fast_lambda_int_to_double_apply
 1.09%  interpreter::lambda::try_invoke_cached_lambda_impl
```

**It is flat, and that is the point.** The lambda-named frames together are
~11%; nothing is a hot spot. This is precisely the shape
`perf/vm-per-call-dispatch-cost-20260813.md` §3 warns about —
two changes made on 2026-08-13 each removed 5–10% of attributed samples and
neither moved CPU measurably. So this page deliberately stops at the
measurement and does **not** prescribe an optimisation.

What it can hand the next reader:

* The interpreter's SAM path takes an **RwLock read plus a HashMap lookup on
  the process-global `classes.lambda_proxies` map on every invocation**, and up
  to twice — once for `is_lambda_proxy_receiver` and again inside
  `try_lambda_dispatch` (`vm/src/runtime/interpreter/invoke.rs`). That is a
  structural per-call cost a plain `invokestatic` does not pay, and it fits
  both the absolute number and the JIT's inability to help.
* A caveat on the probe: `Function<Integer,Integer>.apply` boxes on both sides
  of the call and `plainCall` does not, so the two are not directly comparable.
  The `-Xint` column is what makes the comparison fair — HotSpot's interpreter
  does the same boxing and still lands at 209–236 ns — and `box` measured
  separately at 2.2x accounts for only ~17% of the gap.
* Any candidate fix must be A/B'd on `MultithreadedInsertionTest`'s wall clock
  (219 s today), not on the probe.

Prior art worth reading first: the non-capturing-lambda fast path was already
widened once (2026-08-09, 4x on a microbenchmark) and it moved the Spring
workload 0%. The difference here is that this workload IS lambda-bound, so a
win should convert — but that is a prediction, and it should be A/B'd on
`MultithreadedInsertionTest`'s wall clock, not on the probe.

## 4. What changed in the harness

Two of the five are accommodatable and get per-class overrides
(`class-overrides.tsv`), whose header asks for exactly this once "a categorize
run against a real DB surfaces classes that need more than the flat --timeout".

**A trap found while doing that**, and fixed on both the Windows and Azure
copies of the runner: it assembled

```
eff_flags = base + per-class override flags + @common.args
```

so a per-class `-D` could never override a system property `common.args`
already set — a later `-D` wins, and the argfile came last. The override was
silently inert for the one thing this table most needs to do, which is
indistinguishable from having no override at all. The flags now go after the
argfile.
