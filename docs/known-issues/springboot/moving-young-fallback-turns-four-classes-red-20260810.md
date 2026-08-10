# Four Spring Boot classes are red because of the JIT-triggered `[moving-young]` fallback, not because of the 300s budget

**Status: OPEN — measured 2026-08-10. Supersedes the "margin exhausted",
"recurring timeout" and "cumulative `ModifiedClassPathExtension` cost" framings
of the classes below, whose own docs are retired to
`docs/internal/fixed-suite-bugs/springboot/`.**

> **The conclusion holds; three of the four rows were re-measured after
> `67fadfdd8` and their numbers changed.** The `f695ca875` binary the Flyway,
> Integration and Quartz rows were taken on **predates** `67fadfdd8`
> ("an object hashed while locked kept that hash after the unlock") by 90
> minutes; the Log4J2 row's `6365de194` already has it. That defect gave
> `HashMap` a second entry per re-`put` of a locked-then-unlocked key, which is
> its own unbounded-growth path to an OOM. Re-measuring on a post-fix binary
> leaves the mechanism and the "don't add `slowClasses`" verdict intact but
> moves every affected number — Flyway is no longer slower than `--nojit` at
> all. See ["Re-measured on a post-fix binary"](#re-measured-on-a-post-fix-binary-2026-08-10).

> **A 300s log is too short to see this.** `Log4J2LoggingSystemTests` was first
> read as *not* an instance of this bug, precisely because its 300s suite log
> contains **zero** fallback lines. Given a 3600s budget the same class reaches
> **#4096** and still never finishes. The escalation outlasts the budget that
> kills the process, so absence of the warning in a timed-out log is not
> evidence of absence — re-run with a real budget before ruling a class out.

## The measurement

Standalone, one class at a time (no shard concurrency), `--Xmx 2g`, from the
`CratonVM-spring-boot-residual-20260728` checkout. Binaries
`cratonvm-flymargin-20260810.exe` (dev `f695ca875`) and
`cratonvm-logsys-20260810.exe` (dev `6365de194`). HotSpot control is Temurin
25.0.3 on the same host, same classpath, same runner.

| Class | HotSpot | CratonVM, JIT on | CratonVM `--nojit` | `[moving-young]` fallback peak |
|---|---:|---|---:|---:|
| `…flyway.autoconfigure.FlywayAutoConfigurationTests` | 10.0s ✓73/73 | 496.2s ✓73/73 | **218.5s** ✓73/73 | 7 |
| `…integration.autoconfigure.IntegrationAutoConfigurationTests` | 11.2s ✓34/34 | **OOM after 3.8 hours** | **221.9s** ✓34/34 | **#16384** |
| `…quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` | 12.6s ✓45/45 | **no completion in 2400s** | **262.9s** (see note) | **#4096** |
| `…logging.log4j2.Log4J2LoggingSystemTests` | 10.7s (61, 14 fail) | **no completion in 3600s** | **293.3s** (61, 14 fail) | **#4096** |

**With the JIT off, all four finish in 218–293s — inside the default 300s
budget — and log zero fallbacks.** With the JIT on, one takes 2.3x longer, one
dies of heap exhaustion, and two do not finish in 8–12x their budget.

`Log4J2LoggingSystemTests`'s 14 failures are identical under HotSpot and
CratonVM `--nojit`, so they are pre-existing/environmental and not part of this
defect. Its sibling `LogbackLoggingSystemTests` is **not** affected: it
completes both ways (268.2s JIT / 277.5s `--nojit`, 86/86 both) with **zero**
fallbacks, and is simply ~30x HotSpot — see its retired doc.

Note on Quartz `--nojit`: it reports 29/45 failed, every failure being
`ApplicationContextException: Failed to start bean 'webServerStartStop'` from
`Connector["http-nio-8080"]` failing to bind. **That is not environmental, and
port 8080 was not held by an unrelated process** — see the correction below;
the timing result stands, but this run is not evidence that `--nojit` is clean.

## Re-measured on a post-fix binary (2026-08-10)

Same runner, same classpaths, `--Xmx 2g`, one class at a time, JIT **on**;
binary `cratonvm-idhash-verify-20260810.exe` (release build of dev
`9bf80d7bb`, which contains `67fadfdd8`). HotSpot control re-taken on the same
host in the same window. This host was carrying other sessions' builds
throughout, so both columns are load-inflated by roughly the same factor —
read the contrasts, not the absolutes.

| Class | HotSpot (re-taken) | JIT on, pre-fix `f695ca875` | **JIT on, post-fix `9bf80d7bb`** | fallback peak, reason at peak |
|---|---:|---|---|---|
| Flyway | 17.2s ✓73/73 | 496.2s ✓73/73 | **193.1s ✓73/73** | **#4** `innermost-rbp-belongs-to-unguarded-callee` |
| Integration | 26.9s ✓34/34 | **OOM after 3.8h** | **8,135s ✓34/34** | #16384 `active-safepoint-map-incomplete` |
| Quartz | 31.4s ✓45/45 | no completion in 2400s | **OOM at 8,704s** | #16384 `xt-helper-window-conservative-scan` |

What moved, and what did not:

- **Flyway is no longer an instance of this bug.** 496.2s → 193.1s, peak 7 →
  **#4**, and 193.1s is *below* its own `--nojit` time of 218.5s. The "one takes
  2.3x longer" clause above no longer describes any measured row.
- **Integration no longer OOMs** — 34/34 in 8,135s. The pre-fix OOM was at least
  partly the duplicate-entry map growth of `67fadfdd8`, not free-list
  fragmentation alone. It is still **302x** HotSpot and **36.7x** its own
  `--nojit` (221.9s), so the class stays red for this defect.
- **Quartz now OOMs** where it previously only failed to finish, with a single
  `Root WebApplicationContext` initialization taking **910 seconds**.

The two middle rows therefore **swapped outcomes** across the fix: pre-fix
Integration OOMed and Quartz merely never finished; post-fix Integration passes
and Quartz OOMs. "OOM" versus "never finishes" is not a property of the class —
it is where a given run happens to land once the young generation stops
compacting. That supports the single-mechanism reading of this page and argues
against characterising the classes individually.

### Correction: the Quartz `--nojit` failures were a VM defect, not port contention

Those 29 failures were the same `67fadfdd8` defect, on its most direct path.
Spring Boot's MVC/Jersey endpoint infrastructure builds Tomcat with
`new TomcatServletWebServerFactory(0)` — port **0**, ephemeral — so a connector
named `http-nio-8080` should never exist at all. `TomcatWebServer` parks the
service's connectors in a `Map<Service, Connector[]>` from inside
`LifecycleBase.start()`, which is `synchronized` on that very `StandardService`;
hashing the key while its own monitor was held filed the entry under `i32::MAX`,
the post-unlock lookup missed, the service came back with no connectors, and
`Tomcat.getConnector()` fabricated a fresh **port-8080** connector on the
already-running service.

Established with a standalone 30-second reproducer (no JUnit, no Quartz, no
Spring context: 4/4 cycles fail pre-fix, 4/4 pass post-fix with ephemeral ports)
and by reading the corrupted map directly — the node sat in `bucket[0]` while
lookups went to `bucket[10]`, and `CRATONVM_DBG=map-miss-audit` printed
`searched_hash=14714 searched_bucket=10 found_in_bucket=0 node_hash=2147450880`.
Post-fix the whole-class run logs **0** `http-nio-8080` mentions (was 68) and
**0** `ApplicationContextException`.

Consequence for this page: the Quartz `--nojit` row should be re-taken before
its 262.9s figure is used as the "clean" arm, and no Quartz result predating
`67fadfdd8` distinguishes this defect from that one.

## Why this is not a timeout-budget problem

The retired docs recommend the same fix: raise these classes' per-class timeout
(`slowClasses` in `run-spring-boot-suite.ps1`). The measurements say that is
wrong on both counts:

- For `Integration`, `Quartz` and `Log4J2` it **would not work**. They do not run
  long and then succeed; they fail. `Integration` OOMs — a longer budget moves
  the failure later, it does not produce a pass. `Quartz` had not finished at
  2400s and `Log4J2` had not finished at 3600s, 8–12x the budget they are
  alleged to be marginally over.
- For all four it **would hide the defect**. The same classes complete
  comfortably inside the existing 300s budget with the JIT disabled. Nothing
  about these classes is intrinsically too slow for the budget.

No `slowClasses` entries were added.

## Mechanism, and why it is already known

`[moving-young] fallback` means a young collection could not use the moving
(compacting) collector and fell back to the non-moving sweep — raised by the
conservative stack scan when it cannot prove a JIT frame is safe to move.

**Four distinct reasons appear, not two, and the reason is not a property of the
class.** The pre-fix runs showed `unregistered-jit-frame-on-stack` (Flyway,
Integration) and `innermost-rbp-belongs-to-unguarded-callee` (Quartz, Log4J2).
The post-fix re-measurement above logged, for the *same* classes,
`innermost-rbp-belongs-to-unguarded-callee` (Flyway),
`active-safepoint-map-incomplete` (Integration) and
`xt-helper-window-conservative-scan` (Quartz) — and Quartz passed through
`innermost-rbp-belongs-to-unguarded-callee` at #4096/#8192 before reaching a
different reason at its #16384 peak. So the escalation is driven by whichever
conservative check refuses first on a given run, and a fix aimed at any single
reason will not stop it.

One of those reasons has a located cause. `innermost_frame_method`
(`vm/src/jit/conservative_roots.rs`) identifies the frame standing at
`exact_rbp` by reading the saved return address and then decoding the five-byte
`CALL rel32` immediately before it to recover the callee. An **indirect**
JIT→JIT call — virtual dispatch through an inline cache, i.e. `call rax` — has
no rel32 to decode, so `direct_call_callee` fails closed and the whole
collection drops to the non-moving sweep. That is why deeply virtual stacks
(Spring, Tomcat, Jersey, Netty) hit this and flatter workloads do not. The
callee already publishes its own `exact_rbp`; publishing its `CompiledMethod`
identity at the same point would remove the decode entirely. Not attempted here
— it changes every compiled frame push and wants its own A/B plus a full-suite
run.

Once the process is stuck in that mode, the young generation is swept but never
compacted, so its free list fragments monotonically. That end state is already
documented: a non-moving young sweep decays until the largest hole is a few KB
while the generation reads ~98% free. Allocation then fails with an
`OutOfMemoryError` naming a small object while most of the heap is idle —
exactly the `Integration` failure here, and exactly the previously-diagnosed
`ZipContentTests.nestedZip64CanBeRead` failure, which ran **512** consecutive
`reason=unregistered-jit-frame-on-stack` fallbacks, could not allocate a
`byte[8192]` with 1042 MB of old-generation headroom, and passed 29/29 under
`--nojit` (see the `jit_newarray` comment in `vm/src/jit/helpers.rs` and the A5
notes in `vm/src/jit/conservative_roots.rs`).

The A5 return-address filter (`is_plausible_return_pc`, 2026-08-04) narrowed the
false-positive source that caused the `ZipContentTests` case. **It does not cover
these four**: peaks of #4096 and #16384 are far past the 512 that filter was
built for. Whether the remaining fallbacks are further false positives or
genuine unregistered frames is not established here.

## `innermost-rbp-belongs-to-unguarded-callee` is the indirect-call case

`Log4J2LoggingSystemTests` is **100%** this one reason — all 17 logged lines, up
to #4096. It is now reproducible in 20 seconds, without Spring, JUnit or a
suite: `probes/MovingYoungFallbackCallFormProbe.java` runs identical allocation
and identical recursion depth through three call forms and the reason partitions
perfectly by form.

| Arm | Call form | iters / 20s | peak | reason observed |
|---|---|---:|---:|---|
| `iface` (4 receiver types) | indirect | 8.98M, 10.19M | 32, 32 | **100%** `innermost-rbp-belongs-to-unguarded-callee` |
| `virtual` (one final receiver) | indirect | 5.68M | 16 | **100%** the same |
| `static` (recursive `invokestatic`) | direct `E8 rel32` | 28.3M | 64 | **100%** `parent-frame-map-incomplete`, **zero** innermost-rbp |

That is what a failing `E8 rel32` decode predicts exactly.
`innermost_frame_method` (`vm/src/jit/conservative_roots.rs`) establishes which
`CompiledMethod` owns the innermost frame by decoding the five-byte direct CALL
immediately before the saved return address; a frame entered by an indirect call
— every megamorphic site, every inline-cache miss, every trampoline — has no
such encoding, so `direct_call_callee` returns `None` and the scan fails closed.

Corroborating, at zero build cost: the existing opt-out
`CRATONVM_GC_NO_CALLEE_RESOLVE=1`, which disables that resolution entirely, is
**completely inert** on the indirect arms — identical peak and identical line
count with it on and off. The resolution it gates never succeeds there, so
turning it off changes nothing. An inert lever is not an elimination; here it is
positive evidence that the decode is failing rather than being skipped.

### 2026-08-10, later: `Log4J2LoggingSystemTests` now COMPLETES on dev

Re-run on `cratonvm-mygc-20260810.exe` (dev `3cb0129f6`), twice, `--Xmx 2g`:

```
SBRUNNER_RESULT tests=61 failed=14 aborted=0 skipped=0 containersFailed=0
```

— in **under 600s**, the same 61/14 HotSpot reports, where the `6365de194`
binary did not finish in **3600s**. The row in the table above is therefore
stale for this class. Something between `6365de194` and `3cb0129f6` fixed it;
`cb947ffb8` ("retire a worker's TLAB before withdrawing it from tail
publication") is the plausible candidate but **this has not been attributed by
bisect** — do not cite it as the cause.

Its fallback count is also **intermittent between runs of the same binary**:
one run recorded 478 fallback cycles and completed, the next recorded **zero**
and completed. That is consistent with the binding obligation
(`xt-helper-window-conservative-scan`) depending on whether peer threads happen
to be parked inside JIT frames when a collection lands, rather than on anything
the class does deterministically.

**Consequence for pricing `XT_HELPER_WINDOW`:** the A/B of its kill switch
(`CRATONVM_JIT=xt-helper-window-scan=0` vs default) on this class was
**inconclusive** — both arms recorded zero fallbacks, and a reduction cannot be
measured from a zero baseline. Pricing it needs a workload with a *stable*
fallback rate AND peer threads. `probes/MovingYoungFallbackCallFormProbe.java`
has the stable rate (23–45 cycles per 20s run) but is single-threaded, so it
never produces this reason at all. The missing artifact is that probe plus
worker threads that park inside JIT frames.

### Priced: `XT_HELPER_WINDOW` is also worth ZERO — the binding obligation is `cross-thread-jit-peer`

`probes/MovingYoungFallbackPeerParkProbe.java` supplies what was missing: peers
parked *under compiled frames* (each worker warms a recursive method until it is
compiled, then re-enters it and blocks at the bottom of that chain) plus a main
thread allocating hard enough to force collections while they sit there. It
produces `xt-helper-window-conservative-scan` in every cycle, which no
single-threaded probe can do, at a stable rate.

Interleaved A/B of the existing kill switch, `CRATONVM_JIT=xt-helper-window-scan=0`:

| arm | iters | cycles | with `xt-helper-window` | with `cross-thread-jit-peer` | rate /Miter |
|---|---:|---:|---:|---:|---:|
| on | 7.25M | 27 | 27 | 27 | 3.73 |
| **off** | 7.61M | 28 | **0** | 28 | 3.68 |
| on | 7.70M | 28 | 28 | 28 | 3.63 |
| **off** | 9.07M | 33 | **0** | 33 | 3.64 |

The lever **works** — the reason disappears entirely, 27/28 → 0 — so this is not
the inert-lever ambiguity that made `CRATONVM_GC_NO_CALLEE_RESOLVE` unreadable.
The measurement is sensitive and the answer is still zero: the fallback rate is
flat across all four arms, because `cross-thread-jit-peer` is in **100% of
cycles in both**.

### The actual root: any peer thread with live JIT frames blocks compaction

`CROSS_THREAD_JIT_PEER` is *"another thread holds live JIT frames whose coverage
this thread's scan cannot verify and whose registers/stack are not rewritable."*
That is not a decoding bug, and no per-reason repair reaches it. It explains
every result on this page:

- **single-threaded probes** have no peers, so they fall back only for
  `innermost-rbp` — which is why repairing that looked like a 100% win there;
- **every real workload here** is multithreaded Spring, so a peer holds JIT
  frames essentially always, and moving-young is unreachable *regardless* of any
  other obligation being repaired.

So the honest framing is not "there is a bug making these four classes fall
back". It is that **moving-young does not currently survive contact with a
multithreaded workload**, and the four classes are just where that showed up as
a timeout. Three separate repairs were priced against real cycles and all three
came back at zero (`innermost-rbp` 0%, `xt-helper-window` 0%, and the
`CRATONVM_GC_NO_CALLEE_RESOLVE` path inert).

The only directions that can move this are structural, and should be priced
before being built:

1. **Bring blocked peers to a precise safepoint** so their roots become
   rewritable — principled, and the largest.
2. **Pin conservatively-found objects and compact around them** rather than
   declining the whole collection. This is what production collectors do with
   conservative roots, and it is the only option that converts a whole-heap
   refusal into a bounded cost.
3. **Keep threads out of JIT frames while blocked** — narrows the window without
   closing it.

### Measured: repairing the indirect-call path would convert NOTHING in a real workload

`CRATONVM_DBG_GC_FALLBACK_REASONS=1` records the full per-cycle reason **set**
(the stored reason is first-wins, so it cannot answer "would repairing X have
helped"). `attributable-innermost-rbp=yes` marks a cycle whose entire
incompleteness traces to the indirect-call resolution — counting the
`compiled-frame-band-unbounded` bit that the same `innermost_frame_method`
failure co-emits from the band walk.

| Workload | threads | cycles | attributable |
|---|---|---:|---:|
| probe `iface` (megamorphic) | 1 | 23 | **23 — 100%** |
| probe `virtual` (final receiver) | 1 | 45 | **45 — 100%** |
| probe `static` (direct `E8`) | 1 | 111 | 0 — different pair |
| **`Log4J2LoggingSystemTests`** | many | **478** | **0 — 0%** |

Every one of Log4J2's 478 cycles carries a **third, independent** reason:

```
all=xt-helper-window-conservative-scan,compiled-frame-band-unbounded,innermost-rbp-belongs-to-unguarded-callee
```

`xt-helper-window-conservative-scan` is a **cross-thread** obligation — "a
blocked peer's JIT helper window was scanned conservatively" — and nothing about
`innermost_frame_method` touches it. Repair the indirect-call resolution
perfectly and all 478 cycles still fall back on that reason alone.

**So the indirect-call repair is not the fix for these classes, and was not
attempted.** It would cost a store on every compiled frame push across three
compile doors and convert zero real cycles. The single-threaded probe said 100%
precisely *because* it is single-threaded; the discriminator is peer threads,
which every one of the four affected classes has and the probe does not. A
20-second synthetic reproducer generalised exactly backwards here.

The target is `XT_HELPER_WINDOW` (`vm/src/jit/xt_root_scan.rs`), not
`innermost_frame_method`. It has its own opt-out,
`CRATONVM_XT_HELPER_WINDOW_SCAN=0`, which is where a next pass should start —
price the ceiling with the existing lever before writing anything, the same way
`CRATONVM_GC_NO_CALLEE_RESOLVE` priced this one at zero.

### The earlier worry, resolved

Normalised per unit of work the three arms are comparable — static 2.3
fallbacks per million iterations, iface 3.6, virtual 2.8. The direct-call arm
does not fall back *less*; it falls back under a **different** reason. So making
indirect frames resolvable could simply move those collections into
`parent-frame-map-incomplete` instead of letting them compact.

Before paying for the fix — pairing a method identity with the recorded RBP
touches the frame-record store in the IR backend, the single-pass backend and
the shared allocation stub, on the path that runs at every compiled frame push —
the ceiling should be measured with a **diagnostic-only** build: on a
`FOREIGN_INNERMOST_RBP` fallback, record whether every *other* precondition was
already satisfied. That says how many of these collections would actually become
moving, with no behaviour change. The two worst classes (`Integration`,
`Quartz`) peak under different reasons entirely, so this fix is not expected to
help them.

## The count is the triage signal

The fallback peak separates the two outcomes cleanly, and cheaply:

- **single digits → harmless.** Flyway hits 7 pre-fix / **#4** post-fix and
  passes 73/73 either way; `LogbackLoggingSystemTests` logs none at all and
  passes 86/86.
- **thousands → death spiral.** Pre-fix: Integration #16384 → OOM, Quartz and
  Log4J2 #4096 → no completion. Post-fix: Quartz #16384 → OOM, Integration
  #16384 → passes but at 302x HotSpot.

The split survives the re-measurement, but note the last entry: **#16384 does
not always mean "fails"** — post-fix Integration reaches the same peak as the
OOM rows and still returns 34/34. The count predicts *death spiral*, and a spiral
ends either in an OOM or in a run so slow it is indistinguishable from one.

…with the caveat in the banner above: the count only reaches those thousands if
the process is allowed to run past the budget that would normally kill it.

`run-spring-boot-suite.ps1` now records `moving-young-fallback peak=#N <reason>`
in the `note` column of every row that logs one, so a future `HANG` shows the
escalation at a glance rather than costing a multi-hour standalone rerun to
discover. Validated against all five logs from this session.

## What is not established

- **Which fallbacks are false positives.** No attempt was made here to attribute
  individual fallbacks to real vs conservatively-misread frames. The
  indirect-call decode named under "Mechanism" is a *located* source, not a
  measured share of the total.
- **Whether `Quartz` also has a genuine stall.** Its retired doc raised an
  intermittent Netty/Tomcat lifecycle stall as a second candidate. The post-fix
  run weakens that: with the fabricated-connector defect gone, Quartz's failure
  is a plain `OutOfMemoryError` at fallback peak #16384, which the fallback
  alone explains. Not excluded, but no longer needed to explain any observed row.
- **Whether Flyway ever belonged on this page.** Post-fix it is faster with the
  JIT on than off and peaks at #4. It is kept here as the harmless-end
  calibration point for the count, not as an affected class.
- **Whether the `Log4J2` row moves too.** It was measured on `6365de194`, which
  already carries `67fadfdd8`, so it is not confounded — but it was not re-run
  with the rest of the post-fix table above.
- **Whether `--nojit` is a fix.** It is a diagnostic lever, not a remedy — it
  removes the trigger by removing the JIT.
- **Host load.** Four unrelated CratonVM processes from other sessions were
  running during these measurements. Absolute wall-clock is therefore an upper
  bound; the JIT-vs-`--nojit` contrasts (2.3x, OOM-vs-pass, no-finish-vs-pass)
  are far too large to be explained by it.

## Reproduce

```bash
CV=target/release/cratonvm.exe
CP="$(cat module/spring-boot-integration/build/cratonvm-test-cp.txt);<sb>/sb-runner"
# dies with OutOfMemoryError, fallback peak #16384
"$CV" --Xmx 2g            --cp "$CP" SbRunner org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests
# passes 34/34 in ~222s, zero fallbacks
"$CV" --Xmx 2g --nojit    --cp "$CP" SbRunner org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests
```

## Related

- `docs/known-issues/h2/h2-update-path-throughput-20260802.md` — names the
  non-moving fallback as a scaling target on an unrelated workload; same
  mechanism seen as throughput rather than as failure.
- `docs/internal/fixed-suite-bugs/springboot/flyway-integration-300s-margin-RETIRED-20260810.md`
- `docs/internal/fixed-suite-bugs/springboot/quartzendpointwebintegrationtests-recurring-timeout-RETIRED-20260810.md`
- `docs/internal/fixed-suite-bugs/springboot/log4j2-logback-loggingsystemtests-RETIRED-20260810.md`

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-flyway` | `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests` |
| `module/spring-boot-integration` | `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests` |
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` |
