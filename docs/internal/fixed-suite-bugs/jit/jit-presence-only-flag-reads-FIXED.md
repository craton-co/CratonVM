# FIXED: the JIT read its boolean switches as presence-only, so `CRATONVM_X=0` turned them ON

**Status: FIXED 2026-09-12.** New `cratonvm_types::flags::runtime_flag_on`
(`types/src/flags.rs`). 357 presence-only reads were converted across
`jit/src`, `vm/src/jit` and `vm/src/runtime/interpreter/jit_bridge.rs`, plus one
raw `std::env::var(..).is_ok()`. Ratchet test:
`jit/tests/no_presence_only_flag_reads.rs`.

## The defect

The JIT read its `CRATONVM_*` switches with one idiom:

```rust
cratonvm_types::flags::runtime_var_os("CRATONVM_X").is_some()   // opt-in
cratonvm_types::flags::runtime_var_os("CRATONVM_NO_X").is_none() // opt-out
```

That answers "is the variable set", not "is the switch on". So all of these
turned a feature **on**, or turned a kill switch **on** (the feature off):

```
CRATONVM_X=0    CRATONVM_X=false    CRATONVM_X=off    CRATONVM_X=no    CRATONVM_X=
```

This is the opposite of what anyone writing `=0` means. It also disagreed with
the rest of the codebase, which reads such switches with `parse::truthy_word`.
A JIT code review found it.

It is not only a diagnostic nuisance. `flag_groups.rs` carried a comment on
`CRATONVM_JIT_RANGE_BCE` saying `=0` *enabled* guard-dominated bounds-check
elimination, a pass where a wrong elision is an out-of-bounds heap write.
`ir_lower.rs` documents `CRATONVM_JIT_C2_ALLOC_UPGRADE=0` as a way to switch
the C2 allocation upgrade off, which under presence semantics switched it on.
Every opt-out kill switch (`CRATONVM_NO_*`, `CRATONVM_JIT_NO_*`,
`CRATONVM_DISABLE_*`) had the mirror problem. `=0` meant "kill it".

One read also skipped the flag layer entirely.
`ir_optimize.rs` read `CRATONVM_DBG_LOAD_CSE` with `std::env::var(..).is_ok()`.
The name is declared, so `CRATONVM_DBG=load-cse` writes it into the
latched snapshot, and a raw `std::env` read never sees that.

Grouped tokens were never affected: `CRATONVM_JIT=token` writes `1` into the
legacy key, which is on under either reading.

## The fix

`runtime_flag_on(name)` is false when `runtime_var_os(name)` is `None`.
Otherwise it applies the `truthy_word` value rule to the lossily-decoded value.
A value is off iff, trimmed and ASCII-lowercased, it is empty, `0`, `false`,
`off` or `no`. It goes through `runtime_var_os`, so it keeps the latched
snapshot for declared names, per-thread test overrides for undeclared ones, and
the `CRATONVM_DBG_FLAGREADS` census. The rule lives in one private
`word_is_on`, and `parse::truthy_word` now calls it too, so the two cannot
drift apart. A unit test pins both against the same table: unset, `""`, `0`,
`false`, `OFF`, `" no "` are off; `1`, `true`, `yes`, `anything` are on.

The sweep was scripted and reviewed hunk by hunk:

| Form | Sites |
|---|---|
| `runtime_var_os("…").is_some()` → `runtime_flag_on("…")` | 293 |
| `runtime_var_os("…").is_none()` → `!runtime_flag_on("…")` | 64 |
| `std::env::var("CRATONVM_DBG_LOAD_CSE").is_ok()` → `runtime_flag_on(…)` | 1 |

That includes the forms split across lines and the forms with a trailing
comma in the argument list. Every call site was already fully qualified, so no
import changed. The shorter call let rustfmt rejoin some lines: single-expression
`get_or_init(|| { … })` closures, `if a && b` conditions, and `if … {` braces.
Those hunks were rejoined to match, because CI checks rustfmt on every hunk a
diff touches.

## What was deliberately NOT converted

A name was left presence-parsed when converting only the JIT's read would have
made one flag mean two things:

| Flag | Site(s) | Why it stays |
|---|---|---|
| `CRATONVM_JIT_VERIFY_IR` | `jit/src/ir_verify.rs` test `verify_enabled_matches_the_build_profile_when_unset` | A tri-state value (`env_flag`, `parse::tristate_word`); the test is about the UNSET case. |
| `CRATONVM_DBG_JIT_METHOD_STATS` | `jit/src/lib.rs`, `vm/src/jit/helpers.rs` | Also parsed by `parse::exactly_one` in the typed configuration. |
| `CRATONVM_TIER_C2_THRESHOLD` | `jit/src/tiered.rs` | A number, parsed by the tier policy. |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL` | `jit/src/x64/licm.rs` | A mode word (`nostore`, `all`). |
| `CRATONVM_DBG_TYPECHECK_FILTER` | `vm/src/jit/helpers.rs` | A filter string, read with `runtime_var`. |
| `CRATONVM_SHADOW_STACK` | `vm/src/jit/conservative_roots.rs` (2) | The collector reads it through `parse::present` (`flags().jit.shadow_stack`). A JIT-only reinterpretation would let `=0` enable the JIT scan cache while the collector believes the shadow stack is on. This is the three-way gate skew `x64-flag-skew-and-contracts.md` exists to prevent. |
| `CRATONVM_DBG_FORCE_MOVING` | `vm/src/jit/conservative_roots.rs` | The same argument: `gc_flags().dbg_force_moving` is `parse::present`, and the read gates the same scan cache. |

The non-literal test helper in `ir_verify.rs`
(`options_from_env_defaults_to_structural_only`,
`runtime_var_os(n).is_none()`) was also left alone. It asks whether the harness
left a variable unset before asserting a default.

No test in the tree sets a converted flag to a non-truthy value to exercise
presence semantics. The only `=0` spellings of converted names were comments,
and each of them meant "off".

### Known remaining skew, diagnostic only

Some converted `CRATONVM_DBG_*` / `CRATONVM_TRACE_*` names are also read, still
presence-only, outside the swept tree. Examples: `CRATONVM_DBG_DEOPT` in
`deopt_resume.rs`, `CRATONVM_DBG_JITC` in `env_cache.rs` and `interpreter.rs`,
and `CRATONVM_DBG_PRECISE` / `CRATONVM_TRACE_CLASSVALUE` in the typed
configuration. For those, `=0` now silences the JIT's lines but not the
interpreter's. They print; none of them gates a result. Converting the rest of
the workspace is follow-up work, not part of this fix.

## Regression coverage

* `types/src/flags.rs`: `runtime_flag_on_reads_the_off_words_as_off`.
* `jit/tests/no_presence_only_flag_reads.rs`: a ratchet. It scans `jit/src`
  for `runtime_var_os(` whose balanced argument list is followed, across any
  whitespace and newlines, by `.is_some()` / `.is_none()`. Whole-line
  comments are skipped. The count must equal `ALLOWED = 5`, the five sites
  above that live in `jit/src`, so a new presence read fails, and so does a
  conversion that forgets to lower the number. The needle is built at runtime.
  A second test pins the scanner itself on split calls, comments, value reads
  and non-literal names.
* `jit/src/ir_lower.rs`
  `the_optimizing_tier_stays_shut_to_allocation_while_it_has_no_inline_tlab`
  asserted the C2 allocation-upgrade gate is opt-in by finding `is_some()` in
  its body. Its needle is now `runtime_flag_on(`, which is the same property.

User-facing wording is in `flag-tokens.md` and `flag-inventory.md`. No flag was
added, removed or renamed.
