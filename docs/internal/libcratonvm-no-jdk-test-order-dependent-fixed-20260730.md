# `config_from_args_fails_loudly_when_no_jdk_is_available` passed only when it ran first — FIXED

| | |
|---|---|
| **Status** | FIXED 2026-07-30. Retired from `docs/known-issues/`. |
| **Category** | TEST-ISOLATION (frozen flag snapshot vs. per-test env override) |
| **Found** | 2026-07-30, in a full-workspace run. |
| **Fixed by** | `flags::override_thread` / `override_process`, plus a sweep of every call site in the family. |

## Original symptom

```
cargo test -p libcratonvm --lib config_from_args_fails_loudly   # passed
cargo test -p libcratonvm --lib -- --test-threads=1             # FAILED
```

The failure message showed the resolved config carrying a real JDK
(`java_home: Some("…/jdk-25…")` on Windows, `Some("/usr/lib/jvm/java-21-openjdk-amd64")`
on the Linux build host) — exactly what the test's harness set out to prevent.

## Mechanism

`with_no_jdk` pointed `CRATONVM_JAVA_HOME` at an empty scratch directory with
`std::env::set_var` and unset `JAVA_HOME`, then called `config_from_args`.

But `CRATONVM_JAVA_HOME` is a *declared* CratonVM flag, and declared flags come
from one immutable process-wide snapshot: `flags()` is a `OnceLock` initialised
from the environment on first access. Any earlier test in the same binary that
touched `flags()` froze the snapshot, and from then on `set_var` changed
`environ` and nothing the code under test would read. `resolve_java_home` then
fell through to its `java`-on-`PATH` probe and found the developer's real JDK.

So the test asserted real behaviour only when it won the race to initialise
`FLAGS`. Under `--test-threads=1` the order is deterministic and it lost every
time.

This is the vacuous-gate family: a test that passes because of *when* it ran,
not because of what the code does.

## The fix

`cratonvm_types::flags` grew a supported, scoped override — the "test-only
installer" the original report named as option 1, chosen over making
`CRATONVM_JAVA_HOME` a live read because it fixes the whole family rather than
one test:

```rust
flags::with_thread_overrides(&[("CRATONVM_JAVA_HOME", Some(path))], || { … });
flags::with_process_overrides(&[("CRATONVM_BG_COMPILE", Some("0"))],  || { … });
let guard = flags::override_process(VmFlags::from_env_with_edits(&[…]));
```

* **Thread scope is the default.** `cargo test` runs tests in parallel inside
  one binary, so an override that changed process-global state would perturb
  concurrent tests — the very hazard the old `set_var` fixtures papered over
  with a mutex. A thread-local override cannot.
* **Process scope exists for readers on other threads** — a booted `Vm`'s
  workers, a background JIT compile. It requires the caller to serialise, and
  every current user does (`mp_root_lock`, `env_lock`, or being alone in its
  binary).
* `VmFlags::from_env_with_edits` builds "the process environment, plus these",
  where `None` means *as if never exported* — which `from_env_with_overrides`
  could not express, since a `MapSource` overlay can only add. Edits are
  applied before `flag_groups::resolve`, so overriding either the grouped
  expression (`CRATONVM_JIT=threshold=7`) or the legacy key it expands to
  behaves exactly as exporting it would.
* `MapSource::without` is the matching builder-side removal.

### Cost on the production path

`flags()` gained one relaxed load of a static counter plus a perfectly
predicted branch; the thread-local probe is `#[inline(never)]` and unreachable
until a guard exists. It is compiled in unconditionally, *not* behind a Cargo
feature: a feature would have to be enabled for the whole dependency graph
during `cargo test`, and would flip on every `cargo test` / `cargo build`
alternation, forcing a full workspace rebuild each way.

The override configuration is leaked (`Box::leak`), which is what makes the
`&'static VmFlags` that `flags()` returns sound. Tests install a handful of
these; do not call it in a loop.

## Residuals swept

The original report said the defect "is not specific to this test — **any** test
in any binary that tries to override a declared CratonVM flag through the
environment has the same defect". That was correct, and the sweep found ten
more sites. Two were live defects, not just latent ones:

| Site | Was | Now |
|---|---|---|
| `libcratonvm/src/lib.rs` `with_fake_jdk` / `with_no_jdk` | the reported failure | thread override |
| `vm/src/config.rs` — 4 JDK-detection tests | **vacuous**: measured the developer's real JDK, passed anyway because their assertions happen to hold for any JDK | thread override, via `with_scratch_java_home` |
| `vm/src/vm.rs` `system_getenv_returns_value` | **outright failing** under `--features synthetic-jdk` (see below) | thread override |
| `vm/tests/wp8_10_jboss_modules_smoke.rs` | 7 tests each set a *different* `CRATONVM_JBOSS_MP_ROOT`; at most the first took effect, the rest pointed at an already-deleted tempdir | process override held by the fixture, serialised by `mp_root_lock` |
| `native-builtins/src/lib.rs` — 4 `jboss.home.dir` fallback tests | ambient `CRATONVM_JBOSS_MP_ROOT` stayed in force; "must fall back" held for the wrong reason | thread override, via `with_jboss_env` |
| `jit/src/lib.rs`, `jit/src/x64.rs` | `CRATONVM_JIT_DIRECT_CALLEE_CALLS=1` was a no-op; the assertions rode on the flag's *default* and would have silently stopped covering the direct-callee path if that default flipped | thread override |
| `vm/tests/jit_deep_recursion_fault_recovery.rs` | worked only because the binary holds one test | process override guard |
| `gc/tests/stale_objref_*.rs` (2) | worked only because each binary holds one test | process override guard |

`with_env` in `libcratonvm` and `vm/src/config.rs` still exists for
`JAVA_HOME` / `JBOSS_HOME`, which are **not** declared and so keep
`std::env`'s live-read semantics. Both now `debug_assert!` against being handed
a `CRATONVM_*` name.

Child-process tests (`Command::env`, e.g. `vm/tests/jit_interp_differential.rs`,
`difftest`) were never affected — the child reads its own environment before it
latches anything.

### `system_getenv_returns_value` was not merely vacuous — it was red

`System.getenv` resolves through `flags::runtime_var`, and `CRATONVM_TEST_VAR`
is a *declared* flag, so the lookup was answered from the snapshot. The test
constructed a `SharedVm` (which reads flags, latching the snapshot) *before*
calling `set_var`, so the write could never be seen: `getenv` returned null and
the test panicked with `expected string`.

Nobody saw it because `vm/src/vm.rs`'s whole test module is
`#[cfg(all(test, feature = "synthetic-jdk"))]` and that feature is off by
default — so neither `cargo test` nor `cargo check --all-targets` compiles it.
Confirmed against the parent commit:

```
$ cargo test -p cratonvm-vm --lib --features synthetic-jdk system_getenv
test vm::tests::system_getenv_returns_value ... FAILED
  panicked at vm/src/vm.rs:11591:18: expected string
```

and passing after the fix. Worth remembering as a general point: a default-off
feature gate hides its test module from every routine check in this workspace.

## Guard

`types/tests/flag_env_mutation_guard.rs` walks the workspace's Rust sources and
fails if any `set_var`/`remove_var` names a declared flag literal. Its allowlist
is empty. This matters because the defect is invisible in review: the broken
line is indistinguishable from a working one.

Verified non-vacuous by planting an offending file and confirming the failure
names it.

## The second latch — closed 2026-07-31

`vm/src/runtime/env_cache.rs` memoised ~90 hot flags in their own `OnceLock`s
*downstream* of the snapshot, so an override reached them only before their
first read in the process. This was originally filed as a documented
limitation; it is now fixed, because "install the override before the first
read" is the same order-dependence this whole document is about, one layer
down.

The memo is now **invalidated by the writer** rather than **re-validated by the
reader**. `flags::MemoSlot` holds its value inline in one `AtomicU8`; installing
or dropping a `FlagOverride` walks every registered slot and resets it, and
`MemoSlot::publish` declines to store while an override is live (so nothing
latches an override's value and poisons the process). The registry `Mutex` is
what makes "no override is live" and "store" one step against a concurrent
install.

Five memos whose value does not fit a `u8` — a threshold, an OSR back-edge
count, a pattern list, a selector — keep a `flags::overrides_active()` check per
read instead. None are on the per-bytecode path.

### Why the writer pays, with numbers

Measured with callgrind (`BinT 17 --nojit`, pure interpreter, 5 samples per arm,
run-to-run spread 0.008–0.013%), against the same commit built in the same
target dir:

| design | Ir vs base | of steady-state interpreter work |
|---|---|---|
| `overrides_active()` on every read | +0.507% | +0.909% |
| `MemoSlot`, invalidated by the writer | **+0.240%** | **+0.431%** |
| `MemoSlot` with `fn()` instead of `impl FnOnce` | +0.850% | +1.52% |

That last row is worth keeping: making the cold arm take a `fn` pointer rather
than a ZST closure looks like it should shrink code, and instead each *hot* call
site has to materialise the pointer before the branch that almost never needs
it. Same shape as
`docs/internal/.../per-object-fixed-tax-outlined-gate` — a default-inert gate
still costs whatever its call site has to set up.

**Wall clock could not resolve any of this.** On the shared build host the
`--nojit` baseline alone spread 24% run-to-run, and two 12-round interleaved
sweeps of the same pair of binaries disagreed in sign (+0.8%, then −5.8%).
Instruction counts are the measurement of record here; the JIT-on arms
(`CratonBench fib`, `bintrees`) showed no signal either way. Every checksum
matched across every arm.

The residual +0.43% of interpreter instructions is the honest price of making
~90 flags overridable. It buys the last layer of the vacuous-test family:
`jit_deep_recursion_fault_recovery` and `wp8_10_jboss_modules_smoke` no longer
depend on installing their override before the first read.

## Verification

```
cargo test -p libcratonvm --lib -- --test-threads=1
cargo test -p cratonvm-types --lib flags::
cargo test -p cratonvm-types --test flag_env_mutation_guard
cargo test -p cratonvm-vm --lib --features synthetic-jdk system_getenv
cargo test -p cratonvm-vm --test wp8_10_jboss_modules_smoke
cargo test -p cratonvm-vm --test jit_deep_recursion_fault_recovery
cargo test -p cratonvm-gc --test stale_objref_debug_assertion                           --test stale_objref_quarantine_ring
cargo test -p cratonvm-native-builtins --lib bootstrap_property_fallback_tests
```

The first is the original repro: it failed before, passes now. Non-vacuity of
the fix itself was confirmed by pointing `with_no_jdk` at a directory that *is*
a valid JDK and watching the test fail — it is asserting on the override, not
on the host.
