# Hibernate five-class investigation — final results

**All five classes pass on DEFAULT flags.** No `CRATONVM_NO_MOVING_YOUNG=1`, no
other opt-out. One release binary, `-Xmx2g`, Eclipse Adoptium JDK 25.0.3.9
fixture, whole classes via `CratonRunner`, quiet host.

| Class | Start of investigation | Final (default flags) | HotSpot |
|---|---:|---:|---:|
| `jpa.lock.LockTest` | FAIL 26.5 s (14/15) | **PASS 10.2 s (15/15)** | FAIL 10.3 s (14/15) |
| `bulkid.OracleInlineMutationStrategyIdTest` | PASS 441.7 s | **PASS 118.9 s** | 41.4 s |
| `hql.ASTParserLoadingTest` | PASS 420.6 s | **PASS 137.4 s** | 21.0 s |
| `type.temporal.OffsetDateTimeTest` | TIMEOUT >900 s | **PASS 244.4 s** | 51.1 s |
| `type.temporal.ZonedDateTimeTest` | TIMEOUT >900 s | **PASS 305.7 s** | 64.7 s |

Every row is `failed=0` with `ok`/`aborted` counts **identical to HotSpot**
(Zoned 404/204 of 608; Offset 324/164 of 488; ASTParser 106/106; Oracle 6/6).
Four of five are inside the suite's 300 s per-class cap; `ZonedDateTimeTest` sits
just over it at 306 s.

`LockTest` now passes **15/15 — better than HotSpot on this host**, which fails
its `assertTimeout(5 s)`. The VM simply became fast enough to meet the test's own
wall-clock assertion. It was never a VM defect (see
`docs/internal/fixed-suite-bugs/hibernate/locktest-pessimistic-write-timeout-is-not-a-vm-bug-20260730.md`);
it is now green anyway.

## The two fixes

**1. Relocation-safety gates keyed on a flag instead of on the hazard**
(`f78b72670`). The optimizing IR (C2) tier and direct JIT→JIT calls were gated
on `!moving_young_enabled()`, which is `true` by default — so every compile fell
through to single-pass C1 and the optimizing tier contributed nothing, guarding
against a frame that could not occur. Scoped to
`JIT_PUBLISHES_RELOCATION_CONTRACT`; the runtime veto reads the same constant so
they cannot drift. Took ASTParser 376→~190 s and Oracle 322→188 s.

**2. The moving-young veto keyed on compiled code *existing* rather than a
compiled frame being *live*** (`11901e9a6`). This is the big one. The veto fired
from the first compile onwards, forever, including on cycles with no compiled
frame on any stack — so **the young generation never compacted in a JIT-enabled
run.** Every cycle took the in-place sweep, which reclaims into a free list
without retreating the bump cursor, so the space fragments and the next
collection arrives sooner:

    OffsetDateTimeTest, default flags
      before   TIMEOUT >1500 s    2048+ minor collections
      after    PASS   225 s          16 minor collections
      control  PASS   222 s          12 minor collections  (NO_MOVING_YOUNG=1)

A 170x collection-count explosion. The hazard is an un-rewritable frame that is
*live*, which is exactly `is_active() || unregistered_jit_frame_on_stack()` —
the pair `gen_heap` already computes as `has_conservative_roots`, and whose own
comment says that with no JIT frame on any stack "the moving collector is then
fully precise and correct".

## How long it took to find, and why

Four hypotheses were eliminated by bisection before the right one, each with a
committed lever and a full quiet-host run:

| lane | result |
|---|---|
| default | TIMEOUT 1500 s |
| `CRATONVM_JIT_MY_SCRATCH_FLUSH=0` | TIMEOUT 1500 s |
| `CRATONVM_JIT_MY_SELFCALL_PROOF=0` | TIMEOUT 1500 s |
| both of the above | TIMEOUT 1500 s |
| `CRATONVM_JIT_MY_SHADOW_EMISSION=0` | TIMEOUT 1500 s |
| `CRATONVM_NO_MOVING_YOUNG=1` | PASS 777 s |

Every codegen suspect was wrong; the cost was never in generated code.

**The diagnostic gap that caused it.** `print_gc_summary` printed only the
moving-young line, which is emitted *only when moving-young is requested* — so a
`CRATONVM_NO_MOVING_YOUNG` run printed nothing at all, and "did this
configuration collect more?" could not be answered from a log. It now always
prints `[GC] generational: minor=N major=N`. With that one line the answer was
immediate (12 vs 2048+). Two cheap counters would have saved four wrong turns.

**Two workloads that cannot show this bug**, and which it was first wrongly
measured on: `BinTreesClassic` is a tight allocation loop *inside* compiled code,
so every cycle has a live JIT frame either way (31 vs 26 collections), and
`LockTest` performs **zero** young collections. Pick a probe that exercises the
mechanism, not just one that is fast.

## Safety

Unchanged where it matters. A live compiled frame still vetoes, and `gen_heap`'s
independent `has_conservative_roots && !moving_young` term would force the sweep
even without the veto. Checksums hold: `BinTreesClassic 18` returns the HotSpot
`68332206` at both `-Xmx2g` and `-Xmx512m`, and a `--nojit` 128 m `bt16` returns
`14985902`.

Suites: `cratonvm-jit` 1058 lib + every integration target 0 failed;
`cratonvm-gc` 873 passed 0 failed; `cratonvm-vm` 2300 passed, 1 failed — the
pre-existing `obsaudit_attach_listener_creates_a_real_socket`, which binds a
Unix domain socket at `/tmp/...` on Windows.

## Residual

`ZonedDateTimeTest` at 306 s is ~6 s over the 300 s cap. The remaining gap to
HotSpot (4.7x on the temporal pair, 2.9x Oracle, 6.5x ASTParser) is ordinary
interpreter/JIT throughput work, not a defect in this family. Note the run that
timed out in the five-class sweep completed in 306 s standalone on the same
binary — the first class in a sweep pays warm-up the others do not.
