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
  `gen-on` for a per-backend `default-on` switch, and `zgc-on` for a per-backend
  switch read outside `flags.rs` that is on for `-XX:+UseZGC` only,
  and `gen-g1-on` for
  a per-backend switch on for Generational and G1 and off for ZGC when unset.
  `EFFECTIVE_DEFAULTS` in `render-inventory.py` includes the consumers that
  extend the two-family `BackendDefault`: `CRATONVM_GC_HOTSPOT_INITIAL_HEAP`
  is `on` for every collector (`dc72abfc64`), `CRATONVM_SOFTREF_HOTSPOT_LRU`
  is `on` for all three (G1 via `soft_lru_takes_hotspot_inputs_by_default`,
  ZGC via `SOFTREF_HOTSPOT_LRU_ZGC`), and `CRATONVM_TLAB_SHARE_SIZER` is
  `gen-g1-on` (`TLAB_SHARE_SIZER_G1`, `9fda97cd08`).
  `CRATONVM_TL_INTRINSIC` is `compatible-on`: unset, it registers the side-table
  intrinsics only in Compatible mode (`phases_early.rs`). Its explicit `=0`
  disables them in either mode.
  An `opt-in` key whose reader, unset, follows a parent switch is listed in
  `flag_groups::FOLLOWS`; its Default reads `<parent default> (follows `PARENT`)`
  (guard: `flag_docs_generated`).
  Disabling a following token writes `0`; unsetting it would restore its parent.
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
on". The JIT crate's own on/off reads
go through `cratonvm_types::flags::runtime_flag_on`, which treats unset,
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
| distinct `CRATONVM_*` identifiers appearing anywhere in Rust source | 6,059 |
| exact string literals (i.e. actually named by code, not prose) | 696 |
| **declared** in `flag_groups::INVENTORY` + scalars + group variables | **677** |
| declared before this pass | 576 |
| declared by this pass | **71** |
| allowlisted as intentionally undeclared | 11 |
| user-facing names an operator has to learn | 15 |

The first two rows are **not** generated, though they ARE
enforced -- `types/tests/doc_numeric_claims.rs::flag_inventory_surface_counts_are_current`
re-derives both and fails when either drifts. The recipe is the one in [How to
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
along with `.git` — an earlier form printed **0** for
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
the retirement backlog. The counting rule that
matters: a name's read count is its `--include='*.rs'` hits across the member
crates minus its hits in `types/src/flag_groups.rs`, because the registry names
every variable and that is a declaration, not a read.

Every `INVENTORY` row now also carries a `since:` date, taken from `git log`
rather than guessed, and `flag_groups.rs`'s own `mod tests` enforces a
retirement horizon on it: a newly declared `DBG` knob has
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
the internal test-order audit records.
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

`CRATONVM_GC_CARD_METRICS` is a truthy word:
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

688 rows: 677 declared, 11 allowlisted. Generated by
`tools/flag-census/render-inventory.py` — see
[How to regenerate](#how-to-regenerate).

| Variable | Group | Canonical spelling | Shape | Default | Class | Latched | Read in |
|---|---|---|---|---|---|---|---|
| `CRATONVM_ALLOW_JSR_RET` | LOADER | `CRATONVM_LOADER=allow-jsr-ret` | opt-in | off | behaviour | snapshot | classloading, types |
| `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` | SECURITY | `CRATONVM_SECURITY=noncrypto-sslengine` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_AOT_HMAC_KEY` | SECURITY | `CRATONVM_SECURITY=aot-hmac-key` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_ASSERT_SINGLE_OS_THREAD` | THREADS | `CRATONVM_THREADS=assert-single-os-thread` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | THREADS | `CRATONVM_THREADS=async-handoff-sleep-floor-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | THREADS | `CRATONVM_THREADS=async-submit-grace-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | THREADS | `CRATONVM_THREADS=async-worker-sleep-floor-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_AUDIT_YIELD_INTRINSIC` | LOADER | `CRATONVM_LOADER=audit-yield-intrinsic` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_AVX512` | JIT | `CRATONVM_JIT=avx512` | default-on | on | behaviour | snapshot | jit, native-builtins, native-builtins-crypto |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | THREADS | `CRATONVM_THREADS=await-shortcircuit` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_BIN` | — | `CRATONVM_BIN` | scalar | unset | behaviour | snapshot | — |
| `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS` | THREADS | `CRATONVM_THREADS=blocking-leave-key-reuses-buffers` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_BLOCK_PRIVATE_NETS` | SECURITY | `CRATONVM_SECURITY=block-private-nets` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_BOOT_CLASS_BYTES_REFETCH` | LOADER | `CRATONVM_LOADER=boot-class-bytes-refetch` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_BOOT_IMAGE` | — | `CRATONVM_BOOT_IMAGE` | scalar | unset | behaviour | snapshot | vm-cli |
| `CRATONVM_BOOT_IMAGE_CLASS_BYTES` | LOADER | `CRATONVM_LOADER=boot-image-class-bytes` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_BOOT_IMAGE_DIR` | — | `CRATONVM_BOOT_IMAGE_DIR` | scalar | unset | behaviour | snapshot | vm-cli |
| `CRATONVM_BOOT_IMAGE_TRACE_ADDRS` | DBG | `CRATONVM_DBG=boot-image-trace-addrs` | opt-in | off | diag | snapshot | vm-cli |
| `CRATONVM_BOOT_MODULE_REGISTRY` | LOADER | `CRATONVM_LOADER=boot-module-registry` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_BYTEBUFFER_INTRINSIC` | REAL | `CRATONVM_REAL=bytebuffer-intrinsic` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_CANON_OPENFILE` | IO | `CRATONVM_IO=canon-openfile` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CAPABILITY_GRANTS` | SECURITY | `CRATONVM_SECURITY=capability-grants` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CAPABILITY_LOG` | SECURITY | `CRATONVM_SECURITY=capability-log` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CAPABILITY_MODE` | SECURITY | `CRATONVM_SECURITY=capability-mode` | opt-in | off | behaviour | snapshot | native-api, types |
| `CRATONVM_CARD_TABLE_ONLY` | GC | `CRATONVM_GC=card-table-only` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CF_DELEGATING_YIELD` | LOADER | `CRATONVM_LOADER=cf-delegating-yield` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE` | LOADER | `CRATONVM_LOADER=classpath-jar-unnamed-module` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_CLASS_BY_NAME_NO_ALLOC` | LOADER | `CRATONVM_LOADER=class-by-name-no-alloc` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CLASS_CODE_COMPACT` | LOADER | `CRATONVM_LOADER=class-code-compact` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_CLASS_COMPONENT_JDK_BODY` | JIT | `CRATONVM_JIT=class-component-jdk-body` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_ASSIGNABLE_FALSE` | JIT | `CRATONVM_JIT=class-facts-memo-assignable-false` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_CARRIERS` | JIT | `CRATONVM_JIT=class-facts-memo-carriers` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE` | JIT | `CRATONVM_JIT=class-facts-memo-isinstance-false` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_NAME` | JIT | `CRATONVM_JIT=class-facts-memo-isinstance-name` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_MIRROR` | JIT | `CRATONVM_JIT=class-facts-memo-mirror` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_PROXY` | JIT | `CRATONVM_JIT=class-facts-memo-proxy` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_VALUE_HASH` | JIT | `CRATONVM_JIT=class-facts-memo-value-hash` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_FACTS_MEMO_WRAPPER` | JIT | `CRATONVM_JIT=class-facts-memo-wrapper` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP` | LOADER | `CRATONVM_LOADER=class-mirror-one-loader-lookup` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD` | LOADER | `CRATONVM_LOADER=class-path-live-jar-no-reread` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_CLASS_SHAPE_JDK_BODY` | JIT | `CRATONVM_JIT=class-shape-jdk-body` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_CLV_SUB_FAST_KEY` | LOADER | `CRATONVM_LOADER=clv-sub-fast-key` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_CL_STUB_DELEGATION` | LOADER | `CRATONVM_LOADER=stub-delegation` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_COMPAT` | COMPAT | `CRATONVM_COMPAT=…` | group | unset | — | snapshot | — |
| `CRATONVM_COMPATIBILITY_JDK_ONLY` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | libcratonvm C ABI constant, not a variable |
| `CRATONVM_COMPAT_ALIGN` | COMPAT | `CRATONVM_COMPAT=compat-align` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_COMPAT_AL_ADD_ALL_EMPTY_MODCOUNT` | LOADER | `CRATONVM_LOADER=compat-al-add-all-empty-modcount` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_AL_LISTITR_COMOD` | LOADER | `CRATONVM_LOADER=compat-al-listitr-comod` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_CHM_JDK25_SIZING` | LOADER | `CRATONVM_LOADER=compat-chm-jdk25-sizing` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_GLOBAL_NAME_ASK` | COMPAT | `CRATONVM_COMPAT=compat-global-name-ask` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE` | LOADER | `CRATONVM_LOADER=compat-hashmap-copy-presize` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_HASHMAP_SERIALIZED_THRESHOLD` | LOADER | `CRATONVM_LOADER=compat-hashmap-serialized-threshold` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_IMMUTABLE_OF_JDK_MINT` | LOADER | `CRATONVM_LOADER=compat-immutable-of-jdk-mint` | opt-in | off | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_INITIATING_RECORDS` | COMPAT | `CRATONVM_COMPAT=compat-initiating-records` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_AQLS` | COMPAT | `CRATONVM_COMPAT=compat-jdk-aqls` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE` | COMPAT | `CRATONVM_COMPAT=compat-jdk-completable-future` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP` | COMPAT | `CRATONVM_COMPAT=compat-jdk-concurrent-hash-map` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_COPY_ON_WRITE` | COMPAT | `CRATONVM_COMPAT=compat-jdk-copy-on-write` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET` | COMPAT | `CRATONVM_COMPAT=compat-jdk-copy-on-write-set` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH` | COMPAT | `CRATONVM_COMPAT=compat-jdk-count-down-latch` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_HASHTABLE` | COMPAT | `CRATONVM_COMPAT=compat-jdk-hashtable` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_IMMUTABLES` | COMPAT | `CRATONVM_COMPAT=compat-jdk-immutables` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_LOCK_SUPPORT` | COMPAT | `CRATONVM_COMPAT=compat-jdk-lock-support` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_LOGGING` | COMPAT | `CRATONVM_COMPAT=compat-jdk-logging` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS` | COMPAT | `CRATONVM_COMPAT=compat-jdk-management-beans` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_MEMORY_USAGE` | COMPAT | `CRATONVM_COMPAT=compat-jdk-memory-usage` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE` | COMPAT | `CRATONVM_COMPAT=compat-jdk-priority-blocking-queue` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_PRIORITY_QUEUE` | COMPAT | `CRATONVM_COMPAT=compat-jdk-priority-queue` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_PROCESS` | COMPAT | `CRATONVM_COMPAT=compat-jdk-process` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_STAMPED_LOCK` | COMPAT | `CRATONVM_COMPAT=compat-jdk-stamped-lock` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_STRING_GET_BYTES` | COMPAT | `CRATONVM_COMPAT=compat-jdk-string-get-bytes` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_THREAD_POOLS` | COMPAT | `CRATONVM_COMPAT=compat-jdk-thread-pools` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL` | COMPAT | `CRATONVM_COMPAT=compat-jdk-type-variable-impl` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` | LOADER | `CRATONVM_LOADER=compat-lhm-reversed-live-views` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW` | LOADER | `CRATONVM_LOADER=compat-lhs-reversed-real-view` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_LOADER_CONSTRAINTS` | COMPAT | `CRATONVM_COMPAT=compat-loader-constraints` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_LOADER_THROWS` | COMPAT | `CRATONVM_COMPAT=compat-loader-throws` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_MODULE_MESSAGES` | COMPAT | `CRATONVM_COMPAT=compat-module-messages` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_NAMED_PACKAGES` | COMPAT | `CRATONVM_COMPAT=compat-named-packages` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE` | LOADER | `CRATONVM_LOADER=compat-null-collection-message` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_PARALLEL_LOADERS` | COMPAT | `CRATONVM_COMPAT=compat-parallel-loaders` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_COMPAT_PUT_ALL_NULL_KEY` | LOADER | `CRATONVM_LOADER=compat-put-all-null-key` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_SETN_ITERATION_ORDER` | LOADER | `CRATONVM_LOADER=compat-setn-iteration-order` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_UNMOD_ITERATOR_JDK` | LOADER | `CRATONVM_LOADER=compat-unmod-iterator-jdk` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_COMPAT_URL_APP_HANDLER` | COMPAT | `CRATONVM_COMPAT=compat-url-app-handler` | default-on | on | behaviour | snapshot | native-builtins, types |
| `CRATONVM_CONFINE_IO` | SECURITY | `CRATONVM_SECURITY=confine-io` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_DBG` | DBG | `CRATONVM_DBG=…` | group | unset | — | snapshot | types |
| `CRATONVM_DBG_ACCESS` | DBG | `CRATONVM_DBG=access` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_BYTECODE_ONLY_BODY` | DBG | `CRATONVM_DBG=bytecode-only-body` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EINTR_INJECT` | DBG | `CRATONVM_DBG=eintr-inject` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_EINTR_NO_RETRY` | DBG | `CRATONVM_DBG=eintr-no-retry` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_FORCE_MOVING` | DBG | `CRATONVM_DBG=force-moving` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_GC_AT_BACK_EDGE` | DBG | `CRATONVM_DBG=gc-at-back-edge` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_GETRESOURCES` | DBG | `CRATONVM_DBG=getresources` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_IMAGE_CENSUS` | — | `CRATONVM_DBG_IMAGE_CENSUS` | scalar | unset | behaviour | snapshot | — |
| `CRATONVM_DBG_IR_LINEAR_SCAN` | DBG | `CRATONVM_DBG=ir-linear-scan` | opt-in | off | diag | snapshot | jit |
| `CRATONVM_DBG_JIT_DEOPT_A_LOT` | DBG | `CRATONVM_DBG=jit-deopt-a-lot` | opt-in | off | diag | snapshot | jit, vm |
| `CRATONVM_DBG_JNI_REPEAT_AUDIT` | DBG | `CRATONVM_DBG=jni-repeat-audit` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_MH_LINK_CENSUS` | DBG | `CRATONVM_DBG=mh-link-census` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_MODLOAD` | DBG | `CRATONVM_DBG=modload` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK` | DBG | `CRATONVM_DBG=nativelibraries-load-ok` | opt-in | off | diag | snapshot | native-builtins |
| `CRATONVM_DBG_ROOT_KIND_CENSUS` | DBG | `CRATONVM_DBG=root-kind-census` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_DBG_SELECTOR` | DBG | `CRATONVM_DBG=selector` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_STALE_OBJREF_CYCLES` | DBG | `CRATONVM_DBG=stale-objref-cycles` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_SWEEP_ZERO` | DBG | `CRATONVM_DBG=sweep-zero` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_WATCH_CELL` | DBG | `CRATONVM_DBG=watch-cell` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DBG_ZGC_PARMARK_FAIL` | DBG | `CRATONVM_DBG=zgc-parmark-fail` | opt-in | off | diag | snapshot | gc-zgc |
| `CRATONVM_DEFAULT_HEAP_MAX_MB` | GC | `CRATONVM_GC=default-heap-max-mb` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DEFAULT_WATCHDOG_SEC` | THREADS | `CRATONVM_THREADS=default-watchdog-sec` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_DEFINE_ADD_CLASS_HINTED` | LOADER | `CRATONVM_LOADER=define-add-class-hinted` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK` | LOADER | `CRATONVM_LOADER=define-boot-maps-at-link` | opt-in | off | behaviour | snapshot | classloading |
| `CRATONVM_DEFINE_PREPASS_OBJECT_ONLY` | LOADER | `CRATONVM_LOADER=define-prepass-object-only` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE` | LOADER | `CRATONVM_LOADER=define-vtable-inherit-template` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_DEOPT_EAGER` | DBG | `CRATONVM_DBG=deopt-eager` | opt-in | off | diag | snapshot | — |
| `CRATONVM_DEOPT_REAL` | JIT | `CRATONVM_JIT=deopt-real` | default-on | on | behaviour | snapshot | — |
| `CRATONVM_DEOPT_VERIFY` | DBG | `CRATONVM_DBG=deopt-verify` | opt-in | off | diag | snapshot | — |
| `CRATONVM_DIAG_PROPERTIES` | DBG | `CRATONVM_DBG=diag-properties` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIAG_SERVICELOADER` | DBG | `CRATONVM_DBG=diag-serviceloader` | opt-in | off | diag | snapshot | types |
| `CRATONVM_DIFF_HOTSPOT` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests differential harness |
| `CRATONVM_DISABLE_DEFAULT_WATCHDOG` | THREADS | `CRATONVM_THREADS=default-watchdog` | opt-out | on | behaviour | snapshot | vm-cli |
| `CRATONVM_DISABLE_INTRINSICS` | JIT | `CRATONVM_JIT=intrinsics` | opt-out | on | behaviour | snapshot | — |
| `CRATONVM_DISABLE_JAR_MMAP` | LOADER | `CRATONVM_LOADER=jar-mmap` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_DISABLE_JIT` | — | `CRATONVM_DISABLE_JIT` | scalar | unset | behaviour | snapshot | jit, types, vm-cli |
| `CRATONVM_DISABLE_UNROLL` | JIT | `CRATONVM_JIT=unroll` | both | off | behaviour | snapshot | jit |
| `CRATONVM_EAGER_BOOTSTRAP_ALL` | LOADER | `CRATONVM_LOADER=eager-bootstrap-all` | opt-in | off | behaviour | snapshot | classloading |
| `CRATONVM_ENABLE_ASSERTIONS` | — | `CRATONVM_ENABLE_ASSERTIONS` | scalar | unset | behaviour | snapshot | types, vm-cli |
| `CRATONVM_ENFORCE_NATIVE_SHADOW` | LOADER | `CRATONVM_LOADER=enforce-native-shadow` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_EQE_SYNC_EXECUTE` | THREADS | `CRATONVM_THREADS=eqe-sync-execute` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_EXEC_DEPTH_CEILING` | THREADS | `CRATONVM_THREADS=exec-depth-ceiling` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_EXIT_REPORT_FILE` | DBG | `CRATONVM_DBG=exit-report-file` | opt-in | off | diag | snapshot | types |
| `CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB` | — | `CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB` | scalar | unset | behaviour | snapshot | native-builtins |
| `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY` | JIT | `CRATONVM_JIT=ffm-downcall-in-native-lazy` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_FFM_IN_NATIVE_REPEAT` | JIT | `CRATONVM_JIT=ffm-in-native-repeat` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FJP_EAGER_FORK` | THREADS | `CRATONVM_THREADS=fjp-eager-fork` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_FORCE_WIN_BUILD` | TEST | `CRATONVM_TEST=force-win-build` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FROZEN_PAGES` | GC | `CRATONVM_GC=frozen-pages` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_FUZZ_BOOTCP` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | cargo-fuzz target |
| `CRATONVM_FWD_RESOLVE_STRICT` | LOADER | `CRATONVM_LOADER=fwd-resolve-strict` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_CARD_CLEAN` | GC | `CRATONVM_GC=g1-card-clean` | opt-in | off | behaviour | snapshot | gc-g1, types |
| `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` | GC | `CRATONVM_GC=g1-card-screen-jit-pinned` | default-on | on | behaviour | snapshot | gc-g1, types |
| `CRATONVM_G1_CLEANUP_WALK` | GC | `CRATONVM_GC=g1-cleanup-walk` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_COVERAGE_PIN` | GC | `CRATONVM_GC=g1-coverage-pin` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EDEN_STRIPES` | GC | `CRATONVM_GC=g1-eden-stripes` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_COPY_WATCH` | GC | `CRATONVM_GC=g1-evac-copy-watch` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` | GC | `CRATONVM_GC=g1-evac-ref-implausible-refuse` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_EVAC_SHARE_CENSUS` | GC | `CRATONVM_GC=g1-evac-share-census` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_HUMONGOUS_MARKS` | GC | `CRATONVM_GC=g1-humongous-marks` | default-on | on | behaviour | snapshot | gc-g1, types |
| `CRATONVM_G1_IHOP_BACKOFF` | GC | `CRATONVM_GC=g1-ihop-backoff` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_IHOP_COUNTS_REGIONS` | GC | `CRATONVM_GC=g1-ihop-counts-regions` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_IHOP_GROSS_GROWTH` | GC | `CRATONVM_GC=g1-ihop-gross-growth` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_IHOP_POLL_GATE` | GC | `CRATONVM_GC=g1-ihop-poll-gate` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_MARK_BACKOFF_DEADLINE` | GC | `CRATONVM_GC=g1-mark-backoff-deadline` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_MARK_CONVERGENCE_EPOCH` | GC | `CRATONVM_GC=g1-mark-convergence-epoch` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_MARK_SIDE_TABLES` | GC | `CRATONVM_GC=g1-mark-side-tables` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_NO_EVAC_RETRY` | GC | `CRATONVM_GC=g1-evac-retry` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_G1_PARALLEL_EVAC` | GC | `CRATONVM_GC=g1-parallel-evac` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_G1_PIN_EMPTY_PUBLICATION` | GC | `CRATONVM_GC=g1-pin-empty-publication` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_REF_WRITE_WATCH` | GC | `CRATONVM_GC=g1-ref-write-watch` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_REMARK_ITL_EPHEMERON` | GC | `CRATONVM_GC=g1-remark-itl-ephemeron` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_G1_REMARK_ITL_SWEEP` | GC | `CRATONVM_GC=g1-remark-itl-sweep` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` | GC | `CRATONVM_GC=g1-remark-unload-after-keep` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_G1_SCRUB_FREE` | GC | `CRATONVM_GC=g1-scrub-free` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_UNCOMMIT` | GC | `CRATONVM_GC=g1-uncommit` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_VERIFY_FORWARDS_RETIRED` | GC | `CRATONVM_GC=g1-verify-forwards-retired` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_VERIFY_HOLDERS` | GC | `CRATONVM_GC=g1-verify-holders` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_WORKERS` | GC | `CRATONVM_GC=g1-workers` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_G1_YOUNG_PAUSE_TARGET` | GC | `CRATONVM_GC=g1-young-pause-target` | default-on | on | behaviour | snapshot | gc-g1, types |
| `CRATONVM_GC` | GC | `CRATONVM_GC=…` | group | unset | — | snapshot | — |
| `CRATONVM_GC_ADAPTIVE_TENURING` | GC | `CRATONVM_GC=adaptive-tenuring` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_CARD_METRICS` | GC | `CRATONVM_GC=card-metrics` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_CONC_START_PERCENT` | GC | `CRATONVM_GC=conc-start-percent` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` | GC | `CRATONVM_GC=conc-walk-gap-recovery` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_FULL_GC_TRUE_ROOTS` | GC | `CRATONVM_GC=full-gc-true-roots` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_FULL_RSET_SCAN` | GC | `CRATONVM_GC=full-rset-scan` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_FUTILE_YOUNG_BACKOFF` | GC | `CRATONVM_GC=futile-young-backoff` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_G1_ONLY_JIT_PINS` | GC | `CRATONVM_GC=g1-only-jit-pins` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY` | GC | `CRATONVM_GC=handshake-at-helper-entry` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS` | GC | `CRATONVM_GC=interp-type-map-locals` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD` | GC | `CRATONVM_GC=moving-major-jit-guard` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_NO_CONCURRENT_FIRST` | GC | `CRATONVM_GC=concurrent-first` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_BORROW_YOUNG` | GC | `CRATONVM_GC=old-borrow-young` | default-on | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GC_OLD_CAPACITY_SHRINK` | GC | `CRATONVM_GC=old-capacity-shrink` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_CLOSE_BITMAP` | GC | `CRATONVM_GC=old-close-bitmap` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX` | GC | `CRATONVM_GC=old-compact-forward-index` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_COMPACT_TRUST_MARK_VERIFY` | GC | `CRATONVM_GC=old-compact-trust-mark-verify` | opt-in | off | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_EXACT_START_CLASS` | GC | `CRATONVM_GC=old-exact-start-class` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_FRAG_COMPACT` | GC | `CRATONVM_GC=old-frag-compact` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_FREE_RATIO_GROWTH` | GC | `CRATONVM_GC=old-free-ratio-growth` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_GIVE_BACK` | GC | `CRATONVM_GC=old-give-back` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_HUMONGOUS_TOP` | GC | `CRATONVM_GC=old-humongous-top` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_INTERIOR_DECOMMIT` | GC | `CRATONVM_GC=old-interior-decommit` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_MARK_PREFETCH` | GC | `CRATONVM_GC=old-mark-prefetch` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_OOM_COMPACT` | GC | `CRATONVM_GC=old-oom-compact` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_PAR_ADJUST` | GC | `CRATONVM_GC=old-par-adjust` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_PAR_CLOSE` | GC | `CRATONVM_GC=old-par-close` | opt-in | off | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_PAR_MARK` | GC | `CRATONVM_GC=old-par-mark` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_PINNED_COMPACT` | GC | `CRATONVM_GC=old-pinned-compact` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` | GC | `CRATONVM_GC=old-shrink-after-concurrent` | default-on | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GC_OLD_SHRINK_FACTOR` | GC | `CRATONVM_GC=old-shrink-factor` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` | GC | `CRATONVM_GC=old-trigger-hysteresis` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_OLD_TRIGGER_SERIAL` | GC | `CRATONVM_GC=old-trigger-serial` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GC_OVERHEAD_PROGRESS` | GC | `CRATONVM_GC=overhead-progress` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_PAR_EVAC` | GC | `CRATONVM_GC=par-evac` | default-on | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GC_PAR_EVAC_CARD_SEED` | GC | `CRATONVM_GC=par-evac-card-seed` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GC_PAR_MIN_BYTES` | GC | `CRATONVM_GC=par-min-bytes` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_PAR_THREADS` | GC | `CRATONVM_GC=par-threads` | opt-in | off | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GC_QUEUED_FINALIZABLES_ARE_ROOTS` | GC | `CRATONVM_GC=queued-finalizables-are-roots` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GC_STRESS` | GC | `CRATONVM_GC=stress` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` | GC | `CRATONVM_GC=sweep-anchor-stride` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_SYNC_YOUNG_WIPE` | GC | `CRATONVM_GC=sync-young-wipe` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` | GC | `CRATONVM_GC=system-gc-moving-young` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GC_VERIFY_RSET` | GC | `CRATONVM_GC=verify-rset` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GENERIC_REFLECT_MEMO` | COMPAT | `CRATONVM_COMPAT=generic-reflect-memo` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_CONC_CLASS_UNLOAD` | GC | `CRATONVM_GC=gen-conc-class-unload` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_CONC_INLINE_START` | GC | `CRATONVM_GC=gen-conc-inline-start` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK` | GC | `CRATONVM_GC=gen-conc-mark-local-stack` | opt-out | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` | GC | `CRATONVM_GC=gen-conc-mark-tams-starts` | default-on | on | behaviour | snapshot | gc-core, gc-gen |
| `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN` | GC | `CRATONVM_GC=gen-conc-frozen-eager-scan` | opt-out | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE` | GC | `CRATONVM_GC=gen-conc-pause-no-precise-root-table` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GEN_CONC_PRECEDENCE` | GC | `CRATONVM_GC=gen-conc-precedence` | default-on | on | behaviour | snapshot | gc-core, gc-gen |
| `CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP` | GC | `CRATONVM_GC=gen-conc-preempt-growth-net-of-sweep` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` | GC | `CRATONVM_GC=gen-conc-remark-refproc-hook` | default-on | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GEN_CONC_REMARK_THREAD_SATB_FLUSH` | GC | `CRATONVM_GC=gen-conc-remark-thread-satb-flush` | default-on | on | behaviour | snapshot | gc-gen |
| `CRATONVM_GEN_CONC_SERVICE_THREAD` | GC | `CRATONVM_GC=gen-conc-service-thread` | opt-in | off | behaviour | snapshot | types, vm |
| `CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS` | GC | `CRATONVM_GC=gen-conc-y2o-old-holder-census` | opt-in | off | behaviour | snapshot | gc-core |
| `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` | GC | `CRATONVM_GC=gen-humongous-ref-zero-unlocked` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED` | GC | `CRATONVM_GC=gen-humongous-zero-unlocked` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_PINNED_YOUNG_COPY` | GC | `CRATONVM_GC=gen-pinned-young-copy` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL` | GC | `CRATONVM_GC=gen-pinned-young-copy-parallel` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` | GC | `CRATONVM_GC=gen-precise-root-promote` | opt-in | off | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GEN_SATB_OLD_ONLY` | GC | `CRATONVM_GC=gen-satb-old-only` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_GEN_TLAB_TAIL_SINK` | GC | `CRATONVM_GC=gen-tlab-tail-sink` | default-on | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GEN_UNCOMMIT` | GC | `CRATONVM_GC=gen-uncommit` | default-on | on | behaviour | snapshot | gc-gen, types |
| `CRATONVM_GEN_XMS_USABLE_FIRST` | GC | `CRATONVM_GC=gen-xms-usable-first` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GEN_Y2O_LIVE_SEED` | GC | `CRATONVM_GC=gen-y2o-live-seed` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_GEN_YOUNG_MIRROR_DEFER` | GC | `CRATONVM_GC=gen-young-mirror-defer` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` | GC | `CRATONVM_GC=gen-young-mirror-defer-moving-follows` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE` | GC | `CRATONVM_GC=gen-young-mirror-root-when-promotable` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` | GC | `CRATONVM_GC=gen-young-pin-ledger-term4` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_GEN_ZERO_ONCE` | GC | `CRATONVM_GC=gen-zero-once` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_GPU_CRITICAL_LEASE_MS` | GC | `CRATONVM_GC=gpu-critical-lease-ms` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_CRITICAL_WAIT_MS` | GC | `CRATONVM_GC=gpu-critical-wait-ms` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_HOST_CALLBACK` | JIT | `CRATONVM_JIT=gpu-host-callback` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_GPU_NATIVE_STREAM_POOL` | JIT | `CRATONVM_JIT=gpu-native-stream-pool` | opt-in | off | behaviour | snapshot | cuda-bridge |
| `CRATONVM_GPU_TRACE_BYTES` | DBG | `CRATONVM_DBG=gpu-trace-bytes` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | SECURITY | `CRATONVM_SECURITY=harden-manifest-classpath` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_HTTPSRV_KEEPALIVE` | IO | `CRATONVM_IO=httpsrv-keepalive` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_HTTP_MAX_BODY` | IO | `CRATONVM_IO=http-max-body` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_IAE_TRACE` | DBG | `CRATONVM_DBG=iae-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_INDY_CHECKS_EVERY_MODE` | COMPAT | `CRATONVM_COMPAT=indy-checks-every-mode` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_INFLATER_REUSE_BUFFERS` | IO | `CRATONVM_IO=inflater-reuse-buffers` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_INHERIT_THREAD_CCL` | THREADS | `CRATONVM_THREADS=inherit-thread-ccl` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INHERIT_TL_WORKAROUND` | THREADS | `CRATONVM_THREADS=inherit-tl-workaround` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_INIT_FINALIZER_TAIL` | THREADS | `CRATONVM_THREADS=init-finalizer-tail` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_INVOKESTATIC_LOADER_TRACE` | DBG | `CRATONVM_DBG=invokestatic-loader-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_IO` | IO | `CRATONVM_IO=…` | group | unset | — | snapshot | — |
| `CRATONVM_ISOLATED_LOADER_DICTIONARY` | LOADER | `CRATONVM_LOADER=isolated-loader-dictionary` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_ITR_BYTECODE` | REAL | `CRATONVM_REAL=itr-bytecode` | default-on | on | behaviour | snapshot | native-collections |
| `CRATONVM_JAR_ARCHIVE_CACHE_CAP` | IO | `CRATONVM_IO=jar-archive-cache-cap` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JAR_LAZY_INDEX` | LOADER | `CRATONVM_LOADER=jar-lazy-index` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JAR_LOADER_BYTES_INDEX` | LOADER | `CRATONVM_LOADER=jar-loader-bytes-index` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_JAVA_HOME` | — | `CRATONVM_JAVA_HOME` | scalar | unset | behaviour | snapshot | libcratonvm, native-builtins, types, vm, vm-cli |
| `CRATONVM_JBOSS_MP_ROOT` | COMPAT | `CRATONVM_COMPAT=jboss-mp-root` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JCA_LENIENT_GETINSTANCE` | SECURITY | `CRATONVM_SECURITY=jca-lenient-getinstance` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_JFR_ENABLE_EVENTS` | — | `CRATONVM_JFR_ENABLE_EVENTS` | scalar | unset | behaviour | snapshot | vm |
| `CRATONVM_JIMAGE_DIRECT_MAP` | IO | `CRATONVM_IO=jimage-direct-map` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIMAGE_PACKAGE_INDEX` | LOADER | `CRATONVM_LOADER=jimage-package-index` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS` | LOADER | `CRATONVM_LOADER=jimage-skip-directory-records` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIT` | JIT | `CRATONVM_JIT=…` | group | unset | — | snapshot | jit, native-builtins, types |
| `CRATONVM_JIT_AALOAD_LICM_TYPECHECK_FENCE` | JIT | `CRATONVM_JIT=aaload-licm-typecheck-fence` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ARITH_LICM_TRIM_ENTERED` | JIT | `CRATONVM_JIT=arith-licm-trim-entered` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_ARM64` | JIT | `CRATONVM_JIT=arm64` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_ARM64_SAFEPOINTS` | JIT | `CRATONVM_JIT=arm64-safepoints` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_BYTECODE_ONLY_KEY` | JIT | `CRATONVM_JIT=jit-bytecode-only-key` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIT_FIELD_ARRAY_BCE` | JIT | `CRATONVM_JIT=field-array-bce` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_ARRAY_HOIST` | JIT | `CRATONVM_JIT=field-array-hoist` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN` | JIT | `CRATONVM_JIT=field-array-receiver-screen` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_FIELD_INT_HOIST` | JIT | `CRATONVM_JIT=field-int-hoist` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_FORCE_C2` | JIT | `CRATONVM_JIT=force-c2` | opt-in | off | behaviour | snapshot | — |
| `CRATONVM_JIT_GDB` | JIT | `CRATONVM_JIT=gdb` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS` | JIT | `CRATONVM_JIT=implicit-local-handlers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_INLINE_CARD_MARK` | JIT | `CRATONVM_JIT=inline-card-mark` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIT_INVOKE_FAST_DOOR` | JIT | `CRATONVM_JIT=invoke-fast-door` | both | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_IR_CALL` | JIT | `CRATONVM_JIT=ir-call` | default-on | on | behaviour | snapshot | — |
| `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` | JIT | `CRATONVM_JIT=ir-chain-deep-spine` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE` | JIT | `CRATONVM_JIT=ir-chain-entry-splice` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS` | JIT | `CRATONVM_JIT=ir-chain-inner-loop-proofs` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION` | JIT | `CRATONVM_JIT=ir-elide-homes-spill-at-inspection` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES` | JIT | `CRATONVM_JIT=ir-elide-unnamed-homes` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` | JIT | `CRATONVM_JIT=ir-frame-block-max-homes` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LAZY_REF_HOMES` | JIT | `CRATONVM_JIT=ir-lazy-ref-homes` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` | JIT | `CRATONVM_JIT=ir-loop-intrinsic-call` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS` | JIT | `CRATONVM_JIT=ir-loop-lock-coarsen-blocks` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS` | JIT | `CRATONVM_JIT=ir-loop-lock-coarsen-instance-blocks` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE` | JIT | `CRATONVM_JIT=ir-loop-root-own-compile` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS` | JIT | `CRATONVM_JIT=ir-loop-root-own-compile-strips` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` | JIT | `CRATONVM_JIT=ir-nested-chain-splice` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT` | JIT | `CRATONVM_JIT=ir-nested-in-loop-hot` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE` | JIT | `CRATONVM_JIT=ir-nested-lambda-iface` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PARAM_COPY` | JIT | `CRATONVM_JIT=ir-param-copy` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE` | JIT | `CRATONVM_JIT=ir-phi-coalesce-back-edge` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND` | JIT | `CRATONVM_JIT=ir-seq-charat-entry-bound` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE` | JIT | `CRATONVM_JIT=ir-seq-charat-field-class-predicate` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` | JIT | `CRATONVM_JIT=ir-seq-charat-hoist` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` | JIT | `CRATONVM_JIT=ir-seq-charat-predicate` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED` | JIT | `CRATONVM_JIT=ir-seq-charat-predicate-spliced` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR` | JIT | `CRATONVM_JIT=ir-shadow-frame-block-lazy-clear` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` | JIT | `CRATONVM_JIT=ir-sink-equal-depth` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES` | JIT | `CRATONVM_JIT=ir-site-named-loop-values` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPECULATE` | JIT | `CRATONVM_JIT=ir-speculate` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES` | JIT | `CRATONVM_JIT=ir-splice-chain-fences` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` | JIT | `CRATONVM_JIT=ir-splice-frame-states` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN` | JIT | `CRATONVM_JIT=ir-splice-multi-return` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN` | JIT | `CRATONVM_JIT=ir-splice-putstatic-loop-chain` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY` | JIT | `CRATONVM_JIT=ir-splice-putstatic-loop-replay` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_LOCAL_HANDLERS` | JIT | `CRATONVM_JIT=local-handlers` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_METRICS` | JIT | `CRATONVM_JIT=metrics` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_METRICS_OUT` | JIT | `CRATONVM_JIT=metrics-out` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_METRICS_RING` | JIT | `CRATONVM_JIT=metrics-ring` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_MH_COMPILED_LINK` | JIT | `CRATONVM_JIT=jit-mh-compiled-link` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO` | JIT | `CRATONVM_JIT=jit-mh-compiled-link-agree-memo` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID` | JIT | `CRATONVM_JIT=jit-mh-compiled-link-linker-by-id` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO` | JIT | `CRATONVM_JIT=jit-mh-compiled-link-pair-memo` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE` | JIT | `CRATONVM_JIT=jit-mh-compiled-link-user-site` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_JIT_NO_ARRAYLENGTH_FAST` | JIT | `CRATONVM_JIT=arraylength-fast` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE` | JIT | `CRATONVM_JIT=backedge-poll-gate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_CACHED_NATIVE_FACTS` | JIT | `CRATONVM_JIT=cached-native-facts` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE` | JIT | `CRATONVM_JIT=field-addr-elide` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_FIELD_FAST_PATH` | JIT | `CRATONVM_JIT=field-fast-path` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_INVOKE_FAST_DOOR` | JIT | `CRATONVM_JIT=invoke-fast-door` | both | off | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_LOCALS_SLAB` | JIT | `CRATONVM_JIT=locals-slab` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR` | JIT | `CRATONVM_JIT=nonvirtual-fast-door` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_OPERAND_FOLD` | JIT | `CRATONVM_JIT=operand-fold` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_NO_OSR_INLINE_GATE` | JIT | `CRATONVM_JIT=osr-inline-gate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_REF_ARRAY_FAST` | JIT | `CRATONVM_JIT=ref-array-fast` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NO_SYSTEM_CLASS_LATCH` | JIT | `CRATONVM_JIT=system-class-latch` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_NULL_TRAP_RECOMPILE` | JIT | `CRATONVM_JIT=null-trap-recompile` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_OSR` | JIT | `CRATONVM_JIT=osr` | opt-in | off | behaviour | snapshot | — |
| `CRATONVM_JIT_OSR_LIVE_STACK_RETIRE` | JIT | `CRATONVM_JIT=osr-live-stack-retire` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_PRINT_COMPILATION` | JIT | `CRATONVM_JIT=print-compilation` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PROFILE_FAST_CALL_COUNTS` | JIT | `CRATONVM_JIT=profile-fast-call-counts` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_PROFILE_LOAD` | JIT | `CRATONVM_JIT=profile-load` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_PROFILE_SAVE` | JIT | `CRATONVM_JIT=profile-save` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_REFERS_TO_DIRECT` | JIT | `CRATONVM_JIT=refers-to-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_REF_HOIST_OOP_MAPS` | JIT | `CRATONVM_JIT=ref-hoist-oop-maps` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_SPLICE_NESTED_ROW_PAST_STATIC_INTRINSIC` | JIT | `CRATONVM_JIT=splice-nested-row-past-static-intrinsic` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_SP_IC_DENY` | JIT | `CRATONVM_JIT=sp-ic-deny` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SP_IC_ONLY` | JIT | `CRATONVM_JIT=sp-ic-only` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_SR_SLOTS_PAST_LICM` | JIT | `CRATONVM_JIT=sr-slots-past-licm` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_THRESHOLD` | JIT | `CRATONVM_JIT=threshold` | opt-in | off | behaviour | snapshot | types, vm |
| `CRATONVM_JIT_TL_SET_SINGLE_SATB` | JIT | `CRATONVM_JIT=tl-set-single-satb` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JIT_UNROLL` | JIT | `CRATONVM_JIT=unroll` | both | off | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNSAFE_FENCE_DIRECT` | JIT | `CRATONVM_JIT=unsafe-fence-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNSAFE_REF_STORE_DIRECT` | JIT | `CRATONVM_JIT=unsafe-ref-store-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_UNSAFE_VOLATILE_DIRECT` | JIT | `CRATONVM_JIT=unsafe-volatile-direct` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VECTORIZE` | JIT | `CRATONVM_JIT=vectorize` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VEC_EWISE_FORMS` | JIT | `CRATONVM_JIT=vec-ewise-forms` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_ARENA_ORDER` | JIT | `CRATONVM_JIT=verify-arena-order` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_BRANCHES` | JIT | `CRATONVM_JIT=verify-branches` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_JIT_VERIFY_FRAME_STATES` | JIT | `CRATONVM_JIT=verify-frame-states` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_IR` | JIT | `CRATONVM_JIT=verify-ir` | default-on | on | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` | JIT | `CRATONVM_JIT=verify-memory-chain` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_SCHEDULE` | JIT | `CRATONVM_JIT=verify-schedule` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JIT_VERIFY_TYPES` | JIT | `CRATONVM_JIT=verify-types` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_JMOD_LAZY_INDEX` | LOADER | `CRATONVM_LOADER=jmod-lazy-index` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_JMX_CALLER_HELD_METHOD_MONITOR_DEPTHS` | THREADS | `CRATONVM_THREADS=jmx-caller-held-method-monitor-depths` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_JMX_OWNED_SYNCHRONIZERS` | THREADS | `CRATONVM_THREADS=jmx-owned-synchronizers` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE` | THREADS | `CRATONVM_THREADS=jmx-running-locks-at-trace` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_BULK_ARRAY_COPY` | GC | `CRATONVM_GC=jni-bulk-array-copy` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FOREIGN_TID_PUBLISH` | GC | `CRATONVM_GC=jni-foreign-tid-publish` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_FOREIGN_TRANSITIONS` | GC | `CRATONVM_GC=jni-foreign-transitions` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_INDIRECT_LOCALS` | GC | `CRATONVM_GC=jni-indirect-locals` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT` | GC | `CRATONVM_GC=jni-in-native-arrive-no-wait` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` | GC | `CRATONVM_GC=jni-in-native-band-at-pause` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_DOORS` | JIT | `CRATONVM_JIT=jni-in-native-doors` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST` | GC | `CRATONVM_GC=jni-in-native-envelope-first` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_LAZY` | JIT | `CRATONVM_JIT=jni-in-native-lazy` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_NO_SIGNAL` | JIT | `CRATONVM_JIT=jni-in-native-no-signal` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_IN_NATIVE_REPEAT` | GC | `CRATONVM_GC=jni-in-native-repeat` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_NATIVE_TRANSITIONS` | GC | `CRATONVM_GC=jni-native-transitions` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_JNI_NATIVE_TRANSITIONS_DEFAULT` | GC | `CRATONVM_GC=jni-native-transitions-default` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE` | COMPAT | `CRATONVM_COMPAT=ldc-handle-access-every-mode` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LENIENT_CLINIT` | LOADER | `CRATONVM_LOADER=lenient-clinit` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LEVER_DROP` | LOADER | `CRATONVM_LOADER=lever-drop` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_LEVER_KEEP` | LOADER | `CRATONVM_LOADER=lever-keep` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_LINK_STATE_ONE_WRITE` | LOADER | `CRATONVM_LOADER=link-state-one-write` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY` | LOADER | `CRATONVM_LOADER=link-type-map-walk-loads-only` | opt-in | off | behaviour | snapshot | classloading |
| `CRATONVM_LOADER` | LOADER | `CRATONVM_LOADER=…` | group | unset | — | snapshot | — |
| `CRATONVM_LOADER_AWARE_RESOLUTION` | LOADER | `CRATONVM_LOADER=aware-resolution` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` | LOADER | `CRATONVM_LOADER=loader-dictionary-before-drive` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` | LOADER | `CRATONVM_LOADER=loader-dictionary-one-door` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LOADER_FIND_LOADED_NO_STANDIN` | LOADER | `CRATONVM_LOADER=loader-find-loaded-no-standin` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_LOADER_LABEL_PARENT_MEMO` | LOADER | `CRATONVM_LOADER=loader-label-parent-memo` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LOADER_LINK_VERDICT_MEMO` | LOADER | `CRATONVM_LOADER=loader-link-verdict-memo` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO` | LOADER | `CRATONVM_LOADER=loader-load-class-override-memo` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LOADER_NO_ANN_PROXY_LATCH` | LOADER | `CRATONVM_LOADER=ann-proxy-latch` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE` | LOADER | `CRATONVM_LOADER=dup-name-field-gate` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD` | LOADER | `CRATONVM_LOADER=resolution-failure-record` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_LOADER_NO_SUBTYPE_DISPLAY` | LOADER | `CRATONVM_LOADER=subtype-display` | opt-out | on | behaviour | snapshot | classloading |
| `CRATONVM_LOADER_PARENT_CHAIN` | LOADER | `CRATONVM_LOADER=parent-chain` | default-on | on | behaviour | snapshot | classloading |
| `CRATONVM_LOADER_REQUEST_DOOR` | LOADER | `CRATONVM_LOADER=loader-request-door` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LOADER_UNLOAD` | LOADER | `CRATONVM_LOADER=unload` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_LOCK_KEY_SHARDS` | THREADS | `CRATONVM_THREADS=lock-key-shards` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_LOCK_ORDER_CHECK` | THREADS | `CRATONVM_THREADS=lock-order-check` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_LONGREWRITE_LOOSE` | LOADER | `CRATONVM_LOADER=longrewrite-loose` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_LOOKUP_IN_LOADS_OUTER` | COMPAT | `CRATONVM_COMPAT=lookup-in-loads-outer` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MAVEN_REPO_LOCAL` | — | `CRATONVM_MAVEN_REPO_LOCAL` | scalar | unset | behaviour | snapshot | native-builtins, types |
| `CRATONVM_MAX_INFLATED_BYTES` | GC | `CRATONVM_GC=max-inflated-bytes` | opt-in | off | behaviour | snapshot | native-builtins, types |
| `CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM` | LOADER | `CRATONVM_LOADER=method-type-no-loader-resolves-in-vm` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES` | JIT | `CRATONVM_JIT=mh-bound-type-table-app-classes` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE` | COMPAT | `CRATONVM_COMPAT=mh-combinator-checks-every-mode` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_MH_INTERN_DESCRIPTOR_TYPES` | JIT | `CRATONVM_JIT=mh-intern-descriptor-types` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_LOADER_SCOPED_TYPES` | JIT | `CRATONVM_JIT=mh-loader-scoped-types` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT` | JIT | `CRATONVM_JIT=mh-real-ctor-cast-adapter-lane-under-jit` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_MIMALLOC_ARENA_MAX_OBJECT_KB` | GC | `CRATONVM_GC=mimalloc-arena-max-object-kb` | default-on | on | behaviour | snapshot | vm-cli |
| `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` | GC | `CRATONVM_GC=mimalloc-lazy-arena-commit` | default-on | on | behaviour | snapshot | vm-cli |
| `CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT` | GC | `CRATONVM_GC=mimalloc-sparse-block-boot-count` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB` | GC | `CRATONVM_GC=mimalloc-sparse-block-min-kb` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP` | GC | `CRATONVM_GC=mimalloc-sparse-block-mmap` | default-on | on | behaviour | snapshot | vm-cli |
| `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` | COMPAT | `CRATONVM_COMPAT=module-access-export-one-question` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY` | COMPAT | `CRATONVM_COMPAT=module-export-all-by-identity` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_MONITOR_ENTER_SPIN` | THREADS | `CRATONVM_THREADS=monitor-enter-spin` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_MONITOR_FASTPATH` | THREADS | `CRATONVM_THREADS=monitor-fastpath` | default-on | on | behaviour | snapshot | jit, vm |
| `CRATONVM_MONITOR_HEIR` | THREADS | `CRATONVM_THREADS=monitor-heir` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_HEIR_ORDERED` | THREADS | `CRATONVM_THREADS=monitor-heir-ordered` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_HEIR_SIGNAL` | THREADS | `CRATONVM_THREADS=monitor-heir-signal` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_INDEX_PRUNE` | THREADS | `CRATONVM_THREADS=monitor-index-prune` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_LAZY_PARK_US` | — | `CRATONVM_MONITOR_LAZY_PARK_US` | scalar | unset | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_MAX_SPINNERS` | — | `CRATONVM_MONITOR_MAX_SPINNERS` | scalar | unset | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_PENDING_NOTIFY` | THREADS | `CRATONVM_THREADS=monitor-pending-notify` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_SPIN_AFTER_WAKE` | THREADS | `CRATONVM_THREADS=monitor-spin-after-wake` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT` | THREADS | `CRATONVM_THREADS=monitor-spin-handover-abort` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_MONITOR_THIN_NOTIFY` | THREADS | `CRATONVM_THREADS=monitor-thin-notify` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_MSC_REAL_START` | REAL | `CRATONVM_REAL=msc-real-start` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NATIVE_ENCODING` | — | `CRATONVM_NATIVE_ENCODING` | scalar | unset | behaviour | snapshot | native-api, vm |
| `CRATONVM_NATIVE_MATCHER_FIND` | JIT | `CRATONVM_JIT=native-matcher-find` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | JIT | `CRATONVM_JIT=native-pbe-keyfactory` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_NATIVE_SHADOW_SINK_CAP` | DBG | `CRATONVM_DBG=native-shadow-sink-cap` | opt-in | off | diag | snapshot | jit, types, vm |
| `CRATONVM_NATIVE_STRING_REGEX` | JIT | `CRATONVM_JIT=native-string-regex` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_NESTED_JAR_BYTES_DIRECTORY` | LOADER | `CRATONVM_LOADER=nested-jar-bytes-directory` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_NESTED_JAR_BYTES_INDEX` | LOADER | `CRATONVM_LOADER=nested-jar-bytes-index` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` | LOADER | `CRATONVM_LOADER=nested-jar-class-path-outer` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE` | LOADER | `CRATONVM_LOADER=nested-jar-outer-handle-reuse` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_NESTED_JAR_POOLED_STAT` | LOADER | `CRATONVM_LOADER=nested-jar-pooled-stat` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_NESTED_JAR_SHARED_OUTER` | LOADER | `CRATONVM_LOADER=nested-jar-shared-outer` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | IO | `CRATONVM_IO=netty-queue-bridge` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_NET_JDK_BACKLOG` | IO | `CRATONVM_IO=net-jdk-backlog` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_NONEXISTENT_VAR_12345` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | absent-name probe |
| `CRATONVM_NO_CTOR_DIRECT_CALL` | JIT | `CRATONVM_JIT=ctor-direct-call` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_DEFRAG_PROMOTE` | GC | `CRATONVM_GC=defrag-promote` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` | GC | `CRATONVM_GC=exact-refproc-survival` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_GC_PROMOTION_GUARD` | GC | `CRATONVM_GC=promotion-guard` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_IR_BRANCHY` | JIT | `CRATONVM_JIT=ir-branchy` | opt-out | on | behaviour | snapshot | — |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW` | JIT | `CRATONVM_JIT=inline-tlab-new` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY` | JIT | `CRATONVM_JIT=inline-tlab-newarray` | opt-out | on | behaviour | snapshot | jit |
| `CRATONVM_NO_OLDGEN_COALESCE` | GC | `CRATONVM_GC=oldgen-coalesce` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_OSR_CTOR_BIND` | JIT | `CRATONVM_JIT=osr-ctor-bind` | opt-out | on | behaviour | snapshot | vm |
| `CRATONVM_NO_REFERENT_IDENTITY_SCREEN` | GC | `CRATONVM_GC=referent-identity-screen` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_SELECTIVE_PROMOTE` | GC | `CRATONVM_GC=selective-promote` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_SELECTOR_CONNECT_PROBE` | IO | `CRATONVM_IO=selector-connect-probe` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_NO_STUBS` | REAL | `CRATONVM_REAL=stubs` | opt-out | on | behaviour | snapshot | native-api, types, vm-cli |
| `CRATONVM_NSEE_TRACE` | DBG | `CRATONVM_DBG=nsee-trace` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_OLDGEN_COMPACT` | GC | `CRATONVM_GC=oldgen-compact` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_PARK_ATOMIC_PERMIT` | THREADS | `CRATONVM_THREADS=park-atomic-permit` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_DEPOSIT_FROZEN_BAND` | THREADS | `CRATONVM_THREADS=park-deposit-frozen-band` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` | THREADS | `CRATONVM_THREADS=park-event-lockless-wake` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK` | THREADS | `CRATONVM_THREADS=park-event-signal-after-unlock` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_LAZY_SPIN_NS` | THREADS | `CRATONVM_THREADS=park-lazy-spin-ns` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_LAZY_STW_WAKE` | THREADS | `CRATONVM_THREADS=park-lazy-stw-wake` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_LAZY_SUSPEND_AWARE` | THREADS | `CRATONVM_THREADS=park-lazy-suspend-aware` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_PARK_UNTIMED_EVENT` | THREADS | `CRATONVM_THREADS=park-untimed-event` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PARK_WAKE_AFTER_UNLOCK` | THREADS | `CRATONVM_THREADS=park-wake-after-unlock` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PERF_ATTACH_FOREIGN` | COMPAT | `CRATONVM_COMPAT=perf-attach-foreign` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PERF_ATTACH_MISSING_THROWS` | COMPAT | `CRATONVM_COMPAT=perf-attach-missing-throws` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PERF_DATA_IN_MEMORY_FALLBACK` | COMPAT | `CRATONVM_COMPAT=perf-data-in-memory-fallback` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_PERF_HRT_QPC_FREQUENCY` | COMPAT | `CRATONVM_COMPAT=perf-hrt-qpc-frequency` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_PHASE_ACCOUNTING` | DBG | `CRATONVM_DBG=phase-accounting` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PHASE_ACCOUNTING_JFR` | DBG | `CRATONVM_DBG=phase-accounting-jfr` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PHASE_ACCOUNTING_OUT` | DBG | `CRATONVM_DBG=phase-accounting-out` | opt-in | off | diag | snapshot | jfr |
| `CRATONVM_PROMOTION_OOM_GUARD_BROAD` | GC | `CRATONVM_GC=promotion-oom-guard-broad` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_PROPS_UNROOTED_RECEIVERS` | REAL | `CRATONVM_REAL=props-unrooted-receivers` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_PURE_BYTECODE` | LOADER | `CRATONVM_LOADER=pure-bytecode` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_REAL` | REAL | `CRATONVM_REAL=…` | group | unset | — | snapshot | types, vm, vm-cli |
| `CRATONVM_REAL_AGROAL` | REAL | `CRATONVM_REAL=agroal` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_ANNOTATIONS` | REAL | `CRATONVM_REAL=annotations` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_AQS` | REAL | `CRATONVM_REAL=aqs` | both | off | behaviour | snapshot | types |
| `CRATONVM_REAL_BUILTIN_LOADERS` | LOADER | `CRATONVM_LOADER=real-builtin-loaders` | default-on | on | behaviour | snapshot | native-api |
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
| `CRATONVM_REFERS_TO_LEAF` | THREADS | `CRATONVM_THREADS=refers-to-leaf` | opt-in | off | behaviour | snapshot | native-builtins |
| `CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE` | COMPAT | `CRATONVM_COMPAT=reflect-new-array-one-resolve` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_REFLECT_NO_EXPORT_GATE` | SECURITY | `CRATONVM_SECURITY=reflect-export-gate` | opt-out | on | behaviour | snapshot | native-api |
| `CRATONVM_REF_DISCOVER_AT_ALLOCATION` | GC | `CRATONVM_GC=ref-discover-at-allocation` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_REF_NOTES_PER_THREAD` | GC | `CRATONVM_GC=ref-notes-per-thread` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_REGEN_HEADER` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | libcratonvm/build.rs |
| `CRATONVM_REGEX_MATCHER_BYTECODE` | JIT | `CRATONVM_JIT=regex-matcher-bytecode` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_REQUIRE_POLICY` | SECURITY | `CRATONVM_SECURITY=require-policy` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_RESOLVE_CACHE_CAP` | LOADER | `CRATONVM_LOADER=resolve-cache-cap` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | IO | `CRATONVM_IO=resolve-outbound-host` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests opt-in |
| `CRATONVM_SECURITY` | SECURITY | `CRATONVM_SECURITY=…` | group | unset | — | snapshot | — |
| `CRATONVM_SELECTION_CHECKS_EVERY_MODE` | COMPAT | `CRATONVM_COMPAT=selection-checks-every-mode` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_SELECT_MAX_BLOCK_MS` | IO | `CRATONVM_IO=select-max-block-ms` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_SHADOW_STACK` | JIT | `CRATONVM_JIT=shadow-stack` | opt-in | off | behaviour | snapshot | jit, types |
| `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS` | THREADS | `CRATONVM_THREADS=shutdown-hook-timeout-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL` | THREADS | `CRATONVM_THREADS=sleep-high-resolution-interval` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_SOCKET_CAPTURE` | IO | `CRATONVM_IO=socket-capture` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_SOFTREF_HOTSPOT_FREE` | GC | `CRATONVM_GC=softref-hotspot-free` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_SOFTREF_HOTSPOT_LRU` | GC | `CRATONVM_GC=softref-hotspot-lru` | default-on | on | behaviour | snapshot | gc-core, types, vm |
| `CRATONVM_SOFT_EXIT` | DBG | `CRATONVM_DBG=soft-exit` | opt-in | off | diag | snapshot | types |
| `CRATONVM_SOMETHING_BRAND_NEW` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | unknown-key fall-through probe |
| `CRATONVM_SPRING_BOOT_FATJAR` | — | n/a (undeclared) | live | unset | harness/ABI | live getenv | vm/tests fixture path |
| `CRATONVM_SP_NO_COALESCE` | JIT | `CRATONVM_JIT=sp-coalesce` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_STACKWALK_CALLER_CLASS_TAIL` | JIT | `CRATONVM_JIT=stackwalk-caller-class-tail` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_STACKWALK_DEFERRED_LINES` | JIT | `CRATONVM_JIT=stackwalk-deferred-lines` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID` | JIT | `CRATONVM_JIT=stackwalk-pipeline-invoke-by-id` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE` | JIT | `CRATONVM_JIT=stackwalk-pipeline-layout-once` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_STACKWALK_WINDOW_CAPTURE` | JIT | `CRATONVM_JIT=stackwalk-window-capture` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_STACK_TRACE_BINARY_NAMES` | COMPAT | `CRATONVM_COMPAT=stack-trace-binary-names` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS` | COMPAT | `CRATONVM_COMPAT=stack-trace-element-exact-class` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN` | COMPAT | `CRATONVM_COMPAT=stack-trace-element-full-origin` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO` | COMPAT | `CRATONVM_COMPAT=stack-trace-element-origin-memo` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_STACK_TRACE_MODULE_PREFIX` | COMPAT | `CRATONVM_COMPAT=stack-trace-module-prefix` | default-on | on | behaviour | snapshot | native-api |
| `CRATONVM_STDOUT_ENCODING` | — | `CRATONVM_STDOUT_ENCODING` | scalar | unset | behaviour | snapshot | native-api |
| `CRATONVM_STRESS_THREAD_STATES` | THREADS | `CRATONVM_THREADS=stress-thread-states` | default-on | on | behaviour | snapshot | types, vm |
| `CRATONVM_STRIPED_COUNTERS_OFF` | THREADS | `CRATONVM_THREADS=striped-counters` | opt-out | on | behaviour | snapshot | types |
| `CRATONVM_SUREFIRE_IPC_DBG` | DBG | `CRATONVM_DBG=surefire-ipc-dbg` | opt-in | off | diag | snapshot | types |
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
| `CRATONVM_TEST_JDK` | TEST | `CRATONVM_TEST=jdk` | opt-in | off | behaviour | snapshot | — |
| `CRATONVM_TEST_SEGV` | TEST | `CRATONVM_TEST=segv` | opt-in | off | behaviour | snapshot | vm-cli |
| `CRATONVM_THREADS` | THREADS | `CRATONVM_THREADS=…` | group | unset | — | snapshot | types |
| `CRATONVM_THREAD_CONTAINERS` | THREADS | `CRATONVM_THREADS=thread-containers` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_THREAD_LAYOUT_CACHE` | COMPAT | `CRATONVM_COMPAT=thread-layout-cache` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS` | COMPAT | `CRATONVM_COMPAT=thread-run-thread-declared-fields` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_THREAD_START_GRACE_MS` | THREADS | `CRATONVM_THREADS=thread-start-grace-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_THREAD_START_ONE_PUBLISH` | THREADS | `CRATONVM_THREADS=thread-start-one-publish` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS` | COMPAT | `CRATONVM_COMPAT=throwable-thread-run-nested-tasks` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_TIER_C1_THREADS` | JIT | `CRATONVM_JIT=tier-c1-threads` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C1_THRESHOLD` | JIT | `CRATONVM_JIT=tier-c1-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_MIN_INVOCATIONS` | JIT | `CRATONVM_JIT=tier-c2-min-invocations` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_STANDBY` | JIT | `CRATONVM_JIT=tier-c2-standby` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_STANDBY_DEPTH` | JIT | `CRATONVM_JIT=tier-c2-standby-depth` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_STANDBY_SCALE` | JIT | `CRATONVM_JIT=tier-c2-standby-scale` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_THREADS` | JIT | `CRATONVM_JIT=tier-c2-threads` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_C2_THRESHOLD` | JIT | `CRATONVM_JIT=tier-c2-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_ENABLED` | JIT | `CRATONVM_JIT=tiered` | default-on | on | behaviour | snapshot | jit |
| `CRATONVM_TIER_OSR_BACKEDGE` | JIT | `CRATONVM_JIT=tier-osr-backedge` | opt-in | off | behaviour | snapshot | — |
| `CRATONVM_TIER_OSR_THRESHOLD` | JIT | `CRATONVM_JIT=tier-osr-threshold` | opt-in | off | behaviour | snapshot | jit |
| `CRATONVM_TIER_PGO` | JIT | `CRATONVM_JIT=tier-pgo` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_TLAB_FILLER_SKIP_ZERO` | GC | `CRATONVM_GC=tlab-filler-skip-zero` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_TLAB_GATE_BUMP_FLOOR` | GC | `CRATONVM_GC=tlab-gate-bump-floor` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_TLAB_SHARE_SIZER` | GC | `CRATONVM_GC=tlab-share-sizer` | default-on | gen-g1-on | behaviour | snapshot | types |
| `CRATONVM_TLS_OPENSSL_CLIENT` | SECURITY | `CRATONVM_SECURITY=tls-openssl-client` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_TL_INTRINSIC` | THREADS | `CRATONVM_THREADS=tl-intrinsic` | default-on | compatible-on | behaviour | snapshot | native-builtins, types |
| `CRATONVM_TL_REAL_FAST` | THREADS | `CRATONVM_THREADS=tl-real-fast` | default-on | on | behaviour | snapshot | native-builtins |
| `CRATONVM_TRACE_SB_FILTER` | DBG | `CRATONVM_DBG=trace-sb-filter` | opt-in | off | diag | snapshot | vm |
| `CRATONVM_TRIVIAL_GETTER_DESC_MEMO` | JIT | `CRATONVM_JIT=trivial-getter-desc-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_TRUST_PEM` | SECURITY | `CRATONVM_SECURITY=trust-pem` | opt-in | off | behaviour | snapshot | classloading, types |
| `CRATONVM_UNPARK_EETOP` | THREADS | `CRATONVM_THREADS=unpark-eetop` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_UNRETIRE_NATIVE_SHADOW` | LOADER | `CRATONVM_LOADER=unretire-native-shadow` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_UNTRUSTED_CODE` | SECURITY | `CRATONVM_SECURITY=untrusted-code` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` | LOADER | `CRATONVM_LOADER=user-loader-resource-bytecode-compiled` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS` | LOADER | `CRATONVM_LOADER=user-pass3-records-link-loads` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | REAL | `CRATONVM_REAL=use-wildfly-reflect-shim` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | REAL | `CRATONVM_REAL=use-wildfly-synth-bytecode` | opt-in | off | behaviour | snapshot | types |
| `CRATONVM_VERIFY_CONTROL_RULES` | LOADER | `CRATONVM_LOADER=verify-control-rules` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_HANDLER_CATCH_MEMO` | LOADER | `CRATONVM_LOADER=verify-handler-catch-memo` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_HANDLER_FRAMES` | LOADER | `CRATONVM_LOADER=verify-handler-frames` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_INITIALIZERS_FIRST` | LOADER | `CRATONVM_LOADER=verify-initializers-first` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_MAJOR50_FAILOVER` | LOADER | `CRATONVM_LOADER=verify-major50-failover` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_NAME_ANSWER_MEMO` | LOADER | `CRATONVM_LOADER=verify-name-answer-memo` | opt-in | off | behaviour | snapshot | classloading |
| `CRATONVM_VERIFY_RECEIVER_ASSIGNABLE` | LOADER | `CRATONVM_LOADER=verify-receiver-assignable` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_SUBCLASS_MEMO` | LOADER | `CRATONVM_LOADER=verify-subclass-memo` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_SUBROUTINES` | LOADER | `CRATONVM_LOADER=verify-subroutines` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_THIS_UNINIT_FLAGS` | LOADER | `CRATONVM_LOADER=verify-this-uninit-flags` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE` | LOADER | `CRATONVM_LOADER=verify-type-map-buffer-reuse` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_USER_LOADER_CLASSES` | LOADER | `CRATONVM_LOADER=verify-user-loader-classes` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK` | LOADER | `CRATONVM_LOADER=verify-user-loader-classes-at-link` | default-on | on | behaviour | snapshot | types |
| `CRATONVM_VTHREAD_CARRIER_SLEEPS` | THREADS | `CRATONVM_THREADS=vthread-carrier-sleeps` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_VTHREAD_LEAN_REGISTRY` | THREADS | `CRATONVM_THREADS=vthread-lean-registry` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_VTHREAD_SLICE_ON_CARRIER` | THREADS | `CRATONVM_THREADS=vthread-slice-on-carrier` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED` | THREADS | `CRATONVM_THREADS=vthread-slice-on-carrier-compiled` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_WAIT_SPURIOUS_MS` | THREADS | `CRATONVM_THREADS=wait-spurious-ms` | opt-in | off | behaviour | snapshot | vm |
| `CRATONVM_WIN_HIRES_PARK` | THREADS | `CRATONVM_THREADS=win-hires-park` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_ZGC_CENSUS` | GC | `CRATONVM_GC=zgc-census` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_CENSUS_ACCESS` | GC | `CRATONVM_GC=zgc-census-access` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT` | GC | `CRATONVM_GC=zgc-code-heap-init-hotspot` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY` | GC | `CRATONVM_GC=zgc-collection-interval-only` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_COLOR_FLIP` | GC | `CRATONVM_GC=zgc-color-flip` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET` | GC | `CRATONVM_GC=zgc-conc-close-forgets-pause-budget` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER` | GC | `CRATONVM_GC=zgc-define-mirror-reserved-every-definer` | default-on | on | behaviour | snapshot | native-builtins, vm |
| `CRATONVM_ZGC_FOOTPRINT_TRIM` | GC | `CRATONVM_GC=zgc-footprint-trim` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_FORWARD_CHAIN_BY_EPOCH` | GC | `CRATONVM_GC=zgc-forward-chain-by-epoch` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES` | GC | `CRATONVM_GC=zgc-gcinfo-code-heap-edges` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR` | GC | `CRATONVM_GC=zgc-gen-bitmap-fallback-major` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_ELEMENT_CARDS` | GC | `CRATONVM_GC=zgc-gen-element-cards` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION` | GC | `CRATONVM_GC=zgc-gen-empty-minor-needs-promotion` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR` | GC | `CRATONVM_GC=zgc-gen-escalate-weak-minor` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE` | GC | `CRATONVM_GC=zgc-gen-floor-arm-high-old-live` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR` | GC | `CRATONVM_GC=zgc-gen-full-request-major` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE` | GC | `CRATONVM_GC=zgc-gen-high-young-live` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE` | GC | `CRATONVM_GC=zgc-gen-inline-announce` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR` | GC | `CRATONVM_GC=zgc-gen-jit-promotion-floor` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG` | GC | `CRATONVM_GC=zgc-gen-jmx-major-young` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_JMX_YOUNG_BELOW_FLOOR` | GC | `CRATONVM_GC=zgc-gen-jmx-young-below-floor` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES` | GC | `CRATONVM_GC=zgc-gen-major-rule-causes` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_MINOR_TIMER` | GC | `CRATONVM_GC=zgc-gen-minor-timer` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP` | GC | `CRATONVM_GC=zgc-gen-nursery-after-warmup` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_OLD_BITMAP` | GC | `CRATONVM_GC=zgc-gen-old-bitmap` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY` | GC | `CRATONVM_GC=zgc-gen-old-bitmap-verify` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE` | GC | `CRATONVM_GC=zgc-gen-slide-old-live` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES` | GC | `CRATONVM_GC=zgc-gen-system-gc-tenures` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP` | GC | `CRATONVM_GC=zgc-gen-young-aged-sweep` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT` | GC | `CRATONVM_GC=zgc-gen-young-complement` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_EVACUATE` | GC | `CRATONVM_GC=zgc-gen-young-evacuate` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE` | GC | `CRATONVM_GC=zgc-gen-young-pool-live` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT` | GC | `CRATONVM_GC=zgc-gen-young-predict-exact` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_RELOCATE` | GC | `CRATONVM_GC=zgc-gen-young-relocate` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` | GC | `CRATONVM_GC=zgc-gen-young-skip-references` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_HOTSPOT_POOL_INIT` | GC | `CRATONVM_GC=zgc-hotspot-pool-init` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_INTERVAL_ONLY_OCCUPANCY_DOORS_STALL` | GC | `CRATONVM_GC=zgc-interval-only-occupancy-doors-stall` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS` | GC | `CRATONVM_GC=zgc-jmx-hotspot-non-heap-pools` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_JMX_METASPACE_POOL` | GC | `CRATONVM_GC=zgc-jmx-metaspace-pool` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_MARK_LAYOUT_MEMO` | GC | `CRATONVM_GC=zgc-mark-layout-memo` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_MARK_ONE_DECODE` | GC | `CRATONVM_GC=zgc-mark-one-decode` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_MARK_REF_DISPS` | GC | `CRATONVM_GC=zgc-mark-ref-disps` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_MARK_TEST_FIRST` | GC | `CRATONVM_GC=zgc-mark-test-first` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_METASPACE_FIRST_CYCLES` | GC | `CRATONVM_GC=zgc-metaspace-first-cycles` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_METASPACE_PEAK_AT_GC` | GC | `CRATONVM_GC=zgc-metaspace-peak-at-gc` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_METASPACE_THRESHOLD` | GC | `CRATONVM_GC=zgc-metaspace-threshold` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_NURSERY_CENSUS` | GC | `CRATONVM_GC=zgc-nursery-census` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_NURSERY_CENSUS_AGE` | GC | `CRATONVM_GC=zgc-nursery-census-age` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN` | GC | `CRATONVM_GC=zgc-parmark-fallback-rescan` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE` | GC | `CRATONVM_GC=zgc-pause-gcinfo-no-usage` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_POOL_PEAK_AT_GC` | GC | `CRATONVM_GC=zgc-pool-peak-at-gc` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_REGION_RELOCATE` | GC | `CRATONVM_GC=zgc-region-relocate` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_RELOCATE_SIZE_TABLE` | GC | `CRATONVM_GC=zgc-relocate-size-table` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED` | GC | `CRATONVM_GC=zgc-relocations-prune-amortized` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL` | GC | `CRATONVM_GC=zgc-relocation-set-books-final` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_REMAP_IN_MARK` | GC | `CRATONVM_GC=zgc-remap-in-mark` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_RESET_ACCOUNTING` | GC | `CRATONVM_GC=zgc-reset-accounting` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT` | GC | `CRATONVM_GC=zgc-selective-relocate-frag-pct` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB` | GC | `CRATONVM_GC=zgc-selective-relocate-hole-kib` | opt-in | off | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_SLIDE_PIN_BY_BASE` | GC | `CRATONVM_GC=zgc-slide-pin-by-base` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE` | GC | `CRATONVM_GC=zgc-stall-mark-reaches-joined-pause` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_SWEEP_SIZE_HINTS` | GC | `CRATONVM_GC=zgc-sweep-size-hints` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_SWEEP_SIZE_MEMO` | GC | `CRATONVM_GC=zgc-sweep-size-memo` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS` | GC | `CRATONVM_GC=zgc-target-exemption-after-pins` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_THRESHOLD_SET_DRAIN` | GC | `CRATONVM_GC=zgc-threshold-set-drain` | default-on | on | behaviour | snapshot | gc-core |
| `CRATONVM_ZGC_TICK_STOP_ABORTS` | GC | `CRATONVM_GC=zgc-tick-stop-aborts` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE` | GC | `CRATONVM_GC=zgc-tick-withdraw-keeps-poll-proactive` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_TICK_WITHDRAW_OWN_BIT` | GC | `CRATONVM_GC=zgc-tick-withdraw-own-bit` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_TIMER_NOT_WARMUP` | GC | `CRATONVM_GC=zgc-timer-not-warmup` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_TIMER_SKIPS_STALL_CYCLES` | GC | `CRATONVM_GC=zgc-timer-skips-stall-cycles` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_TLAB` | GC | `CRATONVM_GC=zgc-tlab` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZGC_UNLOAD_CLASS_NAME_MEMO` | GC | `CRATONVM_GC=zgc-unload-class-name-memo` | default-on | on | behaviour | snapshot | vm |
| `CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR` | GC | `CRATONVM_GC=zgc-warmup-explicit-door` | default-on | on | behaviour | snapshot | gc-zgc |
| `CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS` | IO | `CRATONVM_IO=zip-cen-accessor-intrinsics` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_ZIP_ENTRY_MISS_INTRINSIC` | IO | `CRATONVM_IO=zip-entry-miss-intrinsic` | opt-in | off | behaviour | snapshot | native-api |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | IO | `CRATONVM_IO=zip-max-entry-bytes` | opt-in | off | behaviour | snapshot | types |
