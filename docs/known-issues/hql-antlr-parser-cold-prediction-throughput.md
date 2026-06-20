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
`closure`/`closureCheckingStopState`/`computeReachSet`) for the `selectionList`/`selectExpression` grammar
decisions runs **entirely in the interpreter** at ~1000× HotSpot. HotSpot wins via JIT + escape analysis on
the allocation-heavy config objects.

### Why the JIT never engages (instrumented 2026-06-20 via `CRATONVM_DBG_JITC`)

Confirmed at the compile-pipeline level — the hot ATN-simulation methods **cross the invocation threshold
(500) but are declined by the single-pass JIT backend**, so they never run compiled:

- `ParserATNSimulator.closureCheckingStopState` (3925 compile attempts), `ParserATNSimulator.closure_`
  (3922), `ATNConfigSet.add` (1311), `ParserATNSimulator.ruleTransition` (1003),
  `PredictionContext.mergeArrays` (316) — all repeatedly hit `try_compile` and bail, then short-circuit via
  the dynamic `jit_bail_list` (`is_jit_bail_listed`).
- The bail is **not one opcode and not a code-size cap**: these are complex, object- and exception-heavy
  bodies (`closure_` alone has `athrow`×2 + `checkcast`×6 + `instanceof`×3 + `invokeinterface`×5). They are
  exactly the method shapes the **single-pass C1-style x64 backend** (`jit::x64::compile` / `jit_scan`)
  declines; `ATNSimulator.getCachedContext` is the one method observed taking a real `backend_attempted=true`
  backend bail. (Ruled out: `code_len==0` — 0 occurrences when instrumented; an infinite retry loop — the
  bail-list short-circuit already prevents re-running the gauntlet.)

So it is a **JIT backend-coverage** gap, not a single bug, and not (only) the on-stack-recursion/OSR angle:
even a non-recursive hot method of this shape (`ATNConfigSet.add`, `mergeArrays`) is declined.

## Why it surfaces as a "hang" in the suite

The census runs **fork-per-class**, so each class re-pays the cold per-shape cost from scratch; a class with
several distinct complex queries (e.g. `JsonArrayUnnestTest`) exceeds 600s. **Mitigation (works today):**
running the Hibernate suite in a **single shared JVM** amortizes the DFA-cache warmup across classes (warm
parse = sub-second).

## Fix direction (deferred, large — NO safe quick fix)

The real fix is **broadening the JIT backend to compile these complex object/exception-heavy methods** (and,
for the deepest recursion, OSR / tier-up for on-stack methods) — the deferred JIT-throughput / bug-C cluster.
This was investigated to the compile-pipeline level (above) and there is **no one-line VM fix**: the blocker
is the single-pass backend's method-shape coverage, and force-lifting individual codegen gates (athrow with
control flow, etc.) is a substantial, correctness-sensitive change that should not be rushed onto `dev`. This
doc + `_hibrepro/HqlParse` nail the mechanism and give a fast minimal repro for that project.
