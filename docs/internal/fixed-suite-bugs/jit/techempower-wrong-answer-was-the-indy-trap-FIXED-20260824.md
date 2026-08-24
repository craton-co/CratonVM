# TechEmpowerTest's wrong answer was the pre-bridge `invokedynamic` trap — one flag on today's binary puts it back

**Status: FIXED. Closed on `dev` by
`730d3e0d9 Merge perf/reactive-dispatch-and-vector-depth-20260823: compiled
code can now EXECUTE an invokedynamic`, not by anything on this branch.**
Established 2026-08-24 by a **same-binary A/B**: `CRATONVM_JIT_INDY_BRIDGE=0`
on today's `dev` binary reproduces the documented failure 4/4, in the
documented bimodal distribution, and the default arm passes 4/4 beside it.

This retires `techempower-jit-wrong-answer-20260822.md`. Everything that page
measured stands; what it lacked was the lever, and the lever turned out to be a
flag that already existed.

---

## 1. The three-arm base rate, because a clean streak is not a verdict

The page's reproducer is `org.hibernate.reactive.techempower.TechEmpowerTest`,
JIT on, live Postgres via Testcontainers. Six rounds, interleaved run-by-run in
one series on one Azure host (1-minute load 10–19 throughout), three binaries:

| binary | result | wall |
|---|---|---|
| `95c210f37` (2026-08-22, a `dev` ancestor) | **FAIL 6/6** | 9–32 s |
| `3a4dc5626` (today's `dev`) | **PASS 6/6** | 47–65 s |
| this branch (dev + the OSR/exception-routing fixes) | **PASS 6/6** | 52–66 s |

Interleaving is what makes the clean arms readable: the failing arm ran between
them all afternoon, so "it did not fail today" is not a statement about the
host or the hour. The old binary's six failures all carry the page's own
signature — `Expected status code 200 or 204, but was 500`, four of them with
`NullPointerException … because "w" is null` — and none is a harness artifact
(§6).

That establishes the defect is gone. It does not say what closed it.

## 2. The lever: `CRATONVM_JIT_INDY_BRIDGE=0`, on today's binary

The `dev` range between the failing binary and the passing one is 426 commits.
Rather than bisect it, note that two of the four non-documentation merges in it
ship their own kill switch, and try those on the CURRENT binary — a same-binary
A/B needs no build and rules out every difference except the one flag.

Four rounds, all three arms interleaved, `cratonvm-dev` (`3a4dc5626`) for every
run:

| arm | result | wall | mode |
|---|---|---|---|
| **`CRATONVM_JIT_INDY_BRIDGE=0`** | **FAIL 4/4** | 21, 21, 24, **307** s | `npe=2`, `npe=1`, `npe=4`, fixture deadline |
| `CRATONVM_MONITOR_PENDING_NOTIFY=0` | PASS 4/4 | 32–46 s | — |
| default (control) | PASS 4/4 | 38–47 s | — |

The failing arm does not merely fail — it reproduces the page's **bimodal
distribution** (§3 of that page: "NPE → 500 in 3 of 4 runs at 21.7–31.3 s; the
fixture's own 300 s Vert.x deadline in 1 of 4"). Three fast wrong answers at
21–24 s and one 307 s deadline is that table, re-measured from the other
direction two days later.

The lost-wakeup fix (`CRATONVM_MONITOR_PENDING_NOTIFY`) is exonerated by the
same series: its OFF arm passes 4/4 in the same interleave.

It reproduces on this branch's binary too (`CRATONVM_JIT_INDY_BRIDGE=0`,
2 runs, both FAIL), which is the check that says the branch's own fixes did not
paper over it.

## 3. Why an indy trap lands on THIS workload

Without the bridge, a method containing any non-`StringConcatFactory`
`invokedynamic` takes an unconditional reason-8 uncommon trap on its first
compiled execution and is retired `MakeNotCompilable`; OSR is refused for it
method-wide (RBC.7, `compile_osr_artifact`). Hibernate Reactive's entire control
flow is lambdas.

`CRATONVM_DBG_JITC=1` on a reproducing run (`npe=1`) says so by name — every
line below is `org/hibernate/reactive/`:

```
osr-DENY (osr-exc-site-unpublished pc=78 opcode=0xba)
    AsyncTrampoline$TrampolineInternal.unroll(...)
bg-direct-call DECLINED CompletionStages.loop(IILjava/util/function/IntFunction;)…: indy-trap
bg-direct-call DECLINED Cascade.cascadeProperty(…): indy-trap
bg-direct-call DECLINED Cascade.cascadeInternal(…): indy-trap
bg-direct-call DECLINED ForeignKeys.collectNonNullableTransientEntities(…): indy-trap
inline-resolve REFUSED  CompletionStages.loop(…) depth=0: invokedynamic
inline-resolve REFUSED  AsyncTrampoline.asyncWhile(…) depth=0: invokedynamic
```

`0xba` is `invokedynamic`, and `AsyncTrampoline.unroll` is the loop driver the
whole reactive chain runs through.

**This is what the page's bisect table was seeing.** §8.1 established that
compiling `org/hibernate/reactive/` is NECESSARY and not SUFFICIENT, and §9.1
concluded the partner (`java/util/ArrayList`, `Arrays`, `io/vertx/sqlclient/`)
must be a *catalyst* rather than a culprit — "they need not share a mechanism,
only an effect on compilation of the code around the load path". That reading
was right, and the reason it was right is here: the indy-trap population is
entirely inside `org/hibernate/reactive/`, and what a partner supplies is the
compile/OSR timing that decides whether a trapped body's resume is taken from a
stale state.

What this does NOT establish is the last step — whether the wrong value comes
from a stale OSR resume re-running committed iterations (RBC.7's documented
hazard), or from a continuation abandoned at the trap. The A/B pins the
component and the mechanism's entry point; it does not pin the instruction.
Recorded as open rather than guessed, because that page's own history is a
sequence of confident mechanisms that measurement then withdrew (§8.3 → §9).

## 4. What the page got right, and the one thing that misled it

Right, and worth keeping:

* **§2.1** — the rows are all present (`count=10000 minId=1 maxId=10000`), so
  the write path is exonerated. Still true.
* **§9** — `find()` itself returns the `null`, before the list is involved;
  `ArrayList`/`Arrays` exonerated as the SOURCE. Still true.
* **§10** — the failing id is random every time, so it is a race and not a bad
  row. Still true, and it is exactly what a resume-state defect looks like.
* **§9.1's caution** — a bisect names components whose compilation is necessary
  or sufficient FOR THE SYMPTOM, which is not the component that computes the
  wrong value. That caution is the most transferable thing on the page.

What misled it: the search stayed inside the code the workload runs. Every
next-step list — `ReactiveDeferredResultSetAccess`, the load plan, the
`Mutiny`/`Uni` glue, concurrency scaling — names a Java class to instrument.
§7's step 2 ("sweep the JIT feature switches, cheapest first … one run each
answers it") was the step that would have found it, was written down twice, and
was never run. It costs one run per switch and it is the only step on the page
that could have named `CRATONVM_JIT_INDY_BRIDGE`.

## 5. The instrument the page needed and did not have

Its own §10.2 records the problem exactly: instrumenting `WorldVerticle` dropped
the failure rate from 4/4 to 5/9 and made the passing runs markedly slower. A
defect that needs the fast path to lose is a defect a probe can switch off.

The A/B here touches no Java at all. The fixture is pristine, the binary is
unchanged between arms, and the only difference is a process-wide flag — which
is why it can be scored at 4/4 against 4/4 in one interleaved series rather than
argued from 5 of 9.

## 6. Harness note: a "fast FAIL" that is not the defect

A TechEmpower run can leave its VM alive after the harness has its `@@RESULT`
line — `main() returned; VM held alive by 23 non-daemon thread(s)` — and it
keeps LISTENING on port 8088. The next run then dies in ~7 s with
`java.net.BindException: Address already in use: 0.0.0.0:8088`, which looks
exactly like the defect's fast wrong-answer mode (~9–31 s) and is not it. It
cost this session one whole invalidated batch, and it is the kind of artifact
that would read as a reproduction.

`te.sh` now kills every TechEmpower VM it owns by PID (never `pkill -f`, which
matches the persistent ssh session too), waits for the port, and SKIPS the run
outright if the port is still held rather than recording a failure. Every run
quoted above was audited for it: `grep -c "Address already in use"` is `0` in
all eighteen logs of §1 and all twelve of §2.

## 7. Reproduce

```bash
# fails 4/4 on ANY current binary, in both documented modes
CRATONVM_JIT_INDY_BRIDGE=0 \
  <cratonvm> --java-home <jdk25> --Xmx 1500m @common.args \
  -Djunit.jupiter.execution.timeout.default=600s \
  -Dcraton.batch=1 CratonRunner org.hibernate.reactive.techempower.TechEmpowerTest

# passes 4/4 — same binary, flag removed
```

Score on `grep -c 'getRandomNumber()'` **and** on
`grep -c 'Expected status code 200 or 204, but was 500'`. The first alone
misses the deadline mode and misses the runs whose first server-side error is
`NonUniqueObjectException: A different object with the same identifier value
was already associated with this persistence context for entity [World with id
'252']` — a second wrong answer the 2026-08-22 binary produces from
`createData`, in 2 of its 6 failures, which the page never recorded because
its grep could not see it. (§9.2 of that page warns about exactly this class of
grep artifact, in the other direction.)

## 8. Related

* `techempower-jit-wrong-answer-20260822.md` — the page this retires.
* `hibernate/hib-reactive-3gc-run-regressions-20260820.md` §7.3 — where the 500
  was first seen, and still cites the retired page.
* `jit::indy_bridge_enabled` (`CRATONVM_JIT_INDY_BRIDGE`) — the flag, and the
  measurement that motivated the bridge.
* `compile_osr_artifact`'s RBC.7 comment — the stale-resume hazard an indy trap
  in an OSR'd body opens, which is the leading candidate for §3's open step.
