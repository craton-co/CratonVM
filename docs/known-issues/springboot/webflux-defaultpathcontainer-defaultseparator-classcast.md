# WebFlux/WebMvc `DefaultPathContainer$DefaultSeparator` checkcast вЂ” FIXED / RETIRED 2026-08-01

**Status: OPEN — REGRESSED 2026-08-01 (again).** See "Regression note
(2026-08-01, again)" at the end of this doc: 3 of the 5 classes this closure's
validation table lists as green are back to failing with the identical
`DefaultSeparator` CCE, on a build where all three cited fix commits are
genuine ancestors. Three independent defects had to be settled to reach this
closure, and none was the "stale/moved-GC reference or duplicate class
identity" pair the original report guessed at:

1. the reported `ClassCastException` itself вЂ” the **in-place old-gen sweep
   returning a LIVE promoted object's block to the free list**, fixed on `dev`
   by `20cab92aa`, attributed here by a lever A/B on the very binary that
   produced the 2026-07-31 failures;
2. a **JIT regression that landed on `dev` after 07-31**: the optimizing
   (C2/IR) backend silently dropped every argument past the entry-ABI register
   file. Found and fixed while validating this doc;
3. a **second JIT regression from the same window**: a compiled callee's
   exception handler resumed on a frame rebuilt from `this` plus the declared
   parameters, so every NON-parameter local came back 0/null. Diagnosed
   independently here and fixed on `dev` by
   `fix/liquibase-scope-20260801` (`063be4f18`) while this work was in flight вЂ”
   **that** fix is the one in the tree; see defect 3 below for why the
   workaround this session had reached for was dropped in its favour.

Defects 2 and 3 both post-date the binary that produced the original report, so
neither is the cause of the CCE; they are what kept two of the five affected
classes red once the CCE was gone. Defect 3 is also the open JIT-only
`BasicErrorControllerIntegrationTests` regression.

Retired from `docs/known-issues/springboot/`. The three earlier framings in the
original doc вЂ” "no CratonVM source location", "stale/moved-GC reference", and
"duplicate class identity across loaders" вЂ” are **superseded**, not confirmed.

## What the signature actually means

`ClassCastException: java.lang.Object cannot be cast to X` is not a class
identity problem. CratonVM renders `ClassId(0)` as `java/lang/Object`, and a
zeroed object header *is* `ClassId(0)`, so this message is the house signature
for **reading a reclaimed object** вЂ” see the `cce0079` note in
`gc/src/gen_heap.rs` and the `FMT-JIT-CCE` note in `vm/src/jit/helpers.rs`,
both of which name this exact string as the stale-`ObjectRef` family.

Every previous session read the message as a typing problem. It is a
use-after-free.

## Why `DefaultSeparator` specifically

`org.springframework.http.server.DefaultPathContainer`:

```java
private static final Map<Character, DefaultSeparator> SEPARATORS =
        Map.of('/', new DefaultSeparator('/', "%2F"),
               '.', new DefaultSeparator('.', "%2E"));
...
static PathContainer createFromUrlPath(String path, Options options) {
    ...
    DefaultSeparator separator = SEPARATORS.get(options.separator());  // implicit checkcast
```

On CratonVM that map is not ordinary Java state:

* `Map.of` is a native factory (`make_map_of` в†’ `freeze_result`): a real backing
  `HashMap` wrapped in a `cratonvm/internal/UnmodifiableMap`.
* `Character` unboxes to `Value::Int` (`unbox_wrapper`), so `try_hm_int_fast_put`
  adopts the backing map into the **`hm_int_fast` collection overlay** вЂ” a
  process-global Rust side table (`HmIntFastState::entries`), keyed by the map's
  relocation-invariant identity hash.
* The two `DefaultSeparator` instances therefore live **only** in that side
  table. No Java field and no card describes the edge; `try_hm_int_fast_get`
  hands the stored `Value` straight back.

So a collector that loses that root gives `get` a dangling `ObjectRef`, and the
implicit checkcast on the generic `get` is the first instruction to notice вЂ”
which is precisely the reported site,
`DefaultPathContainer.createFromUrlPath(DefaultPathContainer.java:98)`, reached
from `RequestPath.parse` on both the WebFlux routing path and
`ServletRequestPathUtils.parseAndCache` on the servlet path. That is why the
same exception appeared under WebFlux, WebMvc and Jersey: they share
`DefaultPathContainer`, not a bug.

## Defect 1 вЂ” the in-place old-gen sweep freed a live promoted object

Root cause is defect 4 of
[`map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md):
`sweep_young_non_moving` commits selective youngв†’old promotions and records them
in `result.0.pointer_map`, but leaves the roots on their **pre-promotion young
addresses**. `sweep_old_gen_non_moving` marks from that slice and `old_gen_gc`'s
seed loop drops any address `old_gen.contains()` rejects, so a promoted object
at its new old-gen home is never marked and its block goes back on the free
list. The sweep does not zero what it frees, so this presents as silent data
loss until a later allocation is handed the block вЂ” at which point a read of the
old reference sees the newcomer's (or a zeroed) header. That doc records the
identical second face: `ClassCastException: class java.lang.Object cannot be
cast to Bundle`.

Fixed on `dev` by `20cab92aa` ("seed the in-place old sweep from this cycle's
promotion destinations"), paired 0/8 в†’ 8/8.

### The attribution A/B

Run on the **same binary that produced the 2026-07-31 failures**
(`CratonVM-spring-boot-residual-20260728\target\release\cratonvm-spring-boot-residual0728.exe`),
same class, same fixture, same host, JIT on:

| arm | status | `DefaultSeparator` CCEs |
|---|---|---:|
| 07-31 binary, default | FAIL 6/8 | **6** |
| 07-31 binary + `CRATONVM_OLD_SWEEP_JIT=0` | **PASS 8/8** | **0** |
| current `dev` binary | вЂ” | **0** |

`CRATONVM_OLD_SWEEP_JIT=0` disables exactly the in-place old-gen sweep and
nothing else. Turning it off removes the failure on the *unfixed* binary; that
is the attribution the original doc never had. The failing run reproduced the
2026-07-31 log line-for-line (same three `dispatcherServlet` errors at the same
log offsets).

## Defect 2 вЂ” the IR backend dropped every argument past the entry-ABI registers

Found while validating: with defect 1 closed, `GraphQlWebFluxAutoConfigurationTests`
and `WebMvcHealthEndpointAdditionalPathIntegrationTests` were still red under
JIT вЂ” but with a different failure, and green under `--nojit`:

```
BindException: Failed to bind properties under 'spring.jackson.use-jackson2-defaults' to boolean
BindException: Failed to bind properties under 'server.tomcat.accept-count' to int
  Caused by: NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null
    at org.springframework.boot.context.properties.bind.BindConverter.convert(BindConverter.java:108)
```

A compiled method receives its incoming arguments in the platform integer
argument registers вЂ” four on Win64 вЂ” with everything past them on the caller's
stack. The single-pass (C1) backend loads both halves (`x64.rs`, "ROUND-12
fix"). The optimizing (C2/IR) backend's prologue reads registers only and
`break`s on the rest, so `lower()` must refuse any graph with more incoming
slots than the register file. That refusal was scoped to `needs_context`
methods:

```rust
if lowerer.needs_context {                  // <-- the defect
    if 1 + num_params > abi_len { return None; }
}
```

Every **leaf** method вЂ” no `Op::Call`, so `needs_context == false` вЂ” skipped the
check entirely, was lowered anyway, and had its stack-passed parameters dropped:
the local kept whatever the frame slot held.

`probes/EntryAbiArgSlotProbe.java` pins the boundary. The receiver is an
incoming slot too, so an instance method with N parameters occupies N+1:

| shape | incoming slots | 07-31 binary | `dev` before the fix |
|---|---:|---|---|
| `i3` (instance, 3 params) | 4 | ok | ok |
| `i4` (instance, 4 params) | 5 | ok | **SIGSEGV** |
| `i5` (instance, 5 params) | 6 | ok | **SIGSEGV** |
| `i6` (instance, 6 params) | 7 | 0/300 000 | **299 368 wrong, `got=null`** |
| `s4` (static, 4 params) | 4 | ok | ok |
| `s5` (static, 5 params) | 5 | ok | **SIGSEGV** |
| `s6` (static, 6 params) | 6 | 0/300 000 | **299 447 wrong, `got=null`** |

Spring Boot's binder runs straight into it:
`BindHandler.onSuccess(name, target, context, result)` is a pure `aload 4;
areturn` over five slots, so once IR-lowered it returns null and the binder
discards every bound value вЂ” `BindResult.isBound()` answers false for every
property, and `BindConverter.convert`'s `this` is wrong for the same reason.

Why it surfaced only now: the 07-31 binary's C2 attempt bailed back to the
single-pass body (`len=417`, identical to C1); current `dev` genuinely IR-lowers
the same method (`len=244`).

**Fix** (`jit/src/ir_lower.rs`): the capacity check moves out of the
`needs_context` arm and counts the context slot explicitly; both the guard and
`emit_prologue` now read one `incoming_abi_reg_capacity()` instead of two
hand-maintained register lists. Guarded by
`ir_lower_refuses_more_incoming_slots_than_abi_registers`, a true differential вЂ”
it fails with the old guard restored and passes with the fix.

## Defect 3 — a compiled callee's handler resumed on a rebuilt frame

Fixing defect 2 was not enough: both classes kept failing with the identical
`<local5>` NPE, while every isolated binder probe — scalar bind, JavaBean bind,
and the real `JacksonProperties` bind on the graphql module classpath — passed.
`CRATONVM_JIT_BISECT_SKIP=org/springframework/boot/context/properties/bind/BindConverter.convert`
took the class to **PASS 17/17**, which put the miscompile in that one method.

`probes/HandlerCalleeEntryCacheProbe.java` reduces it to ~10 seconds with no
Spring on the classpath, reproducing the reported message verbatim:

```
NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null
```

199 424 of 200 000 calls wrong on `dev`, 0 on a binary predating
`b96731855`. It is correct for the first few hundred calls and then fails
permanently, so a short run proves nothing.

**Root cause and fix — `fix/liquibase-scope-20260801`, merged to `dev` as
`063be4f18` while this investigation was in flight.** `run_jit_callee_handler`
rebuilt a compiled callee's handler frame from `this` plus the declared
parameters and never consumed the reason-9 exceptional frame the compiled body
had published, so every **non-parameter** local resumed as 0/null. Sound when
written; false once `precise_handler_frames_enabled` began admitting the very
methods whose handlers read such locals.

`BindConverter.convert` is the exact witness that commit names too: the loop's
`Iterator` lives in local 5, a `canConvert` inside the `try` throws, and the
handler falls through to the loop head — so `hasNext()` NPEs on a null
iterator. It takes `LiquibaseAutoConfigurationTests` from 27-of-43 FAIL to
43/43.

**What this session had instead, and why it was dropped.** Two levers cleared
the probe — `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE=1` and
`CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES=1` — which pointed at `b96731855`'s
per-thread cache of raw compiled entries. That cache is populated on exactly one
condition:

```rust
let callee_barred_by_table = !mic_publish_exception_table_callees()
    && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info);
if callee_barred_by_table && !callee_has_indy_trap && cacheable_receiver && globally_named {
    publish_mic_rust_cached_entry(info_ptr, receiver_cid, entry_ptr, needs_ctx);
}
```

— i.e. precisely when the callee is BARRED from the machine-code MIC/PIC — so
making that cache opt-in did clear the failure (0 of 200 000, and 199 442 wrong
again with it re-enabled). But it works by keeping the compiled callee's handler
path from being *entered*, not by making it correct: it is a workaround that
also forfeits the throughput the cache was built for. The `dev` fix repairs the
handler frame itself and keeps the optimisation, so it is strictly better and
this session's change was reverted in its favour rather than merged alongside
it.

The probe is kept: it covers the same shape from a different direction (an
exception-table callee reached from a compiled caller) and is cheap.

This is also the open JIT-only `BasicErrorControllerIntegrationTests` regression
(same `<local5>` NPE, same `--nojit` passes / JIT fails shape).
## The fixture note in the original doc is stale

The 2026-07-29 session recorded that `C:\craton\CratonVM\apps\spring-boot` could
not be used вЂ” "its generated classpath lacked `Configurations.class`, and its
incomplete source tree lacked `build-plugin\spring-boot-antlib`". Both are
present now, and a HotSpot baseline over all five affected classes passes 5/5
against that tree. No substitute fixture is needed.

## Validation

Binary `cratonvm-pathcontainer-20260801.exe`, branch
`fix/springboot-pathcontainer-separator-20260801`, JDK 25 (Adoptium
`jdk-25.0.3.9-hotspot` вЂ” note the runner's default `C:\Program Files\Java\jdk-25`
no longer exists on this host, so `-JdkHome` is required).

All five affected classes, JIT on, `pathcontainer-micfix-jit-20260801` вЂ” **50/50
tests, 0 failed, and zero `DefaultSeparator` ClassCastExceptions or `<local5>`
NPEs anywhere in the logs**:

| class | tests | failed | s |
|---|---:|---:|---:|
| `IntegrationGraphEndpointWebIntegrationTests` | 6 | 0 | 143 |
| `GraphQlWebFluxAutoConfigurationTests` | 17 | 0 | 249 |
| `JerseyEndpointRequestIntegrationTests` | 9 | 0 | 474 |
| `ManagementWebSecurityAutoConfigurationTests` | 10 | 0 | 459 |
| `WebMvcHealthEndpointAdditionalPathIntegrationTests` | 8 | 0 | 656 |

Re-run after merging the newer `origin/dev` that carries defect 3's real fix
(run `pathcontainer-final-jit-20260801`): **44/44 tests, 0 failed**, still zero
CCEs and zero `<local5>` NPEs. In that round
`IntegrationGraphEndpointWebIntegrationTests` reports `containersFailed=2` and
runs zero tests — a different, newly-landed `dev` defect, not a residual of this
one. See "One unrelated regression arrived during the re-merge" below.

How the five moved as each defect closed (JIT on, same fixture and host):

| binary | passing |
|---|---|
| 07-31 rerun binary | the CCE round this doc reports |
| `dev` (defects 1 closed, 2+3 live) | 3/5 вЂ” graphql 10/17 and webmvc 6/8 fail on `<local5>` |
| + defect 2 (`ir_lower`) | 3/5 вЂ” same two, same NPE |
| + defect 3 (the handler-frame fix) | **5/5** |

Controls: HotSpot JDK 25 over the same five classes, same fixture вЂ” 5/5 PASS.
CratonVM `--nojit` вЂ” 5/5 PASS both before and after the fixes (defects 2 and 3
are JIT-only, which is what made `--nojit` a clean control throughout).

Unit tests: `cratonvm-jit --lib` **1227 passed / 0 failed** (this also required
repairing `push_stack_refuses_to_cross_spill_limit`, which the merged
`ws-serverdeadlock` branch's `spill_size` headroom change had broken вЂ” its
author recorded being unable to run that suite).

## One unrelated regression arrived during the re-merge

Between the validation above and the final `origin/dev` merge, `dev` picked up a
defect that stops `IntegrationGraphEndpointWebIntegrationTests` before any of
its tests run:

```
org.junit.platform.commons.PreconditionViolationException: displayName must not be null or blank
  at org.junit.platform.engine.support.descriptor.AbstractTestDescriptor.<init>(AbstractTestDescriptor.java:115)
  at org.junit.jupiter.engine.descriptor.TestTemplateInvocationTestDescriptor.<init>(TestTemplateInvocationTestDescriptor.java:52)
  at org.junit.jupiter.engine.descriptor.TestTemplateTestDescriptor$TestTemplateExecutor.createInvocationTestDescriptor(...)
```

A `@TestTemplate` invocation's display name comes back null or blank, so JUnit
refuses to build the descriptor: `containersFailed=2`, `tests=0`, in **1.2
seconds**, before any Spring context starts.

It is not a residual of this doc, and five measurements say so:

| check | result |
|---|---|
| pre-merge binary, same class, same fixture | **PASS**, 139 s, 6/6 |
| post-merge binary | **FAIL**, 1.3 s, 0 tests |
| `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE=1` | no change — not defect 3's cache |
| `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME=1` | no change — not the liquibase handler-frame fix |
| **`--nojit`** | **still fails — not a JIT defect at all** |

Localised to the `feat/c2-review-remediation` merge series: it already fails at
`72da6c55f` (before the liquibase merge) while `be2cbabc5` plus the
`ws-serverdeadlock` merge passes, so it entered in `efba7a8ac..72da6c55f`. Not
narrowed further here — it is another branch's in-flight work, it is not JIT,
and the repro is deterministic and instant for whoever picks it up:

```
run-spring-boot-suite.ps1 -ClassList <one-class tsv> -Vm craton -Jit on -Parallel 1 \
  -Exe <binary> -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
```

Every other affected class passes on that same merged tree, including both
classes that originally carried the `DefaultSeparator` CCE.

## Regression coverage

* `probes/EntryAbiArgSlotProbe.java` (defect 2) вЂ” entry-ABI argument-slot
  boundary, one shape per process. Also carries the `BindHandler.onSuccess`
  shape verbatim.
* `probes/HandlerCalleeEntryCacheProbe.java` (defect 3) вЂ” an exception-table
  callee reached from a compiled caller. Needs hundreds of thousands of
  iterations: it is correct for the first few hundred calls and then fails
  permanently.
* `probes/PathSeparatorMapProbe.java` (defect 1) вЂ” the `Map.of('/' , вЂ¦)`
  overlay under repeated `System.gc()` with promoted survivors, i.e. this doc's
  own signature. Pin `--Xmx`; the default heap makes it load-sensitive and it
  will not express a promotion defect in a run that promotes nothing.
* `jit/src/ir_lower.rs::ir_lower_refuses_more_incoming_slots_than_abi_registers`
  вЂ” a true differential (it fails with the pre-fix guard restored).

## Affected classes (all green вЂ” see the validation table above)

- `module/spring-boot-graphql` вЂ” `org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests`
- `module/spring-boot-integration` вЂ” `org.springframework.boot.integration.actuate.endpoint.IntegrationGraphEndpointWebIntegrationTests`
- `module/spring-boot-webmvc` вЂ” `org.springframework.boot.webmvc.autoconfigure.actuate.endpoint.web.WebMvcHealthEndpointAdditionalPathIntegrationTests`
- `module/spring-boot-security` вЂ” `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests`
- `module/spring-boot-security` вЂ” `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests`

## The SIGSEGV in "Regression note 2"

The 2026-07-31 run escalated to `EXCEPTION_ACCESS_VIOLATION ... read at address
0x0000000100000004` on a `http-nio-auto-8-exec-1` Tomcat worker, and the doc
asked whether it was the same corruption or a second defect. It is **not**
re-attributed here: it did not recur in any run of this session, and its
published Java frames sit in `Http11InputBuffer.fill` в†’ `NioChannel.read`, not
in path parsing. The `compiled-frame-oop-not-published` annotation the doc
flagged is the reason moving-young *declined* that cycle (a safety fallback to
the non-moving sweep), not a fault report вЂ” so it is not the lead it looked
like. Both defects above are independently sufficient to produce a wild read,
and defect 2 in particular SIGSEGVs on the 5-slot shape all by itself.

## Regression note (2026-08-01, again)

Reran the 26-class FAIL/CRASH residual from the 2026-07-31 full-suite round
against `dev` merged to `1b24cca1f` (branch
`feat/spring-boot-residual-rerun-20260728`, binary
`cratonvm-spring-boot-residual0728.exe`, 1 shard, `-Parallel 1`,
`-TimeoutSec 1500`, `RunName=craton-rerun-20260801`). Confirmed `0b18f15eb`,
`20cab92aa`, `c3dbb011a`, and `063be4f18` (this doc's cited fixes) are all
genuine ancestors of `1b24cca1f` (`git merge-base --is-ancestor`), so this is
not a stale-binary artifact.

**2 of 5 classes still hold** — `GraphQlWebFluxAutoConfigurationTests` and
`WebMvcHealthEndpointAdditionalPathIntegrationTests` were not both
independently reverified this round (only the latter was in the 26-class
list; it PASSed, 322.4s).

**3 of 5 classes regressed with the identical signature**:

- `IntegrationGraphEndpointWebIntegrationTests` — FAIL, 6 tests/3 failed,
  118.0s. Same `ClassCastException: java.lang.Object cannot be cast to
  org.springframework.http.server.DefaultPathContainer$DefaultSeparator` at
  `DefaultPathContainer.createFromUrlPath(DefaultPathContainer.java:98)`. This
  is a real recurrence of the CCE, not the unrelated JUnit
  `displayName must not be null or blank` regression this doc's "One
  unrelated regression arrived during the re-merge" section describes for the
  same class — that failure mode is `containersFailed=2, tests=0` in ~1.3s;
  today's run executed all 6 tests in 118s and failed 3 with the CCE.
- `JerseyEndpointRequestIntegrationTests` — FAIL, 6 of 9 tests, 177.1s. Same
  CCE, reached via `ServletRequestPathFilter.doFilter` →
  `ServletRequestPathUtils.parseAndCache` → `RequestPath.parse` →
  `DefaultPathContainer.createFromUrlPath`, both through a Jersey
  `ResourceConfig` servlet and the plain `se1-actuator-endpoint` servlet.
- `ManagementWebSecurityAutoConfigurationTests` — FAIL, 4 of 10 tests, 161.2s.
  Same CCE, same call site.

Logs (all under `craton-rerun-20260801/all-jit/logs/`):
`module_spring-boot-integration.org.springframework.boot.integration.actuate.endpoint.Integ-cf6a6f777358.{out,err}.log`,
`module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.se-a0d9d711811f.{out,err}.log`,
`module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.w-f4cd03f47d82.{out,err}.log`.

Not re-diagnosed at the source level this session. As with the companion
`basicerrorcontroller-class-cluster-20260728.md` regression note filed the
same round: `WebMvcHealthEndpointAdditionalPathIntegrationTests` passing while
3 siblings sharing the exact same `DefaultPathContainer.createFromUrlPath`
call site fail suggests the underlying GC fixes are real but don't cover
every promotion/load pattern that reaches this map — worth comparing what
each failing class's boot sequence does differently (Jersey servlet
init order, security filter chain construction, WebFlux vs. servlet dispatch)
against the passing class rather than assuming a fourth independent cause.
