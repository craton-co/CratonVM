# ✅ RETIRED — the `[moving-young]` fallback spiral on four Spring Boot classes

## Status

**RETIRED 2026-08-18.** The mechanism this page documents — a JIT-triggered
`[moving-young]` fallback that escalates to peaks of #2048-#16384 and turns four
Spring Boot classes into timeouts and OOMs — **no longer reproduces**. It was
closed by work filed elsewhere, not by this page's own prescriptions.

Re-measured on `dev` `24a5d4528`, Azure Linux, one class per process,
`--Xmx 2g`, JIT on, real JDK 25 backend, the three load-bearing suite env vars,
400 s cap. HotSpot 25 on the same host as the control.

| Class | HotSpot | ZGC (shipped default) | `-XX:+UseGenerationalGC` | fallback peak, gen |
|---|---|---|---|---:|
| `FlywayAutoConfigurationTests` | ✓73/73, 9 s | ✓73/73, 79 s | 18 FAIL, 78 s | **#3** |
| `IntegrationAutoConfigurationTests` | ✓34/34, 11 s | ✓34/34, 85 s | 11 FAIL, 76 s | **#5** |
| `Log4J2LoggingSystemTests` | ✓63/63, 9 s | ✓63/63, 51 s | ✓63/63, 72 s | **#1** |
| `QuartzEndpointWebIntegrationTests` | ✓45/45, 18 s | OOM-killed | OOM-killed | 0 |

**The fallback peaks are #1-#5 where this page records #2048 and no completion
in 700 s, and all three classes that used to spiral now finish inside the 300 s
suite budget on both collectors.** By this page's own triage rule — "single
digits → harmless, thousands → death spiral" — the spiral is closed.

### What closed it, and what this page got right

Not the three repairs this page priced. Two merged changes did it, and both are
already recorded on their own pages: the young-pause-goal fix (`eb6e603f7`,
which this page itself measured at cycles 557 → 42) and the indirect-call
identity repair (`fix/moving-young-fallback-indirect-call-20260810`, measured
here at Log4J2 1238 cycles → 0). Both are on `dev`.

What this page got right is the part worth carrying forward. It refused to
generalise from a single-threaded probe; it priced three separate repairs
against real per-cycle obligation sets and reported all three at zero rather
than building the one that looked good on a microbenchmark; and it recorded the
A5 false-positive rate at 87% and then **declined to build the filter anyway**,
because suppressing an unshaped hit is unsound in exactly A5's own scenario. It
also called the risk that closed it: *"the fix might just move the failure."*

### It moved the failure. Twice.

Both successors are filed, and neither is this page's mechanism:

* **generational-young-relocation-nulls-live-string-references-RETIRED-20260818.md**
  (in this folder) — **now itself RETIRED and FIXED, same day.** With the
  refusals gone the young collector actually relocates on these classes, and
  Flyway and Integration failed 18/73 and 11/34 under Generational with
  `Method.getName()` returning null. The cause was NOT this page's mechanism
  and not a precise-map edge: `SharedVm::classes::proxy_method_cache` was
  scanned as a GC root but never remapped, so a moving cycle evacuated the
  cached `java.lang.reflect.Method` and left the cache pointing at from-space.
  Both classes are 73/73 and 34/34 under Generational post-fix.
* **`docs/known-issues/springboot/quartz-endpoint-web-jit-only-spin-loop-20260818.md`**
  — Quartz now dies on **every** collector, growing ~350 MB/s of native memory
  to 22 GB with `--Xmx` having no effect and **zero** `[moving-young]` lines in
  the log. `--nojit` passes it. Three hypotheses tested and refuted there.

So the honest summary of this page is: the spiral it named is gone, its refusal
to build a per-reason repair was correct, and the two things underneath it are
now visible and separately filed.

## Everything below is the page as it stood, kept for the mechanism

The body is unchanged. It is accurate about the Generational collector as of
2026-08-11 and is the only written description of the fallback machinery, the
obligation sets, the conservative-root pricing (~50 roots per parked peer,
linear) and why a semispace has nowhere to pin. Read it before touching
`gen_heap.rs` or `xt_root_scan.rs`. Its *conclusions about which classes are
red* are superseded by the table above.

---

## 2026-08-11: `Integration`, re-measured under Generational across four commits

Taken to settle the row this page assigns the A5 frame walk. One Windows host,
one protocol, one fixture, all four binaries built and run the same afternoon:
`-XX:+UseGenerationalGC --Xmx 2g --stack-dump-on-timeout 0`, the three
load-bearing env vars, `CRATONVM_DBG=gc-fallback-reasons`, killed at 700s.
"cycles" is the number of `[moving-young-reasons]` records, not the rate-limited
`fallback #N` log lines — those differ by ~8x and only the former is a count.

| commit | result | cycles | peak | obligation set in ~every cycle |
|---|---|---:|---:|---|
| `892ab4f40` — **this page's own measurement commit** | **TIMEOUT @700s** | 1912 | #1024 | `unregistered-jit-frame-on-stack` + `band-unbounded` + `innermost-rbp` (99.4%) |
| `a79c7d03f` (36 commits later) | 34/34, **426s** | 557 | #512 | `band-unbounded` + `innermost-rbp` (99.6%) — **A5 absent** |
| `eb6e603f7` (the young-pause-goal fix, `a79c7d03f`'s child) | 34/34 | 42 | #32 | mixed; no single set dominates |
| dev @ `4ec8d51e7` | 34/34, **269s** and **215s** | 75 | #64 / #16 | `cross-thread-jit-peer` + `xt-helper-window` (51%) |

Linux (Azure, idle) for the same class, same collector: **34/34 in 121s**, 3
cycles, peak #3 on dev; **34/34 in 98s**, 46 cycles, peak #32 on `2ad5dd0d2`.
Its siblings there are green too — `Log4J2LoggingSystemTests` 63/63 in 57s (10
cycles, peak #32), `FlywayAutoConfigurationTests` 73/73 in 97s (9 cycles, peak
#16). `QuartzEndpointWebIntegrationTests` discovers **0 tests** on the Azure
fixture and was not measured; that is a fixture gap, not a result.

Three things follow, and the third is the one that matters.

**1. This page's row was real.** Rebuilt at its own commit it reproduces
exactly — TIMEOUT at 700s, peak #1024, A5 in 99.4% of cycles, against this
page's "931/931". The methodology holds up; nothing here is a re-reading of a
contaminated run.

**2. `Integration` is green now, on both platforms, and the largest single step
is the young-pause-goal fix.** `eb6e603f7` cuts cycles 557 → 42 and peak
#512 → #32. That is the fix whose own commit message says
`adapt_young_trigger_to_pause`'s feedback loop "was inert" on the non-moving
branch — and a workload in this page's spiral takes the non-moving branch at
essentially every allocation, so it was inert for precisely these classes. The
young generation was collecting far more often than the goal asked, and each
collection re-ran the whole fallback path.

**3. A5's share is not a property of anything this page can point at.** It is
100% of cycles at `892ab4f40` and **0%** at `a79c7d03f`, and
`git diff 892ab4f40 a79c7d03f -- gc/src vm/src/jit/conservative_roots.rs` is
**empty**. The collector did not change. The probe did not change. What changed
in that range is `vm/src/jit/helpers.rs` (+476 lines), `jit/src/tiered.rs` and
`jit/src/x64/licm.rs` — code that alters how deep the VM's own native frames go
and what they leave behind in them.

That is exactly what the A5 probe reads. It is a raw word scan over the stack
band above the registered JIT entry chain, and a stale return address left in
the uninitialised middle of a live VM frame is indistinguishable to it from a
live compiled frame. So its rate is a function of VM stack residue, and any
change to call depth moves it — between 100% and 0% of cycles, in this case.

### The A5 false-positive rate, measured: 87%

`CRATONVM_DBG=a5-census` (added with this measurement) reports, for every A5
firing, how many band words look like JIT return addresses and how many of those
sit at a slot with real frame shape — a saved caller RBP one slot below,
8-aligned, at a HIGHER address, itself the base of a frame whose return-address
slot holds a plausible PC. Both x64 backends open every compiled body with
`push rbp; mov rbp, rsp`, so that shape is available for every genuine JIT
frame; it is the same invariant this file's other RBP-chain walks already rely
on.

On `Integration`, dev, Windows: **28,483 firings, of which 24,846 (87.2%) had
`shaped=0`** — not one candidate in the band sat at a slot with frame shape.
The band itself is small, 70 KB to 325 KB, so this is not a scan-size artefact.

### What this does to the assigned repair

The prescription reads *"Integration → a frame walk. `unregistered-jit-frame-on-stack`
is in 931/931 cycles; 322 of them carry NOTHING else, so a correct A5 answer
alone converts 35%."* Re-priced on dev, A5 is in **12 of 75** cycles on Windows
(all of them sole, so a perfect A5 converts 16%) and in **0 of 3** on Linux.
The class passes either way.

**The filter is deliberately NOT built here, and the reason is soundness, not
effort.** Suppressing a hit whose slot has no frame shape is unsound in exactly
A5's own scenario. A compiled frame entered from VM Rust code stores *that
caller's* RBP at `slot - 8`; this tree does not build with forced frame
pointers, so a Rust caller may be using RBP as a general register, and the
saved word is then not a stack address at all. The filter would reject a real
unregistered JIT frame — re-arming the corruption the A5 comment describes at
length, in the one direction where being wrong is fatal. The sound route is the
other option that comment already names: **register the entry-point transition**,
so there is nothing for a residue scan to find. The 87% says how much that is
worth; it does not license the shortcut.

## 2026-08-11: re-measured on current dev under all three collectors

**The collector this page calls "default" stopped being the default the day
after these classes were measured.** `GcAlgorithm`'s default is `Zgc` as of
2026-08-10 (`vm/src/config.rs`, and `zgc` joined the default feature set in
`gc/Cargo.toml` and `vm/Cargo.toml` so a plain `cargo build` gets it). The
switch was made partly ON this mechanism: the 651-class Tomcat three-way run
found 62 of the 63 classes that are non-PASS under Generational while passing
under both other backends log `[moving-young] fallback`
(`../tomcat/gc-backend-3way-fullsuite-comparison-20260810.md`). This page's own
G1 numbers also predate `79c302916`, the fix for G1's unaligned TLAB carves.

Windows, dev `892ab4f40`, one class at a time, `--Xmx 2g`, JIT on unless noted,
same launch as `run-spring-boot-suite.ps1` (the three load-bearing env vars,
`--stack-dump-on-timeout 0`, `--add-opens=java.base/java.net`). HotSpot control
is Temurin 25.0.3 on the same host. `gen` is `-XX:+UseGenerationalGC`, the
escape hatch this page's body describes; `zgc` is the bare default.

| Class | HotSpot | **ZGC (the default)** | Generational | G1 |
|---|---:|---|---|---|
| Flyway | 8.1s ✓73/73 | **294.0s ✓73/73**, 0 fb | 160.1s ✓73/73, peak **#4** | 176.3s ✓73/73, 0 fb |
| Integration | 9.0s ✓34/34 | **240.4s ✓34/34**, 0 fb | **no completion in 700s**, peak #2048 | 219.6s ✓34/34, 0 fb |
| Quartz | 8.6s ✓45/45 | **151.6s ✓45/45**, 0 fb | **no completion in 700s**, peak #2048 | 185.8s ✓45/45, 0 fb |
| Log4J2 | 3.8s ✓61/61 | **76.7s ✓61/61**, 0 fb | **no completion in 700s**, peak #2048 | 90.7s ✓61/61, 0 fb |

- **All four pass under the shipped default, and all four are inside the 300s
  per-class budget** (294.0 / 240.4 / 151.6 / 76.7). No `slowClasses` entry is
  needed, which is the same conclusion this page reached by a different route.
  Flyway at 294.0s has almost no margin and is the one to watch.
- **G1 passes all four too**, which settles this page's explicit caveat — "before
  G1 is proposed as the default for these classes, the same comparison should be
  run on one of them". Run on all four, the synthetic-probe result generalised:
  zero fallbacks, zero crashes, and on Flyway G1 is 1.7x faster than ZGC.
- **The mechanism is intact on Generational**, and worse than the body records:
  three of the four now reach peak **#2048** and do not finish in 700s. The
  count remains the triage signal, and Flyway remains the harmless-end
  calibration point at #4.
- **The JIT is a net loss on these classes even under ZGC**, so the collector
  switch masks the cost rather than removing it: Integration is 240.4s with the
  JIT and **110.0s** under `--nojit` (2.2x), Quartz 151.6s vs 146.0s. That is
  the `--nojit` A/B under ZGC this page lists as unestablished; it no longer
  decides pass-vs-fail, only speed.
- **Log4J2's 14 failures are gone.** The body records "61, 14 fail" as
  pre-existing and environmental, identical on HotSpot and CratonVM `--nojit`.
  On this host and this fixture the HotSpot control is 61/61 and so is every
  CratonVM arm. Whatever caused them was fixed or was environmental to the
  earlier host; the row should not be cited as a known-failing class.
- **Five fallback reasons appear, not four.** Quartz's Generational run alone
  logs `xt-helper-window-conservative-scan` (9), `innermost-rbp-belongs-to-
  unguarded-callee` (4), `unregistered-jit-frame-on-stack` (2) and
  `compiled-frame-oop-not-published` (1) — the last is not in the body's list.
  Integration and Log4J2 are ~94% and 100% `innermost-rbp`. This strengthens
  rather than weakens the body's argument that no per-reason repair reaches it.

**What this does not say.** The mechanism is not fixed — it is no longer on the
default path. Anyone running `-XX:+UseGenerationalGC` still gets all of the
below, and the structural options at the end of this page are still the only
things that would repair it. ZGC's own cost is documented in `docs/gc-tuning.md`
(no compaction, ~1.5x heap on buffer-churning workloads).

**Harness note, because it invalidated two earlier attempts at this table.**
`$proc.Kill($true)` is a no-op on Windows PowerShell 5.1: a timed-out VM
survives, and a Generational arm in a fallback spiral will happily burn a core
for another half hour beside the next arm. `taskkill /T /F` by exact PID is the
one that reaps it, and even that needs verifying — three of the four `gen` arms
above had to be reaped by the driver after `taskkill` returned SUCCESS. The
table above was taken by a single driver that refuses to start an arm while any
`cratonvm` is alive. Both failure modes produced *plausible* numbers rather than
obvious breakage (a HotSpot arm read 17.6s contaminated, 11.4s partly
contaminated, 9.0s clean), so an idle-host assertion is not optional here.

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

**Still the right call as of 2026-08-11, now for a second reason.** Under the
shipped default all four finish inside the existing 300s budget with the JIT on
(294.0 / 240.4 / 151.6 / 76.7s), so there is nothing for a raised budget to buy.
The first bullet's reasoning was specific to Generational and is worth keeping
straight: it is true there — those three fail rather than run long — and it
would NOT have been true had the default merely been slow. Flyway's 294.0s is
close enough to 300s to be worth a note if it ever flips to HANG.

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

### Do not build pinning: it already exists in G1, and it works

The pinning design priced below **is already implemented**, in the G1 backend.
`G1::jit_pinned_region_set` (`gc/src/g1.rs`) takes
`gc_quiescence::pinned_jit_roots_snapshot()` — the same conservative JIT roots
counted above — maps each to its containing region, and excludes those regions
from the collection set, "exactly like JNI-pinned regions" (JEP 423). That is
precisely "pin the conservative roots, compact around them", at region
granularity.

Measured on `MovingYoungFallbackPeerParkProbe`, 20s, 4 parked peers, `--Xmx 512m`:

| collector | iters | conservative roots found | reason cycles | fallback lines | collections |
|---|---:|---:|---:|---:|---|
| default (Generational) | 5.39M | 193 | 20 | 9 | `young: cycles=0 coverage_fallbacks=17` |
| **G1** (`-XX:+UseG1GC`) | 6.21M | 161 | **0** | **0** | **`evacuates=7 pauses=7`** |

G1 still *finds* the conservative roots — it is not ignoring them — and performs
**7 evacuating collections** where the default performs 17 collections and moves
on **none** of them. The zero is not vacuous: `evacuates=7` is the check that it
actually compacted, and it was made specifically because "0 fallbacks" and "0
collections" are indistinguishable in the fallback counter alone.

### Why there is no pinning "hook" to add to the default collector

The default young collector is a **Cheney semispace**: it evacuates from-space
to to-space and then recycles from-space wholesale. A semispace has nowhere to
leave a pinned object — the space it sits in is the space being reclaimed. The
source says so directly: *"a semispace cannot pin a conservative JIT root nor
rewrite a register-resident one"* (`gen_heap.rs`). Adding pinning there is not a
hook; it is replacing the algorithm with a region- or block-based one, at which
point it is G1.

**So the route to un-redding these four classes is a collector that compacts
around conservative roots, not new pinning code.** That is a different and much
better-scoped problem, and it is already owned elsewhere — G1 has its own open
regressions (see the 3-way collector comparison recording G1 SIGSEGVs, and
`fix/g1-fullsuite-regression-20260809`). This page's contribution is the
measurement that says the collector question is the whole question.

**Resolved 2026-08-11, and by the default rather than by G1.** The caveat this
section closed on — "the table above is a synthetic probe with four peers parked
under a depth-12 recursion; before G1 is proposed as the default for these
classes, the same comparison should be run on one of them" — has been paid. Run
on all four real classes, G1 passes every one with zero fallbacks, so the probe
generalised. But the shipped default moved to ZGC on 2026-08-10 and it passes
them too, so the route taken was the collector switch, not G1 maturity. See the
2026-08-11 section at the top.

### Priced: pinning is CHEAP — ~50 conservative roots per parked peer, linear

The one structural option that can be priced without building it, because the
conservative root set is already counted (`XT_HELPER_WINDOW_ROOTS`, exposed by
`CRATONVM_DBG=xt-jit-root-scan`). The pin set is exactly that set: the objects a
pinning collector would have to leave in place while compacting everything else.

`MovingYoungFallbackPeerParkProbe`, 15s per arm, `--Xmx 512m`, roots per
helper-window pass:

| parked peers | roots / pass | per peer | passes |
|---:|---:|---:|---:|
| 1 | 41 | 41.0 | 22 |
| 2 | 85 | 42.5 | 22 |
| 4 | 190 | 47.5 | 19 |
| 8 | 403 | 50.4 | 18 |

Dead linear at **~50 roots per parked peer**, and stable run to run (the 4-peer
figure reproduced at 194 in a separate 25s run, every one of its 28 passes
identical).

Extrapolating: a Spring app with 40 threads parked in JIT frames pins ~2,000
objects per collection; 200 threads pins ~10,000. For a compactor those are
small numbers — a region- or block-based young collector marks the blocks
holding them non-evacuable and compacts the rest. **The comparison that matters
is against the status quo, where a SINGLE parked peer forces the entire
collection to be non-moving.** Trading "compact nothing" for "leave ~50 objects
per peer in place" is the whole prize on this page.

Three caveats on the number:

- These are conservative **candidates** — stack/register words that resolve to a
  live object, including duplicates and false positives. Distinct pinned objects
  is `<=` the figure, so ~50/peer is an upper bound.
- A false positive pins a dead object, retaining garbage until the next cycle.
  That is safe, and bounded by the same ~50/peer.
- The per-peer constant is workload-shaped: this probe parks each peer under a
  depth-12 recursion of one compiled method. Deeper or wider frames scan more
  words. **The linearity is the robust finding; the constant is not.** A real
  workload should be measured before the number is used for sizing.

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

> **Superseded for two of the three classes, 2026-08-11.** `cross-thread-jit-peer`
> is no longer in every real cycle: it is in 103/103 of Quartz's and **0** of
> Log4J2's (419) and Integration's (931). Multithreadedness is therefore not the
> universal discriminator this bullet makes it — a Spring class can be as
> multithreaded as any other and still have no cross-thread obligation on the
> cycles that fall back. See the corrected pricing above.

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

### BUILT AND MEASURED 2026-08-11: the indirect-call repair lands, and Log4J2's spiral goes to ZERO

The repair the sections below decline is now implemented: codegen publishes the
compiled method's IDENTITY into a TLS slot beside the RBP it already stores, in
the same instruction pair, so `innermost_frame_method` no longer has to decode
the call that created the frame. A/B on one binary pair,
`-XX:+UseGenerationalGC`, `--Xmx 2g`, one class at a time:

| Class | base | fix | priced beforehand |
|---|---|---|---|
| `Log4J2LoggingSystemTests` | TIMEOUT@700s, 1238 cycles, peak #1024 | **203.1s, 0 cycles, 61/61** | 419/419 attributable |
| `IntegrationAutoConfigurationTests` | TIMEOUT, 1510 cycles | TIMEOUT, 821 cycles | 0% — A5 in 100% |
| `QuartzEndpointWebIntegrationTests` | TIMEOUT, 747 cycles | TIMEOUT, 869 cycles | 0% — cross-thread in 100% |

**The two that do not move are the result, not a shortfall.** Both were priced
at zero from their per-cycle obligation sets BEFORE the code was written, and
both held — Quartz still carries `cross-thread-jit-peer` in 867 of 869 cycles.
Three predictions, three hits.

**Integration shows the "the fix might just move the failure" risk directly**,
which ["The earlier worry"](#the-earlier-worry-resolved) below raises. With the
frame now resolvable, its dominant set changed from
`{unregistered-jit-frame-on-stack, compiled-frame-band-unbounded,
innermost-rbp}` to `{active-safepoint-map-incomplete,
unregistered-jit-frame-on-stack, compiled-frame-oop-not-published}`.
`innermost-rbp` is GONE; what replaced it are checks that only become
*reachable* once the innermost frame resolves. The repair works everywhere and
converts a cycle only where nothing else was failing too — which is exactly why
it had to be priced per class rather than on a probe.

Neutral under the shipped default, where these classes already pass: ZGC Log4J2
261.2s → 231.5s, Quartz 502.6s → 406.7s, 61/61 and 45/45 either way. Unit tests
cratonvm-jit 1977, cratonvm-gc 1463, cratonvm-vm `jit::` 153, zero failures.

Two implementation notes that cost a verification round each:

* **The identity must be captured WITH the RBP, not read at scan time.** The
  first version read the mirror inside `innermost_frame_method` and converted
  NOTHING — 785 of 786 cycles unchanged. `exact_rbp` is a snapshot in
  `PreciseFrameInfo`; by scan time the owning thread has entered and left other
  compiled frames, and the scan often runs on the COLLECTOR's thread, whose
  mirrors describe a different stack. It is now snapshotted into
  `PreciseFrameInfo::exact_cm_id` beside the RBP it belongs to.
* **Both halves move together or the pair lies.** `try_call_compiled_entry`
  brackets a Rust-side compiled call by saving and restoring the RBP mirror.
  Restoring only that half pairs the caller's rbp with the callee's identity,
  and nothing downstream can detect it — both halves still read consistently out
  of the mirrors. A confidently wrong identity is worse than an absent one.

### CORRECTED 2026-08-11: the indirect-call repair now converts 100% of Log4J2, and the binding obligation is PER CLASS

**Everything in the section below was true when measured and is false now for
two of the three classes.** Re-measured on dev `892ab4f40` with
`CRATONVM_DBG=gc-fallback-reasons` under `-XX:+UseGenerationalGC`, one class at
a time, tallying the full per-cycle obligation SET (not the first-wins label):

| Class | cycles | obligation present in EVERY cycle | attributable to innermost-rbp |
|---|---:|---|---:|
| `Log4J2LoggingSystemTests` | 419 | `innermost-rbp` + `compiled-frame-band-unbounded`, **and nothing else** | **419 — 100%** |
| `IntegrationAutoConfigurationTests` | 931 | `unregistered-jit-frame-on-stack` | 0 |
| `QuartzEndpointWebIntegrationTests` | 103 | `cross-thread-jit-peer` **and** `xt-helper-window-conservative-scan` | 0 |

Log4J2 has exactly ONE distinct obligation set across all 419 cycles. The
`xt-helper-window-conservative-scan` that made this 0% below is **gone** from
its cycles, and with it the reason this repair was declined. The same
measurement that priced the repair at nothing now prices it at everything —
for that class.

So the section below is right about Quartz and wrong about Log4J2, and the
generalisation it draws ("the discriminator is peer threads, which every one of
the four affected classes has") no longer holds: on this commit **each class
has its own stable binding obligation**, which also revises "the reason is not
a property of the class" under "Mechanism" above.

Consequences for whoever picks this up — three separate repairs, not one:

* **Log4J2 → publish the callee's identity at frame push.** Converts 419/419.
  This is the repair "Mechanism" already specifies. Design obstacle found while
  scoping it: the prologue must embed the identity as an immediate, and the
  `CompiledMethod` does not exist until after codegen, so the id has to be
  RESERVED before compilation and back-filled. That reaches the compile
  pipeline, not just the five `emit_mov_tls_disp32_rbp` sites
  (`ir_lower::emit_frame_record`, `ir_lower::emit_post_call_frame_record`,
  `x64::frames::emit_prologue`, `x64::frames::emit_post_call_rbp_republish`,
  `runtime_lowering::emit_post_call_frame_republish`). A dense u32 id stored as
  `mov dword <seg>:[disp2], imm32` needs no scratch register, which is what the
  RAX-preserving republish paths require.
* **Integration → a frame walk.** `unregistered-jit-frame-on-stack` is in
  931/931 cycles; 322 of them carry NOTHING else, so a correct A5 answer alone
  converts 35%, and with the Log4J2 repair 464/931 = 50%. A byte-level filter
  cannot get there: the hits are genuine return addresses that have already
  returned, sitting in the uninitialised part of a live frame, which is why
  `is_plausible_return_pc` did not move the count.
* **Quartz → the drain, not the compaction.** `xt-helper-window` sets
  `unrewritable_peer_state`, which clears `selective_on` — the non-moving
  sweep's ONLY young->old drain. Quartz is therefore not merely failing to
  compact; it has no drain at all, which is `HIB-GCOVERHEAD-HALFFULL.1` reached
  through a different door. Narrowing that gate needs conservative pinning to
  accept INTERIOR pointers first (`is_object_address` accepts only exact object
  starts, and a suspended peer's register may hold a derived pointer). There is
  no address->containing-object lookup in `gc/` today — the "object grid" in
  `arena.rs` is 8-byte alignment, not a start bitmap — so this wants a young
  object-start bitmap before it is safe.

None of the three is a small change, and the promotion gate in particular has a
documented OOM-regression history. Price each against the table above rather
than against the (now stale) 0% below.

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
- **Whether Flyway ever belonged on this page.** ANSWERED 2026-08-11: no. It
  passes 73/73 under all three collectors and peaks at #4 under Generational. It
  is kept here as the harmless-end calibration point for the count, not as an
  affected class.
- ~~**Whether the `Log4J2` row moves too.**~~ ANSWERED 2026-08-11: it moves, and
  further than expected. 61/61 with **zero** failures on the HotSpot control and
  on every CratonVM arm, so the "14 fail" in the table above is stale as well as
  the timing.
- **Whether `--nojit` is a fix.** It is a diagnostic lever, not a remedy — it
  removes the trigger by removing the JIT.
- **Host load.** Four unrelated CratonVM processes from other sessions were
  running during these measurements. Absolute wall-clock is therefore an upper
  bound; the JIT-vs-`--nojit` contrasts (2.3x, OOM-vs-pass, no-finish-vs-pass)
  are far too large to be explained by it.

## 2026-08-10 reconciliation — confirmed on `Integration`/`Quartz`, but only the DEFAULT collector shows the named mechanism

Reconciling the 139-class non-passed union from a same-day `default`/`g1`/`zgc`
rerun (binaries `cratonvm-{default,g1,zgc}-20260808f.exe`, `dev@6365de194`, a later
tip than the `f695ca875` binary this page's own measurement used).
`IntegrationAutoConfigurationTests` and `QuartzEndpointWebIntegrationTests` are both
TIMEOUT/HANG at ~300s under **all three** collectors this round:

| Class | default | G1 | ZGC |
|---|---:|---:|---:|
| `IntegrationAutoConfigurationTests` | 300.104s | 300.200s | 300.170s |
| `QuartzEndpointWebIntegrationTests` | 300.123s | 300.012s | 300.109s |

The symptom (HANG at the 300s ceiling) is **collector-agnostic**. The *mechanism*
recorded above is only directly confirmed on the default collector this round,
though:

- **default**: both `.err.log`s are loud — `[moving-young] fallback #N` lines
  (Integration reached #256, Quartz #128, both climbing) interleaved with
  `gc::guard: young non-moving sweep was about to ZERO a span containing a LIVE
  (marked) object` retentions and `gen_heap: selective promotion: unwound N
  candidate(s)` warnings, active right up to the kill — the exact signature this
  page already names.
- **G1 and ZGC**: both `.err.log`s are near-silent — only the routine
  post-clinit-fixup/Mockito-self-attach boilerplate (7 lines each), zero GC/JIT
  diagnostic output for the whole run. `[moving-young]` is default-collector
  terminology (the non-moving *young* sweep fallback is specific to the
  Generational collector's compaction path), so its absence under G1/ZGC is
  expected and does not by itself mean those two collectors are clean — it means
  this page's specific instrumentation doesn't fire there.
- Both classes' `.out.log`s show steady progress on all three collectors (repeated
  Spring/Integration context start-stop or Quartz scheduler cycles, new timestamped
  output up to the moment of the kill on every collector) — no collector shows a
  dead/silent process, so none of these are the previously-fixed STW/AB-BA deadlock
  families.

**Not established by this rerun:** whether G1 and ZGC are hitting an equivalent
JIT-driven degradation via a different (un-instrumented) code path, or are simply
too slow under the JIT for an unrelated reason and would finish given more budget.
The standalone measurement above found the JIT itself is what breaks these
classes on this page (`--nojit` clears them) — that root explanation does not
depend on which GC is compacting the young generation, so the G1/ZGC HANGs are
consistent with the same underlying JIT-triggered cause without independently
proving it. A `--nojit` A/B under G1 and ZGC (the same protocol "The measurement"
above used) would settle it.

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-{default,g1,zgc}-20260808f-s2/all-jit/logs/module_spring-boot-integration.org.springframework.boot.integration.autoco*-9d4bdd63bbf0.{out,err}.log`,
`.../module_spring-boot-quartz.org.springframework.boot.quartz.actuate.endpoint*-e3de43e38499.{out,err}.log`.

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
- `fixed-suite-bugs/springboot/flyway-integration-300s-margin-RETIRED-20260810.md`
- `fixed-suite-bugs/springboot/quartzendpointwebintegrationtests-recurring-timeout-RETIRED-20260810.md`
- `fixed-suite-bugs/springboot/log4j2-logback-loggingsystemtests-RETIRED-20260810.md`

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-flyway` | `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests` |
| `module/spring-boot-integration` | `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests` |
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` |
