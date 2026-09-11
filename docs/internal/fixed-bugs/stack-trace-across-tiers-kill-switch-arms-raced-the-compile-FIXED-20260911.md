# `stack_trace_across_tiers`: the kill-switch arms raced a compile the file said was synchronous — FIXED

> **This is the SECOND of two independent fixes to the same target on the same
> day.** The other one is
> [`stack-trace-across-tiers-fails-deterministically-on-dev-FIXED-20260911.md`](stack-trace-across-tiers-fails-deterministically-on-dev-FIXED-20260911.md),
> which retires the filed page and fixes the DETERMINISTIC red: an inlining
> artifact the probe fixture had stopped producing. This page is about a
> FLAKE the fixture fix does not touch — a 1-in-10 failure whose cause is that
> the arms assumed the compiled-frame set was stable run to run. Both landed;
> neither supersedes the other.

| | |
|---|---|
| **Status** | FIXED 2026-09-11 on `claude/cargo-test-workspace-20260911`. Filed 2026-09-09. |
| **Gate** | `tools/e2e-ratchet.txt` → CI step "E2E prerequisites are real (`CRATONVM_REQUIRE_E2E`)", `ubuntu-latest`. |
| **Fix** | `vm/tests/stack_trace_across_tiers.rs` only. No VM, JIT or probe change. |
| **Evidence** | 24 consecutive `CRATONVM_REQUIRE_E2E=1` runs pass — 18 on the debug binary, 6 on the release binary, host load 40–130. |

## What the original page got right, and the one thing it got wrong

It was right that this is deterministic rather than a timing flake: seven
failures in seven attempts, all at ~7.2 s. It was right that the 2026-09-09
reflection-caller change was not the cause, and it said so with a measurement
rather than an argument.

What it got wrong was the *direction*. It pointed at `jit/src/x64/inlining.rs`
and the two splice commits, and suggested building at `c798aae82^`. Those
commits are indeed why the failure appeared — but they did not break anything.
They changed an **inlining decision**, and the test was asserting that decision
while claiming to assert something else.

## The first half: the assertion pinned a budget, not the map

`CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` is supposed to take the inlined callees out
of a warmed-up trace. The arm asserted that `mid` and `outer` DISAPPEAR from
`after_main_osr`.

`leaf` reads the static `table`. When `ir-splice-getstatic` went default-ON on
2026-09-09, admitting `getstatic` for splicing made every body on this chain
bigger — `outer`'s optimizing body went 493 → 731 bytes — and the inline budget
then refused a splice that used to fit. Visible in `CRATONVM_DBG_SWCHAIN=1`:

```text
before: one JIT entry, activations=3 [leaf | probe | main]
        -- `mid` and `outer` are INLINE LEVELS, so the map supplies them
after:  two JIT entries, activations=1 [main] and 1 [leaf]
        -- `mid` and `outer` are ordinary INTERPRETER frames
```

Both traces are correct and both match the interpreter oracle the same file
asserts. There was simply no inlined callee left at that throw for the switch to
take away, so `leaf:-1` — the innermost frame's line, which the same switch also
gates — was the only difference, and the frame count did not move.

An intermediate version of the test had already noticed this and turned the
strong assertion into a "coverage gap" report. That is honest, and under
`CRATONVM_REQUIRE_E2E` it is still a failure: five runs in five. The step stayed
red for a *reported gap* rather than for a wrong assertion, which is better but
not green.

**The fix is to hold the inlining decision fixed instead of hoping for it.** The
arm now runs `CRATONVM_JIT_IR_SPLICE_GETSTATIC=0` on BOTH sides of the pair —
the lever whose landing removed the coverage, used to restore it, rather than an
edit to the checked-in probe. The control prints the default arm's five frames
with the default arm's lines; the arm prints three. Deterministically.

The assertion was also narrowed to what the map actually owes, because *which*
callees the chain carries is still not this test's decision:

* a callee the map supplied is gone when the map is switched off;
* only a callee it *could* have supplied (`leaf`, `mid`, `outer`) may go —
  `probe` carries an exception table and the planner refuses to inline it
  (`inline-resolve REFUSED … callee-exception-table`), and `main` is the OSR
  root;
* nothing the map did not supply moves.

Under the restored configuration the run drops `mid` and keeps `outer` — exactly
the case the old `mid` AND `outer` form would have called a regression.

## The second half: the file's own premise about background compilation

The remaining arms were intermittent rather than deterministic — about one run
in ten — and that has a single cause. The file's header said:

> background compilation is off by default (`CRATONVM_BG_COMPILE` is opt-in), so
> the compile happens on the mutator at the threshold rather than racing the end
> of the loop from a worker thread

**That has been false since wire-tiered-manager Step 7.**
`vm/src/runtime/env_cache.rs::bg_compile` returns `true` when the variable is
unset — the flag is an opt-OUT, `CRATONVM_BG_COMPILE=0`, which its own comment
offers to "suites that still need the historical inline tier-up".

So the compile does race the end of the loop. Measured on one debug binary with
nothing else changing:

* `CRATONVM_DBG_JITC=1` prints the same compile events every run in a different
  ORDER. When `mid`'s optimizing body supersedes its baseline one
  (`c2-supersede published … epoch_bumped=true`) relative to the third throw
  decides whether `probe`'s bound direct call to `outer` is still bound when
  that throw happens.
* `after_main_osr` is therefore produced sometimes by two JIT entries (`main`'s
  OSR artifact and a compiled `leaf`) and sometimes by one, with the whole chain
  interpreted — and on a host at load 100, twice in ten runs, by NEITHER: the
  OSR body published after the loop it was for had ended.
* The default arm cannot see any of this, because every one of its lines is
  correct either way. That is the point of the fix this file guards, and it is
  why the variance went unnoticed until the reverted arms were asked to be
  deterministic.
* `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` accordingly printed three different
  hot rows across sixteen runs — `leaf:-1 … main:62` fourteen times,
  `leaf:-1 … main:66` once, `leaf:25 … main:62` once. All three are the switch
  working. The assertion (*any frame reports a line ≤ 0*) demanded the shape of
  one of them.

Tier-up itself was never the risk the old text worried about: 400 000 back-edges
against a threshold of 10 000 is a 40x margin no host load moves. It is the
PUBLICATION that raced.

## What the file looks like now

One control, three arms, each one variable from it.

```text
control  CRATONVM_BG_COMPILE=0 CRATONVM_DBG_JIT_METHOD_STATS=1 CRATONVM_JIT_IR_SPLICE_GETSTATIC=0
arm 1    control + CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1
arm 2    control + CRATONVM_JIT_NO_INLINE_FRAME_MAP=1
arm 3    control + CRATONVM_JIT_NO_OSR_PC_REFRESH=1
```

Every row, and every census, is bit-identical run to run. The control is also
asserted to print the same hot row as the DEFAULT arm, which keeps the shipping
configuration measured: who compiled a frame, and whether a callee was spliced,
must not move a line.

Each arm establishes that the feature ENGAGED in its own run before asserting
that switching it off reverted anything, and each pins the revert where it is
DECIDED rather than at the shape it happens to take in a trace:

* **arm 1** on the refusal census `CRATONVM_DBG_JIT_METHOD_STATS=1` prints
  (`jit::compiled_frame_line_counts`). `switched-off > 0` says the kill switch
  was taken, at the site that decides it; `answered == 0` says nothing resolved
  anyway. Then the row must differ from the control, and every cell that moved
  must be attributable: a frame whose bci is gone reports `-1`, `main` falls
  back to the back-edge, and a positive line on anything else is the reverted
  path inventing a line rather than refusing to supply one.
* **arm 2** as described above.
* **arm 3** on `main` having been a COMPILED frame at a throw, read from
  `CRATONVM_DBG_SWCHAIN=1`'s `boundary=StackTraceAfterOsr.main`. That is
  strictly stronger than the OSR-compile line in the log, which is true even in
  a run where the artifact published after the loop ended — and that reading is
  exactly what an unmoved `main` line would otherwise have meant.

Two notes on the diagnostics, since they are now part of the contract:

* `CRATONVM_DBG_SWCHAIN=1` is safe on every arm: `dbg_swchain_enabled` is read
  inside the stack walk, not on a JIT hot path.
* `CRATONVM_DBG_JIT_METHOD_STATS=1` is NOT. It also arms
  `getfield_census_counting_enabled` (`vm/src/jit/helpers.rs`) and
  `code_ptr_memo_census_enabled` (`jit/src/lib.rs`), which count on hot paths —
  enough to move a compile race. It is set on the control and its arms only,
  where `CRATONVM_BG_COMPILE=0` has already made tier-up counter-driven, so the
  extra counting cannot change what gets compiled. The default arm keeps the
  shipping configuration exactly.

## What did NOT change

No VM, JIT or probe code. `probes/StackTraceAfterOsr.java` is untouched, so the
line numbers quoted by
`docs/internal/fixed-bugs/jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`
still hold. The default arm still runs the shipping configuration and is still
compared frame-for-frame and line-for-line against `CRATONVM_DISABLE_JIT=1` on
the same binary and the same `.class` file, and a run in which `main` never
tiered up at all is still its own clearly-worded ENVIRONMENT outcome.

## The general lesson, for the next e2e row

On a loaded runner, "the JIT did not get there in time" and "the feature broke"
are indistinguishable from the assertion's side. A `CRATONVM_REQUIRE_E2E` target
has to be able to tell them apart, and the way to do that is to measure
engagement in the same child process — not to infer it from a log line about a
different moment, and not to assume a compile that has been asynchronous for
two months is still synchronous.

## Reproducing

```bash
JAVA_HOME=<jdk25> PATH=<jdk25>/bin:$PATH \
CRATONVM_BIN=target/debug/cratonvm CRATONVM_REQUIRE_E2E=1 \
cargo test -p cratonvm-vm --test stack_trace_across_tiers
```

Real time ~10–17 s (six child VMs). A run that reports `ok` in 0.05 s measured
nothing: `CRATONVM_REQUIRE_E2E` guards the BINARY only, and a missing `javac`
still turns the target into a bare return.
