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
  is set, `default-on` when `on_key` carries an `off_word` (unless
  `types/src/flags.rs` reads the variable OFF when unset; the Default column
  then says `gen-on` for a per-backend switch that is on for
  `-XX:+UseGenerationalGC` and off for G1/ZGC when unset), `both` when the knob
  has an explicit spelling in each direction, `scalar` for the five named
  scalars, `group` for the ten grouped variables.
* **Default** — `off` for `opt-in`/`both`, `on` for `opt-out`/`default-on`, and
  `gen-on` for a per-backend `default-on` switch.
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

**Boolean values of JIT switches.** A `presence` parser means "set at all is
on". Since 2026-09-12 that no longer describes the JIT crate's own on/off reads.
Those go through `cratonvm_types::flags::runtime_flag_on`, which treats unset,
empty, `0`, `false`, `off` and `no` as **off**. Matching is case-insensitive and
ignores surrounding whitespace. The Shape and Default columns are unaffected. A
handful of JIT-read names are still presence-parsed on purpose: they carry a
value, or the collector reads the same name through `parse::present`. They are
listed in `jit-presence-only-flag-reads-FIXED.md`. No flag was added, removed or
renamed by that change.

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
| distinct `CRATONVM_*` identifiers appearing anywhere in Rust source | 2,269 |
| exact string literals (i.e. actually named by code, not prose) | 2,162 |
| **declared** in `flag_groups::INVENTORY` + scalars + group variables | **2154** |
| declared before this pass | 576 |
| declared by this pass | **71** |
| allowlisted as intentionally undeclared | 11 |
| user-facing names an operator has to learn | 15 |

The first two rows are **not** generated, though since 2026-09 they ARE
enforced -- `types/tests/doc_numeric_claims.rs::flag_inventory_surface_counts_are_current`
re-derives both and fails when either drifts. Before that test existed nothing
checked them, which is
why they read 692 / 658 from 2026-08-06 until 2026-09-01 while the true figures
were 1,056 / 993 — a gap large enough to make the declared count (986) look
like it *exceeded* the number of flags in the source, which is not possible.
They were re-derived again on 2026-09-01 and had already moved to 1,065 / 1,001
on the same day — and moved twice more within that afternoon while eight new
flags were being registered. That drift rate is the reason the instruction
below is "re-derive", not "quote". The recipe is the one in [How to
regenerate](#how-to-regenerate):

```bash
# row 1 — identifiers anywhere in Rust source
find . -mindepth 1 \( -name target -o -name vendor -o -name node_modules \
  -o -name '.*' \) -prune -o -name '*.rs' -print0 \
  | xargs -0 grep -hoE 'CRATONVM_[A-Z0-9_]+' | sort -u | wc -l
# row 2 — exact string literals under <crate>/src
for d in $(sed -n 's/^members = \[//p' Cargo.toml | tr -d '"[],'); do
  [ -d "$d/src" ] && grep -rhoE '"CRATONVM_[A-Z0-9_]+"' "$d/src"
done | tr -d '"' | sort -u | wc -l
```

The row-1 command prunes rather than globbing `--exclude-dir`, and the
`-mindepth 1` is load-bearing. `--exclude-dir` globs match without
`FNM_PATHNAME`, so `*` crosses `/` and `--exclude-dir='.*'` prunes `./types`
along with `.git` — the form this file and
`types/tests/doc_numeric_claims.rs` carried until 2026-09-11 printed **0** for
the whole repository from `.`, and 1,390 only when given an absolute path. The
bare `-name '.*'` has the same trap: it matches the starting `.`. A regeneration
recipe that answers 0 is worse than no recipe, because 0 looks like a finding.

**What the three rows together say about the surface.** Grouping was a renaming,
not a retirement: 995 declared knobs reached through 15 variables is still 995
knobs. Of the names in `flag-surface.txt`, **488** are named nowhere outside
`types/`, the generated documents and `docs/internal` — i.e. neither
operator-facing nor referenced by CI — and **63** of those have at most one Rust
read site outside the declaration table. That set, not the 15, is the honest
retirement backlog, and it is now written down one row at a time in
[flag-retirement-candidates-20260901.md](flag-retirement-candidates-20260901.md),
which states the commands that produce all three numbers. The counting rule that
matters: a name's read count is its `--include='*.rs'` hits across the member
crates minus its hits in `types/src/flag_groups.rs`, because the registry names
every variable and that is a declaration, not a read.

Every `INVENTORY` row now also carries a `since:` date, taken from `git log`
rather than guessed, and `flag_groups.rs`'s own `mod tests` enforces a
retirement horizon on it: a `DBG` knob declared on or after **2026-08-01** has
to be referenced somewhere outside `types/` and the generated documents.
Rows older than the horizon are grandfathered — they are exactly the backlog
above — so the policy is green today and starts biting on the next flag anyone
adds.

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
| `CRATONVM_GC_CARD_METRICS` | `GC=card-metrics` | truthy word | off |
| `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` | `GC=mirror-pin-young-defer` (off) | presence | capability on |
| `CRATONVM_DBG_OOM_BT`, `CRATONVM_DBG_YOUNG_TRIGGER` | `DBG=oom-bt`, `DBG=young-trigger` | presence | off |

`CRATONVM_GC_CARD_METRICS` is a truthy word since 2026-09-24 (gc-common w6-f):
`=0`/`false`/`off`/`no` leave the barrier counters off, in the live gate
(`gc_metrics.rs::resolve_hot_path_gate`, `runtime_flag_on`, since w2-e) and in
its typed mirror (`GcMetricsConfig::card_metrics`). The gate sits on a barrier that runs on
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
| 1 | `gc/src/gc_metrics.rs` → `resolve_hot_path_gate` | `flags::runtime_flag_on("CRATONVM_GC_CARD_METRICS")` (agrees with the typed mirror since w6-f) | `subsystem_config::gc_metrics().card_metrics` |
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

2165 rows: 2154 declared, 11 allowlisted. Generated by
`tools/flag-census/render-inventory.py` — see
[How to regenerate](#how-to-regenerate).

| Variable | Group | Canonical spelling | Shape | Default | Class | Latched | Read in |
|---|---|---|---|---|---|---|---|
| `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE` | DBG | `CRATONVM_DBG=active-profiles-identity-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_AGENT_BOOT_CLASS_PATH_BOOTSTRAP` | COMPAT | `CRATONVM_COMPAT=agent-boot-class-path-bootstrap` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_ALLOW_JSR_RET` | LOADER | `CRATONVM_LOADER=allow-jsr-ret` | opt-in | off | behaviour | snapshot | classloading, types |
| `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` | SECURITY | `CRATONVM_SECURITY=noncrypto-sslengine` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_ANNOTATION_CARRIER_IS_HANDLER` | COMPAT | `CRATONVM_COMPAT=annotation-carrier-is-handler` | default-on | on | behaviour | snapshot | classloading, native-builtins |
| `CRATONVM_ANNOTATION_FOREIGN_EQUALS` | COMPAT | `CRATONVM_COMPAT=annotation-foreign-equals` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_ANNOTATION_JDK_SEMANTICS` | COMPAT | `CRATONVM_COMPAT=annotation-jdk-semantics` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_ANNOTATION_MEMBER_CONFORMANCE` | COMPAT | `CRATONVM_COMPAT=annotation-member-conformance` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY` | COMPAT | `CRATONVM_COMPAT=annotation-reflective-alias-copy` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_ANN_PROXY_DISPATCH_TRACE` | DBG | `CRATONVM_DBG=ann-proxy-dispatch-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_ANN_TRACE` | DBG | `CRATONVM_DBG=ann-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_AOT_HMAC_KEY` | SECURITY | `CRATONVM_SECURITY=aot-hmac-key` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_ASSERT_SINGLE_OS_THREAD` | THREADS | `CRATONVM_THREADS=assert-single-os-thread` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | THREADS | `CRATONVM_THREADS=async-handoff-sleep-floor-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | THREADS | `CRATONVM_THREADS=async-submit-grace-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | THREADS | `CRATONVM_THREADS=async-worker-sleep-floor-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | THREADS | `CRATONVM_THREADS=await-shortcircuit` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_AWT_EVENTS_REAL_BYTECODE` | COMPAT | `CRATONVM_COMPAT=awt-events-real-bytecode` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_BD_DEBUG` | DBG | `CRATONVM_DBG=bd-debug` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_BEANS_REAL_CHANGE_SUPPORT` | COMPAT | `CRATONVM_COMPAT=beans-real-change-support` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BG_COMPILE` | JIT | `CRATONVM_JIT=bg-compile` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_BIGDECIMAL_ADD_MC_JDK` | JIT | `CRATONVM_JIT=bigdecimal-add-mc-jdk` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE` | JIT | `CRATONVM_JIT=bigdecimal-binary-to-double` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_COMPARE_BY_BITS` | JIT | `CRATONVM_JIT=bigdecimal-compare-by-bits` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT` | JIT | `CRATONVM_JIT=bigdecimal-divide-mc-oneshot` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_EXACT_DIVIDE_BY_FACTORS` | JIT | `CRATONVM_JIT=bigdecimal-exact-divide-by-factors` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_FLOAT_VALUE_NATIVE` | JIT | `CRATONVM_JIT=bigdecimal-float-value-native` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_MC_MODE_BY_NAME` | JIT | `CRATONVM_JIT=bigdecimal-mc-mode-by-name` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_MC_RESULT_CONSTANTS` | JIT | `CRATONVM_JIT=bigdecimal-mc-result-constants` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_NULL_ARG_NPE` | JIT | `CRATONVM_JIT=bigdecimal-null-arg-npe` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_POW_MC_JDK` | JIT | `CRATONVM_JIT=bigdecimal-pow-mc-jdk` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_PRECISION_NO_RENDER` | JIT | `CRATONVM_JIT=bigdecimal-precision-no-render` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_RESULT_CONSTANTS` | JIT | `CRATONVM_JIT=bigdecimal-result-constants` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_TOSTRING_CACHE` | JIT | `CRATONVM_JIT=bigdecimal-tostring-cache` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGDECIMAL_VALUEOF_LONG_COMPACT` | JIT | `CRATONVM_JIT=bigdecimal-valueof-long-compact` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINTEGER_MULADD_WINDOW` | JIT | `CRATONVM_JIT=biginteger-muladd-window` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINTEGER_NEGATE_SHARES_MAG` | JIT | `CRATONVM_JIT=biginteger-negate-shares-mag` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_FAST_MUL` | JIT | `CRATONVM_JIT=bigint-fast-mul` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_FROM_DECIMAL_FAST` | COMPAT | `CRATONVM_COMPAT=bigint-from-decimal-fast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_MAG_TO_DECIMAL_CHUNKED` | COMPAT | `CRATONVM_COMPAT=bigint-mag-to-decimal-chunked` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_P71_LIMB_ROAD` | COMPAT | `CRATONVM_COMPAT=bigint-p71-limb-road` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_STR_HELPERS_LIMB` | COMPAT | `CRATONVM_COMPAT=bigint-str-helpers-limb` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_TODECIMAL_DC` | JIT | `CRATONVM_JIT=bigint-todecimal-dc` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGINT_UNICODE_DIGITS` | COMPAT | `CRATONVM_COMPAT=bigint-unicode-digits` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGNUM_JDK_IDENTITY` | JIT | `CRATONVM_JIT=bignum-jdk-identity` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGNUM_POW5_CACHE` | JIT | `CRATONVM_JIT=bignum-pow5-cache` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIGNUM_STRINGS_UNINTERNED` | JIT | `CRATONVM_JIT=bignum-strings-uninterned` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_BIN` | — | `CRATONVM_BIN` | scalar | unset | behaviour | snapshot | difftest |
| `CRATONVM_BLOCK_PRIVATE_NETS` | SECURITY | `CRATONVM_SECURITY=block-private-nets` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_BOOT_APPEND_DIR_CENSUS` | COMPAT | `CRATONVM_COMPAT=boot-append-dir-census` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_BOOT_MODULE_REGISTRY` | LOADER | `CRATONVM_LOADER=boot-module-registry` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_BYTEBUFFER_INTRINSIC` | REAL | `CRATONVM_REAL=bytebuffer-intrinsic` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_C2_ACCEPT` | JIT | `CRATONVM_JIT=c2-accept` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_C2_ACCEPT_MEMO` | JIT | `CRATONVM_JIT=c2-accept-memo` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_C2_SUPERSEDE` | JIT | `CRATONVM_JIT=c2-supersede` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_CANON_OPENFILE` | IO | `CRATONVM_IO=canon-openfile` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CAPABILITY_GRANTS` | SECURITY | `CRATONVM_SECURITY=capability-grants` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CAPABILITY_LOG` | SECURITY | `CRATONVM_SECURITY=capability-log` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CAPABILITY_MODE` | SECURITY | `CRATONVM_SECURITY=capability-mode` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CARD_TABLE_ONLY` | GC | `CRATONVM_GC=card-table-only` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CENSUS_EXACT_INVOCATIONS` | DBG | `CRATONVM_DBG=census-exact-invocations` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_CF_DELEGATING_YIELD` | LOADER | `CRATONVM_LOADER=cf-delegating-yield` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE` | LOADER | `CRATONVM_LOADER=classpath-jar-unnamed-module` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_CL_BOOTSTRAP_SCOPED` | LOADER | `CRATONVM_LOADER=cl-bootstrap-scoped` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_CL_STUB_DELEGATION` | LOADER | `CRATONVM_LOADER=stub-delegation` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_COMPACT_REF_FIELDS` | GC | `CRATONVM_GC=compact-ref-fields` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPACT_TLAB_ALLOC` | GC | `CRATONVM_GC=compact-tlab-alloc` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_COMPACT_TLAB_SITES` | GC | `CRATONVM_GC=compact-tlab-sites` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_COMPAT` | COMPAT | `CRATONVM_COMPAT=…` | group | unset | — | snapshot | — |
| `CRATONVM_COMPATIBILITY_JDK_ONLY` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | libcratonvm C ABI constant, not a variable |
| `CRATONVM_COMPAT_BOOLEAN_ELEMENT_HASH` | COMPAT | `CRATONVM_COMPAT=compat-boolean-element-hash` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_BOOLEAN_KEY_HASH` | COMPAT | `CRATONVM_COMPAT=compat-boolean-key-hash` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK` | COMPAT | `CRATONVM_COMPAT=compat-chm-cia-single-walk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH` | COMPAT | `CRATONVM_COMPAT=compat-chm-remap-single-hash` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK` | COMPAT | `CRATONVM_COMPAT=compat-chm-remap-single-walk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_SINGLE_HASH` | COMPAT | `CRATONVM_COMPAT=compat-chm-single-hash` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_TABLE_HIGH_WATER` | COMPAT | `CRATONVM_COMPAT=compat-chm-table-high-water` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_VALUE_EQUALS` | COMPAT | `CRATONVM_COMPAT=compat-chm-value-equals` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_COLLECTORS_UNMODIFIABLE` | LOADER | `CRATONVM_LOADER=compat-collectors-unmodifiable` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_COPYOF_REAL_IMMUTABLE_IDENTITY` | LOADER | `CRATONVM_LOADER=compat-copyof-real-immutable-identity` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_ENTRY_SET_NODES` | COMPAT | `CRATONVM_COMPAT=compat-entry-set-nodes` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_ENTRY_SET_VALUE_IN_PLACE` | COMPAT | `CRATONVM_COMPAT=compat-entry-set-value-in-place` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_HASHMAP_TREE_BINS` | COMPAT | `CRATONVM_COMPAT=compat-hashmap-tree-bins` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_HASHSET_BUILD_JDK` | COMPAT | `CRATONVM_COMPAT=compat-hashset-build-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY` | COMPAT | `CRATONVM_COMPAT=compat-hashset-build-treeify` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY_LEVELS` | COMPAT | `CRATONVM_COMPAT=compat-hashset-build-treeify-levels` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_HASHSET_CTOR_JDK` | COMPAT | `CRATONVM_COMPAT=compat-hashset-ctor-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE` | COMPAT | `CRATONVM_COMPAT=compat-itr-remove-by-node` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_JOIN_ZERO_INTERRUPTIBLE` | COMPAT | `CRATONVM_COMPAT=compat-join-zero-interruptible` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_COMPAT_LHM_BUCKET_ORDER` | COMPAT | `CRATONVM_COMPAT=compat-lhm-bucket-order` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_CLEAR_IN_PLACE` | COMPAT | `CRATONVM_COMPAT=compat-lhm-clear-in-place` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_CLONE_FRESH_CAPACITY` | COMPAT | `CRATONVM_COMPAT=compat-lhm-clone-fresh-capacity` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_CTOR_CAPACITY` | COMPAT | `CRATONVM_COMPAT=compat-lhm-ctor-capacity` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_GROW_ON_INSERT` | COMPAT | `CRATONVM_COMPAT=compat-lhm-grow-on-insert` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_LOAD_FACTOR` | COMPAT | `CRATONVM_COMPAT=compat-lhm-load-factor` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REMAP_EVICT_LAST` | COMPAT | `CRATONVM_COMPAT=compat-lhm-remap-evict-last` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REMAP_GROW_ON_ENTRY` | COMPAT | `CRATONVM_COMPAT=compat-lhm-remap-grow-on-entry` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REVERSED_BY_NODE` | COMPAT | `CRATONVM_COMPAT=compat-lhm-reversed-by-node` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES` | LOADER | `CRATONVM_LOADER=compat-lhm-reversed-live-values` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` | LOADER | `CRATONVM_LOADER=compat-lhm-reversed-live-views` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW` | LOADER | `CRATONVM_LOADER=compat-lhm-reversed-real-view` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_REVERSED_SEES_OVERWRITES` | COMPAT | `CRATONVM_COMPAT=compat-lhm-reversed-sees-overwrites` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_SINGLE_HASH` | COMPAT | `CRATONVM_COMPAT=compat-lhm-single-hash` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_TREE_BINS` | COMPAT | `CRATONVM_COMPAT=compat-lhm-tree-bins` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHM_TREE_PUT_IF_ABSENT` | COMPAT | `CRATONVM_COMPAT=compat-lhm-tree-put-if-absent` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHS_NODE_OPS` | COMPAT | `CRATONVM_COMPAT=compat-lhs-node-ops` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW` | LOADER | `CRATONVM_LOADER=compat-lhs-reversed-real-view` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_COMPUTE_JDK` | COMPAT | `CRATONVM_COMPAT=compat-map-compute-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_EQUALS_JDK` | COMPAT | `CRATONVM_COMPAT=compat-map-equals-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_FOREACH_LIVE` | COMPAT | `CRATONVM_COMPAT=compat-map-foreach-live` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_JDK_ORDER` | COMPAT | `CRATONVM_COMPAT=compat-map-jdk-order` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_LOAD_FACTOR` | COMPAT | `CRATONVM_COMPAT=compat-map-load-factor` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_LONG_CHAINS` | COMPAT | `CRATONVM_COMPAT=compat-map-long-chains` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_REMAP_CME` | COMPAT | `CRATONVM_COMPAT=compat-map-remap-cme` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_MAP_SINGLE_WALK` | COMPAT | `CRATONVM_COMPAT=compat-map-single-walk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_OPTIONAL_REAL_BYTECODE` | LOADER | `CRATONVM_LOADER=compat-optional-real-bytecode` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_OVERLAY_MATERIALIZE_CAP` | COMPAT | `CRATONVM_COMPAT=compat-overlay-materialize-cap` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_OVERLAY_REMAP_ORDER` | COMPAT | `CRATONVM_COMPAT=compat-overlay-remap-order` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_SET_BULK_JDK` | COMPAT | `CRATONVM_COMPAT=compat-set-bulk-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_SET_OF_START_TABLE` | COMPAT | `CRATONVM_COMPAT=compat-set-of-start-table` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER` | COMPAT | `CRATONVM_COMPAT=compat-treemap-value-equals-order` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_VALUES_REMOVE_JDK` | COMPAT | `CRATONVM_COMPAT=compat-values-remove-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_VIEW_BUILD_BY_NODE` | COMPAT | `CRATONVM_COMPAT=compat-view-build-by-node` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE` | COMPAT | `CRATONVM_COMPAT=compat-view-remove-by-node` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPRESSED_OOPS` | GC | `CRATONVM_GC=compressed-oops` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_COMPUTED_STRINGS_UNINTERNED` | JIT | `CRATONVM_JIT=computed-strings-uninterned` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CONFINE_IO` | SECURITY | `CRATONVM_SECURITY=confine-io` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CRC32C_RANGE_CHECK` | COMPAT | `CRATONVM_COMPAT=crc32c-range-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_DBG` | DBG | `CRATONVM_DBG=…` | group | unset | — | snapshot | native-builtins, types |
| `CRATONVM_DBG_A2` | DBG | `CRATONVM_DBG=a2` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_A5_CENSUS` | DBG | `CRATONVM_DBG=a5-census` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_A5_ENGAGEMENT` | DBG | `CRATONVM_DBG=a5-engagement` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_A5_FALLBACK` | DBG | `CRATONVM_DBG=a5-fallback` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_AASTORE_BARRIER_GATE` | DBG | `CRATONVM_DBG=aastore-barrier-gate-sites` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_ABOVE_CHAIN_KB` | DBG | `CRATONVM_DBG=above-chain-kb` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ACCESS` | DBG | `CRATONVM_DBG=access` | opt-in | off | diag | snapshot | native-builtins, types |
| `CRATONVM_DBG_AFC_PENDING_FUTURE` | DBG | `CRATONVM_DBG=afc-pending-future` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_AIO` | DBG | `CRATONVM_DBG=aio` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_AIOOBE` | DBG | `CRATONVM_DBG=aioobe` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_AIOOBE2` | DBG | `CRATONVM_DBG=aioobe2` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_AIOOBE3` | DBG | `CRATONVM_DBG=aioobe3` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_AIO_INLINE` | DBG | `CRATONVM_DBG=aio-inline` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_ALTRACE` | DBG | `CRATONVM_DBG=altrace` | opt-in | off | diag | snapshot | jit, native-collections, vm |
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
| `CRATONVM_DBG_CALLBACK_MEMO` | DBG | `CRATONVM_DBG=callback-memo` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CALLEE_DEOPT` | DBG | `CRATONVM_DBG=callee-deopt` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CALLEE_PROBE` | DBG | `CRATONVM_DBG=callee-probe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CALLER` | DBG | `CRATONVM_DBG=caller` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CAPVAL` | DBG | `CRATONVM_DBG=capval` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CARRIER` | DBG | `CRATONVM_DBG=carrier` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CAST_MEMO_CROSSCHECK` | DBG | `CRATONVM_DBG=cast-memo-crosscheck` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CATALINA` | DBG | `CRATONVM_DBG=catalina` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_CAUSE` | DBG | `CRATONVM_DBG=cause` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CCE` | DBG | `CRATONVM_DBG=cce` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CCECACHE` | DBG | `CRATONVM_DBG=ccecache` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CCE_BT` | DBG | `CRATONVM_DBG=cce-bt` | opt-in | off | diag | snapshot | native-collections, types, vm |
| `CRATONVM_DBG_CCSPROBE` | DBG | `CRATONVM_DBG=ccsprobe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CELLCORRUPT` | DBG | `CRATONVM_DBG=cellcorrupt` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_CHARSET` | DBG | `CRATONVM_DBG=charset` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CHECKCAST_INLINE` | DBG | `CRATONVM_DBG=checkcast-inline` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_CHECK_OVERRIDE` | DBG | `CRATONVM_DBG=check-override` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CLASSPATH` | DBG | `CRATONVM_DBG=classpath` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CLASS_RESOURCE` | DBG | `CRATONVM_DBG=class-resource` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_CLINIT_FAIL` | DBG | `CRATONVM_DBG=clinit-fail` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CLINIT_ORDER` | DBG | `CRATONVM_DBG=clinit-order` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CLONE` | DBG | `CRATONVM_DBG=clone` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CMCTX` | DBG | `CRATONVM_DBG=cmctx` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_CODE_NEAR_GLOBALS` | DBG | `CRATONVM_DBG=code-near-globals` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_COERCE` | DBG | `CRATONVM_DBG=coerce` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COERCION` | DBG | `CRATONVM_DBG=coercion` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_COLL_REFRESH` | DBG | `CRATONVM_DBG=coll-refresh` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_COMPACTVALUE` | DBG | `CRATONVM_DBG=compactvalue` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COMPACT_INLINE` | DBG | `CRATONVM_DBG=compact-inline` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_COMPACT_LEGACY` | DBG | `CRATONVM_DBG=compact-legacy` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_COMPACT_TLAB` | GC | `CRATONVM_GC=dbg-compact-tlab` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DBG_COMPONENT_TYPE` | DBG | `CRATONVM_DBG=component-type` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_CORRUPT_CELL` | DBG | `CRATONVM_DBG=corrupt-cell` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_CORRUPT_CELL_SELFTEST` | DBG | `CRATONVM_DBG=corrupt-cell-selftest` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CORRUPT_FRAMES` | DBG | `CRATONVM_DBG=corrupt-frames` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_CTOR_FIX` | DBG | `CRATONVM_DBG=ctor-fix` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DBB_ELEM` | DBG | `CRATONVM_DBG=dbb-elem` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_DEADRECV` | DBG | `CRATONVM_DBG=deadrecv` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DEADREF_STORE` | DBG | `CRATONVM_DBG=deadref-store` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_DEFINE` | DBG | `CRATONVM_DBG=define` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DEFINE_CENSUS` | DBG | `CRATONVM_DBG=define-census` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DEFINE_FILTER` | DBG | `CRATONVM_DBG=define-filter` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_DBG_DEFINE_STACK_FILTER` | DBG | `CRATONVM_DBG=define-stack-filter` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_DEFLATE` | DBG | `CRATONVM_DBG=deflate` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DEOPT` | DBG | `CRATONVM_DBG=deopt` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_DBG_DEOPTSLOT` | DBG | `CRATONVM_DBG=deoptslot` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DESCTRACE` | DBG | `CRATONVM_DBG=desctrace` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_DIAL_DOORS` | DBG | `CRATONVM_DBG=dial-doors` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DIRECT_BINDS` | DBG | `CRATONVM_DBG=direct-binds` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DISPATCH_TALLY` | DBG | `CRATONVM_DBG=dispatch-tally` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_DM` | DBG | `CRATONVM_DBG=direct-memory` | opt-in | off | diag | snapshot | gc, native-io, vm |
| `CRATONVM_DBG_DOPRIV` | DBG | `CRATONVM_DBG=dopriv` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_DROPPED_PUTFIELD` | DBG | `CRATONVM_DBG=dropped-putfield` | opt-in | off | diag | snapshot | vm |
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
| `CRATONVM_DBG_EPOCH_CELL` | DBG | `CRATONVM_DBG=epoch-cell` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_EQE` | DBG | `CRATONVM_DBG=eqe` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EXCFRAME` | DBG | `CRATONVM_DBG=excframe` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_EXEC` | DBG | `CRATONVM_DBG=exec` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EXIT` | DBG | `CRATONVM_DBG=exit` | opt-in | off | diag | snapshot | types, vm-cli |
| `CRATONVM_DBG_FBCGLIB` | DBG | `CRATONVM_DBG=fbcglib` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FBREF` | DBG | `CRATONVM_DBG=fbref` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FFM` | DBG | `CRATONVM_DBG=ffm` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_FIELDADDR` | DBG | `CRATONVM_DBG=fieldaddr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FIELD_DESCRIPTOR` | DBG | `CRATONVM_DBG=field-descriptor` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FIELD_GET` | DBG | `CRATONVM_DBG=field-get` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FIELD_PHASES` | DBG | `CRATONVM_DBG=field-phases` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FIELD_SITE` | DBG | `CRATONVM_DBG=field-site` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FIELD_WATCH` | DBG | `CRATONVM_DBG=field-watch` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_FINCAND` | DBG | `CRATONVM_DBG=fincand` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_FINDCLASS` | DBG | `CRATONVM_DBG=findclass` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_FMT_WRONGTYPE` | DBG | `CRATONVM_DBG=fmt-wrongtype` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_FORCE_MOVING` | DBG | `CRATONVM_DBG=force-moving` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FSP` | DBG | `CRATONVM_DBG=fsp` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FULLSTACK_SCAN` | DBG | `CRATONVM_DBG=fullstack-scan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_FWDGUARD` | DBG | `CRATONVM_DBG=fwdguard` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_FWDWALK` | DBG | `CRATONVM_DBG=fwdwalk` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_G1ACCESSOR` | DBG | `CRATONVM_DBG=g1accessor` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_G1DIAG` | DBG | `CRATONVM_DBG=g1diag` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_G1_LIVE_MEMO` | DBG | `CRATONVM_DBG=g1-live-memo` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_G1_OBJADDR` | DBG | `CRATONVM_DBG=g1-objaddr` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_GCPART` | DBG | `CRATONVM_DBG=gcpart` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_GCPAUSE` | DBG | `CRATONVM_DBG=gcpause` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GCPAUSE_MIN_US` | DBG | `CRATONVM_DBG=gcpause-min-us` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GCPHASE` | DBG | `CRATONVM_DBG=gcphase` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GCWRITE` | DBG | `CRATONVM_DBG=gcwrite` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_GC_FALLBACK_REASONS` | DBG | `CRATONVM_DBG=gc-fallback-reasons` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_GC_OVERHEAD` | DBG | `CRATONVM_DBG=gc-overhead` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_GC_STRESS` | DBG | `CRATONVM_DBG=gc-stress` | opt-in | off | diag | snapshot | difftest, gc, types |
| `CRATONVM_DBG_GC_TRIGGER_VERIFY` | DBG | `CRATONVM_DBG=gc-trigger-verify` | opt-in | off | diag | snapshot | gc |
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
| `CRATONVM_DBG_INTERP_FRAMES` | DBG | `CRATONVM_DBG=interp-frames` | opt-in | off | diag | snapshot | vm |
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
| `CRATONVM_DBG_IR_COMPILES` | DBG | `CRATONVM_DBG=ir-compiles` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_IR_ENTRY_FOLD` | DBG | `CRATONVM_DBG=ir-entry-fold` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_GRAPH` | DBG | `CRATONVM_DBG=ir-graph` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_ISEL` | DBG | `CRATONVM_DBG=ir-isel` | opt-in | off | diag | snapshot | jit, vm-cli |
| `CRATONVM_DBG_IR_LINEAR_SCAN` | DBG | `CRATONVM_DBG=ir-linear-scan` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_LONG` | DBG | `CRATONVM_DBG=ir-long` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_PARAM_FILL` | DBG | `CRATONVM_DBG=ir-param-fill` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_POISON_INT_HIGH` | DBG | `CRATONVM_DBG=ir-poison-int-high` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_REF_STORE_TRACE` | JIT | `CRATONVM_JIT=dbg-ir-ref-store-trace` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_DBG_IR_RELOC` | DBG | `CRATONVM_DBG=ir-reloc` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_SELF_CALL_ANSWER` | DBG | `CRATONVM_DBG=ir-self-call-answer` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_SINK` | DBG | `CRATONVM_DBG=ir-sink` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_SLOTS` | DBG | `CRATONVM_DBG=ir-slots` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_IR_STRING` | DBG | `CRATONVM_DBG=ir-string` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_ISINSTANCE` | DBG | `CRATONVM_DBG=isinstance` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ISOLATED_CNF` | DBG | `CRATONVM_DBG=isolated-cnf` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JAR` | DBG | `CRATONVM_DBG=jar` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_JCA_GETINSTANCE` | DBG | `CRATONVM_DBG=jca-getinstance` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_JETTY` | DBG | `CRATONVM_DBG=jetty` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_JETTY2` | DBG | `CRATONVM_DBG=jetty2` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JITC` | DBG | `CRATONVM_DBG=jitc` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JITNPE` | DBG | `CRATONVM_DBG=jitnpe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_ALLOC` | DBG | `CRATONVM_DBG=jit-alloc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_BORROW_SITES` | DBG | `CRATONVM_DBG=jit-borrow-sites` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_CODE` | DBG | `CRATONVM_DBG=jit-code` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JIT_CODE_FREE` | DBG | `CRATONVM_DBG=jit-code-free` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_COMPILED` | DBG | `CRATONVM_DBG=jit-compiled` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_DIRECT_BINDS` | DBG | `CRATONVM_DBG=jit-direct-binds` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_DISASM` | DBG | `CRATONVM_DBG=jit-disasm` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_DISPATCH` | DBG | `CRATONVM_DBG=jit-dispatch` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_EA` | DBG | `CRATONVM_DBG=jit-ea` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_ELIDE_CTOR` | DBG | `CRATONVM_DBG=jit-elide-ctor` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_ENTRY` | DBG | `CRATONVM_DBG=jit-entry` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_FIELD_SITES` | DBG | `CRATONVM_DBG=jit-field-sites` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_GEN` | DBG | `CRATONVM_DBG=jit-gen` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_LDC` | DBG | `CRATONVM_DBG=jit-ldc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_LOCALS_FLOOR` | DBG | `CRATONVM_DBG=jit-locals-floor` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_METHOD_STATS` | DBG | `CRATONVM_DBG=jit-method-stats` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_DBG_JIT_MIC` | DBG | `CRATONVM_DBG=jit-mic` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_NAMES` | DBG | `CRATONVM_DBG=jit-names` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JIT_PIN` | DBG | `CRATONVM_DBG=jit-pin` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_PUTFIELD` | DBG | `CRATONVM_DBG=jit-putfield` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_REF_LOADS` | DBG | `CRATONVM_DBG=jit-ref-loads` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_ROOTSCAN` | DBG | `CRATONVM_DBG=jit-rootscan` | opt-in | off | diag | snapshot | gc, vm |
| `CRATONVM_DBG_JIT_SAFEPOINTS` | DBG | `CRATONVM_DBG=jit-safepoints` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_SCAN_PROF` | DBG | `CRATONVM_DBG=jit-scan-prof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_SLOT_OVERLAP` | DBG | `CRATONVM_DBG=jit-slot-overlap` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_SPIN_WARN` | DBG | `CRATONVM_DBG=jit-spin-warn` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_STALE_AFTER_REMAP` | DBG | `CRATONVM_DBG=jit-stale-after-remap` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_JIT_STALE_BELOW_RBP` | DBG | `CRATONVM_DBG=jit-stale-below-rbp` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JIT_STALE_IC` | DBG | `CRATONVM_DBG=jit-stale-ic` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_UNMAP` | DBG | `CRATONVM_DBG=jit-unmap` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JLM` | DBG | `CRATONVM_DBG=jlm` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_JNI_LOCALREF` | DBG | `CRATONVM_DBG=jni-localref` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JNI_PHASE` | DBG | `CRATONVM_DBG=jni-phase` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_JRT_MODULES` | DBG | `CRATONVM_DBG=jrt-modules` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_JRT_READ_COUNT` | DBG | `CRATONVM_DBG=jrt-read-count` | opt-in | off | diag | snapshot | native-builtins |
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
| `CRATONVM_DBG_LOAD_CSE` | DBG | `CRATONVM_DBG=load-cse` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_LOAD_TRANSFORM_NO_MEMO` | DBG | `CRATONVM_DBG=load-transform-no-memo` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LOGPROV` | DBG | `CRATONVM_DBG=logprov` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LONGROOT` | DBG | `CRATONVM_DBG=longroot` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_LOOKUP` | DBG | `CRATONVM_DBG=lookup` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_LOOP_WORK` | DBG | `CRATONVM_DBG=loop-work` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MAPGEN` | DBG | `CRATONVM_DBG=mapgen` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MAPPER` | DBG | `CRATONVM_DBG=mapper` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_MAP_MISS_AUDIT` | DBG | `CRATONVM_DBG=map-miss-audit` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_MAP_VIEW_CACHE` | DBG | `CRATONVM_DBG=map-view-cache` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_MARKCLEAR` | DBG | `CRATONVM_DBG=markclear` | opt-in | off | diag | snapshot | gc |
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
| `CRATONVM_DBG_MONITOR_CONTENTION` | DBG | `CRATONVM_DBG=monitor-contention` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MONITOR_NOTIFY` | DBG | `CRATONVM_DBG=monitor-notify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MSC` | DBG | `CRATONVM_DBG=msc` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MTROOTS` | DBG | `CRATONVM_DBG=mtroots` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK` | DBG | `CRATONVM_DBG=nativelibraries-load-ok` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_NATIVE_ENTRY` | DBG | `CRATONVM_DBG=native-entry` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NATIVE_LOOKUPS` | DBG | `CRATONVM_DBG=native-lookups` | opt-in | off | diag | snapshot | native-api |
| `CRATONVM_DBG_NATIVE_SHADOW` | DBG | `CRATONVM_DBG=native-shadow` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NCDFE` | DBG | `CRATONVM_DBG=ncdfe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NET` | DBG | `CRATONVM_DBG=net` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_NETTY_QUEUE` | DBG | `CRATONVM_DBG=netty-queue` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_NEXTINT` | DBG | `CRATONVM_DBG=nextint` | opt-in | off | diag | snapshot | native-collections, types |
| `CRATONVM_DBG_NIO_BIND` | DBG | `CRATONVM_DBG=nio-bind` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_NOCODE` | DBG | `CRATONVM_DBG=nocode` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NO_CLEANERS` | DBG | `CRATONVM_DBG=cleaners` | opt-out | on | diag | snapshot | vm |
| `CRATONVM_DBG_NO_JIT_ROOT_SCAN` | DBG | `CRATONVM_DBG=jit-root-scan` | opt-out | on | diag | snapshot | vm |
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
| `CRATONVM_DBG_NULL_FIELD_PROVENANCE` | DBG | `CRATONVM_DBG=null-field-provenance` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_NULL_NATIVE` | DBG | `CRATONVM_DBG=null-native` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OBJECTS` | DBG | `CRATONVM_DBG=objects` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OBJKEY` | DBG | `CRATONVM_DBG=objkey` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_OBJ_EQUALS` | DBG | `CRATONVM_DBG=obj-equals` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OBJ_WATCH` | DBG | `CRATONVM_DBG=obj-watch` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_OBSREG` | DBG | `CRATONVM_DBG=obsreg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OLDMARK_ROOT_CENSUS` | DBG | `CRATONVM_DBG=oldmark-root-census` | opt-in | off | diag | snapshot | gc, vm |
| `CRATONVM_DBG_OLDSWEEP_OWNERS` | DBG | `CRATONVM_DBG=oldsweep-owners` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY` | DBG | `CRATONVM_DBG=old-live-sweep-verify` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_OLD_PLANT_WALK_BREAK` | DBG | `CRATONVM_DBG=old-plant-walk-break` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_OOBFIELD` | DBG | `CRATONVM_DBG=oobfield` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_OOM_BT` | DBG | `CRATONVM_DBG=oom-bt` | opt-in | off | diag | snapshot | gc, vm |
| `CRATONVM_DBG_OOPCOV` | DBG | `CRATONVM_DBG=oopcov` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE` | DBG | `CRATONVM_DBG=oop-oracle-force-refute` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR` | DBG | `CRATONVM_DBG=osr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR_BIND` | DBG | `CRATONVM_DBG=osr-bind` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR_FRAME_TRACE` | DBG | `CRATONVM_DBG=osr-frame-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_OSR_META` | DBG | `CRATONVM_DBG=osr-meta` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OSR_SEED_COLLISION` | DBG | `CRATONVM_DBG=osr-seed-collision` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OSR_SLOTS` | DBG | `CRATONVM_DBG=osr-slots` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_OVERLAY` | DBG | `CRATONVM_DBG=overlay` | opt-in | off | diag | snapshot | classloading, gc, native-builtins, types, vm |
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
| `CRATONVM_DBG_PEER_REG_PAIRING` | DBG | `CRATONVM_DBG=peer-reg-pairing` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_PICOCLI_STYLE` | DBG | `CRATONVM_DBG=picocli-style` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_POPINT` | DBG | `CRATONVM_DBG=popint` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_PRECISE` | DBG | `CRATONVM_DBG=precise` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_PROMOTE_REFUSE` | DBG | `CRATONVM_DBG=promote-refuse` | opt-in | off | diag | snapshot | vm |
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
| `CRATONVM_DBG_REFLECTION_FACTORY` | DBG | `CRATONVM_DBG=reflection-factory` | opt-in | off | diag | snapshot | native-builtins, types, vm |
| `CRATONVM_DBG_REFPROC_AUDIT` | DBG | `CRATONVM_DBG=refproc-audit` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REFPROC_REMARK` | DBG | `CRATONVM_DBG=refproc-remark` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_RELOCATION_BLOCKERS` | DBG | `CRATONVM_DBG=relocation-blockers` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REMAP_RESIDUE` | DBG | `CRATONVM_DBG=remap-residue` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REMAP_TRACE` | DBG | `CRATONVM_DBG=remap-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_REPLOVR` | DBG | `CRATONVM_DBG=replovr` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RESOLVE_SHIM` | DBG | `CRATONVM_DBG=resolve-shim` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RESOURCE_TIMING` | DBG | `CRATONVM_DBG=resource-timing` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RESUME_PC` | DBG | `CRATONVM_DBG=resume-pc` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_RETRANSFORM` | DBG | `CRATONVM_DBG=retransform` | opt-in | off | diag | snapshot | native-builtins, vm |
| `CRATONVM_DBG_ROOTFIXUP` | DBG | `CRATONVM_DBG=rootfixup` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTPROF` | DBG | `CRATONVM_DBG=rootprof` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTSNAP` | DBG | `CRATONVM_DBG=rootsnap` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTSNAP_EVERY` | DBG | `CRATONVM_DBG=rootsnap-every` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOTSNAP_VERIFY` | DBG | `CRATONVM_DBG=rootsnap-verify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOT_REMAP_AUDIT` | DBG | `CRATONVM_DBG=root-remap-audit` | opt-in | off | diag | snapshot | gc, vm |
| `CRATONVM_DBG_ROOT_SOURCE` | DBG | `CRATONVM_DBG=root-source` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_ROOT_WRITE_AUDIT` | DBG | `CRATONVM_DBG=root-write-audit` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_RSET_AUDIT` | DBG | `CRATONVM_DBG=rset-audit` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN` | DBG | `CRATONVM_DBG=rset-audit-young-scan` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_RTERR` | DBG | `CRATONVM_DBG=rterr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_RVAS` | DBG | `CRATONVM_DBG=rvas` | opt-in | off | diag | snapshot | libcratonvm |
| `CRATONVM_DBG_SBLOAD` | DBG | `CRATONVM_DBG=sbload` | opt-in | off | diag | snapshot | native-collections, types |
| `CRATONVM_DBG_SCALAR_DEOPT` | DBG | `CRATONVM_DBG=scalar-deopt` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_SCALAR_NEW` | DBG | `CRATONVM_DBG=scalar-new` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_SC_CLOSE` | DBG | `CRATONVM_DBG=sc-close` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SC_CLOSE_PHASES` | DBG | `CRATONVM_DBG=sc-close-phases` | opt-in | off | diag | snapshot | native-io |
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
| `CRATONVM_DBG_SP_REF_STORE_TRACE` | JIT | `CRATONVM_JIT=dbg-sp-ref-store-trace` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_DBG_STACKLESS` | DBG | `CRATONVM_DBG=stackless` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STACK_KINDS` | DBG | `CRATONVM_DBG=stack-kinds` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_STALELONG` | DBG | `CRATONVM_DBG=stalelong` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STALE_FRAME_WORDS` | JIT | `CRATONVM_JIT=dbg-stale-frame-words` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DBG_STALE_OBJREF` | DBG | `CRATONVM_DBG=stale-objref` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_STALE_OBJREF_CYCLES` | DBG | `CRATONVM_DBG=stale-objref-cycles` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_STALE_RECV` | DBG | `CRATONVM_DBG=stale-recv` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STAMPED` | DBG | `CRATONVM_DBG=stamped` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_STATIC_SLOT_VERIFY` | DBG | `CRATONVM_DBG=static-slot-verify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STRAYSTACK` | DBG | `CRATONVM_DBG=straystack` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STREAMSUPP` | DBG | `CRATONVM_DBG=streamsupp` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_STTRACE` | DBG | `CRATONVM_DBG=sttrace` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_STUBLOADER` | DBG | `CRATONVM_DBG=stubloader` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STUB_BT` | DBG | `CRATONVM_DBG=stub-bt` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_DBG_STUB_DOOR` | DBG | `CRATONVM_DBG=stub-door` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STUB_YIELD` | DBG | `CRATONVM_DBG=stub-yield` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STW_CENSUS` | DBG | `CRATONVM_DBG=stw-census` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STW_EXPECTED_IDS` | DBG | `CRATONVM_DBG=stw-expected-ids` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_STW_NATIVE_RING` | DBG | `CRATONVM_DBG=stw-native-ring` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SWCHAIN` | DBG | `CRATONVM_DBG=swchain` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SWEEP_CENSUS` | DBG | `CRATONVM_DBG=sweep-census` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_EDGES` | DBG | `CRATONVM_DBG=sweep-edges` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_LIVENESS` | DBG | `CRATONVM_DBG=sweep-liveness` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_REFERRERS` | DBG | `CRATONVM_DBG=sweep-referrers` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_SWEEP_TRACE_CLASS` | DBG | `CRATONVM_DBG=sweep-trace-class` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_SWEEP_ZERO` | DBG | `CRATONVM_DBG=sweep-zero` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_THREADREG_PERF` | DBG | `CRATONVM_DBG=threadreg-perf` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_THREADSTART` | DBG | `CRATONVM_DBG=threadstart` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_TIERUP_DECLINE` | DBG | `CRATONVM_DBG=tierup-decline` | opt-in | off | diag | snapshot | vm |
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
| `CRATONVM_DBG_UNREG_DECLINED` | DBG | `CRATONVM_DBG=unreg-declined` | opt-in | off | diag | snapshot | vm |
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
| `CRATONVM_DBG_VERIFY_REG_OOP_MAPS` | DBG | `CRATONVM_DBG=verify-reg-oop-maps` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_VIEWKIND` | DBG | `CRATONVM_DBG=view-kind` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_VIEWRESYNC` | DBG | `CRATONVM_DBG=view-resync` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_VIEW_COMOD` | DBG | `CRATONVM_DBG=view-comod` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_DBG_VISITFILE` | DBG | `CRATONVM_DBG=visitfile` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_VM_STATE` | DBG | `CRATONVM_DBG=vm-state` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WATCHADDR` | DBG | `CRATONVM_DBG=watchaddr` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WATCHREF` | DBG | `CRATONVM_DBG=watchref` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_DBG_WATCH_ADDR` | DBG | `CRATONVM_DBG=watch-addr` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_WATCH_ALLOC_CID` | DBG | `CRATONVM_DBG=watch-alloc-cid` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WATCH_CAUSE_SELF` | DBG | `CRATONVM_DBG=watch-cause-self` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_WATCH_CELL` | DBG | `CRATONVM_DBG=watch-cell` | opt-in | off | diag | snapshot | gc, types |
| `CRATONVM_DBG_WATCH_PUN` | DBG | `CRATONVM_DBG=watch-pun` | opt-in | off | diag | snapshot | gc, vm |
| `CRATONVM_DBG_WEAKREF` | DBG | `CRATONVM_DBG=weakref` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WF` | DBG | `CRATONVM_DBG=wf` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_WF_NPE` | DBG | `CRATONVM_DBG=wf-npe` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_WRITE0_TRACE` | DBG | `CRATONVM_DBG=write0-trace` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_DBG_XNIO_TCP` | DBG | `CRATONVM_DBG=xnio-tcp` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_XT_COVERAGE` | DBG | `CRATONVM_DBG=xt-coverage` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_XT_FORCE_TAKEOVER` | DBG | `CRATONVM_DBG=xt-force-takeover` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_XT_JIT_ROOT_SCAN` | DBG | `CRATONVM_DBG=xt-jit-root-scan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_YOUNGSCAN` | DBG | `CRATONVM_DBG=youngscan` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_YOUNGSTATE` | DBG | `CRATONVM_DBG=youngstate` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_YOUNG_MARK_WATCH` | DBG | `CRATONVM_DBG=young-mark-watch` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_YOUNG_TRIGGER` | DBG | `CRATONVM_DBG=young-trigger` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZERO_RANGES` | DBG | `CRATONVM_DBG=zero-ranges` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ZGC_CORPSE` | DBG | `CRATONVM_DBG=zgc-corpse` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZGC_HIGH` | DBG | `CRATONVM_DBG=zgc-high` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZGC_TARGET` | DBG | `CRATONVM_DBG=zgc-target` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZGC_VERIFY_SLIDE` | DBG | `CRATONVM_DBG=zgc-verify-slide` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_DBG_ZIPIMMUNE` | DBG | `CRATONVM_DBG=zip-immune` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DEBUG_SFI` | DBG | `CRATONVM_DBG=debug-sfi` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DEBUG_STACKWALK` | DBG | `CRATONVM_DBG=debug-stackwalk` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DEBUG_STACK_TAG` | DBG | `CRATONVM_DBG=debug-stack-tag` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DEFAULT_HEAP_ERGONOMICS` | GC | `CRATONVM_GC=default-heap-ergonomics` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEFAULT_HEAP_MAX_MB` | GC | `CRATONVM_GC=default-heap-max-mb` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DEFAULT_WATCHDOG_SEC` | THREADS | `CRATONVM_THREADS=default-watchdog-sec` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_DEOPT_CALLSITE_OWN_SOURCE` | JIT | `CRATONVM_JIT=deopt-callsite-own-source` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CALLSITE_STASH_TEMPLATE` | JIT | `CRATONVM_JIT=deopt-callsite-stash-template` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_CALLSITE_BY_POINT` | JIT | `CRATONVM_JIT=deopt-chain-callsite-by-point` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE` | JIT | `CRATONVM_JIT=deopt-chain-inner-own-source` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_DEOPT_CHAIN_INNER_REDEFINED_REFUSES` | JIT | `CRATONVM_JIT=deopt-chain-inner-redefined-refuses` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC` | JIT | `CRATONVM_JIT=deopt-chain-last-instr-pc` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE` | JIT | `CRATONVM_JIT=deopt-chain-outer-own-source` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_OUTER_REDEFINED_REFUSES` | JIT | `CRATONVM_JIT=deopt-chain-outer-redefined-refuses` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS` | JIT | `CRATONVM_JIT=deopt-chain-own-source-sinks` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST` | JIT | `CRATONVM_JIT=deopt-chain-run-from-outermost` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_EAGER` | DBG | `CRATONVM_DBG=deopt-eager` | opt-in | off | diag | snapshot | difftest, jit |
| `CRATONVM_DEOPT_EAGER_BCI` | DBG | `CRATONVM_DBG=deopt-eager-bci` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DEOPT_EAGER_CHAINS` | DBG | `CRATONVM_DBG=deopt-eager-chains` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DEOPT_REAL` | JIT | `CRATONVM_JIT=deopt-real` | default-on | on | behaviour | snapshot | difftest, jit |
| `CRATONVM_DEOPT_REMATERIALISE_OOM` | JIT | `CRATONVM_JIT=deopt-rematerialise-oom` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_RESTASH_KEEPS_SOURCE` | JIT | `CRATONVM_JIT=deopt-restash-keeps-source` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_RESUME_CURRENT_BODY_OF_REDEFINED` | JIT | `CRATONVM_JIT=deopt-resume-current-body-of-redefined` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_RESUME_OBSOLETE_ACTIVATION` | JIT | `CRATONVM_JIT=deopt-resume-obsolete-activation` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_SYNC_VIRTUALS_RESUME` | JIT | `CRATONVM_JIT=deopt-sync-virtuals-resume` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_DEOPT_TIERUP_SINK_CP_STAMP` | JIT | `CRATONVM_JIT=deopt-tierup-sink-cp-stamp` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_UNRESUMED_STASH_RELEASES` | JIT | `CRATONVM_JIT=deopt-unresumed-stash-releases` | default-on | on | behaviour | snapshot | vm |
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
| `CRATONVM_DISABLE_ARRAYLEN_LICM` | JIT | `CRATONVM_JIT=arraylen-licm` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_DISABLE_DEFAULT_WATCHDOG` | THREADS | `CRATONVM_THREADS=default-watchdog` | opt-out | on | behaviour | snapshot | vm-cli |
| `CRATONVM_DISABLE_INTRINSICS` | JIT | `CRATONVM_JIT=intrinsics` | opt-out | on | behaviour | snapshot | difftest, vm, vm-cli |
| `CRATONVM_DISABLE_JAR_MMAP` | LOADER | `CRATONVM_LOADER=jar-mmap` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_DISABLE_JIT` | — | `CRATONVM_DISABLE_JIT` | scalar | unset | behaviour | snapshot | difftest, jit, types, vm-cli |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | JIT | `CRATONVM_JIT=scalar-replacement` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_DISABLE_UNROLL` | JIT | `CRATONVM_JIT=unroll` | both | off | behaviour | snapshot | jit |
| `CRATONVM_DUMP_THREADS_UNFORMATTED_ELEMENTS` | COMPAT | `CRATONVM_COMPAT=dump-threads-unformatted-elements` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_EAGER_STREAMS` | COMPAT | `CRATONVM_COMPAT=eager-streams` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_ENABLE_ASSERTIONS` | — | `CRATONVM_ENABLE_ASSERTIONS` | scalar | unset | behaviour | snapshot | types, vm-cli |
| `CRATONVM_ENABLE_NATIVE_RING` | DBG | `CRATONVM_DBG=enable-native-ring` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_ENFORCE_NATIVE_SHADOW` | LOADER | `CRATONVM_LOADER=enforce-native-shadow` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_ENUM_ORDINAL_BY_NAME` | JIT | `CRATONVM_JIT=enum-ordinal-by-name` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_EQE_SYNC_EXECUTE` | THREADS | `CRATONVM_THREADS=eqe-sync-execute` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_EXC_MESSAGE_PROPAGATE` | COMPAT | `CRATONVM_COMPAT=exc-message-propagate` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_EXC_TOSTRING_LOCALIZED` | COMPAT | `CRATONVM_COMPAT=exc-tostring-localized` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_EXEC_DEPTH_CEILING` | THREADS | `CRATONVM_THREADS=exec-depth-ceiling` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_EXEC_FRAME_TRACE` | DBG | `CRATONVM_DBG=exec-frame-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_FABRICATED_PROVIDER_MAP` | COMPAT | `CRATONVM_COMPAT=fabricated-provider-map` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FC_FAST_IO` | JIT | `CRATONVM_JIT=fc-fast-io` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_FC_FAST_IO_STATS` | DBG | `CRATONVM_DBG=fc-fast-io-stats` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_FFM_ACCESS_HANDSHAKE` | JIT | `CRATONVM_JIT=ffm-access-handshake` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ACQUIRE_HANDSHAKE` | JIT | `CRATONVM_JIT=ffm-acquire-handshake` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ACTION_LIST_LINKED` | JIT | `CRATONVM_JIT=ffm-action-list-linked` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE` | JIT | `CRATONVM_JIT=ffm-address-global-scope` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ADDRESS_READ_TARGET` | JIT | `CRATONVM_JIT=ffm-address-read-target` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ADDRESS_STORE_VALUE_FIRST` | JIT | `CRATONVM_JIT=ffm-address-store-value-first` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ADD_OR_CLEANUP_RUNS` | JIT | `CRATONVM_JIT=ffm-add-or-cleanup-runs` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ARENA_CLOSE_FREES` | JIT | `CRATONVM_JIT=ffm-arena-close-frees` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ARENA_NOINIT_CARRIER` | JIT | `CRATONVM_JIT=ffm-arena-noinit-carrier` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_ARRAY_ELEMENT_VAR_HANDLE` | JIT | `CRATONVM_JIT=ffm-array-element-var-handle` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_AUTO_ARENA_ACTION_FREE` | JIT | `CRATONVM_JIT=ffm-auto-arena-action-free` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_AUTO_ARENA_BLOCK_FREE` | JIT | `CRATONVM_JIT=ffm-auto-arena-block-free` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_AUTO_ARENA_CLEANER` | JIT | `CRATONVM_JIT=ffm-auto-arena-cleaner` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_BOUNDS_BEFORE_SCOPE` | JIT | `CRATONVM_JIT=ffm-bounds-before-scope` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_BULK_HEAP_SEGMENTS` | JIT | `CRATONVM_JIT=ffm-bulk-heap-segments` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_BULK_SCOPE_CHECK` | JIT | `CRATONVM_JIT=ffm-bulk-scope-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CARRIER_SCOPE_SESSION` | JIT | `CRATONVM_JIT=ffm-carrier-scope-session` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CHAR_RETURN_UNSIGNED` | JIT | `CRATONVM_JIT=ffm-char-return-unsigned` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CHECK_LAYOUTS_GROUPS` | JIT | `CRATONVM_JIT=ffm-check-layouts-groups` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CLOSE_RETIRES_VERDICTS` | JIT | `CRATONVM_JIT=ffm-close-retires-verdicts` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CLOSE_RUNS_CLEANUP` | JIT | `CRATONVM_JIT=ffm-close-runs-cleanup` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CLOSE_STATE_CAS` | JIT | `CRATONVM_JIT=ffm-close-state-cas` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_CLOSE_WALK_HOTSPOT` | JIT | `CRATONVM_JIT=ffm-close-walk-hotspot` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DEFAULT_LOOKUP_C_RUNTIME` | JIT | `CRATONVM_JIT=ffm-default-lookup-c-runtime` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DEFAULT_LOOKUP_SYSLOOKUP` | JIT | `CRATONVM_JIT=ffm-default-lookup-syslookup` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DOWNCALL_ACQUIRE` | JIT | `CRATONVM_JIT=ffm-downcall-acquire` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DOWNCALL_CHECK_LAYOUTS` | JIT | `CRATONVM_JIT=ffm-downcall-check-layouts` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DOWNCALL_GC_SAFE` | JIT | `CRATONVM_JIT=ffm-downcall-gc-safe` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DOWNCALL_RETURN_ALLOCATOR` | JIT | `CRATONVM_JIT=ffm-downcall-return-allocator` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DOWNCALL_SCRATCH_FREE_ON_ERROR` | JIT | `CRATONVM_JIT=ffm-downcall-scratch-free-on-error` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_GLOBAL_SESSION_ADD_NOOP` | JIT | `CRATONVM_JIT=ffm-global-session-add-noop` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_GROUP_MEMBERS_BY_NAME` | JIT | `CRATONVM_JIT=ffm-group-members-by-name` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_GROUP_MEMBERS_UNWRAP` | JIT | `CRATONVM_JIT=ffm-group-members-unwrap` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LAYOUT_ORDER` | JIT | `CRATONVM_JIT=ffm-layout-order` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LAYOUT_RENDER_GROUP_ALIGN` | JIT | `CRATONVM_JIT=ffm-layout-render-group-align` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LAYOUT_VH_ENCLOSING_BOUNDS` | JIT | `CRATONVM_JIT=ffm-layout-vh-enclosing-bounds` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LAYOUT_VH_INDEX_BOUND` | JIT | `CRATONVM_JIT=ffm-layout-vh-index-bound` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LAYOUT_VH_READ_ONLY` | JIT | `CRATONVM_JIT=ffm-layout-vh-read-only` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LAYOUT_VH_VALUE_LEAF` | JIT | `CRATONVM_JIT=ffm-layout-vh-value-leaf` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_LOOKUP_SYMBOL_WRITABLE` | JIT | `CRATONVM_JIT=ffm-lookup-symbol-writable` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_NATIVE_ALIGN_CHECK` | JIT | `CRATONVM_JIT=ffm-native-align-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_NULL_ADDRESS_NPE` | JIT | `CRATONVM_JIT=ffm-null-address-npe` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_NULL_ADDRESS_NPE_MESSAGE` | JIT | `CRATONVM_JIT=ffm-null-address-npe-message` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_OPAQUE_LOW_POINTER_ARGS` | JIT | `CRATONVM_JIT=ffm-opaque-low-pointer-args` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_REAL_SESSION_FAST_CHECK` | JIT | `CRATONVM_JIT=ffm-real-session-fast-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_REINTERPRET_CHECKS` | JIT | `CRATONVM_JIT=ffm-reinterpret-checks` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_SEGMENT_VH_CHECKS` | JIT | `CRATONVM_JIT=ffm-segment-vh-checks` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_SEGMENT_VH_FORM` | JIT | `CRATONVM_JIT=ffm-segment-vh-form` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_SESSION_ADD_ATOMIC` | JIT | `CRATONVM_JIT=ffm-session-add-atomic` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` | JIT | `CRATONVM_JIT=ffm-shared-arena-close-frees` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_SHARED_RECORD_PER_THREAD` | JIT | `CRATONVM_JIT=ffm-shared-record-per-thread` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_SYNTHETIC_ARENA_CLOSED_CHECK` | JIT | `CRATONVM_JIT=ffm-synthetic-arena-closed-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UNION_EIGHTBYTE_CLASS` | JIT | `CRATONVM_JIT=ffm-union-eightbyte-class` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_ADDRESS_ALIGN_CHECK` | JIT | `CRATONVM_JIT=ffm-upcall-address-align-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_ATTACH` | JIT | `CRATONVM_JIT=ffm-upcall-attach` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_AUTO_ARENA_FREE` | JIT | `CRATONVM_JIT=ffm-upcall-auto-arena-free` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_BY_VALUE_GROUPS` | JIT | `CRATONVM_JIT=ffm-upcall-by-value-groups` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_CHECK_EXCEPTIONS` | JIT | `CRATONVM_JIT=ffm-upcall-check-exceptions` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_CHECK_LAYOUTS` | JIT | `CRATONVM_JIT=ffm-upcall-check-layouts` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_CREATE_WINDOW` | JIT | `CRATONVM_JIT=ffm-upcall-create-window` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_CROSS_VM` | JIT | `CRATONVM_JIT=ffm-upcall-cross-vm` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_CROSS_VM_GC_SAFE_WAIT` | JIT | `CRATONVM_JIT=ffm-upcall-cross-vm-gc-safe-wait` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_DIRECT_STATIC` | JIT | `CRATONVM_JIT=ffm-upcall-direct-static` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_RETIRE_AT_CLOSE` | JIT | `CRATONVM_JIT=ffm-upcall-retire-at-close` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_STUBS` | JIT | `CRATONVM_JIT=ffm-upcall-stubs` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_TYPE_CHECK` | JIT | `CRATONVM_JIT=ffm-upcall-type-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_TYPE_CHECK_OBJECT` | JIT | `CRATONVM_JIT=ffm-upcall-type-check-object` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_UPCALL_UNCAUGHT_JAVA_ERR` | JIT | `CRATONVM_JIT=ffm-upcall-uncaught-java-err` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_VERDICT_WAYS` | JIT | `CRATONVM_JIT=ffm-verdict-ways` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_VH_ADDRESS_HEAP_REFUSAL` | JIT | `CRATONVM_JIT=ffm-vh-address-heap-refusal` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FIELD_RESOLUTION_NAME_ONLY` | COMPAT | `CRATONVM_COMPAT=field-resolution-name-only` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FINALIZER_THREAD` | GC | `CRATONVM_GC=finalizer-thread` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FJP_EAGER_FORK` | THREADS | `CRATONVM_THREADS=fjp-eager-fork` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_FORCED_FINALIZERS` | GC | `CRATONVM_GC=forced-finalizers` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_FORCE_WIN_BUILD` | TEST | `CRATONVM_TEST=force-win-build` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FOREIGN_ATTACH` | COMPAT | `CRATONVM_COMPAT=foreign-attach` | opt-in | off | behaviour | snapshot | libcratonvm, vm |
| `CRATONVM_FORNAME_TRACE` | DBG | `CRATONVM_DBG=forname-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_FP_SPECIAL_LITERALS` | JIT | `CRATONVM_JIT=fp-special-literals` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_FRAME_TRACE` | DBG | `CRATONVM_DBG=frame-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_FUZZ_BOOTCP` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | cargo-fuzz target |
| `CRATONVM_FWD_RESOLVE_STRICT` | LOADER | `CRATONVM_LOADER=fwd-resolve-strict` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_G1_ADAPTIVE_IHOP` | GC | `CRATONVM_GC=g1-adaptive-ihop` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_ADAPTIVE_TENURING` | GC | `CRATONVM_GC=g1-adaptive-tenuring` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_ALLOC_MARK_DRIVE` | GC | `CRATONVM_GC=g1-alloc-mark-drive` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_BAND_REJECT_PINS` | GC | `CRATONVM_GC=g1-band-reject-pins` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_G1_BAND_REJECT_PINS_ALL` | GC | `CRATONVM_GC=g1-band-reject-pins-all` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_G1_BLOCK_OFFSETS` | GC | `CRATONVM_GC=g1-block-offsets` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_BLOCK_OFFSET_AUDIT` | GC | `CRATONVM_GC=g1-block-offset-audit` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_CARD_CLEAN` | GC | `CRATONVM_GC=g1-card-clean` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_G1_CARD_CURSOR` | GC | `CRATONVM_GC=g1-card-cursor` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_CARD_RSET` | GC | `CRATONVM_GC=g1-card-rset` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` | GC | `CRATONVM_GC=g1-card-screen-jit-pinned` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_G1_CLEANUP_HONOURS_JIT_PINS` | GC | `CRATONVM_GC=g1-cleanup-honours-jit-pins` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_CLEANUP_RESET_TAMS` | GC | `CRATONVM_GC=g1-cleanup-reset-tams` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_CLEANUP_SATB_DISCARD` | GC | `CRATONVM_GC=g1-cleanup-satb-discard` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_CLEANUP_SCRUB` | GC | `CRATONVM_GC=g1-cleanup-scrub` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_CLEANUP_WALK` | GC | `CRATONVM_GC=g1-cleanup-walk` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_COVERAGE_PIN` | GC | `CRATONVM_GC=g1-coverage-pin` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_DBG_GRAY_PROV` | DBG | `CRATONVM_DBG=g1-dbg-gray-prov` | opt-in | off | diag | snapshot | gc |
| `CRATONVM_G1_DBG_HEADERS` | DBG | `CRATONVM_DBG=g1-dbg-headers` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_PINS` | DBG | `CRATONVM_DBG=g1-dbg-pins` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_REACH` | DBG | `CRATONVM_DBG=g1-dbg-reach` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_ROOTCENSUS` | DBG | `CRATONVM_DBG=g1-dbg-rootcensus` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_RSET` | DBG | `CRATONVM_DBG=g1-dbg-rset` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DBG_ZERO` | DBG | `CRATONVM_DBG=g1-dbg-zero` | opt-in | off | diag | snapshot | types |
| `CRATONVM_G1_DRAIN_NOT_A_PAUSE` | GC | `CRATONVM_GC=g1-drain-not-a-pause` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_EAGER_HUMONGOUS` | GC | `CRATONVM_GC=g1-eager-humongous` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_EDEN_STRIPES` | GC | `CRATONVM_GC=g1-eden-stripes` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_CANDIDATE_ARENA_SCREEN` | GC | `CRATONVM_GC=g1-evac-candidate-arena-screen` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_CENSUS` | GC | `CRATONVM_GC=g1-evac-census` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_EVAC_COPY_WATCH` | GC | `CRATONVM_GC=g1-evac-copy-watch` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_COST_FROM_YOUNG` | GC | `CRATONVM_GC=g1-evac-cost-from-young` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_EVAC_COST_REAL` | GC | `CRATONVM_GC=g1-evac-cost-real` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_EVAC_DEST_LATCH` | GC | `CRATONVM_GC=g1-evac-dest-latch` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_EVAC_EMPTY_HEADER_GRID_PROOF` | GC | `CRATONVM_GC=g1-evac-empty-header-grid-proof` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_HEADROOM_TRIGGER` | GC | `CRATONVM_GC=g1-evac-headroom-trigger` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_EVAC_LOCAL_QUEUE` | GC | `CRATONVM_GC=g1-evac-local-queue` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` | GC | `CRATONVM_GC=g1-evac-ref-implausible-refuse` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_SHARE_CENSUS` | GC | `CRATONVM_GC=g1-evac-share-census` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_SUPPLY_SCREEN` | GC | `CRATONVM_GC=g1-evac-supply-screen` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_FORCE_FULL_FRESH_CYCLE` | GC | `CRATONVM_GC=g1-force-full-fresh-cycle` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_G1_FREE_CENSUS_ON_RECLAIM` | GC | `CRATONVM_GC=g1-free-census-on-reclaim` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_FREE_TRIGGER_YOUNG_FLOOR` | GC | `CRATONVM_GC=g1-free-trigger-young-floor` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_HEAP_RESIZE` | GC | `CRATONVM_GC=g1-heap-resize` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_HUMONGOUS_BEST_FIT` | GC | `CRATONVM_GC=g1-humongous-best-fit` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_HUMONGOUS_MARKS` | GC | `CRATONVM_GC=g1-humongous-marks` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_G1_HUMONGOUS_RUN_GUARD` | GC | `CRATONVM_GC=g1-humongous-run-guard` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_IHOP_BACKOFF` | GC | `CRATONVM_GC=g1-ihop-backoff` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_IHOP_COUNTS_REGIONS` | GC | `CRATONVM_GC=g1-ihop-counts-regions` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_IHOP_GROSS_GROWTH` | GC | `CRATONVM_GC=g1-ihop-gross-growth` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_IHOP_POLL_GATE` | GC | `CRATONVM_GC=g1-ihop-poll-gate` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_INLINE_BARRIER` | GC | `CRATONVM_GC=g1-inline-barrier` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_JIT_MARK_DRIVER` | GC | `CRATONVM_GC=g1-jit-mark-driver` | opt-in | off | behaviour | snapshot | gc, vm |
| `CRATONVM_G1_LATE_HEADER_WRITE` | GC | `CRATONVM_GC=g1-late-header-write` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_MARK_BACKOFF_DEADLINE` | GC | `CRATONVM_GC=g1-mark-backoff-deadline` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_MARK_CAP_SEED_SCREEN` | GC | `CRATONVM_GC=g1-mark-cap-seed-screen` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_MARK_CONVERGENCE_EPOCH` | GC | `CRATONVM_GC=g1-mark-convergence-epoch` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG` | GC | `CRATONVM_GC=g1-mark-gray-requires-header-flag` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_MARK_LOCK_YIELD` | GC | `CRATONVM_GC=g1-mark-lock-yield` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_MARK_OOB_FAILSAFE` | GC | `CRATONVM_GC=g1-mark-oob-failsafe` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_MARK_ROOT_SCREEN` | GC | `CRATONVM_GC=g1-mark-root-screen` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_MARK_SIDE_TABLES` | GC | `CRATONVM_GC=g1-mark-side-tables` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_MIXED_FUTILE_GUARD` | GC | `CRATONVM_GC=g1-mixed-futile-guard` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_NARROW_DRAIN_FIXUP` | GC | `CRATONVM_GC=g1-narrow-drain-fixup` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_NARROW_FIXUP` | GC | `CRATONVM_GC=g1-narrow-fixup` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_NO_EVAC_RETRY` | GC | `CRATONVM_GC=g1-evac-retry` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_G1_NO_LIVE_REGION_MEMO` | GC | `CRATONVM_GC=g1-live-region-memo` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_G1_OVERFLOW_RESCAN_EDEN_BITMAP` | GC | `CRATONVM_GC=g1-overflow-rescan-eden-bitmap` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_PARALLEL_EVAC` | GC | `CRATONVM_GC=g1-parallel-evac` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_EVAC_IN_JIT` | GC | `CRATONVM_GC=g1-parallel-evac-in-jit` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST` | GC | `CRATONVM_GC=g1-parallel-evac-resume-dest` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_EVAC_SCREEN` | GC | `CRATONVM_GC=g1-parallel-evac-screen` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST` | GC | `CRATONVM_GC=g1-parallel-evac-shared-dest` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_MARK` | GC | `CRATONVM_GC=g1-parallel-mark` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_MIXED` | GC | `CRATONVM_GC=g1-parallel-mixed` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_PARALLEL_SEED` | GC | `CRATONVM_GC=g1-parallel-seed` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_PAUSE_COST_MODEL` | GC | `CRATONVM_GC=g1-pause-cost-model` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_PINS_HONOUR_ANY_PUBLICATION` | GC | `CRATONVM_GC=g1-pins-honour-any-publication` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_PINS_HONOUR_UNREGISTERED_FRAME` | GC | `CRATONVM_GC=g1-pins-honour-unregistered-frame` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_PIN_EMPTY_PUBLICATION` | GC | `CRATONVM_GC=g1-pin-empty-publication` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_PRECISE_ONLY_ROOTS` | GC | `CRATONVM_GC=g1-precise-only-roots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_G1_REEVAC_GUARD` | GC | `CRATONVM_GC=g1-reevac-guard` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_REF_WRITE_WATCH` | GC | `CRATONVM_GC=g1-ref-write-watch` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_REMARK_SEEDS_JIT_PINS` | GC | `CRATONVM_GC=g1-remark-seeds-jit-pins` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_RESERVE_HEAP` | GC | `CRATONVM_GC=g1-reserve-heap` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_RETIRE_FORWARDS_LATE` | GC | `CRATONVM_GC=g1-retire-forwards-late` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_RSET_DEAD_HOLDER_FILTER` | GC | `CRATONVM_GC=g1-rset-dead-holder-filter` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_RSET_PRUNE_ON_SCAN` | GC | `CRATONVM_GC=g1-rset-prune-on-scan` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_RSET_SOURCE_CAP` | GC | `CRATONVM_GC=g1-rset-source-cap` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_SCRUB_FREE` | GC | `CRATONVM_GC=g1-scrub-free` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN` | GC | `CRATONVM_GC=g1-serial-evac-holder-screen` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_SHARED_ALLOC` | GC | `CRATONVM_GC=g1-shared-alloc` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_TAKEOVER_LICENCE` | GC | `CRATONVM_GC=g1-takeover-licence` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_TLAB_CLAMP` | GC | `CRATONVM_GC=g1-tlab-clamp` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_G1_UNCOMMIT` | GC | `CRATONVM_GC=g1-uncommit` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_VERIFY_BUDGET` | GC | `CRATONVM_GC=g1-verify-budget` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_VERIFY_FORWARDS_RETIRED` | GC | `CRATONVM_GC=g1-verify-forwards-retired` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_VERIFY_HOLDERS` | GC | `CRATONVM_GC=g1-verify-holders` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_VERIFY_STALE_FORWARD` | DBG | `CRATONVM_DBG=g1-verify-stale-forward` | default-on | on | diag | snapshot | gc |
| `CRATONVM_G1_VERIFY_SWEEP_PAUSES` | GC | `CRATONVM_GC=g1-verify-sweep-pauses` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_G1_WORKERS` | GC | `CRATONVM_GC=g1-workers` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_YOUNG_PAUSE_TARGET` | GC | `CRATONVM_GC=g1-young-pause-target` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GC` | GC | `CRATONVM_GC=…` | group | unset | — | snapshot | types |
| `CRATONVM_GC_ADAPTIVE_TENURING` | GC | `CRATONVM_GC=adaptive-tenuring` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_ARRAY_GUARD_BT` | DBG | `CRATONVM_DBG=gc-array-guard-bt` | opt-in | off | diag | snapshot | types |
| `CRATONVM_GC_CALLEE_SAVED_IMAGE_LIVENESS` | GC | `CRATONVM_GC=callee-saved-image-liveness` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_CARD_METRICS` | GC | `CRATONVM_GC=card-metrics` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_CARD_SUMMARY` | GC | `CRATONVM_GC=card-summary` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_CONC_START_PERCENT` | GC | `CRATONVM_GC=conc-start-percent` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` | GC | `CRATONVM_GC=conc-walk-gap-recovery` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH` | GC | `CRATONVM_GC=conditional-tlab-skip-publish` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_DEAD_SPILL_ROOTS` | GC | `CRATONVM_GC=dead-spill-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS` | GC | `CRATONVM_GC=deopt-image-residue-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS` | GC | `CRATONVM_GC=full-gc-true-roots` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE` | GC | `CRATONVM_GC=full-gc-true-roots-wide` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_FULL_RSET_SCAN` | GC | `CRATONVM_GC=full-rset-scan` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_FUTILE_YOUNG_BACKOFF` | GC | `CRATONVM_GC=futile-young-backoff` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_G1_MOVABLE_PINS` | GC | `CRATONVM_GC=g1-movable-pins` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_G1_ONLY_JIT_PINS` | GC | `CRATONVM_GC=g1-only-jit-pins` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_IR_PRIM_SLOT_ROOTS` | GC | `CRATONVM_GC=ir-prim-slot-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_JIT_REF_STORE_GATES` | GC | `CRATONVM_GC=jit-ref-store-gates` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_LATCHED_GRACE` | GC | `CRATONVM_GC=latched-grace` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_LATE_RESOLVE_DROPPED` | GC | `CRATONVM_GC=late-resolve-dropped` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_LAYOUT_SCAN_CACHE` | GC | `CRATONVM_GC=layout-scan-cache` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_MOVABLE_BAND_ROOTS` | GC | `CRATONVM_GC=movable-band-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD` | GC | `CRATONVM_GC=moving-major-jit-guard` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN` | GC | `CRATONVM_GC=noflag-deposit-skip-jit-scan` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_ARRAY_AUTOBOX_LATCH` | GC | `CRATONVM_GC=array-autobox-latch` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_BAND_MAP_LIVENESS` | GC | `CRATONVM_GC=band-map-liveness` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP` | GC | `CRATONVM_GC=blocked-peer-stack-remap` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_CALLEE_RESOLVE` | GC | `CRATONVM_GC=innermost-callee-resolve` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_CM_ID_PAIRING` | GC | `CRATONVM_GC=cm-id-pairing` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_CONCURRENT_FIRST` | GC | `CRATONVM_GC=concurrent-first` | opt-out | on | behaviour | snapshot | gc, types |
| `CRATONVM_GC_NO_EMPTY_OBJECT_RUN` | GC | `CRATONVM_GC=empty-object-run` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE` | GC | `CRATONVM_GC=frame-trace-span-retire` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_OLD_INTERIOR_PINS` | GC | `CRATONVM_GC=old-interior-pins` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_PEER_PIN_DIVERT` | GC | `CRATONVM_GC=peer-pin-divert` | opt-out | on | behaviour | snapshot | gc, vm |
| `CRATONVM_GC_NO_REFPROC_PREPASS` | GC | `CRATONVM_GC=refproc-prepass` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GC_NO_TLAB_SKIP` | GC | `CRATONVM_GC=tlab-skip` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_NO_VALIDATE_ONCE` | GC | `CRATONVM_GC=validate-once` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_GC_OBJECT_STARTS` | GC | `CRATONVM_GC=object-starts` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GC_OLD_BORROW_YOUNG` | GC | `CRATONVM_GC=old-borrow-young` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_OLD_BOT` | GC | `CRATONVM_GC=old-bot` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_FRAG_COMPACT` | GC | `CRATONVM_GC=old-frag-compact` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_GIVE_BACK` | GC | `CRATONVM_GC=old-give-back` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_HUMONGOUS_TOP` | GC | `CRATONVM_GC=old-humongous-top` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_INTERIOR_DECOMMIT` | GC | `CRATONVM_GC=old-interior-decommit` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_LIVE_SWEEP` | GC | `CRATONVM_GC=old-live-sweep` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GC_OLD_OOM_COMPACT` | GC | `CRATONVM_GC=old-oom-compact` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GC_OLD_PINNED_COMPACT` | GC | `CRATONVM_GC=old-pinned-compact` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_SHRINK` | GC | `CRATONVM_GC=old-shrink` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` | GC | `CRATONVM_GC=old-shrink-after-concurrent` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` | GC | `CRATONVM_GC=old-trigger-hysteresis` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GC_OLD_WALK_GAP_RECOVERY` | GC | `CRATONVM_GC=old-walk-gap-recovery` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OUTGOING_ARG_ROOTS` | GC | `CRATONVM_GC=outgoing-arg-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_OVERHEAD_LIMIT` | GC | `CRATONVM_GC=overhead-limit` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_OVERHEAD_PROGRESS` | GC | `CRATONVM_GC=overhead-progress` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_PAR_EVAC` | GC | `CRATONVM_GC=par-evac` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GC_PAR_EVAC_CARD_SEED` | GC | `CRATONVM_GC=par-evac-card-seed` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_PAR_MIN_BYTES` | GC | `CRATONVM_GC=par-min-bytes` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_PAR_THREADS` | GC | `CRATONVM_GC=par-threads` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_PREALLOCATED_OOME_KINDS` | GC | `CRATONVM_GC=preallocated-oome-kinds` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_PRECISE_ARRAY_CARDS` | GC | `CRATONVM_GC=precise-array-cards` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD` | GC | `CRATONVM_GC=precise-array-header-card` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GC_PRECISE_ONLY_ROOTS` | GC | `CRATONVM_GC=precise-only-roots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED` | GC | `CRATONVM_GC=refill-trigger-unjudged` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GC_REG_OOP_MAPS` | GC | `CRATONVM_GC=reg-oop-maps` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_RESERVE` | GC | `CRATONVM_GC=gc-reserve` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_SP_LOCAL_MAP_ROOTS` | GC | `CRATONVM_GC=sp-local-map-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_STAGED_ARGS_KEEP_MASK` | GC | `CRATONVM_GC=staged-args-keep-mask` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_GC_STATIC_ROOT_SLOTS` | GC | `CRATONVM_GC=static-root-slots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_STATS` | DBG | `CRATONVM_DBG=gc-stats` | opt-in | off | diag | snapshot | native-builtins, vm-cli |
| `CRATONVM_GC_STREAM_REFRESH_EACH` | GC | `CRATONVM_GC=stream-refresh-each` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_GC_STRESS` | GC | `CRATONVM_GC=stress` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` | GC | `CRATONVM_GC=sweep-anchor-stride` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_SYNC_YOUNG_WIPE` | GC | `CRATONVM_GC=sync-young-wipe` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` | GC | `CRATONVM_GC=system-gc-moving-young` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GC_TRIGGER_LOCKFREE` | GC | `CRATONVM_GC=trigger-lockfree` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_VERIFY_RSET` | GC | `CRATONVM_GC=verify-rset` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_VERIFY_STALE` | DBG | `CRATONVM_DBG=gc-verify-stale` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_GC_YOUNG_DRAIN_BLOCKED` | GC | `CRATONVM_GC=young-drain-blocked` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_YOUNG_PAUSE_MS` | GC | `CRATONVM_GC=young-pause-goal-ms` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` | GC | `CRATONVM_GC=young-trigger-percent` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` | GC | `CRATONVM_GC=gen-conc-alloc-fail-door` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_CONC_CLASS_UNLOAD` | GC | `CRATONVM_GC=gen-conc-class-unload` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_CONC_INLINE_START` | GC | `CRATONVM_GC=gen-conc-inline-start` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK` | GC | `CRATONVM_GC=gen-conc-mark-local-stack` | opt-out | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_CONC_MARK_SLICE` | GC | `CRATONVM_GC=gen-conc-mark-slice` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` | GC | `CRATONVM_GC=gen-conc-mark-tams-starts` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN` | GC | `CRATONVM_GC=gen-conc-frozen-eager-scan` | opt-out | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_CONC_PRECEDENCE` | GC | `CRATONVM_GC=gen-conc-precedence` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` | GC | `CRATONVM_GC=gen-conc-remark-refproc-hook` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_CONC_SERVICE_THREAD` | GC | `CRATONVM_GC=gen-conc-service-thread` | opt-in | off | behaviour | snapshot | types, vm |
| `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` | GC | `CRATONVM_GC=gen-humongous-ref-zero-unlocked` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED` | GC | `CRATONVM_GC=gen-humongous-zero-unlocked` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE` | GC | `CRATONVM_GC=gen-pinned-tail-window-live` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GEN_PINNED_YOUNG_COPY` | GC | `CRATONVM_GC=gen-pinned-young-copy` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL` | GC | `CRATONVM_GC=gen-pinned-young-copy-parallel` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER` | GC | `CRATONVM_GC=gen-pinned-young-copy-takeover` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` | GC | `CRATONVM_GC=gen-precise-root-promote` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GEN_PROMOTE_LIVE_FINALIZABLES` | GC | `CRATONVM_GC=gen-promote-live-finalizables` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GEN_SATB_OLD_ONLY` | GC | `CRATONVM_GC=gen-satb-old-only` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` | GC | `CRATONVM_GC=gen-sweep-trigger-own-goal` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GEN_TLAB_TAIL_SINK` | GC | `CRATONVM_GC=gen-tlab-tail-sink` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_UNCOMMIT` | GC | `CRATONVM_GC=gen-uncommit` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS` | GC | `CRATONVM_GC=gen-uncommit-keep-survivors` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_GEN_WEDGED_YOUNG_TRIGGER` | GC | `CRATONVM_GC=gen-wedged-young-trigger` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GEN_XMS_USABLE_FIRST` | GC | `CRATONVM_GC=gen-xms-usable-first` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_Y2O_LIVE_SEED` | GC | `CRATONVM_GC=gen-y2o-live-seed` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_GEN_YOUNG_MIRROR_DEFER` | GC | `CRATONVM_GC=gen-young-mirror-defer` | default-on | on | behaviour | snapshot | gc, vm |
| `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` | GC | `CRATONVM_GC=gen-young-pin-ledger-term4` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GEN_ZERO_ONCE` | GC | `CRATONVM_GC=gen-zero-once` | default-on | on | behaviour | snapshot | gc, types |
| `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` | GC | `CRATONVM_GC=gen-zero-once-free-list` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GETRESOURCE_FIRST_HIT` | COMPAT | `CRATONVM_COMPAT=getresource-first-hit` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_GPU_ADMIT_MODEL` | GC | `CRATONVM_GC=gpu-admit-model` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_APPROX_MATH` | JIT | `CRATONVM_JIT=gpu-approx-math` | opt-in | off | behaviour | snapshot | jit-cuda |
| `CRATONVM_GPU_CHUNKS` | GC | `CRATONVM_GC=gpu-chunks` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_CHUNK_STREAMS` | GC | `CRATONVM_GC=gpu-chunk-streams` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_CRITICAL_LEASE_MS` | GC | `CRATONVM_GC=gpu-critical-lease-ms` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_CRITICAL_WAIT_MS` | GC | `CRATONVM_GC=gpu-critical-wait-ms` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_DEVICE_POOL` | GC | `CRATONVM_GC=gpu-device-pool` | default-on | on | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_DISPATCH_MEMO` | JIT | `CRATONVM_JIT=gpu-dispatch-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GPU_DISPATCH_STREAMS` | GC | `CRATONVM_GC=gpu-dispatch-streams` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_DUMP_PTX` | DBG | `CRATONVM_DBG=gpu-dump-ptx` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_GPU_HOST_CALLBACK` | GC | `CRATONVM_GC=gpu-host-callback` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_IF_CONVERT` | JIT | `CRATONVM_JIT=gpu-if-convert` | opt-in | off | behaviour | snapshot | jit-cuda |
| `CRATONVM_GPU_IF_CONVERT_MAX_OPS` | JIT | `CRATONVM_JIT=gpu-if-convert-max-ops` | opt-in | off | behaviour | snapshot | jit-cuda |
| `CRATONVM_GPU_JIT_ARRAY_WRITERS` | GC | `CRATONVM_GC=gpu-jit-array-writers` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_JIT_GATE_CALLERS` | GC | `CRATONVM_GC=gpu-jit-gate-callers` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_JIT_GATE_DISPATCHABLE` | GC | `CRATONVM_GC=gpu-jit-gate-dispatchable` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_JIT_GATE_LATE_REGISTER` | GC | `CRATONVM_GC=gpu-jit-gate-late-register` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GPU_MIN_WORK_GIVEUP` | GC | `CRATONVM_GC=gpu-min-work-giveup` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_NO_SUBMISSION_DRAIN` | GC | `CRATONVM_GC=gpu-submission-drain` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GPU_NO_ZEROCOPY` | GC | `CRATONVM_GC=gpu-zerocopy` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_GPU_TIME_DISPATCH` | DBG | `CRATONVM_DBG=gpu-time-dispatch` | opt-in | off | diag | snapshot | native-builtins, types |
| `CRATONVM_GPU_TRACE_BYTES` | DBG | `CRATONVM_DBG=gpu-trace-bytes` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_GPU_WAIT_LATCH` | GC | `CRATONVM_GC=gpu-wait-latch` | default-on | on | behaviour | snapshot | cuda-bridge |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | SECURITY | `CRATONVM_SECURITY=harden-manifest-classpath` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_HELPFUL_NPE_OPCODES` | JIT | `CRATONVM_JIT=helpful-npe-opcodes` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_HM_TRACE` | DBG | `CRATONVM_DBG=hm-trace` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_HS_ITR_DBG` | DBG | `CRATONVM_DBG=hs-itr-dbg` | opt-in | off | diag | snapshot | native-collections |
| `CRATONVM_HTTPSRV_KEEPALIVE` | IO | `CRATONVM_IO=httpsrv-keepalive` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_HTTP_MAX_BODY` | IO | `CRATONVM_IO=http-max-body` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_IAE_TRACE` | DBG | `CRATONVM_DBG=iae-trace` | opt-in | off | diag | snapshot | types, vm |
| `CRATONVM_IAE_TRACE2` | DBG | `CRATONVM_DBG=iae-trace2` | opt-in | off | diag | snapshot | types |
| `CRATONVM_IDENTITY_HASH_EVICT` | GC | `CRATONVM_GC=identity-hash-evict` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INDY_CALLSITE_CACHE` | JIT | `CRATONVM_JIT=indy-callsite-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_INHERIT_THREAD_CCL` | THREADS | `CRATONVM_THREADS=inherit-thread-ccl` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INHERIT_TL_WORKAROUND` | THREADS | `CRATONVM_THREADS=inherit-tl-workaround` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INLINE_ALLOW_STATIC` | JIT | `CRATONVM_JIT=inline-allow-static` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_INSTRUMENT_CENSUS` | DBG | `CRATONVM_DBG=instrument-census` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_INTRINSIC_STATS` | DBG | `CRATONVM_DBG=intrinsic-stats` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_INVOKESTATIC_LOADER_TRACE` | DBG | `CRATONVM_DBG=invokestatic-loader-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_INVOKE_CACHE_PC_KEY` | JIT | `CRATONVM_JIT=invoke-cache-pc-key` | opt-in | off | behaviour | snapshot | classloading |
| `CRATONVM_INVOKE_CACHE_STATS` | DBG | `CRATONVM_DBG=invoke-cache-stats` | opt-in | off | diag | snapshot | classloading |
| `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` | DBG | `CRATONVM_DBG=invoke-virtual-entry-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_IO` | IO | `CRATONVM_IO=…` | group | unset | — | snapshot | — |
| `CRATONVM_IR_DEOPT_RESUME` | JIT | `CRATONVM_JIT=ir-deopt-resume` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_ITR_BYTECODE` | REAL | `CRATONVM_REAL=itr-bytecode` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_JAR_ARCHIVE_CACHE_CAP` | IO | `CRATONVM_IO=jar-archive-cache-cap` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JAVA_HOME` | — | `CRATONVM_JAVA_HOME` | scalar | unset | behaviour | snapshot | libcratonvm, native-builtins, types, vm, vm-cli |
| `CRATONVM_JBOSS_BOOT_LOG_FILE` | COMPAT | `CRATONVM_COMPAT=jboss-boot-log-file` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JBOSS_BRUTE_FORCE_JARS` | COMPAT | `CRATONVM_COMPAT=jboss-brute-force-jars` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JBOSS_LOGGER_BASE_EMIT` | COMPAT | `CRATONVM_COMPAT=jboss-logger-base-emit` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JBOSS_LOGGER_LEVEL_FILTER` | COMPAT | `CRATONVM_COMPAT=jboss-logger-level-filter` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_JBOSS_LOG_CONTEXT_INITIALIZER` | COMPAT | `CRATONVM_COMPAT=jboss-log-context-initializer` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_JBOSS_MP_ROOT` | COMPAT | `CRATONVM_COMPAT=jboss-mp-root` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JCA_LENIENT_GETINSTANCE` | SECURITY | `CRATONVM_SECURITY=jca-lenient-getinstance` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JDK_RANDOM` | COMPAT | `CRATONVM_COMPAT=jdk-random` | opt-in | off | behaviour | snapshot | native-builtins, native-collections |
| `CRATONVM_JDK_SCANNER` | COMPAT | `CRATONVM_COMPAT=jdk-scanner` | opt-in | off | behaviour | snapshot | native-io |
| `CRATONVM_JFR_ENABLE_EVENTS` | — | `CRATONVM_JFR_ENABLE_EVENTS` | scalar | unset | behaviour | snapshot | vm |
| `CRATONVM_JIT` | JIT | `CRATONVM_JIT=…` | group | unset | — | snapshot | jit, types |
| `CRATONVM_JIT_A5_FRAME_SCAN` | JIT | `CRATONVM_JIT=a5-frame-scan` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_A5_MARK_SPAN` | JIT | `CRATONVM_JIT=a5-mark-span` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_A5_RESIDUE_FILTER` | JIT | `CRATONVM_JIT=a5-residue-filter` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_A5_SHAPE_FILTER` | JIT | `CRATONVM_JIT=a5-shape-filter` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_AALOAD_LICM_CLINIT_FENCE` | JIT | `CRATONVM_JIT=aaload-licm-clinit-fence` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_AALOAD_LICM_TYPECHECK_FENCE` | JIT | `CRATONVM_JIT=aaload-licm-typecheck-fence` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ABOVE_CHAIN_ALL_PATHS` | JIT | `CRATONVM_JIT=above-chain-all-paths` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_ABOVE_CHAIN_FROM_SP` | JIT | `CRATONVM_JIT=above-chain-from-sp` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_ABOVE_CHAIN_SCAN` | JIT | `CRATONVM_JIT=above-chain-scan` | opt-in | off | behaviour | snapshot | vm, vm-cli |
| `CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX` | JIT | `CRATONVM_JIT=activation-global-mutex` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_ALLOC_SINK` | JIT | `CRATONVM_JIT=alloc-sink` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ARITH_LICM_TRIM_ENTERED` | JIT | `CRATONVM_JIT=arith-licm-trim-entered` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ARM64` | JIT | `CRATONVM_JIT=arm64` | opt-in | off | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_ARM64_SAFEPOINTS` | JIT | `CRATONVM_JIT=arm64-safepoints` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_ASSERT_CODE_FREE_AUDIT` | JIT | `CRATONVM_JIT=assert-code-free-audit` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_JIT_BACKEND_PARITY` | JIT | `CRATONVM_JIT=backend-parity` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_BAKE_ONLY_KEPT_CALLEE` | JIT | `CRATONVM_JIT=bake-only-kept-callee` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_BAND_SKIP` | JIT | `CRATONVM_JIT=band-skip` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_BG_DECLINE_REDEFINED` | JIT | `CRATONVM_JIT=bg-decline-redefined` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_BISECT_ONLY` | DBG | `CRATONVM_DBG=jit-bisect-only` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_JIT_BLOCKED_SUMMARY_DOOR_EXIT` | JIT | `CRATONVM_JIT=blocked-summary-door-exit` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_BUFFER_SESSION_DIRECT` | JIT | `CRATONVM_JIT=buffer-session-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_BULK_BYTE_LOOPS` | JIT | `CRATONVM_JIT=bulk-byte-loops` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_BYTECODE_LOOP_XFORM` | JIT | `CRATONVM_JIT=bytecode-loop-xform` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_C1_VECTOR_VETO` | JIT | `CRATONVM_JIT=c1-vector-veto` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_C2_ALLOC_UPGRADE` | JIT | `CRATONVM_JIT=c2-alloc-upgrade` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_C2_FIRST_CALL` | JIT | `CRATONVM_JIT=c2-first-call` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE` | JIT | `CRATONVM_JIT=cached-entry-owner-reuse` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST` | JIT | `CRATONVM_JIT=callee-declined-verdict-first` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES` | JIT | `CRATONVM_JIT=callee-exception-rerun-raises` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CALLEE_IDENTITY` | JIT | `CRATONVM_JIT=callee-identity` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CALLEE_OWNED_ARGS` | JIT | `CRATONVM_JIT=callee-owned-args` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CALLEE_PROMOTION` | JIT | `CRATONVM_JIT=callee-promotion` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CALLEE_TABLE_MEMO` | JIT | `CRATONVM_JIT=callee-table-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES` | JIT | `CRATONVM_JIT=callee-unsound-rerun-raises` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CALL_SPILL_ELISION` | JIT | `CRATONVM_JIT=call-spill-elision` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CAST_SITE_NAMESPACED_ARRAYS` | JIT | `CRATONVM_JIT=cast-site-namespaced-arrays` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CENSUS_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=census-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CHA` | JIT | `CRATONVM_JIT=cha` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC` | JIT | `CRATONVM_JIT=charseq-string-intrinsic` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_CHECKCAST_INLINE` | JIT | `CRATONVM_JIT=checkcast-inline` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CODE_ARENA` | JIT | `CRATONVM_JIT=code-arena` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB` | JIT | `CRATONVM_JIT=code-cache-max-mb` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_CODE_CACHE_SWEEP` | JIT | `CRATONVM_JIT=code-cache-sweep` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_CODE_NEAR_GLOBALS` | JIT | `CRATONVM_JIT=code-near-globals` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE` | JIT | `CRATONVM_JIT=compact-compiled-backtrace` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE` | JIT | `CRATONVM_JIT=compiled-ldc-const-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_COMPILE_ID_CODE_GRACE` | JIT | `CRATONVM_JIT=compile-id-code-grace` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CTOR_STORE_FROM_NULL` | JIT | `CRATONVM_JIT=ctor-store-from-null` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CYCLE_DIRECT_BIND` | JIT | `CRATONVM_JIT=cycle-direct-bind` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_CYCLE_EDGE_CELL` | JIT | `CRATONVM_JIT=cycle-edge-cell` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DEFERRED_NEW_LOOKS` | JIT | `CRATONVM_JIT=deferred-new-looks` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND` | JIT | `CRATONVM_JIT=deferred-new-retry-blind` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_DEFINE_SCOPED_WITHDRAWAL` | JIT | `CRATONVM_JIT=define-scoped-withdrawal` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DENY` | JIT | `CRATONVM_JIT=deny` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL` | JIT | `CRATONVM_JIT=deopt-replay-first-arrival` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DEOPT_SINK_RESUME` | JIT | `CRATONVM_JIT=deopt-sink-resume` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DESPEC_SPARE_FACTOR` | JIT | `CRATONVM_JIT=despec-spare-factor` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS` | JIT | `CRATONVM_JIT=direct-callee-calls` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS` | JIT | `CRATONVM_JIT=direct-call-arg-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH` | JIT | `CRATONVM_JIT=direct-exc-table-publish` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DISABLE_INLINE_NEW` | JIT | `CRATONVM_JIT=inline-new` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY` | JIT | `CRATONVM_JIT=dispatch-cache-direct-entry` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` | JIT | `CRATONVM_JIT=dispatch-cache-virtual-direct-entry` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DISPATCH_SOE_THROWS` | JIT | `CRATONVM_JIT=dispatch-soe-throws` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DISPATCH_VIRTUAL_MEMO_FIRST` | JIT | `CRATONVM_JIT=dispatch-virtual-memo-first` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DIV_GUARD_RETIER` | JIT | `CRATONVM_JIT=div-guard-retier` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR` | JIT | `CRATONVM_JIT=dominant-receiver-floor` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DOOR_SYNC_ADDITIVE_RESUME` | JIT | `CRATONVM_JIT=jit-door-sync-additive-resume` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY` | JIT | `CRATONVM_JIT=door-sync-instance-body` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES` | JIT | `CRATONVM_JIT=door-unsound-foreign-rerun-raises` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES` | JIT | `CRATONVM_JIT=jit-door-unsound-rerun-raises` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RETIRE` | JIT | `CRATONVM_JIT=door-unsound-rerun-retire` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_DROP_OVERTAKEN_TASKS` | JIT | `CRATONVM_JIT=drop-overtaken-tasks` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_DUPX_EAGER_CANON` | JIT | `CRATONVM_JIT=dupx-eager-canon` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_EAGER_CALLEE_CHAIN` | JIT | `CRATONVM_JIT=eager-callee-chain` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL` | JIT | `CRATONVM_JIT=eager-door-route-all` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_EAGER_DOOR_ROUTE_UNSUPPORTED_LDC` | JIT | `CRATONVM_JIT=eager-door-route-unsupported-ldc` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_EAGER_ORDINARY_DOOR` | JIT | `CRATONVM_JIT=eager-ordinary-door` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_EA_CROSS_BLOCK_LOADS` | JIT | `CRATONVM_JIT=ea-cross-block-loads` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_ELIDE_TRIVIAL_CTOR` | JIT | `CRATONVM_JIT=elide-trivial-ctor` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS` | JIT | `CRATONVM_JIT=enable-callee-saved-gpr-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ENABLE_INLINE_NEW` | JIT | `CRATONVM_JIT=enable-inline-new` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_ENTRY_COUNTER` | JIT | `CRATONVM_JIT=entry-counter` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_EPOCH_CELL` | JIT | `CRATONVM_JIT=epoch-cell` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIT_FFM_OFFSET_ACCESS` | JIT | `CRATONVM_JIT=ffm-offset-access` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_ARRAY_BCE` | JIT | `CRATONVM_JIT=field-array-bce` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_ARRAY_HOIST` | JIT | `CRATONVM_JIT=field-array-hoist` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_INT_HOIST` | JIT | `CRATONVM_JIT=field-int-hoist` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_SITE_CACHE` | JIT | `CRATONVM_JIT=field-site-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FIELD_SITE_CACHE_LOADER` | JIT | `CRATONVM_JIT=field-site-cache-loader` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FIELD_SITE_SLOTS` | JIT | `CRATONVM_JIT=field-site-slots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_FINAL_DEVIRT` | JIT | `CRATONVM_JIT=final-devirt` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN` | JIT | `CRATONVM_JIT=final-devirt-native-screen` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST` | JIT | `CRATONVM_JIT=fjp-subclass-blocklist` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_FMA_INLINE` | JIT | `CRATONVM_JIT=fma-inline` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FORCE_C2` | JIT | `CRATONVM_JIT=force-c2` | opt-in | off | behaviour | snapshot | difftest, jit |
| `CRATONVM_JIT_FRAMELESS_MAP_MISS_FAILS` | JIT | `CRATONVM_JIT=frameless-map-miss-fails` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY` | JIT | `CRATONVM_JIT=frameless-trap-identity` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FRAMELESS_TRAP_METHOD_IDENTITY` | JIT | `CRATONVM_JIT=frameless-trap-method-identity` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FRAME_SLOT_OFFSET_TRIPWIRE` | JIT | `CRATONVM_JIT=frame-slot-offset-tripwire` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_FULL_SELF_CALL_SPILL` | JIT | `CRATONVM_JIT=full-self-call-spill` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_FUSED_BOUNDS_LOAD` | JIT | `CRATONVM_JIT=fused-bounds-load` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_GATED_REF_STORE` | JIT | `CRATONVM_JIT=gated-ref-store` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_GATE_PASS_MEMO` | JIT | `CRATONVM_JIT=gate-pass-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_GC_INERT_SELFREC` | JIT | `CRATONVM_JIT=gc-inert-selfrec` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_GDB` | JIT | `CRATONVM_JIT=gdb` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_GETFIELD_HELPER` | JIT | `CRATONVM_JIT=getfield-helper` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_GETSTATIC_HELPER` | JIT | `CRATONVM_JIT=getstatic-helper` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_GPU_ARRAY_BARRIER` | GC | `CRATONVM_GC=jit-gpu-array-barrier` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` | JIT | `CRATONVM_JIT=guarded-virtual-inline` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_GUARD_EVIDENCE_FIDELITY` | JIT | `CRATONVM_JIT=guard-evidence-fidelity` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER` | JIT | `CRATONVM_JIT=handler-sink-genuine-receiver` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_HASHED_STUB_CLASS_SLOTS` | JIT | `CRATONVM_JIT=hashed-stub-class-slots` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_HASHED_STUB_CLONE_PIC` | JIT | `CRATONVM_JIT=hashed-stub-clone-pic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_HASHED_STUB_MEGA_TABLE` | JIT | `CRATONVM_JIT=hashed-stub-mega-table` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_HASHED_STUB_WIDE` | JIT | `CRATONVM_JIT=hashed-stub-wide` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_HELPER_FAILURE_CONVERSION` | JIT | `CRATONVM_JIT=helper-failure-conversion` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_HELPER_PANICS_FATAL` | JIT | `CRATONVM_JIT=helper-panics-fatal` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_HELPER_WIDE_ENTRY_CALLS` | JIT | `CRATONVM_JIT=helper-wide-entry-calls` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_HOLDSLOCK_METHOD_MONITOR_FOLD` | JIT | `CRATONVM_JIT=holdslock-method-monitor-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_HOT_LOOKUP_CACHE` | JIT | `CRATONVM_JIT=hot-lookup-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS` | JIT | `CRATONVM_JIT=ic-batch-retired-owners` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP` | JIT | `CRATONVM_JIT=ic-catch-up-self-stamp` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE` | JIT | `CRATONVM_JIT=ic-dead-ways-free-at-once` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_FORGET_DEAD_RECEIVERS` | JIT | `CRATONVM_JIT=ic-forget-dead-receivers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_GRACE_AT_STOP` | JIT | `CRATONVM_JIT=ic-grace-at-stop` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS` | JIT | `CRATONVM_JIT=ic-grace-handshake-ms` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_GRACE_THREAD_EVIDENCE` | JIT | `CRATONVM_JIT=ic-grace-thread-evidence` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_PROFILE_MEGA_SEED` | JIT | `CRATONVM_JIT=ic-profile-mega-seed` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_QUIESCENT_STAMP` | JIT | `CRATONVM_JIT=ic-quiescent-stamp` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_SAME_KEY_REFILL` | JIT | `CRATONVM_JIT=ic-same-key-refill` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_STACK_GUARD` | JIT | `CRATONVM_JIT=ic-stack-guard` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_IC_SUPERSEDE_RETARGET` | JIT | `CRATONVM_JIT=ic-supersede-retarget` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IC_WIDE_STACK_WORDS` | JIT | `CRATONVM_JIT=ic-wide-stack-words` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IMPLICIT_CALLEE_THROW_PC` | JIT | `CRATONVM_JIT=implicit-callee-throw-pc` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS` | JIT | `CRATONVM_JIT=implicit-local-handlers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IMPLICIT_NULL_CHECK` | JIT | `CRATONVM_JIT=implicit-null-check` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INCLUSIVE_BCE` | JIT | `CRATONVM_JIT=inclusive-bce` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_INDY_BRIDGE` | JIT | `CRATONVM_JIT=indy-bridge` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_INDY_LAMBDA_FAST` | JIT | `CRATONVM_JIT=indy-lambda-fast` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_INFLATED_EXIT_PRECHECK` | JIT | `CRATONVM_JIT=inflated-exit-precheck` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INFLATED_STICKY_COUNT` | JIT | `CRATONVM_JIT=inflated-sticky-count` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_BODY_OOP_MARKS` | JIT | `CRATONVM_JIT=inline-body-oop-marks` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_CALLS` | JIT | `CRATONVM_JIT=inline-calls` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_CALL_DISPATCH` | JIT | `CRATONVM_JIT=inline-call-dispatch` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_CARD_MARK` | JIT | `CRATONVM_JIT=inline-card-mark` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_INLINE_GETFIELD` | JIT | `CRATONVM_JIT=inline-getfield` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_INFLATED_LOCK` | JIT | `CRATONVM_JIT=inline-inflated-lock` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_INFLATED_RECURSION` | JIT | `CRATONVM_JIT=inline-inflated-recursion` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_INFLATED_SPIN` | JIT | `CRATONVM_JIT=inline-inflated-spin` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_INFLATED_TWO_WAY` | JIT | `CRATONVM_JIT=inline-inflated-two-way` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_LOOP_SITES_HOT` | JIT | `CRATONVM_JIT=inline-loop-sites-hot` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_NEST` | JIT | `CRATONVM_JIT=inline-nest` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_OOP_COVERAGE` | JIT | `CRATONVM_JIT=inline-oop-coverage` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_PRIM_PUTFIELD` | JIT | `CRATONVM_JIT=inline-prim-putfield` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_SELF_GUARD` | JIT | `CRATONVM_JIT=inline-self-guard` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_SPIN_BACKOFF` | JIT | `CRATONVM_JIT=inline-spin-backoff` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` | JIT | `CRATONVM_JIT=inline-splice-devirt` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION` | JIT | `CRATONVM_JIT=inline-thin-lock-recursion` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INSTANCEOF_FINAL_MISS` | JIT | `CRATONVM_JIT=instanceof-final-miss` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INSTANCE_SELF_CALL` | JIT | `CRATONVM_JIT=instance-self-call` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INT_VALUE_DIRECT` | JIT | `CRATONVM_JIT=int-value-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INVOKE_FAST_DOOR` | JIT | `CRATONVM_JIT=invoke-fast-door` | both | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_AASTORE` | JIT | `CRATONVM_JIT=ir-aastore` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ADD_LEA` | JIT | `CRATONVM_JIT=ir-add-lea` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ALLOC_FAST_JOIN` | JIT | `CRATONVM_JIT=ir-alloc-fast-join` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ALLOC_MAP_SINK` | JIT | `CRATONVM_JIT=ir-alloc-map-sink` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ALU_IMM` | JIT | `CRATONVM_JIT=ir-alu-imm` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BCE_MASK_INDEX` | JIT | `CRATONVM_JIT=ir-bce-mask-index` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BCE_RANGE` | JIT | `CRATONVM_JIT=ir-bce-range` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BCE_STRIDE3` | JIT | `CRATONVM_JIT=ir-bce-stride3` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BIMORPHIC_GUARDED_SPLICE` | JIT | `CRATONVM_JIT=ir-bimorphic-guarded-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BOX_HASH_FOLD` | JIT | `CRATONVM_JIT=ir-box-hash-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BRANCH_LAYOUT_POLARITY` | JIT | `CRATONVM_JIT=ir-branch-layout-polarity` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_BUFFER_REF_HOME_RESERVE` | JIT | `CRATONVM_JIT=ir-buffer-ref-home-reserve` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CALL` | JIT | `CRATONVM_JIT=ir-call` | default-on | on | behaviour | snapshot | difftest, vm |
| `CRATONVM_JIT_IR_CALL_ANEWARRAY` | JIT | `CRATONVM_JIT=ir-call-anewarray` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CALL_SPECIAL` | JIT | `CRATONVM_JIT=ir-call-special` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_CALL_VIRTUAL` | JIT | `CRATONVM_JIT=ir-call-virtual` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_CARRY_2ND` | JIT | `CRATONVM_JIT=ir-carry-2nd` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CARRY_RCX_FOLDED` | JIT | `CRATONVM_JIT=ir-carry-rcx-folded` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CARRY_RCX_TWIN` | JIT | `CRATONVM_JIT=ir-carry-rcx-twin` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CARRY_SINGLE_USE` | JIT | `CRATONVM_JIT=ir-carry-single-use` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CATCH_RETIER` | JIT | `CRATONVM_JIT=ir-catch-retier` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS` | JIT | `CRATONVM_JIT=ir-chain-snapshot-range-pins` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CHECK_ELIM` | JIT | `CRATONVM_JIT=ir-check-elim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS` | JIT | `CRATONVM_JIT=ir-clear-dead-ref-slots` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CMP_IN_PLACE` | JIT | `CRATONVM_JIT=ir-cmp-in-place` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_COLD_ARG_STAGE` | JIT | `CRATONVM_JIT=ir-cold-arg-stage` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CONST_DIV` | JIT | `CRATONVM_JIT=ir-const-div` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CONST_IF_FOLD` | JIT | `CRATONVM_JIT=ir-const-if-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CONST_IF_PRUNE` | JIT | `CRATONVM_JIT=ir-const-if-prune` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CONST_IMM` | JIT | `CRATONVM_JIT=ir-const-imm` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS` | JIT | `CRATONVM_JIT=ir-cross-call-cold-tails` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DEAD_HOME_CLEARS` | JIT | `CRATONVM_JIT=ir-dead-home-clears` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES` | JIT | `CRATONVM_JIT=ir-dead-home-value-ranges` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DEOPT_POINTS_AT_TRAPS` | JIT | `CRATONVM_JIT=ir-deopt-points-at-traps` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DEOPT_REGS` | JIT | `CRATONVM_JIT=ir-deopt-regs` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DIRECT_CALL` | JIT | `CRATONVM_JIT=ir-direct-call` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DROP_HOME` | JIT | `CRATONVM_JIT=ir-drop-home` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DROP_PHI_HOME` | JIT | `CRATONVM_JIT=ir-drop-phi-home` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES` | JIT | `CRATONVM_JIT=ir-drop-unreachable-homes` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_EA_NARROW_PHI` | JIT | `CRATONVM_JIT=ir-ea-narrow-phi` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ENTRY_FAST_RETURN` | JIT | `CRATONVM_JIT=ir-entry-fast-return` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ENTRY_FOLD` | JIT | `CRATONVM_JIT=ir-entry-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS` | JIT | `CRATONVM_JIT=ir-entry-fold-self-calls` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE` | JIT | `CRATONVM_JIT=ir-entry-poll-outline` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_EPOCH_GUARD_RIP` | JIT | `CRATONVM_JIT=ir-epoch-guard-rip` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_EXACT_RECEIVER_SPLICE` | JIT | `CRATONVM_JIT=ir-exact-receiver-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_FALLTHROUGH` | JIT | `CRATONVM_JIT=ir-fallthrough` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_FP` | JIT | `CRATONVM_JIT=ir-fp` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` | JIT | `CRATONVM_JIT=ir-frame-block-max-homes` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_FUSED_BRANCH` | JIT | `CRATONVM_JIT=ir-fused-branch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_FUSED_FCMP` | JIT | `CRATONVM_JIT=ir-fused-fcmp` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_G1_REF_STORE` | JIT | `CRATONVM_JIT=ir-g1-ref-store` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_GC_POINT_MAPS` | JIT | `CRATONVM_JIT=ir-gc-point-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_GP_WIDE` | JIT | `CRATONVM_JIT=ir-gp-wide` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_GUARDED_EXACT_FACT_SCOPED` | JIT | `CRATONVM_JIT=ir-guarded-exact-fact-scoped` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_GUARDED_RECEIVER_SPLICE` | JIT | `CRATONVM_JIT=ir-guarded-receiver-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS` | JIT | `CRATONVM_JIT=ir-guarded-splice-trap-miss` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_GUARD_TOKEN` | JIT | `CRATONVM_JIT=ir-guard-token` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_HOLDSLOCK_FOLD` | JIT | `CRATONVM_JIT=ir-holdslock-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_HOLDSLOCK_METHOD_FOLD` | JIT | `CRATONVM_JIT=ir-holdslock-method-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_HOT_LAYOUT` | JIT | `CRATONVM_JIT=ir-hot-layout` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_IC_KIND_SCREEN_ELIDE` | JIT | `CRATONVM_JIT=ir-ic-kind-screen-elide` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_INITIAL_ZERO_STORES` | JIT | `CRATONVM_JIT=ir-initial-zero-stores` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_INLINE` | JIT | `CRATONVM_JIT=ir-inline` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_INLINE_CARD_CHECK` | JIT | `CRATONVM_JIT=ir-inline-card-check` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_INLINE_CHAIN_BY_SP_ID` | JIT | `CRATONVM_JIT=ir-inline-chain-by-sp-id` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_INLINE_TLAB` | JIT | `CRATONVM_JIT=ir-inline-tlab` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ISEL_EMIT` | JIT | `CRATONVM_JIT=ir-isel-emit` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ISEL_SHADOW` | JIT | `CRATONVM_JIT=ir-isel-shadow` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ISEL_VERIFY` | JIT | `CRATONVM_JIT=ir-isel-verify` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS` | JIT | `CRATONVM_JIT=ir-keep-set-def-kills` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE` | JIT | `CRATONVM_JIT=ir-buffer-estimate` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LICM_BEFORE_UNROLL` | JIT | `CRATONVM_JIT=ir-licm-before-unroll` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LICM_GUARD_ATTRIBUTION` | JIT | `CRATONVM_JIT=ir-licm-guard-attribution` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LICM_HOIST_COUNTED` | JIT | `CRATONVM_JIT=ir-licm-hoist-counted` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LICM_HOIST_DEDUP` | JIT | `CRATONVM_JIT=ir-licm-hoist-dedup` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LICM_MEM_EDGE` | JIT | `CRATONVM_JIT=ir-licm-mem-edge` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LINEAR_SCAN` | JIT | `CRATONVM_JIT=ir-linear-scan` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LIST_SCHED` | JIT | `CRATONVM_JIT=ir-list-sched` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOAD_CSE` | JIT | `CRATONVM_JIT=ir-load-cse` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOAD_CSE_ALIAS` | JIT | `CRATONVM_JIT=ir-load-cse-alias` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOAD_CSE_DOMINANCE` | JIT | `CRATONVM_JIT=ir-load-cse-dominance` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOCK_COARSEN` | JIT | `CRATONVM_JIT=ir-lock-coarsen` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LONG` | JIT | `CRATONVM_JIT=ir-long` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD` | JIT | `CRATONVM_JIT=ir-long-box-hash-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LS_CALL_LAST_USE` | JIT | `CRATONVM_JIT=ir-ls-call-last-use` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LS_EDGE_RESOLUTION` | JIT | `CRATONVM_JIT=ir-ls-edge-resolution` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LS_LOOP_WEIGHT` | JIT | `CRATONVM_JIT=ir-ls-loop-weight` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LS_PREPIN_REFUSED` | JIT | `CRATONVM_JIT=ir-ls-prepin-refused` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LS_SPLITS` | JIT | `CRATONVM_JIT=ir-ls-splits` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_MEGA_GATE_ENTRY` | JIT | `CRATONVM_JIT=ir-mega-gate-entry` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_MONITOR_OP_VOUCH` | JIT | `CRATONVM_JIT=ir-monitor-op-vouch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_NESTED_LOCK_ELIM` | JIT | `CRATONVM_JIT=ir-nested-lock-elim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_OSR_ENTRY` | JIT | `CRATONVM_JIT=ir-osr-entry` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_OSR_ENTRY_ALWAYS` | JIT | `CRATONVM_JIT=ir-osr-entry-always` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_OSR_FRAME_VALUE_REFUSAL` | JIT | `CRATONVM_JIT=ir-osr-frame-value-refusal` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_OSR_HEADER_ENTRIES_ONLY` | JIT | `CRATONVM_JIT=ir-osr-header-entries-only` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL` | JIT | `CRATONVM_JIT=ir-osr-predicate-refusal` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_OVER_INTRINSIC` | JIT | `CRATONVM_JIT=ir-over-intrinsic` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PAIR_OPERANDS` | JIT | `CRATONVM_JIT=ir-pair-operands` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PARAM_COPY` | JIT | `CRATONVM_JIT=ir-param-copy` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PARAM_HOME_ALIAS` | JIT | `CRATONVM_JIT=ir-param-home-alias` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PARAM_REGISTER_FILL` | JIT | `CRATONVM_JIT=ir-param-register-fill` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PARTIAL_UNROLL` | JIT | `CRATONVM_JIT=ir-partial-unroll` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR` | JIT | `CRATONVM_JIT=ir-partial-unroll-factor` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PER_COPY_FRAMES` | JIT | `CRATONVM_JIT=ir-per-copy-frames` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PHI_COPY_DIRECT` | JIT | `CRATONVM_JIT=ir-phi-copy-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PHI_COPY_REGS` | JIT | `CRATONVM_JIT=ir-phi-copy-regs` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PHI_EDGE_INTERFERE` | JIT | `CRATONVM_JIT=ir-phi-edge-interfere` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PHI_HOME_PUBLISH_GUARD` | JIT | `CRATONVM_JIT=ir-phi-home-publish-guard` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PHI_RESIDENCY` | JIT | `CRATONVM_JIT=ir-phi-residency` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED` | JIT | `CRATONVM_JIT=ir-precise-frames-synchronized` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES` | JIT | `CRATONVM_JIT=ir-precise-handler-frames` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PRECISE_KEEP_SET` | JIT | `CRATONVM_JIT=ir-precise-keep-set` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PROFILE_GUARDED_SPLICE` | JIT | `CRATONVM_JIT=ir-profile-guarded-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS` | JIT | `CRATONVM_JIT=ir-prune-chain-snapshot-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PRUNE_HANDLER_METHOD_LOCALS` | JIT | `CRATONVM_JIT=ir-prune-handler-method-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS` | JIT | `CRATONVM_JIT=ir-prune-loop-header-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PUBLISH_AT_DEF` | JIT | `CRATONVM_JIT=ir-publish-at-def` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RANGE_EDGES` | JIT | `CRATONVM_JIT=ir-range-edges` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RANGE_FOLD_CMP` | JIT | `CRATONVM_JIT=ir-range-fold-cmp` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RANGE_PREDICATE` | JIT | `CRATONVM_JIT=ir-range-predicate` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RECEIVER_GUARD_CSE` | JIT | `CRATONVM_JIT=ir-receiver-guard-cse` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RECURSIVE_INLINE` | JIT | `CRATONVM_JIT=ir-recursive-inline` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RECURSIVE_INLINE_REPLAY_SAFE` | JIT | `CRATONVM_JIT=ir-recursive-inline-replay-safe` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REFUSE_DEAD_END_BLOCK` | JIT | `CRATONVM_JIT=ir-refuse-dead-end-block` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REF_FREE_LEAN_PROLOGUE` | JIT | `CRATONVM_JIT=ir-ref-free-lean-prologue` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REF_RESIDENCY` | JIT | `CRATONVM_JIT=ir-ref-residency` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REF_RESIDENCY_CROSS_SAFEPOINT` | JIT | `CRATONVM_JIT=ir-ref-residency-cross-safepoint` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REF_STORE` | JIT | `CRATONVM_JIT=ir-ref-store` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REG_AUTHORITATIVE` | JIT | `CRATONVM_JIT=ir-reg-authoritative` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RELOC_EMIT` | JIT | `CRATONVM_JIT=ir-reloc-emit` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_REPLAY_FALLBACK_CHECK` | JIT | `CRATONVM_JIT=ir-replay-fallback-check` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RESERVE_CARRIED` | JIT | `CRATONVM_JIT=ir-reserve-carried` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING` | JIT | `CRATONVM_JIT=ir-residency-call-crossing` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` | JIT | `CRATONVM_JIT=ir-residency-crossblock` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK_BUDGET` | JIT | `CRATONVM_JIT=ir-residency-crossblock-budget` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RESIDENCY_PAYS` | JIT | `CRATONVM_JIT=ir-residency-pays` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_RPO_LAYOUT` | JIT | `CRATONVM_JIT=ir-rpo-layout` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SCALAR_IDENTITIES` | JIT | `CRATONVM_JIT=ir-scalar-identities` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SCALAR_INTRINSICS` | JIT | `CRATONVM_JIT=ir-scalar-intrinsics` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SELFREC_DIRECT` | JIT | `CRATONVM_JIT=ir-selfrec-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SELF_CALL_ANSWER` | JIT | `CRATONVM_JIT=ir-self-call-answer` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS` | JIT | `CRATONVM_JIT=ir-self-call-cold-tails` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID` | JIT | `CRATONVM_JIT=ir-self-call-lazy-sp-id` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SELF_CALL_SAMPLE_DOMINATED` | JIT | `CRATONVM_JIT=ir-self-call-sample-dominated` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK` | JIT | `CRATONVM_JIT=ir-shadow-frame-block` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` | JIT | `CRATONVM_JIT=ir-sink-equal-depth` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SINK_LATE` | JIT | `CRATONVM_JIT=ir-sink-late` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SINK_PHI_EDGE` | JIT | `CRATONVM_JIT=ir-sink-phi-edge` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SITE_TRAP` | JIT | `CRATONVM_JIT=ir-site-trap` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SKIP_REPUBLISH` | JIT | `CRATONVM_JIT=ir-skip-republish` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SNAPSHOT_LIVENESS` | JIT | `CRATONVM_JIT=ir-snapshot-liveness` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPECULATE` | JIT | `CRATONVM_JIT=ir-speculate` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICED_NEW_SKIP_POST_INIT` | JIT | `CRATONVM_JIT=ir-spliced-new-skip-post-init` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_BRANCH` | JIT | `CRATONVM_JIT=ir-splice-branch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES` | JIT | `CRATONVM_JIT=ir-splice-chain-fences` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_COLD_NESTED` | JIT | `CRATONVM_JIT=ir-splice-cold-nested` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE` | JIT | `CRATONVM_JIT=ir-splice-committed-store` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_DESPEC_SKIPS_SITE` | JIT | `CRATONVM_JIT=ir-splice-despec-skips-site` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_DIRECT_CALL` | JIT | `CRATONVM_JIT=ir-splice-direct-call` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_FENCE_FORWARD_BRANCH` | JIT | `CRATONVM_JIT=ir-splice-fence-forward-branch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_FENCE_GUARDED_CALL` | JIT | `CRATONVM_JIT=ir-splice-fence-guarded-call` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_FENCE_NULL_FACTS` | JIT | `CRATONVM_JIT=ir-splice-fence-null-facts` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` | JIT | `CRATONVM_JIT=ir-splice-frame-states` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_GETSTATIC` | JIT | `CRATONVM_JIT=ir-splice-getstatic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_LDC` | JIT | `CRATONVM_JIT=ir-splice-ldc` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN` | JIT | `CRATONVM_JIT=ir-splice-multi-return` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_NESTED_COLD_RESERVE` | JIT | `CRATONVM_JIT=ir-splice-nested-cold-reserve` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_RECEIVER` | JIT | `CRATONVM_JIT=ir-splice-nested-exact-receiver` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_PLAN_STORE_FENCE` | JIT | `CRATONVM_JIT=ir-splice-plan-store-fence` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_PRUNE_NESTED` | JIT | `CRATONVM_JIT=ir-splice-prune-nested` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_REBUILD` | JIT | `CRATONVM_JIT=ir-splice-rebuild` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_REFUSES_FINALIZABLE_NEW` | JIT | `CRATONVM_JIT=ir-splice-refuses-finalizable-new` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_IR_SPLICE_REFUSE_UNBINDABLE` | JIT | `CRATONVM_JIT=ir-splice-refuse-unbindable` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_SKIP_UNINIT_CLASS` | JIT | `CRATONVM_JIT=ir-splice-skip-uninit-class` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_SPLICE_TYPECHECK` | JIT | `CRATONVM_JIT=ir-splice-typecheck` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_STRING_INTRINSICS` | JIT | `CRATONVM_JIT=ir-string-intrinsics` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD` | JIT | `CRATONVM_JIT=ir-string-literal-hash-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD` | JIT | `CRATONVM_JIT=ir-string-literal-query-fold` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SWITCH_SEARCH_TREE` | JIT | `CRATONVM_JIT=ir-switch-search-tree` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` | JIT | `CRATONVM_JIT=ir-sync-method-chains` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS` | JIT | `CRATONVM_JIT=ir-sync-method-monitor-facts` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE` | JIT | `CRATONVM_JIT=ir-sync-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_IN_REGION` | JIT | `CRATONVM_JIT=ir-sync-splice-in-region` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_MULTI_RETURN` | JIT | `CRATONVM_JIT=ir-sync-splice-multi-return` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_ELIM` | JIT | `CRATONVM_JIT=ir-sync-splice-nested-elim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_SAME_RECEIVER` | JIT | `CRATONVM_JIT=ir-sync-splice-nested-same-receiver` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_SELF_CLASS_HOLDSLOCK` | JIT | `CRATONVM_JIT=ir-sync-splice-self-class-holdslock` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_STATIC_MIRROR` | JIT | `CRATONVM_JIT=ir-sync-splice-static-mirror` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_SPLICE_STORE_FENCE_SKIP` | JIT | `CRATONVM_JIT=ir-sync-splice-store-fence-skip` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_STATIC_METHOD_MONITOR_FACTS` | JIT | `CRATONVM_JIT=ir-sync-static-method-monitor-facts` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SYNC_WINDOW_REBUILD` | JIT | `CRATONVM_JIT=ir-sync-window-rebuild` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_THIS_NONNULL` | JIT | `CRATONVM_JIT=ir-this-nonnull` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_TLAB_SKIP_POST_INIT` | JIT | `CRATONVM_JIT=ir-tlab-skip-post-init` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD` | JIT | `CRATONVM_JIT=ir-trap-replay-guard` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_TRAP_REPLAY_LOOP_EXTENT` | JIT | `CRATONVM_JIT=ir-trap-replay-loop-extent` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` | JIT | `CRATONVM_JIT=ir-unresolved-class-trap` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD` | JIT | `CRATONVM_JIT=ir-unresumable-trap-guard` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES` | JIT | `CRATONVM_JIT=ir-unroll-unreachable-frames` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_VALUEOF_INLINE_BOX` | JIT | `CRATONVM_JIT=ir-valueof-inline-box` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_VOLATILE_FIELDS` | JIT | `CRATONVM_JIT=ir-volatile-fields` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ZERO_REF_SLOTS` | JIT | `CRATONVM_JIT=ir-zero-ref-slots` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ZGC_ANNOUNCE` | JIT | `CRATONVM_JIT=ir-zgc-announce` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_JITDUMP` | JIT | `CRATONVM_JIT=jitdump` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_KEEP_OPTIMIZING_BODY` | JIT | `CRATONVM_JIT=keep-optimizing-body` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_KERNEL_REG_LOCALS` | JIT | `CRATONVM_JIT=kernel-reg-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_KERNEL_REG_OSR` | JIT | `CRATONVM_JIT=kernel-reg-osr` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LAMBDA_ADAPTER` | JIT | `CRATONVM_JIT=lambda-adapter` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER` | JIT | `CRATONVM_JIT=lambda-capture-adapter` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_CONST_PROBE` | JIT | `CRATONVM_JIT=lambda-const-probe` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_DIRECT_RERUN_AS_DOORS` | JIT | `CRATONVM_JIT=lambda-direct-rerun-as-doors` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_GET_MEMO` | JIT | `CRATONVM_JIT=lambda-get-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_GET_SERVICE` | JIT | `CRATONVM_JIT=lambda-get-service` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_RESUME_FAILURE_RAISES` | JIT | `CRATONVM_JIT=lambda-resume-failure-raises` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_SITE` | JIT | `CRATONVM_JIT=lambda-site` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LAMBDA_TIERUP` | JIT | `CRATONVM_JIT=lambda-tierup` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_LEAK_CODE` | JIT | `CRATONVM_JIT=leak-code` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LICM` | JIT | `CRATONVM_JIT=licm` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LICM_ROW_OOP_MARK` | JIT | `CRATONVM_JIT=licm-row-oop-mark` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE` | JIT | `CRATONVM_JIT=loader-blind-cp-resolve` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LOCAL_HANDLERS` | JIT | `CRATONVM_JIT=local-handlers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_HANDLER_CLEAR_DEAD` | JIT | `CRATONVM_JIT=local-handler-clear-dead` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS` | JIT | `CRATONVM_JIT=local-handler-drop-orphans` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LOCAL_MASK_FAIL_CLOSED` | JIT | `CRATONVM_JIT=local-mask-fail-closed` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_MASK_UNREACHED_FAIL_CLOSED` | JIT | `CRATONVM_JIT=local-mask-unreached-fail-closed` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_REGS` | JIT | `CRATONVM_JIT=local-regs` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCK_COARSEN` | JIT | `CRATONVM_JIT=lock-coarsen` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP` | JIT | `CRATONVM_JIT=lock-stack-single-bump` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=long-box-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LONG_VALUEOF_INLINE_BOX` | JIT | `CRATONVM_JIT=long-valueof-inline-box` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOOP_WORK_TIERUP` | JIT | `CRATONVM_JIT=loop-work-tierup` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_LS_CARRY_RELIEF` | JIT | `CRATONVM_JIT=ls-carry-relief` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MATRIX_DOT` | JIT | `CRATONVM_JIT=matrix-dot` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER` | JIT | `CRATONVM_JIT=md-update-direct-helper` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_CELL_FIRST` | JIT | `CRATONVM_JIT=mega-cell-first` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_CLASS_SLOTS` | JIT | `CRATONVM_JIT=mega-class-slots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_CLASS_SLOTS_IFACE` | JIT | `CRATONVM_JIT=mega-class-slots-iface` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_DISPATCH_TABLE` | JIT | `CRATONVM_JIT=mega-dispatch-table` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_FORGET_DEAD_LOADERS` | JIT | `CRATONVM_JIT=mega-forget-dead-loaders` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_GATE_FAST_ENTRY` | JIT | `CRATONVM_JIT=mega-gate-fast-entry` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_SELECTOR_BY_LOADER` | JIT | `CRATONVM_JIT=mega-selector-by-loader` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_TABLE_DIR_RECLAIM` | JIT | `CRATONVM_JIT=mega-table-dir-reclaim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES` | JIT | `CRATONVM_JIT=mega-table-exc-callees` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_TABLE_OWNER_INDEX` | JIT | `CRATONVM_JIT=mega-table-owner-index` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_TABLE_PUBLISH_DISPATCH` | JIT | `CRATONVM_JIT=mega-table-publish-dispatch` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_TABLE_PUBLISH_LAMBDA` | JIT | `CRATONVM_JIT=mega-table-publish-lambda` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MEGA_TABLE_RECLAIM` | JIT | `CRATONVM_JIT=mega-table-reclaim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MEGA_TABLE_SUPERSEDED_ROLLBACK` | JIT | `CRATONVM_JIT=mega-table-superseded-rollback` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MERGED_CALL_SENTINEL` | JIT | `CRATONVM_JIT=merged-call-sentinel` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MERGE_MARKS_EXACT` | JIT | `CRATONVM_JIT=merge-marks-exact` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_METHOD_SITE_CACHE` | JIT | `CRATONVM_JIT=method-site-cache` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_METRICS` | JIT | `CRATONVM_JIT=metrics` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_METRICS_OUT` | JIT | `CRATONVM_JIT=metrics-out` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_METRICS_RING` | JIT | `CRATONVM_JIT=metrics-ring` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH` | JIT | `CRATONVM_JIT=mic-exc-table-publish` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND` | JIT | `CRATONVM_JIT=mic-install-wait-bound` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MIC_MISS_CALLS_COMPILED` | JIT | `CRATONVM_JIT=mic-miss-calls-compiled` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_MY_SCRATCH_FLUSH` | JIT | `CRATONVM_JIT=my-scratch-flush` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MY_SELFCALL_PROOF` | JIT | `CRATONVM_JIT=my-selfcall-proof` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_MY_SHADOW_EMISSION` | JIT | `CRATONVM_JIT=my-shadow-emission` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_MY_SHADOW_INDIRECT` | JIT | `CRATONVM_JIT=my-shadow-indirect` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL` | JIT | `CRATONVM_JIT=native-shadow-caller-seal` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND` | JIT | `CRATONVM_JIT=native-shadow-interface-blind` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NEVER_FREE_CODE` | JIT | `CRATONVM_JIT=never-free-code` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_NEW_CP_SITE_MEMO` | JIT | `CRATONVM_JIT=new-cp-site-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=nio-byte-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NOT_ENTRANT_ESCAPE` | JIT | `CRATONVM_JIT=not-entrant-escape` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_AASTORE_BARRIER_GATE` | JIT | `CRATONVM_JIT=aastore-barrier-gate` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ALLOC_SPILL_SINK` | JIT | `CRATONVM_JIT=alloc-spill-sink` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ARRAYLENGTH_FAST` | JIT | `CRATONVM_JIT=arraylength-fast` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_ARRAYLIST_PIN` | JIT | `CRATONVM_JIT=arraylist-pin` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ATOMIC_INTRINSIC` | JIT | `CRATONVM_JIT=atomic-intrinsic` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC` | JIT | `CRATONVM_JIT=atomic-long-intrinsic` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE` | JIT | `CRATONVM_JIT=backedge-poll-gate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_BCE` | JIT | `CRATONVM_JIT=bce` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC` | JIT | `CRATONVM_JIT=box-unbox-intrinsic` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_CACHED_NATIVE_FACTS` | JIT | `CRATONVM_JIT=cached-native-facts` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH` | JIT | `CRATONVM_JIT=callee-oop-flush` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE` | JIT | `CRATONVM_JIT=call-frame-dedupe` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_CAST_SITE_CACHE` | JIT | `CRATONVM_JIT=cast-site-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_CODE_PTR_MEMO` | JIT | `CRATONVM_JIT=code-ptr-memo` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_COMPILED_FRAME_LINES` | JIT | `CRATONVM_JIT=compiled-frame-lines` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_DESCRIPTOR_FACTS` | JIT | `CRATONVM_JIT=descriptor-facts` | opt-out | on | behaviour | snapshot | jit-api |
| `CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD` | JIT | `CRATONVM_JIT=devirt-intrinsic-yield` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD` | JIT | `CRATONVM_JIT=door-receiver-record` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_DOOR_RECV_MEMO` | JIT | `CRATONVM_JIT=door-recv-memo` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_DOOR_SYNC` | JIT | `CRATONVM_JIT=door-sync` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_DUP2_X2` | JIT | `CRATONVM_JIT=dup2-x2` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUPX` | JIT | `CRATONVM_JIT=dupx` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUP_X1` | JIT | `CRATONVM_JIT=dup-x1` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_DUP_X2` | JIT | `CRATONVM_JIT=dup-x2` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_EXC_TABLE_C2` | JIT | `CRATONVM_JIT=exc-table-c2` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_FFM_INTRINSIC` | JIT | `CRATONVM_JIT=ffm-intrinsic` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE` | JIT | `CRATONVM_JIT=field-addr-elide` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_FIELD_FAST_PATH` | JIT | `CRATONVM_JIT=field-fast-path` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_FRAME_BANDS` | JIT | `CRATONVM_JIT=frame-bands` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_FRAME_EMPLACE` | JIT | `CRATONVM_JIT=frame-emplace` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_FRAME_SLOT_REUSE` | JIT | `CRATONVM_JIT=frame-slot-reuse` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_IC_FRAME_REPUBLISH` | JIT | `CRATONVM_JIT=ic-frame-republish` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_IFACE_SELECT_MEMO` | JIT | `CRATONVM_JIT=iface-select-memo` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_INLINE_CALLER_FRAMES` | JIT | `CRATONVM_JIT=inline-caller-frames` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_INLINE_CALL_MAP_AT_RETURN` | JIT | `CRATONVM_JIT=inline-call-map-at-return` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_INLINE_FP_ARITH` | JIT | `CRATONVM_JIT=inline-fp-arith` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_INLINE_FRAME_MAP` | JIT | `CRATONVM_JIT=inline-frame-map` | opt-out | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP` | JIT | `CRATONVM_JIT=inline-live-slot-clamp` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR` | JIT | `CRATONVM_JIT=inline-locals-floor` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON` | JIT | `CRATONVM_JIT=inline-miss-edge-poison` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_INLINE_RESERVE_PATH` | JIT | `CRATONVM_JIT=inline-reserve-path` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_INVOKE_FAST_DOOR` | JIT | `CRATONVM_JIT=invoke-fast-door` | both | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_IR_FRAME_LINES` | JIT | `CRATONVM_JIT=ir-frame-lines` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT` | JIT | `CRATONVM_JIT=ir-string-access-admit` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_LDC_CONST_CACHE` | JIT | `CRATONVM_JIT=ldc-const-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_LICM_READ_HOIST` | JIT | `CRATONVM_JIT=licm-read-hoist` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_LOCALS_SLAB` | JIT | `CRATONVM_JIT=locals-slab` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_LONG_INTRINSICS` | JIT | `CRATONVM_JIT=long-intrinsics` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE` | JIT | `CRATONVM_JIT=mic-rust-entry-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NATIVE_SITE_CACHE` | JIT | `CRATONVM_JIT=native-site-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO` | JIT | `CRATONVM_JIT=negative-cast-memo` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES` | JIT | `CRATONVM_JIT=nested-trace-frames` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NEW_CLASS_INIT_MEMO` | JIT | `CRATONVM_JIT=new-class-init-memo` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NEW_SITE_CACHE` | JIT | `CRATONVM_JIT=new-site-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR` | JIT | `CRATONVM_JIT=nonvirtual-fast-door` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT` | JIT | `CRATONVM_JIT=npe-frame-snapshot` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NPE_TRAP_LINES` | JIT | `CRATONVM_JIT=npe-trap-lines` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_OPERAND_FOLD` | JIT | `CRATONVM_JIT=operand-fold` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_OSR_AMBIGUOUS_DEAD` | JIT | `CRATONVM_JIT=osr-ambiguous-dead` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY` | JIT | `CRATONVM_JIT=osr-empty-stack-entry` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE` | JIT | `CRATONVM_JIT=osr-frame-dedupe` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_OSR_INLINE_GATE` | JIT | `CRATONVM_JIT=osr-inline-gate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_OSR_PC_REFRESH` | JIT | `CRATONVM_JIT=osr-pc-refresh` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_OSR_REFINED_REF` | JIT | `CRATONVM_JIT=osr-refined-ref` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PARAM_TAG_SCAN` | JIT | `CRATONVM_JIT=param-tag-scan` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW` | JIT | `CRATONVM_JIT=precise-alloc-athrow` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_ARRAY_ACCESS` | JIT | `CRATONVM_JIT=precise-array-access` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_FIELD_OPS` | JIT | `CRATONVM_JIT=precise-field-ops` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST` | JIT | `CRATONVM_JIT=precise-getstatic-checkcast` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_INDY` | JIT | `CRATONVM_JIT=precise-indy` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES` | JIT | `CRATONVM_JIT=precise-virtual-invokes` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_REF_ARRAY_FAST` | JIT | `CRATONVM_JIT=ref-array-fast` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_RETPC_VALIDATE` | JIT | `CRATONVM_JIT=retpc-validate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_SELF_CACHE_INHERIT` | JIT | `CRATONVM_JIT=self-cache-inherit` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_SLOT_MIRROR` | JIT | `CRATONVM_JIT=slot-mirror` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_SPEC_BCE` | JIT | `CRATONVM_JIT=spec-bce` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STACK_BANG` | JIT | `CRATONVM_JIT=stack-bang` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STAGED_ARG_SHADOW` | JIT | `CRATONVM_JIT=staged-arg-shadow` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STRING_ACCESS_INLINE_ROWS` | JIT | `CRATONVM_JIT=string-access-inline-rows` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN` | JIT | `CRATONVM_JIT=string-intrinsic-pin` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED` | JIT | `CRATONVM_JIT=string-pin-fail-closed` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_SYSTEM_CLASS_LATCH` | JIT | `CRATONVM_JIT=system-class-latch` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD` | JIT | `CRATONVM_JIT=trusted-oop-getfield` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ZERO_RESERVED_TAIL` | JIT | `CRATONVM_JIT=zero-reserved-tail` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_ZERO_UNSET_LOCALS` | JIT | `CRATONVM_JIT=zero-unset-locals` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NPE_MESSAGE_MEMO` | JIT | `CRATONVM_JIT=npe-message-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OBJECT_HASHCODE_STRING` | JIT | `CRATONVM_JIT=object-hashcode-string` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY` | JIT | `CRATONVM_JIT=oopmap-coverage-presence-only` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OOP_MARK_CHECK` | JIT | `CRATONVM_JIT=oop-mark-check` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OOP_MARK_CHECK_STRICT` | JIT | `CRATONVM_JIT=oop-mark-check-strict` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OPERAND_CACHE` | JIT | `CRATONVM_JIT=operand-cache` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR` | JIT | `CRATONVM_JIT=osr` | opt-in | off | behaviour | snapshot | difftest, vm |
| `CRATONVM_JIT_OSR_ATHROW` | JIT | `CRATONVM_JIT=osr-athrow` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_CHAIN_GUARD_EXITS` | JIT | `CRATONVM_JIT=osr-chain-guard-exits` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE` | JIT | `CRATONVM_JIT=osr-chain-guard-exit-charge` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_DEAD_LOCALS` | JIT | `CRATONVM_JIT=osr-dead-locals` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_DEAD_MASK_BLANKET` | JIT | `CRATONVM_JIT=osr-dead-mask-blanket` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_DRAIN_TRAP_FRAMES` | JIT | `CRATONVM_JIT=osr-drain-trap-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_DROP_ORPHANS` | JIT | `CRATONVM_JIT=osr-drop-orphans` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_EXC_TABLE` | JIT | `CRATONVM_JIT=osr-exc-table` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_GUARD_EXIT_CHARGE` | JIT | `CRATONVM_JIT=osr-guard-exit-charge` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_LOUD_UNRESUMABLE` | JIT | `CRATONVM_JIT=osr-loud-unresumable` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_OPAQUE_ENTRY` | JIT | `CRATONVM_JIT=osr-opaque-entry` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS` | JIT | `CRATONVM_JIT=osr-opaque-handler-headers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_OPTIMIZING` | JIT | `CRATONVM_JIT=osr-optimizing` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_OPTIMIZING_BG` | JIT | `CRATONVM_JIT=osr-optimizing-bg` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_OPTIMIZING_CACHE` | JIT | `CRATONVM_JIT=osr-optimizing-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS` | JIT | `CRATONVM_JIT=osr-optimizing-exc-exits` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS` | JIT | `CRATONVM_JIT=osr-optimizing-guard-exits` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_OSR_OPTIMIZING_MEMO` | JIT | `CRATONVM_JIT=osr-optimizing-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` | JIT | `CRATONVM_JIT=osr-park-entry-locals` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS` | JIT | `CRATONVM_JIT=osr-prefer-single-pass` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_REENTRY_MEMO` | JIT | `CRATONVM_JIT=osr-reentry-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS` | JIT | `CRATONVM_JIT=osr-seed-frame-slots` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_SINGLE_PC` | JIT | `CRATONVM_JIT=osr-single-pc` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_SKIP_ENTRY_COUNTER` | JIT | `CRATONVM_JIT=osr-skip-entry-counter` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_SPLICE` | JIT | `CRATONVM_JIT=osr-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_SPLICE_CALLS` | JIT | `CRATONVM_JIT=osr-splice-calls` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_OSR_SPLICE_ENTRY_LOOP_HOT` | JIT | `CRATONVM_JIT=osr-splice-entry-loop-hot` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES` | JIT | `CRATONVM_JIT=osr-strip-all-high-halves` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_OVERLAP_ARGS` | JIT | `CRATONVM_JIT=overlap-args` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT` | JIT | `CRATONVM_JIT=owned-args-handoff-verdict` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_PERF_MAP` | JIT | `CRATONVM_JIT=perf-map` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PERF_MAP_DIR` | JIT | `CRATONVM_JIT=perf-map-dir` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PGO_LOOP_KEYS` | JIT | `CRATONVM_JIT=pgo-loop-keys` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_PGO_UNROLL` | JIT | `CRATONVM_JIT=pgo-unroll` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_PGO_UNROLL_LEGACY_FACTOR` | JIT | `CRATONVM_JIT=pgo-unroll-legacy-factor` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_PIC_INLINE_FIRST` | JIT | `CRATONVM_JIT=pic-inline-first` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS` | JIT | `CRATONVM_JIT=pin-unnamed-frame-refs` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_POISON_FREE` | JIT | `CRATONVM_JIT=poison-free` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` | JIT | `CRATONVM_JIT=precise-frame-liveness` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PRINT_COMPILATION` | JIT | `CRATONVM_JIT=print-compilation` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PROFILE_FRAME_LEN_CHECK` | JIT | `CRATONVM_JIT=profile-frame-len-check` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_PROFILE_LOAD` | JIT | `CRATONVM_JIT=profile-load` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PROFILE_SAVE` | JIT | `CRATONVM_JIT=profile-save` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PROFILE_SAVE_ATOMIC` | JIT | `CRATONVM_JIT=profile-save-atomic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_QUIESCE_BY_GENERATION` | JIT | `CRATONVM_JIT=quiesce-by-generation` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_QUIESCE_VERDICT_MEMO` | JIT | `CRATONVM_JIT=quiesce-verdict-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_RANGE_BCE` | JIT | `CRATONVM_JIT=range-bce` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_RANGE_SCAN_LEGACY` | JIT | `CRATONVM_JIT=range-scan-legacy` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_RAX_DEAD_AT_CALL` | JIT | `CRATONVM_JIT=rax-dead-at-call` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_REASSOC` | JIT | `CRATONVM_JIT=reassoc` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_RECEIVER_DESPEC` | JIT | `CRATONVM_JIT=receiver-despec` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_RECEIVER_NULL_ELIM` | JIT | `CRATONVM_JIT=receiver-null-elim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_RECLAIM_CASCADE_PROOF` | JIT | `CRATONVM_JIT=reclaim-cascade-proof` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_REDEFINED_CLASS_MEMO` | JIT | `CRATONVM_JIT=redefined-class-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_REFUSE_DEAD_ENTRY` | JIT | `CRATONVM_JIT=refuse-dead-entry` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_REF_HOIST_OOP_MAPS` | JIT | `CRATONVM_JIT=ref-hoist-oop-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_REG_OOP_MAPS` | JIT | `CRATONVM_JIT=reg-oop-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE` | JIT | `CRATONVM_JIT=reloc-gate-map-incomplete` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE` | DBG | `CRATONVM_DBG=jit-remap-all-unverifiable` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_JIT_REMAP_UNMAPPED_DUPES` | JIT | `CRATONVM_JIT=remap-unmapped-dupes` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES` | JIT | `CRATONVM_JIT=replay-extent-handler-edges` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_RESCUE_ENTERED_UNPUBLISHED` | JIT | `CRATONVM_JIT=rescue-entered-unpublished` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_RETIRE_CELL` | JIT | `CRATONVM_JIT=retire-cell` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_RIP_SAFEPOINT_POLL` | JIT | `CRATONVM_JIT=rip-safepoint-poll` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ROTATED_PREHEADER` | JIT | `CRATONVM_JIT=rotated-preheader` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SAFEPOINT_POLLS` | JIT | `CRATONVM_JIT=safepoint-polls` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL` | JIT | `CRATONVM_JIT=safepoint-reg-spill` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SCALAR_NEW` | JIT | `CRATONVM_JIT=scalar-new` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SCALAR_UNDER_PRECISE_FRAMES` | JIT | `CRATONVM_JIT=scalar-under-precise-frames` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SCOPED_MEMORY_DIRECT` | JIT | `CRATONVM_JIT=scoped-memory-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_CALL_ARG_MAPS` | JIT | `CRATONVM_JIT=self-call-arg-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_CALL_BUDGET_FOLLOWS_STACK_SIZE` | JIT | `CRATONVM_JIT=self-call-budget-follows-stack-size` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SELF_CALL_FLOOR_STRIDE_MARGIN` | JIT | `CRATONVM_JIT=self-call-floor-stride-margin` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND` | JIT | `CRATONVM_JIT=self-locking-direct-bind` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP` | JIT | `CRATONVM_JIT=self-locking-door-skip` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SELF_LOCKING_SYNC` | JIT | `CRATONVM_JIT=self-locking-sync` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_LOCKING_SYNC_ARRAY_LOOPS` | JIT | `CRATONVM_JIT=self-locking-sync-array-loops` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS` | JIT | `CRATONVM_JIT=self-locking-sync-loops` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` | JIT | `CRATONVM_JIT=self-locking-sync-static` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` | JIT | `CRATONVM_JIT=self-lock-deopt-handover` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_LOCK_REFUSAL_MEMO` | JIT | `CRATONVM_JIT=self-lock-refusal-memo` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER` | JIT | `CRATONVM_JIT=self-lock-static-deopt-handover` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_LOCK_TRIM` | JIT | `CRATONVM_JIT=self-lock-trim` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SELF_TAILCALL` | JIT | `CRATONVM_JIT=self-tailcall` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SHADOW_BAIL_BLOCKS_MOVING` | JIT | `CRATONVM_JIT=shadow-bail-blocks-moving` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SHADOW_BAIL_STICKY` | JIT | `CRATONVM_JIT=shadow-bail-sticky` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_SINGLE_PASS_REPLAY_CHECK` | JIT | `CRATONVM_JIT=single-pass-replay-check` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SITE_CACHE` | JIT | `CRATONVM_JIT=site-cache` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_SITE_CACHE_CAPABILITY` | JIT | `CRATONVM_JIT=site-cache-capability` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SITE_CACHE_MH_INVOKE` | JIT | `CRATONVM_JIT=site-cache-mh-invoke` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SITE_CACHE_SPECIAL` | JIT | `CRATONVM_JIT=site-cache-special` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SITE_CACHE_STUBS` | JIT | `CRATONVM_JIT=site-cache-stubs` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SOE_BUILD_SUSPENDS_FLOOR` | JIT | `CRATONVM_JIT=soe-build-suspends-floor` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SPILL_ARGS_PUBLISHED` | JIT | `CRATONVM_JIT=spill-args-published` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SPILL_NARROW` | JIT | `CRATONVM_JIT=spill-narrow` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SPILL_SLOTS_CAP` | JIT | `CRATONVM_JIT=spill-slots-cap` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` | JIT | `CRATONVM_JIT=splice-chain-virtuals` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SPLICE_KEEPS_STATIC_INTRINSIC` | JIT | `CRATONVM_JIT=splice-keeps-static-intrinsic` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SP_EPOCH_GUARD_RIP` | JIT | `CRATONVM_JIT=sp-epoch-guard-rip` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_FIELD_LAYOUT_GUARD` | JIT | `CRATONVM_JIT=sp-field-layout-guard` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_DENY` | JIT | `CRATONVM_JIT=sp-ic-deny` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_DEOPT_CHECK` | JIT | `CRATONVM_JIT=sp-ic-deopt-check` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_ONLY` | JIT | `CRATONVM_JIT=sp-ic-only` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_PROTECTED_SITES` | JIT | `CRATONVM_JIT=sp-ic-protected-sites` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_IC` | JIT | `CRATONVM_JIT=sp-inline-ic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_MEGA` | JIT | `CRATONVM_JIT=sp-inline-mega` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_MIC` | JIT | `CRATONVM_JIT=sp-inline-mic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_INLINE_PIC` | JIT | `CRATONVM_JIT=sp-inline-pic` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE` | JIT | `CRATONVM_JIT=sp-sync-direct-instance` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_TAILCALL` | JIT | `CRATONVM_JIT=sp-tailcall` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_VALUEOF_INLINE_BOX` | JIT | `CRATONVM_JIT=sp-valueof-inline-box` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_ZERO_SPILL_BAND` | JIT | `CRATONVM_JIT=sp-zero-spill-band` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SR_SLOTS_PAST_LICM` | JIT | `CRATONVM_JIT=sr-slots-past-licm` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_STACK_BANG` | JIT | `CRATONVM_JIT=stack-bang` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_STACK_FLOOR_WORD` | JIT | `CRATONVM_JIT=stack-floor-word` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_STATIC_BYTECODE_CALLEE` | JIT | `CRATONVM_JIT=static-bytecode-callee` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_STATIC_INTRINSIC_FIRST` | JIT | `CRATONVM_JIT=static-intrinsic-first` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_STRICT_CALLEE_ROOTS` | JIT | `CRATONVM_JIT=strict-callee-roots` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_STRICT_INSTALL_EPOCH` | JIT | `CRATONVM_JIT=strict-install-epoch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SYNC_DIRECT` | JIT | `CRATONVM_JIT=sync-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SYNC_DIRECT_CLASS_MIRROR_SLOT` | JIT | `CRATONVM_JIT=sync-direct-class-mirror-slot` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SYNC_DIRECT_LOOKUP_MEMO` | JIT | `CRATONVM_JIT=sync-direct-lookup-memo` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SYNC_METHODS` | JIT | `CRATONVM_JIT=sync-methods` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_THIS_NONNULL` | JIT | `CRATONVM_JIT=this-nonnull` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_THRESHOLD` | JIT | `CRATONVM_JIT=threshold` | opt-in | off | behaviour | snapshot | difftest, jit, types, vm |
| `CRATONVM_JIT_TIERUP_SINK_REFUSAL_THROWS` | JIT | `CRATONVM_JIT=tierup-sink-refusal-throws` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_TIER_FROM_LIVE_BODY` | JIT | `CRATONVM_JIT=tier-from-live-body` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_TLS_THREAD_FETCH` | JIT | `CRATONVM_JIT=tls-thread-fetch` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_TRACE_ONLY_SP_ID` | JIT | `CRATONVM_JIT=trace-only-sp-id` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_TRANSFORM_CENSUS` | JIT | `CRATONVM_JIT=transform-census` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_TRAP_CAPTURE_ONE_WALK` | JIT | `CRATONVM_JIT=trap-capture-one-walk` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_TRAP_SPLICE_LAZY_LINES` | JIT | `CRATONVM_JIT=trap-splice-lazy-lines` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED` | JIT | `CRATONVM_JIT=unload-withdraws-copied` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE` | JIT | `CRATONVM_JIT=unreg-accept-residue` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNREG_MEMO_GC_RESET` | JIT | `CRATONVM_JIT=unreg-memo-gc-reset` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNREG_MEMO_HIWATER` | JIT | `CRATONVM_JIT=unreg-memo-hiwater` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNREG_RESIDUE_LICENCE` | JIT | `CRATONVM_JIT=unreg-residue-licence` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE` | JIT | `CRATONVM_JIT=unresolved-field-substitute` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNROLL` | JIT | `CRATONVM_JIT=unroll` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT` | JIT | `CRATONVM_JIT=unsafe-accessor-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNSAFE_CAS_DIRECT` | JIT | `CRATONVM_JIT=unsafe-cas-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VARHANDLE_CAS_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=varhandle-cas-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VARHANDLE_CAS_FUNNEL_FAST` | JIT | `CRATONVM_JIT=varhandle-cas-funnel-fast` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=varhandle-read-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT` | JIT | `CRATONVM_JIT=varhandle-ref-read-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS` | JIT | `CRATONVM_JIT=varhandle-write-direct-helpers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VECTORIZE` | JIT | `CRATONVM_JIT=vectorize` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VEC_EWISE_FORMS` | JIT | `CRATONVM_JIT=vec-ewise-forms` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_ARENA_ORDER` | JIT | `CRATONVM_JIT=verify-arena-order` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_BRANCHES` | JIT | `CRATONVM_JIT=verify-branches` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_FRAME_STATES` | JIT | `CRATONVM_JIT=verify-frame-states` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_IR` | JIT | `CRATONVM_JIT=verify-ir` | default-on | on | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` | JIT | `CRATONVM_JIT=verify-memory-chain` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_OPERAND_TYPES_RELEASE` | JIT | `CRATONVM_JIT=verify-operand-types-release` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_POST_OPTIMIZE` | JIT | `CRATONVM_JIT=verify-post-optimize` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_PRE_LOWER_FRAME_STATES_RELEASE` | JIT | `CRATONVM_JIT=verify-pre-lower-frame-states-release` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_SCHEDULE` | JIT | `CRATONVM_JIT=verify-schedule` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_TYPES` | JIT | `CRATONVM_JIT=verify-types` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE` | JIT | `CRATONVM_JIT=virtual-bytecode-callee` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS` | JIT | `CRATONVM_JIT=virtual-nominate-always` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_VIRTUAL_PROMOTE_HANDLER_CALLEE` | JIT | `CRATONVM_JIT=virtual-promote-handler-callee` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL` | JIT | `CRATONVM_JIT=virtual-promote-java-util` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_VIRTUAL_TIERUP` | JIT | `CRATONVM_JIT=virtual-tierup` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_VOLATILE_LOAD_FENCE` | JIT | `CRATONVM_JIT=volatile-load-fence` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_WIDE_LOCAL_OOP_MAPS` | JIT | `CRATONVM_JIT=wide-local-oop-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_WITHDRAWAL_DEMOTES` | JIT | `CRATONVM_JIT=withdrawal-demotes` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ZERO_SPID` | JIT | `CRATONVM_JIT=zero-spid` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JMX_OWNED_SYNCHRONIZERS` | THREADS | `CRATONVM_THREADS=jmx-owned-synchronizers` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_DEFINECLASS_LOADER` | COMPAT | `CRATONVM_COMPAT=jni-defineclass-loader` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FINDCLASS_INITIALIZES` | COMPAT | `CRATONVM_COMPAT=jni-findclass-initializes` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FINDCLASS_LOADER` | COMPAT | `CRATONVM_COMPAT=jni-findclass-loader` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FINDCLASS_NATIVE_HOLDER` | COMPAT | `CRATONVM_COMPAT=jni-findclass-native-holder` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FINDCLASS_RAISES` | COMPAT | `CRATONVM_COMPAT=jni-findclass-raises` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FOREIGN_TRANSITIONS` | GC | `CRATONVM_GC=jni-foreign-transitions` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JNI_INDIRECT_LOCALS` | GC | `CRATONVM_GC=jni-indirect-locals` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JNI_JCLASS_MIRROR` | GC | `CRATONVM_GC=jni-jclass-mirror` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_NATIVE_TRANSITIONS` | GC | `CRATONVM_GC=jni-native-transitions` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JNI_UPCALL_TYPED_ERRORS` | COMPAT | `CRATONVM_COMPAT=jni-upcall-typed-errors` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_LAZY_STREAMS` | COMPAT | `CRATONVM_COMPAT=lazy-streams` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_LDC_CLASSREF_TRACE` | DBG | `CRATONVM_DBG=ldc-classref-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_LENIENT_CLINIT` | LOADER | `CRATONVM_LOADER=lenient-clinit` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LHM_ROOT_ALL` | GC | `CRATONVM_GC=lhm-root-all` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_LIQUIBASE_DATE_GETTIME` | COMPAT | `CRATONVM_COMPAT=liquibase-date-gettime` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_LOADER` | LOADER | `CRATONVM_LOADER=…` | group | unset | — | snapshot | — |
| `CRATONVM_LOADER_AWARE_RESOLUTION` | LOADER | `CRATONVM_LOADER=aware-resolution` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_LOADER_NO_ANN_PROXY_LATCH` | LOADER | `CRATONVM_LOADER=ann-proxy-latch` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE` | LOADER | `CRATONVM_LOADER=dup-name-field-gate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD` | LOADER | `CRATONVM_LOADER=resolution-failure-record` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_LOADER_NO_SUBTYPE_DISPLAY` | LOADER | `CRATONVM_LOADER=subtype-display` | opt-out | on | behaviour | snapshot | classloading |
| `CRATONVM_LOADER_PARENT_CHAIN` | LOADER | `CRATONVM_LOADER=parent-chain` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_LOADER_UNLOAD` | LOADER | `CRATONVM_LOADER=unload` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LOCK_ORDER_CHECK` | THREADS | `CRATONVM_THREADS=lock-order-check` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_LONGREWRITE_LOOSE` | LOADER | `CRATONVM_LOADER=longrewrite-loose` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LONGROOT_STRICT` | JIT | `CRATONVM_JIT=longroot-strict` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MAP_VIEW_CACHE` | COMPAT | `CRATONVM_COMPAT=map-view-cache` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_MATH_FMA_EXACT` | JIT | `CRATONVM_JIT=math-fma-exact` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MATH_FMA_NATIVE` | JIT | `CRATONVM_JIT=math-fma-native` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MAVEN_REPO_LOCAL` | — | `CRATONVM_MAVEN_REPO_LOCAL` | scalar | unset | behaviour | snapshot | native-builtins, types |
| `CRATONVM_MAX_INFLATED_BYTES` | GC | `CRATONVM_GC=max-inflated-bytes` | opt-in | off | behaviour | snapshot | native-builtins, types |
| `CRATONVM_MH_CAST_CHAIN` | COMPAT | `CRATONVM_COMPAT=mh-cast-chain` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_CAST_CHAIN_CONVERTS` | COMPAT | `CRATONVM_COMPAT=mh-cast-chain-converts` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_CAST_CHAIN_CROSS_VARIANT` | COMPAT | `CRATONVM_COMPAT=mh-cast-chain-cross-variant` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_COMBINATOR_CATCHES_NATIVE_THROWS` | COMPAT | `CRATONVM_COMPAT=mh-combinator-catches-native-throws` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_DIRECT_LANE` | JIT | `CRATONVM_JIT=mh-direct-lane` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_DIRECT_LANE_RAW_RETURN` | JIT | `CRATONVM_JIT=mh-direct-lane-raw-return` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_EXPLICIT_INTERFACE_UNCAST` | COMPAT | `CRATONVM_COMPAT=mh-explicit-interface-uncast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_FIND_SPECIAL_JDK` | COMPAT | `CRATONVM_COMPAT=mh-find-special-jdk` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_INVOKEEXACT_INNER_CAST` | COMPAT | `CRATONVM_COMPAT=mh-invokeexact-inner-cast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN` | COMPAT | `CRATONVM_COMPAT=mh-invoke-boxes-declared-return` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_INVOKE_DECLARED_CAST` | COMPAT | `CRATONVM_COMPAT=mh-invoke-declared-cast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED` | COMPAT | `CRATONVM_COMPAT=mh-invoke-declared-cast-skips-typed` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_RETURN_CAST` | COMPAT | `CRATONVM_COMPAT=mh-return-cast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_SPECIAL_CALLER_CAST` | COMPAT | `CRATONVM_COMPAT=mh-special-caller-cast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_STRICT_INVOKEEXACT` | COMPAT | `CRATONVM_COMPAT=mh-strict-invokeexact` | default-on | on | behaviour | snapshot | native-builtins, vm |
| `CRATONVM_MOCKITO_LEGACY_SELECTORS` | COMPAT | `CRATONVM_COMPAT=mockito-legacy-selectors` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_MODULE_MIRROR_LAYER_SLOT_GUARD` | COMPAT | `CRATONVM_COMPAT=module-mirror-layer-slot-guard` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MONITOR_CACHED_NOTIFY` | THREADS | `CRATONVM_THREADS=monitor-cached-notify` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_ENTER_SPIN` | THREADS | `CRATONVM_THREADS=monitor-enter-spin` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_MONITOR_FASTPATH` | THREADS | `CRATONVM_THREADS=monitor-fastpath` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_MONITOR_INDEX_PRUNE` | THREADS | `CRATONVM_THREADS=monitor-index-prune` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET` | THREADS | `CRATONVM_THREADS=monitor-interrupt-wakes-target` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_LAZY_PARK_US` | — | `CRATONVM_MONITOR_LAZY_PARK_US` | scalar | unset | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_LAZY_SPINNERS` | THREADS | `CRATONVM_THREADS=monitor-lazy-spinners` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_MAX_SPINNERS` | — | `CRATONVM_MONITOR_MAX_SPINNERS` | scalar | unset | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_NOTIFY_ONE_WAITER` | THREADS | `CRATONVM_THREADS=monitor-notify-one-waiter` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_PENDING_NOTIFY` | THREADS | `CRATONVM_THREADS=monitor-pending-notify` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_QUIET_RELEASE` | THREADS | `CRATONVM_THREADS=monitor-quiet-release` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_SPIN_AFTER_WAKE` | THREADS | `CRATONVM_THREADS=monitor-spin-after-wake` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_SPIN_BACKOFF` | THREADS | `CRATONVM_THREADS=monitor-spin-backoff` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT` | THREADS | `CRATONVM_THREADS=monitor-spin-handover-abort` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_THIN_NOTIFY` | THREADS | `CRATONVM_THREADS=monitor-thin-notify` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK` | THREADS | `CRATONVM_THREADS=monitor-wait-enrol-interrupt-check` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_WAIT_POLL_BACKOFF` | THREADS | `CRATONVM_THREADS=monitor-wait-poll-backoff` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN` | THREADS | `CRATONVM_THREADS=monitor-wait-reacquire-spin` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_WAIT_SINGLE_PARK` | THREADS | `CRATONVM_THREADS=monitor-wait-single-park` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG` | GC | `CRATONVM_GC=moving-young` | both | off | behaviour | snapshot | jit, types |
| `CRATONVM_MOVING_YOUNG_BAND_DBG` | DBG | `CRATONVM_DBG=moving-young-band-dbg` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_BAND_SKIP_IN_MAP` | GC | `CRATONVM_GC=moving-young-band-skip-in-map` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_COVERAGE_DBG` | DBG | `CRATONVM_DBG=moving-young-coverage-dbg` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_FALLBACKS` | DBG | `CRATONVM_DBG=moving-young-fallbacks` | opt-in | off | diag | snapshot | types |
| `CRATONVM_MOVING_YOUNG_NO_BAND_LIVENESS_SCREEN` | GC | `CRATONVM_GC=moving-young-band-liveness-screen` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_BAND_OBJECT_SCREEN` | GC | `CRATONVM_GC=moving-young-band-object-screen` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_BAND_THREAD_WINDOW` | GC | `CRATONVM_GC=moving-young-band-thread-window` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` | DBG | `CRATONVM_DBG=moving-young-no-band-verify` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD` | GC | `CRATONVM_GC=moving-young-bounds-guard` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_NO_JIT` | GC | `CRATONVM_GC=moving-young-jit-frames` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_MOVING_YOUNG_VERIFY` | DBG | `CRATONVM_DBG=moving-young-verify` | opt-in | off | diag | snapshot | types |
| `CRATONVM_MSC_REAL_START` | REAL | `CRATONVM_REAL=msc-real-start` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NATIVE_CALLBACK_MEMO` | JIT | `CRATONVM_JIT=native-callback-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC` | JIT | `CRATONVM_JIT=native-callback-memo-static` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_NATIVE_CF_COMPLETE` | JIT | `CRATONVM_JIT=native-cf-complete` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT` | JIT | `CRATONVM_JIT=native-cf-postcomplete-direct` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP` | JIT | `CRATONVM_JIT=native-cf-postcomplete-skip` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_NATIVE_CREATE_STRING_HIT_OR_FRESH` | COMPAT | `CRATONVM_COMPAT=native-create-string-hit-or-fresh` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_NATIVE_EC_MULTIPLY` | JIT | `CRATONVM_JIT=native-ec-multiply` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_NATIVE_ENCODING` | — | `CRATONVM_NATIVE_ENCODING` | scalar | unset | behaviour | snapshot | native-api, vm |
| `CRATONVM_NATIVE_INVOKE_STRICT_SELECTION` | COMPAT | `CRATONVM_COMPAT=native-invoke-strict-selection` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_NATIVE_MATCHER_FIND` | JIT | `CRATONVM_JIT=native-matcher-find` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_NATIVE_NO_READ_CURSOR` | GC | `CRATONVM_GC=read-cursor` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | JIT | `CRATONVM_JIT=native-pbe-keyfactory` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_NATIVE_SHADOW_SINK_CAP` | DBG | `CRATONVM_DBG=native-shadow-sink-cap` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_NATIVE_STRING_REGEX` | JIT | `CRATONVM_JIT=native-string-regex` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_NATIVE_SYNC` | JIT | `CRATONVM_JIT=native-sync` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_NEEDS_EXACT_TRACE` | DBG | `CRATONVM_DBG=needs-exact-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | IO | `CRATONVM_IO=netty-queue-bridge` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NET_CLOSE_SKIP_SHUTDOWN` | IO | `CRATONVM_IO=net-close-skip-shutdown` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_NET_EVENT_WAITS` | IO | `CRATONVM_IO=net-event-waits` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_NET_JDK_BACKLOG` | IO | `CRATONVM_IO=net-jdk-backlog` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_NONEXISTENT_VAR_12345` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | absent-name probe |
| `CRATONVM_NORMALIZER_MAYBE_FULL_CHECK` | COMPAT | `CRATONVM_COMPAT=normalizer-maybe-full-check` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP` | GC | `CRATONVM_GC=blocked-wake-jit-remap` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_CONSERVATIVE_LOCALS` | JIT | `CRATONVM_JIT=conservative-locals` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_CTOR_DIRECT_CALL` | JIT | `CRATONVM_JIT=ctor-direct-call` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_DEFRAG_PROMOTE` | GC | `CRATONVM_GC=defrag-promote` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` | GC | `CRATONVM_GC=exact-refproc-survival` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_EXECUTE_SYNC` | JIT | `CRATONVM_JIT=execute-sync` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_FORMAT_ARG_PIN` | GC | `CRATONVM_GC=format-arg-pin` | opt-out | on | behaviour | snapshot | native-builtins |
| `CRATONVM_NO_GC_PROMOTION_GUARD` | GC | `CRATONVM_GC=promotion-guard` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_IR_BOX_UNBOX_FOLD` | JIT | `CRATONVM_JIT=ir-box-unbox-fold` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_IR_BRANCHY` | JIT | `CRATONVM_JIT=ir-branchy` | opt-out | on | behaviour | snapshot | difftest, jit |
| `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` | JIT | `CRATONVM_JIT=alloc-class-cache` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS` | JIT | `CRATONVM_JIT=arraylist-intrinsics` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME` | JIT | `CRATONVM_JIT=callee-handler-precise-frame` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD` | JIT | `CRATONVM_JIT=inline-putfield` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW` | JIT | `CRATONVM_JIT=inline-tlab-new` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY` | JIT | `CRATONVM_JIT=inline-tlab-newarray` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES` | JIT | `CRATONVM_JIT=precise-handler-frames` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_SB_INTRINSICS` | JIT | `CRATONVM_JIT=sb-intrinsics` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_STAGED_ARG_SLOT` | JIT | `CRATONVM_JIT=staged-arg-slot` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_TLAB_ZERO_ELISION` | JIT | `CRATONVM_JIT=tlab-zero-elision` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_LOCAL_LIVENESS` | JIT | `CRATONVM_JIT=local-liveness` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_MAP_ITERATOR_FAILFAST` | COMPAT | `CRATONVM_COMPAT=map-iterator-failfast` | opt-out | on | behaviour | snapshot | native-collections |
| `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` | GC | `CRATONVM_GC=mirror-pin-young-defer` | opt-out | on | behaviour | snapshot | gc, vm |
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
| `CRATONVM_NUMBER_STRINGS_UNINTERNED` | JIT | `CRATONVM_JIT=number-strings-uninterned` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_OLDGEN_COMPACT` | GC | `CRATONVM_GC=oldgen-compact` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_OLD_SWEEP_JIT` | JIT | `CRATONVM_JIT=old-sweep-jit` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` | JIT | `CRATONVM_JIT=omit-stack-trace-in-fast-throw` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_OOP_SPAN_PROBE` | DBG | `CRATONVM_DBG=oop-span-probe` | opt-in | off | diag | snapshot | types |
| `CRATONVM_OSR_COVERAGE_SHADOW` | GC | `CRATONVM_GC=osr-coverage-shadow` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_OSR_EXIT_AFTER` | DBG | `CRATONVM_DBG=osr-exit-after` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_OSR_EXIT_TEST` | DBG | `CRATONVM_DBG=osr-exit-test` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_OSR_NEWARRAY` | JIT | `CRATONVM_JIT=osr-newarray` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER` | COMPAT | `CRATONVM_COMPAT=override-package-by-loader` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_OVERRIDE_SHADOW_PER_SIGNATURE` | COMPAT | `CRATONVM_COMPAT=override-shadow-per-signature` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_OWNER_CLASS_FILTER` | GC | `CRATONVM_GC=owner-class-filter` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_PACK_FIELDS_BY_WIDTH` | GC | `CRATONVM_GC=pack-fields-by-width` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_PHASE_ACCOUNTING` | DBG | `CRATONVM_DBG=phase-accounting` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PHASE_ACCOUNTING_JFR` | DBG | `CRATONVM_DBG=phase-accounting-jfr` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PHASE_ACCOUNTING_OUT` | DBG | `CRATONVM_DBG=phase-accounting-out` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PRECISE_COVERAGE_PIN` | JIT | `CRATONVM_JIT=precise-coverage-pin` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_PROFILE_SAMPLE_MS` | DBG | `CRATONVM_DBG=profile-sample-ms` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_PROMOTION_OOM_GUARD_BROAD` | GC | `CRATONVM_GC=promotion-oom-guard-broad` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_PROPS_UNROOTED_RECEIVERS` | REAL | `CRATONVM_REAL=props-unrooted-receivers` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_ANNOTATION_BODY` | COMPAT | `CRATONVM_COMPAT=proxy-annotation-body` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_BODY_HANDLER_VIRTUAL` | COMPAT | `CRATONVM_COMPAT=proxy-body-handler-virtual` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_BODY_NULL_PRIMITIVE_NPE` | COMPAT | `CRATONVM_COMPAT=proxy-body-null-primitive-npe` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_DYNAMIC_MODULE` | COMPAT | `CRATONVM_COMPAT=proxy-dynamic-module` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_FINAL_OBJECT_METHODS` | COMPAT | `CRATONVM_COMPAT=proxy-final-object-methods` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_INIT_AT_CREATION` | COMPAT | `CRATONVM_COMPAT=proxy-init-at-creation` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_INVOKE_DEFAULT_CHECKS` | COMPAT | `CRATONVM_COMPAT=proxy-invoke-default-checks` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_JDK_CLASS_FLAGS` | COMPAT | `CRATONVM_COMPAT=proxy-jdk-class-flags` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_PROXY_JDK_METHOD_ORDER` | COMPAT | `CRATONVM_COMPAT=proxy-jdk-method-order` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_MERGED_THROWS` | COMPAT | `CRATONVM_COMPAT=proxy-merged-throws` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_MODULE_ACCESS_REFUSAL` | COMPAT | `CRATONVM_COMPAT=proxy-module-access-refusal` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_MODULE_DESCRIPTOR` | COMPAT | `CRATONVM_COMPAT=proxy-module-descriptor` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_NONEXPORTED_PACKAGE` | COMPAT | `CRATONVM_COMPAT=proxy-nonexported-package` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_NONPUBLIC_CLASS_FLAGS` | COMPAT | `CRATONVM_COMPAT=proxy-nonpublic-class-flags` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_NONPUBLIC_IN_IFACE_LOADER` | COMPAT | `CRATONVM_COMPAT=proxy-nonpublic-in-iface-loader` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_NONPUBLIC_REFUSALS` | COMPAT | `CRATONVM_COMPAT=proxy-nonpublic-refusals` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_ONE_DISPATCH_MODEL` | COMPAT | `CRATONVM_COMPAT=proxy-one-dispatch-model` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_TYPES_THROUGH_LOADER` | COMPAT | `CRATONVM_COMPAT=proxy-types-through-loader` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY` | COMPAT | `CRATONVM_COMPAT=proxy-user-subclass-ordinary` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_PROXY_VARARGS_FLAG` | COMPAT | `CRATONVM_COMPAT=proxy-varargs-flag` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_PROXY_VTABLE_FAST_PATH` | COMPAT | `CRATONVM_COMPAT=proxy-vtable-fast-path` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_QUICKEN_STATS` | DBG | `CRATONVM_DBG=quicken-stats` | opt-in | off | diag | snapshot | reader |
| `CRATONVM_QUIET_DEPRECATIONS` | DBG | `CRATONVM_DBG=deprecations` | opt-out | on | diag | snapshot | vm-cli |
| `CRATONVM_QUIET_ENV_FALLBACK` | DBG | `CRATONVM_DBG=quiet-env-fallback` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_RANDOM_LAYOUT_MEMO` | COMPAT | `CRATONVM_COMPAT=random-layout-memo` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_RANDOM_REAL_SEED_FIELD` | COMPAT | `CRATONVM_COMPAT=random-real-seed-field` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_RANDOM_SERIAL_STATE` | COMPAT | `CRATONVM_COMPAT=random-serial-state` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_RANDOM_SUBCLASS_YIELD` | COMPAT | `CRATONVM_COMPAT=random-subclass-yield` | default-on | on | behaviour | snapshot | native-builtins |
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
| `CRATONVM_REFLECTIVE_DISPATCH_SELECTS` | COMPAT | `CRATONVM_COMPAT=reflective-dispatch-selects` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_REFLECT_NO_EXPORT_GATE` | SECURITY | `CRATONVM_SECURITY=reflect-export-gate` | opt-out | on | behaviour | snapshot | native-builtins |
| `CRATONVM_REGEN_HEADER` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | libcratonvm/build.rs |
| `CRATONVM_REGISTER_IMAGE_REMAP` | GC | `CRATONVM_GC=register-image-remap` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_REQUIRE_POLICY` | SECURITY | `CRATONVM_SECURITY=require-policy` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_RESOLVE_CACHE_CAP` | LOADER | `CRATONVM_LOADER=resolve-cache-cap` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | IO | `CRATONVM_IO=resolve-outbound-host` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES` | LOADER | `CRATONVM_LOADER=retired-shadow-masks-ancestor-bridges` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_ROOTSNAP_CACHE` | JIT | `CRATONVM_JIT=rootsnap-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC` | JIT | `CRATONVM_JIT=rootsnap-cache-survive-gc` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests opt-in |
| `CRATONVM_S111_DBG` | DBG | `CRATONVM_DBG=s111-dbg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SCALAR_DEOPT` | JIT | `CRATONVM_JIT=scalar-deopt` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_SCANNER_DEBUG` | DBG | `CRATONVM_DBG=scanner-debug` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_SC_BB_SLOTS` | JIT | `CRATONVM_JIT=sc-bb-slots` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_SC_IO_STATS` | DBG | `CRATONVM_DBG=sc-io-stats` | opt-in | off | diag | snapshot | native-io |
| `CRATONVM_SC_PRERESOLVED` | JIT | `CRATONVM_JIT=sc-preresolved` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_SC_SCRATCH` | JIT | `CRATONVM_JIT=sc-scratch` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_SECURITY` | SECURITY | `CRATONVM_SECURITY=…` | group | unset | — | snapshot | types |
| `CRATONVM_SELECT_MAX_BLOCK_MS` | IO | `CRATONVM_IO=select-max-block-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_SEL_FAST_KEYS` | JIT | `CRATONVM_JIT=sel-fast-keys` | default-on | on | behaviour | snapshot | native-io |
| `CRATONVM_SEL_READY_CACHE` | JIT | `CRATONVM_JIT=sel-ready-cache` | default-on | on | behaviour | snapshot | native-io |
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
| `CRATONVM_SOFTREF_HOTSPOT_LRU` | GC | `CRATONVM_GC=softref-hotspot-lru` | default-on | gen-on | behaviour | snapshot | gc, types |
| `CRATONVM_SOFT_EXIT` | DBG | `CRATONVM_DBG=soft-exit` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SOMETHING_BRAND_NEW` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | unknown-key fall-through probe |
| `CRATONVM_SPRING_BOOT_FATJAR` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests fixture path |
| `CRATONVM_SPRING_DBG` | DBG | `CRATONVM_DBG=spring-dbg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SP_NO_COALESCE` | JIT | `CRATONVM_JIT=sp-coalesce` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SP_STATS` | DBG | `CRATONVM_DBG=sp-stats` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SP_TRACE` | DBG | `CRATONVM_DBG=sp-trace` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SP_VERIFY` | DBG | `CRATONVM_DBG=sp-verify` | opt-in | off | diag | snapshot | types |
| `CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS` | COMPAT | `CRATONVM_COMPAT=stack-trace-element-exact-class` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN` | COMPAT | `CRATONVM_COMPAT=stack-trace-element-full-origin` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO` | COMPAT | `CRATONVM_COMPAT=stack-trace-element-origin-memo` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_STACK_TRACE_MODULE_PREFIX` | COMPAT | `CRATONVM_COMPAT=stack-trace-module-prefix` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_STACK_WALK_THREAD_RUN_FRAMES` | COMPAT | `CRATONVM_COMPAT=stack-walk-thread-run-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_STDOUT_ENCODING` | — | `CRATONVM_STDOUT_ENCODING` | scalar | unset | behaviour | snapshot | native-api |
| `CRATONVM_STRESS_THREAD_STATES` | THREADS | `CRATONVM_THREADS=stress-thread-states` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_STRICT_JIT_ROOTS` | JIT | `CRATONVM_JIT=strict-jit-roots` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_STRICT_SWALLOWS` | COMPAT | `CRATONVM_COMPAT=strict-swallows` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_STRIPED_COUNTERS_OFF` | THREADS | `CRATONVM_THREADS=striped-counters` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SUREFIRE_IPC_DBG` | DBG | `CRATONVM_DBG=surefire-ipc-dbg` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SW_JDK_WALK` | COMPAT | `CRATONVM_COMPAT=stackwalker-jdk-walk` | opt-in | off | behaviour | snapshot | native-builtins |
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
| `CRATONVM_THREAD_INHERITS_DAEMON` | COMPAT | `CRATONVM_COMPAT=thread-inherits-daemon` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THREAD_MIRROR_DAEMON` | COMPAT | `CRATONVM_COMPAT=thread-mirror-daemon` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THREAD_STACK_CALLEE_LINES` | COMPAT | `CRATONVM_COMPAT=thread-stack-callee-lines` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THREAD_STACK_PARKED_LEAF_FRAMES` | COMPAT | `CRATONVM_COMPAT=thread-stack-parked-leaf-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THREAD_START_GRACE_MS` | THREADS | `CRATONVM_THREADS=thread-start-grace-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_CAUSE_CTOR_FILL_FIRST` | COMPAT | `CRATONVM_COMPAT=throwable-cause-ctor-fill-first` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_CTOR_FILL_OVERRIDE_EXACT` | COMPAT | `CRATONVM_COMPAT=throwable-ctor-fill-override-exact` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_CTOR_HONOURS_FILL_OVERRIDE` | COMPAT | `CRATONVM_COMPAT=throwable-ctor-honours-fill-override` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_FILL_OVERRIDE_BRIDGES` | COMPAT | `CRATONVM_COMPAT=throwable-fill-override-bridges` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_FILL_OVERRIDE_DECIDES` | COMPAT | `CRATONVM_COMPAT=throwable-fill-override-decides` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-native-leaf-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_NATIVE_STANDIN_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-native-standin-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_NATIVE_TRACE_FIELD` | COMPAT | `CRATONVM_COMPAT=throwable-native-trace-field` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_PRINT_CAUSE_LOOP` | COMPAT | `CRATONVM_COMPAT=throwable-print-cause-loop` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_RECEIVER_NPE_SCREEN` | COMPAT | `CRATONVM_COMPAT=throwable-receiver-npe-screen` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-standin-arg-check-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_SITE_HINT` | COMPAT | `CRATONVM_COMPAT=throwable-standin-arg-check-site-hint` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-standin-array-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_JOIN_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-standin-join-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_OWNER_RESOLUTION` | COMPAT | `CRATONVM_COMPAT=throwable-standin-owner-resolution` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-standin-timed-join-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_STANDIN_VIRTUAL_JOIN_DECLINE` | COMPAT | `CRATONVM_COMPAT=throwable-standin-virtual-join-decline` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_STANDIN_VIRTUAL_WAIT_FRAMES` | COMPAT | `CRATONVM_COMPAT=throwable-standin-virtual-wait-frames` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_SUBCLASS_FIELDS_AFTER_FILL` | COMPAT | `CRATONVM_COMPAT=throwable-subclass-fields-after-fill` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THROWABLE_THREAD_RUN_FRAME` | COMPAT | `CRATONVM_COMPAT=throwable-thread-run-frame` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_THREAD_RUN_LAMBDA_SCREEN` | COMPAT | `CRATONVM_COMPAT=throwable-thread-run-lambda-screen` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_THREAD_RUN_MEMO` | COMPAT | `CRATONVM_COMPAT=throwable-thread-run-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_THREAD_RUN_MIDDLE_FRAME` | COMPAT | `CRATONVM_COMPAT=throwable-thread-run-middle-frame` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_TIER_C1_THREADS` | JIT | `CRATONVM_JIT=tier-c1-threads` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C1_THRESHOLD` | JIT | `CRATONVM_JIT=tier-c1-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_MIN_INVOCATIONS` | JIT | `CRATONVM_JIT=tier-c2-min-invocations` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_THREADS` | JIT | `CRATONVM_JIT=tier-c2-threads` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_THRESHOLD` | JIT | `CRATONVM_JIT=tier-c2-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_ENABLED` | JIT | `CRATONVM_JIT=tiered` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_TIER_OSR_BACKEDGE` | JIT | `CRATONVM_JIT=tier-osr-backedge` | opt-in | off | behaviour | snapshot | difftest, vm |
| `CRATONVM_TIER_OSR_THRESHOLD` | JIT | `CRATONVM_JIT=tier-osr-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_PGO` | JIT | `CRATONVM_JIT=tier-pgo` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TIER_PGO_ALWAYS` | JIT | `CRATONVM_JIT=tier-pgo-always` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TIER_PGO_C2_WINDOW` | JIT | `CRATONVM_JIT=tier-pgo-c2-window` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_TIER_PGO_RECEIVERS` | JIT | `CRATONVM_JIT=tier-pgo-receivers` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_TLAB_FILLER_SKIP_ZERO` | GC | `CRATONVM_GC=tlab-filler-skip-zero` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_TLAB_GATE_BUMP_FLOOR` | GC | `CRATONVM_GC=tlab-gate-bump-floor` | opt-in | off | behaviour | snapshot | types, vm |
| `CRATONVM_TLAB_GC_TRIGGER` | GC | `CRATONVM_GC=tlab-gc-trigger` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TLAB_SHARE_SIZER` | GC | `CRATONVM_GC=tlab-share-sizer` | opt-in | off | behaviour | snapshot | gc, types |
| `CRATONVM_TLAB_SHARE_SIZER_MAX_KIB` | GC | `CRATONVM_GC=tlab-share-sizer-max-kib` | opt-in | off | behaviour | snapshot | gc |
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
| `CRATONVM_UNRETIRE_NATIVE_SHADOW` | LOADER | `CRATONVM_LOADER=unretire-native-shadow` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_UNTRUSTED_CODE` | SECURITY | `CRATONVM_SECURITY=untrusted-code` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_URI_STRICT_CHARS` | IO | `CRATONVM_IO=uri-strict-chars` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | REAL | `CRATONVM_REAL=use-wildfly-reflect-shim` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | REAL | `CRATONVM_REAL=use-wildfly-synth-bytecode` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_VECTOR_INTRINSICS` | JIT | `CRATONVM_JIT=vector-intrinsics` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VECTOR_INTRINSICS_STATS` | DBG | `CRATONVM_DBG=vector-intrinsics-stats` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_VECTOR_TEMPLATES` | JIT | `CRATONVM_JIT=vector-templates` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VERIFY_MAP_VIEW_CACHE` | COMPAT | `CRATONVM_COMPAT=verify-map-view-cache` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_VH_ARITY_WMTE` | COMPAT | `CRATONVM_COMPAT=vh-arity-wmte` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VH_INDIRECT_REFUSE` | COMPAT | `CRATONVM_COMPAT=vh-indirect-refuse` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VH_INDIRECT_SERVE` | COMPAT | `CRATONVM_COMPAT=vh-indirect-serve` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VH_NULL_COORDINATE_NPE` | COMPAT | `CRATONVM_COMPAT=vh-null-coordinate-npe` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VH_READ_ONLY_HANDLE_UOE` | COMPAT | `CRATONVM_COMPAT=vh-read-only-handle-uoe` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_VH_STRICT_REFERENCE_RETURN` | COMPAT | `CRATONVM_COMPAT=vh-strict-reference-return` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_VH_UNSUPPORTED_MODE_UOE` | COMPAT | `CRATONVM_COMPAT=vh-unsupported-mode-uoe` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_WAIT_SPURIOUS_MS` | THREADS | `CRATONVM_THREADS=wait-spurious-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_WEAKREF_CLEAR` | GC | `CRATONVM_GC=weakref-clear` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_WIN_HIRES_PARK` | THREADS | `CRATONVM_THREADS=win-hires-park` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XBOOTCLASSPATH_APPEND` | COMPAT | `CRATONVM_COMPAT=xbootclasspath-append` | default-on | on | behaviour | snapshot | vm, vm-cli |
| `CRATONVM_XT_BLOCKED_MONITOR_PROOF` | JIT | `CRATONVM_JIT=xt-blocked-monitor-proof` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_XT_FIRST_PASS_GRACE_US` | JIT | `CRATONVM_JIT=xt-first-pass-grace-us` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_HELPER_WINDOW_DISCHARGE` | JIT | `CRATONVM_JIT=xt-helper-window-discharge` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_HELPER_WINDOW_PIN` | JIT | `CRATONVM_JIT=xt-helper-window-pin` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE` | JIT | `CRATONVM_JIT=xt-helper-window-pin-resolve` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_HELPER_WINDOW_SCAN` | JIT | `CRATONVM_JIT=xt-helper-window-scan` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_JIT_COVERAGE_ASSUME` | GC | `CRATONVM_GC=xt-jit-coverage-assume` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE` | GC | `CRATONVM_GC=xt-jit-coverage-handshake` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_JIT_ROOT_SCAN` | JIT | `CRATONVM_JIT=xt-jit-root-scan` | opt-in | off | behaviour | snapshot | jit, vm |
| `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE` | JIT | `CRATONVM_JIT=xt-keep-unrewritable-on-discharge` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_NO_SAFE_PEER_READ` | JIT | `CRATONVM_JIT=xt-no-safe-peer-read` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_PARKED_SPIN_ONLY` | JIT | `CRATONVM_JIT=xt-parked-spin-only` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_PEER_DEADLINE_MS` | JIT | `CRATONVM_JIT=xt-peer-deadline-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_PEER_SHADOW_SCAN` | JIT | `CRATONVM_JIT=xt-peer-shadow-scan` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_PEER_TOTAL_MS` | JIT | `CRATONVM_JIT=xt-peer-total-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_PINNED_PEER_DEPTH` | JIT | `CRATONVM_JIT=xt-pinned-peer-depth` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY` | JIT | `CRATONVM_JIT=xt-pinned-peer-publish-only` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_PINNED_PEER_UNPINNABLE` | JIT | `CRATONVM_JIT=xt-pinned-peer-unpinnable` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_ROOT_SCAN_AUDIT` | JIT | `CRATONVM_JIT=xt-root-scan-audit` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_XT_TAKEOVER_INTERIOR` | JIT | `CRATONVM_JIT=xt-takeover-interior` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY` | JIT | `CRATONVM_JIT=xt-takeover-signal-jit-only` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_YOUNGSCAN_STRIDE` | GC | `CRATONVM_GC=youngscan-stride` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_ZGC_ALLOC_TRIGGER` | GC | `CRATONVM_GC=zgc-alloc-trigger` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_ASSUME_REWRITABLE` | GC | `CRATONVM_GC=zgc-assume-rewritable` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_BITMAP_BOUNDS` | GC | `CRATONVM_GC=zgc-bitmap-bounds` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_BITMAP_SWEEP` | GC | `CRATONVM_GC=zgc-bitmap-sweep` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_CENSUS` | GC | `CRATONVM_GC=zgc-census` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_CENSUS_ACCESS` | GC | `CRATONVM_GC=zgc-census-access` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_CONC_START` | GC | `CRATONVM_GC=zgc-conc-start` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_CONC_WORKERS` | GC | `CRATONVM_GC=zgc-conc-workers` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GENERATIONAL` | GC | `CRATONVM_GC=zgc-generational` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` | GC | `CRATONVM_GC=zgc-gen-minors-per-major` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GEN_NURSERY_PERCENT` | GC | `CRATONVM_GC=zgc-gen-nursery-percent` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_GEN_PROMOTION_AGE` | GC | `CRATONVM_GC=zgc-gen-promotion-age` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM` | GC | `CRATONVM_GC=zgc-headroom-bypasses-rearm` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_HIGH_COMPACTION` | GC | `CRATONVM_GC=zgc-high-compaction` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_JIT_BLANKET_REFUSAL` | GC | `CRATONVM_GC=zgc-jit-blanket-refusal` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` | GC | `CRATONVM_GC=zgc-jit-inline-announce` | default-on | on | behaviour | snapshot | gc, jit |
| `CRATONVM_ZGC_JIT_TLAB` | GC | `CRATONVM_GC=zgc-jit-tlab` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARKBITS` | GC | `CRATONVM_GC=zgc-markbits` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARK_CTX_DIRECT` | GC | `CRATONVM_GC=zgc-mark-ctx-direct` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARK_POOL_PERSISTENT` | GC | `CRATONVM_GC=zgc-mark-pool-persistent` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARK_REF_CHUNK` | GC | `CRATONVM_GC=zgc-mark-ref-chunk` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARK_ROOT_FILTER` | GC | `CRATONVM_GC=zgc-mark-root-filter` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MARK_ROOT_FILTER_CONC` | GC | `CRATONVM_GC=zgc-mark-root-filter-conc` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_METRICS` | GC | `CRATONVM_GC=zgc-metrics` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_METRICS_RUN` | GC | `CRATONVM_GC=zgc-metrics-run` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_METRICS_TSV` | GC | `CRATONVM_GC=zgc-metrics-tsv` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_MOVABLE_COMMIT_SCREEN` | GC | `CRATONVM_GC=zgc-movable-commit-screen` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_NO_JIT_LOAD_BARRIER` | GC | `CRATONVM_GC=zgc-jit-load-barrier` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_ZGC_NO_JIT_READ_BOUNDS` | GC | `CRATONVM_GC=zgc-jit-read-bounds` | opt-out | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PAGE_EVAC` | GC | `CRATONVM_GC=zgc-page-evac` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PAGE_PINNED_RELOCATE` | GC | `CRATONVM_GC=zgc-page-pinned-relocate` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PAGE_SURVEY` | GC | `CRATONVM_GC=zgc-page-survey` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PARMARK` | GC | `CRATONVM_GC=zgc-parmark` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PARSWEEP` | GC | `CRATONVM_GC=zgc-parsweep` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PAR_RELOCATE` | GC | `CRATONVM_GC=zgc-par-relocate` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE` | GC | `CRATONVM_GC=zgc-pause-target-includes-relocate` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PAUSE_TARGET_MS` | GC | `CRATONVM_GC=zgc-pause-target-ms` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PRISTINE_CHUNKS` | GC | `CRATONVM_GC=zgc-pristine-chunks` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_PUBLISH_VACATED` | GC | `CRATONVM_GC=zgc-publish-vacated` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_READABLE_GIVE_BACK` | GC | `CRATONVM_GC=zgc-readable-give-back` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_RELOCATE` | GC | `CRATONVM_GC=zgc-relocate` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_RELOCATE_BUDGET_MB` | GC | `CRATONVM_GC=zgc-relocate-budget-mb` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_RELOCATE_COST_GATE` | GC | `CRATONVM_GC=zgc-relocate-cost-gate` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` | GC | `CRATONVM_GC=zgc-relocate-proven-jit` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_STARTBITS` | GC | `CRATONVM_GC=zgc-startbits` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_SWEEP_DEAD_RUNS` | GC | `CRATONVM_GC=zgc-sweep-dead-runs` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_SWEEP_HEADER_ZERO` | GC | `CRATONVM_GC=zgc-sweep-header-zero` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_SWEEP_PAGE_CENSUS` | GC | `CRATONVM_GC=zgc-sweep-page-census` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TARGETED_COMPACTION` | GC | `CRATONVM_GC=targeted-compaction` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB` | GC | `CRATONVM_GC=zgc-tlab` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB_FLAG_PUBLISH_FALSE` | GC | `CRATONVM_GC=tlab-flag-publish-false` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB_OWNED_STARTS` | GC | `CRATONVM_GC=zgc-tlab-owned-starts` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB_RESERVED_BYTES` | GC | `CRATONVM_GC=zgc-tlab-reserved-bytes` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB_STARVED_RECYCLE` | GC | `CRATONVM_GC=zgc-tlab-starved-recycle` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TLAB_TAIL_SINK` | GC | `CRATONVM_GC=zgc-tlab-tail-sink` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZGC_TRIGGER_SHADOW` | GC | `CRATONVM_GC=zgc-trigger-shadow` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_UNREWRITABLE_PEER_REFUSES` | GC | `CRATONVM_GC=zgc-unrewritable-peer-refuses` | opt-in | off | behaviour | snapshot | gc |
| `CRATONVM_ZGC_VM_TLAB_SHARE` | GC | `CRATONVM_GC=zgc-vm-tlab-share` | default-on | on | behaviour | snapshot | gc |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | IO | `CRATONVM_IO=zip-max-entry-bytes` | opt-in | off | behaviour | snapshot | types |
