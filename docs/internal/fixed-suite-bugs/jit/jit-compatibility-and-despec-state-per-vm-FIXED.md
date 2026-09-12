# FIXED: the JIT kept compatibility and despeculation state in process globals, so one VM changed what another compiled

**Status: FIXED 2026-09-12.** JIT review finding #71 (medium). The
`JIT_COMPATIBILITY_MODE` latch is deleted. `DESPEC_SET` became a per-VM
`DespecRegistry`. `BACKGROUND_COMPILER` was already fixed on this branch.
Ratchet test: `jit/tests/process_global_statics_ratchet.rs`.

## The defect

AGENTS.md forbids process globals for compatibility state: "It is per-VM". A
JIT review counted the `static` declarations in `cratonvm-jit` and found three
that broke the rule for exactly the state it protects.

1. **`JIT_COMPATIBILITY_MODE`** (`jit/src/lib.rs`), an `AtomicU8`.
   `vm/src/jit/helpers.rs::build_helpers` raised it with `fetch_max` on every
   VM start, so it only ever moved `Compatible -> JdkOnly`. The first VM created
   in `--jdk-only` mode made every later VM in the process over-strict, and
   nothing could lower it. The design note (`jdk-only-mode.md`) had logged it
   as a known violation.
2. **`DESPEC_SET`** (`jit/src/deopt.rs`), a
   `OnceLock<RwLock<FxHashSet<(String, u32)>>>` of `(method_key, bci)` sites
   that had deopted past the per-bci limit. A despeculation is a verdict about
   what one VM's program did at a bci. With the set shared, an embedded VM, or
   the second VM in a test, inherited verdicts it never earned. It then
   compiled without speculations its own profile supported: loop-header
   bounds-check guards, LICM hoists, the arraycopy intrinsic, and the guarded
   String receiver intrinsics. It also changed `recommend_action_at_bci`'s
   blacklist decision.
3. **`BACKGROUND_COMPILER`** (`jit/src/tiered.rs`), one process-wide compile
   worker. A second VM's compile queue was never drained.
   `vm/tests/pgo01_call_site_evidence.rs` builds two VMs, and the second VM's
   tiering assertions silently tested the interpreter.

The first two are silent in one-VM production runs, and neither error is loud:
a latch that makes code slower, and a speculation dropped for no reason.

## The fix

### `JIT_COMPATIBILITY_MODE`: deleted, because nothing per-VM still read it

The latch already had no dispatch-affecting reader when this fix started.
Earlier work had threaded the policy as an argument: `jdk_only` on
`try_compile_with_invokespecial_resolver` (2026-08-06) and the policy argument
on `jit_entry_publishable` (2026-08-10). Every VM compile door (the three
`try_compile_with_invokespecial_resolver` calls in `jit_bridge.rs`) passes
`dispatch_policy(shared).is_jdk_only()`. That is `VmConfig::execution_policy()`,
fixed for the life of the VM.

Readers and writers at the start of the fix:

| Site | Role | Now |
|---|---|---|
| `helpers.rs::build_helpers` | writer (`set_jit_execution_policy`) | call removed |
| `lib.rs::try_compile` (legacy wrapper) | the only reader (`jit_is_jdk_only()`) | passes `false` |
| `lib.rs::set_jit_execution_policy`, `jit_compatibility_mode`, `jit_is_jdk_only` | API | deleted, with the static and its two `JIT_MODE_*` constants |

`try_compile` has no VM callers. Its callers are this crate's tests, where
nothing ever latched, so it always read `Compatible`. Passing `false` is what it
already did. Moving a static with no per-VM reader onto `JitCache` would have
created per-VM state that nothing reads, so it was deleted instead.

Semantics within one VM are unchanged. The "ratchet" was only ever needed
because several VMs wrote to one cell. A VM's own policy never changes after
construction, so per VM it trivially never moves back.

The JDK-only refusal counters and violation sinks (`JDK_ONLY_*` in `lib.rs`)
are still process-wide. `SharedVm::jdk_only_process_violations` documents what
that means for a report. They are diagnostics, not policy.

### `DESPEC_SET`: a `DespecRegistry` owned by the VM's `JitRealm`

`jit/src/deopt.rs` now defines `pub struct DespecRegistry`, the same
`std::sync::RwLock<FxHashSet<(String, u32)>>` with the same methods:
`insert`, `contains` and `count_for`. `contains` keeps its fast paths (empty
key, empty set), and a poisoned lock still reads as "not de-spec'd". Only the
owner changed.

The owner is `JitRealm::despec_registry: Arc<DespecRegistry>`
(`vm/src/vm/realms/jit_realm.rs`), created in `SharedVm::new`. It sits beside
`deopt_log` rather than inside it, because the log is behind a `Mutex` and
compiles read the registry without taking it. It is an `Arc` because the x64
`Compiler` has no lifetime parameter and holds a clone for one compile, which
may run on a background compile worker. `Arc<DespecRegistry>` is `Send + Sync`.

Every compile request carries it as `Option<&Arc<DespecRegistry>>`:

| Site | Role | Change |
|---|---|---|
| `deopt_resume.rs::real_frame_deopt_resume_and_despeculate` | writer | `shared.jit.despec_registry.insert(..)` |
| `vm_init.rs::SharedVm::record_deoptimization` → `DeoptimizationLog::recommend_action_at_bci` | reader | takes `despec: &DespecRegistry` |
| `lib.rs::try_compile_with_invokespecial_resolver` → `try_compile_inner` | reader (String receiver guard filter) | new trailing `despec` parameter |
| `x64/driver.rs::compile_with_param_slots` | reader (LICM hoist filter, speculative-BCE guard filter) | new `despec` parameter after `method_key`; sets `Compiler::despec` |
| `x64/bytecode_walk.rs` (arraycopy direct-call filter, arraycopy intrinsic ladder) | reader | `self.despec` |
| `jit_bridge.rs::compile_osr_artifact` (String receiver guard filter) | reader | `shared.jit.despec_registry.contains(..)` |
| `jit_bridge.rs` (three tiering compile calls), `compile_osr_artifact`, `interpreter.rs::execute` (eager compile) | pass-through | `Some(&shared.jit.despec_registry)` |
| `lib.rs::try_compile`, `x64/driver.rs::compile`, crate fixtures | no VM | `None`, which consults nothing |

`despec_clear_for_test` is gone. It existed only because the set leaked across
in-process tests. Tests now own a registry: the unit test in `deopt.rs`,
`jit/tests/intrinsic_arraycopy.rs`, and `deopt_resume.rs`'s
`step9_fuc_per_bci_despec_after_limit` through its own `SharedVm`.

### `BACKGROUND_COMPILER`: already fixed

`TieredCompilationManager` owns its compile workers
(`TieredCompilationManager::ensure_background_compiler`), so each VM's queue
has its own drain. There is no `BACKGROUND_COMPILER` static left in `jit/src`.
`vm/tests/pgo01_call_site_evidence.rs` needed no change. Its second VM is a
negative control for the profiling gates. It does not work around the compile
queue.

## What stays process-wide on purpose

`JIT_PROCESS_REDEFINE_EPOCH` (`lib.rs`) is deliberately process-wide. It
invalidates process-wide verdict stores. `JitCache` carries the per-VM
`redefine_epoch`. Most remaining statics are process-invariant: `fn` helper
addresses, `OnceLock` caches of environment flags, metrics counters and
thread-locals.

## The ratchet

`jit/tests/process_global_statics_ratchet.rs` counts static declarations in
`jit/src/**/*.rs`. The unit is one per line whose first non-blank text is an
optional `pub` or `pub(...)`, the keyword, an optional `mut`, an identifier and
`:`. That covers module-level statics, statics inside functions,
`thread_local!` lines and test modules. It fails when the count exceeds
`BASELINE = 744`, the count after this fix, with the equivalent

```text
grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
    jit/src --include=*.rs | wc -l
```

The review counted 620 by a different rule. The number that matters is this
test's. The failure message says where new per-VM or compatibility state
belongs: the VM's JIT state, threaded into the compile request. It also says
to lower the baseline when a static is removed. A second test pins the scanner
on lines with known answers (comments, `'static`, macro `$name` patterns,
`pub(crate)`, `static mut`).

## Regression coverage

* `jit/src/deopt.rs`: `despec_registry_is_per_method_bci`, and
  `despec_registries_do_not_share_verdicts` (two registries, one verdict).
* `vm/src/runtime/interpreter/deopt_resume.rs`:
  `step9_fuc_per_bci_despec_after_limit` now reads the VM's own registry. It
  also asserts that a second `SharedVm` does not see the first one's
  despeculation.
* `jit/tests/intrinsic_arraycopy.rs`:
  `arraycopy_despec_uses_dispatch_not_intrinsic_sentinel` passes its own
  registry through `compile_with_param_slots`.
* `jit/src/lib.rs`: `direct_native_helper_answers_per_vm_not_per_process`
  (pre-existing) still pins the policy-as-argument half.
* `jit/tests/process_global_statics_ratchet.rs`: the count.
