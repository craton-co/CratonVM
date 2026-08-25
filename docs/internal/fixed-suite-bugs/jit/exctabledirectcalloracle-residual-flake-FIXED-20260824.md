# `ExcTableDirectCallOracle`'s residual flake was the missing arithmetic arm

**Status: FIXED — 2026-08-24.** The defect is the one
`osr-door-refused-every-exception-table-callee-FIXED-20260824.md` §5.1 names:
`route_implicit_exc_through_callee` had no arm for the pending-arithmetic flag.
That page found it from the other end — `probes/EscapeStaticProbe`, 198 000 of
200 000 caller `catch (ArithmeticException)` skipped — and its fix is what
closes this page too. **The fix is not this page's**, and neither page's work
depended on the other's; they were concurrent and they met at the same line.

What this page adds is the part its own evidence was missing: the attribution.
The rate it reported could not be reproduced on the `dev` of the day it was
retired, and understanding why is the only reason it is safe to retire.

## The defect, from this probe's side

A compiled callee that divides by zero leaves through the reason-3 deopt stub,
which calls `jit_throw_arithmetic()`. That sets a thread-local `arithmetic`
flag plus the `i64::MIN` sentinel and returns — no throwable, and **no
reconstructed frame**, so `try_resume_trapped_callee` correctly declines and
the flag is the only thing that says what happened.

Two functions consume that sentinel and route it through the CALLEE's own
exception table:

| door | reached from | had the arithmetic arm? |
|---|---|---|
| `handle_compiled_callee_deopt_sentinel` | the baked direct `CALL`'s `emit_inline_callee_deopt_check`, and the MIC/PIC cascade | **yes** |
| `route_implicit_exc_through_callee` | `jit_invoke_dispatch`'s five compiled-entry fast paths | **no** |

The second asked *"is there an implicit signal at all?"* with a hand-written
`if aioobe.is_none() && !npe`. A zero-divisor signal answered **no**, the
sentinel propagated to the compiled CALLER, and the caller's drain threw the
`ArithmeticException` through the **caller's** table. Here that caller is
`driver`, which has no `try` around `selfCatchDivZero`, so it escaped to
`main` — the reported
`ExcTableDirectCallOracle.main(ExcTableDirectCallOracle.java:156)`, printed
before any of the verification section, exactly as the page described.

The page's own hypothesis pointed at `try_run_callee_handler` and the
frame-rebuild machinery. That machinery is never reached: the classification in
front of it has already said there is nothing to route. The hypothesis was
labelled as one, which is why following it cost nothing.

## Why it was a rate, and why the rate vanished

Which door a call takes depends on whether the callee was already compiled when
its caller was. Nothing about the defect is probabilistic — only its
reachability is. Forcing the dispatch-helper door open makes it deterministic,
which is what the page's "reproduce with the intermittency pinned rather than
sampled" asked for. On the pre-fix `dev` binary (`5a760532c`), ONE binary, no
source change:

| arm | result |
|---|---|
| default | **40 / 40 OK** |
| `CRATONVM_JIT_SP_IC_DEOPT_CHECK=0` | **0 / 25** — all `/ by zero` at `main:156` |
| `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=0` | **0 / 25** — same |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | fails |
| `CRATONVM_C2_SUPERSEDE=0` | pass, door never entered |
| `CRATONVM_JIT_FORCE_C2=1` | pass, door never entered |
| `CRATONVM_JIT_IR_DIRECT_CALL=0` | pass, door never entered |

The last three rows are the useful half. The page nominated `C2_SUPERSEDE` and
`FORCE_C2` as "the two cheap arms to run first"; both are **negative**, and the
counter says why rather than leaving it as a shrug — they never reach this door.

Rows 2 and 3 also falsify a claim made elsewhere. The probe's own header, and
`static-exception-table-callee-pays-the-funnel-20260821`, both state that the
output is byte-identical with the ban restored. It was not: restoring the ban
by either spelling failed **every** run.

**The default arm had gone quiet, and that is not the same as fixed.**
`route_implicit_exc_through_callee` is entered **zero** times across 25 traced
default runs on the pre-fix `dev`, and that arm passes **140 / 140** across two
binaries. The reported 2/25 simply does not reproduce there — on the default arm
the `/ by zero` is serviced 214 705 times per run by the direct-`CALL` door
(`run_jit_callee_handler … handler_pc=9`).

So the page could not have been retired on its own arm. Rebuilt against its OWN
commit it is unambiguous:

| binary | default arm | door entered, 25 runs |
|---|---|---|
| `20fcda31e`, the page's commit, as shipped | **20 / 100 fail** | **112** |
| `5a760532c`, the `dev` of 2026-08-24 | 0 / 100 fail | **0** |

Something between the two moved this probe's calls onto the serviced door. The
defect was untouched the whole time — which is what the concurrent
`EscapeStaticProbe` work then found at 198 000 escapes, on a probe whose caller
does not depend on that routing.

## The rate measurement, on the commit the rate came from

The failure this closes is a RATE, and two binaries built minutes apart on a
shared host are not a control for one. So: `20fcda31e` + the arithmetic arm,
ONE binary, ABBA-interleaved, the DEFAULT arm — the arm the page reported —
with the arm switchable so both sides are the same executable:

| block | arithmetic arm | result |
|---|---|---|
| A1 | on | **25 / 25 OK** |
| B1 | off | 22 / 25 — **3 fail** |
| B2 | off | 23 / 25 — **2 fail** |
| A2 | on | **25 / 25 OK** |

**0 / 50 against 5 / 50.** 10 % brackets the page's 8 % and the 20 % measured on
the same commit at higher load, so the interleave is what makes the two arms
comparable — the absolute number is load-sensitive and on its own says little.

The switch was scaffolding for that measurement and is deliberately **not**
shipped. An opt-out whose OFF position is a known miscompile is a landmine, not
a lever, and `tools/flag-census/check-surface.sh` exists because this surface
grew "roughly one per fixed bug".

## Verified on the merged tree

`dev`'s arithmetic arm, this branch's de-duplication, one binary:

| arm | result |
|---|---|
| default | 25 / 25, byte-identical to HotSpot 25 |
| `CRATONVM_JIT_SP_IC_DEOPT_CHECK=0` | 25 / 25 — was 0 / 25 |
| `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=0` | 25 / 25 — was 0 / 25 |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | 25 / 25 — was failing |
| `--nojit` | 5 / 5 |

Every passing run is diffed against HotSpot's output, not merely exit-0. The
probe's three-arm contract — HotSpot, the default, and the ban restored, all
byte-identical — holds for the first time.

## What this branch changed

The arm was already added where it was missing. This is the other half: `dev`
fixed a two-copy table by adding a third arm to the second copy, and
`ImplicitSignal` / `implicit_signal_of` / `materialize_implicit_signal` /
`restash_implicit_signal` make it one copy that both doors call.

The load-bearing part is not the shared `match`. It is that *"is there an
implicit signal at all?"* is now answered by the SAME function that decides
*which one*. The drift was possible because the first door asked that question
with a hand-written companion test, so adding a flag to one and not the other
was a silent no-op. Five unit tests pin the arithmetic where it is
deterministic, including
`the_no_signal_answer_is_reserved_for_the_empty_triple` — the property a future
fourth signal would break, since landing in the `None` bucket means "propagate
to the caller" and nothing says so out loud.

## The engagement control

A green arm proves nothing if no `/ by zero` ever reaches the door.
`CRATONVM_DBG_RBC6=1` prints the classification, so it is counted rather than
assumed. One `SP_IC_DEOPT_CHECK=0` run:

```text
IMPLICIT=334445  Arithmetic=29728  Aioobe=37161  Npe=29728  None=237828
[rbc6-dbg] route_implicit_exc_through_callee IMPLICIT Arithmetic
           callee_has_handler=true
           ExcTableDirectCallOracle.selfCatchDivZero(II)Ljava/lang/String;
```

29 728 zero-divisor signals classified and routed per run, every sample naming
the method whose `catch` was being skipped.

## Gates, on the merged tree

| gate | result |
|---|---|
| `probes/ExcTableDirectCallOracle`, four arms + `--nojit` | 105 runs, all byte-identical to HotSpot 25 |
| `probes/MapModCountProbe` (doc 2, re-verified rather than trusted) | 16/16 CME, byte-identical to HotSpot; 12 back to `NONE` with `CRATONVM_NO_MAP_ITERATOR_FAILFAST=1`, `UNSEEDED=0` |
| `cargo test -p cratonvm-vm --lib` | **2626 passed, 0 failed** |
| `regression-suite/run.sh` | **71 passed, 0 failed** |
| `tools/flag-census/check-surface.sh` | green — `916 variables, 891 tokens` |
| `scripts/check-no-diag-prints.sh` | green |

## What repairing the flag guard exposed, and what is being handed over

`tools/flag-census/check-surface.sh` is **red on pristine `origin/dev`**
(verified at `a65782ee4`, byte-identical output), and in `ci.yml` it sits at
step 242 — ahead of Clippy (251) and `cargo test --workspace` (258). Neither of
those has been running. Six undeclared names (four in `INVENTORY` but missing
from the fixture; `CRATONVM_DBG_PUNNED_REF` and `CRATONVM_DBG_VIEW_COMOD` in
neither), one real bypass of the config boundary in `offload.rs`, and one false
positive from a doc comment spelling `std::env::var(` in prose. All repaired.
Same species as `ci-fmt-step-blocks-every-later-gate`.

**That moves CI's failure point rather than clearing it, and the next blocker is
measured rather than left as a surprise.** `cargo clippy --workspace
--all-targets` on the merged tree: roughly **120 warnings across at least six
crates**, plus one deny-level `clippy::never_loop` error in
`cratonvm-native-builtins` that stops the sweep before it finishes, so 120 is a
floor and not a total. The distribution:

| count | lint |
|---|---|
| 83 | unneeded unit expression (nearly all `cratonvm-native-builtins`, 94 warnings in that crate alone) |
| 13 | can be more succinctly written as a byte str |
| 4 | creates an owned instance just for comparison |
| 3 | unneeded `()` |
| ~15 | a long tail: `from_*` taking `self`, redundant `eprintln!` reference, redundant guard, manual `Option::map`, single-element `for`, consecutive `str::replace`, `filter().next_back()` |

None of it is in the crates this branch touched, and it is **not** fixed here:
83 of the 120 are in one file that several sessions edit concurrently, so a
mechanical sweep of it would be a merge conflict rather than a cleanup. It is
recorded here with a size so that whoever takes it knows it is one crate's
problem and mostly `--fix`-applicable, not an audit.

`cargo build --workspace` is unaffected — every one of these is a lint, and the
release build of the merged tree is green.
