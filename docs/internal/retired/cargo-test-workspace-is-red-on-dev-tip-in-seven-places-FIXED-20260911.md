# `cargo test --workspace` is red on `dev` tip in seven places — RETIRED

| | |
|---|---|
| **Status** | RETIRED 2026-09-11 on `claude/cargo-test-workspace-20260911`. Filed 2026-09-10, re-taken 2026-09-11 at `237dc6fb1`. |
| **Gate** | `cargo test --workspace` — `ci.yml` line 252. |
| **Scored at** | `origin/dev` = `8f1666414`, Linux, 8 cores, debug profile, JDK 25 on `PATH`. |

## What happened to each of the seven

| # | failing test | outcome |
|---|---|---|
| 1 | `the_drift_baseline_has_no_stale_rows` | **fixed here** — the scanner now reads `register_with_kind(`; see below |
| 2 | `a_warmed_up_stack_trace_keeps_every_frame_and_every_line` | **fixed here** — its own page: [`stack-trace-across-tiers-fails-deterministically-on-dev-FIXED-20260911.md`](stack-trace-across-tiers-fails-deterministically-on-dev-FIXED-20260911.md) |
| 3 | `every_cratonvm_literal_is_declared_or_explicitly_exempt` | already green at `8f1666414` — the c2 JIT lane declared its three flags |
| 4 | `flag_inventory_surface_counts_are_current` | already green — inventory regenerated |
| 5 | `the_flag_inventory_table_matches_the_declared_surface` | already green |
| 6 | `the_flag_token_reference_matches_the_inventory` | already green |
| 7 | `synthetic_stub_count_does_not_regress` | already green — baseline re-frozen at 2 339 (management) |

Rows 3–7 were the ones the page said "belong to the lanes that moved them", and
the lanes moved them: between `237dc6fb1` and `8f1666414` all five went green on
their own. This page's own advice — that regenerating 4–6 while 3 was open would
just move the numbers again — is why they were left, and it was right.

The doctest fix the original page recorded (`REFLECTION_INTERNAL_EXCEPTIONS`
fenced as `text`) is also on `dev`.

## Two more that this page did not know about

Scored the same way, on pristine `origin/dev`, and both red:

* **`the_drift_gate_agrees_about_family_drift_exposure`**
  (`registrar_reachability.rs`) — the sibling of row 1. The two gates ratchet
  each other, so it is the same fix and is described with it below.
* **`io_tests::files_validated_path_rejects_dotdot_segment`**
  (`native-io`, lib tests) — intermittent, 1 run in 12, and a genuine defect in
  test isolation rather than a flake to be waited out. Described below.

## 1. The drift scanner could not see `register_with_kind(`

The original page diagnosed this exactly and then declined to fix it, listing
three options in preference order and taking none. Option 1 — *teach the scanner
`register_with_kind(`* — is what is done here, and the reason it was deferred
("on the order of 750 such sites, so the count moves a long way at once") turned
out to be the argument FOR doing it: the sites are 747, and the pairs they
restore are 54.

`NativeMethodRegistry` has exactly two registration entry points
(`native-api/src/registry.rs`): `register`, and `register_with_kind`, which sets
the ambient kind, calls `register`, and restores it. Its first three arguments
are the same triple. The site matcher required a `(` immediately after
`register`, so every `register_with_kind(` call was skipped — and *not counted*
either, so it never showed up in the blind region `MAX_BLIND_SITES` bounds.

The consequence is this file's own failure mode run backwards: **a kind
adjudication deleted a drift row and read as good news.** `0b2791ac7` moved the
shipping half of `java/lang/Class.getModule()Ljava/lang/Module;` from
`register(` to `register_with_kind(`. Both registrations still exist —
`register_p59_module` and `register_essential_natives_with_shims`, two
canonical-Module caches for one identity invariant — but the census could see
only one, so the pair reported as FIXED. It was not fixed; it was unwatched.

### What the re-take moved

```diff
-const BASELINE_TOTAL_DRIFT: usize = 1224;
-const BASELINE_TOTAL_PAIRS: usize = 1357;
+const BASELINE_TOTAL_DRIFT: usize = 1275;
+const BASELINE_TOTAL_PAIRS: usize = 1411;
```

+51 triples, +54 pairs, **nothing removed**. Every one is a synthetic-only body
against a shipping twin that is now spelled `register_with_kind(`; not one is a
new registration. By pass:

| synthetic-only pass | pairs |
|---|---|
| `register_synthetic_overrides` | 27 |
| `register_core_stdlib_extras` | 16 |
| `register_classloader_define_class` | 3 |
| `register_p69_misc` | 3 |
| `register_object_stream_class` | 2 |
| `register_enterprise_final_natives` | 1 |
| `register_java_lang_extras_natives` | 1 |
| `register_unsafe_define_class` | 1 |

The 27 are the oldest twins in the tree and the ones a mode split hurts most —
`Object.{clone,getClass,hashCode,notify,notifyAll}`,
`System.{arraycopy,currentTimeMillis,nanoTime,identityHashCode}`,
`String.{intern,matches,replaceAll,replaceFirst}`,
`Thread.{currentThread,start0}`, `Throwable.fillInStackTrace`. Spot-checked at
the source: `Class.desiredAssertionStatus0` is `native_assertion_status` in
`register_synthetic_overrides` (`lib.rs:23364`) and a different, inline closure
in `register_essential_natives_with_shims` (`lib.rs:13228`). Two bodies, one
triple, decided by which mode you booted. That is the species this gate exists
for.

`registrar_reachability.rs`'s `FAMILY_DRIFT_EXPOSURE` is derived from that table
and was re-taken with it: six families, +27 (a family's row is the size of the
UNION over its synthetic-only subtree, so the smaller number is deduplication,
not a second disagreement). Each constant's doc carries the full accounting —
"a re-take with no explanation is how a ratchet becomes a rubber stamp" is that
file's rule and it is kept.

The scanner change is eight lines. The rest of the commit is the re-take.

## 2. The warmed-up stack trace

Its own page, because the diagnosis is long and belongs with the e2e ratchet:
[`stack-trace-across-tiers-fails-deterministically-on-dev-FIXED-20260911.md`](stack-trace-across-tiers-fails-deterministically-on-dev-FIXED-20260911.md).

Two sentences of it belong here, because this page framed the question and got
it right: *"what got compiled and inlined is not the same on every run, and the
assertions read as if it were. Establishing engagement before asserting the
revert is the first move here, not chasing the emitter."* That is what was done.
The reason the set moves is that `CRATONVM_BG_COMPILE` has been **default-ON**
since wire-tiered-manager Step 7, so the OSR body races the end of the loop it
was compiled for — while the test's own header said the opposite.

Test-only change. No VM, JIT or probe code.

## 3. `files_validated_path_rejects_dotdot_segment`: a security assertion that was not always made

Not on the original list, and worth more than a line. It failed once in twelve
runs of `cargo test -p cratonvm-native-io --lib`, and passed 1/1 run alone:

```text
thread 'io_tests::files_validated_path_rejects_dotdot_segment' panicked at native-io/src/lib.rs:
Files path with `..` segment accepted: Ok("../../etc/passwd")
```

`PATH_VALIDATION_ENABLED` is a process-global atomic. `native-io`'s test module
already has a shared mutex for the OTHER path-policy global
(`test_support::confine_test_lock`, documented for `set_path_confine_to_cwd`),
but three tests turned VALIDATION off without taking it:

* `path_validation_disabled_allows_dotdot`
* `path_validation_disabled_still_rejects_null_byte`
* `files_validated_path_rejects_null_byte_even_when_disabled`

While any of those holds the flag at `false` the `..`-traversal guard is off
**for every thread**, so a parallel test asserting that a traversal path is
REJECTED reads it accepted instead. Three more read a verdict that depends on
the flag being on without taking the guard either.

The failure mode is the wrong way round from an ordinary flake: the loud version
is a red build, and the quiet version is a *green* one in which a security
assertion was made against a validator that was switched off. All six tests now
take the shared guard, and its comment says the lock covers both globals and
why. 15 consecutive runs of the whole `native-io` lib suite, 0 failures.

## Four that are the host, not the tree — half true

The original page's rule holds and is worth keeping: **read the signal before
reading the failure list**, and read the LOAD before it too. Its own
`--no-fail-fast` run at load 214 with all 15 GB of swap consumed reported ten
failures where a quieter run reported seven, and had `rustc` OOM-killed
(`signal: 9`) in the middle.

Confirmed on this branch, at load 30-90, each re-run alone after failing inside
a workspace run taken at load 200-500:

| target | in the loaded run | alone |
|---|---|---|
| `a_lambda_door_trap_resumes_and_runs_its_side_effect_once` | exception | ok, 1.1 s |
| `socket_input_stream_timeout_is_typed_and_never_eof` | assertion | ok, 26.9 s |
| `arraylist_iterator_terminates_and_remove_still_works` | 15 s timeout | ok, 10.6 s |
| `t1_gc_pause_budget_100k_objects_under_200ms` | budget | ok (58 passed) |
| `real_jdk_thread_pool_prestart_keeps_fresh_threads_startable` | 120 s timeout | **still fails**: the probe prints `PRESTART_OK iterations=2000 workers=2000` after 147 s of wall for 28 s of user time |

So four of the five are the host. The fifth is the same profile mistake the
section below is about — the probe is correct and the clock is not — and it is
left alone here because 120 s is not obviously wrong on a two-core CI runner and
raising a timeout is the change most likely to hide the next real hang.

What is NOT true is the original page's claim that the four "all pass on both
`origin/dev` and the branch once load falls to ~35". `vthread_gc_stress_completes`
does not, and the reason is in the next section.

## What the fail-fast gate had been hiding under `vm/`

Row 1 is a `native-builtins` test and `cargo test --workspace` stops at the
first failing binary, so **every `vm` test target had been unmeasured in CI for
as long as `registrar_drift` was red**. Turning row 1 green is what made them
run, and two of them do not survive the debug profile the gate uses.

Both are the same mistake, and it is not a flake: **a wall-clock cap sized
against a RELEASE measurement, in a gate that runs DEBUG.** Measured on this
8-core Linux host at load 19-30, same fixture, same arguments, one binary of
each profile:

| probe | release | debug | cap |
|---|---|---|---|
| `LoaderUnloadProbe` (182 `System.gc()`) | 15.7 s | did not finish in 400 s | 600 s |
| `VthreadGcStress` | 16-23 s | over 300 s | 120 s |
| `VthreadProbe` | 6.3 s | 58 s | 60 s |

One ZGC cycle on this fixture's heap costs ~86 ms built with `-O` and ~2.5 s
built without, so the class-unload probe's fixed 182-collection schedule needs
more than 400 s in debug before it can even look at the weak references it is
about to assert on. Nothing was wrong with the collector: both profiles print
`liveLoaders=0 liveClasses=0 ok=true` when they are allowed to finish.

### `class_loader_unload_regression` — fixed here

The probe now POLLS instead of counting: the same caps, the same assertions, but
it stops collecting once the weak references have cleared and the MXBean's
unloaded count has caught up. It needs **8 attempts rather than 182**, and it
prints `gcAttempts=` so that reclamation getting harder is visible as a number
rather than absorbed by a fixed schedule. The whole target: **2.9 s release,
24-46 s debug, 3 runs of 3 green.** The 600 s cap is left alone — it is a
hang-catcher, and it now has ~25x margin instead of none.

### `vthread_probe_regression` — measured, not fixed here

`VthreadGcStress` at 16-23 s release sits exactly in the 7-22 s band its cap's
own comment records, so nothing has regressed; the debug profile is simply 13x
slower than the cap allows and `VthreadProbe` at 58 s against 60 s has no margin
left at all. That is a real defect in the target and it is **not** this page's:
`tools/e2e-ratchet.txt` already excludes `vthread_probe_regression` as FLAKY
(3 of 4 runs failed on 2026-08-30), so the row that would have to change is
owned by that file's lane, and the choice between a debug-aware cap, a smaller
probe and a profile gate is a decision rather than a repair. Filed with the
numbers above at
`docs/known-issues/jit/vthread-and-prestart-probe-caps-are-sized-for-release-and-the-gate-runs-debug-20260911.md`,
retired the same day as
`docs/internal/retired/vthread-and-prestart-probe-caps-FIXED-20260911.md` -- it took a
workload decision and a progress-reporting probe, not a bigger cap.

Until that lands, `vthread_probe_regression` is the one target in
`cargo test --workspace` whose red is expected on a debug build.

## An environment trap this gate has, which cost an afternoon

`cargo test --workspace` with no `javac` on `PATH` fails **110 tests** in
`cratonvm-jit-cuda`, all of them with

```text
failed to read fixture …/out/gpu-fixtures/RejectInvoke.class: No such file or directory
```

`jit-cuda/build.rs` compiles its Java fixtures at build time if `javac` is
available and prints a `cargo:warning=` if it is not — deliberately, so that
stale checked-in classes cannot mask a missing compiler. CI sets up Temurin 25
before `cargo build`, so it never sees this; a developer box without a JDK on
`PATH` sees a hundred and ten red tests that say nothing about the tree.

The same omission silently *passes* the e2e targets: `CRATONVM_REQUIRE_E2E`
guards the BINARY only, so a missing `javac` turns
`a_warmed_up_stack_trace_keeps_every_frame_and_every_line` into a bare return
that reports `ok` in 0.05 s — and turns
`custom_loader_metadata_is_reclaimed_with_jit`, the target §5 is about, into one
that reports `ok` in 0.05 s as well. **Both** failure modes come from the same
missing `PATH` entry and they point in opposite directions. Note also that the
fixture build is not re-run when only `PATH` changes: `touch jit-cuda/build.rs`
after putting a JDK on `PATH`, or the 110 stay red.

So the honest invocation of this gate is:

```bash
export JAVA_HOME=<jdk25>; export PATH=$JAVA_HOME/bin:$PATH
touch jit-cuda/build.rs            # only needed if it was built without javac
cargo test --workspace --no-fail-fast
```

and `--no-fail-fast` still matters for the reason this page was opened: the
plain form stops at the first failing binary and everything after it is
unmeasured — which is how §5's two targets went unrun for weeks.

## Where the gate stands

`cargo test --workspace --no-fail-fast` on this branch, 347 suites,
**18 353 passed**. The reds that remain are the two targets §5 hands to their
own page, and they are red on pristine `origin/dev` for the same reason.

* the scanner change and both drift re-takes, with the accounting in the
  constants' own docs;
* a `stack_trace_across_tiers` whose kill-switch arms are deterministic and
  measure engagement before asserting a revert;
* six `native-io` tests that no longer race a process-global security flag;
* a class-unload probe that costs 8 collections instead of 182;
* `tools/e2e-ratchet.txt` updated: the note that said its step is red until
  `stack_trace_across_tiers` is fixed now says how it was fixed and what to
  check before adding the next `REQUIRE_E2E` row.

Nothing from the original seven is left open.
