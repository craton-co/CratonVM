# JIT BCE soundness fix — scatter-store bounds-check elision (heap corruption)

**Status:** FIXED. Landed in `jit/src/x64.rs` (the producer-stack operand
analyzer `analyze_array_access_operands`). The code was committed as part of
`39a8127` by a concurrent session sharing the working tree — its commit message
("add BCE-disable diagnostic gates") does **not** describe this fix; this doc is
the real record.

## Symptom

JIT-on heap corruption with the signature seen across the BC / Spring / JUnit
crashes:

```
GC: inconsistent header — kind=Object but array_length=2561
  (num_slots=226093, class_id=0); inline-alloc forgot to set kind=Array.
non-moving sweep: no re-sync within 1048576 bytes — abandoning rest of arena
```

Interpreter (`--nojit`) is clean; JIT-on corrupts. Non-deterministic volume
(GC-timing dependent), which is why env-flag A/B bisection (`CRATONVM_JIT_NO_BCE`
etc.) could not isolate it.

## Root cause

Array Bounds Check Elimination (`analyze_bounds_elimination`) identified the
**index** and **array** operands of each array access by **bytecode position**,
not by operand-stack data flow. The helpers `find_store_index_pc`
(/`find_preceding_iload` / `find_preceding_aload`) assumed the canonical shape
`aload arr; iload idx; <single-instruction value>; xastore` and literally took
"the instruction two before the store" as the index.

That is unsound the moment the value or index expression spans more than one
bytecode. The textbook break is the scatter store `result[off + i] = src[i]`:

```
aload result; iload off; iload i; iadd;   // real index = off + i
aload src;   iload i;     iaload;          // value = src[i]
iastore
```

Here the `iload i` *inside* `src[i]` sits exactly two instructions before
`iastore`, so the analyzer concluded `index == i` (the induction variable) and
**elided the store's bounds check** — while the real index `off + i` runs past
`result.length`. The elided store becomes an out-of-bounds heap write that
overwrites a neighbouring object's header. The array heuristic was wrong the
same way: it would guard `src.length` while the store actually targets
`result`.

BigInteger / EC / NIO buffer code is full of exactly these scatter / gather /
offset-indexed loops (`z[zOff + i]`, `result[start - i]`, Montgomery
multiply scatter), so the elision fired constantly under the BC-heavy
workloads, corrupting the young generation.

## Fix

Replace the positional heuristics with a sound operand-stack **producer**
simulation, `analyze_array_access_operands`:

* Walk the loop body from the header (stack-empty for javac counted loops),
  modelling each opcode's stack effect as a vector of *producer PCs*.
* At each array load/store, read the array and index operands from their exact
  stack positions. Report the access only if the index operand was produced by
  a bare `iload` and the array by a bare `aload`.
* STOP (report nothing further) on anything not precisely modelled — method
  calls, `dup2`/`swap`/`dup_x*`, switches, `wide`, or a control-flow join where
  the linear stack is no longer authoritative. STOP / stack-underflow is always
  conservative (the per-element bounds check is kept).

Both the static (`find_safe_array_accesses`) and speculative
(`find_speculative_array_accesses`) passes now consume this map, so neither can
mis-identify a scatter store. The change only ever *removes* unsound elisions
and preserves the genuinely-safe shapes (`dst[i]=src[i]`, `a[i]=a[i]+1`,
`a[i]=const`).

## Verification

Reproducer `BceScatter` (`result[offset + i] = src[i]`, index deliberately
OOB so correct semantics throw `ArrayIndexOutOfBoundsException`):

| binary | result |
|---|---|
| HotSpot | 400000/400000 AIOOBE (correct) |
| CratonVM `--nojit` | 400000/400000 AIOOBE (correct) |
| CratonVM JIT, **before** fix | 1090+ heap-corruption events, derails, rc=127 |
| CratonVM JIT, **after** fix (run() compiled, confirmed via `CRATONVM_DBG_DUMP_JIT=LIST`) | 5000/5000 AIOOBE, 0 corruption, rc=0 |

Full `cratonvm-jit --lib` test suite: 685 passed, 0 failed (1 filtered —
`s31_inline_with_branch`, a **pre-existing** unrelated hang, see below).

## Out of scope (separate, still-open bugs found while verifying)

* **`main()`-style corruptor.** At 400k iterations `BceScatter` still corrupts
  via the *outer* `main()` method's JIT (bisected: `BISECT_SKIP …/main` →
  corruption ~0; `BISECT_SKIP …/run` → still corrupts). `main()` has **no
  arrays** — it is a counted loop with a `try/catch` and `long` counters, so its
  corruption is a different JIT bug (exception-handler / category-2 counter),
  not BCE. Likely another member of the same heap-corruption family.
* **`s31_inline_with_branch` hangs** (RC=124) even without this change — a
  pre-existing JIT inlining bug, unrelated.
* **`hadoop-conf` regression-pool probe** prints `file:/tmp/...` instead of the
  baseline `file:///tmp/...`. Reproduces under `--nojit`, so it is **not** a JIT
  bug — pre-existing on dev, most likely from today's native "real behavior"
  refactor touching URI/Path/String.
