# `CRATONVM_*` flag inventory

Report P1, *Centralize configuration reads*. Acceptance:

> Environment variables are parsed once into a typed immutable configuration;
> semantic code contains no direct environment reads.

This file is the inventory half. It records every `CRATONVM_*` identifier, where
it is read, whether it is declared, whether it latches, whether it can change a
program's result, and its default — plus the plan for the reads that still go
straight to `std::env`.

## Contents

1. [How to regenerate](#how-to-regenerate)
2. [Where the surface stands](#where-the-surface-stands)
3. [Latching, and why an undeclared flag is a test hazard](#latching-and-why-an-undeclared-flag-is-a-test-hazard)
4. [What was declared in this pass](#what-was-declared-in-this-pass)
5. [The typed configuration](#the-typed-configuration)
6. [Migration plan](#migration-plan)
7. [The allowlist, and why each row is on it](#the-allowlist-and-why-each-row-is-on-it)
8. [Open reconciliation items](#open-reconciliation-items)
9. [Full inventory](#full-inventory)

---

## How to regenerate

```bash
tools/flag-census/render-inventory.sh
```

The table in [Full inventory](#full-inventory) is generated, not maintained.
For its first eight months that was a claim with no generator behind it, and
the table drifted **42 rows** behind `INVENTORY` while stating a declared count
of 647 against a true 689. Nothing failed, because "generated" is exactly the
label that stops anyone reading a file sceptically. There is now a generator
(above) and a test — `types/tests/flag_docs_generated.rs` — that fails
`cargo test -p cratonvm-types` when the checked-in table and the code disagree
about which variables exist, or when the counts below do not describe the
table.

Its three inputs are all in the tree:

| Input | What it contributes |
|---|---|
| `types/src/flag_groups.rs::INVENTORY` | group, canonical token, `on_key`/`off_key`/`off_word` — which give the shape and the default |
| `types/tests/flag-surface.txt` | the checked-in expected surface, asserted equal to `INVENTORY` by `types/tests/flag_surface.rs` |
| a scan of `<crate>/src/**/*.rs` for exact `"CRATONVM_…"` literals | the *Read in* column |

Derivation rules, so a regenerated table matches this one:

* **Shape** — `opt-in` when only `on_key` is set, `opt-out` when only `off_key`
  is set, `default-on` when `on_key` carries an `off_word`, `both` when the knob
  has an explicit spelling in each direction, `scalar` for the five named
  scalars, `group` for the ten grouped variables.
* **Default** — `off` for `opt-in`/`both`, `on` for `opt-out`/`default-on`.
  This is the default *of the canonical token*, which is always stated
  positively. `CRATONVM_MOVING_YOUNG_NO_JIT` therefore appears as `opt-out` /
  `on`: the token is `moving-young-jit-frames`, that capability is on, and
  exporting the `NO_` variable is what removes it.
* **Class** — `diag` for the `DBG` group, whose contract is that no token in it
  can change a program's result; `behaviour` for every other group. That is a
  contract, not a measurement: a flag that changes behaviour does not belong in
  `DBG` no matter how diagnostic its name sounds. (`CRATONVM_DBG_GC_STRESS` is
  the standing exception and is documented as such at `GcFlags::gc_stress_bytes`
  — it changes GC scheduling.)
* **Latched** — `snapshot` for every declared name, `live getenv` for the
  allowlisted ones. See the next section for why this column is the interesting
  one.
* **Read in** — crates whose `src/` contains the name as an exact string
  literal, outside a whole-line comment. `types/src/flag_groups.rs` is excluded:
  the registry names every variable, and that is a declaration, not a read.

Three tests keep the inputs honest, and all three are `cargo test -p
cratonvm-types`:

| Test | Question it answers |
|---|---|
| `types/tests/flag_surface.rs` | does the fixture match `INVENTORY`? |
| `types/tests/flag_declaration_guard.rs` | is every `CRATONVM_*` literal in the workspace declared or explicitly exempt? |
| `types/tests/flag_env_mutation_guard.rs` | does anything try to `set_var` a declared flag? |
| `types/tests/flag_docs_generated.rs` | do the two *generated* documents — this file's table and `docs/flag-tokens.md` — still match `INVENTORY`? |

`tools/flag-census/check-surface.sh` is the CI-side companion.

---

## Where the surface stands

| | count |
|---|---|
| distinct `CRATONVM_*` identifiers appearing anywhere in Rust source | 692 |
| exact string literals (i.e. actually named by code, not prose) | 658 |
| **declared** in `flag_groups::INVENTORY` + scalars + group variables | **914** |
| declared before this pass | 576 |
| declared by this pass | **71** |
| allowlisted as intentionally undeclared | 11 |
| user-facing names an operator has to learn | 15 |

Read-path split, over `<crate>/src` only:

| Path | sites |
|---|---|
| `flags::runtime_var[_os]` — inside the boundary | 421 |
| direct `std::env` naming a `CRATONVM_*` **literal** in a runtime crate — outside it | 17 |
| direct `std::env` on a `CRATONVM_*` name built at **runtime** | 1 — `cuda-bridge/src/critical.rs::resolve_ms` |
| direct `std::env` in `vm-cli` (the launcher) | 20 |
| direct `std::env` on a **non**-CratonVM name (correct: live semantics) | 3 — `HOME`, `JBOSS_HOME`, one dynamic key in `vm/src/config.rs` |

The 421 already resolve against the snapshot. The 38 CratonVM ones do not, and
they are what the [migration plan](#migration-plan) is about. The 3 non-CratonVM
reads are semantically correct where they sit, but they still trip
`check-surface.sh` check 4, which bans the call *shape* rather than the flag —
see [Open reconciliation items](#open-reconciliation-items).

---

## Latching, and why an undeclared flag is a test hazard

`flags::runtime_var` is a fork in the road:

```rust
if declared_flag_names().contains(name) {
    return flags().legacy_var_os(name);   // the latched snapshot
}
std::env::var(key)                        // live getenv
```

So a variable's declaration status silently decides its semantics:

| | declared | undeclared |
|---|---|---|
| value comes from | the snapshot latched on the first read of *any* flag | a live `getenv`, every time |
| reachable from `CRATONVM_<GROUP>=token` | yes | no |
| reachable from `flags::with_thread_overrides` / `with_process_overrides` | yes | **no** |
| visible to `flag_groups::expand_process_env` | yes | only if something else exported it |
| appears in `docs/CONFIG.md` | yes | no |

The third row is the one that costs debugging days.

Suppose a test wants a flag on. The supported way to do that is
`with_thread_overrides(&[("CRATONVM_X", Some("1"))], || …)`, which builds a
`VmFlags` and installs it for the calling thread. If `CRATONVM_X` is **declared**
this works: the reader calls `runtime_var`, `runtime_var` consults `flags()`,
`flags()` returns the override. If `CRATONVM_X` is **undeclared** the override is
built and installed and then completely ignored — the reader falls through to
`std::env`, which the override never touched. The test does not fail; it
measures whatever the developer's shell happened to export. Worse, the whole
class of "flag off" assertions passes vacuously on a clean machine and starts
failing on a CI runner that inherits the variable, which reads as a flake.

The mirror-image defect — `set_var` on a flag that *is* declared — has the same
shape and is already written up:
`libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`.
`flag_env_mutation_guard.rs` catches that one. `flag_declaration_guard.rs`, added
in this pass, catches this one.

There is a second, quieter consequence. `flags()` latches on first use, and the
first use is whatever runs earliest — `types::field_layout`'s compact-reference
decision, `native_io`'s deployment profile, `classloading`'s classpath setup. An
undeclared flag read *after* that point sees a different world from every
declared flag around it, which is exactly the kind of "these two gates disagree"
bug the `CRATONVM_JIT_MY_SHADOW_EMISSION` note in `jit::x64::licm` warns about:
emission and root-scan are two halves of one agreement, and a flag that reaches
only one half is a collector walking a shadow stack the codegen never pushed to.

---

## What was declared in this pass

71 variables, 71 new tokens, no change to any observable default. Every entry
below was read against its call site before being written down; the *Parser*
column names the exact idiom at that site.

Eight of the 71 were not in the original brief and were found by running the new
guard while writing this document: `CRATONVM_JIT_VERIFY_MEMORY_CHAIN`,
`CRATONVM_JIT_VERIFY_ARENA_ORDER` and `CRATONVM_DBG_IR_SLOTS` arrived from a
concurrent `jit` change, and `CRATONVM_GPU_CRITICAL_WAIT_MS` /
`CRATONVM_GPU_CRITICAL_LEASE_MS` from a concurrent `cuda-bridge` change, and
`CRATONVM_PHASE_ACCOUNTING` / `_OUT` / `_JFR` from a concurrent `jfr` change —
all within the same session. That rate — eight new undeclared flags in one
afternoon on one branch — is the argument for the guard being a test rather
than a convention.

### JIT IR verifier — `jit/src/ir_verify.rs`

| Variable | Token | Parser | Default | Note |
|---|---|---|---|---|
| `CRATONVM_JIT_VERIFY_IR` | `JIT=verify-ir` | tri-state `1/true/yes/on` ÷ `0/false/no/off` | build profile | **three** states, see below |
| `CRATONVM_JIT_VERIFY_TYPES` | `JIT=verify-types` | same | off | type-lattice lane |
| `CRATONVM_JIT_VERIFY_FRAME_STATES` | `JIT=verify-frame-states` | same | off | safepoint-snapshot lane |
| `CRATONVM_JIT_VERIFY_SCHEDULE` | `JIT=verify-schedule` | same | off | compatibility alias for the two below |
| `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` | `JIT=verify-memory-chain` | same | `verify-schedule` | memory-token chain |
| `CRATONVM_JIT_VERIFY_ARENA_ORDER` | `JIT=verify-arena-order` | same | `verify-schedule` | heuristic def-before-use |

`CRATONVM_JIT_VERIFY_IR` is the only genuinely tri-state knob in the table and
the reason `parse::tristate_word` exists:

* unset — the per-pass verifier follows `cfg!(debug_assertions)`, and the
  unconditional pre-lowering check stays on;
* `1` — the per-pass verifier runs in a release build too;
* `0` — the per-pass verifier is off **and** `pre_lower_verify_disabled` fires,
  which is the operator's escape hatch from a verifier false positive costing
  them the entire optimizing tier.

Collapsing that into a `bool` at the parse boundary would lose the third state.
The `off_word: Some("0")` on the inventory row is what lets
`CRATONVM_JIT=-verify-ir` reach it, rather than merely unsetting the variable
back to the profile default.

The last two rows landed mid-pass: a concurrent change split the old
`check_schedule` lane in two and added `CRATONVM_DBG_IR_SLOTS`. They were caught
by running the new guard, which is the argument for the guard.

### JIT metrics — `jit/src/metrics.rs`

| Variable | Token | Parser | Default |
|---|---|---|---|
| `CRATONVM_JIT_METRICS` | `JIT=metrics` | tri-state, defaulted `false` | off, *including in debug builds* |
| `CRATONVM_JIT_METRICS_OUT` | `JIT=metrics-out` | non-empty `OsString` | none |
| `CRATONVM_JIT_METRICS_RING` | `JIT=metrics-ring` | trimmed `usize`, kept when `> 0` | `DEFAULT_RING_CAPACITY` (256) |

`_OUT` treats an exported-but-empty value as "no sink", and `_RING=0` falls back
rather than being honoured — a ring of zero would make `last_compilation_report`
permanently `None`, which is never what an operator means. Both are preserved in
`JitMetricsConfig`. The `256` itself stays in `jit`: it is a tuning constant of
the ring, not of the environment, so `ring_capacity_or(default)` takes it.

### Other JIT / execution-engine knobs

`CRATONVM_DBG_IR_BAILOUT`, `CRATONVM_DBG_IR_COMPILES`, `CRATONVM_DBG_IR_RELOC`,
`CRATONVM_DBG_IR_SLOTS`, `CRATONVM_DBG_EXCFRAME`, `CRATONVM_DBG_JIT_CODE_FREE`,
`CRATONVM_DBG_JIT_PIN`, `CRATONVM_DBG_JIT_STALE_IC`, `CRATONVM_DBG_JIT_UNMAP`,
`CRATONVM_DBG_DEOPTSLOT`, `CRATONVM_DBG_DISPATCH_TALLY`,
`CRATONVM_DBG_ROOTSNAP_EVERY`, `CRATONVM_DBG_ROOTSNAP_VERIFY` — diagnostics,
all default-off, all `DBG` tokens.

Behaviour-changing, and **five of them default ON** — worth stating because the
name does not say so:

| Variable | Token | Default | Off word |
|---|---|---|---|
| `CRATONVM_JIT_BULK_BYTE_LOOPS` | `JIT=bulk-byte-loops` | on | `0`/`false`/`off` |
| `CRATONVM_JIT_GC_INERT_SELFREC` | `JIT=gc-inert-selfrec` | on | `0`/`false`/`off` |
| `CRATONVM_JIT_IR_RELOC_EMIT` | `JIT=ir-reloc-emit` | on | `0`/`false` |
| `CRATONVM_JIT_MATRIX_DOT` | `JIT=matrix-dot` | on | `0`/`false`/`off` |
| `CRATONVM_JIT_STRICT_CALLEE_ROOTS` | `JIT=strict-callee-roots` | **on** | `0` |

`CRATONVM_JIT_STRICT_CALLEE_ROOTS` deserves the bold: its own doc comment
describes it as something you turn on with `=1`, but the code is
`.unwrap_or(true)` — it is on unless you turn it off. The declaration follows the
code, and this line exists so the next reader does not have to re-derive it.

The three moving-young bisect levers — `CRATONVM_JIT_MY_SCRATCH_FLUSH`,
`CRATONVM_JIT_MY_SELFCALL_PROOF`, `CRATONVM_JIT_MY_SHADOW_EMISSION` — are also
default-on. `MY_SHADOW_EMISSION` is read in *two* crates (`jit::x64::licm` and
`vm::jit::conservative_roots`) with a byte-identical expression, on purpose:
they are two halves of one agreement. One declaration now covers both halves.

Remaining JIT rows: `CRATONVM_JIT_FORCE_C2`, `CRATONVM_JIT_LEAK_CODE`,
`CRATONVM_JIT_NEVER_FREE_CODE`, `CRATONVM_JIT_POISON_FREE`,
`CRATONVM_JIT_OSR_DEAD_MASK_BLANKET` (opt-in);
`CRATONVM_JIT_NO_EXC_TABLE_C2`, `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES`,
`CRATONVM_NO_JIT_TLAB_ZERO_ELISION` (opt-out spellings, stated positively as
`exc-table-c2`, `precise-handler-frames`, `tlab-zero-elision`);
`CRATONVM_JIT_PUTFIELD_INIT` and `CRATONVM_TRIVIAL_GETTER` (default-on kill
switches); `CRATONVM_TRIVIAL_GETTER_VERIFY` (diagnostic).

### GC — `gc/src/gc_metrics.rs`, `gc/src/vm_heap.rs`, `gc/src/gen_heap.rs`

| Variable | Token | Parser | Default |
|---|---|---|---|
| `CRATONVM_GC_CARD_METRICS` | `GC=card-metrics` | **presence** | off |
| `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` | `GC=mirror-pin-young-defer` (off) | presence | capability on |
| `CRATONVM_DBG_OOM_BT`, `CRATONVM_DBG_YOUNG_TRIGGER` | `DBG=oom-bt`, `DBG=young-trigger` | presence | off |

`CRATONVM_GC_CARD_METRICS` is a presence check, so `CRATONVM_GC_CARD_METRICS=0`
**enables** it. That is what the code does today and the declaration preserves
it; it is called out in `GcMetricsConfig::card_metrics` and pinned by a test, so
that nobody "fixes" it without measuring. The gate sits on a barrier that runs on
every reference store, so the thread-local byte in `gc_metrics` stays in front of
the snapshot read — only the cold `resolve` step consults the config.

### GPU critical sections — `cuda-bridge/src/critical.rs`

| Variable | Token | Parser | Default |
|---|---|---|---|
| `CRATONVM_GPU_CRITICAL_WAIT_MS` | `GC=gpu-critical-wait-ms` | trimmed `u64` | `DEFAULT_COLLECTOR_WAIT_MS` |
| `CRATONVM_GPU_CRITICAL_LEASE_MS` | `GC=gpu-critical-lease-ms` | trimmed `u64` | `DEFAULT_TOKEN_LEASE_SECS × 1000` |

Filed under `GC` because that is where `gpu-zerocopy` already is, and because
these budgets are what bound a collector's wait on a GPU critical section.

Both are read by `resolve_ms`, which takes the variable *name* as an argument and
calls `std::env::var` on it. That shape is doubly outside the boundary: it
bypasses the snapshot, and because the name is not a literal it is invisible to
`flag_declaration_guard.rs` and to `check-surface.sh`. These two were found by
reading the file, not by the guard. Declaring them fixes the reachability half;
Stage 1 of the migration fixes the read.

### Phase accounting — `jfr/src/phase.rs`

| Variable | Token | Parser | Default |
|---|---|---|---|
| `CRATONVM_PHASE_ACCOUNTING` | `DBG=phase-accounting` | level word: `1`/`true`/`on`/`coarse` → coarse, `fine` → fine, anything else off | off |
| `CRATONVM_PHASE_ACCOUNTING_OUT` | `DBG=phase-accounting-out` | non-empty path | none |
| `CRATONVM_PHASE_ACCOUNTING_JFR` | `DBG=phase-accounting-jfr` | non-empty path | none |

Already read through `runtime_var_os` and already named by `pub const FLAG_*`
items, so declaring them costs nothing and buys `CRATONVM_DBG=phase-accounting=fine`.
The enable gate is the fourth distinct "level word" parser in the tree and is
deliberately left in `jfr` — a `Level` enum is that crate's vocabulary, and the
typed config's job is to hand it the string, not to learn its grammar.

### Threading — `vm/src/threading/thread_state.rs`

`CRATONVM_STRESS_THREAD_STATES` → `THREADS=stress-thread-states`, tri-state,
`off_word: "0"`. Unset follows the build profile; `0`/`false`/`off` stands the
tripwire down even in a debug build (the bisection escape hatch for a wrong entry
in the legality table itself); anything else arms it. Note the parser is *not*
the same tri-state as the JIT one: here an unrecognised word means **on**, because
the read site is `Ok(_) => true`. Hence two parsers, `tristate_word` and
`tristate_off_word`, rather than one that is subtly wrong for one of them.

Arming the checks and making a violation fatal are the same variable but not the
same predicate — a debug build reports without aborting. `ThreadStressConfig`
keeps those as two accessors so the asymmetry cannot be flattened by accident.

### Capabilities — `native-api/src/capability.rs`

| Variable | Token | Parser | Default |
|---|---|---|---|
| `CRATONVM_CAPABILITY_MODE` | `SECURITY=capability-mode` | UTF-8 word, kept raw | permissive |
| `CRATONVM_CAPABILITY_GRANTS` | `SECURITY=capability-grants` | UTF-8, `;`-separated, kept raw | none |
| `CRATONVM_CAPABILITY_LOG` | `SECURITY=capability-log` | presence | off |

The mode word and the grant list stay **unparsed** in `CapabilityConfig`.
`CapabilityMode::parse` accepts operator aliases and prints a stderr warning for
an unrecognised word; that diagnostic belongs to `native-api`, and pushing the
parse down into `types` would either duplicate the alias table or move a
user-facing message into a crate with no business printing one.

One sharp edge, recorded at the inventory rows: `CRATONVM_SECURITY=all` writes
`1` into every token in the group, and `CapabilityMode::parse("1")` is
**`Enforce`**. `all` is not a safe way to "turn on the diagnostics" in this group.

### Class loading, natives, compatibility

| Variable | Token | Default | Note |
|---|---|---|---|
| `CRATONVM_LOADER_PARENT_CHAIN` | `LOADER=parent-chain` | **on** | empty string *and* `0` are off |
| `CRATONVM_CL_STUB_DELEGATION` | `LOADER=stub-delegation` | off | exactly `1` |
| `CRATONVM_JBOSS_LOGGER_LEVEL_FILTER` | `COMPAT=jboss-logger-level-filter` | **on** | off for the exact untrimmed `0` only |
| `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` | `SECURITY=noncrypto-sslengine` | off | re-enables the pre-hardening no-op `SSLEngine` |
| `CRATONVM_QUIET_ENV_FALLBACK` | `DBG=quiet-env-fallback` | off | silences a notice |

Plus the diagnostics `CRATONVM_DBG_CLASS_RESOURCE`, `CRATONVM_DBG_CLINIT_FAIL`,
`CRATONVM_DBG_JUL`, `CRATONVM_DBG_LINKAGE_BT`, `CRATONVM_DBG_LOADER_CHAIN`,
`CRATONVM_DBG_MAPPER`, `CRATONVM_DBG_RTERR`, `CRATONVM_DBG_SETACC`,
`CRATONVM_DBG_STAMPED`, `CRATONVM_DBG_STUB_BT`, `CRATONVM_DBG_STUBLOADER`,
`CRATONVM_DBG_AIO_INLINE`.

`CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` is worth one extra line: it was the only
undeclared flag in this batch that weakens a **security** control, and it was
reachable neither from `CRATONVM_SECURITY=…` nor from any test override.

---

## The typed configuration

`types/src/subsystem_config.rs` — `SubsystemConfig`, reached as
`flags().subsystems` or through the free accessors
`subsystem_config::{jit_verify, jit_metrics, gc_metrics, thread_stress,
capability}`.

```text
SubsystemConfig
├── jit_verify     : JitVerifyConfig     verify_ir: Option<bool>, 5 lane flags
├── jit_metrics    : JitMetricsConfig    enabled, out_path: Option<OsString>,
│                                        ring_capacity: Option<usize>
├── gc_metrics     : GcMetricsConfig     card_metrics
├── thread_stress  : ThreadStressConfig  thread_states: Option<bool>
└── capability     : CapabilityConfig    mode / grants (raw), log_first_use
```

Three properties this design is holding on to:

* **One snapshot.** `SubsystemConfig` is a field of `VmFlags`, so it is filled by
  the same single walk of `environ`, latched by the same `OnceLock`, and reached
  by the same `with_thread_overrides`. A separate `OnceLock` here would be a
  second thing to fall out of step with the first, which is the defect the whole
  refactor is undoing.
* **Additive.** Nothing was moved or renamed. Every `runtime_var` /
  `runtime_var_os` call site keeps working unchanged, so call sites can migrate
  one at a time and nothing outside `types/` has to change in lockstep.
* **The build profile is an argument, not a `cfg!`.** `per_pass_enabled(debug_build)`
  and `checks_enabled(debug_build)` take the caller's profile rather than
  evaluating `cfg!(debug_assertions)` inside `types`. `types` can be compiled
  under a different profile than `jit` or `vm`, and silently answering for the
  wrong one is precisely the class of divergence this exists to remove.

---

## Migration plan

Ordered so that each step is independently landable and independently revertible,
and so the two steps that *change behaviour* come first and separately.

### Stage 1 — the 17 direct `std::env` reads in runtime crates (behaviour fix)

These bypass the snapshot entirely, so they are invisible to the override hooks
and to `CRATONVM_<GROUP>=…`. Each is a one-line change from `std::env::var[_os]`
to `cratonvm_types::flags::runtime_var[_os]`; the parse around it does not move.
`tools/flag-census/check-surface.sh` check 4 already bans these, and is currently
red because of them.

| # | Site | Variable |
|---|---|---|
| 1 | `native-builtins/src/tls.rs` → `noncrypto_sslengine_allowed` | `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` |
| 2 | `classloading/src/loaders.rs` → `loader_parent_chain_enabled` | `CRATONVM_LOADER_PARENT_CHAIN` |
| 3 | `native-builtins/src/logmanager.rs` → `jboss_logger_level_filter` | `CRATONVM_JBOSS_LOGGER_LEVEL_FILTER` |
| 4 | `native-builtins/src/classloader_real.rs` → the `CL_STUB_DELEGATION` gate | `CRATONVM_CL_STUB_DELEGATION` |
| 5 | `native-builtins/src/spring_startup_bootstrap.rs` → the env-fallback notice | `CRATONVM_QUIET_ENV_FALLBACK` |
| 6 | `vm/src/vm/vm_exec.rs` → the interrupt trace | `CRATONVM_DBG_INTERRUPT` — **already declared**, still live-read |
| 7 | `jit/src/lib.rs` ×2 → the compiled-method trace | `CRATONVM_DBG_JIT_COMPILED` — **already declared**, still live-read |
| 8 | `classloading/src/loaders.rs` → `dbg_loader_chain` | `CRATONVM_DBG_LOADER_CHAIN` |
| 9 | `classloading/src/class_manager.rs` → the stub-backtrace filter | `CRATONVM_DBG_STUB_BT` |
| 10 | `native-builtins/src/lang_class.rs` ×2 | `CRATONVM_DBG_CLASS_RESOURCE`, `CRATONVM_DBG_SETACC` |
| 11 | `vm/src/runtime/exceptions.rs` ×2 | `CRATONVM_DBG_RTERR`, `CRATONVM_DBG_LINKAGE_BT` |
| 12 | `vm/src/vm/vm_util.rs` → the clinit-failure trace | `CRATONVM_DBG_CLINIT_FAIL` |
| 13 | `gc/src/gen_heap.rs` ×2 | `CRATONVM_DBG_YOUNG_TRIGGER`, `CRATONVM_DBG_OOM_BT` |
| 14 | `cuda-bridge/src/critical.rs` → `resolve_ms` | `CRATONVM_GPU_CRITICAL_WAIT_MS`, `CRATONVM_GPU_CRITICAL_LEASE_MS` |

Rows 6 and 7 are the interesting ones: those flags were *already declared*, so a
test arranging them through `with_thread_overrides` was already silently
ineffective. Nothing about the declaration protects a call site that does not use
the boundary.

Row 14 is the only one that takes more than a word change: `resolve_ms` receives
the variable name as a `&str` parameter, so the swap is `std::env::var(var)` →
`cratonvm_types::flags::runtime_var(var)` — one token, but it also removes the
last CratonVM read in the tree that no static scan can see.

### Stage 2 — the launcher, `vm-cli/src/main.rs` (20 sites)

All 20 name declared flags and all 20 read `std::env` directly.
`CRATONVM_DBG_ARGS` alone accounts for 7. This is a lower-severity group — the
launcher runs before the snapshot latches and calls
`flag_groups::expand_process_env` — but "reads the same variable twice through
two different mechanisms" is how the three-way `CRATONVM_LOADER_AWARE_RESOLUTION`
split happened. Convert wholesale in one commit, after Stage 1 so a bisect can
tell the two apart.

### Stage 3 — the five subsystems, onto the typed config

Purely a typing change: these already read through `runtime_var[_os]`, so the
value they see does not move. Do them one subsystem per commit, smallest first.

| Order | Subsystem | Replace | With |
|---|---|---|---|
| 1 | `gc/src/gc_metrics.rs` → `resolve_hot_path_gate` | `runtime_var_os("CRATONVM_GC_CARD_METRICS").is_some()` | `subsystem_config::gc_metrics().card_metrics` |
| 2 | `vm/src/threading/thread_state.rs` → `stress_checks_enabled`, `violations_are_fatal` | two `runtime_var` matches | `thread_stress().checks_enabled(cfg!(debug_assertions))` / `.violations_are_fatal()` |
| 3 | `native-api/src/capability.rs` → `CapabilityMode::from_env`, `CapabilitySet::new`/`from_env` | three reads | `capability().mode_word()` / `.grant_list()` / `.log_first_use` |
| 4 | `jit/src/metrics.rs` → `enabled`, `ring_capacity`, `json_sink` | the private `env_flag` + two reads | `jit_metrics().enabled` / `.ring_capacity_or(DEFAULT_RING_CAPACITY)` / `.out_path()` |
| 5 | `jit/src/ir_verify.rs` → `VerifyOptions::from_env`, `verify_enabled`, `pre_lower_verify_disabled` | the private `env_flag` + five reads | `jit_verify()` and its two accessors |

Steps 4 and 5 delete the two duplicated `env_flag` helpers. They are today
byte-identical copies of each other and of `parse::tristate_word`, kept apart by
a comment explaining that the two modules must be able to disagree about
*defaults* — which they still can, because the default is applied at the accessor
(`per_pass_enabled(debug_build)` vs `unwrap_or(false)`), not at the parse.

### Stage 4 — the long tail

421 `runtime_var[_os]` sites remain, ~90 of them memoised on top by
`vm/src/runtime/env_cache.rs`. These are correct as they stand: they resolve
against the snapshot, they honour the override hooks via `MemoSlot`, and the
memo layer is load-bearing for interpreter throughput (`MemoSlot`'s doc records
+1.15% of `execute_instruction` for the naive alternative). Migrate opportunistically,
when a flag is being touched for another reason and its `bool` is hiding
structure. Do **not** do it as a sweep: the win is typing, and the risk is a
per-bytecode read that got slower.

---

## The allowlist, and why each row is on it

`types/tests/flag_declaration_guard.rs` fails the build when a `CRATONVM_*`
string literal appears anywhere in the workspace without a declaration. Eleven
names are exempt. Every row carries a reason string, and
`the_allowlist_has_no_dead_rows` deletes the possibility of a row outliving its
call site — an allowlist nobody prunes is how the previous surface reached 692
names.

Three kinds, and nothing else qualifies:

**Kind 1 — not an environment variable at all.**

| Name | Why |
|---|---|
| `CRATONVM_COMPATIBILITY_JDK_ONLY` | a `libcratonvm` C ABI integer constant; it is matched only because a unit test asserts the diagnostic message names it |
| `CRATONVM_REAL_RAF` | a retired gate. Real-RAF is the default and the opt-out is `CRATONVM_SYNTHETIC_RAF`; the only surviving mention is the `env_remove` baseline list in `vm/tests/synthetic_diff.rs` |

**Kind 2 — deliberate "this variable does not exist" probes.**

| Name | Why |
|---|---|
| `CRATONVM_NONEXISTENT_VAR_12345` | `vm.rs`'s `System.getenv` coverage needs a name guaranteed absent |
| `CRATONVM_SOMETHING_BRAND_NEW` | `flag_groups.rs` proves an unknown key falls through `resolve` unchanged, which requires a key no entry claims |

**Kind 3 — test-harness and build-script knobs no production code reads.** These
are read before or outside a VM, where no snapshot exists and live `std::env`
semantics are the correct ones. Declaring them would put harness plumbing into
`docs/CONFIG.md` and into `CRATONVM_TEST=…`, which is the surface growth this
exercise is undoing.

| Name | Read by |
|---|---|
| `CRATONVM_DIFF_HOTSPOT` | `vm/tests/jit_interp_differential.rs` — run the reference JVM |
| `CRATONVM_FUZZ_BOOTCP` | `fuzz/fuzz_targets/fuzz_verifier.rs` — boot classpath for a `cargo fuzz` target |
| `CRATONVM_REGEN_HEADER` | `libcratonvm/build.rs` — re-run cbindgen; a build script is a different process |
| `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS` | `vm/tests/interpreter_tests.rs` |
| `CRATONVM_SPRING_BOOT_FATJAR` | `vm/tests/wave3_spring_boot_fatjar.rs` — path to a fixture jar not checked in |
| `CRATONVM_TEST_CLASSES_DIR` | `vm/build.rs` via `cargo:rustc-env`, read with `option_env!` — a compile-time constant, not a runtime flag |
| `CRATONVM_TEST_JAVA_HOME` | `vm/tests/*` — a JDK to shell out to, checked ahead of `JAVA_HOME` |

A flag read by anything under a crate's `src/` does not qualify. If a row is
added, the reason has to say why the snapshot cannot serve that call site;
"it was easier" is not a reason.

### How the scanner stays precise

Only an **exact whole-string literal** counts — the closing quote must follow the
name immediately. That is what keeps the guard off `b"CRATONVM_MODULES_MARKER\n"`
(a file's contents), `"[CRATONVM_STREAM_PIN_CANARY] {site}: …"` (a log tag), and
`"CRATONVM_DBG=jit-method-stats"` (advice telling an operator what to export).
Whole-line comments are skipped, so the hundreds of `/// CRATONVM_X=1 does …`
doc comments cost nothing.

One trap, found while writing the guard and now pinned by a unit test: the
obvious spelling of "is this a block-comment continuation" is
`line.trim_start().starts_with('*')`, and it is wrong. This tree is full of
`*ON.get_or_init(|| std::env::var("CRATONVM_…"))`, and that rule swallows every
one — a guard that skips exactly the once-cached flag reads it exists to find.
The rule is `"* "`, `"*/"`, or a bare `*` instead.

Known blind spot, stated rather than papered over: a name assembled at runtime
(`format!("CRATONVM_REAL_{sub}")`) cannot be judged statically. The same caveat
applies to `flag_env_mutation_guard.rs`.

---

## Open reconciliation items

1. **`tools/flag-census/check-surface.sh` needs one exemption.** Its check 1
   excludes `CRATONVM_(NONEXISTENT_VAR_12345|SOMETHING_BRAND_NEW|FOO)`. After
   this pass the only remaining undeclared literal under a crate `src/` is
   `CRATONVM_COMPATIBILITY_JDK_ONLY` at `libcratonvm/src/lib.rs`, which is an ABI
   constant name inside an assertion. Add it to that `grep -vxE` alternation.
2. **`check-surface.sh` check 4 is red** — 17 direct `std::env::var[_os]` calls
   on `CRATONVM_*` literals in core runtime crates, plus three on non-CratonVM
   names (`HOME` in `native-awt`, `JBOSS_HOME` in `native-builtins`, a dynamic
   key in `vm/src/config.rs`). Stage 1 fixes the 17. The three others are
   *correct* — an OS variable should keep live semantics — but check 4 bans the
   call shape, not the flag, so they need either a routing through
   `runtime_var_os` (which passes non-declared names straight through, so it is
   behaviour-neutral) or an explicit exemption in the script. Routing is the
   better answer: it makes the boundary the only door.
3. **`docs/CONFIG.md` and `docs/flag-tokens.md` do not yet list the 71 new
   tokens.** `check-surface.sh` check 2 only fails when the *docs* name a token
   the inventory lacks, not the reverse, so this is not blocking — but the
   documented surface is now 71 tokens behind the code.
4. **Two `env_flag` helpers survive** in `jit/src/ir_verify.rs` and
   `jit/src/metrics.rs`. They are byte-identical to each other and to
   `parse::tristate_word`. Stage 3 steps 4–5 remove them.
5. **`jit/`, `vm/`, `cuda-bridge/` and `jfr/` were changing while this
   inventory was taken.** Eight variables appeared mid-pass and are declared:
   `CRATONVM_JIT_VERIFY_MEMORY_CHAIN`, `CRATONVM_JIT_VERIFY_ARENA_ORDER`,
   `CRATONVM_DBG_IR_SLOTS`, `CRATONVM_GPU_CRITICAL_WAIT_MS`,
   `CRATONVM_GPU_CRITICAL_LEASE_MS`, `CRATONVM_PHASE_ACCOUNTING`,
   `CRATONVM_PHASE_ACCOUNTING_OUT`, `CRATONVM_PHASE_ACCOUNTING_JFR`.
   Line numbers in this document will drift;
   the file and function names will not. Re-run
   `cargo test -p cratonvm-types --test flag_declaration_guard` after merging —
   it is the cheapest way to find what landed in the meantime.
6. **`types/src/subsystem_config.rs` models `jit::ir_verify` as it stood at the
   end of this pass**, including the `check_schedule` → `check_memory_chain` +
   `check_arena_order` split. If `jit` moves again before the migration lands,
   reconcile `JitVerifyConfig` against `VerifyOptions::from_env` — the two must
   agree field for field or Stage 3 step 5 will silently change which lanes run.

---

## Full inventory

925 rows: 914 declared, 11 allowlisted. Generated by
`tools/flag-census/render-inventory.py` — see
[How to regenerate](#how-to-regenerate).

| Variable | Group | Canonical spelling | Shape | Default | Class | Latched | Read in |
|---|---|---|---|---|---|---|---|
| `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE` | DBG | `CRATONVM_DBG=active-profiles-identity-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_ALLOW_JSR_RET` | LOADER | `CRATONVM_LOADER=allow-jsr-ret` | opt-in | off | behaviour | snapshot | classloading, types |
| `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` | SECURITY | `CRATONVM_SECURITY=noncrypto-sslengine` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_ANN_PROXY_DISPATCH_TRACE` | DBG | `CRATONVM_DBG=ann-proxy-dispatch-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_ANN_TRACE` | DBG | `CRATONVM_DBG=ann-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_AOT_HMAC_KEY` | SECURITY | `CRATONVM_SECURITY=aot-hmac-key` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_ASSERT_SINGLE_OS_THREAD` | THREADS | `CRATONVM_THREADS=assert-single-os-thread` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | THREADS | `CRATONVM_THREADS=async-handoff-sleep-floor-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | THREADS | `CRATONVM_THREADS=async-submit-grace-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | THREADS | `CRATONVM_THREADS=async-worker-sleep-floor-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | THREADS | `CRATONVM_THREADS=await-shortcircuit` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_BD_DEBUG` | DBG | `CRATONVM_DBG=bd-debug` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_BG_COMPILE` | JIT | `CRATONVM_JIT=bg-compile` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_BIN` | — | `CRATONVM_BIN` | scalar | unset | behaviour | snapshot | difftest |
| `CRATONVM_BLOCK_PRIVATE_NETS` | SECURITY | `CRATONVM_SECURITY=block-private-nets` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_BOOT_MODULE_REGISTRY` | LOADER | `CRATONVM_LOADER=boot-module-registry` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_BYTEBUFFER_INTRINSIC` | REAL | `CRATONVM_REAL=bytebuffer-intrinsic` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_C2_SUPERSEDE` | JIT | `CRATONVM_JIT=c2-supersede` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_CANON_OPENFILE` | IO | `CRATONVM_IO=canon-openfile` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CAPABILITY_GRANTS` | SECURITY | `CRATONVM_SECURITY=capability-grants` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CAPABILITY_LOG` | SECURITY | `CRATONVM_SECURITY=capability-log` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CAPABILITY_MODE` | SECURITY | `CRATONVM_SECURITY=capability-mode` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CARD_TABLE_ONLY` | GC | `CRATONVM_GC=card-table-only` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CENSUS_EXACT_INVOCATIONS` | DBG | `CRATONVM_DBG=census-exact-invocations` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_CF_DELEGATING_YIELD` | LOADER | `CRATONVM_LOADER=cf-delegating-yield` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_CL_BOOTSTRAP_SCOPED` | LOADER | `CRATONVM_LOADER=cl-bootstrap-scoped` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_CL_STUB_DELEGATION` | LOADER | `CRATONVM_LOADER=stub-delegation` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_COMPACT_REF_FIELDS` | GC | `CRATONVM_GC=compact-ref-fields` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_COMPAT` | COMPAT | `CRATONVM_COMPAT=…` | group | unset | — | snapshot | — |
| `CRATONVM_COMPATIBILITY_JDK_ONLY` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | libcratonvm C ABI constant, not a variable |
| `CRATONVM_COMPRESSED_OOPS` | GC | `CRATONVM_GC=compressed-oops` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_CONFINE_IO` | SECURITY | `CRATONVM_SECURITY=confine-io` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_DBG` | DBG | `CRATONVM_DBG=…` | group | unset | — | snapshot | native-builtins, types |
| `CRATONVM_DBG_A2` | DBG | `CRATONVM_DBG=a2` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_A5_CENSUS` | DBG | `CRATONVM_DBG=a5-census` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_A5_ENGAGEMENT` | DBG | `CRATONVM_DBG=a5-engagement` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ACCESS` | DBG | `CRATONVM_DBG=access` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_AIO` | DBG | `CRATONVM_DBG=aio` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_AIOOBE` | DBG | `CRATONVM_DBG=aioobe` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_AIOOBE2` | DBG | `CRATONVM_DBG=aioobe2` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_AIOOBE3` | DBG | `CRATONVM_DBG=aioobe3` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_AIO_INLINE` | DBG | `CRATONVM_DBG=aio-inline` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_ALTRACE` | DBG | `CRATONVM_DBG=altrace` | opt-in | off | diag | snapshot | native-collections, vm |
| `CRATONVM_DBG_ANNPROXY_WRAP` | DBG | `CRATONVM_DBG=annproxy-wrap` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ANN_PROXY_PROF` | DBG | `CRATONVM_DBG=ann-proxy-prof` | opt-in | off | diag | snapshot | native-builtins, vm |
| `CRATONVM_DBG_ANONALLOC` | DBG | `CRATONVM_DBG=anonalloc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_AQS_TRACE` | DBG | `CRATONVM_DBG=aqs-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ARGS` | DBG | `CRATONVM_DBG=args` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_DBG_ARRAYCOPY` | DBG | `CRATONVM_DBG=arraycopy` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ARRLEN` | DBG | `CRATONVM_DBG=arrlen` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ARRSTORE` | DBG | `CRATONVM_DBG=arrstore` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ASSERTEQ` | DBG | `CRATONVM_DBG=asserteq` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ASSERTJ_ARR` | DBG | `CRATONVM_DBG=assertj-arr` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ATHROW` | DBG | `CRATONVM_DBG=athrow` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ATOMIC_INTRINSIC` | DBG | `CRATONVM_DBG=atomic-intrinsic` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_ATOMIC_UPDATER` | DBG | `CRATONVM_DBG=atomic-updater` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_BADRECV` | DBG | `CRATONVM_DBG=badrecv` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_BADREF` | DBG | `CRATONVM_DBG=badref` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_BB` | DBG | `CRATONVM_DBG=bb` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_BBLP` | DBG | `CRATONVM_DBG=bblp` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_BLOCKED_ACCESS` | DBG | `CRATONVM_DBG=blocked-access` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_BLOCKGC` | DBG | `CRATONVM_DBG=blockgc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_BUFUNDER` | DBG | `CRATONVM_DBG=bufunder` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_BUG03` | DBG | `CRATONVM_DBG=bug03` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_BYTECODE_DUMP` | DBG | `CRATONVM_DBG=bytecode-dump` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CALLEE_DEOPT` | DBG | `CRATONVM_DBG=callee-deopt` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CALLEE_PROBE` | DBG | `CRATONVM_DBG=callee-probe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CALLER` | DBG | `CRATONVM_DBG=caller` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CAPVAL` | DBG | `CRATONVM_DBG=capval` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CATALINA` | DBG | `CRATONVM_DBG=catalina` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_CAUSE` | DBG | `CRATONVM_DBG=cause` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CCE` | DBG | `CRATONVM_DBG=cce` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CCECACHE` | DBG | `CRATONVM_DBG=ccecache` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CCE_BT` | DBG | `CRATONVM_DBG=cce-bt` | opt-in | off | diag | snapshot | native-collections, types, vm |
| `CRATONVM_DBG_CCSPROBE` | DBG | `CRATONVM_DBG=ccsprobe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CELLCORRUPT` | DBG | `CRATONVM_DBG=cellcorrupt` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CHARSET` | DBG | `CRATONVM_DBG=charset` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CHECK_OVERRIDE` | DBG | `CRATONVM_DBG=check-override` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CLASSPATH` | DBG | `CRATONVM_DBG=classpath` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CLASS_RESOURCE` | DBG | `CRATONVM_DBG=class-resource` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_CLINIT_FAIL` | DBG | `CRATONVM_DBG=clinit-fail` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CLINIT_ORDER` | DBG | `CRATONVM_DBG=clinit-order` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CLONE` | DBG | `CRATONVM_DBG=clone` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COERCE` | DBG | `CRATONVM_DBG=coerce` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COERCION` | DBG | `CRATONVM_DBG=coercion` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_COMPACTVALUE` | DBG | `CRATONVM_DBG=compactvalue` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COMPACT_INLINE` | DBG | `CRATONVM_DBG=compact-inline` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_COMPACT_LEGACY` | DBG | `CRATONVM_DBG=compact-legacy` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COMPONENT_TYPE` | DBG | `CRATONVM_DBG=component-type` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CORRUPT_CELL` | DBG | `CRATONVM_DBG=corrupt-cell` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_CORRUPT_CELL_SELFTEST` | DBG | `CRATONVM_DBG=corrupt-cell-selftest` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CORRUPT_FRAMES` | DBG | `CRATONVM_DBG=corrupt-frames` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CTOR_FIX` | DBG | `CRATONVM_DBG=ctor-fix` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DBB_ELEM` | DBG | `CRATONVM_DBG=dbb-elem` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_DEFINE` | DBG | `CRATONVM_DBG=define` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DEFINE_CENSUS` | DBG | `CRATONVM_DBG=define-census` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DEFINE_FILTER` | DBG | `CRATONVM_DBG=define-filter` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_DBG_DEFINE_STACK_FILTER` | DBG | `CRATONVM_DBG=define-stack-filter` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_DEFLATE` | DBG | `CRATONVM_DBG=deflate` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DEOPT` | DBG | `CRATONVM_DBG=deopt` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_DBG_DEOPTSLOT` | DBG | `CRATONVM_DBG=deoptslot` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DESCTRACE` | DBG | `CRATONVM_DBG=desctrace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DIAL_DOORS` | DBG | `CRATONVM_DBG=dial-doors` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DISPATCH_TALLY` | DBG | `CRATONVM_DBG=dispatch-tally` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DM` | DBG | `CRATONVM_DBG=direct-memory` | opt-in | off | diag | snapshot | gc, native-io, vm |
| `CRATONVM_DBG_DOPRIV` | DBG | `CRATONVM_DBG=dopriv` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DROPPED_STUBS` | DBG | `CRATONVM_DBG=dropped-stubs` | opt-in | off | diag | snapshot | native-api |
| `CRATONVM_DBG_DUMP_JIT` | DBG | `CRATONVM_DBG=dump-jit` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_DUPCALL_FILTER` | DBG | `CRATONVM_DBG=dupcall-filter` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DUPCLASS` | DBG | `CRATONVM_DBG=dupclass` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DUPCLASS_BT` | DBG | `CRATONVM_DBG=dupclass-bt` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DUPCLASS_FILTER` | DBG | `CRATONVM_DBG=dupclass-filter` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DUPDEF` | DBG | `CRATONVM_DBG=dupdef` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_DUPX_METHODS` | DBG | `CRATONVM_DBG=dupx-methods` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_DUPX_TRACE` | DBG | `CRATONVM_DBG=dupx-trace` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_ECWATCH` | DBG | `CRATONVM_DBG=ecwatch` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ECWATCH_NATIVE` | DBG | `CRATONVM_DBG=ecwatch-native` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_EINTR_INJECT` | DBG | `CRATONVM_DBG=eintr-inject` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EINTR_NO_RETRY` | DBG | `CRATONVM_DBG=eintr-no-retry` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EQE` | DBG | `CRATONVM_DBG=eqe` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EXCFRAME` | DBG | `CRATONVM_DBG=excframe` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_EXEC` | DBG | `CRATONVM_DBG=exec` | opt-in | off | diag | snapshot | native-collections, types |
| `CRATONVM_DBG_EXIT` | DBG | `CRATONVM_DBG=exit` | opt-in | off | diag | snapshot | types, vm-cli |
| `CRATONVM_DBG_FBCGLIB` | DBG | `CRATONVM_DBG=fbcglib` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FBREF` | DBG | `CRATONVM_DBG=fbref` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FIELDADDR` | DBG | `CRATONVM_DBG=fieldaddr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FIELD_GET` | DBG | `CRATONVM_DBG=field-get` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FIELD_SITE` | DBG | `CRATONVM_DBG=field-site` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FIELD_WATCH` | DBG | `CRATONVM_DBG=field-watch` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_FMT_WRONGTYPE` | DBG | `CRATONVM_DBG=fmt-wrongtype` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_FORCE_MOVING` | DBG | `CRATONVM_DBG=force-moving` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_FSP` | DBG | `CRATONVM_DBG=fsp` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FULLSTACK_SCAN` | DBG | `CRATONVM_DBG=fullstack-scan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FWDGUARD` | DBG | `CRATONVM_DBG=fwdguard` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FWDWALK` | DBG | `CRATONVM_DBG=fwdwalk` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_G1ACCESSOR` | DBG | `CRATONVM_DBG=g1accessor` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_G1DIAG` | DBG | `CRATONVM_DBG=g1diag` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_G1_LIVE_MEMO` | DBG | `CRATONVM_DBG=g1-live-memo` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_GCPART` | DBG | `CRATONVM_DBG=gcpart` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_GCPAUSE` | DBG | `CRATONVM_DBG=gcpause` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GCPHASE` | DBG | `CRATONVM_DBG=gcphase` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GCWRITE` | DBG | `CRATONVM_DBG=gcwrite` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GC_FALLBACK_REASONS` | DBG | `CRATONVM_DBG=gc-fallback-reasons` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_GC_OVERHEAD` | DBG | `CRATONVM_DBG=gc-overhead` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_GC_STRESS` | DBG | `CRATONVM_DBG=gc-stress` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GDM_PROF` | DBG | `CRATONVM_DBG=gdm-prof` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_GETFIELD_RECEIVERS` | DBG | `CRATONVM_DBG=getfield-receivers` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_GETRESOURCES` | DBG | `CRATONVM_DBG=getresources` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GETSTATIC_PROF` | DBG | `CRATONVM_DBG=getstatic-prof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_GOCBF` | DBG | `CRATONVM_DBG=gocbf` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GSE` | DBG | `CRATONVM_DBG=gse` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_H2PARSERREAD` | DBG | `CRATONVM_DBG=h2parserread` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_H2TRACE` | DBG | `CRATONVM_DBG=h2trace` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_HANGWALK` | DBG | `CRATONVM_DBG=hangwalk` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_HANG_SAMPLE` | DBG | `CRATONVM_DBG=hang-sample` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_HEAPCOPY` | DBG | `CRATONVM_DBG=heapcopy` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_HEAP_STALE` | DBG | `CRATONVM_DBG=heap-stale` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_HEAP_TRACE` | DBG | `CRATONVM_DBG=heap-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_HEARTBEAT` | DBG | `CRATONVM_DBG=heartbeat` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_HMINIT_PURGE` | DBG | `CRATONVM_DBG=hminit-purge` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_HMPUT` | DBG | `CRATONVM_DBG=hmput` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_HOTPATH_COUNTS` | DBG | `CRATONVM_DBG=hotpath-counts` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_HTTPSRV` | DBG | `CRATONVM_DBG=httpsrv` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_HW_ATOMIC` | DBG | `CRATONVM_DBG=hw-atomic` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_IMSE` | DBG | `CRATONVM_DBG=imse` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_INDY_ALL` | DBG | `CRATONVM_DBG=indy-all` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_INDY_GENERIC` | DBG | `CRATONVM_DBG=indy-generic` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_INLINE_FR` | DBG | `CRATONVM_DBG=inline-fr` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_INTERRUPT` | DBG | `CRATONVM_DBG=interrupt` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_INTRINSIC` | DBG | `CRATONVM_DBG=intrinsic` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_INVOKESTATS` | DBG | `CRATONVM_DBG=invokestats` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_INVOKE_COERCE` | DBG | `CRATONVM_DBG=invoke-coerce` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_INVOKE_PHASES` | DBG | `CRATONVM_DBG=invoke-phases` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_INVSPECIAL` | DBG | `CRATONVM_DBG=invspecial` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_IRSLOT` | DBG | `CRATONVM_DBG=irslot` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_BAILOUT` | DBG | `CRATONVM_DBG=ir-bailout` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_BUFSIZE` | DBG | `CRATONVM_DBG=ir-bufsize` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_CALL` | DBG | `CRATONVM_DBG=ir-call` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_COMPILES` | DBG | `CRATONVM_DBG=ir-compiles` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_ISEL` | DBG | `CRATONVM_DBG=ir-isel` | opt-in | off | diag | snapshot | jit, vm-cli |
| `CRATONVM_DBG_IR_LINEAR_SCAN` | DBG | `CRATONVM_DBG=ir-linear-scan` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_LONG` | DBG | `CRATONVM_DBG=ir-long` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_RELOC` | DBG | `CRATONVM_DBG=ir-reloc` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_SLOTS` | DBG | `CRATONVM_DBG=ir-slots` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_ISINSTANCE` | DBG | `CRATONVM_DBG=isinstance` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ISOLATED_CNF` | DBG | `CRATONVM_DBG=isolated-cnf` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JAR` | DBG | `CRATONVM_DBG=jar` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_JCA_GETINSTANCE` | DBG | `CRATONVM_DBG=jca-getinstance` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_JETTY` | DBG | `CRATONVM_DBG=jetty` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_JETTY2` | DBG | `CRATONVM_DBG=jetty2` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JITC` | DBG | `CRATONVM_DBG=jitc` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JIT_ALLOC` | DBG | `CRATONVM_DBG=jit-alloc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_BORROW_SITES` | DBG | `CRATONVM_DBG=jit-borrow-sites` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_CODE` | DBG | `CRATONVM_DBG=jit-code` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JIT_CODE_FREE` | DBG | `CRATONVM_DBG=jit-code-free` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_COMPILED` | DBG | `CRATONVM_DBG=jit-compiled` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_DISASM` | DBG | `CRATONVM_DBG=jit-disasm` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_DISPATCH` | DBG | `CRATONVM_DBG=jit-dispatch` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_ENTRY` | DBG | `CRATONVM_DBG=jit-entry` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_GEN` | DBG | `CRATONVM_DBG=jit-gen` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_LDC` | DBG | `CRATONVM_DBG=jit-ldc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_METHOD_STATS` | DBG | `CRATONVM_DBG=jit-method-stats` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_DBG_JIT_MIC` | DBG | `CRATONVM_DBG=jit-mic` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_NAMES` | DBG | `CRATONVM_DBG=jit-names` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JIT_PIN` | DBG | `CRATONVM_DBG=jit-pin` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_PUTFIELD` | DBG | `CRATONVM_DBG=jit-putfield` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_ROOTSCAN` | DBG | `CRATONVM_DBG=jit-rootscan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_SAFEPOINTS` | DBG | `CRATONVM_DBG=jit-safepoints` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_SCAN_PROF` | DBG | `CRATONVM_DBG=jit-scan-prof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_STALE_AFTER_REMAP` | DBG | `CRATONVM_DBG=jit-stale-after-remap` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_JIT_STALE_BELOW_RBP` | DBG | `CRATONVM_DBG=jit-stale-below-rbp` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_STALE_IC` | DBG | `CRATONVM_DBG=jit-stale-ic` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_UNMAP` | DBG | `CRATONVM_DBG=jit-unmap` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JLM` | DBG | `CRATONVM_DBG=jlm` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_JUL` | DBG | `CRATONVM_DBG=jul` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_KCBOOL` | DBG | `CRATONVM_DBG=kcbool` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_LAMBDA` | DBG | `CRATONVM_DBG=lambda` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LAMBDA_DISPATCH` | DBG | `CRATONVM_DBG=lambda-dispatch` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LAMBDA_GENERIC` | DBG | `CRATONVM_DBG=lambda-generic` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LAMBDA_JIT` | DBG | `CRATONVM_DBG=lambda-jit` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LAMBDA_PROF` | DBG | `CRATONVM_DBG=lambda-prof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LAYOUT` | DBG | `CRATONVM_DBG=layout` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LAYOUT_ALIAS` | DBG | `CRATONVM_DBG=layout-alias` | opt-in | off | diag | snapshot | native-api |
| `CRATONVM_DBG_LETSGO` | DBG | `CRATONVM_DBG=letsgo` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LHM_EVICT` | DBG | `CRATONVM_DBG=lhm-evict` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_LICM` | DBG | `CRATONVM_DBG=licm` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_LINKAGE` | DBG | `CRATONVM_DBG=linkage` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LINKAGE_BT` | DBG | `CRATONVM_DBG=linkage-bt` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LINKER` | DBG | `CRATONVM_DBG=linker` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LOADCLASS` | DBG | `CRATONVM_DBG=loadclass` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LOADER_CHAIN` | DBG | `CRATONVM_DBG=loader-chain` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_DBG_LOADER_TRACE` | DBG | `CRATONVM_DBG=loader-trace` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_LOAD_TRANSFORM_NO_MEMO` | DBG | `CRATONVM_DBG=load-transform-no-memo` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LOGPROV` | DBG | `CRATONVM_DBG=logprov` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LONGROOT` | DBG | `CRATONVM_DBG=longroot` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LOOKUP` | DBG | `CRATONVM_DBG=lookup` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LOOP_WORK` | DBG | `CRATONVM_DBG=loop-work` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MAPGEN` | DBG | `CRATONVM_DBG=mapgen` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MAPPER` | DBG | `CRATONVM_DBG=mapper` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_MAP_MISS_AUDIT` | DBG | `CRATONVM_DBG=map-miss-audit` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_MAP_VIEW_CACHE` | DBG | `CRATONVM_DBG=map-view-cache` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_MARK_WHY_CLASS` | DBG | `CRATONVM_DBG=mark-why-class` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MCL` | DBG | `CRATONVM_DBG=mcl` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MEMWATCH` | DBG | `CRATONVM_DBG=memwatch` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_METHOD_INVOKE_BOX` | DBG | `CRATONVM_DBG=method-invoke-box` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MH_ADAPTER` | DBG | `CRATONVM_DBG=mh-adapter` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MH_DISPATCH` | DBG | `CRATONVM_DBG=mh-dispatch` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MH_STACK` | DBG | `CRATONVM_DBG=mh-stack` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MIC_METHOD` | DBG | `CRATONVM_DBG=mic-method` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MIC_PROF` | DBG | `CRATONVM_DBG=mic-prof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MIC_TRACE` | DBG | `CRATONVM_DBG=mic-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MINVOKE` | DBG | `CRATONVM_DBG=minvoke` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MIRRORPIN` | DBG | `CRATONVM_DBG=mirrorpin` | opt-in | off | diag | snapshot | native-collections, types, vm |
| `CRATONVM_DBG_MIRRORPIN_WHY` | DBG | `CRATONVM_DBG=mirrorpin-why` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MODPROV` | DBG | `CRATONVM_DBG=modprov` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MODSTATIC` | DBG | `CRATONVM_DBG=modstatic` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MONENTER` | DBG | `CRATONVM_DBG=monenter` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MONEXIT` | DBG | `CRATONVM_DBG=monexit` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MONITOR_NOTIFY` | DBG | `CRATONVM_DBG=monitor-notify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MSC` | DBG | `CRATONVM_DBG=msc` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MTROOTS` | DBG | `CRATONVM_DBG=mtroots` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK` | DBG | `CRATONVM_DBG=nativelibraries-load-ok` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_NATIVE_LOOKUPS` | DBG | `CRATONVM_DBG=native-lookups` | opt-in | off | diag | snapshot | native-api |
| `CRATONVM_DBG_NCDFE` | DBG | `CRATONVM_DBG=ncdfe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NET` | DBG | `CRATONVM_DBG=net` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_NETTY_QUEUE` | DBG | `CRATONVM_DBG=netty-queue` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_NEXTINT` | DBG | `CRATONVM_DBG=nextint` | opt-in | off | diag | snapshot | native-collections, types |
| `CRATONVM_DBG_NIO_BIND` | DBG | `CRATONVM_DBG=nio-bind` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_NOCODE` | DBG | `CRATONVM_DBG=nocode` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NO_CLEANERS` | DBG | `CRATONVM_DBG=cleaners` | opt-out | on | diag | snapshot | vm |
| `CRATONVM_DBG_NO_NONMOVING_RECLAIM` | DBG | `CRATONVM_DBG=nonmoving-reclaim` | opt-out | on | diag | snapshot | types |
| `CRATONVM_DBG_NO_PRUNE` | DBG | `CRATONVM_DBG=prune` | opt-out | on | diag | snapshot | vm |
| `CRATONVM_DBG_NO_REFPROC` | DBG | `CRATONVM_DBG=refproc` | opt-out | on | diag | snapshot | vm |
| `CRATONVM_DBG_NPE_INVOKE` | DBG | `CRATONVM_DBG=npe-invoke` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NPE_MATCH` | DBG | `CRATONVM_DBG=npe-match` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NPE_NONE` | DBG | `CRATONVM_DBG=npe-none` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NPE_STACK` | DBG | `CRATONVM_DBG=npe-stack` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NPE_TRACE` | DBG | `CRATONVM_DBG=npe-trace` | opt-in | off | diag | snapshot | native-builtins, vm |
| `CRATONVM_DBG_NSME` | DBG | `CRATONVM_DBG=nsme` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NULLTHIS` | DBG | `CRATONVM_DBG=nullthis` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NULL_NATIVE` | DBG | `CRATONVM_DBG=null-native` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OBJECTS` | DBG | `CRATONVM_DBG=objects` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OBJKEY` | DBG | `CRATONVM_DBG=objkey` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_OBJ_EQUALS` | DBG | `CRATONVM_DBG=obj-equals` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OBSREG` | DBG | `CRATONVM_DBG=obsreg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OLDSWEEP_OWNERS` | DBG | `CRATONVM_DBG=oldsweep-owners` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_OOBFIELD` | DBG | `CRATONVM_DBG=oobfield` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OOM_BT` | DBG | `CRATONVM_DBG=oom-bt` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_OOPCOV` | DBG | `CRATONVM_DBG=oopcov` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE` | DBG | `CRATONVM_DBG=oop-oracle-force-refute` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR` | DBG | `CRATONVM_DBG=osr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR_BIND` | DBG | `CRATONVM_DBG=osr-bind` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR_FRAME_TRACE` | DBG | `CRATONVM_DBG=osr-frame-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR_META` | DBG | `CRATONVM_DBG=osr-meta` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OSR_SEED_COLLISION` | DBG | `CRATONVM_DBG=osr-seed-collision` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OSR_SLOTS` | DBG | `CRATONVM_DBG=osr-slots` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OVERLAY` | DBG | `CRATONVM_DBG=overlay` | opt-in | off | diag | snapshot | classloading, native-builtins, vm |
| `CRATONVM_DBG_OVERLAY_ALL` | DBG | `CRATONVM_DBG=overlay-all` | opt-in | off | diag | snapshot | classloading, vm |
| `CRATONVM_DBG_OVERLAY_BT` | DBG | `CRATONVM_DBG=overlay-bt` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OVERLAY_GATE` | DBG | `CRATONVM_DBG=overlay-gate` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OVERLAY_NODEDUP` | DBG | `CRATONVM_DBG=overlay-nodedup` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OVERLAY_PRUNE` | DBG | `CRATONVM_DBG=overlay-prune` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OWNER_FILTER` | DBG | `CRATONVM_DBG=owner-filter` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_PARKLAT` | DBG | `CRATONVM_DBG=parklat` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_PB` | DBG | `CRATONVM_DBG=pb` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_PBE` | DBG | `CRATONVM_DBG=pbe` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_PBSTART` | DBG | `CRATONVM_DBG=pbstart` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_PICOCLI_STYLE` | DBG | `CRATONVM_DBG=picocli-style` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_POPINT` | DBG | `CRATONVM_DBG=popint` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_PRECISE` | DBG | `CRATONVM_DBG=precise` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_PROMO_SEED` | DBG | `CRATONVM_DBG=promo-seed` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_PROXY` | DBG | `CRATONVM_DBG=proxy` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_PUNNED_REF` | DBG | `CRATONVM_DBG=punned-ref` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_QUARKUS_STATICINIT` | DBG | `CRATONVM_DBG=quarkus-staticinit` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RAF_GETFD` | DBG | `CRATONVM_DBG=raf-getfd` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RAF_INIT` | DBG | `CRATONVM_DBG=raf-init` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RBC6` | DBG | `CRATONVM_DBG=rbc6` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_RBC6_EMIT` | DBG | `CRATONVM_DBG=rbc6-emit` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_RE5` | DBG | `CRATONVM_DBG=re5` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_READ0LAT` | DBG | `CRATONVM_DBG=read0-latency` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_REDEFINE_DUMP` | DBG | `CRATONVM_DBG=redefine-dump` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_DBG_REFDISC` | DBG | `CRATONVM_DBG=refdisc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REFERSTO` | DBG | `CRATONVM_DBG=refersto` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_REFLECTION_FACTORY` | DBG | `CRATONVM_DBG=reflection-factory` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_REFPROC_REMARK` | DBG | `CRATONVM_DBG=refproc-remark` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REMAP_TRACE` | DBG | `CRATONVM_DBG=remap-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REPLOVR` | DBG | `CRATONVM_DBG=replovr` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RESOLVE_SHIM` | DBG | `CRATONVM_DBG=resolve-shim` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RESOURCE_TIMING` | DBG | `CRATONVM_DBG=resource-timing` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RESUME_PC` | DBG | `CRATONVM_DBG=resume-pc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_RETRANSFORM` | DBG | `CRATONVM_DBG=retransform` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTPROF` | DBG | `CRATONVM_DBG=rootprof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTSNAP` | DBG | `CRATONVM_DBG=rootsnap` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTSNAP_EVERY` | DBG | `CRATONVM_DBG=rootsnap-every` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTSNAP_VERIFY` | DBG | `CRATONVM_DBG=rootsnap-verify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOT_REMAP_AUDIT` | DBG | `CRATONVM_DBG=root-remap-audit` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOT_SOURCE` | DBG | `CRATONVM_DBG=root-source` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_RSET_AUDIT` | DBG | `CRATONVM_DBG=rset-audit` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN` | DBG | `CRATONVM_DBG=rset-audit-young-scan` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RTERR` | DBG | `CRATONVM_DBG=rterr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_RVAS` | DBG | `CRATONVM_DBG=rvas` | opt-in | off | diag | snapshot | libcratonvm |
| `CRATONVM_DBG_SBLOAD` | DBG | `CRATONVM_DBG=sbload` | opt-in | off | diag | snapshot | native-collections, types |
| `CRATONVM_DBG_SCALAR_DEOPT` | DBG | `CRATONVM_DBG=scalar-deopt` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_SCALAR_NEW` | DBG | `CRATONVM_DBG=scalar-new` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_SC_CLOSE` | DBG | `CRATONVM_DBG=sc-close` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SC_READ` | DBG | `CRATONVM_DBG=sc-read` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SC_WRITE` | DBG | `CRATONVM_DBG=sc-write` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SEEDHUNT` | DBG | `CRATONVM_DBG=seedhunt` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SEED_ALL_OLD` | DBG | `CRATONVM_DBG=seed-all-old` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SEL` | DBG | `CRATONVM_DBG=sel` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SELECTOR` | DBG | `CRATONVM_DBG=selector` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SETACC` | DBG | `CRATONVM_DBG=setacc` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_SHADOW` | DBG | `CRATONVM_DBG=shadow` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SHADOW2` | DBG | `CRATONVM_DBG=shadow2` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_SHADOW2_FILTER` | DBG | `CRATONVM_DBG=shadow2-filter` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_SHADOW_DEPTH` | DBG | `CRATONVM_DBG=shadow-depth` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SHADOW_RELOAD` | DBG | `CRATONVM_DBG=shadow-reload` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_SITE_ALIAS` | DBG | `CRATONVM_DBG=site-alias` | opt-in | off | diag | snapshot | vm, vm-cli |
| `CRATONVM_DBG_SLEEP_TRACE` | DBG | `CRATONVM_DBG=sleep-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SOCK` | DBG | `CRATONVM_DBG=sock` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SOCK_BYTES` | DBG | `CRATONVM_DBG=sock-bytes` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SOE` | DBG | `CRATONVM_DBG=soe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SPID` | DBG | `CRATONVM_DBG=spid` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_SP_IC_SITES` | DBG | `CRATONVM_DBG=sp-ic-sites` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_STACKLESS` | DBG | `CRATONVM_DBG=stackless` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STACK_KINDS` | DBG | `CRATONVM_DBG=stack-kinds` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_STALELONG` | DBG | `CRATONVM_DBG=stalelong` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STALE_OBJREF` | DBG | `CRATONVM_DBG=stale-objref` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_STALE_OBJREF_CYCLES` | DBG | `CRATONVM_DBG=stale-objref-cycles` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_STALE_RECV` | DBG | `CRATONVM_DBG=stale-recv` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STAMPED` | DBG | `CRATONVM_DBG=stamped` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_STRAYSTACK` | DBG | `CRATONVM_DBG=straystack` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STREAMSUPP` | DBG | `CRATONVM_DBG=streamsupp` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_STTRACE` | DBG | `CRATONVM_DBG=sttrace` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_STUBLOADER` | DBG | `CRATONVM_DBG=stubloader` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STUB_BT` | DBG | `CRATONVM_DBG=stub-bt` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_DBG_STUB_YIELD` | DBG | `CRATONVM_DBG=stub-yield` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STW_CENSUS` | DBG | `CRATONVM_DBG=stw-census` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STW_EXPECTED_IDS` | DBG | `CRATONVM_DBG=stw-expected-ids` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STW_NATIVE_RING` | DBG | `CRATONVM_DBG=stw-native-ring` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SWCHAIN` | DBG | `CRATONVM_DBG=swchain` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SWEEP_CENSUS` | DBG | `CRATONVM_DBG=sweep-census` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_EDGES` | DBG | `CRATONVM_DBG=sweep-edges` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_LIVENESS` | DBG | `CRATONVM_DBG=sweep-liveness` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_REFERRERS` | DBG | `CRATONVM_DBG=sweep-referrers` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_SWEEP_ZERO` | DBG | `CRATONVM_DBG=sweep-zero` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_THREADREG_PERF` | DBG | `CRATONVM_DBG=threadreg-perf` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_THREADSTART` | DBG | `CRATONVM_DBG=threadstart` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_TIER_ENQUEUE` | DBG | `CRATONVM_DBG=tier-enqueue` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_TLABMISS` | DBG | `CRATONVM_DBG=tlabmiss` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_TLS_AUTH` | DBG | `CRATONVM_DBG=tls-auth` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_TLS_HS` | DBG | `CRATONVM_DBG=tls-hs` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_TLS_PLS` | DBG | `CRATONVM_DBG=tls-pls` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_TLS_SOCK` | DBG | `CRATONVM_DBG=tls-sock` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_TLS_SRV` | DBG | `CRATONVM_DBG=tls-srv` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_TMVIEW` | DBG | `CRATONVM_DBG=tmview` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_TOARRAY` | DBG | `CRATONVM_DBG=toarray` | opt-in | off | diag | snapshot | native-collections, types, vm |
| `CRATONVM_DBG_TOHEX` | DBG | `CRATONVM_DBG=tohex` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_TYPECHECK_FILTER` | DBG | `CRATONVM_DBG=typecheck-filter` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_UCLREG` | DBG | `CRATONVM_DBG=uclreg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_UCLRES` | DBG | `CRATONVM_DBG=uclres` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_UCLTRACE` | DBG | `CRATONVM_DBG=ucltrace` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_UNCAUGHT` | DBG | `CRATONVM_DBG=uncaught` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_UNDERFLOW` | DBG | `CRATONVM_DBG=underflow` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_UNPARK_MISS` | DBG | `CRATONVM_DBG=unpark-miss` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_UNPIN_RING` | DBG | `CRATONVM_DBG=unpin-ring` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_UNREG_MEMO_AUDIT` | DBG | `CRATONVM_DBG=unreg-memo-audit` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_UNROLL` | DBG | `CRATONVM_DBG=unroll` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_URLCL` | DBG | `CRATONVM_DBG=urlcl` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_UTE` | DBG | `CRATONVM_DBG=ute` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_VACATED_FRAMES` | DBG | `CRATONVM_DBG=vacated-frames` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_VALIDATE_NEW` | DBG | `CRATONVM_DBG=validate-new` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_VDISP` | DBG | `CRATONVM_DBG=vdisp` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_VERIFY_ERROR` | DBG | `CRATONVM_DBG=verify-error` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` | DBG | `CRATONVM_DBG=verify-inline-frame-record` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_VERIFY_OOP_MAPS` | DBG | `CRATONVM_DBG=verify-oop-maps` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_VIEWKIND` | DBG | `CRATONVM_DBG=view-kind` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_VIEWRESYNC` | DBG | `CRATONVM_DBG=view-resync` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_VISITFILE` | DBG | `CRATONVM_DBG=visitfile` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_VM_STATE` | DBG | `CRATONVM_DBG=vm-state` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WATCHADDR` | DBG | `CRATONVM_DBG=watchaddr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WATCHREF` | DBG | `CRATONVM_DBG=watchref` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_WATCH_CAUSE_SELF` | DBG | `CRATONVM_DBG=watch-cause-self` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_WATCH_CELL` | DBG | `CRATONVM_DBG=watch-cell` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_WEAKREF` | DBG | `CRATONVM_DBG=weakref` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WF` | DBG | `CRATONVM_DBG=wf` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_WF_NPE` | DBG | `CRATONVM_DBG=wf-npe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_XNIO_TCP` | DBG | `CRATONVM_DBG=xnio-tcp` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_XT_COVERAGE` | DBG | `CRATONVM_DBG=xt-coverage` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_XT_JIT_ROOT_SCAN` | DBG | `CRATONVM_DBG=xt-jit-root-scan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_YOUNGSCAN` | DBG | `CRATONVM_DBG=youngscan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_YOUNGSTATE` | DBG | `CRATONVM_DBG=youngstate` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_YOUNG_TRIGGER` | DBG | `CRATONVM_DBG=young-trigger` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZERO_RANGES` | DBG | `CRATONVM_DBG=zero-ranges` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ZGC_CORPSE` | DBG | `CRATONVM_DBG=zgc-corpse` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZGC_VERIFY_SLIDE` | DBG | `CRATONVM_DBG=zgc-verify-slide` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DEBUG_SFI` | DBG | `CRATONVM_DBG=debug-sfi` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DEBUG_STACKWALK` | DBG | `CRATONVM_DBG=debug-stackwalk` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DEBUG_STACK_TAG` | DBG | `CRATONVM_DBG=debug-stack-tag` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DEFAULT_HEAP_ERGONOMICS` | GC | `CRATONVM_GC=default-heap-ergonomics` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DEFAULT_HEAP_MAX_MB` | GC | `CRATONVM_GC=default-heap-max-mb` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DEFAULT_WATCHDOG_SEC` | THREADS | `CRATONVM_THREADS=default-watchdog-sec` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_DEOPT_EAGER` | DBG | `CRATONVM_DBG=deopt-eager` | opt-in | off | diag | snapshot | difftest, jit |
| `CRATONVM_DEOPT_EAGER_BCI` | DBG | `CRATONVM_DBG=deopt-eager-bci` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DEOPT_REAL` | JIT | `CRATONVM_JIT=deopt-real` | opt-in | off | behaviour | snapshot | difftest, jit |
| `CRATONVM_DEOPT_VERIFY` | DBG | `CRATONVM_DBG=deopt-verify` | opt-in | off | diag | snapshot | difftest, jit |
| `CRATONVM_DIAG_HIB32` | DBG | `CRATONVM_DBG=diag-hib32` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_JAR_LIST` | DBG | `CRATONVM_DBG=diag-jar-list` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_JBOSS_SERVICES` | DBG | `CRATONVM_DBG=diag-jboss-services` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_JCA` | DBG | `CRATONVM_DBG=diag-jca` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_METHOD_INVOKE_NULL` | DBG | `CRATONVM_DBG=diag-method-invoke-null` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_PROPERTIES` | DBG | `CRATONVM_DBG=diag-properties` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_SERVICELOADER` | DBG | `CRATONVM_DBG=diag-serviceloader` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIFF_HOTSPOT` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests differential harness |
| `CRATONVM_DISABLE_AALOAD_LICM` | JIT | `CRATONVM_JIT=aaload-licm` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_DISABLE_ARITH_LICM` | JIT | `CRATONVM_JIT=arith-licm` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_DISABLE_DEFAULT_WATCHDOG` | THREADS | `CRATONVM_THREADS=default-watchdog` | opt-out | on | behaviour | snapshot | vm-cli |
| `CRATONVM_DISABLE_INTRINSICS` | JIT | `CRATONVM_JIT=intrinsics` | opt-out | on | behaviour | snapshot | difftest, vm, vm-cli |
| `CRATONVM_DISABLE_JAR_MMAP` | LOADER | `CRATONVM_LOADER=jar-mmap` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_DISABLE_JIT` | — | `CRATONVM_DISABLE_JIT` | scalar | unset | behaviour | snapshot | difftest, jit, types, vm-cli |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | JIT | `CRATONVM_JIT=scalar-replacement` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_DISABLE_UNROLL` | JIT | `CRATONVM_JIT=unroll` | both | off | behaviour | snapshot | jit |
| `CRATONVM_EAGER_STREAMS` | COMPAT | `CRATONVM_COMPAT=eager-streams` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_ENABLE_ASSERTIONS` | — | `CRATONVM_ENABLE_ASSERTIONS` | scalar | unset | behaviour | snapshot | types, vm-cli |
| `CRATONVM_ENABLE_NATIVE_RING` | DBG | `CRATONVM_DBG=enable-native-ring` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_ENFORCE_NATIVE_SHADOW` | LOADER | `CRATONVM_LOADER=enforce-native-shadow` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_EQE_SYNC_EXECUTE` | THREADS | `CRATONVM_THREADS=eqe-sync-execute` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_EXEC_DEPTH_CEILING` | THREADS | `CRATONVM_THREADS=exec-depth-ceiling` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_EXEC_FRAME_TRACE` | DBG | `CRATONVM_DBG=exec-frame-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_FC_FAST_IO` | JIT | `CRATONVM_JIT=fc-fast-io` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_FC_FAST_IO_STATS` | DBG | `CRATONVM_DBG=fc-fast-io-stats` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_FJP_EAGER_FORK` | THREADS | `CRATONVM_THREADS=fjp-eager-fork` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_FORCE_WIN_BUILD` | TEST | `CRATONVM_TEST=force-win-build` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FOREIGN_ATTACH` | COMPAT | `CRATONVM_COMPAT=foreign-attach` | opt-in | off | behaviour | snapshot | libcratonvm, vm |
| `CRATONVM_FORNAME_TRACE` | DBG | `CRATONVM_DBG=forname-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_FRAME_TRACE` | DBG | `CRATONVM_DBG=frame-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_FUZZ_BOOTCP` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | cargo-fuzz target |
| `CRATONVM_FWD_RESOLVE_STRICT` | LOADER | `CRATONVM_LOADER=fwd-resolve-strict` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_COVERAGE_PIN` | GC | `CRATONVM_GC=g1-coverage-pin` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_DBG_HEADERS` | DBG | `CRATONVM_DBG=g1-dbg-headers` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_PINS` | DBG | `CRATONVM_DBG=g1-dbg-pins` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_REACH` | DBG | `CRATONVM_DBG=g1-dbg-reach` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_ROOTCENSUS` | DBG | `CRATONVM_DBG=g1-dbg-rootcensus` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_RSET` | DBG | `CRATONVM_DBG=g1-dbg-rset` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_ZERO` | DBG | `CRATONVM_DBG=g1-dbg-zero` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_EAGER_HUMONGOUS` | GC | `CRATONVM_GC=g1-eager-humongous` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_NARROW_FIXUP` | GC | `CRATONVM_GC=g1-narrow-fixup` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_NO_EVAC_RETRY` | GC | `CRATONVM_GC=g1-evac-retry` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_G1_NO_LIVE_REGION_MEMO` | GC | `CRATONVM_GC=g1-live-region-memo` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_G1_PARALLEL_EVAC` | GC | `CRATONVM_GC=g1-parallel-evac` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PIN_EMPTY_PUBLICATION` | GC | `CRATONVM_GC=g1-pin-empty-publication` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_PRECISE_ONLY_ROOTS` | GC | `CRATONVM_GC=g1-precise-only-roots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_G1_RSET_SOURCE_CAP` | GC | `CRATONVM_GC=g1-rset-source-cap` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_SCRUB_FREE` | GC | `CRATONVM_GC=g1-scrub-free` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_G1_VERIFY_BUDGET` | GC | `CRATONVM_GC=g1-verify-budget` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_WORKERS` | GC | `CRATONVM_GC=g1-workers` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_YOUNG_PAUSE_TARGET` | GC | `CRATONVM_GC=g1-young-pause-target` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC` | GC | `CRATONVM_GC=…` | group | unset | — | snapshot | types |
| `CRATONVM_GC_ARRAY_GUARD_BT` | DBG | `CRATONVM_DBG=gc-array-guard-bt` | opt-in | off | diag | snapshot | types |
| `CRATONVM_GC_CARD_METRICS` | GC | `CRATONVM_GC=card-metrics` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_NO_CALLEE_RESOLVE` | GC | `CRATONVM_GC=innermost-callee-resolve` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_EMPTY_OBJECT_RUN` | GC | `CRATONVM_GC=empty-object-run` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_OLD_INTERIOR_PINS` | GC | `CRATONVM_GC=old-interior-pins` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_VALIDATE_ONCE` | GC | `CRATONVM_GC=validate-once` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_OVERHEAD_LIMIT` | GC | `CRATONVM_GC=overhead-limit` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_PAR_MIN_BYTES` | GC | `CRATONVM_GC=par-min-bytes` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_PAR_THREADS` | GC | `CRATONVM_GC=par-threads` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_PRECISE_ONLY_ROOTS` | GC | `CRATONVM_GC=precise-only-roots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_STATS` | DBG | `CRATONVM_DBG=gc-stats` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_GC_STRESS` | GC | `CRATONVM_GC=stress` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` | GC | `CRATONVM_GC=sweep-anchor-stride` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_VERIFY_STALE` | DBG | `CRATONVM_DBG=gc-verify-stale` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_GC_YOUNG_PAUSE_MS` | GC | `CRATONVM_GC=young-pause-goal-ms` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GPU_APPROX_MATH` | JIT | `CRATONVM_JIT=gpu-approx-math` | opt-in | off | behaviour | snapshot | jit-cuda |
| `CRATONVM_GPU_CHUNKS` | GC | `CRATONVM_GC=gpu-chunks` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_CHUNK_STREAMS` | GC | `CRATONVM_GC=gpu-chunk-streams` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_CRITICAL_LEASE_MS` | GC | `CRATONVM_GC=gpu-critical-lease-ms` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_CRITICAL_WAIT_MS` | GC | `CRATONVM_GC=gpu-critical-wait-ms` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_DUMP_PTX` | DBG | `CRATONVM_DBG=gpu-dump-ptx` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_GPU_NO_ZEROCOPY` | GC | `CRATONVM_GC=gpu-zerocopy` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GPU_TIME_DISPATCH` | DBG | `CRATONVM_DBG=gpu-time-dispatch` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_GPU_TRACE_BYTES` | DBG | `CRATONVM_DBG=gpu-trace-bytes` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | SECURITY | `CRATONVM_SECURITY=harden-manifest-classpath` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_HELPFUL_NPE_OPCODES` | JIT | `CRATONVM_JIT=helpful-npe-opcodes` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_HM_TRACE` | DBG | `CRATONVM_DBG=hm-trace` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_HS_ITR_DBG` | DBG | `CRATONVM_DBG=hs-itr-dbg` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_HTTP_MAX_BODY` | IO | `CRATONVM_IO=http-max-body` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_IAE_TRACE` | DBG | `CRATONVM_DBG=iae-trace` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_IAE_TRACE2` | DBG | `CRATONVM_DBG=iae-trace2` | opt-in | off | diag | snapshot | types |
| `CRATONVM_INHERIT_THREAD_CCL` | THREADS | `CRATONVM_THREADS=inherit-thread-ccl` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INHERIT_TL_WORKAROUND` | THREADS | `CRATONVM_THREADS=inherit-tl-workaround` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INLINE_ALLOW_STATIC` | JIT | `CRATONVM_JIT=inline-allow-static` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_INTRINSIC_STATS` | DBG | `CRATONVM_DBG=intrinsic-stats` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_INVOKESTATIC_LOADER_TRACE` | DBG | `CRATONVM_DBG=invokestatic-loader-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` | DBG | `CRATONVM_DBG=invoke-virtual-entry-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_IO` | IO | `CRATONVM_IO=…` | group | unset | — | snapshot | — |
| `CRATONVM_IR_DEOPT_RESUME` | JIT | `CRATONVM_JIT=ir-deopt-resume` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JAVA_HOME` | — | `CRATONVM_JAVA_HOME` | scalar | unset | behaviour | snapshot | libcratonvm, native-builtins, types, vm |
| `CRATONVM_JBOSS_BOOT_LOG_FILE` | COMPAT | `CRATONVM_COMPAT=jboss-boot-log-file` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JBOSS_BRUTE_FORCE_JARS` | COMPAT | `CRATONVM_COMPAT=jboss-brute-force-jars` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JBOSS_LOGGER_BASE_EMIT` | COMPAT | `CRATONVM_COMPAT=jboss-logger-base-emit` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JBOSS_LOGGER_LEVEL_FILTER` | COMPAT | `CRATONVM_COMPAT=jboss-logger-level-filter` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_JBOSS_MP_ROOT` | COMPAT | `CRATONVM_COMPAT=jboss-mp-root` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JCA_LENIENT_GETINSTANCE` | SECURITY | `CRATONVM_SECURITY=jca-lenient-getinstance` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JIT` | JIT | `CRATONVM_JIT=…` | group | unset | — | snapshot | types |
| `CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX` | JIT | `CRATONVM_JIT=activation-global-mutex` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_BISECT_ONLY` | DBG | `CRATONVM_DBG=jit-bisect-only` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_JIT_BULK_BYTE_LOOPS` | JIT | `CRATONVM_JIT=bulk-byte-loops` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_BYTECODE_LOOP_XFORM` | JIT | `CRATONVM_JIT=bytecode-loop-xform` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_C1_VECTOR_VETO` | JIT | `CRATONVM_JIT=c1-vector-veto` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_C2_FIRST_CALL` | JIT | `CRATONVM_JIT=c2-first-call` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE` | JIT | `CRATONVM_JIT=cached-entry-owner-reuse` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CENSUS_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=census-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB` | JIT | `CRATONVM_JIT=code-cache-max-mb` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE` | JIT | `CRATONVM_JIT=compiled-ldc-const-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DENY` | JIT | `CRATONVM_JIT=deny` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS` | JIT | `CRATONVM_JIT=direct-callee-calls` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH` | JIT | `CRATONVM_JIT=direct-exc-table-publish` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DISABLE_INLINE_NEW` | JIT | `CRATONVM_JIT=inline-new` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY` | JIT | `CRATONVM_JIT=dispatch-cache-direct-entry` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` | JIT | `CRATONVM_JIT=dispatch-cache-virtual-direct-entry` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_DUPX_EAGER_CANON` | JIT | `CRATONVM_JIT=dupx-eager-canon` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_EAGER_CALLEE_CHAIN` | JIT | `CRATONVM_JIT=eager-callee-chain` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS` | JIT | `CRATONVM_JIT=enable-callee-saved-gpr-locals` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_ENABLE_INLINE_NEW` | JIT | `CRATONVM_JIT=enable-inline-new` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_SITE_CACHE` | JIT | `CRATONVM_JIT=field-site-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FIELD_SITE_CACHE_LOADER` | JIT | `CRATONVM_JIT=field-site-cache-loader` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_FIELD_SITE_SLOTS` | JIT | `CRATONVM_JIT=field-site-slots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_FORCE_C2` | JIT | `CRATONVM_JIT=force-c2` | opt-in | off | behaviour | snapshot | difftest, jit |
| `CRATONVM_JIT_FULL_SELF_CALL_SPILL` | JIT | `CRATONVM_JIT=full-self-call-spill` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_GATE_PASS_MEMO` | JIT | `CRATONVM_JIT=gate-pass-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_GC_INERT_SELFREC` | JIT | `CRATONVM_JIT=gc-inert-selfrec` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_GETFIELD_HELPER` | JIT | `CRATONVM_JIT=getfield-helper` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_GETSTATIC_HELPER` | JIT | `CRATONVM_JIT=getstatic-helper` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` | JIT | `CRATONVM_JIT=guarded-virtual-inline` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_INCLUSIVE_BCE` | JIT | `CRATONVM_JIT=inclusive-bce` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_INDY_BRIDGE` | JIT | `CRATONVM_JIT=indy-bridge` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_CALLS` | JIT | `CRATONVM_JIT=inline-calls` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_CALL_DISPATCH` | JIT | `CRATONVM_JIT=inline-call-dispatch` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_GETFIELD` | JIT | `CRATONVM_JIT=inline-getfield` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_NEST` | JIT | `CRATONVM_JIT=inline-nest` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_SELF_GUARD` | JIT | `CRATONVM_JIT=inline-self-guard` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` | JIT | `CRATONVM_JIT=inline-splice-devirt` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_CALL` | JIT | `CRATONVM_JIT=ir-call` | opt-in | off | behaviour | snapshot | difftest, vm |
| `CRATONVM_JIT_IR_CALL_SPECIAL` | JIT | `CRATONVM_JIT=ir-call-special` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_CALL_VIRTUAL` | JIT | `CRATONVM_JIT=ir-call-virtual` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_DIRECT_CALL` | JIT | `CRATONVM_JIT=ir-direct-call` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_FP` | JIT | `CRATONVM_JIT=ir-fp` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_ISEL_EMIT` | JIT | `CRATONVM_JIT=ir-isel-emit` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ISEL_SHADOW` | JIT | `CRATONVM_JIT=ir-isel-shadow` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ISEL_VERIFY` | JIT | `CRATONVM_JIT=ir-isel-verify` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE` | JIT | `CRATONVM_JIT=ir-buffer-estimate` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LINEAR_SCAN` | JIT | `CRATONVM_JIT=ir-linear-scan` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LONG` | JIT | `CRATONVM_JIT=ir-long` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_OVER_INTRINSIC` | JIT | `CRATONVM_JIT=ir-over-intrinsic` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RELOC_EMIT` | JIT | `CRATONVM_JIT=ir-reloc-emit` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SELFREC_DIRECT` | JIT | `CRATONVM_JIT=ir-selfrec-direct` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD` | JIT | `CRATONVM_JIT=ir-unresumable-trap-guard` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_KERNEL_REG_LOCALS` | JIT | `CRATONVM_JIT=kernel-reg-locals` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_KERNEL_REG_OSR` | JIT | `CRATONVM_JIT=kernel-reg-osr` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LAMBDA_ADAPTER` | JIT | `CRATONVM_JIT=lambda-adapter` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER` | JIT | `CRATONVM_JIT=lambda-capture-adapter` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_CONST_PROBE` | JIT | `CRATONVM_JIT=lambda-const-probe` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_SITE` | JIT | `CRATONVM_JIT=lambda-site` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_TIERUP` | JIT | `CRATONVM_JIT=lambda-tierup` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LEAK_CODE` | JIT | `CRATONVM_JIT=leak-code` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LICM` | JIT | `CRATONVM_JIT=licm` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_HANDLERS` | JIT | `CRATONVM_JIT=local-handlers` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_REGS` | JIT | `CRATONVM_JIT=local-regs` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=long-box-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOOP_WORK_TIERUP` | JIT | `CRATONVM_JIT=loop-work-tierup` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_MAIN_INLINE` | JIT | `CRATONVM_JIT=main-inline` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_MATRIX_DOT` | JIT | `CRATONVM_JIT=matrix-dot` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_METHOD_SITE_CACHE` | JIT | `CRATONVM_JIT=method-site-cache` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_METRICS` | JIT | `CRATONVM_JIT=metrics` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_METRICS_OUT` | JIT | `CRATONVM_JIT=metrics-out` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_METRICS_RING` | JIT | `CRATONVM_JIT=metrics-ring` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH` | JIT | `CRATONVM_JIT=mic-exc-table-publish` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MY_SCRATCH_FLUSH` | JIT | `CRATONVM_JIT=my-scratch-flush` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MY_SELFCALL_PROOF` | JIT | `CRATONVM_JIT=my-selfcall-proof` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MY_SHADOW_EMISSION` | JIT | `CRATONVM_JIT=my-shadow-emission` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL` | JIT | `CRATONVM_JIT=native-shadow-caller-seal` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND` | JIT | `CRATONVM_JIT=native-shadow-interface-blind` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NEVER_FREE_CODE` | JIT | `CRATONVM_JIT=never-free-code` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ALLOC_SPILL_SINK` | JIT | `CRATONVM_JIT=alloc-spill-sink` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ATOMIC_INTRINSIC` | JIT | `CRATONVM_JIT=atomic-intrinsic` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_BCE` | JIT | `CRATONVM_JIT=bce` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH` | JIT | `CRATONVM_JIT=callee-oop-flush` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_CAST_SITE_CACHE` | JIT | `CRATONVM_JIT=cast-site-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_CODE_PTR_MEMO` | JIT | `CRATONVM_JIT=code-ptr-memo` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUP2_X2` | JIT | `CRATONVM_JIT=dup2-x2` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUPX` | JIT | `CRATONVM_JIT=dupx` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUP_X1` | JIT | `CRATONVM_JIT=dup-x1` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUP_X2` | JIT | `CRATONVM_JIT=dup-x2` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_EXC_TABLE_C2` | JIT | `CRATONVM_JIT=exc-table-c2` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_FRAME_BANDS` | JIT | `CRATONVM_JIT=frame-bands` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_LDC_CONST_CACHE` | JIT | `CRATONVM_JIT=ldc-const-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_LONG_INTRINSICS` | JIT | `CRATONVM_JIT=long-intrinsics` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE` | JIT | `CRATONVM_JIT=mic-rust-entry-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NATIVE_SITE_CACHE` | JIT | `CRATONVM_JIT=native-site-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES` | JIT | `CRATONVM_JIT=nested-trace-frames` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NEW_SITE_CACHE` | JIT | `CRATONVM_JIT=new-site-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_OSR_AMBIGUOUS_DEAD` | JIT | `CRATONVM_JIT=osr-ambiguous-dead` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE` | JIT | `CRATONVM_JIT=osr-frame-dedupe` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_OSR_REFINED_REF` | JIT | `CRATONVM_JIT=osr-refined-ref` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PARAM_TAG_SCAN` | JIT | `CRATONVM_JIT=param-tag-scan` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW` | JIT | `CRATONVM_JIT=precise-alloc-athrow` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_FIELD_OPS` | JIT | `CRATONVM_JIT=precise-field-ops` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST` | JIT | `CRATONVM_JIT=precise-getstatic-checkcast` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES` | JIT | `CRATONVM_JIT=precise-virtual-invokes` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_RETPC_VALIDATE` | JIT | `CRATONVM_JIT=retpc-validate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_SELF_CACHE_INHERIT` | JIT | `CRATONVM_JIT=self-cache-inherit` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_SLOT_MIRROR` | JIT | `CRATONVM_JIT=slot-mirror` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_SPEC_BCE` | JIT | `CRATONVM_JIT=spec-bce` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STACK_BANG` | JIT | `CRATONVM_JIT=stack-bang` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN` | JIT | `CRATONVM_JIT=string-intrinsic-pin` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD` | JIT | `CRATONVM_JIT=trusted-oop-getfield` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY` | JIT | `CRATONVM_JIT=oopmap-coverage-presence-only` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR` | JIT | `CRATONVM_JIT=osr` | opt-in | off | behaviour | snapshot | difftest, vm |
| `CRATONVM_JIT_OSR_ATHROW` | JIT | `CRATONVM_JIT=osr-athrow` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_DEAD_LOCALS` | JIT | `CRATONVM_JIT=osr-dead-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_DEAD_MASK_BLANKET` | JIT | `CRATONVM_JIT=osr-dead-mask-blanket` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_EXC_TABLE` | JIT | `CRATONVM_JIT=osr-exc-table` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS` | JIT | `CRATONVM_JIT=osr-seed-frame-slots` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_SINGLE_PC` | JIT | `CRATONVM_JIT=osr-single-pc` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES` | JIT | `CRATONVM_JIT=osr-strip-all-high-halves` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_POISON_FREE` | JIT | `CRATONVM_JIT=poison-free` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_RANGE_BCE` | JIT | `CRATONVM_JIT=range-bce` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_RANGE_SCAN_LEGACY` | JIT | `CRATONVM_JIT=range-scan-legacy` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_REAL_NEW_SITE_FLAGS` | JIT | `CRATONVM_JIT=real-new-site-flags` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_REASSOC` | JIT | `CRATONVM_JIT=reassoc` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SAFEPOINT_POLLS` | JIT | `CRATONVM_JIT=safepoint-polls` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL` | JIT | `CRATONVM_JIT=safepoint-reg-spill` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SCALAR_NEW` | JIT | `CRATONVM_JIT=scalar-new` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_SELF_TAILCALL` | JIT | `CRATONVM_JIT=self-tailcall` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SITE_CACHE` | JIT | `CRATONVM_JIT=site-cache` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_SP_IC_DENY` | JIT | `CRATONVM_JIT=sp-ic-deny` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_DEOPT_CHECK` | JIT | `CRATONVM_JIT=sp-ic-deopt-check` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_ONLY` | JIT | `CRATONVM_JIT=sp-ic-only` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_IC` | JIT | `CRATONVM_JIT=sp-inline-ic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_MEGA` | JIT | `CRATONVM_JIT=sp-inline-mega` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_MIC` | JIT | `CRATONVM_JIT=sp-inline-mic` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_PIC` | JIT | `CRATONVM_JIT=sp-inline-pic` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_TAILCALL` | JIT | `CRATONVM_JIT=sp-tailcall` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_STACK_BANG` | JIT | `CRATONVM_JIT=stack-bang` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_STATIC_BYTECODE_CALLEE` | JIT | `CRATONVM_JIT=static-bytecode-callee` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_STRICT_CALLEE_ROOTS` | JIT | `CRATONVM_JIT=strict-callee-roots` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_STRICT_INSTALL_EPOCH` | JIT | `CRATONVM_JIT=strict-install-epoch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SYNC_METHODS` | JIT | `CRATONVM_JIT=sync-methods` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_THRESHOLD` | JIT | `CRATONVM_JIT=threshold` | opt-in | off | behaviour | snapshot | difftest, types, vm |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE` | JIT | `CRATONVM_JIT=unreg-accept-residue` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNREG_MEMO_GC_RESET` | JIT | `CRATONVM_JIT=unreg-memo-gc-reset` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNREG_MEMO_HIWATER` | JIT | `CRATONVM_JIT=unreg-memo-hiwater` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNROLL` | JIT | `CRATONVM_JIT=unroll` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=varhandle-read-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VECTORIZE` | JIT | `CRATONVM_JIT=vectorize` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_ARENA_ORDER` | JIT | `CRATONVM_JIT=verify-arena-order` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_FRAME_STATES` | JIT | `CRATONVM_JIT=verify-frame-states` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_IR` | JIT | `CRATONVM_JIT=verify-ir` | default-on | on | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` | JIT | `CRATONVM_JIT=verify-memory-chain` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_SCHEDULE` | JIT | `CRATONVM_JIT=verify-schedule` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_TYPES` | JIT | `CRATONVM_JIT=verify-types` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE` | JIT | `CRATONVM_JIT=virtual-bytecode-callee` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_VIRTUAL_TIERUP` | JIT | `CRATONVM_JIT=virtual-tierup` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LAZY_STREAMS` | COMPAT | `CRATONVM_COMPAT=lazy-streams` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_LDC_CLASSREF_TRACE` | DBG | `CRATONVM_DBG=ldc-classref-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_LENIENT_CLINIT` | LOADER | `CRATONVM_LOADER=lenient-clinit` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LHM_ROOT_ALL` | GC | `CRATONVM_GC=lhm-root-all` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_LOADER` | LOADER | `CRATONVM_LOADER=…` | group | unset | — | snapshot | — |
| `CRATONVM_LOADER_AWARE_RESOLUTION` | LOADER | `CRATONVM_LOADER=aware-resolution` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_LOADER_PARENT_CHAIN` | LOADER | `CRATONVM_LOADER=parent-chain` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_LOADER_UNLOAD` | LOADER | `CRATONVM_LOADER=unload` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LOCK_ORDER_CHECK` | THREADS | `CRATONVM_THREADS=lock-order-check` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_LONGREWRITE_LOOSE` | LOADER | `CRATONVM_LOADER=longrewrite-loose` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LONGROOT_STRICT` | JIT | `CRATONVM_JIT=longroot-strict` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MAP_VIEW_CACHE` | COMPAT | `CRATONVM_COMPAT=map-view-cache` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_MAVEN_REPO_LOCAL` | — | `CRATONVM_MAVEN_REPO_LOCAL` | scalar | unset | behaviour | snapshot | native-builtins, types |
| `CRATONVM_MAX_INFLATED_BYTES` | GC | `CRATONVM_GC=max-inflated-bytes` | opt-in | off | behaviour | snapshot | native-builtins, types |
| `CRATONVM_MH_STRICT_INVOKEEXACT` | COMPAT | `CRATONVM_COMPAT=mh-strict-invokeexact` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MOCKITO_LEGACY_SELECTORS` | COMPAT | `CRATONVM_COMPAT=mockito-legacy-selectors` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_MONITOR_PENDING_NOTIFY` | THREADS | `CRATONVM_THREADS=monitor-pending-notify` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG` | GC | `CRATONVM_GC=moving-young` | both | off | behaviour | snapshot | jit, types |
| `CRATONVM_MOVING_YOUNG_BAND_DBG` | DBG | `CRATONVM_DBG=moving-young-band-dbg` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_COVERAGE_DBG` | DBG | `CRATONVM_DBG=moving-young-coverage-dbg` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_FALLBACKS` | DBG | `CRATONVM_DBG=moving-young-fallbacks` | opt-in | off | diag | snapshot | types |
| `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` | DBG | `CRATONVM_DBG=moving-young-no-band-verify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD` | GC | `CRATONVM_GC=moving-young-bounds-guard` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_JIT` | GC | `CRATONVM_GC=moving-young-jit-frames` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_VERIFY` | DBG | `CRATONVM_DBG=moving-young-verify` | opt-in | off | diag | snapshot | types |
| `CRATONVM_MSC_REAL_START` | REAL | `CRATONVM_REAL=msc-real-start` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NATIVE_EC_MULTIPLY` | JIT | `CRATONVM_JIT=native-ec-multiply` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_NATIVE_MATCHER_FIND` | JIT | `CRATONVM_JIT=native-matcher-find` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | JIT | `CRATONVM_JIT=native-pbe-keyfactory` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_NATIVE_SHADOW_SINK_CAP` | DBG | `CRATONVM_DBG=native-shadow-sink-cap` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_NATIVE_STRING_REGEX` | JIT | `CRATONVM_JIT=native-string-regex` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_NEEDS_EXACT_TRACE` | DBG | `CRATONVM_DBG=needs-exact-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | IO | `CRATONVM_IO=netty-queue-bridge` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NONEXISTENT_VAR_12345` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | absent-name probe |
| `CRATONVM_NO_CONSERVATIVE_LOCALS` | JIT | `CRATONVM_JIT=conservative-locals` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_CTOR_DIRECT_CALL` | JIT | `CRATONVM_JIT=ctor-direct-call` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_DEFRAG_PROMOTE` | GC | `CRATONVM_GC=defrag-promote` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` | GC | `CRATONVM_GC=exact-refproc-survival` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_FORMAT_ARG_PIN` | GC | `CRATONVM_GC=format-arg-pin` | opt-out | on | behaviour | snapshot | native-builtins |
| `CRATONVM_NO_GC_PROMOTION_GUARD` | GC | `CRATONVM_GC=promotion-guard` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_IR_BRANCHY` | JIT | `CRATONVM_JIT=ir-branchy` | opt-out | on | behaviour | snapshot | difftest, jit |
| `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` | JIT | `CRATONVM_JIT=alloc-class-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME` | JIT | `CRATONVM_JIT=callee-handler-precise-frame` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD` | JIT | `CRATONVM_JIT=inline-putfield` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW` | JIT | `CRATONVM_JIT=inline-tlab-new` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES` | JIT | `CRATONVM_JIT=precise-handler-frames` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_SCAN_CACHE` | JIT | `CRATONVM_JIT=scan-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_JIT_TLAB_ZERO_ELISION` | JIT | `CRATONVM_JIT=tlab-zero-elision` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_LOCAL_LIVENESS` | JIT | `CRATONVM_JIT=local-liveness` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_MAP_ITERATOR_FAILFAST` | COMPAT | `CRATONVM_COMPAT=map-iterator-failfast` | opt-out | on | behaviour | snapshot | native-collections |
| `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` | GC | `CRATONVM_GC=mirror-pin-young-defer` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_NO_MOVING_YOUNG` | GC | `CRATONVM_GC=moving-young` | both | off | behaviour | snapshot | types |
| `CRATONVM_NO_OLDGEN_COALESCE` | GC | `CRATONVM_GC=oldgen-coalesce` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_OSR_CTOR_BIND` | JIT | `CRATONVM_JIT=osr-ctor-bind` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` | JIT | `CRATONVM_JIT=precise-inline-frame-record` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | JIT | `CRATONVM_JIT=precise-jit-maps` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_PRECISE_REG_SPILL` | JIT | `CRATONVM_JIT=precise-reg-spill` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_REFERENT_IDENTITY_SCREEN` | GC | `CRATONVM_GC=referent-identity-screen` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_SELECTIVE_PROMOTE` | GC | `CRATONVM_GC=selective-promote` | opt-out | on | behaviour | snapshot | difftest, types |
| `CRATONVM_NO_SELECTOR_CONNECT_PROBE` | IO | `CRATONVM_IO=selector-connect-probe` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_STATICS_INDEX` | JIT | `CRATONVM_JIT=statics-index` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_STUBS` | REAL | `CRATONVM_REAL=stubs` | opt-out | on | behaviour | snapshot | native-api, vm-cli |
| `CRATONVM_NSEE_TRACE` | DBG | `CRATONVM_DBG=nsee-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_OLDGEN_COMPACT` | GC | `CRATONVM_GC=oldgen-compact` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_OLD_SWEEP_JIT` | JIT | `CRATONVM_JIT=old-sweep-jit` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_OOP_SPAN_PROBE` | DBG | `CRATONVM_DBG=oop-span-probe` | opt-in | off | diag | snapshot | types |
| `CRATONVM_OSR_COVERAGE_SHADOW` | GC | `CRATONVM_GC=osr-coverage-shadow` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_OSR_EXIT_AFTER` | DBG | `CRATONVM_DBG=osr-exit-after` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_OSR_EXIT_TEST` | DBG | `CRATONVM_DBG=osr-exit-test` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_OSR_NEWARRAY` | JIT | `CRATONVM_JIT=osr-newarray` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_OWNER_CLASS_FILTER` | GC | `CRATONVM_GC=owner-class-filter` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_PACK_FIELDS_BY_WIDTH` | GC | `CRATONVM_GC=pack-fields-by-width` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_PHASE_ACCOUNTING` | DBG | `CRATONVM_DBG=phase-accounting` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PHASE_ACCOUNTING_JFR` | DBG | `CRATONVM_DBG=phase-accounting-jfr` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PHASE_ACCOUNTING_OUT` | DBG | `CRATONVM_DBG=phase-accounting-out` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PRECISE_COVERAGE_PIN` | JIT | `CRATONVM_JIT=precise-coverage-pin` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_PROMOTION_OOM_GUARD_BROAD` | GC | `CRATONVM_GC=promotion-oom-guard-broad` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_QUICKEN_STATS` | DBG | `CRATONVM_DBG=quicken-stats` | opt-in | off | diag | snapshot | reader |
| `CRATONVM_QUIET_DEPRECATIONS` | DBG | `CRATONVM_DBG=deprecations` | opt-out | on | diag | snapshot | vm-cli |
| `CRATONVM_QUIET_ENV_FALLBACK` | DBG | `CRATONVM_DBG=quiet-env-fallback` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_REAL` | REAL | `CRATONVM_REAL=…` | group | unset | — | snapshot | vm, vm-cli |
| `CRATONVM_REAL_AGROAL` | REAL | `CRATONVM_REAL=agroal` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_ANNOTATIONS` | REAL | `CRATONVM_REAL=annotations` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_AQS` | REAL | `CRATONVM_REAL=aqs` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_FORKJOINPOOL` | REAL | `CRATONVM_REAL=forkjoinpool` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_JCA` | REAL | `CRATONVM_REAL=jca` | opt-in | off | behaviour | snapshot | types, vm |
| `CRATONVM_REAL_NET_SOCKETS` | REAL | `CRATONVM_REAL=net-sockets` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_PROXY` | REAL | `CRATONVM_REAL=proxy` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_REAL_PROXY_STRICT` | REAL | `CRATONVM_REAL=proxy-strict` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_REAL_PROXY_SUPER` | REAL | `CRATONVM_REAL=proxy-super` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_REAL_QUARKUS_START` | REAL | `CRATONVM_REAL=quarkus-start` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_RAF` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | retired gate; env_remove baseline only |
| `CRATONVM_REAL_STAX_FACTORY` | REAL | `CRATONVM_REAL=stax-factory` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_REAL_VERTX` | REAL | `CRATONVM_REAL=vertx` | both | off | behaviour | snapshot | types |
| `CRATONVM_REFLECT_NO_EXPORT_GATE` | SECURITY | `CRATONVM_SECURITY=reflect-export-gate` | opt-out | on | behaviour | snapshot | native-builtins |
| `CRATONVM_REGEN_HEADER` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | libcratonvm/build.rs |
| `CRATONVM_REGISTER_IMAGE_REMAP` | GC | `CRATONVM_GC=register-image-remap` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_REQUIRE_POLICY` | SECURITY | `CRATONVM_SECURITY=require-policy` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_RESOLVE_CACHE_CAP` | LOADER | `CRATONVM_LOADER=resolve-cache-cap` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | IO | `CRATONVM_IO=resolve-outbound-host` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ROOTSNAP_CACHE` | JIT | `CRATONVM_JIT=rootsnap-cache` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC` | JIT | `CRATONVM_JIT=rootsnap-cache-survive-gc` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests opt-in |
| `CRATONVM_S111_DBG` | DBG | `CRATONVM_DBG=s111-dbg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SCALAR_DEOPT` | JIT | `CRATONVM_JIT=scalar-deopt` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_SCANNER_DEBUG` | DBG | `CRATONVM_DBG=scanner-debug` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_SECURITY` | SECURITY | `CRATONVM_SECURITY=…` | group | unset | — | snapshot | types |
| `CRATONVM_SELECT_MAX_BLOCK_MS` | IO | `CRATONVM_IO=select-max-block-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_SFI_NULL_TRACE` | DBG | `CRATONVM_DBG=sfi-null-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SHADOW_NOPUSH` | JIT | `CRATONVM_JIT=shadow-nopush` | opt-in | off | behaviour | snapshot | jit, vm |
| `CRATONVM_SHADOW_NORELOAD` | JIT | `CRATONVM_JIT=shadow-noreload` | opt-in | off | behaviour | snapshot | jit, vm |
| `CRATONVM_SHADOW_NO_END_GUARD` | JIT | `CRATONVM_JIT=shadow-end-guard` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_SHADOW_NO_SAVEBASE` | JIT | `CRATONVM_JIT=shadow-savebase` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_SHADOW_OVERFLOW_DIAG` | JIT | `CRATONVM_JIT=shadow-overflow-diag` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_SHADOW_PIN` | JIT | `CRATONVM_JIT=shadow-pin` | opt-in | off | behaviour | snapshot | jit, vm |
| `CRATONVM_SHADOW_RAW_RELOAD` | JIT | `CRATONVM_JIT=shadow-raw-reload` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_SHADOW_SENTINEL` | DBG | `CRATONVM_DBG=shadow-sentinel` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_SHADOW_STACK` | JIT | `CRATONVM_JIT=shadow-stack` | opt-in | off | behaviour | snapshot | jit, types, vm |
| `CRATONVM_SHADOW_WATCH` | DBG | `CRATONVM_DBG=shadow-watch` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS` | THREADS | `CRATONVM_THREADS=shutdown-hook-timeout-ms` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_SOAK_ITERS` | TEST | `CRATONVM_TEST=soak-iters` | opt-in | off | behaviour | snapshot | libcratonvm |
| `CRATONVM_SOAK_K` | TEST | `CRATONVM_TEST=soak-k` | opt-in | off | behaviour | snapshot | libcratonvm |
| `CRATONVM_SOAK_METHOD` | TEST | `CRATONVM_TEST=soak-method` | opt-in | off | behaviour | snapshot | libcratonvm |
| `CRATONVM_SOAK_TIMEOUT_SECS` | TEST | `CRATONVM_TEST=soak-timeout-secs` | opt-in | off | behaviour | snapshot | libcratonvm |
| `CRATONVM_SOAK_XMX` | TEST | `CRATONVM_TEST=soak-xmx` | opt-in | off | behaviour | snapshot | libcratonvm |
| `CRATONVM_SOCKET_CAPTURE` | IO | `CRATONVM_IO=socket-capture` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_SOFT_EXIT` | DBG | `CRATONVM_DBG=soft-exit` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SOMETHING_BRAND_NEW` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | unknown-key fall-through probe |
| `CRATONVM_SPRING_BOOT_FATJAR` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests fixture path |
| `CRATONVM_SPRING_DBG` | DBG | `CRATONVM_DBG=spring-dbg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SP_NO_COALESCE` | JIT | `CRATONVM_JIT=sp-coalesce` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SP_STATS` | DBG | `CRATONVM_DBG=sp-stats` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SP_TRACE` | DBG | `CRATONVM_DBG=sp-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SP_VERIFY` | DBG | `CRATONVM_DBG=sp-verify` | opt-in | off | diag | snapshot | types |
| `CRATONVM_STRESS_THREAD_STATES` | THREADS | `CRATONVM_THREADS=stress-thread-states` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_STRICT_JIT_ROOTS` | JIT | `CRATONVM_JIT=strict-jit-roots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_STRICT_SWALLOWS` | COMPAT | `CRATONVM_COMPAT=strict-swallows` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_STRIPED_COUNTERS_OFF` | THREADS | `CRATONVM_THREADS=striped-counters` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SUREFIRE_IPC_DBG` | DBG | `CRATONVM_DBG=surefire-ipc-dbg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SYMBOLIZE` | DBG | `CRATONVM_DBG=symbolize` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_SYMBOLIZE_DBG` | DBG | `CRATONVM_DBG=symbolize-dbg` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_SYNTHETIC_AGROAL` | REAL | `CRATONVM_REAL=agroal` | both | off | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_ANNOTATIONS` | REAL | `CRATONVM_REAL=annotations` | both | off | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_AQS` | REAL | `CRATONVM_REAL=aqs` | both | off | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_DSA` | REAL | `CRATONVM_REAL=dsa` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_EC` | REAL | `CRATONVM_REAL=ec` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_EQE` | REAL | `CRATONVM_REAL=eqe` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_FILEWRITER` | REAL | `CRATONVM_REAL=filewriter` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_FORKJOINPOOL` | REAL | `CRATONVM_REAL=forkjoinpool` | both | off | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING` | REAL | `CRATONVM_REAL=memoryusage-tostring` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_MXBEAN_MAPPING` | REAL | `CRATONVM_REAL=mxbean-mapping` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_NETTY_TCNATIVE` | REAL | `CRATONVM_REAL=netty-tcnative` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_NET_SOCKETS` | REAL | `CRATONVM_REAL=net-sockets` | both | off | behaviour | snapshot | native-builtins, types |
| `CRATONVM_SYNTHETIC_PQC` | REAL | `CRATONVM_REAL=pqc` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_QUARKUS_ARC` | REAL | `CRATONVM_REAL=quarkus-arc` | opt-out | on | behaviour | snapshot | native-builtins |
| `CRATONVM_SYNTHETIC_QUARKUS_START` | REAL | `CRATONVM_REAL=quarkus-start` | both | off | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_RAF` | REAL | `CRATONVM_REAL=raf` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_RSA` | REAL | `CRATONVM_REAL=rsa` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SYNTHETIC_VERTX` | REAL | `CRATONVM_REAL=vertx` | both | off | behaviour | snapshot | types |
| `CRATONVM_TEST` | TEST | `CRATONVM_TEST=…` | group | unset | — | snapshot | — |
| `CRATONVM_TEST_CLASSES_DIR` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/build.rs, read with option_env! |
| `CRATONVM_TEST_JAVA_HOME` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests JDK location |
| `CRATONVM_TEST_JDK` | TEST | `CRATONVM_TEST=jdk` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_TEST_SEGV` | TEST | `CRATONVM_TEST=segv` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_TEST_VAR` | TEST | `CRATONVM_TEST=var` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_THREADS` | THREADS | `CRATONVM_THREADS=…` | group | unset | — | snapshot | types |
| `CRATONVM_THREAD_CONTAINERS` | THREADS | `CRATONVM_THREADS=thread-containers` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THREAD_START_GRACE_MS` | THREADS | `CRATONVM_THREADS=thread-start-grace-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TIER_C1_THRESHOLD` | JIT | `CRATONVM_JIT=tier-c1-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_MIN_INVOCATIONS` | JIT | `CRATONVM_JIT=tier-c2-min-invocations` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_THRESHOLD` | JIT | `CRATONVM_JIT=tier-c2-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_ENABLED` | JIT | `CRATONVM_JIT=tiered` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_TIER_OSR_BACKEDGE` | JIT | `CRATONVM_JIT=tier-osr-backedge` | opt-in | off | behaviour | snapshot | difftest, vm |
| `CRATONVM_TIER_OSR_THRESHOLD` | JIT | `CRATONVM_JIT=tier-osr-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_PGO` | JIT | `CRATONVM_JIT=tier-pgo` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TLAB_GC_TRIGGER` | GC | `CRATONVM_GC=tlab-gc-trigger` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TLS_OPENSSL_CLIENT` | SECURITY | `CRATONVM_SECURITY=tls-openssl-client` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_TOMCAT_MAPPER_NATIVES` | COMPAT | `CRATONVM_COMPAT=tomcat-mapper-natives` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_TRACE_ARRAYS_HASHCODE` | DBG | `CRATONVM_DBG=trace-arrays-hashcode` | opt-in | off | diag | snapshot | types |
| `CRATONVM_TRACE_CLASSVALUE` | DBG | `CRATONVM_DBG=trace-classvalue` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_TRACE_PTI_ARGS` | DBG | `CRATONVM_DBG=trace-pti-args` | opt-in | off | diag | snapshot | types |
| `CRATONVM_TRACE_SB_FILTER` | DBG | `CRATONVM_DBG=trace-sb-filter` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_TRACE_UNIMPLEMENTED` | DBG | `CRATONVM_DBG=trace-unimplemented` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_TRACK_NATIVE` | DBG | `CRATONVM_DBG=track-native` | opt-in | off | diag | snapshot | native-api |
| `CRATONVM_TRIVIAL_GETTER` | JIT | `CRATONVM_JIT=trivial-getter` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_TRIVIAL_GETTER_VERIFY` | DBG | `CRATONVM_DBG=trivial-getter-verify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_TRUST_PEM` | SECURITY | `CRATONVM_SECURITY=trust-pem` | opt-in | off | behaviour | snapshot | classloading, types |
| `CRATONVM_UEH_DEBUG` | DBG | `CRATONVM_DBG=ueh-debug` | opt-in | off | diag | snapshot | types |
| `CRATONVM_UNTRUSTED_CODE` | SECURITY | `CRATONVM_SECURITY=untrusted-code` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_URI_STRICT_CHARS` | IO | `CRATONVM_IO=uri-strict-chars` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | REAL | `CRATONVM_REAL=use-wildfly-reflect-shim` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | REAL | `CRATONVM_REAL=use-wildfly-synth-bytecode` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_VECTOR_INTRINSICS` | JIT | `CRATONVM_JIT=vector-intrinsics` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VECTOR_INTRINSICS_STATS` | DBG | `CRATONVM_DBG=vector-intrinsics-stats` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_VECTOR_TEMPLATES` | JIT | `CRATONVM_JIT=vector-templates` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VERIFY_MAP_VIEW_CACHE` | COMPAT | `CRATONVM_COMPAT=verify-map-view-cache` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_VH_STRICT_REFERENCE_RETURN` | COMPAT | `CRATONVM_COMPAT=vh-strict-reference-return` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_WAIT_SPURIOUS_MS` | THREADS | `CRATONVM_THREADS=wait-spurious-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_WEAKREF_CLEAR` | GC | `CRATONVM_GC=weakref-clear` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_HELPER_WINDOW_SCAN` | JIT | `CRATONVM_JIT=xt-helper-window-scan` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE` | GC | `CRATONVM_GC=xt-jit-coverage-handshake` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_JIT_ROOT_SCAN` | JIT | `CRATONVM_JIT=xt-jit-root-scan` | opt-in | off | behaviour | snapshot | jit, vm |
| `CRATONVM_XT_PEER_DEADLINE_MS` | JIT | `CRATONVM_JIT=xt-peer-deadline-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_PEER_TOTAL_MS` | JIT | `CRATONVM_JIT=xt-peer-total-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_YOUNGSCAN_STRIDE` | GC | `CRATONVM_GC=youngscan-stride` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_ZGC_CONC_START` | GC | `CRATONVM_GC=zgc-conc-start` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_CONC_WORKERS` | GC | `CRATONVM_GC=zgc-conc-workers` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GENERATIONAL` | GC | `CRATONVM_GC=zgc-generational` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` | GC | `CRATONVM_GC=zgc-gen-minors-per-major` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GEN_NURSERY_PERCENT` | GC | `CRATONVM_GC=zgc-gen-nursery-percent` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GEN_PROMOTION_AGE` | GC | `CRATONVM_GC=zgc-gen-promotion-age` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARK_CTX_DIRECT` | GC | `CRATONVM_GC=zgc-mark-ctx-direct` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_NO_JIT_READ_BOUNDS` | GC | `CRATONVM_GC=zgc-jit-read-bounds` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PARMARK` | GC | `CRATONVM_GC=zgc-parmark` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_RELOCATE` | GC | `CRATONVM_GC=zgc-relocate` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` | GC | `CRATONVM_GC=zgc-relocate-proven-jit` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_STARTBITS` | GC | `CRATONVM_GC=zgc-startbits` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_SWEEP_DEAD_RUNS` | GC | `CRATONVM_GC=zgc-sweep-dead-runs` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_SWEEP_HEADER_ZERO` | GC | `CRATONVM_GC=zgc-sweep-header-zero` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB` | GC | `CRATONVM_GC=zgc-tlab` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | IO | `CRATONVM_IO=zip-max-entry-bytes` | opt-in | off | behaviour | snapshot | types |
