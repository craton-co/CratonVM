# HQL/ANTLR parser — cold full-context prediction runs interpreted (~1000× HotSpot)

**Status:** 🔴 **OPEN** (root-caused 2026-06-20). Throughput, **not** a correctness bug, **not** a loop, **not**
a broken cache, **not** GC-bound. This is the underlying cause of Hibernate census cluster **H4**
(`function.json.JsonArrayUnnestTest` "timeout") and a latent drag on any HQL-heavy workload.
Same family as the deferred ANTLR deep-recursion / instance-method-tier-up cluster
([[jit-instance-methods-no-invocation-tierup]],
[springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md) §6–7).

## Symptom

`em.createQuery(hql)` for an HQL statement with a **multi-item select list** takes tens of seconds to PARSE
on CratonVM (HotSpot: milliseconds). A test class issuing several distinct complex queries accumulates these
costs and trips the suite's 600s per-class timeout — reported as a "hang".

## Minimal repro (`_hibrepro/HqlParse.java`)

Bootstrap any trivial SessionFactory (one `@Entity`, H2), then time `createQuery`. The parse happens before
semantic resolution, so the HQL can reference non-existent entities — it still parses first, then throws
`UnknownEntityException` (HotSpot) or hangs (CratonVM). Measured on the merged-`dev` binary:

| HQL | CratonVM | HotSpot |
|---|---|---|
| `select e.id from Book e` (1 select item) | **12.7 s** | ~ms |
| `select e.id, e.name from Book e` (2 items) | **52–58 s** | ~ms |
| `select e.id, index(p), p.name from Book e …` (3 items) | **> 600 s** (timeout) | ~ms |

≈ **4–5× per added select item** — super-linear, but it **terminates** (the 2-item case completes at 52 s).

## Root cause (what it is NOT, then what it is)

Ruled out by experiment:
- **Not an infinite loop** — the 2-item parse completes (`@@THREW after 52 s`).
- **Not a broken ANTLR cache** — re-parsing the same *shape* with a different entity (so Hibernate's
  query-plan cache does NOT hit) drops 55,836 ms → **649 ms** (86×). ANTLR's shared DFA cache warms up
  correctly; the cost is **one-time per grammar shape per JVM**.
- **Not GC-bound** — `-Xmx8g` gives the same ~52 s as the default heap.
- **Not the JIT helping** — `--nojit` (51.9 s) ≈ JIT-on (55.8 s). The JIT does **not** accelerate the hot loop.

What it is: ANTLR4's **cold full-context (LL) prediction** (`ParserATNSimulator.adaptivePredict` →
`closure`/`computeReachSet`) for the `selectionList`/`selectExpression` grammar decisions runs **entirely in
the interpreter**. Those methods are deeply **recursive** and are always on the call stack, so although they
cross the invocation threshold (default 500), CratonVM has no **on-stack replacement (OSR)** to switch the
in-flight recursive frames to compiled code — the freshly-compiled method body is never entered, and the
parse runs interpreted at ~1000× HotSpot. (A `CRATONVM_DBG_JIT_COMPILE` capture of the 56 s parse shows the
ANTLR/ATN/`HqlParser` methods are **never** compiled.) HotSpot wins via JIT + escape analysis on the
allocation-heavy config objects.

## Why it surfaces as a "hang" in the suite

The census runs **fork-per-class**, so each class re-pays the cold per-shape cost from scratch; a class with
several distinct complex queries (e.g. `JsonArrayUnnestTest`) exceeds 600s. **Mitigation:** running the
Hibernate suite in a **single shared JVM** amortizes the DFA-cache warmup across classes (warm parse = sub-second).

## Fix direction (deferred, large)

The real fix is **JIT throughput for the recursive ANTLR ATN-simulation hot loop** — i.e. OSR / effective
tier-up for on-stack recursive methods (the deferred bug-C / instance-method-tier-up work). No quick targeted
VM fix; this doc nails the mechanism + minimal repro for that project.
