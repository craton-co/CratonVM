# `CompletableFuture` composition ran INTERPRETED — a stale `ForkJoinTask` blocklist, and a `dup_x2` the backend could not prove

## Status
**FIXED, 2026-08-27.** Both refusals this page chased are gone, and each was
priced on one binary with its own kill switch:

| arm (one binary, ABBA x4, idle host) | compose ms | hot-but-stuck |
|---|---:|---:|
| both fixes (shipped) | **846** | **0** |
| `CRATONVM_JIT_NO_DUP_X2=1` | 1 854 | 1 |
| `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1` | 2 437 | 2 |
| both reverted (= dev tip) | 3 272 | 3 |

`HibfixComposeProbe2` at the page's own headline configuration — 24 threads,
4.8 M chains — went from **108.9 s to 27.6 s (3.95x)**, against HotSpot's
244 ms: the gap this page opened at 442-872x is **113x**.

What remains is a different problem and has its own page:
[`../../known-issues/perf/juc-primitives-are-9-114x-after-the-composition-compile-refusals-20260828.md`](../../known-issues/perf/juc-primitives-are-9-114x-after-the-composition-compile-refusals-20260828.md).

## What the two defects were

### 1. RFJP.1 — a correctness workaround that outlived its defect by a year

`CompletableFuture$UniCompose.tryFire` and `$UniRelay.tryFire` are composition:
they run every dependent stage. Both were **asked and refused**, three times
each, and until 2026-08-26 the report said only `reason=unrecorded`. The first
thing the new `record_compile_refusal` printed was the answer:

```
199476  compile-failed  CompletableFuture$UniCompose.tryFire(I)  reason=vm-fjp-subclass-blocklisted
119668  compile-failed  CompletableFuture$UniRelay.tryFire(I)    reason=vm-fjp-subclass-blocklisted
```

`is_fjp_subclass_blocklisted` force-interpreted every method on a class
transitively extending `java/util/concurrent/ForkJoinTask`. Its own comment
claimed the opposite of what it did:

> This is narrow enough to leave FjpSum (single-task) and CompletableFuture
> paths JIT-eligible because they don't extend `ForkJoinTask` directly in the
> hot path.

`UniCompose` and `UniRelay` extend `Completion`, which extends `ForkJoinTask`.
The whole completion machinery was force-interpreted by a workaround that
believed it was not touching it.

**And the defect it guarded was already fixed.** RFJP.1 was a deeply-recursive
`RecursiveTask<Long>.compute()` returning 0 past depth ~10. Its first diagnosis
— a regalloc clobber of a `long` local across `invokevirtual` — is what the
comment still described; the actual root cause was a `Long.valueOf` boxing
miscompile, fixed as a side effect in Session 108, commit `6f605451d`, whose own
subject line says "RFJP.1 closed as side-effect". The blocklist was never taken
back out.

### 2. `dup_x2` — lowered for pre-increment, not for post-increment

`HibfixComposeProbe2.chain` was `ineligible-by-policy` with
`reason=singlepass-codegen/dup_x2-unprovable-form(pc=15,op=0x5b)`.

The single-pass backend models the operand stack as one entry per VALUE, so a
category-2 `long`/`double` is ONE entry and the stack HEIGHT cannot distinguish
`dup_x2`'s two forms. The arm therefore admitted exactly one shape it could
prove locally — *the next opcode is a category-1 array store*, which is javac's
`++z[i]`. Everything else stayed interpreted, including javac's POST-increment
`arr[n[0]++] = v`, where the `dup_x2` is followed by `iconst_1; iadd; iastore`.

## The fixes

### The blocklist is default-OFF, with the evidence its removal needed

`CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST` is now **opt-in** (it was default-on). The
code and the lever are kept as a one-variable rollback, not because the defect
is expected back.

`probes/FjpStress.java` is new and is the evidence. It sums a `long[200000]` to
**depth 17** through all three FJP bases the blocklist named, 20 rounds each:

* `RecursiveTask<Long>` — the boxed-return shape RFJP.1 was recorded against,
  with a `long` local live across the recursive `compute()`;
* `RecursiveAction` — void, result through a shared array;
* `CountedCompleter<Long>` — the third base, a different completion protocol.

Clean at C1 and at C2 (`CRATONVM_JIT_FORCE_C2=1`): 9 interleaved arms on the
first binary and 6 more on the final one, alongside `FjpProbe` (depth 10) and
`FjpDeepSum` (depth 17), 12/12 and 3/3.

**The witness matters more than the passes.** A green run in which `compute()`
was never compiled proves nothing, so the compile state was read directly:

| `CRATONVM_DBG=jit-method-stats` on `FjpStress` | blocklist ON | OFF |
|---|---:|---:|
| total invocations | **31 457 248** | **7 014** |
| hot-but-stuck methods | 7 | **0** |
| `FjpStress$BoxedSum.compute` | `compile-failed reason=vm-fjp-subclass-blocklisted` | compiled |
| `CountedCompleter.tryComplete` / `setPendingCount` | `compile-failed` | compiled |

Interpreted invocations are counted and compiled ones largely are not, so the
4 500x drop in that counter IS the compile.

`regression-suite/src/RJdkForkJoin.java` gained a `deepRecursion()` section so
this cannot go quiet again. The existing `SumTask` splits 20 000 elements at a
threshold of 64 — depth ~9, i.e. just BELOW the depth RFJP.1 was about, so its
pass proved nothing about it. The new `DeepSumTask` uses a threshold of 2 over
65 536 elements (depth 15), runs 3 rounds so the JIT compiles `compute()`, and
asserts the depth reached as well as the sum: a mistyped threshold would
otherwise turn it into a shallow tree that still adds up.

### `dup_x2` and `dup2_x1` get the second-entry width oracle

`stack_entry_categories` — the `x64::stack_kinds` forward analysis, admitted
only when its depth and per-entry ref-ness agree with the emitter's own model
and, for the top entry, with `dup2_top_cat2`'s independent peephole — already
existed for `dup2_x2` (0x5e). It answers exactly the question `dup_x2` needs:
the width of the entry BELOW the top.

* **`dup_x2` (0x5b)** now lowers FORM-1 and FORM-2. The array-store peephole is
  KEPT as a fallback for methods whose kind analysis poisons — it is javac's
  `++z[i]`, which is BC's `Nat.inc` / `Nat.dec` DRBG block-counter helpers,
  re-running the whole compile pipeline 35 923x in one crypto-prng suite run
  bailing on this opcode.
* **`dup2_x1` (0x5d)** now lowers FORM-1 as well as FORM-2. FORM-1 duplicates
  TWO entries, so the top-width-only peephole could never prove it.
* **A latent bug went with it.** `dup_x1`, `dup2_x1` and `dup2_x2` all guard
  against `push_from_rax` silently failing to reserve a spill slot: it emits
  nothing and does not grow the model, so the following rotate reorders the
  WRONG entries and leaves the operand stack one short — silent wrong code
  rather than a bail. `dup_x2` was the one arm missing that guard. It has it now.

Five new codegen tests in `jit/src/x64/tests.rs` RUN the compiled body and check
a value a wrong-depth insert cannot produce, following the `dup2_x2` tests'
pattern: `.expect(...)` alone would pass against a shuffle that compiles and
computes nonsense.

## Measurements

All on an idle host (load 0.06), interleaved, against frozen binaries.

**Composition, `-Dprobe.threads=2 -Dprobe.chains=40000`, ABBA x6:**

| | min | median | max |
|---|---:|---:|---:|
| dev tip | 3 095 | 3 140 | 3 172 |
| both fixes | 839 | **852** | 864 |
| HotSpot | 41 | 44 | 51 |

3.7x, and the gap to HotSpot goes 70x -> 19x.

**Composition, `-Dprobe.threads=24 -Dprobe.chains=200000` (4.8 M chains):**

| | runs | median |
|---|---|---:|
| dev tip | 95 404 / 108 040 / 109 733 / 117 439 | 108 886 |
| both fixes | 26 047 / 26 118 / 29 061 / 29 250 | **27 590** |
| HotSpot | 229 / 259 | 244 |

**3.95x**, and 446x -> 113x. `wrong=0` in every run of every arm.

**Attribution** is the table at the top of this page, taken on ONE binary with
the two kill switches. Neither fix is the whole of it: disabling `dup_x2` alone
costs 2.19x, restoring the blocklist alone costs 2.88x, and reverting both
reproduces the dev-tip number (3 272 ms) together with all three original
refusal lines, verbatim.

## Gates

`cargo test` on the final tree: **cratonvm-gc 1 686+ pass (rc=0)**,
**cratonvm-jit 2 127 pass**, **cratonvm-vm 2 626 pass**, **cratonvm-types 586
pass**; regression suite **72/72 core** and **111/112 `SUITE=all`**.

Five failures across those runs, every one of them pre-existing on `origin/dev`
and verified as such rather than assumed:

* `RJdkEnumerations` — fails IDENTICALLY on a binary built from the unmodified
  dev tip, and with each of this branch's changes switched off.
* `ir_lower::tests::ir_lower_loads_incoming_arguments_past_the_entry_abi_registers`
  — `Err(TooManyArgs(9))`. `ENTRY_ABI_REGS` has 6 entries on SysV against 4 on
  Win64, so the test's `for extra in 0..=3` reaches 9 arguments while
  `CompiledMethod::try_call` handles at most 8. Platform-dependent, in files
  this branch does not touch, and byte-identical to `origin/dev`.
* `types::doc_citation_paths::no_source_file_links_into_docs_internal` — names
  three lines in another session's `docs/known-issues/netty/` page.
* `vm::runtime::resolve::guard::no_unallowlisted_metadata_table_bypass_exists`
  and `the_allowlist_has_no_dead_rows` — `find_method_recursive(` occurrence
  counts. Counted in this worktree and in `origin/dev`: 4 and 11 in both.

## What this page got right, and what it got wrong

Right: that a Java-frame profile misattributes a native call to the Java frame
that made it, so "34% in `tryPushStack`" was never evidence about `VarHandle`;
that the native-shadow seal is real but worth ~0 here and its per-SITE rewrite
should not be pursued on that evidence; and that `reason=unrecorded` on a method
invoked 97 716 times was the one diagnostic gap worth closing first.

Wrong, in its first revision: that the seal CAUSED the interpretation. It did
not, and the seal's own kill switch is what disproved it.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
javac -d <out> HibfixComposeProbe2.java
cratonvm --java-home <jdk> -Dprobe.threads=2 -Dprobe.chains=40000 -cp <out> HibfixComposeProbe2
```

The two levers exist for bisection only: `CRATONVM_JIT_NO_DUP_X2=1` is 2.19x
slower and `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1` is 2.88x slower.

```bash
javac -d <out> probes/FjpStress.java
cratonvm --java-home <jdk> -cp <out> FjpStress     # @@FJPSTRESS PASS ... max_depth=17
```

## Related

- `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
  — the primitive work this page's first revision predicted would close the
  gap. It did not, and that page says so; this is where the gap actually was.
- [`../../known-issues/perf/juc-primitives-are-9-114x-after-the-composition-compile-refusals-20260828.md`](../../known-issues/perf/juc-primitives-are-9-114x-after-the-composition-compile-refusals-20260828.md)
  — the residual: the 113x that is left, and the per-op costs under it.
- [`../../known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`](../../known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md)
  sections 5.9-5.10 — where this was found.
