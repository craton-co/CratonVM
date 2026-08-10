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

### …but fixing it may shift the reason rather than remove the fallback

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
