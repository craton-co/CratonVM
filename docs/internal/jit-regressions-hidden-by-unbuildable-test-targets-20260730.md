# JIT regressions that landed behind four unbuildable test targets

| | |
|---|---|
| **Status** | RESOLVED 2026-07-30. Every regression below is closed on `dev`; the compile breaks that hid them are fixed. Kept for the lesson, not the bug. |
| **Category** | VM-CORRECTNESS (JIT IR lowering, inline caches, direct calls, FP remainder) |
| **Found** | 2026-07-30, while restoring `cargo test --workspace` on `fix/deep-audit-retire-20260730`. |
| **Window** | Between `c05f85967` (2026-07-27) and `origin/dev` `9b86a9ac1` (2026-07-30). |

## Resolution

Merging `origin/dev` at `cd451facc` closes all of it. After the merge:

```
cargo test -p cratonvm-jit --test ir_vs_singlepass   89 passed, 0 failed
cargo test -p cratonvm-jit --lib                   1051 passed, 0 failed
```

including the `invokeinterface` access violation. The fix was
`1f040498c fix(jit): revert the unsound getfield/putfield RBC.6 widening`,
with `5f67725ad` (IR-pipeline tests state their moving-young dependency) and
`abf40220a` (de-flake the tests that observe process-global JIT state)
alongside it. Concurrent sessions also fixed the four compile breaks
independently — `123611c54`, `4d8a39a39`, `4972b3812`.

So the diagnosis below was right about the window and wrong about nothing that
matters, but it was overtaken within hours. **The regressions were never the
point.** What is worth keeping is how they stayed invisible, and that is
unchanged: four independent compile breaks, and a CI job that fails at step 1
before it builds anything. Read the rest as a case study.

## What happened

`cargo test --workspace` has not built on `dev` for at least two days, in four
independent places:

1. Ten `jit/tests/*.rs` helper literals were never updated when `set_throw_bci`
   and `service_callee_deopt` were appended to `JitRuntimeHelpers`
   (`66548471f`, 2026-07-28 and `31a80bfb8`).
2. `jit-api/src/lib.rs`'s own unit tests had the same gap, plus a golden
   ABI-offset table still describing 56 fields when there are 58.
3. `classloading/tests/wp2_10_nest_host.rs` was missing
   `Class::record_object_methods`.
4. `native-builtins-crypto`'s lib tests import `cratonvm_native_api` without it
   being a dev-dependency.

Any one of these fails the whole `cargo test --workspace` invocation at compile
time, which is why nobody saw the runtime failures underneath. All four are
fixed on this branch.

### And CI never got that far anyway

`cargo fmt --all --check` is step 1 of `ci.yml`'s `build-and-test` job, with no
`continue-on-error`. On `origin/dev` it reports **1,050 diffs across 1,008
files**. The job therefore fails before it builds anything, which is the honest
explanation for how four compile breaks and ~48 test failures accumulated
unnoticed: nobody could have been reading a signal that was already red.

This is deliberately *not* fixed here. `cargo fmt --all` would rewrite a
thousand files in one commit, destroying `git blame` across the whole tree for a
cosmetic gain, and this repository has been burned by exactly that before. The
two real options are to reformat crate by crate over several PRs, or to drop the
gate and stop claiming it. Either is a decision for the maintainers; pretending
the gate works is not.

## Measured comparison

`cargo test --workspace --no-fail-fast`, same host:

| | failing tests |
|---|--:|
| `origin/dev` + compile fixes only, before `dev` moved | 48 |
| `fix/deep-audit-retire-20260730`, after merging `origin/dev` at `0477c8851` | 11 |

**A "39" previously stood in that second row and was wrong.** It was measured
against a stale `target/release/cratonvm.exe` built before the merge:
`synthetic_diff.rs::cratonvm_binary()` prefers a release binary over a debug one,
so every subprocess test in that run validated pre-merge code. Deleting the
stale binary and rebuilding changed the result.

The lesson is worth more than the number. **Rebuilding the debug tree does not
invalidate a stale release binary, and the helper silently prefers the stale
one.** If a suite result disagrees with a change you just made, check the
binary's timestamp before you re-check your reasoning.

The two sides are also not directly comparable any more: the 48 predates several
days of `dev` movement, including the RBC.6 revert that closed the regressions
listed below. Read it as an order of magnitude, not a subtraction.

## The regressions

With the targets buildable again, `jit/tests/ir_vs_singlepass.rs` reports 8
failures and one process-level `STATUS_ACCESS_VIOLATION`, and `cratonvm-jit
--lib` reports a further cluster.

**These are regressions, not stale tests.** At `c05f85967` — the last commit
before the compile break — the same file passes **89/89**, verified by checking
out that commit into a separate worktree and running it with the same
test-source fix applied.

`jit/tests/ir_vs_singlepass.rs`:

| test | symptom |
|---|---|
| `ir_fp_frem_integer_operands` | `IR (FP) failed to compile frem/drem` — the IR pipeline now declines a shape it used to accept |
| `ir_fp_frem_fractional` | same |
| `ir_fp_drem_integer_operands` | same |
| `ir_fp_drem_fractional` | same |
| `ir_direct_call_static_with_context_executes_correctly` | direct-call path |
| `ir_direct_call_static_without_context_executes_correctly` | direct-call path |
| `ir_direct_call_exception_sentinel_bails` | direct-call deopt sentinel |
| `crossmethod_long_return_uses_ir` | cross-method long return no longer routed through IR |
| `ir_vs_singlepass_invokeinterface_instance_call` | **crashes the process** (`0xc0000005`) |

`cratonvm-jit --lib`, same window:

- `ir_lower::tests::ic_site_emits_mic_pic_cascade_not_blind_dispatch` —
  *"TEST RAX,RAX receiver null check must be emitted"*
- `ir_lower::tests::ic_guards_use_published_slot_offsets`
- `ir_lower::tests::ic_cascade_rejects_array_receivers_before_the_class_id_guard`
- `runtime_lowering::tests::hashed_vtable_stub_rejects_array_receivers`
- `tests::ir_call_wiring_routes_through_ir_only_with_flag` and the `ir_fp_` /
  `ir_long_` / `ir_special_call_` / `ir_virtual_call_` siblings
- `tests::step3_optimize_toggle_routes_c1_singlepass_and_c2_ir`
- `tests::protected_field_access_keeps_unsafe_handler_interpreted`
- `tests::recursive_compile_cycle_routes_parent_direct_call_through_dispatch`

The missing receiver null check is the one to look at first: an inline-cache
site that no longer emits `TEST RAX,RAX` will fault on a null receiver instead
of throwing `NullPointerException`, and that is consistent with the
`invokeinterface` access violation in the sibling suite.

## Prime suspects

Ordered by how directly they touch the failing areas. All are in the window and
all are on `dev`:

- `e3cb2ab17` perf(jit): bind background-compiled invokestatic/invokespecial to
  a direct CALL — the `ir_direct_call_*` cluster.
- `e8158038c` fix(jit): reject ARRAY receivers at every class-id-keyed inline
  cache — the `ic_*` and `hashed_vtable_stub_rejects_array_receivers` cluster.
- `31a80bfb8` fix(jit): service a compiled callee's deopt at the inline call
  site — `ir_direct_call_exception_sentinel_bails`.
- `66548471f` fix(jit): run a compiled method's `finally` when an exception
  passes through it.
- `90e63b1b5` fix(jit,gc): close the RBC.6 precise-handler-frame family.

Ruled out: the harness wiring. Setting `service_callee_deopt` to a real-ABI
helper that returns the sentinel unchanged, versus leaving it unwired at 0,
changes nothing about which tests fail.

## Reproducing

```bash
cargo test -p cratonvm-jit --test ir_vs_singlepass -- --test-threads=1
cargo test -p cratonvm-jit --lib
```

For the known-good comparison:

```bash
git worktree add --detach /tmp/wt-0727 c05f85967
```

then copy this branch's `jit/tests/ir_vs_singlepass.rs` over the old one (it
only adds the two new helper fields and a correct-ABI `set_throw_bci` no-op)
and run the same command. Note that a zero-argument `panic!` stub for
`set_throw_bci` faults the process rather than failing a test — it is called
from JIT-generated code and unwinds across the boundary.

## Why this is filed rather than fixed

The compile breaks are fixed on `fix/deep-audit-retire-20260730` because a gate
that cannot build is not a gate. The regressions underneath belong to the JIT
work that introduced them and want their author's context; bisecting them to
individual commits needs a build per step, since the test target only compiles
at the window's start and after this branch's fix.

The general lesson is the one the deep audit was written about: a test target
that stops compiling is indistinguishable from a test target that passes, and
this repository has now hit that failure mode twice — once with the
`synthetic-jdk` module (1,522 tests), once here.
