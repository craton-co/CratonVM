# G1's inline post-write barrier: it already existed, and it is worth 2.7x

*2026-09-03. Branch `feat/g1-ref-store-plan-20260903`.*

## What this was going to be

The residual list after the reference-store work said G1 published no barrier
plan and would need a third gate shape — a same-region test — because neither
existing shape (a flags-byte mask, an unsigned age floor) can express "are
these two references in the same region".

That shape already exists. `x64::objects::emit_g1_barrier_filter` (F-08) emits
exactly it:

```
test  val, val              ; a null stored reference: the barrier returns
jz    done
mov   scratch, &JIT_G1_BARRIER
sub   obj, [scratch + ARENA_BASE]
sub   val, [scratch + ARENA_BASE]
xor   obj, val
and   obj, [scratch + REGION_MASK]  ; same region: the barrier returns
jz    done
call  jit_g1_post_write_barrier
```

Both tests are transcriptions of `post_write_barrier_rset`'s own first two
early-outs — a null referent, and `src_region == dst_region` — so a skipped
call is a call that would have returned having done nothing. It is behind
`CRATONVM_G1_INLINE_BARRIER` (opt-IN), a live `JIT_G1_BARRIER` table and a
wired helper.

So the work was not to design it. It was to find out whether it does anything,
because nothing had ever counted it.

## What was missing was the count

Its only engagement signal was one `tracing::info!` line per process saying the
arm had been emitted at least once. That says the arm exists. It does not say
how many sites got it, and — the whole question — it says nothing about how
often the filter actually spared the call.

This change adds the census: sites at compile time, and, under the existing
`CRATONVM_DBG_SP_REF_STORE_TRACE=1`, `skipped`/`called` at run time. `bt16`
under G1:

```
[cratonvm] G1 inline post-write barrier: sites=6
[cratonvm]   G1 barrier executions: skipped=29961707 called=2785
[cratonvm]   ref-store executions, other inline arms: fresh-ctor=0 body=29964492
```

**29,961,707 of 29,964,492 barrier executions never leave compiled code.** 2,785
do. The filter's two tests answer "nothing to remember" 99.99% of the time on
this workload, which is what a generational-shaped allocation pattern looks
like from G1's point of view: almost every reference a fresh object stores
points into the region it was just allocated in.

## The measurement

`bt16` under `-XX:+UseG1GC`, one binary, order alternated by round:

| arm | median wall |
|---|---|
| `CRATONVM_G1_INLINE_BARRIER=1` | **0.82 s** |
| default (off) | 2.22 s |

**8 of 8 rounds**, ~2.7x, and all sixteen runs printed the same tree checksum
`[14985902]` — so every arm did the same work. This is the largest measured
effect in this line of work, and it is for code that was already written.

## Why it was off

`CRATONVM_G1_INLINE_BARRIER` is `present(...)`: opt-in, no default. The
conjoined conditions in `g1_inline_barrier_available` — the switch, a live
`JIT_G1_BARRIER` table, a wired `g1_post_write_barrier` — read like a feature
being kept behind a gate until someone measured it. Nobody did, and there was
no instrument that would have made the omission visible: with the arm off the
census reads `sites=0`, which is indistinguishable from "this collector has no
such arm".

## Default ON since 2026-09-04

It is now the default. The two things this page said were missing have been
supplied:

- **A 228-program differential soak under G1**, comparing the new default
  against `CRATONVM_G1_INLINE_BARRIER=0`, with each workload first run twice at
  the default to prove it is reproducible at all: **166 agree, 2 divergent, 43
  non-deterministic, 17 already failing.** Both divergences were examined —
  `CpuClockCheck` is a clock probe (`wall=500,0ms` against `500,1ms`), and
  `InvokeAllCount` returned 127 once on the *barrier-off* arm while the
  regression suite was running concurrently, then passed 5 of 5 on re-run in
  both arms. rc=127 is the classic failed-exec code. Zero semantic divergences.
- **The remembered-set audit** (`CRATONVM_G1_DBG_RSET=1`), which is the check
  that would catch the failure mode the WildFly card-miss was:
  **6,392 audited collection cycles, 3,237,469,944 edges examined, `missing=0`**
  with the barrier on. Non-vacuous: `rset_completeness_counts` was deliberately
  split from its printing wrapper so a unit test can prove it CAN report a
  violation — its own comment says "`missing=0` from a checker that is
  incapable of returning anything else is not evidence".

Plus `regression-suite/run.sh` **90/90 under `-XX:+UseG1GC`** with the barrier
on by default, and `=0` verified to restore `sites=0`.

### The evidence this page already had

- `regression-suite/run.sh` 88/88 under `-XX:+UseG1GC` with the barrier ON, and
  88/88 on the default collector with the branch's other changes.
- 2.7x on bt16, 8/8, checksums identical.
- The filter is a transcription of the helper's own early-outs, and the
  fall-through calls the collector's real barrier.

What is not:

- No real application has been run under G1 with it on. The reference-store
  arms taught this lesson the hard way — 3.2x on a probe, and *slower* on an H2
  test (see `singlepass-ref-store-two-shapes-and-the-fourth-door-20260902.md`
  and the H2 numbers in this branch's commit) — so a probe result is not a
  licence to change a default.
- A missing remembered-set edge is invisible until a collection frees a live
  object, and `audits/g1-audit.md` §8.1 (G1-2) is the record of how carefully
  this exact interlock has been kept. Flipping it deserves its own change, with
  a real application and an rset audit (`CRATONVM_DBG_VERIFY_RSET`) in the gate.

The census is what makes that next step checkable, and it is the part that was
missing.

## Levers

- `CRATONVM_G1_INLINE_BARRIER=1` — emit the inline filter. Off by default.
- `CRATONVM_DBG_SP_REF_STORE_TRACE=1` — the run-time `skipped`/`called` pair.
  A `LOCK INC` per execution, so a diagnostic arm and never a timed one; the
  2.7x above was measured without it.
