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
| `vm/src/vm.rs` `system_getenv_returns_value` | **`System.getenv` resolves through `runtime_var`, and `CRATONVM_TEST_VAR` is declared** — so this asserted on the snapshot, not on the `set_var` | thread override |
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

## Guard

`types/tests/flag_env_mutation_guard.rs` walks the workspace's Rust sources and
fails if any `set_var`/`remove_var` names a declared flag literal. Its allowlist
is empty. This matters because the defect is invisible in review: the broken
line is indistinguishable from a working one.

Verified non-vacuous by planting an offending file and confirming the failure
names it.

## Limitation, deliberately not closed

`vm/src/runtime/env_cache.rs` memoises ~37 hot flags in their own `OnceLock`s
*downstream* of the snapshot. An override reaches those only before their first
read in the process. That is by design — they are read from interpreter and JIT
hot paths and the memoisation is what keeps them free — and the two tests that
depend on it (`jit_deep_recursion_fault_recovery`, and the `bg_compile` route)
install their override before VM creation and hold it for the whole test. The
override hooks document the limitation.

## Verification

```
cargo test -p libcratonvm --lib -- --test-threads=1
cargo test -p cratonvm-types --lib flags::
cargo test -p cratonvm-types --test flag_env_mutation_guard
```

The first is the original repro: it failed before, passes now. Non-vacuity of
the fix itself was confirmed by pointing `with_no_jdk` at a directory that *is*
a valid JDK and watching the test fail — it is asserting on the override, not
on the host.
