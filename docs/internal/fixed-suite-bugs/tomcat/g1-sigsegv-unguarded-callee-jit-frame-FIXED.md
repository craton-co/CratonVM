# G1 GC: deterministic SIGSEGV from an unregistered JIT frame — the mechanism in this title was wrong

| | |
|---|---|
| **Status** | **CLOSED 2026-08-11**, retired here out of `known-issues/tomcat/`. All four crashes are gone on a binary carrying the unaligned-TLAB-carve fix, verified by re-running the exact run that produced them. The mechanism this page named — an unguarded JIT frame's *root* pointing at freed memory — was never it, and the diagnostic that suggested it has itself been fixed. |
| **Discovered** | 2026-08-10, complete 651-class Tomcat suite run under `-XX:+UseG1GC`, 2 workers |
| **Closed by** | `79c302916` `fix(g1): TLAB carves must be 8-aligned` (`gc/src/g1.rs`, `tlab_carve_size`), root-caused on the Spring Boot suite. |

## Verdict, 2026-08-11

Same fixture, same runner, same shape as the run that found it: three
concurrent full-suite arms (default / G1 / ZGC), one per collector, 2 workers
each, `-Xmx2g`, 300 s per-class cap, 651 classes. The G1 arm additionally
carried `CRATONVM_DBG=g1-dbg-reach`.

| Class | G1, 2026-08-10 | G1, 2026-08-11 |
|---|---|---|
| `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1ValidWrite511` | **CRASH** 129 s | PASS 141.9 s |
| `…startup.TestHostConfigAutomaticDeploymentWar` | **CRASH** 187.9 s | PASS 240.8 s |
| `…startup.TestHostConfigAutomaticDeploymentWarXml` | **CRASH** 177.1 s | PASS 128.8 s |
| `…startup.TestHostConfigAutomaticDeploymentModification` | **CRASH** 226.1 s | HANG 300.1 s |

`Modification` is now a timeout rather than a crash. That is not a residual of
this defect: this page's own standalone runs already had it TIMEOUT on **both**
arms (plain G1 and `COVERAGE_PIN`), and recorded that it therefore "carries no
signal at all here". It is an ordinary hang that the crash used to pre-empt.

Whole-arm comparison, all three collectors, both dates:

| | PASS | FAIL | HANG | CRASH | wall |
|---|---:|---:|---:|---:|---:|
| Default 08-10 | 519 | 16 | 115 | 0 (+1 NOSUMMARY) | 356.5 min |
| **Default 08-11** | **628** | 12 | **11** | 0 | **177.5 min** |
| G1 08-10 | 578 | 35 | 34 | **4** | 267 min |
| **G1 08-11** | **623** | **7** | 20 | **1**† | 212.9 min |
| ZGC 08-10 | 604 | 18 | 29 | 0 | 247.1 min |
| **ZGC 08-11** | **629** | 11 | **11** | 0 | 178.4 min |

† **Zero in production configuration — corrected 2026-08-11.** The one crash was
a **different class**,
`org.apache.tomcat.integration.httpd.TestChunkedTransferEncodingWithProxy`, and
it turned out to be caused by the diagnostic this arm carried, not by G1: the
fault was inside `dbg_verify_reachable_integrity`, the verifier
`CRATONVM_DBG=g1-dbg-reach` enables, which I had set on the **G1 arm only**.
Same class, same heap, same collector, flag off: 3 runs, 3 passes; flag on:
2 crashes in 3. See
fixed-suite-bugs/tomcat/g1-sigsegv-chunked-transfer-httpd-proxy-20260811-FIXED.md.

So the G1 arm of the re-run crashed **zero** times on anything a user would
run, and "G1-only" for that class was really "flag-only". The measurement of
this page's own defect is unaffected — the walk-break instrument and that BFS
are different code — but the arm's crash column should be read as 0.

### The mechanism-level measurement

`CRATONVM_DBG=g1-dbg-reach` prints `[g1][WALKBRK]` / `[g1][DESYNC-COVER]` at
exactly the abandoned-walk this defect *is*. Across the whole 651-class G1 arm:

| | |
|---|---|
| `[g1][WALKBRK]` + `[g1][DESYNC-COVER]` | **0** |
| class stderr files carrying `[g1][FREED]` (so the instrument was live and the collector really evacuated) | **135** |

**Honest limit on that number.** The instrument's RED was *not* re-demonstrated
on this surface. A binary with `& !7` removed from `tlab_carve_size` was built
and run against three synthetic allocation workloads (uniform, odd-length, and
one sized to make the carve `≡ 4 mod 8`) and against the Spring Boot
`Log4J2LoggingSystemTests` reproducer at 2 g / 512 m / 256 m; none of them broke
the walk, because the mechanism needs a **JIT-pinned** Eden region held out of
the collection set and walked as a remembered-set source, which those workloads
never produce. So `WALKBRK = 0` is corroboration, not proof. The proof that the
instrument fires when the defect is present is in the Spring Boot page's own
instrumented runs (four breaks per failing run → zero after the fix). The
primary evidence here is the crash column.

Also worth carrying forward: the default collector went from 115 HANGs to 11
and from 356 min to 177 min over the same day's work. Whatever this page said
about the default collector being "slow but safe" is now a much smaller effect.

## What it actually was

`G1Collector::refill_tlab` carved TLABs whose size was not a multiple of 8.
`bump_alloc` commits the full size to `region.cursor` while `Tlab::new` rounds
its `end` DOWN to 8, so the bytes between the two ends were covered by no
filler, no skip span and no object. A linear walk read them as an all-zero
16-byte object, desynced 8 bytes off the real grid, and abandoned the walk —
leaving every heap reference past that offset un-rewritten by the pause.
`GenerationalHeap::refill_tlab` has masked with `& !7` since 2026-07-18; G1's
copy never got it, which is exactly why it was G1-only. Full derivation in
known-issues/springboot/g1-fullsuite-regression-20260808.md §3c.

## Why this page believed something else — and the diagnostic that told it so

All four dumps carried these three lines together:

```
#  gc collector: g1
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen actual: 0 moving cycle(s), 0 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee
```

Read as a whole that says: G1 is running, its young generation is a moving
Cheney copy, and the last root-gathering pass could not cover an unguarded
callee's frame. From there "an unguarded JIT frame's root points at freed or
relocated memory" is a short step, and this page took it — and proposed fixing
it in G1's root scanner.

**Two of those lines could not say what they appeared to say.**
`record_moving_young_cycle` and `record_moving_young_coverage_fallback` are each
called from exactly one place, `gen_heap.rs`. `moving_young_enabled()` is read
nowhere in `g1.rs`. So under `-XX:+UseG1GC` the policy line describes a
collector that is not running, and both counters are zero **by construction**,
whatever G1 did — in a process that, measured properly, evacuated thousands of
regions. A structural zero read as a measurement.

The reason line is genuine: `g1.rs` does call
`mark_moving_young_coverage_incomplete_because`, and
`set_unregistered_jit_frame_on_stack` comes from the collector-agnostic
`jit::conservative_roots`. `innermost-rbp-belongs-to-unguarded-callee` is a real
condition and a real **precondition** — it is what pushes the collector onto the
linear-walk path, which is where the unaligned carve bit. That is why it
correlated so well. It was never the cause.

Fixed alongside this retirement: `gc_state_lines_for` (`vm/src/runtime/crash_handler.rs`)
now emits the `young-gen policy` / `young-gen actual` pair only for the
generational collector, and for any other says so in as many words —
`gc young-gen policy/actual: n/a — those counters are the GENERATIONAL young
collector's and are never incremented by this one, so they would read zero
whatever happened`. Guarded by
`gc_state_lines_omit_generational_only_counters_under_another_collector`, which
was verified to fail when the gate is removed.

## What did not work, kept because it cost real time

### `CRATONVM_G1_COVERAGE_PIN` could not be used as the discriminator

The lever only tells you something when a crash is there to survive it, and
standalone there is no crash. Eight standalone runs (four classes × plain G1 /
`COVERAGE_PIN`) produced **zero** `EXCEPTION_ACCESS_VIOLATION`;
`…DeploymentWar` passed cleanly in 287 s and `TestHttpServletDoHead…` in 85 s.
The pin arm's own timeouts (287 s pass → 600 s timeout; 85 s pass → 575 s
failure) are the lever's documented no-op-pause cost, **not** evidence about the
defect.

### 4-way concurrency was not enough either

All four classes launched simultaneously under G1, own `-Xmx2g` each, 900 s cap,
two rounds: zero crash reports in 8 more process-runs (16 across both
experiments). `…DeploymentWarXml` passed here where it returned EXIT=1
standalone, so these classes' *ordinary* outcomes are load-dependent too — but
the crash never appeared.

Both findings held up: the crash needed the full-suite shape, and reproducing it
meant re-running the actual 651-class arm, which is what finally answered it.

### The original symptom, for the record

Three of the four crashed at the same faulting instruction
(`pc=0x00007FF74B49243F`, RVA `0x135243F`) — one JIT-compiled address, not three
independent defects. The fourth crashed elsewhere with the identical diagnostic
shape plus `the faulting thread had an UNREGISTERED JIT frame on its native
stack in the last root-gathering pass`.

## The rule to take away

A crash report that prints one collector's counters while another collector is
running will be believed. Before building a mechanism on a dump line, find the
line's **increment**, not just its value — and check that the code path which
increments it is the one that was running.

Same shape as reference `a counter printed before it is incremented`: a
structural zero is not an observation.
