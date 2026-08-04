# Single-pass codegen refused a handler body with an internal branch

**Status:** ✅ **FIXED 2026-08-03** (`fix/singlepass-handler-merge-20260803`).
Found 2026-08-02 while validating
[RBC.6's `getfield`/`putfield` admission](rbc6-protected-field-ops-FIXED-20260802.md);
pre-existing, not caused by that change — RBC.6 was refusing these methods
before codegen ever ran, so the hole had nothing to be seen through.

## The refusal

`probes/Rbc6FieldProbe.java`, method `getfieldRefHandlerLocal`:

```
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk25> -cp <probes-out> Rbc6FieldProbe
```

```
[cratonvm-jitc] compile-bail Rbc6FieldProbe.getfieldRefHandlerLocal(LRbc6FieldProbe$Holder;I)I
                backend_attempted=true reason=singlepass-codegen(pc=36,op=0xac)
```

Its four siblings in the same class — including the `long` and `putfield`
variants — compiled. The one structural difference was that this method's
HANDLER body contains a branch:

```
 22: ireturn                  <-- the live path ends here
 23: astore        4          // handler entry
 25: iload_2
 26: aload_3
 27: ifnonnull     34
 30: iconst_0
 31: goto          35
 34: iconst_1
 35: iadd
 36: ireturn                  <-- refused here
```

## Root cause

The single-pass DCE walk went dead at the `ireturn` at 22 and skipped forward,
reviving at the first PC its `branch_targets` map called a target. That map
answers *"does SOME instruction branch here"*, which is not the same question
as *"can control reach here"*.

The x86-64 backend has no in-method exception-handler dispatch — an implicit
exception leaves through the `i64::MIN` sentinel, `athrow` lowers to the same,
and the interpreter is the only thing that consults the exception table
(`route_jit_signal_exception` / `run_jit_callee_handler` rebuild an interpreter
frame). So a handler body is dead code in the emitted image. But the ternary
inside this one still marked pc 34 and pc 35 as branch targets, and the walk
could not tell them from a live join.

It therefore revived at 34 with **no recorded state**: nothing had emitted the
`ifnonnull` at 27, so `branch_target_stack_depth` held no entry and
`expected_depth` fell back to `0`. `iconst_1` pushed one value, and the `iadd`
at 35 popped two from a stack of one — `pop_stack` raised `failed`, the walk
carried on to the `ireturn` at 36, and the post-loop `failed` check refused the
method there.

Two things followed from the same revival, beyond the refusal:

* the revived block was published as an **OSR entry point**
  (`osr_entry_native[34]`), described by a stack model that never applied to
  it. The interpreter resumes a handler body itself, so a hot back-edge inside
  one could ask to enter exactly that block.
* when the accidental depth-0 guess happened to be *right* (a handler body
  whose merge really is stack-empty — the common `catch` shape), the method
  compiled and emitted a whole dead block for nothing.

## The fix

`jit/src/x64/licm.rs` — new `compute_reachable_pcs`, a worklist over
fall-through plus explicit branch/switch edges, built on the `branch_targets_at`
/ `opcode_falls_through` helpers that were already there (and, until now,
unused). Exception-table handler entries are deliberately **not** roots.
`compile_bytecode` revives on that map instead of on `branch_targets`.

Nothing live is lost by construction: the first PC of every reachable run is
either PC 0 or a branch target — exactly where a revival happens — and the
fall-through successor of a reachable instruction is reachable. The analysis
returns `None` on opaque control flow (`jsr`/`ret`/`jsr_w`) or a malformed
encoding, and the walk keeps its historical behaviour there rather than guess.

**The optimizing tier had already settled this same question, for the same
reason.** `ir::IrBuilder::build` skips every PC outside
`ir::normally_reachable_pcs`; walking handler bodies there produced orphan
nodes referencing `NO_NODE`, and was "the whole reason the optimizing tier
refused every method with an exception table". Two tiers, one contract — the
single-pass backend had simply never caught up.

### Two residuals fixed with it

**1. A `failed`-flag refusal now names itself.** `failed` is a flag checked only
after the whole dispatch loop, so `reason=singlepass-codegen(pc=36,op=0xac)`
named the last opcode the emitter touched — an arm that has no `return false`
of its own — for an underflow one instruction earlier. All sixteen raising
sites now go through `Compiler::fail(site)`, which records the first site with
the pc/op live at that moment, and `compile_with_param_slots` publishes it:

```
reason=singlepass-codegen/operand-stack-underflow-pop(pc=35,op=0x60)
```

The site names are `singlepass-codegen/<what>`, so the old string is still the
grep prefix. `emit_prologue`'s bail, which recorded nothing at all before, now
reports `singlepass-prologue` or its own site. Direct `return false` arms were
already accurately attributed (`dbg_last_pc` IS the refusing opcode there) and
are unchanged.

**2. Switch arms record their branch-target state.** Neither the `tableswitch`
nor the `lookupswitch` arm called `record_branch_target_depth`, so every switch
arm revived from dead code got depth 0 *and* an all-`false` oop-mark vector —
a guess in the depth, and precisely the unsoundness
`branch_target_stack_oop_marks` documents in the marks (a live reference
mis-marked as a plain word defeats the deopt snapshot's operand-stack
decoding). Found because the reachability fix let the depth-0 fallback become
a hard refusal, which the twelve `s34_*switch*` backend tests failed on
immediately. With the arms recording, that fallback is gone: a revived merge
with no recorded depth is now
`singlepass-codegen/revived-merge-depth-unrecorded` rather than a silent empty
stack.

## Evidence

| | base (`origin/dev`) | fixed |
| --- | --- | --- |
| `Rbc6FieldProbe.getfieldRefHandlerLocal` | `compile-bail … singlepass-codegen(pc=36,op=0xac)` | `full-compile … len=1737` (C1 **and** C2) |
| `Rbc6FieldProbe` output vs HotSpot | matches | matches (incl. `getfieldRef(null,5)=60`, the throwing path) |
| `cargo test -p cratonvm-jit` | 1815 pass | 1815 pass + 7 new |
| `regression-suite/run.sh` | — | 22 passed, 0 failed |
| `bench/CratonBench` all 7 checksums | `5000000003999999995 / 701408733 / 9592 / 173943680 / 1549999915000000 / 5000050000 / 68332206` | identical |
| Spring Boot `BinderTests` / `JettyServletWebServerFactoryTests` / `OAuth2ResourceServerAutoConfigurationTests` | 32+113+52 tests PASS | PASS |

Across those three Spring classes the two binaries compiled 336 / 2788 / 3777
(base) vs 336 / 2785 / 3763 (fixed) methods — the small drift is background-
compile tiering nondeterminism, not lost compiles: the bail-reason histograms
and, in particular, the exact SET of methods refused with
`branch-target-not-an-instruction-boundary` are identical on both arms (5
methods, same names). That was the one way this change could plausibly have
cost a compile — a target that used to be revived, and emitted, now being
skipped — and it does not happen, because the branch naming such a target is
itself in unreachable code and is never emitted either.

`singlepass-codegen/revived-merge-depth-unrecorded` — the hard refusal that
replaced the depth-0 guess — fired **zero** times across ~6,900 real compiled
methods.

One `OAuth2ResourceServerAutoConfigurationTests` run failed on the fixed arm
with `SocketTimeoutException: Read timed out` against its own localhost mock
server, on a host at load average 20–36. Three re-runs on the same binary:
52/52, 52/52, 52/52.

The two backend fixtures are real guards, not decoration: built on a pristine
`origin/dev` they both FAIL —

```
test x64::tests::a_dead_region_between_two_live_ones_is_skipped_whole ... FAILED
test x64::tests::a_dead_region_with_an_internal_branch_does_not_refuse_the_method ... FAILED
assertion failed: compile_probe_method(&code, 2, 2).is_some()
```

Fixtures: `x64::tests::a_dead_region_with_an_internal_branch_does_not_refuse_the_method`,
`a_dead_region_between_two_live_ones_is_skipped_whole`,
`a_flag_refusal_names_the_site_that_raised_it`, and four
`compute_reachable_pcs` unit tests in `x64::licm::loop_xform_tests`.

## What the reason string means now

`reason=singlepass-codegen(pc=N,op=0xNN)` with no `/site` suffix is now only a
direct `return false` arm, where the pc/op ARE the refusing opcode. Anything of
the form `singlepass-codegen/<site>(pc=N,op=0xNN)` came through the `failed`
flag, and the pc/op are the instruction that was being emitted when the flag
went up — not necessarily the one the walk stopped at.
